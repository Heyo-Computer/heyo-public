//! Primary entry-point. Mirrors `sdk-ts/src/sandbox.ts`.

use std::time::{Duration, Instant};

use reqwest::Method;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::client::{HeyoClient, HeyoClientOptions, RequestOptions};
use crate::commands::{encode_path, Commands};
use crate::errors::HeyoError;
use crate::files::Files;
use crate::shell::{ShellOptions, ShellSession};
use crate::types::{
    BoundUrl, PublicImage, SandboxCreateOptions, SandboxInfo, SandboxSize, SandboxStatus,
};

const DEFAULT_WAIT_FOR_READY: Duration = Duration::from_secs(5 * 60);
const READY_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Sandbox {
    sandbox_id: String,
    client: HeyoClient,
    commands: Commands,
    files: Files,
}

#[derive(Deserialize)]
struct CreateResponse {
    id: String,
    #[allow(dead_code)]
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
struct PublicImagesEnvelope {
    #[serde(default)]
    images: Vec<PublicImage>,
}

#[derive(Serialize)]
struct ReplaceMountRequest<'a> {
    archive_id: &'a str,
    sandbox_path: &'a str,
}

#[derive(Serialize)]
struct TtlRequest {
    ttl_seconds: u64,
}

#[derive(Serialize)]
struct ResizeRequest<'a> {
    size_class: &'a str,
}

#[derive(Serialize)]
struct ResizeDiskRequest {
    disk_size_gb: u64,
}

#[derive(Deserialize)]
struct BindPortResponse {
    subdomain: String,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    url: Option<String>,
    port: u16,
    #[serde(default, rename = "is_public")]
    is_public: Option<bool>,
}

impl Sandbox {
    fn from_id(client: HeyoClient, sandbox_id: String) -> Self {
        let commands = Commands::new(client.clone(), sandbox_id.clone());
        let files = Files::new(client.clone(), sandbox_id.clone());
        Self {
            sandbox_id,
            client,
            commands,
            files,
        }
    }

    pub fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    pub fn commands(&self) -> &Commands {
        &self.commands
    }

    pub fn files(&self) -> &Files {
        &self.files
    }

    pub fn client(&self) -> &HeyoClient {
        &self.client
    }

    /// Create a new sandbox and (by default) wait for it to leave the
    /// `provisioning` state before returning.
    ///
    /// Set `options.wait_for_ready = Some(Duration::ZERO)` to return
    /// immediately and `wait_for_ready()` later.
    pub async fn create(
        mut options: SandboxCreateOptions,
        client_options: HeyoClientOptions,
    ) -> Result<Self, HeyoError> {
        let wait_for = options.wait_for_ready.take().unwrap_or(DEFAULT_WAIT_FOR_READY);
        let client = HeyoClient::new(client_options)?;
        let body = serde_json::to_value(&options)
            .map_err(|e| HeyoError::api(0, format!("serialize create body: {}", e)))?;
        let body = augment_create_body(body);
        let created: CreateResponse = client
            .request(Method::POST, "/sandbox-deploy", Some(&body), RequestOptions::default())
            .await?;
        let sandbox = Sandbox::from_id(client, created.id);
        if !wait_for.is_zero() {
            sandbox.wait_for_ready(wait_for).await?;
        }
        Ok(sandbox)
    }

    /// Reattach to an existing sandbox by ID. Issues no network call.
    pub fn connect(sandbox_id: String, client_options: HeyoClientOptions) -> Result<Self, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        Ok(Sandbox::connect_with_client(client, sandbox_id))
    }

    /// Reattach using a client the caller already holds. Issues no network
    /// call, and is infallible because there is nothing left to construct.
    ///
    /// This is the only way to reach a sandbox over a unix socket:
    /// [`HeyoClientOptions`] carries a URL, which cannot describe one, so a
    /// client built by [`HeyoClient::local_socket`] or
    /// [`HeyoClient::local_auto`] has to be passed through rather than
    /// rebuilt.
    pub fn connect_with_client(client: HeyoClient, sandbox_id: String) -> Self {
        Sandbox::from_id(client, sandbox_id)
    }

    /// List all deployed sandboxes the caller can see.
    pub async fn list(client_options: HeyoClientOptions) -> Result<Vec<SandboxInfo>, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        Sandbox::list_with_client(&client).await
    }

    /// [`Sandbox::list`] over a client the caller already holds — the socket
    /// counterpart, for the same reason [`Sandbox::connect_with_client`] exists.
    pub async fn list_with_client(client: &HeyoClient) -> Result<Vec<SandboxInfo>, HeyoError> {
        client
            .request(Method::GET, "/deployed-sandboxes", None::<&()>, RequestOptions::default())
            .await
    }

    /// Find a deployed sandbox by its *exact* display name.
    ///
    /// Sends `GET /deployed-sandboxes?name=<name>`: a daemon that understands
    /// the filter (heyvmd ≥0.44) answers with at most one entry instead of its
    /// whole inventory; an older daemon ignores the query and returns the full
    /// list. The result is filtered client-side either way, so this is correct
    /// against both. `Ok(None)` means no sandbox has that name.
    pub async fn find_by_name(
        name: &str,
        client_options: HeyoClientOptions,
    ) -> Result<Option<SandboxInfo>, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        Sandbox::find_by_name_with_client(&client, name).await
    }

    /// [`Sandbox::find_by_name`] over a client the caller already holds — the
    /// socket counterpart, for the same reason [`Sandbox::connect_with_client`]
    /// exists.
    pub async fn find_by_name_with_client(
        client: &HeyoClient,
        name: &str,
    ) -> Result<Option<SandboxInfo>, HeyoError> {
        let mut opts = RequestOptions::default();
        opts.query.push(("name".to_string(), name.to_string()));
        let matches: Vec<SandboxInfo> = client
            .request(Method::GET, "/deployed-sandboxes", None::<&()>, opts)
            .await?;
        Ok(matches.into_iter().find(|s| s.name == name))
    }

    /// List public images available to deploy.
    pub async fn list_public_images(
        backend: Option<&str>,
        client_options: HeyoClientOptions,
    ) -> Result<Vec<PublicImage>, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let mut opts = RequestOptions::default();
        if let Some(b) = backend {
            opts.query.push(("backend".to_string(), b.to_string()));
        }
        let env: PublicImagesEnvelope = client
            .request(Method::GET, "/public-images", None::<&()>, opts)
            .await?;
        Ok(env.images)
    }

    /// Fetch the latest info for this sandbox.
    ///
    /// Delegates to [`Self::get`] (a per-id `GET`), so it no longer pulls the
    /// whole `/deployed-sandboxes` inventory to answer for one sandbox.
    /// Behavior change from ≤0.1.6: a stopped sandbox now resolves to its
    /// persisted metadata where it previously surfaced `NotFound` whenever it
    /// was transiently absent from the list.
    pub async fn info(&self) -> Result<SandboxInfo, HeyoError> {
        self.get().await
    }

    /// Fetch this sandbox's info directly by id (`GET /deployed-sandboxes/:id`).
    /// Unlike [`Self::info`], which filters the full list, this resolves by id
    /// and returns persisted metadata even for a stopped sandbox that can be
    /// transiently absent from the list right after it stops. Prefer this when
    /// you hold a specific sandbox and need its current state reliably.
    pub async fn get(&self) -> Result<SandboxInfo, HeyoError> {
        let path = format!("/deployed-sandboxes/{}", encode_path(&self.sandbox_id));
        self.client
            .request(Method::GET, &path, None::<&()>, RequestOptions::default())
            .await
    }

    /// Block until the sandbox transitions out of `provisioning`.
    pub async fn wait_for_ready(&self, timeout: Duration) -> Result<SandboxInfo, HeyoError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.info().await {
                Ok(info) => match info.status {
                    SandboxStatus::Running => return Ok(info),
                    SandboxStatus::Failed => {
                        return Err(HeyoError::SandboxFailed {
                            sandbox_id: self.sandbox_id.clone(),
                            reason: info
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "no reason reported".to_string()),
                        });
                    }
                    SandboxStatus::Provisioning | SandboxStatus::Unknown => {}
                    _ => return Ok(info),
                },
                Err(HeyoError::NotFound(_)) if Instant::now() < deadline => {
                    sleep(READY_POLL_INTERVAL).await;
                    continue;
                }
                Err(e) => return Err(e),
            }
            if Instant::now() >= deadline {
                return Err(HeyoError::Timeout(
                    timeout,
                    format!("Sandbox {} did not become ready", self.sandbox_id),
                ));
            }
            sleep(READY_POLL_INTERVAL).await;
        }
    }

    /// Permanently delete this sandbox. 404 is treated as success.
    pub async fn kill(&self) -> Result<(), HeyoError> {
        let path = format!("/deployed-sandboxes/{}", encode_path(&self.sandbox_id));
        match self
            .client
            .request::<serde_json::Value>(Method::DELETE, &path, None::<&()>, RequestOptions::default())
            .await
        {
            Ok(_) => Ok(()),
            Err(HeyoError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn stop(&self) -> Result<(), HeyoError> {
        let path = format!("/sandbox/{}/stop", encode_path(&self.sandbox_id));
        self.client
            .request::<serde_json::Value>(Method::POST, &path, None::<&()>, RequestOptions::default())
            .await?;
        Ok(())
    }

    pub async fn start(&self) -> Result<(), HeyoError> {
        let path = format!("/sandbox/{}/start", encode_path(&self.sandbox_id));
        self.client
            .request::<serde_json::Value>(Method::POST, &path, None::<&()>, RequestOptions::default())
            .await?;
        Ok(())
    }

    pub async fn restart(&self) -> Result<(), HeyoError> {
        let path = format!("/deployed-sandboxes/{}/restart", encode_path(&self.sandbox_id));
        self.client
            .request::<serde_json::Value>(Method::POST, &path, None::<&()>, RequestOptions::default())
            .await?;
        Ok(())
    }

    /// Update the sandbox's TTL (seconds). `0` for unlimited (if plan allows).
    pub async fn set_ttl(&self, ttl_seconds: u64) -> Result<(), HeyoError> {
        let body = TtlRequest { ttl_seconds };
        let path = format!("/deployed-sandboxes/{}/ttl", encode_path(&self.sandbox_id));
        self.client
            .request::<serde_json::Value>(Method::POST, &path, Some(&body), RequestOptions::default())
            .await?;
        Ok(())
    }

    pub async fn resize(&self, size: SandboxSize) -> Result<(), HeyoError> {
        let body = ResizeRequest {
            size_class: size.as_str(),
        };
        let path = format!("/deployed-sandboxes/{}/resize", encode_path(&self.sandbox_id));
        self.client
            .request::<serde_json::Value>(Method::POST, &path, Some(&body), RequestOptions::default())
            .await?;
        Ok(())
    }

    /// Grow the Firecracker workspace disk to the requested size in GiB.
    pub async fn resize_disk(&self, disk_size_gb: u64) -> Result<(), HeyoError> {
        if !(1..=250).contains(&disk_size_gb) {
            return Err(HeyoError::InvalidArgument(
                "disk_size_gb must be between 1 and 250 GiB".to_string(),
            ));
        }
        let body = ResizeDiskRequest { disk_size_gb };
        let path = format!("/deployed-sandboxes/{}/resize", encode_path(&self.sandbox_id));
        self.client
            .request::<serde_json::Value>(Method::POST, &path, Some(&body), RequestOptions::default())
            .await?;
        Ok(())
    }

    pub async fn checkpoint(&self) -> Result<(), HeyoError> {
        let path = format!("/deployed-sandboxes/{}/checkpoint", encode_path(&self.sandbox_id));
        self.client
            .request::<serde_json::Value>(Method::POST, &path, None::<&()>, RequestOptions::default())
            .await?;
        Ok(())
    }

    pub async fn restore(&self) -> Result<(), HeyoError> {
        let path = format!("/deployed-sandboxes/{}/restore", encode_path(&self.sandbox_id));
        self.client
            .request::<serde_json::Value>(Method::POST, &path, None::<&()>, RequestOptions::default())
            .await?;
        Ok(())
    }

    pub async fn replace_mount(
        &self,
        archive_id: &str,
        sandbox_path: Option<&str>,
    ) -> Result<(), HeyoError> {
        let body = ReplaceMountRequest {
            archive_id,
            sandbox_path: sandbox_path.unwrap_or("/workspace"),
        };
        let path = format!(
            "/deployed-sandboxes/{}/replace-mount",
            encode_path(&self.sandbox_id)
        );
        self.client
            .request::<serde_json::Value>(Method::POST, &path, Some(&body), RequestOptions::default())
            .await?;
        Ok(())
    }

    /// Public URL for a port the sandbox has bound, or `None` if not exposed.
    pub async fn get_host(&self, port: u16) -> Result<Option<String>, HeyoError> {
        let info = self.info().await?;
        Ok(info.urls.into_iter().find(|u| u.port == port).map(|u| u.url))
    }

    /// Open a persistent interactive shell. The returned [`ShellSession`]
    /// is already past the `init`/`ready` handshake.
    pub async fn shell(&self, options: ShellOptions) -> Result<ShellSession, HeyoError> {
        ShellSession::open(self.client.clone(), self.sandbox_id.clone(), options).await
    }

    /// Bind a port and return the resulting public URL.
    pub async fn bind_port(
        &self,
        port: u16,
        is_public: Option<bool>,
    ) -> Result<BoundUrl, HeyoError> {
        let mut body = serde_json::Map::new();
        body.insert("sandbox_id".into(), serde_json::Value::String(self.sandbox_id.clone()));
        body.insert("port".into(), serde_json::Value::Number(port.into()));
        if let Some(p) = is_public {
            body.insert("is_public".into(), serde_json::Value::Bool(p));
        }
        let raw: BindPortResponse = self
            .client
            .request(
                Method::POST,
                "/proxy-endpoints/for-deployed",
                Some(&serde_json::Value::Object(body)),
                RequestOptions::default(),
            )
            .await?;
        let hostname = raw
            .hostname
            .clone()
            .or_else(|| {
                raw.url
                    .as_ref()
                    .and_then(|u| url::Url::parse(u).ok())
                    .and_then(|u| u.host_str().map(String::from))
            })
            .unwrap_or_else(|| raw.subdomain.clone());
        let url = raw.url.unwrap_or_else(|| format!("https://{}", hostname));
        Ok(BoundUrl {
            subdomain: raw.subdomain,
            hostname,
            url,
            port: raw.port,
            is_public: raw.is_public.unwrap_or(true),
        })
    }

    /// Expose a guest TCP `port` over an iroh `hey-proxy` tunnel and return a
    /// `heyo://` ticket. Feed the ticket to [`crate::P2pTunnel::connect`] to get
    /// a local `127.0.0.1` socket that forwards raw TCP into the VM — the right
    /// primitive for non-HTTP services like Postgres (5432).
    ///
    /// Unlike [`Sandbox::bind_port`] (a public HTTPS subdomain, HTTP only), this
    /// is a raw byte-stream tunnel. The daemon keeps the tunnel alive; repeated
    /// calls for the same port return the same ticket.
    pub async fn expose_tcp(&self, port: u16) -> Result<String, HeyoError> {
        #[derive(Serialize)]
        struct Req {
            port: u16,
        }
        #[derive(Deserialize)]
        struct Resp {
            connection_url: String,
        }
        let resp: Resp = self
            .client
            .request(
                Method::POST,
                &format!("/sandboxes/{}/tcp-tunnel", self.sandbox_id),
                Some(&Req { port }),
                RequestOptions::default(),
            )
            .await?;
        Ok(resp.connection_url)
    }
}

/// Apply server-side defaults that the TS SDK mirrors: region=US,
/// image=ubuntu:24.04, size_class=small.
pub(crate) fn augment_create_body(mut body: serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::Object(map) = &mut body {
        map.entry("region")
            .or_insert(serde_json::Value::String("US".to_string()));
        map.entry("image")
            .or_insert(serde_json::Value::String("ubuntu:24.04".to_string()));
        map.entry("size_class")
            .or_insert(serde_json::Value::String("small".to_string()));
        map.entry("open_ports").or_insert(serde_json::Value::Array(vec![]));
    }
    body
}
