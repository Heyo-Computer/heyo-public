//! HTTP Basic auth middleware. Reuses the pooler's constant-time comparison so
//! a prober can't learn the credentials from response timing.

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;

use crate::auth::constant_time_eq;

use super::state::DashState;

/// Gate every request behind Basic auth when credentials are configured. When
/// `basic_auth` is `None` the dashboard is open (a startup warning covers the
/// non-loopback case).
pub async fn basic_auth(State(st): State<DashState>, req: Request, next: Next) -> Response {
    let Some((user, pass)) = st.cfg.basic_auth.as_ref() else {
        return next.run(req).await;
    };
    if authorized(req.headers(), user, pass) {
        next.run(req).await
    } else {
        challenge()
    }
}

/// Parse `Authorization: Basic base64(user:pass)` and compare both halves in
/// constant time. Both must match; evaluating both (rather than short-circuiting
/// on the username) avoids leaking which half was wrong via timing.
fn authorized(headers: &HeaderMap, user: &str, pass: &str) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(b64) = value.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return false;
    };
    let Ok(creds) = String::from_utf8(decoded) else {
        return false;
    };
    // Split on the first colon: passwords may contain colons, usernames may not.
    let Some((u, p)) = creds.split_once(':') else {
        return false;
    };
    let u_ok = constant_time_eq(u.as_bytes(), user.as_bytes());
    let p_ok = constant_time_eq(p.as_bytes(), pass.as_bytes());
    u_ok & p_ok
}

fn challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"pg-vm-pool\"")],
        "401 Unauthorized",
    )
        .into_response()
}
