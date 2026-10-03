//! pair-ops: data-lifecycle operations for the PAIR database (spec section 11).
//!
//! * [`retention::purge`] erases aged content in place (ids and audit rows stay).
//! * [`export::export_all`] writes a deterministic, verifiable export of the owner's data.
//! * [`audit::stuck_executions`] reports audit rows that never reached a terminal outcome.
//! * [`cli`] parses the `pair-ops` command line.
//!
//! This crate depends on the schema only (SQL text), never on the api, workflows or models crates.
pub mod audit;
pub mod cli;
pub mod error;
pub mod export;
pub mod retention;
mod schema;

pub use error::{OpsError, Result};
