use std::collections::{BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::llm::{AgentV2LlmClient, ExecutionPurpose, LlmConfig, LlmProvider, Message, Tool, ToolRegistry};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::warn;

use crate::config::Config;

const MAX_GUIDANCE_FILES: usize = 10;
const MAX_GUIDANCE_FILE_CHARS: usize = 4_000;
const SHELL_TIMEOUT_SECONDS: u64 = 30;

type ToolUseCallback = fn(&str, bool, &str, Option<&serde_json::Value>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTaskMode {
    ReadOnly,
    Mutating,
}

#[derive(Debug, Clone)]
struct AgentExecutionConfig {
    provider: LlmProvider,
    provider_name: String,
    model: String,
    api_key: String,
    timeout_seconds: u64,
    max_iterations: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepoSummary {
    top_level_entries: Vec<String>,
    key_files: Vec<String>,
    frameworks: Vec<String>,
    package_managers: Vec<String>,
    file_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GuidanceExcerpt {
    path: String,
    excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepoUnderstandingSeed {
    repo_root: String,
    repo_summary: RepoSummary,
    guidance_files: Vec<GuidanceExcerpt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepoUnderstandingCacheEntry {
    provider: String,
    model: String,
    summary: String,
    seed: RepoUnderstandingSeed,
}

impl AgentExecutionConfig {
    fn from_config(config: &Config, purpose: Option<&str>) -> Result<Self, String> {
        // Resolve (provider, model) for this purpose. Phase override wins,
        // otherwise the global pair is used.
        let (resolved_provider, resolved_model) = match purpose {
            Some(p) => config.agent_for(p),
            None => (config.agent_provider.as_str(), config.agent_model.as_str()),
        };
        let provider_name = resolved_provider.trim().to_lowercase();
        let provider = match provider_name.as_str() {
            "anthropic" => LlmProvider::Anthropic,
            "openai" => LlmProvider::Openai,
            "mistral" => LlmProvider::Mistral,
            "gemini" => LlmProvider::Gemini,
            other => {
                return Err(format!(
                    "Unsupported orchestrator agent provider `{other}`. Supported providers: anthropic, openai, mistral, gemini"
                ))
            }
        };

        let model = if resolved_model.trim().is_empty() {
            default_model_for_provider(&provider_name).to_string()
        } else {
            resolved_model.trim().to_string()
        };

        // API key: the global `agent_api_key` only applies when the resolved
        // provider matches the global provider. Phase overrides that switch
        // providers must bring their own key via the env.
        let global_provider = config.agent_provider.trim().to_lowercase();
        let api_key = if provider_name == global_provider && !config.agent_api_key.trim().is_empty()
        {
            config.agent_api_key.trim().to_string()
        } else {
            provider_api_key_from_env(&provider_name).ok_or_else(|| {
                format!(
                    "No API key configured for orchestrator agent provider `{}`. Set {}",
                    provider_name,
                    provider_env_var(&provider_name)
                )
            })?
        };

        Ok(Self {
            provider,
            provider_name,
            model,
            api_key,
            timeout_seconds: config.agent_timeout_seconds,
            max_iterations: config.agent_max_iterations,
        })
    }

    fn llm_config(&self) -> LlmConfig {
        // Intentionally no `temperature` / `top_p`: reasoning-model families
        // (Claude Opus 4.7+, GPT-5 codex, etc.) reject them as deprecated,
        // and the remaining providers' defaults are fine for our agent
        // workloads. Models that need determinism use structured-output
        // validation rather than temperature 0.
        //
        // max_tokens: 16k is sized for a multi-sandbox deploy plan — 4-sandbox
        // topologies with assumptions/risks/verification/setupHooks blocks
        // were truncating at the old 4k limit and failing JSON parse.
        LlmConfig {
            provider: self.provider.clone(),
            model: self.model.clone(),
            prompt: String::new(),
            parameters: json!({
                "max_tokens": 16000,
            }),
            api_key: Some(self.api_key.clone()),
        }
    }

    fn repo_summary_llm_config(&self) -> LlmConfig {
        LlmConfig {
            provider: self.provider.clone(),
            model: self.model.clone(),
            prompt: String::new(),
            parameters: json!({
                "max_tokens": 1400,
            }),
            api_key: Some(self.api_key.clone()),
        }
    }
}

pub async fn run_repo_agent(
    config: &Config,
    source_repo_root: &Path,
    sandbox_repo_root: &Path,
    prompt: &str,
    mode: AgentTaskMode,
    purpose: Option<&str>,
) -> Result<String, String> {
    let execution = AgentExecutionConfig::from_config(config, purpose)?;
    // The repo-understanding summary is a one-shot prose memo shared across
    // every downstream phase; route it to the `discovery` slot (Mistral by
    // default) so we pay the cheap-tier price once and reuse the result for
    // planning/review/patch, which still run on the global provider (Opus).
    let summary_execution = AgentExecutionConfig::from_config(config, Some("discovery"))?;
    let cached_repo_understanding =
        load_or_create_repo_understanding(&summary_execution, source_repo_root).await?;

    let mut tool_registry = ToolRegistry::new();
    let sandbox = RepoSandbox::new(sandbox_repo_root.to_path_buf());
    for tool in sandbox.tools(mode) {
        tool_registry.register(tool);
    }

    let full_prompt = build_execution_prompt(prompt, &cached_repo_understanding, mode);
    let llm_client = AgentV2LlmClient::new();
    let messages = vec![Message::user(full_prompt)];
    let no_callback: Option<ToolUseCallback> = None;
    let purpose = Some(match mode {
        AgentTaskMode::ReadOnly => ExecutionPurpose::ToolUse,
        AgentTaskMode::Mutating => ExecutionPurpose::CodeGeneration,
    });

    let result = timeout(
        Duration::from_secs(execution.timeout_seconds),
        llm_client.execute_with_tools(
            &execution.llm_config(),
            messages,
            &tool_registry,
            execution.max_iterations,
            no_callback,
            None,
            purpose,
            None,
        ),
    )
    .await
    .map_err(|_| {
        format!(
            "Orchestrator agent timed out after {} seconds",
            execution.timeout_seconds
        )
    })
    .and_then(|result| result.map_err(|error| error.to_string()))?;

    Ok(result.0.trim().to_string())
}

async fn load_or_create_repo_understanding(
    execution: &AgentExecutionConfig,
    repo_root: &Path,
) -> Result<String, String> {
    let seed = build_repo_understanding_seed(repo_root)?;
    let cache_key = repo_understanding_cache_key(&seed)?;
    let cache_path = repo_understanding_cache_path(&cache_key)?;

    if let Ok(contents) = tokio::fs::read_to_string(&cache_path).await {
        if let Ok(entry) = serde_json::from_str::<RepoUnderstandingCacheEntry>(&contents) {
            if entry.provider == execution.provider_name && entry.model == execution.model {
                return Ok(entry.summary);
            }
        }
    }

    let prompt = build_repo_understanding_prompt(&seed);
    let llm_client = AgentV2LlmClient::new();
    let summary = timeout(
        Duration::from_secs(execution.timeout_seconds.min(180)),
        llm_client.execute(
            &execution.repo_summary_llm_config(),
            &prompt,
            Some(ExecutionPurpose::PlanGeneration),
        ),
    )
    .await
    .map_err(|_| "Timed out while generating cached repo understanding".to_string())
    .and_then(|result| result.map_err(|error| error.to_string()))?
    .trim()
    .to_string();

    let entry = RepoUnderstandingCacheEntry {
        provider: execution.provider_name.clone(),
        model: execution.model.clone(),
        summary: summary.clone(),
        seed,
    };
    let serialized = serde_json::to_string_pretty(&entry)
        .map_err(|error| format!("Failed to serialize repo understanding cache: {error}"))?;

    if let Some(parent) = cache_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            format!(
                "Failed to create repo understanding cache directory {}: {error}",
                parent.display()
            )
        })?;
    }
    tokio::fs::write(&cache_path, serialized)
        .await
        .map_err(|error| {
            format!(
                "Failed to write repo understanding cache {}: {error}",
                cache_path.display()
            )
        })?;

    Ok(summary)
}

fn build_execution_prompt(
    prompt: &str,
    cached_repo_understanding: &str,
    mode: AgentTaskMode,
) -> String {
    let mut full_prompt = String::new();
    full_prompt.push_str(
        "Use the available repository tools instead of relying on memory. Start from the cached repo understanding below, but verify any important claim against the repository before you conclude. Prefer `rg`, `sed`, `cat`, `git`, and other read-oriented commands through the shell tool when they are the fastest path.\n\n",
    );

    match mode {
        AgentTaskMode::ReadOnly => {
            full_prompt.push_str(
                "This is a read-only analysis task. Inspect the repository and return the requested answer, but do not intentionally modify files.\n\n",
            );
        }
        AgentTaskMode::Mutating => {
            full_prompt.push_str(
                "This is a repository mutation task. Keep edits minimal, leave changes in the working tree, and summarize what you changed in the final answer.\n\n",
            );
        }
    }

    full_prompt.push_str("Cached repo understanding:\n");
    full_prompt.push_str(cached_repo_understanding.trim());
    full_prompt.push_str("\n\nTask:\n");
    full_prompt.push_str(prompt.trim());
    full_prompt.push('\n');
    full_prompt
}

fn build_repo_understanding_prompt(seed: &RepoUnderstandingSeed) -> String {
    let mut prompt = String::new();
    prompt.push_str("Create a durable repository-understanding memo for future agent runs. Base it only on the provided repo inventory and file excerpts. Be concise, factual, and useful for deployment planning and code changes.\n\n");
    prompt.push_str("Return markdown with these sections:\n");
    prompt.push_str("- Repo Shape\n");
    prompt.push_str("- Key Services And Entry Points\n");
    prompt.push_str("- Build Test And Run Signals\n");
    prompt.push_str("- Deployment-Relevant Clues\n");
    prompt.push_str("- Files Future Agents Should Read First\n");
    prompt.push_str("- Risks And Caveats\n\n");
    prompt.push_str("Avoid speculation. If something is uncertain, say so briefly. Keep the memo under 900 words.\n\n");
    prompt.push_str("Repository inventory and excerpts:\n");
    prompt.push_str(
        &serde_json::to_string_pretty(seed)
            .unwrap_or_else(|_| "{\"error\":\"failed to serialize seed\"}".to_string()),
    );
    prompt.push('\n');
    prompt
}

fn build_repo_understanding_seed(repo_root: &Path) -> Result<RepoUnderstandingSeed, String> {
    let canonical_root = repo_root
        .canonicalize()
        .map_err(|error| format!("Failed to canonicalize {}: {error}", repo_root.display()))?;
    let repo_summary = summarize_repo(&canonical_root)?;
    let guidance_files = collect_guidance_files(&canonical_root, &repo_summary)?;

    Ok(RepoUnderstandingSeed {
        repo_root: canonical_root.display().to_string(),
        repo_summary,
        guidance_files,
    })
}

fn repo_understanding_cache_key(seed: &RepoUnderstandingSeed) -> Result<String, String> {
    let serialized = serde_json::to_vec(seed)
        .map_err(|error| format!("Failed to serialize repo understanding seed: {error}"))?;
    let mut hasher = Sha256::new();
    hasher.update(serialized);
    Ok(format!("{:x}", hasher.finalize()))
}

fn repo_understanding_cache_path(cache_key: &str) -> Result<PathBuf, String> {
    let base = dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".cache")))
        .ok_or_else(|| "Failed to resolve cache directory for orchestrator agent".to_string())?;
    Ok(base
        .join("heyo")
        .join("orchestrator")
        .join("repo-understanding")
        .join(format!("{cache_key}.json")))
}

fn collect_guidance_files(
    repo_root: &Path,
    repo_summary: &RepoSummary,
) -> Result<Vec<GuidanceExcerpt>, String> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    for path in [
        "AGENTS.md",
        "README.md",
        "ENVIRONMENT_VARIABLES.md",
        ".env.example",
        "docker-compose.yml",
        "docker-compose.yaml",
        "docker-compose.dev.yml",
        "docker-compose.dev.yaml",
    ] {
        push_candidate(repo_root, path, &mut candidates, &mut seen);
    }

    for path in find_nested_files(repo_root, "AGENTS.md", 3, 4)? {
        push_candidate(repo_root, &path, &mut candidates, &mut seen);
    }

    for path in &repo_summary.key_files {
        push_candidate(repo_root, path, &mut candidates, &mut seen);
    }

    let mut guidance = Vec::new();
    for relative_path in candidates.into_iter().take(MAX_GUIDANCE_FILES) {
        let full_path = repo_root.join(&relative_path);
        match std::fs::read_to_string(&full_path) {
            Ok(contents) => guidance.push(GuidanceExcerpt {
                path: relative_path,
                excerpt: truncate_chars(&contents, MAX_GUIDANCE_FILE_CHARS),
            }),
            Err(error) => warn!(
                "Failed to read guidance file {} for repo-understanding cache: {}",
                full_path.display(),
                error
            ),
        }
    }

    Ok(guidance)
}

fn push_candidate(
    repo_root: &Path,
    relative_path: &str,
    candidates: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let normalized = relative_path.replace('\\', "/");
    if normalized.is_empty() || !repo_root.join(&normalized).is_file() {
        return;
    }
    if seen.insert(normalized.clone()) {
        candidates.push(normalized);
    }
}

fn find_nested_files(
    repo_root: &Path,
    file_name: &str,
    max_depth: usize,
    max_results: usize,
) -> Result<Vec<String>, String> {
    let mut results = Vec::new();
    let mut queue = VecDeque::from([(repo_root.to_path_buf(), 0usize)]);

    while let Some((directory, depth)) = queue.pop_front() {
        if results.len() >= max_results {
            break;
        }
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("Failed to read {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("Failed to inspect repo entry: {error}"))?;
            let path = entry.path();
            let entry_name = entry.file_name().to_string_lossy().to_string();
            if entry_name == ".git" || entry_name == "node_modules" || entry_name == "target" {
                continue;
            }
            if path.is_dir() {
                if depth < max_depth {
                    queue.push_back((path, depth + 1));
                }
                continue;
            }
            if entry_name == file_name {
                if let Ok(relative) = path.strip_prefix(repo_root) {
                    results.push(relative.to_string_lossy().replace('\\', "/"));
                }
                if results.len() >= max_results {
                    break;
                }
            }
        }
    }

    Ok(results)
}

fn summarize_repo(repo_root: &Path) -> Result<RepoSummary, String> {
    let ignored = [".git", "node_modules", "target", ".next", "dist", "build"];
    let mut top_level_entries = BTreeSet::new();
    let mut file_paths = Vec::new();
    let mut directories = vec![repo_root.to_path_buf()];

    while let Some(directory) = directories.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("Failed to read {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("Failed to read repo entry: {error}"))?;
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().to_string();
            if ignored.iter().any(|ignored| ignored == &file_name) {
                continue;
            }

            let relative = path
                .strip_prefix(repo_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");

            if directory == repo_root {
                top_level_entries.insert(relative.clone());
            }

            if path.is_dir() {
                directories.push(path);
                continue;
            }

            file_paths.push(relative);
            if file_paths.len() >= 500 {
                break;
            }
        }
        if file_paths.len() >= 500 {
            break;
        }
    }

    let key_files = prioritized_key_files(&file_paths);
    let frameworks = detect_frameworks(&file_paths);
    let package_managers = detect_package_managers(&file_paths);

    Ok(RepoSummary {
        top_level_entries: top_level_entries.into_iter().take(20).collect(),
        key_files,
        frameworks,
        package_managers,
        file_count: file_paths.len(),
    })
}

fn prioritized_key_files(files: &[String]) -> Vec<String> {
    let important = [
        "AGENTS.md",
        "README.md",
        "Cargo.toml",
        "package.json",
        "bun.lock",
        "bun.lockb",
        "pnpm-lock.yaml",
        "package-lock.json",
        "yarn.lock",
        "docker-compose.yml",
        "docker-compose.yaml",
        ".env.example",
        "src/main.rs",
        "src/main.ts",
        "src/main.js",
    ];

    let mut priority = Vec::new();
    for name in important {
        if let Some(found) = files.iter().find(|file| file.ends_with(name)) {
            priority.push(found.clone());
        }
    }
    priority.truncate(20);
    priority
}

fn detect_frameworks(files: &[String]) -> Vec<String> {
    let mut frameworks = BTreeSet::new();
    if files.iter().any(|file| file == "Cargo.toml") {
        frameworks.insert("rust".to_string());
    }
    if files.iter().any(|file| file == "package.json") {
        frameworks.insert("node".to_string());
    }
    if files
        .iter()
        .any(|file| file == "svelte.config.js" || file == "svelte.config.ts")
    {
        frameworks.insert("sveltekit".to_string());
    }
    if files
        .iter()
        .any(|file| file == "next.config.js" || file == "next.config.mjs")
    {
        frameworks.insert("nextjs".to_string());
    }
    if files.iter().any(|file| file.ends_with("tauri.conf.json")) {
        frameworks.insert("tauri".to_string());
    }
    frameworks.into_iter().collect()
}

fn detect_package_managers(files: &[String]) -> Vec<String> {
    let mut managers = BTreeSet::new();
    if files
        .iter()
        .any(|file| file == "bun.lock" || file == "bun.lockb")
    {
        managers.insert("bun".to_string());
    }
    if files.iter().any(|file| file == "pnpm-lock.yaml") {
        managers.insert("pnpm".to_string());
    }
    if files.iter().any(|file| file == "yarn.lock") {
        managers.insert("yarn".to_string());
    }
    if files.iter().any(|file| file == "package-lock.json") {
        managers.insert("npm".to_string());
    }
    managers.into_iter().collect()
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let truncated: String = input.chars().take(max_chars).collect();
    format!("{}\n... [truncated]", truncated)
}

/// Each provider's default is that provider's current flagship — the
/// "best" tier the user is paying for:
///   - Anthropic: Opus 4.7 (Claude Max 5 plan)
///   - OpenAI:    codex (GPT-5 coding-tuned model for Amp-like work)
///   - Mistral:   mistral-large-latest (flagship alias)
///   - Gemini:    gemini-1.5-pro
/// Cost control comes from phase-specific overrides (see `Config::agent_for`),
/// not from downgrading the default. For example, discovery defaults to
/// Mistral so a cheap provider handles repo scans while planning/patch stay
/// on Opus 4.7.
fn default_model_for_provider(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "claude-opus-4-7",
        "openai" => "gpt-5.3-codex",
        "mistral" => "mistral-large-latest",
        "gemini" => "gemini-1.5-pro",
        _ => "claude-opus-4-7",
    }
}

fn provider_env_var(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "gemini" => "GOOGLE_API_KEY",
        _ => "API_KEY",
    }
}

fn provider_api_key_from_env(provider: &str) -> Option<String> {
    std::env::var(provider_env_var(provider)).ok()
}

#[derive(Clone)]
struct RepoSandbox {
    root: PathBuf,
}

impl RepoSandbox {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn tools(&self, mode: AgentTaskMode) -> Vec<std::sync::Arc<dyn Tool>> {
        let mut tools: Vec<std::sync::Arc<dyn Tool>> = vec![
            std::sync::Arc::new(ExecuteShellTool::new(self.root.clone())),
            std::sync::Arc::new(ReadFileTool::new(self.root.clone())),
            std::sync::Arc::new(ListFilesTool::new(self.root.clone())),
            std::sync::Arc::new(GrepTool::new(self.root.clone())),
        ];

        if mode == AgentTaskMode::Mutating {
            tools.push(std::sync::Arc::new(WriteFileTool::new(self.root.clone())));
            tools.push(std::sync::Arc::new(EditFileTool::new(self.root.clone())));
        }

        tools
    }
}

fn sanitize_relative_path(path: &str) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("Path cannot be empty"));
    }
    if trimmed.starts_with('/') || trimmed.contains("..") {
        return Err(anyhow::anyhow!(
            "Invalid path `{}`: paths must be relative and cannot contain `..`",
            path
        ));
    }

    Ok(trimmed.replace('\\', "/"))
}

struct ExecuteShellTool {
    root: PathBuf,
}

impl ExecuteShellTool {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for ExecuteShellTool {
    fn name(&self) -> &str {
        "execute_shell"
    }

    fn description(&self) -> &str {
        "Execute a non-interactive shell command in the repository root. Prefer this for fast repo inspection with rg, git, sed, cat, ls, find, cargo metadata, or similar command-line tools. Returns stdout, stderr, and exit_code."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute from the repository root"
                }
            },
            "required": ["command"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, arguments: Value) -> Result<Value> {
        let command = arguments["command"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing `command`"))?;

        let mut child = Command::new("sh");
        child
            .arg("-lc")
            .arg(command)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = timeout(Duration::from_secs(SHELL_TIMEOUT_SECONDS), child.output())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Shell command timed out after {} seconds",
                    SHELL_TIMEOUT_SECONDS
                )
            })??;

        Ok(json!({
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "exit_code": output.status.code().unwrap_or(-1),
        }))
    }
}

struct ReadFileTool {
    root: PathBuf,
}

impl ReadFileTool {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file from the repository. Use this when you need precise file contents after locating the right path with list_files, grep, or execute_shell."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path to the file"
                }
            },
            "required": ["path"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, arguments: Value) -> Result<Value> {
        let path = sanitize_relative_path(
            arguments["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing `path`"))?,
        )?;
        let full_path = self.root.join(&path);
        let content = tokio::fs::read_to_string(&full_path)
            .await
            .map_err(|error| anyhow::anyhow!("Failed to read {}: {}", path, error))?;

        Ok(json!({
            "path": path,
            "content": content,
        }))
    }
}

struct ListFilesTool {
    root: PathBuf,
}

impl ListFilesTool {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "List files or directories within the repository. Use this to inspect a directory tree before reading specific files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative directory path. Use '.' for repository root.",
                    "default": "."
                },
                "recursive": {
                    "type": "boolean",
                    "description": "Whether to recurse into subdirectories",
                    "default": false
                },
                "max_entries": {
                    "type": "integer",
                    "description": "Maximum number of entries to return",
                    "default": 100
                }
            },
            "additionalProperties": false,
        })
    }

    async fn execute(&self, arguments: Value) -> Result<Value> {
        let path_arg = arguments["path"].as_str().unwrap_or(".");
        let recursive = arguments["recursive"].as_bool().unwrap_or(false);
        let max_entries = arguments["max_entries"].as_u64().unwrap_or(100).min(500) as usize;
        let normalized = if path_arg == "." || path_arg.is_empty() {
            ".".to_string()
        } else {
            sanitize_relative_path(path_arg)?
        };
        let base_path = if normalized == "." {
            self.root.clone()
        } else {
            self.root.join(&normalized)
        };

        let mut entries = Vec::new();
        if recursive {
            let mut queue = VecDeque::from([base_path.clone()]);
            while let Some(directory) = queue.pop_front() {
                for entry in std::fs::read_dir(&directory)? {
                    let entry = entry?;
                    let path = entry.path();
                    let relative = path
                        .strip_prefix(&self.root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let metadata = entry.metadata()?;
                    entries.push(json!({
                        "path": relative,
                        "is_dir": metadata.is_dir(),
                        "size": metadata.len(),
                    }));
                    if metadata.is_dir() {
                        queue.push_back(path);
                    }
                    if entries.len() >= max_entries {
                        break;
                    }
                }
                if entries.len() >= max_entries {
                    break;
                }
            }
        } else {
            for entry in std::fs::read_dir(&base_path)? {
                let entry = entry?;
                let path = entry.path();
                let metadata = entry.metadata()?;
                let relative = path
                    .strip_prefix(&self.root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                entries.push(json!({
                    "path": relative,
                    "is_dir": metadata.is_dir(),
                    "size": metadata.len(),
                }));
                if entries.len() >= max_entries {
                    break;
                }
            }
        }

        Ok(json!({
            "path": normalized,
            "entries": entries,
            "truncated": entries.len() >= max_entries,
        }))
    }
}

struct GrepTool {
    root: PathBuf,
}

impl GrepTool {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search repository text with ripgrep. Returns matching file paths, line numbers, and lines. Prefer this before reading files when you know a pattern or symbol."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Optional relative directory or file path to scope the search"
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "Whether the search should be case-sensitive",
                    "default": true
                }
            },
            "required": ["pattern"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, arguments: Value) -> Result<Value> {
        let pattern = arguments["pattern"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing `pattern`"))?;
        let case_sensitive = arguments["case_sensitive"].as_bool().unwrap_or(true);
        let path = arguments["path"].as_str().unwrap_or(".");
        let scope = if path == "." || path.is_empty() {
            ".".to_string()
        } else {
            sanitize_relative_path(path)?
        };

        let mut command = Command::new("rg");
        command.arg("-n").arg("--no-heading");
        if !case_sensitive {
            command.arg("-i");
        }
        command.arg(pattern).arg(&scope);
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = timeout(Duration::from_secs(SHELL_TIMEOUT_SECONDS), command.output())
            .await
            .map_err(|_| {
                anyhow::anyhow!("grep timed out after {} seconds", SHELL_TIMEOUT_SECONDS)
            })??;

        Ok(json!({
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "exit_code": output.status.code().unwrap_or(-1),
        }))
    }
}

struct WriteFileTool {
    root: PathBuf,
}

impl WriteFileTool {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or replace a text file in the repository working tree. Use for full-file writes when editing directly is simpler than shell redirection."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, arguments: Value) -> Result<Value> {
        let path = sanitize_relative_path(
            arguments["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing `path`"))?,
        )?;
        let content = arguments["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing `content`"))?;
        let full_path = self.root.join(&path);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full_path, content).await?;
        Ok(json!({ "path": path, "size": content.len() }))
    }
}

struct EditFileTool {
    root: PathBuf,
}

impl EditFileTool {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace or append text in an existing file. Use mode=replace to overwrite the file or mode=append to append content."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" },
                "mode": {
                    "type": "string",
                    "enum": ["replace", "append"],
                    "default": "replace"
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, arguments: Value) -> Result<Value> {
        let path = sanitize_relative_path(
            arguments["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing `path`"))?,
        )?;
        let content = arguments["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing `content`"))?;
        let mode = arguments["mode"].as_str().unwrap_or("replace");
        let full_path = self.root.join(&path);

        if mode == "append" {
            use tokio::io::AsyncWriteExt;

            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&full_path)
                .await
                .map_err(|error| {
                    anyhow::anyhow!("Failed to open {} for append: {}", path, error)
                })?;
            file.write_all(content.as_bytes()).await?;
        } else {
            tokio::fs::write(&full_path, content).await?;
        }

        Ok(json!({ "path": path, "mode": mode, "size": content.len() }))
    }
}
