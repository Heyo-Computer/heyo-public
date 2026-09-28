-- Preserve v1/v2 journaling verbatim. V3 shares phase names but not occurrence
-- identities between forward and rollback programs; prefixes grant no bypass.
CREATE OR REPLACE FUNCTION record_application_rollout_progress() RETURNS TRIGGER AS $$
DECLARE
    current_step TEXT;
    previous_step TEXT;
    previous_id TEXT;
    item JSONB;
    program JSONB;
    old_forward BOOLEAN;
    new_forward BOOLEAN;
    rollback_entry BOOLEAN := FALSE;
BEGIN
    IF NEW.plan->>'version' IS DISTINCT FROM '3' THEN
        RAISE EXCEPTION 'Unsupported regional plan version';
    END IF;
    current_step := NEW.region_index || ':' || NEW.phase || ':' || NEW.slot_index;
    IF TG_OP = 'INSERT' THEN
        IF jsonb_typeof(NEW.plan->'steps') IS DISTINCT FROM 'array'
            OR jsonb_typeof(NEW.plan->'rollbackSteps') IS DISTINCT FROM 'array'
            OR jsonb_array_length(NEW.plan->'steps') = 0
            OR jsonb_array_length(NEW.plan->'rollbackSteps') = 0
            OR NEW.plan->'steps'->0->>'id' IS DISTINCT FROM current_step
            OR NEW.phase <> 'preflight' OR NEW.status <> 'running'
            OR NEW.plan->'steps'->-1->>'phase' IS DISTINCT FROM 'passed'
            OR NEW.plan->'rollbackSteps'->-1->>'phase' IS DISTINCT FROM 'rolled_back'
        THEN
            RAISE EXCEPTION 'Invalid application program or initial cursor';
        END IF;
        IF EXISTS (
            SELECT 1 FROM jsonb_array_elements((NEW.plan->'steps') || (NEW.plan->'rollbackSteps')) s
            GROUP BY s->>'id' HAVING count(*) <> 1
        ) THEN
            RAISE EXCEPTION 'Application program item identities must be unique';
        END IF;
        FOR program IN SELECT NEW.plan->'steps' UNION ALL SELECT NEW.plan->'rollbackSteps' LOOP
            previous_id := NULL;
            FOR item IN SELECT value FROM jsonb_array_elements(program) LOOP
                IF item->>'id' IS DISTINCT FROM ((item->>'regionIndex') || ':' || (item->>'phase') || ':' || (item->>'slotIndex'))
                    OR item->>'dependsOn' IS DISTINCT FROM previous_id
                    OR (item->>'regionIndex')::integer < 0 OR (item->>'slotIndex')::integer < 0
                    OR item->>'phase' IS NULL OR item->>'id' IS NULL
                THEN
                    RAISE EXCEPTION 'Application program dependency or cursor identity is invalid';
                END IF;
                previous_id := item->>'id';
            END LOOP;
        END LOOP;
        IF jsonb_typeof(NEW.regions) IS DISTINCT FROM 'array' OR jsonb_array_length(NEW.regions) = 0
            OR EXISTS (
                SELECT 1 FROM jsonb_array_elements_text(NEW.regions) WITH ORDINALITY r(region,position)
                WHERE NOT EXISTS (
                    SELECT 1 FROM jsonb_array_elements(NEW.plan->'rollbackSteps') s
                    WHERE s->>'phase'='rollback_entry' AND (s->>'regionIndex')::bigint=r.position-1
                        AND s->>'slotIndex'='2' AND s->>'region'=r.region
                )
            )
        THEN
            RAISE EXCEPTION 'Application program lacks a rollback entry for each region';
        END IF;
        INSERT INTO regional_rollout_items(operation_id,step_id,status)
            SELECT NEW.operation_id,s->>'id','pending'
            FROM jsonb_array_elements((NEW.plan->'steps') || (NEW.plan->'rollbackSteps')) s;
    ELSE
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
        IF OLD.status NOT IN ('running','blocked') THEN
            RAISE EXCEPTION 'A terminal application program cannot restart';
        END IF;
        previous_step := OLD.region_index || ':' || OLD.phase || ':' || OLD.slot_index;
        IF previous_step <> current_step THEN
            SELECT EXISTS (SELECT 1 FROM jsonb_array_elements(NEW.plan->'steps') s WHERE s->>'id'=previous_step) INTO old_forward;
            SELECT EXISTS (SELECT 1 FROM jsonb_array_elements(NEW.plan->'steps') s WHERE s->>'id'=current_step) INTO new_forward;
            rollback_entry := old_forward AND NOT new_forward AND NEW.phase='rollback_entry'
                AND NEW.region_index=OLD.region_index AND NEW.slot_index=2 AND NEW.status='running';
            IF NOT rollback_entry AND (
                OLD.status <> 'running' OR old_forward <> new_forward OR NOT EXISTS (
                    SELECT 1 FROM jsonb_array_elements(CASE WHEN old_forward THEN NEW.plan->'steps' ELSE NEW.plan->'rollbackSteps' END) s
                    WHERE s->>'id'=current_step AND s->>'dependsOn'=previous_step
                )
            ) THEN
                RAISE EXCEPTION 'Application transition skips a persisted dependency or rollback entry';
            END IF;
            IF NOT EXISTS (SELECT 1 FROM regional_rollout_items WHERE operation_id=NEW.operation_id
                AND step_id=current_step AND status='pending' AND attempts=0) THEN
                RAISE EXCEPTION 'Application transition cannot rewind an executed item';
            END IF;
            UPDATE regional_rollout_items SET status=CASE WHEN rollback_entry THEN 'interrupted' ELSE 'completed' END,
                completed_at=clock_timestamp() WHERE operation_id=NEW.operation_id AND step_id=previous_step;
        END IF;
    END IF;
    IF (NEW.phase='passed') IS DISTINCT FROM (NEW.status='passed')
        OR (NEW.phase='rolled_back') IS DISTINCT FROM (NEW.status='rolled_back') THEN
        RAISE EXCEPTION 'Application terminal phase and status disagree';
    END IF;
    UPDATE regional_rollout_items SET
        status=CASE WHEN NEW.status IN ('passed','rolled_back') THEN 'completed' ELSE NEW.status END,
        attempts=attempts + CASE WHEN NEW.status='running' AND status <> 'running' THEN 1 ELSE 0 END,
        started_at=COALESCE(started_at,clock_timestamp()),
        completed_at=CASE WHEN NEW.status IN ('passed','rolled_back') THEN clock_timestamp() ELSE NULL END,
        error_message=NEW.error_message
    WHERE operation_id=NEW.operation_id AND step_id=current_step;
    IF NOT FOUND THEN RAISE EXCEPTION 'Execution cursor is outside the application program'; END IF;
    INSERT INTO regional_rollout_events(operation_id,step_id,status,discovery_version,deadline_at,error_message)
        VALUES(NEW.operation_id,current_step,NEW.status,NEW.discovery_version,NEW.deadline_at,NEW.error_message);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE TRIGGER regional_rollout_progress
    AFTER INSERT OR UPDATE ON regional_service_rollouts FOR EACH ROW
    WHEN (NEW.plan->>'version' IN ('1','2'))
    EXECUTE FUNCTION record_regional_rollout_progress();

CREATE OR REPLACE TRIGGER application_rollout_progress
    AFTER INSERT OR UPDATE ON regional_service_rollouts FOR EACH ROW
    WHEN ((NEW.plan->>'version' IN ('1','2')) IS NOT TRUE)
    EXECUTE FUNCTION record_application_rollout_progress();
