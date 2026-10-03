//! Per-run limits enforced by the workflows themselves (spec sections 6 and 10): at most
//! 20 tool calls and a wall-clock budget of 15 minutes (interactive) or 30 (background).
//! One `RunLimits` is created per task and shared by everything that task does.
use pair_core::error::{ErrorCode, PairError, Result};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

pub const MAX_TOOL_CALLS: usize = 20;
/// Most sources one research run may capture (config constant). Together with the query
/// cap it keeps a full run inside [`MAX_TOOL_CALLS`] (searches + fetches).
pub const MAX_RESEARCH_SOURCES: usize = 12;
pub const INTERACTIVE_WALL: Duration = Duration::from_secs(15 * 60);
pub const BACKGROUND_WALL: Duration = Duration::from_secs(30 * 60);
/// Slack between creating limits and starting the run.
const WALL_TOLERANCE: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct RunLimits {
    tool_calls: AtomicUsize,
    max_tool_calls: usize,
    deadline: Instant,
}

impl RunLimits {
    pub fn interactive() -> Self {
        Self::with_deadline(MAX_TOOL_CALLS, Instant::now() + INTERACTIVE_WALL)
    }

    pub fn background() -> Self {
        Self::with_deadline(MAX_TOOL_CALLS, Instant::now() + BACKGROUND_WALL)
    }

    /// Explicit limits; lets tests pick a clock without sleeping for minutes. Limits can only
    /// be tightened: the tool-call count is clamped to [`MAX_TOOL_CALLS`] and the deadline to
    /// [`BACKGROUND_WALL`] from now, so a caller cannot make the caps opt-in.
    pub fn with_deadline(max_tool_calls: usize, deadline: Instant) -> Self {
        Self {
            tool_calls: AtomicUsize::new(0),
            max_tool_calls: max_tool_calls.min(MAX_TOOL_CALLS),
            deadline: deadline.min(Instant::now() + BACKGROUND_WALL),
        }
    }

    /// Refuses limits looser than `wall` (e.g. a coding task must not run on background
    /// limits), so the workflow, not its caller, decides which cap applies.
    pub fn require_within(&self, wall: Duration) -> Result<()> {
        if self.remaining()? > wall + WALL_TOLERANCE {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!(
                    "run limits allow more than the {}s this workflow may take",
                    wall.as_secs()
                ),
            ));
        }
        Ok(())
    }

    pub fn tool_calls_used(&self) -> usize {
        self.tool_calls.load(Ordering::SeqCst)
    }

    /// Time left, or `LimitExceeded` once the deadline passed.
    pub fn remaining(&self) -> Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(PairError::new(
                ErrorCode::LimitExceeded,
                "run wall-clock limit reached",
            ));
        }
        Ok(left)
    }

    /// Counts one tool call (a sandboxed command, a write batch, a search or a fetch).
    pub fn begin_tool_call(&self) -> Result<()> {
        self.remaining()?;
        self.tool_calls
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < self.max_tool_calls).then_some(n + 1)
            })
            .map(|_| ())
            .map_err(|_| {
                PairError::new(
                    ErrorCode::LimitExceeded,
                    format!("tool-call limit of {} reached", self.max_tool_calls),
                )
            })
    }
}
