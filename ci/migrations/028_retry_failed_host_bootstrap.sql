-- A later authorized release may supersede a failed bootstrap while preserving
-- the failed deployment record. The replacement is inserted in the same
-- transaction, so the target is never visible without a fence.
ALTER TABLE ci_host_heyvm_bootstrap
    DROP CONSTRAINT IF EXISTS ci_host_heyvm_bootstrap_phase_check;
ALTER TABLE ci_host_heyvm_bootstrap
    ADD CONSTRAINT ci_host_heyvm_bootstrap_phase_check
    CHECK (phase IN ('releasing','draining','registering','delivery_ready','armed','polling','failed','passed','superseded'));

DROP INDEX IF EXISTS ci_host_heyvm_bootstrap_fence;
CREATE UNIQUE INDEX ci_host_heyvm_bootstrap_fence
    ON ci_host_heyvm_bootstrap(runner_hd_id)
    WHERE phase NOT IN ('passed','superseded');
