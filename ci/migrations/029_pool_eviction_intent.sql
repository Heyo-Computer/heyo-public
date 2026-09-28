-- Draining also means an in-flight resize. Only explicit eviction intent
-- authorizes the automatic retry worker to delete a VM after restart.
ALTER TABLE ci_vm_pool ADD COLUMN IF NOT EXISTS eviction_requested BOOLEAN NOT NULL DEFAULT FALSE;
