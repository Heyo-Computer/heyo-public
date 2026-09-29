-- Serialize replacements of the same registered CI deployment, not all CI
-- apps/regions. Existing records remain intact and are never auto-completed.
DROP INDEX IF EXISTS ci_controller_rollout_active;
CREATE UNIQUE INDEX ci_controller_rollout_active
    ON ci_controller_rollout ((request->>'base_url'), (request->>'deployment'))
    WHERE phase <> 'complete';
