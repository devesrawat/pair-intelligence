//! pair-memory: PostgreSQL-backed memory store, inbox and retrieval (spec section 7).
pub mod audit;
pub mod embedding;
pub mod error;
pub mod export;
pub mod inbox;
pub mod model;
pub mod normalize;
pub mod read;
pub mod retrieval;
pub mod sources;
pub mod store;

pub use model::{
    CandidateDraft, EvidenceRecord, MemoryRecord, MemoryStatus, NewSource, SourceRecord, Visibility,
};
pub use store::PgMemory;
