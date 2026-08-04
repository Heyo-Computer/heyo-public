use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct HeyoSecretClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct HeyoSecretClientOptions {
    pub base_url: String,
    pub token: String,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Error)]
pub enum HeyoSecretError {
    #[error("missing HeyoSecret configuration: {0}")]
    MissingConfig(&'static str),
    #[error("invalid secret value encoding: {0}")]
    Encoding(String),
    #[error("http error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("network error: {0}")]
    Network(String),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecretStatus {
    Active,
    Retiring,
    Revoked,
    Deleted,
}

#[derive(Clone)]
pub struct SecretValue {
    pub path: String,
    pub version: i32,
    pub status: SecretStatus,
    pub value: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretMetadata {
    pub path: String,
    pub owner: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub read_access: Vec<String>,
    pub write_access: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub active_version: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretVersionMetadata {
    pub version: i32,
    pub status: SecretStatus,
    pub value_sha256: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretHistory {
    pub path: String,
    pub versions: Vec<SecretVersionMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteSecretResult {
    pub path: String,
    pub version: i32,
    pub status: SecretStatus,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PutSecretOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_access: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_access: Vec<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

impl HeyoSecretClient {
    pub fn new(options: HeyoSecretClientOptions) -> Result<Self, HeyoSecretError> {
        if options.base_url.trim().is_empty() {
            return Err(HeyoSecretError::MissingConfig("base_url"));
        }
        if options.token.trim().is_empty() {
            return Err(HeyoSecretError::MissingConfig("token"));
        }
        let http = reqwest::Client::builder()
            .timeout(options.timeout.unwrap_or(DEFAULT_TIMEOUT))
            .build()
            .map_err(|error| HeyoSecretError::Network(error.to_string()))?;
        Ok(Self {
            base_url: options.base_url.trim_end_matches('/').to_string(),
            token: options.token,
            http,
        })
    }

    pub fn from_env() -> Result<Self, HeyoSecretError> {
        let base_url = std::env::var("HEYOSECRET_URL")
            .map_err(|_| HeyoSecretError::MissingConfig("HEYOSECRET_URL"))?;
        let token = std::env::var("HEYOSECRET_INTERNAL_API_KEY")
            .or_else(|_| std::env::var("PLATFORM_INTERNAL_API_KEY"))
            .map_err(|_| HeyoSecretError::MissingConfig("HEYOSECRET_INTERNAL_API_KEY"))?;
        Self::new(HeyoSecretClientOptions {
            base_url,
            token,
            timeout: None,
        })
    }

    pub async fn read_active(&self, path: &str) -> Result<SecretValue, HeyoSecretError> {
        self.read(path, None).await
    }

    pub async fn read(
        &self,
        path: &str,
        version: Option<i32>,
    ) -> Result<SecretValue, HeyoSecretError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            path: &'a str,
            version: Option<i32>,
        }
        let raw: SecretValueWire = self
            .post("/v1/secrets/read", &Request { path, version })
            .await?;
        raw.try_into()
    }

    pub async fn put(
        &self,
        path: &str,
        value: &[u8],
        options: PutSecretOptions,
    ) -> Result<WriteSecretResult, HeyoSecretError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            path: &'a str,
            value_base64: String,
            #[serde(flatten)]
            options: PutSecretOptions,
        }
        self.post(
            "/v1/secrets/write",
            &Request {
                path,
                value_base64: base64::engine::general_purpose::STANDARD.encode(value),
                options,
            },
        )
        .await
    }

    pub async fn rotate_random(
        &self,
        path: &str,
        bytes: usize,
        actor: Option<&str>,
    ) -> Result<WriteSecretResult, HeyoSecretError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            path: &'a str,
            bytes: usize,
            actor: Option<&'a str>,
        }
        self.post("/v1/secrets/rotate-random", &Request { path, bytes, actor })
            .await
    }

    pub async fn revoke(
        &self,
        path: &str,
        version: i32,
        actor: Option<&str>,
    ) -> Result<(), HeyoSecretError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            path: &'a str,
            version: i32,
            actor: Option<&'a str>,
        }
        let _: Value = self
            .post("/v1/secrets/revoke", &Request { path, version, actor })
            .await?;
        Ok(())
    }

    pub async fn metadata(&self, path: &str) -> Result<SecretMetadata, HeyoSecretError> {
        self.get(&format!("/v1/secrets/metadata?path={}", encode_query(path)))
            .await
    }

    pub async fn history(&self, path: &str) -> Result<SecretHistory, HeyoSecretError> {
        self.get(&format!("/v1/secrets/history?path={}", encode_query(path)))
            .await
    }

    pub async fn list(&self, prefix: Option<&str>) -> Result<Vec<SecretMetadata>, HeyoSecretError> {
        #[derive(Deserialize)]
        struct Response {
            secrets: Vec<SecretMetadata>,
        }
        let path = match prefix {
            Some(prefix) => format!("/v1/secrets?prefix={}", encode_query(prefix)),
            None => "/v1/secrets".to_string(),
        };
        let response: Response = self.get(&path).await?;
        Ok(response.secrets)
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, HeyoSecretError> {
        let response = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| HeyoSecretError::Network(error.to_string()))?;
        consume_response(response).await
    }

    async fn post<T: for<'de> Deserialize<'de>, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, HeyoSecretError> {
        let response = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|error| HeyoSecretError::Network(error.to_string()))?;
        consume_response(response).await
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SecretValueWire {
    path: String,
    version: i32,
    status: SecretStatus,
    value_base64: String,
    created_at: DateTime<Utc>,
    metadata: Value,
}

impl TryFrom<SecretValueWire> for SecretValue {
    type Error = HeyoSecretError;

    fn try_from(value: SecretValueWire) -> Result<Self, Self::Error> {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(value.value_base64)
            .map_err(|error| HeyoSecretError::Encoding(error.to_string()))?;
        Ok(Self {
            path: value.path,
            version: value.version,
            status: value.status,
            value: decoded,
            created_at: value.created_at,
            metadata: value.metadata,
        })
    }
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: Option<String>,
}

async fn consume_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, HeyoSecretError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| HeyoSecretError::Network(error.to_string()))?;
    if !status.is_success() {
        let message = serde_json::from_slice::<ErrorEnvelope>(&bytes)
            .ok()
            .and_then(|value| value.error)
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).to_string());
        return Err(api_error(status, message));
    }
    serde_json::from_slice(&bytes).map_err(|error| HeyoSecretError::Api {
        status: 0,
        message: format!("invalid JSON response: {error}"),
    })
}

fn api_error(status: StatusCode, message: String) -> HeyoSecretError {
    HeyoSecretError::Api {
        status: status.as_u16(),
        message,
    }
}

fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
