-- A lost controller connection must leave unresolved health work visible. The
-- next owner fences its epoch and resets the bake window before probing again.
ALTER TABLE regional_service_rollouts
    ADD COLUMN IF NOT EXISTS probe_epoch BIGINT NOT NULL DEFAULT 0 CHECK (probe_epoch >= 0),
    ADD COLUMN IF NOT EXISTS probe_step_id TEXT,
    ADD COLUMN IF NOT EXISTS probe_policy_generation BIGINT,
    ADD COLUMN IF NOT EXISTS probe_discovery_version BIGINT,
    ADD COLUMN IF NOT EXISTS probe_expires_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS probe_context JSONB,
    ADD COLUMN IF NOT EXISTS attempt_started_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp();

ALTER TABLE regional_rollout_items ADD COLUMN IF NOT EXISTS evidence JSONB;

CREATE OR REPLACE FUNCTION invalidate_application_probe() RETURNS TRIGGER AS $$
BEGIN
    IF OLD.plan->>'version' = '3' AND
        ROW(NEW.phase,NEW.region_index,NEW.slot_index,NEW.status)
            IS DISTINCT FROM ROW(OLD.phase,OLD.region_index,OLD.slot_index,OLD.status)
    THEN
        NEW.probe_epoch := OLD.probe_epoch + 1;
        NEW.probe_expires_at := NULL;
        NEW.deadline_at := NULL;
        NEW.last_observed_at := NULL;
        IF NEW.status = 'running' THEN
            NEW.attempt_started_at := clock_timestamp();
        END IF;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE TRIGGER application_probe_invalidation
    BEFORE UPDATE ON regional_service_rollouts FOR EACH ROW
    EXECUTE FUNCTION invalidate_application_probe();
