-- Preserve target trunk separately from the submitted branch identity.
ALTER TABLE ci_run ADD COLUMN IF NOT EXISTS default_branch TEXT;
ALTER TABLE ci_run ADD COLUMN IF NOT EXISTS release_base_sha TEXT;
