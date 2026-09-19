-- Native heyvm bootstrap is fail-closed: every state except exact success fences the target.
CREATE TABLE IF NOT EXISTS ci_host_heyvm_bootstrap (
    id TEXT PRIMARY KEY REFERENCES ci_service_deployment(id),
    runner_hd_id TEXT NOT NULL,
    request JSONB NOT NULL,
    launcher_recipe JSONB NOT NULL,
    deadline TIMESTAMPTZ NOT NULL,
    phase TEXT NOT NULL DEFAULT 'releasing' CHECK (phase IN ('releasing','draining','registering','delivery_ready','armed','polling','failed','passed')),
    delivery_armed BOOLEAN NOT NULL DEFAULT false,
    launcher_deployment_id TEXT NOT NULL,
    launcher_job_id TEXT,
    result JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS ci_host_heyvm_bootstrap_fence
    ON ci_host_heyvm_bootstrap(runner_hd_id) WHERE phase <> 'passed';
