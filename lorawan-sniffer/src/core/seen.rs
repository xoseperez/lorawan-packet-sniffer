//! A bounded, best-effort record of the device identifiers seen live.
//!
//! The storage writer sheds records when it falls behind, so a device that the
//! WebUI shows on screen may have no row in the database.  To still let an
//! operator label it, the sniffer remembers every identifier (DevAddr or
//! DevEUI, in the text form used by message records) it has observed, with the
//! time it was last seen.
//!
//! The list is deliberately lossy and small:
//!
//! - It is filled by a listener on the record broadcast, never from the
//!   forwarding path, and that listener can itself lag, so an identifier can
//!   be missed.  It becomes known again with its next record.
//! - It holds at most [`MAX_SEEN`] identifiers.  A gateway in a busy area, or
//!   a flood of random addresses, must not grow it without bound: once full,
//!   new identifiers are ignored (and a rate-limited warning says so) until
//!   [`SeenDevices::expire`] frees room, while identifiers already present
//!   keep being refreshed.
//!
//! Every critical section is a few hash-map operations; nothing here does I/O
//! or logging while the lock is held.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::core::ratelimit::RateLimitedLog;

/// Most identifiers the list holds at once.
pub const MAX_SEEN: usize = 100_000;

/// Identifiers seen live, shared between the task that records sightings and
/// the code that answers "was this device seen recently?".
///
/// Cloning gives another handle to the same list.
#[derive(Clone)]
pub struct SeenDevices {
    inner: Arc<Mutex<HashMap<String, Instant>>>,
    cap: usize,
    /// Shared by all clones so the "list is full" warning is limited once,
    /// not once per handle.
    full_log: Arc<RateLimitedLog>,
}

impl SeenDevices {
    /// An empty list that holds up to [`MAX_SEEN`] identifiers.
    pub fn new() -> Self {
        Self::with_capacity(MAX_SEEN)
    }

    /// An empty list that holds up to `cap` identifiers.
    ///
    /// Production code uses [`SeenDevices::new`]; a small cap lets tests reach
    /// the full state without inserting a hundred thousand entries.
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            cap,
            full_log: Arc::new(RateLimitedLog::new()),
        }
    }

    /// Record that `device` was seen just now.
    ///
    /// An identifier already in the list has its time refreshed, even when the
    /// list is full.  A new one is added unless the list is full, in which
    /// case it is skipped and a rate-limited warning is logged.
    pub fn note(&self, device: &str) {
        self.note_at(device, Instant::now());
    }

    /// `true` if `device` was last seen no more than `window` ago.
    pub fn seen_within(&self, device: &str, window: Duration) -> bool {
        self.seen_within_at(device, window, Instant::now())
    }

    /// Forget identifiers last seen more than `window` ago and return how
    /// many were dropped.
    pub fn expire(&self, window: Duration) -> usize {
        self.expire_at(window, Instant::now())
    }

    /// Number of identifiers currently remembered.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    // The `_at` variants take the clock as a parameter so tests can cover the
    // time-dependent rules without sleeping.

    fn note_at(&self, device: &str, now: Instant) {
        let full = {
            let mut map = self.lock();
            if let Some(last) = map.get_mut(device) {
                *last = now;
                false
            } else if map.len() >= self.cap {
                true
            } else {
                map.insert(device.to_owned(), now);
                false
            }
        };
        // Logged after the lock is released: the logger may block on I/O.
        if full {
            self.full_log.warn(format_args!(
                "Live device list is full ({} identifiers): new devices are not tracked \
                 until older ones expire, so they cannot be labelled until then",
                self.cap
            ));
        }
    }

    fn seen_within_at(&self, device: &str, window: Duration, now: Instant) -> bool {
        self.lock()
            .get(device)
            .is_some_and(|&last| now.saturating_duration_since(last) <= window)
    }

    fn expire_at(&self, window: Duration, now: Instant) -> usize {
        let mut map = self.lock();
        let before = map.len();
        map.retain(|_, &mut last| now.saturating_duration_since(last) <= window);
        before - map.len()
    }

    /// Lock the map, ignoring poisoning.
    ///
    /// The critical sections only insert, refresh or remove entries, so a
    /// panic elsewhere cannot leave the map half-updated; refusing to answer
    /// after one would only make labelling fail for no reason.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Instant>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for SeenDevices {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(100);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn unknown_device_is_not_seen() {
        let seen = SeenDevices::new();
        assert!(!seen.seen_within("00E8A80B", WINDOW));
        assert_eq!(seen.len(), 0);
    }

    #[test]
    fn noted_device_is_seen_within_a_window_longer_than_the_gap() {
        let seen = SeenDevices::new();
        let t0 = Instant::now();
        seen.note_at("00E8A80B", t0);

        assert!(seen.seen_within_at("00E8A80B", WINDOW, t0));
        assert!(seen.seen_within_at("00E8A80B", WINDOW, t0 + secs(99)));
        // The boundary counts as within.
        assert!(seen.seen_within_at("00E8A80B", WINDOW, t0 + secs(100)));
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn noted_device_is_not_seen_within_a_window_shorter_than_the_gap() {
        let seen = SeenDevices::new();
        let t0 = Instant::now();
        seen.note_at("00E8A80B", t0);

        assert!(!seen.seen_within_at("00E8A80B", WINDOW, t0 + secs(101)));
        assert!(!seen.seen_within_at("00E8A80B", secs(5), t0 + secs(10)));
        assert!(!seen.seen_within_at("00E8A80B", Duration::ZERO, t0 + secs(1)));
    }

    #[test]
    fn devices_are_tracked_independently() {
        let seen = SeenDevices::new();
        let t0 = Instant::now();
        seen.note_at("00E8A80B", t0);
        seen.note_at("36:31:39:54:36:5D:05:39", t0 + secs(50));

        let now = t0 + secs(120);
        assert!(!seen.seen_within_at("00E8A80B", WINDOW, now));
        assert!(seen.seen_within_at("36:31:39:54:36:5D:05:39", WINDOW, now));
        assert!(!seen.seen_within_at("DEADBEEF", WINDOW, now));
    }

    #[test]
    fn real_clock_entry_is_seen_now() {
        let seen = SeenDevices::new();
        seen.note("01ABCDEF");
        assert!(seen.seen_within("01ABCDEF", WINDOW));
        assert!(!seen.seen_within("01ABCDE0", WINDOW));
    }

    #[test]
    fn expire_drops_only_old_entries_and_counts_them() {
        let seen = SeenDevices::new();
        let t0 = Instant::now();
        seen.note_at("A0000001", t0);
        seen.note_at("A0000002", t0 + secs(10));
        seen.note_at("A0000003", t0 + secs(90));
        seen.note_at("A0000004", t0 + secs(95));

        // At t0 + 120 the first two are older than the window, the rest not.
        assert_eq!(seen.expire_at(WINDOW, t0 + secs(120)), 2);
        assert_eq!(seen.len(), 2);
        assert!(!seen.seen_within_at("A0000001", WINDOW, t0 + secs(120)));
        assert!(seen.seen_within_at("A0000003", WINDOW, t0 + secs(120)));
        assert!(seen.seen_within_at("A0000004", WINDOW, t0 + secs(120)));

        // Nothing left to drop, and a later pass takes the remainder.
        assert_eq!(seen.expire_at(WINDOW, t0 + secs(120)), 0);
        assert_eq!(seen.expire_at(WINDOW, t0 + secs(500)), 2);
        assert_eq!(seen.len(), 0);
    }

    #[test]
    fn expire_keeps_an_entry_exactly_at_the_window() {
        let seen = SeenDevices::new();
        let t0 = Instant::now();
        seen.note_at("A0000001", t0);
        assert_eq!(seen.expire_at(WINDOW, t0 + secs(100)), 0);
        assert_eq!(seen.expire_at(WINDOW, t0 + secs(101)), 1);
    }

    #[test]
    fn expire_on_an_empty_list_is_a_no_op() {
        let seen = SeenDevices::new();
        assert_eq!(seen.expire(WINDOW), 0);
    }

    #[test]
    fn real_clock_expire() {
        let seen = SeenDevices::new();
        seen.note("A0000001");
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(seen.expire(Duration::from_secs(60)), 0);
        assert_eq!(seen.expire(Duration::from_millis(5)), 1);
        assert_eq!(seen.len(), 0);
    }

    #[test]
    fn a_new_sighting_keeps_an_entry_alive() {
        let seen = SeenDevices::new();
        let t0 = Instant::now();
        seen.note_at("A0000001", t0);
        seen.note_at("A0000001", t0 + secs(80));

        // 120 s after the first sighting but only 40 s after the second.
        let now = t0 + secs(120);
        assert!(seen.seen_within_at("A0000001", WINDOW, now));
        assert_eq!(seen.expire_at(WINDOW, now), 0);
        assert_eq!(seen.len(), 1);

        // And it does eventually go.
        assert_eq!(seen.expire_at(WINDOW, t0 + secs(181)), 1);
    }

    #[test]
    fn a_clock_earlier_than_the_sighting_counts_as_just_seen() {
        // The age is computed with saturating arithmetic, so a reading taken
        // slightly before the stored time can never underflow or panic.
        let seen = SeenDevices::new();
        let t0 = Instant::now();
        seen.note_at("A0000001", t0 + secs(50));
        assert!(seen.seen_within_at("A0000001", secs(1), t0));
        assert_eq!(seen.expire_at(secs(1), t0), 0);
    }

    #[test]
    fn the_cap_stops_new_entries_but_refreshes_existing_ones() {
        let seen = SeenDevices::with_capacity(3);
        let t0 = Instant::now();
        seen.note_at("A0000001", t0);
        seen.note_at("A0000002", t0);
        seen.note_at("A0000003", t0);
        assert_eq!(seen.len(), 3);

        // A fourth identifier is skipped, and the skip is logged (once, then
        // limited).
        seen.note_at("A0000004", t0 + secs(1));
        seen.note_at("A0000005", t0 + secs(2));
        assert_eq!(seen.len(), 3);
        assert!(!seen.seen_within_at("A0000004", WINDOW, t0 + secs(2)));
        assert_eq!(seen.full_log.logged(), 1);
        assert_eq!(seen.full_log.suppressed(), 1);

        // Existing entries still refresh while the list is full, without
        // logging.
        seen.note_at("A0000001", t0 + secs(60));
        assert_eq!(seen.len(), 3);
        assert!(seen.seen_within_at("A0000001", secs(10), t0 + secs(65)));
        assert_eq!(seen.full_log.logged() + seen.full_log.suppressed(), 2);
    }

    #[test]
    fn expiring_makes_room_for_new_entries() {
        let seen = SeenDevices::with_capacity(2);
        let t0 = Instant::now();
        seen.note_at("A0000001", t0);
        seen.note_at("A0000002", t0 + secs(90));
        seen.note_at("A0000003", t0 + secs(95)); // refused: full
        assert_eq!(seen.len(), 2);

        assert_eq!(seen.expire_at(WINDOW, t0 + secs(120)), 1);
        seen.note_at("A0000003", t0 + secs(121));
        assert_eq!(seen.len(), 2);
        assert!(seen.seen_within_at("A0000003", WINDOW, t0 + secs(121)));
    }

    #[test]
    fn a_zero_cap_tracks_nothing() {
        let seen = SeenDevices::with_capacity(0);
        seen.note("A0000001");
        assert_eq!(seen.len(), 0);
        assert!(!seen.seen_within("A0000001", WINDOW));
    }

    #[test]
    fn clones_share_one_list() {
        let a = SeenDevices::new();
        let b = a.clone();
        a.note("A0000001");
        assert!(b.seen_within("A0000001", WINDOW));
        b.note("A0000002");
        assert_eq!(a.len(), 2);
        assert_eq!(a.expire_at(WINDOW, Instant::now() + secs(1000)), 2);
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn clones_share_the_full_warning_budget() {
        let a = SeenDevices::with_capacity(1);
        let b = a.clone();
        a.note("A0000001");
        a.note("A0000002");
        b.note("A0000003");
        assert_eq!(a.full_log.logged(), 1);
        assert_eq!(a.full_log.suppressed(), 1);
    }

    #[test]
    fn concurrent_notes_do_not_lose_entries() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 500;

        let seen = SeenDevices::new();
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let seen = seen.clone();
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        seen.note(&format!("{:08X}", t * PER_THREAD + i));
                        // Overlapping identifiers exercise the refresh path.
                        seen.note("FFFFFFFF");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(seen.len(), THREADS * PER_THREAD + 1);
    }
}
