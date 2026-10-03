-- Indexes for deletion (source -> chunks/candidate evidence) and for contradiction lookups
-- over pending candidates.
CREATE INDEX IF NOT EXISTS memory_chunks_source_idx
    ON memory_chunks (source_id) WHERE source_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS memory_candidate_evidence_source_idx
    ON memory_candidate_evidence (source_id);
CREATE INDEX IF NOT EXISTS memory_candidates_pending_topic_idx
    ON memory_candidates (kind, project, topic_key) WHERE state = 'pending';
