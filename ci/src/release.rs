//! Persisted coordinator for the built-in `ci/merge-release` action.

use crate::bus::JobMessage;
use crate::plan::JobPlan;
use crate::release_git::{self, PreparedRelease};
use crate::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseRow {
    pub prepared: PreparedRelease,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug)]
struct GateJob {
    status: String,
    carried: bool,
    plan: JobPlan,
    steps: Vec<(i32, String)>,
}

fn gate(plan: &JobPlan, jobs: &[GateJob]) -> Result<(), String> {
    if plan.needs.is_empty() {
        return Err("release job must declare at least one dependency".into());
    }
    if plan.continue_on_error || plan.steps.iter().any(|s| s.continue_on_error) {
        return Err("release job may not tolerate errors".into());
    }
    for need in &plan.needs {
        let cells: Vec<_> = jobs.iter().filter(|j| j.plan.base_id == *need).collect();
        if cells.is_empty() {
            return Err(format!("release dependency {need:?} has no jobs"));
        }
        for cell in cells {
            if cell.status != "success" {
                return Err(format!("release dependency {need:?} did not succeed"));
            }
            if cell.plan.continue_on_error || cell.plan.steps.iter().any(|s| s.continue_on_error) {
                return Err(format!("release dependency {need:?} tolerates errors"));
            }
            if cell.carried {
                return Err("carried release validations are not accepted".into());
            }
            let expected = cell.plan.steps.len();
            if expected == 0
                || cell.steps.len() != expected
                || cell
                    .steps
                    .iter()
                    .enumerate()
                    .any(|(expected_idx, (idx, status))| {
                        *idx != expected_idx as i32 || status != "success"
                    })
            {
                return Err(format!(
                    "release dependency {need:?} lacks complete successful validation evidence"
                ));
            }
        }
    }
    Ok(())
}

pub async fn merge(
    store: &Store,
    msg: &JobMessage,
    plan: &JobPlan,
    source: &Path,
    manifests: &[String],
    tags: &BTreeMap<String, String>,
    token: &str,
) -> Result<PreparedRelease, String> {
    if token.is_empty() {
        return Err("release token is empty".into());
    }
    if !source.join(".git").exists() {
        return Err("release requires a materialized Git checkout".into());
    }
    let run = store
        .get_run(&msg.run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "release run does not exist".to_string())?;
    if run.status == "cancelled" {
        return Err("cancelled run cannot publish a release".into());
    }

    let rows = sqlx::query(
        "SELECT j.status, j.plan, j.carried_from IS NOT NULL AS carried,
                COALESCE(jsonb_agg(jsonb_build_array(s.idx,s.status) ORDER BY s.idx)
                         FILTER (WHERE s.id IS NOT NULL AND s.idx >= 0), '[]'::jsonb) AS steps
           FROM ci_job j
           LEFT JOIN ci_step s ON s.job_id=j.id
          WHERE j.run_id=$1 AND j.base_id = ANY($2)
          GROUP BY j.id",
    )
    .bind(&msg.run_id)
    .bind(&plan.needs)
    .fetch_all(store.pool())
    .await
    .map_err(|e| format!("read release gates: {e}"))?;
    let mut jobs = Vec::with_capacity(rows.len());
    for row in rows {
        let steps: Vec<(i32, String)> = serde_json::from_value(row.get("steps"))
            .map_err(|e| format!("decode validation steps: {e}"))?;
        jobs.push(GateJob {
            status: row.get("status"),
            carried: row.get("carried"),
            plan: serde_json::from_value(row.get("plan"))
                .map_err(|e| format!("decode dependency plan: {e}"))?,
            steps,
        });
    }
    gate(plan, &jobs)?;
    if store
        .is_job_cancelled(&msg.job_id)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err("cancelled job cannot publish a release".into());
    }

    let mut policy = manifests.to_vec();
    policy.sort();
    policy.dedup();
    let default_branch = run
        .default_branch
        .as_deref()
        .ok_or("release requires a validated repository default branch")?;
    let release_base = run.release_base_sha.as_deref()
        .ok_or("release requires the submitted target-trunk base; update the public git-submit client")?;
    let target_ref = format!("refs/heads/{default_branch}");
    let mut identity = json!({
        "version": 1, "repo": run.repo_url, "base": release_base,
        "source": run.sha, "source_ref": run.git_ref,
        "target_ref": target_ref, "manifests": policy,
    });
    // Preserve identities for pre-tag workflows and persisted retries.
    if !tags.is_empty() {
        identity["tags"] = serde_json::to_value(tags).map_err(|e| e.to_string())?;
    }
    let request_hash = hex::encode(Sha256::digest(
        serde_json::to_vec(&identity).map_err(|e| e.to_string())?,
    ));
    let prepared =
        release_git::prepare(source, release_base, &run.sha, &target_ref, &policy, tags).await?;
    let prepared_json = serde_json::to_value(&prepared).map_err(|e| e.to_string())?;

    let mut tx = store.pool().begin().await.map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO ci_release (run_id,request_hash,source_sha,base_sha,git_ref,versions,candidate_sha,prepared,status)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'prepared') ON CONFLICT (run_id) DO NOTHING")
        .bind(&msg.run_id).bind(&request_hash).bind(&run.sha).bind(release_base)
        .bind(&target_ref).bind(&prepared.versions).bind(&prepared.release_sha).bind(&prepared_json)
        .execute(&mut *tx).await.map_err(|e| e.to_string())?;
    let owned = sqlx::query(
        "SELECT request_hash,prepared,status FROM ci_release WHERE run_id=$1 FOR UPDATE",
    )
    .bind(&msg.run_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    let saved: PreparedRelease =
        serde_json::from_value(owned.get("prepared")).map_err(|e| e.to_string())?;
    if owned.get::<String, _>("request_hash") != request_hash || saved != prepared {
        return Err("release replay does not match the persisted candidate".into());
    }
    if owned.get::<String, _>("status") == "published" {
        tx.commit().await.map_err(|e| e.to_string())?;
        return Ok(saved);
    }
    let event = Store::add_event(
        &mut tx,
        &msg.run_id,
        Some(&msg.job_id),
        Some(&msg.job_key),
        None,
        "ci.release.status.v1",
        "prepared",
        None,
    )
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE ci_event_outbox SET payload=payload || $2 WHERE id=$1")
        .bind(event)
        .bind(json!({"release_sha": saved.release_sha, "versions": saved.versions, "tags": saved.tags}))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;

    if store
        .is_job_cancelled(&msg.job_id)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err("cancelled job cannot publish a release".into());
    }
    if let Err(_publish_error) =
        release_git::publish(source, &run.repo_url, token, release_base, &saved).await
    {
        let generic = "release publication outcome is unknown; retry the persisted candidate";
        let mut tx = store.pool().begin().await.map_err(|e| e.to_string())?;
        let changed = sqlx::query("UPDATE ci_release SET status='unknown',error=$2,updated_at=now() WHERE run_id=$1 AND candidate_sha=$3 AND status <> 'published'")
            .bind(&msg.run_id).bind(generic).bind(&saved.release_sha).execute(&mut *tx).await.map_err(|e| e.to_string())?;
        if changed.rows_affected() > 0 {
            let event = Store::add_event(
                &mut tx,
                &msg.run_id,
                Some(&msg.job_id),
                Some(&msg.job_key),
                None,
                "ci.release.status.v1",
                "unknown",
                Some(generic),
            )
            .await
            .map_err(|e| e.to_string())?;
            sqlx::query("UPDATE ci_event_outbox SET payload=payload || $2 WHERE id=$1")
                .bind(event)
                .bind(json!({"release_sha": saved.release_sha, "versions": saved.versions, "tags": saved.tags}))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())?;
        return Err(generic.into());
    }
    let mut tx = store.pool().begin().await.map_err(|e| e.to_string())?;
    sqlx::query("UPDATE ci_release SET status='published',error=NULL,updated_at=now() WHERE run_id=$1 AND candidate_sha=$2")
        .bind(&msg.run_id).bind(&saved.release_sha).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    let event = Store::add_event(
        &mut tx,
        &msg.run_id,
        Some(&msg.job_id),
        Some(&msg.job_key),
        None,
        "ci.release.status.v1",
        "published",
        None,
    )
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE ci_event_outbox SET payload=payload || $2 WHERE id=$1")
        .bind(event)
        .bind(json!({"release_sha": saved.release_sha, "versions": saved.versions, "tags": saved.tags}))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(saved)
}

pub async fn get(store: &Store, run: &str) -> Result<Option<ReleaseRow>, String> {
    let row = sqlx::query("SELECT prepared,status,error FROM ci_release WHERE run_id=$1")
        .bind(run)
        .fetch_optional(store.pool())
        .await
        .map_err(|e| e.to_string())?;
    row.map(|r| {
        Ok(ReleaseRow {
            prepared: serde_json::from_value(r.get("prepared")).map_err(|e| e.to_string())?,
            status: r.get("status"),
            error: r.get("error"),
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plans(extra: &str) -> (JobPlan, JobPlan) {
        let yaml = format!(
            "jobs:\n  validate:\n    vm: {{ driver: firecracker }}\n    {extra}steps:\n      - run: cargo test\n      - run: cargo check\n  release:\n    needs: [validate]\n    vm: {{ driver: firecracker }}\n    steps:\n      - uses: ci/merge-release\n"
        );
        let workflow = crate::workflow::Workflow::parse("release.yml", &yaml).unwrap();
        let plan = crate::plan::Plan::build(&workflow).unwrap();
        (plan.jobs[0].clone(), plan.jobs[1].clone())
    }

    #[test]
    fn release_gate_requires_exact_fresh_successful_plan_evidence() {
        let (validation, release) = plans("");
        let job = |status: &str, carried, steps: &[(i32, &str)]| GateJob {
            status: status.into(),
            carried,
            plan: validation.clone(),
            steps: steps.iter().map(|(i, s)| (*i, (*s).into())).collect(),
        };
        assert!(
            gate(
                &release,
                &[job("success", false, &[(0, "success"), (1, "success")])]
            )
            .is_ok()
        );
        assert!(gate(&release, &[]).is_err());
        assert!(
            gate(
                &release,
                &[job("failure", false, &[(0, "success"), (1, "success")])]
            )
            .is_err()
        );
        assert!(gate(&release, &[job("success", false, &[(0, "success")])]).is_err());
        assert!(
            gate(
                &release,
                &[job("success", false, &[(0, "success"), (2, "success")])]
            )
            .is_err()
        );
        assert!(
            gate(
                &release,
                &[job("success", false, &[(0, "success"), (1, "skipped")])]
            )
            .is_err()
        );
        assert!(
            gate(
                &release,
                &[job("success", true, &[(0, "success"), (1, "success")])]
            )
            .is_err()
        );
        let (tolerated, release) = plans("continue-on-error: true\n    ");
        let tolerated = GateJob {
            status: "success".into(),
            carried: false,
            plan: tolerated,
            steps: vec![(0, "success".into()), (1, "success".into())],
        };
        assert!(gate(&release, &[tolerated]).is_err());
    }

    #[test]
    fn release_example_preserves_stage_order_and_output_handoff() {
        let workflow = crate::workflow::Workflow::parse("release-example.yml", include_str!("../release-example.yml")).unwrap();
        let plan = crate::plan::Plan::build(&workflow).unwrap();
        let build = plan.jobs.iter().find(|j| j.base_id == "build").unwrap();
        assert_eq!(build.needs, ["release"]);
        assert_eq!(build.steps[0].uses.as_deref(), Some("ci/checkout-release"));
        let mut ctx = crate::expr::Context::default();
        ctx.set("steps", json!({"archive":{"outputs":{"archive-id":"finalized-123"}}}));
        assert_eq!(ctx.substitute(&build.outputs["archive_id"]), "finalized-123");
        let deploy = plan.jobs.iter().find(|j| j.base_id == "deploy").unwrap();
        assert_eq!(deploy.needs, ["build"]);
    }

    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL and local git"]
    async fn release_is_gated_persisted_before_push_and_replayable() {
        use crate::store::{JobStatus, RunRequest, StepStatus, step_id};
        use std::process::Command;
        use std::time::Duration;

        fn git(dir: &Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().into()
        }
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote.git");
        let source = root.path().join("source");
        std::fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--bare", "--initial-branch=main"]);
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "--initial-branch=main"]);
        git(&source, &["config", "user.name", "Test"]);
        git(&source, &["config", "user.email", "test@example.test"]);
        std::fs::write(
            source.join("package.json"),
            r#"{"name":"demo","version":"1.0.0"}"#,
        )
        .unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "chore: base"]);
        let base = git(&source, &["rev-parse", "HEAD"]);
        git(
            &source,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&source, &["push", "origin", "main"]);
        std::fs::write(source.join("code.txt"), "feature\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "feat: change"]);
        let head = git(&source, &["rev-parse", "HEAD"]);

        let workflow = crate::workflow::Workflow::parse("release.yml", "jobs:\n  validate:\n    vm: { driver: firecracker }\n    steps:\n      - run: cargo test\n  release:\n    needs: [validate]\n    vm: { driver: firecracker }\n    steps:\n      - uses: ci/merge-release\n").unwrap();
        let plan = crate::plan::Plan::build(&workflow).unwrap();
        let store = Store::connect(
            &std::env::var("CI_TEST_DATABASE_URL").unwrap(),
            root.path().join("logs"),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        store.migrate().await.unwrap();

        async fn make_run(
            store: &Store,
            plan: &crate::plan::Plan,
            remote: &Path,
            base: &str,
            head: &str,
            success: bool,
        ) -> (String, JobMessage) {
            let run = crate::vm::new_id();
            store
                .create_run(
                    &run,
                    &RunRequest {
                        repo_url: remote.to_string_lossy().into(),
                        git_ref: "refs/heads/main".into(),
                        sha: head.into(),
                        before_sha: base.into(),
                        default_branch: Some("main".into()),
                        release_base_sha: Some(base.into()),
                        ..Default::default()
                    },
                    plan,
                )
                .await
                .unwrap();
            let jobs = store.jobs_of(&run).await.unwrap();
            let validation = jobs.iter().find(|j| j.base_id == "validate").unwrap();
            store
                .start_job(&validation.id, "host", "sandbox", "fp", 1)
                .await
                .unwrap();
            let sid = step_id(&validation.id, 0);
            store
                .create_step(&sid, &validation.id, 0, "Test", None)
                .await
                .unwrap();
            store
                .finish_step(
                    &sid,
                    if success {
                        StepStatus::Success
                    } else {
                        StepStatus::Failure
                    },
                    Some(if success { 0 } else { 1 }),
                    None,
                )
                .await
                .unwrap();
            store
                .set_job_status(
                    &validation.id,
                    if success {
                        JobStatus::Success
                    } else {
                        JobStatus::Failure
                    },
                    None,
                )
                .await
                .unwrap();
            let release = jobs.iter().find(|j| j.base_id == "release").unwrap();
            store
                .start_job(&release.id, "host", "sandbox-release", "fp", 1)
                .await
                .unwrap();
            (
                run.clone(),
                JobMessage {
                    run_id: run,
                    job_id: release.id.clone(),
                    job_key: release.job_key.clone(),
                },
            )
        }
        let (run, msg) = make_run(&store, &plan, &remote, &base, &head, true).await;
        let release_plan = plan.jobs.iter().find(|j| j.base_id == "release").unwrap();
        let constraint = format!("reject_release_{}", run.replace('-', "_"));
        sqlx::query(&format!("ALTER TABLE ci_event_outbox ADD CONSTRAINT \"{constraint}\" CHECK (run_id <> '{run}' OR event_type <> 'ci.release.status.v1')")).execute(store.pool()).await.unwrap();
        assert!(
            merge(
                &store,
                &msg,
                release_plan,
                &source,
                &["package.json".into()],
                &BTreeMap::new(),
                "test"
            )
            .await
            .is_err()
        );
        assert!(
            get(&store, &run).await.unwrap().is_none(),
            "candidate and event must commit atomically before push"
        );
        assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), base);
        sqlx::query(&format!(
            "ALTER TABLE ci_event_outbox DROP CONSTRAINT \"{constraint}\""
        ))
        .execute(store.pool())
        .await
        .unwrap();
        let first = merge(
            &store,
            &msg,
            release_plan,
            &source,
            &["package.json".into()],
            &BTreeMap::new(),
            "test",
        )
        .await
        .unwrap();
        let row = get(&store, &run).await.unwrap().unwrap();
        assert_eq!(row.status, "published");
        assert_eq!(row.prepared, first);
        assert_eq!(
            git(&remote, &["rev-parse", "refs/heads/main"]),
            first.release_sha
        );
        assert_eq!(
            merge(
                &store,
                &msg,
                release_plan,
                &source,
                &["package.json".into()],
                &BTreeMap::new(),
                "test"
            )
            .await
            .unwrap(),
            first
        );

        let remote_before = git(&remote, &["rev-parse", "refs/heads/main"]);
        let (failed_run, failed_msg) = make_run(&store, &plan, &remote, &base, &head, false).await;
        assert!(
            merge(
                &store,
                &failed_msg,
                release_plan,
                &source,
                &["package.json".into()],
                &BTreeMap::new(),
                "test"
            )
            .await
            .is_err()
        );
        assert!(get(&store, &failed_run).await.unwrap().is_none());
        assert_eq!(
            git(&remote, &["rev-parse", "refs/heads/main"]),
            remote_before
        );
    }
}
