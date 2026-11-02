//! Rate-limited logging for conditions that can repeat at packet rate.
//!
//! A gateway can send hundreds of packets per second, so a warning raised once
//! per dropped packet (full queue, unreachable broker, refused session) would
//! bury the log and cost more CPU than the work being dropped.  A
//! [`RateLimitedLog`] lets through at most one line per interval and says how
//! many were held back, so the log still shows both that the problem exists
//! and how big it is.
//!
//! One instance limits one kind of message: give each distinct condition its
//! own, otherwise a noisy one would hide a rare one.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use log::Level;

use crate::core::linelog;

/// Interval used by [`RateLimitedLog::new`].
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// Lets through at most one log line per interval, counting the rest.
///
/// All methods take `&self`, so an instance can be shared through an `Arc`
/// (or a `static`) between tokio tasks and plain threads.
#[derive(Debug)]
pub struct RateLimitedLog {
    interval: Duration,
    /// When the last line was let through; `None` until the first one.
    last: Mutex<Option<Instant>>,
    /// Messages held back since the last line, reported with the next one.
    pending: AtomicU64,
    /// Lines let through over the lifetime of the instance.
    logged: AtomicU64,
    /// Messages held back over the lifetime of the instance.
    suppressed: AtomicU64,
}

impl RateLimitedLog {
    /// A limiter that lets one line through every [`DEFAULT_INTERVAL`].
    pub const fn new() -> Self {
        Self::with_interval(DEFAULT_INTERVAL)
    }

    /// A limiter that lets one line through every `interval`.
    pub const fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
            pending: AtomicU64::new(0),
            logged: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }

    /// Log `msg` at warn level unless a line was already let through within
    /// the interval.
    ///
    /// Pass `format_args!(..)` to build the message only when it is actually
    /// logged; a pre-built `String` costs an allocation even when suppressed.
    pub fn warn(&self, msg: impl fmt::Display) {
        self.log(Level::Warn, msg);
    }

    /// Log `msg` at error level unless a line was already let through within
    /// the interval.  See [`RateLimitedLog::warn`].
    pub fn error(&self, msg: impl fmt::Display) {
        self.log(Level::Error, msg);
    }

    /// Number of lines let through so far.
    // Read by tests, which assert rate limiting without capturing log output.
    #[allow(dead_code)]
    pub fn logged(&self) -> u64 {
        self.logged.load(Ordering::Relaxed)
    }

    /// Number of messages held back so far.
    // Read by tests, which assert rate limiting without capturing log output.
    #[allow(dead_code)]
    pub fn suppressed(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }

    fn log(&self, level: Level, msg: impl fmt::Display) {
        let target = module_path!();
        match self.permit(Instant::now()) {
            None => {}
            Some(0) => linelog::line(level, target, format_args!("{}", msg)),
            Some(n) => linelog::line(
                level,
                target,
                format_args!("{} ({} suppressed since the last message)", msg, n),
            ),
        }
    }

    /// Decide whether a message arriving at `now` may be logged.
    ///
    /// Returns `Some(held_back)` when it may, where `held_back` counts the
    /// messages suppressed since the previous line, and `None` when it must be
    /// dropped.  Taking the time as a parameter lets tests cover the interval
    /// logic without sleeping.
    fn permit(&self, now: Instant) -> Option<u64> {
        // The critical section cannot panic, but a poisoned lock is harmless
        // here anyway: the timestamp inside is always valid.
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let due = match *last {
            None => true,
            Some(prev) => now.saturating_duration_since(prev) >= self.interval,
        };
        if due {
            *last = Some(now);
            self.logged.fetch_add(1, Ordering::Relaxed);
            Some(self.pending.swap(0, Ordering::Relaxed))
        } else {
            self.pending.fetch_add(1, Ordering::Relaxed);
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

impl Default for RateLimitedLog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn first_message_passes_then_interval_gates() {
        let limiter = RateLimitedLog::with_interval(Duration::from_secs(10));
        let t0 = Instant::now();

        assert_eq!(limiter.permit(t0), Some(0));
        assert_eq!(limiter.permit(t0 + Duration::from_secs(1)), None);
        assert_eq!(limiter.permit(t0 + Duration::from_secs(9)), None);
        assert_eq!((limiter.logged(), limiter.suppressed()), (1, 2));

        // The next line reports how many were held back, then the count restarts.
        assert_eq!(limiter.permit(t0 + Duration::from_secs(10)), Some(2));
        assert_eq!(limiter.permit(t0 + Duration::from_secs(11)), None);
        assert_eq!(limiter.permit(t0 + Duration::from_secs(20)), Some(1));
        assert_eq!(limiter.permit(t0 + Duration::from_secs(30)), Some(0));
        assert_eq!((limiter.logged(), limiter.suppressed()), (4, 3));
    }

    #[test]
    fn interval_is_measured_from_the_last_line_that_passed() {
        let limiter = RateLimitedLog::with_interval(Duration::from_secs(10));
        let t0 = Instant::now();

        assert_eq!(limiter.permit(t0), Some(0));
        // Suppressed messages must not push the next line further away.
        assert_eq!(limiter.permit(t0 + Duration::from_secs(9)), None);
        assert_eq!(limiter.permit(t0 + Duration::from_secs(10)), Some(1));
    }

    #[test]
    fn default_interval_is_ten_seconds() {
        assert_eq!(RateLimitedLog::new().interval, Duration::from_secs(10));
        assert_eq!(RateLimitedLog::default().interval, Duration::from_secs(10));
    }

    #[test]
    fn warn_and_error_share_one_budget() {
        let limiter = RateLimitedLog::with_interval(Duration::from_secs(3600));
        limiter.warn("first");
        limiter.error("second");
        limiter.warn(format_args!("third {}", 3));
        assert_eq!((limiter.logged(), limiter.suppressed()), (1, 2));
    }

    #[test]
    fn lets_another_line_through_after_a_real_interval() {
        let limiter = RateLimitedLog::with_interval(Duration::from_millis(20));
        limiter.warn("first");
        std::thread::sleep(Duration::from_millis(40));
        limiter.warn("second");
        assert_eq!(limiter.logged(), 2);
    }

    #[test]
    fn counts_stay_exact_across_threads() {
        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 1_000;

        // An hour-long interval makes the outcome independent of scheduling:
        // exactly one call passes and every other one is held back.
        let limiter = Arc::new(RateLimitedLog::with_interval(Duration::from_secs(3600)));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let limiter = Arc::clone(&limiter);
                std::thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        limiter.warn("contended");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(limiter.logged(), 1);
        assert_eq!(limiter.suppressed(), THREADS * PER_THREAD - 1);
    }

    #[test]
    fn usable_from_a_static() {
        static LIMITER: RateLimitedLog = RateLimitedLog::new();
        LIMITER.warn("from a static");
        assert_eq!(LIMITER.logged(), 1);
    }
}
