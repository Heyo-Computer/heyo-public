use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::warn;

use super::{APP_DEPLOY_TO_HEYO_TEMPLATE_ID, APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID};
use crate::agent::{run_repo_agent, AgentTaskMode};
use crate::cloud_client::{
    deployment_healthcheck_url, deployment_preflight, CreateDeploymentRequest,
    DeployPreflightRequest, DeployPreflightResponse,
};
use crate::entities::{
    orchestration_artifact, orchestration_step_run, orchestration_thread,
    orchestration_workflow_run,
};
use crate::repositories::{OrchestrationRepository, ParentJobDetails, ToolCallLogCreateInput};
use crate::AppState;

const DEFAULT_DEPLOY_REGION: &str = "EU";
const DEFAULT_DEPLOY_IMAGE: &str = "alpine:3.21";
const DEFAULT_SIZE_CLASS: &str = "small";
const DOMAIN_MAP_SCHEMA_VERSION: i32 = 2;
const INTEGRATION_PLAN_SCHEMA_VERSION: i32 = 2;
const PATCH_SET_SCHEMA_VERSION: i32 = 2;
const AMP_SANDBOX_GIT_EXCLUDES: &[&str] = &[
    "_cacache/",
    ".npm/",
    ".pnpm-store/",
    ".yarn/cache/",
    "node_modules/",
    ".next/",
    ".turbo/",
    ".cache/",
];

/// Capabilities of the *target backend* (the host where sandboxes actually
/// run). The orchestrator may run on a different OS than the backend, so
/// driver decisions cannot use `cfg(target_os)`. Initialized once at startup
/// from `Config` (and ultimately from a `GET /capabilities` lookup against
/// the registered mvm-ctrl backend).
#[derive(Debug, Clone)]
pub(crate) struct BackendCaps {
    pub target_os: String,
    pub supported_drivers: Vec<String>,
    pub archive_supported_drivers: Vec<String>,
}

static BACKEND_CAPS: std::sync::OnceLock<BackendCaps> = std::sync::OnceLock::new();

pub(crate) fn init_backend_caps(caps: BackendCaps) {
    if BACKEND_CAPS.set(caps).is_err() {
        tracing::warn!("BACKEND_CAPS already initialized; ignoring later set");
    }
}

pub(crate) fn current_backend_caps() -> BackendCaps {
    backend_caps().clone()
}

fn backend_caps() -> &'static BackendCaps {
    BACKEND_CAPS.get_or_init(|| BackendCaps {
        target_os: std::env::consts::OS.to_string(),
        supported_drivers: match std::env::consts::OS {
            "macos" => vec!["apple_container".to_string(), "apple_virt".to_string()],
            _ => vec![
                "firecracker_containerd".to_string(),
                "firecracker".to_string(),
                "libvirt".to_string(),
            ],
        },
        archive_supported_drivers: match std::env::consts::OS {
            "macos" => vec!["apple_container".to_string(), "apple_virt".to_string()],
            _ => vec!["libvirt".to_string()],
        },
    })
}

fn default_deploy_driver() -> String {
    backend_caps()
        .supported_drivers
        .first()
        .cloned()
        .unwrap_or_else(|| "firecracker_containerd".to_string())
}

fn supports_orchestration_deploy_driver(driver: &str) -> bool {
    backend_caps().supported_drivers.iter().any(|d| d == driver)
}

fn supports_archive_deploy_driver(driver: &str) -> bool {
    backend_caps()
        .archive_supported_drivers
        .iter()
        .any(|d| d == driver)
}

fn supports_repo_archive_overlay(driver: &str) -> bool {
    supports_archive_deploy_driver(driver)
        || matches!(driver, "firecracker" | "firecracker_containerd")
}

fn target_backend_is_macos() -> bool {
    backend_caps().target_os == "macos"
}

fn looks_like_managed_image_id(image: &str) -> bool {
    image.starts_with("img-") || image.starts_with("im-")
}

fn looks_like_firecracker_image_spec(image: &str) -> bool {
    let trimmed = image.trim();
    if trimmed.is_empty() {
        return false;
    }
    if looks_like_managed_image_id(trimmed) {
        return true;
    }
    if is_dockerfile_image(trimmed) {
        return true;
    }
    // Absolute path to a host-resident rootfs (.ext4) is accepted by the driver.
    if trimmed.starts_with('/') && trimmed.ends_with(".ext4") {
        return true;
    }
    // OCI reference: `<name>:<tag>` with a non-path name and a valid tag fragment.
    let Some(colon_idx) = trimmed.find(':') else {
        return false;
    };
    let (name, tag) = trimmed.split_at(colon_idx);
    let tag = &tag[1..];
    if name.is_empty() || tag.is_empty() {
        return false;
    }
    if name.starts_with('/') || name.starts_with('.') || name.starts_with('~') {
        return false;
    }
    tag.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

// firecracker_containerd runs OCI images directly (and can build `dockerfile:`
// refs into containerd images on the host); rootfs (.ext4) + managed image IDs
// are Firecracker-only and not meaningful here.
fn looks_like_firecracker_containerd_image_spec(image: &str) -> bool {
    let trimmed = image.trim();
    if trimmed.is_empty() {
        return false;
    }
    if is_dockerfile_image(trimmed) {
        return true;
    }
    let Some(colon_idx) = trimmed.find(':') else {
        return false;
    };
    let (name, tag) = trimmed.split_at(colon_idx);
    let tag = &tag[1..];
    if name.is_empty() || tag.is_empty() {
        return false;
    }
    if name.starts_with('/') || name.starts_with('.') || name.starts_with('~') {
        return false;
    }
    tag.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn orchestration_driver_image_error(driver: &str, image: &str) -> Option<String> {
    if driver == "firecracker" && !looks_like_firecracker_image_spec(image) {
        return Some(format!(
            "driver `{driver}` requires a managed Heyo image id (`img-*`/`im-*`), \
             an absolute `.ext4` path, a `dockerfile:<path>` reference, or an OCI tag \
             (e.g. `ubuntu:24.04`), but got `{image}`"
        ));
    }
    if driver == "firecracker_containerd" && !looks_like_firecracker_containerd_image_spec(image) {
        return Some(format!(
            "driver `{driver}` requires a `dockerfile:<path>` reference or an OCI tag \
             (e.g. `ubuntu:24.04`, `ghcr.io/org/image:tag`), but got `{image}`"
        ));
    }

    None
}

fn is_postgres_like_image(image: &str) -> bool {
    let image = image.trim().to_ascii_lowercase();
    image.starts_with("postgres:")
        || image.contains("/postgres:")
        || image.starts_with("pgvector/pgvector:")
        || image.contains("/pgvector/pgvector:")
}

fn firecracker_containerd_postgres_start_command_needs_explicit_path(
    image: &str,
    start_command: &str,
) -> bool {
    is_postgres_like_image(image)
        && start_command.contains("docker-entrypoint.sh")
        && !start_command.contains("/usr/lib/postgresql/")
}

#[derive(Debug, Clone)]
pub(crate) struct ArtifactDraft {
    pub kind: String,
    pub format: String,
    pub schema_version: i32,
    pub title: Option<String>,
    pub body: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug)]
pub(crate) enum ExternalAction {
    CreateCloudDeployment {
        request: CreateDeploymentRequest,
    },
    CreateCloudDeployments {
        requests: Vec<CreateDeploymentRequest>,
    },
}

#[derive(Debug)]
pub(crate) enum StepExecutionResult {
    Completed {
        outputs: Value,
        artifacts: Vec<ArtifactDraft>,
    },
    WaitingExternal {
        outputs: Value,
        artifacts: Vec<ArtifactDraft>,
        external_ref: String,
        action: ExternalAction,
    },
    Failed {
        error: String,
        outputs: Value,
        artifacts: Vec<ArtifactDraft>,
    },
}

#[derive(Clone)]
pub(crate) struct StepExecutionContext {
    pub workflow_run: orchestration_workflow_run::Model,
    pub step_run: orchestration_step_run::Model,
    pub thread: orchestration_thread::Model,
    pub artifacts: Vec<orchestration_artifact::Model>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowInputs {
    #[serde(default)]
    repo_root: String,
    #[serde(default)]
    app_name: String,
    #[serde(default)]
    app_type: Option<String>,
    #[serde(default)]
    target_platform: Option<String>,
    #[serde(default)]
    objective: String,
    #[serde(default)]
    deploy_target: Option<String>,
    #[serde(default)]
    deploy_config: Option<DeployConfig>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    verification_commands: Vec<String>,
    #[serde(default)]
    patch_diff: Option<String>,
    #[serde(default)]
    patch_set: Option<PatchSetInput>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchSetInput {
    #[serde(default, alias = "patch")]
    diff: Option<String>,
    #[serde(default)]
    verification_commands: Vec<String>,
    #[serde(default)]
    deploy_config: Option<DeployConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeployConfig {
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    driver: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default, alias = "openPorts")]
    ports: Vec<u16>,
    #[serde(default)]
    healthcheck_path: Option<String>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    env_refs: Vec<String>,
    #[serde(default)]
    start_command: Option<String>,
    #[serde(default)]
    working_directory: Option<String>,
    #[serde(default)]
    setup_hooks: Option<Vec<String>>,
    #[serde(default)]
    size_class: Option<String>,
    #[serde(default)]
    ttl_seconds: Option<u64>,
    #[serde(default)]
    expected_status: Option<u16>,
}

#[derive(Debug)]
struct RepoSummary {
    top_level_entries: Vec<String>,
    key_files: Vec<String>,
    frameworks: Vec<String>,
    package_managers: Vec<String>,
    file_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AnalysisMode {
    InformationRequest,
    TransformationRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AnalysisRequestType {
    RepositoryDiscovery,
    IntegrationPlanning,
    PatchPlanning,
    DeploymentPlanning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalysisContract {
    mode: AnalysisMode,
    request_type: AnalysisRequestType,
    execution_policy: String,
    allowed_tools: Vec<String>,
    output_contract: String,
    review_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryRepoArtifact {
    root: String,
    top_level_entries: Vec<String>,
    key_files: Vec<String>,
    frameworks: Vec<String>,
    package_managers: Vec<String>,
    file_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceMappingArtifact {
    heyo_org: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserMappingArtifact {
    heyo_member: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoleMappingArtifact {
    admin: String,
    user: String,
    readonly: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DomainMapArtifact {
    analysis: AnalysisContract,
    repo: DiscoveryRepoArtifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_mapping: Option<WorkspaceMappingArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_mapping: Option<UserMappingArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role_mapping: Option<RoleMappingArtifact>,
    objective: String,
    risks: Vec<String>,
    operator_review_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanChangeArtifact {
    id: String,
    intent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IntegrationPlanArtifact {
    analysis: AnalysisContract,
    repo_root: String,
    objective: String,
    depends_on_artifact_ids: Vec<String>,
    assumptions: Vec<String>,
    changes: Vec<PlanChangeArtifact>,
    verification_commands: Vec<String>,
    verification: Vec<String>,
    deploy_config_preview: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeployPlanArtifact {
    analysis: AnalysisContract,
    repo_root: String,
    objective: String,
    depends_on_artifact_ids: Vec<String>,
    topology: DeployTopologyArtifact,
    verification_commands: Vec<String>,
    manual_verification: Vec<String>,
    assumptions: Vec<String>,
    risks: Vec<String>,
    operator_questions: Vec<String>,
    execution_readiness: DeployExecutionReadinessArtifact,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AmpDeployPlanPayload {
    topology: DeployTopologyArtifact,
    verification_commands: Vec<String>,
    manual_verification: Vec<String>,
    assumptions: Vec<String>,
    risks: Vec<String>,
    operator_questions: Vec<String>,
    execution_readiness: DeployExecutionReadinessArtifact,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeployTopologyArtifact {
    sandbox_count: usize,
    sandboxes: Vec<PlannedSandboxArtifact>,
    not_deployed: Vec<PlannedOmissionArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlannedSandboxArtifact {
    key: String,
    purpose: String,
    runs: Vec<String>,
    region: String,
    driver: String,
    image: String,
    size_class: String,
    ports: Vec<u16>,
    /// Explicit host:container port mappings. For the Apple container CLI
    /// backend (driver: apple_container) host ports MUST be specified here —
    /// the CLI requires `--publish host:guest` and does not support
    /// dynamic-host allocation. For other backends these are optional.
    #[serde(default)]
    port_mappings: Vec<PlannedPortMappingArtifact>,
    env_refs: Vec<String>,
    env_keys: Vec<String>,
    start_command: Option<String>,
    working_directory: Option<String>,
    setup_hooks: Vec<String>,
    ttl_seconds: Option<u64>,
    health_checks: Vec<PlannedHealthCheckArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PlannedPortMappingArtifact {
    host: u16,
    container: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlannedHealthCheckArtifact {
    path: String,
    #[serde(default = "default_health_check_status")]
    expected_status: u16,
}

fn default_health_check_status() -> u16 {
    200
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlannedOmissionArtifact {
    component: String,
    reason: String,
    impact: String,
    blocking: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeployExecutionReadinessArtifact {
    executable_by_orchestrator: bool,
    blocking_reasons: Vec<String>,
    missing_inputs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeployPlanReviewArtifact {
    analysis: AnalysisContract,
    based_on_artifact_ids: Vec<String>,
    recommendation: String,
    summary: String,
    findings: Vec<DeployPlanReviewFinding>,
    unanswered_questions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AmpDeployPlanReviewPayload {
    recommendation: String,
    summary: String,
    findings: Vec<DeployPlanReviewFinding>,
    unanswered_questions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeployPlanReviewFinding {
    severity: String,
    title: String,
    message: String,
    sandbox_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeployPreflightArtifact {
    analysis: AnalysisContract,
    based_on_artifact_ids: Vec<String>,
    account_id: String,
    active_sandboxes: u64,
    max_active_sandboxes: u64,
    remaining_sandbox_capacity: u64,
    requested_sandbox_count: u64,
    allowed: bool,
    blocking_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeployQuestionPrompt {
    pub questions: Vec<String>,
    pub summary_markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchSetMetadata {
    analysis: AnalysisContract,
    based_on_artifact_ids: Vec<String>,
    files: Vec<String>,
    has_diff: bool,
    verification_commands: Vec<String>,
    deploy_config: Value,
    notes: String,
    application_strategy: String,
}

#[derive(Debug)]
struct AmpPatchGeneration {
    diff: String,
    summary: String,
}

pub(crate) async fn execute_adapter(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
) -> StepExecutionResult {
    match context.step_run.adapter.as_deref() {
        Some("ai.discovery") => execute_ai_discovery(context).await,
        Some("ai.planning") => execute_ai_planning(state, repo, context).await,
        Some("plan.import") => execute_plan_import(context).await,
        Some("review.import") => execute_review_import(context).await,
        Some("heyo.deploy_preflight") => execute_deploy_preflight(state, context).await,
        Some("repo.patch") => execute_repo_patch(repo, context).await,
        Some("repo.verify") => execute_repo_verify(repo, context).await,
        Some("heyo.deploy") => execute_heyo_deploy(state, context).await,
        Some("heyo.healthcheck") => execute_heyo_healthcheck(state, repo, context).await,
        Some(adapter) => StepExecutionResult::Failed {
            error: format!("Unsupported orchestration adapter: {adapter}"),
            outputs: json!({ "adapter": adapter }),
            artifacts: vec![],
        },
        None => StepExecutionResult::Failed {
            error: format!(
                "Step {} does not have an adapter configured",
                context.step_run.key
            ),
            outputs: json!({ "stepKey": context.step_run.key }),
            artifacts: vec![],
        },
    }
}

pub(crate) fn build_deploy_spec_preview(
    workflow_run: &orchestration_workflow_run::Model,
    artifacts: &[orchestration_artifact::Model],
) -> Result<Value, String> {
    if let Ok((_, deploy_plan)) =
        latest_validated_artifact::<DeployPlanArtifact>(artifacts, "deploy-plan")
    {
        let primary_sandbox = primary_planned_sandbox(&deploy_plan)?;
        let deployments = deploy_plan
            .topology
            .sandboxes
            .iter()
            .map(|sandbox| {
                let deployment_id = generate_deployed_sandbox_id();
                json!({
                    "key": sandbox.key,
                    "purpose": sandbox.purpose,
                    "region": sandbox.region,
                    "driver": sandbox.driver,
                    "image": sandbox.image,
                    "ports": sandbox.ports,
                    "portMappings": sandbox.port_mappings,
                    "envRefs": sandbox.env_refs,
                    "envKeys": sandbox.env_keys,
                    "startCommand": sandbox.start_command,
                    "workingDirectory": sandbox.working_directory,
                    "sizeClass": sandbox.size_class,
                    "ttlSeconds": sandbox.ttl_seconds,
                    "healthChecks": sandbox.health_checks.iter().map(|check| {
                        json!({
                            "path": check.path,
                            "expectedStatus": check.expected_status,
                        })
                    }).collect::<Vec<_>>(),
                    "deploymentId": deployment_id,
                    "primary": sandbox.key == primary_sandbox.key,
                })
            })
            .collect::<Vec<_>>();
        let primary_deployment_id = deployments
            .iter()
            .find(|deployment| deployment["primary"].as_bool().unwrap_or(false))
            .and_then(|deployment| deployment["deploymentId"].as_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(generate_deployed_sandbox_id);
        return Ok(json!({
            "target": workflow_run.target,
            "topology": deploy_plan_topology_summary(&deploy_plan),
            "executionReadiness": {
                "executableByOrchestrator": deploy_plan.execution_readiness.executable_by_orchestrator,
                "blockingReasons": deploy_plan.execution_readiness.blocking_reasons,
                "missingInputs": deploy_plan.execution_readiness.missing_inputs,
            },
            "release": {
                "source": "repo-archive",
                "ref": format!("workspace://{}", deploy_plan.repo_root),
            },
            "runtime": {
                "region": primary_sandbox.region,
                "driver": primary_sandbox.driver,
                "image": primary_sandbox.image,
                "ports": primary_sandbox.ports,
                "portMappings": primary_sandbox.port_mappings,
                "envRefs": primary_sandbox.env_refs,
                "envKeys": primary_sandbox.env_keys,
                "startCommand": primary_sandbox.start_command,
                "workingDirectory": primary_sandbox.working_directory,
                "sizeClass": primary_sandbox.size_class,
                "ttlSeconds": primary_sandbox.ttl_seconds,
            },
            "healthChecks": primary_sandbox.health_checks.iter().map(|check| {
                json!({
                    "path": check.path,
                    "expectedStatus": check.expected_status,
                })
            }).collect::<Vec<_>>(),
            "deployments": deployments,
            "deploymentId": primary_deployment_id,
            "primaryDeploymentId": primary_deployment_id,
        }));
    }

    let inputs = parse_workflow_inputs(&workflow_run.inputs)?;
    let deploy_config = effective_deploy_config(&inputs);
    let region = deploy_config
        .region
        .clone()
        .unwrap_or_else(|| DEFAULT_DEPLOY_REGION.to_string());
    let driver = deploy_config
        .driver
        .clone()
        .unwrap_or_else(|| default_deploy_driver());
    let image = deploy_config
        .image
        .clone()
        .unwrap_or_else(|| DEFAULT_DEPLOY_IMAGE.to_string());
    let ports = deploy_config.ports.clone();
    let env_refs = deploy_config.env_refs.clone();
    let env_keys = env_keys(&deploy_config);
    let start_command = deploy_config.start_command.clone();
    let working_directory = deploy_config.working_directory.clone();
    let size_class = deploy_config
        .size_class
        .clone()
        .unwrap_or_else(|| DEFAULT_SIZE_CLASS.to_string());
    let ttl_seconds = deploy_config.ttl_seconds;
    let health_checks = health_checks(&deploy_config);

    Ok(json!({
        "target": workflow_run.target,
        "release": {
            "source": "repo-archive",
            "ref": format!("workspace://{}", inputs.repo_root),
        },
        "runtime": {
            "region": region,
            "driver": driver,
            "image": image,
            "ports": ports,
            "envRefs": env_refs,
            "envKeys": env_keys,
            "startCommand": start_command,
            "workingDirectory": working_directory,
            "sizeClass": size_class,
            "ttlSeconds": ttl_seconds,
        },
        "healthChecks": health_checks,
    }))
}

async fn execute_ai_discovery(context: &StepExecutionContext) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let repo_root = match validate_repo_root(&inputs.repo_root) {
        Ok(path) => path,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let repo_summary = match summarize_repo(&repo_root) {
        Ok(summary) => summary,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let template_id = context.workflow_run.template_id.as_str();
    let is_integration = template_id == APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID;
    let risks = discovery_risks(&inputs, &repo_summary, template_id);
    let artifact = DomainMapArtifact {
        analysis: discovery_analysis_contract(),
        repo: DiscoveryRepoArtifact {
            root: repo_root.display().to_string(),
            top_level_entries: repo_summary.top_level_entries.clone(),
            key_files: repo_summary.key_files.clone(),
            frameworks: repo_summary.frameworks.clone(),
            package_managers: repo_summary.package_managers.clone(),
            file_count: repo_summary.file_count,
        },
        workspace_mapping: is_integration.then(|| WorkspaceMappingArtifact {
            heyo_org: "sharedWorkspace".to_string(),
        }),
        user_mapping: is_integration.then(|| UserMappingArtifact {
            heyo_member: "appUser".to_string(),
        }),
        role_mapping: is_integration.then(|| RoleMappingArtifact {
            admin: "admin".to_string(),
            user: "collaborator".to_string(),
            readonly: "guest".to_string(),
        }),
        objective: non_empty(inputs.objective.clone())
            .unwrap_or_else(|| context.workflow_run.goal.clone()),
        risks,
        operator_review_hints: discovery_review_hints(&inputs, &repo_summary, template_id),
    };
    let artifact_value = match validated_structured_output("domain-map", &artifact) {
        Ok(value) => value,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    StepExecutionResult::Completed {
        outputs: json!({
            "requestType": "repository_discovery",
            "analysisMode": "information_request",
            "repoRoot": repo_root.display().to_string(),
            "frameworks": artifact.repo.frameworks,
            "fileCount": artifact.repo.file_count,
        }),
        artifacts: vec![ArtifactDraft {
            kind: "domain-map".to_string(),
            format: "json".to_string(),
            schema_version: DOMAIN_MAP_SCHEMA_VERSION,
            title: Some(format!(
                "{} Domain Map",
                display_app_name(&inputs, &context.workflow_run)
            )),
            body: Some(pretty_json(&artifact_value)),
            metadata: Some(artifact_value),
        }],
    }
}

async fn execute_plan_import(context: &StepExecutionContext) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let plan_value = match context.workflow_run.inputs.get("plan") {
        Some(value) if !value.is_null() => value.clone(),
        _ => {
            let error = "Run-from-plan workflow input is missing the `plan` object.".to_string();
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            };
        }
    };

    let mut artifact: DeployPlanArtifact = match serde_json::from_value(plan_value) {
        Ok(parsed) => parsed,
        Err(err) => {
            let error = format!("Imported plan does not match deploy-plan schema: {err}");
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            };
        }
    };

    // Refresh repo_root and objective with the current workflow's inputs so the
    // imported plan binds to the new run's checkout.
    if !inputs.repo_root.is_empty() {
        artifact.repo_root = inputs.repo_root.clone();
    }
    if !inputs.objective.is_empty() {
        artifact.objective = inputs.objective.clone();
    }

    // Re-target driver/region per current host. The saved plan may have been
    // produced on a different platform (e.g. macOS apple_container) — applying
    // the operator's chosen driver here lets the same plan run on Linux.
    let deploy_config = effective_deploy_config(&inputs);
    if let Some(driver) = non_empty_option(deploy_config.driver.clone()) {
        for sandbox in artifact.topology.sandboxes.iter_mut() {
            sandbox.driver = driver.clone();
        }
    }
    if let Some(region) = non_empty_option(deploy_config.region.clone()) {
        for sandbox in artifact.topology.sandboxes.iter_mut() {
            sandbox.region = region.clone();
        }
    }
    // The saved plan may have flagged the original host as not executable; the
    // operator-imported plan is being re-targeted, so trust them and clear
    // those flags.
    artifact.execution_readiness.executable_by_orchestrator = true;
    artifact.execution_readiness.blocking_reasons.clear();
    artifact.execution_readiness.missing_inputs.clear();

    apply_deploy_plan_guardrails(&mut artifact, &inputs);

    let artifact_value = match validated_structured_output("deploy-plan", &artifact) {
        Ok(value) => value,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    StepExecutionResult::Completed {
        outputs: json!({
            "requestType": "plan_import",
            "sandboxCount": artifact.topology.sandbox_count,
            "verificationCommands": artifact.verification_commands,
            "executableByOrchestrator": artifact.execution_readiness.executable_by_orchestrator,
        }),
        artifacts: vec![ArtifactDraft {
            kind: "deploy-plan".to_string(),
            format: "markdown".to_string(),
            schema_version: 1,
            title: Some(format!(
                "{} Deploy Plan (imported)",
                display_app_name(&inputs, &context.workflow_run)
            )),
            body: Some(render_deploy_plan_markdown(&artifact)),
            metadata: Some(artifact_value),
        }],
    }
}

async fn execute_review_import(context: &StepExecutionContext) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let (deploy_plan_artifact, _deploy_plan) =
        match latest_validated_artifact::<DeployPlanArtifact>(&context.artifacts, "deploy-plan") {
            Ok(result) => result,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        };

    let feedback = context
        .workflow_run
        .inputs
        .get("feedback")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            "Operator requested a revision; no specific guidance provided.".to_string()
        });

    let review = DeployPlanReviewArtifact {
        analysis: deploy_plan_review_analysis_contract(),
        based_on_artifact_ids: vec![deploy_plan_artifact.id.clone()],
        recommendation: "needs_revision".to_string(),
        summary: feedback.clone(),
        findings: vec![DeployPlanReviewFinding {
            severity: "operator_feedback".to_string(),
            title: "Operator revision request".to_string(),
            message: feedback.clone(),
            sandbox_key: None,
        }],
        unanswered_questions: vec![],
    };

    let artifact_value = match validated_structured_output("deploy-plan-review", &review) {
        Ok(value) => value,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    StepExecutionResult::Completed {
        outputs: json!({
            "requestType": "review_import",
            "recommendation": review.recommendation,
        }),
        artifacts: vec![ArtifactDraft {
            kind: "deploy-plan-review".to_string(),
            format: "markdown".to_string(),
            schema_version: 1,
            title: Some(format!(
                "{} Deploy Plan Review (operator request)",
                display_app_name(&inputs, &context.workflow_run)
            )),
            body: Some(render_deploy_plan_review_markdown(&review)),
            metadata: Some(artifact_value),
        }],
    }
}

async fn execute_ai_planning(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
) -> StepExecutionResult {
    match context.step_run.key.as_str() {
        "draft_integration_plan" => execute_integration_plan(context).await,
        "draft_deploy_plan" | "revise_deploy_plan" => {
            execute_deploy_plan(state, repo, context).await
        }
        "review_deploy_plan" | "review_revised_deploy_plan" => {
            execute_deploy_plan_review(state, repo, context).await
        }
        "draft_patch_set" => execute_patch_plan(state, repo, context).await,
        _ => StepExecutionResult::Failed {
            error: format!(
                "Planning adapter does not support step {}",
                context.step_run.key
            ),
            outputs: json!({ "stepKey": context.step_run.key }),
            artifacts: vec![],
        },
    }
}

async fn execute_integration_plan(context: &StepExecutionContext) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let repo_root = match validate_repo_root(&inputs.repo_root) {
        Ok(path) => path,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let (domain_map_artifact, domain_map) =
        match latest_validated_artifact::<DomainMapArtifact>(&context.artifacts, "domain-map") {
            Ok(result) => result,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        };

    let verification_commands = derive_verification_commands(&repo_root, &inputs, None);
    let deploy_preview = redact_deploy_config(&effective_deploy_config(&inputs));
    let changes = vec![
        PlanChangeArtifact {
            id: "bind-heyo-tenant-config".to_string(),
            intent: format!(
                "Bind {} to Heyo tenant and account configuration",
                display_app_name(&inputs, &context.workflow_run)
            ),
        },
        PlanChangeArtifact {
            id: "shared-workspace-membership".to_string(),
            intent: "Create or join an org-scoped shared workspace on Heyo login".to_string(),
        },
        PlanChangeArtifact {
            id: "heyo-auth-route".to_string(),
            intent: auth_entrypoint_intent(&domain_map.repo.frameworks),
        },
        PlanChangeArtifact {
            id: "preserve-role-semantics".to_string(),
            intent: format!(
                "Preserve the discovered readonly-to-{} mapping while adding Heyo auth enforcement",
                domain_map
                    .role_mapping
                    .as_ref()
                    .map(|role| role.readonly.as_str())
                    .unwrap_or("guest")
            ),
        },
    ];
    let verification = if verification_commands.is_empty() {
        vec![
            "No deterministic verification commands were discovered; operator review required"
                .to_string(),
        ]
    } else {
        verification_commands.clone()
    };
    let artifact = IntegrationPlanArtifact {
        analysis: integration_plan_analysis_contract(),
        repo_root: repo_root.display().to_string(),
        objective: non_empty(inputs.objective.clone())
            .unwrap_or_else(|| context.workflow_run.goal.clone()),
        depends_on_artifact_ids: vec![domain_map_artifact.id.clone()],
        assumptions: planning_assumptions(&inputs, &domain_map),
        changes,
        verification_commands: verification_commands.clone(),
        verification: verification.clone(),
        deploy_config_preview: deploy_preview,
    };
    let artifact_value = match validated_structured_output("integration-plan", &artifact) {
        Ok(value) => value,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    StepExecutionResult::Completed {
        outputs: json!({
            "requestType": "integration_planning",
            "analysisMode": "transformation_request",
            "changeCount": artifact.changes.len(),
            "verificationCommands": verification_commands,
        }),
        artifacts: vec![ArtifactDraft {
            kind: "integration-plan".to_string(),
            format: "json".to_string(),
            schema_version: INTEGRATION_PLAN_SCHEMA_VERSION,
            title: Some(format!(
                "{} Integration Plan",
                display_app_name(&inputs, &context.workflow_run)
            )),
            body: Some(pretty_json(&artifact_value)),
            metadata: Some(artifact_value),
        }],
    }
}

async fn execute_deploy_plan(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let repo_root = match validate_repo_root(&inputs.repo_root) {
        Ok(path) => path,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let (domain_map_artifact, domain_map) =
        match latest_validated_artifact::<DomainMapArtifact>(&context.artifacts, "domain-map") {
            Ok(result) => result,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        };

    let revision_context = if context.step_run.key == "revise_deploy_plan" {
        match load_deploy_plan_revision_context(repo, context).await {
            Ok(revision) => Some(revision),
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        }
    } else {
        None
    };

    let mut based_on_artifact_ids = vec![domain_map_artifact.id.clone()];
    let mut artifact = if let Some(revision) = &revision_context {
        based_on_artifact_ids.push(revision.previous_plan_artifact_id.clone());
        based_on_artifact_ids.push(revision.previous_review_artifact_id.clone());

        if let Some(operator_response) = revision.operator_response.as_deref() {
            match revise_deploy_plan_with_amp(
                state,
                repo,
                context,
                &repo_root,
                &inputs,
                &domain_map,
                &revision.previous_plan,
                &revision.previous_review,
                revision.question_prompt.as_ref(),
                operator_response,
            )
            .await
            {
                Ok(plan) => plan,
                Err(error) => {
                    return StepExecutionResult::Failed {
                        error: error.clone(),
                        outputs: json!({ "error": error }),
                        artifacts: vec![],
                    }
                }
            }
        } else {
            revision.previous_plan.clone()
        }
    } else {
        match generate_deploy_plan_with_amp(state, repo, context, &repo_root, &inputs, &domain_map)
            .await
        {
            Ok(plan) => plan,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        }
    };

    artifact.analysis = deploy_plan_analysis_contract();
    artifact.repo_root = repo_root.display().to_string();
    artifact.objective = non_empty(inputs.objective.clone()).unwrap_or_else(|| {
        format!(
            "Deploy {} to Heyo",
            display_app_name(&inputs, &context.workflow_run)
        )
    });
    artifact.depends_on_artifact_ids = merge_unique_strings(based_on_artifact_ids, vec![]);
    artifact.assumptions = merge_unique_strings(
        artifact.assumptions,
        deploy_planning_assumptions(&inputs, &domain_map),
    );
    if artifact.verification_commands.is_empty() {
        artifact.verification_commands = derive_verification_commands(&repo_root, &inputs, None);
    }
    if artifact.manual_verification.is_empty() {
        artifact.manual_verification.push(
            "Review the proposed runtime topology, exposed ports, and omitted dependencies before approving deploy planning."
                .to_string(),
        );
    }
    apply_deploy_plan_guardrails(&mut artifact, &inputs);

    let artifact_value = match validated_structured_output("deploy-plan", &artifact) {
        Ok(value) => value,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    StepExecutionResult::Completed {
        outputs: json!({
            "requestType": "deployment_planning",
            "analysisMode": "transformation_request",
            "sandboxCount": artifact.topology.sandbox_count,
            "verificationCommands": artifact.verification_commands,
            "executableByOrchestrator": artifact.execution_readiness.executable_by_orchestrator,
        }),
        artifacts: vec![ArtifactDraft {
            kind: "deploy-plan".to_string(),
            format: "markdown".to_string(),
            schema_version: 1,
            title: Some(format!(
                "{} Deploy Plan",
                display_app_name(&inputs, &context.workflow_run)
            )),
            body: Some(render_deploy_plan_markdown(&artifact)),
            metadata: Some(artifact_value),
        }],
    }
}

async fn execute_deploy_plan_review(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let repo_root = match validate_repo_root(&inputs.repo_root) {
        Ok(path) => path,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let (deploy_plan_artifact, deploy_plan) =
        match latest_validated_artifact::<DeployPlanArtifact>(&context.artifacts, "deploy-plan") {
            Ok(result) => result,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        };

    let mut review =
        match review_deploy_plan_with_amp(state, repo, context, &repo_root, &inputs, &deploy_plan)
            .await
        {
            Ok(review) => review,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        };
    review.analysis = deploy_plan_review_analysis_contract();
    review.based_on_artifact_ids = vec![deploy_plan_artifact.id.clone()];

    let artifact_value = match validated_structured_output("deploy-plan-review", &review) {
        Ok(value) => value,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    StepExecutionResult::Completed {
        outputs: json!({
            "recommendation": review.recommendation,
            "findingCount": review.findings.len(),
        }),
        artifacts: vec![ArtifactDraft {
            kind: "deploy-plan-review".to_string(),
            format: "markdown".to_string(),
            schema_version: 1,
            title: Some("Deploy Plan Review".to_string()),
            body: Some(render_deploy_plan_review_markdown(&review)),
            metadata: Some(artifact_value),
        }],
    }
}

async fn execute_deploy_preflight(
    state: &AppState,
    context: &StepExecutionContext,
) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };
    let account_id = match non_empty_option(inputs.account_id.clone()) {
        Some(account_id) => account_id,
        None => {
            let error =
                "Workflow inputs are missing accountId for deployment preflight".to_string();
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            };
        }
    };
    let (deploy_plan_artifact, deploy_plan) =
        match latest_validated_artifact::<DeployPlanArtifact>(&context.artifacts, "deploy-plan") {
            Ok(result) => result,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        };

    // Review artifact is optional: from-plan and ad-hoc deploy paths skip the
    // review step entirely.
    let plan_review = latest_validated_artifact::<DeployPlanReviewArtifact>(
        &context.artifacts,
        "deploy-plan-review",
    )
    .ok()
    .map(|(_, review)| review);

    let requested_sandbox_count =
        u64::try_from(deploy_plan.topology.sandbox_count).unwrap_or(u64::MAX);
    // Reviewer `block` recommendations are advisory at preflight time; the
    // operator's `approve_deploy` step is the real gate. Log loudly and
    // continue so the deploy can proceed with explicit operator consent.
    if let Some(plan_review) = plan_review.as_ref() {
        if plan_review.recommendation == "block" {
            tracing::warn!(
                "Plan review recommended `block`; preflight continuing because operator approval is the deploy gate. Review summary: {}",
                plan_review.summary
            );
        }
    }
    let response = if !deploy_plan.execution_readiness.executable_by_orchestrator {
        let mut blocking_reasons = vec![
            "Skipped cloud deploy preflight because the deploy plan is not executable on the current target backend"
                .to_string(),
        ];
        blocking_reasons.extend(deploy_plan.execution_readiness.blocking_reasons.clone());
        DeployPreflightResponse {
            allowed: false,
            active_sandboxes: 0,
            max_active_sandboxes: 0,
            remaining_sandbox_capacity: 0,
            requested_sandbox_count,
            blocking_reasons: merge_unique_strings(blocking_reasons, vec![]),
        }
    } else {
        match deployment_preflight(
            state,
            &DeployPreflightRequest {
                account_id: account_id.clone(),
                requested_sandbox_count,
            },
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                let message = error.to_string();
                return StepExecutionResult::Failed {
                    error: message.clone(),
                    outputs: json!({ "error": message }),
                    artifacts: vec![],
                };
            }
        }
    };

    let artifact = build_deploy_preflight_artifact(&account_id, &deploy_plan_artifact.id, response);
    let artifact_value = match validated_structured_output("deploy-preflight", &artifact) {
        Ok(value) => value,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    StepExecutionResult::Completed {
        outputs: json!({
            "allowed": artifact.allowed,
            "requestedSandboxCount": artifact.requested_sandbox_count,
            "remainingSandboxCapacity": artifact.remaining_sandbox_capacity,
        }),
        artifacts: vec![ArtifactDraft {
            kind: "deploy-preflight".to_string(),
            format: "markdown".to_string(),
            schema_version: 1,
            title: Some("Deploy Preflight".to_string()),
            body: Some(render_deploy_preflight_markdown(&artifact)),
            metadata: Some(artifact_value),
        }],
    }
}

async fn generate_deploy_plan_with_amp(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
    repo_root: &Path,
    inputs: &WorkflowInputs,
    domain_map: &DomainMapArtifact,
) -> Result<DeployPlanArtifact, String> {
    let prompt =
        build_amp_deploy_plan_prompt(inputs, domain_map, &context.workflow_run.template_id);
    let payload = run_amp_structured_analysis::<AmpDeployPlanPayload>(
        state,
        repo,
        context,
        repo_root,
        "agent.repo_analysis",
        &prompt,
        "deploy plan",
    )
    .await?;

    Ok(DeployPlanArtifact {
        analysis: deploy_plan_analysis_contract(),
        repo_root: repo_root.display().to_string(),
        objective: non_empty(inputs.objective.clone())
            .unwrap_or_else(|| format!("Deploy {} to Heyo", inputs.app_name)),
        depends_on_artifact_ids: vec![],
        topology: payload.topology,
        verification_commands: payload.verification_commands,
        manual_verification: payload.manual_verification,
        assumptions: payload.assumptions,
        risks: payload.risks,
        operator_questions: payload.operator_questions,
        execution_readiness: payload.execution_readiness,
    })
}

async fn revise_deploy_plan_with_amp(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
    repo_root: &Path,
    inputs: &WorkflowInputs,
    domain_map: &DomainMapArtifact,
    previous_plan: &DeployPlanArtifact,
    previous_review: &DeployPlanReviewArtifact,
    question_prompt: Option<&DeployQuestionPrompt>,
    operator_response: &str,
) -> Result<DeployPlanArtifact, String> {
    let prompt = build_amp_deploy_plan_revision_prompt(
        inputs,
        domain_map,
        previous_plan,
        previous_review,
        question_prompt,
        operator_response,
        &context.workflow_run.template_id,
    );
    let payload = run_amp_structured_analysis::<AmpDeployPlanPayload>(
        state,
        repo,
        context,
        repo_root,
        "agent.repo_analysis",
        &prompt,
        "deploy plan",
    )
    .await?;

    Ok(DeployPlanArtifact {
        analysis: deploy_plan_analysis_contract(),
        repo_root: repo_root.display().to_string(),
        objective: non_empty(inputs.objective.clone())
            .unwrap_or_else(|| format!("Deploy {} to Heyo", inputs.app_name)),
        depends_on_artifact_ids: vec![],
        topology: payload.topology,
        verification_commands: payload.verification_commands,
        manual_verification: payload.manual_verification,
        assumptions: payload.assumptions,
        risks: payload.risks,
        operator_questions: payload.operator_questions,
        execution_readiness: payload.execution_readiness,
    })
}

async fn review_deploy_plan_with_amp(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
    repo_root: &Path,
    inputs: &WorkflowInputs,
    deploy_plan: &DeployPlanArtifact,
) -> Result<DeployPlanReviewArtifact, String> {
    let prompt =
        build_amp_deploy_plan_review_prompt(inputs, deploy_plan, &context.workflow_run.template_id);
    let payload = run_amp_structured_analysis::<AmpDeployPlanReviewPayload>(
        state,
        repo,
        context,
        repo_root,
        "agent.repo_analysis",
        &prompt,
        "deploy plan review",
    )
    .await?;

    let mut review = DeployPlanReviewArtifact {
        analysis: deploy_plan_review_analysis_contract(),
        based_on_artifact_ids: vec![],
        recommendation: payload.recommendation,
        summary: payload.summary,
        findings: payload.findings,
        unanswered_questions: payload.unanswered_questions,
    };

    review.recommendation = match review.recommendation.trim() {
        "approve" => "approve".to_string(),
        "block" => "block".to_string(),
        _ => "needs_revision".to_string(),
    };
    review.summary = non_empty(review.summary.clone()).unwrap_or_else(|| {
        format!(
            "{} finding(s) identified while reviewing the deploy plan.",
            review.findings.len()
        )
    });
    review.unanswered_questions = merge_unique_strings(review.unanswered_questions, vec![]);

    let mut findings = Vec::with_capacity(review.findings.len());
    let mut seen_finding_messages = BTreeSet::new();
    for finding in review.findings {
        let severity = match finding.severity.trim() {
            "critical" => "critical".to_string(),
            "info" => "info".to_string(),
            _ => "warning".to_string(),
        };
        let title = non_empty(finding.title).unwrap_or_else(|| "Deploy plan finding".to_string());
        let message = non_empty(finding.message)
            .unwrap_or_else(|| "The deploy plan requires additional operator review.".to_string());
        if !seen_finding_messages.insert(message.clone()) {
            continue;
        }
        findings.push(DeployPlanReviewFinding {
            severity,
            title,
            message,
            sandbox_key: finding.sandbox_key.and_then(non_empty),
        });
    }

    for reason in &deploy_plan.execution_readiness.blocking_reasons {
        if !seen_finding_messages.insert(reason.clone()) {
            continue;
        }
        findings.push(DeployPlanReviewFinding {
            severity: "critical".to_string(),
            title: "Plan is not executable on the current target backend".to_string(),
            message: reason.clone(),
            sandbox_key: None,
        });
    }
    for missing_input in &deploy_plan.execution_readiness.missing_inputs {
        let message = format!("Missing input required for deployment execution: {missing_input}");
        if !seen_finding_messages.insert(message.clone()) {
            continue;
        }
        findings.push(DeployPlanReviewFinding {
            severity: "critical".to_string(),
            title: "Workflow input is missing".to_string(),
            message,
            sandbox_key: None,
        });
    }

    let has_critical = findings
        .iter()
        .any(|finding| finding.severity == "critical");
    review.findings = findings;
    review.recommendation = if has_critical {
        "block".to_string()
    } else if !review.unanswered_questions.is_empty() {
        "needs_revision".to_string()
    } else {
        review.recommendation
    };

    Ok(review)
}

#[derive(Debug, Clone)]
struct DeployPlanRevisionContext {
    previous_plan_artifact_id: String,
    previous_review_artifact_id: String,
    previous_plan: DeployPlanArtifact,
    previous_review: DeployPlanReviewArtifact,
    operator_response: Option<String>,
    question_prompt: Option<DeployQuestionPrompt>,
}

async fn load_deploy_plan_revision_context(
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
) -> Result<DeployPlanRevisionContext, String> {
    let (previous_plan_artifact, previous_plan) =
        latest_validated_artifact::<DeployPlanArtifact>(&context.artifacts, "deploy-plan")?;
    let (previous_review_artifact, previous_review) = latest_validated_artifact::<
        DeployPlanReviewArtifact,
    >(
        &context.artifacts, "deploy-plan-review"
    )?;
    let details = repo
        .find_parent_job_details(&context.workflow_run.id)
        .await
        .map_err(|error| {
            format!("Failed to load workflow details for deploy plan revision: {error}")
        })?
        .ok_or_else(|| {
            format!(
                "Workflow run disappeared while loading deploy plan revision context: {}",
                context.workflow_run.id
            )
        })?;

    Ok(DeployPlanRevisionContext {
        previous_plan_artifact_id: previous_plan_artifact.id.clone(),
        previous_review_artifact_id: previous_review_artifact.id.clone(),
        previous_plan,
        previous_review,
        operator_response: latest_approval_comment_for_step_key(
            &details,
            "answer_deploy_questions",
        ),
        question_prompt: deploy_question_prompt_from_artifacts(&context.artifacts)?,
    })
}

fn latest_approval_comment_for_step_key(
    details: &ParentJobDetails,
    step_key: &str,
) -> Option<String> {
    let step_id = details
        .step_runs
        .iter()
        .find(|step| step.key == step_key)
        .map(|step| step.id.clone())?;

    details
        .approvals
        .iter()
        .rev()
        .find(|approval| approval.step_run_id == step_id && approval.status == "approved")
        .and_then(|approval| approval.response_comment.clone())
        .and_then(non_empty)
}

fn apply_deploy_plan_guardrails(artifact: &mut DeployPlanArtifact, inputs: &WorkflowInputs) {
    let deploy_config = effective_deploy_config(inputs);
    let configured_region = non_empty_option(deploy_config.region.clone());
    let configured_driver = non_empty_option(deploy_config.driver.clone());
    let configured_image = non_empty_option(deploy_config.image.clone());
    let configured_start_command = non_empty_option(deploy_config.start_command.clone());
    let configured_working_directory = non_empty_option(deploy_config.working_directory.clone());
    let configured_size_class = non_empty_option(deploy_config.size_class.clone());
    let configured_setup_hooks = deploy_config.setup_hooks.clone().unwrap_or_default();
    let configured_env_keys = env_keys(&deploy_config);
    let configured_env_refs = merge_unique_strings(deploy_config.env_refs.clone(), vec![]);
    let default_region = configured_region
        .clone()
        .unwrap_or_else(|| DEFAULT_DEPLOY_REGION.to_string());
    let default_driver = configured_driver
        .clone()
        .unwrap_or_else(|| default_deploy_driver());
    let default_image = configured_image
        .clone()
        .unwrap_or_else(|| DEFAULT_DEPLOY_IMAGE.to_string());
    let default_size_class = configured_size_class
        .clone()
        .unwrap_or_else(|| DEFAULT_SIZE_CLASS.to_string());

    artifact.verification_commands =
        merge_unique_strings(artifact.verification_commands.clone(), vec![]);
    artifact.manual_verification =
        merge_unique_strings(artifact.manual_verification.clone(), vec![]);
    artifact.assumptions = merge_unique_strings(artifact.assumptions.clone(), vec![]);
    artifact.risks = merge_unique_strings(artifact.risks.clone(), vec![]);
    artifact.operator_questions = merge_unique_strings(artifact.operator_questions.clone(), vec![]);

    for (index, sandbox) in artifact.topology.sandboxes.iter_mut().enumerate() {
        sandbox.key =
            non_empty(sandbox.key.clone()).unwrap_or_else(|| format!("sandbox-{}", index + 1));
        sandbox.purpose = non_empty(sandbox.purpose.clone())
            .unwrap_or_else(|| "Run the primary application service".to_string());
        sandbox.runs = merge_unique_strings(sandbox.runs.clone(), vec![]);
        if sandbox.runs.is_empty() {
            sandbox.runs.push("primary application service".to_string());
        }
        sandbox.region = configured_region.clone().unwrap_or_else(|| {
            non_empty(sandbox.region.clone()).unwrap_or_else(|| default_region.clone())
        });
        sandbox.driver = configured_driver.clone().unwrap_or_else(|| {
            non_empty(sandbox.driver.clone()).unwrap_or_else(|| default_driver.clone())
        });
        sandbox.image = configured_image.clone().unwrap_or_else(|| {
            non_empty(sandbox.image.clone()).unwrap_or_else(|| default_image.clone())
        });
        sandbox.size_class = configured_size_class.clone().unwrap_or_else(|| {
            non_empty(sandbox.size_class.clone()).unwrap_or_else(|| default_size_class.clone())
        });

        let selected_ports = if deploy_config.ports.is_empty() {
            sandbox.ports.clone()
        } else {
            deploy_config.ports.clone()
        };
        let mut unique_ports = BTreeSet::new();
        sandbox.ports = selected_ports
            .into_iter()
            .filter(|port| *port > 0)
            .filter(|port| unique_ports.insert(*port))
            .collect();

        sandbox.env_refs =
            merge_unique_strings(configured_env_refs.clone(), sandbox.env_refs.clone());
        sandbox.env_keys =
            merge_unique_strings(configured_env_keys.clone(), sandbox.env_keys.clone());
        sandbox.start_command = configured_start_command
            .clone()
            .or_else(|| non_empty_option(sandbox.start_command.clone()));
        sandbox.working_directory = configured_working_directory
            .clone()
            .or_else(|| non_empty_option(sandbox.working_directory.clone()));
        sandbox.setup_hooks =
            merge_unique_strings(configured_setup_hooks.clone(), sandbox.setup_hooks.clone());
        sandbox.ttl_seconds = deploy_config.ttl_seconds.or(sandbox.ttl_seconds);

        sandbox.health_checks =
            if let Some(path) = non_empty_option(deploy_config.healthcheck_path.clone()) {
                vec![PlannedHealthCheckArtifact {
                    path: normalize_path(&path),
                    expected_status: deploy_config.expected_status.unwrap_or(200),
                }]
            } else {
                let mut seen_health_checks = BTreeSet::new();
                sandbox
                    .health_checks
                    .iter()
                    .filter_map(|check| {
                        let path = non_empty(check.path.clone())?;
                        let normalized = normalize_path(&path);
                        let expected_status = if check.expected_status == 0 {
                            200
                        } else {
                            check.expected_status
                        };
                        let dedupe_key = format!("{}:{}", normalized, expected_status);
                        if !seen_health_checks.insert(dedupe_key) {
                            return None;
                        }
                        Some(PlannedHealthCheckArtifact {
                            path: normalized,
                            expected_status,
                        })
                    })
                    .collect()
            };
    }

    for (index, omission) in artifact.topology.not_deployed.iter_mut().enumerate() {
        omission.component = non_empty(omission.component.clone())
            .unwrap_or_else(|| format!("component-{}", index + 1));
        omission.reason = non_empty(omission.reason.clone()).unwrap_or_else(|| {
            "The plan could not confidently deploy this component yet.".to_string()
        });
        omission.impact = non_empty(omission.impact.clone()).unwrap_or_else(|| {
            "Operator review is required before assuming this omission is safe.".to_string()
        });
    }

    artifact.topology.sandbox_count = artifact.topology.sandboxes.len();

    let mut blocking_reasons = merge_unique_strings(
        artifact.execution_readiness.blocking_reasons.clone(),
        vec![],
    );
    let mut missing_inputs =
        merge_unique_strings(artifact.execution_readiness.missing_inputs.clone(), vec![]);

    if artifact.topology.sandboxes.is_empty() {
        blocking_reasons.push(
            "Deploy plan did not identify a runnable sandbox topology for this app.".to_string(),
        );
    }
    for sandbox in &artifact.topology.sandboxes {
        if !supports_orchestration_deploy_driver(&sandbox.driver) {
            blocking_reasons.push(format!(
                "Sandbox `{}` requires driver `{}` which is not supported by the current target backend.",
                sandbox.key, sandbox.driver
            ));
        }
        if let Some(error) = orchestration_driver_image_error(&sandbox.driver, &sandbox.image) {
            blocking_reasons.push(format!("Sandbox `{}` {}.", sandbox.key, error));
        }
        if !sandbox.health_checks.is_empty() && sandbox.ports.is_empty() {
            blocking_reasons.push(format!(
                "Sandbox `{}` defines a health check but does not expose a port for that probe.",
                sandbox.key
            ));
        }

        let mut provided_env_inputs = BTreeSet::new();
        provided_env_inputs.extend(configured_env_keys.iter().cloned());
        provided_env_inputs.extend(configured_env_refs.iter().cloned());
        provided_env_inputs.extend(sandbox.env_refs.iter().cloned());
        for env_key in &sandbox.env_keys {
            if !provided_env_inputs.contains(env_key) {
                missing_inputs.push(format!("deployConfig.env.{env_key}"));
            }
        }

        if sandbox.ports.is_empty() {
            artifact.operator_questions.push(format!(
                "Which port should sandbox `{}` expose publicly on Heyo?",
                sandbox.key
            ));
        }
        if sandbox.driver == "firecracker_containerd" && sandbox.start_command.is_none() {
            blocking_reasons.push(format!(
                "Sandbox `{}` uses `firecracker_containerd` but does not define `startCommand`; that backend leaves OCI images idle unless an explicit command is provided.",
                sandbox.key
            ));
        }
        if sandbox.driver == "firecracker_containerd" {
            if let Some(start_command) = sandbox.start_command.as_deref() {
                if firecracker_containerd_postgres_start_command_needs_explicit_path(
                    &sandbox.image,
                    start_command,
                ) {
                    blocking_reasons.push(format!(
                        "Sandbox `{}` invokes `docker-entrypoint.sh` for a Postgres-style image on `firecracker_containerd` without exporting the versioned PostgreSQL bin directory into PATH. That backend runs `startCommand` via `sh -c` without the image's baked PATH, so `initdb`/`postgres` will not be found.",
                        sandbox.key
                    ));
                }
            }
        }
        if sandbox.start_command.is_none() && !is_dockerfile_image(&sandbox.image) {
            artifact.operator_questions.push(format!(
                "What start command should sandbox `{}` run when the image boots?",
                sandbox.key
            ));
        }
    }

    let blocking_omissions = artifact
        .topology
        .not_deployed
        .iter()
        .filter(|omission| omission.blocking)
        .map(|omission| omission.component.clone())
        .collect::<Vec<_>>();
    if !blocking_omissions.is_empty() {
        blocking_reasons.push(format!(
            "The plan omits blocking component(s): {}.",
            blocking_omissions.join(", ")
        ));
        artifact.risks.push(format!(
            "Blocking omissions require follow-up before the deployment can be considered complete: {}.",
            blocking_omissions.join(", ")
        ));
    }

    if non_empty_option(inputs.user_id.clone()).is_none() {
        missing_inputs.push("userId".to_string());
    }
    if non_empty_option(inputs.account_id.clone()).is_none() {
        missing_inputs.push("accountId".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        let _ = configured_driver; // default driver is `apple_virt`; operators can override via deployConfig.driver
    }
    if artifact.verification_commands.is_empty() {
        artifact.manual_verification.push(
            "No deterministic verification commands are configured; operator review must confirm how the release candidate will be validated before deploy."
                .to_string(),
        );
    }
    if artifact
        .topology
        .sandboxes
        .iter()
        .all(|sandbox| sandbox.health_checks.is_empty())
    {
        artifact.manual_verification.push(
            "No automated health check was planned; deploy approval should confirm how runtime health will be assessed after release."
                .to_string(),
        );
    }

    missing_inputs = merge_unique_strings(missing_inputs, vec![]);
    if !missing_inputs.is_empty() {
        blocking_reasons.push(format!(
            "Missing workflow inputs required for deployment execution: {}.",
            missing_inputs.join(", ")
        ));
    }

    artifact.manual_verification =
        merge_unique_strings(artifact.manual_verification.clone(), vec![]);
    artifact.risks = merge_unique_strings(artifact.risks.clone(), vec![]);
    artifact.operator_questions = merge_unique_strings(artifact.operator_questions.clone(), vec![]);
    artifact.execution_readiness.blocking_reasons = merge_unique_strings(blocking_reasons, vec![]);
    artifact.execution_readiness.missing_inputs = missing_inputs;
    artifact.execution_readiness.executable_by_orchestrator =
        artifact.execution_readiness.blocking_reasons.is_empty()
            && artifact.execution_readiness.missing_inputs.is_empty();
}

pub(crate) fn deploy_question_prompt_from_artifacts(
    artifacts: &[orchestration_artifact::Model],
) -> Result<Option<DeployQuestionPrompt>, String> {
    let (_, deploy_plan) =
        latest_validated_artifact::<DeployPlanArtifact>(artifacts, "deploy-plan")?;
    let (_, review) =
        latest_validated_artifact::<DeployPlanReviewArtifact>(artifacts, "deploy-plan-review")?;

    let mut questions = merge_unique_strings(
        deploy_plan.operator_questions.clone(),
        review.unanswered_questions.clone(),
    );

    for missing_input in &deploy_plan.execution_readiness.missing_inputs {
        questions.push(question_for_missing_input(missing_input));
    }

    questions = merge_unique_strings(questions, vec![]);

    if questions.is_empty()
        && (review.recommendation != "approve"
            || !deploy_plan.execution_readiness.executable_by_orchestrator)
    {
        if let Some(question) = synthesized_deploy_revision_question(&deploy_plan, &review) {
            questions.push(question);
        }
    }

    questions = merge_unique_strings(questions, vec![]);

    if questions.is_empty() {
        return Ok(None);
    }

    Ok(Some(DeployQuestionPrompt {
        summary_markdown: render_deploy_question_prompt_markdown(&questions, &deploy_plan, &review),
        questions,
    }))
}

fn question_for_missing_input(missing_input: &str) -> String {
    if let Some(env_key) = missing_input.strip_prefix("deployConfig.env.") {
        return format!(
            "What value or secret reference should the deploy plan use for environment variable `{env_key}`?"
        );
    }

    if missing_input == "accountId" {
        return "Which Heyo account should this app deploy into?".to_string();
    }

    if missing_input == "userId" {
        return "Which Heyo user is requesting this deployment?".to_string();
    }

    format!("Please provide the missing deployment input `{missing_input}`.")
}

fn synthesized_deploy_revision_question(
    deploy_plan: &DeployPlanArtifact,
    review: &DeployPlanReviewArtifact,
) -> Option<String> {
    let mut issues = review
        .findings
        .iter()
        .map(|finding| finding.message.clone())
        .collect::<Vec<_>>();
    issues.extend(deploy_plan.execution_readiness.blocking_reasons.clone());
    issues = merge_unique_strings(issues, vec![]);
    if issues.is_empty() {
        return None;
    }

    Some(format!(
        "What repo-local deployment guidance or operator constraints should the planner incorporate to address: {}?",
        issues.into_iter().take(3).collect::<Vec<_>>().join("; ")
    ))
}

fn render_deploy_question_prompt_markdown(
    questions: &[String],
    deploy_plan: &DeployPlanArtifact,
    review: &DeployPlanReviewArtifact,
) -> String {
    let mut markdown = String::new();
    markdown.push_str("# Deploy Planning Questions\n\n");
    markdown.push_str("The current deploy plan needs more operator input before the orchestrator can confidently continue. Reply in the approval comment with concrete answers, repo-local guidance, commands, env var names, or doc references.\n\n");
    markdown.push_str(&format!(
        "- Current review recommendation: `{}`\n",
        review.recommendation
    ));
    markdown.push_str(&format!(
        "- Executable by orchestrator right now: {}\n\n",
        if deploy_plan.execution_readiness.executable_by_orchestrator {
            "yes"
        } else {
            "no"
        }
    ));
    markdown.push_str("## Questions\n\n");
    for question in questions {
        markdown.push_str(&format!("- {}\n", question));
    }
    markdown
}

fn render_deploy_plan_markdown(artifact: &DeployPlanArtifact) -> String {
    let mut markdown = String::new();
    markdown.push_str("# Deploy Plan\n\n");
    markdown.push_str(&format!("Objective: {}\n\n", artifact.objective));

    markdown.push_str("## Execution Readiness\n\n");
    markdown.push_str(&format!(
        "- Executable by orchestrator: {}\n",
        if artifact.execution_readiness.executable_by_orchestrator {
            "yes"
        } else {
            "no"
        }
    ));
    if !artifact.execution_readiness.blocking_reasons.is_empty() {
        markdown.push_str("- Blocking reasons:\n");
        for reason in &artifact.execution_readiness.blocking_reasons {
            markdown.push_str(&format!("  - {}\n", reason));
        }
    }
    if !artifact.execution_readiness.missing_inputs.is_empty() {
        markdown.push_str("- Missing inputs:\n");
        for item in &artifact.execution_readiness.missing_inputs {
            markdown.push_str(&format!("  - {}\n", item));
        }
    }
    markdown.push('\n');

    markdown.push_str("## Topology\n\n");
    markdown.push_str(&format!(
        "- Planned sandboxes: {}\n\n",
        artifact.topology.sandbox_count
    ));
    for sandbox in &artifact.topology.sandboxes {
        markdown.push_str(&format!("### Sandbox `{}`\n\n", sandbox.key));
        markdown.push_str(&format!("- Purpose: {}\n", sandbox.purpose));
        if !sandbox.runs.is_empty() {
            markdown.push_str(&format!("- Runs: {}\n", sandbox.runs.join(", ")));
        }
        markdown.push_str(&format!(
            "- Runtime: region `{}`, driver `{}`, image `{}`, size class `{}`\n",
            sandbox.region, sandbox.driver, sandbox.image, sandbox.size_class
        ));
        markdown.push_str(&format!(
            "- Ports: {}\n",
            if sandbox.ports.is_empty() {
                "none".to_string()
            } else {
                sandbox
                    .ports
                    .iter()
                    .map(|port| port.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        markdown.push_str(&format!(
            "- Env refs: {}\n",
            if sandbox.env_refs.is_empty() {
                "none".to_string()
            } else {
                sandbox.env_refs.join(", ")
            }
        ));
        markdown.push_str(&format!(
            "- Env keys: {}\n",
            if sandbox.env_keys.is_empty() {
                "none".to_string()
            } else {
                sandbox.env_keys.join(", ")
            }
        ));
        markdown.push_str(&format!(
            "- Start command: {}\n",
            sandbox.start_command.as_deref().unwrap_or("not specified")
        ));
        markdown.push_str(&format!(
            "- Working directory: {}\n",
            sandbox.working_directory.as_deref().unwrap_or("repo root")
        ));
        markdown.push_str(&format!(
            "- Setup hooks: {}\n",
            if sandbox.setup_hooks.is_empty() {
                "none".to_string()
            } else {
                sandbox.setup_hooks.join(" | ")
            }
        ));
        markdown.push_str(&format!(
            "- TTL seconds: {}\n",
            sandbox
                .ttl_seconds
                .map(|ttl| ttl.to_string())
                .unwrap_or_else(|| "default".to_string())
        ));
        if sandbox.health_checks.is_empty() {
            markdown.push_str("- Health checks: none\n\n");
        } else {
            markdown.push_str("- Health checks:\n");
            for check in &sandbox.health_checks {
                markdown.push_str(&format!(
                    "  - `{}` expecting HTTP {}\n",
                    check.path, check.expected_status
                ));
            }
            markdown.push('\n');
        }
    }

    if !artifact.topology.not_deployed.is_empty() {
        markdown.push_str("## Not Deployed\n\n");
        for omission in &artifact.topology.not_deployed {
            markdown.push_str(&format!(
                "- `{}`: {} Impact: {} Blocking: {}\n",
                omission.component,
                omission.reason,
                omission.impact,
                if omission.blocking { "yes" } else { "no" }
            ));
        }
        markdown.push('\n');
    }

    if !artifact.verification_commands.is_empty() {
        markdown.push_str("## Verification Commands\n\n");
        for command in &artifact.verification_commands {
            markdown.push_str(&format!("- `{}`\n", command));
        }
        markdown.push('\n');
    }

    if !artifact.manual_verification.is_empty() {
        markdown.push_str("## Manual Verification\n\n");
        for item in &artifact.manual_verification {
            markdown.push_str(&format!("- {}\n", item));
        }
        markdown.push('\n');
    }

    if !artifact.assumptions.is_empty() {
        markdown.push_str("## Assumptions\n\n");
        for assumption in &artifact.assumptions {
            markdown.push_str(&format!("- {}\n", assumption));
        }
        markdown.push('\n');
    }

    if !artifact.risks.is_empty() {
        markdown.push_str("## Risks\n\n");
        for risk in &artifact.risks {
            markdown.push_str(&format!("- {}\n", risk));
        }
        markdown.push('\n');
    }

    if !artifact.operator_questions.is_empty() {
        markdown.push_str("## Operator Questions\n\n");
        for question in &artifact.operator_questions {
            markdown.push_str(&format!("- {}\n", question));
        }
    }

    markdown
}

fn render_deploy_plan_review_markdown(review: &DeployPlanReviewArtifact) -> String {
    let mut markdown = String::new();
    markdown.push_str("# Deploy Plan Review\n\n");
    markdown.push_str(&format!("- Recommendation: `{}`\n", review.recommendation));
    markdown.push_str(&format!("- Summary: {}\n\n", review.summary));

    if review.findings.is_empty() {
        markdown.push_str("## Findings\n\n- No findings were recorded.\n\n");
    } else {
        markdown.push_str("## Findings\n\n");
        for finding in &review.findings {
            markdown.push_str(&format!(
                "- [{}] {}{}: {}\n",
                finding.severity,
                finding.title,
                finding
                    .sandbox_key
                    .as_deref()
                    .map(|key| format!(" (`{key}`)"))
                    .unwrap_or_default(),
                finding.message
            ));
        }
        markdown.push('\n');
    }

    if !review.unanswered_questions.is_empty() {
        markdown.push_str("## Unanswered Questions\n\n");
        for question in &review.unanswered_questions {
            markdown.push_str(&format!("- {}\n", question));
        }
    }

    markdown
}

fn render_deploy_preflight_markdown(artifact: &DeployPreflightArtifact) -> String {
    let mut markdown = String::new();
    markdown.push_str("# Deploy Preflight\n\n");
    markdown.push_str(&format!(
        "- Status: {}\n",
        if artifact.allowed {
            "allowed"
        } else {
            "blocked"
        }
    ));
    markdown.push_str(&format!("- Account: `{}`\n", artifact.account_id));
    markdown.push_str(&format!(
        "- Active sandboxes: {} / {}\n",
        artifact.active_sandboxes, artifact.max_active_sandboxes
    ));
    markdown.push_str(&format!(
        "- Requested sandbox count: {}\n",
        artifact.requested_sandbox_count
    ));
    markdown.push_str(&format!(
        "- Remaining sandbox capacity: {}\n\n",
        artifact.remaining_sandbox_capacity
    ));

    if artifact.blocking_reasons.is_empty() {
        markdown.push_str("No blocking reasons were returned by the cloud preflight check.\n");
    } else {
        markdown.push_str("## Blocking Reasons\n\n");
        for reason in &artifact.blocking_reasons {
            markdown.push_str(&format!("- {}\n", reason));
        }
    }

    markdown
}

fn build_deploy_preflight_artifact(
    account_id: &str,
    deploy_plan_artifact_id: &str,
    response: DeployPreflightResponse,
) -> DeployPreflightArtifact {
    DeployPreflightArtifact {
        analysis: deploy_preflight_analysis_contract(),
        based_on_artifact_ids: vec![deploy_plan_artifact_id.to_string()],
        account_id: account_id.to_string(),
        active_sandboxes: response.active_sandboxes,
        max_active_sandboxes: response.max_active_sandboxes,
        remaining_sandbox_capacity: response.remaining_sandbox_capacity,
        requested_sandbox_count: response.requested_sandbox_count,
        allowed: response.allowed,
        blocking_reasons: merge_unique_strings(response.blocking_reasons, vec![]),
    }
}

fn primary_planned_sandbox(plan: &DeployPlanArtifact) -> Result<PlannedSandboxArtifact, String> {
    if plan.topology.sandboxes.is_empty() {
        return Err("Deploy plan does not contain any runnable sandboxes".to_string());
    }

    let selected = plan
        .topology
        .sandboxes
        .iter()
        .max_by_key(|sandbox| {
            let key = sandbox.key.to_ascii_lowercase();
            let mut score = 0;
            if !sandbox.health_checks.is_empty() {
                score += 100;
            }
            if !sandbox.ports.is_empty() {
                score += 50;
            }
            if ["primary", "web", "app", "frontend", "api", "server"]
                .iter()
                .any(|needle| key.contains(needle))
            {
                score += 10;
            }
            score
        })
        .cloned();

    selected.ok_or_else(|| "Deploy plan did not identify a primary sandbox".to_string())
}

fn is_dockerfile_image(image: &str) -> bool {
    let trimmed = image.trim();
    trimmed.starts_with("dockerfile:")
        || trimmed.eq_ignore_ascii_case("dockerfile")
        || trimmed.ends_with("/Dockerfile")
}

fn deploy_plan_topology_summary(plan: &DeployPlanArtifact) -> Value {
    json!({
        "sandboxCount": plan.topology.sandbox_count,
        "sandboxes": plan.topology.sandboxes.iter().map(|sandbox| {
            json!({
                "key": sandbox.key,
                "purpose": sandbox.purpose,
                "runs": sandbox.runs,
                "region": sandbox.region,
                "driver": sandbox.driver,
                "image": sandbox.image,
                "sizeClass": sandbox.size_class,
                "ports": sandbox.ports,
                "portMappings": sandbox.port_mappings,
                "envRefs": sandbox.env_refs,
                "envKeys": sandbox.env_keys,
                "startCommand": sandbox.start_command,
                "workingDirectory": sandbox.working_directory,
                "setupHooks": sandbox.setup_hooks,
                "ttlSeconds": sandbox.ttl_seconds,
                "healthChecks": sandbox.health_checks.iter().map(|check| {
                    json!({
                        "path": check.path,
                        "expectedStatus": check.expected_status,
                    })
                }).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
        "notDeployed": plan.topology.not_deployed.iter().map(|omission| {
            json!({
                "component": omission.component,
                "reason": omission.reason,
                "impact": omission.impact,
                "blocking": omission.blocking,
            })
        }).collect::<Vec<_>>(),
    })
}

fn merge_unique_strings(primary: Vec<String>, secondary: Vec<String>) -> Vec<String> {
    let mut merged = Vec::new();
    let mut seen = BTreeSet::new();
    for value in primary.into_iter().chain(secondary) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let normalized = trimmed.to_string();
        if seen.insert(normalized.clone()) {
            merged.push(normalized);
        }
    }
    merged
}

async fn execute_patch_plan(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let repo_root = match validate_repo_root(&inputs.repo_root) {
        Ok(path) => path,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let (integration_plan_artifact, integration_plan) =
        match latest_validated_artifact::<IntegrationPlanArtifact>(
            &context.artifacts,
            "integration-plan",
        ) {
            Ok(result) => result,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        };

    let domain_map =
        latest_validated_artifact::<DomainMapArtifact>(&context.artifacts, "domain-map")
            .ok()
            .map(|(_, artifact)| artifact);
    let explicit_patch_diff = inputs
        .patch_set
        .as_ref()
        .and_then(|patch| patch.diff.clone())
        .or(inputs.patch_diff.clone());
    let mut patch_source = "workflow_input".to_string();
    let mut patch_summary: Option<String> = None;
    let patch_diff = if let Some(patch_diff) = explicit_patch_diff {
        patch_diff
    } else {
        patch_source = "amp".to_string();
        match generate_patch_with_amp(
            state,
            repo,
            context,
            &repo_root,
            &inputs,
            &integration_plan,
            domain_map.as_ref(),
        )
        .await
        {
            Ok(result) => {
                patch_summary = non_empty(result.summary);
                result.diff
            }
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        }
    };
    let patch_deploy_config = inputs
        .patch_set
        .as_ref()
        .and_then(|patch| patch.deploy_config.clone())
        .unwrap_or_else(|| effective_deploy_config(&inputs));
    let mut verification_commands = derive_verification_commands(
        &repo_root,
        &inputs,
        inputs
            .patch_set
            .as_ref()
            .map(|patch| patch.verification_commands.clone()),
    );
    if verification_commands.is_empty() {
        verification_commands = integration_plan.verification_commands.clone();
    }
    let artifact_metadata = PatchSetMetadata {
        analysis: patch_plan_analysis_contract(),
        based_on_artifact_ids: vec![integration_plan_artifact.id.clone()],
        files: diff_files(&patch_diff),
        has_diff: !patch_diff.trim().is_empty(),
        verification_commands: verification_commands.clone(),
        deploy_config: redact_deploy_config(&patch_deploy_config),
        notes: if patch_diff.trim().is_empty() {
            if patch_source == "amp" {
                match patch_summary.as_deref() {
                    Some(summary) => format!(
                        "Amp completed without producing a patch diff. Summary: {}",
                        summary
                    ),
                    None => "Amp completed without producing a patch diff; repo mutation will be a no-op unless a later patch is supplied.".to_string(),
                }
            } else {
                "No patch diff was provided in workflow inputs; repo mutation will be a no-op until a diff is supplied.".to_string()
            }
        } else {
            let mut note = format!(
                "Patch set is ready for approval and deterministic application via git apply after reviewing {} planned changes.",
                integration_plan.changes.len()
            );
            if patch_source == "amp" {
                note.push_str(" Amp generated the repo changes in a temporary sandbox and the orchestrator captured the resulting git diff.");
                if let Some(summary) = patch_summary.as_deref() {
                    note.push_str(" Summary: ");
                    note.push_str(summary);
                }
            }
            note
        },
        application_strategy: "deterministic_git_apply".to_string(),
    };
    let artifact_metadata_value =
        match validated_structured_output("patch-set metadata", &artifact_metadata) {
            Ok(value) => value,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        };

    StepExecutionResult::Completed {
        outputs: json!({
            "requestType": "patch_planning",
            "analysisMode": "transformation_request",
            "hasDiff": !patch_diff.trim().is_empty(),
            "source": patch_source,
            "verificationCommands": verification_commands,
            "basedOnPlanArtifactId": integration_plan_artifact.id,
        }),
        artifacts: vec![ArtifactDraft {
            kind: "patch-set".to_string(),
            format: "diff".to_string(),
            schema_version: PATCH_SET_SCHEMA_VERSION,
            title: Some(format!(
                "{} Patch Set",
                display_app_name(&inputs, &context.workflow_run)
            )),
            body: Some(patch_diff),
            metadata: Some(artifact_metadata_value),
        }],
    }
}

async fn execute_repo_patch(
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let repo_root = match validate_repo_root(&inputs.repo_root) {
        Ok(path) => path,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let Some(patch_artifact) = latest_artifact(&context.artifacts, "patch-set") else {
        let error = "Patch application requires a patch-set artifact".to_string();
        return StepExecutionResult::Failed {
            error: error.clone(),
            outputs: json!({ "error": error }),
            artifacts: vec![],
        };
    };

    let patch_metadata = match artifact_metadata::<PatchSetMetadata>(patch_artifact, "patch-set") {
        Ok(metadata) => metadata,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };
    if patch_metadata.application_strategy != "deterministic_git_apply" {
        let error = format!(
            "Patch set requires unsupported application strategy {}",
            patch_metadata.application_strategy
        );
        return StepExecutionResult::Failed {
            error: error.clone(),
            outputs: json!({ "error": error }),
            artifacts: vec![],
        };
    }

    let patch_diff = patch_artifact.body.clone().unwrap_or_default();
    let has_diff = !patch_diff.trim().is_empty();
    if patch_metadata.has_diff != has_diff {
        let error = format!(
            "Patch set metadata/body mismatch: hasDiff={} but patch body is {}",
            patch_metadata.has_diff,
            if has_diff { "non-empty" } else { "empty" }
        );
        return StepExecutionResult::Failed {
            error: error.clone(),
            outputs: json!({ "error": error }),
            artifacts: vec![],
        };
    }
    let changed_files = if patch_metadata.files.is_empty() {
        diff_files(&patch_diff)
    } else {
        patch_metadata.files.clone()
    };
    if patch_diff.trim().is_empty() {
        return StepExecutionResult::Completed {
            outputs: json!({
                "applied": false,
                "changedFiles": changed_files,
                "message": "Patch set was empty; repo mutation skipped",
            }),
            artifacts: vec![],
        };
    }

    let apply_check_started_at: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
    if let Err(error) = run_git_apply(&repo_root, &["apply", "--check", "-"], &patch_diff).await {
        record_tool_call(
            repo,
            context,
            "git.apply.check",
            json!({
                "args": ["apply", "--check", "-"],
                "patchBytes": patch_diff.len(),
                "files": changed_files,
            }),
            Some(json!({ "error": error })),
            "failed",
            apply_check_started_at,
            Some(chrono::Utc::now().into()),
        )
        .await;
        return StepExecutionResult::Failed {
            error: error.clone(),
            outputs: json!({ "error": error }),
            artifacts: vec![],
        };
    }
    record_tool_call(
        repo,
        context,
        "git.apply.check",
        json!({
            "args": ["apply", "--check", "-"],
            "patchBytes": patch_diff.len(),
            "files": changed_files,
        }),
        Some(json!({ "status": "ok" })),
        "completed",
        apply_check_started_at,
        Some(chrono::Utc::now().into()),
    )
    .await;

    let apply_started_at: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
    if let Err(error) = run_git_apply(&repo_root, &["apply", "-"], &patch_diff).await {
        record_tool_call(
            repo,
            context,
            "git.apply",
            json!({
                "args": ["apply", "-"],
                "patchBytes": patch_diff.len(),
                "files": changed_files,
            }),
            Some(json!({ "error": error })),
            "failed",
            apply_started_at,
            Some(chrono::Utc::now().into()),
        )
        .await;
        return StepExecutionResult::Failed {
            error: error.clone(),
            outputs: json!({ "error": error }),
            artifacts: vec![],
        };
    }
    record_tool_call(
        repo,
        context,
        "git.apply",
        json!({
            "args": ["apply", "-"],
            "patchBytes": patch_diff.len(),
            "files": changed_files,
        }),
        Some(json!({ "applied": true })),
        "completed",
        apply_started_at,
        Some(chrono::Utc::now().into()),
    )
    .await;

    StepExecutionResult::Completed {
        outputs: json!({
            "applied": true,
            "changedFiles": changed_files,
        }),
        artifacts: vec![],
    }
}

async fn execute_repo_verify(
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let repo_root = match validate_repo_root(&inputs.repo_root) {
        Ok(path) => path,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let commands = verification_commands_from_artifacts(&context.artifacts)
        .filter(|commands| !commands.is_empty())
        .unwrap_or_else(|| derive_verification_commands(&repo_root, &inputs, None));

    if commands.is_empty() {
        let report_value = json!({
            "status": "skipped",
            "commands": [],
            "message": "No verification commands were configured for this workflow",
        });
        return StepExecutionResult::Completed {
            outputs: json!({ "status": "skipped", "commands": [] }),
            artifacts: vec![ArtifactDraft {
                kind: "verification-report".to_string(),
                format: "json".to_string(),
                schema_version: 1,
                title: Some("Verification Report".to_string()),
                body: Some(pretty_json(&report_value)),
                metadata: Some(report_value),
            }],
        };
    }

    let mut command_results = Vec::with_capacity(commands.len());
    for command in &commands {
        let command_started_at: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        match run_shell_command(&repo_root, command).await {
            Ok(result) => {
                record_tool_call(
                    repo,
                    context,
                    "shell.verify",
                    json!({ "command": command }),
                    Some(truncate_json_strings(result.clone(), 4000)),
                    "completed",
                    command_started_at,
                    Some(chrono::Utc::now().into()),
                )
                .await;
                command_results.push(result)
            }
            Err(error) => {
                record_tool_call(
                    repo,
                    context,
                    "shell.verify",
                    json!({ "command": command }),
                    Some(json!({ "error": error })),
                    "failed",
                    command_started_at,
                    Some(chrono::Utc::now().into()),
                )
                .await;
                let report_value = json!({
                    "status": "failed",
                    "commands": command_results,
                    "failedCommand": command,
                    "error": error,
                });
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({
                        "status": "failed",
                        "failedCommand": command,
                    }),
                    artifacts: vec![ArtifactDraft {
                        kind: "verification-report".to_string(),
                        format: "json".to_string(),
                        schema_version: 1,
                        title: Some("Verification Report".to_string()),
                        body: Some(pretty_json(&report_value)),
                        metadata: Some(report_value),
                    }],
                };
            }
        }
    }

    let report_value = json!({
        "status": "passed",
        "commands": command_results,
    });
    StepExecutionResult::Completed {
        outputs: json!({
            "status": "passed",
            "commandCount": commands.len(),
        }),
        artifacts: vec![ArtifactDraft {
            kind: "verification-report".to_string(),
            format: "json".to_string(),
            schema_version: 1,
            title: Some("Verification Report".to_string()),
            body: Some(pretty_json(&report_value)),
            metadata: Some(report_value),
        }],
    }
}

async fn execute_heyo_deploy(
    _state: &AppState,
    context: &StepExecutionContext,
) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let repo_root = match validate_repo_root(&inputs.repo_root) {
        Ok(path) => path,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let deploy_config = effective_deploy_config(&inputs);
    let deploy_plan =
        latest_validated_artifact::<DeployPlanArtifact>(&context.artifacts, "deploy-plan")
            .ok()
            .map(|(_, artifact)| artifact);
    let planned_sandboxes = if let Some(deploy_plan) = deploy_plan.as_ref() {
        // Review artifact is optional: from-plan and ad-hoc deploy paths skip
        // the review steps. When a review is present, honor its recommendation.
        let plan_review = latest_validated_artifact::<DeployPlanReviewArtifact>(
            &context.artifacts,
            "deploy-plan-review",
        )
        .ok()
        .map(|(_, review)| review);
        let (_, preflight) = match latest_validated_artifact::<DeployPreflightArtifact>(
            &context.artifacts,
            "deploy-preflight",
        ) {
            Ok(result) => result,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        };
        if let Some(plan_review) = plan_review.as_ref() {
            if plan_review.recommendation == "block" {
                // The reviewer's `block` is advisory; the operator has already
                // signed off via the `approve_deploy` step (otherwise this
                // step wouldn't be reachable). Log loudly and proceed.
                tracing::warn!(
                    "Deploy plan review recommended `block` but operator approved deploy; proceeding anyway. Review summary: {}",
                    plan_review.summary
                );
            }
        }
        if !preflight.allowed {
            let error = if preflight.blocking_reasons.is_empty() {
                "Deploy preflight did not allow this deployment".to_string()
            } else {
                preflight.blocking_reasons.join("; ")
            };
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            };
        }
        if !deploy_plan.execution_readiness.executable_by_orchestrator {
            let error = if deploy_plan.execution_readiness.blocking_reasons.is_empty() {
                "Deploy plan is not executable by the orchestrator yet".to_string()
            } else {
                deploy_plan.execution_readiness.blocking_reasons.join("; ")
            };
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            };
        }
        deploy_plan.topology.sandboxes.clone()
    } else {
        vec![PlannedSandboxArtifact {
            key: "primary".to_string(),
            purpose: "Run the primary application service".to_string(),
            runs: vec!["primary application service".to_string()],
            region: deploy_config
                .region
                .clone()
                .unwrap_or_else(|| DEFAULT_DEPLOY_REGION.to_string()),
            driver: deploy_config
                .driver
                .clone()
                .unwrap_or_else(|| default_deploy_driver()),
            image: deploy_config
                .image
                .clone()
                .unwrap_or_else(|| DEFAULT_DEPLOY_IMAGE.to_string()),
            size_class: deploy_config
                .size_class
                .clone()
                .unwrap_or_else(|| DEFAULT_SIZE_CLASS.to_string()),
            ports: deploy_config.ports.clone(),
            port_mappings: Vec::new(),
            env_refs: deploy_config.env_refs.clone(),
            env_keys: env_keys(&deploy_config),
            start_command: deploy_config.start_command.clone(),
            working_directory: deploy_config.working_directory.clone(),
            setup_hooks: deploy_config.setup_hooks.clone().unwrap_or_default(),
            ttl_seconds: deploy_config.ttl_seconds,
            health_checks: health_checks(&deploy_config)
                .into_iter()
                .map(|check| PlannedHealthCheckArtifact {
                    path: check["path"].as_str().unwrap_or("/").to_string(),
                    expected_status: check["expectedStatus"].as_u64().unwrap_or(200) as u16,
                })
                .collect(),
        }]
    };
    let user_id = match non_empty_option(inputs.user_id.clone()) {
        Some(user_id) => user_id,
        None => {
            let error = "Workflow inputs are missing userId for deployment".to_string();
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            };
        }
    };
    let account_id = match non_empty_option(inputs.account_id.clone()) {
        Some(account_id) => account_id,
        None => {
            let error = "Workflow inputs are missing accountId for deployment".to_string();
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            };
        }
    };

    for sandbox in &planned_sandboxes {
        if !supports_orchestration_deploy_driver(&sandbox.driver) {
            let error = format!(
                "Orchestration deploy does not support driver `{}` for sandbox `{}` on the current target backend",
                sandbox.driver, sandbox.key
            );
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error, "driver": sandbox.driver, "sandboxKey": sandbox.key }),
                artifacts: vec![],
            };
        }
        if let Some(error) = orchestration_driver_image_error(&sandbox.driver, &sandbox.image) {
            return StepExecutionResult::Failed {
                error: format!("Sandbox `{}` {}", sandbox.key, error),
                outputs: json!({
                    "error": error,
                    "driver": sandbox.driver,
                    "image": sandbox.image,
                    "sandboxKey": sandbox.key,
                }),
                artifacts: vec![],
            };
        }
    }

    let archive_bytes = if planned_sandboxes
        .iter()
        .any(|sandbox| supports_repo_archive_overlay(&sandbox.driver))
    {
        match archive_repo_root(&repo_root).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                };
            }
        }
    } else {
        Vec::new()
    };

    let primary_sandbox = if let Some(deploy_plan) = deploy_plan.as_ref() {
        match primary_planned_sandbox(deploy_plan) {
            Ok(sandbox) => sandbox,
            Err(error) => {
                return StepExecutionResult::Failed {
                    error: error.clone(),
                    outputs: json!({ "error": error }),
                    artifacts: vec![],
                }
            }
        }
    } else {
        planned_sandboxes
            .first()
            .cloned()
            .expect("default planned sandbox should exist")
    };
    let app_name = display_app_name(&inputs, &context.workflow_run);
    let health_checks = primary_sandbox
        .health_checks
        .iter()
        .map(|check| {
            json!({
                "path": check.path,
                "expectedStatus": check.expected_status,
            })
        })
        .collect::<Vec<_>>();
    let deployments = planned_sandboxes
        .iter()
        .map(|sandbox| {
            let deployment_id = generate_deployed_sandbox_id();
            let request = CreateDeploymentRequest {
                deployment_id: deployment_id.clone(),
                user_id: user_id.clone(),
                account_id: account_id.clone(),
                name: if planned_sandboxes.len() == 1 {
                    app_name.clone()
                } else {
                    format!("{} ({})", app_name, sandbox.key)
                },
                slug: if planned_sandboxes.len() == 1 {
                    None
                } else {
                    Some(sandbox.key.clone())
                },
                target: context.workflow_run.target.clone(),
                archive_name: if supports_repo_archive_overlay(&sandbox.driver) {
                    Some(format!("{} release", app_name))
                } else {
                    None
                },
                archive_bytes: if supports_repo_archive_overlay(&sandbox.driver) {
                    archive_bytes.clone()
                } else {
                    Vec::new()
                },
                region: sandbox.region.clone(),
                backend_type: sandbox.driver.clone(),
                image: sandbox.image.clone(),
                ports: sandbox.ports.clone(),
                port_mappings: sandbox
                    .port_mappings
                    .iter()
                    .map(|m| crate::cloud_client::PortMapping {
                        host: m.host,
                        container: m.container,
                    })
                    .collect(),
                mounts: Vec::new(),
                env: deploy_config.env.clone(),
                env_refs: sandbox.env_refs.clone(),
                start_command: sandbox.start_command.clone(),
                working_directory: sandbox.working_directory.clone(),
                setup_hooks: Some(sandbox.setup_hooks.clone()),
                size_class: sandbox.size_class.clone(),
                ttl_seconds: sandbox.ttl_seconds,
                metadata: None,
            };
            (sandbox.clone(), deployment_id, request)
        })
        .collect::<Vec<_>>();
    let primary_deployment_id = deployments
        .iter()
        .find(|(sandbox, _, _)| sandbox.key == primary_sandbox.key)
        .map(|(_, deployment_id, _)| deployment_id.clone())
        .unwrap_or_else(generate_deployed_sandbox_id);
    let deployment_specs = deployments
        .iter()
        .map(|(sandbox, deployment_id, _)| {
            json!({
                "key": sandbox.key,
                "purpose": sandbox.purpose,
                "runs": sandbox.runs,
                "region": sandbox.region,
                "driver": sandbox.driver,
                "image": sandbox.image,
                "sizeClass": sandbox.size_class,
                "ports": sandbox.ports,
                "portMappings": sandbox.port_mappings,
                "envRefs": sandbox.env_refs,
                "envKeys": sandbox.env_keys,
                "startCommand": sandbox.start_command,
                "workingDirectory": sandbox.working_directory,
                "setupHooks": sandbox.setup_hooks,
                "ttlSeconds": sandbox.ttl_seconds,
                "healthChecks": sandbox.health_checks.iter().map(|check| {
                    json!({
                        "path": check.path,
                        "expectedStatus": check.expected_status,
                    })
                }).collect::<Vec<_>>(),
                "deploymentId": deployment_id,
                "primary": sandbox.key == primary_sandbox.key,
            })
        })
        .collect::<Vec<_>>();

    let mut deploy_spec_value = json!({
        "target": context.workflow_run.target.clone(),
        "release": {
            "source": "repo-archive",
            "ref": format!("workspace://{}", inputs.repo_root),
        },
        "runtime": {
            "region": primary_sandbox.region.clone(),
            "driver": primary_sandbox.driver.clone(),
            "image": primary_sandbox.image.clone(),
            "ports": primary_sandbox.ports.clone(),
            "portMappings": primary_sandbox.port_mappings.clone(),
            "envRefs": primary_sandbox.env_refs.clone(),
            "envKeys": primary_sandbox.env_keys.clone(),
            "startCommand": primary_sandbox.start_command.clone(),
            "workingDirectory": primary_sandbox.working_directory.clone(),
            "sizeClass": primary_sandbox.size_class.clone(),
            "ttlSeconds": primary_sandbox.ttl_seconds,
        },
        "healthChecks": health_checks,
        "deployments": deployment_specs.clone(),
        "deploymentId": primary_deployment_id.clone(),
        "primaryDeploymentId": primary_deployment_id.clone(),
    });
    if let Some(deploy_plan) = deploy_plan.as_ref() {
        if let Value::Object(object) = &mut deploy_spec_value {
            object.insert(
                "topology".to_string(),
                deploy_plan_topology_summary(deploy_plan),
            );
            object.insert(
                "executionReadiness".to_string(),
                json!({
                    "executableByOrchestrator": deploy_plan.execution_readiness.executable_by_orchestrator,
                    "blockingReasons": deploy_plan.execution_readiness.blocking_reasons,
                    "missingInputs": deploy_plan.execution_readiness.missing_inputs,
                }),
            );
        }
    }
    let deploy_spec_artifact = ArtifactDraft {
        kind: "deploy-spec".to_string(),
        format: "json".to_string(),
        schema_version: 1,
        title: Some("Deploy Spec".to_string()),
        body: Some(pretty_json(&deploy_spec_value)),
        metadata: Some(deploy_spec_value.clone()),
    };

    StepExecutionResult::WaitingExternal {
        outputs: json!({
            "deploymentId": primary_deployment_id.clone(),
            "primaryDeploymentId": primary_deployment_id.clone(),
            "deploymentIds": deployments.iter().map(|(_, deployment_id, _)| deployment_id.clone()).collect::<Vec<_>>(),
            "deployments": deployment_specs,
            "deploymentStatuses": deployments.iter().map(|(sandbox, deployment_id, _)| {
                (
                    deployment_id.clone(),
                    json!({
                        "sandboxKey": sandbox.key,
                        "status": "queued",
                    }),
                )
            }).collect::<serde_json::Map<String, Value>>(),
            "status": "queued",
        }),
        artifacts: vec![deploy_spec_artifact],
        external_ref: primary_deployment_id,
        action: if deployments.len() == 1 {
            ExternalAction::CreateCloudDeployment {
                request: deployments
                    .into_iter()
                    .next()
                    .expect("single deployment request should exist")
                    .2,
            }
        } else {
            ExternalAction::CreateCloudDeployments {
                requests: deployments
                    .into_iter()
                    .map(|(_, _, request)| request)
                    .collect(),
            }
        },
    }
}

async fn execute_heyo_healthcheck(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
) -> StepExecutionResult {
    let inputs = match parse_workflow_inputs(&context.workflow_run.inputs) {
        Ok(inputs) => inputs,
        Err(error) => {
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            }
        }
    };

    let deploy_config = effective_deploy_config(&inputs);
    let planned_sandbox =
        latest_validated_artifact::<DeployPlanArtifact>(&context.artifacts, "deploy-plan")
            .ok()
            .and_then(|(_, plan)| primary_planned_sandbox(&plan).ok());
    let healthcheck_path = planned_sandbox
        .as_ref()
        .and_then(|sandbox| {
            sandbox
                .health_checks
                .first()
                .map(|check| check.path.clone())
        })
        .or_else(|| deploy_config.healthcheck_path.clone());
    let Some(deploy_step_outputs) = latest_step_outputs(&context.artifacts, "deploy-spec") else {
        let Some(deployment_id) = deployment_id_from_artifacts(&context.artifacts) else {
            let error = "Healthcheck step requires a deploymentId from the deploy step".to_string();
            return StepExecutionResult::Failed {
                error: error.clone(),
                outputs: json!({ "error": error }),
                artifacts: vec![],
            };
        };
        return finalize_healthcheck_without_url(
            deployment_id,
            healthcheck_path,
            "Deployment completed without a deploy-spec artifact containing a runtime URL candidate"
                .to_string(),
        );
    };

    let deployment_id = deploy_step_outputs
        .get("deploymentId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| deployment_id_from_artifacts(&context.artifacts));
    let Some(deployment_id) = deployment_id else {
        let error = "Healthcheck step could not resolve deploymentId".to_string();
        return StepExecutionResult::Failed {
            error: error.clone(),
            outputs: json!({ "error": error }),
            artifacts: vec![],
        };
    };

    if healthcheck_path.is_none() {
        return finalize_healthcheck_without_url(
            deployment_id,
            None,
            "No healthcheckPath was configured for this workflow".to_string(),
        );
    }

    let healthcheck_lookup_started_at: chrono::DateTime<chrono::FixedOffset> =
        chrono::Utc::now().into();

    // When the primary planned sandbox has an explicit `portMappings` entry
    // (apple_container deployments that publish to an explicit host port),
    // probe the host port directly. Cloud's `/healthcheck-url` returns the
    // public wildcard proxy URL, which can resolve to a remote edge proxy —
    // not this dev machine — and return 404 for a locally-running backend.
    // Direct-local probe proves the service is up where it actually runs.
    let local_base_url: Option<String> = planned_sandbox.as_ref().and_then(|sandbox| {
        sandbox
            .port_mappings
            .iter()
            .find(|m| m.host > 0)
            .map(|m| format!("http://127.0.0.1:{}", m.host))
    });
    if let Some(local_url) = local_base_url.clone() {
        record_tool_call(
            repo,
            context,
            "cloud.deployment_healthcheck_url",
            json!({ "deploymentId": deployment_id, "source": "plan.portMappings" }),
            Some(json!({ "url": local_url })),
            "completed",
            healthcheck_lookup_started_at,
            Some(chrono::Utc::now().into()),
        )
        .await;
    }

    let base_url = if let Some(local_url) = local_base_url {
        local_url
    } else {
        let cloud_url = match deployment_healthcheck_url(state, &deployment_id).await {
            Ok(Some(url)) => url,
            Ok(None) => {
                record_tool_call(
                    repo,
                    context,
                    "cloud.deployment_healthcheck_url",
                    json!({ "deploymentId": deployment_id }),
                    Some(json!({ "url": Value::Null })),
                    "completed",
                    healthcheck_lookup_started_at,
                    Some(chrono::Utc::now().into()),
                )
                .await;
                return finalize_healthcheck_without_url(
                    deployment_id,
                    healthcheck_path,
                    "Deployment is running but no public proxy endpoint is available to probe"
                        .to_string(),
                );
            }
            Err(error) => {
                let message = error.to_string();
                record_tool_call(
                    repo,
                    context,
                    "cloud.deployment_healthcheck_url",
                    json!({ "deploymentId": deployment_id }),
                    Some(json!({ "error": message })),
                    "failed",
                    healthcheck_lookup_started_at,
                    Some(chrono::Utc::now().into()),
                )
                .await;
                return StepExecutionResult::Failed {
                    error: message.clone(),
                    outputs: json!({ "error": message }),
                    artifacts: vec![],
                };
            }
        };
        record_tool_call(
            repo,
            context,
            "cloud.deployment_healthcheck_url",
            json!({ "deploymentId": deployment_id }),
            Some(json!({ "url": cloud_url.clone() })),
            "completed",
            healthcheck_lookup_started_at,
            Some(chrono::Utc::now().into()),
        )
        .await;
        cloud_url
    };

    let path = healthcheck_path.unwrap_or_else(|| "/".to_string());
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        normalize_path(&path)
    );
    let expected_status = planned_sandbox
        .as_ref()
        .and_then(|sandbox| {
            sandbox
                .health_checks
                .first()
                .map(|check| check.expected_status)
        })
        .unwrap_or_else(|| deploy_config.expected_status.unwrap_or(200));

    let probe_started_at: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
    match state.http_client.get(&url).send().await {
        Ok(response) if response.status().as_u16() == expected_status => {
            let actual_status = response.status().as_u16();
            record_tool_call(
                repo,
                context,
                "http.healthcheck",
                json!({
                    "url": url,
                    "expectedStatus": expected_status,
                }),
                Some(json!({ "actualStatus": actual_status })),
                "completed",
                probe_started_at,
                Some(chrono::Utc::now().into()),
            )
            .await;
            let report_value = json!({
                "status": "passed",
                "deploymentId": deployment_id,
                "url": url,
                "expectedStatus": expected_status,
                "actualStatus": actual_status,
            });
            StepExecutionResult::Completed {
                outputs: json!({
                    "deploymentId": deployment_id,
                    "url": url,
                    "status": "passed",
                }),
                artifacts: vec![ArtifactDraft {
                    kind: "deploy-report".to_string(),
                    format: "json".to_string(),
                    schema_version: 1,
                    title: Some("Deploy Report".to_string()),
                    body: Some(pretty_json(&report_value)),
                    metadata: Some(report_value),
                }],
            }
        }
        Ok(response) => {
            let actual_status = response.status().as_u16();
            record_tool_call(
                repo,
                context,
                "http.healthcheck",
                json!({
                    "url": url,
                    "expectedStatus": expected_status,
                }),
                Some(json!({ "actualStatus": actual_status })),
                "failed",
                probe_started_at,
                Some(chrono::Utc::now().into()),
            )
            .await;
            let report_value = json!({
                "status": "failed",
                "deploymentId": deployment_id,
                "url": url,
                "expectedStatus": expected_status,
                "actualStatus": actual_status,
            });
            StepExecutionResult::Failed {
                error: format!(
                    "Healthcheck returned {} for {}, expected {}",
                    response.status(),
                    url,
                    expected_status
                ),
                outputs: json!({
                    "deploymentId": deployment_id,
                    "url": url,
                    "status": "failed",
                    "actualStatus": response.status().as_u16(),
                }),
                artifacts: vec![ArtifactDraft {
                    kind: "deploy-report".to_string(),
                    format: "json".to_string(),
                    schema_version: 1,
                    title: Some("Deploy Report".to_string()),
                    body: Some(pretty_json(&report_value)),
                    metadata: Some(report_value),
                }],
            }
        }
        Err(error) => {
            let message = format!("Healthcheck request failed for {url}: {error}");
            record_tool_call(
                repo,
                context,
                "http.healthcheck",
                json!({
                    "url": url,
                    "expectedStatus": expected_status,
                }),
                Some(json!({ "error": message })),
                "failed",
                probe_started_at,
                Some(chrono::Utc::now().into()),
            )
            .await;
            let report_value = json!({
                "status": "failed",
                "deploymentId": deployment_id,
                "url": url,
                "expectedStatus": expected_status,
                "error": message,
            });
            StepExecutionResult::Failed {
                error: message.clone(),
                outputs: json!({
                    "deploymentId": deployment_id,
                    "url": url,
                    "status": "failed",
                }),
                artifacts: vec![ArtifactDraft {
                    kind: "deploy-report".to_string(),
                    format: "json".to_string(),
                    schema_version: 1,
                    title: Some("Deploy Report".to_string()),
                    body: Some(pretty_json(&report_value)),
                    metadata: Some(report_value),
                }],
            }
        }
    }
}

fn finalize_healthcheck_without_url(
    deployment_id: String,
    healthcheck_path: Option<String>,
    message: String,
) -> StepExecutionResult {
    let report_value = json!({
        "status": "skipped",
        "deploymentId": deployment_id,
        "healthcheckPath": healthcheck_path,
        "message": message,
    });
    StepExecutionResult::Completed {
        outputs: json!({
            "deploymentId": report_value["deploymentId"],
            "status": "skipped",
            "message": report_value["message"],
        }),
        artifacts: vec![ArtifactDraft {
            kind: "deploy-report".to_string(),
            format: "json".to_string(),
            schema_version: 1,
            title: Some("Deploy Report".to_string()),
            body: Some(pretty_json(&report_value)),
            metadata: Some(report_value),
        }],
    }
}

async fn record_message(
    repo: &OrchestrationRepository,
    thread_id: &str,
    role: &str,
    text: String,
    metadata: Option<Value>,
) {
    if let Err(error) = repo
        .create_message(thread_id, role, json!({ "text": text }), metadata)
        .await
    {
        warn!(
            "Failed to append orchestration adapter message for thread {}: {}",
            thread_id, error
        );
    }
}

async fn record_tool_call(
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
    tool_name: &str,
    input: Value,
    output: Option<Value>,
    status: &str,
    started_at: chrono::DateTime<chrono::FixedOffset>,
    completed_at: Option<chrono::DateTime<chrono::FixedOffset>>,
) {
    if let Err(error) = repo
        .create_tool_call_log(ToolCallLogCreateInput {
            thread_id: context.thread.id.clone(),
            workflow_run_id: Some(context.workflow_run.id.clone()),
            step_run_id: Some(context.step_run.id.clone()),
            tool_name: tool_name.to_string(),
            input: truncate_json_strings(input, 12000),
            output: output.map(|value| truncate_json_strings(value, 12000)),
            status: status.to_string(),
            started_at,
            completed_at,
        })
        .await
    {
        warn!(
            "Failed to append orchestration adapter tool log for thread {}: {}",
            context.thread.id, error
        );
    }
}

async fn begin_tool_call(
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
    tool_name: &str,
    input: Value,
    started_at: chrono::DateTime<chrono::FixedOffset>,
) -> Option<String> {
    match repo
        .create_tool_call_log(ToolCallLogCreateInput {
            thread_id: context.thread.id.clone(),
            workflow_run_id: Some(context.workflow_run.id.clone()),
            step_run_id: Some(context.step_run.id.clone()),
            tool_name: tool_name.to_string(),
            input: truncate_json_strings(input, 12000),
            output: None,
            status: "started".to_string(),
            started_at,
            completed_at: None,
        })
        .await
    {
        Ok(log) => Some(log.id),
        Err(error) => {
            warn!(
                "Failed to append running orchestration adapter tool log for thread {}: {}",
                context.thread.id, error
            );
            None
        }
    }
}

async fn finish_tool_call(
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
    tool_call_log_id: Option<&str>,
    output: Option<Value>,
    status: &str,
    completed_at: chrono::DateTime<chrono::FixedOffset>,
) {
    if let Some(tool_call_log_id) = tool_call_log_id {
        if let Err(error) = repo
            .update_tool_call_log(
                tool_call_log_id,
                output.map(|value| truncate_json_strings(value, 12000)),
                status,
                Some(completed_at),
            )
            .await
        {
            warn!(
                "Failed to update orchestration adapter tool log {} for thread {}: {}",
                tool_call_log_id, context.thread.id, error
            );
        }
    }
}

fn truncate_json_strings(value: Value, max_len: usize) -> Value {
    match value {
        Value::String(text) => Value::String(truncate_text(&text, max_len)),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|entry| truncate_json_strings(entry, max_len))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, entry)| (key, truncate_json_strings(entry, max_len)))
                .collect(),
        ),
        other => other,
    }
}

fn truncate_text(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_string();
    }

    let truncated = value.chars().take(max_len).collect::<String>();
    format!("{}\n\n... truncated ...", truncated)
}

fn parse_workflow_inputs(value: &Value) -> Result<WorkflowInputs, String> {
    serde_json::from_value(value.clone())
        .map_err(|error| format!("Workflow inputs are invalid: {error}"))
}

fn effective_deploy_config(inputs: &WorkflowInputs) -> DeployConfig {
    inputs
        .patch_set
        .as_ref()
        .and_then(|patch| patch.deploy_config.clone())
        .or_else(|| inputs.deploy_config.clone())
        .unwrap_or_default()
}

fn display_app_name(
    inputs: &WorkflowInputs,
    workflow_run: &orchestration_workflow_run::Model,
) -> String {
    non_empty(inputs.app_name.clone()).unwrap_or_else(|| workflow_run.goal.clone())
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn non_empty_option(value: Option<String>) -> Option<String> {
    value.and_then(non_empty)
}

fn validate_repo_root(repo_root: &str) -> Result<PathBuf, String> {
    if repo_root.trim().is_empty() {
        return Err("Workflow inputs are missing repoRoot".to_string());
    }

    let path = PathBuf::from(repo_root);
    let canonical = std::fs::canonicalize(&path)
        .map_err(|error| format!("Failed to resolve repoRoot {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "repoRoot {} is not a directory",
            canonical.display()
        ));
    }

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|dir| std::fs::canonicalize(dir).ok());
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|dir| std::fs::canonicalize(dir).ok());
    let allowed = cwd.as_ref().is_some_and(|dir| canonical.starts_with(dir))
        || home.as_ref().is_some_and(|dir| canonical.starts_with(dir));

    if !allowed {
        return Err(format!(
            "repoRoot {} is outside the allowed workspace or HOME directory",
            canonical.display()
        ));
    }

    Ok(canonical)
}

fn summarize_repo(repo_root: &Path) -> Result<RepoSummary, String> {
    let mut directories = vec![repo_root.to_path_buf()];
    let mut file_paths = Vec::new();
    let mut top_level_entries = BTreeSet::new();
    let ignored = [
        ".git",
        "node_modules",
        "target",
        "dist",
        "build",
        ".next",
        ".turbo",
        ".idea",
        ".vscode",
    ];

    while let Some(directory) = directories.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("Failed to read {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("Failed to read repo entry: {error}"))?;
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if ignored.iter().any(|ignored| ignored == &file_name.as_ref()) {
                continue;
            }

            let relative = path
                .strip_prefix(repo_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\u{5c}', "/");

            if directory == repo_root {
                top_level_entries.insert(relative.clone());
            }

            if path.is_dir() {
                directories.push(path);
                continue;
            }

            file_paths.push(relative);
            if file_paths.len() >= 400 {
                break;
            }
        }
        if file_paths.len() >= 400 {
            break;
        }
    }

    let key_files = prioritized_key_files(&file_paths);
    let frameworks = detect_frameworks(&file_paths);
    let package_managers = detect_package_managers(&file_paths);

    Ok(RepoSummary {
        top_level_entries: top_level_entries.into_iter().take(20).collect(),
        key_files,
        frameworks,
        package_managers,
        file_count: file_paths.len(),
    })
}

fn prioritized_key_files(files: &[String]) -> Vec<String> {
    let mut priority = Vec::new();
    let important = [
        "Cargo.toml",
        "package.json",
        "bun.lock",
        "bun.lockb",
        "pnpm-lock.yaml",
        "package-lock.json",
        "yarn.lock",
        "svelte.config.js",
        "svelte.config.ts",
        "next.config.js",
        "next.config.mjs",
        "tauri.conf.json",
        "src/main.rs",
        "src/main.ts",
        "src/main.js",
        "README.md",
    ];

    for name in important {
        if let Some(found) = files.iter().find(|file| file.ends_with(name)) {
            priority.push(found.clone());
        }
    }
    priority.truncate(20);
    priority
}

fn detect_frameworks(files: &[String]) -> Vec<String> {
    let mut frameworks = BTreeSet::new();
    if files.iter().any(|file| file == "Cargo.toml") {
        frameworks.insert("rust".to_string());
    }
    if files.iter().any(|file| file == "package.json") {
        frameworks.insert("node".to_string());
    }
    if files
        .iter()
        .any(|file| file == "svelte.config.js" || file == "svelte.config.ts")
    {
        frameworks.insert("sveltekit".to_string());
    }
    if files
        .iter()
        .any(|file| file == "next.config.js" || file == "next.config.mjs")
    {
        frameworks.insert("nextjs".to_string());
    }
    if files.iter().any(|file| file.ends_with("tauri.conf.json")) {
        frameworks.insert("tauri".to_string());
    }
    frameworks.into_iter().collect()
}

fn detect_package_managers(files: &[String]) -> Vec<String> {
    let mut managers = BTreeSet::new();
    if files
        .iter()
        .any(|file| file == "bun.lock" || file == "bun.lockb")
    {
        managers.insert("bun".to_string());
    }
    if files.iter().any(|file| file == "pnpm-lock.yaml") {
        managers.insert("pnpm".to_string());
    }
    if files.iter().any(|file| file == "yarn.lock") {
        managers.insert("yarn".to_string());
    }
    if files.iter().any(|file| file == "package-lock.json") {
        managers.insert("npm".to_string());
    }
    managers.into_iter().collect()
}

fn discovery_risks(
    inputs: &WorkflowInputs,
    repo_summary: &RepoSummary,
    template_id: &str,
) -> Vec<String> {
    let mut risks = if template_id == APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID {
        vec![
            "first login must create or join the shared org-scoped workspace deterministically"
                .to_string(),
            "role mapping must preserve Heyo readonly semantics without granting extra workspace access"
                .to_string(),
        ]
    } else {
        vec![
            "sandbox runtime selection must match what the repo actually requires to build and start"
                .to_string(),
            "exposed ports, env vars, and start command must be derivable from the repo rather than guessed"
                .to_string(),
        ]
    };
    if repo_summary.frameworks.contains(&"rust".to_string())
        && repo_summary.frameworks.contains(&"node".to_string())
    {
        let multi_runtime = if template_id == APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID {
            "multi-runtime repository detected; auth and workspace changes may span more than one service"
        } else {
            "multi-runtime repository detected; deploy plan may require more than one sandbox or a supervisor-style startCommand"
        };
        risks.push(multi_runtime.to_string());
    }
    if repo_summary.package_managers.is_empty() && repo_summary.frameworks.is_empty() {
        risks.push(
            "repo discovery could not infer a primary framework; operator review should validate plan scope"
                .to_string(),
        );
    }
    if inputs.deploy_config.is_none() {
        risks.push(
            "deployConfig was not provided; deploy adapter will use conservative defaults that may need operator approval"
                .to_string(),
        );
    }
    risks
}

fn discovery_review_hints(
    inputs: &WorkflowInputs,
    repo_summary: &RepoSummary,
    template_id: &str,
) -> Vec<String> {
    let mut hints = if template_id == APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID {
        vec![
            "Confirm the shared workspace model matches the app's existing tenancy boundaries."
                .to_string(),
            "Review the readonly role mapping before any patch generation or deploy approval."
                .to_string(),
        ]
    } else {
        vec![
            "Confirm the repo already contains a runnable app; the deploy workflow does not modify source code."
                .to_string(),
            "Review the inferred runtime, ports, and start command before approving deploy planning."
                .to_string(),
        ]
    };
    if repo_summary.frameworks.contains(&"sveltekit".to_string())
        || repo_summary.frameworks.contains(&"nextjs".to_string())
    {
        let web_hint = if template_id == APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID {
            "Validate the web entrypoint and session bootstrap path before drafting the integration plan."
        } else {
            "Validate the web entrypoint and production build command before drafting the deploy plan."
        };
        hints.push(web_hint.to_string());
    }
    if inputs.patch_diff.is_some()
        || inputs
            .patch_set
            .as_ref()
            .and_then(|patch| patch.diff.as_ref())
            .is_some()
    {
        hints.push(
            "A patch diff was pre-supplied, so patch planning should focus on validation and approval scope rather than authoring changes."
                .to_string(),
        );
    }
    hints
}

fn planning_assumptions(inputs: &WorkflowInputs, domain_map: &DomainMapArtifact) -> Vec<String> {
    let mut assumptions = vec![
        format!(
            "The target runtime remains {}.",
            inputs
                .deploy_target
                .clone()
                .unwrap_or_else(|| "heyo-sandbox".to_string())
        ),
        "The approved implementation must preserve existing role semantics while adding Heyo auth/org integration."
            .to_string(),
    ];
    if !domain_map.repo.frameworks.is_empty() {
        assumptions.push(format!(
            "Discovery inferred the primary framework/runtime set as {}.",
            domain_map.repo.frameworks.join(", ")
        ));
    }
    if !domain_map.risks.is_empty() {
        assumptions.push(format!(
            "Operator review should resolve the highest-risk discovery finding first: {}.",
            domain_map.risks[0]
        ));
    }
    assumptions
}

fn deploy_planning_assumptions(
    inputs: &WorkflowInputs,
    domain_map: &DomainMapArtifact,
) -> Vec<String> {
    let mut assumptions = vec![format!(
        "The deploy target remains {}.",
        inputs
            .deploy_target
            .clone()
            .unwrap_or_else(|| "heyo-sandbox".to_string())
    )];
    if !domain_map.repo.frameworks.is_empty() {
        assumptions.push(format!(
            "Discovery inferred the app runtime/framework set as {}.",
            domain_map.repo.frameworks.join(", ")
        ));
    }
    if inputs.deploy_config.is_none() {
        assumptions.push(
            "No explicit deployConfig was supplied, so default runtime/image/size values will be used unless the operator revises them before approval."
                .to_string(),
        );
    }
    assumptions.push(
        "The deploy plan should remain generic to the analyzed app and should not encode Colanode-specific product logic in the orchestrator itself."
            .to_string(),
    );
    assumptions
}

fn deploy_plan_changes(
    inputs: &WorkflowInputs,
    domain_map: &DomainMapArtifact,
) -> Vec<PlanChangeArtifact> {
    let mut changes = vec![PlanChangeArtifact {
        id: "package-release-archive".to_string(),
        intent: format!(
            "Archive {} from {} for release packaging",
            display_name_from_domain_map(inputs, domain_map),
            domain_map.repo.root
        ),
    }];

    changes.push(PlanChangeArtifact {
        id: "select-runtime-profile".to_string(),
        intent: deploy_runtime_intent(inputs, &domain_map.repo.frameworks),
    });

    changes.push(PlanChangeArtifact {
        id: "verify-release-candidate".to_string(),
        intent: if inputs.verification_commands.is_empty() {
            "Run the framework-appropriate verification commands discovered from the repo before deployment."
                .to_string()
        } else {
            "Run the operator-supplied verification commands before deployment.".to_string()
        },
    });

    changes.push(PlanChangeArtifact {
        id: "deploy-to-heyo-sandbox".to_string(),
        intent: "Deploy the verified release candidate to a Heyo sandbox with an operator-approved deploy spec."
            .to_string(),
    });

    changes.push(PlanChangeArtifact {
        id: "probe-runtime-health".to_string(),
        intent: if effective_deploy_config(inputs).healthcheck_path.is_some() {
            "Probe the deployed app's healthcheck endpoint and capture a deploy report."
                .to_string()
        } else {
            "Capture a deploy report after release even if no explicit healthcheck path was configured."
                .to_string()
        },
    });

    changes
}

fn display_name_from_domain_map(inputs: &WorkflowInputs, domain_map: &DomainMapArtifact) -> String {
    non_empty(inputs.app_name.clone()).unwrap_or_else(|| {
        domain_map
            .repo
            .top_level_entries
            .first()
            .cloned()
            .unwrap_or_else(|| "the app".to_string())
    })
}

fn deploy_runtime_intent(inputs: &WorkflowInputs, frameworks: &[String]) -> String {
    let deploy_config = effective_deploy_config(inputs);
    let configured_driver = non_empty_option(deploy_config.driver.clone());
    let configured_image = deploy_config
        .image
        .unwrap_or_else(|| DEFAULT_DEPLOY_IMAGE.to_string());
    let configured_driver = if target_backend_is_macos() && configured_driver.is_none() {
        "apple_container or apple_virt (prefer apple_container for OCI/Dockerfile packaging; apple_virt for native archive/VM workflows)".to_string()
    } else if !target_backend_is_macos() && configured_driver.is_none() {
        "firecracker_containerd (default on Linux; runs OCI images and Dockerfile references inside Firecracker microVMs via containerd — preferred when the repo ships a Dockerfile or docker-compose)".to_string()
    } else {
        configured_driver.unwrap_or_else(|| default_deploy_driver())
    };

    if frameworks.iter().any(|framework| framework == "rust") {
        return format!(
            "Use a Rust-oriented release profile and deploy it with the {} driver on {}.",
            configured_driver, configured_image
        );
    }
    if frameworks.iter().any(|framework| framework == "sveltekit")
        || frameworks.iter().any(|framework| framework == "nextjs")
        || frameworks.iter().any(|framework| framework == "node")
    {
        return format!(
            "Use a Node/web release profile and deploy it with the {} driver on {}.",
            configured_driver, configured_image
        );
    }

    format!(
        "Use the repo-derived runtime profile and deploy it with the {} driver on {}.",
        configured_driver, configured_image
    )
}

fn derive_verification_commands(
    _repo_root: &Path,
    inputs: &WorkflowInputs,
    explicit: Option<Vec<String>>,
) -> Vec<String> {
    if let Some(commands) = explicit.filter(|commands| !commands.is_empty()) {
        return commands;
    }
    if let Some(commands) = inputs
        .patch_set
        .as_ref()
        .map(|patch| patch.verification_commands.clone())
        .filter(|commands| !commands.is_empty())
    {
        return commands;
    }
    if !inputs.verification_commands.is_empty() {
        return inputs.verification_commands.clone();
    }

    vec![]
}

fn verification_commands_from_artifacts(
    artifacts: &[orchestration_artifact::Model],
) -> Option<Vec<String>> {
    if let Ok((_, metadata)) = latest_validated_artifact::<PatchSetMetadata>(artifacts, "patch-set")
    {
        if !metadata.verification_commands.is_empty() {
            return Some(metadata.verification_commands);
        }
    }
    if let Ok((_, metadata)) =
        latest_validated_artifact::<IntegrationPlanArtifact>(artifacts, "integration-plan")
    {
        if !metadata.verification_commands.is_empty() {
            return Some(metadata.verification_commands);
        }
    }
    None
}

fn discovery_analysis_contract() -> AnalysisContract {
    AnalysisContract {
        mode: AnalysisMode::InformationRequest,
        request_type: AnalysisRequestType::RepositoryDiscovery,
        execution_policy: "tool_constrained".to_string(),
        allowed_tools: vec![
            "repo.summary".to_string(),
            "repo.key_files".to_string(),
            "repo.framework_detection".to_string(),
        ],
        output_contract: "domain-map.v2".to_string(),
        review_required: true,
    }
}

fn integration_plan_analysis_contract() -> AnalysisContract {
    AnalysisContract {
        mode: AnalysisMode::TransformationRequest,
        request_type: AnalysisRequestType::IntegrationPlanning,
        execution_policy: "tool_constrained".to_string(),
        allowed_tools: vec![
            "artifact.domain_map.read".to_string(),
            "verification.command_derivation".to_string(),
            "deploy.preview.redaction".to_string(),
        ],
        output_contract: "integration-plan.v2".to_string(),
        review_required: true,
    }
}

fn patch_plan_analysis_contract() -> AnalysisContract {
    AnalysisContract {
        mode: AnalysisMode::TransformationRequest,
        request_type: AnalysisRequestType::PatchPlanning,
        execution_policy: "tool_constrained".to_string(),
        allowed_tools: vec![
            "artifact.integration_plan.read".to_string(),
            "artifact.domain_map.read".to_string(),
            "agent.repo_codegen".to_string(),
            "workflow.patch_input.read".to_string(),
            "verification.command_derivation".to_string(),
        ],
        output_contract: "patch-set.v2".to_string(),
        review_required: true,
    }
}

fn deploy_plan_analysis_contract() -> AnalysisContract {
    AnalysisContract {
        mode: AnalysisMode::TransformationRequest,
        request_type: AnalysisRequestType::DeploymentPlanning,
        execution_policy: "tool_constrained".to_string(),
        allowed_tools: vec![
            "artifact.domain_map.read".to_string(),
            "verification.command_derivation".to_string(),
            "deploy.preview.redaction".to_string(),
            "repo.framework_detection".to_string(),
            "agent.repo_analysis".to_string(),
        ],
        output_contract: "deploy-plan.v1".to_string(),
        review_required: true,
    }
}

fn deploy_plan_review_analysis_contract() -> AnalysisContract {
    AnalysisContract {
        mode: AnalysisMode::TransformationRequest,
        request_type: AnalysisRequestType::DeploymentPlanning,
        execution_policy: "tool_constrained".to_string(),
        allowed_tools: vec![
            "artifact.deploy_plan.read".to_string(),
            "repo.framework_detection".to_string(),
            "agent.repo_analysis".to_string(),
        ],
        output_contract: "deploy-plan-review.v1".to_string(),
        review_required: true,
    }
}

fn deploy_preflight_analysis_contract() -> AnalysisContract {
    AnalysisContract {
        mode: AnalysisMode::InformationRequest,
        request_type: AnalysisRequestType::DeploymentPlanning,
        execution_policy: "deterministic".to_string(),
        allowed_tools: vec!["cloud.deploy_preflight".to_string()],
        output_contract: "deploy-preflight.v1".to_string(),
        review_required: false,
    }
}

fn auth_entrypoint_intent(frameworks: &[String]) -> String {
    if frameworks.iter().any(|framework| framework == "sveltekit") {
        return "Add or wire the Heyo-authenticated SvelteKit entrypoint and session bootstrap"
            .to_string();
    }
    if frameworks.iter().any(|framework| framework == "nextjs") {
        return "Add or wire the Heyo-authenticated Next.js entrypoint and session bootstrap"
            .to_string();
    }
    if frameworks.iter().any(|framework| framework == "tauri") {
        return "Add or wire the Heyo-authenticated desktop entrypoint and session bootstrap"
            .to_string();
    }
    "Add or wire the Heyo-authenticated entrypoint for the app".to_string()
}

async fn generate_patch_with_amp(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
    repo_root: &Path,
    inputs: &WorkflowInputs,
    integration_plan: &IntegrationPlanArtifact,
    domain_map: Option<&DomainMapArtifact>,
) -> Result<AmpPatchGeneration, String> {
    let sandbox_dir = create_amp_sandbox(repo_root).await?;
    let generation_result = async {
        let prompt = build_amp_patch_prompt(inputs, integration_plan, domain_map);
        record_message(
            repo,
            &context.thread.id,
            "user",
            truncate_text(&prompt, 16000),
            Some(json!({
                "workflowRunId": context.workflow_run.id,
                "stepRunId": context.step_run.id,
                "stepKey": context.step_run.key,
                "kind": "agent_prompt",
            })),
        )
        .await;
        let agent_started_at: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let agent_tool_input = json!({
            "provider": state.config.agent_provider,
            "model": state.config.agent_model,
            "repoRoot": sandbox_dir.display().to_string(),
            "prompt": truncate_text(&prompt, 12000),
        });
        let agent_tool_call_id = begin_tool_call(
            repo,
            context,
            "agent.repo_codegen",
            agent_tool_input.clone(),
            agent_started_at,
        )
        .await;
        let summary = match run_repo_agent(
            state.config.as_ref(),
            repo_root,
            &sandbox_dir,
            &prompt,
            AgentTaskMode::Mutating,
            Some("patch"),
        )
        .await
        {
            Ok(summary) => {
                finish_tool_call(
                    repo,
                    context,
                    agent_tool_call_id.as_deref(),
                    Some(json!({
                        "summary": truncate_text(&summary, 12000),
                    })),
                    "completed",
                    chrono::Utc::now().into(),
                )
                .await;
                summary
            }
            Err(error) => {
                finish_tool_call(
                    repo,
                    context,
                    agent_tool_call_id.as_deref(),
                    Some(json!({ "error": error })),
                    "failed",
                    chrono::Utc::now().into(),
                )
                .await;
                return Err(error);
            }
        };
        record_message(
            repo,
            &context.thread.id,
            "assistant",
            truncate_text(&summary, 16000),
            Some(json!({
                "workflowRunId": context.workflow_run.id,
                "stepRunId": context.step_run.id,
                "stepKey": context.step_run.key,
                "kind": "agent_summary",
            })),
        )
        .await;
        let diff = collect_amp_patch_diff(&sandbox_dir).await?;
        record_tool_call(
            repo,
            context,
            "git.diff.cached",
            json!({
                "repoRoot": sandbox_dir.display().to_string(),
            }),
            Some(json!({
                "diffBytes": diff.len(),
                "files": diff_files(&diff),
            })),
            "completed",
            chrono::Utc::now().into(),
            Some(chrono::Utc::now().into()),
        )
        .await;
        Ok::<AmpPatchGeneration, String>(AmpPatchGeneration { diff, summary })
    }
    .await;

    let cleanup_result = tokio::fs::remove_dir_all(&sandbox_dir)
        .await
        .map_err(|error| {
            format!(
                "Failed to clean up Amp sandbox {}: {error}",
                sandbox_dir.display()
            )
        });

    match (generation_result, cleanup_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(format!("{error}; {cleanup_error}")),
    }
}

async fn run_amp_structured_analysis<T>(
    state: &AppState,
    repo: &OrchestrationRepository,
    context: &StepExecutionContext,
    repo_root: &Path,
    tool_name: &str,
    prompt: &str,
    output_label: &str,
) -> Result<T, String>
where
    T: DeserializeOwned,
{
    let sandbox_dir = create_amp_sandbox(repo_root).await?;
    let analysis_result = async {
        record_message(
            repo,
            &context.thread.id,
            "user",
            truncate_text(prompt, 16000),
            Some(json!({
                "workflowRunId": context.workflow_run.id,
                "stepRunId": context.step_run.id,
                "stepKey": context.step_run.key,
                "kind": "agent_prompt",
                "toolName": tool_name,
            })),
        )
        .await;

        let agent_started_at: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
        let agent_tool_input = json!({
            "provider": state.config.agent_provider,
            "model": state.config.agent_model,
            "repoRoot": sandbox_dir.display().to_string(),
            "prompt": truncate_text(prompt, 12000),
        });
        let agent_tool_call_id =
            begin_tool_call(repo, context, tool_name, agent_tool_input, agent_started_at).await;
        let raw_output = match run_repo_agent(
            state.config.as_ref(),
            repo_root,
            &sandbox_dir,
            prompt,
            AgentTaskMode::ReadOnly,
            Some(crate::config::purpose_from_step_key(&context.step_run.key)),
        )
        .await
        {
            Ok(output) => {
                finish_tool_call(
                    repo,
                    context,
                    agent_tool_call_id.as_deref(),
                    Some(json!({
                        "output": truncate_text(&output, 12000),
                    })),
                    "completed",
                    chrono::Utc::now().into(),
                )
                .await;
                output
            }
            Err(error) => {
                finish_tool_call(
                    repo,
                    context,
                    agent_tool_call_id.as_deref(),
                    Some(json!({ "error": error })),
                    "failed",
                    chrono::Utc::now().into(),
                )
                .await;
                return Err(error);
            }
        };

        record_message(
            repo,
            &context.thread.id,
            "assistant",
            truncate_text(&raw_output, 16000),
            Some(json!({
                "workflowRunId": context.workflow_run.id,
                "stepRunId": context.step_run.id,
                "stepKey": context.step_run.key,
                "kind": "agent_summary",
                "toolName": tool_name,
            })),
        )
        .await;

        parse_amp_structured_output(output_label, &raw_output)
    }
    .await;

    let cleanup_result = tokio::fs::remove_dir_all(&sandbox_dir)
        .await
        .map_err(|error| {
            format!(
                "Failed to clean up Amp sandbox {}: {error}",
                sandbox_dir.display()
            )
        });

    match (analysis_result, cleanup_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(format!("{error}; {cleanup_error}")),
    }
}

async fn create_amp_sandbox(repo_root: &Path) -> Result<PathBuf, String> {
    let sandbox_dir = std::env::temp_dir().join(format!(
        "heyo-orchestrator-amp-{}",
        uuid::Uuid::new_v4().simple()
    ));
    tokio::fs::create_dir_all(&sandbox_dir)
        .await
        .map_err(|error| {
            format!(
                "Failed to create Amp sandbox {}: {error}",
                sandbox_dir.display()
            )
        })?;

    let archive = archive_repo_root(repo_root).await?;
    extract_archive_into_dir(&archive, &sandbox_dir).await?;
    initialize_git_baseline(&sandbox_dir).await?;
    configure_amp_sandbox_git_excludes(&sandbox_dir).await?;

    Ok(sandbox_dir)
}

async fn extract_archive_into_dir(archive: &[u8], target_dir: &Path) -> Result<(), String> {
    let mut command = Command::new("tar");
    command
        .arg("-xzf")
        .arg("-")
        .arg("-C")
        .arg(target_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| {
        format!(
            "Failed to spawn tar extraction into {}: {error}",
            target_dir.display()
        )
    })?;

    let Some(mut stdin) = child.stdin.take() else {
        return Err("Failed to open stdin for tar extraction".to_string());
    };
    stdin
        .write_all(archive)
        .await
        .map_err(|error| format!("Failed to write repo archive to tar stdin: {error}"))?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .map_err(|error| format!("Failed waiting for tar extraction: {error}"))?;
    if output.status.success() {
        return Ok(());
    }

    Err(command_error("tar", &output.stdout, &output.stderr))
}

async fn initialize_git_baseline(repo_root: &Path) -> Result<(), String> {
    run_command(repo_root, "git", &["init", "-q"], None).await?;
    run_command(repo_root, "git", &["add", "-A"], None).await?;
    run_command(
        repo_root,
        "git",
        &[
            "-c",
            "user.name=Heyo Orchestrator",
            "-c",
            "user.email=orchestrator@heyo.local",
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-gpg-sign",
            "-m",
            "orchestrator baseline",
        ],
        None,
    )
    .await
    .map(|_| ())
}

async fn configure_amp_sandbox_git_excludes(repo_root: &Path) -> Result<(), String> {
    let exclude_path = repo_root.join(".git/info/exclude");
    let mut contents = tokio::fs::read_to_string(&exclude_path)
        .await
        .map_err(|error| {
            format!(
                "Failed to read Amp sandbox git excludes {}: {error}",
                exclude_path.display()
            )
        })?;

    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("# Orchestrator sandbox cache excludes\n");
    for pattern in AMP_SANDBOX_GIT_EXCLUDES {
        contents.push_str(pattern);
        contents.push('\n');
    }

    tokio::fs::write(&exclude_path, contents)
        .await
        .map_err(|error| {
            format!(
                "Failed to update Amp sandbox git excludes {}: {error}",
                exclude_path.display()
            )
        })
}

fn build_amp_patch_prompt(
    inputs: &WorkflowInputs,
    integration_plan: &IntegrationPlanArtifact,
    domain_map: Option<&DomainMapArtifact>,
) -> String {
    let app_name = non_empty(inputs.app_name.clone()).unwrap_or_else(|| "the app".to_string());
    let objective =
        non_empty(inputs.objective.clone()).unwrap_or_else(|| integration_plan.objective.clone());

    let mut prompt = String::new();
    prompt.push_str("Implement the approved Heyo integration directly in this repository working tree. This repository is a temporary sandbox copy, so you should edit files here instead of printing a diff. Do not create commits or branches. Keep the change minimal and production-quality.\n\n");
    prompt.push_str(&format!("App: {app_name}\n"));
    prompt.push_str(&format!("Objective: {objective}\n\n"));
    prompt.push_str("Required behavior:\n");
    prompt.push_str(
        "- Use Heyo users/orgs as the source of truth for app user/workspace membership.\n",
    );
    prompt.push_str("- Map the selected Heyo organization/account to a shared app workspace.\n");
    prompt.push_str("- Map Heyo members to app users.\n");
    prompt.push_str("- Preserve role semantics: Heyo admin -> app admin, Heyo user -> collaborator, Heyo readonly -> guest.\n");
    prompt.push_str("- Make first-login shared workspace creation deterministic and concurrency-safe when the app supports concurrent logins.\n");
    prompt.push_str("- Prefer the existing configuration shape of the app; when applicable, use AUTH_URL, HEYO_ACCOUNT_ID, and HEYO_TENANT_NAME instead of hardcoding tenancy.\n");
    prompt.push_str("- Do not add unrelated refactors or speculative abstractions.\n\n");

    if let Some(domain_map) = domain_map {
        prompt.push_str("Discovery domain map:\n");
        prompt.push_str(&pretty_json(
            &serde_json::to_value(domain_map).unwrap_or_else(|_| json!({})),
        ));
        prompt.push_str("\n\n");
    }

    prompt.push_str("Approved integration plan:\n");
    prompt.push_str(&pretty_json(
        &serde_json::to_value(integration_plan).unwrap_or_else(|_| json!({})),
    ));
    prompt.push_str("\n\n");

    if !integration_plan.verification_commands.is_empty() {
        prompt.push_str("The orchestrator will verify later with these commands:\n");
        for command in &integration_plan.verification_commands {
            prompt.push_str("- ");
            prompt.push_str(command);
            prompt.push('\n');
        }
        prompt.push('\n');
    }

    prompt.push_str("Before finishing, leave your code edits in the working tree. Do not revert them. In your final response, briefly summarize the files you changed and the core behavior you implemented.\n");
    prompt
}

fn workflow_scope_prompt_line(template_id: &str) -> &'static str {
    match template_id {
        APP_DEPLOY_TO_HEYO_TEMPLATE_ID => {
            "- Workflow scope: DEPLOY ONLY. The operator chose the deploy-only workflow, so this run will not modify repository code. Do NOT require Heyo user/workspace integration to be present in the app, do NOT flag missing Heyo login, `/config` capability wiring, role-mapping, or similar app-level Heyo integration as blockers or critical findings, and do NOT put such items in `notDeployed`. Judge the plan purely on whether the app, as it exists in the repo today, can be built and run in a Heyo sandbox.\n"
        }
        APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID => {
            "- Workflow scope: INTEGRATE + DEPLOY. A separate patch step will modify the repo to add Heyo accounts/workspaces integration before this deploy runs, so it is legitimate to flag missing Heyo user-system integration as a gap the patch must cover.\n"
        }
        _ => "",
    }
}

fn build_amp_deploy_plan_prompt(
    inputs: &WorkflowInputs,
    domain_map: &DomainMapArtifact,
    template_id: &str,
) -> String {
    let app_name = non_empty(inputs.app_name.clone()).unwrap_or_else(|| "the app".to_string());
    let objective = non_empty(inputs.objective.clone())
        .unwrap_or_else(|| format!("Deploy {app_name} to a Heyo sandbox"));
    let deploy_preview = redact_deploy_config(&effective_deploy_config(inputs));
    let expected_milestones = deploy_plan_changes(inputs, domain_map);
    let backend_caps = current_backend_caps();
    let target_is_macos = backend_caps.target_os == "macos";
    let example_driver = default_deploy_driver();

    let mut prompt = String::new();
    prompt.push_str("Analyze this repository in read-only mode and produce a deployment plan for a Heyo sandbox. You are running inside a temporary sandbox copy of the repo. Do not edit files, create commits, or print explanations outside the requested JSON.\n\n");
    prompt.push_str(&format!("App: {app_name}\n"));
    prompt.push_str(&format!("Objective: {objective}\n\n"));
    prompt.push_str("Current execution guardrails:\n");
    let scope_line = workflow_scope_prompt_line(template_id);
    if !scope_line.is_empty() {
        prompt.push_str(scope_line);
    }
    prompt.push_str(&format!(
        "- Target backend capabilities for this run are authoritative: target OS `{}`, supported drivers [{}], archive-capable drivers [{}]. Do NOT emit any sandbox driver outside those sets, even if the repo or orchestrator host mentions other backends.\n",
        backend_caps.target_os,
        backend_caps.supported_drivers.join(", "),
        backend_caps.archive_supported_drivers.join(", ")
    ));
    if !target_is_macos {
        prompt.push_str("- Linux target rule: `apple_container` and `apple_virt` are invalid choices. Use Linux drivers only.\n");
    }
    prompt.push_str("- The orchestrator can execute multiple sandbox deployments per workflow run when the plan stays concrete and operationally justified.\n");
    prompt.push_str("- Start by reading repo-local guidance before inferring topology: `AGENTS.md`, root and app-level `README` files, `docs/`, `.claude/skills/`, deploy scripts, `docker-compose*`, and config templates such as `config.example.*` or `.env.example`. When those files describe runtime requirements, treat them as authoritative evidence for the plan.\n");
    prompt.push_str("- One deployed sandbox does not necessarily mean one Unix process. A single sandbox may still be valid if one explicit `startCommand` launches the required cooperating processes via a shell wrapper or lightweight supervisor that is installed/configured in `setupHooks`. Do not mark a plan unsupported solely because the app has both an API and a web process; decide based on whether the repo supports a concrete single-sandbox launch strategy.\n");
    prompt.push_str("- If the app likely requires multiple sandboxes or a component you cannot confidently deploy, be explicit: keep the topology honest, put unsupported pieces in `notDeployed`, and set `executionReadiness.executableByOrchestrator` to false only for real blockers.\n");
    prompt.push_str("- Install self-hostable runtime dependencies (Postgres/pgvector, Redis/Valkey, queues, local object storage) **inside the sandbox** via deterministic `setupHooks`, and launch them alongside the app using a supervisor-style `startCommand` (for example `supervisord`, `s6`, or a `bash -c 'a & b & wait'` wrapper). The sandbox user has passwordless `sudo`, so `setupHooks` may `apt-get install` packages, write config files, initialize databases, and enable extensions (for example `CREATE EXTENSION vector`). Only treat these as external managed dependencies when the operator has explicitly supplied a connection URL through `deployConfig.env`/`envRefs` or in the workflow inputs. Do **not** put self-installable dependencies in `notDeployed` — reserve `notDeployed` for components the sandbox genuinely cannot host (desktop clients, mobile clients, SaaS-only services, or components requiring a separate machine).\n");
    prompt.push_str("- Never invent secret values. Put direct env var names in `envKeys`, and secret/reference names in `envRefs`.\n");
    prompt.push_str("- `envKeys` and `envRefs` are **operator-supplied**: every entry becomes a required workflow input that blocks the deployment until the operator sets `deployConfig.env.<KEY>` (or a matching ref). Therefore list only env vars that genuinely need operator input — secrets, per-environment URLs, account/tenant IDs, custom public origins. Do NOT list well-known deployment defaults such as `NODE_ENV`, `PORT`, `HOST`, `LOG_LEVEL`, `CONFIG` path, etc. in `envKeys`; instead inline them in the `startCommand` (for example `NODE_ENV=production node …`) or export them inside a `setupHook` / generated supervisor config. If a value can be hardcoded to a sensible default for a sandbox, hardcode it rather than asking the operator.\n");
    prompt.push_str("- If the documented startup flow copies or generates runtime config files, reflect that directly in deterministic `setupHooks` and env wiring. For example, copy from a template, write the required Heyo block, and point the app at the generated file with the documented env var.\n");
    prompt.push_str("- For monorepos, prefer the repo-native build and start flow from workspace docs/manifests (for example `turbo`, root workspace scripts, or documented package-manager commands) instead of guessing per-package commands when a root orchestration layer exists.\n");
    if target_is_macos {
        prompt.push_str("- On macOS, `apple_container` (Apple container CLI, good for OCI image / Dockerfile packaging) and `apple_virt` (direct Apple Virtualization Framework, good for archive-backed VM workflows) are both valid. Prefer `apple_container` when the repo ships a Dockerfile or upstream OCI image; prefer `apple_virt` when the deploy is built from a repo archive with a native `startCommand`.\n");
    } else {
        prompt.push_str("- On Linux, the DEFAULT driver is `firecracker_containerd`: it natively boots OCI images (and `dockerfile:<path>` references, which it builds into a containerd image on the backend host) inside per-sandbox Firecracker microVMs via containerd. Prefer it whenever the repo ships a `Dockerfile` or `docker-compose*.yaml` — the stack maps cleanly onto firecracker_containerd with the same per-service topology the compose file describes. `firecracker` (rootfs-based) and `libvirt` (archive-backed VM) are still valid fallbacks, but choose `firecracker_containerd` by default on Linux. Accepted `image` forms for `firecracker_containerd`: a `dockerfile:<path>` reference (built on the backend) or an OCI tag (e.g. `pgvector/pgvector:pg17`, `ubuntu:24.04`, `ghcr.io/org/image:tag`). Bare image names with no `:` tag are not accepted.\n");
        prompt.push_str("- `firecracker` (rootfs-based Firecracker) accepts several image forms: a managed Heyo Firecracker image ID (`img-*`/`im-*`), an absolute `.ext4` path on the backend host, a `dockerfile:<path>` reference the orchestrator builds into a rootfs, or an OCI tag that the backend bakes into a rootfs on the fly. Use it only when the repo specifically needs raw Firecracker semantics (kernel, rootfs) instead of a containerd OCI image; otherwise prefer `firecracker_containerd`.\n");
        prompt.push_str("- **`firecracker_containerd` runtime contract.** The backend launches `sh -c <startCommand>` inside the OCI image and keeps the VM alive separately. That means a non-empty `startCommand` is REQUIRED, the image's baked `CMD`/`ENTRYPOINT` does NOT run automatically, and the shell does NOT inherit image-baked env like `PATH`. If the documented startup recipe depends on Dockerfile env or versioned binary directories, re-export them explicitly or use absolute paths. Keep quoting simple: prefer stable command-line flags or idempotent `setupHooks` over brittle inline config-file rewrites when the service needs bootstrap work.\n");
    }
    prompt.push_str("- **Prefer upstream published OCI images over repo Dockerfiles.** If the repo documents (in README, docs, docker-compose, or CI) an image it publishes to a registry (`ghcr.io/<org>/<app>:<tag>`, `docker.io/<org>/<app>:<tag>`, etc.), use that image **directly** in `image: …`. Only fall back to `dockerfile:<relative path>` when no upstream image is published. Monorepo Dockerfiles frequently use `COPY ../..` paths that only build correctly under a specific `docker buildx bake` configuration and WILL fail in mvm-ctrl's plain build flow — so building from a monorepo Dockerfile is a last resort, not a default. For Colanode specifically: use `ghcr.io/colanode/server:latest` for the server sandbox and `ghcr.io/colanode/web:latest` for the web sandbox; do NOT use `dockerfile:apps/server/Dockerfile` or `dockerfile:apps/web/Dockerfile`.\n");
    if target_is_macos {
        prompt.push_str("- If no upstream image exists AND the only available Dockerfile uses monorepo-relative `COPY ../..` paths, fall back to `driver: apple_virt` with the repo-archive/start-command flow instead of attempting the broken Dockerfile build.\n");
        prompt.push_str("- **`startCommand` IS MANDATORY on `apple_container` — the image's baked `CMD` does NOT run automatically.** The Apple container backend runs every container with `sleep infinity` as its process and then exec's the sandbox's `startCommand` as a sibling. If `startCommand` is null/empty on apple_container, NOTHING runs and the sandbox just idles — ports listen at the host layer but no service is bound inside. **Every apple_container sandbox in the plan MUST emit a non-empty `startCommand` that invokes the service.** This applies even to simple images like `nginx`, `valkey/valkey`, `pgvector/pgvector` — you cannot rely on the image's CMD. To know what to invoke, read the image's Dockerfile in the repo (for example `apps/server/Dockerfile`, or look up the upstream Dockerfile for third-party images) and replicate its `CMD [...]` line as the final `exec` call.\n");
    } else {
        prompt.push_str("- If no upstream image exists AND the only available Dockerfile uses monorepo-relative `COPY ../..` paths, fall back to `driver: libvirt` with the repo-archive/start-command flow instead of attempting the broken Dockerfile build.\n");
    }
    prompt.push_str("- **Do NOT guess paths when replicating the image's CMD.** Use the TOKENS from the Dockerfile's `CMD [...]` line, VERBATIM. If the Dockerfile says `CMD [\"node\", \"apps/server/dist/index.js\"]`, your exec is `exec node apps/server/dist/index.js` — nothing more. Do NOT wrap it in `docker-entrypoint.sh`, `tini`, `gosu`, `su -c`, or any other shim unless you have verified (via the Dockerfile itself) that the image actually ships that binary. Postgres and MySQL images DO ship `docker-entrypoint.sh`; most Node/Python/Go app images (including `ghcr.io/colanode/*`) DO NOT — assume absent unless proven present. If you truly cannot find the image's CMD in repo docs or the upstream Dockerfile, add an `operatorQuestion` asking for it instead of guessing a path.\n");
    if target_is_macos {
        prompt.push_str("- **On `firecracker`** the rules are different: Firecracker DOES run the image's `CMD`, but as a child of a synthetic init shim (see the Firecracker boot-contract bullet below). Leaving `startCommand` null is acceptable on firecracker when the image's CMD is a working root-launchable process; on `apple_container` it is NEVER acceptable.\n");
    }
    prompt.push_str("- **Mirror `docker-compose*.yaml` env vars onto the corresponding sandboxes.** If the repo ships a compose file (e.g. `hosting/docker/docker-compose.yaml`, `docker-compose.yml`) that sets `environment:` on a service, the orchestrator has NO way to guess those values from the image alone — the compose file IS the source of truth for how the stack is expected to boot. For every sandbox you emit, walk the matching compose service and:\n");
    prompt.push_str("    - Replicate every env var. Inline deterministic values (like `POSTGRES_USER=colanode_user`, `POSTGRES_DB=colanode_db`, `NODE_ENV=production`) directly in the `startCommand` (e.g. `sh -lc 'export POSTGRES_USER=colanode_user POSTGRES_PASSWORD=… POSTGRES_DB=…; exec …'`) or in a prepended setupHook. Only promote a value to `envKeys`/`envRefs` when it is a genuine per-operator secret the plan cannot choose safely (e.g. a production DB password); the dev-default values in compose are fine to hardcode.\n");
    if target_is_macos {
        prompt.push_str("    - Translate compose service names into `host.containers.internal:<host_port>` for cross-sandbox URLs. If compose has `POSTGRES_URL=postgres://user:pass@postgres:5432/db` and your plan publishes postgres on host port 15432, the apple_container version of that URL is `postgres://user:pass@host.containers.internal:15432/db`. Do NOT leave the compose hostname (`postgres`) as-is — apple_container sandboxes do not share a user-defined docker network.\n");
    } else {
        prompt.push_str("    - On Linux multi-sandbox plans, keep sibling connection strings on the sibling sandbox key and container port. If compose has `POSTGRES_URL=postgres://user:pass@postgres:5432/db`, preserve that shape; do NOT rewrite it to `host.containers.internal:<host_port>`. Published host ports are for operator/browser access, not sibling-to-sibling traffic.\n");
    }
    prompt.push_str("    - If the compose file mounts a config file (e.g. `./config.json:/app/config.json:ro`, `CONFIG=/app/config.json`), the plan MUST reproduce that config at boot, either by writing it via a setupHook (`cat > /app/config.json <<'EOF' … EOF`) or by listing the required override env vars in `envKeys`. A plan that assumes the image's baked default config is correct — when the repo clearly ships an override config — is a failed deploy.\n");
    prompt.push_str("    - Cross-reference `apps/*/config.example.json` / `.env.example` / `config.example.yaml`. These files enumerate the env vars the app reads at runtime. Anything pattern-matched as `env://<VAR>` or `${<VAR>}` in those configs MUST either be set in the sandbox's env or explicitly listed in `envKeys`/`envRefs`.\n");
    if target_is_macos {
        prompt.push_str("- **Multi-sandbox OCI-image stack (preferred for apps whose dependencies ship as upstream images).** If the app's stack can be assembled from prebuilt OCI images (for example Postgres with pgvector via `pgvector/pgvector:pg17`, Redis/Valkey via `valkey/valkey:8.1`, plus the app's own published image or a Dockerfile), emit **one `apple_container` sandbox per component** instead of installing everything inside one VM via `setupHooks`. Each sandbox uses its own `image`, its own `portMappings`, and talks to siblings over the container network. This is operationally simpler and faster than an archive+setupHooks single-sandbox VM and matches the Apple container CLI's model.\n");
        prompt.push_str("- **`portMappings` over `ports` for `apple_container`.** The Apple container CLI requires explicit host ports (the equivalent of `docker run --publish HOST:CONTAINER`) and does NOT support dynamic-host allocation. Express every published port as `portMappings: [{\"host\": <unique host port>, \"container\": <service port inside the container>}]` with `host > 0`. Pick distinct host ports per sandbox to avoid collisions (for example `15432`, `16379`, `13000`, `14000`). The legacy `ports: [N]` field (container port only, dynamic host) is still accepted for non-Apple backends, but `portMappings` wins when both are present.\n");
        prompt.push_str("- **Cross-sandbox networking.** Sibling sandboxes reach each other via the host's published ports using `host.containers.internal` as the hostname and the published host port (e.g. `POSTGRES_URL=postgres://user:pass@host.containers.internal:15432/db`). The service inside the target sandbox still listens on its container port; clients connect via the host port that maps to it. Express these connection strings in the app sandbox's `env` / `startCommand`, not in `envRefs`/`envKeys`, so they don't block on operator input.\n");
    } else {
        prompt.push_str("- **Multi-sandbox OCI-image stack (preferred for apps whose dependencies ship as upstream images).** If the app's stack can be assembled from prebuilt OCI images, emit one Linux sandbox per component — usually `firecracker_containerd` — instead of collapsing everything into one VM via `setupHooks`. Each sandbox keeps its own image and external `portMappings`, while sibling communication stays on sandbox-key hostnames plus container ports.\n");
        prompt.push_str("- **Use `portMappings` for operator-facing ports, not sibling service discovery.** Assign explicit host ports for the ports humans or browsers must reach from outside the stack, but keep app-to-app URLs on sibling keys like `postgres:5432` and `valkey:6379`. Do not force Linux sibling traffic through `host.containers.internal:<host_port>`.\n");
        prompt.push_str("- **Cross-sandbox networking on Linux.** Sibling sandboxes reach each other via the sibling sandbox `key` and the sibling service's container port (e.g. `POSTGRES_URL=postgres://user:pass@postgres:5432/db`). Reserve published host ports for operator/browser access from outside the stack. Express these connection strings directly in the app sandbox's `env` / `startCommand`, not in `envRefs`/`envKeys`, so they don't block on operator input.\n");
    }
    prompt.push_str("- **Readiness gates in the startCommand for dependent sandboxes.** Sandboxes boot concurrently — there is no implicit ordering between `postgres`, `valkey`, and the app. If an app sandbox calls into a sibling at boot (migrations, connection pool init, Redis init, etc.), its `startCommand` MUST wait for those dependencies to accept connections before exec'ing the app. Portability rules — read carefully:\n");
    prompt.push_str("    - PROHIBITED: `/dev/tcp/<host>/<port>` bash TCP redirect. It is a bash-only builtin and FAILS silently (or loops forever) on any image whose `/bin/sh` is dash, ash, or busybox sh — which includes nearly every minimal OCI image (Alpine, distroless, most Node images). Never emit `/dev/tcp` under `sh -lc`.\n");
    prompt.push_str("    - MOST PORTABLE (Node app images): a `node -e` TCP probe. This always works in any image that has `node` — including `ghcr.io/colanode/server:latest`, which does NOT ship `nc`, `pg_isready`, or `valkey-cli`. Example: `sh -lc 'until node -e \"require(\\\"net\\\").connect(PORT,HOST).on(\\\"connect\\\",()=>process.exit(0)).on(\\\"error\\\",()=>process.exit(1))\" 2>/dev/null; do sleep 1; done'`. Prefer this for any Node server sandbox.\n");
    prompt.push_str("    - FOR NON-NODE IMAGES that ship `nc`: `until nc -z <host> <port> 2>/dev/null; do sleep 1; done` works under any POSIX shell. Do NOT use `nc` on a Node-only image (Colanode's server, most Vite/Next/Node apps) — it won't be installed and the loop will spin forever with command-not-found.\n");
    prompt.push_str("    - ACCEPTABLE when the dep-specific client is known to be present (verify the image's Dockerfile first): `pg_isready` for Postgres-client images, `valkey-cli`/`redis-cli` for Valkey/Redis images.\n");
    prompt.push_str("    - PROHIBITED: assuming `nc`, `pg_isready`, `valkey-cli`, `curl`, or `wget` are present without evidence. If the wait loop's probe binary is missing, `until` runs the probe (exit 127, non-zero) every iteration and loops forever — the sandbox never starts the app. This is the single most common cause of boot hangs.\n");
    prompt.push_str("    - Sibling hostname: in multi-sandbox Firecracker plans the orchestrator injects sibling `/etc/hosts` entries keyed by each sandbox's plan `key` (e.g. `postgres`, `valkey`). Use those names directly as hostnames. Do NOT use `host.containers.internal` for sibling traffic on Firecracker (it resolves to the VM's TAP gateway, not to siblings).\n");
    prompt.push_str("    Do NOT rely on orchestrator-level ordering or on the app's own internal retry/backoff — many apps (Colanode's server included) call `migrate()` and `initRedis()` synchronously on boot without retries. This wait loop is required, not optional, whenever the app sandbox has sibling dependencies in the same plan.\n");
    if target_is_macos {
        prompt.push_str("- **Concrete multi-sandbox template for a Colanode-style stack (app + pgvector Postgres + Valkey + web SPA).** Use this as a direct copy-paste starting point when the repo is Colanode or the deploy config preview names it. Adjust passwords/ports only if the operator has explicitly set them; otherwise use these dev defaults verbatim. `driver: apple_container` on all four sandboxes.\n");
        prompt.push_str("    **postgres sandbox** (image `pgvector/pgvector:pg17`, portMappings `[{host:15432,container:5432}]`):\n");
        prompt.push_str("        - `startCommand`: `sh -lc 'export PATH=/usr/lib/postgresql/17/bin:$PATH POSTGRES_USER=colanode_user POSTGRES_PASSWORD=postgrespass123 POSTGRES_DB=colanode_db POSTGRES_HOST_AUTH_METHOD=md5; exec docker-entrypoint.sh postgres'` — the Postgres image DOES ship `docker-entrypoint.sh` and uses it to run first-boot `initdb` with the POSTGRES_* env. The `PATH` export is MANDATORY on apple_container: `container exec sh -c` does NOT inherit the image's baked ENV, so without prepending `/usr/lib/postgresql/17/bin` (or `/usr/lib/postgresql/16/bin`, matching the image tag), `docker-entrypoint.sh` exits with `initdb: command not found` and the sandbox sits there never initializing its data dir. NEVER omit `POSTGRES_PASSWORD` (the image refuses to initialize without it) and NEVER invoke `postgres` directly on apple_container (bypasses initdb).\n");
        prompt
            .push_str("        - `envKeys`: `[]` (these values are dev defaults, hardcode them)\n");
        prompt.push_str("        - `setupHooks`: `[]`\n");
        prompt.push_str("    **valkey sandbox** (image `valkey/valkey:8.1`, portMappings `[{host:16379,container:6379}]`):\n");
        prompt.push_str("        - `startCommand`: `exec valkey-server --bind 0.0.0.0 --protected-mode no --requirepass your_valkey_password --appendonly yes`\n");
        prompt.push_str("        - `envKeys`/`envRefs`/`setupHooks`: `[]`\n");
        prompt.push_str("    **server sandbox** (image `ghcr.io/colanode/server:latest`, portMappings `[{host:13000,container:3000}]`):\n");
        prompt.push_str("        - `startCommand` (wait for deps via `node -e` TCP probes — ghcr.io/colanode/server:latest is node:22-alpine and ships NEITHER `nc`, `pg_isready`, NOR `valkey-cli`; only `node` is guaranteed present — then export env and hand off to the image's CMD verbatim; the server Dockerfile's `CMD` is `[\"node\",\"apps/server/dist/index.js\"]`):\n");
        prompt.push_str("          `sh -lc 'until node -e \"require(\\\"net\\\").connect(15432,\\\"host.containers.internal\\\").on(\\\"connect\\\",()=>process.exit(0)).on(\\\"error\\\",()=>process.exit(1))\" 2>/dev/null; do sleep 1; done; until node -e \"require(\\\"net\\\").connect(16379,\\\"host.containers.internal\\\").on(\\\"connect\\\",()=>process.exit(0)).on(\\\"error\\\",()=>process.exit(1))\" 2>/dev/null; do sleep 1; done; export POSTGRES_URL=postgres://colanode_user:postgrespass123@host.containers.internal:15432/colanode_db REDIS_URL=redis://:your_valkey_password@host.containers.internal:16379/0 NODE_ENV=production; exec node apps/server/dist/index.js'`\n");
        prompt.push_str("        - The `CREATE EXTENSION vector` bootstrap that earlier revisions ran here is NOT needed — pgvector/pgvector images ship the extension already installed; the server's own migrations run it as part of `app.migrate()` on boot.\n");
        prompt.push_str("        - `envKeys`/`envRefs`: `[]` (all values are dev defaults, hardcode them — do NOT block the deploy on operator input for well-known compose defaults)\n");
        prompt.push_str("        - `healthChecks`: `[]` or a single `{path:\"/health\",expectedStatus:200}` entry. Omit rather than guess if unsure — a wrong healthcheck flaps the sandbox.\n");
        prompt.push_str("    **web sandbox** (image `ghcr.io/colanode/web:latest`, portMappings `[{host:14000,container:80}]`):\n");
        prompt.push_str("        - `startCommand` — writes a nginx config with COOP/COEP headers, then exec's nginx. This is non-negotiable: Colanode's SPA uses an OPFS-backed SQLite-wasm that requires `SharedArrayBuffer`, which the browser only enables for cross-origin-isolated pages. The public `ghcr.io/colanode/web:latest` image does NOT ship these headers; without them the SPA hangs on boot with `Missing SharedArrayBuffer and/or Atomics`. Use this startCommand verbatim:\n");
        prompt.push_str("          `sh -lc 'cat > /etc/nginx/conf.d/default.conf <<NGINX\\nserver {\\n    listen 80;\\n    server_name _;\\n    root /usr/share/nginx/html;\\n    index index.html;\\n    add_header Cross-Origin-Opener-Policy \"same-origin\" always;\\n    add_header Cross-Origin-Embedder-Policy \"require-corp\" always;\\n    add_header Cross-Origin-Resource-Policy \"same-origin\" always;\\n    location / { try_files \\$uri \\$uri/ /index.html; }\\n}\\nNGINX\\nexec nginx -g \"daemon off;\"'`\n");
        prompt.push_str("        - `envKeys`/`envRefs`/`setupHooks`: `[]`\n");
        prompt.push_str("        - **Server registration is a runtime UI flow, not a build-time concern.** Colanode's SPA does NOT auto-discover the API from its own origin; users add a server by entering its `/config` URL in the \"Add server\" dialog. For this deploy the operator will paste `http://localhost:13000/config` (or whichever public URL the operator uses to reach the server sandbox). The SPA then talks cross-origin to that server — no reverse proxy is needed. Do NOT add a proxy sandbox and do NOT try to bake the API origin into the web image.\n");
    } else {
        prompt.push_str("- **Concrete multi-sandbox template for a Colanode-style stack on Linux.** Use `firecracker_containerd` on the Postgres, Valkey, server, and web sandboxes. Keep `portMappings` for operator-facing access, but the server sandbox must talk to siblings via `postgres:5432` and `valkey:6379`, not `host.containers.internal`. The server sandbox's `startCommand` should wait on those sibling ports with the Node TCP probe, export `POSTGRES_URL=postgres://colanode_user:postgrespass123@postgres:5432/colanode_db` and `REDIS_URL=redis://:your_valkey_password@valkey:6379/0`, then `cd /app/apps/server && exec node dist/index.js`. The web sandbox still needs the nginx COOP/COEP header config before `exec nginx -g \"daemon off;\"`.\n");
    }
    prompt.push_str("- **Required runtime defaults for well-known images — treat these as non-negotiable, not operator input.** For any sandbox whose image matches:\n");
    prompt.push_str("    - `pgvector/pgvector:*` or `postgres:*` → keep the database role, password, and database name consistent everywhere they appear (database bootstrap, sibling connection URLs, migration commands). On `firecracker_containerd`, if you invoke `docker-entrypoint.sh`, prepend the versioned PostgreSQL bin directory (for example `/usr/lib/postgresql/17/bin`) to `PATH`; otherwise prefer an explicit `initdb` + `postgres -c ...` bootstrap that creates both the data dir and `/var/run/postgresql` before launch.\n");
    prompt.push_str("    - `valkey/*` or `redis:*` → optional password via `--requirepass <pw>` argument; password is usually hardcoded at the sandbox level (not an operator secret) so that sibling sandboxes can embed it in URLs.\n");
    prompt.push_str("    - `mysql:*` → requires `MYSQL_ROOT_PASSWORD` (or `MYSQL_ALLOW_EMPTY_PASSWORD=yes`) plus `MYSQL_USER`, `MYSQL_PASSWORD`, `MYSQL_DATABASE`.\n");
    prompt.push_str("    - `mongo:*` → `MONGO_INITDB_ROOT_USERNAME`, `MONGO_INITDB_ROOT_PASSWORD`, `MONGO_INITDB_DATABASE`.\n");
    prompt.push_str("    For these images, copy the values the repo's docker-compose file uses (or synthesize sensible dev defaults if the repo has none) and inline them in the startCommand. Do NOT list them in `envKeys` — they are infrastructure defaults, not operator-supplied secrets, and promoting them blocks the deploy for no benefit.\n");
    prompt.push_str("- **Browser reachability for web SPAs.** `host.containers.internal` is not resolvable from a browser, so an SPA sandbox cannot secretly embed that hostname in baked URLs. Two legitimate deploy shapes exist:\n");
    prompt.push_str("    (a) **SPA with its own server-picker UI** (Colanode, Mattermost-style, Matrix clients). The user enters the server URL at runtime in the UI — no proxy, no build-time override required. Just ship web and server as sibling sandboxes on distinct public ports. This is the default for Colanode.\n");
    prompt.push_str("    (b) **SPA that calls a fixed API base** (most Next/Vite apps). If the web image honors a runtime env (`VITE_API_URL`, `NEXT_PUBLIC_API_URL`, `PUBLIC_API_ORIGIN`) list it in `envKeys`, or add a reverse-proxy sandbox only if no such env hook exists.\n");
    prompt.push_str("    Do NOT add a proxy sandbox by default — it's a fallback for apps whose SPA cannot take a runtime server URL, not a universal fix.\n");
    prompt.push_str("- **Health check paths must be real endpoints the service exposes.** Do not default to `/` unless you have evidence (repo code, README, Dockerfile HEALTHCHECK, or upstream docs) that the root path returns 200. Many API servers (Fastify, Express with explicit routes) return 404 on `/`. Prefer `/health`, `/healthz`, `/status`, `/api/health`, or whatever the repo documents; if no health endpoint exists, leave `healthChecks` empty rather than inventing one — a wrong health check flaps the sandbox and masks real boot failures.\n");
    prompt.push_str("- **Firecracker-family boot contract.** On `firecracker`, the image's `ENTRYPOINT`/`CMD` runs under a synthetic init shim rather than as PID 1. On `firecracker_containerd`, the baked `ENTRYPOINT`/`CMD` does not run automatically; your `startCommand` is executed under `sh -c`. In both cases, do not assume Docker-style PID-1 behavior, Dockerfile `USER` handling, or entrypoint side effects. When first-boot initialization matters, own the bootstrap explicitly instead of assuming the upstream image will do it for you.\n");
    prompt.push_str("- **Firecracker-family + Postgres explicit-bootstrap guidance.** Prefer a simple, explicit bootstrap over nested shell edits to `postgresql.conf`/`pg_hba.conf` inside the `startCommand`. A robust recipe is: export the versioned PostgreSQL bin dir into `PATH` (or use absolute paths), create `/var/lib/postgresql/data` and `/var/run/postgresql` with `postgres` ownership, run `initdb` only when `PG_VERSION` is absent, and start postgres with command-line flags like `-c listen_addresses='*'` instead of mutating config files inline. Keep app-specific schema/database creation on the app sandbox or in a follow-up hook, not mixed into the database image bootstrap unless the repo docs require it.\n");
    prompt.push_str("- **Firecracker + Valkey/Redis** — simpler; avoid the upstream entrypoint too. StartCommand `exec valkey-server --bind 0.0.0.0 --protected-mode no --appendonly yes` (or `redis-server ...`) works reliably. No setupHooks needed for basic operation.\n");
    prompt.push_str("- **Firecracker multi-sandbox uses sibling-key hostnames, NOT host.containers.internal.** If the effective deploy config specifies `driver: firecracker` OR `driver: firecracker_containerd`, emit the same multi-sandbox topology as `apple_container` (one sandbox per component with its own image and `portMappings`), but for sibling-to-sibling connection strings use the sibling's **sandbox `key`** as the hostname and the sibling's **container port** (not a published host port). Example: if you name the Postgres sandbox `postgres` (container port 5432), the app sandbox's `POSTGRES_URL` is `postgres://user:pass@postgres:5432/db`, NOT `postgres://user:pass@host.containers.internal:15432/db`. Reason: Firecracker VMs share a flat /30-per-VM network in 172.16.0.0/12 with cross-subnet forwarding open, and the orchestrator injects sibling guest_ips into each VM's /etc/hosts keyed by the plan's sandbox `key`. `portMappings` still apply for ports the operator needs to reach from outside the stack (e.g. the web SPA on the host). `host.containers.internal` is still valid on Firecracker but points at the per-VM TAP gateway, not at sibling-published ports — use it only for reaching the host, not for sibling dependencies.\n");
    prompt.push_str("- **`firecracker_containerd` inherits the Firecracker multi-sandbox rules above** (sibling-key hostnames + container ports). **`startCommand` IS REQUIRED** and it must be written for the backend's `sh -c` execution model, not for native Docker PID-1 semantics. For app images, the safest pattern is usually the exact documented service exec (`exec node ...`, `exec gunicorn ...`, `exec nginx -g \"daemon off;\"`). For images whose upstream startup depends on baked env or entrypoint bootstrap, recreate those prerequisites explicitly inside the command or via `setupHooks` before the final `exec`. `firecracker_containerd` does NOT support host mounts or per-sandbox cpus/memory tuning yet, so do not emit `mounts` for it and keep sizing expressed only via `sizeClass`.\n");
    prompt.push_str("- Prefer the smallest correct topology that can actually run the app.\n\n");

    prompt.push_str("OUTPUT FORMAT — read before generating. Your entire response MUST be a single JSON object and nothing else:\n");
    prompt.push_str(
        "  - Begin with `{` as the FIRST character. End with `}` as the LAST character.\n",
    );
    prompt.push_str("  - No preamble like \"I have enough information...\" or \"Here is the plan:\". No trailing notes.\n");
    prompt.push_str("  - No markdown code fences (```json / ```). The orchestrator parses raw JSON, not fenced blocks.\n");
    prompt.push_str("  - The full object must fit in the response — do not truncate or use \"...\" placeholders. If you need to shorten, tighten prose inside fields rather than omitting structure.\n");
    prompt.push_str("Exact shape:\n");
    prompt.push_str("{\n");
    prompt.push_str("  \"topology\": {\n");
    prompt.push_str("    \"sandboxCount\": 1,\n");
    prompt.push_str("    \"sandboxes\": [\n");
    prompt.push_str("      {\n");
    prompt.push_str("        \"key\": \"primary\",\n");
    prompt.push_str("        \"purpose\": \"Serve the main app\",\n");
    prompt.push_str("        \"runs\": [\"web app\"],\n");
    prompt.push_str("        \"region\": \"EU\",\n");
    prompt.push_str(&format!("        \"driver\": \"{}\",\n", example_driver));
    prompt.push_str("        \"image\": \"ubuntu:24.04\",\n");
    prompt.push_str("        \"sizeClass\": \"small\",\n");
    prompt.push_str("        \"ports\": [3000],\n");
    prompt.push_str("        \"portMappings\": [{ \"host\": 13000, \"container\": 3000 }],\n");
    prompt.push_str("        \"envRefs\": [],\n");
    prompt.push_str("        \"envKeys\": [],\n");
    prompt.push_str("        \"startCommand\": \"NODE_ENV=production npm run start\",\n");
    prompt.push_str("        \"workingDirectory\": \"apps/web\",\n");
    prompt.push_str("        \"setupHooks\": [\"npm install\", \"npm run build\"],\n");
    prompt.push_str("        \"ttlSeconds\": 86400,\n");
    prompt.push_str(
        "        \"healthChecks\": [{ \"path\": \"/health\", \"expectedStatus\": 200 }]\n",
    );
    prompt.push_str("      }\n");
    prompt.push_str("    ],\n");
    prompt.push_str("    \"notDeployed\": [\n");
    prompt.push_str("      {\n");
    prompt.push_str("        \"component\": \"electron-desktop-client\",\n");
    prompt.push_str(
        "        \"reason\": \"end-user desktop binary cannot run on a server sandbox\",\n",
    );
    prompt.push_str("        \"impact\": \"desktop users access the web UI instead\",\n");
    prompt.push_str("        \"blocking\": false\n");
    prompt.push_str("      }\n");
    prompt.push_str("    ]\n");
    prompt.push_str("  },\n");
    prompt.push_str("  \"verificationCommands\": [\"npm test\"],\n");
    prompt
        .push_str("  \"manualVerification\": [\"Confirm the login flow works in the sandbox\"],\n");
    prompt.push_str("  \"assumptions\": [\"The app can run in a single sandbox\"],\n");
    prompt.push_str("  \"risks\": [\"Websocket fanout may need extra infrastructure\"],\n");
    prompt.push_str("  \"operatorQuestions\": [\"Does the repo ship a Dockerfile we should use via `image: dockerfile:<path>` instead of the plain ubuntu image?\"],\n");
    prompt.push_str("  \"executionReadiness\": {\n");
    prompt.push_str("    \"executableByOrchestrator\": true,\n");
    prompt.push_str("    \"blockingReasons\": [],\n");
    prompt.push_str("    \"missingInputs\": []\n");
    prompt.push_str("  }\n");
    prompt.push_str("}\n\n");

    prompt.push_str("Discovery domain map:\n");
    prompt.push_str(&pretty_json(
        &serde_json::to_value(domain_map).unwrap_or_else(|_| json!({})),
    ));
    prompt.push_str("\n\n");

    prompt.push_str("Deploy config inputs (authoritative when present):\n");
    prompt.push_str(&pretty_json(&deploy_preview));
    prompt.push_str("\n\n");

    prompt.push_str("Expected workflow milestones:\n");
    prompt.push_str(&pretty_json(
        &serde_json::to_value(expected_milestones).unwrap_or_else(|_| json!([])),
    ));
    prompt.push_str("\n\n");

    prompt.push_str("Focus on actual runtime needs you can justify from the repo: topology, software/runtime, ports, start command, env requirements, health checks, generated config files, and what should not be deployed yet. If repo docs describe a working startup recipe, translate that recipe into the plan instead of replacing it with a more generic guess. Output JSON only.\n");
    prompt
}

fn build_amp_deploy_plan_revision_prompt(
    inputs: &WorkflowInputs,
    domain_map: &DomainMapArtifact,
    previous_plan: &DeployPlanArtifact,
    previous_review: &DeployPlanReviewArtifact,
    question_prompt: Option<&DeployQuestionPrompt>,
    operator_response: &str,
    template_id: &str,
) -> String {
    let app_name = non_empty(inputs.app_name.clone()).unwrap_or_else(|| "the app".to_string());
    let objective = non_empty(inputs.objective.clone())
        .unwrap_or_else(|| format!("Deploy {app_name} to a Heyo sandbox"));
    let deploy_preview = redact_deploy_config(&effective_deploy_config(inputs));
    let backend_caps = current_backend_caps();
    let target_is_macos = backend_caps.target_os == "macos";

    let mut prompt = String::new();
    prompt.push_str("Revise the existing Heyo deploy plan using the operator feedback and repo-local deployment guidance. You are running in a temporary sandbox copy of the repo in read-only mode. Do not edit files. Return JSON only.\n\n");
    prompt.push_str(&format!("App: {app_name}\n"));
    prompt.push_str(&format!("Objective: {objective}\n\n"));
    prompt.push_str("Revision rules:\n");
    let scope_line = workflow_scope_prompt_line(template_id);
    if !scope_line.is_empty() {
        prompt.push_str(scope_line);
    }
    prompt.push_str(&format!(
        "- Target backend capabilities for this run are authoritative: target OS `{}`, supported drivers [{}], archive-capable drivers [{}]. Remove or rewrite any sandbox that uses a driver outside those sets.\n",
        backend_caps.target_os,
        backend_caps.supported_drivers.join(", "),
        backend_caps.archive_supported_drivers.join(", ")
    ));
    prompt.push_str("- Re-read repo-local deployment evidence before revising: `AGENTS.md`, README files, docs, `.claude/skills/`, config templates, deploy scripts, and compose files. Treat them as authoritative when they describe the real startup/config flow.\n");
    prompt.push_str("- Incorporate the operator response concretely. If it points to a repo doc, startup command, generated config file, Postgres/Redis co-location or external URL, or monorepo build command, reflect that directly in the revised plan.\n");
    prompt.push_str("- Default to installing self-hostable dependencies (Postgres/pgvector, Redis/Valkey, queues, local object storage) inside the sandbox via `setupHooks` and running them alongside the app through a supervisor-style `startCommand`. Treat them as external only when the operator has supplied a connection URL via `deployConfig.env`/`envRefs` or workflow inputs. Never put a self-installable dependency in `notDeployed`.\n");
    prompt.push_str("- `envKeys` and `envRefs` are operator-supplied: every entry becomes a required workflow input that blocks the deployment. Drop any entry the operator does not genuinely need to set (for example `NODE_ENV`, `PORT`, `HOST`, `LOG_LEVEL`, `CONFIG` path) and instead inline such defaults in the `startCommand` or `setupHooks`. Only keep `envKeys`/`envRefs` entries for real secrets, per-environment URLs, account/tenant IDs, or custom public origins.\n");
    if target_is_macos {
        prompt.push_str("- For apps whose stack ships as OCI images, prefer a multi-sandbox `apple_container` topology with one sandbox per component, each with explicit `portMappings: [{host, container}]` (host > 0) and cross-sandbox URLs that point at `host.containers.internal:<host_port>`. Do not rely on dynamic host-port allocation with `apple_container` — the Apple container CLI does not support it.\n");
        prompt.push_str("- If the plan has an app sandbox depending on sibling sandboxes (Postgres, Redis/Valkey, etc.), the app's `startCommand` MUST wait for those dependencies to accept connections before exec'ing the app. Use a bash wait loop like `sh -lc 'until nc -z host.containers.internal <host_port>; do sleep 1; done; …; exec <app>'`. Sandboxes start concurrently with no implicit ordering, and apps that call `migrate()` / `initRedis()` at boot without retries will crash-loop if the dependency isn't ready.\n");
    } else {
        prompt.push_str("- For apps whose stack ships as OCI images on Linux, prefer a multi-sandbox `firecracker_containerd` topology with one sandbox per component. Keep `portMappings` for operator-facing access, but sibling connection strings must stay on sibling sandbox keys plus container ports (for example `postgres:5432`, `valkey:6379`).\n");
        prompt.push_str("- If the plan has an app sandbox depending on sibling sandboxes (Postgres, Redis/Valkey, etc.), the app's `startCommand` MUST wait for those dependencies to accept connections before exec'ing the app. On Linux multi-sandbox plans, wait on the sibling hostname and container port (for example `postgres 5432`), not `host.containers.internal <host_port>`. Sandboxes start concurrently with no implicit ordering, and apps that call `migrate()` / `initRedis()` at boot without retries will crash-loop if the dependency isn't ready.\n");
        prompt.push_str("- On Linux, remember the `firecracker_containerd` shell contract while revising: `startCommand` runs via `sh -c`, not the image's native PID 1, so if the startup recipe depends on image-baked env like `PATH`, Docker entrypoint side effects, or generated runtime directories, the revised plan must recreate those explicitly.\n");
    }
    prompt.push_str("- Keep the topology as small as possible, but do not collapse the plan dishonestly. One sandbox may still be valid when one explicit `startCommand` launches the required cooperating processes.\n");
    prompt.push_str("- Keep `executionReadiness.executableByOrchestrator` false only for blockers that still remain after applying the operator response and repo guidance.\n");
    prompt.push_str("- Preserve unanswered questions only when the repo and operator response still leave something materially ambiguous.\n\n");

    if let Some(question_prompt) = question_prompt {
        prompt.push_str("Outstanding deploy questions that triggered this revision:\n");
        prompt.push_str(&question_prompt.summary_markdown);
        prompt.push_str("\n\n");
    }

    prompt.push_str("Operator response:\n");
    prompt.push_str(operator_response.trim());
    prompt.push_str("\n\n");

    prompt.push_str("Discovery domain map:\n");
    prompt.push_str(&pretty_json(
        &serde_json::to_value(domain_map).unwrap_or_else(|_| json!({})),
    ));
    prompt.push_str("\n\n");

    prompt.push_str("Deploy config inputs (authoritative when present):\n");
    prompt.push_str(&pretty_json(&deploy_preview));
    prompt.push_str("\n\n");

    prompt.push_str("Previous deploy plan:\n");
    prompt.push_str(&pretty_json(
        &serde_json::to_value(previous_plan).unwrap_or_else(|_| json!({})),
    ));
    prompt.push_str("\n\n");

    prompt.push_str("Previous deploy plan review:\n");
    prompt.push_str(&pretty_json(
        &serde_json::to_value(previous_review).unwrap_or_else(|_| json!({})),
    ));
    prompt.push_str("\n\n");

    prompt.push_str("Return the same JSON shape as the original deploy plan: topology, verificationCommands, manualVerification, assumptions, risks, operatorQuestions, and executionReadiness. Output JSON only.\n");
    prompt
}

fn build_amp_deploy_plan_review_prompt(
    inputs: &WorkflowInputs,
    deploy_plan: &DeployPlanArtifact,
    template_id: &str,
) -> String {
    let app_name = non_empty(inputs.app_name.clone()).unwrap_or_else(|| "the app".to_string());
    let objective = non_empty(inputs.objective.clone())
        .unwrap_or_else(|| format!("Deploy {app_name} to a Heyo sandbox"));
    let deploy_preview = redact_deploy_config(&effective_deploy_config(inputs));
    let backend_caps = current_backend_caps();
    let target_is_macos = backend_caps.target_os == "macos";

    let mut prompt = String::new();
    prompt.push_str("Review the proposed Heyo deploy plan for this repository in read-only mode. You are running in a temporary sandbox copy of the repo. Do not edit files. Return JSON only and no prose outside the JSON.\n\n");
    prompt.push_str(&format!("App: {app_name}\n"));
    prompt.push_str(&format!("Objective: {objective}\n\n"));
    prompt.push_str("Review criteria:\n");
    let scope_line = workflow_scope_prompt_line(template_id);
    if !scope_line.is_empty() {
        prompt.push_str(scope_line);
    }
    prompt.push_str(&format!(
        "- Target backend capabilities for this run are authoritative: target OS `{}`, supported drivers [{}], archive-capable drivers [{}]. Any sandbox driver outside those sets is a blocking issue.\n",
        backend_caps.target_os,
        backend_caps.supported_drivers.join(", "),
        backend_caps.archive_supported_drivers.join(", ")
    ));
    prompt.push_str("- Validate the plan against repo-local deployment evidence first: `AGENTS.md`, README files, docs, `.claude/skills/`, config templates, deploy scripts, and compose files. Treat those instructions as authoritative when they describe how the app is configured or launched.\n");
    prompt.push_str("- Check whether the topology, ports, env requirements, start command, working directory, setup hooks, and health checks match the repo.\n");
    prompt.push_str("- Do not block a single-sandbox plan merely because the app has multiple cooperating processes if the plan gives one concrete `startCommand` that launches them together and the repo supports that approach.\n");
    prompt.push_str("- Prefer `needs_revision` over `block` when the repo already documents a workable setup but the proposed plan failed to translate that setup faithfully (for example, missing generated config files, missing root monorepo build steps, or missing external service env refs).\n");
    prompt.push_str("- Check whether the plan is actually executable on the current target backend, including multi-sandbox deployments and any backend-specific packaging the repo implies.\n");
    if target_is_macos {
        prompt.push_str("- For any sandbox using `driver: apple_container`, require explicit `portMappings: [{host, container}]` with `host > 0` for every port the stack needs to reach. `apple_container` does NOT allocate host ports dynamically, so plans relying on `ports: [N]` alone (without `portMappings`) are incomplete. Flag a missing or zero-host `portMappings` entry as a finding. Also verify sibling-sandbox connection strings use `host.containers.internal:<host_port>` and that host ports across sandboxes don't collide.\n");
    }
    prompt.push_str("- On Linux, `firecracker_containerd` is the preferred default driver: it boots OCI images (and `dockerfile:<path>` references, built on the backend host) inside Firecracker microVMs via containerd. Treat a plan that picks `firecracker_containerd` as preferred when the repo ships a `Dockerfile` or `docker-compose*.yaml`, as long as the `image` is a `dockerfile:<path>` reference or an OCI tag (e.g. `ghcr.io/colanode/server:latest`, `pgvector/pgvector:pg17`). Bare image names with no colon/tag, absolute `.ext4` paths, and managed Heyo image IDs (`img-*`/`im-*`) are NOT valid for `firecracker_containerd` — flag as needs_revision.\n");
    prompt.push_str("- **`firecracker_containerd` requires a non-empty `startCommand` on every sandbox.** The driver executes that command via `sh -c` and does NOT run the image's baked ENTRYPOINT/CMD automatically, so a null/empty startCommand means the container idles and the service never starts. Flag any such sandbox as `needs_revision`. Also flag plans that blindly assume image-baked env is present inside that shell. For postgres-like images (`postgres:*`, `pgvector/pgvector:*`), the command must either export the versioned `/usr/lib/postgresql/<major>/bin` path before invoking `docker-entrypoint.sh` / `initdb` / `postgres`, or use explicit absolute paths and its own bootstrap.\n");
    prompt.push_str("- For any sandbox using `driver: firecracker`, `image` may be a managed Heyo image ID (`img-*`/`im-*`), an absolute `.ext4` path on the backend host, a `dockerfile:<path>` reference that the orchestrator builds into a rootfs, or an OCI tag (e.g. `ubuntu:24.04`) that the backend bakes into a rootfs on the fly. Bare words with no colon/tag are treated as local image names and only work if pre-built on the host — flag those as needs_revision.\n");
    prompt.push_str("- For `driver: firecracker` OR `driver: firecracker_containerd`, multi-sandbox topology is supported. Sibling connection strings MUST use the sibling's sandbox `key` as the hostname with the sibling's **container** port (e.g. `postgres://…@postgres:5432/db`), NOT `host.containers.internal:<host_port>`. A firecracker plan that uses `host.containers.internal:<host_port>` for sibling links is `needs_revision` — ask for sibling-key hostnames + container ports. `portMappings` on firecracker still apply for ports reached from outside the stack (e.g. the web SPA), but are not needed for sibling-to-sibling traffic.\n");
    prompt.push_str("- For postgres-like images on `firecracker_containerd`, prefer plans that keep bootstrap explicit and quoting simple: create required runtime directories, use command-line flags like `postgres -c listen_addresses='*'` instead of editing config files inline, and keep database credentials/database names consistent with every sibling connection URL or migration step that references them. Treat mismatched credentials or a missing PATH export for `docker-entrypoint.sh` as `needs_revision`.\n");
    prompt.push_str("- If a sandbox uses `image: dockerfile:<path>` pointing at a monorepo Dockerfile that contains `COPY ../..` paths, flag it as a critical finding: mvm-ctrl's plain build flow does not support context-flattening, so the build WILL fail. Recommend the published upstream OCI image instead (for example `ghcr.io/colanode/server:latest` / `ghcr.io/colanode/web:latest` for Colanode), or fall back to an archive-backed VM flow (`apple_virt` on macOS, `libvirt` on Linux) with an explicit startCommand.\n");
    prompt.push_str("- For multi-sandbox plans where an app sandbox depends on sibling sandboxes (Postgres, Redis/Valkey, etc.), verify the app's `startCommand` contains an explicit wait loop (`until nc -z … ; do sleep 1; done` or equivalent `pg_isready` / `valkey-cli ping`) that gates the app launch on sibling port-readiness. The wait loop MUST be POSIX-portable under `sh -lc`: flag any startCommand that uses bash-only `/dev/tcp/<host>/<port>` redirects without wrapping in `bash -lc` as `needs_revision` — `/dev/tcp` silently breaks on dash/ash/busybox sh, which is what most minimal OCI images ship. If the `startCommand` goes straight to `node …` / equivalent without such a gate, treat it as `needs_revision`, not `block` — a missing wait loop is fixable by revising the startCommand. Reserve `block` for plans that are genuinely unsafe or fundamentally unworkable (for example `apple_container` with no host ports, or `dockerfile:` pointing at a Dockerfile known to fail the build). Host health checks at the infra layer are nice-to-have but not a substitute for the wait loop, and their absence should be at most a warning.\n");
    prompt.push_str(
        "- Recommend `approve` only when the plan looks executable and materially complete.\n",
    );
    prompt.push_str("- Use `needs_revision` for incomplete or uncertain plans. Use `block` for unsafe or clearly incorrect plans.\n\n");

    prompt.push_str("OUTPUT FORMAT — read before generating. Your entire response MUST be a single JSON object and nothing else:\n");
    prompt.push_str(
        "  - Begin with `{` as the FIRST character. End with `}` as the LAST character.\n",
    );
    prompt.push_str("  - No preamble like \"I have enough information...\" or \"Here is the plan:\". No trailing notes.\n");
    prompt.push_str("  - No markdown code fences (```json / ```). The orchestrator parses raw JSON, not fenced blocks.\n");
    prompt.push_str("  - The full object must fit in the response — do not truncate or use \"...\" placeholders. If you need to shorten, tighten prose inside fields rather than omitting structure.\n");
    prompt.push_str("Exact shape:\n");
    prompt.push_str("{\n");
    prompt.push_str("  \"recommendation\": \"approve\",\n");
    prompt.push_str("  \"summary\": \"Short review summary\",\n");
    prompt.push_str("  \"findings\": [\n");
    prompt.push_str("    {\n");
    prompt.push_str("      \"severity\": \"warning\",\n");
    prompt.push_str("      \"title\": \"Missing health check\",\n");
    prompt.push_str("      \"message\": \"The main web process exposes a port but no health check was identified.\",\n");
    prompt.push_str("      \"sandboxKey\": \"primary\"\n");
    prompt.push_str("    }\n");
    prompt.push_str("  ],\n");
    prompt.push_str(
        "  \"unansweredQuestions\": [\"Should websocket workers be deployed separately?\"]\n",
    );
    prompt.push_str("}\n\n");

    prompt.push_str("Deploy config inputs:\n");
    prompt.push_str(&pretty_json(&deploy_preview));
    prompt.push_str("\n\n");

    prompt.push_str("Proposed deploy plan:\n");
    prompt.push_str(&pretty_json(
        &serde_json::to_value(deploy_plan).unwrap_or_else(|_| json!({})),
    ));
    prompt.push_str("\n\n");

    prompt.push_str("Be skeptical and explicit about omissions, unsupported topology, missing ports, or missing env requirements. Output JSON only.\n");
    prompt
}

async fn collect_amp_patch_diff(repo_root: &Path) -> Result<String, String> {
    run_command(repo_root, "git", &["add", "-A"], None).await?;
    let output = run_command(
        repo_root,
        "git",
        &[
            "diff",
            "--cached",
            "--binary",
            "--full-index",
            "--no-color",
            "HEAD",
        ],
        None,
    )
    .await?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn validated_structured_output<T>(label: &str, value: &T) -> Result<Value, String>
where
    T: Serialize + DeserializeOwned,
{
    let serialized = serde_json::to_value(value)
        .map_err(|error| format!("Failed to serialize {label} structured output: {error}"))?;
    serde_json::from_value::<T>(serialized.clone())
        .map_err(|error| format!("{label} structured output failed validation: {error}"))?;
    Ok(serialized)
}

fn artifact_metadata<T>(
    artifact: &orchestration_artifact::Model,
    expected_kind: &str,
) -> Result<T, String>
where
    T: DeserializeOwned,
{
    if artifact.kind != expected_kind {
        return Err(format!(
            "Expected artifact kind {} but found {}",
            expected_kind, artifact.kind
        ));
    }
    let metadata = artifact.metadata.clone().ok_or_else(|| {
        format!(
            "Artifact {} ({expected_kind}) is missing structured metadata",
            artifact.id
        )
    })?;
    serde_json::from_value(metadata).map_err(|error| {
        format!(
            "Artifact {} ({expected_kind}) failed structured metadata validation: {error}",
            artifact.id
        )
    })
}

fn latest_validated_artifact<'a, T>(
    artifacts: &'a [orchestration_artifact::Model],
    kind: &str,
) -> Result<(&'a orchestration_artifact::Model, T), String>
where
    T: DeserializeOwned,
{
    let artifact = latest_artifact(artifacts, kind)
        .ok_or_else(|| format!("Workflow is missing required {kind} artifact"))?;
    let metadata = artifact_metadata::<T>(artifact, kind)?;
    Ok((artifact, metadata))
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

fn latest_step_outputs(artifacts: &[orchestration_artifact::Model], kind: &str) -> Option<Value> {
    latest_artifact(artifacts, kind).and_then(|artifact| artifact.metadata.clone())
}

fn deployment_id_from_artifacts(artifacts: &[orchestration_artifact::Model]) -> Option<String> {
    latest_artifact(artifacts, "deploy-spec")
        .and_then(|artifact| artifact.metadata.as_ref())
        .and_then(|metadata| {
            metadata
                .get("primaryDeploymentId")
                .or_else(|| metadata.get("deploymentId"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn redact_deploy_config(config: &DeployConfig) -> Value {
    json!({
        "region": config.region,
        "driver": config.driver,
        "image": config.image,
        "ports": config.ports,
        "healthcheckPath": config.healthcheck_path,
        "envRefs": config.env_refs,
        "envKeys": env_keys(config),
        "startCommand": config.start_command,
        "workingDirectory": config.working_directory,
        "sizeClass": config.size_class,
        "ttlSeconds": config.ttl_seconds,
    })
}

fn env_keys(config: &DeployConfig) -> Vec<String> {
    config
        .env
        .as_ref()
        .map(|env| {
            let mut keys = env.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            keys
        })
        .unwrap_or_default()
}

fn health_checks(config: &DeployConfig) -> Vec<Value> {
    config
        .healthcheck_path
        .as_ref()
        .map(|path| {
            vec![json!({
                "path": path,
                "expectedStatus": config.expected_status.unwrap_or(200),
            })]
        })
        .unwrap_or_default()
}

fn diff_files(diff: &str) -> Vec<String> {
    let mut files = BTreeSet::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            files.insert(path.to_string());
        }
    }
    files.into_iter().collect()
}

async fn run_command(
    repo_root: &Path,
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<std::process::Output, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if input.is_some() {
        command.stdin(Stdio::piped());
    }

    let mut child = command.spawn().map_err(|error| {
        format!(
            "Failed to spawn {} in {}: {error}",
            program,
            repo_root.display()
        )
    })?;

    if let Some(input) = input {
        let Some(mut stdin) = child.stdin.take() else {
            return Err(format!("Failed to open stdin for {program}"));
        };
        stdin
            .write_all(input)
            .await
            .map_err(|error| format!("Failed to write stdin for {program}: {error}"))?;
        drop(stdin);
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|error| format!("Failed waiting for {program}: {error}"))?;
    if output.status.success() {
        return Ok(output);
    }

    Err(command_error(program, &output.stdout, &output.stderr))
}

async fn run_git_apply(repo_root: &Path, args: &[&str], input: &str) -> Result<(), String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| {
        format!(
            "Failed to spawn git apply in {}: {error}",
            repo_root.display()
        )
    })?;

    let Some(mut stdin) = child.stdin.take() else {
        return Err("Failed to open stdin for git apply".to_string());
    };
    stdin
        .write_all(input.as_bytes())
        .await
        .map_err(|error| format!("Failed to write patch to git apply stdin: {error}"))?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .map_err(|error| format!("Failed waiting for git apply: {error}"))?;

    if output.status.success() {
        return Ok(());
    }

    Err(command_error("git apply", &output.stdout, &output.stderr))
}

async fn run_shell_command(repo_root: &Path, command: &str) -> Result<Value, String> {
    let output = Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| format!("Failed to run `{command}`: {error}"))?;

    if output.status.success() {
        return Ok(json!({
            "command": command,
            "status": "passed",
            "exitCode": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }));
    }

    Err(format!(
        "Verification command `{command}` failed: {}",
        command_error(command, &output.stdout, &output.stderr)
    ))
}

async fn archive_repo_root(repo_root: &Path) -> Result<Vec<u8>, String> {
    archive_repo_root_with_options(repo_root, false).await
}

async fn archive_repo_root_with_options(
    repo_root: &Path,
    include_git: bool,
) -> Result<Vec<u8>, String> {
    let staging_root = if include_git {
        let staging_root =
            std::env::temp_dir().join(format!("heyo-ci-archive-{}", uuid::Uuid::new_v4().simple()));
        let checkout_dir = staging_root.join("repo");
        tokio::fs::create_dir_all(&staging_root)
            .await
            .map_err(|error| {
                format!(
                    "Failed to create CI archive staging dir {}: {error}",
                    staging_root.display()
                )
            })?;
        let output = Command::new("git")
            .arg("clone")
            .arg("--depth")
            .arg("50")
            .arg("--no-hardlinks")
            .arg("--no-local")
            .arg(repo_root)
            .arg(&checkout_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|error| {
                format!(
                    "Failed to clone {} for CI archive: {error}",
                    repo_root.display()
                )
            })?;
        if !output.status.success() {
            let _ = tokio::fs::remove_dir_all(&staging_root).await;
            return Err(command_error("git clone", &output.stdout, &output.stderr));
        }
        Some(staging_root)
    } else {
        None
    };
    let archive_root = staging_root
        .as_ref()
        .map(|path| path.join("repo"))
        .unwrap_or_else(|| repo_root.to_path_buf());

    let mut command = Command::new("tar");
    command.env("COPYFILE_DISABLE", "1");
    command.arg("-czf").arg("-");
    if !include_git {
        command.arg("--exclude=.git");
    }
    let output = command
        .arg("--exclude=node_modules")
        .arg("--exclude=target")
        .arg("--exclude=dist")
        .arg("--exclude=build")
        .arg("--exclude=._*")
        .arg("-C")
        .arg(&archive_root)
        .arg(".")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| format!("Failed to archive {}: {error}", archive_root.display()))?;

    if let Some(staging_root) = staging_root {
        let _ = tokio::fs::remove_dir_all(staging_root).await;
    }

    if !output.status.success() {
        return Err(command_error("tar", &output.stdout, &output.stderr));
    }

    Ok(output.stdout)
}

fn generate_deployed_sandbox_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("dep-{}", &id[..8])
}

fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn command_error(command: &str, stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "command produced no output".to_string()
    };
    format!("{command} failed: {detail}")
}

fn parse_amp_structured_output<T>(label: &str, output: &str) -> Result<T, String>
where
    T: DeserializeOwned,
{
    let candidates = amp_json_candidates(output);
    let mut errors = Vec::new();
    for candidate in candidates {
        match serde_json::from_str::<T>(&candidate) {
            Ok(value) => return Ok(value),
            Err(error) => errors.push(error.to_string()),
        }
    }

    let parse_detail = if errors.is_empty() {
        "no JSON object was found in the Amp response".to_string()
    } else {
        errors.join(" | ")
    };
    Err(format!(
        "Amp {label} output was not valid structured JSON: {parse_detail}. Raw output: {}",
        truncate_text(output, 2000)
    ))
}

fn amp_json_candidates(output: &str) -> Vec<String> {
    let trimmed = output.trim_start_matches('\u{feff}').trim();
    let mut candidates = Vec::new();

    if !trimmed.is_empty() {
        candidates.push(trimmed.to_string());
    }
    if let Some(block) = fenced_code_block(trimmed, "json") {
        candidates.push(block);
    }
    if let Some(block) = fenced_code_block(trimmed, "") {
        candidates.push(block);
    }
    candidates.extend(embedded_json_objects(trimmed));

    let mut deduped = Vec::new();
    for candidate in candidates {
        if deduped.iter().all(|existing| existing != &candidate) {
            deduped.push(candidate);
        }
    }
    deduped
}

fn fenced_code_block(output: &str, language: &str) -> Option<String> {
    let start_marker = if language.is_empty() {
        "```"
    } else {
        "```json"
    };
    let start = output.find(start_marker)?;
    let after_start = &output[start + start_marker.len()..];
    let end = after_start.find("```")?;
    let block = after_start[..end].trim();
    if block.is_empty() {
        None
    } else {
        Some(block.to_string())
    }
}

fn embedded_json_objects(output: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut object_starts = Vec::new();
    let mut in_string = false;
    let mut escape_next = false;

    for (index, ch) in output.char_indices() {
        if in_string {
            if escape_next {
                escape_next = false;
                continue;
            }
            match ch {
                '\\' => escape_next = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => object_starts.push(index),
            '}' => {
                if let Some(start) = object_starts.pop() {
                    candidates.push(output[start..index + ch.len_utf8()].to_string());
                }
            }
            _ => {}
        }
    }

    candidates
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(kind: &str, metadata: Value) -> orchestration_artifact::Model {
        orchestration_artifact::Model {
            id: format!("artifact-{kind}"),
            thread_id: "thread-1".to_string(),
            workflow_run_id: Some("workflow-1".to_string()),
            step_run_id: Some("step-1".to_string()),
            kind: kind.to_string(),
            format: "json".to_string(),
            schema_version: 2,
            title: None,
            uri: None,
            body: None,
            metadata: Some(metadata),
            created_at: chrono::Utc::now().into(),
        }
    }

    #[test]
    fn validates_domain_map_structured_output() {
        let artifact = DomainMapArtifact {
            analysis: discovery_analysis_contract(),
            repo: DiscoveryRepoArtifact {
                root: "/tmp/repo".to_string(),
                top_level_entries: vec!["src".to_string()],
                key_files: vec!["Cargo.toml".to_string()],
                frameworks: vec!["rust".to_string()],
                package_managers: vec![],
                file_count: 12,
            },
            workspace_mapping: Some(WorkspaceMappingArtifact {
                heyo_org: "sharedWorkspace".to_string(),
            }),
            user_mapping: Some(UserMappingArtifact {
                heyo_member: "appUser".to_string(),
            }),
            role_mapping: Some(RoleMappingArtifact {
                admin: "admin".to_string(),
                user: "collaborator".to_string(),
                readonly: "guest".to_string(),
            }),
            objective: "Integrate auth".to_string(),
            risks: vec!["risk".to_string()],
            operator_review_hints: vec!["hint".to_string()],
        };

        let value = validated_structured_output("domain-map", &artifact).unwrap();
        assert_eq!(
            value["analysis"]["requestType"],
            Value::String("repository_discovery".to_string())
        );
    }

    #[test]
    fn verification_commands_use_validated_patch_metadata_first() {
        let patch_metadata = validated_structured_output(
            "patch-set metadata",
            &PatchSetMetadata {
                analysis: patch_plan_analysis_contract(),
                based_on_artifact_ids: vec!["artifact-plan".to_string()],
                files: vec!["src/main.rs".to_string()],
                has_diff: true,
                verification_commands: vec!["cargo test".to_string()],
                deploy_config: json!({}),
                notes: "ready".to_string(),
                application_strategy: "deterministic_git_apply".to_string(),
            },
        )
        .unwrap();
        let plan_metadata = validated_structured_output(
            "integration-plan",
            &IntegrationPlanArtifact {
                analysis: integration_plan_analysis_contract(),
                repo_root: "/tmp/repo".to_string(),
                objective: "Integrate auth".to_string(),
                depends_on_artifact_ids: vec!["artifact-domain".to_string()],
                assumptions: vec![],
                changes: vec![],
                verification_commands: vec!["bun run build".to_string()],
                verification: vec!["bun run build".to_string()],
                deploy_config_preview: json!({}),
            },
        )
        .unwrap();

        let artifacts = vec![
            artifact("integration-plan", plan_metadata),
            artifact("patch-set", patch_metadata),
        ];

        assert_eq!(
            verification_commands_from_artifacts(&artifacts),
            Some(vec!["cargo test".to_string()])
        );
    }

    #[test]
    fn rejects_invalid_structured_artifact_metadata() {
        let artifacts = vec![artifact(
            "integration-plan",
            json!({ "repoRoot": "/tmp/repo" }),
        )];

        let error =
            latest_validated_artifact::<IntegrationPlanArtifact>(&artifacts, "integration-plan")
                .unwrap_err();
        assert!(error.contains("failed structured metadata validation"));
    }

    #[test]
    fn builds_amp_prompt_from_plan_and_domain_map() {
        let inputs = WorkflowInputs {
            app_name: "colanode".to_string(),
            objective: "Use Heyo orgs as workspaces".to_string(),
            ..Default::default()
        };
        let plan = IntegrationPlanArtifact {
            analysis: integration_plan_analysis_contract(),
            repo_root: "/tmp/colanode".to_string(),
            objective: "Use Heyo orgs as workspaces".to_string(),
            depends_on_artifact_ids: vec!["artifact-domain".to_string()],
            assumptions: vec!["Preserve role semantics".to_string()],
            changes: vec![PlanChangeArtifact {
                id: "shared-workspace-membership".to_string(),
                intent: "Create or join an org-scoped shared workspace on Heyo login".to_string(),
            }],
            verification_commands: vec!["npm run test".to_string()],
            verification: vec!["npm run test".to_string()],
            deploy_config_preview: json!({}),
        };
        let domain_map = DomainMapArtifact {
            analysis: discovery_analysis_contract(),
            repo: DiscoveryRepoArtifact {
                root: "/tmp/colanode".to_string(),
                top_level_entries: vec!["apps".to_string()],
                key_files: vec!["apps/server/package.json".to_string()],
                frameworks: vec!["node".to_string()],
                package_managers: vec!["npm".to_string()],
                file_count: 42,
            },
            workspace_mapping: Some(WorkspaceMappingArtifact {
                heyo_org: "sharedWorkspace".to_string(),
            }),
            user_mapping: Some(UserMappingArtifact {
                heyo_member: "appUser".to_string(),
            }),
            role_mapping: Some(RoleMappingArtifact {
                admin: "admin".to_string(),
                user: "collaborator".to_string(),
                readonly: "guest".to_string(),
            }),
            objective: "Use Heyo orgs as workspaces".to_string(),
            risks: vec![],
            operator_review_hints: vec![],
        };

        let prompt = build_amp_patch_prompt(&inputs, &plan, Some(&domain_map));

        assert!(prompt.contains("shared app workspace"));
        assert!(prompt.contains("shared-workspace-membership"));
        assert!(prompt.contains("Heyo readonly -> guest"));
        assert!(prompt.contains("npm run test"));
    }

    #[test]
    fn deploy_plan_prompt_prioritizes_repo_local_guidance() {
        let prompt = build_amp_deploy_plan_prompt(
            &WorkflowInputs {
                app_name: "colanode".to_string(),
                objective: "Deploy colanode to Heyo".to_string(),
                ..Default::default()
            },
            &DomainMapArtifact {
                analysis: discovery_analysis_contract(),
                repo: DiscoveryRepoArtifact {
                    root: "/tmp/colanode".to_string(),
                    top_level_entries: vec!["apps".to_string(), "docs".to_string()],
                    key_files: vec!["README.md".to_string(), "turbo.json".to_string()],
                    frameworks: vec!["node".to_string()],
                    package_managers: vec!["npm".to_string()],
                    file_count: 42,
                },
                workspace_mapping: None,
                user_mapping: None,
                role_mapping: None,
                objective: "Deploy colanode to Heyo".to_string(),
                risks: vec![],
                operator_review_hints: vec![],
            },
            APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID,
        );

        assert!(prompt.contains(
            "`AGENTS.md`, root and app-level `README` files, `docs/`, `.claude/skills/`"
        ));
        assert!(prompt.contains("One deployed sandbox does not necessarily mean one Unix process"));
        assert!(prompt.contains("Install self-hostable runtime dependencies"));
        assert!(prompt
            .contains("If the documented startup flow copies or generates runtime config files"));
        assert!(prompt.contains("For monorepos, prefer the repo-native build and start flow"));
        assert!(prompt.contains("Target backend capabilities for this run are authoritative"));
        if target_backend_is_macos() {
            assert!(prompt.contains("`apple_container`") && prompt.contains("`apple_virt`"));
            assert!(prompt.contains("`driver: apple_container`"));
        } else {
            assert!(prompt.contains(
                "Linux target rule: `apple_container` and `apple_virt` are invalid choices"
            ));
            assert!(prompt.contains("`firecracker_containerd`"));
            assert!(prompt.contains("`firecracker_containerd` runtime contract"));
            assert!(!prompt.contains("`driver: apple_container`"));
        }
    }

    #[test]
    fn deploy_guardrails_add_macos_driver_choice_question_when_unpinned() {
        let mut artifact = DeployPlanArtifact {
            analysis: deploy_plan_analysis_contract(),
            repo_root: "/tmp/repo".to_string(),
            objective: "Deploy the app".to_string(),
            depends_on_artifact_ids: vec![],
            topology: DeployTopologyArtifact {
                sandbox_count: 1,
                sandboxes: vec![PlannedSandboxArtifact {
                    key: "web".to_string(),
                    purpose: "Serve traffic".to_string(),
                    runs: vec!["web".to_string()],
                    region: "US".to_string(),
                    driver: default_deploy_driver(),
                    image: DEFAULT_DEPLOY_IMAGE.to_string(),
                    size_class: DEFAULT_SIZE_CLASS.to_string(),
                    ports: vec![3000],
                    port_mappings: vec![],
                    env_refs: vec![],
                    env_keys: vec![],
                    start_command: Some("npm run start".to_string()),
                    working_directory: None,
                    setup_hooks: vec![],
                    ttl_seconds: None,
                    health_checks: vec![PlannedHealthCheckArtifact {
                        path: "/health".to_string(),
                        expected_status: 200,
                    }],
                }],
                not_deployed: vec![],
            },
            verification_commands: vec!["npm test".to_string()],
            manual_verification: vec![],
            assumptions: vec![],
            risks: vec![],
            operator_questions: vec![],
            execution_readiness: DeployExecutionReadinessArtifact {
                executable_by_orchestrator: true,
                blocking_reasons: vec![],
                missing_inputs: vec![],
            },
        };

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        // Guardrails must never surface `apple_container` as an option —
        // the runtime rejects it.
        assert!(!artifact
            .operator_questions
            .iter()
            .any(|question| question.contains("apple_container")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_allow_firecracker_with_managed_image_id() {
        let mut artifact = DeployPlanArtifact {
            analysis: deploy_plan_analysis_contract(),
            repo_root: "/tmp/repo".to_string(),
            objective: "Deploy the app".to_string(),
            depends_on_artifact_ids: vec![],
            topology: DeployTopologyArtifact {
                sandbox_count: 1,
                sandboxes: vec![PlannedSandboxArtifact {
                    key: "web".to_string(),
                    purpose: "Serve traffic".to_string(),
                    runs: vec!["web".to_string()],
                    region: "US".to_string(),
                    driver: "firecracker".to_string(),
                    image: "img-12345678".to_string(),
                    size_class: DEFAULT_SIZE_CLASS.to_string(),
                    ports: vec![3000],
                    port_mappings: vec![],
                    env_refs: vec![],
                    env_keys: vec![],
                    start_command: Some("./start.sh".to_string()),
                    working_directory: None,
                    setup_hooks: vec![],
                    ttl_seconds: None,
                    health_checks: vec![PlannedHealthCheckArtifact {
                        path: "/health".to_string(),
                        expected_status: 200,
                    }],
                }],
                not_deployed: vec![],
            },
            verification_commands: vec!["curl -f http://127.0.0.1:3000/health".to_string()],
            manual_verification: vec![],
            assumptions: vec![],
            risks: vec![],
            operator_questions: vec![],
            execution_readiness: DeployExecutionReadinessArtifact {
                executable_by_orchestrator: true,
                blocking_reasons: vec![],
                missing_inputs: vec![],
            },
        };

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(artifact.execution_readiness.executable_by_orchestrator);
        assert!(artifact
            .execution_readiness
            .blocking_reasons
            .iter()
            .all(|reason| !reason.contains("managed Heyo image ID")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_allow_firecracker_with_oci_reference() {
        let mut artifact = firecracker_plan_artifact("ubuntu:24.04");

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(artifact.execution_readiness.executable_by_orchestrator);
        assert!(artifact
            .execution_readiness
            .blocking_reasons
            .iter()
            .all(|reason| !reason.contains("managed Heyo image")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_allow_firecracker_with_dockerfile_reference() {
        let mut artifact = firecracker_plan_artifact("dockerfile:./Dockerfile");

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(artifact.execution_readiness.executable_by_orchestrator);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_block_firecracker_with_bare_local_name() {
        // Bare local names (no `:` tag, not an absolute .ext4 path, no `dockerfile:`
        // prefix, not a managed id) are ambiguous at the orchestrator level: we
        // can't know whether that name exists in the backend host's image cache.
        // Require an explicit spec form.
        let mut artifact = firecracker_plan_artifact("my-custom-image");

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(!artifact.execution_readiness.executable_by_orchestrator);
        assert!(artifact
            .execution_readiness
            .blocking_reasons
            .iter()
            .any(|reason| reason.contains("managed Heyo image id")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_allow_firecracker_containerd_with_oci_reference() {
        let mut artifact = firecracker_containerd_plan_artifact("ghcr.io/colanode/server:latest");

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(artifact.execution_readiness.executable_by_orchestrator);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_allow_firecracker_containerd_with_dockerfile_reference() {
        let mut artifact = firecracker_containerd_plan_artifact("dockerfile:./Dockerfile");

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(artifact.execution_readiness.executable_by_orchestrator);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_block_firecracker_containerd_with_bare_local_name() {
        let mut artifact = firecracker_containerd_plan_artifact("my-custom-image");

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(!artifact.execution_readiness.executable_by_orchestrator);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_block_firecracker_containerd_with_managed_image_id() {
        // Managed Heyo image IDs are a Firecracker-rootfs concept; they are NOT
        // valid for firecracker_containerd, which expects OCI tags or
        // `dockerfile:` refs.
        let mut artifact = firecracker_containerd_plan_artifact("img-12345678");

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(!artifact.execution_readiness.executable_by_orchestrator);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_block_firecracker_containerd_without_start_command() {
        let mut artifact = firecracker_containerd_plan_artifact("ghcr.io/org/app:latest");
        artifact.topology.sandboxes[0].start_command = None;

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(!artifact.execution_readiness.executable_by_orchestrator);
        assert!(artifact
            .execution_readiness
            .blocking_reasons
            .iter()
            .any(|reason| reason.contains("does not define `startCommand`")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_block_firecracker_containerd_postgres_without_path_export() {
        let mut artifact = firecracker_containerd_plan_artifact("pgvector/pgvector:pg17");
        artifact.topology.sandboxes[0].start_command = Some(
            "sh -lc 'export POSTGRES_USER=app POSTGRES_PASSWORD=secret POSTGRES_DB=app; exec docker-entrypoint.sh postgres'"
                .to_string(),
        );

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(!artifact.execution_readiness.executable_by_orchestrator);
        assert!(artifact
            .execution_readiness
            .blocking_reasons
            .iter()
            .any(|reason| reason
                .contains("without exporting the versioned PostgreSQL bin directory into PATH")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deploy_guardrails_allow_firecracker_containerd_postgres_with_path_export() {
        let mut artifact = firecracker_containerd_plan_artifact("pgvector/pgvector:pg17");
        artifact.topology.sandboxes[0].start_command = Some(
            "sh -lc 'export PATH=/usr/lib/postgresql/17/bin:$PATH POSTGRES_USER=app POSTGRES_PASSWORD=secret POSTGRES_DB=app; exec docker-entrypoint.sh postgres'"
                .to_string(),
        );

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert!(artifact.execution_readiness.executable_by_orchestrator);
        assert!(artifact
            .execution_readiness
            .blocking_reasons
            .iter()
            .all(|reason| !reason.contains("versioned PostgreSQL bin directory")));
    }

    #[cfg(target_os = "linux")]
    fn firecracker_containerd_plan_artifact(image: &str) -> DeployPlanArtifact {
        let mut artifact = firecracker_plan_artifact(image);
        artifact.topology.sandboxes[0].driver = "firecracker_containerd".to_string();
        artifact
    }

    #[cfg(target_os = "linux")]
    fn firecracker_plan_artifact(image: &str) -> DeployPlanArtifact {
        DeployPlanArtifact {
            analysis: deploy_plan_analysis_contract(),
            repo_root: "/tmp/repo".to_string(),
            objective: "Deploy the app".to_string(),
            depends_on_artifact_ids: vec![],
            topology: DeployTopologyArtifact {
                sandbox_count: 1,
                sandboxes: vec![PlannedSandboxArtifact {
                    key: "web".to_string(),
                    purpose: "Serve traffic".to_string(),
                    runs: vec!["web".to_string()],
                    region: "US".to_string(),
                    driver: "firecracker".to_string(),
                    image: image.to_string(),
                    size_class: DEFAULT_SIZE_CLASS.to_string(),
                    ports: vec![3000],
                    port_mappings: vec![],
                    env_refs: vec![],
                    env_keys: vec![],
                    start_command: Some("./start.sh".to_string()),
                    working_directory: None,
                    setup_hooks: vec![],
                    ttl_seconds: None,
                    health_checks: vec![PlannedHealthCheckArtifact {
                        path: "/health".to_string(),
                        expected_status: 200,
                    }],
                }],
                not_deployed: vec![],
            },
            verification_commands: vec!["curl -f http://127.0.0.1:3000/health".to_string()],
            manual_verification: vec![],
            assumptions: vec![],
            risks: vec![],
            operator_questions: vec![],
            execution_readiness: DeployExecutionReadinessArtifact {
                executable_by_orchestrator: true,
                blocking_reasons: vec![],
                missing_inputs: vec![],
            },
        }
    }

    #[test]
    fn deploy_plan_review_prompt_checks_repo_guidance_before_blocking() {
        let prompt = build_amp_deploy_plan_review_prompt(
            &WorkflowInputs {
                app_name: "colanode".to_string(),
                objective: "Deploy colanode to Heyo".to_string(),
                ..Default::default()
            },
            &DeployPlanArtifact {
                analysis: deploy_plan_analysis_contract(),
                repo_root: "/tmp/colanode".to_string(),
                objective: "Deploy colanode to Heyo".to_string(),
                depends_on_artifact_ids: vec![],
                topology: DeployTopologyArtifact {
                    sandbox_count: 1,
                    sandboxes: vec![],
                    not_deployed: vec![],
                },
                verification_commands: vec![],
                manual_verification: vec![],
                assumptions: vec![],
                risks: vec![],
                operator_questions: vec![],
                execution_readiness: DeployExecutionReadinessArtifact {
                    executable_by_orchestrator: true,
                    blocking_reasons: vec![],
                    missing_inputs: vec![],
                },
            },
            APP_INTEGRATE_WITH_HEYO_TEMPLATE_ID,
        );

        assert!(prompt.contains("Validate the plan against repo-local deployment evidence first"));
        assert!(prompt.contains("Do not block a single-sandbox plan merely because the app has multiple cooperating processes"));
        assert!(prompt.contains(
            "Prefer `needs_revision` over `block` when the repo already documents a workable setup"
        ));
        if !target_backend_is_macos() {
            assert!(prompt.contains("image-baked env is present inside that shell"));
            assert!(prompt.contains("versioned `/usr/lib/postgresql/<major>/bin` path"));
        }
    }

    #[test]
    fn deploy_question_prompt_combines_plan_questions_review_questions_and_missing_inputs() {
        let deploy_plan = DeployPlanArtifact {
            analysis: deploy_plan_analysis_contract(),
            repo_root: "/tmp/repo".to_string(),
            objective: "Deploy the app".to_string(),
            depends_on_artifact_ids: vec![],
            topology: DeployTopologyArtifact {
                sandbox_count: 1,
                sandboxes: vec![],
                not_deployed: vec![],
            },
            verification_commands: vec![],
            manual_verification: vec![],
            assumptions: vec![],
            risks: vec![],
            operator_questions: vec!["Which port should the app expose?".to_string()],
            execution_readiness: DeployExecutionReadinessArtifact {
                executable_by_orchestrator: false,
                blocking_reasons: vec!["missing env".to_string()],
                missing_inputs: vec!["deployConfig.env.POSTGRES_URL".to_string()],
            },
        };
        let review = DeployPlanReviewArtifact {
            analysis: deploy_plan_review_analysis_contract(),
            based_on_artifact_ids: vec![],
            recommendation: "needs_revision".to_string(),
            summary: "Needs more detail".to_string(),
            findings: vec![DeployPlanReviewFinding {
                severity: "warning".to_string(),
                title: "Missing runtime detail".to_string(),
                message: "The runtime env is incomplete.".to_string(),
                sandbox_key: Some("primary".to_string()),
            }],
            unanswered_questions: vec!["How should CONFIG be generated?".to_string()],
        };
        let artifacts = vec![
            artifact(
                "deploy-plan",
                validated_structured_output("deploy-plan", &deploy_plan).unwrap(),
            ),
            artifact(
                "deploy-plan-review",
                validated_structured_output("deploy-plan-review", &review).unwrap(),
            ),
        ];

        let prompt = deploy_question_prompt_from_artifacts(&artifacts)
            .unwrap()
            .expect("deploy question prompt should exist");

        assert!(prompt
            .questions
            .iter()
            .any(|q| q.contains("Which port should the app expose")));
        assert!(prompt
            .questions
            .iter()
            .any(|q| q.contains("How should CONFIG be generated")));
        assert!(prompt.questions.iter().any(|q| q.contains("POSTGRES_URL")));
        assert!(prompt
            .summary_markdown
            .contains("Deploy Planning Questions"));
    }

    #[test]
    fn deploy_guardrails_allow_multi_sandbox_execution() {
        let mut artifact = DeployPlanArtifact {
            analysis: deploy_plan_analysis_contract(),
            repo_root: "/tmp/repo".to_string(),
            objective: "Deploy the app".to_string(),
            depends_on_artifact_ids: vec![],
            topology: DeployTopologyArtifact {
                sandbox_count: 2,
                sandboxes: vec![
                    PlannedSandboxArtifact {
                        key: "web".to_string(),
                        purpose: "Serve the UI".to_string(),
                        runs: vec!["web".to_string()],
                        region: "US".to_string(),
                        driver: default_deploy_driver(),
                        image: DEFAULT_DEPLOY_IMAGE.to_string(),
                        size_class: DEFAULT_SIZE_CLASS.to_string(),
                        ports: vec![3000],
                        port_mappings: vec![],
                        env_refs: vec![],
                        env_keys: vec![],
                        start_command: Some("npm run start".to_string()),
                        working_directory: None,
                        setup_hooks: vec![],
                        ttl_seconds: None,
                        health_checks: vec![PlannedHealthCheckArtifact {
                            path: "/health".to_string(),
                            expected_status: 200,
                        }],
                    },
                    PlannedSandboxArtifact {
                        key: "worker".to_string(),
                        purpose: "Run background jobs".to_string(),
                        runs: vec!["worker".to_string()],
                        region: "US".to_string(),
                        driver: default_deploy_driver(),
                        image: DEFAULT_DEPLOY_IMAGE.to_string(),
                        size_class: DEFAULT_SIZE_CLASS.to_string(),
                        ports: vec![],
                        port_mappings: vec![],
                        env_refs: vec![],
                        env_keys: vec![],
                        start_command: Some("npm run worker".to_string()),
                        working_directory: None,
                        setup_hooks: vec![],
                        ttl_seconds: None,
                        health_checks: vec![],
                    },
                ],
                not_deployed: vec![],
            },
            verification_commands: vec!["npm test".to_string()],
            manual_verification: vec![],
            assumptions: vec![],
            risks: vec![],
            operator_questions: vec![],
            execution_readiness: DeployExecutionReadinessArtifact {
                executable_by_orchestrator: true,
                blocking_reasons: vec![],
                missing_inputs: vec![],
            },
        };

        apply_deploy_plan_guardrails(
            &mut artifact,
            &WorkflowInputs {
                user_id: Some("user-1".to_string()),
                account_id: Some("account-1".to_string()),
                ..Default::default()
            },
        );

        assert_eq!(artifact.topology.sandbox_count, 2);
        assert!(artifact.execution_readiness.executable_by_orchestrator);
        assert!(!artifact
            .execution_readiness
            .blocking_reasons
            .iter()
            .any(|reason| reason.contains("only one archive-backed sandbox")));
    }

    #[test]
    fn primary_planned_sandbox_prefers_healthchecked_service() {
        let plan = DeployPlanArtifact {
            analysis: deploy_plan_analysis_contract(),
            repo_root: "/tmp/repo".to_string(),
            objective: "Deploy the app".to_string(),
            depends_on_artifact_ids: vec![],
            topology: DeployTopologyArtifact {
                sandbox_count: 2,
                sandboxes: vec![
                    PlannedSandboxArtifact {
                        key: "worker".to_string(),
                        purpose: "Run jobs".to_string(),
                        runs: vec!["jobs".to_string()],
                        region: "US".to_string(),
                        driver: default_deploy_driver(),
                        image: DEFAULT_DEPLOY_IMAGE.to_string(),
                        size_class: DEFAULT_SIZE_CLASS.to_string(),
                        ports: vec![],
                        port_mappings: vec![],
                        env_refs: vec![],
                        env_keys: vec![],
                        start_command: Some("npm run worker".to_string()),
                        working_directory: None,
                        setup_hooks: vec![],
                        ttl_seconds: None,
                        health_checks: vec![],
                    },
                    PlannedSandboxArtifact {
                        key: "web".to_string(),
                        purpose: "Serve traffic".to_string(),
                        runs: vec!["web".to_string()],
                        region: "US".to_string(),
                        driver: default_deploy_driver(),
                        image: DEFAULT_DEPLOY_IMAGE.to_string(),
                        size_class: DEFAULT_SIZE_CLASS.to_string(),
                        ports: vec![3000],
                        port_mappings: vec![],
                        env_refs: vec![],
                        env_keys: vec![],
                        start_command: Some("npm run start".to_string()),
                        working_directory: None,
                        setup_hooks: vec![],
                        ttl_seconds: None,
                        health_checks: vec![PlannedHealthCheckArtifact {
                            path: "/health".to_string(),
                            expected_status: 200,
                        }],
                    },
                ],
                not_deployed: vec![],
            },
            verification_commands: vec![],
            manual_verification: vec![],
            assumptions: vec![],
            risks: vec![],
            operator_questions: vec![],
            execution_readiness: DeployExecutionReadinessArtifact {
                executable_by_orchestrator: false,
                blocking_reasons: vec![],
                missing_inputs: vec![],
            },
        };

        let selected = primary_planned_sandbox(&plan).unwrap();
        assert_eq!(selected.key, "web");
    }

    #[test]
    fn deploy_spec_preview_includes_topology_when_plan_exists() {
        let workflow_run = orchestration_workflow_run::Model {
            id: "workflow-1".to_string(),
            thread_id: "thread-1".to_string(),
            template_id: "app.deploy_to_heyo".to_string(),
            template_version: 1,
            goal: "Deploy the app".to_string(),
            status: "running".to_string(),
            phase: "waiting_deploy_approval".to_string(),
            target: "heyo-sandbox".to_string(),
            inputs: json!({ "repoRoot": "/tmp/repo" }),
            compiled_plan: json!({}),
            current_child_job_key: None,
            started_at: None,
            completed_at: None,
            created_at: chrono::Utc::now().into(),
            updated_at: chrono::Utc::now().into(),
        };
        let deploy_plan = DeployPlanArtifact {
            analysis: deploy_plan_analysis_contract(),
            repo_root: "/tmp/repo".to_string(),
            objective: "Deploy the app".to_string(),
            depends_on_artifact_ids: vec![],
            topology: DeployTopologyArtifact {
                sandbox_count: 1,
                sandboxes: vec![PlannedSandboxArtifact {
                    key: "web".to_string(),
                    purpose: "Serve traffic".to_string(),
                    runs: vec!["web".to_string()],
                    region: "US".to_string(),
                    driver: default_deploy_driver(),
                    image: DEFAULT_DEPLOY_IMAGE.to_string(),
                    size_class: DEFAULT_SIZE_CLASS.to_string(),
                    ports: vec![3000],
                    port_mappings: vec![],
                    env_refs: vec!["DATABASE_URL".to_string()],
                    env_keys: vec!["NODE_ENV".to_string()],
                    start_command: Some("npm run start".to_string()),
                    working_directory: Some("apps/web".to_string()),
                    setup_hooks: vec!["npm install".to_string()],
                    ttl_seconds: Some(3600),
                    health_checks: vec![PlannedHealthCheckArtifact {
                        path: "/health".to_string(),
                        expected_status: 200,
                    }],
                }],
                not_deployed: vec![PlannedOmissionArtifact {
                    component: "worker".to_string(),
                    reason: "requires separate queue infra".to_string(),
                    impact: "background jobs will not run".to_string(),
                    blocking: true,
                }],
            },
            verification_commands: vec![],
            manual_verification: vec![],
            assumptions: vec![],
            risks: vec![],
            operator_questions: vec![],
            execution_readiness: DeployExecutionReadinessArtifact {
                executable_by_orchestrator: false,
                blocking_reasons: vec!["multiple runtimes not supported".to_string()],
                missing_inputs: vec!["accountId".to_string()],
            },
        };
        let artifacts = vec![artifact(
            "deploy-plan",
            validated_structured_output("deploy-plan", &deploy_plan).unwrap(),
        )];

        let preview = build_deploy_spec_preview(&workflow_run, &artifacts).unwrap();

        assert_eq!(preview["topology"]["sandboxCount"], json!(1));
        assert_eq!(preview["topology"]["sandboxes"][0]["key"], json!("web"));
        assert_eq!(
            preview["topology"]["notDeployed"][0]["component"],
            json!("worker")
        );
        assert_eq!(
            preview["executionReadiness"]["missingInputs"][0],
            json!("accountId")
        );
    }

    #[test]
    fn parses_amp_deploy_plan_payload_from_fenced_json() {
        let payload = parse_amp_structured_output::<AmpDeployPlanPayload>(
            "deploy plan",
            "Intro\n```json\n{\n  \"topology\": {\n    \"sandboxCount\": 1,\n    \"sandboxes\": [],\n    \"notDeployed\": []\n  },\n  \"verificationCommands\": [],\n  \"manualVerification\": [],\n  \"assumptions\": [],\n  \"risks\": [],\n  \"operatorQuestions\": [],\n  \"executionReadiness\": {\n    \"executableByOrchestrator\": false,\n    \"blockingReasons\": [\"missing db\"],\n    \"missingInputs\": [\"POSTGRES_URL\"]\n  }\n}\n```",
        )
        .unwrap();

        assert_eq!(payload.topology.sandbox_count, 1);
        assert_eq!(
            payload.execution_readiness.missing_inputs,
            vec!["POSTGRES_URL"]
        );
    }

    #[test]
    fn parses_amp_deploy_plan_payload_from_inline_json_with_prose() {
        let payload = parse_amp_structured_output::<AmpDeployPlanPayload>(
            "deploy plan",
            "Now I have a thorough understanding of the architecture. Let me produce the deployment plan JSON. ```json {\"topology\":{\"sandboxCount\":1,\"sandboxes\":[],\"notDeployed\":[]},\"verificationCommands\":[],\"manualVerification\":[],\"assumptions\":[],\"risks\":[],\"operatorQuestions\":[],\"executionReadiness\":{\"executableByOrchestrator\":false,\"blockingReasons\":[\"missing db\"],\"missingInputs\":[\"POSTGRES_URL\"]}} ```",
        )
        .unwrap();

        assert_eq!(payload.topology.sandbox_count, 1);
        assert_eq!(
            payload.execution_readiness.blocking_reasons,
            vec!["missing db"]
        );
    }

    #[tokio::test]
    async fn amp_sandbox_git_excludes_generated_caches() {
        let repo_root = std::env::temp_dir().join(format!(
            "heyo-orchestrator-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir_all(&repo_root).await.unwrap();

        initialize_git_baseline(&repo_root).await.unwrap();
        configure_amp_sandbox_git_excludes(&repo_root)
            .await
            .unwrap();

        let cache_file = repo_root.join("_cacache/content-v2/cache.bin");
        tokio::fs::create_dir_all(cache_file.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&cache_file, b"cache").await.unwrap();

        let output = run_command(&repo_root, "git", &["status", "--short", "--ignored"], None)
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("!! _cacache/"));

        tokio::fs::remove_dir_all(&repo_root).await.unwrap();
    }
}
