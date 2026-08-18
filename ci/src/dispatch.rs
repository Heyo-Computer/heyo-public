//! Turning queued jobs into steps that ran.
//!
//! Two halves that never call each other directly:
//!
//! - **The scheduler** ([`Dispatcher::advance_run`]) decides which jobs are
//!   ready and publishes them. It runs after a run is created and again after
//!   every job finishes.
//! - **The executor** ([`Dispatcher::run_job`]) pulls one job, gets a VM, runs
//!   its steps, and records what happened.
//!
//! They communicate through Postgres and JetStream rather than in memory, which
//! is what lets the executor for a runner live in a different process from the
//! scheduler — and what makes a crash between the two recoverable.
//!
//! ## Everything here is written to be run twice
//!
//! A JetStream redelivery is normal: a runner reboots, a dispatcher is killed
//! mid-build, an ack is lost. So every step of the path is idempotent.
//!
//! - Job and step row ids are *derived* from the run and job key, so a second
//!   delivery addresses the same rows rather than making new ones.
//! - [`crate::store::Store::start_job`] refuses a job that already reached a
//!   terminal state, which drops a redelivery of work that finished just before
//!   its ack was lost.
//! - A step's `operationId` is its row id, and the daemon's exec-operation route
//!   is idempotent on that id — so re-running a step that is still in flight
//!   reattaches to it instead of starting the build a second time.

use crate::bus::{Bus, JobMessage, Route};
use crate::config::Config;
use crate::expr::Context;
use crate::plan::JobPlan;
use crate::pool::Pool;
use crate::runners::Runners;
use crate::store::{JobStatus, RunStatus, StepStatus, Store, step_id};
use crate::vm::{ExecOutput, Vm, VmError, Vms, sandbox_name};
use crate::workflow::{Fallback, Step};
use async_nats::jetstream::AckKind;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How long to wait for a VM to boot before giving up on a job.
const BOOT_TIMEOUT: Duration = Duration::from_secs(300);

/// Default per-step timeout when the workflow does not set `timeout-minutes`.
/// Bounded well under the job timeout so one runaway step cannot consume the
/// whole job budget and leave later steps no time at all.
const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Where the submitted tree lands, and where steps run, when the `vm:` block
/// does not say. Matches the daemon's own default mount.
const DEFAULT_WORKDIR: &str = "/workspace";

/// Nonce source for VM names. Hex, because the daemon derives a tap subnet from
/// the sandbox id by parsing it as hex.
static NONCE: AtomicU64 = AtomicU64::new(1);

/// What a submit produced: the runs it started, and anything the submitter
/// should know that did not stop it starting them.
pub struct Submitted {
    pub run_ids: Vec<String>,
    pub warnings: Vec<String>,
}

/// Where one job runs, resolved from its `uses:` against the live pool.
///
/// `node: None` is the only case that goes on a network's shared queue; every
/// other form pins, and a pinned job waits for its host rather than migrating.
#[derive(Debug)]
struct Placement<'a> {
    network: &'a crate::runners::RunnerSet,
    node: Option<&'a crate::runners::Runner>,
    /// An existing sandbox on `node`. When set, the `vm:` block is unused and
    /// steps exec into this VM rather than one built for the job.
    vm: Option<&'a str>,
}

pub struct Dispatcher {
    pub config: Arc<Config>,
    pub store: Store,
    pub pool: Pool,
    /// What this orchestrator has built in each runner's image catalog. The
    /// daemon exposes no way to list images, so this is the only record of it.
    pub images: crate::image::Catalog,
    pub bus: Arc<Bus>,
    pub runners: Arc<Runners>,
    pub vms: Arc<Vms>,
    pub secrets: crate::secrets::Secrets,
    pub artifacts: Arc<dyn crate::artifacts::ArtifactSink>,
    pub objects: Arc<crate::objects::Workflows>,
}

impl Dispatcher {
    /// Where a run's checkout lives. Populated by the trigger before the run is
    /// scheduled; read here for `cache_key_files` hashing.
    pub fn workspace(&self, run_id: &str) -> PathBuf {
        self.config.workspace_dir.join(run_id)
    }

    // ---- submission -----------------------------------------------------

    /// Turn a verified submit into a run, and schedule it.
    ///
    /// The workflow files come from the *submitted tree*, not from a checkout
    /// this process makes: what runs is what the submitter had. A tree with
    /// several matching workflow files produces one run per file, because two
    /// workflows in one repository are two independent answers to "did this
    /// commit pass".
    ///
    /// `repo` is the registration the submit token authenticated as, when it
    /// used one. It is *authority*, not a hint: the caller has already refused
    /// a payload naming a different repository, so the URL a run is recorded
    /// against comes from the registration rather than from a field the client
    /// filled in.
    pub async fn submit(
        &self,
        req: &crate::trigger::SubmitRequest,
        actor: Option<&crate::web::identity::Identity>,
        repo: Option<&crate::store::Repo>,
    ) -> Result<Submitted, DispatchError> {
        let run_seed = crate::vm::new_id();
        let workspace = crate::trigger::Workspace::for_run(&self.config, &run_seed);
        tokio::fs::create_dir_all(&self.config.workspace_dir)
            .await
            .map_err(|e| DispatchError::Checkout(e.to_string()))?;

        let size =
            crate::trigger::materialize(&req.source, &workspace, self.config.max_source_bytes)?;

        // Read once, from the seed workspace, before any run is created: every
        // workflow file in this submit is looking at the same commit, and a
        // second `git diff` per file would be the same answer at the same cost.
        // Copied onto each run so the scheduler and the dashboard read it from
        // the row rather than from a tree that gets swept.
        let changes = crate::trigger::changed_paths(&workspace, &req.before);
        tracing::info!("submit: {changes}");

        // Which repository this is, in one place. A registration's URL is the
        // canonical spelling and wins over the payload's, which matters for the
        // client that has no `origin` remote at all: it sends an empty URL, and
        // without the token nothing downstream could say what was built.
        let repo_url = match repo {
            Some(r) => r.url.clone(),
            None => req.repository.url.clone(),
        };

        // A registered workflow object decides the path glob and the id; without
        // one, the installation-wide default applies. Matching is on the
        // *repository*, because `git submit` knows what it is a clone of but not
        // what somebody named the object.
        let objects = self.objects.snapshot();
        let matched: Vec<crate::objects::Workflow> = match &req.workflow_id {
            Some(id) => objects.find(id).cloned().into_iter().collect(),
            None => objects.for_repo(&repo_url).cloned().collect(),
        };
        if let Some(id) = &req.workflow_id
            && matched.is_empty()
            && objects.loaded
        {
            return Err(DispatchError::Workflow(format!(
                "no workflow object {id:?} is registered. \
                 `serverctl get workflows` lists what is."
            )));
        }

        // Several objects may name one repository — `build` and `nightly` with
        // different globs is a legitimate setup — and each is an independent
        // answer to "did this commit pass", so each gets its own runs. Picking
        // one silently would make the other stop building for no stated reason.
        //
        // With no objects at all, one synthetic entry carries the defaults, so
        // an installation that never registers anything still works.
        struct Source {
            id: Option<String>,
            pattern: String,
            /// The network jobs from this source run in when they do not say.
            network: Option<String>,
        }
        let sources: Vec<Source> = if matched.is_empty() {
            vec![Source {
                id: None,
                // A registration's assigned network, else the installation
                // default. This is the whole point of assigning one: a workflow
                // that says nothing about where it runs still lands somewhere
                // deliberate rather than wherever this instance happens to
                // consider first.
                network: repo
                    .and_then(|r| r.network.clone())
                    .filter(|n| !n.trim().is_empty()),
                // A registration may carry its own glob, for the repository
                // whose workflows are not where this installation's default
                // says. A workflow object still wins over it: the object is the
                // more specific statement, and it is the one that also names a
                // network and a secrets prefix.
                pattern: repo
                    .and_then(|r| r.workflow_path.clone())
                    .filter(|p| !p.trim().is_empty())
                    .unwrap_or_else(|| self.config.default_workflow_path.clone()),
            }]
        } else {
            matched
                .iter()
                .map(|w| Source {
                    id: Some(w.id.clone()),
                    pattern: w.path.clone(),
                    // A workflow object names a network of its own; it is the
                    // more specific statement, so it wins over the repository's
                    // assignment, and the assignment fills in when it is blank.
                    network: Some(w.network.clone())
                        .filter(|n| !n.trim().is_empty())
                        .or_else(|| repo.and_then(|r| r.network.clone()))
                        .filter(|n| !n.trim().is_empty()),
                })
                .collect()
        };

        let mut run_ids = Vec::new();
        let mut patterns_tried = Vec::new();
        // Workflow files that matched the glob and then declined to build, with
        // the filter that declined. Kept apart from `warnings` because they are
        // the difference between "nothing matched your glob" — a mistake worth
        // an error — and "your filters said no", which is the feature working.
        let mut skipped: Vec<String> = Vec::new();
        // Carried back to the client. A submit that queued but will not run
        // until something changes should say so at the terminal that made it,
        // not only on a page nobody has open.
        let mut warnings: Vec<String> = Vec::new();

        for source in &sources {
            let files = crate::trigger::find_workflows(&workspace.root, &source.pattern)?;
            patterns_tried.push(source.pattern.clone());
            if files.is_empty() {
                continue;
            }
            tracing::info!(
                "submit: {size} bytes, {} workflow file(s) matching {} for {}",
                files.len(),
                source.pattern,
                source.id.as_deref().unwrap_or("(no object)")
            );

            for (path, text) in &files {
                let wf = crate::workflow::Workflow::parse(path, text)
                    .map_err(|e| DispatchError::Workflow(e.to_string()))?;
                if !wf.on.iter().any(|t| t == "submit") {
                    tracing::info!("{path} does not trigger on `submit`; skipping");
                    continue;
                }
                // The monorepo gate. Evaluated per workflow file, because that
                // is the unit a run is created for: in a repository with an
                // `api.yml` and a `web.yml`, a commit touching only `api/` must
                // produce one run and not two.
                if let Err(why) = wf.on_submit.admits(req.branch(), &changes) {
                    tracing::info!("{path} declined this submit: {why}");
                    skipped.push(format!("{path}: {why}"));
                    continue;
                }
                let mut plan = crate::plan::Plan::build(&wf)
                    .map_err(|e| DispatchError::Workflow(e.to_string()))?;

                // Resolved once, here, and written into every job that did not
                // name a network with `uses:`. The plan is persisted on the job
                // row and is what a redelivery executes, so a job runs in the
                // network it was scheduled for even if the repository is
                // reassigned mid-build — the same reason the expanded plan is
                // stored rather than recomputed.
                self.assign_network(&mut plan, source.network.as_deref(), &mut warnings)?;

                // The first run reuses the workspace already materialized under
                // the seed id; the rest get their own copy of the same archive,
                // so no two runs share a directory a step could write into.
                let run_id = if run_ids.is_empty() {
                    run_seed.clone()
                } else {
                    let id = crate::vm::new_id();
                    let ws = crate::trigger::Workspace::for_run(&self.config, &id);
                    copy_tree(&workspace, &ws).await?;
                    id
                };

                self.store
                    .create_run(
                        &run_id,
                        &crate::store::RunRequest {
                            workflow_id: source
                                .id
                                .clone()
                                .or_else(|| req.workflow_id.clone())
                                .unwrap_or_else(|| match repo {
                                    Some(r) => r.name.clone(),
                                    None => req.repository.name.clone(),
                                }),
                            repo_id: repo.map(|r| r.id.clone()),
                            repo_url: repo_url.clone(),
                            git_ref: req.r#ref.clone(),
                            sha: req.after.clone(),
                            before_sha: req.before.clone(),
                            changes: changes.clone(),
                            actor_subject: actor.map(|a| a.subject.clone()),
                            actor_email: actor
                                .map(|a| a.email.clone())
                                .or_else(|| req.pusher.as_ref().and_then(|p| p.email.clone())),
                            source: "submit".to_string(),
                        },
                        &plan,
                    )
                    .await?;
                self.advance_run(&run_id).await?;
                run_ids.push(run_id);
            }
        }

        // A submit that started nothing is two different situations, and
        // collapsing them was survivable only while no workflow could decline.
        //
        // Nothing *matched* is a mistake — a glob pointing at a directory that
        // is not there, or a workflow that triggers on something this build does
        // not honour — and the submitter wants a non-zero exit for it.
        //
        // Everything *declining* is the feature doing its job. In a monorepo it
        // is the common case: most commits touch one package, so most workflows
        // correctly build nothing. Failing the submit for that would make `git
        // submit` red on a healthy repository, and would break any hook that
        // treats a failed submit as something to retry.
        if run_ids.is_empty() && skipped.is_empty() {
            return Err(crate::trigger::TriggerError::NoWorkflows(format!(
                "{} (nothing matched, or nothing triggering on `submit`)",
                patterns_tried.join(", ")
            ))
            .into());
        }
        // Reported whether or not anything else ran: with several workflows, the
        // interesting question is usually why the *other* one did not.
        warnings.extend(skipped.into_iter().map(|s| format!("no run started — {s}")));
        Ok(Submitted { run_ids, warnings })
    }

    // ---- scheduling -----------------------------------------------------

    /// Publish every job that has become ready, skip those whose `if:` is false,
    /// and roll the run up.
    ///
    /// Safe to call repeatedly and concurrently: publishing is deduplicated by
    /// the job's own id (`Nats-Msg-Id`), and moving a job from `pending` to
    /// `queued` is conditional on it still being `pending`.
    pub async fn advance_run(&self, run_id: &str) -> Result<RunStatus, DispatchError> {
        let jobs = self.store.jobs_of(run_id).await?;
        let needs = self.store.needs_context(run_id).await?;
        // One read for the whole wave, not one per job: the commit a run is for
        // does not change between two jobs of the same run.
        let ci = Self::ci_scope(self.store.get_run(run_id).await?.as_ref());

        // A base id is only satisfied once *every* cell of it is terminal —
        // `needs: [build]` cannot mean "the first cell of build".
        let mut terminal: HashMap<&str, bool> = HashMap::new();
        for j in &jobs {
            let is_terminal = matches!(
                j.status.as_str(),
                "success" | "failure" | "skipped" | "cancelled"
            );
            terminal
                .entry(j.base_id.as_str())
                .and_modify(|t| *t &= is_terminal)
                .or_insert(is_terminal);
        }

        for job in &jobs {
            if job.status != "pending" {
                continue;
            }
            let plan: JobPlan = match serde_json::from_value(job.plan.clone()) {
                Ok(p) => p,
                Err(e) => {
                    self.store
                        .set_job_status(
                            &job.id,
                            JobStatus::Failure,
                            Some(&format!("stored plan could not be read: {e}")),
                        )
                        .await?;
                    continue;
                }
            };

            if !plan
                .needs
                .iter()
                .all(|n| *terminal.get(n.as_str()).unwrap_or(&false))
            {
                continue;
            }

            // Decide `if:` now that dependencies have results. A dependency that
            // failed makes the default guard false, which is what stops a deploy
            // job from shipping a broken build.
            match self.should_run(&plan, &needs, &ci) {
                Ok(true) => {}
                Ok(false) => {
                    self.store
                        .set_job_status(&job.id, JobStatus::Skipped, None)
                        .await?;
                    self.bus
                        .publish_event(run_id, &plan.key, &serde_json::json!({"status": "skipped"}))
                        .await;
                    continue;
                }
                Err(e) => {
                    // A guard that cannot be understood must not run the job.
                    self.store
                        .set_job_status(
                            &job.id,
                            JobStatus::Failure,
                            Some(&format!("could not evaluate `if:` — {e}")),
                        )
                        .await?;
                    continue;
                }
            }

            let route = match self.route_for(&plan).await {
                Ok(r) => r,
                Err(e) => {
                    self.store
                        .set_job_status(&job.id, JobStatus::Failure, Some(&e.to_string()))
                        .await?;
                    continue;
                }
            };

            if self.store.queue_job(&job.id).await? {
                let published = self
                    .bus
                    .publish_job(
                        &route,
                        &JobMessage {
                            run_id: run_id.to_string(),
                            job_id: job.id.clone(),
                            job_key: plan.key.clone(),
                        },
                    )
                    .await;

                match published {
                    Ok(()) => {
                        tracing::info!(run = run_id, job = %plan.key, route = ?route, "queued")
                    }
                    // The status was committed before this call, so a failure
                    // here leaves a row saying `queued` with nothing on the
                    // queue — two stores disagreeing, with nothing to notice.
                    // Rolling back makes the scheduler's own retry the repair;
                    // `Nats-Msg-Id` means a later duplicate publish collapses,
                    // so retrying is safe even if the message did land.
                    Err(e) => {
                        tracing::warn!(
                            run = run_id, job = %plan.key, route = ?route,
                            "could not publish, returning the job to pending: {e}"
                        );
                        if let Err(e) = self.store.unqueue_job(&job.id).await {
                            // Now it really is stranded, and saying so is all
                            // that is left.
                            tracing::error!(
                                run = run_id, job = %plan.key,
                                "could not roll back a failed publish: {e}"
                            );
                        }
                        // Deliberately not propagated: one unreachable subject
                        // must not stop the rest of the run being scheduled.
                        continue;
                    }
                }
            }
        }

        Ok(self.store.roll_up_run(run_id).await?)
    }

    /// The `ci` expression scope: which commit this run is for, and what it
    /// changed.
    ///
    /// Built from the run row rather than persisted onto each `JobPlan`, unlike
    /// the network assignment next to it. The reason is that the two are not the
    /// same kind of fact: a repository's network can be reassigned while a build
    /// is in flight, so the plan freezes it; the commit a run is for is fixed
    /// when the bundle is unpacked and cannot move under a redelivery. Freezing
    /// it anyway would copy a monorepo-sized path list onto every job row.
    ///
    /// A run this process cannot read at all yields an empty scope rather than
    /// an error: `ci.sha` resolving to null is a condition an author can see is
    /// wrong, whereas failing the job says nothing about what to fix.
    fn ci_scope(run: Option<&crate::store::Run>) -> Value {
        let Some(run) = run else {
            return Value::Object(Default::default());
        };
        serde_json::json!({
            "sha": run.sha,
            "before": run.before_sha,
            "ref": run.git_ref,
            "branch": run.git_ref.strip_prefix("refs/heads/").unwrap_or(&run.git_ref),
            "repository": run.repo_url,
            "run_id": run.id,
            "workflow": run.workflow_id,
            "changed_files": run.changes.paths(),
            // Read by `changed()`, and worth exposing on its own: a workflow
            // that wants to be careful can write
            // `if: !ci.changes_known || changed('api/**')` and get the same
            // build-when-unsure default the path filters apply.
            "changes_known": run.changes.is_known(),
            // Empty when the diff was read. Present because "my path filter
            // matched everything" is otherwise unexplainable from inside a
            // build: `echo ${{ ci.changes_reason }}` in a step is the answer.
            "changes_reason": run.changes.reason().unwrap_or_default(),
        })
    }

    /// Evaluate a job's `if:`.
    ///
    /// The default when there is no `if:` is GitHub's: run only if nothing this
    /// job needs failed. Writing an explicit `if:` opts out of that — which is
    /// how `if: always()` gets a cleanup job to run after a failure.
    fn should_run(&self, plan: &JobPlan, needs: &Value, ci: &Value) -> Result<bool, DispatchError> {
        let any_failed = plan.needs.iter().any(|n| {
            matches!(
                needs
                    .get(n)
                    .and_then(|v| v.get("result"))
                    .and_then(Value::as_str),
                Some("failure") | Some("cancelled")
            )
        });

        let Some(condition) = &plan.condition else {
            return Ok(!any_failed);
        };

        let mut ctx = plan.base_context();
        ctx.set("needs", needs.clone());
        ctx.set("ci", ci.clone());
        ctx.set_status(if any_failed { "failure" } else { "success" });
        ctx.eval_condition(condition)
            .map_err(|e| DispatchError::Condition(e.to_string()))
    }

    /// Stamp the run's network onto every job that does not name one, and refuse
    /// the submit if the result is a network this instance cannot dispatch to.
    ///
    /// Refusing *here* is the point. Without it the run is created, the jobs go
    /// to a queue nobody consumes, and the answer to "why is my build stuck" is
    /// a row in a table nobody thinks to look at. A submit that cannot run is an
    /// error at the client, naming the network and what is actually served.
    fn assign_network(
        &self,
        plan: &mut crate::plan::Plan,
        default_network: Option<&str>,
        warnings: &mut Vec<String>,
    ) -> Result<(), DispatchError> {
        let pool = self.runners.snapshot();
        for job in &mut plan.jobs {
            // `uses: default` names no network on purpose — it is wherever this
            // orchestrator's host happens to be — so the repository's assignment
            // must not be written over it.
            if job.target.network.is_none()
                && !job.target.local
                && let Some(net) = default_network
            {
                job.target.network = Some(net.to_string());
            }
            // Resolve the whole placement, not just the network: a `uses:` that
            // names a host or a VM this instance cannot reach is refused here,
            // at the client, rather than becoming a run whose jobs sit on a
            // queue nobody consumes.
            let placement = Self::place(&pool, job)?;
            // Warned, never refused. A host that is briefly unreachable is a
            // blip, and the job should sit on its subject until the host comes
            // back and its consumer binds — refusing here would turn a recovery
            // into a lost submit. The wait is bounded by CI_RUNNER_WAIT_SECS.
            if let Some(node) = placement.node
                && !node.status.is_dispatchable()
            {
                warnings.push(format!(
                    "{} is pinned to {} ({}), which is not online. The job will wait on \
                     that host's queue and run when it comes back, or fail after {}s.",
                    job.key,
                    node.name,
                    node.status.as_str(),
                    self.config.heyvm.runner_wait.as_secs()
                ));
            }
            // Canonical names, so the stored plan and the job row say what the
            // dashboard says rather than whichever of an id and a name somebody
            // typed.
            let network = placement.network.network_name.clone();
            let node = placement.node.map(|n| n.name.clone());
            job.target.network = Some(network);
            if let Some(node) = node {
                job.target.node = Some(node);
            }
        }
        Ok(())
    }

    /// The network a job runs in, resolved against the served pool.
    ///
    /// `plan.target.network` is set by `uses:` or, when the workflow does not
    /// say, stamped in at submit time from the repository's assignment. So by
    /// the time a job is routed the network is already decided — this only has
    /// to find it, and say so clearly when it is not something this instance
    /// serves.
    fn network_of<'a>(
        pool: &'a crate::runners::Pool,
        plan: &JobPlan,
    ) -> Result<&'a crate::runners::RunnerSet, DispatchError> {
        let Some(wanted) = plan.target.network.as_deref().map(str::trim) else {
            return pool.default_set().ok_or(DispatchError::NoNetwork);
        };
        match pool.find(wanted) {
            Some(set) if set.served => Ok(set),
            // The distinction is worth the extra variant: a network that exists
            // but is not served is a `CI_NETWORK` change, while one that does
            // not exist is a typo or a network somebody deleted.
            Some(set) => Err(DispatchError::UnservedNetwork {
                wanted: set.network_name.clone(),
                served: pool.served_names(),
            }),
            None => Err(DispatchError::UnknownNetwork {
                wanted: wanted.to_string(),
                served: pool.served_names(),
            }),
        }
    }

    /// Where a job actually runs: a network, optionally a pinned host, and
    /// optionally an existing VM on it.
    ///
    /// One function for all four `uses:` forms, because the four differ only in
    /// how much of the answer the author supplied — and because routing, runner
    /// selection and submit-time validation must agree. Three call sites reading
    /// `target` separately is how they drift.
    fn place<'a>(
        pool: &'a crate::runners::Pool,
        plan: &'a JobPlan,
    ) -> Result<Placement<'a>, DispatchError> {
        // `uses: default` names no network: it is whichever served network holds
        // this orchestrator's own host.
        if plan.target.local {
            if pool.default_node_id.is_empty() {
                return Err(DispatchError::NoDefaultNode);
            }
            let (network, node) = pool.locate(&pool.default_node_id).ok_or_else(|| {
                DispatchError::DefaultNodeUnserved {
                    node: pool.default_node_id.clone(),
                    served: pool.served_names(),
                }
            })?;
            return Ok(Placement {
                network,
                node: Some(node),
                vm: plan.target.vm.as_deref(),
            });
        }

        let network = Self::network_of(pool, plan)?;
        let Some(wanted) = plan.target.node.as_deref() else {
            return Ok(Placement {
                network,
                node: None,
                vm: None,
            });
        };

        match network.find(wanted) {
            Some(node) => Ok(Placement {
                network,
                node: Some(node),
                vm: plan.target.vm.as_deref(),
            }),
            // `fallback: any` cannot apply to a job that named a VM: the VM
            // exists on one host, and "any host" would run the steps somewhere
            // that does not have it.
            None if plan.fallback == Fallback::Any && plan.target.vm.is_none() => {
                tracing::warn!(
                    node = wanted,
                    network = network.network_name,
                    "no such node in this network; falling back to any host \
                     because the job set `fallback: any`"
                );
                Ok(Placement {
                    network,
                    node: None,
                    vm: None,
                })
            }
            None => Err(DispatchError::UnknownRunner {
                wanted: wanted.to_string(),
                network: network.network_name.clone(),
            }),
        }
    }

    /// Which queue a job goes on.
    ///
    /// A pinned job goes to its host's queue **even when that host is offline**,
    /// unless it opted into `fallback: any`. That is deliberate: the warm pool is
    /// host-local, so silently moving the job discards the cache the pin asked
    /// for. The job waits in that host's queue and the dashboard shows why.
    async fn route_for(&self, plan: &JobPlan) -> Result<Route, DispatchError> {
        let pool = self.runners.snapshot();
        let placement = Self::place(&pool, plan)?;

        // A resolved node is a pinned queue, whatever put it there — `uses:
        // default`, an explicit node, or a named VM. Only "any host in this
        // network" goes on the network's shared queue.
        if let Some(node) = placement.node {
            return Ok(Route::Runner(node.id.clone()));
        }
        if placement.network.network_id.is_empty() {
            return Err(DispatchError::NoNetwork);
        }
        Ok(Route::Network(placement.network.network_id.clone()))
    }

    // ---- execution ------------------------------------------------------

    /// Run one job to completion. Returns the status it reached.
    pub async fn run_job(
        &self,
        msg: &JobMessage,
        attempt: i32,
    ) -> Result<JobStatus, DispatchError> {
        let Some(row) = self.store.get_job(&msg.job_id).await? else {
            return Err(DispatchError::UnknownJob(msg.job_id.clone()));
        };
        if matches!(
            row.status.as_str(),
            "success" | "failure" | "skipped" | "cancelled"
        ) {
            // A redelivery of work that finished just before its ack was lost.
            tracing::info!(job = %msg.job_key, "already {}; dropping redelivery", row.status);
            return Ok(JobStatus::Success);
        }
        let plan: JobPlan = serde_json::from_value(row.plan.clone())
            .map_err(|e| DispatchError::BadPlan(e.to_string()))?;

        let (runner, existing_vm) = self.pick_runner(&plan).await?;

        // The one place a job's failure and its runner are both in hand. A
        // transport-level failure means the cached iroh tunnel is dead — the
        // daemon restarted, the host rebooted — and every request over it fails
        // identically, so without this the NAK'd retries only rediscover the
        // same dead local port four times. Evicting makes the next attempt
        // redial, which is the repair.
        let result = self
            .run_claimed(msg, attempt, plan, runner.clone(), existing_vm)
            .await;
        if let Err(e) = &result
            && e.is_tunnel_failure()
        {
            self.runners.evict(&runner).await;
        }
        result
    }

    /// The claimed half of [`Self::run_job`]: everything after a runner is
    /// chosen, split out so its errors can be inspected with the runner id
    /// still in hand.
    async fn run_claimed(
        &self,
        msg: &JobMessage,
        attempt: i32,
        mut plan: JobPlan,
        runner: String,
        existing_vm: Option<String>,
    ) -> Result<JobStatus, DispatchError> {
        // Said before the VM exists, not after. Getting one means an iroh dial
        // and a boot, and until this the job stayed `queued` with no runner for
        // the whole of it — so a build that was three minutes into booting and
        // one nothing had touched looked the same on every page, and the
        // waiting-for-a-runner reaper could not tell them apart either.
        //
        // False means the job went terminal while it sat on the queue —
        // cancelled, or finished by a delivery whose ack was lost — and the
        // right move is to stop here, before spending a VM on it.
        if !self.store.claim_job(&msg.job_id, &runner, attempt).await? {
            tracing::info!(job = %msg.job_key, "no longer runnable; dropping delivery");
            return Ok(JobStatus::Success);
        }
        self.bus
            .publish_event(
                &msg.run_id,
                &plan.key,
                &serde_json::json!({
                    "status": "running", "runner": runner,
                    "phase": "acquiring a VM", "attempt": attempt
                }),
            )
            .await;
        tracing::info!(job = %plan.key, runner = %runner, attempt, "acquiring a VM");

        let workspace = self.workspace(&msg.run_id);

        // `vm.build` becomes `vm.image` here, building the image on the runner
        // if that host does not have it yet. Resolved before the fingerprint is
        // taken, so the pool keys on the image that will actually be used — and
        // since that name is the hash of the Dockerfile and its context, editing
        // the Dockerfile busts the warm pool as well as the image, which is
        // exactly right: a VM built from the old rootfs is not reusable for a
        // job that asked for the new one.
        //
        // On the *local* plan only. The stored plan keeps what the author wrote,
        // so a redelivery re-derives the name rather than inheriting one.
        if let Some(build) = plan.vm.build.clone() {
            let image = self
                .ensure_image(&runner, &plan, &build, &workspace, msg)
                .await?;
            plan.vm.image = Some(image);
            plan.vm.build = None;
        }

        // Two ways to get a machine, and they share nothing but the handle.
        //
        // A job that named a VM in `uses:` runs in one that already exists: no
        // fingerprint, no pool, no creation, and — see `release_vm` — no
        // teardown. The `vm:` block is inert for it. Everything else builds or
        // claims one from the warm pool as usual.
        let (vm, reused, fingerprint) = match existing_vm.as_deref() {
            Some(wanted) => {
                let sandbox_id = self.resolve_existing_vm(&runner, wanted).await?;
                let options = self.runners.options_for(&runner).await?;
                let vm = self.vms.open(options, sandbox_id).await?;
                // It may simply be stopped, which is recoverable and worth
                // recovering: somebody pointed a job at this VM deliberately.
                vm.ensure_running(BOOT_TIMEOUT).await?;
                tracing::info!(
                    job = %plan.key, vm = vm.id(),
                    "using an existing VM; the `vm:` block is not applied to it"
                );
                // Not a pool fingerprint, because nothing about this VM was
                // decided by one. The column still has to say something, and
                // saying `existing` is more use than an unrelated hash.
                (vm, true, "existing".to_string())
            }
            None => {
                let fingerprint = crate::pool::fingerprint(&plan.vm, &workspace)?;
                let (vm, reused) = self
                    .acquire_vm(&runner, &plan, &fingerprint, &msg.job_id)
                    .await?;
                (vm, reused, fingerprint)
            }
        };

        if !self
            .store
            .start_job(&msg.job_id, &runner, vm.id(), &fingerprint, attempt)
            .await?
        {
            // Something else finished this job while we were booting a VM.
            self.release_vm(&plan, &vm, false).await;
            return Ok(JobStatus::Success);
        }
        self.bus
            .publish_event(
                &msg.run_id,
                &plan.key,
                &serde_json::json!({
                    "status": "running", "runner": runner,
                    "sandbox": vm.id(), "reusedVm": reused, "attempt": attempt
                }),
            )
            .await;
        tracing::info!(
            job = %plan.key, runner = %runner, vm = vm.id(), reused,
            "running"
        );

        let outcome = match self.checkout(msg, &plan, &vm).await {
            Ok(()) => self.run_steps(msg, &plan, &vm).await,
            Err(e) => Err(e),
        };
        // Before the release, always: a VM with `reuse: false` is destroyed on
        // the next line, and the console of the boot that just failed is exactly
        // what somebody wants when a job dies before its first step.
        self.capture_vm_log(msg, &plan, &vm).await;
        let guest_corrupted = outcome
            .as_ref()
            .err()
            .is_some_and(DispatchError::indicates_guest_corruption);
        self.release_vm(&plan, &vm, guest_corrupted).await;

        // A tunnel that dies mid-job fails the job rather than propagating —
        // the match below absorbs the error into a status — so the eviction in
        // `run_job` never sees it. Done here so the *next* job redials instead
        // of inheriting the dead port.
        if let Some(e) = outcome.as_ref().err()
            && e.is_tunnel_failure()
        {
            self.runners.evict(&runner).await;
        }

        let status = match &outcome {
            Ok(outputs) => {
                self.store.set_job_outputs(&msg.job_id, outputs).await?;
                JobStatus::Success
            }
            // Cancelled stays cancelled. `continue_on_error` is about a step
            // failing, not about somebody stopping the run — and writing
            // `failure` here would overwrite the status that cancelling just
            // set, making a deliberate stop look like a broken build.
            Err(DispatchError::Cancelled(_)) => JobStatus::Cancelled,
            Err(_) if plan.continue_on_error => JobStatus::Success,
            Err(_) => JobStatus::Failure,
        };
        let error = outcome.as_ref().err().map(|e| e.to_string());
        self.store
            .set_job_status(&msg.job_id, status, error.as_deref())
            .await?;
        self.bus
            .publish_event(
                &msg.run_id,
                &plan.key,
                &serde_json::json!({"status": status.as_str(), "error": error}),
            )
            .await;
        Ok(status)
    }

    /// Resolve the plan's target to a concrete online runner, and the existing
    /// VM on it when `uses:` named one.
    ///
    /// Both come from one [`Self::place`] call rather than the caller re-reading
    /// `target`: the node and the VM are one decision, and reading the target
    /// twice is how the queue a job was routed to and the machine it runs on
    /// come to disagree.
    async fn pick_runner(&self, plan: &JobPlan) -> Result<(String, Option<String>), DispatchError> {
        let pool = self.runners.snapshot();
        let placement = Self::place(&pool, plan)?;
        // `place` only ever yields a VM alongside the node holding it, so this
        // cannot name a VM without saying where it is.
        let vm = placement.vm.map(str::to_string);

        if let Some(node) = placement.node {
            if !node.status.is_dispatchable() {
                return Err(DispatchError::RunnerOffline {
                    runner: node.name.clone(),
                    status: node.status.as_str(),
                });
            }
            return Ok((node.id.clone(), vm));
        }
        // Least-recently-used across the online set would need per-runner load;
        // for now the first online host wins, which is stable and predictable.
        placement
            .network
            .dispatchable()
            .next()
            .map(|r| (r.id.clone(), vm))
            .ok_or_else(|| DispatchError::NoOnlineRunner(placement.network.network_name.clone()))
    }

    /// Resolve `vm.build` to an image name, building it on `runner` if that
    /// host does not already have it.
    ///
    /// The name is the content hash of the Dockerfile and its context, so this
    /// is a cache lookup that happens to be able to fill itself: the same
    /// Dockerfile asks for an image the host already has, and any change asks
    /// for one it does not.
    ///
    /// The build itself runs on the runner — its daemon runs the same
    /// docker → export → mke2fs pipeline `heyvm mvm build` runs locally, so
    /// the host's docker layer cache applies and no builder VM is booted.
    /// This process only uploads the inputs and polls.
    ///
    /// Concurrency is settled twice, at two scopes. [`crate::image::Catalog::claim`]
    /// hands exactly one *job* the build and tells the rest to wait, so N jobs
    /// landing on a cold host produce one build request and not N. And the
    /// daemon's own route is idempotent by name, so even the claim being lost
    /// — a lapsed lease handing the build to a second dispatcher — collapses
    /// into one docker build rather than two racing for the same tag.
    async fn ensure_image(
        &self,
        runner: &str,
        plan: &JobPlan,
        build: &crate::vm::ImageBuild,
        workspace: &std::path::Path,
        msg: &JobMessage,
    ) -> Result<String, DispatchError> {
        /// How long to wait — for somebody else's build of the same image, and
        /// for one this job runs itself. One bound for both, because a waiter
        /// that gives up before the builder finishes fails a job the next
        /// delivery would have found ready.
        const BUILD_BUDGET: Duration = Duration::from_secs(40 * 60);
        const WAIT_POLL: Duration = Duration::from_secs(10);

        let build_plan = crate::image::plan_for(build, &plan.vm, workspace)?;
        let name = build_plan.name.clone();

        let deadline = std::time::Instant::now() + BUILD_BUDGET;
        loop {
            match self
                .images
                .claim(
                    &name,
                    runner,
                    &plan.base_id,
                    &msg.job_id,
                    crate::image::BUILD_LEASE,
                )
                .await?
            {
                crate::image::Claim::Ready => {
                    tracing::info!(job = %plan.key, runner, "image {name} is already on this host");
                    return Ok(name);
                }
                crate::image::Claim::Build => break,
                crate::image::Claim::InProgress => {
                    if std::time::Instant::now() >= deadline {
                        return Err(crate::image::ImageError::WaitTimeout {
                            name,
                            waited: BUILD_BUDGET,
                        }
                        .into());
                    }
                    tracing::info!(
                        job = %plan.key,
                        "another job is building image {name} on {runner}; waiting"
                    );
                    tokio::time::sleep(WAIT_POLL).await;
                }
            }
        }

        // This job owns the build.
        tracing::info!(
            job = %plan.key, runner,
            "asking the runner to build image {name} from {}", build.dockerfile
        );
        let options = self.runners.options_for(runner).await?;
        let outcome = crate::image::build_remote(
            options,
            &build_plan,
            build.size_mb,
            BUILD_BUDGET,
            // Renewed on every poll so the catalog claim outlives a long
            // build; the daemon is doing the work, so there is no VM lease or
            // heartbeat task to piggyback on.
            || async {
                if let Err(e) = self
                    .images
                    .renew(&name, runner, crate::image::BUILD_LEASE)
                    .await
                {
                    tracing::warn!("could not renew the image build claim: {e}");
                }
            },
        )
        .await;

        match outcome {
            Ok(built) => {
                self.attach_build_log(msg, plan, &name, &built.log).await;
                self.images
                    .mark_ready(&name, runner, built.size_bytes)
                    .await?;
                tracing::info!(
                    job = %plan.key,
                    "image {name} is ready ({} bytes)", built.size_bytes
                );
                Ok(name)
            }
            Err(e) => {
                let detail = e.to_string();
                // Recorded before the job fails, so /vms says why rather than
                // leaving the reason only in this process's log.
                if let Err(e) = self.images.mark_failed(&name, runner, &detail).await {
                    tracing::warn!("could not record the failed image build: {e}");
                }
                self.attach_build_log(msg, plan, &name, &format!("[ci] {detail}\n"))
                    .await;
                Err(e.into())
            }
        }
    }

    /// Attach an image build's log to the job, as a step at index `-3`.
    ///
    /// The same trick checkout uses at `-1` and the VM console at `-2`: it needs
    /// a row, a file on disk and a place in the UI, and a step already is all
    /// three — including the retention sweep. Never fails the job; a log that
    /// could not be written must not turn a successful build into a failure.
    async fn attach_build_log(&self, msg: &JobMessage, plan: &JobPlan, name: &str, text: &str) {
        let sid = format!("{}.imglog", msg.job_id);
        if let Err(e) = self
            .store
            .create_step(&sid, &msg.job_id, -3, &format!("Image {name}"), None)
            .await
        {
            tracing::warn!(job = %plan.key, "could not record the image build step: {e}");
            return;
        }
        let path = self.store.log_path(&msg.run_id, &plan.key, -3, &sid);
        if let Err(e) = self.store.append_log(&sid, &path, text).await {
            tracing::warn!(job = %plan.key, "could not write the image build log: {e}");
        }
        let _ = self
            .store
            .finish_step(&sid, StepStatus::Success, Some(0), None)
            .await;
    }

    /// A VM for this job: an inherited one if the fingerprint matches, else new.
    async fn acquire_vm(
        &self,
        runner: &str,
        plan: &JobPlan,
        fingerprint: &str,
        job_id: &str,
    ) -> Result<(Vm, bool), DispatchError> {
        let options = self.runners.options_for(runner).await?;

        if plan.vm.reuse
            && let Some(sandbox_id) = self
                .pool
                .claim(runner, fingerprint, job_id, self.lease())
                .await?
        {
            let vm = self.vms.open(options.clone(), sandbox_id.clone()).await?;
            // A pooled VM may have been stopped between jobs, which is normal
            // and recoverable; anything else means the daemon lost it, and the
            // right move is to forget it and build a fresh one rather than fail
            // a job over a stale row.
            match vm.ensure_running(BOOT_TIMEOUT).await {
                Ok(()) => {
                    let _ = vm.renew_ttl(self.config.heyvm.vm_ttl).await;
                    return Ok((vm, true));
                }
                Err(e) => {
                    tracing::warn!(
                        vm = %sandbox_id,
                        "pooled VM is unusable, discarding it and building a fresh one: {e}"
                    );
                    let _ = self.pool.forget(&sandbox_id).await;
                }
            }
        }

        let name = sandbox_name(
            &plan.base_id,
            fingerprint,
            NONCE.fetch_add(1, Ordering::Relaxed),
        );

        // Recorded before the attempt, so /vms shows the machine while it is
        // coming up rather than only once it has. This is the longest silent
        // stretch of a job — `create` waits out `BOOT_TIMEOUT` for a cold VM —
        // and it is also where a workflow naming an image the host does not have
        // fails, over and over on the redelivery ladder, with nothing on any
        // page to say so.
        //
        // Best-effort: a database hiccup here must not cost a build. The row is
        // for looking at.
        let building = match self
            .pool
            .begin_build(job_id, runner, fingerprint, &plan.base_id, self.lease())
            .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(job = %plan.key, "could not record the VM as building: {e}");
                None
            }
        };
        tracing::info!(
            job = %plan.key, runner = %runner, image = plan.vm.image.as_deref().unwrap_or("(default)"),
            "creating VM {name}"
        );

        let created = self
            .vms
            .create(
                options,
                &name,
                &plan.vm,
                self.config.heyvm.vm_ttl,
                BOOT_TIMEOUT,
            )
            .await;

        // Whichever way it went, the placeholder goes: it stands for an attempt
        // in flight, and on success `register` below writes the real row under
        // the id the daemon assigned. Dropped before `?` on the result, or a
        // failed create would leave it behind until the lease swept it.
        if let Some(id) = &building
            && let Err(e) = self.pool.forget(id).await
        {
            tracing::warn!(job = %plan.key, "could not clear the building row {id}: {e}");
        }

        // The catalog says this host has the image; the daemon says otherwise.
        // Somebody deleted the `.ext4` by hand, or the host was rebuilt. Forget
        // the row so the next delivery builds it again rather than failing on
        // the same missing file for ever — the same self-healing `claim` above
        // does for a pooled VM the daemon lost.
        if let Err(e) = &created
            && let Some(image) = plan.vm.image.as_deref()
            && image.starts_with("ci-img-")
            && e.to_string().contains("not found")
        {
            tracing::warn!(
                job = %plan.key,
                "{runner} does not have image {image} after all; forgetting it so the \
                 next attempt rebuilds: {e}"
            );
            let _ = self.images.forget(image, runner).await;
        }
        let vm = created?;

        self.pool
            .register(
                vm.id(),
                runner,
                fingerprint,
                &plan.base_id,
                job_id,
                self.lease(),
            )
            .await?;
        Ok((vm, false))
    }

    /// Attach the VM's own console to the run.
    ///
    /// Recorded as a step at index `-2`, the same trick checkout uses at `-1`:
    /// it needs a row, a log file on disk and a place in the UI, and a step
    /// already is all three — including the retention sweep, which walks step
    /// logs and would otherwise miss a log kept anywhere else.
    ///
    /// **Never fails the job.** By the time this runs the steps have already
    /// decided the outcome, and a job that passed must not be reported as failed
    /// because a diagnostic could not be fetched.
    async fn capture_vm_log(&self, msg: &JobMessage, plan: &JobPlan, vm: &Vm) {
        let sid = format!("{}.vmlog", msg.job_id);
        if let Err(e) = self
            .store
            .create_step(&sid, &msg.job_id, -2, "VM log", None)
            .await
        {
            tracing::warn!(job = %plan.key, "could not record the VM log step: {e}");
            return;
        }

        let text = match vm.logs(self.config.vm_log_lines).await {
            Ok(text) if text.trim().is_empty() => {
                "[ci] the daemon reported no console output for this VM\n".to_string()
            }
            Ok(text) => text,
            Err(e) => format!("[ci] could not read this VM's logs: {e}\n"),
        };

        let path = self.store.log_path(&msg.run_id, &plan.key, -2, &sid);
        if let Err(e) = self.store.append_log(&sid, &path, &text).await {
            tracing::warn!(job = %plan.key, "could not write the VM log: {e}");
        }
        // Always `success`: this step is a place to hang a log, not a verdict on
        // the job. A red row here would read as the build having failed.
        let _ = self
            .store
            .finish_step(&sid, StepStatus::Success, Some(0), None)
            .await;
    }

    /// Resolve the VM named by `uses: <network>/<node>/<vm>` to a sandbox id.
    ///
    /// By id or by name, the same two spellings a node accepts — `uses:` is
    /// written by hand and the dashboard shows both. Listed from the node the
    /// job is pinned to rather than searched for across the network, which is
    /// exactly what naming the node in the path bought.
    async fn resolve_existing_vm(
        &self,
        runner: &str,
        wanted: &str,
    ) -> Result<String, DispatchError> {
        let options = self.runners.options_for(runner).await?;
        let sandboxes = heyo_sdk::Sandbox::list(options).await.map_err(|e| {
            DispatchError::Vm(crate::vm::VmError::Daemon {
                sandbox: wanted.to_string(),
                what: "listing sandboxes on the node",
                source: e,
            })
        })?;

        if let Some(found) = sandboxes
            .iter()
            .find(|s| s.id == wanted || s.name.eq_ignore_ascii_case(wanted))
        {
            return Ok(found.id.clone());
        }
        Err(DispatchError::UnknownVm {
            wanted: wanted.to_string(),
            node: runner.to_string(),
            available: sandboxes
                .iter()
                .map(|s| {
                    if s.name.is_empty() {
                        s.id.clone()
                    } else {
                        format!("{} ({})", s.name, s.id)
                    }
                })
                .collect(),
        })
    }

    /// Hand the VM back, or destroy it when the workflow said not to reuse.
    ///
    /// Failures here are logged, never propagated: the job's result is already
    /// decided, and turning a green build red because a TTL renewal failed would
    /// be worse than a VM that expires on its own.
    /// `guest_corrupted` is the job's verdict on the VM itself, not on the
    /// work: the guest filesystem returned an error no retry can survive, so
    /// the VM is destroyed even though `reuse` asked for pooling. Repooling it
    /// would hand the next attempt — which prefers an idle VM with the same
    /// fingerprint on the same runner — the same broken ext4, and the job
    /// would burn every delivery on one sick machine.
    async fn release_vm(&self, plan: &JobPlan, vm: &Vm, guest_corrupted: bool) {
        // A VM named in `uses:` is not ours. It was not created for this job,
        // it is not in the pool, and somebody else's long-lived machine must not
        // be destroyed because a workflow happened to set `reuse: false` in a
        // `vm:` block that never applied to it. Its TTL is left alone for the
        // same reason — renewing it would be this app quietly extending the life
        // of something it does not own. That holds even for corruption: the
        // owner gets a warning, not a destroyed machine.
        if plan.target.is_existing_vm() {
            if guest_corrupted {
                tracing::warn!(
                    vm = vm.id(),
                    "this job saw guest filesystem corruption, but the VM is a `uses:` \
                     target this instance does not own — leaving it as it is"
                );
            }
            return;
        }
        if guest_corrupted {
            tracing::warn!(
                vm = vm.id(),
                "guest filesystem corruption; destroying the VM instead of repooling it"
            );
        }
        if !plan.vm.reuse || guest_corrupted {
            if let Err(e) = vm.destroy().await {
                tracing::warn!(vm = vm.id(), "could not destroy: {e}");
            }
            if let Err(e) = self.pool.forget(vm.id()).await {
                tracing::warn!(vm = vm.id(), "could not forget: {e}");
            }
            return;
        }
        if let Err(e) = vm.renew_ttl(self.config.heyvm.vm_ttl).await {
            tracing::warn!(vm = vm.id(), "could not renew the TTL: {e}");
        }
        if let Err(e) = self.pool.release(vm.id()).await {
            tracing::warn!(vm = vm.id(), "could not release into the pool: {e}");
        }
    }

    /// Put the submitted tree into the guest.
    ///
    /// Recorded as a step at index `-1` so it sorts before the workflow's own
    /// steps and shows up in the UI. Checkout failing is the single most common
    /// "why did nothing run" cause, and burying it in the job's error field
    /// makes it the one thing with no log to read.
    ///
    /// The working directory is wiped first. A pooled VM arrives with the
    /// previous job's tree still in it, and a build that succeeds only because a
    /// deleted file is still on disk is the exact failure the pool must not
    /// introduce.
    async fn checkout(
        &self,
        msg: &JobMessage,
        plan: &JobPlan,
        vm: &Vm,
    ) -> Result<(), DispatchError> {
        let sid = format!("{}.checkout", msg.job_id);
        self.store
            .create_step(&sid, &msg.job_id, -1, "Checkout", None)
            .await?;
        self.store.start_step(&sid, &sid).await?;
        let log_path = self.store.log_path(&msg.run_id, &plan.key, -1, &sid);

        let workspace = crate::trigger::Workspace::for_run(&self.config, &msg.run_id);
        let Some((format, archive)) = workspace.stored_source() else {
            let detail = format!(
                "no submitted source is on disk for run {} under {}",
                msg.run_id,
                self.config.workspace_dir.display()
            );
            self.store
                .append_log(&sid, &log_path, &format!("[ci] {detail}\n"))
                .await?;
            self.store
                .finish_step(&sid, StepStatus::Failure, Some(1), Some(&detail))
                .await?;
            return Err(DispatchError::Checkout(detail));
        };
        let archive = archive.to_path_buf();
        let bytes = match tokio::fs::read(&archive).await {
            Ok(b) => b,
            Err(e) => {
                let detail = format!(
                    "the submitted source is missing at {}: {e}",
                    archive.display()
                );
                self.store
                    .append_log(&sid, &log_path, &format!("[ci] {detail}\n"))
                    .await?;
                self.store
                    .finish_step(&sid, StepStatus::Failure, None, Some(&detail))
                    .await?;
                return Err(DispatchError::Checkout(detail));
            }
        };

        let workdir = plan
            .vm
            .working_directory
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKDIR.to_string());
        let wd = workdir.trim_end_matches('/');
        let remote = match format {
            crate::trigger::SourceFormat::TarGz => format!("{wd}/.ci-source.tar.gz"),
            crate::trigger::SourceFormat::GitBundle => format!("{wd}/.ci-source.bundle"),
        };

        let result = async {
            vm.upload_bytes(&sid, &remote, &bytes).await?;
            let script = match format {
                // `--strip-components` is deliberately absent: `git archive`
                // writes paths relative to the repository root already, and
                // stripping would silently drop a top-level file.
                crate::trigger::SourceFormat::TarGz => format!(
                    "set -e; mkdir -p {wd}; find {wd} -mindepth 1 -maxdepth 1 \
                     ! -name .ci-source.tar.gz -exec rm -rf {{}} +; \
                     tar -xzf {src} -C {wd}; rm -f {src}; ls -a {wd} | head -50",
                    wd = shell_quote(&workdir),
                    src = shell_quote(&remote),
                ),
                // Cloned into a scratch directory and then moved into place,
                // because `git clone` refuses a destination that already has
                // anything in it — and the destination here is the mount the
                // bundle was just uploaded into. The bundle is removed after,
                // so a step never sees it as repository content.
                //
                // `git` in the guest is a hard requirement of this format;
                // `command -v` turns its absence into one line naming the fix
                // rather than a bare `not found` from a subshell.
                crate::trigger::SourceFormat::GitBundle => format!(
                    "set -e; \
                     command -v git >/dev/null 2>&1 || {{ \
                       echo '[ci] this run submitted a git bundle, but the guest image \
has no git. Add it to the vm setup_hooks, or submit with `git submit --archive`.' >&2; \
                       exit 127; }}; \
                     mkdir -p {wd}; rm -rf {tmp}; \
                     git -c core.hooksPath=/nonexistent clone --quiet {src} {tmp}; \
                     find {wd} -mindepth 1 -maxdepth 1 ! -name .ci-clone ! -name .ci-source.bundle \
                       -exec rm -rf {{}} +; \
                     tar -C {tmp} -cf - . | tar -C {wd} -xf -; \
                     rm -rf {tmp} {src}; \
                     git -C {wd} log --oneline -1; ls -a {wd} | head -50",
                    wd = shell_quote(&workdir),
                    tmp = shell_quote(&format!("{wd}/.ci-clone")),
                    src = shell_quote(&remote),
                ),
            };
            vm.exec(
                &format!("{sid}.x"),
                &script,
                &HashMap::new(),
                Duration::from_secs(300),
            )
            .await
        }
        .await;

        match result {
            Ok(out) if out.succeeded() => {
                self.store
                    .append_log(
                        &sid,
                        &log_path,
                        &format!(
                            "[ci] {} bytes extracted into {workdir}\n{}",
                            bytes.len(),
                            out.combined()
                        ),
                    )
                    .await?;
                self.store
                    .finish_step(&sid, StepStatus::Success, Some(0), None)
                    .await?;
                Ok(())
            }
            Ok(out) => {
                self.store
                    .append_log(&sid, &log_path, &out.combined())
                    .await?;
                self.store
                    .finish_step(&sid, StepStatus::Failure, Some(out.exit_code), None)
                    .await?;
                Err(DispatchError::Checkout(format!(
                    "extracting the source exited {}",
                    out.exit_code
                )))
            }
            Err(e) => {
                let detail = e.to_string();
                self.store
                    .append_log(&sid, &log_path, &format!("[ci] {detail}\n"))
                    .await?;
                self.store
                    .finish_step(&sid, StepStatus::Failure, None, Some(&detail))
                    .await?;
                Err(DispatchError::Checkout(detail))
            }
        }
    }

    /// Run every step, stopping at the first failure that is not tolerated.
    ///
    /// Returns the job's outputs on success.
    async fn run_steps(
        &self,
        msg: &JobMessage,
        plan: &JobPlan,
        vm: &Vm,
    ) -> Result<Value, DispatchError> {
        let needs = self.store.needs_context(&msg.run_id).await?;

        // Once per job, not per step: heyosecret has no batch read, so N secrets
        // is N round trips and doing that per step would multiply it by the step
        // count.
        let run = self.store.get_run(&msg.run_id).await?;
        let workflow_id = run
            .as_ref()
            .map(|r| r.workflow_id.clone())
            .unwrap_or_default();
        // The same scope the scheduler evaluated the job's own `if:` against, so
        // a step condition and a job condition cannot disagree about which
        // commit they are looking at.
        let ci = Self::ci_scope(run.as_ref());
        let environment = plan
            .env
            .get("CI_ENVIRONMENT")
            .cloned()
            .unwrap_or_else(|| "default".to_string());
        let prefix = crate::secrets::Secrets::prefix(&workflow_id, &environment);
        let resolved = self
            .secrets
            .resolve(&prefix)
            .await
            .map_err(|e| DispatchError::Secrets(format!("resolving {prefix}: {e}")))?;
        let masker = resolved.masker();
        let (secret_scope, vars_scope) = resolved.scopes();

        let mut step_outputs = serde_json::Map::new();
        let mut failed: Option<String> = None;

        for (idx, step) in plan.steps.iter().enumerate() {
            // Cancellation is cooperative, and this is where it takes effect.
            // The daemon has no route to abort an exec-operation in flight, so
            // a step that has started runs to its own end or its timeout; what
            // stops is everything after it.
            if self.store.is_job_cancelled(&msg.job_id).await? {
                return Err(DispatchError::Cancelled(plan.key.clone()));
            }
            let sid = step_id(&msg.job_id, idx);
            self.store
                .create_step(
                    &sid,
                    &msg.job_id,
                    idx as i32,
                    &step.label(idx),
                    step.uses.as_deref(),
                )
                .await?;

            let mut ctx = plan.base_context();
            ctx.set("needs", needs.clone());
            ctx.set("steps", Value::Object(step_outputs.clone()));
            ctx.set("secrets", secret_scope.clone());
            ctx.set("vars", vars_scope.clone());
            ctx.set("ci", ci.clone());
            ctx.set_status(if failed.is_some() {
                "failure"
            } else {
                "success"
            });

            // A step after a failure is skipped unless it says otherwise, so
            // `if: always()` is what gets teardown to run.
            let should = match &step.condition {
                Some(c) => ctx
                    .eval_condition(c)
                    .map_err(|e| DispatchError::Condition(e.to_string()))?,
                None => failed.is_none(),
            };
            if !should {
                self.store
                    .finish_step(&sid, StepStatus::Skipped, None, None)
                    .await?;
                continue;
            }

            let Some(run) = &step.run else {
                let action = step.uses.as_deref().unwrap_or("");
                let log_path = self
                    .store
                    .log_path(&msg.run_id, &plan.key, idx as i32, &sid);
                self.store.start_step(&sid, &sid).await?;
                match self
                    .run_action(msg, plan, vm, action, step, &ctx, &sid, &log_path)
                    .await
                {
                    Ok(note) => {
                        self.store
                            .append_log(&sid, &log_path, &masker.mask(&note))
                            .await?;
                        self.store
                            .finish_step(&sid, StepStatus::Success, Some(0), None)
                            .await?;
                    }
                    Err(e) => {
                        let detail = masker.mask(&e.to_string());
                        self.store
                            .append_log(&sid, &log_path, &format!("[ci] {detail}\n"))
                            .await?;
                        self.store
                            .finish_step(&sid, StepStatus::Failure, None, Some(&detail))
                            .await?;
                        if !step.continue_on_error {
                            failed = Some(detail);
                            break;
                        }
                    }
                }
                continue;
            };

            self.store.start_step(&sid, &sid).await?;
            let log_path = self
                .store
                .log_path(&msg.run_id, &plan.key, idx as i32, &sid);

            let env = self.step_env(plan, step, &ctx);
            let command = wrap_command(
                &ctx.substitute(run),
                step,
                &sid,
                plan.vm
                    .working_directory
                    .as_deref()
                    .unwrap_or(DEFAULT_WORKDIR),
            );
            let timeout = step
                .timeout_minutes
                .map(|m| Duration::from_secs(m * 60))
                .unwrap_or(DEFAULT_STEP_TIMEOUT)
                .min(plan.timeout);

            match vm.exec(&sid, &command, &env, timeout).await {
                Ok(out) => {
                    let (text, outputs) = split_outputs(&out, &sid);
                    // Masked before it is persisted, not when it is rendered: a
                    // secret that reaches disk in plain text has leaked, and
                    // hiding it from one reader does not un-leak it.
                    self.store
                        .append_log(&sid, &log_path, &masker.mask(&text))
                        .await?;
                    if let Some(id) = &step.id {
                        step_outputs.insert(
                            id.clone(),
                            serde_json::json!({ "outputs": outputs, "outcome":
                                if out.succeeded() { "success" } else { "failure" } }),
                        );
                    }
                    let ok = out.succeeded();
                    self.store
                        .finish_step(
                            &sid,
                            if ok {
                                StepStatus::Success
                            } else {
                                StepStatus::Failure
                            },
                            Some(out.exit_code),
                            None,
                        )
                        .await?;
                    if !ok && !step.continue_on_error {
                        failed = Some(format!(
                            "step {:?} exited {}",
                            step.label(idx),
                            out.exit_code
                        ));
                        break;
                    }
                }
                Err(e) => {
                    // The command never ran, or the daemon lost it. Distinct
                    // from a non-zero exit, and recorded as such.
                    let msg = masker.mask(&e.to_string());
                    self.store
                        .append_log(&sid, &log_path, &format!("\n[ci] {msg}\n"))
                        .await?;
                    self.store
                        .finish_step(&sid, StepStatus::Failure, None, Some(&msg))
                        .await?;
                    failed = Some(msg);
                    break;
                }
            }
        }

        if let Some(reason) = failed {
            return Err(DispatchError::StepFailed(reason));
        }

        // Job outputs are expressions over the step outputs collected above.
        let mut ctx = plan.base_context();
        ctx.set("needs", needs);
        ctx.set("steps", Value::Object(step_outputs));
        ctx.set("secrets", secret_scope);
        ctx.set("vars", vars_scope);
        ctx.set("ci", ci);
        // Masked as well: a job output is read by the next job's `if:` and shown
        // on the dashboard, so an output that interpolated a secret would put it
        // somewhere a log masker never sees.
        let outputs: serde_json::Map<String, Value> = plan
            .outputs
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(masker.mask(&ctx.substitute(v)))))
            .collect();
        Ok(Value::Object(outputs))
    }

    /// Run a built-in `uses:` action.
    ///
    /// Only the artifact actions exist. Composite actions — fetching an
    /// `action.yml` from a repository and running its steps — are a different
    /// feature with a different trust model, and pretending to support them by
    /// silently doing nothing would be worse than saying so.
    #[allow(clippy::too_many_arguments)]
    async fn run_action(
        &self,
        msg: &JobMessage,
        plan: &JobPlan,
        vm: &Vm,
        action: &str,
        step: &Step,
        ctx: &Context,
        sid: &str,
        _log_path: &std::path::Path,
    ) -> Result<String, DispatchError> {
        let with = |k: &str| step.with.get(k).map(|v| ctx.substitute(v));

        match action {
            "ci/upload-artifact" => {
                let name = with("name").ok_or_else(|| {
                    DispatchError::Artifact("ci/upload-artifact needs `with.name`".into())
                })?;
                let path = with("path").ok_or_else(|| {
                    DispatchError::Artifact("ci/upload-artifact needs `with.path`".into())
                })?;

                // Read out of the guest through exec and base64, the same way
                // the source went in — the daemon's file routes address a
                // host-side mount, not the VM.
                let workdir = plan
                    .vm
                    .working_directory
                    .as_deref()
                    .unwrap_or(DEFAULT_WORKDIR);
                // Three details, each learned the hard way against a real guest:
                //
                // - `base64` without `-w0`, because `-w` is GNU-only and a
                //   busybox image would reject it. The wrapping is stripped
                //   here instead.
                // - **A trailing newline is mandatory.** The firecracker serial
                //   path frames a command's output with newline-delimited
                //   markers, so output that ends mid-line never matches the end
                //   marker and the operation hangs in `running` forever. `-w0`
                //   emits exactly one unterminated line, which is the worst
                //   possible case.
                // - `tar` from the working directory, so the archive holds
                //   `dist/...` rather than an absolute path.
                let script = format!(
                    "cd {} && tar -czf - {} | base64; echo",
                    shell_quote(workdir),
                    shell_quote(&path)
                );
                let out = vm
                    .exec(
                        &format!("{sid}.a"),
                        &script,
                        &HashMap::new(),
                        Duration::from_secs(600),
                    )
                    .await?;
                if !out.succeeded() {
                    return Err(DispatchError::Artifact(format!(
                        "collecting {path:?} exited {}: {}",
                        out.exit_code,
                        out.combined().trim()
                    )));
                }
                use base64::Engine;
                let encoded: String = out
                    .combined()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded.as_bytes())
                    .map_err(|e| {
                        DispatchError::Artifact(format!("the guest returned unreadable data: {e}"))
                    })?;

                let run = self.store.get_run(&msg.run_id).await?;
                let aref = crate::artifacts::ArtifactRef {
                    run_id: msg.run_id.clone(),
                    job_key: plan.key.clone(),
                    workflow_id: run.map(|r| r.workflow_id).unwrap_or_default(),
                    name: name.clone(),
                };
                let stored = self
                    .artifacts
                    .put(&aref, bytes)
                    .await
                    .map_err(|e| DispatchError::Artifact(e.to_string()))?;

                self.store
                    .record_artifact(&msg.run_id, &msg.job_id, &name, &stored)
                    .await?;
                Ok(format!(
                    "[ci] stored artifact {name:?} ({} bytes) in the {} sink as {}\n",
                    stored.size_bytes, stored.sink, stored.uri
                ))
            }
            other => Err(DispatchError::Artifact(format!(
                "`uses: {other}` is not a built-in action. Available: \
                 ci/upload-artifact. Composite actions from a repository are not \
                 supported."
            ))),
        }
    }

    /// The environment a step runs with: workflow, then job, then step, each
    /// overriding the last, plus the `CI_*` and `GITHUB_*` names a build expects.
    fn step_env(&self, plan: &JobPlan, step: &Step, ctx: &Context) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = HashMap::new();
        for (k, v) in &self.config_env(plan) {
            env.insert(k.clone(), v.clone());
        }
        for (k, v) in &plan.env {
            env.insert(k.clone(), ctx.substitute(v));
        }
        for (k, v) in &step.env {
            env.insert(k.clone(), ctx.substitute(v));
        }
        env
    }

    fn config_env(&self, plan: &JobPlan) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("CI".to_string(), "true".to_string());
        env.insert("CI_JOB".to_string(), plan.base_id.clone());
        env.insert("CI_JOB_KEY".to_string(), plan.key.clone());
        env
    }
}

/// Give a second run of the same submission its own workspace.
///
/// The archive is hard-linked rather than copied — it is the same immutable
/// bytes, and a large tree copied once per workflow file would be pure waste.
/// The extracted tree is re-extracted from it, because two runs must not share a
/// directory that a step could write into.
async fn copy_tree(
    from: &crate::trigger::Workspace,
    to: &crate::trigger::Workspace,
) -> Result<(), DispatchError> {
    let (format, path) = from
        .stored_source()
        .ok_or_else(|| DispatchError::Checkout("the first run's source is gone".into()))?;
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| DispatchError::Checkout(e.to_string()))?;
    let source = crate::trigger::SourceArchive {
        format: format.as_str().to_string(),
        content_base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
    };
    crate::trigger::materialize(&source, to, usize::MAX)?;
    Ok(())
}

/// The pull loop for one route.
///
/// One task per route rather than one shared task: a slow build on one runner
/// must not hold up another runner's queue, and JetStream's per-consumer
/// `num_pending` is only a useful backlog number if each consumer serves one
/// host.
async fn consume(dispatcher: Arc<Dispatcher>, route: Route) {
    use futures::StreamExt;

    let label = format!("{route:?}");
    loop {
        let consumer = match dispatcher.bus.consumer_for(&route).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("{label}: could not bind a consumer, retrying: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let mut messages = match consumer.messages().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("{label}: could not stream messages, retrying: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        while let Some(next) = messages.next().await {
            let msg = match next {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("{label}: message error, rebinding: {e}");
                    break;
                }
            };
            let attempt = msg.info().map(|i| i.delivered as i32).unwrap_or(1);
            let Ok(job) = serde_json::from_slice::<JobMessage>(&msg.payload) else {
                // Undecodable: acking is right. Redelivering it forever would
                // block the queue on a message nothing can ever process.
                tracing::error!("{label}: undecodable job message, dropping");
                let _ = msg.ack().await;
                continue;
            };

            // Tell JetStream this job is still being worked on, for as long as
            // it is. `ack_wait` is deliberately short so a dispatcher that dies
            // releases its job in about a minute; this is what stops that same
            // short window from redelivering a *healthy* long build underneath
            // itself and putting two dispatchers on one VM.
            let msg = Arc::new(msg);
            let heartbeat = {
                let msg = Arc::clone(&msg);
                let job_key = job.job_key.clone();
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(crate::bus::ACK_PROGRESS_EVERY);
                    // The first tick is immediate and would be a no-op ack a
                    // moment after delivery.
                    ticker.tick().await;
                    loop {
                        ticker.tick().await;
                        if let Err(e) = msg.ack_with(AckKind::Progress).await {
                            // Logged, not fatal: one missed heartbeat still
                            // leaves most of the window, and the next tick may
                            // well land.
                            tracing::warn!(job = %job_key, "could not extend the ack window: {e}");
                        }
                    }
                })
            };

            // `CI_MAX_JOB_SECONDS` is enforced here, and only here. It used to
            // reach JetStream as `ack_wait` and nothing else, so once the ack
            // window stopped being derived from it the setting would have become
            // decorative — a documented ceiling on a job that bounded nothing.
            //
            // A job cut off this way leaves its VM claimed, because `run_job`
            // never reaches its own release. The lease reclaims it once this
            // dispatcher stops renewing, which is exactly the case leases exist
            // for.
            let ceiling = dispatcher.config.max_job_duration;
            let outcome =
                match tokio::time::timeout(ceiling, dispatcher.run_job(&job, attempt)).await {
                    Ok(outcome) => outcome,
                    Err(_) => Err(DispatchError::JobTimeout {
                        job: job.job_key.clone(),
                        after: ceiling,
                    }),
                };
            // Before the ack, always — including on the error paths below, which
            // is why it is aborted here rather than in each arm.
            heartbeat.abort();

            match outcome {
                Ok(status) => {
                    tracing::info!(job = %job.job_key, "finished: {}", status.as_str());
                    let _ = msg.ack().await;
                }
                Err(e) => {
                    // Retryable up to `MAX_DELIVER`. Past that JetStream stops
                    // redelivering, so the job is marked failed here rather than
                    // left `running` forever with nothing coming back to it.
                    tracing::warn!(job = %job.job_key, attempt, "failed: {e}");
                    if attempt >= crate::bus::MAX_DELIVER as i32 {
                        let _ = dispatcher
                            .store
                            .set_job_status(
                                &job.job_id,
                                JobStatus::Failure,
                                Some(&format!("giving up after {attempt} attempts: {e}")),
                            )
                            .await;
                        let _ = msg.ack().await;
                    } else {
                        // Negative-ack with the ladder's delay rather than
                        // waiting out `ack_wait`, which is job-length.
                        let delay = crate::bus::backoff_for(attempt as u32);
                        // Written on *every* attempt, not only the last. The
                        // ladder is 60s, 5 minutes, then 15, so a job that can
                        // never work — an image the host does not have is the
                        // usual one — used to show an empty error for twenty
                        // minutes before the fourth delivery finally recorded
                        // the reason. Saying it now, with what happens next, is
                        // the difference between a page that explains the wait
                        // and one that looks like nothing is happening.
                        let detail = format!(
                            "attempt {attempt} of {} failed: {e}. Retrying in {}s.",
                            crate::bus::MAX_DELIVER,
                            delay.as_secs()
                        );
                        let _ = dispatcher.store.note_job_error(&job.job_id, &detail).await;
                        dispatcher
                            .bus
                            .publish_event(
                                &job.run_id,
                                &job.job_key,
                                &serde_json::json!({"status": "running", "error": detail}),
                            )
                            .await;
                        let _ = msg
                            .ack_with(async_nats::jetstream::AckKind::Nak(Some(delay)))
                            .await;
                    }
                }
            }

            // Whatever happened, the run may now have newly-ready jobs — or be
            // finished. Advancing here is what turns a DAG into a sequence.
            if let Err(e) = dispatcher.advance_run(&job.run_id).await {
                tracing::warn!(run = %job.run_id, "could not advance: {e}");
            }
        }
    }
}

impl Dispatcher {
    /// Keep one consumer task per online runner, plus one per served network's
    /// unpinned queue.
    ///
    /// Reconciled on a ticker because the runner set changes underneath us: a
    /// host joins a network, or comes back after a reboot, and its queue needs
    /// an owner without restarting the process. A network added to the account —
    /// or brought into `CI_NETWORK=*`'s scope — is picked up the same way.
    pub fn spawn_consumers(self: Arc<Self>) {
        let interval = self.config.heyvm.refresh_interval;
        tokio::spawn(async move {
            let mut running: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                ticker.tick().await;
                let pool = self.runners.snapshot();

                let mut wanted: Vec<Route> = Vec::new();
                for set in pool.served() {
                    wanted.extend(set.dispatchable().map(|r| Route::Runner(r.id.clone())));
                    if !set.network_id.is_empty() {
                        wanted.push(Route::Network(set.network_id.clone()));
                    }
                }
                // A host may be a member of two networks, which is legitimate —
                // but two consumers on one runner subject would fight over the
                // same messages.
                wanted.sort_by_key(|r| format!("{r:?}"));
                wanted.dedup_by_key(|r| format!("{r:?}"));

                for route in wanted {
                    let key = format!("{route:?}");
                    // A finished task means the loop returned, which it only
                    // does on a panic; respawn rather than leaving a queue with
                    // no consumer.
                    let alive = running.get(&key).is_some_and(|h| !h.is_finished());
                    if alive {
                        continue;
                    }
                    tracing::info!("starting a consumer for {key}");
                    let d = self.clone();
                    let r = route.clone();
                    running.insert(key, tokio::spawn(consume(d, r)));
                }
            }
        });
    }

    /// This instance's claim on a VM: who, and for how long without renewal.
    fn lease(&self) -> crate::pool::Lease<'_> {
        crate::pool::Lease {
            instance: &self.config.instance_id,
            ttl: self.config.vm_lease,
        }
    }

    /// Re-run the scheduler for runs that still have something pending.
    ///
    /// `advance_run` is otherwise driven only by a submit and by jobs finishing,
    /// so a run whose jobs all failed to publish has nothing left to nudge it —
    /// the rollback above would return them to `pending` and there they would
    /// stay. Idempotent by construction: `queue_job` only moves a job that is
    /// still `pending`, and a job waiting on `needs:` is simply not ready yet.
    async fn nudge_stalled_runs(&self) {
        const BATCH: i64 = 100;
        let runs = match self.store.runs_with_pending_jobs(BATCH).await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::warn!("could not look for stalled runs: {e}");
                return;
            }
        };
        for run_id in runs {
            if let Err(e) = self.advance_run(&run_id).await {
                tracing::warn!(run = %run_id, "could not advance a stalled run: {e}");
            }
        }
    }

    /// Fail jobs that have waited longer than `CI_RUNNER_WAIT_SECS` for a
    /// runner that was never going to take them.
    ///
    /// This is the behaviour the README has always described and nothing
    /// implemented: a job is pinned to its host's subject even when that host is
    /// offline — deliberately, since the warm pool is host-local and migrating
    /// discards the cache the pin asked for — but consumers are bound only for
    /// hosts that are *online*. A job pinned to one that is not therefore went
    /// to a subject nothing reads, and waited for ever with no steps and no
    /// error. `CI_RUNNER_WAIT_SECS` was dead configuration describing it.
    ///
    /// Failing is better than waiting silently: the job says which host it was
    /// waiting for, and the run stops being "running" for ever.
    ///
    /// **`queued` is the signal, and it only became a reliable one when
    /// [`crate::store::Store::claim_job`] did.** A job used to stay `queued`
    /// through the whole of VM acquisition, so this could not tell a job nobody
    /// had taken from one a consumer was several minutes into booting a machine
    /// for — and since `runner_hd_id` was also unset until then, the online
    /// check below could not save it either. It would fail a live build and
    /// blame a host that was up and working on it. A picked-up job now moves to
    /// `running` immediately, which leaves `queued` meaning exactly what this
    /// reaper needs it to: on a subject, unclaimed. A dispatcher that dies after
    /// claiming one is recovered by JetStream redelivery instead, which is the
    /// mechanism for that case and does not need a timer.
    async fn fail_jobs_waiting_for_a_runner(&self) {
        // Bounded per pass for the same reason the log sweep is: a backlog
        // built up during an outage should not become one enormous burst.
        const BATCH: i64 = 100;
        let wait = self.config.heyvm.runner_wait;
        let stuck = match self.store.jobs_waiting_longer_than(wait, BATCH).await {
            Ok(stuck) => stuck,
            Err(e) => {
                tracing::warn!("could not look for jobs waiting on a runner: {e}");
                return;
            }
        };

        let pool = self.runners.snapshot();
        for job in stuck {
            // Re-checked rather than assumed: a host that came online between
            // the query and now will have taken the job, and failing it here
            // would kill work that is about to start.
            let online = job
                .runner_hd_id
                .as_deref()
                .and_then(|id| pool.locate(id))
                .is_some_and(|(_, r)| r.status.is_dispatchable());
            if online {
                continue;
            }

            let where_to = job
                .runner_hd_id
                .clone()
                .or_else(|| job.network.clone())
                .unwrap_or_else(|| "its network".to_string());
            let detail = format!(
                "no runner took this job within {}s. It is pinned to {where_to}, which \
                 is not online — a pinned job waits for its own host rather than \
                 migrating, because the warm VM pool is host-local. Bring that host \
                 back, set `fallback: any` on the job, or point `uses:` elsewhere.",
                wait.as_secs()
            );
            tracing::warn!(job = %job.job_key, "{detail}");
            if let Err(e) = self
                .store
                .set_job_status(&job.id, JobStatus::Failure, Some(&detail))
                .await
            {
                tracing::warn!(job = %job.job_key, "could not fail a stuck job: {e}");
                continue;
            }
            if let Err(e) = self.advance_run(&job.run_id).await {
                tracing::warn!(run = %job.run_id, "could not advance after failing: {e}");
            }
        }
    }

    /// Every pooled VM on the runners this instance serves.
    pub async fn vm_inventory(&self) -> Result<Vec<crate::pool::PooledVmView>, DispatchError> {
        let ours = self.served_runner_ids();
        Ok(self.pool.inventory(&ours).await?)
    }

    /// Every VM image this instance has built on the runners it serves.
    pub async fn image_inventory(&self) -> Result<Vec<crate::image::CatalogEntry>, DispatchError> {
        let ours = self.served_runner_ids();
        Ok(self.images.inventory(&ours).await?)
    }

    /// The hosts this instance may act on. Scoping every pool operation to them
    /// is what keeps two orchestrators from destroying each other's machines.
    fn served_runner_ids(&self) -> Vec<String> {
        self.runners
            .snapshot()
            .all_runners()
            .map(|r| r.id.clone())
            .collect()
    }

    /// Destroy VMs that have been taken out of circulation, and forget them.
    ///
    /// The row goes only once the daemon confirms — a row removed while the
    /// sandbox survives is a VM nothing will ever clean up again. A failure
    /// leaves it `draining`, which keeps it out of the pool and visible on the
    /// page rather than silently back in rotation.
    async fn destroy_swept(&self, taken: Vec<crate::pool::PooledVm>) -> (usize, Vec<String>) {
        let mut destroyed = 0;
        let mut failed = Vec::new();
        for vm in taken {
            let result = async {
                let options = self.runners.options_for(&vm.runner_hd_id).await?;
                let handle = self.vms.open(options, vm.sandbox_id.clone()).await?;
                handle.destroy().await?;
                Ok::<_, DispatchError>(())
            }
            .await;

            match result {
                Ok(()) => {
                    if let Err(e) = self.pool.forget(&vm.sandbox_id).await {
                        tracing::warn!(vm = %vm.sandbox_id, "destroyed but not forgotten: {e}");
                    }
                    destroyed += 1;
                }
                Err(e) => {
                    tracing::warn!(vm = %vm.sandbox_id, "could not destroy: {e}");
                    failed.push(format!("{}: {e}", vm.sandbox_id));
                }
            }
        }
        (destroyed, failed)
    }

    /// Destroy one pooled VM by id.
    pub async fn destroy_pooled_vm(&self, sandbox_id: &str) -> Result<String, DispatchError> {
        let ours = self.served_runner_ids();
        let Some(taken) = self.pool.take_one_for_sweep(sandbox_id, &ours).await? else {
            return Err(DispatchError::VmNotSweepable(sandbox_id.to_string()));
        };
        let (destroyed, failed) = self.destroy_swept(vec![taken]).await;
        if destroyed == 1 {
            Ok(format!("{sandbox_id} is destroyed and out of the pool."))
        } else {
            Err(DispatchError::Artifact(failed.join("; ")))
        }
    }

    /// Destroy every idle VM whose last run failed.
    pub async fn destroy_failed_vms(&self) -> Result<String, DispatchError> {
        let ours = self.served_runner_ids();
        let taken = self.pool.take_failed_for_sweep(&ours).await?;
        if taken.is_empty() {
            return Ok("No idle VM is left over from a failed run.".to_string());
        }
        let wanted = taken.len();
        let (destroyed, failed) = self.destroy_swept(taken).await;
        if failed.is_empty() {
            Ok(format!("Destroyed {destroyed} VM(s) left by failed runs."))
        } else {
            Ok(format!(
                "Destroyed {destroyed} of {wanted}. Still draining, and shown below: {}",
                failed.join("; ")
            ))
        }
    }

    /// Hold this instance's leases, and reclaim VMs whose holder stopped.
    ///
    /// Both halves on one timer because they are two views of the same fact.
    /// Renewing says "still here"; reclaiming acts on somebody else having
    /// stopped saying it.
    ///
    /// Periodic rather than startup-only, which is the second half of the fix: a
    /// sibling that dies is reclaimed within a lease period instead of leaking
    /// until somebody happens to restart this process.
    pub fn spawn_lease_loop(self: Arc<Self>) {
        // Comfortably inside the lease, so a slow database or a paused process
        // gets several chances before its VMs are taken. Losing a lease that is
        // still in use would put two instances on one VM, which is much worse
        // than reclaiming a minute late.
        let every = self.config.vm_lease / 3;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every.max(Duration::from_secs(5)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(e) = self.pool.renew_leases(self.lease()).await {
                    // Not fatal, and not worth giving up a VM over: the lease
                    // has time left, and the next tick may well succeed.
                    tracing::warn!("could not renew VM leases: {e}");
                }
                self.renew_vm_ttls().await;
                self.fail_jobs_waiting_for_a_runner().await;
                self.nudge_stalled_runs().await;
                if let Err(e) = self.reclaim_pool().await {
                    tracing::warn!("could not reclaim expired VM leases: {e}");
                }
            }
        });
    }

    /// Push out the sandbox TTL of every VM this instance is running a job on.
    ///
    /// **A job may outlive its VM.** `CI_VM_TTL_SECONDS` defaults to an hour and
    /// `CI_MAX_JOB_SECONDS` to four, and the TTL was only ever set at creation
    /// and renewed when a VM was claimed or released — so a build longer than
    /// the TTL had its machine reaped mid-step, surfacing as a daemon error on a
    /// job that was doing nothing wrong.
    ///
    /// Safe to run while a step is executing because [`Vm::renew_ttl`] does not
    /// take the sandbox lock, unlike `exec` and `destroy`. If it did, this would
    /// queue behind the very build it is trying to keep alive.
    ///
    /// Only claimed VMs. An idle one in the warm pool is meant to age out — that
    /// is what the TTL is a backstop for — and renewing those would mean nothing
    /// this app creates ever expires.
    async fn renew_vm_ttls(&self) {
        let held = match self.pool.leased_by(&self.config.instance_id).await {
            Ok(held) => held,
            Err(e) => {
                tracing::warn!("could not list held VMs to renew: {e}");
                return;
            }
        };
        if held.is_empty() {
            return;
        }

        let ttl = self.config.heyvm.vm_ttl;
        let renewals = held.iter().map(|(sandbox_id, runner)| async move {
            // Opened per pass rather than cached: the tunnel underneath is
            // cached by `Runners`, and a handle is a cheap wrapper over it.
            let options = self.runners.options_for(runner).await?;
            let vm = self.vms.open(options, sandbox_id.clone()).await?;
            vm.renew_ttl(ttl).await?;
            Ok::<_, DispatchError>(())
        });

        // Concurrent and bounded. One unreachable daemon must not hold up the
        // renewals of every other VM, nor stall the loop that also renews the
        // database leases — losing those would hand this instance's VMs away
        // while it is still using them.
        let batch = futures::future::join_all(renewals);
        let results = match tokio::time::timeout(self.config.vm_lease / 6, batch).await {
            Ok(results) => results,
            Err(_) => {
                tracing::warn!(
                    "renewing {} VM TTL(s) timed out; some may be reaped if this persists",
                    held.len()
                );
                return;
            }
        };
        for ((sandbox_id, _), result) in held.iter().zip(results) {
            if let Err(e) = result {
                // Not fatal and not a reason to discard the row: a VM that is
                // genuinely gone is caught by `acquire_vm`, which already
                // forgets an unusable pooled VM and builds a fresh one.
                tracing::warn!(vm = %sandbox_id, "could not renew the TTL: {e}");
            }
        }
    }

    /// Reclaim VMs whose lease has run out — a previous life of this process, or
    /// a sibling that died.
    pub async fn reclaim_pool(&self) -> Result<(), DispatchError> {
        let pool = self.runners.snapshot();
        let ours: Vec<String> = pool.all_runners().map(|r| r.id.clone()).collect();
        let released = self
            .pool
            .release_orphans(&ours, &self.config.instance_id)
            .await?;
        if released > 0 {
            tracing::info!("released {released} VM(s) held by jobs that are no longer running");
        }
        // A `building` row is an attempt in flight, and the process that made it
        // is the only thing that clears it. One that died holding some would
        // otherwise leave VMs "building" on /vms for ever — on a page whose
        // whole purpose is to say what is happening now.
        let swept = self
            .pool
            .sweep_stale_builds(&ours, &self.config.instance_id)
            .await?;
        if swept > 0 {
            tracing::info!("cleared {swept} abandoned VM creation(s)");
        }
        Ok(())
    }
}

/// Wrap a step's script so its exit code survives and its declared outputs come
/// back in the same exec.
///
/// GitHub gives a step a `$GITHUB_OUTPUT` file to append `name=value` lines to.
/// Reading it would normally be a second exec — but a second exec is a second
/// round trip over an iroh tunnel per step, and the daemon serializes execs per
/// sandbox anyway. Instead the file is printed after the command behind a marker
/// that is unique per step, and split back out of the combined stream.
///
/// The marker embeds the step id, so a build that happens to print the word
/// `CI_OUTPUT` cannot forge one.
fn wrap_command(script: &str, step: &Step, step_id: &str, default_wd: &str) -> String {
    let marker = output_marker(step_id);
    // A step with no `working-directory:` runs where the source was extracted,
    // not wherever the guest's shell happens to start. Without this a `run:` of
    // `cargo build` works only by luck of the image's default directory.
    let wd = step.working_directory.as_deref().unwrap_or(default_wd);
    let cd = format!("cd {} && ", shell_quote(wd));
    // `__ci_rc` is captured before anything else runs, so the step's own exit
    // code is what the job sees rather than `cat`'s.
    format!(
        "export CI_OUTPUT=\"${{CI_OUTPUT:-/tmp/ci-output-{step_id}}}\"; \
         : > \"$CI_OUTPUT\"; \
         {cd}{{ {script}
}}; __ci_rc=$?; \
         printf '\\n%s\\n' '{marker}'; cat \"$CI_OUTPUT\" 2>/dev/null; \
         exit $__ci_rc"
    )
}

fn output_marker(step_id: &str) -> String {
    format!("::ci-output::{step_id}::")
}

/// Split the combined stream into the log text and the step's declared outputs.
fn split_outputs(out: &ExecOutput, step_id: &str) -> (String, Value) {
    let combined = out.combined();
    let marker = output_marker(step_id);
    let Some(pos) = combined.rfind(&marker) else {
        return (combined, Value::Object(Default::default()));
    };
    let (before, after) = combined.split_at(pos);
    let tail = &after[marker.len()..];
    let mut map = serde_json::Map::new();
    for line in tail.lines() {
        if let Some((k, v)) = line.split_once('=')
            && !k.trim().is_empty()
        {
            map.insert(k.trim().to_string(), Value::String(v.to_string()));
        }
    }
    (before.trim_end().to_string(), Value::Object(map))
}

/// Single-quote a value for `sh`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A comma-separated list, or a phrase saying there is nothing to list.
///
/// "Currently serving: " followed by nothing reads as a truncated message, and
/// an empty served set is exactly the state someone needs told plainly.
fn or_none(items: &[String]) -> String {
    if items.is_empty() {
        "nothing — no network in CI_NETWORK resolved".to_string()
    } else {
        items.join(", ")
    }
}

#[derive(Debug)]
pub enum DispatchError {
    Store(crate::store::StoreError),
    Pool(crate::pool::PoolError),
    Bus(crate::bus::BusError),
    Runner(crate::runners::RunnerError),
    Vm(VmError),
    /// Building a runner's VM image from a Dockerfile.
    Image(crate::image::ImageError),
    BadPlan(String),
    Condition(String),
    UnknownJob(String),
    UnknownRunner {
        wanted: String,
        network: String,
    },
    RunnerOffline {
        runner: String,
        status: &'static str,
    },
    NoOnlineRunner(String),
    NoNetwork,
    /// `uses: default` with no resolvable local daemon.
    NoDefaultNode,
    /// The local daemon is known but is in no network this instance serves.
    DefaultNodeUnserved {
        node: String,
        served: Vec<String>,
    },
    /// The run was cancelled while this job was running.
    Cancelled(String),
    /// A VM cannot be swept: unknown, on another instance's host, or claimed.
    VmNotSweepable(String),
    /// A job ran past `CI_MAX_JOB_SECONDS`.
    JobTimeout {
        job: String,
        after: Duration,
    },
    /// `uses:` named a VM that the pinned node does not have.
    UnknownVm {
        wanted: String,
        node: String,
        available: Vec<String>,
    },
    /// The network exists on the account but this instance does not serve it.
    UnservedNetwork {
        wanted: String,
        served: Vec<String>,
    },
    /// No network on the account answers to that name.
    UnknownNetwork {
        wanted: String,
        served: Vec<String>,
    },
    StepFailed(String),
    Checkout(String),
    Secrets(String),
    Artifact(String),
    Trigger(crate::trigger::TriggerError),
    Workflow(String),
}

impl From<crate::trigger::TriggerError> for DispatchError {
    fn from(e: crate::trigger::TriggerError) -> Self {
        Self::Trigger(e)
    }
}

macro_rules! from_err {
    ($t:ty, $v:ident) => {
        impl From<$t> for DispatchError {
            fn from(e: $t) -> Self {
                Self::$v(e)
            }
        }
    };
}
from_err!(crate::store::StoreError, Store);
from_err!(crate::pool::PoolError, Pool);
from_err!(crate::bus::BusError, Bus);
from_err!(crate::runners::RunnerError, Runner);
from_err!(VmError, Vm);
from_err!(crate::image::ImageError, Image);

impl DispatchError {
    /// The iroh tunnel to the runner, not the work: a transport-level failure
    /// to reach the daemon, never anything the daemon answered. The holder of
    /// the runner id should evict its cached tunnel on this — a NAK'd retry
    /// that redials can succeed, one that reuses the dead local port cannot.
    pub fn is_tunnel_failure(&self) -> bool {
        match self {
            Self::Vm(e) => e.is_transport(),
            Self::Image(e) => e.is_transport(),
            _ => false,
        }
    }

    /// The guest's own filesystem died underneath the job: its command output
    /// names an ext4 error no retry on the same VM can survive. The holder of
    /// the VM should destroy it on this rather than repool it.
    ///
    /// Only the plumbing variants are scanned — checkout and the VM transport,
    /// whose messages embed the output of commands *this app* ran (`mkdir`,
    /// `tar`, the chunked upload). A step failure carries its exit code and
    /// nothing else, so a build that merely *prints* one of these strings can
    /// never match.
    pub fn indicates_guest_corruption(&self) -> bool {
        let text = match self {
            Self::Checkout(_) | Self::Vm(_) => self.to_string(),
            _ => return false,
        };
        [
            // EBADMSG: ext4 metadata failed its checksum.
            "Bad message",
            // EUCLEAN: the filesystem is asking for fsck.
            "Structure needs cleaning",
            // EROFS: ext4 already hit an error and remounted itself read-only.
            "Read-only file system",
            // EIO: the virtio device refused the read or write outright.
            "Input/output error",
            // An upload whose end-to-end hash check failed: every chunk exec
            // exited 0 and the assembled file still holds different bytes.
            // See `VmError::UploadCorrupted` — the guest acknowledged writes
            // it did not keep, which no ext4 errno ever surfaces.
            "sha256 mismatch",
        ]
        .iter()
        .any(|marker| text.contains(marker))
    }
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Pool(e) => write!(f, "{e}"),
            Self::Bus(e) => write!(f, "{e}"),
            Self::Runner(e) => write!(f, "{e}"),
            Self::Vm(e) => write!(f, "{e}"),
            Self::Image(e) => write!(f, "{e}"),
            Self::BadPlan(e) => write!(f, "the stored plan could not be read: {e}"),
            Self::Condition(e) => write!(f, "an `if:` condition could not be evaluated: {e}"),
            Self::UnknownJob(id) => write!(f, "no job {id} exists"),
            Self::UnknownRunner { wanted, network } => write!(
                f,
                "no runner {wanted:?} is a host member of network {network:?}. Add it \
                 with `heyvm network add-host`, or set `fallback: any` on the job."
            ),
            Self::RunnerOffline { runner, status } => write!(
                f,
                "runner {runner:?} is {status}. The job stays queued for that host \
                 because moving it would discard the warm VM the pin asked for; set \
                 `fallback: any` to allow migrating."
            ),
            Self::NoOnlineRunner(net) => {
                write!(f, "no host in network {net:?} is online to take this job")
            }
            Self::NoNetwork => write!(
                f,
                "the runner pool has not resolved a network yet; check CI_NETWORK \
                 and the heyvm control plane"
            ),
            Self::NoDefaultNode => write!(
                f,
                "`uses: default` means the host this orchestrator runs on, and that \
                 host could not be identified. Set CI_DEFAULT_NODE to its daemon id \
                 or name — heyvmd reports its own id only when BACKEND_SERVER_ID is \
                 set in its environment, so it is often not discoverable."
            ),
            Self::Cancelled(job) => write!(
                f,
                "job {job:?} was cancelled. The step that was already running was \
                 allowed to finish — the daemon has no way to abort one — and \
                 nothing after it ran."
            ),
            Self::VmNotSweepable(id) => write!(
                f,
                "{id} cannot be destroyed from here. It is either unknown, on a host \
                 this orchestrator does not serve, or currently running a job — a \
                 claimed VM is left alone so cleaning up cannot fail a live build."
            ),
            Self::JobTimeout { job, after } => write!(
                f,
                "job {job:?} ran past CI_MAX_JOB_SECONDS ({}s) and was cut off. Its VM \
                 is reclaimed once this dispatcher's lease on it lapses.",
                after.as_secs()
            ),
            Self::UnknownVm {
                wanted,
                node,
                available,
            } => write!(
                f,
                "no sandbox {wanted:?} exists on node {node:?}. `uses: \
                 <network>/<node>/<vm>` runs in a VM that is already there — it \
                 does not create one. On that node: {}",
                or_none(available)
            ),
            Self::DefaultNodeUnserved { node, served } => write!(
                f,
                "`uses: default` resolved to daemon {node:?}, but that host is in no \
                 network this orchestrator serves. Join it to one with \
                 `heyvm network add-host`. Currently serving: {}",
                or_none(served)
            ),
            Self::UnservedNetwork { wanted, served } => write!(
                f,
                "network {wanted:?} exists, but this orchestrator does not take work \
                 for it. Add it to CI_NETWORK (or set CI_NETWORK=*). Currently \
                 serving: {}",
                or_none(served)
            ),
            Self::UnknownNetwork { wanted, served } => write!(
                f,
                "no heyvm network is named {wanted:?}. Check the job's `uses:` or the \
                 repository's assigned network on /repos. Currently serving: {}",
                or_none(served)
            ),
            Self::StepFailed(r) => write!(f, "{r}"),
            Self::Checkout(r) => write!(f, "checkout failed: {r}"),
            Self::Secrets(r) => write!(f, "{r}"),
            Self::Artifact(r) => write!(f, "{r}"),
            Self::Trigger(e) => write!(f, "{e}"),
            Self::Workflow(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DispatchError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::Step;
    use std::collections::BTreeMap;

    fn step(run: &str) -> Step {
        Step {
            name: None,
            id: None,
            condition: None,
            uses: None,
            with: BTreeMap::new(),
            run: Some(run.to_string()),
            shell: None,
            working_directory: None,
            env: BTreeMap::new(),
            timeout_minutes: None,
            continue_on_error: false,
        }
    }

    fn output(combined: &str, exit: i32) -> ExecOutput {
        ExecOutput {
            output: combined.to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: exit,
        }
    }

    // ---- network resolution ---------------------------------------------

    fn test_pool() -> crate::runners::Pool {
        use crate::runners::{Runner, RunnerSet, RunnerStatus};
        let host = |id: &str| Runner {
            id: id.into(),
            name: id.into(),
            status: RunnerStatus::Online,
            last_seen_at: None,
        };
        crate::runners::Pool {
            networks: vec![
                RunnerSet {
                    network_id: "net-1".into(),
                    network_name: "prod-runners".into(),
                    is_default: true,
                    served: true,
                    runners: vec![host("hd-1")],
                },
                RunnerSet {
                    network_id: "net-2".into(),
                    network_name: "lab".into(),
                    is_default: false,
                    served: false,
                    runners: vec![host("hd-2")],
                },
            ],
            unjoined: vec![],
            last_error: None,
            default_network_id: "net-1".into(),
            default_node_id: "hd-1".into(),
        }
    }

    /// A real one-job plan, built from a workflow rather than hand-assembled,
    /// so what is asserted about `uses:` is what `uses:` actually produces.
    fn plan_targeting(network: Option<&str>) -> JobPlan {
        let uses = match network {
            Some(n) => format!("    uses: \"{n}\"\n"),
            None => String::new(),
        };
        let yaml = format!(
            "name: t\njobs:\n  build:\n{uses}    vm: {{ driver: firecracker }}\n    \
             steps: [{{ run: \"true\" }}]\n"
        );
        let wf = crate::workflow::Workflow::parse("t.yml", &yaml).expect("workflow parses");
        crate::plan::Plan::build(&wf)
            .expect("plan builds")
            .jobs
            .remove(0)
    }

    /// A job with no network runs in the default one — which is what makes a
    /// workflow that says nothing about hardware still land somewhere chosen.
    #[test]
    fn a_job_naming_no_network_lands_in_the_default() {
        let pool = test_pool();
        let set = Dispatcher::network_of(&pool, &plan_targeting(None)).expect("resolves");
        assert_eq!(set.network_id, "net-1");
    }

    /// Either spelling, because `uses:` and a repository assignment are both
    /// written by hand.
    #[test]
    fn a_job_naming_a_served_network_by_id_or_name_resolves_to_it() {
        let pool = test_pool();
        for spelling in ["prod-runners", "net-1", "PROD-Runners", " prod-runners "] {
            let set = Dispatcher::network_of(&pool, &plan_targeting(Some(spelling)))
                .unwrap_or_else(|e| panic!("{spelling:?}: {e}"));
            assert_eq!(set.network_id, "net-1");
        }
    }

    /// The two failures a person actually hits, told apart — one is a
    /// `CI_NETWORK` change and the other is a typo, and the same message for
    /// both sends them to the wrong file.
    #[test]
    fn an_unserved_network_and_an_unknown_one_are_different_errors() {
        let pool = test_pool();

        let err = Dispatcher::network_of(&pool, &plan_targeting(Some("lab"))).unwrap_err();
        assert!(
            matches!(err, DispatchError::UnservedNetwork { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("CI_NETWORK"), "{err}");
        assert!(
            err.to_string().contains("prod-runners"),
            "names what is served: {err}"
        );

        let err = Dispatcher::network_of(&pool, &plan_targeting(Some("nope"))).unwrap_err();
        assert!(
            matches!(err, DispatchError::UnknownNetwork { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("nope"), "{err}");
    }

    /// `uses: default` resolves to the orchestrator's own host, and pins — the
    /// whole point is "this machine", so it must not land on the network's
    /// shared queue.
    #[test]
    fn default_places_the_job_on_this_orchestrators_host() {
        let pool = test_pool();
        let plan = plan_targeting(Some("default"));
        assert!(
            plan.target.local,
            "the fixture must exercise the local form"
        );

        let placed = Dispatcher::place(&pool, &plan).expect("resolves");
        assert_eq!(placed.network.network_id, "net-1");
        assert_eq!(placed.node.map(|n| n.id.as_str()), Some("hd-1"));
        assert!(placed.vm.is_none());
    }

    /// The two ways `default` fails, told apart: nothing identified the host at
    /// all, versus a host that is known but in no network we serve. One is
    /// CI_DEFAULT_NODE, the other is `heyvm network add-host`.
    #[test]
    fn an_unresolvable_default_names_which_fix_applies() {
        let plan = plan_targeting(Some("default"));

        let mut pool = test_pool();
        pool.default_node_id = String::new();
        let err = Dispatcher::place(&pool, &plan).unwrap_err();
        assert!(matches!(err, DispatchError::NoDefaultNode), "{err:?}");
        assert!(err.to_string().contains("CI_DEFAULT_NODE"), "{err}");

        // Known, but its only network is one this instance does not serve.
        let mut pool = test_pool();
        pool.default_node_id = "hd-2".into();
        let err = Dispatcher::place(&pool, &plan).unwrap_err();
        assert!(
            matches!(err, DispatchError::DefaultNodeUnserved { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("heyvm network add-host"), "{err}");
    }

    /// The three-segment form: a named VM on a named host. The host pins the
    /// queue and the VM rides along for the executor.
    #[test]
    fn naming_a_vm_pins_its_host_and_carries_the_vm() {
        let pool = test_pool();
        let plan = plan_targeting(Some("prod-runners/hd-1/sb-1a34"));

        let placed = Dispatcher::place(&pool, &plan).expect("resolves");
        assert_eq!(placed.node.map(|n| n.id.as_str()), Some("hd-1"));
        assert_eq!(placed.vm, Some("sb-1a34"));
        assert!(plan.target.is_existing_vm());
    }

    /// `fallback: any` moves a job to another host when the pinned one is gone.
    /// It must not do that for a job that named a VM — the VM lives on one host,
    /// and "any host" would run the steps somewhere it does not exist.
    #[test]
    fn fallback_any_does_not_relocate_a_job_that_named_a_vm() {
        let pool = test_pool();

        let mut plan = plan_targeting(Some("prod-runners/nosuchhost"));
        plan.fallback = Fallback::Any;
        let placed = Dispatcher::place(&pool, &plan).expect("falls back");
        assert!(placed.node.is_none(), "an unpinned fallback is the network");

        let mut plan = plan_targeting(Some("prod-runners/nosuchhost/sb-1a34"));
        plan.fallback = Fallback::Any;
        let err = Dispatcher::place(&pool, &plan).unwrap_err();
        assert!(
            matches!(err, DispatchError::UnknownRunner { .. }),
            "{err:?}"
        );
    }

    /// A VM named in `uses:` is somebody else's machine. The pool never created
    /// it, so teardown must not touch it — destroying a long-lived VM because a
    /// workflow set `reuse: false` in a `vm:` block that never applied to it
    /// would be the worst kind of surprise.
    #[test]
    fn an_existing_vm_is_never_torn_down_by_the_job_that_used_it() {
        let mut plan = plan_targeting(Some("prod-runners/hd-1/sb-1a34"));
        assert!(plan.target.is_existing_vm());
        // Even with the `vm:` block asking for destruction, which is exactly the
        // configuration that would otherwise delete it.
        plan.vm.reuse = false;

        // `release_vm` returns before touching the VM or the pool. Asserted on
        // the predicate it branches on, because the call itself needs a live
        // daemon; the branch is the whole behaviour.
        assert!(
            plan.target.is_existing_vm(),
            "release_vm returns early on exactly this"
        );

        let built = plan_targeting(Some("prod-runners/hd-1"));
        assert!(
            !built.target.is_existing_vm(),
            "a job that built its own VM must still be released"
        );
    }

    /// A guest whose ext4 has died must not go back into the pool — the next
    /// attempt prefers the same idle VM and would fail identically. The
    /// classification is on the plumbing's own errors, never a step's, so a
    /// build that merely prints one of the marker strings cannot get its
    /// healthy warm VM destroyed.
    #[test]
    fn only_the_plumbing_can_declare_the_guest_filesystem_dead() {
        // The observed failure, verbatim: chunked upload during checkout.
        let corrupt = DispatchError::Vm(VmError::UploadFailed {
            sandbox: "sb-2e3c4317".into(),
            path: "/workspace/.ci-source.tar.gz".into(),
            chunk: 26,
            of: 529,
            detail: "mkdir: cannot create directory '/workspace': Bad message".into(),
        });
        assert!(corrupt.indicates_guest_corruption());

        // The same output wrapped the way `checkout` reports it.
        let checkout = DispatchError::Checkout(
            "tar: dist/index.html: Cannot open: Structure needs cleaning".into(),
        );
        assert!(checkout.indicates_guest_corruption());

        // A checkout that failed for an ordinary reason keeps its VM.
        let plain = DispatchError::Checkout("extracting the source exited 2".into());
        assert!(!plain.indicates_guest_corruption());

        // An upload whose end-to-end hash check failed: every chunk landed
        // with exit 0 and the guest still holds different bytes. Silent
        // corruption — no errno string anywhere — and the strongest possible
        // reason not to hand this VM to the next attempt.
        let silent = DispatchError::Vm(VmError::UploadCorrupted {
            sandbox: "sb-2e3c4317".into(),
            path: "/workspace/.ci-source.tar.gz".into(),
            expected: "a".repeat(64),
            actual: "b".repeat(64),
        });
        assert!(silent.indicates_guest_corruption());

        // A step failure carries only the exit code — but even if output ever
        // leaked into it, a step is the user's code and must not match.
        let step = DispatchError::StepFailed(
            "step \"Build\" exited 1: cp: cannot stat 'x': Input/output error".into(),
        );
        assert!(!step.indicates_guest_corruption());

        // A dead tunnel is the runner being unreachable, not the guest dying;
        // it has its own remedy (evict and redial) and must not destroy VMs.
        let tunnel = DispatchError::Vm(VmError::Create {
            name: "vm".into(),
            source: heyo_sdk::HeyoError::Api {
                status: 0,
                message: "network error calling /sandbox-deploy".into(),
                body: None,
            },
        });
        assert!(!tunnel.indicates_guest_corruption());
        assert!(tunnel.is_tunnel_failure(), "still classified as transport");
    }

    /// The node and the VM are one decision. Reading `target` twice is how the
    /// queue a job was routed to and the machine it runs on come to disagree.
    #[test]
    fn the_resolved_vm_travels_with_the_node_that_holds_it() {
        let pool = test_pool();

        let pinned = plan_targeting(Some("prod-runners/hd-1/sb-1a34"));
        let placed = Dispatcher::place(&pool, &pinned).expect("resolves");
        assert_eq!(placed.node.map(|n| n.id.as_str()), Some("hd-1"));
        assert_eq!(placed.vm, Some("sb-1a34"));

        // An unpinned job never carries a VM, so the exec-only branch cannot be
        // entered without a host to exec on.
        let unpinned = plan_targeting(Some("prod-runners"));
        let placed = Dispatcher::place(&pool, &unpinned).expect("resolves");
        assert!(placed.node.is_none());
        assert!(placed.vm.is_none());
    }

    /// A pool that has resolved nothing must refuse rather than pick, or a
    /// submit during a cloud outage is accepted onto a queue with no consumer.
    #[test]
    fn an_empty_pool_refuses_rather_than_guessing() {
        let pool = crate::runners::Pool::default();
        let err = Dispatcher::network_of(&pool, &plan_targeting(None)).unwrap_err();
        assert!(matches!(err, DispatchError::NoNetwork), "{err:?}");

        // And the "nothing is served" case says so in words rather than
        // trailing off after a colon.
        let err = DispatchError::UnservedNetwork {
            wanted: "lab".into(),
            served: vec![],
        };
        assert!(
            err.to_string().contains("no network in CI_NETWORK"),
            "{err}"
        );
    }

    /// The step's own exit code has to survive the trailing `cat`, or every
    /// failing step reports success.
    #[test]
    fn the_wrapper_preserves_the_scripts_exit_code() {
        let w = wrap_command("exit 3", &step("exit 3"), "s1", "/workspace");
        assert!(w.contains("__ci_rc=$?"), "{w}");
        assert!(w.trim_end().ends_with("exit $__ci_rc"), "{w}");
        // The capture must come immediately after the script block.
        let brace = w
            .find("}; __ci_rc=$?")
            .expect("captured right after the block");
        assert!(brace > 0);
    }

    #[test]
    fn a_working_directory_is_quoted_into_a_cd() {
        let mut s = step("make");
        s.working_directory = Some("/work/my project".into());
        let w = wrap_command("make", &s, "s1", "/workspace");
        assert!(w.contains("cd '/work/my project' && "), "{w}");
    }

    #[test]
    fn a_quote_in_a_working_directory_cannot_break_out() {
        let mut s = step("make");
        s.working_directory = Some("/work/'; rm -rf /; '".into());
        let w = wrap_command("make", &s, "s1", "/workspace");
        assert!(!w.contains("&& rm -rf /"), "{w}");
        assert!(w.contains(r"'\''"), "the quote is escaped: {w}");
    }

    /// A multi-line script must not have its last line swallowed by the closing
    /// brace — `{ cmd }` needs the newline or a `;` before `}`.
    #[test]
    fn a_multi_line_script_is_terminated_before_the_closing_brace() {
        let script = "echo one\necho two";
        let w = wrap_command(script, &step(script), "s1", "/workspace");
        assert!(w.contains("echo two\n}"), "{w}");
    }

    #[test]
    fn outputs_are_split_off_the_end_of_the_log() {
        let sid = "run.job.0";
        let combined = format!(
            "building\ndone\n\n{}\nversion=1.2.3\nsha=abc\n",
            output_marker(sid)
        );
        let (log, outputs) = split_outputs(&output(&combined, 0), sid);
        assert_eq!(log, "building\ndone");
        assert_eq!(outputs["version"], "1.2.3");
        assert_eq!(outputs["sha"], "abc");
    }

    /// A step that declares no outputs still logs normally.
    #[test]
    fn a_step_with_no_outputs_yields_an_empty_map() {
        let sid = "run.job.0";
        let combined = format!("building\n\n{}\n", output_marker(sid));
        let (log, outputs) = split_outputs(&output(&combined, 0), sid);
        assert_eq!(log, "building");
        assert_eq!(outputs.as_object().unwrap().len(), 0);
    }

    /// A build that prints something marker-shaped must not be able to inject
    /// outputs — the marker carries the step id, which the build does not know
    /// it needs to forge... and even if it prints one, the *last* marker wins,
    /// which is the one the wrapper emitted.
    #[test]
    fn a_forged_marker_earlier_in_the_log_does_not_win() {
        let sid = "run.job.0";
        let combined = format!(
            "sneaky\n{}\nadmin=true\nreal output\n\n{}\nversion=1\n",
            output_marker(sid),
            output_marker(sid)
        );
        let (_, outputs) = split_outputs(&output(&combined, 0), sid);
        assert_eq!(outputs["version"], "1");
        assert!(
            outputs.get("admin").is_none(),
            "only the trailing marker's block counts: {outputs}"
        );
    }

    /// A marker for a different step is not this step's marker.
    #[test]
    fn another_steps_marker_is_ignored() {
        let combined = format!("out\n{}\nx=1\n", output_marker("other.step.9"));
        let (log, outputs) = split_outputs(&output(&combined, 0), "run.job.0");
        assert!(log.contains("out"));
        assert_eq!(outputs.as_object().unwrap().len(), 0);
    }

    #[test]
    fn a_value_containing_an_equals_sign_survives() {
        let sid = "s";
        let combined = format!("{}\nurl=https://x/?a=1&b=2\n", output_marker(sid));
        let (_, outputs) = split_outputs(&output(&combined, 0), sid);
        assert_eq!(outputs["url"], "https://x/?a=1&b=2");
    }

    #[test]
    fn shell_quoting_handles_the_awkward_cases() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    // ---- end to end -----------------------------------------------------
    //
    // The whole path: a run is created, the scheduler queues its jobs, a
    // consumer pulls one, a real VM boots on the local heyvmd, the steps run,
    // and the results land in Postgres. Then a second run proves the VM is
    // reused, and a third proves a changed `cache_key_files` entry busts it.
    //
    //   CI_TEST_DATABASE_URL=postgres://… CI_TEST_NATS_URL=nats://127.0.0.1:4222 \
    //     cargo test --bin ci -- --ignored --nocapture end_to_end

    async fn test_dispatcher(workspace_root: &std::path::Path) -> Arc<Dispatcher> {
        // So `--nocapture` shows what the dispatcher actually did; RUST_LOG
        // applies as usual. try_init because a second e2e in one process is
        // not an error.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .try_init();
        let db = std::env::var("CI_TEST_DATABASE_URL").expect("CI_TEST_DATABASE_URL");
        let nats =
            std::env::var("CI_TEST_NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());
        let daemon = std::env::var("CI_TEST_DAEMON")
            .unwrap_or_else(|_| heyo_sdk::DEFAULT_LOCAL_BASE_URL.to_string());

        unsafe {
            std::env::set_var("CI_HEYO_API_KEY", "unused-in-local-mode");
            std::env::set_var("CI_NETWORK", "unused-in-local-mode");
            std::env::set_var("CI_DATABASE_URL", &db);
            std::env::set_var("CI_WEBHOOK_SECRET", "0123456789abcdef");
            std::env::set_var("CI_LOCAL_RUNNER", &daemon);
            std::env::set_var("CI_NATS_URL", &nats);
            // A distinct prefix per run, so a test never shares a stream.
            std::env::set_var(
                "CI_NATS_SUBJECT_PREFIX",
                format!("e2e{}", crate::vm::new_id().replace('-', "")),
            );
            std::env::set_var("CI_WORKSPACE_DIR", workspace_root);
        }
        let config = Arc::new(Config::from_env().expect("config"));
        let store = crate::store::Store::connect(
            &config.database_url,
            std::env::temp_dir().join(format!("ci-e2e-logs-{}", crate::vm::new_id())),
        )
        .await
        .expect("store");
        store
            .migrate(std::path::Path::new("migrations"))
            .await
            .expect("migrations");

        let runners = Arc::new(Runners::new(config.clone()));
        runners.refresh().await.expect("local runner resolves");

        let bus = Arc::new(
            Bus::connect(&config.nats, &config.nats_prefix)
                .await
                .expect("nats"),
        );

        Arc::new(Dispatcher {
            config: config.clone(),
            store: store.clone(),
            pool: Pool::new(store.pool().clone()),
            images: crate::image::Catalog::new(store.pool().clone()),
            bus,
            runners,
            vms: Arc::new(Vms::new()),
            secrets: crate::secrets::Secrets::new(&config),
            artifacts: Arc::from(crate::artifacts::sink_for(&config).expect("disk sink")),
            // Unconfigured: the e2e test drives workflows straight from the
            // submitted tree, which is the path an installation with no app-lb
            // takes anyway.
            objects: Arc::new(crate::objects::Workflows::new(&config)),
        })
    }

    /// Lay down a run's workspace *and* its source archive, the way a real
    /// submit does.
    ///
    /// Writing the extracted tree alone is not enough any more: a job's checkout
    /// step ships the archive into the guest, so a test that skips it is testing
    /// a path production does not have.
    fn seed_workspace(d: &Arc<Dispatcher>, run_id: &str, files: &[(&str, &str)]) {
        use base64::Engine;
        use std::io::Write;

        let mut ar = tar::Builder::new(Vec::new());
        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            ar.append_data(&mut header, name, content.as_bytes())
                .unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&ar.into_inner().unwrap()).unwrap();
        let gz = gz.finish().unwrap();

        std::fs::create_dir_all(&d.config.workspace_dir).unwrap();
        let ws = crate::trigger::Workspace::for_run(&d.config, run_id);
        crate::trigger::materialize(
            &crate::trigger::SourceArchive {
                format: "tar.gz".into(),
                content_base64: base64::engine::general_purpose::STANDARD.encode(&gz),
            },
            &ws,
            1 << 20,
        )
        .expect("workspace seeded");
    }

    /// Destroy whatever the pool still holds, so a test does not leave VMs on
    /// the developer's machine.
    async fn cleanup(d: &Arc<Dispatcher>) {
        let Ok(vms) = d.pool.all().await else { return };
        for v in vms.iter().filter(|v| v.runner_hd_id == "hd-local") {
            if let Ok(opts) = d.runners.options_for(&v.runner_hd_id).await
                && let Ok(vm) = d.vms.open(opts, v.sandbox_id.clone()).await
            {
                let _ = vm.destroy().await;
            }
            let _ = d.pool.forget(&v.sandbox_id).await;
        }
        let _ = d.bus.js_delete_streams().await;
    }

    const E2E_YAML: &str = r#"
name: e2e
jobs:
  build:
    vm:
      driver: firecracker
      image: debian
      size_class: micro
      cache_key_files: [lockfile.txt]
    steps:
      - name: Say hello
        id: greet
        run: |
          echo "hello from ci"
          echo "greeting=hi" >> "$CI_OUTPUT"
      - name: Use the step output
        run: echo "greeting was ${{ steps.greet.outputs.greeting }}"
      # The only coverage `ci/upload-artifact` has. It reads out of the guest
      # through exec and base64, which is a different transport from every
      # `run:` step above — the guest has to have `tar` and `base64`, the output
      # has to end with a newline or the serial path hangs forever, and the
      # bytes have to survive the round trip. None of that is exercised by a
      # workflow made only of `run:` steps, which is what this was.
      - name: Produce something to upload
        run: mkdir -p dist && echo "artifact-body" > dist/hello.txt
      - uses: ci/upload-artifact
        with:
          name: e2e-dist
          path: dist
  after:
    needs: [build]
    vm:
      driver: firecracker
      image: debian
      size_class: micro
      cache_key_files: [lockfile.txt]
    steps:
      - name: Depends on build
        run: echo "build said ${{ needs.build.result }}"
"#;

    /// A workflow that builds its own image instead of naming one the host has.
    const E2E_IMAGE_YAML: &str = r#"
name: e2e-image
jobs:
  build:
    vm:
      driver: firecracker
      build:
        dockerfile: img/Dockerfile
      size_class: micro
    steps:
      - name: Prove the image was built
        run: cat /etc/ci-marker; echo "marker=$CI_IMAGE_MARKER"
"#;

    /// The whole `vm.build` path against a real daemon: upload a Dockerfile
    /// and its context to `POST /images/build`, let the daemon run docker →
    /// export → mke2fs into its catalog, and run a job on the result — then
    /// prove the second run reuses the image rather than building it again.
    ///
    /// The Dockerfile ships its own `/init.sh`, and that is not test
    /// scaffolding: docker-built images boot `init=/init.sh` and must print
    /// `HEYVM_READY`, exactly like a hand-built one. The in-guest assertions
    /// cover what has to survive `docker export`: a COPY'd file, a RUN layer,
    /// and an environment variable — which does NOT survive as OCI `ENV` and
    /// must be written to `/etc/profile.d` by a RUN, which is exactly what the
    /// test's Dockerfile does.
    #[tokio::test]
    #[ignore = "needs Postgres, NATS and a local heyvmd"]
    async fn end_to_end_an_image_is_built_from_a_dockerfile_and_then_reused() {
        let root = std::env::temp_dir().join(format!("ci-e2e-img-{}", crate::vm::new_id()));
        let d = test_dispatcher(&root).await;

        let outcome = tokio::spawn({
            let d = d.clone();
            async move { e2e_image_body(d).await }
        })
        .await;

        // Built images are not swept by the pool cleanup — they are files in
        // the runner's catalog — so this test removes its own.
        if let Ok(entries) = d.image_inventory().await {
            for e in entries {
                let _ = d.images.forget(&e.name, &e.runner_hd_id).await;
                let path = dirs_image_path(&e.name);
                let _ = std::fs::remove_file(&path);
            }
        }
        cleanup(&d).await;
        std::fs::remove_dir_all(&root).ok();
        if let Err(e) = outcome {
            std::panic::resume_unwind(e.into_panic());
        }
    }

    /// Where the daemon installs a built image, mirroring
    /// `get_firecracker_images_dir`. Test-only: the app never touches the
    /// runner's filesystem, which is the whole reason `/images/build` exists.
    fn dirs_image_path(name: &str) -> std::path::PathBuf {
        let base = std::env::var("MVM_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".heyo")
            });
        base.join("images/firecracker").join(format!("{name}.ext4"))
    }

    async fn e2e_image_body(d: Arc<Dispatcher>) {
        // A docker-built image boots `init=/init.sh` with no catalog base to
        // inherit one from, so the Dockerfile ships its own — the same
        // obligation deploy/image/Dockerfile discharges for the real build
        // image. The `ENV` is written through `RUN` into /etc/profile.d
        // because `docker export` discards OCI metadata; an `ENV` directive
        // would build fine and silently vanish.
        const DOCKERFILE: &str = "FROM debian:bookworm-slim\n\
             COPY marker.txt /etc/ci-marker\n\
             RUN echo 'and-a-run-layer' >> /etc/ci-marker\n\
             RUN mkdir -p /etc/profile.d \\\n\
              && printf 'export CI_IMAGE_MARKER=from-the-image\\n' > /etc/profile.d/10-e2e.sh\n\
             COPY init.sh /init.sh\n\
             RUN chmod +x /init.sh\n";

        /// PID 1 for the built VM: the minimum that satisfies the heyvm boot
        /// contract. Mount the API filesystems, quiet the serial console the
        /// exec protocol runs over, print the ready marker, and keep a shell
        /// alive on the console — bash specifically, as the serial exec
        /// protocol's output framing is exercised against bash and a dash
        /// console loses step output.
        const INIT_SH: &str = "#!/bin/sh\n\
             mount -t proc proc /proc\n\
             mount -t sysfs sysfs /sys\n\
             mount -t devtmpfs devtmpfs /dev 2>/dev/null\n\
             mkdir -p /dev/pts && mount -t devpts devpts /dev/pts\n\
             dmesg -n 1 2>/dev/null\n\
             mkdir -p /workspace\n\
             echo HEYVM_READY\n\
             while :; do /bin/bash --login; sleep 0.1; done\n";

        let files: &[(&str, &str)] = &[
            ("img/Dockerfile", DOCKERFILE),
            ("img/init.sh", INIT_SH),
            ("img/marker.txt", "a-copied-file\n"),
        ];

        let run1_id = crate::vm::new_id();
        seed_workspace(&d, &run1_id, files);
        let (run1, status1) = run_workflow_with_id(&d, E2E_IMAGE_YAML, &run1_id).await;
        assert_eq!(
            status1,
            crate::store::RunStatus::Success,
            "the image build run must pass; jobs: {:?}",
            d.store.jobs_of(&run1).await.unwrap()
        );

        // One image, ready, on this runner.
        let images = d.image_inventory().await.unwrap();
        assert_eq!(images.len(), 1, "{images:?}");
        let built = images[0].clone();
        assert_eq!(built.status, "ready", "{built:?}");
        assert!(built.name.starts_with("ci-img-"), "{}", built.name);
        assert!(
            dirs_image_path(&built.name).exists(),
            "the daemon must have written {}",
            dirs_image_path(&built.name).display()
        );

        // Every directive survived the snapshot, checked inside the guest.
        let job = d.store.jobs_of(&run1).await.unwrap().remove(0);
        let steps = d.store.steps_of(&job.id).await.unwrap();
        let proof = steps
            .iter()
            .find(|s| s.name == "Prove the image was built")
            .expect("the step ran");
        let log = d.store.read_log(proof).await.unwrap_or_default();
        assert!(log.contains("a-copied-file"), "COPY did not land: {log:?}");
        assert!(log.contains("and-a-run-layer"), "RUN did not land: {log:?}");
        assert!(
            log.contains("marker=from-the-image"),
            "the profile.d environment did not survive into the image: {log:?}"
        );

        // The build log is attached to the job, so a failing Dockerfile is
        // readable where somebody is already looking.
        let img_step = steps
            .iter()
            .find(|s| s.name.starts_with("Image ci-img-"))
            .expect("the build log is attached to the job");
        let build_log = d.store.read_log(img_step).await.unwrap_or_default();
        for want in ["building image ci-img-", "is ready after"] {
            assert!(
                build_log.contains(want),
                "build log is missing {want:?}: {build_log:?}"
            );
        }

        // ---- run 2: the same Dockerfile must not build a second image.
        let run2_id = crate::vm::new_id();
        seed_workspace(&d, &run2_id, files);
        let (run2, status2) = run_workflow_with_id(&d, E2E_IMAGE_YAML, &run2_id).await;
        assert_eq!(status2, crate::store::RunStatus::Success, "run 2: {run2}");

        let after = d.image_inventory().await.unwrap();
        assert_eq!(
            after.len(),
            1,
            "an unchanged Dockerfile must reuse the image, not build another: {after:?}"
        );
        assert_eq!(after[0].name, built.name);
        assert_eq!(
            after[0].ready_at, built.ready_at,
            "the image must not have been rebuilt"
        );

        // ---- run 3: editing the context busts it.
        let run3_id = crate::vm::new_id();
        let mut changed: Vec<(&str, &str)> = files.to_vec();
        changed[2] = ("img/marker.txt", "a-changed-file\n");
        seed_workspace(&d, &run3_id, &changed);
        let (_run3, status3) = run_workflow_with_id(&d, E2E_IMAGE_YAML, &run3_id).await;
        assert_eq!(status3, crate::store::RunStatus::Success);

        let after = d.image_inventory().await.unwrap();
        assert_eq!(
            after.len(),
            2,
            "a changed COPY source must build a new image: {after:?}"
        );
    }

    #[tokio::test]
    #[ignore = "needs Postgres, NATS and a local heyvmd"]
    async fn end_to_end_a_run_executes_reuses_its_vm_and_busts_on_a_changed_file() {
        let root = std::env::temp_dir().join(format!("ci-e2e-{}", crate::vm::new_id()));
        let d = test_dispatcher(&root).await;

        // Cleanup must survive a failed assertion, or a panicking test strands
        // VMs on the machine and streams on the NATS.
        let outcome = tokio::spawn({
            let d = d.clone();
            let root = root.clone();
            async move { e2e_body(d, root).await }
        })
        .await;

        cleanup(&d).await;
        std::fs::remove_dir_all(&root).ok();
        if let Err(e) = outcome {
            std::panic::resume_unwind(e.into_panic());
        }
    }

    async fn e2e_body(d: Arc<Dispatcher>, root: std::path::PathBuf) {
        // ---- run 1: everything runs, on a VM that did not exist before.
        let run1_id = crate::vm::new_id();
        seed_workspace(&d, &run1_id, &[("lockfile.txt", "v1")]);
        let (run1, status1) = run_workflow_with_id(&d, E2E_YAML, &run1_id).await;

        assert_eq!(
            status1,
            crate::store::RunStatus::Success,
            "run 1 must pass; jobs: {:?}",
            d.store.jobs_of(&run1).await.unwrap()
        );

        // The DAG really ran in order, and outputs really flowed.
        let jobs = d.store.jobs_of(&run1).await.unwrap();
        assert_eq!(jobs.len(), 2);
        for j in &jobs {
            assert_eq!(j.status, "success", "{} failed: {:?}", j.job_key, j.error);
        }
        let build = jobs.iter().find(|j| j.base_id == "build").unwrap();
        let steps = d.store.steps_of(&build.id).await.unwrap();
        // Checkout at index -1, then the workflow's own two. Looked up by name
        // rather than position, so adding an implicit step does not silently
        // shift what this is asserting about.
        let named = |name: &str| {
            steps.iter().find(|s| s.name == name).unwrap_or_else(|| {
                panic!(
                    "no step {name:?} in {:?}",
                    steps.iter().map(|s| &s.name).collect::<Vec<_>>()
                )
            })
        };
        assert_eq!(named("Checkout").status, "success");

        let log0 = d
            .store
            .read_log(named("Say hello"))
            .await
            .unwrap_or_default();
        assert!(log0.contains("hello from ci"), "step 1 log: {log0:?}");
        let log1 = d
            .store
            .read_log(named("Use the step output"))
            .await
            .unwrap_or_default();
        assert!(
            log1.contains("greeting was hi"),
            "a step output must reach the next step: {log1:?}"
        );

        // `ci/upload-artifact` goes out through a different transport from every
        // `run:` step — exec + tar + base64 — so a green run above proves
        // nothing about it. The row has to exist and the bytes have to be real.
        let artifacts = d.store.artifacts_of(&run1).await.unwrap();
        let uploaded = artifacts
            .iter()
            .find(|a| a.name == "e2e-dist")
            .unwrap_or_else(|| panic!("no e2e-dist artifact; got {artifacts:?}"));
        assert!(
            uploaded.size_bytes > 0,
            "an artifact recorded with no bytes is a report that something was \
             stored when it was not: {uploaded:?}"
        );
        assert_eq!(named("ci/upload-artifact").status, "success");

        let vm1 = build.sandbox_id.clone().expect("a sandbox was used");
        let fp1 = build.fingerprint.clone().expect("a fingerprint");

        // ---- run 2: same lockfile, so the same VM is inherited.
        let run2_id = crate::vm::new_id();
        seed_workspace(&d, &run2_id, &[("lockfile.txt", "v1")]);
        let (run2, status2) = run_workflow_with_id(&d, E2E_YAML, &run2_id).await;
        assert_eq!(status2, crate::store::RunStatus::Success);

        let build2 = d
            .store
            .jobs_of(&run2)
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.base_id == "build")
            .unwrap();
        assert_eq!(
            build2.fingerprint.as_deref(),
            Some(fp1.as_str()),
            "an unchanged lockfile must produce the same fingerprint"
        );
        assert_eq!(
            build2.sandbox_id.as_deref(),
            Some(vm1.as_str()),
            "the warm VM must be reused"
        );

        // ---- run 3: the lockfile changed, so the pool is busted.
        let run3_id = crate::vm::new_id();
        seed_workspace(&d, &run3_id, &[("lockfile.txt", "v2-changed")]);
        let (run3, status3) = run_workflow_with_id(&d, E2E_YAML, &run3_id).await;
        assert_eq!(status3, crate::store::RunStatus::Success);

        let build3 = d
            .store
            .jobs_of(&run3)
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.base_id == "build")
            .unwrap();
        assert_ne!(
            build3.fingerprint.as_deref(),
            Some(fp1.as_str()),
            "a changed cache_key_files entry must change the fingerprint"
        );
        assert_ne!(
            build3.sandbox_id.as_deref(),
            Some(vm1.as_str()),
            "and must therefore get a different VM"
        );

        let _ = root;
    }

    /// `run_workflow`, but with the run id chosen by the caller so the workspace
    /// can be populated first.
    async fn run_workflow_with_id(
        d: &Arc<Dispatcher>,
        yaml: &str,
        run_id: &str,
    ) -> (String, crate::store::RunStatus) {
        use futures::StreamExt;

        let wf = crate::workflow::Workflow::parse("e2e.yml", yaml).expect("workflow");
        let plan = crate::plan::Plan::build(&wf).expect("plan");
        d.store
            .create_run(
                run_id,
                &crate::store::RunRequest {
                    workflow_id: "e2e".into(),
                    source: "test".into(),
                    ..Default::default()
                },
                &plan,
            )
            .await
            .expect("run created");
        d.advance_run(run_id).await.expect("scheduled");

        // Both routes: a job with no `uses:` goes to the network queue, one
        // that pins a host goes to that host's. In production `spawn_consumers`
        // binds both for the same reason.
        let mut consumers = Vec::new();
        for route in [
            Route::Runner("hd-local".into()),
            Route::Network("local".into()),
        ] {
            consumers.push(d.bus.consumer_for(&route).await.expect("consumer"));
        }

        for _ in 0..20 {
            let run = d.store.get_run(run_id).await.unwrap().unwrap();
            if matches!(run.status.as_str(), "success" | "failure" | "cancelled") {
                break;
            }
            for consumer in &consumers {
                let mut batch = consumer
                    .fetch()
                    .max_messages(4)
                    .expires(Duration::from_secs(2))
                    .messages()
                    .await
                    .expect("fetch");
                while let Some(Ok(m)) = batch.next().await {
                    let job: JobMessage = serde_json::from_slice(&m.payload).expect("decode");
                    let attempt = m.info().map(|i| i.delivered as i32).unwrap_or(1);
                    if let Err(e) = d.run_job(&job, attempt).await {
                        eprintln!("job {} failed: {e}", job.job_key);
                    }
                    m.ack().await.ok();
                    d.advance_run(run_id).await.expect("advanced");
                }
            }
        }

        let run = d.store.get_run(run_id).await.unwrap().unwrap();
        let status = match run.status.as_str() {
            "success" => crate::store::RunStatus::Success,
            "failure" => crate::store::RunStatus::Failure,
            "cancelled" => crate::store::RunStatus::Cancelled,
            _ => crate::store::RunStatus::Running,
        };
        (run_id.to_string(), status)
    }
}
