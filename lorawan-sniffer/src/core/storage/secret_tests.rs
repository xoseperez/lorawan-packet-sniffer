//! Tests of the session secret: the migration that creates its table, and
//! [`Storage::session_secret`] with the read-first rule behind it.
//!
//! The last two tests are `#[ignore]`d helpers for the rollback script, which
//! needs a database made, and read back, by this build.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, OpenFlags};

use super::tests::{sample_record, TempDb, SCHEMA_2_LABELS, SCHEMA_V2_0_0, SCHEMA_V2_1_0_UPGRADE};
use super::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Longest a test waits for another thread.  Every wait has this limit, so a
/// test that goes wrong fails instead of hanging.
const WAIT: Duration = Duration::from_secs(5);

/// A 32-byte secret that depends on `seed`, so two seeds never agree.
fn bytes(seed: u8) -> [u8; 32] {
    std::array::from_fn(|i| seed.wrapping_mul(37).wrapping_add(i as u8))
}

/// `fresh` for a call that must find a secret already stored.
fn never_called() -> Result<[u8; 32], RandomError> {
    panic!("a secret was stored, so no random bytes should have been asked for");
}

/// A `fresh` that counts its calls and hands out `secret`.
fn counting(
    calls: &Arc<AtomicUsize>,
    secret: [u8; 32],
) -> impl FnOnce() -> Result<[u8; 32], RandomError> {
    let calls = Arc::clone(calls);
    move || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(secret)
    }
}

/// A database file at the current schema, with `secret` stored if given.
/// Nothing is open on it when this returns.
fn database(tmp: &TempDb, secret: Option<[u8; 32]>) -> String {
    let path = tmp.path();
    let s = Storage::open(&path).unwrap();
    if let Some(secret) = secret {
        assert_eq!(s.session_secret(|| Ok(secret)).unwrap(), secret);
    }
    drop(s);
    path
}

/// The rows of `session_secret`, read through a connection that cannot write.
fn stored(path: &str) -> Vec<(i64, Vec<u8>, String)> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = conn
        .prepare("SELECT id, secret, created_at FROM session_secret ORDER BY id")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
}

/// Make the database file read-only for its owner.  Returns `false` when that
/// changes nothing (the tests run as root, which can write to any file).
fn make_unwritable(path: &str) -> bool {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444)).unwrap();
    std::fs::OpenOptions::new().write(true).open(path).is_err()
}

/// The call failed because SQLite would not write.
fn assert_cannot_write(result: &Result<[u8; 32], SecretError>) {
    match result {
        Err(SecretError::Storage(e)) => {
            assert_eq!(e.sqlite_error_code(), Some(ErrorCode::ReadOnly), "{e}")
        }
        other => panic!("expected a storage error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The migration
// ---------------------------------------------------------------------------

/// Version 2 database plus the step to version 3, as it was when the
/// `malformed` and `classified` columns were added.  Frozen, like the other
/// schema fixtures: it stands in for databases that already exist.
const SCHEMA_3_CLASSIFIED: &str = "
ALTER TABLE messages ADD COLUMN malformed  TEXT;
ALTER TABLE messages ADD COLUMN classified INTEGER NOT NULL DEFAULT 0;
PRAGMA user_version = 3;
";

/// A database as an older release leaves it, at `user_version` `version`.
fn database_at(path: &str, version: i64) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(SCHEMA_V2_0_0).unwrap();
    if version >= 1 {
        conn.execute_batch(SCHEMA_V2_1_0_UPGRADE).unwrap();
    }
    if version >= 2 {
        conn.execute_batch(SCHEMA_2_LABELS).unwrap();
    }
    if version >= 3 {
        conn.execute_batch(SCHEMA_3_CLASSIFIED).unwrap();
    }
    let reached: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reached, version);
}

/// Something only the entry at that position contains, in the order the
/// entries must stay in.  A database at version `i` runs entries `i..`, so an
/// entry that moves is skipped by databases already past its new position and
/// runs twice on others.
const MIGRATION_ORDER: [(&str, &str); 4] = [
    ("0 -> 1", "ADD COLUMN gateway_eui"),
    ("1 -> 2", "CREATE TABLE device_labels"),
    ("2 -> 3", "ADD COLUMN classified"),
    ("3 -> 4", "CREATE TABLE session_secret"),
];

#[test]
fn migrations_keep_the_order_they_shipped_in() {
    assert_eq!(MIGRATIONS.len(), MIGRATION_ORDER.len());
    for (i, (step, marker)) in MIGRATION_ORDER.iter().enumerate() {
        assert!(
            MIGRATIONS[i].contains(marker),
            "MIGRATIONS[{i}] ({step}) should contain {marker:?}"
        );
        let holders = MIGRATIONS.iter().filter(|m| m.contains(marker)).count();
        assert_eq!(holders, 1, "{marker:?} should be in one entry only");
    }
}

#[test]
fn every_earlier_version_migrates_to_the_current_schema_with_an_empty_secret_table() {
    let current = MIGRATIONS.len() as i64;
    // `None` is a path with no file yet: a new installation.
    for from in [None, Some(0), Some(1), Some(2), Some(3)] {
        let tmp = TempDb::new("secret-migrate");
        let path = tmp.path();
        if let Some(version) = from {
            database_at(&path, version);
        }

        let s = Storage::open(&path).unwrap();
        let conn = s.conn.lock().unwrap();
        let what = format!("from version {from:?}");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, current, "{what}");

        // Earlier features' schema.
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(messages)")
            .unwrap()
            .query_map([], |r| r.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for column in ["gateway_eui", "f_opts_len", "malformed", "classified"] {
            assert!(columns.iter().any(|c| c == column), "{what}: {column}");
        }
        let schema_object = |kind: &str, name: &str| -> bool {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2)",
                params![kind, name],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(schema_object("table", "device_labels"), "{what}");
        assert!(schema_object("index", "messages_unclassified"), "{what}");

        // This feature's table: there, with the columns it is read by, and
        // empty until a start with the login on creates the secret.
        assert!(schema_object("table", "session_secret"), "{what}");
        let secret_columns: Vec<String> = conn
            .prepare("PRAGMA table_info(session_secret)")
            .unwrap()
            .query_map([], |r| r.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(secret_columns, ["id", "secret", "created_at"], "{what}");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_secret", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "{what}");
    }
}

#[test]
fn the_table_takes_one_row_of_exactly_32_bytes() {
    let tmp = TempDb::new("secret-check");
    let path = database(&tmp, None);
    let conn = Connection::open(&path).unwrap();
    let insert = |id: i64, secret: &[u8]| {
        conn.execute(
            "INSERT INTO session_secret (id, secret, created_at) VALUES (?1, ?2, 'x')",
            params![id, secret],
        )
    };

    for wrong in [&[][..], &[7u8; 16][..], &[7u8; 33][..]] {
        let refused = insert(1, wrong).unwrap_err();
        assert_eq!(
            refused.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation),
            "{} bytes",
            wrong.len()
        );
    }
    let refused = insert(2, &[7u8; 32]).unwrap_err();
    assert_eq!(
        refused.sqlite_error_code(),
        Some(ErrorCode::ConstraintViolation),
        "only row 1 may exist"
    );
    assert_eq!(insert(1, &[7u8; 32]).unwrap(), 1);
}

#[test]
fn the_writer_waits_five_seconds_for_a_lock() {
    // The wait for another process that is creating the secret at the same
    // moment is the connection's busy timeout.  Nothing in this crate sets
    // it; it is the library's default, and the documented wait relies on it.
    let tmp = TempDb::new("secret-busy-timeout");
    let s = Storage::open(&tmp.path()).unwrap();
    let wait: i64 = s
        .conn
        .lock()
        .unwrap()
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(wait, 5000);
}

// ---------------------------------------------------------------------------
// Creating and reading
// ---------------------------------------------------------------------------

#[test]
fn the_first_call_stores_what_fresh_gave_and_later_calls_return_it() {
    let tmp = TempDb::new("secret-create");
    let path = tmp.path();
    let calls = Arc::new(AtomicUsize::new(0));

    let s = Storage::open(&path).unwrap();
    let first = s.session_secret(counting(&calls, bytes(1))).unwrap();
    assert_eq!(first, bytes(1));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Same handle, a clone of it, and then the file opened again.
    assert_eq!(s.session_secret(never_called).unwrap(), first);
    assert_eq!(s.clone().session_secret(never_called).unwrap(), first);
    drop(s);
    let s = Storage::open(&path).unwrap();
    assert_eq!(s.session_secret(never_called).unwrap(), first);
    drop(s);

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let rows = stored(&path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, 1);
    assert_eq!(rows[0].1, first);
    let created = DateTime::parse_from_rfc3339(&rows[0].2).expect("created_at is RFC 3339");
    assert_eq!(created.offset().local_minus_utc(), 0, "UTC");
    let age = Utc::now() - created.with_timezone(&Utc);
    assert!(age < chrono::Duration::seconds(60), "{age}");
}

#[test]
fn an_in_memory_database_creates_it_once_as_well() {
    let s = Storage::open_in_memory().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let first = s.session_secret(counting(&calls, bytes(2))).unwrap();
    assert_eq!(s.session_secret(never_called).unwrap(), first);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_failing_random_source_is_an_error_and_stores_nothing() {
    let tmp = TempDb::new("secret-random");
    let path = tmp.path();
    let s = Storage::open(&path).unwrap();

    let failed = s.session_secret(|| Err(RandomError));
    assert!(matches!(failed, Err(SecretError::Random(RandomError))));
    assert!(stored(&path).is_empty());

    // The transaction was rolled back and its lock released: the next call
    // creates the secret.
    let calls = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        s.session_secret(counting(&calls, bytes(3))).unwrap(),
        bytes(3)
    );
    assert_eq!(stored(&path).len(), 1);
}

#[test]
fn errors_name_what_went_wrong_and_hold_no_secret() {
    let secret_hex = crate::core::random::hex(&bytes(4));
    let tmp = TempDb::new("secret-display");
    let s = Storage::open(&tmp.path()).unwrap();
    s.session_secret(|| Ok(bytes(4))).unwrap();
    let damaged = {
        let conn = s.conn.lock().unwrap();
        conn.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        conn.execute("UPDATE session_secret SET secret = zeroblob(16)", [])
            .unwrap();
        drop(conn);
        s.session_secret(never_called).unwrap_err()
    };

    let errors = [
        SecretError::Random(RandomError),
        SecretError::Storage(rusqlite::Error::QueryReturnedNoRows),
        damaged,
    ];
    for e in &errors {
        for text in [e.to_string(), format!("{e:?}")] {
            assert!(!text.is_empty());
            assert!(!text.contains(&secret_hex), "{text}");
        }
    }
    assert_eq!(
        errors[0].to_string(),
        RandomError.to_string(),
        "the random source's own words"
    );
    assert_eq!(
        errors[2].to_string(),
        "the stored session secret is damaged"
    );
}

// ---------------------------------------------------------------------------
// Reading without writing
// ---------------------------------------------------------------------------

#[test]
fn a_stored_secret_is_read_through_a_read_only_connection() {
    let tmp = TempDb::new("secret-ro-flag");
    let path = database(&tmp, Some(bytes(5)));
    let before = std::fs::read(&path).unwrap();

    let mut conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let secret = read_or_create_session_secret(&mut conn, never_called).unwrap();

    assert_eq!(secret, bytes(5));
    drop(conn);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the file is untouched"
    );
}

#[test]
fn a_stored_secret_is_read_from_a_file_that_is_not_writable() {
    let tmp = TempDb::new("secret-0444");
    let path = database(&tmp, Some(bytes(6)));
    if !make_unwritable(&path) {
        eprintln!("skipped: file permissions do not bind this user");
        return;
    }
    let before = std::fs::read(&path).unwrap();

    // Opened the ordinary way, as the sniffer does: SQLite falls back to
    // read-only when the file cannot be written.
    let mut conn = Connection::open(&path).unwrap();
    let secret = read_or_create_session_secret(&mut conn, never_called).unwrap();

    assert_eq!(secret, bytes(6));
    drop(conn);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the file is untouched"
    );
}

#[test]
fn a_stored_secret_is_read_while_another_connection_holds_the_write_lock() {
    // The read comes first and takes no write lock, so a start does not wait
    // behind (or fail because of) whoever is writing to the database.
    let tmp = TempDb::new("secret-read-first");
    let path = database(&tmp, Some(bytes(5)));

    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let mut conn = Connection::open(&path).unwrap();
    conn.busy_timeout(Duration::ZERO).unwrap();
    let secret = read_or_create_session_secret(&mut conn, never_called).unwrap();

    assert_eq!(secret, bytes(5));
    holder.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn no_row_in_a_read_only_connection_is_a_storage_error_and_stores_nothing() {
    let tmp = TempDb::new("secret-ro-empty");
    let path = database(&tmp, None);
    let before = std::fs::read(&path).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));

    let mut conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let result = read_or_create_session_secret(&mut conn, counting(&calls, bytes(7)));
    drop(conn);

    assert_cannot_write(&result);
    // SQLite lets a read-only connection begin the transaction and refuses
    // the INSERT, so the random bytes were asked for and are thrown away.
    assert!(calls.load(Ordering::SeqCst) <= 1);
    assert!(stored(&path).is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn no_row_in_a_file_that_is_not_writable_is_a_storage_error_and_stores_nothing() {
    let tmp = TempDb::new("secret-0444-empty");
    let path = database(&tmp, None);
    if !make_unwritable(&path) {
        eprintln!("skipped: file permissions do not bind this user");
        return;
    }
    let before = std::fs::read(&path).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));

    let mut conn = Connection::open(&path).unwrap();
    let result = read_or_create_session_secret(&mut conn, counting(&calls, bytes(7)));
    drop(conn);

    assert_cannot_write(&result);
    assert!(calls.load(Ordering::SeqCst) <= 1);
    assert!(stored(&path).is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

// ---------------------------------------------------------------------------
// A row that is not a secret
// ---------------------------------------------------------------------------

/// Store `value` (an SQL expression) as the secret, past the table's check.
fn plant(path: &str, value: &str) {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    conn.execute(
        &format!(
            "INSERT INTO session_secret (id, secret, created_at) VALUES (1, {value}, 'planted')"
        ),
        [],
    )
    .unwrap();
}

#[test]
fn a_row_that_is_not_32_bytes_is_damaged_and_is_left_alone() {
    for (label, value) in [
        ("sixteen bytes", "zeroblob(16)"),
        ("an empty blob", "zeroblob(0)"),
        // Passes the length check (32 characters) but is text, not bytes.
        (
            "text of 32 characters",
            "'abcdefghijklmnopqrstuvwxyz012345'",
        ),
    ] {
        let tmp = TempDb::new("secret-damaged");
        let path = database(&tmp, None);
        plant(&path, value);
        let calls = Arc::new(AtomicUsize::new(0));

        let s = Storage::open(&path).unwrap();
        let result = s.session_secret(counting(&calls, bytes(8)));
        assert!(matches!(result, Err(SecretError::Damaged)), "{label}");
        drop(s);

        assert_eq!(calls.load(Ordering::SeqCst), 0, "{label}: not replaced");
        let conn = Connection::open(&path).unwrap();
        let (n, planted): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(created_at) FROM session_secret",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((n, planted.as_str()), (1, "planted"), "{label}");
    }
}

#[test]
fn a_damaged_row_is_found_by_the_read_first_path_without_a_write() {
    let tmp = TempDb::new("secret-damaged-ro");
    let path = database(&tmp, None);
    plant(&path, "zeroblob(16)");

    let mut conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let result = read_or_create_session_secret(&mut conn, never_called);
    assert!(matches!(result, Err(SecretError::Damaged)), "{result:?}");
}

// ---------------------------------------------------------------------------
// Two connections at once
// ---------------------------------------------------------------------------

#[test]
fn two_connections_creating_at_once_end_up_with_one_secret() {
    // The sniffer is one process, but a second sniffer, or a tool that opens
    // the database, is another connection to the same file, and clones of one
    // `Storage` share a mutex that would hide the race.  So these are two
    // `Connection`s of their own.
    static SECOND_IS_WAITING: AtomicBool = AtomicBool::new(false);

    /// Called by SQLite each time the second connection finds the write lock
    /// taken.  It is how the test knows the second really is waiting.
    fn note_waiting(attempt: i32) -> bool {
        SECOND_IS_WAITING.store(true, Ordering::SeqCst);
        if attempt > 1000 {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
        true
    }

    let tmp = TempDb::new("secret-race");
    let path = database(&tmp, None);
    let calls = Arc::new(AtomicUsize::new(0));
    let (first_inside_tx, first_inside_rx) = mpsc::channel::<()>();
    let (second_started_tx, second_started_rx) = mpsc::channel::<()>();

    let first = thread::spawn({
        let path = path.clone();
        let fresh = counting(&calls, bytes(1));
        move || {
            let mut conn = Connection::open(&path).unwrap();
            read_or_create_session_secret(&mut conn, move || {
                // The write lock is held from here to the commit.
                first_inside_tx.send(()).unwrap();
                // Hold it until the other connection has started and is
                // waiting for it, or the wait runs out.
                second_started_rx
                    .recv_timeout(WAIT)
                    .map_err(|_| RandomError)?;
                let give_up = Instant::now() + WAIT;
                while !SECOND_IS_WAITING.load(Ordering::SeqCst) && Instant::now() < give_up {
                    thread::sleep(Duration::from_millis(5));
                }
                fresh()
            })
        }
    });
    first_inside_rx
        .recv_timeout(WAIT)
        .expect("the first connection should reach its transaction");

    let second = thread::spawn({
        let path = path.clone();
        let fresh = counting(&calls, bytes(2));
        move || {
            let mut conn = Connection::open(&path).unwrap();
            conn.busy_handler(Some(note_waiting)).unwrap();
            second_started_tx.send(()).unwrap();
            read_or_create_session_secret(&mut conn, fresh)
        }
    });

    let first = first.join().unwrap().unwrap();
    let second = second.join().unwrap().unwrap();

    assert!(
        SECOND_IS_WAITING.load(Ordering::SeqCst),
        "the second connection never waited for the lock, so nothing was raced"
    );
    assert_eq!(first, bytes(1));
    assert_eq!(second, first, "the second reads what the first stored");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "random bytes asked for once"
    );
    let rows = stored(&path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, first);
}

#[test]
fn a_connection_that_waits_past_its_busy_timeout_fails_and_stores_nothing() {
    let tmp = TempDb::new("secret-busy");
    let path = database(&tmp, None);
    let (first_inside_tx, first_inside_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));

    let first = thread::spawn({
        let path = path.clone();
        let fresh = counting(&first_calls, bytes(1));
        move || {
            let mut conn = Connection::open(&path).unwrap();
            read_or_create_session_secret(&mut conn, move || {
                first_inside_tx.send(()).unwrap();
                // Holds the write lock until the test lets go, or the wait
                // runs out.
                release_rx.recv_timeout(WAIT).map_err(|_| RandomError)?;
                fresh()
            })
        }
    });
    first_inside_rx
        .recv_timeout(WAIT)
        .expect("the first connection should reach its transaction");

    // The second connection is short of patience for this test.
    let mut conn = Connection::open(&path).unwrap();
    conn.busy_timeout(Duration::from_millis(200)).unwrap();
    let started = Instant::now();
    let result = read_or_create_session_secret(&mut conn, counting(&second_calls, bytes(2)));
    let waited = started.elapsed();
    release_tx.send(()).unwrap();

    match &result {
        Err(SecretError::Storage(e)) => {
            assert_eq!(e.sqlite_error_code(), Some(ErrorCode::DatabaseBusy), "{e}")
        }
        other => panic!("expected a busy storage error, got {other:?}"),
    }
    assert!(waited >= Duration::from_millis(150), "{waited:?}");
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);

    // The first connection was unharmed, and what is stored is its secret.
    assert_eq!(first.join().unwrap().unwrap(), bytes(1));
    let rows = stored(&path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, bytes(1));
    // The second, once the lock is free, reads it.
    assert_eq!(
        read_or_create_session_secret(&mut conn, never_called).unwrap(),
        bytes(1)
    );
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Helpers for the rollback script
// ---------------------------------------------------------------------------
//
// The script runs the previous release on a database this build made and
// then asks this build to read it back.  These two tests do the making and
// the reading, so the schema is exactly this build's.  They print the
// SHA-256 of the secret, never the secret.

/// How many messages the database starts with.
const FIXTURE_MESSAGES: usize = 20;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn sha256_hex(secret: &[u8; 32]) -> String {
    crate::core::random::hex(ring::digest::digest(&ring::digest::SHA256, secret).as_ref())
}

fn records_file(db: &str) -> String {
    format!("{db}.records.json")
}

#[test]
#[ignore = "run by the rollback script, which sets ROLLBACK_DB"]
fn rollback_fixture_write() {
    use crate::core::random::{random_bytes, SystemRandomSource};

    let path = env("ROLLBACK_DB");
    let s = Storage::open(&path).unwrap();
    let secret = s
        .session_secret(|| random_bytes::<32>(&SystemRandomSource::new()))
        .unwrap();

    let now = Utc::now();
    let mut records = Vec::new();
    for i in 0..FIXTURE_MESSAGES {
        let mut r = sample_record();
        r.id = format!("before-{i:02}");
        r.timestamp = now - chrono::Duration::seconds((FIXTURE_MESSAGES - i) as i64);
        r.dev_addr = Some(format!("0100{:04X}", i % 4));
        r.fcnt = Some(i as u32);
        s.insert(&r).unwrap();
        records.push(serde_json::to_value(&r).unwrap());
    }
    std::fs::write(
        records_file(&path),
        serde_json::to_string(&records).unwrap(),
    )
    .unwrap();

    println!("secret_sha256={}", sha256_hex(&secret));
    println!("messages={}", s.count().unwrap());
}

#[test]
#[ignore = "run by the rollback script, which sets ROLLBACK_DB, SECRET_SHA256, TOTAL_MESSAGES and LABEL"]
fn rollback_fixture_check() {
    let path = env("ROLLBACK_DB");
    let expected_secret = env("SECRET_SHA256");
    let expected_total: usize = env("TOTAL_MESSAGES").parse().unwrap();
    // "DEVICE=label": the label the previous release was asked to set.
    let label = env("LABEL");
    let (device, text) = label.split_once('=').expect("LABEL is DEVICE=text");

    let s = Storage::open(&path).unwrap();

    let secret = s.session_secret(never_called).unwrap();
    let actual = sha256_hex(&secret);
    assert_eq!(actual, expected_secret, "the stored secret changed");
    println!("secret_sha256={actual}");

    let page = s
        .query_history(
            &HistoryQuery {
                lower: None,
                upper: None,
                filter: Default::default(),
                limit: MAX_HISTORY_ROWS,
                newest: true,
                count: true,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert_eq!(
        page.records.len(),
        expected_total,
        "every message reads back"
    );
    assert_eq!(page.total, Some(expected_total as u64));

    let by_id: std::collections::BTreeMap<_, _> = page
        .records
        .iter()
        .map(|r| (r.id.clone(), serde_json::to_value(r).unwrap()))
        .collect();
    let originals: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(records_file(&path)).unwrap()).unwrap();
    assert_eq!(originals.len(), FIXTURE_MESSAGES);
    for original in &originals {
        let id = original["id"].as_str().unwrap();
        assert_eq!(by_id.get(id), Some(original), "{id} changed");
    }
    println!(
        "messages={} (the {} written before the rollback are unchanged, {} written by the previous release)",
        page.records.len(),
        FIXTURE_MESSAGES,
        page.records.len() - FIXTURE_MESSAGES
    );

    assert_eq!(
        s.labels().unwrap().get(device).map(String::as_str),
        Some(text),
        "the label set by the previous release reads back"
    );
    println!("label {device}={text}");
}
