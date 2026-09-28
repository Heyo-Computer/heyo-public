-- Draft intent, immutable policy generations and discovery revisions are distinct.
ALTER TABLE service_discovery_sets
    ADD COLUMN IF NOT EXISTS policy_generation BIGINT NOT NULL DEFAULT 0 CHECK (policy_generation >= 0);

CREATE UNIQUE INDEX IF NOT EXISTS regional_service_rollouts_service_operation_idx
    ON regional_service_rollouts(service_id, operation_id);

CREATE TABLE IF NOT EXISTS regional_policy_proposals (
    service_id TEXT NOT NULL REFERENCES service_discovery_sets(service_id),
    generation BIGINT NOT NULL CHECK (generation > 0),
    operation_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    expected_predecessor BIGINT,
    policy JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (service_id, generation),
    UNIQUE (operation_id, step_id),
    FOREIGN KEY (service_id, operation_id)
        REFERENCES regional_service_rollouts(service_id, operation_id),
    FOREIGN KEY (operation_id, step_id)
        REFERENCES regional_rollout_items(operation_id, step_id),
    FOREIGN KEY (service_id, expected_predecessor)
        REFERENCES regional_policy_proposals(service_id, generation),
    CHECK (expected_predecessor IS NULL OR expected_predecessor < generation)
);

-- One pointer, not independently writable prepared and active policy documents.
CREATE TABLE IF NOT EXISTS service_active_regional_policies (
    service_id TEXT PRIMARY KEY REFERENCES service_discovery_sets(service_id),
    generation BIGINT NOT NULL,
    FOREIGN KEY (service_id, generation)
        REFERENCES regional_policy_proposals(service_id, generation)
);

-- A later operation cannot accidentally reopen peer admission for old work.
CREATE TABLE IF NOT EXISTS service_regional_admission_fences (
    service_id TEXT NOT NULL REFERENCES service_discovery_sets(service_id),
    region TEXT NOT NULL,
    closed_through_generation BIGINT NOT NULL,
    PRIMARY KEY (service_id, region),
    FOREIGN KEY (service_id, closed_through_generation)
        REFERENCES regional_policy_proposals(service_id, generation)
);

-- High-water marks span operations: replaying a report cannot renew its age.
-- Boot IDs are pinned in the operation's immutable observer_topology.
CREATE TABLE IF NOT EXISTS regional_gateway_reports (
    service_id TEXT NOT NULL,
    gateway_id TEXT NOT NULL,
    boot_id TEXT NOT NULL,
    sequence BIGINT NOT NULL CHECK (sequence >= 0),
    invalidated BOOLEAN NOT NULL DEFAULT FALSE,
    operation_id TEXT NOT NULL REFERENCES regional_service_rollouts(operation_id),
    generation BIGINT NOT NULL,
    report JSONB NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    observed_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (service_id, gateway_id, boot_id),
    FOREIGN KEY (service_id, generation)
        REFERENCES regional_policy_proposals(service_id, generation)
);

CREATE OR REPLACE FUNCTION preserve_regional_policy_proposal() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'An operation-owned policy proposal cannot be rewritten or deleted';
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE TRIGGER regional_policy_proposal_immutable
    BEFORE UPDATE OR DELETE ON regional_policy_proposals
    FOR EACH ROW EXECUTE FUNCTION preserve_regional_policy_proposal();
