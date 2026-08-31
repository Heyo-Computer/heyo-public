-- `ci/upload-artifact` with `public: true`: the store marks the blob
-- anonymously downloadable and the orchestrator records the resulting link,
-- so the run page can show it and a person does not have to reconstruct it
-- from the sink's URL and the digest.
--
-- Same rules as the rest: idempotent, because the directory is re-executed on
-- every startup. Null for every artifact uploaded privately, and for every one
-- stored before this column existed — a public flag set by hand in the store
-- is not reflected here.
ALTER TABLE ci_artifact ADD COLUMN IF NOT EXISTS public_url TEXT;
