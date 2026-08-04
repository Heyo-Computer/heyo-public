use anyhow::Result;
use sea_orm::{
    sea_query::Expr, ActiveModelTrait, ColumnTrait, DatabaseConnection, DatabaseTransaction,
    EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait,
};
use serde_json::{json, Value};
use tracing::{error, info};
use uuid::Uuid;

use crate::entities::{
    orchestration_approval, orchestration_artifact, orchestration_external_event,
    orchestration_message, orchestration_step_attempt, orchestration_step_run,
    orchestration_thread, orchestration_tool_call_log, orchestration_workflow_run,
    orchestration_workflow_template, OrchestrationApproval as OrchestrationApprovalEntity,
    OrchestrationArtifact as OrchestrationArtifactEntity,
    OrchestrationExternalEvent as OrchestrationExternalEventEntity,
    OrchestrationMessage as OrchestrationMessageEntity,
    OrchestrationStepAttempt as OrchestrationStepAttemptEntity,
    OrchestrationStepRun as OrchestrationStepRunEntity,
    OrchestrationThread as OrchestrationThreadEntity,
    OrchestrationToolCallLog as OrchestrationToolCallLogEntity,
    OrchestrationWorkflowRun as OrchestrationWorkflowRunEntity,
    OrchestrationWorkflowTemplate as OrchestrationWorkflowTemplateEntity,
};
use crate::orchestration::{compile_plan, WorkflowTemplateDefinition};

pub struct ParentJobDetails {
    pub workflow_run: orchestration_workflow_run::Model,
    pub step_runs: Vec<orchestration_step_run::Model>,
    pub artifacts: Vec<orchestration_artifact::Model>,
    pub approvals: Vec<orchestration_approval::Model>,
}

pub struct ArtifactCreateInput {
    pub kind: String,
    pub format: String,
    pub schema_version: i32,
    pub title: Option<String>,
    pub body: Option<String>,
    pub metadata: Option<Value>,
}

pub struct ExternalEventCreateInput {
    pub external_ref: String,
    pub event_type: String,
    pub event_status: String,
    pub idempotency_key: String,
    pub message_id: Option<String>,
    pub payload: Value,
}

pub struct ToolCallLogCreateInput {
    pub thread_id: String,
    pub workflow_run_id: Option<String>,
    pub step_run_id: Option<String>,
    pub tool_name: String,
    pub input: Value,
    pub output: Option<Value>,
    pub status: String,
    pub started_at: chrono::DateTime<chrono::FixedOffset>,
    pub completed_at: Option<chrono::DateTime<chrono::FixedOffset>>,
}

pub struct ThreadTimeline {
    pub thread: orchestration_thread::Model,
    pub messages: Vec<orchestration_message::Model>,
    pub tool_call_logs: Vec<orchestration_tool_call_log::Model>,
    pub artifacts: Vec<orchestration_artifact::Model>,
}

pub enum ExternalEventConsumeOutcome {
    AlreadyHandled,
    PendingRetry,
    Processed {
        workflow_run_id: Option<String>,
        resume_workflow: bool,
    },
    Ignored,
}

#[derive(Clone)]
pub struct OrchestrationRepository {
    db: DatabaseConnection,
}

impl OrchestrationRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn create_thread(
        &self,
        created_by: &str,
        title: Option<String>,
        context: Value,
    ) -> Result<orchestration_thread::Model> {
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let active_model = orchestration_thread::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            title: Set(title),
            created_by: Set(created_by.to_string()),
            status: Set("active".to_string()),
            context: Set(context),
            created_at: Set(now),
            updated_at: Set(now),
        };

        active_model.insert(&self.db).await.map_err(|e| {
            error!("Failed to create orchestration thread: {}", e);
            anyhow::anyhow!("Database error: {}", e)
        })
    }

    pub async fn find_thread_for_user(
        &self,
        thread_id: &str,
        user_id: &str,
    ) -> Result<Option<orchestration_thread::Model>> {
        OrchestrationThreadEntity::find()
            .filter(orchestration_thread::Column::Id.eq(thread_id))
            .filter(orchestration_thread::Column::CreatedBy.eq(user_id))
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))
    }

    pub async fn find_thread_timeline_for_user(
        &self,
        thread_id: &str,
        user_id: &str,
    ) -> Result<Option<ThreadTimeline>> {
        let Some(thread) = self.find_thread_for_user(thread_id, user_id).await? else {
            return Ok(None);
        };

        let messages = OrchestrationMessageEntity::find()
            .filter(orchestration_message::Column::ThreadId.eq(thread_id))
            .order_by_asc(orchestration_message::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;

        let tool_call_logs = OrchestrationToolCallLogEntity::find()
            .filter(orchestration_tool_call_log::Column::ThreadId.eq(thread_id))
            .order_by_asc(orchestration_tool_call_log::Column::StartedAt)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;

        let artifacts = OrchestrationArtifactEntity::find()
            .filter(orchestration_artifact::Column::ThreadId.eq(thread_id))
            .order_by_asc(orchestration_artifact::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;

        Ok(Some(ThreadTimeline {
            thread,
            messages,
            tool_call_logs,
            artifacts,
        }))
    }

    pub async fn create_message(
        &self,
        thread_id: &str,
        role: &str,
        content: Value,
        metadata: Option<Value>,
    ) -> Result<orchestration_message::Model> {
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        orchestration_message::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            thread_id: Set(thread_id.to_string()),
            role: Set(role.to_string()),
            content: Set(content),
            metadata: Set(metadata),
            created_at: Set(now),
        }
        .insert(&self.db)
        .await
        .map_err(|e| anyhow::anyhow!("Database error: {}", e))
    }

    pub async fn create_tool_call_log(
        &self,
        input: ToolCallLogCreateInput,
    ) -> Result<orchestration_tool_call_log::Model> {
        orchestration_tool_call_log::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            thread_id: Set(input.thread_id),
            workflow_run_id: Set(input.workflow_run_id),
            step_run_id: Set(input.step_run_id),
            tool_name: Set(input.tool_name),
            input: Set(input.input),
            output: Set(input.output),
            status: Set(input.status),
            started_at: Set(input.started_at),
            completed_at: Set(input.completed_at),
        }
        .insert(&self.db)
        .await
        .map_err(|e| anyhow::anyhow!("Database error: {}", e))
    }

    pub async fn update_tool_call_log(
        &self,
        tool_call_log_id: &str,
        output: Option<Value>,
        status: &str,
        completed_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    ) -> Result<orchestration_tool_call_log::Model> {
        let Some(existing) = OrchestrationToolCallLogEntity::find_by_id(tool_call_log_id)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        else {
            return Err(anyhow::anyhow!(
                "Tool call log not found: {}",
                tool_call_log_id
            ));
        };

        let mut active: orchestration_tool_call_log::ActiveModel = existing.into();
        active.output = Set(output);
        active.status = Set(status.to_string());
        active.completed_at = Set(completed_at);
        active
            .update(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error: {}", e))
    }

    pub async fn ensure_template(
        &self,
        template: &WorkflowTemplateDefinition,
    ) -> Result<orchestration_workflow_template::Model> {
        if let Some(existing) = OrchestrationWorkflowTemplateEntity::find()
            .filter(orchestration_workflow_template::Column::Id.eq(&template.id))
            .filter(orchestration_workflow_template::Column::Version.eq(template.version))
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        {
            return Ok(existing);
        }

        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let active_model = orchestration_workflow_template::ActiveModel {
            id: Set(template.id.clone()),
            version: Set(template.version),
            name: Set(template.name.clone()),
            description: Set(template.description.clone()),
            input_schema: Set(serde_json::to_value(&template.input_schema)?),
            output_schema: Set(serde_json::to_value(&template.output_schema)?),
            phase_graph: Set(serde_json::to_value(&template.phase_graph)?),
            step_templates: Set(serde_json::to_value(&template.step_templates)?),
            policy: Set(serde_json::to_value(&template.policy)?),
            created_at: Set(now),
            updated_at: Set(now),
        };

        active_model.insert(&self.db).await.map_err(|e| {
            error!("Failed to insert orchestration template: {}", e);
            anyhow::anyhow!("Database error: {}", e)
        })
    }

    pub async fn create_compiled_parent_job(
        &self,
        thread: &orchestration_thread::Model,
        template: &WorkflowTemplateDefinition,
        goal: String,
        target: String,
        inputs: Value,
    ) -> Result<(
        orchestration_workflow_run::Model,
        Vec<orchestration_step_run::Model>,
    )> {
        let compiled_plan = compile_plan(template);
        let phase = template
            .phase_graph
            .first()
            .cloned()
            .unwrap_or_else(|| "discovering".to_string());
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let workflow_run_id = Uuid::new_v4().to_string();

        let txn = self.db.begin().await?;

        let workflow_run = orchestration_workflow_run::ActiveModel {
            id: Set(workflow_run_id.clone()),
            thread_id: Set(thread.id.clone()),
            template_id: Set(template.id.clone()),
            template_version: Set(template.version),
            goal: Set(goal),
            status: Set("compiled".to_string()),
            phase: Set(phase),
            target: Set(target),
            inputs: Set(inputs),
            compiled_plan: Set(compiled_plan),
            current_child_job_key: Set(None),
            started_at: Set(None),
            completed_at: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database error: {}", e))?;

        let mut step_runs = Vec::with_capacity(template.step_templates.len());

        for step in &template.step_templates {
            let status = if step.depends_on.is_empty() {
                "ready"
            } else {
                "pending"
            };

            let active_step = orchestration_step_run::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                workflow_run_id: Set(workflow_run_id.clone()),
                key: Set(step.key.clone()),
                r#type: Set(step.r#type.clone()),
                kind: Set(step.kind.clone()),
                phase: Set(step.phase.clone()),
                status: Set(status.to_string()),
                adapter: Set(step.adapter.clone()),
                depends_on: Set(serde_json::to_value(&step.depends_on)?),
                can_fan_out: Set(step.can_fan_out),
                requires_approval_before_start: Set(step.requires_approval_before_start),
                inputs: Set(json!({})),
                outputs: Set(None),
                artifact_contract: Set(match &step.artifact_contract {
                    Some(contract) => Some(serde_json::to_value(contract)?),
                    None => None,
                }),
                retry_count: Set(0),
                max_retries: Set(step.retry_policy.as_ref().map_or(0, |p| p.max_retries)),
                idempotency_key: Set(Some(format!(
                    "parent-job:{}:child-job:{}",
                    workflow_run_id, step.key
                ))),
                external_ref: Set(None),
                started_at: Set(None),
                completed_at: Set(None),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database error: {}", e))?;

            step_runs.push(active_step);
        }

        txn.commit().await?;

        info!(
            "Compiled parent job {} for thread {} with {} child jobs",
            workflow_run.id,
            thread.id,
            step_runs.len()
        );

        Ok((workflow_run, step_runs))
    }

    pub async fn find_parent_job_for_user(
        &self,
        workflow_run_id: &str,
        user_id: &str,
    ) -> Result<(
        Option<orchestration_workflow_run::Model>,
        Vec<orchestration_step_run::Model>,
    )> {
        let Some(workflow_run) = OrchestrationWorkflowRunEntity::find_by_id(workflow_run_id)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        else {
            return Ok((None, vec![]));
        };

        let Some(_) = self
            .find_thread_for_user(&workflow_run.thread_id, user_id)
            .await?
        else {
            return Ok((None, vec![]));
        };

        let step_runs = OrchestrationStepRunEntity::find()
            .filter(orchestration_step_run::Column::WorkflowRunId.eq(workflow_run_id))
            .order_by_asc(orchestration_step_run::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;

        Ok((Some(workflow_run), step_runs))
    }

    pub async fn find_parent_job_details_for_user(
        &self,
        workflow_run_id: &str,
        user_id: &str,
    ) -> Result<Option<ParentJobDetails>> {
        let Some(workflow_run) = OrchestrationWorkflowRunEntity::find_by_id(workflow_run_id)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        else {
            return Ok(None);
        };

        let Some(_) = self
            .find_thread_for_user(&workflow_run.thread_id, user_id)
            .await?
        else {
            return Ok(None);
        };

        self.load_parent_job_details(workflow_run).await.map(Some)
    }

    pub async fn find_parent_job_details(
        &self,
        workflow_run_id: &str,
    ) -> Result<Option<ParentJobDetails>> {
        let Some(workflow_run) = OrchestrationWorkflowRunEntity::find_by_id(workflow_run_id)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        else {
            return Ok(None);
        };

        self.load_parent_job_details(workflow_run).await.map(Some)
    }

    async fn load_parent_job_details(
        &self,
        workflow_run: orchestration_workflow_run::Model,
    ) -> Result<ParentJobDetails> {
        let step_runs = OrchestrationStepRunEntity::find()
            .filter(orchestration_step_run::Column::WorkflowRunId.eq(&workflow_run.id))
            .order_by_asc(orchestration_step_run::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;

        let artifacts = OrchestrationArtifactEntity::find()
            .filter(orchestration_artifact::Column::WorkflowRunId.eq(&workflow_run.id))
            .order_by_asc(orchestration_artifact::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;

        let approvals = OrchestrationApprovalEntity::find()
            .filter(orchestration_approval::Column::WorkflowRunId.eq(&workflow_run.id))
            .order_by_asc(orchestration_approval::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;

        Ok(ParentJobDetails {
            workflow_run,
            step_runs,
            artifacts,
            approvals,
        })
    }

    pub async fn find_workflow_run(
        &self,
        workflow_run_id: &str,
    ) -> Result<Option<orchestration_workflow_run::Model>> {
        OrchestrationWorkflowRunEntity::find_by_id(workflow_run_id)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))
    }

    pub async fn list_workflow_runs_for_user(
        &self,
        user_id: &str,
        limit: u64,
    ) -> Result<Vec<orchestration_workflow_run::Model>> {
        let thread_ids = OrchestrationThreadEntity::find()
            .filter(orchestration_thread::Column::CreatedBy.eq(user_id))
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .into_iter()
            .map(|t| t.id)
            .collect::<Vec<_>>();

        if thread_ids.is_empty() {
            return Ok(vec![]);
        }

        OrchestrationWorkflowRunEntity::find()
            .filter(orchestration_workflow_run::Column::ThreadId.is_in(thread_ids))
            .order_by_desc(orchestration_workflow_run::Column::CreatedAt)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))
    }

    pub async fn list_artifacts_by_kind_for_workflow(
        &self,
        workflow_run_id: &str,
        kind: &str,
    ) -> Result<Vec<orchestration_artifact::Model>> {
        OrchestrationArtifactEntity::find()
            .filter(orchestration_artifact::Column::WorkflowRunId.eq(workflow_run_id))
            .filter(orchestration_artifact::Column::Kind.eq(kind))
            .order_by_desc(orchestration_artifact::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))
    }

    pub async fn find_step_run(
        &self,
        step_run_id: &str,
    ) -> Result<Option<orchestration_step_run::Model>> {
        OrchestrationStepRunEntity::find_by_id(step_run_id)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))
    }

    pub async fn try_mark_step_ready(&self, step_run_id: &str) -> Result<bool> {
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let result = OrchestrationStepRunEntity::update_many()
            .col_expr(
                orchestration_step_run::Column::Status,
                Expr::value("ready".to_string()),
            )
            .col_expr(orchestration_step_run::Column::UpdatedAt, Expr::value(now))
            .filter(orchestration_step_run::Column::Id.eq(step_run_id))
            .filter(orchestration_step_run::Column::Status.eq("pending"))
            .exec(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;

        Ok(result.rows_affected == 1)
    }

    pub async fn try_claim_step(
        &self,
        step_run_id: &str,
        worker_id: &str,
        lease_seconds: i64,
    ) -> Result<bool> {
        let txn = self.db.begin().await?;
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let Some(step) = OrchestrationStepRunEntity::find_by_id(step_run_id)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        else {
            txn.rollback().await?;
            return Ok(false);
        };
        if step.status != "ready" {
            txn.rollback().await?;
            return Ok(false);
        }

        let attempt_number = OrchestrationStepAttemptEntity::find()
            .filter(orchestration_step_attempt::Column::StepRunId.eq(step_run_id))
            .order_by_desc(orchestration_step_attempt::Column::AttemptNumber)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .map(|attempt| attempt.attempt_number + 1)
            .unwrap_or(1);

        orchestration_step_attempt::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            workflow_run_id: Set(step.workflow_run_id.clone()),
            step_run_id: Set(step.id.clone()),
            attempt_number: Set(attempt_number),
            worker_id: Set(worker_id.to_string()),
            status: Set("running".to_string()),
            lease_expires_at: Set(
                (chrono::Utc::now() + chrono::Duration::seconds(lease_seconds)).into(),
            ),
            heartbeat_at: Set(Some(now)),
            failure_reason: Set(None),
            started_at: Set(now),
            completed_at: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database error: {}", e))?;

        let mut active_step: orchestration_step_run::ActiveModel = step.into();
        active_step.status = Set("running".to_string());
        active_step.started_at = Set(Some(now));
        active_step.updated_at = Set(now);
        active_step
            .update(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;

        txn.commit().await?;
        Ok(true)
    }

    pub async fn has_live_attempt(&self, step_run_id: &str) -> Result<bool> {
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let attempt = OrchestrationStepAttemptEntity::find()
            .filter(orchestration_step_attempt::Column::StepRunId.eq(step_run_id))
            .filter(orchestration_step_attempt::Column::Status.eq("running"))
            .filter(orchestration_step_attempt::Column::LeaseExpiresAt.gte(now))
            .order_by_desc(orchestration_step_attempt::Column::AttemptNumber)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;

        Ok(attempt.is_some())
    }

    pub async fn heartbeat_step_attempt(
        &self,
        step_run_id: &str,
        worker_id: &str,
        lease_seconds: i64,
    ) -> Result<bool> {
        let txn = self.db.begin().await?;
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let Some(attempt) = OrchestrationStepAttemptEntity::find()
            .filter(orchestration_step_attempt::Column::StepRunId.eq(step_run_id))
            .filter(orchestration_step_attempt::Column::WorkerId.eq(worker_id))
            .filter(orchestration_step_attempt::Column::Status.eq("running"))
            .order_by_desc(orchestration_step_attempt::Column::AttemptNumber)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        else {
            txn.rollback().await?;
            return Ok(false);
        };

        let mut active_attempt: orchestration_step_attempt::ActiveModel = attempt.into();
        active_attempt.lease_expires_at =
            Set((chrono::Utc::now() + chrono::Duration::seconds(lease_seconds)).into());
        active_attempt.heartbeat_at = Set(Some(now));
        active_attempt.updated_at = Set(now);
        active_attempt
            .update(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;

        let step = OrchestrationStepRunEntity::find_by_id(step_run_id)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;
        let Some(step) = step else {
            txn.rollback().await?;
            return Ok(false);
        };

        if step.status != "running" {
            txn.rollback().await?;
            return Ok(false);
        }

        let mut active_step: orchestration_step_run::ActiveModel = step.into();
        active_step.updated_at = Set(now);
        active_step
            .update(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;

        txn.commit().await?;
        Ok(true)
    }

    pub async fn set_step_waiting_approval(
        &self,
        step_run_id: &str,
        outputs: Value,
    ) -> Result<orchestration_step_run::Model> {
        let step = OrchestrationStepRunEntity::find_by_id(step_run_id)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("Step run not found: {}", step_run_id))?;

        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let started_at = step.started_at;
        let mut active: orchestration_step_run::ActiveModel = step.into();
        active.status = Set("waiting_approval".to_string());
        active.outputs = Set(Some(outputs));
        if started_at.is_none() {
            active.started_at = Set(Some(now));
        }
        active.updated_at = Set(now);
        active
            .update(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))
    }

    pub async fn begin_approval_step(
        &self,
        thread_id: &str,
        workflow_run_id: &str,
        step_run_id: &str,
        phase: &str,
        step_key: &str,
        kind: &str,
        request_artifact_ids: Vec<String>,
        preview_artifact: Option<ArtifactCreateInput>,
    ) -> Result<Option<orchestration_approval::Model>> {
        let txn = self.db.begin().await?;
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let mut request_artifact_ids = request_artifact_ids;

        if let Some(preview_artifact) = preview_artifact {
            let preview = insert_artifact(
                &txn,
                thread_id,
                Some(workflow_run_id),
                Some(step_run_id),
                preview_artifact,
            )
            .await?;
            request_artifact_ids.push(preview.id.clone());
        }

        let approval_id = Uuid::new_v4().to_string();
        let step_outputs = json!({
            "approvalId": approval_id,
            "approvalKind": kind,
            "requestArtifactIds": request_artifact_ids,
        });

        let result = OrchestrationStepRunEntity::update_many()
            .col_expr(
                orchestration_step_run::Column::Status,
                Expr::value("waiting_approval".to_string()),
            )
            .col_expr(
                orchestration_step_run::Column::Outputs,
                Expr::value(step_outputs),
            )
            .col_expr(
                orchestration_step_run::Column::StartedAt,
                Expr::value(Some(now)),
            )
            .col_expr(orchestration_step_run::Column::UpdatedAt, Expr::value(now))
            .filter(orchestration_step_run::Column::Id.eq(step_run_id))
            .filter(orchestration_step_run::Column::Status.eq("ready"))
            .exec(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;

        if result.rows_affected != 1 {
            txn.rollback().await?;
            return Ok(None);
        }

        let approval = orchestration_approval::ActiveModel {
            id: Set(approval_id),
            thread_id: Set(thread_id.to_string()),
            workflow_run_id: Set(workflow_run_id.to_string()),
            step_run_id: Set(step_run_id.to_string()),
            kind: Set(kind.to_string()),
            status: Set("pending".to_string()),
            request_artifact_ids: Set(json!(request_artifact_ids)),
            decided_by: Set(None),
            response_comment: Set(None),
            decided_at: Set(None),
            created_at: Set(now),
        }
        .insert(&txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database error: {}", e))?;

        update_workflow_state_txn(
            &txn,
            workflow_run_id,
            "waiting_approval",
            phase,
            Some(step_key.to_string()),
        )
        .await?;

        txn.commit().await?;
        Ok(Some(approval))
    }

    pub async fn persist_blocked_step_result(
        &self,
        thread_id: &str,
        workflow_run_id: &str,
        step_run_id: &str,
        phase: &str,
        step_key: &str,
        outputs: Value,
        external_ref: Option<&str>,
        artifacts: Vec<ArtifactCreateInput>,
    ) -> Result<orchestration_step_run::Model> {
        let txn = self.db.begin().await?;
        let mut persisted_artifacts = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            persisted_artifacts.push(
                insert_artifact(
                    &txn,
                    thread_id,
                    Some(workflow_run_id),
                    Some(step_run_id),
                    artifact,
                )
                .await?,
            );
        }

        let merged_outputs = attach_artifact_refs(outputs, &persisted_artifacts);
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let result = OrchestrationStepRunEntity::update_many()
            .col_expr(
                orchestration_step_run::Column::Status,
                Expr::value("blocked".to_string()),
            )
            .col_expr(
                orchestration_step_run::Column::Outputs,
                Expr::value(merged_outputs),
            )
            .col_expr(
                orchestration_step_run::Column::ExternalRef,
                Expr::value(external_ref.map(ToOwned::to_owned)),
            )
            .col_expr(orchestration_step_run::Column::UpdatedAt, Expr::value(now))
            .filter(orchestration_step_run::Column::Id.eq(step_run_id))
            .filter(orchestration_step_run::Column::Status.eq("running"))
            .exec(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;

        if result.rows_affected != 1 {
            txn.rollback().await?;
            return Err(anyhow::anyhow!(
                "Step {} was not running when persisting an external wait",
                step_run_id
            ));
        }

        close_active_attempt_txn(&txn, step_run_id, "blocked", None).await?;

        update_workflow_state_txn(
            &txn,
            workflow_run_id,
            "running",
            phase,
            Some(step_key.to_string()),
        )
        .await?;

        let step = OrchestrationStepRunEntity::find_by_id(step_run_id)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("Step run not found: {}", step_run_id))?;

        txn.commit().await?;
        Ok(step)
    }

    pub async fn block_step(
        &self,
        step_run_id: &str,
        outputs: Value,
    ) -> Result<orchestration_step_run::Model> {
        self.update_step_terminalish(step_run_id, "blocked", outputs, false)
            .await
    }

    pub async fn complete_step(
        &self,
        step_run_id: &str,
        outputs: Value,
    ) -> Result<orchestration_step_run::Model> {
        self.update_step_terminalish(step_run_id, "completed", outputs, true)
            .await
    }

    pub async fn fail_step(
        &self,
        step_run_id: &str,
        outputs: Value,
    ) -> Result<orchestration_step_run::Model> {
        self.update_step_terminalish(step_run_id, "failed", outputs, true)
            .await
    }

    async fn update_step_terminalish(
        &self,
        step_run_id: &str,
        status: &str,
        outputs: Value,
        set_completed_at: bool,
    ) -> Result<orchestration_step_run::Model> {
        let txn = self.db.begin().await?;
        let updated = update_step_status_txn(
            &txn,
            step_run_id,
            status,
            outputs,
            set_completed_at,
            Some(status),
            if status == "failed" {
                Some("step failed".to_string())
            } else {
                None
            },
        )
        .await?;
        txn.commit().await?;
        Ok(updated)
    }

    pub async fn update_workflow_state(
        &self,
        workflow_run_id: &str,
        status: &str,
        phase: &str,
        current_child_job_key: Option<String>,
    ) -> Result<orchestration_workflow_run::Model> {
        let workflow = OrchestrationWorkflowRunEntity::find_by_id(workflow_run_id)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("Workflow run not found: {}", workflow_run_id))?;

        let started_at = workflow.started_at;
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let mut active: orchestration_workflow_run::ActiveModel = workflow.into();
        active.status = Set(status.to_string());
        active.phase = Set(phase.to_string());
        active.current_child_job_key = Set(current_child_job_key);
        if started_at.is_none() {
            active.started_at = Set(Some(now));
        }
        if matches!(status, "completed" | "failed" | "cancelled") {
            active.completed_at = Set(Some(now));
        }
        active.updated_at = Set(now);
        active
            .update(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))
    }

    pub async fn create_artifact(
        &self,
        thread_id: &str,
        workflow_run_id: Option<&str>,
        step_run_id: Option<&str>,
        kind: &str,
        format: &str,
        schema_version: i32,
        title: Option<String>,
        body: Option<String>,
        metadata: Option<Value>,
    ) -> Result<orchestration_artifact::Model> {
        let active_model = orchestration_artifact::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            thread_id: Set(thread_id.to_string()),
            workflow_run_id: Set(workflow_run_id.map(ToOwned::to_owned)),
            step_run_id: Set(step_run_id.map(ToOwned::to_owned)),
            kind: Set(kind.to_string()),
            format: Set(format.to_string()),
            schema_version: Set(schema_version),
            title: Set(title),
            uri: Set(None),
            body: Set(body),
            metadata: Set(metadata),
            created_at: Set(chrono::Utc::now().into()),
        };

        active_model.insert(&self.db).await.map_err(|e| {
            error!("Failed to create orchestration artifact: {}", e);
            anyhow::anyhow!("Database error: {}", e)
        })
    }

    pub async fn ingest_external_event(
        &self,
        input: ExternalEventCreateInput,
    ) -> Result<orchestration_external_event::Model> {
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let active_model = orchestration_external_event::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            workflow_run_id: Set(None),
            step_run_id: Set(None),
            external_ref: Set(input.external_ref.clone()),
            event_type: Set(input.event_type.clone()),
            event_status: Set(input.event_status.clone()),
            idempotency_key: Set(input.idempotency_key.clone()),
            message_id: Set(input.message_id),
            payload: Set(input.payload),
            processing_status: Set("pending".to_string()),
            processing_error: Set(None),
            received_at: Set(now),
            processed_at: Set(None),
            updated_at: Set(now),
        };

        match active_model.insert(&self.db).await {
            Ok(event) => Ok(event),
            Err(e) => {
                if let Some(existing) = self
                    .find_external_event_by_idempotency(
                        &input.external_ref,
                        &input.event_type,
                        &input.idempotency_key,
                    )
                    .await?
                {
                    return Ok(existing);
                }
                error!("Failed to create orchestration external event: {}", e);
                Err(anyhow::anyhow!("Database error: {}", e))
            }
        }
    }

    pub async fn list_pending_external_event_ids(&self, limit: u64) -> Result<Vec<String>> {
        Ok(OrchestrationExternalEventEntity::find()
            .filter(orchestration_external_event::Column::ProcessingStatus.eq("pending"))
            .order_by_asc(orchestration_external_event::Column::ReceivedAt)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .into_iter()
            .map(|event| event.id)
            .collect())
    }

    pub async fn consume_pending_external_event(
        &self,
        event_id: &str,
    ) -> Result<ExternalEventConsumeOutcome> {
        let txn = self.db.begin().await?;
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();

        let Some(event) = OrchestrationExternalEventEntity::find_by_id(event_id)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        else {
            txn.rollback().await?;
            return Ok(ExternalEventConsumeOutcome::AlreadyHandled);
        };
        if event.processing_status == "processed" || event.processing_status == "ignored" {
            txn.rollback().await?;
            return Ok(ExternalEventConsumeOutcome::AlreadyHandled);
        }
        if event.processing_status != "pending" {
            txn.rollback().await?;
            return Ok(ExternalEventConsumeOutcome::PendingRetry);
        }

        let claim = OrchestrationExternalEventEntity::update_many()
            .col_expr(
                orchestration_external_event::Column::ProcessingStatus,
                Expr::value("processing".to_string()),
            )
            .col_expr(
                orchestration_external_event::Column::ProcessingError,
                Expr::value(None::<String>),
            )
            .col_expr(
                orchestration_external_event::Column::UpdatedAt,
                Expr::value(now),
            )
            .filter(orchestration_external_event::Column::Id.eq(event_id))
            .filter(orchestration_external_event::Column::ProcessingStatus.eq("pending"))
            .exec(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;
        if claim.rows_affected != 1 {
            txn.rollback().await?;
            return Ok(ExternalEventConsumeOutcome::PendingRetry);
        }

        let Some(step) = find_deploy_step_for_external_ref_txn(&txn, &event.external_ref).await?
        else {
            update_external_event_state_txn(
                &txn,
                &event,
                None,
                None,
                "pending",
                Some("No matching deploy step is ready for this external event yet".to_string()),
                false,
            )
            .await?;
            txn.commit().await?;
            return Ok(ExternalEventConsumeOutcome::PendingRetry);
        };

        let workflow_run = OrchestrationWorkflowRunEntity::find_by_id(&step.workflow_run_id)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;
        let Some(workflow_run) = workflow_run else {
            update_external_event_state_txn(
                &txn,
                &event,
                None,
                Some(step.id.clone()),
                "ignored",
                Some("Workflow run for external event step no longer exists".to_string()),
                true,
            )
            .await?;
            txn.commit().await?;
            return Ok(ExternalEventConsumeOutcome::Ignored);
        };

        let step_status = step.status.clone();
        if !matches!(step_status.as_str(), "blocked" | "running") {
            update_external_event_state_txn(
                &txn,
                &event,
                Some(workflow_run.id.clone()),
                Some(step.id.clone()),
                "ignored",
                Some(format!(
                    "Deploy step {} is already {}",
                    step.id, step.status
                )),
                true,
            )
            .await?;
            txn.commit().await?;
            return Ok(ExternalEventConsumeOutcome::Ignored);
        }

        let merged_outputs =
            merge_deploy_event_outputs(step.outputs.clone().unwrap_or_else(|| json!({})), &event);
        let mut resume_workflow = false;
        match event.event_status.as_str() {
            "provisioning" => {
                update_step_status_txn(
                    &txn,
                    &step.id,
                    &step.status,
                    merged_outputs,
                    false,
                    None,
                    None,
                )
                .await?;
            }
            "running" => {
                if all_tracked_deployments_running(&merged_outputs) {
                    update_step_status_txn(
                        &txn,
                        &step.id,
                        "completed",
                        merged_outputs,
                        true,
                        Some("completed"),
                        None,
                    )
                    .await?;
                    update_workflow_state_txn(
                        &txn,
                        &workflow_run.id,
                        "running",
                        &step.phase,
                        Some(step.key.clone()),
                    )
                    .await?;
                    resume_workflow = true;
                } else {
                    update_step_status_txn(
                        &txn,
                        &step.id,
                        &step.status,
                        merged_outputs,
                        false,
                        None,
                        None,
                    )
                    .await?;
                }
            }
            "failed" => {
                let failed_sandbox_key =
                    deployment_status_entry(&merged_outputs, &event.external_ref)
                        .and_then(|entry| entry.get("sandboxKey"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                let report_value = json!({
                    "status": "failed",
                    "deploymentId": event.external_ref,
                    "sandboxKey": failed_sandbox_key,
                    "error": event.payload.get("error").cloned(),
                    "eventType": event.event_type,
                });
                insert_artifact(
                    &txn,
                    &workflow_run.thread_id,
                    Some(&workflow_run.id),
                    Some(&step.id),
                    ArtifactCreateInput {
                        kind: "deploy-report".to_string(),
                        format: "json".to_string(),
                        schema_version: 1,
                        title: Some("Deploy Report".to_string()),
                        body: Some(pretty_json(&report_value)),
                        metadata: Some(report_value),
                    },
                )
                .await?;
                update_step_status_txn(
                    &txn,
                    &step.id,
                    "failed",
                    merged_outputs,
                    true,
                    Some("failed"),
                    event
                        .payload
                        .get("error")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .or_else(|| Some("deployment lifecycle reported failure".to_string())),
                )
                .await?;
                update_workflow_state_txn(
                    &txn,
                    &workflow_run.id,
                    "failed",
                    "failed",
                    Some(step.key.clone()),
                )
                .await?;
            }
            _ => {
                update_external_event_state_txn(
                    &txn,
                    &event,
                    Some(workflow_run.id.clone()),
                    Some(step.id.clone()),
                    "ignored",
                    Some(format!(
                        "Unsupported deploy lifecycle status {}",
                        event.event_status
                    )),
                    true,
                )
                .await?;
                txn.commit().await?;
                return Ok(ExternalEventConsumeOutcome::Ignored);
            }
        }

        update_external_event_state_txn(
            &txn,
            &event,
            Some(workflow_run.id.clone()),
            Some(step.id.clone()),
            "processed",
            None,
            true,
        )
        .await?;

        txn.commit().await?;
        Ok(ExternalEventConsumeOutcome::Processed {
            workflow_run_id: Some(workflow_run.id),
            resume_workflow,
        })
    }

    pub async fn list_running_step_ids(&self, limit: u64) -> Result<Vec<String>> {
        Ok(OrchestrationStepRunEntity::find()
            .filter(orchestration_step_run::Column::Status.eq("running"))
            .order_by_asc(orchestration_step_run::Column::UpdatedAt)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .into_iter()
            .map(|step| step.id)
            .collect())
    }

    pub async fn fail_stale_running_step(
        &self,
        step_run_id: &str,
        failure_reason: &str,
    ) -> Result<Option<String>> {
        let txn = self.db.begin().await?;
        let Some(step) = OrchestrationStepRunEntity::find_by_id(step_run_id)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        else {
            txn.rollback().await?;
            return Ok(None);
        };
        if step.status != "running" {
            txn.rollback().await?;
            return Ok(None);
        }

        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let active_attempt = OrchestrationStepAttemptEntity::find()
            .filter(orchestration_step_attempt::Column::StepRunId.eq(step_run_id))
            .filter(orchestration_step_attempt::Column::Status.eq("running"))
            .order_by_desc(orchestration_step_attempt::Column::AttemptNumber)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;
        if active_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.lease_expires_at >= now)
        {
            txn.rollback().await?;
            return Ok(None);
        }

        let failed_outputs = merge_output(
            step.outputs.clone().unwrap_or_else(|| json!({})),
            json!({ "error": failure_reason }),
        );
        update_step_status_txn(
            &txn,
            &step.id,
            "failed",
            failed_outputs,
            true,
            Some("abandoned"),
            Some(failure_reason.to_string()),
        )
        .await?;
        update_workflow_state_txn(
            &txn,
            &step.workflow_run_id,
            "failed",
            "failed",
            Some(step.key.clone()),
        )
        .await?;
        txn.commit().await?;
        Ok(Some(step.workflow_run_id))
    }

    pub async fn find_latest_artifact_for_workflow(
        &self,
        workflow_run_id: &str,
        kind: &str,
    ) -> Result<Option<orchestration_artifact::Model>> {
        OrchestrationArtifactEntity::find()
            .filter(orchestration_artifact::Column::WorkflowRunId.eq(workflow_run_id))
            .filter(orchestration_artifact::Column::Kind.eq(kind))
            .order_by_desc(orchestration_artifact::Column::CreatedAt)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))
    }

    pub async fn list_artifacts_for_workflow(
        &self,
        workflow_run_id: &str,
    ) -> Result<Vec<orchestration_artifact::Model>> {
        OrchestrationArtifactEntity::find()
            .filter(orchestration_artifact::Column::WorkflowRunId.eq(workflow_run_id))
            .order_by_asc(orchestration_artifact::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))
    }

    pub async fn find_pending_approval_for_step(
        &self,
        step_run_id: &str,
    ) -> Result<Option<orchestration_approval::Model>> {
        OrchestrationApprovalEntity::find()
            .filter(orchestration_approval::Column::StepRunId.eq(step_run_id))
            .filter(orchestration_approval::Column::Status.eq("pending"))
            .order_by_desc(orchestration_approval::Column::CreatedAt)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))
    }

    pub async fn find_latest_approval_for_step(
        &self,
        step_run_id: &str,
    ) -> Result<Option<orchestration_approval::Model>> {
        OrchestrationApprovalEntity::find()
            .filter(orchestration_approval::Column::StepRunId.eq(step_run_id))
            .order_by_desc(orchestration_approval::Column::CreatedAt)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))
    }

    pub async fn create_pending_approval(
        &self,
        thread_id: &str,
        workflow_run_id: &str,
        step_run_id: &str,
        kind: &str,
        request_artifact_ids: Vec<String>,
    ) -> Result<orchestration_approval::Model> {
        if let Some(existing) = self.find_pending_approval_for_step(step_run_id).await? {
            return Ok(existing);
        }

        let active_model = orchestration_approval::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            thread_id: Set(thread_id.to_string()),
            workflow_run_id: Set(workflow_run_id.to_string()),
            step_run_id: Set(step_run_id.to_string()),
            kind: Set(kind.to_string()),
            status: Set("pending".to_string()),
            request_artifact_ids: Set(json!(request_artifact_ids)),
            decided_by: Set(None),
            response_comment: Set(None),
            decided_at: Set(None),
            created_at: Set(chrono::Utc::now().into()),
        };

        match active_model.insert(&self.db).await {
            Ok(approval) => Ok(approval),
            Err(e) => {
                if let Some(existing) = self.find_pending_approval_for_step(step_run_id).await? {
                    return Ok(existing);
                }
                error!("Failed to create orchestration approval: {}", e);
                Err(anyhow::anyhow!("Database error: {}", e))
            }
        }
    }

    pub async fn find_approval_for_user(
        &self,
        approval_id: &str,
        user_id: &str,
    ) -> Result<Option<orchestration_approval::Model>> {
        let Some(approval) = OrchestrationApprovalEntity::find_by_id(approval_id)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        else {
            return Ok(None);
        };

        let Some(workflow) = self.find_workflow_run(&approval.workflow_run_id).await? else {
            return Ok(None);
        };

        let Some(_) = self
            .find_thread_for_user(&workflow.thread_id, user_id)
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(approval))
    }

    pub async fn decide_approval(
        &self,
        approval_id: &str,
        decided_by: &str,
        status: &str,
        response_comment: Option<String>,
        step_status: &str,
        workflow_status: &str,
        workflow_phase: &str,
        step_output_patch: Value,
    ) -> Result<Option<orchestration_approval::Model>> {
        let txn = self.db.begin().await?;
        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let approval = OrchestrationApprovalEntity::find_by_id(approval_id)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("Approval not found: {}", approval_id))?;

        if approval.status != "pending" {
            txn.rollback().await?;
            return Ok(None);
        }

        let step = OrchestrationStepRunEntity::find_by_id(&approval.step_run_id)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("Step run not found: {}", approval.step_run_id))?;

        if step.status != "waiting_approval" {
            txn.rollback().await?;
            return Err(anyhow::anyhow!(
                "Approval step {} is not waiting for approval",
                step.id
            ));
        }

        let approval_update = OrchestrationApprovalEntity::update_many()
            .col_expr(
                orchestration_approval::Column::Status,
                Expr::value(status.to_string()),
            )
            .col_expr(
                orchestration_approval::Column::DecidedBy,
                Expr::value(Some(decided_by.to_string())),
            )
            .col_expr(
                orchestration_approval::Column::ResponseComment,
                Expr::value(response_comment.clone()),
            )
            .col_expr(
                orchestration_approval::Column::DecidedAt,
                Expr::value(Some(now)),
            )
            .filter(orchestration_approval::Column::Id.eq(approval_id))
            .filter(orchestration_approval::Column::Status.eq("pending"))
            .exec(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;

        if approval_update.rows_affected != 1 {
            txn.rollback().await?;
            return Ok(None);
        }

        let existing_outputs = step.outputs.clone().unwrap_or_else(|| json!({}));
        let mut step_update = OrchestrationStepRunEntity::update_many()
            .col_expr(
                orchestration_step_run::Column::Status,
                Expr::value(step_status.to_string()),
            )
            .col_expr(
                orchestration_step_run::Column::Outputs,
                Expr::value(merge_output(existing_outputs, step_output_patch)),
            )
            .col_expr(orchestration_step_run::Column::UpdatedAt, Expr::value(now))
            .filter(orchestration_step_run::Column::Id.eq(&approval.step_run_id))
            .filter(orchestration_step_run::Column::Status.eq("waiting_approval"));
        if matches!(step_status, "completed" | "failed" | "cancelled") {
            step_update = step_update.col_expr(
                orchestration_step_run::Column::CompletedAt,
                Expr::value(Some(now)),
            );
        }
        let step_update = step_update
            .exec(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;

        if step_update.rows_affected != 1 {
            txn.rollback().await?;
            return Err(anyhow::anyhow!(
                "Approval step {} could not be finalized from waiting_approval",
                approval.step_run_id
            ));
        }

        let approval = OrchestrationApprovalEntity::find_by_id(approval_id)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("Approval not found after update: {}", approval_id))?;
        let step = OrchestrationStepRunEntity::find_by_id(&approval.step_run_id)
            .one(&txn)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
            .ok_or_else(|| {
                anyhow::anyhow!("Step run not found after update: {}", approval.step_run_id)
            })?;

        update_workflow_state_txn(
            &txn,
            &approval.workflow_run_id,
            workflow_status,
            workflow_phase,
            Some(step.key.clone()),
        )
        .await?;

        txn.commit().await?;
        Ok(Some(approval))
    }

    pub async fn find_deploy_step_for_deployment(
        &self,
        deployment_id: &str,
    ) -> Result<
        Option<(
            orchestration_workflow_run::Model,
            orchestration_step_run::Model,
        )>,
    > {
        let step = OrchestrationStepRunEntity::find()
            .filter(orchestration_step_run::Column::Adapter.eq("heyo.deploy"))
            .filter(orchestration_step_run::Column::ExternalRef.eq(deployment_id))
            .filter(orchestration_step_run::Column::Status.is_in(["running", "blocked"]))
            .order_by_desc(orchestration_step_run::Column::CreatedAt)
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;

        let Some(step) = step else {
            return Ok(None);
        };

        let Some(workflow_run) = self.find_workflow_run(&step.workflow_run_id).await? else {
            return Ok(None);
        };

        Ok(Some((workflow_run, step)))
    }

    async fn find_external_event_by_idempotency(
        &self,
        external_ref: &str,
        event_type: &str,
        idempotency_key: &str,
    ) -> Result<Option<orchestration_external_event::Model>> {
        OrchestrationExternalEventEntity::find()
            .filter(orchestration_external_event::Column::ExternalRef.eq(external_ref))
            .filter(orchestration_external_event::Column::EventType.eq(event_type))
            .filter(orchestration_external_event::Column::IdempotencyKey.eq(idempotency_key))
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database query error: {}", e))
    }
}

async fn insert_artifact(
    txn: &DatabaseTransaction,
    thread_id: &str,
    workflow_run_id: Option<&str>,
    step_run_id: Option<&str>,
    artifact: ArtifactCreateInput,
) -> Result<orchestration_artifact::Model> {
    orchestration_artifact::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        thread_id: Set(thread_id.to_string()),
        workflow_run_id: Set(workflow_run_id.map(ToOwned::to_owned)),
        step_run_id: Set(step_run_id.map(ToOwned::to_owned)),
        kind: Set(artifact.kind),
        format: Set(artifact.format),
        schema_version: Set(artifact.schema_version),
        title: Set(artifact.title),
        uri: Set(None),
        body: Set(artifact.body),
        metadata: Set(artifact.metadata),
        created_at: Set(chrono::Utc::now().into()),
    }
    .insert(txn)
    .await
    .map_err(|e| anyhow::anyhow!("Database error: {}", e))
}

async fn update_step_status_txn(
    txn: &DatabaseTransaction,
    step_run_id: &str,
    status: &str,
    outputs: Value,
    set_completed_at: bool,
    attempt_status: Option<&str>,
    failure_reason: Option<String>,
) -> Result<orchestration_step_run::Model> {
    let step = OrchestrationStepRunEntity::find_by_id(step_run_id)
        .one(txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        .ok_or_else(|| anyhow::anyhow!("Step run not found: {}", step_run_id))?;

    let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
    let mut active: orchestration_step_run::ActiveModel = step.into();
    active.status = Set(status.to_string());
    active.outputs = Set(Some(outputs));
    active.updated_at = Set(now);
    if set_completed_at {
        active.completed_at = Set(Some(now));
    }
    let updated = active
        .update(txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;

    if let Some(attempt_status) = attempt_status {
        close_active_attempt_txn(txn, step_run_id, attempt_status, failure_reason).await?;
    }

    Ok(updated)
}

async fn close_active_attempt_txn(
    txn: &DatabaseTransaction,
    step_run_id: &str,
    status: &str,
    failure_reason: Option<String>,
) -> Result<()> {
    let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
    OrchestrationStepAttemptEntity::update_many()
        .col_expr(
            orchestration_step_attempt::Column::Status,
            Expr::value(status.to_string()),
        )
        .col_expr(
            orchestration_step_attempt::Column::HeartbeatAt,
            Expr::value(Some(now)),
        )
        .col_expr(
            orchestration_step_attempt::Column::CompletedAt,
            Expr::value(Some(now)),
        )
        .col_expr(
            orchestration_step_attempt::Column::FailureReason,
            Expr::value(failure_reason),
        )
        .col_expr(
            orchestration_step_attempt::Column::UpdatedAt,
            Expr::value(now),
        )
        .filter(orchestration_step_attempt::Column::StepRunId.eq(step_run_id))
        .filter(orchestration_step_attempt::Column::Status.is_in(["running", "blocked"]))
        .exec(txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database update error: {}", e))?;
    Ok(())
}

async fn find_deploy_step_for_external_ref_txn(
    txn: &DatabaseTransaction,
    external_ref: &str,
) -> Result<Option<orchestration_step_run::Model>> {
    // `SELECT … FOR UPDATE` is required here. A multi-sandbox deploy emits one
    // `sandbox.evt.ready` per deployment, and those events are processed
    // concurrently by the external-event consumer. Each handler reads
    // `step.outputs`, merges its deployment's status into
    // `outputs.deploymentStatuses`, and writes the merged JSON back. Without
    // a row lock, concurrent transactions observe the same pre-merge snapshot
    // and the last commit overwrites earlier merges — lost-update race that
    // left deployments stuck at `queued` in the step output even though their
    // events were marked `processed` in `orchestration_external_events`.
    // Locking the step row serializes the merge per step, not globally.
    if let Some(step) = OrchestrationStepRunEntity::find()
        .filter(orchestration_step_run::Column::Adapter.eq("heyo.deploy"))
        .filter(orchestration_step_run::Column::ExternalRef.eq(external_ref))
        .filter(orchestration_step_run::Column::Status.is_in(["running", "blocked"]))
        .order_by_desc(orchestration_step_run::Column::CreatedAt)
        .lock_exclusive()
        .one(txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
    {
        return Ok(Some(step));
    }

    let steps = OrchestrationStepRunEntity::find()
        .filter(orchestration_step_run::Column::Adapter.eq("heyo.deploy"))
        .filter(orchestration_step_run::Column::Status.is_in(["running", "blocked"]))
        .order_by_desc(orchestration_step_run::Column::CreatedAt)
        .lock_exclusive()
        .all(txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?;
    Ok(steps.into_iter().find(|step| {
        step.outputs
            .as_ref()
            .map(|outputs| {
                deployment_ids_from_outputs(outputs)
                    .iter()
                    .any(|id| id == external_ref)
            })
            .unwrap_or(false)
    }))
}

async fn update_external_event_state_txn(
    txn: &DatabaseTransaction,
    event: &orchestration_external_event::Model,
    workflow_run_id: Option<String>,
    step_run_id: Option<String>,
    processing_status: &str,
    processing_error: Option<String>,
    mark_processed_at: bool,
) -> Result<orchestration_external_event::Model> {
    let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
    let mut active: orchestration_external_event::ActiveModel = event.clone().into();
    active.workflow_run_id = Set(workflow_run_id.or(event.workflow_run_id.clone()));
    active.step_run_id = Set(step_run_id.or(event.step_run_id.clone()));
    active.processing_status = Set(processing_status.to_string());
    active.processing_error = Set(processing_error);
    active.processed_at = Set(if mark_processed_at { Some(now) } else { None });
    active.updated_at = Set(now);
    active
        .update(txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database update error: {}", e))
}

fn merge_deploy_event_outputs(base: Value, event: &orchestration_external_event::Model) -> Value {
    let mut merged = base;
    let Some(root) = merged.as_object_mut() else {
        return merged;
    };

    root.insert(
        "lastDeploymentEvent".to_string(),
        json!({
            "deploymentId": event.external_ref,
            "status": event.event_status,
            "backendSandboxId": event.payload.get("backendSandboxId").cloned().unwrap_or(Value::Null),
            "lifecycleError": event.payload.get("error").cloned().unwrap_or(Value::Null),
            "eventType": event.event_type,
            "eventIdempotencyKey": event.idempotency_key,
            "eventMessageId": event.message_id,
        }),
    );

    let deployment_statuses = root
        .entry("deploymentStatuses".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(statuses) = deployment_statuses.as_object_mut() else {
        return merged;
    };

    let mut next = statuses
        .remove(&event.external_ref)
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    next.insert("status".to_string(), json!(event.event_status));
    next.insert(
        "backendSandboxId".to_string(),
        event
            .payload
            .get("backendSandboxId")
            .cloned()
            .unwrap_or(Value::Null),
    );
    next.insert(
        "lifecycleError".to_string(),
        event.payload.get("error").cloned().unwrap_or(Value::Null),
    );
    next.insert("eventType".to_string(), json!(event.event_type));
    statuses.insert(event.external_ref.clone(), Value::Object(next));
    merged
}

fn deployment_ids_from_outputs(outputs: &Value) -> Vec<String> {
    let mut deployment_ids = outputs
        .get("deploymentIds")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if deployment_ids.is_empty() {
        deployment_ids = outputs
            .get("deployments")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("deploymentId").and_then(Value::as_str))
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
    }
    if deployment_ids.is_empty() {
        if let Some(deployment_id) = outputs.get("deploymentId").and_then(Value::as_str) {
            deployment_ids.push(deployment_id.to_string());
        }
    }
    deployment_ids
}

fn deployment_status_entry<'a>(outputs: &'a Value, deployment_id: &str) -> Option<&'a Value> {
    outputs
        .get("deploymentStatuses")
        .and_then(Value::as_object)
        .and_then(|statuses| statuses.get(deployment_id))
}

fn all_tracked_deployments_running(outputs: &Value) -> bool {
    let deployment_ids = deployment_ids_from_outputs(outputs);
    !deployment_ids.is_empty()
        && deployment_ids.iter().all(|deployment_id| {
            deployment_status_entry(outputs, deployment_id)
                .and_then(|entry| entry.get("status"))
                .and_then(Value::as_str)
                == Some("running")
        })
}

async fn update_workflow_state_txn(
    txn: &DatabaseTransaction,
    workflow_run_id: &str,
    status: &str,
    phase: &str,
    current_child_job_key: Option<String>,
) -> Result<orchestration_workflow_run::Model> {
    let workflow = OrchestrationWorkflowRunEntity::find_by_id(workflow_run_id)
        .one(txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database query error: {}", e))?
        .ok_or_else(|| anyhow::anyhow!("Workflow run not found: {}", workflow_run_id))?;

    let started_at = workflow.started_at;
    let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
    let mut active: orchestration_workflow_run::ActiveModel = workflow.into();
    active.status = Set(status.to_string());
    active.phase = Set(phase.to_string());
    active.current_child_job_key = Set(current_child_job_key);
    if started_at.is_none() {
        active.started_at = Set(Some(now));
    }
    if matches!(status, "completed" | "failed" | "cancelled") {
        active.completed_at = Set(Some(now));
    }
    active.updated_at = Set(now);
    active
        .update(txn)
        .await
        .map_err(|e| anyhow::anyhow!("Database update error: {}", e))
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

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        all_tracked_deployments_running, deployment_ids_from_outputs, merge_deploy_event_outputs,
    };
    use crate::entities::orchestration_external_event;
    use serde_json::json;

    fn deployment_event(
        external_ref: &str,
        event_status: &str,
    ) -> orchestration_external_event::Model {
        orchestration_external_event::Model {
            id: format!("event-{external_ref}-{event_status}"),
            workflow_run_id: Some("workflow-1".to_string()),
            step_run_id: Some("step-1".to_string()),
            external_ref: external_ref.to_string(),
            event_type: "sandbox.lifecycle".to_string(),
            event_status: event_status.to_string(),
            idempotency_key: format!("idemp-{external_ref}-{event_status}"),
            message_id: Some(format!("message-{external_ref}-{event_status}")),
            payload: json!({
                "backendSandboxId": format!("sandbox-{external_ref}"),
            }),
            processing_status: "pending".to_string(),
            processing_error: None,
            received_at: chrono::Utc::now().into(),
            processed_at: None,
            updated_at: chrono::Utc::now().into(),
        }
    }

    #[test]
    fn deployment_ids_fall_back_to_deployments_array() {
        let outputs = json!({
            "deployments": [
                { "deploymentId": "dep-a" },
                { "deploymentId": "dep-b" }
            ]
        });

        assert_eq!(
            deployment_ids_from_outputs(&outputs),
            vec!["dep-a".to_string(), "dep-b".to_string()]
        );
    }

    #[test]
    fn merge_deploy_event_outputs_waits_for_all_deployments() {
        let outputs = json!({
            "deploymentIds": ["dep-a", "dep-b"],
            "deploymentStatuses": {
                "dep-a": {
                    "sandboxKey": "web",
                    "status": "queued"
                },
                "dep-b": {
                    "sandboxKey": "worker",
                    "status": "queued"
                }
            }
        });

        let first = merge_deploy_event_outputs(outputs, &deployment_event("dep-a", "running"));
        assert!(!all_tracked_deployments_running(&first));
        assert_eq!(
            first["deploymentStatuses"]["dep-a"]["sandboxKey"].as_str(),
            Some("web")
        );
        assert_eq!(
            first["deploymentStatuses"]["dep-a"]["backendSandboxId"].as_str(),
            Some("sandbox-dep-a")
        );

        let second = merge_deploy_event_outputs(first, &deployment_event("dep-b", "running"));
        assert!(all_tracked_deployments_running(&second));
    }
}
