-- Who is holding a claimed VM, and until when.
--
-- Same rules as the rest: every statement idempotent, because the whole
-- directory is re-executed on each startup.
--
-- Without this, "is anybody working on this VM" was answered by the *job's*
-- status — and a job left `running` by a process that died reads exactly like a
-- job another instance is running right now. So `release_orphans` had to leave
-- it alone, and an orchestrator could not reclaim even its own VMs after a
-- restart: they stayed `claimed` until their TTL reaped the sandbox, and the row
-- leaked until some later restart happened to find the job terminal.
--
-- A lease answers the question directly. The instance holding a VM renews
-- `leased_until` on a timer; if it stops, nothing is renewing, and the VM is
-- reclaimable no matter what the job row says. That is a fact about the *holder*
-- rather than an inference from the work.
ALTER TABLE ci_vm_pool ADD COLUMN IF NOT EXISTS leased_by    TEXT;
ALTER TABLE ci_vm_pool ADD COLUMN IF NOT EXISTS leased_until TIMESTAMPTZ;

-- The reclaim scan is "claimed rows whose lease has run out", and the heartbeat
-- is "rows I hold". Both are covered by this.
CREATE INDEX IF NOT EXISTS ci_vm_pool_lease_idx
    ON ci_vm_pool (status, leased_until);
