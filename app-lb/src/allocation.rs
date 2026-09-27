//! Correlated heyvmd allocation. Persist Intent before submit; after uncertainty
//! only recover via GET. Request bodies (including resolved secrets) stay in RAM.
use heyo_sdk::{DaemonCreateRequest, HeyoClient};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub operation_id: String,
    pub request_digest: String,
    pub backend_request_digest: String,
    pub transport: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Receipt {
    pub operation_id: String,
    pub sandbox_id: String,
    pub request_digest: String,
    pub backend_request_digest: String,
}

// Deliberately neither Serialize nor Debug: body contains resolved secrets.
pub struct Prepared {
    pub intent: Intent,
    body: Value,
}

fn digest(mut value: Value) -> String {
    value.sort_all_objects();
    format!("{:x}", Sha256::digest(serde_json::to_vec(&value).expect("JSON value")))
}

pub fn prepare(request: &DaemonCreateRequest, transport: &str, scope: &str) -> Result<Prepared, String> {
    if !transport.starts_with("unix:") {
        let url = reqwest::Url::parse(transport).map_err(|_| "invalid allocation transport")?;
        if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty()
            || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            return Err("allocation transport must not contain credentials, query or fragment".into());
        }
    }
    let mut request = serde_json::to_value(request).map_err(|_| "cannot encode creation request")?;
    let object = request.as_object_mut().ok_or("creation request is not an object")?;
    // Match heyo-sdk 0.1.11 augment_create_body, then the native compatibility
    // adapter. Do not add sandbox_type: the daemon infers it from the image.
    object.entry("region").or_insert(json!("US"));
    object.entry("image").or_insert(json!("ubuntu:24.04"));
    object.entry("size_class").or_insert(json!("small"));
    object.entry("open_ports").or_insert(json!([]));
    if let Some(driver) = object.remove("driver") {
        object.entry("backend_type").or_insert(driver);
    }
    let kind = if request["s3_archive_key"].as_str().is_some_and(|s| !s.trim().is_empty()) { "archive" } else { "plain" };
    let backend_request_digest = digest(json!({"kind":kind,"request":request}));
    let request_digest = digest(json!({"scope":scope,"backendRequestDigest":backend_request_digest}));
    let mut nonce = [0u8; 16];
    openssl::rand::rand_bytes(&mut nonce).map_err(|_| "cannot allocate operation identity")?;
    let operation_id = format!("applb-{}", nonce.iter().map(|b| format!("{b:02x}")).collect::<String>());
    Ok(Prepared {
        intent: Intent { operation_id, request_digest:request_digest.clone(), backend_request_digest, transport:transport.into() },
        body: json!({"requestDigest":request_digest,"kind":kind,"request":request}),
    })
}

impl Intent {
    pub(crate) fn accepts(&self, receipt: &Receipt) -> bool {
        receipt.operation_id == self.operation_id
            && receipt.request_digest == self.request_digest
            && receipt.backend_request_digest == self.backend_request_digest
            && receipt.sandbox_id.len() == 35 && receipt.sandbox_id.starts_with("sb-")
            && receipt.sandbox_id[3..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
}

pub async fn submit(client: &HeyoClient, transport: &str, prepared: &Prepared) -> Result<Receipt, String> {
    request(client, transport, &prepared.intent, Some(&prepared.body)).await
}

pub async fn recover(client: &HeyoClient, transport: &str, intent: &Intent) -> Result<Receipt, String> {
    request(client, transport, intent, None).await
}

async fn request(client: &HeyoClient, transport: &str, intent: &Intent, body: Option<&Value>) -> Result<Receipt, String> {
    if intent.transport != transport || intent.operation_id.is_empty() || intent.operation_id.len() > 128
        || !intent.operation_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)) {
        return Err("creation operation transport or identity changed".into());
    }
    let key = client.api_key().filter(|key| !key.is_empty()).ok_or("correlated creation requires internal service authentication")?;
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never()).timeout(std::time::Duration::from_secs(30));
    #[cfg(unix)]
    if let Some(socket) = client.socket_path() { builder = builder.unix_socket(socket.to_path_buf()); }
    let http = builder.build().map_err(|_| "cannot build creation transport")?;
    let url = format!("{}/sandbox-creations/{}", client.base_url().trim_end_matches('/'), intent.operation_id);
    let req = match body { Some(body) => http.post(&url).json(body), None => http.get(&url) };
    let mut response = req.bearer_auth(key).send().await.map_err(|_| "creation outcome unknown")?;
    if response.status() != reqwest::StatusCode::OK
        && !(body.is_some() && response.status() == reqwest::StatusCode::ACCEPTED) {
        return Err("creation receipt unavailable; allocation remains unresolved".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "creation receipt incomplete")? {
        if bytes.len() + chunk.len() > 65536 { return Err("creation receipt exceeds limit".into()); }
        bytes.extend_from_slice(&chunk);
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|_| "invalid creation receipt")?;
    if !intent.accepts(&receipt) { return Err("creation receipt identity mismatch".into()); }
    Ok(receipt)
}

#[cfg(test)]
#[path = "allocation_tests.rs"]
mod tests;
