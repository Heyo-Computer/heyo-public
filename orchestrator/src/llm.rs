//! Small, provider-independent LLM adapter used by the repository agent.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub provider: LlmProvider,
    pub model: String,
    pub prompt: String,
    pub parameters: Value,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LlmProvider {
    Anthropic,
    Openai,
    Mistral,
    Gemini,
}

#[derive(Debug, Clone, Copy)]
pub enum ExecutionPurpose {
    ToolUse,
    CodeGeneration,
    PlanGeneration,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    async fn execute(&self, arguments: Value) -> Result<Value>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    fn schemas(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|tool| {
                json!({
                    "name": tool.name(),
                    "description": tool.description(),
                    "input_schema": tool.input_schema(),
                })
            })
            .collect()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value> {
        self.tools
            .get(name)
            .ok_or_else(|| anyhow!("Tool not found: {name}"))?
            .execute(input)
            .await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: MessageContent::Text(content.into()),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug)]
struct ProviderResponse {
    text: String,
    calls: Vec<ToolCall>,
    usage: TokenUsage,
}

#[derive(Debug)]
struct ToolCall {
    id: String,
    name: String,
    input: Value,
}

pub struct AgentV2LlmClient {
    http: reqwest::Client,
}

impl AgentV2LlmClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    pub async fn execute(
        &self,
        config: &LlmConfig,
        prompt: &str,
        _purpose: Option<ExecutionPurpose>,
    ) -> Result<String> {
        let response = self.request(config, &[Message::user(prompt)], &[]).await?;
        if !response.calls.is_empty() {
            return Err(anyhow!(
                "Malformed provider response: completion unexpectedly contained tool calls"
            ));
        }
        Ok(response.text)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute_with_tools<F>(
        &self,
        config: &LlmConfig,
        mut messages: Vec<Message>,
        registry: &ToolRegistry,
        max_iterations: usize,
        mut callback: Option<F>,
        _shared: Option<Arc<std::sync::Mutex<String>>>,
        _purpose: Option<ExecutionPurpose>,
        _dynamic: Option<Arc<std::sync::Mutex<Option<ExecutionPurpose>>>>,
    ) -> Result<(String, TokenUsage)>
    where
        F: FnMut(&str, bool, &str, Option<&Value>),
    {
        let mut total = TokenUsage::default();
        for _ in 0..max_iterations {
            let response = self.request(config, &messages, &registry.schemas()).await?;
            total.input_tokens += response.usage.input_tokens;
            total.output_tokens += response.usage.output_tokens;
            if response.calls.is_empty() {
                return Ok((response.text, total));
            }
            let mut blocks = Vec::new();
            if !response.text.is_empty() {
                blocks.push(ContentBlock::Text {
                    text: response.text.clone(),
                });
            }
            blocks.extend(response.calls.iter().map(|call| ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            }));
            messages.push(Message { role: MessageRole::Assistant, content: MessageContent::Blocks(blocks) });
            let mut results = Vec::new();
            for call in response.calls {
                match registry.execute(&call.name, call.input).await {
                    Ok(value) => {
                        if let Some(cb) = callback.as_mut() {
                            cb(&call.name, true, "Tool executed", Some(&value));
                        }
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: serde_json::to_string(&value)?,
                        });
                    }
                    Err(error) => {
                        let value = json!({"error":error.to_string()});
                        if let Some(cb) = callback.as_mut() {
                            cb(&call.name, false, &error.to_string(), Some(&value));
                        }
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: value.to_string(),
                        });
                    }
                }
            }
            messages.push(Message { role: MessageRole::User, content: MessageContent::Blocks(results) });
        }
        Err(anyhow!("LLM did not produce a final answer after {max_iterations} tool iterations"))
    }

    async fn request(
        &self,
        config: &LlmConfig,
        messages: &[Message],
        tools: &[Value],
    ) -> Result<ProviderResponse> {
        let key = config
            .api_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| anyhow!("An explicit API key is required"))?;
        let (url, body) = match config.provider {
            LlmProvider::Anthropic => (
                "https://api.anthropic.com/v1/messages".to_string(),
                anthropic_body(config, messages, tools),
            ),
            LlmProvider::Openai => (
                "https://api.openai.com/v1/chat/completions".to_string(),
                openai_body(config, messages, tools),
            ),
            LlmProvider::Mistral => (
                "https://api.mistral.ai/v1/chat/completions".to_string(),
                openai_body(config, messages, tools),
            ),
            LlmProvider::Gemini => (
                format!(
                    "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={key}",
                    normalize_gemini_model(&config.model)
                ),
                gemini_body(config, messages, tools),
            ),
        };
        let mut request = self.http.post(&url).json(&body);
        request = match config.provider {
            LlmProvider::Anthropic => request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01"),
            LlmProvider::Openai | LlmProvider::Mistral => request.bearer_auth(key),
            LlmProvider::Gemini => request,
        };
        let response = request
            .send()
            .await
            .context("Provider request failed")?;
        let status = response.status();
        let text = response
            .text()
            .await
            .context("Failed to read provider response")?;
        if !status.is_success() {
            return Err(anyhow!("Provider API returned {status}: {text}"));
        }
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("Malformed provider JSON response: {text}"))?;
        if let Some(error) = value.get("error") {
            return Err(anyhow!("Provider API error: {error}"));
        }
        match config.provider {
            LlmProvider::Anthropic => parse_anthropic(&value),
            LlmProvider::Openai | LlmProvider::Mistral => parse_openai(&value),
            LlmProvider::Gemini => parse_gemini(&value),
        }
    }
}

fn merge_parameters(mut body: Value, config: &LlmConfig) -> Value {
    if let (Some(out), Some(params)) = (body.as_object_mut(), config.parameters.as_object()) {
        for (key, value) in params {
            if key != "messages" && key != "tools" {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    body
}

fn openai_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        let role = match message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        };
        match &message.content {
            MessageContent::Text(text) => out.push(json!({"role": role, "content": text})),
            MessageContent::Blocks(blocks) => {
                let text = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let calls: Vec<_> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, name, input } => Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": input.to_string()},
                        })),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    out.push(json!({
                        "role": "assistant",
                        "content": if text.is_empty() { Value::Null } else { json!(text) },
                        "tool_calls": calls,
                    }));
                } else if !text.is_empty() {
                    out.push(json!({"role": role, "content": text}));
                }
                for block in blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    } = block
                    {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                    }
                }
            }
        }
    }
    out
}

fn openai_body(config: &LlmConfig, messages: &[Message], tools: &[Value]) -> Value {
    let definitions: Vec<_> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool["name"],
                    "description": tool["description"],
                    "parameters": tool["input_schema"],
                },
            })
        })
        .collect();
    let mut body = json!({"model": config.model, "messages": openai_messages(messages)});
    if !definitions.is_empty() {
        body["tools"] = json!(definitions);
    }
    merge_parameters(body, config)
}

fn anthropic_body(config: &LlmConfig, messages: &[Message], tools: &[Value]) -> Value {
    let mut body = json!({"model": config.model, "max_tokens": 4096, "messages": messages});
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    merge_parameters(body, config)
}

fn normalize_gemini_model(model: &str) -> String {
    match model {
        "flash" | "1.5-flash" => "gemini-1.5-flash".to_string(),
        "pro" | "1.5-pro" => "gemini-1.5-pro".to_string(),
        "2.0-flash" => "gemini-2.0-flash".to_string(),
        explicit if explicit.starts_with("gemini-") => explicit.to_string(),
        other => format!("gemini-{other}"),
    }
}

fn gemini_body(config: &LlmConfig, messages: &[Message], tools: &[Value]) -> Value {
    let mut names = HashMap::new();
    for message in messages {
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                if let ContentBlock::ToolUse { id, name, .. } = block {
                    names.insert(id, name);
                }
            }
        }
    }
    let contents: Vec<_> = messages
        .iter()
        .filter_map(|message| {
            let role = if matches!(message.role, MessageRole::Assistant) {
                "model"
            } else {
                "user"
            };
            let parts: Vec<_> = match &message.content {
                MessageContent::Text(text) => vec![json!({"text": text})],
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text } => json!({"text": text}),
                        ContentBlock::ToolUse { name, input, .. } => {
                            json!({"functionCall": {"name": name, "args": input}})
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                        } => json!({"functionResponse": {
                            "name": names.get(tool_use_id).map(|name| name.as_str()).unwrap_or("unknown"),
                            "response": serde_json::from_str::<Value>(content)
                                .unwrap_or(json!({"result": content})),
                        }}),
                    })
                    .collect(),
            };
            (!parts.is_empty()).then(|| json!({"role": role, "parts": parts}))
        })
        .collect();
    let declarations: Vec<_> = tools
        .iter()
        .map(|tool| json!({
            "name": tool["name"],
            "description": tool["description"],
            "parameters": clean_gemini_schema(&tool["input_schema"]),
        }))
        .collect();
    let max_tokens = config
        .parameters
        .get("max_tokens")
        .or_else(|| config.parameters.get("maxOutputTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(4096);
    let mut generation_config = json!({"maxOutputTokens": max_tokens});
    if let Some(temperature) = config.parameters.get("temperature") {
        generation_config["temperature"] = temperature.clone();
    }
    let mut body = json!({"contents": contents, "generationConfig": generation_config});
    if !declarations.is_empty() {
        body["tools"] = json!([{"functionDeclarations": declarations}]);
        body["toolConfig"] = json!({"functionCallingConfig": {"mode": "AUTO"}});
    }
    body
}

fn clean_gemini_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    *key != "additionalProperties" && *key != "examples" && *key != "$schema"
                })
                .map(|(key, value)| (key.clone(), clean_gemini_schema(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(clean_gemini_schema).collect()),
        _ => value.clone(),
    }
}

fn parse_openai(value: &Value) -> Result<ProviderResponse> {
    let message = value
        .pointer("/choices/0/message")
        .ok_or_else(|| anyhow!("Malformed OpenAI-compatible response: missing choices[0].message"))?;
    let mut calls = Vec::new();
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let arguments = call
                .pointer("/function/arguments")
                .ok_or_else(|| anyhow!("Malformed tool call: missing arguments"))?;
            let input = if let Some(arguments) = arguments.as_str() {
                serde_json::from_str(arguments).context("Malformed tool call arguments")?
            } else {
                arguments.clone()
            };
            calls.push(ToolCall {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .context("Malformed tool call: missing id")?
                    .into(),
                name: call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .context("Malformed tool call: missing name")?
                    .into(),
                input,
            });
        }
    }
    let usage = value.get("usage");
    Ok(ProviderResponse {
        text: message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        calls,
        usage: TokenUsage {
            input_tokens: usage
                .and_then(|usage| usage.get("prompt_tokens").or_else(|| usage.get("input_tokens")))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .and_then(|usage| usage.get("completion_tokens").or_else(|| usage.get("output_tokens")))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        },
    })
}

fn parse_anthropic(value: &Value) -> Result<ProviderResponse> {
    let parts = value
        .get("content")
        .and_then(Value::as_array)
        .context("Malformed Anthropic response: missing content array")?;
    let mut text = Vec::new();
    let mut calls = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => text.push(
                part.get("text")
                    .and_then(Value::as_str)
                    .context("Malformed Anthropic text block")?
                    .to_string(),
            ),
            Some("tool_use") => calls.push(ToolCall {
                id: part.get("id").and_then(Value::as_str).context("Malformed Anthropic tool id")?.into(),
                name: part.get("name").and_then(Value::as_str).context("Malformed Anthropic tool name")?.into(),
                input: part.get("input").cloned().unwrap_or(json!({})),
            }),
            _ => {}
        }
    }
    let usage = value.get("usage");
    Ok(ProviderResponse {
        text: text.join("\n"),
        calls,
        usage: TokenUsage {
            input_tokens: usage.and_then(|usage| usage.get("input_tokens")).and_then(Value::as_u64).unwrap_or(0),
            output_tokens: usage.and_then(|usage| usage.get("output_tokens")).and_then(Value::as_u64).unwrap_or(0),
        },
    })
}

fn parse_gemini(value: &Value) -> Result<ProviderResponse> {
    let parts = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .context("Malformed Gemini response: missing candidates[0].content.parts")?;
    let mut text = Vec::new();
    let mut calls = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if let Some(part_text) = part.get("text").and_then(Value::as_str) {
            text.push(part_text.to_string());
        }
        if let Some(function) = part.get("functionCall") {
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .context("Malformed Gemini function call name")?;
            calls.push(ToolCall {
                id: format!("gemini_{index}_{name}"),
                name: name.into(),
                input: function.get("args").cloned().unwrap_or(json!({})),
            });
        }
    }
    let usage = value.get("usageMetadata");
    Ok(ProviderResponse {
        text: text.join("\n"),
        calls,
        usage: TokenUsage {
            input_tokens: usage.and_then(|usage| usage.get("promptTokenCount")).and_then(Value::as_u64).unwrap_or(0),
            output_tokens: usage.and_then(|usage| usage.get("candidatesTokenCount")).and_then(Value::as_u64).unwrap_or(0),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_tool_call_and_fallback_usage() {
        let response = parse_openai(&json!({
            "choices": [{"message": {
                "content": "I will inspect it.",
                "tool_calls": [{"id": "1", "function": {
                    "name": "read_file",
                    "arguments": "{\"path\":\"a\"}"
                }}]
            }}],
            "usage": {"input_tokens": 2, "output_tokens": 3}
        }))
        .unwrap();
        assert_eq!(response.text, "I will inspect it.");
        assert_eq!(response.calls[0].input["path"], "a");
        assert_eq!(response.usage.input_tokens, 2);
        assert_eq!(response.usage.output_tokens, 3);
    }

    #[test]
    fn openai_messages_preserve_text_with_tool_calls() {
        let messages = openai_messages(&[Message {
            role: MessageRole::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text { text: "Looking now".into() },
                ContentBlock::ToolUse { id: "1".into(), name: "read".into(), input: json!({"path": "a"}) },
            ]),
        }]);
        assert_eq!(messages[0]["content"], "Looking now");
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "read");
    }

    #[test]
    fn parses_anthropic_mixed_content() {
        let response = parse_anthropic(&json!({"content": [
            {"type": "text", "text": "thinking"},
            {"type": "tool_use", "id": "x", "name": "grep", "input": {"q": "x"}}
        ]})).unwrap();
        assert_eq!(response.text, "thinking");
        assert_eq!(response.calls[0].name, "grep");
    }

    #[test]
    fn parses_gemini_function_call() {
        let response = parse_gemini(&json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "list", "args": {}}}
        ]}}]})).unwrap();
        assert_eq!(response.calls[0].name, "list");
        assert_eq!(response.calls[0].id, "gemini_0_list");
    }

    #[test]
    fn normalizes_gemini_model_aliases() {
        assert_eq!(normalize_gemini_model("flash"), "gemini-1.5-flash");
        assert_eq!(normalize_gemini_model("pro"), "gemini-1.5-pro");
        assert_eq!(normalize_gemini_model("1.5-flash"), "gemini-1.5-flash");
        assert_eq!(normalize_gemini_model("1.5-pro"), "gemini-1.5-pro");
        assert_eq!(normalize_gemini_model("2.0-flash"), "gemini-2.0-flash");
        assert_eq!(normalize_gemini_model("gemini-2.5-pro"), "gemini-2.5-pro");
    }

    #[test]
    fn gemini_generation_config_includes_temperature() {
        let config = LlmConfig {
            provider: LlmProvider::Gemini,
            model: "flash".into(),
            prompt: String::new(),
            parameters: json!({"max_tokens": 100, "temperature": 0.25}),
            api_key: None,
        };
        let body = gemini_body(&config, &[Message::user("hello")], &[]);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 100);
        assert_eq!(body["generationConfig"]["temperature"], 0.25);
    }

    #[test]
    fn malformed_response_is_structured_error() {
        assert!(parse_openai(&json!({})).unwrap_err().to_string().contains("missing choices"));
    }
}
