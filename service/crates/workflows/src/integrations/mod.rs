//! workflows::integrations — optional Gmail/Calendar ingestion behind an injected
//! `SourceClient`. Read-only, allowlist-gated, cursor-incremental, revision-tracked.
//! No real client lives in this crate; production clients are owner-gated (OAuth).
pub mod client;
pub mod disconnect;
pub mod ingest;
pub mod store;
pub mod writes;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

pub use client::{
    ChangeBatch, ClientError, Provider, RemoteItem, SourceChange, SourceClient, SourceKind,
};
pub use disconnect::{DeletionChoice, DisconnectReport, ExportBundle};
pub use ingest::{ingest_account, IngestStats};
pub use store::{AccountState, IntegrationAccount};
pub use writes::{attempt_write, ExternalWrite};

/// Hard cap on pages fetched per scope in one ingestion run.
pub const MAX_PAGES_PER_SCOPE: usize = 50;
