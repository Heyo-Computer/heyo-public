//! Hits `GET /health` on a live `heyvmd --socket` daemon over its unix
//! socket. Gated on the `HEYVM_TEST_SOCKET` env var and skipped silently
//! when it is unset.
//!
//! Run with:
//!   HEYVM_TEST_SOCKET=$HOME/.heyo/heyvmd.sock cargo test --test uds_health
#![cfg(unix)]

use heyo_sdk::{HeyoClient, RequestOptions};
use reqwest::Method;

#[tokio::test]
async fn health_over_unix_socket() {
    let Ok(socket) = std::env::var("HEYVM_TEST_SOCKET") else {
        return; // skip silently — no daemon socket to test against
    };
    if socket.is_empty() {
        return;
    }
    let client = HeyoClient::local_socket(&socket).expect("build UDS client");
    let health: serde_json::Value = client
        .request(Method::GET, "/health", None::<&()>, RequestOptions::default())
        .await
        .expect("GET /health over unix socket");
    assert!(
        !health.is_null(),
        "expected a /health payload over {socket}, got null"
    );
}
