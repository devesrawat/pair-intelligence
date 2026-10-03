-- Retention support (spec section 11). Purging ERASES content in place and keeps ids and audit rows.

-- Pinned sources are exempt from the 90-day raw-source retention.
ALTER TABLE sources             ADD COLUMN IF NOT EXISTS pinned boolean NOT NULL DEFAULT false;
ALTER TABLE research_sources    ADD COLUMN IF NOT EXISTS pinned boolean NOT NULL DEFAULT false;
ALTER TABLE integration_sources ADD COLUMN IF NOT EXISTS pinned boolean NOT NULL DEFAULT false;

-- Raw material a source row once pointed at is erased by purge; the row itself and its hash stay.
ALTER TABLE sources ADD COLUMN IF NOT EXISTS purged_at timestamptz;

-- Detailed model payloads: nullable text, written by callers that choose to keep them (they are
-- erased after 30 days; cost, usage and routing columns are permanent).
ALTER TABLE model_calls ADD COLUMN IF NOT EXISTS request_payload   text;
ALTER TABLE model_calls ADD COLUMN IF NOT EXISTS response_payload  text;
ALTER TABLE model_calls ADD COLUMN IF NOT EXISTS payload_purged_at timestamptz;
