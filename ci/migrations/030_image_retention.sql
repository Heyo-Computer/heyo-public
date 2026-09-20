-- Start existing images' grace period at migration, not their historic build.
ALTER TABLE ci_vm_image ADD COLUMN IF NOT EXISTS last_used_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE ci_vm_image ADD COLUMN IF NOT EXISTS cleanup_after TIMESTAMPTZ NOT NULL DEFAULT now();
