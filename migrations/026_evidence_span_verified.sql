-- Whether an evidence span was checked against the source text supplied when the candidate was
-- proposed. Sources carry no stored text, so evidence proposed without text stays unverified.
ALTER TABLE memory_candidate_evidence ADD COLUMN span_verified boolean NOT NULL DEFAULT false;
ALTER TABLE memory_evidence ADD COLUMN span_verified boolean NOT NULL DEFAULT false;
