-- New controller updates require durable acceptance by the shared app authority.
-- Existing in-flight operations keep their phase and finish on the original path.
ALTER TABLE ci_controller_rollout DROP CONSTRAINT IF EXISTS ci_controller_rollout_phase_check;
ALTER TABLE ci_controller_rollout ADD CONSTRAINT ci_controller_rollout_phase_check
    CHECK (phase IN ('prepared', 'pending', 'draining', 'quiesced', 'submitting', 'verifying', 'complete'));
ALTER TABLE ci_controller_rollout ADD COLUMN IF NOT EXISTS application_id TEXT;
ALTER TABLE ci_controller_rollout ADD COLUMN IF NOT EXISTS activation_hash TEXT;
