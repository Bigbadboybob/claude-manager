-- Enforce uniqueness on warm_vms.vm_name so two concurrent dispatchers
-- can't race on the same generated instance name. The application now
-- pre-generates a name with a uuid suffix, but the DB-side constraint
-- is what makes the race actually safe (the second insert fails fast
-- instead of producing two rows that both try to create the same GCE
-- instance and trip a 409 Conflict).
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'warm_vms_vm_name_key'
    ) THEN
        ALTER TABLE warm_vms ADD CONSTRAINT warm_vms_vm_name_key UNIQUE (vm_name);
    END IF;
END $$;
