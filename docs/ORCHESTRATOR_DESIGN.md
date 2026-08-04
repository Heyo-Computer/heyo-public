# Heyo Orchestrator Service - Complete Design Document

## 1. Overview

The **Heyo Orchestrator** is a distributed workflow execution engine that orchestrates complex, multi-step application integration, testing, and deployment workflows. It implements a hybrid AI/deterministic model where AI handles discovery and planning (non-side-effecting), while deterministic adapters execute all side effects (code mutation, verification, deployment).

**Key Design Principle**: Safe AI-assisted workflows with human approval gates and durable execution guarantees.

---

## 2. Architecture

### Core Components

#### 2.1 OrchestrationExecutor
- In-process state machine that advances workflows through their execution graph
- Loops through eligible steps based on status and dependency satisfaction
- Continues until hitting: approval gate, async wait, failure, or completion
- Implements ORCA-style idempotent, durable step execution

#### 2.2 Step Adapters
Pluggable logic for specific task types:

| Adapter | Type | Function |
|---------|------|----------|
| `ai.discovery` | Agent | Analyzes repository structure, detects frameworks, technologies |
| `ai.planning` | Agent | Generates integration plan from discovery insights |
| `repo.patch` | Deterministic | Applies diffs using `git apply` with verification |
| `repo.verify` | Deterministic | Runs build/test commands (cargo, bun, pytest, etc.) |
| `heyo.deploy` | Deterministic | Archives repo and dispatches sandbox deployment via NATS |
| `heyo.healthcheck` | Deterministic | Probes deployed sandbox proxy endpoint for readiness |

#### 2.3 OrchestrationRepository
- Atomic state machine for workflow and step transitions
- Manages artifact persistence and approval records
- Implements transaction-based durability for async operations
- Correlates external events back to workflow steps via `external_ref`

#### 2.4 Cloud Integration Layer (`src/cloud_client.rs`)
- Communicates with Heyo Cloud for sandbox archiving and deployment
- Publishes deployment commands to NATS JetStream
- Subscribes to Cloud lifecycle events for resumption

#### 2.5 Background Runtime Reconciler
- Continuously monitors pending/ready/blocked steps
- Claims work and advances steps through the executor
- Handles crash recovery and lease management

---

## 3. Workflow System

### 3.1 Parent and Child Jobs

**Parent Job (`workflow_run`)**
- Top-level coordinating object representing a user goal
- Example: "Integrate Heyo auth into Colanode"
- Contains compiled execution plan from workflow template
- Tracks overall phase progression (discovering → planning → implementing → verifying → deploying)

**Child Job (`step_run`)**
- Atomic unit of work within the parent job
- Represents a single adapter invocation (e.g., "Run discovery", "Apply patch")
- Has explicit inputs, outputs, and dependencies
- Tracks idempotency via `idempotency_key` to prevent duplicate execution

### 3.2 Step Lifecycle States

```
pending → ready → running → [waiting_approval | blocked | completed | failed]
                    ↓
            waiting_approval → approved → ready (resume)
                    ↓
                 rejected (fail)
```

| State | Meaning |
|-------|---------|
| **pending** | Waiting for dependencies to complete |
| **ready** | Dependencies satisfied; eligible for claiming |
| **running** | Currently executing (agent or deterministic) |
| **waiting_approval** | Paused for human-in-the-loop review |
| **blocked** | Waiting for async external event (deployment) |
| **completed** | Step finished with outputs |
| **failed** | Step encountered error |
| **cancelled** | Workflow was aborted |

### 3.3 Workflow Templates

Templates define the structure of workflows:
- Workflow ID (e.g., `app.integrate_with_heyo_and_deploy`)
- Version number (for tracking iterations)
- Input schema
- Output schema
- Phase graph (dependency DAG)
- Step templates (definition of each step)
- Policy constraints (retry limits, timeouts, approval requirements)

The source-of-truth authoring format should be YAML, not JSON. For example, workflow templates should live as files such as `orchestrator/templates/<template-id>.yaml`. The orchestrator may still compile those YAML templates into JSON values and persist them in Postgres JSONB columns for execution, indexing, and API responses, but operators should edit YAML.

Example template:
```yaml
id: app.integrate_with_heyo_and_deploy
version: 1
name: Discover, Plan, and Deploy App to Heyo
phaseGraph:
  - discovering
  - planning
  - implementing
  - verifying
  - deploying
steps:
  - key: discover
    type: agent
    kind: analysis
    adapter: ai.discovery
    requiresApprovalBeforeStart: false
  - key: plan
    type: agent
    kind: planning
    adapter: ai.planning
    dependsOn:
      - discover
  - key: patch
    type: deterministic
    kind: mutation
    adapter: repo.patch
    dependsOn:
      - plan
    requiresApprovalBeforeStart: true
```

This same YAML-first approach should apply to extensibility. If we add an action/plugin runtime for CI, plugin manifests should also be YAML-authored. GitHub's model is a good default to copy:

- Heyo-local workflow definitions live in `.heyo/workflows/*.yml` or `.yaml`, with `.github/workflows` supported as a compatibility source
- action metadata lives in `action.yml` or `action.yaml`
- the action manifest declares `inputs`, `outputs`, and `runs`
- `runs.using` selects a runtime such as `composite`, `node20`/`node24`, or `docker`

Heyo-native plugins should prefer that same shape so a customer can provide an `action.yml` and have orchestrator normalize it into the internal runtime contract.

---

## 4. Execution Model

### 4.1 Durable Async Deployment Flow

The `heyo.deploy` adapter implements durable handoff to Cloud:

**Step 1: Dispatch Phase (Local)**
1. Adapter generates unique `deployment_id`
2. Archives repository into byte stream
3. Prepares `SandboxDeployCreateCommandPayload`
4. **Persists step as "blocked"** with `external_ref = deployment_id`
5. Publishes command to NATS `sandbox.cmd.create` queue

**Step 2: Cloud Processing (Remote)**
1. Cloud consumes deployment command
2. Provisions sandbox, runs deployment
3. Emits `SANDBOX_STATUS_RUNNING` event to NATS

**Step 3: Resumption (Local)**
1. Orchestrator subscribes to Cloud lifecycle events
2. Correlates `deployment_id` back to step via `external_ref` column
3. Updates step status to `completed` with deployment outputs
4. Re-triggers executor to advance workflow

**Durability Guarantee**: If orchestrator crashes between dispatch and resumption, background reconciler finds "blocked" steps and re-subscribes to events, ensuring no lost work.

### 4.2 Atomic Approval Transitions

To prevent race conditions and duplicate approvals:

```rust
pub async fn begin_approval_step(
    &self,
    thread_id: &str,
    workflow_run_id: &str,
    step_run_id: &str,
    phase: &str,
    kind: &str,
    request_artifact_ids: Vec<String>,
) -> Result<Option<orchestration_approval::Model>> {
    let txn = self.db.begin().await?;
    // All three operations atomic:
    // 1. Insert preview artifact
    // 2. Update step status to 'waiting_approval'
    // 3. Insert orchestration_approval record
    // 4. Update workflow state
    txn.commit().await?;
    Ok(Some(approval))
}
```

Single transaction ensures:
- Cannot have partial approval state
- Cannot create duplicate approval records
- Cannot miss workflow state update

---

## 5. Database Schema

### 5.1 Core Tables (`023_add_orchestration_tables.sql`)

#### `orchestration_threads`
```sql
CREATE TABLE orchestration_threads (
    id TEXT PRIMARY KEY,
    title TEXT,
    created_by TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'archived')),
    context JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```
- Conversation thread containing all user interactions for a workflow
- Stores conversation context and metadata

#### `orchestration_workflow_runs`
```sql
CREATE TABLE orchestration_workflow_runs (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES orchestration_threads(id),
    template_id TEXT NOT NULL,
    template_version INTEGER NOT NULL,
    goal TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('draft', 'compiled', 'waiting_approval', 'running', 'paused', 'failed', 'completed', 'cancelled')),
    phase TEXT NOT NULL,
    target TEXT NOT NULL CHECK (target IN ('heyo-sandbox')),
    inputs JSONB NOT NULL,
    compiled_plan JSONB NOT NULL,
    current_child_job_key TEXT,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```
- Represents a single execution of a workflow template
- Tracks overall progress through phases

#### `orchestration_step_runs`
```sql
CREATE TABLE orchestration_step_runs (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT NOT NULL REFERENCES orchestration_workflow_runs(id),
    key TEXT NOT NULL,
    type TEXT NOT NULL CHECK (type IN ('agent', 'approval', 'deterministic')),
    kind TEXT NOT NULL CHECK (kind IN ('analysis', 'planning', 'mutation', 'verification', 'deployment', 'approval')),
    phase TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'ready', 'running', 'waiting_approval', 'blocked', 'failed', 'completed', 'cancelled')),
    adapter TEXT,
    depends_on JSONB NOT NULL DEFAULT '[]'::jsonb,
    can_fan_out BOOLEAN NOT NULL DEFAULT FALSE,
    requires_approval_before_start BOOLEAN NOT NULL DEFAULT FALSE,
    inputs JSONB NOT NULL DEFAULT '{}'::jsonb,
    outputs JSONB,
    retry_count INTEGER NOT NULL DEFAULT 0,
    max_retries INTEGER NOT NULL DEFAULT 0,
    idempotency_key TEXT,
    external_ref TEXT,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (workflow_run_id, key)
);

CREATE INDEX idx_orch_step_runs_external_ref
    ON orchestration_step_runs(external_ref)
    WHERE external_ref IS NOT NULL;
```
- Core execution unit within a workflow
- `external_ref` correlates async deployments back to steps
- `idempotency_key` prevents duplicate execution on retry

#### `orchestration_approvals`
```sql
CREATE TABLE orchestration_approvals (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES orchestration_threads(id),
    workflow_run_id TEXT NOT NULL REFERENCES orchestration_workflow_runs(id),
    step_run_id TEXT NOT NULL REFERENCES orchestration_step_runs(id),
    kind TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
    request_artifact_ids JSONB NOT NULL,
    decided_by TEXT,
    response_comment TEXT,
    decided_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX idx_orch_approvals_pending_step_run_id
    ON orchestration_approvals(step_run_id)
    WHERE status = 'pending';
```
- Human-in-the-loop approval records
- Unique index prevents duplicate pending approvals

#### `orchestration_artifacts`
```sql
CREATE TABLE orchestration_artifacts (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES orchestration_threads(id),
    workflow_run_id TEXT REFERENCES orchestration_workflow_runs(id),
    step_run_id TEXT REFERENCES orchestration_step_runs(id),
    kind TEXT NOT NULL CHECK (kind IN ('domain-map', 'integration-plan', 'patch-set', 'verification-report', 'deploy-spec', 'deploy-report')),
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
```
- Durable artifacts produced by steps (plans, patches, reports)
- Displayed to users for review/approval

#### `orchestration_messages`
```sql
CREATE TABLE orchestration_messages (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES orchestration_threads(id),
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'tool', 'system')),
    content JSONB NOT NULL,
    metadata JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_orch_messages_thread_created_at
    ON orchestration_messages(thread_id, created_at);
```
- Conversation transcript for thread history

### 5.2 Runtime Hardening Tables (`024_orchestration_runtime_hardening.sql`)

#### `orchestration_step_attempts`
```sql
CREATE TABLE orchestration_step_attempts (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT NOT NULL REFERENCES orchestration_workflow_runs(id),
    step_run_id TEXT NOT NULL REFERENCES orchestration_step_runs(id),
    attempt_number INTEGER NOT NULL,
    worker_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'blocked', 'completed', 'failed', 'abandoned')),
    lease_expires_at TIMESTAMPTZ NOT NULL,
    heartbeat_at TIMESTAMPTZ,
    failure_reason TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (step_run_id, attempt_number)
);

CREATE UNIQUE INDEX idx_orch_step_attempts_active_step_run_id
    ON orchestration_step_attempts(step_run_id)
    WHERE status = 'running';

CREATE INDEX idx_orch_step_attempts_status_lease
    ON orchestration_step_attempts(status, lease_expires_at);
```
- Tracks individual execution attempts for retries
- Lease-based locking for distributed claim

#### `orchestration_external_events`
```sql
CREATE TABLE orchestration_external_events (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT REFERENCES orchestration_workflow_runs(id),
    step_run_id TEXT REFERENCES orchestration_step_runs(id),
    external_ref TEXT NOT NULL,
    event_type TEXT NOT NULL,
    event_status TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    message_id TEXT,
    payload JSONB NOT NULL,
    processing_status TEXT NOT NULL CHECK (processing_status IN ('pending', 'processing', 'processed', 'ignored')),
    processing_error TEXT,
    received_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX idx_orch_external_events_idempotency
    ON orchestration_external_events(external_ref, event_type, idempotency_key);

CREATE INDEX idx_orch_external_events_processing_status
    ON orchestration_external_events(processing_status, received_at);
```
- Durable event log for async operations (deployments)
- Idempotency key prevents duplicate processing
- Tracks processing status for resumability

---

## 6. API Surface

### 6.1 Public HTTP Endpoints

#### `GET /orchestration/templates`
Lists available workflow templates.

Response:
```json
{
  "templates": [
    {
      "id": "app.integrate_with_heyo_and_deploy",
      "version": 1,
      "name": "Discover, Plan, and Deploy App",
      "description": "..."
    }
  ]
}
```

#### `POST /orchestration/threads`
Creates a new orchestration thread (conversation + workflow).

Request:
```json
{
  "title": "Integrating my app",
  "goal": "Integrate Heyo auth into my application",
  "template_id": "app.integrate_with_heyo_and_deploy",
  "inputs": {
    "repo_url": "https://github.com/user/app",
    "branch": "main"
  }
}
```

Response:
```json
{
  "thread": {
    "id": "thread-123",
    "title": "...",
    "created_by": "user-456",
    "status": "active",
    "workflow_run": { ... }
  }
}
```

#### `GET /orchestration/threads/{thread_id}/timeline`
Retrieves thread execution history with artifacts and messages.

Response:
```json
{
  "thread": { ... },
  "workflow_run": { ... },
  "messages": [ ... ],
  "artifacts": [ ... ],
  "approvals": [ ... ]
}
```

#### `POST /orchestration/approvals/{approval_id}/decide`
User approves or rejects a step's output.

Request:
```json
{
  "decision": "approved",
  "comment": "Looks good!"
}
```

### 6.2 Internal Endpoints (Cloud ↔ Orchestrator)

#### `POST /internal/deployments/lifecycle`
Cloud publishes deployment lifecycle events.

Request:
```json
{
  "external_ref": "deployment-xyz",
  "event_type": "SANDBOX_STATUS_RUNNING",
  "event_status": "success",
  "payload": {
    "sandbox_id": "sandbox-123",
    "proxy_url": "https://app-xyz.heyo.computer"
  }
}
```

---

## 7. Configuration

### Required Environment Variables

| Variable | Description |
|----------|-------------|
| `DATABASE_URL` | PostgreSQL connection string |
| `JWT_SECRET` | Shared secret with Auth service for token verification |
| `CLOUD_INTERNAL_API_KEY` | API key for Cloud internal endpoints |
| `ORCHESTRATOR_SERVER_PORT` | HTTP server port (default: 4446) |

### Optional

| Variable | Default | Description |
|----------|---------|-------------|
| `MVM_CTRL_BACKEND_SCHEME` | `http` | Scheme for mvm-ctrl backend |
| `ORCHESTRATOR_AMP_COMMAND` | `amp` | Command to invoke Amp CLI |
| `ORCHESTRATOR_AMP_MODE` | `smart` | Amp execution mode |
| `ORCHESTRATOR_AMP_TIMEOUT_SECONDS` | `900` | Timeout for Amp operations |

---

## 8. Security

- **JWT Authentication**: All endpoints require valid JWT token from Auth service
- **API Key Authentication**: Internal Cloud endpoints require `CLOUD_INTERNAL_API_KEY`
- **Input Validation**: Repository paths validated and canonicalized
- **Artifact Isolation**: Artifacts scoped to threads/workflows
- **Approval Gates**: Critical mutations require user approval

---

## 9. Testing

### Unit Tests
- Step executor state transitions
- Artifact persistence
- Approval atomic transitions

### Integration Tests
- Full workflow execution from template to completion
- Async deployment flow with event correlation
- Approval review and decision flow
- Retry and crash recovery

### E2E Tests
- Create thread → compile workflow → execute steps → approve → deploy → verify

---

## 10. Future Enhancements

- **Conditional Steps**: Branch workflows based on outputs
- **Parallel Steps**: Execute non-dependent steps concurrently
- **Rollback**: Automatic rollback on deployment failure
- **Metrics**: OpenTelemetry instrumentation for observability
- **Custom Adapters**: Plugin system for domain-specific steps
- **Secrets Management**: Integration with secure secret storage

---

## 11. Files

**Core Implementation**
- `orchestrator/src/main.rs` - Entry point and HTTP server
- `orchestrator/src/config.rs` - Configuration loading
- `orchestrator/src/db/mod.rs` - Database migrations
- `orchestrator/src/orchestration/executor.rs` - Step execution engine
- `orchestrator/src/orchestration/adapters.rs` - Step adapters
- `orchestrator/src/orchestration/repository.rs` - State management
- `orchestrator/src/orchestration/runtime.rs` - Background reconciler
- `orchestrator/src/cloud_client.rs` - Cloud integration
- `orchestrator/src/handlers/orchestration.rs` - HTTP handlers

**Migrations**
- `orchestrator/migrations/023_add_orchestration_tables.sql`
- `orchestrator/migrations/024_orchestration_runtime_hardening.sql`
- `orchestrator/migrations/025_add_orchestration_attempts_and_events.sql`
