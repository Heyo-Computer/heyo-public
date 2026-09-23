-- The create permission is a durable, single-use intent owned by a plan item.
-- No environment values or archive payloads belong in this journal.
CREATE TABLE IF NOT EXISTS regional_candidate_creations (
    operation_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    deployment_id TEXT NOT NULL UNIQUE,
    intent JSONB NOT NULL,
    receipt JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    observed_at TIMESTAMPTZ,
    PRIMARY KEY (operation_id, step_id),
    FOREIGN KEY (operation_id, step_id)
        REFERENCES regional_rollout_items(operation_id, step_id)
);

CREATE OR REPLACE FUNCTION preserve_regional_candidate_creation() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'A regional candidate creation intent cannot be deleted';
    END IF;
    IF ROW(NEW.operation_id,NEW.step_id,NEW.deployment_id,NEW.intent,NEW.created_at)
        IS DISTINCT FROM ROW(OLD.operation_id,OLD.step_id,OLD.deployment_id,OLD.intent,OLD.created_at)
        OR (OLD.receipt IS NOT NULL AND NEW.receipt IS DISTINCT FROM OLD.receipt)
    THEN
        RAISE EXCEPTION 'A regional candidate creation identity cannot be rewritten';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE TRIGGER regional_candidate_creation_immutable
    BEFORE UPDATE OR DELETE ON regional_candidate_creations
    FOR EACH ROW EXECUTE FUNCTION preserve_regional_candidate_creation();
