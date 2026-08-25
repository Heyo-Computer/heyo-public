-- What the daemon says a pooled VM was actually given, beside what the job
-- asked for (`size_class`, migration 010).
--
-- Same rules as the rest: idempotent, because the directory is re-executed on
-- every startup.
--
-- Read back from the runner's heyvmd (`GET /sandboxes/<id>`) every time this
-- app touches the VM — creation, claim, resize — and stored so /vms can show
-- it without a round trip per row. `observed_size` is the daemon's own class
-- name; `observed_cpus`/`observed_memory` are the numbers behind it, and the
-- only thing a daemon too old to report a class gives. All nullable: an old
-- daemon reports nothing, and "unreported" must stay distinguishable from
-- "small".
ALTER TABLE ci_vm_pool ADD COLUMN IF NOT EXISTS observed_size   TEXT;
ALTER TABLE ci_vm_pool ADD COLUMN IF NOT EXISTS observed_cpus   INTEGER;
ALTER TABLE ci_vm_pool ADD COLUMN IF NOT EXISTS observed_memory BIGINT;
ALTER TABLE ci_vm_pool ADD COLUMN IF NOT EXISTS observed_at     TIMESTAMPTZ;
