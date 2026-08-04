# Heyo AI Orchestration Service Spec

This document defines the first implementation-oriented design for a Heyo orchestration service that combines:

- AI for discovery, planning, and drafting.
- Flatfile-style constraints for persisted conversations, typed tool access, approvals, and inspectability.
- ORCA-style workflow execution for resumable parent/child runs, dependency tracking, retries, and deterministic side effects.

The target product outcome is simple:

1. An operator describes a goal in natural language.
2. The system discovers the target app and proposes an integration plan.
3. The system applies approved changes.
4. The system deploys the revised app to a Heyo sandbox.

Example future request:

> I have Colanode at `~/dev/colanode`. Integrate it with Heyo auth/orgs and deploy it to a Heyo sandbox.

Conceptually, this service should follow the ORCA mental model:

- the top-level orchestration object is a parent job
- the executable graph beneath it is made of child jobs

In the persistence schema below, the storage names remain:

- `workflow_run` for the parent job
- `step_run` for the child job

This keeps the runtime model aligned with existing workflow terminology while making the orchestration behavior explicitly ORCA-like.

## Goals

- Support open-ended app integration tasks without requiring the workflow to be fully known ahead of time.
- Keep AI at the intent, discovery, and planning boundary.
- Keep code mutation, verification, and deployment deterministic once a plan is approved.
- Persist enough state that a run is resumable, auditable, and reviewable.
- Target Heyo sandboxes directly. No local runtime and no Docker-based deployment path in this service.

## Non-Goals

- Replacing existing `cloud/` sandbox provisioning internals.
- Giving LLMs unconstrained runtime control over deployment steps.
- Supporting arbitrary production deploy targets in the first version.
- Treating a single long-running chat session as the execution model.

## Core Principles

### 1. AI Proposes, Runtime Executes

The AI may:

- inspect repos
- infer domain mappings
- draft plans
- draft patch sets
- summarize verification evidence

The AI may not:

- perform direct production deployment actions on its own authority
- invent new runtime steps after a workflow has been compiled
- bypass policy or approval gates

### 2. Every Important Output Becomes An Artifact

Examples:

- domain map
- integration plan
- patch set
- verification report
- deploy spec
- deploy report

Artifacts are durable and can be cited in later messages, approvals, and retries.

### 3. Side Effects Happen Only In Deterministic Steps

Only compiled workflow steps using registered adapters can:

- mutate repositories
- run verification commands
- create Heyo deployments
- bind ports
- perform health checks

### 4. Approval Gates Are First-Class

At minimum, the first version should support:

- plan approval
- patch application approval
- deploy approval

## High-Level Architecture

The orchestration service should be its own control-plane surface.

### Components

1. Conversation Service
2. Artifact Store
3. AI Planning Service
4. Workflow Compiler
5. Workflow Runtime
6. Approval Service
7. Adapter Gateway
8. Event Ingestor

### Responsibilities

#### Conversation Service

- stores threads and messages
- stores tool-call transcripts and references to artifacts
- provides the working memory for AI planning

#### Artifact Store

- stores structured outputs from planning and execution
- provides immutable references for approvals and reruns

#### AI Planning Service

- accepts a thread plus current task context
- classifies each request before planning so discovery and mutation planning remain distinct
- calls only typed tools
- emits schema-validated artifacts
- records the allowed tool surface and output contract in each artifact so downstream deterministic steps can re-validate inputs before execution

#### Workflow Compiler

- validates AI proposals against policy and workflow templates
- produces a frozen parent-job and child-job graph
- persists that graph as `workflow_run` and `step_run` records
- rejects incomplete or unsafe plans before execution starts

#### Workflow Runtime

- executes parent jobs and child jobs
- tracks dependencies, retries, cancellation, pause/resume, and approval waits
- records step-attempt leases separately from business step state so resumed runs can distinguish live execution from abandoned work
- keeps side-effecting execution deterministic

#### Approval Service

- creates approval requests bound to artifacts and steps
- captures approve/reject decisions and comments

#### Adapter Gateway

- exposes registered execution adapters
- isolates repo analysis, repo mutation, verification, and Heyo deployment logic

#### Event Ingestor

- consumes runtime events from deterministic backends
- persists inbound external events durably before applying them to workflow state
- updates workflow and step state
- attaches final reports back to the originating thread

## Execution Model

The execution model is parent job first, child jobs second.

### Parent Job

The parent job owns the full user request.

Examples:

- integrate an app with Heyo auth and org semantics
- deploy an app to a Heyo sandbox
- inspect an existing codebase and propose an implementation plan

The parent job owns:

- the user goal
- the target repo and deploy target
- the current phase
- the approval state
- the child-job graph
- the artifact set
- the final outcome

### Child Jobs

Child jobs are the bounded units of execution under the parent job.

The parent job may create child jobs at phase boundaries, based on accumulated artifacts and approvals.

The parent job should not allow a running child job to invent new siblings directly. A child job returns structured outputs; the parent job decides what to spawn next.

### Parent Job Phases

The first version should use explicit parent-job phases.

- `discovering`
- `planning`
- `waiting_plan_approval`
- `implementing`
- `verifying`
- `waiting_deploy_approval`
- `deploying`
- `completed`
- `failed`
- `cancelled`

### Child Job Types

There are six child-job types.

#### Analysis Child Jobs

Analysis children are AI-assisted, constrained, and artifact-producing.

Examples:

- inspect auth model
- inspect workspace model
- infer org/workspace mapping

Rules:

- may use typed tools only
- may create artifacts
- may not cause external side effects beyond artifact creation

#### Planning Child Jobs

Planning children turn discovery outputs into an executable proposal.

Examples:

- draft integration plan
- draft verification plan
- draft deploy plan

Rules:

- may use typed tools only
- must emit schema-validated artifacts
- may not directly mutate repos or deploy

#### Mutation Child Jobs

Mutation children apply approved implementation changes.

Examples:

- apply config changes
- apply auth changes
- apply workspace changes

Rules:

- must have an approved plan behind them
- must be scoped to explicit targets
- should be reviewable and replayable

#### Verification Child Jobs

Verification children gather evidence and summarize residual risk.

Examples:

- run targeted checks
- run integration verification
- summarize residual risks

Rules:

- should produce verification artifacts
- should not silently mutate deploy state

#### Deployment Child Jobs

Deployment children perform bounded, deterministic side effects.

Examples:

- create release bundle
- create deploy spec
- deploy to Heyo sandbox
- verify runtime health

Rules:

- use registered adapters only
- use explicit inputs only
- use idempotency keys where applicable
- do not call the LLM for additional runtime planning

#### Approval Child Jobs

Approval children are human-in-the-loop pauses.

Examples:

- approve plan
- approve patch application
- approve deployment

Rules:

- runtime pauses until a decision exists
- decision is persisted and immutable once finalized

## Runtime State Machine

### Workflow Run Status

- `draft`
- `compiled`
- `waiting_approval`
- `running`
- `paused`
- `failed`
- `completed`
- `cancelled`

### Parent Job Phase

- `discovering`
- `planning`
- `waiting_plan_approval`
- `implementing`
- `verifying`
- `waiting_deploy_approval`
- `deploying`
- `completed`
- `failed`
- `cancelled`

### Step Run Status

- `pending`
- `ready`
- `running`
- `waiting_approval`
- `blocked`
- `failed`
- `completed`
- `cancelled`

### Approval Status

- `pending`
- `approved`
- `rejected`
- `expired`

## TypeScript Domain Schema

The following interfaces define the minimum application-level model.

```ts
type ThreadStatus = 'active' | 'archived'

type MessageRole = 'user' | 'assistant' | 'tool' | 'system'

type ArtifactKind =
  | 'domain-map'
  | 'integration-plan'
  | 'patch-set'
  | 'verification-report'
  | 'deploy-spec'
  | 'deploy-report'

type ArtifactFormat = 'json' | 'markdown' | 'diff' | 'text'

type WorkflowRunStatus =
  | 'draft'
  | 'compiled'
  | 'waiting_approval'
  | 'running'
  | 'paused'
  | 'failed'
  | 'completed'
  | 'cancelled'

type StepRunType = 'agent' | 'approval' | 'deterministic'

type ParentJobPhase =
  | 'discovering'
  | 'planning'
  | 'waiting_plan_approval'
  | 'implementing'
  | 'verifying'
  | 'waiting_deploy_approval'
  | 'deploying'
  | 'completed'
  | 'failed'
  | 'cancelled'

type ChildJobKind =
  | 'analysis'
  | 'planning'
  | 'mutation'
  | 'verification'
  | 'deployment'
  | 'approval'

type StepRunStatus =
  | 'pending'
  | 'ready'
  | 'running'
  | 'waiting_approval'
  | 'blocked'
  | 'failed'
  | 'completed'
  | 'cancelled'

type ApprovalKind = 'plan' | 'patch' | 'deploy'

type ApprovalStatus = 'pending' | 'approved' | 'rejected' | 'expired'

type RegisteredAdapter =
  | 'ai.discovery'
  | 'ai.planning'
  | 'repo.patch'
  | 'repo.verify'
  | 'heyo.deploy'
  | 'heyo.healthcheck'

type WorkflowTarget = 'heyo-sandbox'

interface Thread {
  id: string
  title?: string
  createdBy: string
  status: ThreadStatus
  context: {
    repoRoots?: string[]
    targetPlatform?: 'heyo'
    targetEnv?: WorkflowTarget
  }
  createdAt: string
  updatedAt: string
}

interface Message {
  id: string
  threadId: string
  role: MessageRole
  content: unknown
  metadata?: Record<string, unknown>
  createdAt: string
}

interface Artifact {
  id: string
  threadId: string
  workflowRunId?: string
  stepRunId?: string
  kind: ArtifactKind
  format: ArtifactFormat
  schemaVersion: number
  title?: string
  uri?: string
  body?: string
  metadata?: Record<string, unknown>
  createdAt: string
}

interface WorkflowTemplate {
  id: string
  name: string
  version: number
  description: string
  inputSchema: Record<string, unknown>
  outputSchema: Record<string, unknown>
  phaseGraph: ParentJobPhase[]
  stepTemplates: StepTemplate[]
  policy: WorkflowPolicy
  createdAt: string
  updatedAt: string
}

interface StepTemplate {
  key: string
  type: StepRunType
  kind: ChildJobKind
  phase: ParentJobPhase
  adapter?: RegisteredAdapter
  dependsOn: string[]
  canFanOut?: boolean
  requiresApprovalBeforeStart?: boolean
  inputSchema?: Record<string, unknown>
  outputSchema?: Record<string, unknown>
  artifactContract?: {
    produces?: string[]
  }
  retryPolicy?: {
    maxRetries: number
  }
}

interface WorkflowPolicy {
  requirePlanApproval: boolean
  requirePatchApproval: boolean
  requireDeployApproval: boolean
  allowRepoMutation: boolean
  allowedTargets: WorkflowTarget[]
}

interface WorkflowRun {
  id: string
  threadId: string
  templateId: string
  templateVersion: number
  goal: string
  status: WorkflowRunStatus
  phase: ParentJobPhase
  target: WorkflowTarget
  inputs: Record<string, unknown>
  compiledPlan: Record<string, unknown>
  currentChildJobKey?: string
  startedAt?: string
  completedAt?: string
  createdAt: string
  updatedAt: string
}

interface StepRun {
  id: string
  workflowRunId: string
  key: string
  type: StepRunType
  kind: ChildJobKind
  phase: ParentJobPhase
  status: StepRunStatus
  adapter?: RegisteredAdapter
  dependsOn: string[]
  canFanOut: boolean
  requiresApprovalBeforeStart: boolean
  inputs: Record<string, unknown>
  outputs?: Record<string, unknown>
  artifactContract?: Record<string, unknown>
  retryCount: number
  maxRetries: number
  idempotencyKey?: string
  startedAt?: string
  completedAt?: string
  createdAt: string
  updatedAt: string
}

interface Approval {
  id: string
  threadId: string
  workflowRunId: string
  stepRunId: string
  kind: ApprovalKind
  status: ApprovalStatus
  requestArtifactIds: string[]
  decidedBy?: string
  responseComment?: string
  decidedAt?: string
  createdAt: string
}

interface ToolCallLog {
  id: string
  threadId: string
  workflowRunId?: string
  stepRunId?: string
  toolName: string
  input: Record<string, unknown>
  output?: Record<string, unknown>
  status: 'started' | 'completed' | 'failed'
  startedAt: string
  completedAt?: string
}
```

## Postgres Schema

The runtime should persist state in Postgres. JSONB is preferred for flexible step and artifact payloads.

```sql
CREATE TABLE orchestration_threads (
    id UUID PRIMARY KEY,
    title TEXT,
    created_by TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'archived')),
    context JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE orchestration_messages (
    id UUID PRIMARY KEY,
    thread_id UUID NOT NULL REFERENCES orchestration_threads(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'tool', 'system')),
    content JSONB NOT NULL,
    metadata JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_orch_messages_thread_created_at
    ON orchestration_messages(thread_id, created_at);

CREATE TABLE orchestration_workflow_templates (
    id TEXT NOT NULL,
    version INTEGER NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    input_schema JSONB NOT NULL,
    output_schema JSONB NOT NULL,
    phase_graph JSONB NOT NULL,
    step_templates JSONB NOT NULL,
    policy JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, version)
);

CREATE TABLE orchestration_workflow_runs (
    id UUID PRIMARY KEY,
    thread_id UUID NOT NULL REFERENCES orchestration_threads(id) ON DELETE CASCADE,
    template_id TEXT NOT NULL,
    template_version INTEGER NOT NULL,
    goal TEXT NOT NULL,
    status TEXT NOT NULL CHECK (
        status IN (
            'draft',
            'compiled',
            'waiting_approval',
            'running',
            'paused',
            'failed',
            'completed',
            'cancelled'
        )
    ),
    phase TEXT NOT NULL CHECK (
        phase IN (
            'discovering',
            'planning',
            'waiting_plan_approval',
            'implementing',
            'verifying',
            'waiting_deploy_approval',
            'deploying',
            'completed',
            'failed',
            'cancelled'
        )
    ),
    target TEXT NOT NULL CHECK (target IN ('heyo-sandbox')),
    inputs JSONB NOT NULL,
    compiled_plan JSONB NOT NULL,
    current_child_job_key TEXT,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    FOREIGN KEY (template_id, template_version)
        REFERENCES orchestration_workflow_templates(id, version)
);

CREATE INDEX idx_orch_workflow_runs_thread_created_at
    ON orchestration_workflow_runs(thread_id, created_at DESC);

CREATE INDEX idx_orch_workflow_runs_status
    ON orchestration_workflow_runs(status);

CREATE TABLE orchestration_step_runs (
    id UUID PRIMARY KEY,
    workflow_run_id UUID NOT NULL REFERENCES orchestration_workflow_runs(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    type TEXT NOT NULL CHECK (type IN ('agent', 'approval', 'deterministic')),
    kind TEXT NOT NULL CHECK (
        kind IN ('analysis', 'planning', 'mutation', 'verification', 'deployment', 'approval')
    ),
    phase TEXT NOT NULL CHECK (
        phase IN (
            'discovering',
            'planning',
            'waiting_plan_approval',
            'implementing',
            'verifying',
            'waiting_deploy_approval',
            'deploying',
            'completed',
            'failed',
            'cancelled'
        )
    ),
    status TEXT NOT NULL CHECK (
        status IN (
            'pending',
            'ready',
            'running',
            'waiting_approval',
            'blocked',
            'failed',
            'completed',
            'cancelled'
        )
    ),
    adapter TEXT,
    depends_on JSONB NOT NULL DEFAULT '[]'::jsonb,
    can_fan_out BOOLEAN NOT NULL DEFAULT FALSE,
    requires_approval_before_start BOOLEAN NOT NULL DEFAULT FALSE,
    inputs JSONB NOT NULL DEFAULT '{}'::jsonb,
    outputs JSONB,
    artifact_contract JSONB,
    retry_count INTEGER NOT NULL DEFAULT 0,
    max_retries INTEGER NOT NULL DEFAULT 0,
    idempotency_key TEXT,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (workflow_run_id, key)
);

CREATE INDEX idx_orch_step_runs_workflow_status
    ON orchestration_step_runs(workflow_run_id, status);

CREATE INDEX idx_orch_step_runs_idempotency_key
    ON orchestration_step_runs(idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE orchestration_artifacts (
    id UUID PRIMARY KEY,
    thread_id UUID NOT NULL REFERENCES orchestration_threads(id) ON DELETE CASCADE,
    workflow_run_id UUID REFERENCES orchestration_workflow_runs(id) ON DELETE CASCADE,
    step_run_id UUID REFERENCES orchestration_step_runs(id) ON DELETE SET NULL,
    kind TEXT NOT NULL CHECK (
        kind IN (
            'domain-map',
            'integration-plan',
            'patch-set',
            'verification-report',
            'deploy-spec',
            'deploy-report'
        )
    ),
    format TEXT NOT NULL CHECK (format IN ('json', 'markdown', 'diff', 'text')),
    schema_version INTEGER NOT NULL,
    title TEXT,
    uri TEXT,
    body TEXT,
    metadata JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_orch_artifacts_thread_kind
    ON orchestration_artifacts(thread_id, kind, created_at DESC);

CREATE TABLE orchestration_approvals (
    id UUID PRIMARY KEY,
    thread_id UUID NOT NULL REFERENCES orchestration_threads(id) ON DELETE CASCADE,
    workflow_run_id UUID NOT NULL REFERENCES orchestration_workflow_runs(id) ON DELETE CASCADE,
    step_run_id UUID NOT NULL REFERENCES orchestration_step_runs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('plan', 'patch', 'deploy')),
    status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
    request_artifact_ids JSONB NOT NULL,
    decided_by TEXT,
    response_comment TEXT,
    decided_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_orch_approvals_workflow_status
    ON orchestration_approvals(workflow_run_id, status);

CREATE TABLE orchestration_tool_call_logs (
    id UUID PRIMARY KEY,
    thread_id UUID NOT NULL REFERENCES orchestration_threads(id) ON DELETE CASCADE,
    workflow_run_id UUID REFERENCES orchestration_workflow_runs(id) ON DELETE CASCADE,
    step_run_id UUID REFERENCES orchestration_step_runs(id) ON DELETE CASCADE,
    tool_name TEXT NOT NULL,
    input JSONB NOT NULL,
    output JSONB,
    status TEXT NOT NULL CHECK (status IN ('started', 'completed', 'failed')),
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ
);

CREATE INDEX idx_orch_tool_logs_step_started_at
    ON orchestration_tool_call_logs(step_run_id, started_at DESC);
```

## Tooling Contract

The AI planning layer should receive only typed tools.

### Discovery Tools

- `repo.search`
- `repo.read_file`
- `repo.list_files`
- `thread.find_related`
- `artifact.write`

### Planning Tools

- `plan.create_domain_map`
- `plan.create_integration_plan`
- `plan.create_verification_plan`
- `plan.create_deploy_spec`

### Deterministic Adapter Operations

These are not AI planner tools. These are runtime adapter entry points.

- `repo.apply_patch_set`
- `repo.run_check`
- `repo.collect_results`
- `heyo.deploy_release`
- `heyo.bind_ports`
- `heyo.check_health`
- `heyo.rollback_deploy`

## Adapter Contracts

Each registered adapter should expose a narrow, versioned contract.

```ts
interface AdapterExecutionContext {
  threadId: string
  workflowRunId: string
  stepRunId: string
  actorId: string
}

interface AdapterResult {
  outputs?: Record<string, unknown>
  artifactIds?: string[]
  externalRefs?: Record<string, string>
}

interface WorkflowAdapter<I = Record<string, unknown>, O = AdapterResult> {
  name: RegisteredAdapter
  execute(input: I, context: AdapterExecutionContext): Promise<O>
}

interface RepoPatchAdapterInput {
  repoRoot: string
  patchArtifactId: string
  allowedFiles?: string[]
}

interface RepoVerifyAdapterInput {
  repoRoot: string
  commands: string[]
}

interface HeyoDeployAdapterInput {
  appName: string
  repoRoot: string
  releaseRef: string
  target: 'heyo-sandbox'
  environment: Record<string, string>
  ports: number[]
  healthChecks: Array<{
    path: string
    expectedStatus: number
  }>
}

interface HeyoHealthcheckAdapterInput {
  deploymentId: string
  checks: Array<{
    url: string
    expectedStatus: number
  }>
}
```

## Workflow Template DSL

Templates should be stored as structured data, not handwritten runtime code.

Example parent-job template definition:

```json
{
  "id": "app.integrate_with_heyo_and_deploy",
  "version": 1,
  "name": "Integrate App With Heyo And Deploy",
  "description": "Discover an app, plan Heyo integration changes, apply approved patches, verify behavior, and deploy to a Heyo sandbox.",
  "phaseGraph": [
    "discovering",
    "planning",
    "waiting_plan_approval",
    "implementing",
    "verifying",
    "waiting_deploy_approval",
    "deploying",
    "completed"
  ],
  "policy": {
    "requirePlanApproval": true,
    "requirePatchApproval": true,
    "requireDeployApproval": true,
    "allowRepoMutation": true,
    "allowedTargets": ["heyo-sandbox"]
  },
  "stepTemplates": [
    {
      "key": "discover_domain_model",
      "type": "agent",
      "kind": "analysis",
      "phase": "discovering",
      "adapter": "ai.discovery",
      "dependsOn": [],
      "canFanOut": true,
      "artifactContract": {
        "produces": ["domain-map"]
      }
    },
    {
      "key": "draft_integration_plan",
      "type": "agent",
      "kind": "planning",
      "phase": "planning",
      "adapter": "ai.planning",
      "dependsOn": ["discover_domain_model"],
      "artifactContract": {
        "produces": ["integration-plan"]
      }
    },
    {
      "key": "approve_plan",
      "type": "approval",
      "kind": "approval",
      "phase": "waiting_plan_approval",
      "dependsOn": ["draft_integration_plan"]
    },
    {
      "key": "draft_patch_set",
      "type": "agent",
      "kind": "planning",
      "phase": "implementing",
      "adapter": "ai.planning",
      "dependsOn": ["approve_plan"],
      "artifactContract": {
        "produces": ["patch-set"]
      }
    },
    {
      "key": "approve_patch_application",
      "type": "approval",
      "kind": "approval",
      "phase": "implementing",
      "dependsOn": ["draft_patch_set"]
    },
    {
      "key": "apply_patch_set",
      "type": "deterministic",
      "kind": "mutation",
      "phase": "implementing",
      "adapter": "repo.patch",
      "dependsOn": ["approve_patch_application"],
      "requiresApprovalBeforeStart": true
    },
    {
      "key": "verify_changes",
      "type": "deterministic",
      "kind": "verification",
      "phase": "verifying",
      "adapter": "repo.verify",
      "dependsOn": ["apply_patch_set"],
      "artifactContract": {
        "produces": ["verification-report"]
      }
    },
    {
      "key": "approve_deploy",
      "type": "approval",
      "kind": "approval",
      "phase": "waiting_deploy_approval",
      "dependsOn": ["verify_changes"]
    },
    {
      "key": "create_deploy_spec",
      "type": "deterministic",
      "kind": "deployment",
      "phase": "deploying",
      "adapter": "heyo.deploy",
      "dependsOn": ["approve_deploy"],
      "artifactContract": {
        "produces": ["deploy-spec"]
      }
    },
    {
      "key": "confirm_runtime_health",
      "type": "deterministic",
      "kind": "deployment",
      "phase": "deploying",
      "adapter": "heyo.healthcheck",
      "dependsOn": ["create_deploy_spec"],
      "artifactContract": {
        "produces": ["deploy-report"]
      }
    }
  ]
}
```

## Parent Job And Child Job Graph

The first reusable templates should be thought of as parent jobs that each own a child-job graph.

For an app integration task, the expected child-job graph is:

1. `discover_domain_model`
2. `draft_integration_plan`
3. `approve_plan`
4. `draft_patch_set`
5. `approve_patch_application`
6. `apply_patch_set`
7. `verify_changes`
8. `approve_deploy`
9. `create_deploy_spec`
10. `confirm_runtime_health`

For a generic app deploy task, the expected child-job graph is:

1. `discover_domain_model`
2. `draft_deploy_plan`
3. `verify_release_candidate`
4. `approve_deploy`
5. `create_deploy_spec`
6. `confirm_runtime_health`

Later versions can split these further into parallel discovery or scoped mutation children, but the parent job remains the coordinating object.

## Compiler Rules

The workflow compiler must reject a run if any of the following are true.

- The template target is not `heyo-sandbox`.
- A deterministic step references an unknown adapter.
- A plan approval step is missing when the policy requires one.
- A deploy approval step is missing when the policy requires one.
- A deterministic deploy step has no idempotency key.
- A side-effecting step appears before its required approval.
- A workflow includes runtime shell execution that is not mediated by a registered adapter.
- Secrets appear inline in plan artifacts instead of by reference.

## Example Input Contract

The initial reusable workflow should accept inputs shaped like this.

```json
{
  "repoRoot": "/Users/alice/dev/colanode",
  "appName": "colanode",
  "appType": "fullstack",
  "targetPlatform": "heyo",
  "objective": "Integrate the app with Heyo auth, org, and sandbox conventions",
  "deployTarget": "heyo-sandbox",
  "deployConfig": {
    "ports": [3002, 4000],
    "healthcheckPath": "/config",
    "envRefs": [
      "secret://AUTH_URL",
      "secret://HEYO_ACCOUNT_ID",
      "secret://HEYO_TENANT_NAME"
    ]
  }
}
```

## Example Artifacts

### `domain-map`

```json
{
  "workspaceMapping": {
    "heyoOrg": "appWorkspace"
  },
  "userMapping": {
    "heyoMember": "appUser"
  },
  "roleMapping": {
    "admin": "admin",
    "user": "collaborator",
    "readonly": "guest"
  },
  "risks": [
    "personal workspace creation can conflict with shared org workspace expectations",
    "first login race may create duplicate org-bound workspace state"
  ]
}
```

### `integration-plan`

```json
{
  "changes": [
    {
      "id": "bind-heyo-tenant-config",
      "intent": "Add Heyo tenant binding config"
    },
    {
      "id": "shared-workspace-membership",
      "intent": "Create or join org-scoped workspace on login"
    },
    {
      "id": "heyo-auth-route",
      "intent": "Add Heyo login flow"
    }
  ],
  "verification": [
    "first org member creates shared workspace",
    "later org member joins same workspace",
    "readonly maps to guest",
    "non-bound org login is rejected"
  ]
}
```

### `deploy-spec`

```json
{
  "target": "heyo-sandbox",
  "release": {
    "source": "repo-archive",
    "ref": "workspace://revisions/current"
  },
  "runtime": {
    "ports": [3002, 4000],
    "envRefs": [
      "secret://AUTH_URL",
      "secret://HEYO_ACCOUNT_ID",
      "secret://HEYO_TENANT_NAME"
    ]
  },
  "healthChecks": [
    {
      "path": "/config",
      "expectedStatus": 200
    }
  ]
}
```

## Example Parent Job Run

```json
{
  "id": "b0d2f7c8-16d4-44f2-8d3d-5a932e4d3dbe",
  "threadId": "f4d0831d-0f5c-4ec9-b95d-9d3d4d4f6f55",
  "templateId": "app.integrate_with_heyo_and_deploy",
  "templateVersion": 1,
  "goal": "Integrate Colanode with Heyo and deploy it to a Heyo sandbox",
  "status": "running",
  "phase": "implementing",
  "target": "heyo-sandbox",
  "inputs": {
    "repoRoot": "/Users/alice/dev/colanode",
    "appName": "colanode",
    "deployTarget": "heyo-sandbox"
  },
  "compiledPlan": {
    "childJobs": [
      {
        "key": "discover_domain_model",
        "kind": "analysis",
        "phase": "discovering"
      },
      {
        "key": "draft_integration_plan",
        "kind": "planning",
        "phase": "planning"
      },
      {
        "key": "approve_plan",
        "kind": "approval",
        "phase": "waiting_plan_approval"
      },
      {
        "key": "draft_patch_set",
        "kind": "planning",
        "phase": "implementing"
      },
      {
        "key": "approve_patch_application",
        "kind": "approval",
        "phase": "implementing"
      },
      {
        "key": "apply_patch_set",
        "kind": "mutation",
        "phase": "implementing"
      },
      {
        "key": "verify_changes",
        "kind": "verification",
        "phase": "verifying"
      },
      {
        "key": "approve_deploy",
        "kind": "approval",
        "phase": "waiting_deploy_approval"
      },
      {
        "key": "create_deploy_spec",
        "kind": "deployment",
        "phase": "deploying"
      },
      {
        "key": "confirm_runtime_health",
        "kind": "deployment",
        "phase": "deploying"
      }
    ]
  }
}
```

## Heyo Deployment Boundary

The orchestration service must not deploy locally and must not use Docker as a primary app runtime path.

All deploys in this service should resolve to Heyo sandbox execution. The deploy adapter should:

1. create or reference a release artifact
2. publish a Heyo sandbox deployment command
3. wait on deployment lifecycle events
4. attach the resulting deployment metadata back to the workflow
5. run deterministic health checks

The expected implementation path is to integrate with the existing Heyo sandbox queue/event model rather than inventing a second deployment plane.

## Security And Policy

### Required Policy Defaults

- plan approval required
- patch application approval required
- deploy approval required
- allowed target restricted to `heyo-sandbox`
- secret references required for deployment inputs

### Audit Requirements

For each completed parent job, the system should retain:

- who initiated the thread
- what repo and goal were targeted
- what artifacts were generated
- what approvals were requested and how they were decided
- what deterministic steps executed
- what deployment identifier and runtime URL were produced

## MVP Rollout

### Phase 1

- conversation persistence
- artifact persistence
- parent-job template storage
- workflow compiler
- workflow runtime state machine

### Phase 2

- AI discovery adapter
- AI planning adapter
- approval service
- repo patch adapter
- repo verify adapter

### Phase 3

- Heyo deploy adapter
- Heyo health-check adapter
- end-to-end `app.integrate_with_heyo_and_deploy` parent-job workflow

## Definition Of Ready

This orchestration service is ready for the first real app integration task when it can:

1. accept a repo root and natural-language objective
2. create a thread and durable artifacts
3. compile a valid parent job from a reusable template
4. pause for plan approval
5. create and execute child jobs under that parent job
6. run verification through a deterministic adapter
7. deploy the revised app to a Heyo sandbox through a deterministic adapter
8. return a final deploy report artifact with runtime health and identifiers

## Final Recommendation

Build the first version of this service as a new Heyo control-plane capability with:

1. Postgres as the source of truth for orchestration state.
2. AI constrained to typed planning tools.
3. Parent-job/child-job execution with resumable state.
4. Approval gates before every meaningful side effect.
5. Heyo sandbox deployment as the only supported deploy target in v1.

This is the smallest architecture that can reproduce the successful "talk to Amp, integrate an app, deploy it" experience as a repeatable product capability rather than an ad hoc session.
