-- Reactivation invalidates old retirement authority permanently, including recovery
-- performed through a direct state update. Preserve the audit history and do not
-- rewrite the historical rollout's result.
CREATE OR REPLACE FUNCTION cancel_reactivated_service_retirements()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.active_deployment_id IS NULL THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'UPDATE' THEN
        IF NEW.active_deployment_id IS NOT DISTINCT FROM OLD.active_deployment_id THEN
            RETURN NEW;
        END IF;
    END IF;
    INSERT INTO service_deployment_events
        (deployment_id, service_id, phase, status, message, metadata)
    SELECT DISTINCT intent.deployment_id, intent.service_id,
        'previous-retire-cancelled', 'passed',
        'Retirement cancelled because the target deployment was made active again.',
        jsonb_build_object('response', jsonb_build_object(
            'previousDeploymentId', NEW.active_deployment_id))
    FROM service_deployment_events intent
    WHERE intent.service_id = NEW.service_id
        AND intent.phase = 'previous-retire-wait'
        AND intent.metadata->'response'->>'previousDeploymentId' = NEW.active_deployment_id
        AND NOT EXISTS (
            SELECT 1 FROM service_deployment_events cancelled
            WHERE cancelled.deployment_id = intent.deployment_id
                AND cancelled.phase = 'previous-retire-cancelled'
                AND cancelled.metadata->'response'->>'previousDeploymentId' = NEW.active_deployment_id
        );
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS cancel_reactivated_service_retirements ON service_deployment_states;
CREATE TRIGGER cancel_reactivated_service_retirements
AFTER INSERT OR UPDATE OF active_deployment_id ON service_deployment_states
FOR EACH ROW EXECUTE FUNCTION cancel_reactivated_service_retirements();

-- Also protect deployments restored before this migration existed. Re-running
-- migrations must not add duplicate cancellation events.
INSERT INTO service_deployment_events
    (deployment_id, service_id, phase, status, message, metadata)
SELECT DISTINCT intent.deployment_id, intent.service_id,
    'previous-retire-cancelled', 'passed',
    'Retirement cancelled because its target is currently active.',
    jsonb_build_object('response', jsonb_build_object(
        'previousDeploymentId', state.active_deployment_id))
FROM service_deployment_events intent
JOIN service_deployment_states state ON state.service_id = intent.service_id
    AND state.active_deployment_id = intent.metadata->'response'->>'previousDeploymentId'
WHERE intent.phase = 'previous-retire-wait'
    AND NOT EXISTS (
        SELECT 1 FROM service_deployment_events cancelled
        WHERE cancelled.deployment_id = intent.deployment_id
            AND cancelled.phase = 'previous-retire-cancelled'
            AND cancelled.metadata->'response'->>'previousDeploymentId' = state.active_deployment_id
    );
