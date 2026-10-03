//! Sliding-window request quota for subscription-billed models.
//!
//! A subscription call costs zero marginal dollars, so the dollar budget cannot bound it. This
//! limiter is the bound: `limit` calls per `window`, in memory, per adapter instance.
use pair_core::error::{ErrorCode, PairError, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Window the registry's `quota_requests_per_minute` applies to.
pub(crate) const QUOTA_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) struct QuotaLimiter {
    window: Duration,
    calls: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl QuotaLimiter {
    pub(crate) fn per_minute() -> Self {
        Self::with_window(QUOTA_WINDOW)
    }

    fn with_window(window: Duration) -> Self {
        Self {
            window,
            calls: Mutex::new(HashMap::new()),
        }
    }

    /// Record a call or refuse it. `ProviderUnavailable`, so a caller may fall back to another
    /// provider, never to a costlier tier of the same quota.
    pub(crate) fn admit(&self, model: &str, limit: u32) -> Result<()> {
        self.admit_at(Instant::now(), model, limit)
    }

    fn admit_at(&self, now: Instant, model: &str, limit: u32) -> Result<()> {
        let mut calls = self.calls.lock().map_err(|_| {
            PairError::new(ErrorCode::Internal, "subscription quota limiter poisoned")
        })?;
        let calls = calls.entry(model.to_owned()).or_default();
        while calls
            .front()
            .is_some_and(|t| now.duration_since(*t) >= self.window)
        {
            calls.pop_front();
        }
        if calls.len() >= usize::try_from(limit).unwrap_or(usize::MAX) {
            return Err(PairError::new(
                ErrorCode::ProviderUnavailable,
                format!("subscription quota of {limit} requests per window reached for {model}"),
            ));
        }
        calls.push_back(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_admits_up_to_limit_then_refuses() {
        let q = QuotaLimiter::per_minute();
        q.admit("m", 2).expect("first");
        q.admit("m", 2).expect("second");
        let err = q.admit("m", 2).expect_err("third refused");
        assert_eq!(err.code, ErrorCode::ProviderUnavailable);
    }

    #[test]
    fn quota_is_tracked_per_model() {
        let q = QuotaLimiter::per_minute();
        q.admit("haiku", 1).expect("haiku");
        q.admit("opus", 1).expect("opus has its own window");
        assert!(q.admit("haiku", 1).is_err());
    }

    #[test]
    fn quota_window_expiry_readmits() {
        let q = QuotaLimiter::with_window(Duration::from_secs(60));
        let t0 = Instant::now();
        q.admit_at(t0, "m", 1).expect("first");
        assert!(q.admit_at(t0 + Duration::from_secs(59), "m", 1).is_err());
        q.admit_at(t0 + Duration::from_secs(61), "m", 1)
            .expect("window rolled");
    }
}
