//! Provisioning surface for dedicated databases — a JSON admin API plus the
//! dashboard's HTML form, both over the same store and the same validation.
//!
//! Both live behind the dashboard's Basic-auth layer (see `router`), so the
//! credential that can provision a database is the same one that can already
//! stop and resize every VM. There is no separate token: adding one would be a
//! second secret to rotate for no extra isolation.
//!
//! The password is the one thing here that is only ever shown **once**. It is
//! generated (or accepted) at provisioning time, returned in that one response,
//! and never rendered again — the listing endpoints and the dashboard table
//! carry no secrets. An operator who loses it can read `dedicated.tsv` on the
//! host, which is where it lives anyway.

use axum::extract::{Form, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use maud::Markup;
use serde::{Deserialize, Serialize};

use crate::dedicated;

use super::error::AppError;
use super::handlers::Banner;
use super::state::DashState;
use super::views;

/// Request body for `POST /api/databases`.
///
/// Only `database` is required: `username` defaults to the database name (the
/// common single-tenant-app shape) and `password` to a freshly generated one,
/// so the minimal call is `{"database":"acme"}`.
#[derive(Deserialize)]
pub struct CreateRequest {
    pub database: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

/// Response body for a successful provision — the only place the password is
/// ever returned.
#[derive(Serialize)]
pub struct CreateResponse {
    pub database: String,
    pub username: String,
    pub password: String,
    /// `provisioning` — the credential is live immediately (a client can
    /// connect right now); the VM is being brought up in the background and the
    /// first connection waits for it either way.
    pub status: &'static str,
    pub created_at: u64,
}

/// One record in `GET /api/databases`. No password, by construction.
#[derive(Serialize)]
pub struct DatabaseInfo {
    pub database: String,
    pub username: String,
    pub created_at: u64,
    /// Sandbox id backing it, once one exists — `null` before the first
    /// bring-up completes.
    pub sandbox_id: Option<String>,
    /// Storage tier from the schema registry (`live`, `compacted`, `frozen`,
    /// `archived`), or `null` when the pooler has never backed it yet.
    pub tier: Option<&'static str>,
}

#[derive(Serialize)]
pub struct ApiError {
    pub error: String,
}

/// `POST /api/databases` — provision a dedicated database.
///
/// `201` with the credential on success. Validation failures (a taken name, a
/// bad identifier, a short password) are `400` with the reason, because they
/// are all things the caller can fix by changing the request.
pub async fn api_create(
    State(st): State<DashState>,
    Json(req): Json<CreateRequest>,
) -> Response {
    match provision(&st, &req.database, req.username.as_deref(), req.password.as_deref()) {
        Ok(cred) => (
            StatusCode::CREATED,
            Json(CreateResponse {
                database: cred.database,
                username: cred.role,
                password: cred.password,
                status: "provisioning",
                created_at: cred.created_at,
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("{e:#}"),
            }),
        )
            .into_response(),
    }
}

/// `GET /api/databases` — every provisioned database, joined with whatever the
/// schema registry knows about the VM behind it. Never includes passwords.
pub async fn api_list(State(st): State<DashState>) -> Json<Vec<DatabaseInfo>> {
    Json(rows(&st))
}

/// `DELETE /api/databases/{database}` — revoke the credential.
///
/// Non-destructive on purpose: the VM, its disk and its data survive. The name
/// simply drops back to ordinary schema routing, which is also how an operator
/// gets at the data afterwards. Reclaiming the storage stays with the existing
/// reap/purge controls, which is where the confirmation for a destructive act
/// already lives.
pub async fn api_delete(State(st): State<DashState>, Path(database): Path<String>) -> Response {
    match st.registry.dedicated().remove(&database) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("no dedicated database named {database:?}"),
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("{e:#}"),
            }),
        )
            .into_response(),
    }
}

/// The dashboard page: the provisioned list plus the create form.
pub async fn page(
    State(st): State<DashState>,
    Query(banner): Query<Banner>,
) -> Result<Markup, AppError> {
    Ok(views::dedicated_page(&st, &rows(&st), &banner, None))
}

/// Form fields posted by the dashboard's create form. Empty strings mean
/// "unset" here — an HTML form always submits its inputs, so a blank optional
/// field arrives as `""` rather than being absent.
#[derive(Deserialize)]
pub struct CreateForm {
    pub database: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

/// `POST /dedicated` — the dashboard's create action.
///
/// Renders the result page directly instead of the usual Post-Redirect-Get,
/// because the response carries the generated password: a redirect would put
/// it in a URL, and from there into browser history, the referer header and
/// any proxy log in between.
pub async fn create(State(st): State<DashState>, Form(f): Form<CreateForm>) -> Markup {
    let blank_to_none = |s: &str| Some(s.trim()).filter(|s| !s.is_empty()).map(str::to_string);
    let username = blank_to_none(&f.username);
    let password = blank_to_none(&f.password);
    let outcome = provision(&st, &f.database, username.as_deref(), password.as_deref());
    views::dedicated_page(&st, &rows(&st), &Banner::default(), Some(&outcome))
}

/// `POST /dedicated/{database}/delete` — the dashboard's revoke action. No
/// secret in the response, so this one does redirect normally.
pub async fn delete(State(st): State<DashState>, Path(database): Path<String>) -> Redirect {
    let query = match st.registry.dedicated().remove(&database) {
        Ok(true) => format!(
            "msg={}",
            super::handlers::qenc("credential revoked - the VM and its data were left untouched")
        ),
        Ok(false) => format!("err={}", super::handlers::qenc("no such dedicated database")),
        Err(e) => format!("err={}", super::handlers::qenc(&e.to_string())),
    };
    Redirect::to(&format!("/dedicated?{query}"))
}

/// Shared by the API and the form: resolve the optional fields, record the
/// credential, and kick off a background bring-up so the tenant's first
/// connection doesn't pay a cold start.
fn provision(
    st: &DashState,
    database: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> anyhow::Result<dedicated::Credential> {
    let database = database.trim();
    // Default the role to the database name: for the single-app-per-database
    // shape this feature exists for, a separate name is noise.
    let role = username.map(str::trim).unwrap_or(database);
    let password = match password {
        Some(p) => p.to_string(),
        None => dedicated::generate_password()?,
    };
    let cred = st.registry.create_dedicated(database, role, &password)?;
    st.registry.spawn_provision(&cred.database);
    Ok(cred)
}

/// Join the provisioned credentials with the schema registry's durable record
/// for each, so both surfaces show where the data currently lives.
fn rows(st: &DashState) -> Vec<DatabaseInfo> {
    st.registry
        .dedicated()
        .list()
        .into_iter()
        .map(|c| {
            let record = st.registry.store_record(&c.database);
            DatabaseInfo {
                database: c.database,
                username: c.role,
                created_at: c.created_at,
                sandbox_id: record.as_ref().map(|r| r.sandbox_id.clone()),
                tier: record.as_ref().map(|r| r.tier.as_str()),
            }
        })
        .collect()
}
