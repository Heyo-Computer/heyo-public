//! Workflow objects, read from app-lb.
//!
//! `serverctl create workflow` registers one; this polls `GET /workflows` and
//! caches the result. Two decisions worth stating.
//!
//! **Polled, not pushed.** app-lb has no way to notify anything — it is a load
//! balancer, not an event bus — so a poll is the only mechanism available. It is
//! also the more robust one: a missed notification is a workflow that silently
//! stops building, whereas a missed poll is corrected thirty seconds later.
//!
//! **Cached in an `ArcSwap`, and a failed poll keeps the last good copy.**
//! app-lb being briefly unreachable must not stop builds that were already
//! configured. The alternative — an empty cache on error — rejects every submit
//! for the duration of a blip, with an error message that says the workflow does
//! not exist.
//!
//! ## Not typed against serverctl
//!
//! serverctl is a `path` dependency away, and pulling it in would drag
//! `serde_json/preserve_order` into this build globally (its own Cargo.toml says
//! so). The wire shape is five fields; re-declaring them here costs less than
//! that, and the `extra` map means an app-lb that adds a field does not break
//! this parse.

use crate::config::Config;
use crate::repos::same_repo;
use arc_swap::ArcSwap;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// One workflow object as app-lb serves it.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Workflow {
    pub id: String,
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub path: String,
    pub network: String,
    pub secrets_prefix: Option<String>,
    pub enabled: bool,
    /// Anything a newer app-lb sends. Kept rather than discarded so a field this
    /// build does not know about is at least visible in a debug dump.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Workflow {
    /// Where this workflow's secrets live, for a given environment.
    pub fn secrets_prefix(&self, environment: &str) -> String {
        match &self.secrets_prefix {
            Some(p) => format!("{}/{environment}", p.trim_end_matches('/')),
            None => crate::secrets::Secrets::prefix(&self.id, environment),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct WorkflowList {
    workflows: Vec<Workflow>,
}

/// The cached set, plus whether the last refresh worked.
#[derive(Debug, Default)]
pub struct WorkflowSet {
    pub workflows: Vec<Workflow>,
    /// `None` when the last poll succeeded; otherwise why it did not, so a
    /// stale cache is visibly stale.
    pub last_error: Option<String>,
    /// True once a poll has succeeded at least once. Distinguishes "app-lb says
    /// there are none" from "we have never managed to ask".
    pub loaded: bool,
}

impl WorkflowSet {
    pub fn find(&self, id: &str) -> Option<&Workflow> {
        self.workflows.iter().find(|w| w.id == id)
    }

    /// Every workflow that should build a given repository.
    ///
    /// Matched on the repository rather than the id, because `git submit` knows
    /// what repository it is in but not what somebody named the object. The
    /// comparison is [`crate::repos::same_repo`], the same one a submit token is
    /// scoped by, so "which workflows build this" and "which repository is this
    /// token for" can never disagree about what one repository is.
    pub fn for_repo<'a>(&'a self, repo: &'a str) -> impl Iterator<Item = &'a Workflow> {
        self.workflows
            .iter()
            .filter(move |w| w.enabled && same_repo(&w.repo, repo))
    }
}

/// Polls app-lb and caches the result.
pub struct Workflows {
    http: reqwest::Client,
    base_url: Option<String>,
    token: Option<String>,
    interval: Duration,
    cache: ArcSwap<WorkflowSet>,
}

impl Workflows {
    pub fn new(config: &Config) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            base_url: config.app_lb_url.clone(),
            token: config.app_lb_token.clone(),
            interval: config.heyvm.refresh_interval,
            cache: ArcSwap::from_pointee(WorkflowSet::default()),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.base_url.is_some()
    }

    pub fn snapshot(&self) -> Arc<WorkflowSet> {
        self.cache.load_full()
    }

    /// Re-read from app-lb. A failure keeps the previous set and records why.
    pub async fn refresh(&self) -> Result<(), ObjectsError> {
        let Some(base) = &self.base_url else {
            // Not configured is not an error: an installation can run entirely
            // on `CI_WORKFLOW_PATH` without registering objects at all.
            return Ok(());
        };

        let mut request = self.http.get(format!("{base}/workflows"));
        if let Some(t) = &self.token {
            request = request.bearer_auth(t);
        }

        let result = async {
            let response = request
                .send()
                .await
                .map_err(|e| ObjectsError::Transport(e.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(ObjectsError::Api {
                    status: status.as_u16(),
                    body: body.chars().take(200).collect(),
                });
            }
            response
                .json::<WorkflowList>()
                .await
                .map_err(|e| ObjectsError::Decode(e.to_string()))
        }
        .await;

        match result {
            Ok(list) => {
                self.cache.store(Arc::new(WorkflowSet {
                    workflows: list.workflows,
                    last_error: None,
                    loaded: true,
                }));
                Ok(())
            }
            Err(e) => {
                let prev = self.cache.load();
                self.cache.store(Arc::new(WorkflowSet {
                    workflows: prev.workflows.clone(),
                    last_error: Some(e.to_string()),
                    loaded: prev.loaded,
                }));
                Err(e)
            }
        }
    }

    pub fn spawn_refresh_loop(self: Arc<Self>) {
        if !self.is_configured() {
            tracing::info!(
                "CI_APP_LB_URL is not set, so no workflow objects are read; submits fall \
                 back to CI_WORKFLOW_PATH"
            );
            return;
        }
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(e) = self.refresh().await {
                    tracing::warn!("could not read workflow objects, keeping the last set: {e}");
                }
            }
        });
    }
}

#[derive(Debug)]
pub enum ObjectsError {
    Transport(String),
    Api { status: u16, body: String },
    Decode(String),
}

impl fmt::Display for ObjectsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "could not reach app-lb: {e}. Check CI_APP_LB_URL."),
            Self::Api { status, body } => write!(
                f,
                "app-lb answered {status} for GET /workflows: {body}. \
                 A 401 usually means CI_APP_LB_TOKEN is missing or unscoped."
            ),
            Self::Decode(e) => write!(f, "app-lb sent a workflow list this build cannot read: {e}"),
        }
    }
}

impl std::error::Error for ObjectsError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn wf(id: &str, repo: &str, enabled: bool) -> Workflow {
        Workflow {
            id: id.into(),
            repo: repo.into(),
            git_ref: "main".into(),
            path: ".ci/workflows/*.yml".into(),
            network: "prod".into(),
            secrets_prefix: None,
            enabled,
            extra: BTreeMap::new(),
        }
    }

    /// A workflow object is app-lb's wire format; this parse has to survive both
    /// the `ref` rename and a field this build has never heard of.
    #[test]
    fn a_workflow_parses_from_app_lbs_own_shape() {
        let w: Workflow = serde_json::from_str(
            r#"{"id":"build","repo":"git@github.com:me/app.git","ref":"main",
                "path":".ci/workflows/*.yml","network":"prod-runners",
                "auth":{"secret":"github","key":"token"},
                "secrets_prefix":"ci/app","enabled":true,
                "concurrency_group":"prod"}"#,
        )
        .expect("parses");
        assert_eq!(w.git_ref, "main", "`ref` on the wire");
        assert_eq!(w.network, "prod-runners");
        assert!(w.enabled);
        assert!(
            w.extra.contains_key("concurrency_group"),
            "a newer field stays reachable: {:?}",
            w.extra
        );
    }

    /// The two spellings of one clone URL are matched by
    /// [`crate::repos::same_repo`], which has its own tests; what this asserts
    /// is that `for_repo` actually uses it, and honours `enabled`.
    #[test]
    fn a_disabled_workflow_is_not_offered_for_its_repository() {
        let set = WorkflowSet {
            workflows: vec![
                wf("on", "git@github.com:me/app.git", true),
                wf("off", "git@github.com:me/app.git", false),
                wf("other", "git@github.com:me/other.git", true),
            ],
            last_error: None,
            loaded: true,
        };
        let ids: Vec<&str> = set
            .for_repo("https://github.com/me/app")
            .map(|w| w.id.as_str())
            .collect();
        assert_eq!(ids, ["on"]);
        assert!(set.find("off").is_some(), "still listed, just not offered");
    }

    /// The default keeps every workflow's secrets under its own id; an override
    /// lets two workflows share a set.
    #[test]
    fn the_secrets_prefix_defaults_to_the_workflow_id() {
        let w = wf("myapp", "r", true);
        assert_eq!(w.secrets_prefix("prod"), "ci/myapp/prod");

        let mut shared = wf("myapp", "r", true);
        shared.secrets_prefix = Some("ci/shared/".into());
        assert_eq!(shared.secrets_prefix("prod"), "ci/shared/prod");
    }

    /// "app-lb says there are none" and "we have never managed to ask" are
    /// different states, and only one of them should make a submit fail.
    #[tokio::test]
    async fn an_unconfigured_client_is_not_an_error_and_reports_never_loaded() {
        let w = Workflows {
            http: reqwest::Client::new(),
            base_url: None,
            token: None,
            interval: Duration::from_secs(30),
            cache: ArcSwap::from_pointee(WorkflowSet::default()),
        };
        assert!(!w.is_configured());
        w.refresh().await.expect("not configured is not an error");
        assert!(!w.snapshot().loaded);
    }

    /// app-lb being briefly unreachable must not empty the cache — that would
    /// reject every submit for the duration of a blip.
    #[tokio::test]
    async fn a_failed_refresh_keeps_the_last_good_set() {
        let w = Workflows {
            http: reqwest::Client::builder()
                .timeout(Duration::from_millis(200))
                .build()
                .unwrap(),
            // Nothing listens on port 1.
            base_url: Some("http://127.0.0.1:1".into()),
            token: None,
            interval: Duration::from_secs(30),
            cache: ArcSwap::from_pointee(WorkflowSet {
                workflows: vec![wf("build", "git@github.com:me/app.git", true)],
                last_error: None,
                loaded: true,
            }),
        };

        let err = w.refresh().await.unwrap_err();
        assert!(matches!(err, ObjectsError::Transport(_)), "{err:?}");

        let set = w.snapshot();
        assert_eq!(set.workflows.len(), 1, "the previous set survives");
        assert!(set.loaded);
        assert!(set.last_error.is_some(), "but it is visibly stale");
        assert!(
            set.last_error.as_ref().unwrap().contains("CI_APP_LB_URL"),
            "the error names what to check: {:?}",
            set.last_error
        );
    }
}
