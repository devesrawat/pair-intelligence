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

    /// Explicit deadline; lets tests pick a clock without sleeping for minutes.
    pub fn with_deadline(max_tool_calls: usize, deadline: Instant) -> Self {
        Self {
            tool_calls: AtomicUsize::new(0),
            max_tool_calls,
            deadline,
        }
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
