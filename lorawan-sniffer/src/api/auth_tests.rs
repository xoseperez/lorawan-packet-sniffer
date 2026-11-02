//! Tests of the login core: sessions, the token comparison, the guessing
//! limit and the credential found in a request.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use chrono::{DateTime, Utc};
use log::Level;
use tokio::time::Instant;

use super::*;
use crate::core::linelog::{start_capture, take_capture};
use crate::core::random::testing::FailingRandom;
use crate::core::random::SystemRandomSource;

/// A token of the shape the login asks for.
const TOKEN: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// A nonce that holds letters as well as digits, for sessions built by hand.
const NONCE: &str = "00112233445566778899aabbccddeeff";

const ISSUED: i64 = 1_700_000_000;

fn secret(fill: u8) -> [u8; 32] {
    [fill; 32]
}

fn system_random() -> Arc<dyn RandomSource> {
    Arc::new(SystemRandomSource::new())
}

fn issued_at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).unwrap()
}

fn auth(token: &str, secret: &[u8; 32]) -> Auth {
    Auth::new(token, secret, system_random()).unwrap()
}

/// Sign `issued` and `nonce` with `key`, the way a session is, but with
/// whatever key the test wants to try.
fn built_by_hand(key: &hmac::Key, issued: &str, nonce: &str) -> String {
    let signed = format!("v1.{issued}.{nonce}");
    let mac = hmac::sign(key, signed.as_bytes());
    format!("{signed}.{}", hex(mac.as_ref()))
}

// -- Time and counting helpers ---------------------------------------------------

/// A clock the test moves.
struct ManualClock {
    base: Instant,
    offset: Mutex<Duration>,
}

impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(ManualClock {
            base: Instant::now(),
            offset: Mutex::new(Duration::ZERO),
        })
    }

    /// Move the clock to `ms` milliseconds after it was made.
    fn set_ms(&self, ms: u64) {
        *self.offset.lock().unwrap() = Duration::from_millis(ms);
    }

    fn at_ms(&self, ms: u64) -> Instant {
        self.base + Duration::from_millis(ms)
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock().unwrap()
    }
}

/// Calls of the comparison, counted.
#[derive(Default)]
struct Calls(AtomicUsize);

impl Calls {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    /// One attempt whose comparison answers `answer`.
    fn attempt(&self, lockout: &Lockout, answer: bool) -> TokenCheck {
        lockout.attempt(|| {
            self.0.fetch_add(1, Ordering::SeqCst);
            answer
        })
    }

    fn wrong(&self, lockout: &Lockout) -> TokenCheck {
        self.attempt(lockout, false)
    }

    fn right(&self, lockout: &Lockout) -> TokenCheck {
        self.attempt(lockout, true)
    }
}

/// The window's start and count.
fn window(lockout: &Lockout) -> (Instant, u32) {
    let window = lockout.inner.lock().unwrap_or_else(PoisonError::into_inner);
    (window.start, window.wrong)
}

fn lockout_at_zero() -> (Arc<ManualClock>, Lockout) {
    let clock = ManualClock::new();
    let lockout = Lockout::new(clock.clone());
    (clock, lockout)
}

/// A source that serves `fills` requests and then fails.
struct FillsThenFails {
    left: AtomicUsize,
}

impl FillsThenFails {
    fn new(fills: usize) -> Arc<Self> {
        Arc::new(FillsThenFails {
            left: AtomicUsize::new(fills),
        })
    }
}

impl RandomSource for FillsThenFails {
    fn fill(&self, out: &mut [u8]) -> Result<(), RandomError> {
        let served = self
            .left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if served {
            SystemRandomSource::new().fill(out)
        } else {
            Err(RandomError)
        }
    }
}

// -- Sessions ---------------------------------------------------------------------

#[test]
fn a_session_round_trips_and_has_the_public_shape() {
    let auth = auth(TOKEN, &secret(1));
    let session = auth.new_session(issued_at(ISSUED)).unwrap();

    let parts: Vec<&str> = session.split('.').collect();
    assert_eq!(parts.len(), 4);
    assert_eq!(parts[0], "v1");
    assert_eq!(parts[1], ISSUED.to_string());
    assert_eq!(parts[2].len(), 32);
    assert_eq!(parts[3].len(), 64);
    assert!(parts[2..]
        .iter()
        .all(|p| p.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))));

    assert!(auth.session_valid(&session));
}

#[test]
fn every_session_has_its_own_nonce() {
    let auth = auth(TOKEN, &secret(1));
    let a = auth.new_session(issued_at(ISSUED)).unwrap();
    let b = auth.new_session(issued_at(ISSUED)).unwrap();
    assert_ne!(a, b);
    assert!(auth.session_valid(&a) && auth.session_valid(&b));
}

#[test]
fn a_clock_before_1970_still_makes_a_valid_session() {
    let auth = auth(TOKEN, &secret(1));
    let session = auth.new_session(issued_at(-5)).unwrap();
    assert!(session.starts_with("v1.0."));
    assert!(auth.session_valid(&session));
}

#[test]
fn a_session_survives_a_restart() {
    // A restart builds a new `Auth` with a new comparison key, but from the
    // same token and the same stored secret.
    let before = auth(TOKEN, &secret(1));
    let after = auth(TOKEN, &secret(1));
    let session = before.new_session(issued_at(ISSUED)).unwrap();
    assert!(after.session_valid(&session));
}

#[test]
fn a_session_made_with_another_token_is_invalid() {
    let ours = auth(TOKEN, &secret(1));
    let theirs = auth("ffeeddccbbaa99887766554433221100", &secret(1));
    let session = theirs.new_session(issued_at(ISSUED)).unwrap();
    assert!(theirs.session_valid(&session));
    assert!(!ours.session_valid(&session));
}

#[test]
fn a_session_made_with_another_secret_is_invalid() {
    let ours = auth(TOKEN, &secret(1));
    let theirs = auth(TOKEN, &secret(2));
    let session = theirs.new_session(issued_at(ISSUED)).unwrap();
    assert!(theirs.session_valid(&session));
    assert!(!ours.session_valid(&session));
}

#[test]
fn a_session_built_without_the_stored_secret_is_invalid() {
    let auth = auth(TOKEN, &secret(1));

    // The control: built by hand with the real key, the helper makes a valid
    // session, so the refusals below are down to the key.
    assert!(auth.session_valid(&built_by_hand(&session_key(&secret(1), TOKEN), "1", NONCE)));

    // Keyed with the token itself, as when the token was all there was to know.
    let token_only = hmac::Key::new(hmac::HMAC_SHA256, TOKEN.as_bytes());
    assert!(!auth.session_valid(&built_by_hand(&token_only, "1", NONCE)));

    // Derived from an all-zero secret, or from some other one.
    for guess in [secret(0), secret(2)] {
        let key = session_key(&guess, TOKEN);
        assert!(!auth.session_valid(&built_by_hand(&key, "1", NONCE)));
    }

    // Keyed with the secret alone, without the token.
    let secret_only = hmac::Key::new(hmac::HMAC_SHA256, &secret(1));
    assert!(!auth.session_valid(&built_by_hand(&secret_only, "1", NONCE)));
}

#[test]
fn every_part_of_a_session_is_checked() {
    let auth = auth(TOKEN, &secret(1));
    let key = session_key(&secret(1), TOKEN);
    let session = built_by_hand(&key, &ISSUED.to_string(), NONCE);
    assert!(auth.session_valid(&session));
    let parts: Vec<String> = session.split('.').map(String::from).collect();

    // Each variant replaces one part and keeps the rest, so it is the changed
    // part alone that has to make it invalid.
    let with = |index: usize, value: &str| {
        let mut parts = parts.clone();
        parts[index] = value.to_string();
        parts.join(".")
    };
    let flipped = |text: &str| {
        let mut chars: Vec<char> = text.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == '0' { '1' } else { '0' };
        chars.into_iter().collect::<String>()
    };

    let variants = vec![
        ("version 2", with(0, "v2")),
        ("version in upper case", with(0, "V1")),
        ("no version", with(0, "")),
        ("issued one later", with(1, &(ISSUED + 1).to_string())),
        ("issued empty", with(1, "")),
        ("issued signed", with(1, &format!("+{ISSUED}"))),
        ("issued with a space", with(1, &format!(" {ISSUED}"))),
        ("issued in exponent form", with(1, "17e8")),
        ("issued in other digits", with(1, "\u{661}\u{662}")),
        ("nonce changed", with(2, &flipped(&parts[2]))),
        ("nonce one short", with(2, &parts[2][1..])),
        ("nonce one long", with(2, &format!("{}0", parts[2]))),
        ("nonce in upper case", with(2, &parts[2].to_uppercase())),
        ("nonce empty", with(2, "")),
        ("mac changed", with(3, &flipped(&parts[3]))),
        ("mac one short", with(3, &parts[3][1..])),
        ("mac half", with(3, &parts[3][..32])),
        ("mac one long", with(3, &format!("{}0", parts[3]))),
        ("mac in upper case", with(3, &parts[3].to_uppercase())),
        ("mac not hex", with(3, &format!("{}g", &parts[3][1..]))),
        ("mac empty", with(3, "")),
        ("an extra part", format!("{session}.00")),
        (
            "an extra part before the mac",
            with(3, &format!("00.{}", parts[3])),
        ),
        ("no mac", parts[..3].join(".")),
        (
            "no nonce",
            format!("{}.{}.{}", parts[0], parts[1], parts[3]),
        ),
        ("empty", String::new()),
        ("only dots", "...".to_string()),
    ];
    for (what, value) in variants {
        assert!(!auth.session_valid(&value), "accepted: {what}");
    }
}

#[test]
fn no_single_edit_of_a_session_is_accepted() {
    let auth = auth(TOKEN, &secret(1));
    let session = auth.new_session(issued_at(ISSUED)).unwrap();
    let chars: Vec<char> = session.chars().collect();
    let replacements = ['0', 'f', '.', 'G', ' ', 'é'];

    for i in 0..chars.len() {
        let mut deleted = chars.clone();
        deleted.remove(i);
        assert!(!auth.session_valid(&deleted.iter().collect::<String>()));

        for &r in &replacements {
            if r != chars[i] {
                let mut replaced = chars.clone();
                replaced[i] = r;
                assert!(!auth.session_valid(&replaced.iter().collect::<String>()));
            }
        }
    }
    for i in 0..=chars.len() {
        for &r in &replacements {
            let mut inserted = chars.clone();
            inserted.insert(i, r);
            assert!(!auth.session_valid(&inserted.iter().collect::<String>()));
        }
    }
    assert!(auth.session_valid(&session));
}

#[test]
fn malformed_input_is_invalid_and_never_panics() {
    let auth = auth(TOKEN, &secret(1));

    // A small generator with a fixed seed, so a failure repeats.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let alphabet: Vec<char> = "0123456789abcdefABCDEF.v1 \t\0gzé\u{661}\u{1F600}=;\"'"
        .chars()
        .collect();
    for _ in 0..20_000 {
        let len = (next() % 200) as usize;
        let text: String = (0..len)
            .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
            .collect();
        assert!(!auth.session_valid(&text));
    }

    for text in ["v1.", "v1..", "v1...", ".", "..", "v1.1.", "v1.1.x.y"] {
        assert!(!auth.session_valid(text));
    }
    // Long inputs, in the shapes that stress the parser.
    assert!(!auth.session_valid(&"a".repeat(1 << 20)));
    assert!(!auth.session_valid(&".".repeat(1 << 20)));
    assert!(!auth.session_valid(&format!(
        "v1.{}.{NONCE}.{}",
        "9".repeat(1 << 20),
        "0".repeat(64)
    )));
}

#[test]
fn the_session_key_changes_with_every_byte_of_the_token_and_the_secret() {
    let tag = |key: &hmac::Key| hmac::sign(key, b"a fixed message").as_ref().to_vec();
    let base = tag(&session_key(&secret(1), TOKEN));
    assert_eq!(base, tag(&session_key(&secret(1), TOKEN)), "not repeatable");

    for i in 0..TOKEN.len() {
        let mut bytes = TOKEN.as_bytes().to_vec();
        bytes[i] = if bytes[i] == b'0' { b'1' } else { b'0' };
        let changed = String::from_utf8(bytes).unwrap();
        assert_ne!(
            base,
            tag(&session_key(&secret(1), &changed)),
            "token byte {i}"
        );
    }
    for i in 0..32 {
        let mut changed = secret(1);
        changed[i] ^= 1;
        assert_ne!(base, tag(&session_key(&changed, TOKEN)), "secret byte {i}");
    }
}

#[test]
fn the_session_key_is_hmac_of_label_zero_token_under_the_secret() {
    let mut input = SESSION_KEY_LABEL.to_vec();
    input.push(0);
    input.extend_from_slice(TOKEN.as_bytes());
    let derived = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &secret(1)), &input);
    let expected = hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref());

    let tag = |key: &hmac::Key| hmac::sign(key, b"a fixed message").as_ref().to_vec();
    assert_eq!(tag(&session_key(&secret(1), TOKEN)), tag(&expected));
}

// -- Random source failures ---------------------------------------------------------

#[test]
fn a_failing_random_source_stops_the_setup_with_a_message() {
    let err = Auth::new(TOKEN, &secret(1), Arc::new(FailingRandom))
        .err()
        .expect("setup needs random bytes");
    assert!(matches!(err, AuthError::Random(_)));
    assert_eq!(
        err.to_string(),
        "cannot read random bytes for the WebUI login: the operating system's random source failed"
    );

    // The test seam asks for the same bytes, even with its own comparison.
    let double: Comparer = Box::new(|_| true);
    let result = Auth::with_parts(
        TOKEN,
        &secret(1),
        Arc::new(FailingRandom),
        ManualClock::new(),
        Some(double),
    );
    assert!(result.is_err());
}

#[test]
fn a_failing_random_source_means_no_session() {
    // One fill serves the setup; the next, for the nonce, fails.
    let auth = Auth::new(TOKEN, &secret(1), FillsThenFails::new(1)).unwrap();
    let err = auth.new_session(issued_at(ISSUED)).unwrap_err();
    assert!(matches!(err, AuthError::Random(_)));
    // Still failing, still no panic and still no session.
    assert!(auth.new_session(issued_at(ISSUED)).is_err());
}

// -- The token comparison --------------------------------------------------------------

#[test]
fn token_matches_only_the_exact_token() {
    let key = hmac::Key::new(hmac::HMAC_SHA256, &[7u8; 32]);
    let tag = hmac::sign(&key, TOKEN.as_bytes());
    let matches = |candidate: &str| token_matches(&key, tag.as_ref(), candidate);

    assert!(matches(TOKEN));
    assert!(!matches(&TOKEN[..TOKEN.len() - 1]), "a prefix");
    assert!(!matches(&TOKEN[1..]), "a suffix");
    assert!(!matches(&format!("{TOKEN}0")), "a longer one");
    assert!(!matches(&format!("{TOKEN}{TOKEN}")), "twice");
    assert!(!matches(""), "an empty one");
    assert!(!matches(&TOKEN.to_uppercase()), "another case");
    assert!(!matches(&format!(" {TOKEN}")), "with a space");
}

#[test]
fn the_production_setup_compares_with_the_real_comparison() {
    let production = Auth::new(TOKEN, &secret(1), system_random()).unwrap();
    assert_eq!(production.check_token(TOKEN), TokenCheck::Right);
    assert_eq!(production.check_token("wrong"), TokenCheck::Wrong);
    assert_eq!(
        production.check_token(&TOKEN.to_uppercase()),
        TokenCheck::Wrong
    );
    assert_eq!(production.check_token(""), TokenCheck::Wrong);
    assert_eq!(production.check_token(TOKEN), TokenCheck::Right);

    // The test seam, left without a double, is the same comparison.
    let seam =
        Auth::with_parts(TOKEN, &secret(1), system_random(), ManualClock::new(), None).unwrap();
    assert_eq!(seam.check_token(TOKEN), TokenCheck::Right);
    assert_eq!(seam.check_token("wrong"), TokenCheck::Wrong);
}

#[test]
fn a_comparison_double_replaces_the_real_one() {
    let double: Comparer = Box::new(|candidate| candidate.len() == 3);
    let auth = Auth::with_parts(
        TOKEN,
        &secret(1),
        system_random(),
        ManualClock::new(),
        Some(double),
    )
    .unwrap();
    assert_eq!(auth.check_token("abc"), TokenCheck::Right);
    assert_eq!(auth.check_token(TOKEN), TokenCheck::Wrong);
}

// -- Logging ---------------------------------------------------------------------------

#[test]
fn a_failure_line_names_the_address_and_is_rate_limited() {
    let auth = auth(TOKEN, &secret(1));
    start_capture();
    auth.note_failure(Some("192.0.2.7:4321".parse().unwrap()), "wrong token");
    auth.note_failure(Some("192.0.2.8:1".parse().unwrap()), "invalid session");
    auth.note_failure(None, "wrong token");

    let lines = take_capture();
    assert_eq!(
        lines,
        vec![(
            Level::Warn,
            "WebUI login failed from 192.0.2.7:4321: wrong token".to_string()
        )]
    );
}

#[test]
fn a_failure_line_without_an_address_says_unknown() {
    let auth = auth(TOKEN, &secret(1));
    start_capture();
    auth.note_failure(None, "invalid session");
    assert_eq!(
        take_capture(),
        vec![(
            Level::Warn,
            "WebUI login failed from unknown: invalid session".to_string()
        )]
    );
}

#[test]
fn noting_failures_counts_nothing() {
    let auth = auth(TOKEN, &secret(1));
    for _ in 0..50 {
        auth.note_failure(None, "wrong token");
    }
    assert_eq!(window(&auth.lockout).1, 0);
    assert_eq!(auth.check_token(TOKEN), TokenCheck::Right);
}

#[test]
fn the_origin_line_is_limited_on_its_own() {
    let auth = auth(TOKEN, &secret(1));
    start_capture();
    auth.note_failure(None, "wrong token");
    auth.warn_origin("the proxy changed Host");
    auth.warn_origin("the proxy changed Host again");

    let lines: Vec<String> = take_capture().into_iter().map(|(_, line)| line).collect();
    assert_eq!(
        lines,
        vec![
            "WebUI login failed from unknown: wrong token".to_string(),
            "the proxy changed Host".to_string()
        ]
    );
}

// -- Lockout -----------------------------------------------------------------------------

#[test]
fn ten_wrong_tokens_lock_the_next_one_out_unexamined() {
    let (clock, lockout) = lockout_at_zero();
    let calls = Calls::default();

    for _ in 0..10 {
        assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    }
    assert_eq!(calls.count(), 10);

    clock.set_ms(25_000);
    assert_eq!(
        calls.wrong(&lockout),
        TokenCheck::Locked(Duration::from_secs(35))
    );
    // The right token is refused too, and neither was looked at.
    assert_eq!(
        calls.right(&lockout),
        TokenCheck::Locked(Duration::from_secs(35))
    );
    assert_eq!(calls.count(), 10);

    // The time left follows the clock, to the end of the window.
    clock.set_ms(59_999);
    assert_eq!(
        calls.right(&lockout),
        TokenCheck::Locked(Duration::from_millis(1))
    );
    assert_eq!(calls.count(), 10);
}

#[test]
fn the_window_ending_lifts_the_lockout() {
    let (clock, lockout) = lockout_at_zero();
    let calls = Calls::default();
    for _ in 0..10 {
        calls.wrong(&lockout);
    }
    assert!(matches!(calls.right(&lockout), TokenCheck::Locked(_)));

    clock.set_ms(60_000);
    assert_eq!(calls.right(&lockout), TokenCheck::Right);
    assert_eq!(window(&lockout).1, 0, "a new window starts empty");
    // And it fills again from nothing.
    for _ in 0..10 {
        assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    }
    assert_eq!(
        calls.right(&lockout),
        TokenCheck::Locked(Duration::from_secs(60))
    );
}

#[test]
fn a_right_token_is_never_counted() {
    let (_clock, lockout) = lockout_at_zero();
    let calls = Calls::default();

    // 9 wrong, 1 right, 1 wrong: ten wrong in all, so the next is locked.
    for _ in 0..9 {
        assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    }
    assert_eq!(calls.right(&lockout), TokenCheck::Right);
    assert_eq!(window(&lockout).1, 9);
    assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    assert!(matches!(calls.wrong(&lockout), TokenCheck::Locked(_)));
    assert_eq!(calls.count(), 11);
}

#[test]
fn right_tokens_alone_never_lock() {
    let (_clock, lockout) = lockout_at_zero();
    let calls = Calls::default();
    for _ in 0..100 {
        assert_eq!(calls.right(&lockout), TokenCheck::Right);
    }
    assert_eq!(calls.count(), 100);
    // The window is still empty: a wrong token is compared and counted.
    assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    assert_eq!(window(&lockout).1, 1);
}

#[test]
fn twenty_wrong_tokens_fit_around_a_window_edge_and_no_more() {
    let (clock, lockout) = lockout_at_zero();
    let calls = Calls::default();

    clock.set_ms(59_900);
    for _ in 0..10 {
        assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    }
    assert!(matches!(calls.wrong(&lockout), TokenCheck::Locked(_)));
    assert_eq!(calls.count(), 10);

    // 0.1 s later the next window has begun: ten more are compared.
    clock.set_ms(60_000);
    for _ in 0..10 {
        assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    }
    assert_eq!(
        calls.wrong(&lockout),
        TokenCheck::Locked(Duration::from_secs(60))
    );
    assert_eq!(calls.count(), 20);
}

#[test]
fn the_clock_is_read_under_the_lock() {
    let (clock, lockout) = lockout_at_zero();
    let calls = Calls::default();

    // Nine wrong tokens just before the window ends.
    clock.set_ms(59_999);
    for _ in 0..9 {
        calls.wrong(&lockout);
    }

    // The tenth is read at 59.999 s; while it is being compared the clock
    // passes the end of the window.  It was compared in the old window and is
    // counted there.
    let moved = clock.clone();
    let check = lockout.attempt(|| {
        moved.set_ms(60_001);
        false
    });
    assert_eq!(check, TokenCheck::Wrong);
    assert_eq!(window(&lockout), (clock.at_ms(0), 10));

    // The next attempt reads 60.001 s, so it starts a window of its own, and
    // is compared instead of refused.
    assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    assert_eq!(window(&lockout), (clock.at_ms(60_001), 1));

    // What it is told about the time left comes from the clock it read: this
    // window began at 60.001 s, which is where the clock still stands.
    for _ in 0..9 {
        assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    }
    assert_eq!(
        calls.wrong(&lockout),
        TokenCheck::Locked(Duration::from_secs(60))
    );
    clock.set_ms(70_001);
    assert_eq!(
        calls.wrong(&lockout),
        TokenCheck::Locked(Duration::from_secs(50))
    );
}

#[test]
fn a_comparison_that_panics_is_not_counted_and_the_next_attempt_works() {
    let (_clock, lockout) = lockout_at_zero();
    let calls = Calls::default();
    for _ in 0..3 {
        calls.wrong(&lockout);
    }

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        lockout.attempt(|| panic!("the comparison failed"))
    }));
    assert!(outcome.is_err());
    assert!(
        lockout.inner.is_poisoned(),
        "the test needs a poisoned lock"
    );
    assert_eq!(
        window(&lockout).1,
        3,
        "a comparison that did not finish counts for nothing"
    );

    // The lock is recovered, and counting goes on from where it was.
    assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    assert_eq!(calls.right(&lockout), TokenCheck::Right);
    for _ in 0..6 {
        assert_eq!(calls.wrong(&lockout), TokenCheck::Wrong);
    }
    assert!(matches!(calls.right(&lockout), TokenCheck::Locked(_)));
}

#[test]
fn a_locked_window_logs_one_warning_on_its_first_refusal() {
    let (clock, lockout) = lockout_at_zero();
    let calls = Calls::default();
    start_capture();

    for _ in 0..10 {
        calls.wrong(&lockout);
    }
    assert!(
        take_capture().is_empty(),
        "nothing is logged before a refusal"
    );

    clock.set_ms(1_000);
    for _ in 0..5 {
        assert!(matches!(calls.wrong(&lockout), TokenCheck::Locked(_)));
    }
    assert_eq!(
        take_capture(),
        vec![(
            Level::Warn,
            "WebUI login locked for 59 s after 10 wrong tokens".to_string()
        )]
    );

    // The next window locks again: one more line.
    clock.set_ms(61_000);
    for _ in 0..10 {
        calls.wrong(&lockout);
    }
    clock.set_ms(62_000);
    for _ in 0..5 {
        calls.right(&lockout);
    }
    assert_eq!(
        take_capture(),
        vec![(
            Level::Warn,
            "WebUI login locked for 59 s after 10 wrong tokens".to_string()
        )]
    );
}

#[test]
fn the_time_left_is_rounded_up_to_whole_seconds() {
    assert_eq!(ceil_secs(Duration::ZERO), 0);
    assert_eq!(ceil_secs(Duration::from_nanos(1)), 1);
    assert_eq!(ceil_secs(Duration::from_secs(59)), 59);
    assert_eq!(ceil_secs(Duration::from_millis(59_001)), 60);
}

#[tokio::test(start_paused = true)]
async fn the_production_clock_follows_the_tokio_clock() {
    let auth = auth(TOKEN, &secret(1));
    for _ in 0..10 {
        assert_eq!(auth.check_token("wrong"), TokenCheck::Wrong);
    }
    assert_eq!(auth.check_token(TOKEN), TokenCheck::Locked(Lockout::WINDOW));

    tokio::time::advance(Lockout::WINDOW).await;
    assert_eq!(auth.check_token(TOKEN), TokenCheck::Right);
}

// -- Lockout under concurrency ---------------------------------------------------------------

/// 64 threads, released together, each present one token to an `Auth` whose
/// comparison takes 2 ms and answers `answer`.  Returns what each was told and
/// how many comparisons ran.
fn present_64_tokens_together(answer: bool) -> (Vec<TokenCheck>, usize) {
    const THREADS: usize = 64;
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    // The sleep widens any gap between checking the count and adding to it.
    let compare: Comparer = Box::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(2));
        answer
    });
    let auth = Arc::new(
        Auth::with_parts(
            TOKEN,
            &secret(1),
            system_random(),
            ManualClock::new(),
            Some(compare),
        )
        .unwrap(),
    );
    let barrier = Arc::new(Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let auth = auth.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                auth.check_token(&format!("candidate {i}"))
            })
        })
        .collect();
    let results = handles.into_iter().map(|h| h.join().unwrap()).collect();
    (results, calls.load(Ordering::SeqCst))
}

#[test]
fn sixty_four_wrong_tokens_together_are_ten_comparisons_and_fifty_four_refusals() {
    let (results, compared) = present_64_tokens_together(false);
    let count = |wanted: fn(&TokenCheck) -> bool| results.iter().filter(|r| wanted(r)).count();

    assert_eq!(compared, 10);
    assert_eq!(count(|r| *r == TokenCheck::Wrong), 10);
    assert_eq!(count(|r| matches!(r, TokenCheck::Locked(_))), 54);
    assert_eq!(count(|r| *r == TokenCheck::Right), 0);
}

#[test]
fn sixty_four_right_tokens_together_are_all_compared_and_all_accepted() {
    let (results, compared) = present_64_tokens_together(true);

    assert_eq!(compared, 64);
    assert!(results.iter().all(|r| *r == TokenCheck::Right));
}

// -- Credentials in a request -------------------------------------------------------------------

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

/// A credential as plain values, for comparing in tests.  `Credential` has no
/// `==` on purpose: production code must never compare a token that way.
fn plain(c: Credential<'_>) -> (&'static str, Option<String>) {
    match c {
        Credential::None => ("none", None),
        Credential::Bearer(token) => ("bearer", Some(token.to_string())),
        Credential::Cookie(value) => ("cookie", Some(value.to_string())),
    }
}

#[test]
fn a_request_with_no_credential_has_none() {
    assert_eq!(
        plain(credential(&HeaderMap::new())),
        plain(Credential::None)
    );
    assert_eq!(
        plain(credential(&headers(&[("cookie", "theme=dark; lang=en")]))),
        plain(Credential::None)
    );
}

#[test]
fn a_bearer_token_is_found_whatever_the_case_of_the_scheme_or_the_spacing() {
    for value in [
        "Bearer tok",
        "bearer tok",
        "BEARER tok",
        "BeArEr tok",
        "Bearer    tok",
    ] {
        assert_eq!(
            plain(credential(&headers(&[("authorization", value)]))),
            plain(Credential::Bearer("tok")),
            "{value}"
        );
    }
}

#[test]
fn a_bearer_token_wins_over_the_cookie() {
    let both = headers(&[
        ("cookie", "lorawan_sniffer_session=abc"),
        ("authorization", "Bearer tok"),
    ]);
    assert_eq!(plain(credential(&both)), plain(Credential::Bearer("tok")));
}

#[test]
fn the_session_cookie_is_found_among_other_cookies() {
    for line in [
        "lorawan_sniffer_session=abc",
        "theme=dark; lorawan_sniffer_session=abc; lang=en",
        "theme=dark;lorawan_sniffer_session=abc",
        "  lorawan_sniffer_session=abc  ;theme=dark",
        "lorawan_sniffer_session=abc;",
    ] {
        assert_eq!(
            plain(credential(&headers(&[("cookie", line)]))),
            plain(Credential::Cookie("abc")),
            "{line}"
        );
    }

    // Any `Cookie` header will do, as HTTP/2 clients may split them.
    let split = headers(&[
        ("cookie", "theme=dark"),
        ("cookie", "lorawan_sniffer_session=abc"),
    ]);
    assert_eq!(plain(credential(&split)), plain(Credential::Cookie("abc")));
}

#[test]
fn other_cookies_are_not_the_session() {
    for line in [
        "x_lorawan_sniffer_session=abc",
        "lorawan_sniffer_session2=abc",
        "Lorawan_Sniffer_Session=abc",
        "lorawan_sniffer_session",
        "theme=lorawan_sniffer_session=abc",
        "lorawan_sniffer_session=",
    ] {
        assert_eq!(
            plain(credential(&headers(&[("cookie", line)]))),
            plain(Credential::None),
            "{line}"
        );
    }
}

#[test]
fn a_bearer_header_that_is_not_usable_is_no_credential_even_with_a_cookie() {
    let cookie = ("cookie", "lorawan_sniffer_session=abc");
    for value in [
        "Bearer",
        "Bearer ",
        "Bearer   ",
        "Bearer tok extra",
        "Bearer tok\t",
        "Bearer to\tk",
        "Bearer\ttok",
        "bearer",
    ] {
        assert_eq!(
            plain(credential(&headers(&[("authorization", value), cookie]))),
            plain(Credential::None),
            "{value:?}"
        );
    }

    // A header with bytes outside visible ASCII, and a repeated header.
    let mut odd = headers(&[cookie]);
    odd.insert(
        header::AUTHORIZATION,
        HeaderValue::from_bytes(b"Bearer t\xffk").unwrap(),
    );
    assert_eq!(plain(credential(&odd)), plain(Credential::None));

    let twice = headers(&[
        ("authorization", "Bearer tok"),
        ("authorization", "Bearer tok"),
        cookie,
    ]);
    assert_eq!(plain(credential(&twice)), plain(Credential::None));

    // A Bearer header next to one of another scheme is a repeated header too.
    let mixed = headers(&[
        ("authorization", "Basic dXNlcjpwYXNz"),
        ("authorization", "Bearer tok"),
        cookie,
    ]);
    assert_eq!(plain(credential(&mixed)), plain(Credential::None));
}

#[test]
fn an_authorization_header_of_another_scheme_leaves_the_cookie_to_be_read() {
    // A reverse proxy that asks for its own password makes the browser send
    // `Authorization: Basic …` with every request, the cookie included.
    let cookie = ("cookie", "lorawan_sniffer_session=abc");
    for value in [
        "Basic dXNlcjpwYXNz",
        "basic dXNlcjpwYXNz",
        "Digest username=\"u\"",
        "Token tok",
        "Bearertok",
        "tok",
        "",
        " Bearer tok",
    ] {
        assert_eq!(
            plain(credential(&headers(&[("authorization", value), cookie]))),
            plain(Credential::Cookie("abc")),
            "{value:?}"
        );
        assert_eq!(
            plain(credential(&headers(&[("authorization", value)]))),
            plain(Credential::None),
            "{value:?} without the cookie"
        );
    }

    // Several headers of other schemes, and one that is not valid UTF-8.
    let twice = headers(&[
        ("authorization", "Basic dXNlcjpwYXNz"),
        ("authorization", "Negotiate abc"),
        cookie,
    ]);
    assert_eq!(plain(credential(&twice)), plain(Credential::Cookie("abc")));
    let mut opaque = headers(&[cookie]);
    opaque.insert(
        header::AUTHORIZATION,
        HeaderValue::from_bytes(b"Basic \xff\xfe").unwrap(),
    );
    assert_eq!(plain(credential(&opaque)), plain(Credential::Cookie("abc")));
}

#[test]
fn a_credential_prints_without_its_value() {
    let shown = format!(
        "{:?} {:?} {:?}",
        Credential::None,
        Credential::Bearer("canary-token"),
        Credential::Cookie("canary-session")
    );
    assert!(!shown.contains("canary"));
    assert!(shown.contains("Bearer") && shown.contains("Cookie"));
}
