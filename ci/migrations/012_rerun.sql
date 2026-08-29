-- Re-runs from the dashboard: "Run again" and "Re-run failed jobs".
--
-- Same rules as the rest: idempotent, because the directory is re-executed on
-- every startup.
--
-- A re-run is a *new* run of an old run's stored source, so what it needs is
-- lineage, not state. `rerun_of` points a run at the one it re-plays, which is
-- what the run page links in both directions. `carried_from` marks a job of a
-- failed-only re-run whose result and outputs were copied from its counterpart
-- in that run rather than earned here — it names the run they came from, and
-- the job page says so instead of showing an empty step list as a success.
ALTER TABLE ci_run ADD COLUMN IF NOT EXISTS rerun_of     TEXT;
ALTER TABLE ci_job ADD COLUMN IF NOT EXISTS carried_from TEXT;
CREATE INDEX IF NOT EXISTS ci_run_rerun_of ON ci_run (rerun_of) WHERE rerun_of IS NOT NULL;
