//! Liveness of the background tasks, as reported by `/readyz`.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Default stall threshold: this many of a task's intervals without a successful tick.
pub const STALL_INTERVALS: u32 = 3;

#[derive(Debug, Clone)]
struct TaskState {
    stall_after: Duration,
    registered_at: Instant,
    last_ok: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskStatus {
    pub name: String,
    pub stalled: bool,
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct Liveness {
    tasks: Mutex<BTreeMap<&'static str, TaskState>>,
}

impl Liveness {
    fn with<R>(&self, f: impl FnOnce(&mut BTreeMap<&'static str, TaskState>) -> R) -> R {
        let mut guard = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }

    pub fn register(&self, name: &'static str, stall_after: Duration) {
        self.register_at(name, stall_after, Instant::now());
    }

    pub fn register_at(&self, name: &'static str, stall_after: Duration, at: Instant) {
        self.with(|t| {
            t.insert(
                name,
                TaskState {
                    stall_after,
                    registered_at: at,
                    last_ok: None,
                    last_error: None,
                },
            );
        });
    }

    pub fn tick_ok(&self, name: &'static str) {
        self.with(|t| {
            if let Some(s) = t.get_mut(name) {
                s.last_ok = Some(Instant::now());
                s.last_error = None;
            }
        });
    }

    pub fn tick_err(&self, name: &'static str, message: String) {
        self.with(|t| {
            if let Some(s) = t.get_mut(name) {
                s.last_error = Some(message);
            }
        });
    }

    /// Statuses as of `now` (injectable so tests need no sleeping).
    pub fn statuses_at(&self, now: Instant) -> Vec<TaskStatus> {
        self.with(|t| {
            t.iter()
                .map(|(name, s)| {
                    let since = s.last_ok.unwrap_or(s.registered_at);
                    let age = now.saturating_duration_since(since);
                    let stalled = age > s.stall_after;
                    let detail = match (&s.last_error, stalled) {
                        (Some(e), true) => format!("stalled; last error: {e}"),
                        (None, true) => {
                            format!("stalled: no successful tick for {}s", age.as_secs())
                        }
                        (Some(e), false) => format!("running; last tick failed: {e}"),
                        (None, false) => "running".to_owned(),
                    };
                    TaskStatus {
                        name: (*name).to_owned(),
                        stalled,
                        detail,
                    }
                })
                .collect()
        })
    }

    pub fn statuses(&self) -> Vec<TaskStatus> {
        self.statuses_at(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_secs(10);

    #[test]
    fn test_statuses_task_without_tick_is_live_inside_grace_and_stalled_after() {
        let live = Liveness::default();
        let start = Instant::now();
        live.register_at("sweeper", INTERVAL * STALL_INTERVALS, start);
        let inside = live.statuses_at(start + INTERVAL * 2);
        assert!(!inside[0].stalled, "{inside:?}");
        let after = live.statuses_at(start + INTERVAL * (STALL_INTERVALS + 1));
        assert!(after[0].stalled, "{after:?}");
    }

    #[test]
    fn test_statuses_successful_tick_resets_the_stall_clock() {
        let live = Liveness::default();
        let start = Instant::now();
        live.register_at("sweeper", INTERVAL * STALL_INTERVALS, start);
        live.tick_ok("sweeper");
        let later = Instant::now() + INTERVAL * 2;
        assert!(!live.statuses_at(later)[0].stalled);
    }

    #[test]
    fn test_statuses_error_ticks_do_not_count_as_liveness() {
        let live = Liveness::default();
        let start = Instant::now();
        live.register_at("sweeper", INTERVAL * STALL_INTERVALS, start);
        live.tick_err("sweeper", "db down".into());
        let st = live.statuses_at(start + INTERVAL * (STALL_INTERVALS + 1));
        assert!(st[0].stalled);
        assert!(st[0].detail.contains("db down"), "{}", st[0].detail);
    }
}
