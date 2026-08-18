//! Turning a parsed workflow into the list of jobs to dispatch.
//!
//! Three things happen here, in order:
//!
//! 1. **Matrix expansion.** `strategy.matrix` becomes one job per combination,
//!    with `include:` and `exclude:` applied the way GitHub applies them.
//! 2. **Key assignment.** Each expanded job gets an id that is safe to
//!    interpolate into a NATS subject and a VM name, plus a separate display
//!    label for humans. These are not the same string, and conflating them is
//!    how `build (x86_64, 1.70)` ends up in a subject.
//! 3. **Topological ordering.** `needs` becomes a run order, stable with respect
//!    to the order the author wrote.
//!
//! `needs` names a *base* job, not a matrix cell, so a job that needs `build`
//! waits for every cell of `build`. That is GitHub's rule and the only one that
//! makes sense: the author of the dependent job does not know how many cells
//! there are.

use crate::expr::Context;
use crate::vm::VmSpec;
use crate::workflow::{Fallback, Job, Step, Target, Workflow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

/// GitHub defaults a job to 360 minutes. An hour is the more useful default for
/// a system where a stuck job holds a VM out of the pool the whole time; a
/// workflow that genuinely needs longer says so.
const DEFAULT_JOB_TIMEOUT_MINUTES: u64 = 60;

/// One dispatchable unit: a job, or one cell of a job's matrix.
///
/// Serialized into `ci_job.plan`, so a redelivered job runs exactly what the
/// original delivery would have.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobPlan {
    /// Unique, and safe for a NATS subject token and a VM name.
    pub key: String,
    /// The job id as written, shared by every cell of a matrix.
    pub base_id: String,
    /// What to show a human: `build (x86_64)`.
    pub display: String,
    pub target: Target,
    pub fallback: Fallback,
    pub vm: VmSpec,
    /// Base ids this job waits for.
    pub needs: Vec<String>,
    pub condition: Option<String>,
    pub matrix: BTreeMap<String, Value>,
    pub env: BTreeMap<String, String>,
    pub outputs: BTreeMap<String, String>,
    pub continue_on_error: bool,
    pub timeout: Duration,
    pub steps: Vec<Step>,
    /// Cells of this base job that may run at once. `None` is unlimited.
    pub max_parallel: Option<usize>,
    pub fail_fast: bool,
}

impl JobPlan {
    /// The expression context this job's own fields are evaluated against.
    /// `needs` and `steps` are added by the executor as results arrive.
    pub fn base_context(&self) -> Context {
        let mut c = Context::new();
        c.set(
            "matrix",
            Value::Object(self.matrix.clone().into_iter().collect()),
        );
        c.set(
            "env",
            Value::Object(
                self.env
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            ),
        );
        c.set(
            "job",
            serde_json::json!({ "id": self.base_id, "key": self.key }),
        );
        c
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub workflow_path: String,
    pub workflow_name: Option<String>,
    pub env: BTreeMap<String, String>,
    /// Topologically ordered: every job appears after everything it needs.
    pub jobs: Vec<JobPlan>,
}

impl Plan {
    pub fn build(wf: &Workflow) -> Result<Self, PlanError> {
        let mut jobs = Vec::new();
        let mut used_keys = BTreeSet::new();

        for (id, job) in &wf.jobs {
            let combos = expand_matrix(id, job)?;
            let strategy = job.strategy.as_ref();
            for (index, matrix) in combos.into_iter().enumerate() {
                let key = unique_key(id, &matrix, index, &mut used_keys);
                let display = display_label(job, id, &matrix);
                // Job-level env is substituted against the matrix now, so
                // `env: { TARGET: "${{ matrix.target }}" }` differs per cell.
                let mut cell = JobPlan {
                    key,
                    base_id: id.clone(),
                    display,
                    target: job
                        .target()
                        .map_err(|e| PlanError::Workflow(e.to_string()))?,
                    fallback: job.fallback,
                    vm: job.vm.clone(),
                    needs: job.needs.clone(),
                    condition: job.condition.clone(),
                    matrix,
                    env: job.env.clone(),
                    outputs: job.outputs.clone(),
                    continue_on_error: job.continue_on_error,
                    timeout: Duration::from_secs(
                        job.timeout_minutes.unwrap_or(DEFAULT_JOB_TIMEOUT_MINUTES) * 60,
                    ),
                    steps: job.steps.clone(),
                    max_parallel: strategy.and_then(|s| s.max_parallel),
                    fail_fast: strategy.map(|s| s.fail_fast).unwrap_or(true),
                };
                substitute_matrix(&mut cell);
                jobs.push(cell);
            }
        }

        let jobs = topological_order(jobs)?;
        Ok(Self {
            workflow_path: wf.path.clone(),
            workflow_name: wf.name.clone(),
            env: wf.env.clone(),
            jobs,
        })
    }

    /// Every plan sharing a base id — one matrix cell each.
    pub fn cells_of<'a>(&'a self, base_id: &'a str) -> impl Iterator<Item = &'a JobPlan> {
        self.jobs.iter().filter(move |j| j.base_id == base_id)
    }
}

/// Resolve `${{ matrix.* }}` in the fields that select where and how a job runs.
///
/// Only the matrix scope is available this early — `needs` and `steps` do not
/// exist until other jobs finish. That is enough for the fields that must be
/// known before dispatch: which runner, which image, which env.
fn substitute_matrix(job: &mut JobPlan) {
    let ctx = job.base_context();
    if let Some(net) = &job.target.network {
        job.target.network = Some(ctx.substitute(net));
    }
    if let Some(runner) = &job.target.node {
        job.target.node = Some(ctx.substitute(runner));
    }
    if let Some(image) = &job.vm.image {
        job.vm.image = Some(ctx.substitute(image));
    }
    job.env = job
        .env
        .iter()
        .map(|(k, v)| (k.clone(), ctx.substitute(v)))
        .collect();
    job.vm.env_vars = job
        .vm
        .env_vars
        .iter()
        .map(|(k, v)| (k.clone(), ctx.substitute(v)))
        .collect();
    job.vm.setup_hooks = job
        .vm
        .setup_hooks
        .iter()
        .map(|h| ctx.substitute(h))
        .collect();
}

/// Expand `strategy.matrix` into one map per combination.
///
/// No matrix yields a single empty combination, so the caller has one code path.
fn expand_matrix(job_id: &str, job: &Job) -> Result<Vec<BTreeMap<String, Value>>, PlanError> {
    let Some(matrix) = job.strategy.as_ref().and_then(|s| s.matrix.as_ref()) else {
        return Ok(vec![BTreeMap::new()]);
    };
    let Some(mapping) = matrix.as_mapping() else {
        return Err(PlanError::BadMatrix {
            job: job_id.to_string(),
            detail: "`strategy.matrix` must be a mapping of axes".to_string(),
        });
    };

    // Axes, in sorted key order so the cartesian product is deterministic
    // regardless of how the YAML was written.
    let mut axes: Vec<(String, Vec<Value>)> = Vec::new();
    let mut include: Vec<BTreeMap<String, Value>> = Vec::new();
    let mut exclude: Vec<BTreeMap<String, Value>> = Vec::new();

    for (k, v) in mapping {
        let Some(name) = k.as_str() else {
            return Err(PlanError::BadMatrix {
                job: job_id.to_string(),
                detail: "a matrix axis name must be a string".to_string(),
            });
        };
        match name {
            "include" | "exclude" => {
                let Some(seq) = v.as_sequence() else {
                    return Err(PlanError::BadMatrix {
                        job: job_id.to_string(),
                        detail: format!("`{name}` must be a list of mappings"),
                    });
                };
                let entries: Vec<BTreeMap<String, Value>> =
                    seq.iter().filter_map(yaml_map_to_json).collect();
                if name == "include" {
                    include = entries;
                } else {
                    exclude = entries;
                }
            }
            _ => {
                let Some(items) = v.as_sequence() else {
                    return Err(PlanError::BadMatrix {
                        job: job_id.to_string(),
                        detail: format!("matrix axis {name:?} must be a list of values"),
                    });
                };
                axes.push((name.to_string(), items.iter().map(yaml_to_json).collect()));
            }
        }
    }
    axes.sort_by(|a, b| a.0.cmp(&b.0));

    // Cartesian product.
    let mut combos: Vec<BTreeMap<String, Value>> = vec![BTreeMap::new()];
    for (name, values) in &axes {
        if values.is_empty() {
            return Err(PlanError::BadMatrix {
                job: job_id.to_string(),
                detail: format!("matrix axis {name:?} is empty"),
            });
        }
        let mut next = Vec::with_capacity(combos.len() * values.len());
        for base in &combos {
            for v in values {
                let mut c = base.clone();
                c.insert(name.clone(), v.clone());
                next.push(c);
            }
        }
        combos = next;
    }

    combos.retain(|c| !exclude.iter().any(|e| matches_subset(c, e)));

    // GitHub's `include` rule: an entry that matches an existing combination
    // extends it in place; one that matches none is appended as its own
    // combination. That is what lets `include` both add a variable to one cell
    // and add a whole extra cell.
    for entry in include {
        let mut merged_any = false;
        for c in combos.iter_mut() {
            if matches_subset(c, &entry) {
                for (k, v) in &entry {
                    c.insert(k.clone(), v.clone());
                }
                merged_any = true;
            }
        }
        if !merged_any {
            combos.push(entry);
        }
    }

    if combos.is_empty() {
        return Err(PlanError::EmptyMatrix(job_id.to_string()));
    }
    Ok(combos)
}

/// Whether every key `entry` sets that `combo` also has agrees.
fn matches_subset(combo: &BTreeMap<String, Value>, entry: &BTreeMap<String, Value>) -> bool {
    entry
        .iter()
        .filter(|(k, _)| combo.contains_key(*k))
        .all(|(k, v)| combo.get(k) == Some(v))
}

fn yaml_to_json(v: &serde_yaml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn yaml_map_to_json(v: &serde_yaml::Value) -> Option<BTreeMap<String, Value>> {
    let m = v.as_mapping()?;
    Some(
        m.iter()
            .filter_map(|(k, v)| k.as_str().map(|k| (k.to_string(), yaml_to_json(v))))
            .collect(),
    )
}

/// A key safe for a NATS subject token and a VM name.
///
/// Prefers something readable — `build-x86_64` — and falls back to the cell
/// index when the matrix values do not sanitize to anything useful or the result
/// would collide. Collisions are real: `target: [a/b, a-b]` sanitizes both to
/// `a-b`, and two jobs sharing a subject would each consume the other's work.
fn unique_key(
    base_id: &str,
    matrix: &BTreeMap<String, Value>,
    index: usize,
    used: &mut BTreeSet<String>,
) -> String {
    let mut candidate = base_id.to_string();
    if !matrix.is_empty() {
        let suffix: Vec<String> = matrix
            .values()
            .map(|v| sanitize(&display_value(v)))
            .filter(|s| !s.is_empty())
            .collect();
        if !suffix.is_empty() {
            candidate = format!("{base_id}-{}", suffix.join("-"));
        }
    }
    if candidate.len() > 64 || used.contains(&candidate) {
        candidate = format!("{base_id}-{index}");
    }
    // Still taken (two cells, both falling back) — extend until unique.
    let mut final_key = candidate.clone();
    let mut n = index;
    while used.contains(&final_key) {
        n += 1;
        final_key = format!("{base_id}-{n}");
    }
    used.insert(final_key.clone());
    final_key
}

/// Reduce a matrix value to the subject-token alphabet.
///
/// `_` and `-` pass through — both are already legal in a NATS subject token and
/// in a sandbox name, and folding `_` to `-` would turn `x86_64` into `x86-64`
/// for no benefit while making two distinct axis values collide.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn display_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `build (x86_64, 1.70)` — GitHub's shape.
fn display_label(job: &Job, id: &str, matrix: &BTreeMap<String, Value>) -> String {
    let base = job.name.clone().unwrap_or_else(|| id.to_string());
    if matrix.is_empty() {
        return base;
    }
    let values: Vec<String> = matrix.values().map(display_value).collect();
    format!("{base} ({})", values.join(", "))
}

/// Kahn's algorithm over base ids, stable with respect to input order.
///
/// Stability matters for the dashboard: two runs of the same workflow should
/// list their jobs identically, and a `HashMap` iteration would not.
fn topological_order(jobs: Vec<JobPlan>) -> Result<Vec<JobPlan>, PlanError> {
    let all_bases: BTreeSet<String> = jobs.iter().map(|j| j.base_id.clone()).collect();

    let mut ordered: Vec<JobPlan> = Vec::with_capacity(jobs.len());
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut remaining: Vec<JobPlan> = jobs;

    while !remaining.is_empty() {
        // Every cell whose dependencies are all satisfied, in input order.
        let ready: Vec<usize> = remaining
            .iter()
            .enumerate()
            .filter(|(_, j)| j.needs.iter().all(|n| done.contains(n)))
            .map(|(i, _)| i)
            .collect();

        if ready.is_empty() {
            // The workflow validator rejects cycles, so reaching here means a
            // need on a base id that produced no cells at all.
            let stuck: Vec<String> = remaining
                .iter()
                .flat_map(|j| j.needs.iter().filter(|n| !all_bases.contains(*n)).cloned())
                .collect();
            return Err(PlanError::Unsatisfiable(if stuck.is_empty() {
                remaining.iter().map(|j| j.key.clone()).collect()
            } else {
                stuck
            }));
        }

        // A whole ready wave is promoted before recomputing `done`, so that all
        // cells of one base job land together and a dependent job cannot start
        // after only the first cell finished planning.
        let mut wave: Vec<JobPlan> = Vec::with_capacity(ready.len());
        for i in ready.into_iter().rev() {
            wave.push(remaining.remove(i));
        }
        wave.reverse();
        for j in &wave {
            done.insert(j.base_id.clone());
        }
        ordered.extend(wave);
    }

    Ok(ordered)
}

#[derive(Debug, PartialEq, Eq)]
pub enum PlanError {
    Workflow(String),
    BadMatrix { job: String, detail: String },
    EmptyMatrix(String),
    Unsatisfiable(Vec<String>),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workflow(detail) => write!(f, "{detail}"),
            Self::BadMatrix { job, detail } => {
                write!(f, "job {job:?} has an invalid matrix: {detail}")
            }
            Self::EmptyMatrix(job) => write!(
                f,
                "job {job:?} has a matrix that excludes every combination, so it \
                 would never run"
            ),
            Self::Unsatisfiable(names) => write!(
                f,
                "these jobs can never start because their dependencies produced no \
                 jobs: {}",
                names.join(", ")
            ),
        }
    }
}

impl std::error::Error for PlanError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::Workflow;

    fn plan(yaml: &str) -> Result<Plan, PlanError> {
        let wf = Workflow::parse("wf.yml", yaml).expect("workflow parses");
        Plan::build(&wf)
    }

    fn keys(p: &Plan) -> Vec<&str> {
        p.jobs.iter().map(|j| j.key.as_str()).collect()
    }

    #[test]
    fn a_job_without_a_matrix_yields_exactly_one_plan() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        assert_eq!(keys(&p), ["build"]);
        assert!(p.jobs[0].matrix.is_empty());
        assert_eq!(p.jobs[0].display, "build");
    }

    #[test]
    fn a_single_axis_matrix_expands_to_one_job_per_value() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: [x86_64, aarch64]
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        assert_eq!(keys(&p), ["build-x86_64", "build-aarch64"]);
        assert_eq!(p.jobs[0].display, "build (x86_64)");
        assert_eq!(p.jobs[0].matrix["target"], serde_json::json!("x86_64"));
    }

    #[test]
    fn two_axes_produce_the_cartesian_product() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: [x86, arm]
        toolchain: [stable, nightly]
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        assert_eq!(p.jobs.len(), 4);
        let displays: Vec<&str> = p.jobs.iter().map(|j| j.display.as_str()).collect();
        // Axes sort by name, so `target` varies slowest under `toolchain`.
        assert!(displays.contains(&"build (x86, stable)"), "{displays:?}");
        assert!(displays.contains(&"build (arm, nightly)"), "{displays:?}");
    }

    #[test]
    fn exclude_removes_a_combination() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: [x86, arm]
        toolchain: [stable, nightly]
        exclude:
          - target: arm
            toolchain: nightly
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        assert_eq!(p.jobs.len(), 3);
        assert!(!p.jobs.iter().any(|j| j.display.contains("arm, nightly")));
    }

    /// GitHub's rule, and the reason `include` is not just "the whole matrix":
    /// an entry that matches extends in place, one that matches nothing is added.
    #[test]
    fn include_extends_a_matching_cell_and_appends_a_non_matching_one() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: [x86, arm]
        include:
          - target: x86
            extra: fast
          - target: riscv
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        assert_eq!(p.jobs.len(), 3);
        let x86 = p
            .jobs
            .iter()
            .find(|j| j.matrix["target"] == serde_json::json!("x86"))
            .unwrap();
        assert_eq!(
            x86.matrix["extra"],
            serde_json::json!("fast"),
            "extended in place"
        );
        assert!(
            p.jobs
                .iter()
                .any(|j| j.matrix["target"] == serde_json::json!("riscv")),
            "a non-matching include becomes its own cell"
        );
    }

    #[test]
    fn a_matrix_that_excludes_everything_is_an_error() {
        let err = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: [x86]
        exclude:
          - target: x86
    steps: [{ run: "true" }]
"#,
        )
        .unwrap_err();
        assert_eq!(err, PlanError::EmptyMatrix("build".into()));
    }

    /// The key reaches a NATS subject and a VM name; the display label does not.
    #[test]
    fn keys_stay_subject_safe_while_labels_stay_readable() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: ["ubuntu 24.04", "debian/12"]
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        for j in &p.jobs {
            assert!(
                crate::config::is_subject_token(&j.key),
                "key {:?} is not a subject token",
                j.key
            );
        }
        let displays: Vec<&str> = p.jobs.iter().map(|j| j.display.as_str()).collect();
        assert!(displays.contains(&"build (ubuntu 24.04)"), "{displays:?}");
    }

    /// Two axis values that sanitize to the same string must not share a key —
    /// they would share a NATS subject and consume each other's work.
    #[test]
    fn colliding_sanitized_values_still_get_distinct_keys() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: ["a/b", "a-b", "a b"]
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        let ks = keys(&p);
        let unique: BTreeSet<&&str> = ks.iter().collect();
        assert_eq!(unique.len(), ks.len(), "keys collided: {ks:?}");
    }

    #[test]
    fn needs_orders_the_plan() {
        let p = plan(
            r#"
jobs:
  deploy:
    needs: [build]
    vm: { driver: firecracker }
    steps: [{ run: "true" }]
  build:
    vm: { driver: firecracker }
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        // Declared deploy-first, but build must be planned first.
        assert_eq!(keys(&p), ["build", "deploy"]);
    }

    /// A dependent job waits for every cell, because the author of that job has
    /// no way to name one.
    #[test]
    fn a_dependent_job_is_ordered_after_every_matrix_cell() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: [a, b, c]
    steps: [{ run: "true" }]
  deploy:
    needs: [build]
    vm: { driver: firecracker }
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        let ks = keys(&p);
        assert_eq!(ks.len(), 4);
        assert_eq!(ks[3], "deploy");
        assert_eq!(p.cells_of("build").count(), 3);
    }

    #[test]
    fn ordering_is_stable_across_runs() {
        let yaml = r#"
jobs:
  a:
    vm: { driver: firecracker }
    steps: [{ run: "true" }]
  b:
    vm: { driver: firecracker }
    steps: [{ run: "true" }]
  c:
    needs: [a, b]
    vm: { driver: firecracker }
    steps: [{ run: "true" }]
"#;
        let first = keys(&plan(yaml).unwrap()).join(",");
        for _ in 0..20 {
            assert_eq!(keys(&plan(yaml).unwrap()).join(","), first);
        }
        assert_eq!(first, "a,b,c");
    }

    /// The runner a cell lands on can depend on the matrix — that is how one
    /// job builds on two different machines.
    #[test]
    fn matrix_values_substitute_into_the_target_and_the_image() {
        let p = plan(
            r#"
jobs:
  build:
    uses: prod/${{ matrix.host }}
    vm:
      driver: firecracker
      image: "base-${{ matrix.host }}"
    strategy:
      matrix:
        host: [bigbox, smallbox]
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        let runners: Vec<Option<&str>> = p.jobs.iter().map(|j| j.target.node.as_deref()).collect();
        assert_eq!(runners, [Some("bigbox"), Some("smallbox")]);
        assert_eq!(p.jobs[0].vm.image.as_deref(), Some("base-bigbox"));
        assert_eq!(p.jobs[1].vm.image.as_deref(), Some("base-smallbox"));
    }

    #[test]
    fn matrix_values_substitute_into_env_and_setup_hooks() {
        let p = plan(
            r#"
jobs:
  build:
    vm:
      driver: firecracker
      env_vars:
        TARGET: "${{ matrix.target }}"
      setup_hooks:
        - "rustup target add ${{ matrix.target }}"
    env:
      JOB_TARGET: "${{ matrix.target }}"
    strategy:
      matrix:
        target: [x86_64]
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        assert_eq!(p.jobs[0].vm.env_vars["TARGET"], "x86_64");
        assert_eq!(p.jobs[0].env["JOB_TARGET"], "x86_64");
        assert_eq!(p.jobs[0].vm.setup_hooks[0], "rustup target add x86_64");
    }

    #[test]
    fn strategy_settings_reach_the_plan() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      max-parallel: 2
      fail-fast: false
      matrix:
        target: [a, b, c]
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        assert_eq!(p.jobs[0].max_parallel, Some(2));
        assert!(!p.jobs[0].fail_fast);
    }

    #[test]
    fn fail_fast_defaults_on_and_max_parallel_defaults_unlimited() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        assert!(p.jobs[0].fail_fast);
        assert_eq!(p.jobs[0].max_parallel, None);
    }

    #[test]
    fn the_job_timeout_defaults_to_an_hour_and_is_overridable() {
        let p = plan(
            r#"
jobs:
  a:
    vm: { driver: firecracker }
    steps: [{ run: "true" }]
  b:
    timeout-minutes: 5
    vm: { driver: firecracker }
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        assert_eq!(p.jobs[0].timeout, Duration::from_secs(3600));
        assert_eq!(p.jobs[1].timeout, Duration::from_secs(300));
    }

    #[test]
    fn the_base_context_exposes_matrix_env_and_job() {
        let p = plan(
            r#"
jobs:
  build:
    env: { NAME: ci }
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: [x86_64]
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        let ctx = p.jobs[0].base_context();
        assert_eq!(ctx.substitute("${{ matrix.target }}"), "x86_64");
        assert_eq!(ctx.substitute("${{ env.NAME }}"), "ci");
        assert_eq!(ctx.substitute("${{ job.id }}"), "build");
    }

    /// A numeric axis has to stay numeric, or `matrix.n > 1` compares strings.
    #[test]
    fn numeric_matrix_values_keep_their_type() {
        let p = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        n: [1, 2, 10]
    steps: [{ run: "true" }]
"#,
        )
        .unwrap();
        assert!(p.jobs[0].matrix["n"].is_number());
        let ctx = p.jobs[2].base_context();
        assert!(ctx.eval_condition("matrix.n > 5").unwrap());
        assert_eq!(ctx.substitute("${{ matrix.n }}"), "10", "not 10.0");
    }

    #[test]
    fn an_empty_axis_is_rejected() {
        let err = plan(
            r#"
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: []
    steps: [{ run: "true" }]
"#,
        )
        .unwrap_err();
        assert!(matches!(err, PlanError::BadMatrix { .. }), "{err:?}");
        assert!(err.to_string().contains("empty"), "{err}");
    }
}
