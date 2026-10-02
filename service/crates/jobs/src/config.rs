use std::time::Duration;

pub const INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub const RESEARCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const MAX_TOOL_CALLS: u32 = 20;
pub const APPROVAL_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(60);
pub const DEFAULT_MAX_ATTEMPTS: u32 = 4;
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(200);
pub const DEFAULT_BACKOFF_CAP: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunClass {
    Interactive,
    Research,
}

impl RunClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Research => "research",
        }
    }
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "interactive" => Some(Self::Interactive),
            "research" => Some(Self::Research),
            _ => None,
        }
    }
}

/// Capped exponential backoff for transient step errors: delay = min(cap, base * 2^(attempt-1)),
/// at most `max_attempts` executions of a step per worker pass.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl RetryPolicy {
    pub fn delay(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap)
    }
}

#[derive(Debug, Clone)]
pub struct JobConfig {
    pub lease_ttl: Duration,
    pub interactive_timeout: Duration,
    pub research_timeout: Duration,
    pub max_tool_calls: u32,
    pub retry: RetryPolicy,
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            lease_ttl: DEFAULT_LEASE_TTL,
            interactive_timeout: INTERACTIVE_TIMEOUT,
            research_timeout: RESEARCH_TIMEOUT,
            max_tool_calls: MAX_TOOL_CALLS,
            retry: RetryPolicy {
                max_attempts: DEFAULT_MAX_ATTEMPTS,
                base: DEFAULT_BACKOFF_BASE,
                cap: DEFAULT_BACKOFF_CAP,
            },
        }
    }
}

impl JobConfig {
    pub(crate) fn timeout(&self, class: RunClass) -> Duration {
        match class {
            RunClass::Interactive => self.interactive_timeout,
            RunClass::Research => self.research_timeout,
        }
    }
}
