-- Creation provenance, never reconstructed from mutable service metadata.
CREATE TABLE IF NOT EXISTS service_creation_recipes (
    deployment_id TEXT PRIMARY KEY,
    service_id TEXT NOT NULL,
    recipe JSONB NOT NULL,
    request_digest TEXT NOT NULL,
    binding JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE OR REPLACE FUNCTION protect_service_creation_recipe() RETURNS trigger AS $$
BEGIN
    IF TG_OP='DELETE' OR ROW(NEW.deployment_id,NEW.service_id,NEW.recipe,NEW.request_digest)
        IS DISTINCT FROM ROW(OLD.deployment_id,OLD.service_id,OLD.recipe,OLD.request_digest)
        OR (OLD.binding IS NOT NULL AND NEW.binding IS DISTINCT FROM OLD.binding) THEN
        RAISE EXCEPTION 'Creation recipe and established binding are immutable';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE OR REPLACE TRIGGER protect_service_creation_recipe
    BEFORE UPDATE OR DELETE ON service_creation_recipes FOR EACH ROW
    EXECUTE FUNCTION protect_service_creation_recipe();
