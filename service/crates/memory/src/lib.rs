//! pair-memory: PostgreSQL-backed memory store, inbox and retrieval (spec section 7).
#![cfg_attr(test, allow(clippy::unwrap_used))]
pub mod accept;
pub mod audit;
pub(crate) mod contradiction;
pub mod embedding;
pub mod error;
pub mod export;
pub mod extraction;
pub mod inbox;
pub mod model;
pub mod normalize;
pub mod policy;
pub mod read;
pub mod retrieval;
pub mod review;
pub mod sources;
pub mod spans;
pub mod store;

pub use model::{
    CandidateDraft, EvidenceRecord, MemoryRecord, MemoryStatus, NewSource, SourceRecord, Visibility,
};
pub use store::PgMemory;
