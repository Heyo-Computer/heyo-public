CREATE TABLE IF NOT EXISTS orchestration_threads (
    id TEXT PRIMARY KEY,
    title TEXT,
    created_by TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'archived')),
    context JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS orchestration_messages (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES orchestration_threads(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'tool', 'system')),
    content JSONB NOT NULL,
    metadata JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_orch_messages_thread_created_at
    ON orchestration_messages(thread_id, created_at);

CREATE TABLE IF NOT EXISTS orchestration_workflow_templates (
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

CREATE TABLE IF NOT EXISTS orchestration_workflow_runs (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES orchestration_threads(id) ON DELETE CASCADE,
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

CREATE INDEX IF NOT EXISTS idx_orch_workflow_runs_thread_created_at
    ON orchestration_workflow_runs(thread_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_orch_workflow_runs_status
    ON orchestration_workflow_runs(status);

CREATE TABLE IF NOT EXISTS orchestration_step_runs (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT NOT NULL REFERENCES orchestration_workflow_runs(id) ON DELETE CASCADE,
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

CREATE INDEX IF NOT EXISTS idx_orch_step_runs_workflow_status
    ON orchestration_step_runs(workflow_run_id, status);

CREATE INDEX IF NOT EXISTS idx_orch_step_runs_idempotency_key
    ON orchestration_step_runs(idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS orchestration_artifacts (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES orchestration_threads(id) ON DELETE CASCADE,
    workflow_run_id TEXT REFERENCES orchestration_workflow_runs(id) ON DELETE CASCADE,
    step_run_id TEXT REFERENCES orchestration_step_runs(id) ON DELETE SET NULL,
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

CREATE INDEX IF NOT EXISTS idx_orch_artifacts_thread_kind
    ON orchestration_artifacts(thread_id, kind, created_at DESC);

CREATE TABLE IF NOT EXISTS orchestration_approvals (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES orchestration_threads(id) ON DELETE CASCADE,
    workflow_run_id TEXT NOT NULL REFERENCES orchestration_workflow_runs(id) ON DELETE CASCADE,
    step_run_id TEXT NOT NULL REFERENCES orchestration_step_runs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('plan', 'patch', 'deploy')),
    status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
    request_artifact_ids JSONB NOT NULL,
    decided_by TEXT,
    response_comment TEXT,
    decided_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_orch_approvals_workflow_status
    ON orchestration_approvals(workflow_run_id, status);

CREATE TABLE IF NOT EXISTS orchestration_tool_call_logs (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES orchestration_threads(id) ON DELETE CASCADE,
    workflow_run_id TEXT REFERENCES orchestration_workflow_runs(id) ON DELETE CASCADE,
    step_run_id TEXT REFERENCES orchestration_step_runs(id) ON DELETE CASCADE,
    tool_name TEXT NOT NULL,
    input JSONB NOT NULL,
    output JSONB,
    status TEXT NOT NULL CHECK (status IN ('started', 'completed', 'failed')),
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_orch_tool_logs_step_started_at
    ON orchestration_tool_call_logs(step_run_id, started_at DESC);
