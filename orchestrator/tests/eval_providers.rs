//! Eval harness — compares providers on the same prompt.
//!
//! Run with API keys in env:
//!   ANTHROPIC_API_KEY=... OPENAI_API_KEY=... MISTRAL_API_KEY=... GOOGLE_API_KEY=... \
//!     cargo test --test eval_providers -- --ignored --nocapture
//!
//! Each provider is skipped if its API key is not set, so the harness is
//! usable with any subset of providers. Output goes to stdout as a
//! per-fixture comparison block (`cargo test -- --nocapture`).
//!
//! Design: each case is `(fixture_name, purpose, prompt, assertions...)`.
//! Purpose maps to per-phase model overrides; assertions are plain closures
//! so callers can check structural properties (valid JSON, substring match,
//! minimum length) without coupling to one grading style.

use serde_json::json;
use std::time::Instant;

#[path = "../src/llm.rs"]
mod llm;
use llm::{AgentV2LlmClient, LlmConfig, LlmProvider};

/// One provider to test, plus the model it should use.
struct ProviderCase {
    name: &'static str,
    provider: LlmProvider,
    env_key: &'static str,
    default_model: &'static str,
}

fn providers_to_test() -> Vec<ProviderCase> {
    vec![
        ProviderCase {
            name: "anthropic",
            provider: LlmProvider::Anthropic,
            env_key: "ANTHROPIC_API_KEY",
            default_model: "claude-haiku-4-5-20251001",
        },
        ProviderCase {
            name: "openai",
            provider: LlmProvider::Openai,
            env_key: "OPENAI_API_KEY",
            default_model: "gpt-4o-mini",
        },
        ProviderCase {
            name: "mistral",
            provider: LlmProvider::Mistral,
            env_key: "MISTRAL_API_KEY",
            default_model: "mistral-small-latest",
        },
        ProviderCase {
            name: "gemini",
            provider: LlmProvider::Gemini,
            env_key: "GOOGLE_API_KEY",
            default_model: "gemini-1.5-flash",
        },
    ]
}

/// Build a minimal LlmConfig for a provider case.
fn build_config(case: &ProviderCase) -> Option<LlmConfig> {
    let api_key = std::env::var(case.env_key).ok()?;
    if api_key.trim().is_empty() {
        return None;
    }
    Some(LlmConfig {
        provider: case.provider.clone(),
        model: case.default_model.to_string(),
        prompt: String::new(),
        parameters: json!({
            "temperature": 0.0,
            "max_tokens": 300,
        }),
        api_key: Some(api_key),
    })
}

/// Call each provider with `prompt` and pass the output to `assert_fn` for
/// structural checks. Prints a comparison block per fixture to stdout.
/// Does NOT fail the test if a provider is absent — only if a present
/// provider's output fails its assertion.
async fn run_fixture(
    fixture_name: &str,
    prompt: &str,
    assert_fn: impl Fn(&str) -> Result<(), String>,
) {
    println!("\n========== fixture: {} ==========", fixture_name);
    println!("prompt: {}\n", prompt);

    let client = AgentV2LlmClient::new();
    for case in providers_to_test() {
        let Some(config) = build_config(&case) else {
            println!("[{:<12}] SKIP — {} not set", case.name, case.env_key);
            continue;
        };
        let t0 = Instant::now();
        match client.execute(&config, prompt, None).await {
            Ok(output) => {
                let elapsed_ms = t0.elapsed().as_millis();
                let preview: String = output.chars().take(200).collect();
                let ellipsis = if output.chars().count() > 200 {
                    "…"
                } else {
                    ""
                };
                match assert_fn(&output) {
                    Ok(()) => println!(
                        "[{:<12}] OK    ({:>5} ms, {:>5} chars) {}{}",
                        case.name,
                        elapsed_ms,
                        output.len(),
                        preview,
                        ellipsis
                    ),
                    Err(reason) => panic!(
                        "[{}] FAIL assertion on fixture `{}`: {}\nfull output:\n{}",
                        case.name, fixture_name, reason, output
                    ),
                }
            }
            Err(e) => println!(
                "[{:<12}] ERROR ({:>5} ms) {}",
                case.name,
                t0.elapsed().as_millis(),
                e
            ),
        }
    }
}

/// Fixture 1 — structured JSON. Every provider must return a valid JSON
/// object. Catches broken output formatting.
#[tokio::test]
#[ignore] // requires live API keys; run with `--ignored`
async fn eval_structured_json() {
    run_fixture(
        "structured_json",
        r#"Return ONLY a JSON object with exactly these keys: "service" (string), "port" (integer between 1 and 65535), "replicas" (integer >= 1). Example: {"service":"web","port":3000,"replicas":2}. Do not include any prose or markdown."#,
        |output| {
            let trimmed = output.trim();
            let json: serde_json::Value = serde_json::from_str(trimmed)
                .map_err(|e| format!("not valid JSON: {} — raw: {}", e, trimmed))?;
            if !json.is_object() {
                return Err(format!("expected object, got {}", json));
            }
            for k in ["service", "port", "replicas"] {
                if json.get(k).is_none() {
                    return Err(format!("missing key `{k}`"));
                }
            }
            Ok(())
        },
    )
    .await;
}

/// Fixture 2 — instruction following. Catches providers that ignore explicit
/// constraints or pad with boilerplate.
#[tokio::test]
#[ignore]
async fn eval_instruction_following() {
    run_fixture(
        "instruction_following",
        "Answer with exactly one word, all lowercase, no punctuation: what is the keyword in Rust for declaring an immutable binding?",
        |output| {
            let answer = output.trim().trim_end_matches('.').to_lowercase();
            if answer == "let" {
                Ok(())
            } else {
                Err(format!("expected `let`, got `{}`", answer))
            }
        },
    )
    .await;
}

/// Fixture 3 — brief prose for eyeballing voice/tone differences between
/// providers. Only asserts non-trivial output length.
#[tokio::test]
#[ignore]
async fn eval_brief_prose() {
    run_fixture(
        "brief_prose",
        "In under 80 words, explain what an LLM adapter abstraction is in an orchestrator — aimed at a senior engineer who has never written one.",
        |output| {
            let trimmed = output.trim();
            if trimmed.len() < 80 {
                return Err(format!("response too short ({} chars)", trimmed.len()));
            }
            Ok(())
        },
    )
    .await;
}
