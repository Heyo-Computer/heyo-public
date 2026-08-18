-- Which job last used a pooled VM, kept after the claim is released.
--
-- `claimed_by_job` is nulled on release, which is right — it answers "who holds
-- this now". It cannot answer "which run left this machine behind", and that is
-- the question somebody asks when a build failed and they want the VM it failed
-- on gone rather than handed to the next job.
--
-- Set alongside `claimed_by_job` and never cleared, so an idle VM still says
-- where it came from and can be joined back to its run's outcome.
ALTER TABLE ci_vm_pool ADD COLUMN IF NOT EXISTS last_job TEXT;
