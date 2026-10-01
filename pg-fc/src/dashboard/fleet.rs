//! Fleet rollup: health and metrics for every pooler instance on one page.
//!
//! Each pooler host runs its own dashboard and knows only about itself. The
//! instances listed in `PG_VM_POOL_DASHBOARD_FLEET` (see
//! [`crate::config::parse_fleet`]) let one dashboard read the others' JSON
//! API — `/api/health`, `/api/host` and `/api/metrics` — and show them next to
//! its own, which it reads in-process. Nothing is cached or stored: every
//! `/fleet` load is a fresh, parallel read, bounded per instance by
//! `PG_VM_POOL_DASHBOARD_FLEET_TIMEOUT_SECS`, so one dead host costs the page
//! that timeout, not a hang.
//!
//! Read-only by construction: the rollup only ever issues GETs, and never
//! asks a peer for its own `/api/fleet`, so instances that list each other
//! can't recurse.

use std::sync::OnceLock;
use std::time::Instant;

use axum::Json;
use axum::extract::{Query, State};
use maud::Markup;
use pg_fc_api::{FleetInstance, FleetRollup, FleetTotals, Health, HostInfo, Metrics};
use serde::de::DeserializeOwned;

use crate::config::FleetMember;

use super::api::{self, WindowQuery};
use super::state::DashState;
use super::views;

/// `GET /api/fleet` — the rollup as JSON.
pub async fn api_fleet(
    State(st): State<DashState>,
    Query(q): Query<WindowQuery>,
) -> Json<FleetRollup> {
    Json(rollup(&st, q.hours()).await)
}

/// `GET /fleet` — the rollup as a page.
pub async fn page(State(st): State<DashState>, Query(q): Query<WindowQuery>) -> Markup {
    let fleet = rollup(&st, q.hours()).await;
    views::fleet_page(&st, &fleet)
}

/// Read every instance, this one included, and sum what answered.
pub(super) async fn rollup(st: &DashState, hours: u64) -> FleetRollup {
    let local = local_instance(st, hours);
    let remotes = futures::future::join_all(
        st.cfg
            .fleet
            .iter()
            .map(|m| remote_instance(m, hours, st.cfg.fleet_timeout)),
    );
    let (local, remotes) = tokio::join!(local, remotes);
    let mut instances = Vec::with_capacity(1 + remotes.len());
    instances.push(local);
    instances.extend(remotes);
    FleetRollup {
        generated_at: crate::events::now_unix(),
        window_hours: hours,
        totals: totals(&instances),
        instances,
    }
}

async fn local_instance(st: &DashState, hours: u64) -> FleetInstance {
    let started = Instant::now();
    let (health, host) = tokio::join!(api::health_view(st), api::host_view(st));
    FleetInstance {
        name: st.cfg.fleet_name.clone(),
        url: None,
        local: true,
        reachable: true,
        error: None,
        fetch_ms: elapsed_ms(started),
        health: Some(health),
        host: Some(host),
        metrics: Some(api::metrics_view(hours)),
    }
}

/// One shared client: connection pools and TLS sessions survive across page
/// loads. The deadline is applied per request, from config.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("pg-vm-pool-fleet/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default()
    })
}

async fn remote_instance(
    m: &FleetMember,
    hours: u64,
    timeout: std::time::Duration,
) -> FleetInstance {
    let started = Instant::now();
    let metrics_path = format!("/api/metrics?hours={hours}");
    let (health, host, metrics) = tokio::join!(
        get::<Health>(m, "/api/health", timeout),
        get::<HostInfo>(m, "/api/host", timeout),
        get::<Metrics>(m, &metrics_path, timeout),
    );
    // Health decides reachability; host and metrics are best-effort extras,
    // and an instance older than `/api/metrics` is still a healthy instance.
    let errors: Vec<String> = [
        health.as_ref().err().map(|e| format!("health: {e}")),
        host.as_ref().err().map(|e| format!("host: {e}")),
        metrics.as_ref().err().map(|e| format!("metrics: {e}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    FleetInstance {
        name: m.name.clone(),
        url: Some(m.base_url.clone()),
        local: false,
        reachable: health.is_ok(),
        error: (!errors.is_empty()).then(|| errors.join("; ")),
        fetch_ms: elapsed_ms(started),
        health: health.ok(),
        host: host.ok(),
        metrics: metrics.ok(),
    }
}

/// GET one JSON document from a fleet member. Errors are short and carry no
/// credentials — the URL they could mention has had its userinfo stripped.
async fn get<T: DeserializeOwned>(
    m: &FleetMember,
    path: &str,
    timeout: std::time::Duration,
) -> Result<T, String> {
    let mut req = client()
        .get(format!("{}{path}", m.base_url))
        .timeout(timeout);
    if let Some((u, p)) = &m.basic_auth {
        req = req.basic_auth(u, Some(p));
    }
    let resp = req.send().await.map_err(|e| {
        if e.is_timeout() {
            format!("timed out after {timeout:?}")
        } else if e.is_connect() {
            "connection failed".to_string()
        } else {
            e.without_url().to_string()
        }
    })?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err("not served (older pooler?)".to_string());
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    resp.json::<T>()
        .await
        .map_err(|e| format!("bad response: {}", e.without_url()))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn totals(instances: &[FleetInstance]) -> FleetTotals {
    let mut t = FleetTotals {
        instances: instances.len(),
        ..FleetTotals::default()
    };
    for i in instances.iter().filter(|i| i.reachable) {
        t.reachable += 1;
        if let Some(h) = &i.health {
            t.warm_schemas += h.warm_schemas;
            t.known_schemas += h.known_schemas;
        }
        if let Some(h) = &i.host {
            t.tiers.live += h.tiers.live;
            t.tiers.compacted += h.tiers.compacted;
            t.tiers.frozen += h.tiers.frozen;
            t.tiers.archived += h.tiers.archived;
            t.memory_total_bytes += h.memory_total_bytes.unwrap_or(0);
            t.memory_used_bytes += h.memory_used_bytes.unwrap_or(0);
            t.cpu_count += u64::from(h.cpu_count.unwrap_or(0));
        }
        if let Some(m) = &i.metrics {
            t.events.vms_created += m.events.vms_created;
            t.events.restores_s3 += m.events.restores_s3;
            t.events.restores_local += m.events.restores_local;
            t.events.offloads_done += m.events.offloads_done;
            t.events.vms_deleted += m.events.vms_deleted;
            t.events.spares_claimed += m.events.spares_claimed;
            t.bringup_errors += m.bringup_errors;
            t.errors += m.errors;
        }
    }
    t
}

/// Fraction at or above which a host resource reads as degraded — the same
/// line the monitoring page's meters turn red at.
const DEGRADED_FRAC: f64 = 0.9;

/// How an instance reads at a glance: `down` when it didn't answer, else
/// `degraded` with the reasons, else `ok`.
pub(super) fn verdict(i: &FleetInstance) -> (&'static str, Vec<String>) {
    if !i.reachable {
        return ("down", Vec::new());
    }
    let mut why = Vec::new();
    if let Some(h) = &i.host {
        if let (Some(used), Some(total)) = (h.memory_used_bytes, h.memory_total_bytes)
            && total > 0
            && used as f64 / total as f64 >= DEGRADED_FRAC
        {
            why.push(format!("memory {:.0}%", used as f64 * 100.0 / total as f64));
        }
        for d in &h.disks {
            if d.total > 0 && d.used as f64 / d.total as f64 >= DEGRADED_FRAC {
                why.push(format!(
                    "{} {:.0}% full",
                    d.mount,
                    d.used as f64 * 100.0 / d.total as f64
                ));
            }
        }
        if let (Some(0), Some(target)) = (h.spares_ready, h.spares_target)
            && target > 0
        {
            why.push("no warm spares ready".to_string());
        }
    }
    if let Some(m) = &i.metrics
        && m.bringup_errors > 0
    {
        why.push(format!("{} failed bring-up(s)", m.bringup_errors));
    }
    if why.is_empty() {
        ("ok", why)
    } else {
        ("degraded", why)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_fc_api::{EventCounts, HostDisk, TierCounts};

    fn instance(name: &str, reachable: bool) -> FleetInstance {
        FleetInstance {
            name: name.into(),
            reachable,
            health: Some(Health {
                warm_schemas: 3,
                known_schemas: 10,
                ..Health::default()
            }),
            host: Some(HostInfo {
                memory_total_bytes: Some(100),
                memory_used_bytes: Some(40),
                cpu_count: Some(8),
                tiers: TierCounts {
                    live: 4,
                    compacted: 3,
                    frozen: 2,
                    archived: 1,
                },
                ..HostInfo::default()
            }),
            metrics: Some(Metrics {
                window_hours: 1,
                events: EventCounts {
                    vms_created: 5,
                    restores_s3: 1,
                    ..EventCounts::default()
                },
                bringup_errors: 1,
                errors: 2,
                timings: Vec::new(),
            }),
            ..FleetInstance::default()
        }
    }

    #[test]
    fn totals_sum_only_reachable_instances() {
        let t = totals(&[
            instance("a", true),
            instance("b", true),
            instance("c", false),
        ]);
        assert_eq!(t.instances, 3);
        assert_eq!(t.reachable, 2);
        assert_eq!(t.warm_schemas, 6);
        assert_eq!(t.known_schemas, 20);
        assert_eq!(t.tiers.compacted, 6);
        assert_eq!(t.events.vms_created, 10);
        assert_eq!(t.bringup_errors, 2);
        assert_eq!(t.errors, 4);
        assert_eq!(t.memory_total_bytes, 200);
        assert_eq!(t.cpu_count, 16);
    }

    /// A real HTTP round trip against a stand-in dashboard: credentials are
    /// sent, an instance without `/api/metrics` (an older pooler) is still up,
    /// and one that refuses connections is down — each within the deadline.
    #[tokio::test]
    async fn reads_remote_instances_over_http() {
        use axum::http::{StatusCode, header};
        use axum::routing::get;

        let app = axum::Router::new()
            .route(
                "/api/health",
                get(|headers: axum::http::HeaderMap| async move {
                    // base64("ops:pw")
                    if headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        != Some("Basic b3BzOnB3")
                    {
                        return Err(StatusCode::UNAUTHORIZED);
                    }
                    Ok(Json(Health {
                        version: "9.9.9".into(),
                        warm_schemas: 7,
                        ..Health::default()
                    }))
                }),
            )
            .route("/api/host", get(|| async { Json(HostInfo::default()) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let member = |auth: Option<(&str, &str)>| FleetMember {
            name: "peer".into(),
            base_url: format!("http://{addr}"),
            basic_auth: auth.map(|(u, p)| (u.to_string(), p.to_string())),
        };
        let timeout = std::time::Duration::from_secs(2);

        let up = remote_instance(&member(Some(("ops", "pw"))), 1, timeout).await;
        assert!(up.reachable, "{:?}", up.error);
        assert_eq!(up.health.as_ref().unwrap().warm_schemas, 7);
        assert!(up.host.is_some());
        assert!(up.metrics.is_none());
        assert_eq!(
            up.error.as_deref(),
            Some("metrics: not served (older pooler?)")
        );
        assert_eq!(verdict(&up).0, "ok");

        let denied = remote_instance(&member(Some(("ops", "nope"))), 1, timeout).await;
        assert!(!denied.reachable);
        assert!(denied.error.unwrap().contains("401"));

        // Nothing listens on the port the stand-in just gave back.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = closed.local_addr().unwrap();
        drop(closed);
        let mut dead = member(None);
        dead.base_url = format!("http://{dead_addr}");
        let down = remote_instance(&dead, 1, timeout).await;
        assert!(!down.reachable);
        assert_eq!(verdict(&down).0, "down");
    }

    #[test]
    fn verdict_names_what_is_wrong() {
        assert_eq!(verdict(&instance("a", false)).0, "down");

        let mut healthy = instance("a", true);
        healthy.metrics.as_mut().unwrap().bringup_errors = 0;
        assert_eq!(verdict(&healthy), ("ok", Vec::new()));

        let mut sick = healthy.clone();
        let host = sick.host.as_mut().unwrap();
        host.disks.push(HostDisk {
            mount: "/mnt/md1".into(),
            total: 100,
            used: 95,
            ..HostDisk::default()
        });
        host.spares_ready = Some(0);
        host.spares_target = Some(4);
        let (state, why) = verdict(&sick);
        assert_eq!(state, "degraded");
        assert_eq!(why, vec!["/mnt/md1 95% full", "no warm spares ready"]);
    }
}
