CREATE TABLE IF NOT EXISTS regional_service_rollouts (
    operation_id TEXT PRIMARY KEY,
    service_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    deployment_request JSONB NOT NULL,
    target_revision TEXT NOT NULL,
    observer_topology TEXT NOT NULL,
    baseline_state JSONB NOT NULL,
    regions JSONB NOT NULL,
    slots JSONB NOT NULL,
    plan JSONB NOT NULL,
    old_endpoints JSONB NOT NULL DEFAULT '{}'::jsonb,
    minimum_serving_replicas INTEGER NOT NULL CHECK (minimum_serving_replicas > 0),
    bake_seconds BIGINT NOT NULL CHECK (bake_seconds > 0),
    drain_timeout_seconds BIGINT NOT NULL CHECK (drain_timeout_seconds > 0),
    status TEXT NOT NULL CHECK (status IN ('running', 'passed', 'blocked', 'rolled_back')),
    phase TEXT NOT NULL,
    region_index INTEGER NOT NULL DEFAULT 0 CHECK (region_index >= 0),
    slot_index INTEGER NOT NULL DEFAULT 0 CHECK (slot_index >= 0),
    discovery_version BIGINT,
    deadline_at TIMESTAMPTZ,
    last_observed_at TIMESTAMPTZ,
    error_message TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS regional_service_rollouts_active_service_idx
    ON regional_service_rollouts (service_id)
    WHERE status IN ('running', 'blocked');

CREATE INDEX IF NOT EXISTS regional_service_rollouts_running_idx
    ON regional_service_rollouts (updated_at)
    WHERE status = 'running';

CREATE TABLE IF NOT EXISTS service_region_drains (
    service_id TEXT NOT NULL REFERENCES service_discovery_sets(service_id),
    region TEXT NOT NULL,
    PRIMARY KEY (service_id, region)
);

CREATE TABLE IF NOT EXISTS regional_rollout_items (
    operation_id TEXT NOT NULL REFERENCES regional_service_rollouts(operation_id),
    step_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending','running','blocked','completed','interrupted')),
    attempts INTEGER NOT NULL DEFAULT 0,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    error_message TEXT,
    PRIMARY KEY (operation_id, step_id)
);

CREATE TABLE IF NOT EXISTS regional_rollout_events (
    id BIGSERIAL PRIMARY KEY,
    operation_id TEXT NOT NULL REFERENCES regional_service_rollouts(operation_id),
    step_id TEXT NOT NULL,
    status TEXT NOT NULL,
    discovery_version BIGINT,
    deadline_at TIMESTAMPTZ,
    error_message TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS regional_rollout_events_operation_idx
    ON regional_rollout_events(operation_id, id);

-- Journal all writers (including resume, rollback, and terminal transitions)
-- in the same transaction as their cursor update. Probe heartbeats alone do
-- not create history. The plan and its inputs cannot change underneath workers.
CREATE OR REPLACE FUNCTION record_regional_rollout_progress() RETURNS TRIGGER AS $$
DECLARE
    current_step TEXT;
    previous_step TEXT;
BEGIN
    current_step := NEW.region_index || ':' || NEW.phase || ':' || NEW.slot_index;
    IF TG_OP = 'UPDATE' THEN
        IF ROW(NEW.operation_id,NEW.service_id,NEW.request_hash,NEW.deployment_request,
            NEW.target_revision,NEW.observer_topology,NEW.baseline_state,NEW.regions,
            NEW.slots,NEW.plan,NEW.minimum_serving_replicas,NEW.bake_seconds,NEW.drain_timeout_seconds)
            IS DISTINCT FROM
            ROW(OLD.operation_id,OLD.service_id,OLD.request_hash,OLD.deployment_request,
            OLD.target_revision,OLD.observer_topology,OLD.baseline_state,OLD.regions,
            OLD.slots,OLD.plan,OLD.minimum_serving_replicas,OLD.bake_seconds,OLD.drain_timeout_seconds)
        THEN
            RAISE EXCEPTION 'An admitted regional rollout plan cannot be rewritten';
        END IF;
        IF ROW(NEW.phase,NEW.region_index,NEW.slot_index,NEW.status,NEW.discovery_version,NEW.deadline_at,NEW.error_message)
            IS NOT DISTINCT FROM
            ROW(OLD.phase,OLD.region_index,OLD.slot_index,OLD.status,OLD.discovery_version,OLD.deadline_at,OLD.error_message)
        THEN
            RETURN NEW;
        END IF;
        previous_step := OLD.region_index || ':' || OLD.phase || ':' || OLD.slot_index;
        IF previous_step <> current_step THEN
            IF NEW.phase NOT LIKE 'rollback_%' AND NEW.phase <> 'rolled_back' AND NOT EXISTS (
                SELECT 1 FROM jsonb_array_elements(NEW.plan->'steps') item
                WHERE item->>'id' = current_step AND item->>'dependsOn' = previous_step
            ) THEN
                RAISE EXCEPTION 'Forward transition skips a persisted plan dependency';
            END IF;
            UPDATE regional_rollout_items SET
                status = CASE WHEN NEW.phase = 'rollback_restore' AND OLD.phase NOT LIKE 'rollback_%'
                    THEN 'interrupted' ELSE 'completed' END,
                completed_at = NOW()
            WHERE operation_id = NEW.operation_id AND step_id = previous_step;
        END IF;
    ELSE
        INSERT INTO regional_rollout_items(operation_id,step_id,status)
            SELECT NEW.operation_id,item->>'id','pending'
            FROM jsonb_array_elements((NEW.plan->'steps') || (NEW.plan->'rollbackSteps')) item;
    END IF;
    UPDATE regional_rollout_items SET
        status = CASE WHEN NEW.status IN ('passed','rolled_back') THEN 'completed' ELSE NEW.status END,
        attempts = attempts + CASE WHEN NEW.status = 'running' AND status <> 'running' THEN 1 ELSE 0 END,
        started_at = COALESCE(started_at,NOW()),
        completed_at = CASE WHEN NEW.status IN ('passed','rolled_back') THEN NOW() ELSE NULL END,
        error_message = NEW.error_message
    WHERE operation_id = NEW.operation_id AND step_id = current_step;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'Execution cursor is not in the persisted regional rollout plan';
    END IF;
    INSERT INTO regional_rollout_events(operation_id,step_id,status,discovery_version,deadline_at,error_message)
        VALUES(NEW.operation_id,current_step,NEW.status,NEW.discovery_version,NEW.deadline_at,NEW.error_message);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE TRIGGER regional_rollout_progress
    AFTER INSERT OR UPDATE ON regional_service_rollouts
    FOR EACH ROW EXECUTE FUNCTION record_regional_rollout_progress();
