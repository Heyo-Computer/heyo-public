-- Which VM images this orchestrator has built on which runner.
--
-- Same rules as the rest: every statement idempotent, because the whole
-- directory is re-executed on each startup.
--
-- **This table exists because the daemon has no route to list its images.**
-- `heyvm mvm images` reads `~/.heyo/images/firecracker/` on the machine it runs
-- on; there is no HTTP equivalent, so "does this host already have the image
-- this job needs" cannot be asked over the iroh tunnel. Without an answer, a job
-- would either rebuild a multi-gigabyte rootfs every time or find out by
-- attempting a create and reading the failure — one wastes an hour, the other
-- makes the common path an error path.
--
-- Same reasoning as `ci_vm_pool`, which is likewise the source of truth rather
-- than the daemon: a record kept here is one indexed query, and drift is
-- self-healing — a create that fails because the file is gone forgets the row
-- and the next job rebuilds, exactly as `acquire_vm` already recovers from a
-- pooled VM the daemon lost.
--
-- Keyed on `(name, runner_hd_id)` and not on name alone: an image is a file on
-- one host's disk. The same name on two runners is two builds, and one host
-- having it says nothing about the other.
CREATE TABLE IF NOT EXISTS ci_vm_image (
    -- `ci-img-<12 hex>`, the content hash of the Dockerfile's directives and
    -- every byte of its build context. The name *is* the cache key: identical
    -- inputs name an image the host already has, and any change names one it
    -- does not. Nothing has to remember to invalidate anything.
    name            TEXT        NOT NULL,
    runner_hd_id    TEXT        NOT NULL,
    -- What asked for it first, for the dashboard. Not part of the key: two
    -- workflows with byte-identical Dockerfiles legitimately share an image.
    workflow_id     TEXT        NOT NULL DEFAULT '',
    status          TEXT        NOT NULL
        CHECK (status IN ('building','ready','failed')),
    built_by_job    TEXT,
    size_bytes      BIGINT      NOT NULL DEFAULT 0,
    -- Why the last attempt failed. Kept on a `failed` row rather than deleting
    -- it, so /vms can say what happened instead of showing nothing and letting
    -- the next job rediscover it.
    error           TEXT,
    -- Held while a build runs. This is what stops two concurrent jobs on one
    -- host from both building the same image: the upsert only takes a row whose
    -- lease has lapsed, so exactly one wins and the other waits. A lapsed lease
    -- is taken over rather than waited on, so a dispatcher that died mid-build
    -- does not block the image for ever.
    leased_until    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    ready_at        TIMESTAMPTZ,
    PRIMARY KEY (name, runner_hd_id)
);

-- The inventory query: everything on the hosts this instance serves.
CREATE INDEX IF NOT EXISTS ci_vm_image_runner_idx
    ON ci_vm_image (runner_hd_id, created_at DESC);
