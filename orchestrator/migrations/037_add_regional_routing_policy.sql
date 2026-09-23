-- Policy and endpoint membership share one authoritative generation. This is
-- desired topology, not a claim that any gateway has adopted it or drained.
ALTER TABLE service_discovery_sets
    ADD COLUMN IF NOT EXISTS regional_policy JSONB;
