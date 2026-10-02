-- Full-text retrieval over derived chunks (memory text + evidence spans).
CREATE INDEX memory_chunks_search_idx ON memory_chunks USING GIN (search_vector);
CREATE INDEX memory_conflicts_b_idx ON memory_conflicts (memory_b);
