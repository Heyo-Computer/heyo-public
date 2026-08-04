#[cfg(test)]
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod adapters;
mod reconciler;
mod runtime;

pub const APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID: &str = "app.integrate_with_heyo_and_deploy";
pub const APP_DEPLOY_TO_HEYO_TEMPLATE_ID: &str = "app.deploy_to_heyo";
pub const APP_DEPLOY_FROM_PLAN_TEMPLATE_ID: &str = "app.deploy_from_plan";
pub const APP_ADHOC_REVIEW_PLAN_TEMPLATE_ID: &str = "app.adhoc_review_plan";
pub const APP_ADHOC_REVISE_PLAN_TEMPLATE_ID: &str = "app.adhoc_revise_plan";
pub const DEFAULT_TEMPLATE_VERSION: i32 = 1;

pub(crate) use adapters::{current_backend_caps, init_backend_caps, BackendCaps};
pub use reconciler::run_runtime_reconciler;
pub use runtime::{spawn_execute_until_blocked, OrchestrationExecutor};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPolicy {
    pub require_plan_approval: bool,
    pub require_patch_approval: bool,
    pub require_deploy_approval: bool,
    pub allow_repo_mutation: bool,
    pub allowed_targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactContract {
    #[serde(default)]
    pub produces: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    pub max_retries: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepTemplateDefinition {
    pub key: String,
    pub r#type: String,
    pub kind: String,
    pub phase: String,
    pub adapter: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub can_fan_out: bool,
    #[serde(default)]
    pub requires_approval_before_start: bool,
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    #[serde(default = "empty_object")]
    pub output_schema: Value,
    pub artifact_contract: Option<ArtifactContract>,
    pub retry_policy: Option<RetryPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTemplateDefinition {
    pub id: String,
    pub version: i32,
    pub name: String,
    pub description: String,
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    #[serde(default = "empty_object")]
    pub output_schema: Value,
    pub phase_graph: Vec<String>,
    #[serde(alias = "steps")]
    pub step_templates: Vec<StepTemplateDefinition>,
    pub policy: WorkflowPolicy,
}

fn empty_object() -> Value {
    json!({})
}

pub fn builtin_templates() -> Vec<WorkflowTemplateDefinition> {
    vec![
        app_integrate_with_heyo_and_deploy_template(),
        app_deploy_to_heyo_template(),
        app_deploy_from_plan_template(),
        app_adhoc_review_plan_template(),
        app_adhoc_revise_plan_template(),
    ]
}

pub fn find_builtin_template(id: &str, version: i32) -> Option<WorkflowTemplateDefinition> {
    builtin_templates()
        .into_iter()
        .find(|template| template.id == id && template.version == version)
}

pub fn compile_plan(template: &WorkflowTemplateDefinition) -> Value {
    let child_jobs = template
        .step_templates
        .iter()
        .map(|step| {
            json!({
                "key": step.key,
                "type": step.r#type,
                "kind": step.kind,
                "phase": step.phase,
                "dependsOn": step.depends_on,
                "adapter": step.adapter,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "childJobs": child_jobs,
    })
}

#[cfg(test)]
fn load_builtin_yaml_template(
    source_name: &str,
    yaml: &str,
) -> Result<WorkflowTemplateDefinition, String> {
    let template: WorkflowTemplateDefinition = serde_yaml::from_str(yaml)
        .map_err(|error| format!("failed to parse {source_name}: {error}"))?;
    validate_workflow_template(&template).map_err(|error| format!("{source_name}: {error}"))?;
    Ok(template)
}

#[cfg(test)]
fn validate_workflow_template(template: &WorkflowTemplateDefinition) -> Result<(), String> {
    if template.id.trim().is_empty() {
        return Err("template id is required".to_string());
    }
    if template.version <= 0 {
        return Err(format!(
            "template {} has non-positive version {}",
            template.id, template.version
        ));
    }
    if template.phase_graph.is_empty() {
        return Err(format!("template {} has empty phaseGraph", template.id));
    }
    if template.step_templates.is_empty() {
        return Err(format!("template {} has no steps", template.id));
    }
    if template.policy.allowed_targets.is_empty() {
        return Err(format!(
            "template {} policy.allowedTargets must not be empty",
            template.id
        ));
    }

    let phases = template
        .phase_graph
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut step_keys = BTreeSet::new();
    for step in &template.step_templates {
        if step.key.trim().is_empty() {
            return Err(format!(
                "template {} contains a step with no key",
                template.id
            ));
        }
        if !step_keys.insert(step.key.as_str()) {
            return Err(format!(
                "template {} contains duplicate step key {}",
                template.id, step.key
            ));
        }
        if !phases.contains(step.phase.as_str()) {
            return Err(format!(
                "template {} step {} uses phase {} not present in phaseGraph",
                template.id, step.key, step.phase
            ));
        }
    }

    for step in &template.step_templates {
        for dependency in &step.depends_on {
            if !step_keys.contains(dependency.as_str()) {
                return Err(format!(
                    "template {} step {} depends on missing step {}",
                    template.id, step.key, dependency
                ));
            }
        }
    }

    Ok(())
}

fn app_integrate_with_heyo_and_deploy_template() -> WorkflowTemplateDefinition {
    WorkflowTemplateDefinition {
        id: APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID.to_string(),
        version: DEFAULT_TEMPLATE_VERSION,
        name: "Integrate App With Heyo And Deploy".to_string(),
        description: "Discover an app, plan Heyo integration changes, apply approved patches, verify behavior, and deploy to a Heyo sandbox.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["repoRoot", "appName", "objective", "deployTarget"],
            "properties": {
                "repoRoot": { "type": "string" },
                "appName": { "type": "string" },
                "appType": { "type": "string" },
                "targetPlatform": { "type": "string", "enum": ["heyo"] },
                "objective": { "type": "string" },
                "deployTarget": { "type": "string", "enum": ["heyo-sandbox"] },
                "deployConfig": { "type": "object" }
            }
        }),
        output_schema: json!({
            "type": "object",
            "required": ["childJobs"]
        }),
        phase_graph: vec![
            "discovering".to_string(),
            "planning".to_string(),
            "waiting_plan_approval".to_string(),
            "implementing".to_string(),
            "verifying".to_string(),
            "waiting_deploy_approval".to_string(),
            "deploying".to_string(),
            "completed".to_string(),
        ],
        step_templates: vec![
            StepTemplateDefinition {
                key: "discover_domain_model".to_string(),
                r#type: "agent".to_string(),
                kind: "analysis".to_string(),
                phase: "discovering".to_string(),
                adapter: Some("ai.discovery".to_string()),
                depends_on: vec![],
                can_fan_out: true,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["domain-map".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "draft_integration_plan".to_string(),
                r#type: "agent".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("ai.planning".to_string()),
                depends_on: vec!["discover_domain_model".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["integration-plan".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "approve_plan".to_string(),
                r#type: "approval".to_string(),
                kind: "approval".to_string(),
                phase: "waiting_plan_approval".to_string(),
                adapter: None,
                depends_on: vec!["draft_integration_plan".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: None,
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "draft_patch_set".to_string(),
                r#type: "agent".to_string(),
                kind: "planning".to_string(),
                phase: "implementing".to_string(),
                adapter: Some("ai.planning".to_string()),
                depends_on: vec!["approve_plan".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["patch-set".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "approve_patch_application".to_string(),
                r#type: "approval".to_string(),
                kind: "approval".to_string(),
                phase: "implementing".to_string(),
                adapter: None,
                depends_on: vec!["draft_patch_set".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: None,
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "apply_patch_set".to_string(),
                r#type: "deterministic".to_string(),
                kind: "mutation".to_string(),
                phase: "implementing".to_string(),
                adapter: Some("repo.patch".to_string()),
                depends_on: vec!["approve_patch_application".to_string()],
                can_fan_out: false,
                requires_approval_before_start: true,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: None,
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "verify_changes".to_string(),
                r#type: "deterministic".to_string(),
                kind: "verification".to_string(),
                phase: "verifying".to_string(),
                adapter: Some("repo.verify".to_string()),
                depends_on: vec!["apply_patch_set".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["verification-report".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "approve_deploy".to_string(),
                r#type: "approval".to_string(),
                kind: "approval".to_string(),
                phase: "waiting_deploy_approval".to_string(),
                adapter: None,
                depends_on: vec!["verify_changes".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: None,
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "create_deploy_spec".to_string(),
                r#type: "deterministic".to_string(),
                kind: "deployment".to_string(),
                phase: "deploying".to_string(),
                adapter: Some("heyo.deploy".to_string()),
                depends_on: vec!["approve_deploy".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-spec".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "confirm_runtime_health".to_string(),
                r#type: "deterministic".to_string(),
                kind: "deployment".to_string(),
                phase: "deploying".to_string(),
                adapter: Some("heyo.healthcheck".to_string()),
                depends_on: vec!["create_deploy_spec".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-report".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
        ],
        policy: WorkflowPolicy {
            require_plan_approval: true,
            require_patch_approval: true,
            require_deploy_approval: true,
            allow_repo_mutation: true,
            allowed_targets: vec!["heyo-sandbox".to_string()],
        },
    }
}

fn app_deploy_to_heyo_template() -> WorkflowTemplateDefinition {
    WorkflowTemplateDefinition {
        id: APP_DEPLOY_TO_HEYO_TEMPLATE_ID.to_string(),
        version: DEFAULT_TEMPLATE_VERSION,
        name: "Deploy App To Heyo".to_string(),
        description:
            "Analyze an app, draft a deploy plan, verify it, and deploy it to a Heyo sandbox."
                .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["repoRoot", "appName", "deployTarget"],
            "properties": {
                "repoRoot": { "type": "string" },
                "appName": { "type": "string" },
                "objective": { "type": "string" },
                "deployTarget": { "type": "string", "enum": ["heyo-sandbox"] },
                "deployConfig": { "type": "object" },
                "verificationCommands": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            }
        }),
        output_schema: json!({
            "type": "object",
            "required": ["childJobs"]
        }),
        phase_graph: vec![
            "discovering".to_string(),
            "planning".to_string(),
            "verifying".to_string(),
            "waiting_deploy_approval".to_string(),
            "deploying".to_string(),
            "completed".to_string(),
        ],
        step_templates: vec![
            StepTemplateDefinition {
                key: "discover_domain_model".to_string(),
                r#type: "agent".to_string(),
                kind: "analysis".to_string(),
                phase: "discovering".to_string(),
                adapter: Some("ai.discovery".to_string()),
                depends_on: vec![],
                can_fan_out: true,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["domain-map".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "draft_deploy_plan".to_string(),
                r#type: "agent".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("ai.planning".to_string()),
                depends_on: vec!["discover_domain_model".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-plan".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "review_deploy_plan".to_string(),
                r#type: "agent".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("ai.planning".to_string()),
                depends_on: vec!["draft_deploy_plan".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-plan-review".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "answer_deploy_questions".to_string(),
                r#type: "approval".to_string(),
                kind: "approval".to_string(),
                phase: "planning".to_string(),
                adapter: None,
                depends_on: vec!["review_deploy_plan".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: None,
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "revise_deploy_plan".to_string(),
                r#type: "agent".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("ai.planning".to_string()),
                depends_on: vec!["answer_deploy_questions".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-plan".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "review_revised_deploy_plan".to_string(),
                r#type: "agent".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("ai.planning".to_string()),
                depends_on: vec!["revise_deploy_plan".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-plan-review".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "check_deploy_preflight".to_string(),
                r#type: "deterministic".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("heyo.deploy_preflight".to_string()),
                depends_on: vec!["review_revised_deploy_plan".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-preflight".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "verify_release_candidate".to_string(),
                r#type: "deterministic".to_string(),
                kind: "verification".to_string(),
                phase: "verifying".to_string(),
                adapter: Some("repo.verify".to_string()),
                depends_on: vec!["check_deploy_preflight".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["verification-report".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "approve_deploy".to_string(),
                r#type: "approval".to_string(),
                kind: "approval".to_string(),
                phase: "waiting_deploy_approval".to_string(),
                adapter: None,
                depends_on: vec!["verify_release_candidate".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: None,
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "create_deploy_spec".to_string(),
                r#type: "deterministic".to_string(),
                kind: "deployment".to_string(),
                phase: "deploying".to_string(),
                adapter: Some("heyo.deploy".to_string()),
                depends_on: vec!["approve_deploy".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-spec".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "confirm_runtime_health".to_string(),
                r#type: "deterministic".to_string(),
                kind: "deployment".to_string(),
                phase: "deploying".to_string(),
                adapter: Some("heyo.healthcheck".to_string()),
                depends_on: vec!["create_deploy_spec".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-report".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
        ],
        policy: WorkflowPolicy {
            require_plan_approval: false,
            require_patch_approval: false,
            require_deploy_approval: true,
            allow_repo_mutation: false,
            allowed_targets: vec!["heyo-sandbox".to_string()],
        },
    }
}

fn app_deploy_from_plan_template() -> WorkflowTemplateDefinition {
    WorkflowTemplateDefinition {
        id: APP_DEPLOY_FROM_PLAN_TEMPLATE_ID.to_string(),
        version: DEFAULT_TEMPLATE_VERSION,
        name: "Deploy From Saved Plan".to_string(),
        description:
            "Take a previously approved deploy plan as input and run only the preflight + deploy steps."
                .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["repoRoot", "appName", "deployTarget", "plan"],
            "properties": {
                "repoRoot": { "type": "string" },
                "appName": { "type": "string" },
                "objective": { "type": "string" },
                "deployTarget": { "type": "string", "enum": ["heyo-sandbox"] },
                "deployConfig": { "type": "object" },
                "plan": {
                    "type": "object",
                    "description": "Deploy plan metadata exported from a prior workflow run."
                }
            }
        }),
        output_schema: json!({
            "type": "object",
            "required": ["childJobs"]
        }),
        phase_graph: vec![
            "planning".to_string(),
            "verifying".to_string(),
            "waiting_deploy_approval".to_string(),
            "deploying".to_string(),
            "completed".to_string(),
        ],
        step_templates: vec![
            StepTemplateDefinition {
                key: "import_deploy_plan".to_string(),
                r#type: "deterministic".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("plan.import".to_string()),
                depends_on: vec![],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-plan".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "check_deploy_preflight".to_string(),
                r#type: "deterministic".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("heyo.deploy_preflight".to_string()),
                depends_on: vec!["import_deploy_plan".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-preflight".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "verify_release_candidate".to_string(),
                r#type: "deterministic".to_string(),
                kind: "verification".to_string(),
                phase: "verifying".to_string(),
                adapter: Some("repo.verify".to_string()),
                depends_on: vec!["check_deploy_preflight".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["verification-report".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "approve_deploy".to_string(),
                r#type: "approval".to_string(),
                kind: "approval".to_string(),
                phase: "waiting_deploy_approval".to_string(),
                adapter: None,
                depends_on: vec!["verify_release_candidate".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: None,
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "create_deploy_spec".to_string(),
                r#type: "deterministic".to_string(),
                kind: "deployment".to_string(),
                phase: "deploying".to_string(),
                adapter: Some("heyo.deploy".to_string()),
                depends_on: vec!["approve_deploy".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-spec".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "confirm_runtime_health".to_string(),
                r#type: "deterministic".to_string(),
                kind: "deployment".to_string(),
                phase: "deploying".to_string(),
                adapter: Some("heyo.healthcheck".to_string()),
                depends_on: vec!["create_deploy_spec".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-report".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
        ],
        policy: WorkflowPolicy {
            require_plan_approval: false,
            require_patch_approval: false,
            require_deploy_approval: true,
            allow_repo_mutation: false,
            allowed_targets: vec!["heyo-sandbox".to_string()],
        },
    }
}

fn app_adhoc_review_plan_template() -> WorkflowTemplateDefinition {
    WorkflowTemplateDefinition {
        id: APP_ADHOC_REVIEW_PLAN_TEMPLATE_ID.to_string(),
        version: DEFAULT_TEMPLATE_VERSION,
        name: "Re-review Deploy Plan".to_string(),
        description: "Take an existing deploy plan and produce a fresh review against it."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["repoRoot", "appName", "plan"],
            "properties": {
                "repoRoot": { "type": "string" },
                "appName": { "type": "string" },
                "objective": { "type": "string" },
                "deployTarget": { "type": "string" },
                "plan": { "type": "object" }
            }
        }),
        output_schema: json!({
            "type": "object",
            "required": ["childJobs"]
        }),
        phase_graph: vec!["planning".to_string(), "completed".to_string()],
        step_templates: vec![
            StepTemplateDefinition {
                key: "import_deploy_plan".to_string(),
                r#type: "deterministic".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("plan.import".to_string()),
                depends_on: vec![],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-plan".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "review_deploy_plan".to_string(),
                r#type: "agent".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("ai.planning".to_string()),
                depends_on: vec!["import_deploy_plan".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-plan-review".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
        ],
        policy: WorkflowPolicy {
            require_plan_approval: false,
            require_patch_approval: false,
            require_deploy_approval: false,
            allow_repo_mutation: false,
            allowed_targets: vec!["heyo-sandbox".to_string()],
        },
    }
}

fn app_adhoc_revise_plan_template() -> WorkflowTemplateDefinition {
    WorkflowTemplateDefinition {
        id: APP_ADHOC_REVISE_PLAN_TEMPLATE_ID.to_string(),
        version: DEFAULT_TEMPLATE_VERSION,
        name: "Revise Deploy Plan".to_string(),
        description:
            "Take an existing deploy plan plus operator feedback and produce a revised plan."
                .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["repoRoot", "appName", "plan", "feedback"],
            "properties": {
                "repoRoot": { "type": "string" },
                "appName": { "type": "string" },
                "objective": { "type": "string" },
                "deployTarget": { "type": "string" },
                "plan": { "type": "object" },
                "feedback": {
                    "type": "string",
                    "description": "Operator instructions for revising the plan."
                }
            }
        }),
        output_schema: json!({
            "type": "object",
            "required": ["childJobs"]
        }),
        phase_graph: vec!["planning".to_string(), "completed".to_string()],
        step_templates: vec![
            StepTemplateDefinition {
                key: "import_deploy_plan".to_string(),
                r#type: "deterministic".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("plan.import".to_string()),
                depends_on: vec![],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-plan".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "import_deploy_plan_review".to_string(),
                r#type: "deterministic".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("review.import".to_string()),
                depends_on: vec!["import_deploy_plan".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-plan-review".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
            StepTemplateDefinition {
                key: "revise_deploy_plan".to_string(),
                r#type: "agent".to_string(),
                kind: "planning".to_string(),
                phase: "planning".to_string(),
                adapter: Some("ai.planning".to_string()),
                depends_on: vec!["import_deploy_plan_review".to_string()],
                can_fan_out: false,
                requires_approval_before_start: false,
                input_schema: json!({}),
                output_schema: json!({}),
                artifact_contract: Some(ArtifactContract {
                    produces: vec!["deploy-plan".to_string()],
                }),
                retry_policy: Some(RetryPolicy { max_retries: 0 }),
            },
        ],
        policy: WorkflowPolicy {
            require_plan_approval: false,
            require_patch_approval: false,
            require_deploy_approval: false,
            allow_repo_mutation: false,
            allowed_targets: vec!["heyo-sandbox".to_string()],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_templates_include_generic_deploy_template() {
        assert!(builtin_templates()
            .into_iter()
            .any(|template| template.id == APP_DEPLOY_TO_HEYO_TEMPLATE_ID));
    }

    #[test]
    fn yaml_template_accepts_steps_alias_and_camel_case_fields() {
        let yaml = r#"
id: test.template
version: 1
name: Test Template
description: Test template
phaseGraph:
  - planning
steps:
  - key: plan
    type: deterministic
    kind: planning
    phase: planning
    adapter: test.adapter
    dependsOn: []
    requiresApprovalBeforeStart: false
    artifactContract:
      produces:
        - integration-plan
    retryPolicy:
      maxRetries: 0
policy:
  requirePlanApproval: false
  requirePatchApproval: false
  requireDeployApproval: false
  allowRepoMutation: false
  allowedTargets:
    - heyo-sandbox
"#;

        let template =
            load_builtin_yaml_template("inline-test-template.yaml", yaml).expect("valid template");
        assert_eq!(template.step_templates[0].key, "plan");
        assert_eq!(
            template.step_templates[0]
                .artifact_contract
                .as_ref()
                .expect("artifact contract")
                .produces,
            vec!["integration-plan".to_string()]
        );
        assert_eq!(
            template.step_templates[0]
                .retry_policy
                .as_ref()
                .expect("retry policy")
                .max_retries,
            0
        );
    }

    #[test]
    fn yaml_template_validation_rejects_missing_dependency() {
        let yaml = r#"
id: test.bad-template
version: 1
name: Bad Template
description: Bad template
phaseGraph:
  - planning
steps:
  - key: plan
    type: deterministic
    kind: planning
    phase: planning
    dependsOn:
      - missing
policy:
  requirePlanApproval: false
  requirePatchApproval: false
  requireDeployApproval: false
  allowRepoMutation: false
  allowedTargets:
    - heyo-sandbox
"#;

        let error = load_builtin_yaml_template("bad-template.yaml", yaml)
            .expect_err("missing dependency should fail");
        assert!(error.contains("depends on missing step missing"));
    }

    #[test]
    fn deploy_template_flows_directly_from_planning_to_verification() {
        let template = app_deploy_to_heyo_template();

        assert!(!template
            .phase_graph
            .iter()
            .any(|phase| phase == "waiting_plan_approval"));
        assert!(!template
            .step_templates
            .iter()
            .any(|step| step.key == "approve_plan"));

        let verify_step = template
            .step_templates
            .iter()
            .find(|step| step.key == "verify_release_candidate")
            .expect("verify_release_candidate step should exist");
        assert_eq!(
            verify_step.depends_on,
            vec!["check_deploy_preflight".to_string()]
        );
        assert!(template
            .step_templates
            .iter()
            .any(|step| step.key == "answer_deploy_questions"));
        assert!(template
            .step_templates
            .iter()
            .any(|step| step.key == "revise_deploy_plan"));
        let preflight_step = template
            .step_templates
            .iter()
            .find(|step| step.key == "check_deploy_preflight")
            .expect("check_deploy_preflight step should exist");
        assert_eq!(
            preflight_step.depends_on,
            vec!["review_revised_deploy_plan".to_string()]
        );
        assert!(!template.policy.require_plan_approval);
    }
}
