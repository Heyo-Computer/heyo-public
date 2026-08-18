//! The HTTP surface: a server-rendered dashboard plus the machine endpoints.
//!
//! **Auth is app-lb's job, not ours.** Deployed behind an app-lb `AuthGate`, a
//! browser request arrives carrying `x-auth-request-email` / `-user` / `-name`,
//! which app-lb strips unconditionally before setting, so they cannot be
//! spoofed. This module reads them and never re-implements a login.
//!
//! One consequence shapes every route below: **an app-lb gate admits browsers
//! and nothing else.** The split is `Accept: text/html`, so curl, `git submit`,
//! and a page's own `EventSource` all get `401 {"error":"authentication
//! required"}`. Machine routes therefore live under `/api` and are listed in the
//! deployment's `public_paths`, each carrying its own credential — the submit
//! endpoint a repository token or an HMAC, the log stream a short-TTL
//! run-scoped token minted by the page that opens it.
//!
//! The repository-management routes are the mirror image: they are *not* in
//! `public_paths`, precisely because minting a submit token is minting the right
//! to run code on a runner. They are for browsers, they run behind the gate, and
//! they check an admin role on top of it.

pub mod identity;
pub mod pages;
pub mod stream;

use crate::config::Config;
use crate::dispatch::Dispatcher;
use crate::repos;
use crate::runners::Runners;
use crate::store::{Repo, Store};
use crate::trigger;
use axum::Form;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use identity::Identity;
use pages::{RepoFlash, RepoView};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub runners: Arc<Runners>,
    pub store: Store,
    pub dispatcher: Arc<Dispatcher>,
}

pub fn router(
    config: Arc<Config>,
    runners: Arc<Runners>,
    store: Store,
    dispatcher: Arc<Dispatcher>,
) -> Router {
    // The submit body carries a whole source tree, so it needs its own limit —
    // axum defaults to 2 MiB, which no real repository fits in. The real ceiling
    // is `CI_MAX_SOURCE_BYTES`, checked after decoding; this one just keeps a
    // hostile body from being buffered first. Base64 inflates by 4/3, plus room
    // for the JSON envelope.
    let submit_limit = config.max_source_bytes.saturating_mul(4) / 3 + (1 << 20);

    let state = AppState {
        config,
        runners,
        store,
        dispatcher,
    };

    Router::new()
        // Unauthenticated by design and listed in `public_paths`: app-lb probes
        // it after an `update` job to decide whether the service actually came
        // back, so a gate in front of it would make every deploy look failed.
        .route("/healthz", get(healthz))
        .route("/", get(runs_page))
        .route("/runs/{run_id}", get(run_page))
        // Admin-only like the other state-changing routes: cancelling stops
        // somebody's build.
        .route("/runs/{run_id}/cancel", post(cancel_run))
        .route("/runs/{run_id}/jobs/{job_key}", get(job_page))
        // `/runners` was this page's name when there was one network to show.
        // Kept because it is in people's history and in the README of a running
        // deployment; both render the same page.
        .route("/networks", get(networks_page))
        .route("/runners", get(networks_page))
        // Behind the gate and admin-only, like /repos: joining a host to a
        // network grants host-shell access to it through the network, so it is
        // not a read.
        .route("/networks/{network_id}/join", post(join_network))
        .route("/vms", get(vms_page))
        // Admin-only: destroying a VM is destroying somebody's warm cache, and
        // on a claimed one it would fail a live build — which is why the pool
        // refuses those rather than trusting the page not to offer them.
        .route("/vms/{sandbox_id}/destroy", post(destroy_vm))
        .route("/vms/cleanup-failed", post(cleanup_failed_vms))
        .route("/workflows", get(workflows_page))
        // Behind the gate on purpose, and admin-only on top of it: a submit
        // token is the right to run code on a runner, so minting one is not a
        // read.
        .route("/repos", get(repos_page).post(register_repo))
        .route("/repos/{repo_id}/tokens", post(create_repo_token))
        .route(
            "/repos/{repo_id}/tokens/{token_id}/revoke",
            post(revoke_repo_token),
        )
        .route("/repos/{repo_id}/enabled", post(set_repo_enabled))
        .route("/repos/{repo_id}/network", post(set_repo_network))
        .route("/repos/{repo_id}/delete", post(delete_repo))
        // In `public_paths`, because an `EventSource` sends
        // `Accept: text/event-stream` and app-lb's gate admits only
        // `text/html`. It carries its own run-scoped token instead.
        .route("/api/stream/{run_id}/{job_key}", get(log_stream))
        .route(
            "/api/submit",
            post(submit).layer(DefaultBodyLimit::max(submit_limit)),
        )
        .with_state(state)
}

/// How a submit proved it may start a build.
enum Credential {
    /// A per-repository token, and the registration it is scoped to.
    Repo(Box<Repo>),
    /// The installation-wide `CI_WEBHOOK_SECRET`, HMAC'd over the raw body.
    /// Says nothing about which repository is submitting, which is the reason
    /// the other one exists.
    Shared,
}

/// The `Bearer` a submit token arrives as.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    // Case-insensitive on the scheme, per RFC 7235; curl and every client
    // library spell it `Bearer`, but a shell script pasting `bearer` should not
    // fail with "not signed correctly".
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// Decide whether a submit may proceed, before its body is parsed.
///
/// The ordering is the security property: a `Json` extractor would deserialize
/// an unauthenticated body first, and the credential check would then be
/// guarding a decision already made.
async fn authenticate_submit(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Credential, axum::response::Response> {
    if let Some(token) = bearer(headers) {
        return match state.store.authenticate_repo_token(token).await {
            Ok(Some(repo)) => Ok(Credential::Repo(Box::new(repo))),
            // One message for malformed, unknown, wrong, revoked and disabled
            // alike: saying which one it was tells an attacker how far they got.
            Ok(None) => {
                tracing::debug!("rejected a submit token that resolves to no repository");
                Err(error(
                    StatusCode::UNAUTHORIZED,
                    &format!(
                        "that submit token is not valid for any registered repository. \
                         Register the repository at {}/repos, then \
                         `git config ci.token <token>`.",
                        state.config.public_url
                    ),
                ))
            }
            Err(e) => {
                // A database failure is ours, not the caller's, and it must not
                // read as a rejected credential — that sends someone to rotate
                // a token that was fine.
                tracing::error!("could not check a submit token: {e}");
                Err(error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not check that submit token; the database is unreachable",
                ))
            }
        };
    }

    if state.config.require_repo_token {
        tracing::debug!("rejected a shared-secret submit; CI_REQUIRE_REPO_TOKEN is set");
        return Err(error(
            StatusCode::UNAUTHORIZED,
            &format!(
                "this server accepts only per-repository submit tokens \
                 (CI_REQUIRE_REPO_TOKEN is set). Register the repository at \
                 {}/repos, then `git config ci.token <token>`.",
                state.config.public_url
            ),
        ));
    }

    let signature = headers
        .get(trigger::SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok());
    match trigger::verify_signature(&state.config.webhook_secret, body, signature) {
        Ok(()) => Ok(Credential::Shared),
        Err(e) => {
            // Logged at debug, not warn: an unauthenticated public endpoint gets
            // scanned, and a warn per probe is how a log becomes unreadable.
            tracing::debug!("rejected an unsigned submit: {e}");
            Err(error(e.status(), &e.to_string()))
        }
    }
}

/// `POST /api/submit` — what `git submit` calls.
async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let credential = match authenticate_submit(&state, &headers, &body).await {
        Ok(c) => c,
        Err(response) => return response,
    };

    let req: trigger::SubmitRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("malformed submit: {e}")),
    };

    // The shared secret cannot say which repository it is for, but the payload
    // can, and a registration matching it still decides the workflow glob and
    // gives the run a home on the repositories page. This is not a privilege
    // grant: whoever holds the installation-wide secret may already submit as
    // anything, which is the weakness the token path exists to fix.
    let matched;
    let repo = match &credential {
        Credential::Repo(r) => Some(r.as_ref()),
        Credential::Shared if !req.repository.url.trim().is_empty() => {
            matched = state
                .store
                .repo_by_url(&req.repository.url)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!("could not look up {:?}: {e}", req.repository.url);
                    None
                })
                .filter(|r| r.enabled);
            matched.as_ref()
        }
        Credential::Shared => None,
    };

    // A token is scoped to one repository, and this is where that is worth
    // anything: without it, a token issued for a repository somebody may push to
    // would build any repository at all, which is a build of *their* workflow
    // file with *this* repository's secrets.
    if let Some(repo) = repo
        && !req.repository.url.trim().is_empty()
        && !repos::same_repo(&repo.url, &req.repository.url)
    {
        tracing::warn!(
            "refused a submit for {:?} with a token for {:?}",
            req.repository.url,
            repo.url
        );
        return error(
            StatusCode::FORBIDDEN,
            &format!(
                "this submit token is registered to {}, but the submit is for {}. \
                 Use the token minted for that repository.",
                repo.url, req.repository.url
            ),
        );
    }

    // Present only when a browser reached this through app-lb's gate; `git
    // submit` arrives with a token and no identity, so the payload's `pusher` is
    // the fallback.
    let who = Identity::from_headers(&headers);

    match state.dispatcher.submit(&req, who.as_ref(), repo).await {
        Ok(submitted) => {
            tracing::info!(
                "accepted a submit for {} ({}) via {}: {} run(s)",
                repo.map(|r| r.name.as_str())
                    .unwrap_or(&req.repository.name),
                req.branch(),
                match repo {
                    Some(r) => format!("a token for {}", r.name),
                    None => "the shared secret".to_string(),
                },
                submitted.run_ids.len()
            );
            (
                StatusCode::ACCEPTED,
                axum::Json(serde_json::json!({
                    "runs": submitted.run_ids,
                    "url": format!("{}/", state.config.public_url),
                    // Warnings, not errors: the runs exist. A job pinned to a
                    // host that is briefly offline waits rather than failing,
                    // and the client says so at the terminal that submitted it.
                    "warnings": submitted.warnings,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!("submit failed: {e}");
            error(StatusCode::BAD_REQUEST, &e.to_string())
        }
    }
}

fn error(status: StatusCode, message: &str) -> axum::response::Response {
    (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
}

async fn healthz() -> &'static str {
    "ok\n"
}

/// Where the executor records a VM's own console. Mirrors checkout at `-1`; see
/// `Dispatcher::capture_vm_log`.
const VM_LOG_STEP_IDX: i32 = -2;

/// How many runs the landing page shows. A dashboard is for "what happened
/// recently"; anything older is a query, not a scroll.
const RECENT_RUNS: i64 = 50;

fn who_of(headers: &HeaderMap) -> Option<Identity> {
    Identity::from_headers(headers)
}

async fn runs_page(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let who = who_of(&headers);
    // `?repo=<id>` narrows to one registered repository. An id that matches
    // nothing filters everything out rather than erroring — the page says "no
    // runs" and offers the way back, which is the right answer for a stale
    // bookmark of a deleted registration.
    let repo = q.get("repo").map(String::as_str).filter(|r| !r.is_empty());
    let repos = match state.store.repos().await {
        Ok(repos) => repos,
        Err(e) => return page_error(&state, who.as_ref(), &e.to_string()),
    };
    match state.store.recent_runs(RECENT_RUNS, repo).await {
        Ok(runs) => pages::runs_page(
            &state.config.name,
            who.as_ref().map(|i| i.display()),
            &runs,
            &repos,
            repo,
        )
        .into_response(),
        Err(e) => page_error(&state, who.as_ref(), &e.to_string()),
    }
}

async fn run_page(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let who = who_of(&headers);
    let name = who.as_ref().map(|i| i.display());
    match state.store.get_run(&run_id).await {
        Ok(Some(run)) => {
            let jobs = state.store.jobs_of(&run_id).await.unwrap_or_default();
            let artifacts = state.store.artifacts_of(&run_id).await.unwrap_or_default();

            // The VM log is the step recorded at index -2 by the executor. Read
            // from the same place as any other step log, so a swept run shows
            // the row with the bytes gone rather than vanishing from the page.
            let mut vm_logs = Vec::new();
            for job in &jobs {
                let steps = state.store.steps_of(&job.id).await.unwrap_or_default();
                if let Some(step) = steps.iter().find(|s| s.idx == VM_LOG_STEP_IDX) {
                    vm_logs.push((job.display.clone(), state.store.read_log(step).await));
                }
            }

            pages::run_page(
                &state.config.name,
                name,
                &run,
                &jobs,
                &artifacts,
                &vm_logs,
                state.config.log_retention.map(|d| d.as_secs() / 86_400),
            )
            .into_response()
        }
        Ok(None) => not_found(&state, who.as_ref(), &format!("No run {run_id}.")),
        Err(e) => page_error(&state, who.as_ref(), &e.to_string()),
    }
}

/// `POST /runs/{id}/cancel` — stop a run.
///
/// Marks the run and every unfinished job cancelled, which is all it takes to
/// stop work in each of the three states it might be in: a queued job is dropped
/// when JetStream delivers it, a job about to start is refused by `start_job`,
/// and a running one notices at its next step boundary. Nothing has to reach a
/// runner.
async fn cancel_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };

    match state.store.cancel_run(&run_id).await {
        Ok(Some(jobs)) => {
            tracing::info!(
                "cancelled run {run_id} ({jobs} job(s)) by {}",
                who.as_ref().map(|w| w.display()).unwrap_or("anonymous")
            );
        }
        Ok(None) => {
            tracing::debug!("run {run_id} was already finished; nothing to cancel");
        }
        Err(e) => return page_error(&state, who.as_ref(), &e.to_string()),
    }
    // Back to the run, which now shows what happened — rather than a flash on a
    // page the browser would re-post on refresh.
    axum::response::Redirect::to(&format!("/runs/{run_id}")).into_response()
}

async fn job_page(
    State(state): State<AppState>,
    Path((run_id, job_key)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let who = who_of(&headers);
    let name = who.as_ref().map(|i| i.display());

    let Ok(Some(run)) = state.store.get_run(&run_id).await else {
        return not_found(&state, who.as_ref(), &format!("No run {run_id}."));
    };
    let jobs = state.store.jobs_of(&run_id).await.unwrap_or_default();
    let Some(job) = jobs.into_iter().find(|j| j.job_key == job_key) else {
        return not_found(
            &state,
            who.as_ref(),
            &format!("Run {run_id} has no job {job_key}."),
        );
    };

    let mut steps = Vec::new();
    for step in state.store.steps_of(&job.id).await.unwrap_or_default() {
        let log = state.store.read_log(&step).await.unwrap_or_default();
        steps.push((step, log));
    }

    // Only mint a token while there is still something to stream. A finished
    // job gets a static page, and no credential is handed out that nothing
    // needs.
    let token = (!matches!(
        job.status.as_str(),
        "success" | "failure" | "skipped" | "cancelled"
    ))
    .then(|| stream::mint(&state.config, &run_id, &job_key));

    pages::job_page(
        &state.config.name,
        name,
        &run,
        &job,
        &steps,
        token.as_deref(),
    )
    .into_response()
}

async fn workflows_page(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let who = who_of(&headers);
    let name = who.as_ref().map(|i| i.display());
    match state.store.recent_runs(500, None).await {
        Ok(runs) => {
            // Newest first already, so the first sighting of an id is its most
            // recent run.
            let mut seen: Vec<(String, Option<crate::store::Run>)> = Vec::new();
            for r in runs {
                if !seen.iter().any(|(id, _)| *id == r.workflow_id) {
                    seen.push((r.workflow_id.clone(), Some(r)));
                }
            }
            pages::workflows_page(
                &state.config.name,
                name,
                &seen,
                &state.config.default_workflow_path,
            )
            .into_response()
        }
        Err(e) => page_error(&state, who.as_ref(), &e.to_string()),
    }
}

// ---- registered repositories --------------------------------------------

/// Who may register a repository and mint a token for it.
///
/// Two admissible cases, and the second one is a deliberate trade:
///
/// - **A gated deployment.** app-lb forwards an identity, and the person behind
///   it must hold the `admin` role in `ci_user`, seeded from `CI_ADMIN_EMAILS`.
/// - **A deployment that forwards no identity at all and names no admins.**
///   That is the local loop: no gate, no accounts, and anyone who can reach the
///   dashboard can already read every build log. Refusing here would leave the
///   page unusable in the one configuration it is developed in.
///
/// The moment `CI_ADMIN_EMAILS` names anybody, an anonymous request is refused —
/// an installation that has declared who its admins are has said that "nobody in
/// particular" is not one of them.
async fn may_manage(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<Identity>, axum::response::Response> {
    check_origin(state, headers)?;

    let who = Identity::from_headers(headers);
    let Some(who) = who else {
        if state.config.admin_emails.is_empty() {
            return Ok(None);
        }
        return Err(refused(
            state,
            None,
            StatusCode::UNAUTHORIZED,
            "This request carries no identity, and this installation names its admins in \
             CI_ADMIN_EMAILS. Reach this page through the app-lb gate that forwards \
             x-auth-request-user.",
        ));
    };

    match state
        .store
        .upsert_user(
            &who.subject,
            &who.email,
            who.name.as_deref(),
            &state.config.admin_emails,
        )
        .await
    {
        Ok(role) if role == "admin" => Ok(Some(who)),
        Ok(_) => Err(refused(
            state,
            Some(&who),
            StatusCode::FORBIDDEN,
            "Registering a repository mints a credential that can run code on a runner, \
             so it is admin-only. Ask someone on CI_ADMIN_EMAILS to add you.",
        )),
        Err(e) => {
            tracing::error!("could not resolve a role: {e}");
            Err(page_error(state, Some(&who), &e.to_string()))
        }
    }
}

/// Refuse a request that a different site made on a logged-in browser's behalf.
///
/// These routes are POST forms authenticated by an app-lb session cookie, which
/// a cross-site form submission carries just as happily as the real page does.
/// Without this, a page anywhere could delete a registration — or register one —
/// on behalf of whoever is signed in. It could not *read* the minted token back,
/// so this is vandalism rather than credential theft, but it is still somebody
/// else deciding what this installation builds.
///
/// **Absent `Origin` passes**, which is the deliberate limit. A browser sends it
/// on every cross-origin form POST, so the attack this defends against always
/// carries one; `curl` and a scripted client send none, and a cross-site request
/// cannot forge one. It is checked on GET too — a navigation carries no `Origin`
/// and a same-origin fetch carries a matching one, so nothing legitimate is
/// caught.
fn check_origin(state: &AppState, headers: &HeaderMap) -> Result<(), axum::response::Response> {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|o| !o.is_empty() && *o != "null")
    else {
        return Ok(());
    };

    if origin.trim_end_matches('/') == state.config.public_url {
        return Ok(());
    }

    tracing::warn!(
        "refused a repositories request from origin {origin:?}; this app is {:?}",
        state.config.public_url
    );
    Err(refused(
        state,
        None,
        StatusCode::FORBIDDEN,
        "This request came from another site. If it came from this dashboard, \
         CI_PUBLIC_URL does not match the address the browser is using — set it to \
         the URL people actually visit.",
    ))
}

fn refused(
    state: &AppState,
    who: Option<&Identity>,
    status: StatusCode,
    message: &str,
) -> axum::response::Response {
    (
        status,
        pages::layout(
            &state.config.name,
            "repos",
            who.map(|i| i.display()),
            maud::html! {
                div .banner { (message) }
                p { a href="/" { "Back to runs" } }
            },
        ),
    )
        .into_response()
}

/// Render `/repos`, optionally with the outcome of the POST that produced it.
async fn render_repos(
    state: &AppState,
    who: Option<&Identity>,
    flash: RepoFlash,
) -> axum::response::Response {
    let repos = match state.store.repos().await {
        Ok(r) => r,
        Err(e) => return page_error(state, who, &e.to_string()),
    };

    let mut views = Vec::with_capacity(repos.len());
    for repo in repos {
        // A failure to read one repository's tokens must not blank the page;
        // an empty token list is visibly wrong in a way a 500 is not fixable
        // from.
        let tokens = state.store.repo_tokens(&repo.id).await.unwrap_or_default();
        let last_run = state.store.last_run_of_repo(&repo.id).await.ok().flatten();
        views.push(RepoView {
            repo,
            tokens,
            last_run,
        });
    }

    pages::repos_page(
        &state.config.name,
        who.map(|i| i.display()),
        &views,
        &state.config.public_url,
        state.config.require_repo_token,
        &state.runners.snapshot(),
        &flash,
    )
    .into_response()
}

async fn repos_page(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };
    render_repos(&state, who.as_ref(), RepoFlash::default()).await
}

#[derive(serde::Deserialize)]
struct RegisterForm {
    url: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    workflow_path: String,
    #[serde(default)]
    network: String,
}

async fn register_repo(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RegisterForm>,
) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };

    let url = form.url.trim();
    if url.is_empty() {
        return render_repos(
            &state,
            who.as_ref(),
            RepoFlash::failed("A clone URL is required; it is what a submit is matched against."),
        )
        .await;
    }

    let name = match form.name.trim() {
        "" => repos::name_from_url(url),
        given => given.to_string(),
    };
    let workflow_path = Some(form.workflow_path.trim()).filter(|p| !p.is_empty());
    let actor = who.as_ref().map(|w| (w.subject.as_str(), w.email.as_str()));

    // The picker only offers served networks, so anything else arrived from
    // somewhere other than this page. Resolved to the canonical name rather than
    // stored verbatim, so an id typed into the form still reads as a name later.
    let pool = state.runners.snapshot();
    let network = match form.network.trim() {
        "" => None,
        name => match pool.find(name).filter(|s| s.served) {
            Some(set) => Some(set.network_name.clone()),
            None => {
                return render_repos(
                    &state,
                    who.as_ref(),
                    RepoFlash::failed(format!(
                        "This orchestrator does not serve a network named {name:?}. \
                         See Networks for what it does serve."
                    )),
                )
                .await;
            }
        },
    };

    match state
        .store
        .register_repo(url, &name, workflow_path, network.as_deref(), actor)
        .await
    {
        Ok(repo) => {
            tracing::info!(
                "registered repository {} ({}) on network {:?} by {}",
                repo.name,
                repo.normalized,
                repo.network.as_deref().unwrap_or("(default)"),
                who.as_ref().map(|w| w.display()).unwrap_or("anonymous")
            );
            render_repos(
                &state,
                who.as_ref(),
                RepoFlash::done(format!(
                    "{} is registered. Mint a token for it below.",
                    repo.name
                )),
            )
            .await
        }
        Err(e) => render_repos(&state, who.as_ref(), RepoFlash::failed(e.to_string())).await,
    }
}

#[derive(serde::Deserialize)]
struct TokenForm {
    #[serde(default)]
    name: String,
}

/// Mint a token, and render the page that shows it.
///
/// A redirect would be the conventional answer to a POST, but the token can only
/// be shown once and a redirect would have to carry it in the URL — where it
/// lands in browser history, in a `Referer`, and in every access log between
/// here and the browser. So the POST renders its own result.
async fn create_repo_token(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };

    let repo = match state.store.get_repo(&repo_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return render_repos(
                &state,
                who.as_ref(),
                RepoFlash::failed("That repository is not registered any more."),
            )
            .await;
        }
        Err(e) => return page_error(&state, who.as_ref(), &e.to_string()),
    };

    let name = match form.name.trim() {
        "" => who
            .as_ref()
            .map(|w| w.email.clone())
            .unwrap_or_else(|| "unnamed".to_string()),
        given => given.to_string(),
    };
    let actor = who.as_ref().map(|w| (w.subject.as_str(), w.email.as_str()));

    match state.store.create_repo_token(&repo.id, &name, actor).await {
        Ok((token, plaintext)) => {
            tracing::info!(
                "minted submit token {} for {} by {}",
                token.id,
                repo.name,
                who.as_ref().map(|w| w.display()).unwrap_or("anonymous")
            );
            render_repos(
                &state,
                who.as_ref(),
                RepoFlash::minted(repo.name.clone(), plaintext),
            )
            .await
        }
        Err(e) => render_repos(&state, who.as_ref(), RepoFlash::failed(e.to_string())).await,
    }
}

async fn revoke_repo_token(
    State(state): State<AppState>,
    Path((_repo_id, token_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };

    let flash = match state.store.revoke_repo_token(&token_id).await {
        Ok(true) => {
            tracing::info!(
                "revoked submit token {token_id} by {}",
                who.as_ref().map(|w| w.display()).unwrap_or("anonymous")
            );
            RepoFlash::done("That token no longer works. Any build already running is unaffected.")
        }
        Ok(false) => RepoFlash::done("That token was already revoked."),
        Err(e) => RepoFlash::failed(e.to_string()),
    };
    render_repos(&state, who.as_ref(), flash).await
}

#[derive(serde::Deserialize)]
struct NetworkForm {
    /// Empty means "the installation default", which is a real choice and so is
    /// an option in the select rather than an absent field.
    #[serde(default)]
    network: String,
}

/// Point a repository at a heyvm network.
///
/// The chosen network is checked against the pool *now*, so a typo or an
/// unserved network is refused while somebody is looking at the page — rather
/// than at the next submit, by whoever is waiting on a build.
async fn set_repo_network(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<NetworkForm>,
) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };

    let wanted = form.network.trim();
    let pool = state.runners.snapshot();
    let chosen = match wanted {
        "" => None,
        name => match pool.find(name) {
            Some(set) if set.served => Some(set.network_name.clone()),
            Some(set) => {
                return render_repos(
                    &state,
                    who.as_ref(),
                    RepoFlash::failed(format!(
                        "Network {} exists but this orchestrator does not take work for it. \
                         Add it to CI_NETWORK, or set CI_NETWORK=*.",
                        set.network_name
                    )),
                )
                .await;
            }
            None => {
                return render_repos(
                    &state,
                    who.as_ref(),
                    RepoFlash::failed(format!("No heyvm network is named {name:?}.")),
                )
                .await;
            }
        },
    };

    let flash = match state
        .store
        .set_repo_network(&repo_id, chosen.as_deref())
        .await
    {
        Ok(true) => {
            tracing::info!(
                "assigned repository {repo_id} to network {:?} by {}",
                chosen.as_deref().unwrap_or("(default)"),
                who.as_ref().map(|w| w.display()).unwrap_or("anonymous")
            );
            RepoFlash::done(match &chosen {
                Some(n) => format!(
                    "New builds of this repository run in {n}. A job with its own \
                     `uses:` still goes where the workflow says, and anything already \
                     queued keeps the network it was scheduled for."
                ),
                None => "This repository is back on the installation default network.".to_string(),
            })
        }
        Ok(false) => RepoFlash::failed("That repository is not registered."),
        Err(e) => RepoFlash::failed(e.to_string()),
    };
    render_repos(&state, who.as_ref(), flash).await
}

#[derive(serde::Deserialize)]
struct EnabledForm {
    enabled: bool,
}

/// Pause or resume a repository.
///
/// The desired state is submitted rather than flipped, so two admins clicking
/// at once converge instead of toggling each other's decision away.
async fn set_repo_enabled(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<EnabledForm>,
) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };

    let flash = match state.store.set_repo_enabled(&repo_id, form.enabled).await {
        Ok(true) if form.enabled => RepoFlash::done("That repository can submit again."),
        Ok(true) => RepoFlash::done(
            "That repository is paused. Its tokens still exist, and every submit with one \
             is refused until it is resumed. The shared CI_WEBHOOK_SECRET is not affected \
             — it belongs to no repository, so nothing about one can stop it.",
        ),
        Ok(false) => RepoFlash::failed("That repository is not registered."),
        Err(e) => RepoFlash::failed(e.to_string()),
    };
    tracing::info!(
        "set repository {repo_id} enabled={} by {}",
        form.enabled,
        who.as_ref().map(|w| w.display()).unwrap_or("anonymous")
    );
    render_repos(&state, who.as_ref(), flash).await
}

async fn delete_repo(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };

    let flash = match state.store.delete_repo(&repo_id).await {
        Ok(true) => {
            tracing::info!(
                "removed repository registration {repo_id} by {}",
                who.as_ref().map(|w| w.display()).unwrap_or("anonymous")
            );
            RepoFlash::done(
                "The registration and its tokens are gone. Its runs are kept, and a \
                 submit with one of those tokens is now refused.",
            )
        }
        Ok(false) => RepoFlash::failed("That repository is not registered."),
        Err(e) => RepoFlash::failed(e.to_string()),
    };
    render_repos(&state, who.as_ref(), flash).await
}

/// Tail a job's step logs.
///
/// Emits **rendered text appended to a specific step**, not a JSON model the
/// browser assembles into markup — the page stays server-rendered, and the
/// fifteen lines of script only append what arrived. On completion it sends
/// `done`, and the browser reloads so the final state is the server's rendering
/// rather than one stitched together client-side.
async fn log_stream(
    State(state): State<AppState>,
    Path((run_id, job_key)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let token = q.get("token").cloned().unwrap_or_default();
    if !stream::verify(&state.config, &token, &run_id, &job_key) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": "this log stream token is missing, expired, or for another job"
            })),
        )
            .into_response();
    }

    let stream = async_stream::stream! {
        // Byte offsets already sent, per step index. A step's log only ever
        // grows, so an offset is all the state a tail needs.
        let mut sent: HashMap<i32, usize> = HashMap::new();

        loop {
            let Ok(jobs) = state.store.jobs_of(&run_id).await else { break };
            let Some(job) = jobs.into_iter().find(|j| j.job_key == job_key) else { break };

            for step in state.store.steps_of(&job.id).await.unwrap_or_default() {
                let text = state.store.read_log(&step).await.unwrap_or_default();
                let already = *sent.get(&step.idx).unwrap_or(&0);
                if text.len() > already {
                    // Split on a character boundary: a log is arbitrary bytes
                    // and slicing mid-codepoint would panic.
                    let mut cut = already.min(text.len());
                    while cut < text.len() && !text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    let fresh = &text[cut..];
                    if !fresh.is_empty() {
                        let payload = serde_json::json!({ "idx": step.idx, "text": fresh });
                        yield Ok::<Event, std::convert::Infallible>(
                            Event::default().event("log").data(payload.to_string()),
                        );
                    }
                    sent.insert(step.idx, text.len());
                }
            }

            if matches!(
                job.status.as_str(),
                "success" | "failure" | "skipped" | "cancelled"
            ) {
                yield Ok(Event::default().event("done").data("{}"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
    };

    Sse::new(stream)
        // A proxy that sees nothing for a minute will close the connection;
        // a comment every fifteen seconds keeps it open through a slow step.
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

fn page_error(state: &AppState, who: Option<&Identity>, message: &str) -> axum::response::Response {
    tracing::warn!("page render failed: {message}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        pages::layout(
            &state.config.name,
            "",
            who.map(|i| i.display()),
            maud::html! { div .banner { (message) } },
        ),
    )
        .into_response()
}

fn not_found(state: &AppState, who: Option<&Identity>, message: &str) -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        pages::layout(
            &state.config.name,
            "",
            who.map(|i| i.display()),
            maud::html! {
                div .banner { (message) }
                p { a href="/" { "Back to runs" } }
            },
        ),
    )
        .into_response()
}

/// Read every served route's backlog, concurrently.
///
/// One round trip per network and per runner, so they go together and under one
/// deadline — a dashboard must not hang because NATS is slow, and a page with no
/// gauges beats a page that never renders.
async fn queue_depths(state: &AppState) -> pages::QueueDepths {
    let pool = state.runners.snapshot();
    let mut routes = Vec::new();
    for set in pool.served() {
        if !set.network_id.is_empty() {
            routes.push((
                set.network_id.clone(),
                crate::bus::Route::Network(set.network_id.clone()),
            ));
        }
        // Every runner, not just the dispatchable ones. A queue on an offline
        // host is exactly what somebody needs to see — that is where a pinned
        // job goes to wait.
        for r in &set.runners {
            routes.push((r.id.clone(), crate::bus::Route::Runner(r.id.clone())));
        }
    }

    let reads = routes
        .iter()
        .map(|(key, route)| async move { (key.clone(), state.dispatcher.bus.depth(route).await) });

    let mut depths = pages::QueueDepths::default();
    let results = match tokio::time::timeout(
        Duration::from_secs(5),
        futures::future::join_all(reads),
    )
    .await
    {
        Ok(results) => results,
        Err(_) => {
            depths.error = Some("The queue did not answer within 5s.".to_string());
            return depths;
        }
    };
    for (key, result) in results {
        match result {
            Ok(depth) => {
                depths.by_route.insert(key, depth);
            }
            // A stream-level failure is NATS being unreachable, not an idle
            // queue, and saying so once at the top beats a page of blanks.
            Err(e) => depths
                .error
                .get_or_insert_with(|| e.to_string())
                .clone_from(&e.to_string()),
        }
    }
    depths
}

async fn networks_page(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let who = Identity::from_headers(&headers);
    let depths = queue_depths(&state).await;
    pages::networks_page(
        &state.config.name,
        who.as_ref().map(|i| i.display()),
        &state.runners.snapshot(),
        &depths,
        &pages::Notice::default(),
    )
}

async fn vms_page(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let who = Identity::from_headers(&headers);
    render_vms(&state, who.as_ref(), pages::Notice::default()).await
}

async fn render_vms(
    state: &AppState,
    who: Option<&Identity>,
    notice: pages::Notice,
) -> axum::response::Response {
    let vms = match state.dispatcher.vm_inventory().await {
        Ok(vms) => vms,
        Err(e) => return page_error(state, who, &e.to_string()),
    };
    // Read separately and never fatal: the images table is context for the
    // pool, and a page that refuses to render the VMs because the image
    // catalog was unreadable would hide the more important half.
    let images = state.dispatcher.image_inventory().await.unwrap_or_default();
    pages::vms_page(
        &state.config.name,
        who.map(|i| i.display()),
        &vms,
        &images,
        &notice,
    )
    .into_response()
}

async fn destroy_vm(
    State(state): State<AppState>,
    Path(sandbox_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };
    let notice = match state.dispatcher.destroy_pooled_vm(&sandbox_id).await {
        Ok(message) => {
            tracing::info!(
                "destroyed pooled VM {sandbox_id} by {}",
                who.as_ref().map(|w| w.display()).unwrap_or("anonymous")
            );
            pages::Notice::done(message)
        }
        Err(e) => pages::Notice::failed(e.to_string()),
    };
    render_vms(&state, who.as_ref(), notice).await
}

async fn cleanup_failed_vms(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };
    let notice = match state.dispatcher.destroy_failed_vms().await {
        Ok(message) => {
            tracing::info!(
                "swept VMs from failed runs by {}",
                who.as_ref().map(|w| w.display()).unwrap_or("anonymous")
            );
            pages::Notice::done(message)
        }
        Err(e) => pages::Notice::failed(e.to_string()),
    };
    render_vms(&state, who.as_ref(), notice).await
}

async fn render_networks(
    state: &AppState,
    who: Option<&Identity>,
    notice: pages::Notice,
) -> axum::response::Response {
    let depths = queue_depths(state).await;
    pages::networks_page(
        &state.config.name,
        who.map(|i| i.display()),
        &state.runners.snapshot(),
        &depths,
        &notice,
    )
    .into_response()
}

/// `POST /networks/{id}/join` — put this orchestrator's own host in a network.
///
/// The button exists because `uses: default` is useless until this machine is a
/// member of some network the instance serves, and the alternative was an error
/// message telling somebody to go and run `heyvm network add-host` on a box they
/// may not have a shell on.
async fn join_network(
    State(state): State<AppState>,
    Path(network_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let who = match may_manage(&state, &headers).await {
        Ok(w) => w,
        Err(response) => return response,
    };

    let pool = state.runners.snapshot();
    let node_id = pool.default_node_id.clone();
    if node_id.is_empty() {
        return render_networks(
            &state,
            who.as_ref(),
            pages::Notice::failed(
                "This machine's daemon could not be identified, so there is no host to                  add. Set CI_DEFAULT_NODE to its daemon id or name.",
            ),
        )
        .await;
    }
    let Some(network) = pool.find(&network_id) else {
        return render_networks(
            &state,
            who.as_ref(),
            pages::Notice::failed("That network no longer exists on this account."),
        )
        .await;
    };
    let network_name = network.network_name.clone();

    let notice = match state.runners.join_network(&network_id, &node_id).await {
        Ok(()) => {
            tracing::info!(
                "joined host {node_id} to network {network_name} by {}",
                who.as_ref().map(|w| w.display()).unwrap_or("anonymous")
            );
            // Re-read before rendering, or the page shows the state from before
            // the click and reads as though nothing happened. A failure here is
            // not the join failing — the join already succeeded — so it must not
            // be reported as one.
            if let Err(e) = state.runners.refresh().await {
                tracing::warn!("joined, but could not re-read the pool: {e}");
            }
            let served = state
                .runners
                .snapshot()
                .find(&network_id)
                .is_some_and(|n| n.served);
            let mut message = format!(
                "This host is now a member of {network_name}. It may take a moment to appear online."
            );
            if !served {
                // Joining does not make an instance take work for a network,
                // and finding that out from a queued job that never runs is
                // worse than being told now.
                message.push_str(
                    " This orchestrator does not serve that network, though, so jobs                      still cannot be sent to it — add it to CI_NETWORK, or set                      CI_NETWORK=*.",
                );
            }
            pages::Notice::done(message)
        }
        Err(e) => {
            tracing::warn!("could not join {network_name}: {e}");
            pages::Notice::failed(e.to_string())
        }
    };
    render_networks(&state, who.as_ref(), notice).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Binds no port — `oneshot` drives the router directly.
    pub(crate) fn test_config() -> Arc<Config> {
        // Set the required vars for a config that validates, then drop them so
        // tests stay independent of each other's environment.
        unsafe {
            std::env::set_var("CI_HEYO_API_KEY", "test-key");
            std::env::set_var("CI_NETWORK", "test-net");
            std::env::set_var("CI_DATABASE_URL", "postgres://localhost/ci_test");
            std::env::set_var("CI_WEBHOOK_SECRET", "0123456789abcdef");
        }
        let c = Config::from_env().expect("test config resolves");
        Arc::new(c)
    }

    /// A pool that has never refreshed. No network call is made — `Runners`
    /// only dials when `refresh` or `client_for` is called.
    fn test_runners(config: Arc<Config>) -> Arc<Runners> {
        Arc::new(Runners::new(config))
    }

    /// A router wired to a real database.
    ///
    /// The pages read run history, so there is no honest way to exercise them
    /// without a store; a mock would test the mock.
    async fn test_router() -> Router {
        let url = std::env::var("CI_TEST_DATABASE_URL").expect("CI_TEST_DATABASE_URL");
        let config = test_config();
        let dir = std::env::temp_dir().join(format!("ci-web-logs-{}", crate::vm::new_id()));
        let store = Store::connect(&url, dir).await.expect("store");
        store
            .migrate(std::path::Path::new("migrations"))
            .await
            .expect("migrations");
        let runners = test_runners(config.clone());
        let dispatcher = Arc::new(Dispatcher {
            config: config.clone(),
            store: store.clone(),
            pool: crate::pool::Pool::new(store.pool().clone()),
            images: crate::image::Catalog::new(store.pool().clone()),
            bus: Arc::new(
                // A fixed prefix, not a fresh one per test: these tests never
                // publish, so sharing one stream pair is harmless, and minting
                // a new pair per run leaves a NATS littered with them.
                crate::bus::Bus::connect(&config.nats, "citestweb")
                    .await
                    .expect("nats"),
            ),
            runners: runners.clone(),
            vms: Arc::new(crate::vm::Vms::new()),
            secrets: crate::secrets::Secrets::new(&config),
            artifacts: Arc::from(crate::artifacts::sink_for(&config).expect("disk sink")),
            objects: Arc::new(crate::objects::Workflows::new(&config)),
        });
        router(config, runners, store, dispatcher)
    }

    /// The token half of the submit credential, without a database: what does
    /// and does not count as a `Bearer` presentation.
    #[test]
    fn a_bearer_is_recognised_however_it_is_spelled_and_not_otherwise() {
        let headers = |v: &str| {
            let mut h = HeaderMap::new();
            if !v.is_empty() {
                h.insert(AUTHORIZATION, v.parse().unwrap());
            }
            h
        };
        assert_eq!(bearer(&headers("Bearer cis_k.s")), Some("cis_k.s"));
        assert_eq!(bearer(&headers("bearer cis_k.s")), Some("cis_k.s"));
        assert_eq!(bearer(&headers("Bearer   cis_k.s  ")), Some("cis_k.s"));

        // A credential for something else must fall through to the HMAC path
        // rather than being taken as a failed submit token.
        assert_eq!(bearer(&headers("Basic dXNlcjpwdw==")), None);
        assert_eq!(bearer(&headers("Bearer")), None);
        assert_eq!(bearer(&headers("Bearer ")), None);
        assert_eq!(bearer(&headers("")), None);
    }

    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn healthz_answers_without_a_credential() {
        let app = test_router().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// The networks page must render before the first refresh lands, because
    /// that is exactly when someone is looking at it — a cold start with a
    /// cloud that has not answered yet.
    ///
    /// Both spellings, because `/runners` is what this page was called and is
    /// in people's history.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn the_networks_page_renders_with_an_empty_pool() {
        for uri in ["/networks", "/runners"] {
            let app = test_router().await;
            let res = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{uri}");
            let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap();
            let html = String::from_utf8(body.to_vec()).unwrap();
            assert!(html.contains("heyvm network create"), "{uri}: {html}");
        }
    }

    /// A cross-site form POST carries the session cookie; without this check it
    /// would also carry the decision.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_repository_post_from_another_origin_is_refused() {
        let app = test_router().await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/repos")
                    .header("origin", "https://evil.example.com")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("url=git@github.com:evil/app.git"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    /// And the page's own form still works. `CI_PUBLIC_URL` defaults to
    /// `http://<listen addr>` in the test config, which is what the browser
    /// would send.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_repository_post_from_this_app_is_allowed() {
        let config = test_config();
        let app = test_router().await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/repos")
                    .header("origin", &config.public_url)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("url=&name=&workflow_path="))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Rejected for the empty URL, not for the origin — which is the point.
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("A clone URL is required"), "{html}");
    }
}
