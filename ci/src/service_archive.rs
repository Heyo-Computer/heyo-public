//! Publish a CI-produced service archive through orchestrator's two-phase API.
//!
//! The upstream API has no idempotency key. Retrying an action can therefore
//! create duplicate archives. In particular, a finalize error is never treated
//! as success: callers must not infer that finalization completed.

use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresignRequest<'a> {
    user_id: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresignResponse {
    archive_id: String,
    upload_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FinalizeRequest<'a> {
    archive_id: &'a str,
    user_id: &'a str,
    deployment_id: &'a str,
    archive_name: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FinalizeResponse {
    archive_id: String,
    size_bytes: u64,
}

fn safe_url(value: &str, kind: &str, permit_query: bool) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| format!("invalid {kind} URL"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || (!permit_query && url.query().is_some())
        || url.fragment().is_some()
    {
        return Err(format!(
            "{kind} URL must use HTTPS (HTTP allowed on loopback) and contain no credentials or fragment"
        ));
    }
    Ok(url)
}

fn endpoint(base: &str, suffix: &str) -> Result<Url, String> {
    let base = safe_url(base, "orchestrator", false)?;
    let value = format!(
        "{}/orchestration/services/archives/{suffix}",
        base.as_str().trim_end_matches('/')
    );
    Url::parse(&value).map_err(|_| "invalid orchestrator URL".to_string())
}

/// Upload `bytes` and finalize them as an orchestrator service archive.
///
/// Bearer authentication is intentionally attached only to the two
/// orchestrator requests, never to the independently supplied presigned URL.
pub async fn publish(
    base: &str,
    token: &str,
    user_id: &str,
    operation: &str,
    name: &str,
    bytes: Vec<u8>,
) -> Result<String, String> {
    if token.trim().is_empty() {
        return Err("service archive publication needs an orchestrator credential".into());
    }
    let presign_url = endpoint(base, "presign")?;
    let finalize_url = endpoint(base, "finalize")?;
    let http = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "could not initialize service archive HTTP client".to_string())?;

    let response = http
        .post(presign_url)
        .bearer_auth(token)
        .json(&PresignRequest { user_id })
        .send()
        .await
        .map_err(|_| "service archive presign request failed".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "service archive presign returned HTTP {}",
            response.status().as_u16()
        ));
    }
    let slot: PresignResponse = response
        .json()
        .await
        .map_err(|_| "service archive presign returned an invalid response".to_string())?;
    if slot.archive_id.trim().is_empty() || slot.upload_url.trim().is_empty() {
        return Err("service archive presign returned an invalid response".into());
    }
    let upload_url = safe_url(&slot.upload_url, "archive upload", true)?;
    let size_bytes = bytes.len() as u64;
    let response = http
        .put(upload_url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .map_err(|_| "service archive upload failed".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "service archive upload returned HTTP {}",
            response.status().as_u16()
        ));
    }

    let response = http
        .post(finalize_url)
        .bearer_auth(token)
        .json(&FinalizeRequest {
            archive_id: &slot.archive_id,
            user_id,
            deployment_id: operation,
            archive_name: name,
        })
        .send()
        .await
        .map_err(|_| {
            "service archive finalize request failed; finalization status is unknown".to_string()
        })?;
    if !response.status().is_success() {
        return Err(format!(
            "service archive finalize returned HTTP {}; finalization status is unknown",
            response.status().as_u16()
        ));
    }
    let finalized: FinalizeResponse = response.json().await.map_err(|_| {
        "service archive finalize returned an invalid response; finalization status is unknown"
            .to_string()
    })?;
    if finalized.archive_id != slot.archive_id || finalized.size_bytes != size_bytes {
        return Err("service archive finalize returned mismatched identity or size".into());
    }
    Ok(finalized.archive_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::{post, put},
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<Vec<String>>>);

    async fn server(upload_status: StatusCode) -> (String, Seen) {
        let seen = Seen::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let upload = format!("{base}/upload?signature=secret");
        let app = Router::new()
            .route("/orchestration/services/archives/presign", post({
                let upload = upload.clone();
                move |State(seen): State<Seen>, headers: HeaderMap, Json(body): Json<Value>| {
                    let upload = upload.clone();
                    async move {
                        assert_eq!(headers["authorization"], "Bearer token-value");
                        assert_eq!(body, json!({"userId":"user-1"}));
                        seen.0.lock().unwrap().push("presign".into());
                        Json(json!({"archiveId":"archive-1", "uploadUrl":upload}))
                    }
                }
            }))
            .route("/upload", put(move |State(seen): State<Seen>, headers: HeaderMap, body: Bytes| async move {
                assert!(!headers.contains_key("authorization"));
                assert_eq!(headers["content-type"], "application/octet-stream");
                assert_eq!(&body[..], b"archive bytes");
                seen.0.lock().unwrap().push("upload".into());
                upload_status
            }))
            .route("/orchestration/services/archives/finalize", post(|State(seen): State<Seen>, headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer token-value");
                assert_eq!(body, json!({"archiveId":"archive-1", "userId":"user-1", "deploymentId":"operation-1", "archiveName":"service.tar.gz"}));
                seen.0.lock().unwrap().push("finalize".into());
                (StatusCode::CREATED, Json(json!({"archiveId":"archive-1", "sizeBytes":13})))
            }))
            .with_state(seen.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, seen)
    }

    #[tokio::test]
    async fn publishes_full_roundtrip_without_leaking_auth_to_upload() {
        let (base, seen) = server(StatusCode::OK).await;
        let id = publish(
            &base,
            "token-value",
            "user-1",
            "operation-1",
            "service.tar.gz",
            b"archive bytes".to_vec(),
        )
        .await
        .unwrap();
        assert_eq!(id, "archive-1");
        assert_eq!(*seen.0.lock().unwrap(), ["presign", "upload", "finalize"]);
    }

    #[tokio::test]
    async fn failed_upload_does_not_finalize() {
        let (base, seen) = server(StatusCode::BAD_GATEWAY).await;
        let error = publish(
            &base,
            "token-value",
            "user-1",
            "operation-1",
            "service.tar.gz",
            b"archive bytes".to_vec(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "service archive upload returned HTTP 502");
        assert_eq!(*seen.0.lock().unwrap(), ["presign", "upload"]);
    }
}
