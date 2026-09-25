use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::client::{HeyoClient, RequestOptions};
use crate::commands::encode_path;
use crate::errors::HeyoError;

const DEFAULT_MOUNT: &str = "/workspace";

/// File-system surface, obtained via [`crate::Sandbox::files`].
#[derive(Clone)]
pub struct Files {
    client: HeyoClient,
    sandbox_id: String,
}

/// Options for `Files::read` / `Files::write`.
#[derive(Debug, Clone, Default)]
pub struct FileOptions {
    /// Mount on which the path is rooted. Defaults to `/workspace`.
    pub mount_path: Option<String>,
}

/// Discriminated content for `Files::write`. `String` is encoded as UTF-8;
/// `Bytes` is written verbatim.
#[derive(Debug, Clone)]
pub enum FileContent {
    Text(String),
    Bytes(Vec<u8>),
}

impl From<&str> for FileContent {
    fn from(s: &str) -> Self {
        FileContent::Text(s.to_string())
    }
}
impl From<String> for FileContent {
    fn from(s: String) -> Self {
        FileContent::Text(s)
    }
}
impl From<Vec<u8>> for FileContent {
    fn from(b: Vec<u8>) -> Self {
        FileContent::Bytes(b)
    }
}

#[derive(Serialize)]
struct ReadRequest<'a> {
    file_path: &'a str,
    mount_path: &'a str,
}

#[derive(Deserialize)]
struct ReadResponse {
    content: String,
}

#[derive(Serialize)]
struct WriteRequest<'a> {
    file_path: &'a str,
    mount_path: &'a str,
    content: String,
}

impl Files {
    pub(crate) fn new(client: HeyoClient, sandbox_id: String) -> Self {
        Self { client, sandbox_id }
    }

    /// Read the file at `file_path` (relative to the mount). Returns the
    /// raw bytes — use `read_text` for UTF-8 convenience.
    pub async fn read(
        &self,
        file_path: &str,
        options: FileOptions,
    ) -> Result<Vec<u8>, HeyoError> {
        let mount = options.mount_path.as_deref().unwrap_or(DEFAULT_MOUNT);
        let body = ReadRequest {
            file_path,
            mount_path: mount,
        };
        let path = format!("/sandbox/{}/read-file", encode_path(&self.sandbox_id));
        let resp: ReadResponse = self
            .client
            .request(Method::POST, &path, Some(&body), RequestOptions::default())
            .await?;
        BASE64
            .decode(resp.content.as_bytes())
            .map_err(|e| HeyoError::api(0, format!("invalid base64 in read-file response: {}", e)))
    }

    /// Convenience wrapper around `read` that decodes the bytes as UTF-8.
    pub async fn read_text(
        &self,
        file_path: &str,
        options: FileOptions,
    ) -> Result<String, HeyoError> {
        let bytes = self.read(file_path, options).await?;
        String::from_utf8(bytes).map_err(|e| HeyoError::api(0, format!("non-UTF-8 file: {}", e)))
    }

    /// Write `content` to `file_path`.
    pub async fn write(
        &self,
        file_path: &str,
        content: impl Into<FileContent>,
        options: FileOptions,
    ) -> Result<(), HeyoError> {
        let mount = options.mount_path.as_deref().unwrap_or(DEFAULT_MOUNT);
        let bytes = match content.into() {
            FileContent::Text(s) => s.into_bytes(),
            FileContent::Bytes(b) => b,
        };
        let body = WriteRequest {
            file_path,
            mount_path: mount,
            content: BASE64.encode(&bytes),
        };
        let path = format!("/sandbox/{}/write-file", encode_path(&self.sandbox_id));
        self.client
            .request::<serde_json::Value>(Method::POST, &path, Some(&body), RequestOptions::default())
            .await?;
        Ok(())
    }
}
