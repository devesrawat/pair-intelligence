//! Provider-reported usage limits for subscription-billed models.
//!
//! The provider, not the registry, says when a subscription is exhausted: Claude Code reports
//! per-window utilization and paid-overage use on its event stream, and Ollama Cloud answers 429
//! with `Retry-After`. An adapter records what the provider reported here, and refuses further
//! calls locally (a `ProviderUnavailable`, so the caller falls through to the next model) until the
//! reported reset time has passed. State is in memory; the first call after a restart learns it
//! again from the provider.
use pair_core::error::{ErrorCode, PairError, Result};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Block {
    /// Unix seconds at which the provider says calls are allowed again.
    pub(crate) until: u64,
    pub(crate) reason: String,
}

#[derive(Debug, Default)]
pub(crate) struct LimitGate {
    block: Mutex<Option<Block>>,
}

pub(crate) fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl LimitGate {
    /// Replace what is known: `Some` blocks until its reset, `None` clears.
    pub(crate) fn record(&self, block: Option<Block>) {
        if let Ok(mut state) = self.block.lock() {
            *state = block;
        }
    }

    pub(crate) fn check(&self, model: &str) -> Result<()> {
        self.check_at(now_epoch(), model)
    }

    fn check_at(&self, now: u64, model: &str) -> Result<()> {
        let state = self
            .block
            .lock()
            .map_err(|_| PairError::new(ErrorCode::Internal, "limit gate poisoned"))?;
        match state.as_ref() {
            Some(b) if b.until > now => Err(PairError::new(
                ErrorCode::ProviderUnavailable,
                format!(
                    "provider reports {} for {model}; retry in {}s",
                    b.reason,
                    b.until - now
                ),
            )),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(until: u64) -> Block {
        Block {
            until,
            reason: "five_hour window at 97%".to_owned(),
        }
    }

    #[test]
    fn gate_is_open_until_the_provider_reports_a_block() {
        let g = LimitGate::default();
        g.check_at(1_000, "m").expect("nothing reported yet");
    }

    #[test]
    fn gate_refuses_until_reset_then_reopens() {
        let g = LimitGate::default();
        g.record(Some(block(2_000)));
        let err = g.check_at(1_500, "m").expect_err("blocked");
        assert_eq!(err.code, ErrorCode::ProviderUnavailable);
        assert!(err.message.contains("97%") && err.message.contains("500s"));
        g.check_at(2_000, "m").expect("reset time reached");
    }

    #[test]
    fn gate_record_none_clears_a_block() {
        let g = LimitGate::default();
        g.record(Some(block(2_000)));
        g.record(None);
        g.check_at(1_500, "m").expect("cleared");
    }
}
