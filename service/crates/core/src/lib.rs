//! Shared contracts for PAIR (schema version 1). Every other crate depends only on this.
pub mod error;
pub mod ids;
pub mod money;
pub mod traits;
pub mod types;

/// Version of the serialized contracts in `config/contracts.json`.
pub const CONTRACT_SCHEMA_VERSION: u32 = 1;
