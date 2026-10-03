-- Lifecycle consistency CHECKs for the memory tables.
--
-- Each constraint is added NOT VALID (enforced for every new INSERT/UPDATE immediately, without
-- scanning existing rows) and then VALIDATEd. Validation is attempted inside a sub-block: on an
-- empty or fixture database it succeeds; on a database holding legacy rows that violate a rule
-- the migration does NOT fail and does NOT rewrite those rows (memory_audit_events in particular
-- is an audit log and must not be edited). The constraint then stays NOT VALID with a WARNING;
-- repair the rows and run `ALTER TABLE .. VALIDATE CONSTRAINT ..` by hand. Re-running is safe:
-- existing constraints are skipped.
DO $$
DECLARE
    c record;
BEGIN
    FOR c IN SELECT * FROM (VALUES
        ('memories', 'memories_closed_has_valid_to',
         'CHECK (status NOT IN (''superseded'', ''expired'') OR valid_to IS NOT NULL)'),
        ('memories', 'memories_invalidated_is_expired',
         'CHECK (invalidated_reason IS NULL OR status = ''expired'')'),
        ('memory_candidates', 'memory_candidates_edited_has_replacement',
         'CHECK (state <> ''edited'' OR replaced_by IS NOT NULL)'),
        ('memory_audit_events', 'memory_audit_events_outcome_check',
         'CHECK (outcome IN (''ok'', ''denied'', ''error''))')
    ) AS t(tbl, name, def)
    LOOP
        IF NOT EXISTS (
            SELECT 1 FROM pg_constraint WHERE conname = c.name AND conrelid = c.tbl::regclass
        ) THEN
            EXECUTE format('ALTER TABLE %I ADD CONSTRAINT %I %s NOT VALID', c.tbl, c.name, c.def);
        END IF;
        BEGIN
            EXECUTE format('ALTER TABLE %I VALIDATE CONSTRAINT %I', c.tbl, c.name);
        EXCEPTION WHEN check_violation THEN
            RAISE WARNING 'constraint % left NOT VALID: existing rows violate it', c.name;
        END;
    END LOOP;
END;
$$;
