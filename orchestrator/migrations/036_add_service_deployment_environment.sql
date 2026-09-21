ALTER TABLE service_deployment_states
    ADD COLUMN IF NOT EXISTS deployment_environment TEXT;

CREATE OR REPLACE FUNCTION preserve_service_deployment_environment()
RETURNS TRIGGER AS $$
BEGIN
    IF OLD.deployment_environment IS NOT NULL
       AND NEW.deployment_environment IS DISTINCT FROM OLD.deployment_environment THEN
        RAISE EXCEPTION 'deployment environment for service % is immutable', OLD.service_id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS preserve_service_deployment_environment
    ON service_deployment_states;
CREATE TRIGGER preserve_service_deployment_environment
BEFORE UPDATE OF deployment_environment ON service_deployment_states
FOR EACH ROW EXECUTE FUNCTION preserve_service_deployment_environment();
