//! pair-jobs: PostgreSQL-backed durable runs, leases, checkpoints, side-effect intents and
//! hash-bound approvals (spec sections 9 and 10).
//!
//! Retry policy: the only retry logic is a capped exponential backoff for steps that return
//! `StepError::Transient` (see `RetryPolicy`). Everything else is durable state, not retries.
pub mod approvals;
pub mod config;
pub mod effects;
pub mod error;
pub mod hash;
pub mod records;
pub mod step;
pub mod store;
pub mod worker;

mod state;

pub use approvals::PgApprovals;
pub use config::{JobConfig, RetryPolicy, RunClass};
pub use effects::{EffectError, Intent, Reconciliation};
pub use error::StepError;
pub use hash::{action_hash, sha256_hex};
pub use records::{RunRecord, StepRecord};
pub use step::{StepCtx, StepHandler, StepOutcome};
pub use store::JobStore;
pub use worker::Worker;
