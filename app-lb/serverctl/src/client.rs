//! The HTTP client for the app-lb admin API.
//!
//! Everything is `serde_json::Value` at this layer rather than typed structs.
//! Two reasons: reads pass the server's own JSON through untouched for
//! `-o json`/`-o yaml`, and the write commands are read-modify-write against an
//! API whose `PUT` replaces the *whole* spec — round-tripping through a typed
//! struct would silently drop any field this CLI is a version behind on. Typed
//! views ([`crate::types`]) are parsed from the `Value` only for rendering.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Where app-lb's admin API listens out of the box (`APP_LB_ADMIN_ADDR`).
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:9090";

pub struct Client {
    agent: ureq::Agent,
    base: String,
    /// The prebuilt `Authorization` header, `None` when no password is known.
    auth: Option<String>,
}

impl Client {
    pub fn new(
        server: &str,
        user: Option<&str>,
        password: Option<&str>,
        insecure: bool,
        timeout: Duration,
    ) -> Result<Self> {
        // The admin listener is plaintext by default, but it can be reached over
        // app-lb's own TLS proxy (see examples/app-lb-admin.json), so HTTPS has
        // to work — including against the self-signed cert people start with.
        let mut tls = ureq::native_tls::TlsConnector::builder();
        if insecure {
            tls.danger_accept_invalid_certs(true);
            tls.danger_accept_invalid_hostnames(true);
        }
        let connector = tls.build().context("building the TLS connector")?;

        let agent = ureq::AgentBuilder::new()
            .timeout(timeout)
            .user_agent(concat!("serverctl/", env!("CARGO_PKG_VERSION")))
            .tls_connector(Arc::new(connector))
            .build();

        Ok(Self {
            agent,
            base: normalize_server(server),
            auth: password.map(|p| basic_auth(user.unwrap_or("admin"), p)),
        })
    }

    pub fn server(&self) -> &str {
        &self.base
    }

    pub fn has_credentials(&self) -> bool {
        self.auth.is_some()
    }

    /// A copy of this client with its credentials dropped, for probing which
    /// surfaces the server actually gates.
    pub fn anonymous(&self) -> Self {
        Self {
            agent: self.agent.clone(),
            base: self.base.clone(),
            auth: None,
        }
    }

    /// Status + body for any answer the server gave, including 4xx/5xx. Only a
    /// transport failure — nothing came back at all — is an error here.
    fn raw(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, String)> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.request(method, &url);
        if let Some(auth) = &self.auth {
            req = req.set("Authorization", auth);
        }
        let sent = match body {
            Some(v) => req.send_json(v),
            None => req.call(),
        };
        match sent {
            Ok(resp) => {
                let status = resp.status();
                let text = resp
                    .into_string()
                    .with_context(|| format!("reading the response body from {url}"))?;
                Ok((status, text))
            }
            // ureq treats a 4xx/5xx as an error; for us it is an answer.
            Err(ureq::Error::Status(code, resp)) => {
                Ok((code, resp.into_string().unwrap_or_default()))
            }
            Err(ureq::Error::Transport(t)) => Err(anyhow!(
                "cannot reach {} ({t}) — is app-lb running, and is --server right?",
                self.base
            )),
        }
    }

    /// As [`raw`](Self::raw), but a 4xx/5xx becomes an error worth printing.
    fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, String)> {
        let (code, body) = self.raw(method, path, body)?;
        if code >= 400 {
            return Err(self.api_error(code, &body));
        }
        Ok((code, body))
    }

    /// Turn a 4xx/5xx into a message worth reading. app-lb answers errors with
    /// `{"error": "..."}`, so prefer that over the raw body.
    fn api_error(&self, code: u16, body: &str) -> anyhow::Error {
        let detail = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("error")?.as_str().map(str::to_string))
            .unwrap_or_else(|| body.trim().to_string());

        match code {
            401 if self.auth.is_some() => anyhow!(
                "the server rejected these credentials (HTTP 401) — check the user/password, \
                 or re-run `serverctl login`"
            ),
            401 => anyhow!(
                "this server requires authentication (HTTP 401) — run `serverctl login`, \
                 or pass --user/--password"
            ),
            _ if detail.is_empty() => anyhow!("the server returned HTTP {code}"),
            _ => anyhow!("{detail} (HTTP {code})"),
        }
    }

    fn json(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let (_, text) = self.send(method, path, body)?;
        if text.trim().is_empty() {
            return Ok(Value::Null); // 204 No Content, e.g. a delete
        }
        serde_json::from_str(&text)
            .with_context(|| format!("the server's answer to {method} {path} was not JSON"))
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        self.json("GET", path, None)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.json("POST", path, Some(body))
    }

    pub fn put(&self, path: &str, body: &Value) -> Result<Value> {
        self.json("PUT", path, Some(body))
    }

    pub fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.json("PATCH", path, Some(body))
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        self.json("DELETE", path, None)
    }

    /// The response status, with 4xx/5xx reported rather than raised. Used to
    /// ask "is this route gated?" without treating a 401 as a failure.
    pub fn status_of(&self, path: &str) -> Result<u16> {
        Ok(self.raw("GET", path, None)?.0)
    }

    // -- The endpoints app-lb exposes --------------------------------------

    pub fn list_deployments(&self) -> Result<Value> {
        self.get("/deployments")
    }

    pub fn get_deployment(&self, id: &str) -> Result<Value> {
        self.get(&format!("/deployments/{}", escape(id)))
    }

    /// Whether a deployment exists, without turning a 404 into an error.
    pub fn deployment_exists(&self, id: &str) -> Result<bool> {
        match self.status_of(&format!("/deployments/{}", escape(id)))? {
            200 => Ok(true),
            404 => Ok(false),
            401 | 403 => bail!(
                "not authorized to read deployment {id:?} — run `serverctl login`, \
                 or pass --user/--password"
            ),
            code => bail!("unexpected HTTP {code} while looking up deployment {id:?}"),
        }
    }

    pub fn create_deployment(&self, spec: &Value) -> Result<Value> {
        self.post("/deployments", spec)
    }

    pub fn replace_deployment(&self, id: &str, spec: &Value) -> Result<Value> {
        self.put(&format!("/deployments/{}", escape(id)), spec)
    }

    pub fn patch_scaling(&self, id: &str, patch: &Value) -> Result<Value> {
        self.patch(&format!("/deployments/{}/scaling", escape(id)), patch)
    }

    pub fn delete_deployment(&self, id: &str) -> Result<()> {
        self.delete(&format!("/deployments/{}", escape(id))).map(|_| ())
    }

    /// Evict one VM. `force` kills it now; otherwise it drains.
    pub fn evict_vm(&self, id: &str, sandbox_id: &str, force: bool) -> Result<Value> {
        let path = format!(
            "/deployments/{}/vms/{}{}",
            escape(id),
            escape(sandbox_id),
            if force { "?force=true" } else { "" }
        );
        self.delete(&path)
    }

    // -- secrets -----------------------------------------------------------
    //
    // There is no `get_secret_value`, and there will not be one: app-lb has no
    // endpoint that reveals a stored value. A secret is written once and read
    // only in-process, by the builder resolving a deployment's `build.auth`.

    pub fn list_secrets(&self) -> Result<Value> {
        self.get("/secrets")
    }

    pub fn get_secret(&self, id: &str) -> Result<Value> {
        self.get(&format!("/secrets/{}", escape(id)))
    }

    pub fn secret_exists(&self, id: &str) -> Result<bool> {
        match self.status_of(&format!("/secrets/{}", escape(id)))? {
            200 => Ok(true),
            404 => Ok(false),
            401 | 403 => bail!(
                "not authorized to read secret {id:?} — run `serverctl login`, \
                 or pass --user/--password"
            ),
            code => bail!("unexpected HTTP {code} while looking up secret {id:?}"),
        }
    }

    pub fn create_secret(&self, spec: &Value) -> Result<Value> {
        self.post("/secrets", spec)
    }

    /// Merge keys into an existing secret: `{"data": {"K": "v", "GONE": null}}`.
    pub fn patch_secret(&self, id: &str, patch: &Value) -> Result<Value> {
        self.patch(&format!("/secrets/{}", escape(id)), patch)
    }

    pub fn delete_secret(&self, id: &str, force: bool) -> Result<()> {
        let path = format!(
            "/secrets/{}{}",
            escape(id),
            if force { "?force=true" } else { "" }
        );
        self.delete(&path).map(|_| ())
    }

    // -- jobs --------------------------------------------------------------
    //
    // Two verbs, one history: `build` produces a managed deployment's guest
    // image, `update` runs a static one's commands on the app-lb host, and both
    // are polled from `/jobs`.

    /// Start an image build. Returns immediately with a record; the build itself
    /// takes minutes.
    pub fn start_build(&self, id: &str, body: &Value) -> Result<Value> {
        self.post(&format!("/deployments/{}/build", escape(id)), body)
    }

    /// Start an artifact pull: materialize a rootfs from a store and roll the
    /// pool onto it. Same shape as a build, and polled from the same `/jobs`.
    pub fn start_pull(&self, id: &str, body: &Value) -> Result<Value> {
        self.post(&format!("/deployments/{}/pull", escape(id)), body)
    }

    /// Start a host update. Same shape as a build: scheduled, then polled.
    pub fn start_update(&self, id: &str) -> Result<Value> {
        self.post(
            &format!("/deployments/{}/update", escape(id)),
            &Value::Object(Default::default()),
        )
    }

    pub fn list_jobs(&self) -> Result<Value> {
        self.get("/jobs")
    }

    pub fn deployment_jobs(&self, id: &str) -> Result<Value> {
        self.get(&format!("/deployments/{}/jobs", escape(id)))
    }

    pub fn get_job(&self, job_id: &str) -> Result<Value> {
        self.get(&format!("/jobs/{}", escape(job_id)))
    }

    pub fn metrics(&self) -> Result<Value> {
        self.get("/metrics")
    }

    pub fn certs(&self) -> Result<Value> {
        self.get("/certs")
    }

    /// The always-open probe endpoint: proves we are talking to an app-lb admin
    /// listener at all, regardless of auth.
    pub fn healthz(&self) -> Result<()> {
        let (code, _) = self.send("GET", "/healthz", None)?;
        if code == 200 {
            Ok(())
        } else {
            bail!("GET /healthz answered HTTP {code}; that does not look like an app-lb admin API")
        }
    }
}

/// Which surfaces this server gates, discovered by asking without credentials.
///
/// app-lb has two independent switches — `APP_LB_DASHBOARD_PASSWORD` gates the
/// dashboard and `/metrics`, and `APP_LB_ADMIN_AUTH` extends the same gate to
/// the deployment CRUD API — so "does this need a password?" has two answers.
pub struct AuthProbe {
    pub metrics_gated: bool,
    pub crud_gated: bool,
}

impl AuthProbe {
    pub fn any(&self) -> bool {
        self.metrics_gated || self.crud_gated
    }
}

pub fn probe_auth(client: &Client) -> Result<AuthProbe> {
    let anon = client.anonymous();
    Ok(AuthProbe {
        metrics_gated: anon.status_of("/metrics")? == 401,
        crud_gated: anon.status_of("/deployments")? == 401,
    })
}

/// Accept `host:port` as shorthand for `http://host:port`, and drop a trailing
/// slash so paths concatenate cleanly.
fn normalize_server(s: &str) -> String {
    let s = s.trim();
    let with_scheme = if s.contains("://") {
        s.to_string()
    } else {
        format!("http://{s}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

fn basic_auth(user: &str, password: &str) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Basic {token}")
}

/// Percent-encode a path segment. Deployment ids and sandbox ids are normally
/// tame, but an id with a slash in it would otherwise retarget the request.
fn escape(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_gets_a_scheme_and_loses_its_trailing_slash() {
        assert_eq!(normalize_server("localhost:9090"), "http://localhost:9090");
        assert_eq!(normalize_server("http://h:9090/"), "http://h:9090");
        assert_eq!(normalize_server(" https://lb.example.com "), "https://lb.example.com");
    }

    #[test]
    fn basic_auth_splits_on_the_first_colon_only() {
        // Matches app-lb's own encoding: `user:pass:word` is password `pass:word`.
        assert_eq!(basic_auth("admin", "s3cret"), "Basic YWRtaW46czNjcmV0");
    }

    #[test]
    fn path_segments_are_escaped() {
        assert_eq!(escape("web-1.demo"), "web-1.demo");
        assert_eq!(escape("a/b"), "a%2Fb");
        assert_eq!(escape("a b"), "a%20b");
    }
}
