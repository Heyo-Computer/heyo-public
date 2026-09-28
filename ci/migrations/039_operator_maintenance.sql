-- Operator pause is independent of rollout/retirement and never releases ownership.
CREATE TABLE IF NOT EXISTS ci_operator_maintenance (
    operation_id UUID PRIMARY KEY,
    phase TEXT NOT NULL CHECK (phase IN ('draining','paused','running')),
    requested_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS ci_operator_maintenance_active ON ci_operator_maintenance ((TRUE))
    WHERE phase <> 'running';
