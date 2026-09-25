use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::client::{HeyoClient, RequestOptions};
use crate::errors::HeyoError;
use crate::types::{CommandResult, CommandRunOptions};

/// Command-execution surface, obtained via [`crate::Sandbox::commands`].
#[derive(Clone)]
pub struct Commands {
    client: HeyoClient,
    sandbox_id: String,
}

#[derive(Serialize)]
struct ExecRequest<'a> {
    command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<&'a std::collections::HashMap<String, String>>,
}

#[derive(Deserialize, Default)]
struct ExecResponse {
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    stderr: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
}

impl Commands {
    pub(crate) fn new(client: HeyoClient, sandbox_id: String) -> Self {
        Self { client, sandbox_id }
    }

    /// `POST /sandbox/:id/exec`. Runs `command` with `sh -c`.
    pub async fn run(
        &self,
        command: &str,
        options: CommandRunOptions,
    ) -> Result<CommandResult, HeyoError> {
        let body = ExecRequest {
            command,
            cwd: options.cwd.as_deref(),
            env: options.env.as_ref(),
        };
        let path = format!(
            "/sandbox/{}/exec",
            urlencoding(&self.sandbox_id)
        );
        let mut opts = RequestOptions::default();
        opts.timeout = options.timeout;
        let raw = self
            .client
            .request::<ExecResponse>(Method::POST, &path, Some(&body), opts)
            .await?;
        Ok(CommandResult {
            stdout: raw.stdout.unwrap_or_default(),
            stderr: raw.stderr.unwrap_or_default(),
            output: raw.output.unwrap_or_default(),
            exit_code: raw.exit_code.unwrap_or(0),
        })
    }
}

fn urlencoding(s: &str) -> String {
    // Minimal path-segment encoding: percent-escape anything that isn't an
    // unreserved char. Sandbox IDs are `dep-…` so this is mostly a no-op.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub(crate) fn encode_path(s: &str) -> String {
    urlencoding(s)
}
