//! One-hop owner routing. Authentication stays on the original route; the
//! application's transport credential is never substituted for caller auth.
use axum::{body::Body, extract::{Request, State}, http::StatusCode, middleware::Next, response::{IntoResponse, Response}};
use base64::Engine;
use super::AppState;

pub(super) async fn route(State(state): State<AppState>, request: Request, next: Next) -> Response {
    match route_inner(&state, request, next).await {
        Ok(response) => response,
        Err(_) => super::error(StatusCode::SERVICE_UNAVAILABLE, "CI owner routing is unavailable; request was not retried"),
    }
}

async fn route_inner(state: &AppState, request: Request, next: Next) -> Result<Response, anyhow::Error> {
    let target = request.headers().get("x-ci-target-boot");
    let forwarded = request.headers().get("x-ci-forwarded");
    if let Some(target) = target {
        if target.to_str().ok().and_then(|v| v.parse::<uuid::Uuid>().ok()) != Some(state.dispatcher.executor.boot_id()) {
            return Ok(StatusCode::CONFLICT.into_response());
        }
    }
    if let Some(forwarded) = forwarded {
        if forwarded != "1" || target.is_none() || !state.dispatcher.executor.is_owner().await.map_err(anyhow::Error::msg)? {
            return Ok(StatusCode::CONFLICT.into_response());
        }
        // The existing handlers still take their effect permits. Do not take a
        // nested read permit here: a waiting handoff writer could deadlock it.
        return Ok(next.run(request).await);
    }
    if state.config.managed_deployment.is_none()
        || matches!(*request.method(), axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS)
        || request.uri().path() == "/api/lifecycle" || request.uri().path().starts_with("/api/lifecycle/")
        || state.dispatcher.executor.is_owner().await.map_err(anyhow::Error::msg)? {
        return Ok(next.run(request).await);
    }
    use anyhow::Context;
    let (boot, deployment): (uuid::Uuid, String) = sqlx::query_as(
        "SELECT o.boot_id,b.deployment_id FROM ci_executor_owner o JOIN ci_executor_boot b ON b.boot_id=o.boot_id WHERE o.singleton=TRUE",
    ).fetch_one(state.store.pool()).await?;
    let base = state.config.application_orchestrator_url.as_deref().context("application authority missing")?;
    let service = state.config.application_id.as_deref().context("application identity missing")?;
    let token = state.config.application_lifecycle_token.as_deref().context("application credential missing")?;
    let mut url = reqwest::Url::parse(base)?;
    anyhow::ensure!(matches!(url.scheme(), "http" | "https") && url.username().is_empty() && url.password().is_none()
        && url.query().is_none() && url.fragment().is_none(), "invalid application authority");
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("invalid application authority"))?
        .pop_if_empty().extend(["orchestration", "services", service, "instances", &deployment, "http-request"]);
    let (parts, body) = request.into_parts();
    let nominated: Vec<_> = parts.headers.get_all("connection").iter().filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',')).map(|v| v.trim().to_ascii_lowercase()).collect();
    let mut headers = Vec::new();
    for (name, value) in &parts.headers {
        if matches!(name.as_str(), "host" | "content-length" | "connection" | "keep-alive" | "proxy-authenticate"
            | "proxy-authorization" | "te" | "trailer" | "transfer-encoding" | "upgrade" | "x-ci-target-boot" | "x-ci-forwarded")
            || nominated.iter().any(|v| v == name.as_str()) { continue; }
        headers.push((name.as_str(), value.to_str()?));
    }
    let metadata = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({
        "bootId": boot, "method": parts.method.as_str(),
        "path": parts.uri.path_and_query().context("request path missing")?.as_str(), "headers": headers,
    }))?);
    if metadata.len() > 16 * 1024 { return Ok(StatusCode::PAYLOAD_TOO_LARGE.into_response()); }
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(std::time::Duration::from_secs(10)).build()?;
    let response = client.post(url).bearer_auth(token).header("x-heyo-instance-request", metadata)
        .header("content-type", "application/octet-stream")
        .body(reqwest::Body::wrap_stream(body.into_data_stream())).send().await?;
    let mut result = Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if matches!(name.as_str(), "connection" | "transfer-encoding" | "content-length" | "keep-alive"
            | "proxy-authenticate" | "proxy-authorization" | "te" | "trailer" | "upgrade") { continue; }
        result = result.header(name, value);
    }
    Ok(result.body(Body::from_stream(response.bytes_stream()))?)
}
