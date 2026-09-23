//! Browser sessions use the same Heyo scopes authority as API bearers.
use super::*;
use axum::http::{HeaderMap, Method};

const COOKIE: &str = "__Host-heyo-admin";

pub(super) fn same_origin(headers: &HeaderMap) -> bool {
    let expected = headers.get(header::HOST).and_then(|v| v.to_str().ok())
        .and_then(|host| reqwest::Url::parse(&format!("https://{host}")).ok());
    let actual = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        .and_then(|origin| reqwest::Url::parse(origin).ok());
    matches!((expected, actual), (Some(e), Some(a)) if a.scheme() == "https" && a.origin() == e.origin())
}

pub(super) fn session(headers: &HeaderMap, method: &Method) -> Result<Option<String>, ()> {
    // Explicit API credentials always take precedence over ambient cookies.
    if headers.contains_key(header::AUTHORIZATION) { return Ok(None); }
    let mut values = headers.get_all(header::COOKIE).iter()
        .filter_map(|h| h.to_str().ok()).flat_map(|h| h.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .filter_map(|(name, value)| (name == COOKIE).then_some(value));
    let Some(token) = values.next() else { return Ok(None); };
    if values.next().is_some() || token.is_empty() || token.len() > 3800
        || !token.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) { return Err(()); }
    // SameSite is additional protection, not a substitute for a CSRF check.
    // WebSocket handshakes are GETs but can execute commands after upgrading.
    if (!matches!(*method, Method::GET | Method::HEAD) || headers.contains_key(header::UPGRADE))
        && !same_origin(headers) { return Err(()); }
    Ok(Some(format!("Bearer {token}")))
}

pub(super) async fn page(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !state.gate_admin || state.auth.is_none() || state.federated.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    ([(header::CACHE_CONTROL, "no-store"), (header::REFERRER_POLICY, "no-referrer")],
        Html(render_page(&state, include_str!("admin_login.html"), &headers))).into_response()
}

#[derive(Deserialize)]
pub(super) struct Credentials { email: String, password: String }

pub(super) async fn login(State(state): State<AdminState>, headers: HeaderMap, Json(input): Json<Credentials>) -> Response {
    if !state.gate_admin || state.auth.is_none() { return StatusCode::NOT_FOUND.into_response(); }
    let Some(auth) = &state.federated else { return StatusCode::NOT_FOUND.into_response(); };
    if !same_origin(&headers) { return forbidden("sign-in requires this HTTPS origin"); }
    if input.email.trim().is_empty() || input.email.len() > 320 || input.password.is_empty() || input.password.len() > 4096 {
        return err(StatusCode::BAD_REQUEST, "email and password are required").into_response();
    }
    let Some((token, lifetime)) = auth.login(input.email.trim(), &input.password).await else {
        return err(StatusCode::UNAUTHORIZED, "Sign-in refused or unavailable. A Heyo platform administrator account is required.").into_response();
    };
    ([(header::CACHE_CONTROL, "no-store".to_owned()),
        (header::SET_COOKIE, format!("{COOKIE}={token}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={lifetime}"))],
        Json(serde_json::json!({"ok":true}))).into_response()
}

pub(super) async fn logout(headers: HeaderMap) -> Response {
    if !same_origin(&headers) { return forbidden("sign-out requires this HTTPS origin"); }
    (StatusCode::SEE_OTHER, [(header::LOCATION, "/login".to_owned()),
        (header::CACHE_CONTROL, "no-store".to_owned()),
        (header::SET_COOKIE, format!("{COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0"))]).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(origin: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "admin.eu1.example".parse().unwrap());
        h.insert(header::COOKIE, "__Host-heyo-admin=valid.jwt.signature".parse().unwrap());
        if let Some(origin) = origin { h.insert(header::ORIGIN, origin.parse().unwrap()); }
        h
    }

    #[test]
    fn cookie_writes_and_websockets_require_exact_https_origin() {
        for origin in [None, Some("null"), Some("https://admin.us3.example"), Some("http://admin.eu1.example"), Some("https://admin.eu1.example:444")] {
            let mut h = headers(origin);
            assert!(session(&h, &Method::GET).unwrap().is_some());
            assert!(session(&h, &Method::POST).is_err());
            h.insert(header::UPGRADE, "websocket".parse().unwrap());
            assert!(session(&h, &Method::GET).is_err());
        }
        let h = headers(Some("https://admin.eu1.example"));
        assert_eq!(session(&h, &Method::PUT).unwrap().as_deref(), Some("Bearer valid.jwt.signature"));
    }

    #[test]
    fn api_credentials_win_and_ambiguous_cookies_fail_closed() {
        let mut h = headers(None);
        h.append(header::COOKIE, "__Host-heyo-admin=other.jwt.signature".parse().unwrap());
        assert!(session(&h, &Method::GET).is_err());
        h.insert(header::AUTHORIZATION, "Bearer explicit".parse().unwrap());
        assert!(session(&h, &Method::POST).unwrap().is_none());
    }
}
