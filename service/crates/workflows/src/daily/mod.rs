//! workflows::daily — goals, commitments, open loops, manual and scheduled briefings/reviews.
//!
//! Invariants: stated intent never completes a loop; only an `observed_completion`
//! event does; overdue commitments stay open; briefs rank commitments above inferences.
pub mod brief;
pub mod model;
pub mod review;
pub mod schedule;
pub mod store;

#[cfg(test)]
pub(crate) mod testdb;
#[cfg(test)]
mod tests;

pub use brief::{build_brief, Brief, Priority};
pub use model::{EventKind, LoopKind, LoopStatus, NewEvent, NewLoop, OpenLoop};
pub use review::{build_review, Review};
pub use schedule::{next_run, run_scheduled, Routine, RoutineKind};

use chrono::{DateTime, Duration, TimeZone, Utc};
use chrono_tz::Asia::Kolkata;

/// Number of priorities a briefing surfaces.
pub const MAX_PRIORITIES: usize = 3;
/// Upper bound on delegation suggestions in a briefing.
pub const MAX_DELEGATIONS: usize = 3;

/// UTC bounds `[start, end)` of the Asia/Kolkata calendar day containing `now`.
pub fn kolkata_day_bounds(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let local_date = now.with_timezone(&Kolkata).date_naive();
    let start = local_midnight_utc(local_date);
    (start, start + Duration::days(1))
}

fn local_midnight_utc(date: chrono::NaiveDate) -> DateTime<Utc> {
    // Kolkata has no DST; midnight is unambiguous. Fall back to UTC midnight defensively.
    let midnight = date.and_time(chrono::NaiveTime::MIN);
    Kolkata
        .from_local_datetime(&midnight)
        .earliest()
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&midnight))
}
