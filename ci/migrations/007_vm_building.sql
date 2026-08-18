-- A VM that is being created, before the daemon has given it an id.
--
-- Same rules as the rest: every statement idempotent, because the whole
-- directory is re-executed on each startup.
--
-- Creating a VM is the longest thing a job does before it does anything
-- visible — an iroh dial, then a `POST /sandbox-deploy` that waits up to
-- `BOOT_TIMEOUT` for the machine to come up — and until now none of it was
-- recorded anywhere. A row appeared in this table only once `Sandbox::create`
-- *returned*, so for the whole of that window /vms said "Nothing is pooled" and
-- the job row still said `queued`. A build that was booting and a build whose
-- VM creation was failing and being retried looked identical from every page,
-- and both looked like nothing happening at all.
--
-- `building` is that window made into a state. The row is written before the
-- create is attempted and replaced by the real one when the daemon answers, so
-- the pool table says what is being built as well as what has been built.
--
-- Deliberately part of *this* table rather than one of its own: /vms is the
-- page that answers "what machines are there", and a VM that is three minutes
-- into booting is one of the machines there. A second table would mean a second
-- query, a second join to the run, and a page that shows half its subject.
ALTER TABLE ci_vm_pool DROP CONSTRAINT IF EXISTS ci_vm_pool_status_check;
ALTER TABLE ci_vm_pool ADD CONSTRAINT ci_vm_pool_status_check
    CHECK (status IN ('building','idle','claimed','draining'));

-- `sandbox_id` is the primary key and a `building` row has no sandbox yet, so
-- it is keyed on `building-<job_id>` — derived from the job, which makes a
-- queue redelivery address the same row rather than adding a second one.
--
-- **Nothing may treat that value as a sandbox id.** Every query that reaches a
-- daemon filters on `status`: `claim` and the sweeps take `idle`, the lease
-- queries take `claimed`. `take_one_for_sweep` is the one that took anything
-- not `claimed`, and it now excludes `building` explicitly — destroying a
-- placeholder would post a kill for a sandbox that does not exist.
--
-- The lease is what bounds it. A process that dies mid-create stops renewing,
-- and the row is deleted by the same loop that reclaims expired claims; the
-- half-created sandbox, if there is one, is left to its TTL exactly as it was
-- before this existed.
CREATE INDEX IF NOT EXISTS ci_vm_pool_building_idx
    ON ci_vm_pool (status, leased_until)
    WHERE status = 'building';
