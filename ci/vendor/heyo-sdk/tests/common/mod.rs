//! Shared bootstrap for integration tests. Each `tests/*.rs` is its own
//! crate, so this module is included via `mod common;`.

#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;

use heyo_sdk::{HeyoClient, HeyoClientOptions};

/// Load `sdk-rs/.env` into `std::env`. Existing values win. Silent no-op if
/// the file is absent.
pub fn load_dotenv() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env");
    let Ok(raw) = fs::read_to_string(&path) else {
        return;
    };
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(eq) = trimmed.find('=') else { continue };
        let key = trimmed[..eq].trim();
        if key.is_empty() || std::env::var(key).is_ok() {
            continue;
        }
        let mut value = trimmed[eq + 1..].trim();
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = &value[1..value.len() - 1];
        }
        std::env::set_var(key, value);
    }
}

/// Resolve the cloud base URL from env. Mirrors the TS scripts:
/// `HEYO_BASE_URL` wins; otherwise `HEYO_ENV` picks "local" (default,
/// http://localhost:4445) or "production" (https://server.heyo.computer).
pub fn base_url() -> String {
    if let Ok(v) = std::env::var("HEYO_BASE_URL") {
        return v;
    }
    let env = std::env::var("HEYO_ENV").unwrap_or_else(|_| "local".into());
    match env.to_lowercase().as_str() {
        "prod" | "production" => "https://server.heyo.computer".into(),
        _ => "http://localhost:4445".into(),
    }
}

pub fn client_options() -> Option<HeyoClientOptions> {
    load_dotenv();
    let api_key = std::env::var("HEYO_API_KEY").ok()?;
    Some(HeyoClientOptions {
        api_key: Some(api_key),
        base_url: Some(base_url()),
        timeout: None,
    })
}

/// Build a client or print a skip notice + return None.
pub fn client_or_skip(test_name: &str) -> Option<HeyoClient> {
    let opts = client_options()?;
    match HeyoClient::new(opts) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[{}] skipping — could not build client: {}", test_name, e);
            None
        }
    }
}
