use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecretStatus {
    Active,
    Retiring,
    Revoked,
    Deleted,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PutSecretRequest {
    pub path: String,
    pub value_base64: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub read_access: Vec<String>,
    #[serde(default)]
    pub write_access: Vec<String>,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadSecretRequest {
    pub path: String,
    #[serde(default)]
    pub version: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RotateRandomRequest {
    pub path: String,
    #[serde(default = "default_random_bytes")]
    pub bytes: usize,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokeSecretRequest {
    pub path: String,
    pub version: i32,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretValueResponse {
    pub path: String,
    pub version: i32,
    pub status: SecretStatus,
    pub value_base64: String,
    pub created_at: DateTime<Utc>,
    pub metadata: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretMetadataResponse {
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

#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretHistoryResponse {
    pub path: String,
    pub versions: Vec<SecretVersionMetadata>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSecretsResponse {
    pub secrets: Vec<SecretMetadataResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteSecretResponse {
    pub path: String,
    pub version: i32,
    pub status: SecretStatus,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    pub error: String,
}

fn default_random_bytes() -> usize {
    32
}
