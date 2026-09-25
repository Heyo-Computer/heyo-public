//! Per-account sandbox networks. Mirrors `sdk-ts/src/networks.ts`.
//!
//! A `Network` is a control-plane record local and deployed sandboxes can
//! register into so they're addressable across machines from the same
//! account. Each account lazily gets a `default` network on first read
//! (preserved from the unmerged `feat/sdn-basics` prototype); additional
//! named networks can be created alongside it.

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::client::{HeyoClient, HeyoClientOptions, RequestOptions};
use crate::commands::encode_path;
use crate::errors::HeyoError;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkInfo {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub is_default: bool,
    #[serde(default)]
    pub description: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkMember {
    pub network_id: String,
    pub sandbox_kind: String,
    pub sandbox_ref: String,
    #[serde(default)]
    pub device_name: Option<String>,
    pub registered_at: String,
    #[serde(default)]
    pub last_seen_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NetworkCreateOptions {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NetworkUpdateOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `None` leaves the description untouched; `Some(None)` clears it;
    /// `Some(Some("…"))` sets it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMemberKind {
    Local,
    Deployed,
    /// A daemon host machine (`sandbox_ref` = daemon `hd-*` id). Assigning a
    /// host to a network unlocks host-shell access to it.
    Host,
}

impl NetworkMemberKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NetworkMemberKind::Local => "local",
            NetworkMemberKind::Deployed => "deployed",
            NetworkMemberKind::Host => "host",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkMemberRegistration {
    pub sandbox_kind: NetworkMemberKind,
    pub sandbox_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
}

/// A private service route registered in a network — a `name:port` address
/// other members can reach. When `connection_url` is set the service has a live
/// iroh proxy route; otherwise it's `agent_proxy_pending` until someone runs
/// `heyvm network expose-service`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkService {
    pub id: String,
    pub network_id: String,
    /// Service name (the `name` in `name:port`).
    pub name: String,
    /// Convenience `name:port` form.
    pub address: String,
    pub sandbox_kind: String,
    pub sandbox_ref: String,
    pub port: u16,
    pub protocol: String,
    pub status: String,
    /// `heyo://` ticket when a live route exists; `None` while pending.
    #[serde(default)]
    pub connection_url: Option<String>,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    /// `"iroh_tcp_proxy"` when routable, `"agent_proxy_pending"` otherwise.
    #[serde(default)]
    pub transport: Option<String>,
}

/// Result of dialing a service: a live route to it. `connection_url` is a
/// `heyo://` ticket consumable by [`P2pTunnel::connect`](crate::P2pTunnel::connect).
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceRoute {
    /// `name:port` address that was dialed.
    pub address: String,
    /// Transport — `"iroh_tcp_proxy"`.
    pub transport: String,
    /// `heyo://` ticket for the live route.
    pub connection_url: String,
}

#[derive(Deserialize)]
struct NetworksEnvelope {
    #[serde(default)]
    networks: Vec<NetworkInfo>,
}

#[derive(Deserialize)]
struct MembersEnvelope {
    #[serde(default)]
    members: Vec<NetworkMember>,
}

#[derive(Clone)]
pub struct Network {
    info: NetworkInfo,
    client: HeyoClient,
}

impl Network {
    fn from_raw(client: HeyoClient, info: NetworkInfo) -> Self {
        Self { info, client }
    }

    pub fn id(&self) -> &str {
        &self.info.id
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn is_default(&self) -> bool {
        self.info.is_default
    }

    pub fn info(&self) -> &NetworkInfo {
        &self.info
    }

    pub fn client(&self) -> &HeyoClient {
        &self.client
    }

    /// `POST /networks` — create a named network.
    pub async fn create(
        options: NetworkCreateOptions,
        client_options: HeyoClientOptions,
    ) -> Result<Self, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let raw: NetworkInfo = client
            .request(
                Method::POST,
                "/networks",
                Some(&options),
                RequestOptions::default(),
            )
            .await?;
        Ok(Network::from_raw(client, raw))
    }

    /// `GET /networks` — list networks for the caller's account (lazily
    /// creates the default network so the result is never empty).
    pub async fn list(client_options: HeyoClientOptions) -> Result<Vec<NetworkInfo>, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let env: NetworksEnvelope = client
            .request(Method::GET, "/networks", None::<&()>, RequestOptions::default())
            .await?;
        Ok(env.networks)
    }

    /// `GET /networks/{id}`.
    pub async fn get(id: &str, client_options: HeyoClientOptions) -> Result<Self, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let path = format!("/networks/{}", encode_path(id));
        let raw: NetworkInfo = client
            .request(Method::GET, &path, None::<&()>, RequestOptions::default())
            .await?;
        Ok(Network::from_raw(client, raw))
    }

    /// `GET /networks/me` — the caller's default network, lazily created.
    pub async fn default_for_me(
        client_options: HeyoClientOptions,
    ) -> Result<Self, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let raw: NetworkInfo = client
            .request(Method::GET, "/networks/me", None::<&()>, RequestOptions::default())
            .await?;
        Ok(Network::from_raw(client, raw))
    }

    /// `DELETE /networks/{id}` — refuses the default network (400).
    pub async fn delete_by_id(
        id: &str,
        client_options: HeyoClientOptions,
    ) -> Result<(), HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let path = format!("/networks/{}", encode_path(id));
        client
            .request::<serde_json::Value>(
                Method::DELETE,
                &path,
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(())
    }

    pub async fn delete(self) -> Result<(), HeyoError> {
        let path = format!("/networks/{}", encode_path(&self.info.id));
        self.client
            .request::<serde_json::Value>(
                Method::DELETE,
                &path,
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(())
    }

    /// `PATCH /networks/{id}` — rename or change description. The default
    /// network refuses rename attempts (server returns 400).
    pub async fn update(&mut self, options: NetworkUpdateOptions) -> Result<(), HeyoError> {
        let path = format!("/networks/{}", encode_path(&self.info.id));
        let raw: NetworkInfo = self
            .client
            .request(Method::PATCH, &path, Some(&options), RequestOptions::default())
            .await?;
        self.info = raw;
        Ok(())
    }

    /// `GET /networks/me/services` — services registered in the caller's
    /// default network (each a `name:port` route). Use [`dial_service`](Self::dial_service)
    /// to resolve one to a live connection ticket.
    pub async fn list_services(
        client_options: HeyoClientOptions,
    ) -> Result<Vec<NetworkService>, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        client
            .request(
                Method::GET,
                "/networks/me/services",
                None::<&()>,
                RequestOptions::default(),
            )
            .await
    }

    /// `GET /networks/me/services/{name}/{port}/dial` — resolve a service to a
    /// live iroh route. The returned [`ServiceRoute::connection_url`] is a
    /// `heyo://` ticket consumable by [`P2pTunnel::connect`](crate::P2pTunnel::connect).
    /// Errors with a 409 when the service has no live proxy route yet (run
    /// `heyvm network expose-service` for the target).
    pub async fn dial_service(
        name: &str,
        port: u16,
        client_options: HeyoClientOptions,
    ) -> Result<ServiceRoute, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let path = format!(
            "/networks/me/services/{}/{}/dial",
            encode_path(name),
            port
        );
        client
            .request(Method::GET, &path, None::<&()>, RequestOptions::default())
            .await
    }

    /// `GET /networks/{id}/members`.
    pub async fn list_members(&self) -> Result<Vec<NetworkMember>, HeyoError> {
        let path = format!("/networks/{}/members", encode_path(&self.info.id));
        let env: MembersEnvelope = self
            .client
            .request(Method::GET, &path, None::<&()>, RequestOptions::default())
            .await?;
        Ok(env.members)
    }

    /// `POST /networks/{id}/members` — register a sandbox into the
    /// network. Idempotent on (network_id, sandbox_kind, sandbox_ref);
    /// re-registering updates `device_name`.
    pub async fn add_member(
        &self,
        registration: NetworkMemberRegistration,
    ) -> Result<NetworkMember, HeyoError> {
        let path = format!("/networks/{}/members", encode_path(&self.info.id));
        self.client
            .request(
                Method::POST,
                &path,
                Some(&registration),
                RequestOptions::default(),
            )
            .await
    }

    /// `DELETE /networks/{id}/members/{kind}/{ref}`.
    pub async fn remove_member(
        &self,
        sandbox_kind: NetworkMemberKind,
        sandbox_ref: &str,
    ) -> Result<(), HeyoError> {
        let path = format!(
            "/networks/{}/members/{}/{}",
            encode_path(&self.info.id),
            sandbox_kind.as_str(),
            encode_path(sandbox_ref)
        );
        self.client
            .request::<serde_json::Value>(
                Method::DELETE,
                &path,
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(())
    }
}
