//! pair-budget: transactional budget ledger (spec section 6).
//!
//! Money is integer micro-USD. Reservations serialize on a Postgres advisory lock, so
//! concurrent reserves cannot jointly exceed task/day/month/classifier caps.
pub mod config;
mod ledger;
pub mod period;
mod pricebook;
mod reconcile;
mod reserve;

pub use config::BudgetConfig;
pub use ledger::PgBudget;
pub use pair_core::types::{BudgetCategory, ReserveRequest, TaskKind};
pub use pricebook::PriceBook;
pub use reconcile::Reconciled;
pub use reserve::{ReservationBinding, MAX_COST_MICROS, RESERVE_LOCK_KEY};

/// Migrations shared by the whole workspace (`migrations/` at the repo root).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../../migrations");
