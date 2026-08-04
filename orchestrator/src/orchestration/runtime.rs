use std::collections::HashSet;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::{sync::oneshot, task::JoinHandle};
use tracing::{error, warn};

use crate::cloud_client;
use crate::cloud_client::CreateDeploymentRequest;
use crate::entities::{
    orchestration_artifact, orchestration_step_run, orchestration_thread, OrchestrationThread,
};
use crate::orchestration::adapters::{
    build_deploy_spec_preview, deploy_question_prompt_from_artifacts, execute_adapter,
    ArtifactDraft, ExternalAction, StepExecutionContext, StepExecutionResult,
};
use crate::repositories::{ArtifactCreateInput, OrchestrationRepository, ParentJobDetails};
use crate::AppState;

const STEP_ATTEMPT_LEASE_SECONDS: i64 = 1800;
const STEP_ATTEMPT_HEARTBEAT_INTERVAL_SECONDS: u64 = 90;

pub struct OrchestrationExecutor {
    state: AppState,
    repo: OrchestrationRepository,
}

pub fn spawn_execute_until_blocked(
    state: AppState,
    repo: OrchestrationRepository,
    workflow_run_id: String,
) -> JoinHandle<anyhow::Result<ParentJobDetails>> {
    tokio::spawn(async move {
        let executor = OrchestrationExecutor::new(state, repo);
        let result = executor.execute_until_blocked(&workflow_run_id).await;
        if let Err(run_error) = &result {
            error!(
                workflow_run_id = %workflow_run_id,
                "Background orchestration execution failed: {}",
                run_error
            );
        }
        result
    })
}

impl OrchestrationExecutor {
    pub fn new(state: AppState, repo: OrchestrationRepository) -> Self {
        Self { state, repo }
    }

    pub async fn execute_until_blocked(
        &self,
        workflow_run_id: &str,
    ) -> anyhow::Result<ParentJobDetails> {
        loop {
            let mut details = self
                .repo
                .find_parent_job_details(workflow_run_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Workflow run not found: {}", workflow_run_id))?;

            let thread = self.load_thread(&details.workflow_run.thread_id).await?;

            if let Some(step) = details
                .step_runs
                .iter()
                .find(|step| step.status == "failed")
            {
                self.record_message(
                    &thread.id,
                    "assistant",
                    format!("{} failed.", step_label(step)),
                    Some(json!({
                        "workflowRunId": details.workflow_run.id,
                        "stepRunId": step.id,
                        "stepKey": step.key,
                        "status": step.status,
                    })),
                )
                .await;
                self.repo
                    .update_workflow_state(
                        &details.workflow_run.id,
                        "failed",
                        "failed",
                        Some(step.key.clone()),
                    )
                    .await?;
                return self
                    .repo
                    .find_parent_job_details(workflow_run_id)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Workflow run disappeared: {}", workflow_run_id)
                    });
            }

            if details
                .step_runs
                .iter()
                .all(|step| step.status == "completed")
            {
                self.record_message(
                    &thread.id,
                    "assistant",
                    "Workflow completed successfully.".to_string(),
                    Some(json!({
                        "workflowRunId": details.workflow_run.id,
                        "status": "completed",
                    })),
                )
                .await;
                self.repo
                    .update_workflow_state(&details.workflow_run.id, "completed", "completed", None)
                    .await?;
                return self
                    .repo
                    .find_parent_job_details(workflow_run_id)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Workflow run disappeared: {}", workflow_run_id)
                    });
            }

            let completed_keys = details
                .step_runs
                .iter()
                .filter(|step| step.status == "completed")
                .map(|step| step.key.clone())
                .collect::<HashSet<_>>();
            let mut promoted = false;
            for step in details
                .step_runs
                .iter()
                .filter(|step| step.status == "pending")
            {
                if dependencies_satisfied(step, &completed_keys)
                    && self.repo.try_mark_step_ready(&step.id).await?
                {
                    promoted = true;
                }
            }
            if promoted {
                continue;
            }

            if let Some(step) = details
                .step_runs
                .iter()
                .find(|step| step.status == "waiting_approval")
            {
                self.ensure_pending_approval(&details, step).await?;
                self.repo
                    .update_workflow_state(
                        &details.workflow_run.id,
                        "waiting_approval",
                        &step.phase,
                        Some(step.key.clone()),
                    )
                    .await?;
                return self
                    .repo
                    .find_parent_job_details(workflow_run_id)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Workflow run disappeared: {}", workflow_run_id)
                    });
            }

            if let Some(step) = details
                .step_runs
                .iter()
                .find(|step| step.status == "blocked")
            {
                self.repo
                    .update_workflow_state(
                        &details.workflow_run.id,
                        "running",
                        &step.phase,
                        Some(step.key.clone()),
                    )
                    .await?;
                return self
                    .repo
                    .find_parent_job_details(workflow_run_id)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Workflow run disappeared: {}", workflow_run_id)
                    });
            }

            if let Some(step) = details
                .step_runs
                .iter()
                .find(|step| step.status == "running")
            {
                if self.repo.has_live_attempt(&step.id).await? {
                    self.repo
                        .update_workflow_state(
                            &details.workflow_run.id,
                            "running",
                            &step.phase,
                            Some(step.key.clone()),
                        )
                        .await?;
                    return self
                        .repo
                        .find_parent_job_details(workflow_run_id)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("Workflow run disappeared: {}", workflow_run_id)
                        });
                }

                let failure_reason = if step.adapter.as_deref() == Some("heyo.deploy")
                    && step.external_ref.is_some()
                {
                    "workflow executor found a deploy step whose execution lease expired before the external wait could be finalized"
                } else {
                    "workflow executor found a running step without a live execution lease after resumption"
                };
                self.repo
                    .fail_stale_running_step(&step.id, failure_reason)
                    .await?;
                self.repo
                    .update_workflow_state(
                        &details.workflow_run.id,
                        "failed",
                        "failed",
                        Some(step.key.clone()),
                    )
                    .await?;
                return self
                    .repo
                    .find_parent_job_details(workflow_run_id)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Workflow run disappeared: {}", workflow_run_id)
                    });
            }

            let Some(next_step) = details
                .step_runs
                .iter()
                .find(|step| step.status == "ready")
                .cloned()
            else {
                self.repo
                    .update_workflow_state(
                        &details.workflow_run.id,
                        "failed",
                        "failed",
                        details.workflow_run.current_child_job_key.clone(),
                    )
                    .await?;
                return self
                    .repo
                    .find_parent_job_details(workflow_run_id)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Workflow run disappeared: {}", workflow_run_id)
                    });
            };

            if next_step.r#type == "approval" {
                self.begin_approval_step(&details, &thread, &next_step)
                    .await?;
                continue;
            }

            if !self
                .repo
                .try_claim_step(
                    &next_step.id,
                    self.state.worker_id.as_str(),
                    STEP_ATTEMPT_LEASE_SECONDS,
                )
                .await?
            {
                continue;
            }

            self.repo
                .update_workflow_state(
                    &details.workflow_run.id,
                    "running",
                    &next_step.phase,
                    Some(next_step.key.clone()),
                )
                .await?;

            details = self
                .repo
                .find_parent_job_details(workflow_run_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Workflow run disappeared: {}", workflow_run_id))?;
            let current_step = details
                .step_runs
                .iter()
                .find(|step| step.id == next_step.id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Claimed step disappeared: {}", next_step.id))?;

            let execution_context = StepExecutionContext {
                workflow_run: details.workflow_run.clone(),
                step_run: current_step.clone(),
                thread: thread.clone(),
                artifacts: details.artifacts.clone(),
            };

            let (heartbeat_stop, heartbeat_task) = spawn_step_heartbeat(
                self.repo.clone(),
                self.state.worker_id.to_string(),
                current_step.id.clone(),
            );
            self.record_message(
                &thread.id,
                "assistant",
                format!("Running {}.", step_label(&current_step)),
                Some(json!({
                    "workflowRunId": details.workflow_run.id,
                    "stepRunId": current_step.id,
                    "stepKey": current_step.key,
                    "phase": current_step.phase,
                    "status": "running",
                })),
            )
            .await;
            let execution_result =
                execute_adapter(&self.state, &self.repo, &execution_context).await;
            stop_step_heartbeat(heartbeat_stop, heartbeat_task).await;

            match execution_result {
                StepExecutionResult::Completed { outputs, artifacts } => {
                    let persisted = match self
                        .persist_artifacts(
                            &details.workflow_run.id,
                            &thread,
                            &current_step,
                            artifacts,
                        )
                        .await
                    {
                        Ok(persisted) => persisted,
                        Err(error) => {
                            self.fail_step_after_persistence_error(
                                &details.workflow_run.id,
                                &thread,
                                &current_step,
                                outputs,
                                &error,
                            )
                            .await?;
                            continue;
                        }
                    };
                    self.repo
                        .complete_step(&current_step.id, attach_artifact_refs(outputs, &persisted))
                        .await?;
                    self.record_message(
                        &thread.id,
                        "assistant",
                        format!("Completed {}.", step_label(&current_step)),
                        Some(json!({
                            "workflowRunId": details.workflow_run.id,
                            "stepRunId": current_step.id,
                            "stepKey": current_step.key,
                            "artifactKinds": persisted
                                .iter()
                                .map(|artifact| artifact.kind.clone())
                                .collect::<Vec<_>>(),
                            "status": "completed",
                        })),
                    )
                    .await;
                }
                StepExecutionResult::WaitingExternal {
                    outputs,
                    artifacts,
                    external_ref,
                    action,
                } => {
                    let blocked_step = match self
                        .repo
                        .persist_blocked_step_result(
                            &thread.id,
                            &details.workflow_run.id,
                            &current_step.id,
                            &current_step.phase,
                            &current_step.key,
                            outputs.clone(),
                            Some(&external_ref),
                            artifacts
                                .into_iter()
                                .map(artifact_input)
                                .collect::<Vec<_>>(),
                        )
                        .await
                    {
                        Ok(blocked_step) => blocked_step,
                        Err(error) => {
                            self.fail_step_after_persistence_error(
                                &details.workflow_run.id,
                                &thread,
                                &current_step,
                                outputs,
                                &error,
                            )
                            .await?;
                            continue;
                        }
                    };
                    self.record_message(
                        &thread.id,
                        "assistant",
                        format!(
                            "{} is waiting on an external action.",
                            step_label(&current_step)
                        ),
                        Some(json!({
                            "workflowRunId": details.workflow_run.id,
                            "stepRunId": current_step.id,
                            "stepKey": current_step.key,
                            "externalRef": external_ref,
                            "status": "blocked",
                        })),
                    )
                    .await;
                    if let Err(error) = self
                        .dispatch_external_action(
                            &thread.id,
                            &details.workflow_run.id,
                            &current_step.id,
                            action,
                        )
                        .await
                    {
                        let failed_outputs = merge_output(
                            blocked_step.outputs.unwrap_or_else(|| json!({})),
                            json!({ "error": error.to_string() }),
                        );
                        self.repo
                            .fail_step(&current_step.id, failed_outputs)
                            .await?;
                        self.repo
                            .update_workflow_state(
                                &details.workflow_run.id,
                                "failed",
                                "failed",
                                Some(current_step.key.clone()),
                            )
                            .await?;
                        self.record_message(
                            &thread.id,
                            "assistant",
                            format!("{} failed: {}", step_label(&current_step), error),
                            Some(json!({
                                "workflowRunId": details.workflow_run.id,
                                "stepRunId": current_step.id,
                                "stepKey": current_step.key,
                                "status": "failed",
                            })),
                        )
                        .await;
                    }
                    return self
                        .repo
                        .find_parent_job_details(workflow_run_id)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("Workflow run disappeared: {}", workflow_run_id)
                        });
                }
                StepExecutionResult::Failed {
                    error,
                    outputs,
                    artifacts,
                } => {
                    let persisted = match self
                        .persist_artifacts(
                            &details.workflow_run.id,
                            &thread,
                            &current_step,
                            artifacts,
                        )
                        .await
                    {
                        Ok(persisted) => persisted,
                        Err(persist_error) => {
                            let failed_outputs =
                                merge_output(outputs, json!({ "error": error.clone() }));
                            self.fail_step_after_persistence_error(
                                &details.workflow_run.id,
                                &thread,
                                &current_step,
                                failed_outputs,
                                &persist_error,
                            )
                            .await?;
                            continue;
                        }
                    };
                    let failed_outputs = merge_output(
                        attach_artifact_refs(outputs, &persisted),
                        json!({ "error": error }),
                    );
                    self.repo
                        .fail_step(&current_step.id, failed_outputs)
                        .await?;
                    self.repo
                        .update_workflow_state(
                            &details.workflow_run.id,
                            "failed",
                            "failed",
                            Some(current_step.key.clone()),
                        )
                        .await?;
                    self.record_message(
                        &thread.id,
                        "assistant",
                        format!("{} failed: {}", step_label(&current_step), error),
                        Some(json!({
                            "workflowRunId": details.workflow_run.id,
                            "stepRunId": current_step.id,
                            "stepKey": current_step.key,
                            "status": "failed",
                        })),
                    )
                    .await;
                    return self
                        .repo
                        .find_parent_job_details(workflow_run_id)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("Workflow run disappeared: {}", workflow_run_id)
                        });
                }
            }
        }
    }

    async fn load_thread(&self, thread_id: &str) -> anyhow::Result<orchestration_thread::Model> {
        use sea_orm::EntityTrait;

        let db = crate::db::get_db()?;
        OrchestrationThread::find_by_id(thread_id)
            .one(db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Thread not found: {}", thread_id))
    }

    async fn begin_approval_step(
        &self,
        details: &ParentJobDetails,
        thread: &orchestration_thread::Model,
        step: &orchestration_step_run::Model,
    ) -> anyhow::Result<()> {
        if step.key == "answer_deploy_questions" {
            let question_prompt = deploy_question_prompt_from_artifacts(&details.artifacts)
                .map_err(|error| anyhow::anyhow!(error))?;

            if let Some(question_prompt) = question_prompt {
                let requested_artifact_ids = approval_requested_artifact_kinds(step)
                    .iter()
                    .filter_map(|kind| latest_artifact(&details.artifacts, kind))
                    .map(|artifact| artifact.id.clone())
                    .collect::<Vec<_>>();
                let preview_artifact = Some(ArtifactCreateInput {
                    kind: "deploy-plan-questions".to_string(),
                    format: "markdown".to_string(),
                    schema_version: 1,
                    title: Some("Deploy Planning Questions".to_string()),
                    body: Some(question_prompt.summary_markdown.clone()),
                    metadata: Some(json!({
                        "questions": question_prompt.questions,
                    })),
                });

                let Some(_) = self
                    .repo
                    .begin_approval_step(
                        &thread.id,
                        &details.workflow_run.id,
                        &step.id,
                        &step.phase,
                        &step.key,
                        "deploy_questions",
                        requested_artifact_ids,
                        preview_artifact,
                    )
                    .await?
                else {
                    return Ok(());
                };

                self.record_message(
                    &thread.id,
                    "assistant",
                    "Deploy planning needs operator answers before the plan can be revised. Reply in the approval comment with the requested guidance.".to_string(),
                    Some(json!({
                        "workflowRunId": details.workflow_run.id,
                        "stepRunId": step.id,
                        "stepKey": step.key,
                        "status": "waiting_approval",
                    })),
                )
                .await;

                return Ok(());
            }

            self.repo
                .complete_step(
                    &step.id,
                    json!({
                        "skipped": true,
                        "reason": "No deploy planning questions required",
                    }),
                )
                .await?;
            self.record_message(
                &thread.id,
                "assistant",
                "No deploy planning questions were required, so the workflow continued automatically.".to_string(),
                Some(json!({
                    "workflowRunId": details.workflow_run.id,
                    "stepRunId": step.id,
                    "stepKey": step.key,
                    "status": "completed",
                })),
            )
            .await;
            return Ok(());
        }

        let approval_kind = approval_kind_for_step(step)
            .ok_or_else(|| anyhow::anyhow!("Unsupported approval step: {}", step.key))?;
        let requested_artifact_ids = approval_requested_artifact_kinds(step)
            .iter()
            .filter_map(|kind| latest_artifact(&details.artifacts, kind))
            .map(|artifact| artifact.id.clone())
            .collect::<Vec<_>>();
        let preview_artifact = if step.key == "approve_deploy"
            && latest_artifact(&details.artifacts, "deploy-spec").is_none()
        {
            let preview = build_deploy_spec_preview(&details.workflow_run, &details.artifacts)
                .map_err(|error| anyhow::anyhow!(error))?;
            Some(ArtifactCreateInput {
                kind: "deploy-spec".to_string(),
                format: "json".to_string(),
                schema_version: 1,
                title: Some("Deploy Spec Preview".to_string()),
                body: Some(pretty_json(&preview)),
                metadata: Some(preview),
            })
        } else {
            None
        };

        let Some(_) = self
            .repo
            .begin_approval_step(
                &thread.id,
                &details.workflow_run.id,
                &step.id,
                &step.phase,
                &step.key,
                approval_kind,
                requested_artifact_ids,
                preview_artifact,
            )
            .await?
        else {
            return Ok(());
        };

        self.record_message(
            &thread.id,
            "assistant",
            format!("{} is ready for approval.", step_label(step)),
            Some(json!({
                "workflowRunId": details.workflow_run.id,
                "stepRunId": step.id,
                "stepKey": step.key,
                "status": "waiting_approval",
            })),
        )
        .await;

        Ok(())
    }

    async fn ensure_pending_approval(
        &self,
        details: &ParentJobDetails,
        step: &orchestration_step_run::Model,
    ) -> anyhow::Result<()> {
        if self
            .repo
            .find_pending_approval_for_step(&step.id)
            .await?
            .is_some()
        {
            return Ok(());
        }

        if let Some(approval) = self.repo.find_latest_approval_for_step(&step.id).await? {
            anyhow::bail!(
                "Approval step {} is waiting without a pending approval; latest approval is {}",
                step.key,
                approval.status
            );
        }

        let thread = self.load_thread(&details.workflow_run.thread_id).await?;
        let approval_kind = approval_kind_for_step(step)
            .ok_or_else(|| anyhow::anyhow!("Unsupported approval step: {}", step.key))?;
        let requested_artifact_ids = approval_requested_artifact_kinds(step)
            .iter()
            .filter_map(|kind| latest_artifact(&details.artifacts, kind))
            .map(|artifact| artifact.id.clone())
            .collect::<Vec<_>>();
        let approval = self
            .repo
            .create_pending_approval(
                &thread.id,
                &details.workflow_run.id,
                &step.id,
                approval_kind,
                requested_artifact_ids.clone(),
            )
            .await?;
        self.repo
            .set_step_waiting_approval(
                &step.id,
                json!({
                    "approvalId": approval.id,
                    "approvalKind": approval.kind,
                    "requestArtifactIds": requested_artifact_ids,
                }),
            )
            .await?;
        self.record_message(
            &thread.id,
            "assistant",
            format!("{} is ready for approval.", step_label(step)),
            Some(json!({
                "workflowRunId": details.workflow_run.id,
                "stepRunId": step.id,
                "stepKey": step.key,
                "status": "waiting_approval",
            })),
        )
        .await;
        Ok(())
    }

    async fn fail_step_after_persistence_error(
        &self,
        workflow_run_id: &str,
        thread: &orchestration_thread::Model,
        step: &orchestration_step_run::Model,
        outputs: Value,
        error: &anyhow::Error,
    ) -> anyhow::Result<()> {
        let failure_message = format!("Failed to persist step artifacts: {error}");
        let failed_outputs = merge_output(outputs, json!({ "error": failure_message }));

        self.repo
            .fail_step(&step.id, failed_outputs)
            .await
            .map_err(|mark_error| {
                anyhow::anyhow!(
                    "{}; additionally failed to mark step {} as failed: {}",
                    failure_message,
                    step.id,
                    mark_error
                )
            })?;

        if let Err(update_error) = self
            .repo
            .update_workflow_state(workflow_run_id, "failed", "failed", Some(step.key.clone()))
            .await
        {
            warn!(
                "Failed to mark workflow {} as failed after step {} persistence error: {}",
                workflow_run_id, step.id, update_error
            );
        }

        self.record_message(
            &thread.id,
            "assistant",
            format!("{} failed: {}", step_label(step), failure_message),
            Some(json!({
                "workflowRunId": workflow_run_id,
                "stepRunId": step.id,
                "stepKey": step.key,
                "status": "failed",
            })),
        )
        .await;

        Ok(())
    }

    async fn persist_artifacts(
        &self,
        workflow_run_id: &str,
        thread: &orchestration_thread::Model,
        step: &orchestration_step_run::Model,
        artifacts: Vec<ArtifactDraft>,
    ) -> anyhow::Result<Vec<orchestration_artifact::Model>> {
        let mut persisted = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            persisted.push(
                self.repo
                    .create_artifact(
                        &thread.id,
                        Some(workflow_run_id),
                        Some(&step.id),
                        &artifact.kind,
                        &artifact.format,
                        artifact.schema_version,
                        artifact.title,
                        artifact.body,
                        artifact.metadata,
                    )
                    .await?,
            );
        }
        Ok(persisted)
    }

    async fn dispatch_external_action(
        &self,
        thread_id: &str,
        workflow_run_id: &str,
        step_run_id: &str,
        action: ExternalAction,
    ) -> anyhow::Result<()> {
        match action {
            ExternalAction::CreateCloudDeployment { request } => {
                let started_at: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
                let result = cloud_client::create_deployment(&self.state, &request)
                    .await
                    .map(|_| ());
                self.record_tool_call(
                    thread_id,
                    workflow_run_id,
                    step_run_id,
                    "cloud.create_deployment",
                    json!({
                        "deploymentId": request.deployment_id,
                        "name": request.name,
                        "target": request.target,
                        "region": request.region,
                        "backendType": request.backend_type,
                        "image": request.image,
                        "ports": request.ports,
                        "envRefs": request.env_refs,
                        "startCommand": request.start_command,
                        "workingDirectory": request.working_directory,
                        "sizeClass": request.size_class,
                    }),
                    Some(match &result {
                        Ok(()) => json!({ "status": "queued" }),
                        Err(error) => json!({ "error": error.to_string() }),
                    }),
                    if result.is_ok() {
                        "completed"
                    } else {
                        "failed"
                    },
                    started_at,
                    Some(chrono::Utc::now().into()),
                )
                .await;
                result
            }
            ExternalAction::CreateCloudDeployments { requests } => {
                // Dispatch all cloud deployments concurrently. Each downstream
                // create can take minutes (image pulls, cold caches), and the
                // cloud→mvm-ctrl timeout is per-request, so doing them in
                // sequence compounds latency and trips timeouts for later
                // sandboxes while earlier ones are still pulling.
                let mut handles = Vec::with_capacity(requests.len());
                for request in requests {
                    let state = self.state.clone();
                    handles.push(tokio::spawn(async move {
                        let result = cloud_client::create_deployment(&state, &request)
                            .await
                            .map(|_| ());
                        (request, result)
                    }));
                }
                let mut dispatch_results: Vec<(
                    CreateDeploymentRequest,
                    Result<(), anyhow::Error>,
                )> = Vec::with_capacity(handles.len());
                for handle in handles {
                    match handle.await {
                        Ok(pair) => dispatch_results.push(pair),
                        Err(join_err) => {
                            return Err(anyhow::anyhow!(
                                "cloud deployment task panicked: {}",
                                join_err
                            ));
                        }
                    }
                }

                let mut first_error: Option<anyhow::Error> = None;
                let mut successful_deployment_ids = Vec::new();
                for (request, result) in dispatch_results {
                    let started_at: chrono::DateTime<chrono::FixedOffset> =
                        chrono::Utc::now().into();
                    if result.is_ok() {
                        successful_deployment_ids.push(request.deployment_id.clone());
                    }
                    self.record_tool_call(
                        thread_id,
                        workflow_run_id,
                        step_run_id,
                        "cloud.create_deployment",
                        json!({
                            "deploymentId": request.deployment_id,
                            "name": request.name,
                            "target": request.target,
                            "region": request.region,
                            "backendType": request.backend_type,
                            "image": request.image,
                            "ports": request.ports,
                            "envRefs": request.env_refs,
                            "startCommand": request.start_command,
                            "workingDirectory": request.working_directory,
                            "sizeClass": request.size_class,
                        }),
                        Some(match &result {
                            Ok(()) => json!({ "status": "queued" }),
                            Err(error) => json!({ "error": error.to_string() }),
                        }),
                        if result.is_ok() {
                            "completed"
                        } else {
                            "failed"
                        },
                        started_at,
                        Some(chrono::Utc::now().into()),
                    )
                    .await;
                    if let Err(error) = result {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
                match first_error {
                    Some(error) => Err(error),
                    None => {
                        let started_at: chrono::DateTime<chrono::FixedOffset> =
                            chrono::Utc::now().into();
                        let result = cloud_client::reconcile_deployment_hosts(
                            &self.state,
                            &successful_deployment_ids,
                        )
                        .await;
                        self.record_tool_call(
                            thread_id,
                            workflow_run_id,
                            step_run_id,
                            "cloud.reconcile_deployment_hosts",
                            json!({
                                "deploymentIds": successful_deployment_ids,
                            }),
                            Some(match &result {
                                Ok(()) => json!({ "status": "ok" }),
                                Err(error) => json!({ "error": error.to_string() }),
                            }),
                            if result.is_ok() {
                                "completed"
                            } else {
                                "failed"
                            },
                            started_at,
                            Some(chrono::Utc::now().into()),
                        )
                        .await;
                        result
                    }
                }
            }
        }
    }

    async fn record_message(
        &self,
        thread_id: &str,
        role: &str,
        text: String,
        metadata: Option<Value>,
    ) {
        if let Err(error) = self
            .repo
            .create_message(thread_id, role, json!({ "text": text }), metadata)
            .await
        {
            warn!(
                "Failed to append runtime orchestration message for thread {}: {}",
                thread_id, error
            );
        }
    }

    async fn record_tool_call(
        &self,
        thread_id: &str,
        workflow_run_id: &str,
        step_run_id: &str,
        tool_name: &str,
        input: Value,
        output: Option<Value>,
        status: &str,
        started_at: chrono::DateTime<chrono::FixedOffset>,
        completed_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    ) {
        if let Err(error) = self
            .repo
            .create_tool_call_log(crate::repositories::ToolCallLogCreateInput {
                thread_id: thread_id.to_string(),
                workflow_run_id: Some(workflow_run_id.to_string()),
                step_run_id: Some(step_run_id.to_string()),
                tool_name: tool_name.to_string(),
                input,
                output,
                status: status.to_string(),
                started_at,
                completed_at,
            })
            .await
        {
            warn!(
                "Failed to append runtime orchestration tool log for thread {}: {}",
                thread_id, error
            );
        }
    }
}

fn spawn_step_heartbeat(
    repo: OrchestrationRepository,
    worker_id: String,
    step_run_id: String,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                _ = tokio::time::sleep(Duration::from_secs(STEP_ATTEMPT_HEARTBEAT_INTERVAL_SECONDS)) => {}
            }

            match repo
                .heartbeat_step_attempt(&step_run_id, &worker_id, STEP_ATTEMPT_LEASE_SECONDS)
                .await
            {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    warn!(
                        "Failed to heartbeat orchestration step {} for worker {}: {}",
                        step_run_id, worker_id, error
                    );
                    break;
                }
            }
        }
    });

    (stop_tx, task)
}

async fn stop_step_heartbeat(stop_tx: oneshot::Sender<()>, task: JoinHandle<()>) {
    let _ = stop_tx.send(());
    if let Err(error) = task.await {
        warn!("Step heartbeat task ended unexpectedly: {}", error);
    }
}

fn dependencies_satisfied(
    step: &orchestration_step_run::Model,
    completed_keys: &HashSet<String>,
) -> bool {
    step.depends_on
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .all(|dependency| completed_keys.contains(dependency))
        })
        .unwrap_or(true)
}

fn approval_kind_for_step(step: &orchestration_step_run::Model) -> Option<&'static str> {
    match step.key.as_str() {
        "approve_plan" => Some("plan"),
        "approve_patch_application" => Some("patch"),
        "answer_deploy_questions" => Some("deploy_questions"),
        "approve_deploy" => Some("deploy"),
        _ => None,
    }
}

fn approval_requested_artifact_kinds(
    step: &orchestration_step_run::Model,
) -> &'static [&'static str] {
    match step.key.as_str() {
        "approve_plan" => &[
            "deploy-plan",
            "deploy-plan-review",
            "deploy-preflight",
            "integration-plan",
        ],
        "answer_deploy_questions" => &["deploy-plan", "deploy-plan-review"],
        "approve_patch_application" => &["patch-set"],
        "approve_deploy" => &[
            "deploy-plan",
            "integration-plan",
            "deploy-spec",
            "verification-report",
        ],
        _ => &[],
    }
}

fn step_label(step: &orchestration_step_run::Model) -> String {
    humanize_identifier(&step.key)
}

fn humanize_identifier(value: &str) -> String {
    value
        .replace(['.', '_', '-'], " ")
        .split_whitespace()
        .map(|token| {
            let mut chars = token.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn attach_artifact_refs(outputs: Value, artifacts: &[orchestration_artifact::Model]) -> Value {
    merge_output(
        outputs,
        json!({
            "artifactIds": artifacts.iter().map(|artifact| artifact.id.clone()).collect::<Vec<_>>(),
            "artifactKinds": artifacts.iter().map(|artifact| artifact.kind.clone()).collect::<Vec<_>>(),
        }),
    )
}

fn merge_output(base: Value, extra: Value) -> Value {
    let mut merged = match base {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        value => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), value);
            map
        }
    };
    if let Value::Object(extra_map) = extra {
        for (key, value) in extra_map {
            merged.insert(key, value);
        }
    }
    Value::Object(merged)
}

fn latest_artifact<'a>(
    artifacts: &'a [orchestration_artifact::Model],
    kind: &str,
) -> Option<&'a orchestration_artifact::Model> {
    artifacts
        .iter()
        .rev()
        .find(|artifact| artifact.kind == kind)
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn artifact_input(artifact: ArtifactDraft) -> ArtifactCreateInput {
    ArtifactCreateInput {
        kind: artifact.kind,
        format: artifact.format,
        schema_version: artifact.schema_version,
        title: artifact.title,
        body: artifact.body,
        metadata: artifact.metadata,
    }
}
