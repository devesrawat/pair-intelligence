-- Monotonic ordering for integration sources: the provider's own update time, compared per
-- source so a replayed older page can never overwrite newer content.
ALTER TABLE integration_sources ADD COLUMN source_updated_at timestamptz;
UPDATE integration_sources SET source_updated_at = updated_at;
ALTER TABLE integration_sources ALTER COLUMN source_updated_at SET NOT NULL;
