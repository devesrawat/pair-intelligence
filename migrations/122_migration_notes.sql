-- Correction of the lock claims in the header of 101 (which is immutable once applied).
-- See docs/migrations-notes.md. sqlx wraps every migration file in one transaction, so 101 held
-- its strongest lock for the whole file; both CHECKs below were validated in line, so there is
-- nothing to re-validate. Comment-only: each statement takes a brief SHARE UPDATE EXCLUSIVE on its
-- table (does not block reads or writes) and rewrites no rows.
COMMENT ON CONSTRAINT approvals_decision_check ON approvals IS
    'Added and validated in migration 101 (plain ADD CONSTRAINT: ACCESS EXCLUSIVE for the scan, held to the end of the file). Nothing to revalidate. See docs/migrations-notes.md.';
COMMENT ON CONSTRAINT audit_events_outcome_check ON audit_events IS
    'Added and validated in migration 101 (plain ADD CONSTRAINT: ACCESS EXCLUSIVE for the scan, held to the end of the file). Nothing to revalidate. See docs/migrations-notes.md.';
