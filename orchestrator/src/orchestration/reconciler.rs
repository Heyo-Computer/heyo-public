use std::time::Duration;

use tracing::{error, info};

use crate::db;
use crate::repositories::{ExternalEventConsumeOutcome, OrchestrationRepository};
use crate::AppState;

use super::runtime::OrchestrationExecutor;

const RECONCILER_INTERVAL_SECS: u64 = 15;
const RECONCILER_BATCH_LIMIT: u64 = 50;

pub async fn run_runtime_reconciler(state: AppState) {
    let mut ticker = tokio::time::interval(Duration::from_secs(RECONCILER_INTERVAL_SECS));
    loop {
        ticker.tick().await;
        if let Err(error) = reconcile_once(&state).await {
            error!("Orchestration runtime reconciler failed: {}", error);
        }
    }
}

async fn reconcile_once(state: &AppState) -> anyhow::Result<()> {
    let db = db::get_db()?.clone();

    let repo = OrchestrationRepository::new(db.clone());
    let pending_event_ids = repo
        .list_pending_external_event_ids(RECONCILER_BATCH_LIMIT)
        .await?;
    for event_id in pending_event_ids {
        let repo = OrchestrationRepository::new(db.clone());
        let outcome = repo.consume_pending_external_event(&event_id).await?;
        maybe_resume_workflow(state, db.clone(), outcome).await?;
    }

    let repo = OrchestrationRepository::new(db.clone());
    let running_step_ids = repo.list_running_step_ids(RECONCILER_BATCH_LIMIT).await?;
    for step_id in running_step_ids {
        let repo = OrchestrationRepository::new(db.clone());
        if let Some(workflow_run_id) = repo
            .fail_stale_running_step(
                &step_id,
                "step attempt lease expired before the worker completed or heartbeated the step",
            )
            .await?
        {
            info!(
                "Reconciler failed stale running step {} in workflow {}",
                step_id, workflow_run_id
            );
        }
    }

    Ok(())
}

async fn maybe_resume_workflow(
    state: &AppState,
    db: sea_orm::DatabaseConnection,
    outcome: ExternalEventConsumeOutcome,
) -> anyhow::Result<()> {
    if let ExternalEventConsumeOutcome::Processed {
        workflow_run_id: Some(workflow_run_id),
        resume_workflow: true,
    } = outcome
    {
        let executor = OrchestrationExecutor::new(state.clone(), OrchestrationRepository::new(db));
        executor.execute_until_blocked(&workflow_run_id).await?;
    }

    Ok(())
}
