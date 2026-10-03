-- Correlation ids on approvals and effect intents (spec section 12). Nullable with no default so
-- the change is metadata-only and rows written by earlier builds stay valid; they simply have no
-- trace. Partial indexes keep the index small until backfilled.

ALTER TABLE approvals ADD COLUMN trace_id uuid;
ALTER TABLE effect_intents ADD COLUMN trace_id uuid;

CREATE INDEX approvals_trace_idx ON approvals (trace_id) WHERE trace_id IS NOT NULL;
CREATE INDEX effect_intents_trace_idx ON effect_intents (trace_id) WHERE trace_id IS NOT NULL;
