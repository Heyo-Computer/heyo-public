//! The HTTP surface exposes content, never materialization — a hardlink cannot
//! cross a wire.
//!
//! So there is no `materialize` route and no `gc` route. Materialization is the
//! whole point of running this next to the thing that consumes it; asking for it
//! remotely can only ever mean "send me the bytes", which is `GET /blobs/…`.
//! Garbage collection is destructive and lives in the CLI.
//!
//! Uploads stream: `PUT /blobs/{digest}` feeds the request body straight into
//! the store's blocking insert through a [`tokio_util::io::SyncIoBridge`], so a
//! twenty-gigabyte image never lands in memory. Downloads stream the same way,
//! through a [`tokio_util::io::ReaderStream`].
//!
//! Everything except `/healthz` is behind the API key when one is configured.
//! `/healthz` stays open because app-lb's health probe has no credentials, and a
//! deployment whose readiness check needs a secret is a deployment that reports
//! itself unhealthy the day the secret rotates.

use crate::config::Config;
use crate::digest::Digest;
use crate::error::Error;
use crate::manifest::Manifest;
use crate::store::{BlobInfo, Store, Usage};
use crate::tags::{Ref, TagName};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use std::net::SocketAddr;
use std::sync::Arc;

/// Body cap for everything that is not a blob upload. A manifest or a tag is
/// small; anything claiming otherwise is a mistake or an attack.
const SMALL_BODY_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct ServeState {
    store: Store,
    api_key: Option<Arc<String>>,
    /// Reject every mutating route. For a VM that only serves content.
    read_only: bool,
}

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub addr: SocketAddr,
    pub api_key: Option<String>,
    pub read_only: bool,
    /// Whether the dashboard is unmounted, gated, or open.
    pub dashboard: crate::config::DashboardAccess,
}

/// Serve until SIGTERM or Ctrl-C.
pub async fn serve(config: &Config, opts: ServeOptions) -> crate::Result<()> {
    let store = Store::open(config)?;
    if opts.api_key.is_none() {
        tracing::warn!(
            "no API key set (ART_API_KEY); every route is open to anyone who can reach this listener"
        );
    }
    let state = ServeState {
        store: store.clone(),
        api_key: opts.api_key.map(Arc::new),
        read_only: opts.read_only,
    };

    let web = match &opts.dashboard {
        crate::config::DashboardAccess::Password(creds) => {
            tracing::info!(user = %creds.user, "dashboard enabled at /dashboard");
            Some(crate::web::WebState {
                store,
                auth: Some(Arc::new(crate::admin::AdminAuth::new(
                    creds.user.clone(),
                    creds.password.clone(),
                ))),
            })
        }
        crate::config::DashboardAccess::Open => {
            // Warn, not info: this one was asked for explicitly, but the
            // operator's reason for it ("the listener is private") is a fact
            // about the network that this process cannot verify, and the log is
            // the only place the assumption is written down.
            tracing::warn!(
                addr = %opts.addr,
                "dashboard open at /dashboard with no login (ART_DASHBOARD_OPEN); \
                 anyone who can reach this listener can read every tag and blob listing"
            );
            Some(crate::web::WebState { store, auth: None })
        }
        crate::config::DashboardAccess::Off => {
            // Off, not open. Tag names and blob sizes describe what an
            // organisation builds; that is not a thing to expose because a
            // variable went unset.
            tracing::info!(
                "dashboard disabled — set ART_ADMIN_PASSWORD to enable it at /dashboard, \
                 or ART_DASHBOARD_OPEN=1 if the listener is already private"
            );
            None
        }
    };

    let listener = tokio::net::TcpListener::bind(opts.addr)
        .await
        .map_err(|e| Error::Io {
            context: format!("bind {}", opts.addr),
            source: e,
        })?;
    tracing::info!(
        addr = %opts.addr,
        root = %config.root.display(),
        read_only = opts.read_only,
        "artifacts daemon listening"
    );

    axum::serve(listener, router(state).merge(web_router(web)))
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|e| Error::Io {
            context: "http server".into(),
            source: e,
        })
}

pub fn router(state: ServeState) -> Router {
    // Blob upload is the one route whose body is legitimately enormous; the
    // free-space guard in the store is what actually bounds it.
    let blob_upload = Router::new()
        .route("/blobs/{digest}", put(put_blob))
        .layer(DefaultBodyLimit::disable())
        .with_state(state.clone());

    let rest = Router::new()
        .route("/blobs/{digest}", get(get_blob).head(head_blob))
        .route("/manifests/{reference}", get(get_manifest))
        .route("/manifests", put(put_manifest))
        .route("/tags", get(list_tags))
        .route("/tags/{name}", get(get_tag).put(put_tag).delete(delete_tag))
        .route("/usage", get(get_usage))
        .layer(DefaultBodyLimit::max(SMALL_BODY_LIMIT))
        .with_state(state.clone());

    Router::new()
        .merge(blob_upload)
        .merge(rest)
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        // Registered after the auth layer, so it is not subject to it.
        .route("/healthz", get(|| async { "ok\n" }))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// The dashboard, on its own credentials — or on none, when the operator has
/// declared the listener private.
///
/// Deliberately a second router with a second auth layer rather than another
/// branch inside the API's: the two have different identities, different
/// failure modes (a redirect to a login page, not a `401` with
/// `WWW-Authenticate`), and different blast radii. Keeping them apart means an
/// edit to one cannot quietly widen the other.
pub fn web_router(web: Option<crate::web::WebState>) -> Router {
    use crate::web;
    let Some(state) = web else {
        return Router::new();
    };

    Router::new()
        .route("/", get(web::index))
        .route("/dashboard", get(web::overview))
        .route("/dashboard/blobs", get(web::blobs_page))
        .route("/dashboard/blob/{digest}", get(web::blob_page))
        .route("/dashboard/manifests", get(web::manifests_page))
        .route("/dashboard/manifest/{digest}", get(web::manifest_page))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_dashboard_auth,
        ))
        // Login and logout sit outside the gate — the login page is how you get
        // through it, and a sign-out that needs a valid session is a sign-out
        // that cannot clear a stale one.
        .route("/login", get(web::login_page).post(web::login_submit))
        .route("/logout", axum::routing::post(web::logout))
        .layer(DefaultBodyLimit::max(SMALL_BODY_LIMIT))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

async fn require_dashboard_auth(
    State(st): State<crate::web::WebState>,
    req: Request,
    next: Next,
) -> Response {
    use axum_extra::extract::cookie::CookieJar;
    let jar = CookieJar::from_headers(req.headers());
    if crate::web::dashboard_authorized(&st, req.headers(), &jar) {
        return next.run(req).await;
    }
    // A browser gets a login page, not a Basic-auth prompt: the prompt cannot
    // be styled, cannot be signed out of, and teaches people to type admin
    // credentials into a chrome dialog.
    axum::response::Redirect::to("/login").into_response()
}

/// Constant-time API-key check over `Authorization: Bearer` or `X-Api-Key`.
async fn authorize(State(st): State<ServeState>, req: Request, next: Next) -> Response {
    let Some(expected) = &st.api_key else {
        return next.run(req).await;
    };
    let presented = bearer(req.headers()).or_else(|| {
        req.headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    });

    let ok = match presented {
        Some(p) => {
            use subtle::ConstantTimeEq;
            // Compare digests, not the raw keys: `ct_eq` is only constant-time
            // for equal lengths, so hashing first stops the comparison from
            // leaking the key's length.
            let a = sha2::Sha256::digest_of(p.as_bytes());
            let b = sha2::Sha256::digest_of(expected.as_bytes());
            a.ct_eq(&b).into()
        }
        None => false,
    };
    if !ok {
        // Through `IntoResponse`, not a hand-built tuple: that is what attaches
        // the `WWW-Authenticate` header, and building the response here instead
        // silently drops it.
        return ApiError(Error::Unauthorized).into_response();
    }
    next.run(req).await
}

/// Small helper so the auth path does not depend on the `Digest` newtype.
trait DigestOf {
    fn digest_of(bytes: &[u8]) -> [u8; 32];
}
impl DigestOf for sha2::Sha256 {
    fn digest_of(bytes: &[u8]) -> [u8; 32] {
        use sha2::Digest as _;
        let mut h = sha2::Sha256::new();
        h.update(bytes);
        let out = h.finalize();
        let mut a = [0u8; 32];
        a.copy_from_slice(&out);
        a
    }
}

fn bearer(h: &HeaderMap) -> Option<String> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    v.strip_prefix("Bearer ").map(str::to_string)
}

// ---------------------------------------------------------------------------
// Blobs
// ---------------------------------------------------------------------------

async fn head_blob(
    State(st): State<ServeState>,
    Path(digest): Path<String>,
) -> Result<Response, ApiError> {
    let d = Digest::parse(&digest).map_err(Error::from)?;
    let info = st.store.stat(&d).await?;
    Ok(blob_headers(&info).into_response())
}

async fn get_blob(
    State(st): State<ServeState>,
    Path(digest): Path<String>,
) -> Result<Response, ApiError> {
    let d = Digest::parse(&digest).map_err(Error::from)?;
    let info = st.store.stat(&d).await?;
    let file = st.store.open_blob(&d).await?;
    // Reads through the holes of a sparsified blob, which is exactly right: the
    // digest covers the logical stream, and that is what a caller expects.
    let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file));
    let mut resp = Body::from_stream(stream).into_response();
    *resp.headers_mut() = blob_headers(&info);
    Ok(resp)
}

async fn put_blob(
    State(st): State<ServeState>,
    Path(digest): Path<String>,
    body: Body,
) -> Result<Response, ApiError> {
    st.writable()?;
    let expected = Digest::parse(&digest).map_err(Error::from)?;

    use futures_util::TryStreamExt;
    let reader = tokio_util::io::StreamReader::new(
        body.into_data_stream()
            .map_err(|e| std::io::Error::other(e.to_string())),
    );
    // Squash: an uploaded artifact is usually a disk image, and a caller who
    // knows otherwise loses nothing — a zero-run scan of incompressible data
    // finds nothing and writes everything.
    let info = st.store.insert_async(reader, crate::Shape::SQUASH).await?;

    if info.digest != expected {
        // The bytes are stored under their true name — this is a
        // content-addressed store, so there is nothing else they could be
        // called — but the caller asked for something it did not send.
        return Err(ApiError(Error::DigestMismatch {
            expected,
            actual: info.digest,
        }));
    }
    let status = if info.deduped {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(blob_json(&info))).into_response())
}

fn blob_headers(info: &BlobInfo) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_LENGTH, info.size.into());
    h.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    // Immutable by construction: the name is the hash of the content.
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    if let Ok(v) = header::HeaderValue::from_str(&info.allocated.to_string()) {
        h.insert("x-art-allocated", v);
    }
    if let Ok(v) = header::HeaderValue::from_str(&format!("\"{}\"", info.digest)) {
        h.insert(header::ETAG, v);
    }
    h
}

// ---------------------------------------------------------------------------
// Manifests and tags
// ---------------------------------------------------------------------------

async fn get_manifest(
    State(st): State<ServeState>,
    Path(reference): Path<String>,
) -> Result<Response, ApiError> {
    let r = Ref::parse(&reference).map_err(Error::from)?;
    let d = st.store.resolve(&r).await?;
    let m = st.store.get_manifest(&d).await?;
    Ok(Json(m).into_response())
}

async fn put_manifest(
    State(st): State<ServeState>,
    Json(m): Json<Manifest>,
) -> Result<Response, ApiError> {
    st.writable()?;
    let d = st.store.put_manifest(&m).await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "digest": d.as_str() })),
    )
        .into_response())
}

async fn list_tags(State(st): State<ServeState>) -> Result<Response, ApiError> {
    let tags = st.store.list_tags().await?;
    let body: Vec<_> = tags
        .iter()
        .map(|(t, d)| serde_json::json!({"tag": t.as_str(), "digest": d.as_str()}))
        .collect();
    Ok(Json(body).into_response())
}

/// What one tag points at.
///
/// A client that only wants a single tag had to `GET /tags` and filter, which
/// grows with the store rather than with the question. It matters because a tag
/// may name a *blob* rather than a manifest — `art put --tag` does exactly that
/// — so "resolve this reference" cannot always be answered by
/// `GET /manifests/{ref}`, and the fallback should not be a full listing.
async fn get_tag(
    State(st): State<ServeState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let t = TagName::parse(&name).map_err(Error::from)?;
    let d = st.store.get_tag(&t).await?;
    Ok(Json(serde_json::json!({"tag": t.as_str(), "digest": d.as_str()})).into_response())
}

async fn put_tag(
    State(st): State<ServeState>,
    Path(name): Path<String>,
    body: String,
) -> Result<Response, ApiError> {
    st.writable()?;
    let t = TagName::parse(&name).map_err(Error::from)?;
    let d = Digest::parse(body.trim()).map_err(Error::from)?;
    st.store.set_tag(&t, &d).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn delete_tag(
    State(st): State<ServeState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    st.writable()?;
    let t = TagName::parse(&name).map_err(Error::from)?;
    if !st.store.remove_tag(&t).await? {
        return Err(ApiError(Error::TagNotFound(name)));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn get_usage(State(st): State<ServeState>) -> Result<Response, ApiError> {
    let u = st.store.usage().await?;
    Ok(Json(usage_json(&u)).into_response())
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

impl ServeState {
    fn writable(&self) -> Result<(), ApiError> {
        if self.read_only {
            return Err(ApiError(Error::ReadOnly));
        }
        Ok(())
    }
}

/// Wraps [`Error`] so handlers can `?` and still produce a JSON body with the
/// same slug the CLI reports.
pub struct ApiError(Error);

impl ApiError {
    fn with_status(self, status: StatusCode) -> (StatusCode, Json<serde_json::Value>) {
        let body = Json(serde_json::json!({
            "error": self.0.slug(),
            "message": self.0.to_string(),
        }));
        (status, body)
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            // Forbidden, not Unauthorized: the caller's credentials are fine and
            // a better key would not help.
            Error::ReadOnly => StatusCode::FORBIDDEN,
            Error::NotFound(_) | Error::TagNotFound(_) => StatusCode::NOT_FOUND,
            Error::Digest(_) | Error::TagName(_) | Error::ManifestVersion(_) => {
                StatusCode::BAD_REQUEST
            }
            Error::AmbiguousManifest { .. } => StatusCode::CONFLICT,
            Error::DigestMismatch { .. } => StatusCode::CONFLICT,
            Error::NoSpace { .. } => StatusCode::INSUFFICIENT_STORAGE,
            Error::LinkLimit { .. } | Error::SharedInode { .. } => StatusCode::CONFLICT,
            Error::Io { source, .. } => match source.kind() {
                std::io::ErrorKind::PermissionDenied => StatusCode::UNAUTHORIZED,
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
                    StatusCode::BAD_REQUEST
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %self.0, "request failed");
        }
        let mut resp = self.with_status(status).into_response();
        if status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        resp
    }
}

fn blob_json(b: &BlobInfo) -> serde_json::Value {
    serde_json::json!({
        "digest": b.digest.as_str(),
        "size": b.size,
        "allocated": b.allocated,
        "nlink": b.nlink,
        "deduped": b.deduped,
    })
}

fn usage_json(u: &Usage) -> serde_json::Value {
    serde_json::json!({
        "blobs": u.blobs,
        "logical": u.logical,
        "allocated": u.allocated,
        "manifests": u.manifests,
        "tags": u.tags,
        "fsAvailable": u.fs_available,
        "fsTotal": u.fs_total,
    })
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use base64::Engine as _;
    use axum::http::Request as HttpRequest;
    use std::path::PathBuf;
    use std::time::Duration;
    use tower::ServiceExt;

    fn tmpdir() -> tempfile::TempDir {
        match std::env::var_os("ART_TEST_DIR").map(PathBuf::from) {
            Some(b) => tempfile::tempdir_in(b).unwrap(),
            None => tempfile::tempdir().unwrap(),
        }
    }

    fn app(d: &tempfile::TempDir, api_key: Option<&str>, read_only: bool) -> (Router, Store) {
        let store = Store::open(&Config {
            root: d.path().join("store"),
            min_free_bytes: 0,
            gc_min_age: Duration::ZERO,
            heyvm_images_dir: d.path().join("images"),
        })
        .unwrap();
        let state = ServeState {
            store: store.clone(),
            api_key: api_key.map(|k| Arc::new(k.to_string())),
            read_only,
        };
        (router(state), store)
    }

    async fn body_string(r: Response) -> String {
        String::from_utf8(to_bytes(r.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn healthz_is_open_even_with_a_key_set() {
        // app-lb's health probe carries no credentials. A readiness check that
        // needs a secret fails the day the secret rotates.
        let d = tmpdir();
        let (app, _) = app(&d, Some("secret"), false);
        let r = app
            .oneshot(HttpRequest::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body_string(r).await, "ok\n");
    }

    #[tokio::test]
    async fn everything_else_needs_the_key() {
        let d = tmpdir();
        let (app, _) = app(&d, Some("secret"), false);
        let r = app
            .clone()
            .oneshot(HttpRequest::get("/tags").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            r.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer"
        );

        // Both accepted spellings work.
        for req in [
            HttpRequest::get("/tags")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
            HttpRequest::get("/tags")
                .header("x-api-key", "secret")
                .body(Body::empty())
                .unwrap(),
        ] {
            let r = app.clone().oneshot(req).await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn a_wrong_key_is_rejected() {
        let d = tmpdir();
        let (app, _) = app(&d, Some("secret"), false);
        for bad in ["Bearer wrong", "Bearer secretx", "Bearer ", "secret"] {
            let r = app
                .clone()
                .oneshot(
                    HttpRequest::get("/tags")
                        .header("authorization", bad)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "accepted {bad:?}");
        }
    }

    #[tokio::test]
    async fn blob_upload_download_round_trip() {
        let d = tmpdir();
        let (app, store) = app(&d, None, false);
        let data = vec![7u8; 100_000];
        let digest = {
            use sha2::Digest as _;
            let mut h = sha2::Sha256::new();
            h.update(&data);
            hex::encode(h.finalize())
        };

        let r = app
            .clone()
            .oneshot(
                HttpRequest::put(format!("/blobs/{digest}"))
                    .body(Body::from(data.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);

        // A second upload of the same bytes deduplicates.
        let r = app
            .clone()
            .oneshot(
                HttpRequest::put(format!("/blobs/{digest}"))
                    .body(Body::from(data.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(store.list_blobs().await.unwrap().len(), 1);

        let r = app
            .clone()
            .oneshot(
                HttpRequest::head(format!("/blobs/{digest}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers().get(header::CONTENT_LENGTH).unwrap(), "100000");

        let r = app
            .oneshot(
                HttpRequest::get(format!("/blobs/{digest}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let got = to_bytes(r.into_body(), usize::MAX).await.unwrap();
        assert_eq!(got.as_ref(), data.as_slice());
    }

    #[tokio::test]
    async fn uploading_the_wrong_bytes_is_a_conflict() {
        let d = tmpdir();
        let (app, _) = app(&d, None, false);
        let wrong = hex::encode([0u8; 32]);
        let r = app
            .oneshot(
                HttpRequest::put(format!("/blobs/{wrong}"))
                    .body(Body::from("not what the name says"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert!(body_string(r).await.contains("digest_mismatch"));
    }

    #[tokio::test]
    async fn a_missing_blob_is_404_and_a_bad_digest_is_400() {
        let d = tmpdir();
        let (app, _) = app(&d, None, false);
        let absent = hex::encode([9u8; 32]);
        let r = app
            .clone()
            .oneshot(
                HttpRequest::get(format!("/blobs/{absent}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        let r = app
            .oneshot(
                HttpRequest::get("/blobs/not-a-digest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn tags_round_trip_and_delete() {
        let d = tmpdir();
        let (app, store) = app(&d, None, false);
        let info = store.insert_bytes(b"tagged".to_vec()).await.unwrap();

        let r = app
            .clone()
            .oneshot(
                HttpRequest::put("/tags/debian")
                    .body(Body::from(info.digest.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);

        let r = app
            .clone()
            .oneshot(HttpRequest::get("/tags").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(body_string(r).await.contains("debian"));

        let r = app
            .clone()
            .oneshot(
                HttpRequest::delete("/tags/debian")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);

        let r = app
            .oneshot(
                HttpRequest::delete("/tags/debian")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn one_tag_can_be_read_without_listing_them_all() {
        let d = tmpdir();
        let (app, store) = app(&d, None, false);
        let info = store.insert_bytes(b"tagged".to_vec()).await.unwrap();
        store
            .set_tag(&TagName::parse("web").unwrap(), &info.digest)
            .await
            .unwrap();

        let r = app
            .clone()
            .oneshot(HttpRequest::get("/tags/web").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["tag"], "web");
        assert_eq!(v["digest"], info.digest.as_str());

        let r = app
            .clone()
            .oneshot(HttpRequest::get("/tags/absent").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        // A name the store would never write is a bad request, not a 404.
        let r = app
            .oneshot(HttpRequest::get("/tags/.hidden").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_dockerfile_manifest_round_trips_over_http() {
        // The push path heyctl uses: PUT the blobs, PUT the manifest, move a
        // tag onto it, and read the whole thing back by tag.
        let d = tmpdir();
        let (app, store) = app(&d, None, false);
        let recipe = store
            .insert_bytes(b"FROM debian\nRUN true\n".to_vec())
            .await
            .unwrap();
        let m = crate::dockerfile::manifest_for(
            &recipe,
            None,
            &crate::dockerfile::Options {
                image_name: Some("web".into()),
                size_mb: Some(2048),
                source: None,
            },
        );

        let r = app
            .clone()
            .oneshot(
                HttpRequest::put("/manifests")
                    .header("content-type", "application/json")
                    .body(Body::from(m.to_canonical_json()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        let digest = serde_json::from_str::<serde_json::Value>(&body_string(r).await).unwrap()
            ["digest"]
            .as_str()
            .unwrap()
            .to_string();

        app.clone()
            .oneshot(
                HttpRequest::put("/tags/web-recipe")
                    .body(Body::from(digest))
                    .unwrap(),
            )
            .await
            .unwrap();

        let r = app
            .oneshot(
                HttpRequest::get("/manifests/web-recipe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let back: Manifest = serde_json::from_str(&body_string(r).await).unwrap();
        assert!(crate::dockerfile::is_dockerfile(&back));
        assert_eq!(
            crate::dockerfile::dockerfile_entry(&back).unwrap().digest,
            recipe.digest
        );
        assert_eq!(crate::dockerfile::size_mb(&back), Some(2048));
    }

    #[tokio::test]
    async fn manifest_round_trips_and_resolves_by_tag() {
        let d = tmpdir();
        let (app, store) = app(&d, None, false);
        let blob = store.insert_bytes(b"member".to_vec()).await.unwrap();
        let m = Manifest::new(crate::manifest::KIND_GENERIC).with_entry(
            "rootfs.ext4",
            blob.digest.clone(),
            blob.size,
        );

        let r = app
            .clone()
            .oneshot(
                HttpRequest::put("/manifests")
                    .header("content-type", "application/json")
                    .body(Body::from(m.to_canonical_json()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        let digest = serde_json::from_str::<serde_json::Value>(&body_string(r).await).unwrap()
            ["digest"]
            .as_str()
            .unwrap()
            .to_string();

        app.clone()
            .oneshot(
                HttpRequest::put("/tags/img")
                    .body(Body::from(digest.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();

        for reference in [digest.as_str(), "img"] {
            let r = app
                .clone()
                .oneshot(
                    HttpRequest::get(format!("/manifests/{reference}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK, "failed for {reference}");
            assert!(body_string(r).await.contains("rootfs.ext4"));
        }
    }

    #[tokio::test]
    async fn read_only_mode_rejects_writes_but_serves_reads() {
        let d = tmpdir();
        let (app, store) = app(&d, None, true);
        let info = store.insert_bytes(b"served".to_vec()).await.unwrap();

        let r = app
            .clone()
            .oneshot(
                HttpRequest::put(format!("/blobs/{}", info.digest))
                    .body(Body::from("served"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Forbidden, not Unauthorized — no key would make this work.
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(body_string(r).await.contains("read_only"));

        let r = app
            .oneshot(
                HttpRequest::get(format!("/blobs/{}", info.digest))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    /// The API router and the dashboard router are separate, with separate
    /// credentials. These tests pin that separation, because the failure mode of
    /// getting it wrong is silent: a dashboard reachable with the machine key,
    /// or a store listing readable with none.
    fn dashboard_app(d: &tempfile::TempDir, password: Option<&str>) -> Router {
        let store = store_at(d);
        let web = password.map(|p| crate::web::WebState {
            store,
            auth: Some(Arc::new(crate::admin::AdminAuth::new(
                "admin".into(),
                p.into(),
            ))),
        });
        web_router(web)
    }

    /// The third state: mounted, no gate. Only reachable from an explicit
    /// `ART_DASHBOARD_OPEN`, which [`crate::config::DashboardAccess::resolve`]
    /// is what enforces.
    fn open_dashboard_app(d: &tempfile::TempDir) -> Router {
        web_router(Some(crate::web::WebState {
            store: store_at(d),
            auth: None,
        }))
    }

    fn store_at(d: &tempfile::TempDir) -> Store {
        Store::open(&Config {
            root: d.path().join("store"),
            min_free_bytes: 0,
            gc_min_age: Duration::ZERO,
            heyvm_images_dir: d.path().join("images"),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn dashboard_is_not_mounted_without_a_password() {
        // Off, not open.
        let d = tmpdir();
        let app = dashboard_app(&d, None);
        for path in ["/", "/dashboard", "/login", "/dashboard/blobs"] {
            let r = app
                .clone()
                .oneshot(HttpRequest::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path} was served");
        }
    }

    #[tokio::test]
    async fn an_open_dashboard_serves_every_page_without_credentials() {
        // The private-network deployment: the gate is off because the operator
        // said so, not because a variable went missing.
        let d = tmpdir();
        let app = open_dashboard_app(&d);
        for path in ["/dashboard", "/dashboard/blobs", "/dashboard/manifests"] {
            let r = app
                .clone()
                .oneshot(HttpRequest::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK, "{path}");
        }
    }

    #[tokio::test]
    async fn an_open_dashboard_has_no_login_and_no_sign_out() {
        // A login form that accepts anything, or a sign-out button that ends no
        // session, would both be theatre. Neither is offered.
        let d = tmpdir();
        let app = open_dashboard_app(&d);
        let r = app
            .clone()
            .oneshot(HttpRequest::get("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers().get(header::LOCATION).unwrap(), "/dashboard");

        let r = app
            .oneshot(HttpRequest::get("/dashboard").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let html = body_string(r).await;
        assert!(!html.contains("sign out"), "sign-out button on open dashboard");
        assert!(!html.contains("/logout"), "logout form on open dashboard");
    }

    #[tokio::test]
    async fn an_open_dashboard_does_not_open_the_api() {
        // The two gates stay independent in both directions, and the merge is
        // where that could quietly stop being true: `serve` hands both routers
        // to one listener, so this composes them the same way.
        let d = tmpdir();
        let store = store_at(&d);
        let api = router(ServeState {
            store: store.clone(),
            api_key: Some(Arc::new("secret".to_string())),
            read_only: false,
        });
        let merged = api.merge(web_router(Some(crate::web::WebState { store, auth: None })));

        let r = merged
            .clone()
            .oneshot(HttpRequest::get("/tags").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "/tags went open");

        let r = merged
            .oneshot(HttpRequest::get("/dashboard").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "/dashboard should be open");
    }

    #[tokio::test]
    async fn dashboard_redirects_to_login_when_unauthenticated() {
        let d = tmpdir();
        let app = dashboard_app(&d, Some("pw"));
        for path in ["/dashboard", "/dashboard/blobs", "/dashboard/manifests"] {
            let r = app
                .clone()
                .oneshot(HttpRequest::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::SEE_OTHER, "{path}");
            assert_eq!(r.headers().get(header::LOCATION).unwrap(), "/login");
        }
    }

    #[tokio::test]
    async fn the_machine_api_key_does_not_open_the_dashboard() {
        // The two identities are separate on purpose. A bearer token that works
        // on /blobs must not work on /dashboard.
        let d = tmpdir();
        let app = dashboard_app(&d, Some("pw"));
        let r = app
            .oneshot(
                HttpRequest::get("/dashboard")
                    .header("authorization", "Bearer pw")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn login_sets_a_hardened_cookie_that_opens_the_dashboard() {
        let d = tmpdir();
        let app = dashboard_app(&d, Some("pw"));

        let r = app
            .clone()
            .oneshot(
                HttpRequest::post("/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("user=admin&password=pw"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let cookie = r
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        // The password must never be what the cookie carries.
        assert!(!cookie.contains("pw;"), "{cookie}");

        let value = cookie.split(';').next().unwrap().to_string();
        let r = app
            .oneshot(
                HttpRequest::get("/dashboard")
                    .header(header::COOKIE, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let html = body_string(r).await;
        assert!(html.contains("effective capacity"));
        // The VM has no route to a CDN, so a remote asset would simply not load.
        assert!(!html.contains("http://"), "dashboard must not link out");
        assert!(!html.contains("https://"), "dashboard must not link out");
        assert!(!html.contains("<script"), "dashboard must not need JS");
    }

    #[tokio::test]
    async fn a_wrong_password_does_not_set_a_cookie() {
        let d = tmpdir();
        let app = dashboard_app(&d, Some("pw"));
        let r = app
            .oneshot(
                HttpRequest::post("/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("user=admin&password=wrong"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert!(r.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn basic_auth_works_for_scripted_checks() {
        let d = tmpdir();
        let app = dashboard_app(&d, Some("pw"));
        let header_value = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("admin:pw")
        );
        let r = app
            .oneshot(
                HttpRequest::get("/dashboard")
                    .header("authorization", header_value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn usage_reports_logical_and_physical() {
        let d = tmpdir();
        let (app, store) = app(&d, None, false);
        store.insert_bytes(vec![0u8; 4096]).await.unwrap();
        let r = app
            .oneshot(HttpRequest::get("/usage").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["blobs"], 1);
        assert!(v["fsTotal"].as_u64().unwrap() > 0);
    }
}
