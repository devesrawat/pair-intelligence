-- Missing foreign keys (review C) and approval/audit columns (spec section 7).
--
-- Foreign keys are added NOT VALID (enforced for every new write immediately, no table scan under
-- a heavy lock) and then VALIDATEd inside a sub-block, the same pattern as 071: if legacy rows
-- violate one, the migration does not fail and does not rewrite them; the constraint stays NOT
-- VALID with a WARNING for manual repair. Re-running is safe: existing constraints are skipped.
DO $$
DECLARE
    c record;
BEGIN
    FOR c IN SELECT * FROM (VALUES
        ('model_calls', 'model_calls_reservation_fk',
         'FOREIGN KEY (reservation_id) REFERENCES budget_reservations (id) ON DELETE RESTRICT'),
        ('workflow_runs', 'workflow_runs_approval_fk',
         'FOREIGN KEY (approval_id) REFERENCES approvals (id)'),
        ('approvals', 'approvals_consumed_by_fk',
         'FOREIGN KEY (consumed_by) REFERENCES workflow_runs (id)')
    ) AS t(tbl, name, def)
    LOOP
        IF NOT EXISTS (
            SELECT 1 FROM pg_constraint WHERE conname = c.name AND conrelid = c.tbl::regclass
        ) THEN
            EXECUTE format('ALTER TABLE %I ADD CONSTRAINT %I %s NOT VALID', c.tbl, c.name, c.def);
        END IF;
        BEGIN
            EXECUTE format('ALTER TABLE %I VALIDATE CONSTRAINT %I', c.tbl, c.name);
        EXCEPTION WHEN foreign_key_violation THEN
            RAISE WARNING 'constraint % left NOT VALID: existing rows violate it', c.name;
        END;
    END LOOP;
END;
$$;
CREATE INDEX IF NOT EXISTS model_calls_reservation_idx
    ON model_calls (reservation_id) WHERE reservation_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS workflow_runs_approval_idx
    ON workflow_runs (approval_id) WHERE approval_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS approvals_consumed_by_idx
    ON approvals (consumed_by) WHERE consumed_by IS NOT NULL;

-- Approvals: what the grant covers and what was decided. A row is only ever written when an actor
-- grants, so the default for `decision` is 'granted' (builds from before this migration keep working).
ALTER TABLE approvals ADD COLUMN IF NOT EXISTS scope text;
ALTER TABLE approvals ADD COLUMN IF NOT EXISTS decision text NOT NULL DEFAULT 'granted';
ALTER TABLE approvals ADD CONSTRAINT approvals_decision_check
    CHECK (decision IN ('granted', 'denied', 'revoked'));

-- Audit events: nullable, so existing writers need no change.
ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS policy_version text;
ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS outcome text;
ALTER TABLE audit_events ADD CONSTRAINT audit_events_outcome_check
    CHECK (outcome IS NULL OR outcome IN ('ok', 'denied', 'error'));

-- Rollback safety (review C): 051 added source_updated_at NOT NULL with no default, so a build
-- from before 051 cannot insert integration_sources rows. A default keeps that build working.
ALTER TABLE integration_sources ALTER COLUMN source_updated_at SET DEFAULT now();
