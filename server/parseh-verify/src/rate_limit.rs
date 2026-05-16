//! Per-node verification rate limiter.
//!
//! Implements the **10% / hour rolling window** cap from
//! the project notes §2.1. A node may not verify more
//! than `cap_fraction` of the tasks it has observed in any
//! `window_secs` window.
//!
//! ## Design notes
//!
//! - **No clock dependency in the type.** Tests pass `SystemTime`
//!   values explicitly via [`RateLimit::record_observed_task_at`] and
//!   [`RateLimit::exceeded_at`]. Production callers use the
//!   `_at_now` shortcuts which read `SystemTime::now()` once.
//! - **Simple sliding-tail eviction.** We keep two [`std::collections::VecDeque`]s
//!   of timestamps — one for observed tasks, one for own
//!   verifications — and evict any entry older than `window_secs`
//!   on every read or write. Memory is bounded by traffic, not time.
//!   For the V0.2 hot path (≤10 tasks/sec on a busy node) this is
//!   ample; if profiling shows otherwise we can switch to a ring
//!   buffer in a follow-up.
//! - **Cap fraction interpretation.** `own / max(observed, 1) >
//!   cap_fraction` mirrors the spec almost literally; the `max(_, 1)`
//!   prevents a divide-by-zero on a fresh node that has yet to observe
//!   anything.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::params;

/// Sliding-window rate limiter tracking observed-vs-own-verification
/// ratio over a fixed window.
///
/// Cheap to construct, cheap to mutate, cheap to query.
#[derive(Debug, Clone)]
pub struct RateLimit {
    window: Duration,
    cap_fraction: f64,
    observed: VecDeque<SystemTime>,
    own: VecDeque<SystemTime>,
}

impl RateLimit {
    /// Construct a [`RateLimit`] with the V0.2 defaults
    /// (`window = 1h`, `cap_fraction = 10%`).
    pub fn v0_2_defaults() -> Self {
        Self::with(
            Duration::from_secs(params::RATE_WINDOW_SECS),
            params::RATE_CAP_PER_HOUR,
        )
    }

    /// Construct a [`RateLimit`] with explicit parameters. Useful for
    /// tests that compress the time scale.
    pub fn with(window: Duration, cap_fraction: f64) -> Self {
        Self {
            window,
            cap_fraction,
            observed: VecDeque::new(),
            own: VecDeque::new(),
        }
    }

    /// Record that the local node observed a fresh task on the network.
    pub fn record_observed_task_at(&mut self, at: SystemTime) {
        self.observed.push_back(at);
        self.evict_before(at);
    }

    /// Convenience for production callers — uses `SystemTime::now()`.
    pub fn record_observed_task(&mut self) {
        self.record_observed_task_at(SystemTime::now());
    }

    /// Record that the local node decided to verify a task.
    pub fn record_own_verification_at(&mut self, at: SystemTime) {
        self.own.push_back(at);
        self.evict_before(at);
    }

    /// Convenience for production callers — uses `SystemTime::now()`.
    pub fn record_own_verification(&mut self) {
        self.record_own_verification_at(SystemTime::now());
    }

    /// Whether the per-window cap is currently exceeded, as observed at
    /// `at`.
    pub fn exceeded_at(&self, at: SystemTime) -> bool {
        let (observed_in_window, own_in_window) = self.counts_at(at);
        let observed = observed_in_window.max(1) as f64;
        let own = own_in_window as f64;
        own / observed > self.cap_fraction
    }

    /// Convenience for production callers — uses `SystemTime::now()`.
    pub fn exceeded_at_now(&self) -> bool {
        self.exceeded_at(SystemTime::now())
    }

    /// Inspect raw `(observed, own)` counts within the trailing
    /// window as of `at`. Exposed for diagnostics / tests.
    pub fn counts_at(&self, at: SystemTime) -> (u32, u32) {
        let cutoff = at.checked_sub(self.window).unwrap_or(UNIX_EPOCH);
        let observed = self.observed.iter().filter(|t| **t >= cutoff).count() as u32;
        let own = self.own.iter().filter(|t| **t >= cutoff).count() as u32;
        (observed, own)
    }

    /// Drop entries older than `now - window` from both deques. We
    /// always evict from the front, since pushes are append-only and
    /// timestamps are monotone-non-decreasing under normal operation.
    /// If a caller injects an out-of-order timestamp (test scenario),
    /// we tolerate it: the filter in [`Self::counts_at`] is the
    /// authoritative read.
    fn evict_before(&mut self, now: SystemTime) {
        let cutoff = now.checked_sub(self.window).unwrap_or(UNIX_EPOCH);
        while let Some(front) = self.observed.front() {
            if *front < cutoff {
                self.observed.pop_front();
            } else {
                break;
            }
        }
        while let Some(front) = self.own.front() {
            if *front < cutoff {
                self.own.pop_front();
            } else {
                break;
            }
        }
    }
}

impl Default for RateLimit {
    fn default() -> Self {
        Self::v0_2_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn fresh_limiter_not_exceeded() {
        let r = RateLimit::v0_2_defaults();
        assert!(!r.exceeded_at(t(1_000)));
    }

    #[test]
    fn under_cap_not_exceeded() {
        // 100 observed, 5 own = 5% — under the 10% cap.
        let mut r = RateLimit::v0_2_defaults();
        for i in 0..100 {
            r.record_observed_task_at(t(1_000 + i));
        }
        for i in 0..5 {
            r.record_own_verification_at(t(1_100 + i));
        }
        assert!(!r.exceeded_at(t(1_200)));
    }

    #[test]
    fn over_cap_exceeded() {
        // 100 observed, 11 own = 11% — over the 10% cap.
        let mut r = RateLimit::v0_2_defaults();
        for i in 0..100 {
            r.record_observed_task_at(t(1_000 + i));
        }
        for i in 0..11 {
            r.record_own_verification_at(t(1_100 + i));
        }
        assert!(r.exceeded_at(t(1_200)));
    }

    #[test]
    fn entries_outside_window_do_not_count() {
        let mut r = RateLimit::with(Duration::from_secs(60), 0.10);
        // Old: 100 own at t=0 — well outside the 60-second window.
        for _ in 0..100 {
            r.record_own_verification_at(t(0));
        }
        // Recent: only 1 own at t=10_000 along with 100 observed.
        for i in 0..100 {
            r.record_observed_task_at(t(10_000 + i));
        }
        r.record_own_verification_at(t(10_050));
        assert!(!r.exceeded_at(t(10_059)));
    }

    #[test]
    fn zero_observed_does_not_panic() {
        let r = RateLimit::v0_2_defaults();
        // No observed, no own → not exceeded (cap is on a ratio).
        assert!(!r.exceeded_at(t(1_000)));
    }

    #[test]
    fn counts_at_reports_within_window_only() {
        let mut r = RateLimit::with(Duration::from_secs(100), 0.10);
        r.record_observed_task_at(t(0));
        r.record_observed_task_at(t(50));
        r.record_observed_task_at(t(150));
        let (obs, own) = r.counts_at(t(160));
        // t=0 is at distance 160 → outside 100s window.
        // t=50 is at distance 110 → outside.
        // t=150 is at distance 10 → inside.
        assert_eq!(obs, 1);
        assert_eq!(own, 0);
    }
}
