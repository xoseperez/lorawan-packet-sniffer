//! SQLite persistence layer for captured LoRaWAN messages.
//!
//! ## Design
//!
//! `Storage` holds up to two `rusqlite::Connection`s, each behind an
//! `Arc<Mutex<>>`, so it can be cheaply cloned and shared across tokio tasks
//! via `tokio::task::spawn_blocking`.  The writer connection saves records,
//! labels and the retention sweep; the optional reader connection answers
//! history queries (see "History reads" below).
//!
//! All timestamps are persisted with `DateTime::to_rfc3339()`: UTC written as
//! `+00:00`, followed by 0, 3, 6 or 9 fraction digits, whichever is the least
//! that holds the value (`2026-02-23T10:30:00+00:00`,
//! `2026-02-23T10:30:00.123+00:00`).  The text is understood by SQLite's
//! built-in date functions, and it sorts as time does: the fields are
//! fixed-width and most significant first, and the `+` that ends the time of
//! day (0x2B) sorts below `.` and the digits, so a whole second comes before
//! its fractions, and a shorter fraction before a longer one that extends it
//! (`…:00.5+00:00` before `…:00.500001+00:00`).  Time bounds are rendered with
//! the same function, so a bound compares correctly with every stored form.
//!
//! ## Schema versions
//!
//! The database carries its schema version in SQLite's `PRAGMA user_version`.
//! [`INIT_SQL`] creates the original (version 0) schema, and each entry in
//! [`MIGRATIONS`] moves it up one version.  New databases and databases
//! written by older releases both take that same path, so there is only one
//! way a schema can come about.
//!
//! - version 0: the `messages` table and its indexes ([`INIT_SQL`]);
//! - version 1: gateway, join and frame-control columns on `messages`;
//! - version 2: the `device_labels` table, one operator-chosen label per
//!   device identifier;
//! - version 3: the `malformed` and `classified` columns on `messages`.
//!
//! ## Malformed frames
//!
//! The decoder marks a frame that cannot be a valid LoRaWAN frame and leaves
//! its identifiers empty, so a record this version stores already says what it
//! is.  Rows written before that, or by an older version run on the same
//! database after a rollback, have no verdict yet.  Every row therefore
//! carries `classified`: this version writes 1, an older version's `INSERT`
//! does not list the column and so gets the default 0.  On every start,
//! [`Storage::open`] finds the rows with `classified = 0`, marks the ones that
//! fail the CRC or length rule from their stored fields, clears what was read
//! from their bytes, and sets the flag.  A flag in each row, rather than a
//! "checked up to here" mark, survives whatever happens to the table in
//! between: SQLite reuses the row numbers of deleted rows, and `VACUUM` may
//! renumber the rest.
//!
//! The first pass after the upgrade visits every row, since each one gets the
//! flag.  It ends by creating a partial index over `classified = 0`, which is
//! empty at that moment; from then on a start counts the unchecked rows
//! through that index and returns at once when there are none.  A pass that
//! fails (a full disk, say) rolls back whole and is repeated at the next
//! start; it never stops the sniffer from opening the database.
//!
//! ## Device labels
//!
//! A label is keyed by the same text that message records carry in
//! `dev_addr` or `dev_eui`, so the two kinds of identifier can never collide
//! (8 against 16 hex digits).  Callers pass identifiers already normalised by
//! [`crate::core::labels::parse_device_id`]; this module compares them
//! verbatim.
//!
//! A label lives as long as its device has a stored message, or, failing
//! that, as long as the rule given to [`OrphanPolicy::KeepIf`] says the
//! device is still around.  [`Storage::delete_older_than`] applies that rule in the same
//! transaction that expires the messages, so a label is never judged against
//! a half-pruned table.
//!
//! ## History reads
//!
//! [`Storage::query_history`] can scan a large part of the table: a device
//! text cannot use an index, and on a million rows that is hundreds of
//! milliseconds.  Run on the writer's connection it would hold that
//! connection's mutex for all that time and stall every insert behind it.
//!
//! [`Storage::open`] therefore opens a second, read-only connection, but only
//! once `PRAGMA journal_mode` reads back `wal`.  In WAL mode a reader sees a
//! consistent snapshot and neither blocks the writer's commits nor is blocked
//! by them.  Outside WAL a reader would block every commit, and a scan on the
//! writer would block every insert; both are worse than refusing the request.
//! So without WAL there is no reader, a request with a row filter fails with
//! [`SnifferError::HistoryUnavailable`], and a time-only request (an index
//! range and at most [`MAX_HISTORY_ROWS`] rows) still runs on the writer, as
//! every read did before.  An in-memory database, used by tests, keeps
//! everything on its one connection: a second `:memory:` connection would be
//! a different, empty database.
//!
//! A query runs in one deferred read transaction, so the labels, the counts
//! and the rows it reads belong to one snapshot, and it can be cancelled: a
//! progress handler looks at a flag every few thousand VM steps and
//! interrupts the statement, and a guard removes the handler on every way out
//! so the next query on the connection is not affected.
//!
//! The reader is declared before the writer in [`Storage`], so it closes
//! first.  The last connection to close checkpoints the log and deletes the
//! `-wal` and `-shm` files, and a read-only connection cannot checkpoint:
//! closing it last would leave both files behind.
//!
//! ## Concurrency note
//!
//! `rusqlite::Connection` is `Send` but not `Sync`.  `Arc<Mutex<Connection>>`
//! is both `Send` and `Sync`, so `Storage` can be cloned freely and passed
//! into `spawn_blocking` closures.

use std::collections::BTreeMap;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use log::{warn, Level};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{params, Connection, ErrorCode, OpenFlags, ToSql, TransactionBehavior};

use crate::core::error::SnifferError;
use crate::core::history::{
    devices_with_matching_label, fold_case, HistoryPage, HistoryQuery, LowerBound, MAX_HISTORY_ROWS,
};
use crate::core::metadata::{
    CrcStatus, Direction, MType, Malformed, MessageRecord, Protocol, Status,
};

const SELECT_COLS: &str = "
    id, timestamp, direction, status, blocked_by, source_protocol,
    freq_mhz, datarate, rssi, snr, crc_status,
    mtype, dev_addr, dev_eui, fcnt, fport, payload_size, toa_ms,
    gateway_eui, join_eui, dev_nonce,
    adr, adr_ack_req, ack, class_b, f_pending, f_opts_len,
    malformed
";

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// The size the write-ahead log file is truncated back to after SQLite
/// restarts it (`PRAGMA journal_size_limit`, on the writer).
const WAL_SIZE_LIMIT_BYTES: i64 = 8 * 1024 * 1024;

/// The schema as shipped in v2.0.0, before versioned migrations existed.
///
/// Do not add columns here: later changes go in [`MIGRATIONS`] so that
/// databases created by earlier releases are upgraded the same way as new ones.
const INIT_SQL: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS messages (
    id              TEXT    NOT NULL PRIMARY KEY,
    timestamp       TEXT    NOT NULL,
    direction       TEXT    NOT NULL,
    status          TEXT    NOT NULL,
    blocked_by      TEXT,
    source_protocol TEXT    NOT NULL,
    freq_mhz        REAL    NOT NULL,
    datarate        TEXT    NOT NULL,
    rssi            INTEGER,
    snr             REAL,
    crc_status      TEXT,
    mtype           TEXT    NOT NULL,
    dev_addr        TEXT,
    dev_eui         TEXT,
    fcnt            INTEGER,
    fport           INTEGER,
    payload_size    INTEGER NOT NULL,
    toa_ms          REAL
);

CREATE INDEX IF NOT EXISTS idx_messages_timestamp
    ON messages (timestamp);

CREATE INDEX IF NOT EXISTS idx_messages_dev_addr
    ON messages (dev_addr)
    WHERE dev_addr IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_messages_dev_eui
    ON messages (dev_eui)
    WHERE dev_eui IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_messages_mtype
    ON messages (mtype);
";

/// Schema migrations, applied in order on top of [`INIT_SQL`].
///
/// Entry `i` upgrades a database from `user_version` `i` to `i + 1`.  Never
/// edit or reorder an entry that has shipped; append a new one instead.
///
/// SQLite adds a column with a constant default without rewriting the table,
/// so upgrading a large database is fast.  Booleans are stored as INTEGER 0/1.
/// A new table does not touch `messages` at all.
const MIGRATIONS: &[&str] = &[
    // 0 -> 1: gateway, join and frame-control fields.
    //
    // There is deliberately no index on gateway_eui: nothing queries by
    // gateway in SQL, and building one would turn the upgrade into a full
    // table scan.
    "ALTER TABLE messages ADD COLUMN gateway_eui TEXT;
     ALTER TABLE messages ADD COLUMN join_eui    TEXT;
     ALTER TABLE messages ADD COLUMN dev_nonce   INTEGER;
     ALTER TABLE messages ADD COLUMN adr         INTEGER;
     ALTER TABLE messages ADD COLUMN adr_ack_req INTEGER;
     ALTER TABLE messages ADD COLUMN ack         INTEGER;
     ALTER TABLE messages ADD COLUMN class_b     INTEGER;
     ALTER TABLE messages ADD COLUMN f_pending   INTEGER;
     ALTER TABLE messages ADD COLUMN f_opts_len  INTEGER;",
    // 1 -> 2: operator-chosen device labels.
    //
    // `device` is a DevAddr or a DevEUI exactly as message records spell it.
    // There is no foreign key to `messages`: a label may outlive every stored
    // message of its device (see `Storage::delete_older_than`).
    "CREATE TABLE device_labels (
         device     TEXT PRIMARY KEY,
         label      TEXT NOT NULL,
         updated_at TEXT NOT NULL
     );",
    // 2 -> 3: malformed-frame verdict and the "checked" flag.
    //
    // Only DDL, so it is instant at any size.  Rows already stored read
    // `classified = 0` without being rewritten, and so do the rows an older
    // version stores later, because its `INSERT` lists only its own columns.
    // That is how those rows are found again: see `classify_unchecked`.  The
    // partial index over the flag is not created here, for the reason given
    // on `UNCLASSIFIED_INDEX_SQL`.
    "ALTER TABLE messages ADD COLUMN malformed  TEXT;
     ALTER TABLE messages ADD COLUMN classified INTEGER NOT NULL DEFAULT 0;",
];

const INSERT_SQL: &str = "
INSERT OR REPLACE INTO messages (
    id, timestamp, direction, status, blocked_by, source_protocol,
    freq_mhz, datarate, rssi, snr, crc_status,
    mtype, dev_addr, dev_eui, fcnt, fport, payload_size, toa_ms,
    gateway_eui, join_eui, dev_nonce,
    adr, adr_ack_req, ack, class_b, f_pending, f_opts_len,
    malformed, classified
) VALUES (
    ?1,  ?2,  ?3,  ?4,  ?5,  ?6,
    ?7,  ?8,  ?9,  ?10, ?11,
    ?12, ?13, ?14, ?15, ?16, ?17, ?18,
    ?19, ?20, ?21,
    ?22, ?23, ?24, ?25, ?26, ?27,
    ?28, 1
)";

/// Mark the stored rows that cannot be LoRaWAN frames and clear what was read
/// from their bytes.
///
/// The `WHERE` clause is the stored-fields form of the decoder's rules
/// (`decoder::crc_failed` and `decoder::length_fits`); the test that compares
/// the two keeps them together, so change both or neither.  The first byte of
/// a frame is not stored, so the MHDR rule cannot be applied, and a row
/// written by 2.0.0 has no `f_opts_len`, so only the 12-byte floor applies to
/// it.  The reason is a `CASE` on `crc_status`, which the `SET` clause reads
/// before it changes anything.
const CLASSIFY_MALFORMED_SQL: &str = "
UPDATE messages
   SET malformed   = CASE WHEN crc_status = 'fail' THEN 'crc' ELSE 'length' END,
       classified  = 1,
       dev_addr    = NULL, dev_eui     = NULL, join_eui = NULL, dev_nonce = NULL,
       fcnt        = NULL, fport       = NULL,
       adr         = NULL, adr_ack_req = NULL, ack      = NULL, class_b   = NULL,
       f_pending   = NULL, f_opts_len  = NULL
 WHERE classified = 0
   AND (crc_status = 'fail'
        OR (mtype = 'JoinRequest'   AND payload_size <> 23)
        OR (mtype = 'JoinAccept'    AND payload_size NOT IN (17, 33))
        OR (mtype = 'RejoinRequest' AND payload_size NOT IN (19, 24))
        OR (mtype IN ('UnconfirmedDataUp', 'UnconfirmedDataDown',
                      'ConfirmedDataUp',   'ConfirmedDataDown')
            AND payload_size < 12 + COALESCE(f_opts_len, 0)))";

/// Everything left unchecked after [`CLASSIFY_MALFORMED_SQL`] is a valid frame.
///
/// Kept apart from the first statement on purpose: one `CASE` update would
/// repeat the malformed rule in the reason and in each of the twelve cleared
/// columns, and the agreement test would have to keep all of those copies in
/// step.
const CLASSIFY_REST_SQL: &str = "UPDATE messages SET classified = 1 WHERE classified = 0";

/// Partial index that finds the unchecked rows without reading the table.
///
/// It is created at the end of the first successful pass, when every row has
/// just been set to 1 and the index therefore has no entries to write.
/// Creating it in the migration would index every existing row (all of them
/// 0 at that point) only to remove them all again a moment later.  After that
/// it holds only the rows an older version stores, and SQLite keeps it up to
/// date for every writer, older versions included.
const UNCLASSIFIED_INDEX_SQL: &str = "
CREATE INDEX IF NOT EXISTS messages_unclassified ON messages (classified)
    WHERE classified = 0";

/// Is the partial index over the unchecked rows there?  It is created by the
/// first successful pass, so its absence means no pass has completed yet.
const UNCLASSIFIED_INDEX_EXISTS_SQL: &str =
    "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'messages_unclassified')";

/// How many rows are still unchecked; answered from the partial index.
const COUNT_UNCLASSIFIED_SQL: &str = "SELECT COUNT(*) FROM messages WHERE classified = 0";

/// Labels whose device has no stored message.
///
/// Two separate `NOT EXISTS` probes, one per identifier column, because each
/// can use its own partial index (`idx_messages_dev_addr`,
/// `idx_messages_dev_eui`).  Written as `dev_addr = x OR dev_eui = x` the
/// planner may fall back to scanning `messages`, which on a large database
/// would hold the storage lock for seconds.
const ORPHAN_LABELS_SQL: &str = "
SELECT device FROM device_labels
WHERE NOT EXISTS (SELECT 1 FROM messages WHERE dev_addr = device_labels.device)
  AND NOT EXISTS (SELECT 1 FROM messages WHERE dev_eui  = device_labels.device)";

/// Does the device have a stored message as a DevAddr?
const HAS_DEV_ADDR_SQL: &str = "SELECT EXISTS (SELECT 1 FROM messages WHERE dev_addr = ?1)";

/// Does the device have a stored message as a DevEUI?
const HAS_DEV_EUI_SQL: &str = "SELECT EXISTS (SELECT 1 FROM messages WHERE dev_eui = ?1)";

/// Insert a label, or replace the existing one for the same device.
const UPSERT_LABEL_SQL: &str = "
INSERT INTO device_labels (device, label, updated_at) VALUES (?1, ?2, ?3)
ON CONFLICT(device) DO UPDATE SET label = excluded.label, updated_at = excluded.updated_at";

// ---------------------------------------------------------------------------
// History SQL
// ---------------------------------------------------------------------------
//
// Every statement below is assembled from these fragments and none has a
// value formatted into its text: values are always bound.

/// How long the history connection waits for a lock before the query fails
/// with [`SnifferError::Busy`].
///
/// rusqlite's default is five seconds, and SQLite's progress handler, which is
/// what cancels a query, does not run while a busy handler waits.  A second is
/// the longest a cancelled request may be kept in a lock wait.
const READER_BUSY_TIMEOUT: Duration = Duration::from_secs(1);

/// SQLite virtual-machine steps between two looks at a query's cancel flag.
const CANCEL_CHECK_STEPS: i32 = 10_000;

/// Every range query names this index.  Left to choose, the planner picks the
/// message-type index plus a temporary B-tree sort for a type filter, which
/// turns into a sort of most of the table for a common type.
const RANGE_FROM: &str = "FROM messages INDEXED BY idx_messages_timestamp";

const AFTER_SQL: &str = "timestamp > :bound";
const FROM_SQL: &str = "timestamp >= :bound";

/// Device text: a substring of the DevAddr or the DevEUI, or a label match.
///
/// `instr` takes its needle literally, where `LIKE` would give `%` and `_` a
/// meaning.  `lower` is ASCII-only, which is all a hex identifier needs, and
/// the needle arrives already folded by [`fold_case`].  The label belongs to
/// the record's identifier: the DevAddr, or the DevEUI when there is none.
const DEVICE_SQL: &str = "(instr(lower(dev_addr), :needle) > 0 \
     OR instr(lower(dev_eui), :needle) > 0 \
     OR (dev_addr IS NOT NULL AND dev_addr IN (SELECT value FROM json_each(:ids))) \
     OR (dev_addr IS NULL AND dev_eui IN (SELECT value FROM json_each(:ids))))";

const MTYPE_SQL: &str = "mtype IN (SELECT value FROM json_each(:mtypes))";
const STATUS_SQL: &str = "status = :status";
const GATEWAY_SQL: &str = "gateway_eui = :gateway";
const MALFORMED_SQL: &str = "malformed IS NOT NULL";

/// The rows a filtered scan chose, read in full, oldest first.
///
/// The scan reads only `rowid` and the filter columns: selecting all the
/// columns of a million matches took several times as long.  Ties in the
/// timestamp are ordered by `rowid`, as the index orders them.
fn fetch_by_rowid_sql() -> String {
    format!(
        "SELECT {SELECT_COLS} FROM messages \
         WHERE rowid IN (SELECT value FROM json_each(?1)) \
         ORDER BY timestamp ASC, rowid ASC"
    )
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Outcome of [`Storage::set_label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelWrite {
    /// The label was saved (new, or replacing the previous one).
    Set,
    /// The device has no stored message and was not seen live, so there is
    /// nothing to attach a label to.  Nothing was written.
    UnknownDevice,
    /// The device has no label yet and the table already holds the maximum
    /// number.  Nothing was written.  Replacing an existing label never
    /// returns this.
    LimitReached,
}

/// What [`Storage::delete_older_than`] does with labels whose device has no
/// stored message left.
pub enum OrphanPolicy<'a> {
    /// Leave every label alone.
    Keep,
    /// Keep an orphaned label when the predicate returns `true` for its
    /// device, and delete it otherwise.
    ///
    /// The predicate runs while the storage lock and a write transaction are
    /// held, once per orphaned label (at most the label limit).  It must be
    /// quick and must not call back into [`Storage`], which would deadlock.
    KeepIf(&'a dyn Fn(&str) -> bool),
}

/// What a [`Storage::delete_older_than`] sweep removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pruned {
    /// Number of expired messages deleted.
    pub messages: usize,
    /// Devices whose label was deleted, in no particular order.
    pub labels: Vec<String>,
}

/// Thread-safe, cheaply-cloneable SQLite storage handle.
#[derive(Clone)]
pub struct Storage {
    // Field order is drop order, and `reader` must close before `conn`.  The
    // last connection to close checkpoints the write-ahead log and deletes
    // the `-wal` and `-shm` files; a read-only connection cannot checkpoint,
    // so if it closed last it would leave both behind.  The clones of a
    // `Storage` share these `Arc`s, so the same order holds for whichever
    // clone is dropped last.
    /// The read-only connection for history queries; `None` unless the
    /// database is in WAL mode.
    reader: Option<Arc<Mutex<Connection>>>,
    /// The connection that saves records.
    conn: Arc<Mutex<Connection>>,
    /// Row-filtered history queries may run on `conn`.  True only for an
    /// in-memory database, which cannot have a second connection.
    filters_on_writer: bool,
    /// What the history queries of this handle did, for the tests.
    #[cfg(test)]
    probe: Arc<QueryProbe>,
}

impl Storage {
    /// Open (or create) the database at `path`, applying WAL mode and schema.
    ///
    /// Parent directories are created automatically.
    ///
    /// Rows stored before malformed-frame detection existed, and rows an older
    /// version stored since, are checked here before this returns; see the
    /// module documentation.  A failed check is logged and does not fail the
    /// open.
    ///
    /// Once the schema is in place the journal mode is read back.  Only in WAL
    /// mode is the read-only connection for history queries opened; in any
    /// other mode a warning is logged and queries with a row filter are
    /// refused (see the module documentation).
    pub fn open(path: &str) -> Result<Self, SnifferError> {
        Self::open_file(path, |_| {})
    }

    /// [`Storage::open`], with a chance to configure the connection first.
    ///
    /// The tests use it to set `max_page_count` so that the first allocation
    /// of a new page fails the way a full disk would.
    #[cfg(test)]
    fn open_with(path: &str, before_init: impl FnOnce(&Connection)) -> Result<Self, SnifferError> {
        Self::open_file(path, before_init)
    }

    /// [`Storage::open`] on a database that ends up outside WAL mode, as one
    /// on a filesystem without shared memory does.
    #[cfg(test)]
    pub(crate) fn open_without_wal(path: &str) -> Result<Self, SnifferError> {
        let mut conn = connect(path)?;
        init(&mut conn)?;
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        Ok(Self::with_reader(path, conn))
    }

    fn open_file(path: &str, before_init: impl FnOnce(&Connection)) -> Result<Self, SnifferError> {
        let mut conn = connect(path)?;
        before_init(&conn);
        init(&mut conn)?;
        // While history queries run back to back there is always a reader, so
        // SQLite cannot restart the write-ahead log and the file keeps
        // growing (about 11 MiB/s under a minute of sustained queries on a
        // 1,000,000-row database).  The log restarts once the readers are
        // gone, but the file keeps its size.  This limit lets the writer
        // truncate it back after a restart; lock waits and write times were
        // unchanged with it in the large-database measurement.
        conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT_BYTES)?;
        Ok(Self::with_reader(path, conn))
    }

    /// Complete a handle around the writer `conn`, opening the reader if the
    /// database allows one.
    fn with_reader(path: &str, conn: Connection) -> Self {
        Self {
            reader: open_reader(path, &conn).map(|r| Arc::new(Mutex::new(r))),
            conn: Arc::new(Mutex::new(conn)),
            filters_on_writer: false,
            #[cfg(test)]
            probe: Arc::default(),
        }
    }

    /// Open an in-memory database (useful for tests).
    ///
    /// It has no reader connection: a second `:memory:` connection would be a
    /// different, empty database.  History queries run on its one connection.
    pub fn open_in_memory() -> Result<Self, SnifferError> {
        let mut conn = Connection::open_in_memory()?;
        init(&mut conn)?;
        Ok(Self {
            reader: None,
            conn: Arc::new(Mutex::new(conn)),
            filters_on_writer: true,
            #[cfg(test)]
            probe: Arc::default(),
        })
    }

    /// A handle on the same writer that answers every history query on it, as
    /// an in-memory database does.  The large-database test uses it to show
    /// what the reader connection saves the writer from.
    #[cfg(test)]
    fn on_writer_only(&self) -> Self {
        Self {
            reader: None,
            conn: Arc::clone(&self.conn),
            filters_on_writer: true,
            probe: Arc::clone(&self.probe),
        }
    }

    /// Persist a `MessageRecord` to the database.
    ///
    /// Uses `INSERT OR REPLACE` so duplicate IDs are handled gracefully.
    pub fn insert(&self, rec: &MessageRecord) -> Result<(), SnifferError> {
        let conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
        insert_record(&conn, rec)?;
        Ok(())
    }

    /// Delete all messages with a timestamp older than `cutoff`, then prune
    /// orphaned labels according to `orphans`.
    ///
    /// Everything happens in one transaction, so a label is judged against
    /// the table as it is after the expired messages are gone: a device whose
    /// last message just expired counts as an orphan in this same sweep.
    ///
    /// The label step reads at most [`crate::core::labels::MAX_LABELS`] rows
    /// and probes two indexes for each, so the time it adds to the sweep is
    /// bounded no matter how large `messages` is.  The set of devices a
    /// predicate accepts is never turned into SQL: candidates come out of the
    /// database and are filtered in Rust.
    pub fn delete_older_than(
        &self,
        cutoff: DateTime<Utc>,
        orphans: OrphanPolicy<'_>,
    ) -> Result<Pruned, SnifferError> {
        let mut conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let messages = tx.execute(
            "DELETE FROM messages WHERE timestamp < ?1",
            params![cutoff.to_rfc3339()],
        )?;

        let mut labels = Vec::new();
        if let OrphanPolicy::KeepIf(keep) = orphans {
            let candidates = tx
                .prepare(ORPHAN_LABELS_SQL)?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut delete = tx.prepare("DELETE FROM device_labels WHERE device = ?1")?;
            for device in candidates {
                if !keep(&device) {
                    delete.execute(params![device])?;
                    labels.push(device);
                }
            }
        }
        tx.commit()?;
        Ok(Pruned { messages, labels })
    }

    /// Return every label, keyed by device identifier.
    pub fn labels(&self) -> Result<BTreeMap<String, String>, SnifferError> {
        let conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
        let mut stmt = conn.prepare("SELECT device, label FROM device_labels")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<Result<BTreeMap<_, _>, _>>()?)
    }

    /// Set or replace the label of `device`.
    ///
    /// `device` and `label` must already be normalised and validated (see
    /// [`crate::core::labels`]).  `seen_live` says whether the caller has seen
    /// the device in live traffic; a device that was not seen live must have a
    /// stored message, otherwise nothing is written and
    /// [`LabelWrite::UnknownDevice`] is returned.
    ///
    /// A device without a label is refused with [`LabelWrite::LimitReached`]
    /// once the table holds `max_labels` rows.  Production code passes
    /// [`crate::core::labels::MAX_LABELS`]; the parameter exists so tests can
    /// reach the limit with a handful of rows.
    ///
    /// The checks and the write share one transaction, so two callers racing
    /// for the last free slot cannot both succeed.
    pub fn set_label(
        &self,
        device: &str,
        label: &str,
        seen_live: bool,
        max_labels: usize,
    ) -> Result<LabelWrite, SnifferError> {
        let mut conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Dropping `tx` on an early return rolls back; nothing was written yet.
        if !seen_live && !has_stored_record(&tx, device)? {
            return Ok(LabelWrite::UnknownDevice);
        }

        let has_label: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM device_labels WHERE device = ?1)",
            params![device],
            |r| r.get(0),
        )?;
        if !has_label {
            let count: i64 =
                tx.query_row("SELECT COUNT(*) FROM device_labels", [], |r| r.get(0))?;
            if count as usize >= max_labels {
                return Ok(LabelWrite::LimitReached);
            }
        }

        tx.execute(
            UPSERT_LABEL_SQL,
            params![device, label, Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(LabelWrite::Set)
    }

    /// Remove the label of `device`.
    ///
    /// Returns `true` if a label was removed and `false` if there was none,
    /// which is not an error.
    pub fn delete_label(&self, device: &str) -> Result<bool, SnifferError> {
        let conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
        let n = conn.execute(
            "DELETE FROM device_labels WHERE device = ?1",
            params![device],
        )?;
        Ok(n > 0)
    }

    /// Return the total number of rows in the messages table.
    pub fn count(&self) -> Result<i64, SnifferError> {
        let conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
        let n = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        Ok(n)
    }

    /// Answer a history request: the records that match `q`, oldest first, and
    /// on request how many match.
    ///
    /// `cancel` is looked at before the query starts and then every few
    /// thousand steps of it.  Once it is set the query stops and this returns
    /// [`SnifferError::Cancelled`]; whoever set it is no longer waiting for the
    /// answer.
    ///
    /// Which connection answers depends on the request:
    /// - the reader, when there is one;
    /// - otherwise the writer, for a request with only a time bound (an index
    ///   range and at most [`MAX_HISTORY_ROWS`] rows), or for any request on
    ///   an in-memory database;
    /// - otherwise none: [`SnifferError::HistoryUnavailable`], because the
    ///   scan would stall the writer.
    ///
    /// A lock held past one second on the reader gives
    /// [`SnifferError::Busy`].  So the time between setting `cancel` and the
    /// return is one progress tick, or up to that second when the query is
    /// waiting for a lock.
    ///
    /// Two shapes, both in one read transaction, so the labels, the counts and
    /// the rows come from one snapshot:
    ///
    /// - **Time bound only.**  The count is an index count, the rows are the
    ///   first `limit` of the index range.  The malformed count is a range
    ///   scan that reads the table, so it is computed only where a scan may
    ///   run, and is `None` otherwise.
    /// - **Row filters.**  One scan along the timestamp index reads `rowid`
    ///   and the columns the filters need, counts every match and keeps the
    ///   first `limit` row numbers; only those rows are then read in full.
    ///   Without a count the scan stops once it has `limit` rows.
    pub fn query_history(
        &self,
        q: &HistoryQuery,
        cancel: Arc<AtomicBool>,
    ) -> Result<HistoryPage, SnifferError> {
        #[cfg(test)]
        let _probe = self.probe.enter();
        let (handle, scans_allowed) = match &self.reader {
            Some(reader) => (reader, true),
            None if self.filters_on_writer => (&self.conn, true),
            None if !q.filter.has_row_filter() => (&self.conn, false),
            None => return Err(SnifferError::HistoryUnavailable),
        };
        let conn = handle.lock().map_err(|_| SnifferError::LockPoisoned)?;
        let result =
            run_history(&conn, q, &cancel, scans_allowed).map_err(|e| history_error(e, &cancel));
        #[cfg(test)]
        if matches!(result, Err(SnifferError::Cancelled)) {
            self.probe.cancelled.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
}

/// What the history queries of a [`Storage`] did, for tests that need to know
/// whether two of them ever overlapped.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct QueryProbe {
    /// Queries inside `query_history` right now, including one that is
    /// waiting for the connection.
    running: AtomicUsize,
    /// The most that was ever running at once.
    most_running: AtomicUsize,
    /// Queries that went in.
    started: AtomicUsize,
    /// Queries that came out as cancelled.
    cancelled: AtomicUsize,
}

#[cfg(test)]
impl QueryProbe {
    fn enter(&self) -> ProbeGuard<'_> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_running.fetch_max(now, Ordering::SeqCst);
        ProbeGuard(self)
    }

    pub(crate) fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

    pub(crate) fn most_running(&self) -> usize {
        self.most_running.load(Ordering::SeqCst)
    }

    pub(crate) fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    pub(crate) fn cancelled(&self) -> usize {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
struct ProbeGuard<'a>(&'a QueryProbe);

#[cfg(test)]
impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Hooks for the tests of other modules.
#[cfg(test)]
impl Storage {
    /// Hold the lock the record writer needs, so that its next insert waits
    /// for the guard to be dropped.  Reads on the history connection do not
    /// need that lock, which is the point of having one.
    pub(crate) fn hold_writer(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    /// Save `records` in one transaction, which is how a test loads thousands.
    pub(crate) fn insert_many(
        &self,
        records: impl IntoIterator<Item = MessageRecord>,
    ) -> Result<(), SnifferError> {
        let mut conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
        let tx = conn.transaction()?;
        for record in records {
            insert_record(&tx, &record)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Save `count` plain uplinks of 5,000 devices, one a second from the
    /// start of 2026-03-01, by SQL, which is much faster than building the
    /// records.  A filtered scan over this many takes long enough for a test
    /// to cancel it.
    pub(crate) fn insert_generated(&self, count: i64) {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < ?2 - 1)
             INSERT INTO messages (id, timestamp, direction, status, source_protocol,
                                   freq_mhz, datarate, mtype, dev_addr, payload_size, classified)
             SELECT 'gen-' || i,
                    strftime('%Y-%m-%dT%H:%M:%S', ?1 + i, 'unixepoch') || '+00:00',
                    'up', 'forwarded', 'udp', 868.1, 'SF7BW125', 'UnconfirmedDataUp',
                    printf('%08X', 0x26000000 + (i % 5000)), 23, 1
               FROM n",
            params![1_772_359_200_i64, count],
        )
        .unwrap();
    }

    /// What the history queries of this handle (and its clones) did.
    pub(crate) fn probe(&self) -> Arc<QueryProbe> {
        Arc::clone(&self.probe)
    }

    /// A handle on the database at `path`, which must already hold the
    /// schema, whose history connection finds the whole file locked by the
    /// connection returned with it.
    ///
    /// The three connections are opened before any of them touches the file:
    /// once a reader and a writer have attached, no connection can switch to
    /// exclusive locking.
    pub(crate) fn locked_by_another_connection(path: &str) -> (Self, Connection) {
        let reader = connect_reader(path).unwrap();
        let writer = Connection::open(path).unwrap();
        let holder = Connection::open(path).unwrap();
        holder
            .pragma_update(None, "locking_mode", "EXCLUSIVE")
            .unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        // A write, so that the lock is taken and not only promised.
        insert_record(&holder, &tests::sample_record()).unwrap();
        let storage = Self {
            reader: Some(Arc::new(Mutex::new(reader))),
            conn: Arc::new(Mutex::new(writer)),
            filters_on_writer: false,
            probe: Arc::default(),
        };
        (storage, holder)
    }
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

/// Open the writer connection to the database at `path`, creating its parent
/// directories.
fn connect(path: &str) -> Result<Connection, SnifferError> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(Connection::open(p)?)
}

/// The read-only connection for history queries, if the database allows it.
///
/// The journal mode is read back from `writer` rather than assumed: setting
/// it to WAL can quietly fail (a filesystem without shared memory, say), and
/// outside WAL a reader would block every commit of the writer.  Failing to
/// open the connection is a warning, not an error: the sniffer's job is to
/// forward and save, and history without filters still works.
fn open_reader(path: &str, writer: &Connection) -> Option<Connection> {
    let mode: String = match writer.query_row("PRAGMA journal_mode", [], |r| r.get(0)) {
        Ok(mode) => mode,
        Err(e) => {
            emit(
                Level::Warn,
                format!("Could not read the journal mode: {e}; history filters are unavailable"),
            );
            return None;
        }
    };
    if !mode.eq_ignore_ascii_case("wal") {
        emit(
            Level::Warn,
            format!("The database is not in WAL mode ({mode}); history filters are unavailable"),
        );
        return None;
    }
    match connect_reader(path) {
        Ok(reader) => Some(reader),
        Err(e) => {
            emit(
                Level::Warn,
                format!(
                    "Could not open the read-only history connection: {e}; \
                     history filters are unavailable"
                ),
            );
            None
        }
    }
}

/// Open `path` read-only, for queries that must not write and must not wait
/// long for a lock.
fn connect_reader(path: &str) -> rusqlite::Result<Connection> {
    let reader = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    reader.pragma_update(None, "query_only", "ON")?;
    reader.busy_timeout(READER_BUSY_TIMEOUT)?;
    Ok(reader)
}

fn insert_record(conn: &Connection, rec: &MessageRecord) -> rusqlite::Result<usize> {
    conn.execute(
        INSERT_SQL,
        params![
            rec.id,
            rec.timestamp.to_rfc3339(),
            direction_str(&rec.direction),
            status_str(&rec.status),
            rec.blocked_by.as_deref(),
            protocol_str(&rec.source_protocol),
            rec.freq_mhz,
            rec.datarate,
            rec.rssi.map(|v| v as i64),
            rec.snr.map(|v| v as f64),
            rec.crc_status.as_ref().map(crc_str),
            mtype_str(&rec.mtype),
            rec.dev_addr.as_deref(),
            rec.dev_eui.as_deref(),
            rec.fcnt.map(|v| v as i64),
            rec.fport.map(|v| v as i64),
            rec.payload_size as i64,
            rec.toa_ms,
            rec.gateway_eui.as_deref(),
            rec.join_eui.as_deref(),
            rec.dev_nonce.map(|v| v as i64),
            rec.adr.map(|v| v as i64),
            rec.adr_ack_req.map(|v| v as i64),
            rec.ack.map(|v| v as i64),
            rec.class_b.map(|v| v as i64),
            rec.f_pending.map(|v| v as i64),
            rec.f_opts_len.map(|v| v as i64),
            rec.malformed.as_ref().map(malformed_str),
        ],
    )
}

// ---------------------------------------------------------------------------
// Schema initialisation
// ---------------------------------------------------------------------------

fn init(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    let existed = table_exists(conn, "messages")?;
    conn.execute_batch(INIT_SQL)?;
    let from = schema_version(conn)?;
    migrate(conn)?;
    let to = schema_version(conn)?;
    if !existed {
        emit(
            Level::Info,
            format!("Database created at schema version {to}"),
        );
    } else if to != from {
        emit(
            Level::Info,
            format!("Database schema upgraded from version {from} to {to}"),
        );
    }

    // The schema is in place, so a failure from here on must not stop the
    // sniffer: forwarding matters more than hiding old noise.  The rows keep
    // `classified = 0` and the next start tries again.
    match classify_unchecked(conn) {
        Ok(Some(checked)) => {
            emit(
                Level::Info,
                format!(
                    "Checked {} stored rows, {} malformed, in {} ms",
                    checked.rows,
                    checked.malformed,
                    checked.elapsed.as_millis()
                ),
            );
            // The pass has committed by now.  Shrinking the log is only
            // housekeeping, so a failure is a warning and nothing is redone.
            if checked.first_pass {
                if let Err(e) = truncate_wal(conn) {
                    emit(
                        Level::Warn,
                        format!(
                            "Checked the stored rows, but could not shrink the write-ahead log: {e}; \
                             it is removed when the sniffer next stops cleanly"
                        ),
                    );
                }
            }
        }
        Ok(None) => {}
        Err(e) => emit(
            Level::Error,
            format!(
                "Could not check stored rows for malformed frames: {e}; will retry at the next start"
            ),
        ),
    }
    Ok(())
}

/// The database's schema version (`PRAGMA user_version`).
fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![name],
        |r| r.get(0),
    )
}

/// Apply every entry of [`MIGRATIONS`] newer than the database's
/// `user_version`.
///
/// Each migration runs in its own transaction together with the version bump,
/// so a failure partway through leaves the database at the previous version
/// instead of half-migrated (SQLite DDL and `user_version` are both
/// transactional).
fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let current = schema_version(conn)?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let target = i as i64 + 1;
        if target <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", target)?;
        tx.commit()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Classifying stored rows
// ---------------------------------------------------------------------------

/// What a committed pass of [`classify_unchecked`] did.
#[derive(Debug)]
struct Checked {
    /// Rows that were unchecked and now are checked.
    rows: usize,
    /// How many of them were marked malformed.
    malformed: usize,
    /// Whether this was the first pass, which visits every row and creates
    /// the partial index.
    first_pass: bool,
    elapsed: Duration,
}

/// Check the stored rows that have not been checked, in one transaction.
///
/// Returns `Ok(None)` when there was nothing to do, which is every normal
/// start.  Once the partial index exists that costs one count over an empty
/// index.  Before it exists, which is the first start after the upgrade and
/// every start until one pass has succeeded, every row is visited; a line in
/// the log says so first, because on a large database that takes a while and
/// forwarding does not start until it is over.
///
/// A failure rolls back the whole pass, the index included, and is returned
/// as SQLite reported it so that a caller can tell a full disk from anything
/// else.  Logging it is the caller's job.
fn classify_unchecked(conn: &mut Connection) -> rusqlite::Result<Option<Checked>> {
    let index_exists: bool = conn.query_row(UNCLASSIFIED_INDEX_EXISTS_SQL, [], |r| r.get(0))?;
    // A new database has nothing to check: its pass only makes the index, and
    // says nothing about it.
    let mut empty = false;
    if index_exists {
        let unchecked: i64 = conn.query_row(COUNT_UNCLASSIFIED_SQL, [], |r| r.get(0))?;
        if unchecked == 0 {
            return Ok(None);
        }
        emit(
            Level::Info,
            format!(
                "Checking {unchecked} stored rows recorded by an older version for malformed frames"
            ),
        );
    } else {
        // The highest row number stands in for the row count: unlike
        // COUNT(*), it does not read the table.
        let about: i64 =
            conn.query_row("SELECT COALESCE(MAX(rowid), 0) FROM messages", [], |r| {
                r.get(0)
            })?;
        empty = about == 0;
        if !empty {
            emit(
                Level::Info,
                format!(
                    "Checking every stored row for malformed frames (about {about} rows); \
                     forwarding starts when this is done"
                ),
            );
        }
    }

    let started = Instant::now();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let malformed = tx.execute(CLASSIFY_MALFORMED_SQL, [])?;
    let rest = tx.execute(CLASSIFY_REST_SQL, [])?;
    tx.execute_batch(UNCLASSIFIED_INDEX_SQL)?;
    tx.commit()?;

    if empty {
        return Ok(None);
    }
    Ok(Some(Checked {
        rows: malformed + rest,
        malformed,
        first_pass: !index_exists,
        elapsed: started.elapsed(),
    }))
}

/// Fold the write-ahead log into the database and truncate it.
///
/// The first pass rewrites every row, so the log grows with the database, and
/// SQLite does not shrink it while this connection stays open, which in the
/// sniffer is until it stops.  Without this the file keeps that size for the
/// whole run.
///
/// The checkpoint does not wait for other connections: a reader that still
/// holds an older snapshot (a backup tool, say) makes it report `busy`, which
/// is returned as an error here.  Nothing is lost by that.  The log is removed
/// when the last connection closes.
fn truncate_wal(conn: &Connection) -> Result<(), String> {
    // Connections wait for a lock for a few seconds by default.  That is
    // right for a write but pointless here: the start is already delayed by
    // the pass, and the log goes away at the next clean stop anyway.
    let wait: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::ZERO)
        .map_err(|e| e.to_string())?;
    let checkpoint = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        r.get::<_, i64>(0)
    });
    conn.busy_timeout(Duration::from_millis(wait.max(0) as u64))
        .map_err(|e| e.to_string())?;

    match checkpoint {
        Ok(0) => Ok(()),
        Ok(_) => Err("another connection is still reading the database".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Log a line, and in tests also record it for the test that is running.
///
/// The process-wide logger belongs to whichever test installs one first, and
/// the tests run in parallel, so a test cannot rely on it to see what a
/// particular `open` logged.
fn emit(level: Level, message: String) {
    log::log!(level, "{message}");
    #[cfg(test)]
    log_capture::record(level, &message);
}

/// What the current test's thread logged through [`emit`].
#[cfg(test)]
mod log_capture {
    use log::Level;
    use std::cell::RefCell;

    thread_local! {
        static LINES: RefCell<Option<Vec<(Level, String)>>> = const { RefCell::new(None) };
    }

    pub(super) fn record(level: Level, message: &str) {
        LINES.with(|lines| {
            if let Some(lines) = lines.borrow_mut().as_mut() {
                lines.push((level, message.to_string()));
            }
        });
    }

    /// Run `f` and return its result with the lines it logged on this thread.
    pub(super) fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<(Level, String)>) {
        LINES.with(|lines| *lines.borrow_mut() = Some(Vec::new()));
        let result = f();
        let lines = LINES.with(|lines| lines.borrow_mut().take().unwrap_or_default());
        (result, lines)
    }
}

// ---------------------------------------------------------------------------
// Label helpers
// ---------------------------------------------------------------------------

/// Does any stored message carry `device` as its DevAddr or DevEUI?
///
/// Two indexed probes rather than one `OR`, for the reason given on
/// [`ORPHAN_LABELS_SQL`].  The first hit ends the search.
fn has_stored_record(conn: &Connection, device: &str) -> rusqlite::Result<bool> {
    for sql in [HAS_DEV_ADDR_SQL, HAS_DEV_EUI_SQL] {
        if conn.query_row(sql, params![device], |r| r.get::<_, bool>(0))? {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// History queries
// ---------------------------------------------------------------------------

/// The conditions of a history query and the values they bind.
///
/// Built once per request and shared by every statement of it, so the count,
/// the scan and the fetch can never disagree about which rows match.
struct Conditions {
    /// The static SQL fragments, ANDed together.
    parts: Vec<&'static str>,
    /// The named values the fragments use.
    values: Vec<(&'static str, Value)>,
}

impl Conditions {
    /// `label_ids` is the JSON list of identifiers whose label matches the
    /// device text; it is present exactly when the request has device text.
    fn new(q: &HistoryQuery, label_ids: Option<String>) -> Self {
        let mut c = Self {
            parts: Vec::new(),
            values: Vec::new(),
        };
        match q.lower {
            Some(LowerBound::After(at)) => c.add(AFTER_SQL, [(":bound", at.to_rfc3339().into())]),
            Some(LowerBound::From(at)) => c.add(FROM_SQL, [(":bound", at.to_rfc3339().into())]),
            None => {}
        }
        if let (Some(text), Some(ids)) = (q.filter.device_text(), label_ids) {
            c.add(
                DEVICE_SQL,
                [(":needle", fold_case(text).into()), (":ids", ids.into())],
            );
        }
        if !q.filter.mtypes.is_empty() {
            let names: Vec<&str> = q.filter.mtypes.iter().map(mtype_str).collect();
            let names = serde_json::to_string(&names).expect("a list of names is valid JSON");
            c.add(MTYPE_SQL, [(":mtypes", names.into())]);
        }
        if let Some(status) = &q.filter.status {
            c.add(
                STATUS_SQL,
                [(":status", status_str(status).to_string().into())],
            );
        }
        if let Some(gateway) = q.filter.gateway_text() {
            c.add(GATEWAY_SQL, [(":gateway", gateway.to_string().into())]);
        }
        c
    }

    fn add<const N: usize>(&mut self, part: &'static str, values: [(&'static str, Value); N]) {
        self.parts.push(part);
        self.values.extend(values);
    }

    /// ` WHERE …` with the conditions and then `extra`, or nothing.
    fn where_sql(&self, extra: &[&str]) -> String {
        let all: Vec<&str> = self
            .parts
            .iter()
            .copied()
            .chain(extra.iter().copied())
            .collect();
        if all.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", all.join(" AND "))
        }
    }

    /// The bound values, for a statement made from these conditions.  Extra
    /// names would be an error, so a statement that also binds `:limit` adds
    /// it to this list itself.
    fn bindings(&self) -> Vec<(&str, &dyn ToSql)> {
        self.values
            .iter()
            .map(|(name, value)| (*name, value as &dyn ToSql))
            .collect()
    }

    /// `COUNT(*)` of the rows the conditions let through, over the timestamp
    /// index alone when the conditions are the time bound.
    fn count_sql(&self) -> String {
        format!("SELECT COUNT(*) {RANGE_FROM}{}", self.where_sql(&[]))
    }

    /// `COUNT(*)` of the malformed rows among them.
    fn malformed_count_sql(&self) -> String {
        format!(
            "SELECT COUNT(*) {RANGE_FROM}{}",
            self.where_sql(&[MALFORMED_SQL])
        )
    }

    /// The first `:limit` rows in full, in index order.
    fn page_sql(&self, newest: bool) -> String {
        format!(
            "SELECT {SELECT_COLS} {RANGE_FROM}{} ORDER BY timestamp {} LIMIT :limit",
            self.where_sql(&[]),
            direction_sql(newest)
        )
    }

    /// Every matching row's number and whether it is malformed, in index order.
    fn scan_sql(&self, newest: bool) -> String {
        format!(
            "SELECT rowid, malformed {RANGE_FROM}{} ORDER BY timestamp {}",
            self.where_sql(&[]),
            direction_sql(newest)
        )
    }
}

fn direction_sql(newest: bool) -> &'static str {
    if newest {
        "DESC"
    } else {
        "ASC"
    }
}

/// Removes the progress handler when dropped, on every way out of a query.
///
/// The handler captures the request's flag.  Left on the connection it would
/// cancel the next query as soon as that flag was set, and the next query
/// belongs to someone else.
struct CancelGuard<'a>(&'a Connection);

impl<'a> CancelGuard<'a> {
    fn install(conn: &'a Connection, cancel: &Arc<AtomicBool>) -> Self {
        let flag = Arc::clone(cancel);
        conn.progress_handler(
            CANCEL_CHECK_STEPS,
            Some(move || flag.load(Ordering::Relaxed)),
        );
        Self(conn)
    }
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

/// The error SQLite itself raises for an interrupted statement, so that a
/// check between two statements ends a query the way a check inside one does.
fn interrupted() -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
        None,
    )
}

fn check_cancel(cancel: &AtomicBool) -> rusqlite::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(interrupted())
    } else {
        Ok(())
    }
}

/// An interrupt that the request's own flag asked for is a cancellation;
/// a lock that outlasted the wait is a busy database; anything else is a
/// database error.
fn history_error(e: rusqlite::Error, cancel: &AtomicBool) -> SnifferError {
    match e.sqlite_error_code() {
        Some(ErrorCode::OperationInterrupted) if cancel.load(Ordering::Relaxed) => {
            SnifferError::Cancelled
        }
        Some(ErrorCode::DatabaseBusy) => SnifferError::Busy,
        _ => SnifferError::Database(e),
    }
}

/// Run `q` on `conn`; see [`Storage::query_history`].  `scans_allowed` says
/// whether a scan that reads the table may run here.
fn run_history(
    conn: &Connection,
    q: &HistoryQuery,
    cancel: &Arc<AtomicBool>,
    scans_allowed: bool,
) -> rusqlite::Result<HistoryPage> {
    check_cancel(cancel)?;
    let limit = q.limit.min(MAX_HISTORY_ROWS);
    // Declared before the transaction so that it is dropped after it: the
    // handler stays in place for the rollback too.
    let _guard = CancelGuard::install(conn, cancel);
    let tx = conn.unchecked_transaction()?;

    let label_ids = match q.filter.device_text() {
        Some(text) => Some(matching_label_ids(&tx, text)?),
        None => None,
    };
    let conditions = Conditions::new(q, label_ids);
    check_cancel(cancel)?;
    if q.filter.has_row_filter() {
        filtered_history(&tx, q, &conditions, limit, cancel)
    } else {
        ranged_history(&tx, q, &conditions, limit, scans_allowed, cancel)
    }
}

/// The identifiers whose label contains `text`, as a JSON list for `json_each`.
fn matching_label_ids(conn: &Connection, text: &str) -> rusqlite::Result<String> {
    let mut stmt = conn.prepare("SELECT device, label FROM device_labels")?;
    let labels = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let ids = devices_with_matching_label(
        &fold_case(text),
        labels
            .iter()
            .map(|(device, label)| (device.as_str(), label.as_str())),
    );
    Ok(serde_json::to_string(&ids).expect("a list of identifiers is valid JSON"))
}

fn count_rows(
    conn: &Connection,
    sql: &str,
    bindings: &[(&str, &dyn ToSql)],
) -> rusqlite::Result<u64> {
    let n: i64 = conn.query_row(sql, bindings, |r| r.get(0))?;
    Ok(n as u64)
}

/// A request with only a time bound: an index count, and the first `limit` rows
/// of the index range.
fn ranged_history(
    conn: &Connection,
    q: &HistoryQuery,
    conditions: &Conditions,
    limit: usize,
    scans_allowed: bool,
    cancel: &AtomicBool,
) -> rusqlite::Result<HistoryPage> {
    let (mut total, mut malformed) = (None, None);
    if q.count {
        total = Some(count_rows(
            conn,
            &conditions.count_sql(),
            &conditions.bindings(),
        )?);
        check_cancel(cancel)?;
        if scans_allowed {
            malformed = Some(count_rows(
                conn,
                &conditions.malformed_count_sql(),
                &conditions.bindings(),
            )?);
            check_cancel(cancel)?;
        }
    }

    let mut bindings = conditions.bindings();
    let limit_value = limit as i64;
    bindings.push((":limit", &limit_value));
    let mut stmt = conn.prepare(&conditions.page_sql(q.newest))?;
    let mut rows = stmt.query(&*bindings)?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        records.push(row_to_record(row)?);
    }
    if q.newest {
        records.reverse();
    }
    Ok(HistoryPage {
        records,
        total,
        malformed,
    })
}

/// A request with row filters: one scan that counts and chooses, then a fetch
/// of the chosen rows.
fn filtered_history(
    conn: &Connection,
    q: &HistoryQuery,
    conditions: &Conditions,
    limit: usize,
    cancel: &AtomicBool,
) -> rusqlite::Result<HistoryPage> {
    let hide_malformed = q.filter.hide_malformed;
    let mut chosen: Vec<i64> = Vec::new();
    let (mut total, mut malformed) = (0u64, 0u64);
    // With no rows to keep and nothing to count there is nothing to scan for.
    if limit > 0 || q.count {
        let mut stmt = conn.prepare(&conditions.scan_sql(q.newest))?;
        let mut rows = stmt.query(&*conditions.bindings())?;
        while let Some(row) = rows.next()? {
            // Counted here, not in SQL, so that the one scan yields both
            // counts whichever way the box is set.
            if !matches!(row.get_ref(1)?, ValueRef::Null) {
                malformed += 1;
                if hide_malformed {
                    continue;
                }
            }
            total += 1;
            if chosen.len() < limit {
                chosen.push(row.get(0)?);
            }
            if !q.count && chosen.len() >= limit {
                break;
            }
        }
    }
    check_cancel(cancel)?;

    let mut records = Vec::with_capacity(chosen.len());
    if !chosen.is_empty() {
        let ids = serde_json::to_string(&chosen).expect("a list of numbers is valid JSON");
        let mut stmt = conn.prepare(&fetch_by_rowid_sql())?;
        let mut rows = stmt.query(params![ids])?;
        while let Some(row) = rows.next()? {
            records.push(row_to_record(row)?);
        }
    }
    Ok(HistoryPage {
        records,
        total: q.count.then_some(total),
        malformed: q.count.then_some(malformed),
    })
}

// ---------------------------------------------------------------------------
// Row deserialization
// ---------------------------------------------------------------------------

fn row_to_record(row: &rusqlite::Row) -> Result<MessageRecord, rusqlite::Error> {
    let ts_str: String = row.get(1)?;
    let timestamp = DateTime::parse_from_rfc3339(&ts_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|e| {
            warn!(
                "Failed to parse stored timestamp '{}': {} — using current time",
                ts_str, e
            );
            Utc::now()
        });

    let direction = match row.get::<_, String>(2)?.as_str() {
        "up" => Direction::Up,
        _ => Direction::Down,
    };
    let status = match row.get::<_, String>(3)?.as_str() {
        "forwarded" => Status::Forwarded,
        "captured" => Status::Captured,
        _ => Status::Blocked,
    };
    let source_protocol = match row.get::<_, String>(5)?.as_str() {
        "udp" => Protocol::Udp,
        _ => Protocol::Concentratord,
    };
    let crc_status = row.get::<_, Option<String>>(10)?.map(|s| match s.as_str() {
        "ok" => CrcStatus::Ok,
        "fail" => CrcStatus::Fail,
        _ => CrcStatus::NoCrc,
    });
    let mtype = match row.get::<_, String>(11)?.as_str() {
        "JoinRequest" => MType::JoinRequest,
        "JoinAccept" => MType::JoinAccept,
        "UnconfirmedDataUp" => MType::UnconfirmedDataUp,
        "UnconfirmedDataDown" => MType::UnconfirmedDataDown,
        "ConfirmedDataUp" => MType::ConfirmedDataUp,
        "ConfirmedDataDown" => MType::ConfirmedDataDown,
        "RejoinRequest" => MType::RejoinRequest,
        "Proprietary" => MType::Proprietary,
        _ => MType::Unknown,
    };

    Ok(MessageRecord {
        id: row.get(0)?,
        timestamp,
        direction,
        status,
        blocked_by: row.get(4)?,
        source_protocol,
        freq_mhz: row.get(6)?,
        datarate: row.get(7)?,
        rssi: row.get::<_, Option<i64>>(8)?.map(|v| v as i16),
        snr: row.get::<_, Option<f64>>(9)?.map(|v| v as f32),
        crc_status,
        mtype,
        dev_addr: row.get(12)?,
        dev_eui: row.get(13)?,
        fcnt: row.get::<_, Option<i64>>(14)?.map(|v| v as u32),
        fport: row.get::<_, Option<i64>>(15)?.map(|v| v as u8),
        payload_size: row.get::<_, i64>(16)? as u16,
        toa_ms: row.get(17)?,
        gateway_eui: row.get(18)?,
        join_eui: row.get(19)?,
        dev_nonce: row.get::<_, Option<i64>>(20)?.map(|v| v as u16),
        adr: get_flag(row, 21)?,
        adr_ack_req: get_flag(row, 22)?,
        ack: get_flag(row, 23)?,
        class_b: get_flag(row, 24)?,
        f_pending: get_flag(row, 25)?,
        f_opts_len: row.get::<_, Option<i64>>(26)?.map(|v| v as u8),
        malformed: row
            .get::<_, Option<String>>(27)?
            .as_deref()
            .and_then(parse_malformed),
    })
}

/// Read a nullable INTEGER 0/1 column as an optional boolean.
fn get_flag(row: &rusqlite::Row, idx: usize) -> Result<Option<bool>, rusqlite::Error> {
    Ok(row.get::<_, Option<i64>>(idx)?.map(|v| v != 0))
}

// ---------------------------------------------------------------------------
// Enum → SQL string helpers
// ---------------------------------------------------------------------------

fn direction_str(d: &Direction) -> &'static str {
    match d {
        Direction::Up => "up",
        Direction::Down => "down",
    }
}

fn status_str(s: &Status) -> &'static str {
    match s {
        Status::Forwarded => "forwarded",
        Status::Blocked => "blocked",
        Status::Captured => "captured",
    }
}

fn protocol_str(p: &Protocol) -> &'static str {
    match p {
        Protocol::Udp => "udp",
        Protocol::Concentratord => "concentratord",
    }
}

fn crc_str(c: &CrcStatus) -> &'static str {
    match c {
        CrcStatus::Ok => "ok",
        CrcStatus::Fail => "fail",
        CrcStatus::NoCrc => "no_crc",
    }
}

fn malformed_str(m: &Malformed) -> &'static str {
    m.as_str()
}

/// The reason for a stored `malformed` value, or `None` for anything that is
/// not one of the three names (an empty column included).
fn parse_malformed(s: &str) -> Option<Malformed> {
    match s {
        "crc" => Some(Malformed::Crc),
        "mhdr" => Some(Malformed::Mhdr),
        "length" => Some(Malformed::Length),
        _ => None,
    }
}

fn mtype_str(m: &MType) -> &'static str {
    match m {
        MType::JoinRequest => "JoinRequest",
        MType::JoinAccept => "JoinAccept",
        MType::UnconfirmedDataUp => "UnconfirmedDataUp",
        MType::UnconfirmedDataDown => "UnconfirmedDataDown",
        MType::ConfirmedDataUp => "ConfirmedDataUp",
        MType::ConfirmedDataDown => "ConfirmedDataDown",
        MType::RejoinRequest => "RejoinRequest",
        MType::Proprietary => "Proprietary",
        MType::Unknown => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod history_large_tests;
#[cfg(test)]
mod history_tests;
#[cfg(test)]
pub(crate) use tests::TempDb;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::eui::Eui64;
    use crate::core::metadata::{CrcStatus, Direction, MType, MessageRecord, Protocol, Status};
    use chrono::Utc;

    pub(super) fn sample_record() -> MessageRecord {
        MessageRecord {
            id: MessageRecord::new_id(),
            timestamp: Utc::now(),
            direction: Direction::Up,
            status: Status::Forwarded,
            blocked_by: None,
            source_protocol: Protocol::Udp,
            freq_mhz: 868.1,
            datarate: "SF7BW125".to_string(),
            rssi: Some(-55),
            snr: Some(8.5),
            crc_status: Some(CrcStatus::Ok),
            mtype: MType::UnconfirmedDataUp,
            dev_addr: Some("01ABCDEF".to_string()),
            dev_eui: None,
            fcnt: Some(1234),
            fport: Some(1),
            payload_size: 23,
            toa_ms: Some(61.7),
            gateway_eui: None,
            join_eui: None,
            dev_nonce: None,
            adr: None,
            adr_ack_req: None,
            ack: None,
            class_b: None,
            f_pending: None,
            f_opts_len: None,
            malformed: None,
        }
    }

    /// The newest `n` records, oldest first: what `/api/messages?limit=n`
    /// returns.
    fn recent(s: &Storage, n: usize) -> Vec<MessageRecord> {
        let q = HistoryQuery {
            lower: None,
            filter: Default::default(),
            limit: n,
            newest: true,
            count: false,
        };
        s.query_history(&q, Arc::new(AtomicBool::new(false)))
            .unwrap()
            .records
    }

    /// The oldest `n` records strictly after `at`: what
    /// `/api/messages?since=at&limit=n` returns.
    fn since(s: &Storage, at: DateTime<Utc>, n: usize) -> Vec<MessageRecord> {
        let q = HistoryQuery {
            lower: Some(LowerBound::After(at)),
            filter: Default::default(),
            limit: n,
            newest: false,
            count: false,
        };
        s.query_history(&q, Arc::new(AtomicBool::new(false)))
            .unwrap()
            .records
    }

    #[test]
    fn open_in_memory_succeeds() {
        Storage::open_in_memory().unwrap();
    }

    #[test]
    fn insert_and_count() {
        let s = Storage::open_in_memory().unwrap();
        assert_eq!(s.count().unwrap(), 0);
        s.insert(&sample_record()).unwrap();
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn insert_duplicate_id_replaces() {
        let s = Storage::open_in_memory().unwrap();
        let mut rec = sample_record();
        rec.id = "fixed-id".to_string();
        s.insert(&rec).unwrap();
        s.insert(&rec).unwrap(); // same id, INSERT OR REPLACE
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn insert_blocked_record() {
        let s = Storage::open_in_memory().unwrap();
        let mut rec = sample_record();
        rec.status = Status::Blocked;
        rec.blocked_by = Some("my-rule".to_string());
        s.insert(&rec).unwrap();
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn insert_nulls_for_downlink() {
        let s = Storage::open_in_memory().unwrap();
        let rec = MessageRecord {
            id: MessageRecord::new_id(),
            timestamp: Utc::now(),
            direction: Direction::Down,
            status: Status::Forwarded,
            blocked_by: None,
            source_protocol: Protocol::Udp,
            freq_mhz: 869.525,
            datarate: "SF9BW125".to_string(),
            rssi: None,
            snr: None,
            crc_status: None,
            mtype: MType::UnconfirmedDataDown,
            dev_addr: Some("AABBCCDD".to_string()),
            dev_eui: None,
            fcnt: Some(42),
            fport: None,
            payload_size: 13,
            toa_ms: None,
            gateway_eui: None,
            join_eui: None,
            dev_nonce: None,
            adr: None,
            adr_ack_req: None,
            ack: None,
            class_b: None,
            f_pending: None,
            f_opts_len: None,
            malformed: None,
        };
        s.insert(&rec).unwrap();
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn insert_join_request() {
        let s = Storage::open_in_memory().unwrap();
        let rec = MessageRecord {
            id: MessageRecord::new_id(),
            timestamp: Utc::now(),
            direction: Direction::Up,
            status: Status::Forwarded,
            blocked_by: None,
            source_protocol: Protocol::Udp,
            freq_mhz: 868.1,
            datarate: "SF10BW125".to_string(),
            rssi: Some(-101),
            snr: Some(-3.5),
            crc_status: Some(CrcStatus::Ok),
            mtype: MType::JoinRequest,
            dev_addr: None,
            dev_eui: Some("01:02:03:04:05:06:07:08".to_string()),
            fcnt: None,
            fport: None,
            payload_size: 23,
            toa_ms: Some(370.0),
            gateway_eui: None,
            join_eui: None,
            dev_nonce: None,
            adr: None,
            adr_ack_req: None,
            ack: None,
            class_b: None,
            f_pending: None,
            f_opts_len: None,
            malformed: None,
        };
        s.insert(&rec).unwrap();
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn delete_older_than_removes_expired() {
        let s = Storage::open_in_memory().unwrap();

        // Old record (31 days ago)
        let mut old = sample_record();
        old.timestamp = Utc::now() - chrono::Duration::days(31);
        s.insert(&old).unwrap();

        // Recent record (now)
        s.insert(&sample_record()).unwrap();

        assert_eq!(s.count().unwrap(), 2);

        let cutoff = Utc::now() - chrono::Duration::days(30);
        let pruned = s.delete_older_than(cutoff, OrphanPolicy::Keep).unwrap();
        assert_eq!(pruned.messages, 1);
        assert!(pruned.labels.is_empty());
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn delete_older_than_keeps_all_when_recent() {
        let s = Storage::open_in_memory().unwrap();
        s.insert(&sample_record()).unwrap();
        s.insert(&sample_record()).unwrap();

        let cutoff = Utc::now() - chrono::Duration::days(30);
        let pruned = s.delete_older_than(cutoff, OrphanPolicy::Keep).unwrap();
        assert_eq!(pruned.messages, 0);
        assert!(pruned.labels.is_empty());
        assert_eq!(s.count().unwrap(), 2);
    }

    #[test]
    fn delete_older_than_removes_all_when_all_old() {
        let s = Storage::open_in_memory().unwrap();
        for i in 0..5 {
            let mut rec = sample_record();
            rec.timestamp = Utc::now() - chrono::Duration::days(31 + i);
            s.insert(&rec).unwrap();
        }
        let cutoff = Utc::now() - chrono::Duration::days(1);
        let pruned = s.delete_older_than(cutoff, OrphanPolicy::Keep).unwrap();
        assert_eq!(pruned.messages, 5);
        assert!(pruned.labels.is_empty());
        assert_eq!(s.count().unwrap(), 0);
    }

    // -----------------------------------------------------------------------
    // Schema upgrades
    // -----------------------------------------------------------------------

    /// The schema exactly as v2.0.0 created it, before versioned migrations.
    ///
    /// This is a frozen fixture standing in for databases that already exist
    /// in the field.  It must never be edited to track later schemas: the
    /// whole point is that it stays the old schema while `INIT_SQL` and
    /// `MIGRATIONS` move on.
    const SCHEMA_V2_0_0: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous  = NORMAL;
    PRAGMA foreign_keys = ON;

    CREATE TABLE IF NOT EXISTS messages (
        id              TEXT    NOT NULL PRIMARY KEY,
        timestamp       TEXT    NOT NULL,
        direction       TEXT    NOT NULL,
        status          TEXT    NOT NULL,
        blocked_by      TEXT,
        source_protocol TEXT    NOT NULL,
        freq_mhz        REAL    NOT NULL,
        datarate        TEXT    NOT NULL,
        rssi            INTEGER,
        snr             REAL,
        crc_status      TEXT,
        mtype           TEXT    NOT NULL,
        dev_addr        TEXT,
        dev_eui         TEXT,
        fcnt            INTEGER,
        fport           INTEGER,
        payload_size    INTEGER NOT NULL,
        toa_ms          REAL
    );

    CREATE INDEX IF NOT EXISTS idx_messages_timestamp
        ON messages (timestamp);

    CREATE INDEX IF NOT EXISTS idx_messages_dev_addr
        ON messages (dev_addr)
        WHERE dev_addr IS NOT NULL;

    CREATE INDEX IF NOT EXISTS idx_messages_dev_eui
        ON messages (dev_eui)
        WHERE dev_eui IS NOT NULL;

    CREATE INDEX IF NOT EXISTS idx_messages_mtype
        ON messages (mtype);
    ";

    /// The column list v2.0.0 inserted with.
    const INSERT_V2_0_0: &str = "
    INSERT INTO messages (
        id, timestamp, direction, status, blocked_by, source_protocol,
        freq_mhz, datarate, rssi, snr, crc_status,
        mtype, dev_addr, dev_eui, fcnt, fport, payload_size, toa_ms
    ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6,
        ?7, ?8, ?9, ?10, ?11,
        ?12, ?13, ?14, ?15, ?16, ?17, ?18
    )";

    /// A database file in a private temp directory, removed on drop even when
    /// the test fails.
    pub(crate) struct TempDb {
        dir: std::path::PathBuf,
    }

    impl TempDb {
        pub(crate) fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "lorawan-sniffer-{}-{}-{}",
                label,
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        pub(crate) fn path(&self) -> String {
            self.dir.join("sniffer.db").to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn user_version(s: &Storage) -> i64 {
        let conn = s.conn.lock().unwrap();
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    fn column_names(s: &Storage) -> Vec<String> {
        let conn = s.conn.lock().unwrap();
        let mut stmt = conn.prepare("PRAGMA table_info(messages)").unwrap();
        let names = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        names
    }

    fn json(rec: &MessageRecord) -> serde_json::Value {
        serde_json::to_value(rec).unwrap()
    }

    #[test]
    fn upgrades_v2_0_0_database() {
        let tmp = TempDb::new("upgrade");
        let path = tmp.path();

        // A database as v2.0.0 left it: old schema, rows with the old columns,
        // and no schema version recorded.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V2_0_0).unwrap();
            conn.execute(
                INSERT_V2_0_0,
                params![
                    "old-forwarded",
                    "2026-03-01T10:00:00+00:00",
                    "up",
                    "forwarded",
                    None::<String>,
                    "udp",
                    868.1,
                    "SF7BW125",
                    -55,
                    8.5,
                    "ok",
                    "UnconfirmedDataUp",
                    "01ABCDEF",
                    None::<String>,
                    1234,
                    1,
                    23,
                    61.7,
                ],
            )
            .unwrap();
            conn.execute(
                INSERT_V2_0_0,
                params![
                    "old-blocked",
                    "2026-03-01T10:00:05+00:00",
                    "down",
                    "blocked",
                    "no-downlinks",
                    "concentratord",
                    869.525,
                    "SF9BW125",
                    None::<i64>,
                    None::<f64>,
                    None::<String>,
                    "UnconfirmedDataDown",
                    "AABBCCDD",
                    None::<String>,
                    42,
                    None::<i64>,
                    13,
                    None::<f64>,
                ],
            )
            .unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 0, "fixture must look like an unversioned database");
        }

        let s = Storage::open(&path).unwrap();
        assert_eq!(user_version(&s), MIGRATIONS.len() as i64);
        let columns = column_names(&s);
        for new in [
            "gateway_eui",
            "join_eui",
            "dev_nonce",
            "adr",
            "adr_ack_req",
            "ack",
            "class_b",
            "f_pending",
            "f_opts_len",
            "malformed",
            "classified",
        ] {
            assert!(columns.iter().any(|c| c == new), "missing column {new}");
        }
        assert_eq!(columns.len(), 29);

        // The labels table arrives with the same upgrade, empty and usable.
        assert!(s.labels().unwrap().is_empty());
        assert_eq!(
            s.set_label("01ABCDEF", "upgraded", false, 10).unwrap(),
            LabelWrite::Set
        );
        assert!(s.delete_label("01ABCDEF").unwrap());

        // Old rows survive intact; every new field reads as absent.
        let rows = recent(&s, 10);
        assert_eq!(rows.len(), 2);
        let forwarded = rows.iter().find(|r| r.id == "old-forwarded").unwrap();
        assert_eq!(forwarded.status, Status::Forwarded);
        assert_eq!(forwarded.mtype, MType::UnconfirmedDataUp);
        assert_eq!(forwarded.dev_addr.as_deref(), Some("01ABCDEF"));
        assert_eq!(forwarded.fcnt, Some(1234));
        assert_eq!(forwarded.rssi, Some(-55));
        let blocked = rows.iter().find(|r| r.id == "old-blocked").unwrap();
        assert_eq!(blocked.status, Status::Blocked);
        assert_eq!(blocked.blocked_by.as_deref(), Some("no-downlinks"));
        assert_eq!(blocked.source_protocol, Protocol::Concentratord);
        for old in &rows {
            assert_eq!(old.gateway_eui, None);
            assert_eq!(old.join_eui, None);
            assert_eq!(old.dev_nonce, None);
            assert_eq!(old.adr, None);
            assert_eq!(old.adr_ack_req, None);
            assert_eq!(old.ack, None);
            assert_eq!(old.class_b, None);
            assert_eq!(old.f_pending, None);
            assert_eq!(old.f_opts_len, None);
        }

        // The upgraded database takes new rows, next to the old ones.
        let mut fresh = sample_record();
        fresh.id = "new-row".to_string();
        fresh.timestamp = Utc::now();
        fresh.gateway_eui = Some("AA:BB:CC:DD:00:11:22:33".to_string());
        fresh.adr = Some(true);
        fresh.f_opts_len = Some(3);
        s.insert(&fresh).unwrap();
        assert_eq!(s.count().unwrap(), 3);
        let back = recent(&s, 10)
            .into_iter()
            .find(|r| r.id == "new-row")
            .unwrap();
        assert_eq!(json(&back), json(&fresh));

        // Reopening an already-upgraded database must not run the migration
        // again (adding a column twice is an error).
        drop(s);
        let s = Storage::open(&path).unwrap();
        assert_eq!(user_version(&s), MIGRATIONS.len() as i64);
        assert_eq!(s.count().unwrap(), 3);
    }

    #[test]
    fn new_database_is_at_the_latest_version() {
        let s = Storage::open_in_memory().unwrap();
        assert_eq!(user_version(&s), MIGRATIONS.len() as i64);
        let columns = column_names(&s);
        assert_eq!(columns.len(), 29);
        assert_eq!(&columns[27..], ["malformed", "classified"]);
    }

    #[test]
    fn failed_migration_leaves_the_version_unchanged() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V2_0_0).unwrap();
        // Make the third ALTER in the migration collide with an existing
        // column, so the migration fails after two statements have run.
        conn.execute_batch("ALTER TABLE messages ADD COLUMN dev_nonce INTEGER;")
            .unwrap();

        assert!(migrate(&mut conn).is_err());

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 0);
        let has_gateway_eui = conn.prepare("SELECT gateway_eui FROM messages").is_ok();
        assert!(!has_gateway_eui, "partial migration must be rolled back");
    }

    #[test]
    fn new_fields_round_trip() {
        let s = Storage::open_in_memory().unwrap();
        let base = Utc::now();

        // Uplink with flags set in a pattern that would expose swapped columns.
        let mut up = sample_record();
        up.id = "up".to_string();
        up.timestamp = base;
        up.gateway_eui = Some("AA:BB:CC:DD:00:11:22:33".to_string());
        up.adr = Some(true);
        up.adr_ack_req = Some(false);
        up.ack = Some(false);
        up.class_b = Some(true);
        up.f_pending = None;
        up.f_opts_len = Some(7);

        // Downlink, captured, with the opposite pattern.
        let mut down = sample_record();
        down.id = "down".to_string();
        down.timestamp = base + chrono::Duration::seconds(1);
        down.direction = Direction::Down;
        down.status = Status::Captured;
        down.mtype = MType::ConfirmedDataDown;
        down.gateway_eui = Some("11:22:33:44:55:66:77:88".to_string());
        down.adr = Some(false);
        down.adr_ack_req = None;
        down.ack = Some(true);
        down.class_b = None;
        down.f_pending = Some(true);
        down.f_opts_len = Some(15);

        // Join request.
        let mut join = sample_record();
        join.id = "join".to_string();
        join.timestamp = base + chrono::Duration::seconds(2);
        join.mtype = MType::JoinRequest;
        join.dev_addr = None;
        join.dev_eui = Some("01:02:03:04:05:06:07:08".to_string());
        join.join_eui = Some("00:00:00:00:00:00:00:01".to_string());
        join.dev_nonce = Some(0xFFFF);

        // Nothing set at all.
        let mut bare = sample_record();
        bare.id = "bare".to_string();
        bare.timestamp = base + chrono::Duration::seconds(3);
        bare.status = Status::Blocked;
        bare.blocked_by = Some("rule".to_string());

        let records = [up, down, join, bare];
        for rec in &records {
            s.insert(rec).unwrap();
        }

        let rows = recent(&s, 10);
        assert_eq!(rows.len(), 4);
        for expected in &records {
            let got = rows.iter().find(|r| r.id == expected.id).unwrap();
            assert_eq!(json(got), json(expected), "record {}", expected.id);
        }

        // false and NULL stay distinct, and 0/1 map back to booleans.
        let up = rows.iter().find(|r| r.id == "up").unwrap();
        assert_eq!(up.adr_ack_req, Some(false));
        assert_eq!(up.f_pending, None);
        let down = rows.iter().find(|r| r.id == "down").unwrap();
        assert_eq!(down.status, Status::Captured);
        assert_eq!(down.ack, Some(true));
        assert_eq!(down.adr, Some(false));
        assert_eq!(down.class_b, None);

        // They also come back through the incremental query used by the API.
        let since = base - chrono::Duration::seconds(1);
        let rows = self::since(&s, since, 10);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].id, "down");
        assert_eq!(rows[1].f_opts_len, Some(15));
    }

    #[test]
    fn captured_status_is_stored_as_text() {
        let s = Storage::open_in_memory().unwrap();
        let mut rec = sample_record();
        rec.status = Status::Captured;
        s.insert(&rec).unwrap();
        let stored: String = s
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT status FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "captured");
    }

    // -----------------------------------------------------------------------
    // Device labels
    // -----------------------------------------------------------------------

    const HAVANA: &str = "00E8A80B";
    const QINGPING_EUI: &str = "36:31:39:54:36:5D:05:39";

    /// Store one message for `device`, in the column its shape implies: 8 hex
    /// digits are a DevAddr, anything longer a DevEUI.
    fn store_record(s: &Storage, device: &str, timestamp: DateTime<Utc>) {
        let mut rec = sample_record();
        rec.timestamp = timestamp;
        if device.len() == 8 {
            rec.dev_addr = Some(device.to_string());
            rec.dev_eui = None;
        } else {
            rec.mtype = MType::JoinRequest;
            rec.dev_addr = None;
            rec.dev_eui = Some(device.to_string());
        }
        s.insert(&rec).unwrap();
    }

    fn days_ago(n: i64) -> DateTime<Utc> {
        Utc::now() - chrono::Duration::days(n)
    }

    /// The `detail` column of `EXPLAIN QUERY PLAN` for `sql`.
    fn query_plan(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt
            .query_map(args, |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }

    /// The frozen layout of a 2.1.0 database: the 2.0.0 schema plus the
    /// columns added by the first migration, marked as version 1.
    const SCHEMA_V2_1_0_UPGRADE: &str = "
    ALTER TABLE messages ADD COLUMN gateway_eui TEXT;
    ALTER TABLE messages ADD COLUMN join_eui    TEXT;
    ALTER TABLE messages ADD COLUMN dev_nonce   INTEGER;
    ALTER TABLE messages ADD COLUMN adr         INTEGER;
    ALTER TABLE messages ADD COLUMN adr_ack_req INTEGER;
    ALTER TABLE messages ADD COLUMN ack         INTEGER;
    ALTER TABLE messages ADD COLUMN class_b     INTEGER;
    ALTER TABLE messages ADD COLUMN f_pending   INTEGER;
    ALTER TABLE messages ADD COLUMN f_opts_len  INTEGER;
    PRAGMA user_version = 1;
    ";

    #[test]
    fn upgrades_v2_1_0_database_keeping_rows_and_adding_the_label_table() {
        let tmp = TempDb::new("upgrade-labels");
        let path = tmp.path();

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V2_0_0).unwrap();
            conn.execute_batch(SCHEMA_V2_1_0_UPGRADE).unwrap();
            let insert = "INSERT INTO messages (
                id, timestamp, direction, status, source_protocol, freq_mhz,
                datarate, mtype, dev_addr, dev_eui, payload_size, gateway_eui, adr
            ) VALUES (?1, ?2, 'up', 'forwarded', 'udp', 868.1, 'SF7BW125', ?3, ?4, ?5, 23, ?6, ?7)";
            conn.execute(
                insert,
                params![
                    "data",
                    "2026-03-01T10:00:00+00:00",
                    "UnconfirmedDataUp",
                    "01ABCDEF",
                    None::<String>,
                    "AA:BB:CC:DD:00:11:22:33",
                    1
                ],
            )
            .unwrap();
            conn.execute(
                insert,
                params![
                    "join",
                    "2026-03-01T10:00:05+00:00",
                    "JoinRequest",
                    None::<String>,
                    QINGPING_EUI,
                    "AA:BB:CC:DD:00:11:22:33",
                    None::<i64>
                ],
            )
            .unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 1, "fixture must look like a 2.1.0 database");
            let has_labels: bool = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'device_labels')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(!has_labels, "a 2.1.0 database has no label table");
        }

        let s = Storage::open(&path).unwrap();
        assert_eq!(user_version(&s), MIGRATIONS.len() as i64);
        assert!(
            MIGRATIONS.len() >= 3,
            "the upgrade passes through the labels table"
        );

        // The table exists with the documented columns.
        let columns: Vec<String> = {
            let conn = s.conn.lock().unwrap();
            let mut stmt = conn.prepare("PRAGMA table_info(device_labels)").unwrap();
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows
        };
        assert_eq!(columns, ["device", "label", "updated_at"]);
        assert!(s.labels().unwrap().is_empty());

        // The rows from before the upgrade are intact.
        assert_eq!(s.count().unwrap(), 2);
        let rows = recent(&s, 10);
        let data = rows.iter().find(|r| r.id == "data").unwrap();
        assert_eq!(data.dev_addr.as_deref(), Some("01ABCDEF"));
        assert_eq!(data.gateway_eui.as_deref(), Some("AA:BB:CC:DD:00:11:22:33"));
        assert_eq!(data.adr, Some(true));
        let join = rows.iter().find(|r| r.id == "join").unwrap();
        assert_eq!(join.mtype, MType::JoinRequest);
        assert_eq!(join.dev_eui.as_deref(), Some(QINGPING_EUI));

        // Those rows make their devices labellable, and the label survives
        // another open (the migration must not run twice).
        assert_eq!(
            s.set_label("01ABCDEF", "old sensor", false, 10).unwrap(),
            LabelWrite::Set
        );
        assert_eq!(
            s.set_label(QINGPING_EUI, "old join", false, 10).unwrap(),
            LabelWrite::Set
        );
        drop(s);
        let s = Storage::open(&path).unwrap();
        assert_eq!(user_version(&s), MIGRATIONS.len() as i64);
        assert_eq!(s.count().unwrap(), 2);
        assert_eq!(s.labels().unwrap().len(), 2);
    }

    #[test]
    fn set_replace_get_and_delete_a_label() {
        let s = Storage::open_in_memory().unwrap();
        store_record(&s, HAVANA, Utc::now());
        assert!(s.labels().unwrap().is_empty());

        assert_eq!(
            s.set_label(HAVANA, "Qingping T&H Havana", false, 10)
                .unwrap(),
            LabelWrite::Set
        );
        let labels = s.labels().unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[HAVANA], "Qingping T&H Havana");

        // Replacing keeps one row and takes the new text.
        assert_eq!(
            s.set_label(HAVANA, "Havana kitchen", false, 10).unwrap(),
            LabelWrite::Set
        );
        let labels = s.labels().unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[HAVANA], "Havana kitchen");

        assert!(s.delete_label(HAVANA).unwrap());
        assert!(s.labels().unwrap().is_empty());
    }

    #[test]
    fn deleting_a_label_that_does_not_exist_is_not_an_error() {
        let s = Storage::open_in_memory().unwrap();
        assert!(!s.delete_label(HAVANA).unwrap());
        s.set_label(HAVANA, "x", true, 10).unwrap();
        assert!(s.delete_label(HAVANA).unwrap());
        assert!(!s.delete_label(HAVANA).unwrap());
    }

    #[test]
    fn deleting_one_label_leaves_the_others() {
        let s = Storage::open_in_memory().unwrap();
        s.set_label("00000001", "one", true, 10).unwrap();
        s.set_label("00000002", "two", true, 10).unwrap();
        assert!(s.delete_label("00000001").unwrap());
        let labels = s.labels().unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels["00000002"], "two");
    }

    #[test]
    fn updated_at_is_rfc3339_and_moves_forward_on_replace() {
        let s = Storage::open_in_memory().unwrap();
        s.set_label(HAVANA, "first", true, 10).unwrap();
        let read = |s: &Storage| -> DateTime<Utc> {
            let text: String = s
                .conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT updated_at FROM device_labels WHERE device = ?1",
                    params![HAVANA],
                    |r| r.get(0),
                )
                .unwrap();
            DateTime::parse_from_rfc3339(&text)
                .unwrap()
                .with_timezone(&Utc)
        };
        let first = read(&s);
        assert!((Utc::now() - first).num_seconds().abs() < 5);

        std::thread::sleep(std::time::Duration::from_millis(5));
        s.set_label(HAVANA, "second", true, 10).unwrap();
        assert!(read(&s) > first);
    }

    #[test]
    fn a_device_never_seen_and_not_stored_is_refused() {
        let s = Storage::open_in_memory().unwrap();
        store_record(&s, "01ABCDEF", Utc::now());
        store_record(&s, QINGPING_EUI, Utc::now());

        // Not stored, not seen live: nothing is written.
        assert_eq!(
            s.set_label(HAVANA, "ghost", false, 10).unwrap(),
            LabelWrite::UnknownDevice
        );
        // Close to a stored identifier is still a different identifier.
        assert_eq!(
            s.set_label("01ABCDE0", "ghost", false, 10).unwrap(),
            LabelWrite::UnknownDevice
        );
        assert_eq!(
            s.set_label("36:31:39:54:36:5D:05:3A", "ghost", false, 10)
                .unwrap(),
            LabelWrite::UnknownDevice
        );
        assert!(s.labels().unwrap().is_empty());
    }

    #[test]
    fn a_stored_devaddr_or_deveui_can_be_labelled_without_a_live_sighting() {
        let s = Storage::open_in_memory().unwrap();
        store_record(&s, "01ABCDEF", Utc::now());
        store_record(&s, QINGPING_EUI, Utc::now());

        assert_eq!(
            s.set_label("01ABCDEF", "uplink node", false, 10).unwrap(),
            LabelWrite::Set
        );
        assert_eq!(
            s.set_label(QINGPING_EUI, "joining node", false, 10)
                .unwrap(),
            LabelWrite::Set
        );
        let labels = s.labels().unwrap();
        assert_eq!(labels["01ABCDEF"], "uplink node");
        assert_eq!(labels[QINGPING_EUI], "joining node");
    }

    #[test]
    fn a_device_seen_only_live_can_be_labelled() {
        let s = Storage::open_in_memory().unwrap();
        assert_eq!(s.count().unwrap(), 0);

        assert_eq!(
            s.set_label(HAVANA, "live only", true, 10).unwrap(),
            LabelWrite::Set
        );
        assert_eq!(
            s.set_label(QINGPING_EUI, "live only eui", true, 10)
                .unwrap(),
            LabelWrite::Set
        );
        assert_eq!(s.labels().unwrap().len(), 2);
    }

    #[test]
    fn renaming_a_label_still_needs_the_device_to_be_known() {
        // The "known device" rule applies to every write, not only to the
        // first one: a label whose device has no stored message and is not
        // vouched for as seen live cannot be changed, and stays as it was.
        // (Removing it is always possible; see delete_label.)
        let s = Storage::open_in_memory().unwrap();
        s.set_label(HAVANA, "kept", true, 10).unwrap();
        assert_eq!(
            s.set_label(HAVANA, "renamed", false, 10).unwrap(),
            LabelWrite::UnknownDevice
        );
        assert_eq!(s.labels().unwrap()[HAVANA], "kept");
        assert!(s.delete_label(HAVANA).unwrap());
    }

    #[test]
    fn the_limit_refuses_new_labels_but_not_replacing_or_removing() {
        let s = Storage::open_in_memory().unwrap();
        for n in 1..=3 {
            let device = format!("0000000{n}");
            assert_eq!(
                s.set_label(&device, "x", true, 3).unwrap(),
                LabelWrite::Set,
                "label {n} fits"
            );
        }

        // A fourth device is refused and nothing is written.
        assert_eq!(
            s.set_label("00000004", "x", true, 3).unwrap(),
            LabelWrite::LimitReached
        );
        assert_eq!(s.labels().unwrap().len(), 3);
        assert!(!s.labels().unwrap().contains_key("00000004"));

        // Replacing an existing label still works at the limit.
        assert_eq!(
            s.set_label("00000002", "renamed", true, 3).unwrap(),
            LabelWrite::Set
        );
        assert_eq!(s.labels().unwrap()["00000002"], "renamed");

        // Removing one frees a slot.
        assert!(s.delete_label("00000001").unwrap());
        assert_eq!(
            s.set_label("00000004", "x", true, 3).unwrap(),
            LabelWrite::Set
        );
        assert_eq!(s.labels().unwrap().len(), 3);
    }

    #[test]
    fn a_limit_of_zero_refuses_every_new_label() {
        let s = Storage::open_in_memory().unwrap();
        assert_eq!(
            s.set_label(HAVANA, "x", true, 0).unwrap(),
            LabelWrite::LimitReached
        );
        assert!(s.labels().unwrap().is_empty());
    }

    #[test]
    fn deveui_and_devaddr_keys_sort_and_coexist() {
        let s = Storage::open_in_memory().unwrap();
        s.set_label(QINGPING_EUI, "eui", true, 10).unwrap();
        s.set_label(HAVANA, "addr", true, 10).unwrap();

        let labels = s.labels().unwrap();
        let keys: Vec<&str> = labels.keys().map(String::as_str).collect();
        assert_eq!(keys, [HAVANA, QINGPING_EUI]);
        assert_eq!(labels[HAVANA], "addr");
        assert_eq!(labels[QINGPING_EUI], "eui");
    }

    #[test]
    fn label_text_is_stored_verbatim() {
        let s = Storage::open_in_memory().unwrap();
        let texts = [
            "Qingping \"T&H\" Havana, ground floor",
            "温度センサー 🌡 café",
            "x'); DROP TABLE device_labels; --",
            "=1+1",
            "  inner  spaces  ",
        ];
        for (n, text) in texts.iter().enumerate() {
            let device = format!("0000000{n}");
            assert_eq!(
                s.set_label(&device, text, true, 10).unwrap(),
                LabelWrite::Set
            );
        }
        let labels = s.labels().unwrap();
        assert_eq!(labels.len(), texts.len());
        for (n, text) in texts.iter().enumerate() {
            assert_eq!(labels[&format!("0000000{n}")], *text);
        }
    }

    #[test]
    fn labels_survive_reopening_a_file_database() {
        let tmp = TempDb::new("labels-reopen");
        let path = tmp.path();
        {
            let s = Storage::open(&path).unwrap();
            store_record(&s, HAVANA, Utc::now());
            store_record(&s, QINGPING_EUI, Utc::now());
            s.set_label(HAVANA, "Havana", false, 10).unwrap();
            s.set_label(QINGPING_EUI, "Join sensor", false, 10).unwrap();
            s.set_label("DEADBEEF", "live only", true, 10).unwrap();
            assert!(s.delete_label("DEADBEEF").unwrap());
        }

        let s = Storage::open(&path).unwrap();
        let labels = s.labels().unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[HAVANA], "Havana");
        assert_eq!(labels[QINGPING_EUI], "Join sensor");

        // A sweep that keeps labels leaves them in place across the restart.
        let pruned = s
            .delete_older_than(Utc::now() + chrono::Duration::days(1), OrphanPolicy::Keep)
            .unwrap();
        assert_eq!(pruned.messages, 2);
        assert!(pruned.labels.is_empty());
        assert_eq!(s.labels().unwrap().len(), 2);
    }

    // -----------------------------------------------------------------------
    // Label lifetime
    // -----------------------------------------------------------------------

    /// Five labelled devices of one kind, in the states the lifetime rules
    /// care about, swept with `Keep` and then with `KeepIf`.
    ///
    /// `devices` are, in order: stored, orphan the predicate accepts, orphan
    /// the predicate rejects, expired-only the predicate rejects, expired-only
    /// the predicate accepts.
    fn check_lifetime_rules(devices: [&str; 5]) {
        let [stored, live_orphan, dead_orphan, expired_dead, expired_live] = devices;
        let accepted = [live_orphan, expired_live];

        let build = || {
            let s = Storage::open_in_memory().unwrap();
            // `stored` has a record that survives the sweep and one that
            // does not; the expired-only devices have nothing left after it.
            store_record(&s, stored, days_ago(1));
            store_record(&s, stored, days_ago(60));
            store_record(&s, expired_dead, days_ago(60));
            store_record(&s, expired_live, days_ago(61));
            for d in devices {
                assert_eq!(s.set_label(d, d, true, 10).unwrap(), LabelWrite::Set);
            }
            s
        };
        let cutoff = Utc::now() - chrono::Duration::days(30);
        let all: Vec<String> = {
            let mut v: Vec<String> = devices.iter().map(|d| d.to_string()).collect();
            v.sort();
            v
        };
        let label_keys = |s: &Storage| s.labels().unwrap().into_keys().collect::<Vec<_>>();

        // Keep: every label stays, whatever became of its messages.
        let s = build();
        let pruned = s.delete_older_than(cutoff, OrphanPolicy::Keep).unwrap();
        assert_eq!(pruned.messages, 3);
        assert!(pruned.labels.is_empty());
        assert_eq!(label_keys(&s), all);

        // KeepIf: a stored device is kept without asking, an accepted orphan
        // is kept, a rejected orphan is removed and reported.
        let s = build();
        let asked = std::cell::RefCell::new(Vec::<String>::new());
        let keep = |d: &str| {
            asked.borrow_mut().push(d.to_string());
            accepted.contains(&d)
        };
        let pruned = s
            .delete_older_than(cutoff, OrphanPolicy::KeepIf(&keep))
            .unwrap();
        assert_eq!(pruned.messages, 3);
        let mut removed = pruned.labels.clone();
        removed.sort();
        let mut expect_removed = vec![dead_orphan.to_string(), expired_dead.to_string()];
        expect_removed.sort();
        assert_eq!(removed, expect_removed);

        let mut kept = vec![
            stored.to_string(),
            live_orphan.to_string(),
            expired_live.to_string(),
        ];
        kept.sort();
        assert_eq!(label_keys(&s), kept);

        // The rule is consulted only for devices with no stored message.
        let mut asked = asked.into_inner();
        asked.sort();
        let mut orphans: Vec<String> = [live_orphan, dead_orphan, expired_dead, expired_live]
            .iter()
            .map(|d| d.to_string())
            .collect();
        orphans.sort();
        assert_eq!(asked, orphans);

        // A second sweep has nothing more to remove and the survivors stay.
        let again = s
            .delete_older_than(cutoff, OrphanPolicy::KeepIf(&|d| accepted.contains(&d)))
            .unwrap();
        assert_eq!(again.messages, 0);
        assert!(again.labels.is_empty());
        assert_eq!(label_keys(&s), kept);

        // Once the rule stops accepting an orphan, its label goes too.
        let last = s
            .delete_older_than(cutoff, OrphanPolicy::KeepIf(&|_| false))
            .unwrap();
        assert_eq!(last.messages, 0);
        let mut last_removed = last.labels;
        last_removed.sort();
        let mut expect_last = vec![live_orphan.to_string(), expired_live.to_string()];
        expect_last.sort();
        assert_eq!(last_removed, expect_last);
        assert_eq!(label_keys(&s), [stored.to_string()]);
    }

    #[test]
    fn lifetime_rules_for_a_devaddr() {
        check_lifetime_rules(["A0000001", "A0000002", "A0000003", "A0000004", "A0000005"]);
    }

    #[test]
    fn lifetime_rules_for_a_deveui() {
        check_lifetime_rules([
            "70:B3:D5:7E:D0:00:00:01",
            "70:B3:D5:7E:D0:00:00:02",
            "70:B3:D5:7E:D0:00:00:03",
            "70:B3:D5:7E:D0:00:00:04",
            "70:B3:D5:7E:D0:00:00:05",
        ]);
    }

    #[test]
    fn a_stored_record_in_either_column_protects_a_label() {
        // One DevAddr label and one DevEUI label, each backed by a message in
        // its own column.  A rule that rejects everything must not touch
        // either, which fails if only one column were consulted.
        let s = Storage::open_in_memory().unwrap();
        store_record(&s, HAVANA, days_ago(1));
        store_record(&s, QINGPING_EUI, days_ago(1));
        s.set_label(HAVANA, "addr", false, 10).unwrap();
        s.set_label(QINGPING_EUI, "eui", false, 10).unwrap();

        let pruned = s
            .delete_older_than(
                Utc::now() - chrono::Duration::days(30),
                OrphanPolicy::KeepIf(&|_| false),
            )
            .unwrap();
        assert_eq!(pruned.messages, 0);
        assert!(pruned.labels.is_empty());
        assert_eq!(s.labels().unwrap().len(), 2);
    }

    #[test]
    fn a_label_is_judged_after_its_last_message_expires_in_the_same_sweep() {
        let s = Storage::open_in_memory().unwrap();
        store_record(&s, HAVANA, days_ago(40));
        s.set_label(HAVANA, "was stored", false, 10).unwrap();

        // Before the sweep the device has a record, so no rule is asked; the
        // same sweep expires it and then removes the label.
        let pruned = s
            .delete_older_than(days_ago(30), OrphanPolicy::KeepIf(&|_| false))
            .unwrap();
        assert_eq!(pruned.messages, 1);
        assert_eq!(pruned.labels, [HAVANA]);
        assert!(s.labels().unwrap().is_empty());
    }

    #[test]
    fn keep_if_with_no_labels_only_deletes_messages() {
        let s = Storage::open_in_memory().unwrap();
        store_record(&s, HAVANA, days_ago(40));
        let pruned = s
            .delete_older_than(
                days_ago(30),
                OrphanPolicy::KeepIf(&|_| panic!("no label to ask about")),
            )
            .unwrap();
        assert_eq!(pruned.messages, 1);
        assert!(pruned.labels.is_empty());
    }

    // -----------------------------------------------------------------------
    // Malformed frames: persistence
    // -----------------------------------------------------------------------

    /// The `malformed` verdict with the body fields cleared, as the decoder
    /// leaves a record it rejects.
    fn malformed_record(id: &str, at: DateTime<Utc>, reason: Malformed) -> MessageRecord {
        let mut rec = sample_record();
        rec.id = id.to_string();
        rec.timestamp = at;
        rec.malformed = Some(reason);
        rec.dev_addr = None;
        rec.fcnt = None;
        rec.fport = None;
        rec
    }

    #[test]
    fn the_stored_reason_text_round_trips_and_anything_else_reads_as_none() {
        for reason in [Malformed::Crc, Malformed::Mhdr, Malformed::Length] {
            assert_eq!(parse_malformed(malformed_str(&reason)), Some(reason));
        }
        for text in ["", "CRC", "Length", "unknown", "0", "crc "] {
            assert_eq!(parse_malformed(text), None, "{text:?}");
        }

        // A value from a newer version, or a damaged one, reads as not
        // malformed instead of failing the whole query.
        let s = Storage::open_in_memory().unwrap();
        s.insert(&sample_record()).unwrap();
        s.conn
            .lock()
            .unwrap()
            .execute("UPDATE messages SET malformed = 'bogus'", [])
            .unwrap();
        assert_eq!(recent(&s, 10)[0].malformed, None);
    }

    #[test]
    fn each_reason_and_none_round_trip_and_every_inserted_row_is_marked_checked() {
        let s = Storage::open_in_memory().unwrap();
        let base = Utc::now();

        let mut valid = sample_record();
        valid.id = "valid".to_string();
        valid.timestamp = base;
        let records = [
            valid,
            malformed_record("crc", base + chrono::Duration::seconds(1), Malformed::Crc),
            malformed_record("mhdr", base + chrono::Duration::seconds(2), Malformed::Mhdr),
            malformed_record(
                "length",
                base + chrono::Duration::seconds(3),
                Malformed::Length,
            ),
        ];
        for rec in &records {
            s.insert(rec).unwrap();
        }

        let rows = recent(&s, 10);
        assert_eq!(rows.len(), 4);
        for expected in &records {
            let got = rows.iter().find(|r| r.id == expected.id).unwrap();
            assert_eq!(got.malformed, expected.malformed, "record {}", expected.id);
            assert_eq!(json(got), json(expected), "record {}", expected.id);
        }

        // The incremental query used by the API reads the column too.
        let rows = since(&s, base - chrono::Duration::seconds(1), 10);
        let reasons: Vec<Option<Malformed>> = rows.iter().map(|r| r.malformed).collect();
        assert_eq!(
            reasons,
            [
                None,
                Some(Malformed::Crc),
                Some(Malformed::Mhdr),
                Some(Malformed::Length)
            ]
        );

        // In the table the reason is the plain text of its name, and this
        // version says it has checked every row it writes.
        let conn = s.conn.lock().unwrap();
        let stored: Vec<(String, Option<String>)> = conn
            .prepare("SELECT id, malformed FROM messages ORDER BY timestamp")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let expected: Vec<(String, Option<String>)> = [
            ("valid", None),
            ("crc", Some("crc")),
            ("mhdr", Some("mhdr")),
            ("length", Some("length")),
        ]
        .into_iter()
        .map(|(id, reason)| (id.to_string(), reason.map(str::to_string)))
        .collect();
        assert_eq!(stored, expected);
        let unchecked: i64 = conn
            .query_row(COUNT_UNCLASSIFIED_SQL, [], |r| r.get(0))
            .unwrap();
        assert_eq!(unchecked, 0);
        let checked: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE classified = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(checked, 4);
    }

    #[test]
    fn replacing_a_record_keeps_it_checked() {
        let s = Storage::open_in_memory().unwrap();
        let mut rec = sample_record();
        rec.id = "same".to_string();
        s.insert(&rec).unwrap();
        s.insert(&malformed_record("same", rec.timestamp, Malformed::Crc))
            .unwrap();

        let rows = recent(&s, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].malformed, Some(Malformed::Crc));
        let unchecked: i64 = s
            .conn
            .lock()
            .unwrap()
            .query_row(COUNT_UNCLASSIFIED_SQL, [], |r| r.get(0))
            .unwrap();
        assert_eq!(unchecked, 0);
    }

    // -----------------------------------------------------------------------
    // Rows written by older versions
    // -----------------------------------------------------------------------

    /// The `device_labels` table as the previous version created it, which
    /// makes the database one at schema version 2.
    const SCHEMA_2_LABELS: &str = "
    CREATE TABLE device_labels (
        device     TEXT PRIMARY KEY,
        label      TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    PRAGMA user_version = 2;
    ";

    /// The column list the previous version inserted with: no `malformed`, no
    /// `classified`.  Frozen, like the schemas above.
    const INSERT_SCHEMA_2: &str = "
    INSERT OR REPLACE INTO messages (
        id, timestamp, direction, status, blocked_by, source_protocol,
        freq_mhz, datarate, rssi, snr, crc_status,
        mtype, dev_addr, dev_eui, fcnt, fport, payload_size, toa_ms,
        gateway_eui, join_eui, dev_nonce,
        adr, adr_ack_req, ack, class_b, f_pending, f_opts_len
    ) VALUES (
        ?1,  ?2,  ?3,  ?4,  ?5,  ?6,
        ?7,  ?8,  ?9,  ?10, ?11,
        ?12, ?13, ?14, ?15, ?16, ?17, ?18,
        ?19, ?20, ?21,
        ?22, ?23, ?24, ?25, ?26, ?27
    )";

    /// The columns the previous version selected.  Followed by `NULL` in a
    /// query, they give `row_to_record` the column it expects at the end.
    const SELECT_SCHEMA_2_COLS: &str = "
        id, timestamp, direction, status, blocked_by, source_protocol,
        freq_mhz, datarate, rssi, snr, crc_status,
        mtype, dev_addr, dev_eui, fcnt, fport, payload_size, toa_ms,
        gateway_eui, join_eui, dev_nonce,
        adr, adr_ack_req, ack, class_b, f_pending, f_opts_len
    ";

    /// The twelve fields read from a frame's body, which a malformed record
    /// does not carry.
    const BODY_KEYS: [&str; 12] = [
        "dev_addr",
        "dev_eui",
        "join_eui",
        "dev_nonce",
        "fcnt",
        "fport",
        "adr",
        "adr_ack_req",
        "ack",
        "class_b",
        "f_pending",
        "f_opts_len",
    ];

    /// A row as the previous version stored it.
    #[derive(Clone)]
    struct OldRow {
        id: String,
        timestamp: String,
        direction: &'static str,
        crc_status: Option<&'static str>,
        mtype: &'static str,
        dev_addr: Option<String>,
        dev_eui: Option<String>,
        join_eui: Option<String>,
        dev_nonce: Option<i64>,
        fcnt: Option<i64>,
        fport: Option<i64>,
        payload_size: i64,
        /// ADR, ADRACKReq, ACK, ClassB, FPending.
        flags: [Option<i64>; 5],
        f_opts_len: Option<i64>,
    }

    impl OldRow {
        /// An uplink that carries no identifier and no header field.
        fn bare(id: &str, mtype: &'static str, size: i64) -> Self {
            Self {
                id: id.to_string(),
                timestamp: "2026-03-01T10:00:00+00:00".to_string(),
                direction: "up",
                crc_status: Some("ok"),
                mtype,
                dev_addr: None,
                dev_eui: None,
                join_eui: None,
                dev_nonce: None,
                fcnt: None,
                fport: None,
                payload_size: size,
                flags: [None; 5],
                f_opts_len: None,
            }
        }

        /// An uplink data frame with every header field filled in.
        fn data(id: &str, mtype: &'static str, size: i64) -> Self {
            Self {
                dev_addr: Some("01ABCDEF".to_string()),
                fcnt: Some(1234),
                fport: Some(1),
                flags: [Some(1), Some(0), Some(0), Some(1), None],
                f_opts_len: Some(0),
                ..Self::bare(id, mtype, size)
            }
        }

        /// A JoinRequest with its EUIs and nonce.
        fn join(id: &str, size: i64) -> Self {
            Self {
                dev_eui: Some("01:02:03:04:05:06:07:08".to_string()),
                join_eui: Some("00:00:00:00:00:00:00:01".to_string()),
                dev_nonce: Some(0xBEEF),
                ..Self::bare(id, "JoinRequest", size)
            }
        }

        fn downlink(mut self) -> Self {
            self.direction = "down";
            self.crc_status = None;
            self.flags = [Some(1), None, Some(0), None, Some(1)];
            self
        }

        fn crc(mut self, status: Option<&'static str>) -> Self {
            self.crc_status = status;
            self
        }

        fn f_opts(mut self, len: Option<i64>) -> Self {
            self.f_opts_len = len;
            self
        }

        fn dev_addr(mut self, dev_addr: &str) -> Self {
            self.dev_addr = Some(dev_addr.to_string());
            self
        }

        /// Seconds after the common start time, to order rows.
        fn at(mut self, seconds: i64) -> Self {
            let start = DateTime::parse_from_rfc3339("2026-03-01T10:00:00+00:00").unwrap();
            self.timestamp = (start + chrono::Duration::seconds(seconds)).to_rfc3339();
            self
        }
    }

    /// Insert a row through the column list of the previous version.
    fn insert_old(conn: &Connection, row: &OldRow) {
        let up = row.direction == "up";
        conn.prepare_cached(INSERT_SCHEMA_2)
            .unwrap()
            .execute(params![
                row.id,
                row.timestamp,
                row.direction,
                "forwarded",
                None::<String>,
                "udp",
                868.1,
                "SF7BW125",
                up.then_some(-55),
                up.then_some(8.5),
                row.crc_status,
                row.mtype,
                row.dev_addr,
                row.dev_eui,
                row.fcnt,
                row.fport,
                row.payload_size,
                Some(61.7),
                "AA:BB:CC:DD:00:11:22:33",
                row.join_eui,
                row.dev_nonce,
                row.flags[0],
                row.flags[1],
                row.flags[2],
                row.flags[3],
                row.flags[4],
                row.f_opts_len,
            ])
            .unwrap();
    }

    fn insert_old_rows(path: &str, rows: &[OldRow]) {
        let conn = Connection::open(path).unwrap();
        for row in rows {
            insert_old(&conn, row);
        }
    }

    /// A file database as the previous version leaves it: schema version 2,
    /// holding `rows`.
    fn schema_2_database(path: &str, rows: &[OldRow]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA_V2_0_0).unwrap();
        conn.execute_batch(SCHEMA_V2_1_0_UPGRADE).unwrap();
        conn.execute_batch(SCHEMA_2_LABELS).unwrap();
        for row in rows {
            insert_old(&conn, row);
        }
        assert_eq!(schema_version(&conn).unwrap(), 2);
    }

    fn read_records(conn: &Connection, sql: &str) -> BTreeMap<String, MessageRecord> {
        let mut stmt = conn.prepare(sql).unwrap();
        let rows = stmt
            .query_map([], row_to_record)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.into_iter().map(|r| (r.id.clone(), r)).collect()
    }

    /// The rows of a schema 2 database, read the way the previous version did.
    fn records_at_schema_2(path: &str) -> BTreeMap<String, MessageRecord> {
        let conn = Connection::open(path).unwrap();
        read_records(
            &conn,
            &format!("SELECT {SELECT_SCHEMA_2_COLS}, NULL FROM messages"),
        )
    }

    fn records(s: &Storage) -> BTreeMap<String, MessageRecord> {
        let conn = s.conn.lock().unwrap();
        read_records(&conn, &format!("SELECT {SELECT_COLS} FROM messages"))
    }

    fn scalar(s: &Storage, sql: &str) -> i64 {
        s.conn
            .lock()
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    fn unchecked_rows(s: &Storage) -> i64 {
        scalar(s, COUNT_UNCLASSIFIED_SQL)
    }

    fn index_exists(s: &Storage) -> bool {
        scalar(s, UNCLASSIFIED_INDEX_EXISTS_SQL) != 0
    }

    type Lines = Vec<(Level, String)>;

    fn open_logged(path: &str) -> (Storage, Lines) {
        let (s, lines) = log_capture::capture(|| Storage::open(path));
        (s.unwrap(), lines)
    }

    fn logged(lines: &Lines, level: Level, text: &str) -> bool {
        lines.iter().any(|(l, m)| *l == level && m.contains(text))
    }

    fn mentions(lines: &Lines, text: &str) -> bool {
        lines.iter().any(|(_, m)| m.contains(text))
    }

    /// What a record becomes when it was `before` and is now judged `reason`:
    /// the same, with the body fields gone and the reason set.
    fn expected_json(before: &MessageRecord, reason: Option<Malformed>) -> serde_json::Value {
        let mut expected = json(before);
        if let Some(reason) = reason {
            for key in BODY_KEYS {
                expected[key] = serde_json::Value::Null;
            }
            expected["malformed"] = serde_json::json!(reason.as_str());
        }
        expected
    }

    #[test]
    fn upgrades_a_schema_2_database_classifying_existing_rows() {
        use Malformed::{Crc, Length};

        let cases: Vec<(OldRow, Option<Malformed>)> = vec![
            (OldRow::data("valid", "UnconfirmedDataUp", 23).at(1), None),
            (
                OldRow::data("crc", "UnconfirmedDataUp", 23)
                    .crc(Some("fail"))
                    .at(2),
                Some(Crc),
            ),
            (
                OldRow::data("short", "UnconfirmedDataUp", 9).at(3),
                Some(Length),
            ),
            // 14 bytes cannot hold 3 bytes of FOpts, 15 can.
            (
                OldRow::data("fopts-short", "ConfirmedDataUp", 14)
                    .f_opts(Some(3))
                    .at(4),
                Some(Length),
            ),
            (
                OldRow::data("fopts-fits", "ConfirmedDataUp", 15)
                    .f_opts(Some(3))
                    .at(5),
                None,
            ),
            // A row from 2.0.0 has no FOptsLen: only the 12-byte floor applies.
            (
                OldRow::data("no-fopts-len", "UnconfirmedDataUp", 12)
                    .f_opts(None)
                    .at(6),
                None,
            ),
            (OldRow::join("join", 23).at(7), None),
            (OldRow::join("join-20", 20).at(8), Some(Length)),
            (OldRow::bare("accept-17", "JoinAccept", 17).at(9), None),
            (
                OldRow::bare("accept-10", "JoinAccept", 10).at(10),
                Some(Length),
            ),
            (OldRow::bare("rejoin-19", "RejoinRequest", 19).at(11), None),
            (OldRow::bare("rejoin-24", "RejoinRequest", 24).at(12), None),
            (
                OldRow::bare("rejoin-20", "RejoinRequest", 20).at(13),
                Some(Length),
            ),
            (OldRow::bare("proprietary", "Proprietary", 5).at(14), None),
            (OldRow::bare("unknown", "Unknown", 1).at(15), None),
            (
                OldRow::data("downlink", "UnconfirmedDataDown", 13)
                    .downlink()
                    .at(16),
                None,
            ),
            (
                OldRow::data("no-crc", "UnconfirmedDataUp", 23)
                    .crc(Some("no_crc"))
                    .at(17),
                None,
            ),
            // The failed CRC is the reason even when the length is wrong too.
            (
                OldRow::data("crc-and-short", "UnconfirmedDataUp", 9)
                    .crc(Some("fail"))
                    .at(18),
                Some(Crc),
            ),
            (
                OldRow::data("one-byte", "ConfirmedDataDown", 1)
                    .crc(Some("fail"))
                    .at(19),
                Some(Crc),
            ),
            (
                OldRow::join("join-crc", 23).crc(Some("fail")).at(20),
                Some(Crc),
            ),
        ];
        let rows: Vec<OldRow> = cases.iter().map(|(row, _)| row.clone()).collect();

        let tmp = TempDb::new("classify-existing");
        let path = tmp.path();
        schema_2_database(&path, &rows);
        let before = records_at_schema_2(&path);
        assert_eq!(before.len(), rows.len());

        let (s, lines) = open_logged(&path);

        assert_eq!(s.count().unwrap(), rows.len() as i64);
        assert_eq!(user_version(&s), MIGRATIONS.len() as i64);
        assert!(MIGRATIONS.len() >= 3);
        assert_eq!(unchecked_rows(&s), 0);
        assert_eq!(
            scalar(&s, "SELECT COUNT(*) FROM messages WHERE classified = 1"),
            rows.len() as i64
        );
        assert!(index_exists(&s));

        let after = records(&s);
        let mut marked = 0;
        for (row, reason) in &cases {
            let got = &after[&row.id];
            assert_eq!(got.malformed, *reason, "row {}", row.id);
            assert_eq!(
                json(got),
                expected_json(&before[&row.id], *reason),
                "row {}",
                row.id
            );
            if reason.is_some() {
                crate::core::metadata::assert_body_cleared(got);
                marked += 1;
            }
        }

        // The log says what is happening before it does it, and what it did.
        let upgraded = format!(
            "Database schema upgraded from version 2 to {}",
            MIGRATIONS.len()
        );
        assert!(logged(&lines, Level::Info, &upgraded), "{lines:?}");
        assert!(
            logged(
                &lines,
                Level::Info,
                &format!(
                    "Checking every stored row for malformed frames (about {} rows); \
                     forwarding starts when this is done",
                    rows.len()
                )
            ),
            "{lines:?}"
        );
        assert!(
            logged(
                &lines,
                Level::Info,
                &format!("Checked {} stored rows, {marked} malformed, in", rows.len())
            ),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|(l, _)| matches!(l, Level::Warn | Level::Error)),
            "{lines:?}"
        );

        // A second start finds nothing to check and changes nothing.
        let snapshot: Vec<serde_json::Value> = after.values().map(json).collect();
        drop(s);
        let (s, lines) = open_logged(&path);
        assert!(!mentions(&lines, "Checking"), "{lines:?}");
        assert!(!mentions(&lines, "Checked"), "{lines:?}");
        assert!(!mentions(&lines, "upgraded"), "{lines:?}");
        let again: Vec<serde_json::Value> = records(&s).values().map(json).collect();
        assert_eq!(again, snapshot);
        assert_eq!(unchecked_rows(&s), 0);
    }

    #[test]
    fn a_new_database_says_it_was_created_and_checks_nothing() {
        let tmp = TempDb::new("created");
        let (s, lines) = open_logged(&tmp.path());
        assert!(
            logged(
                &lines,
                Level::Info,
                &format!("Database created at schema version {}", MIGRATIONS.len())
            ),
            "{lines:?}"
        );
        assert!(!mentions(&lines, "Check"), "{lines:?}");
        assert!(index_exists(&s));
        drop(s);

        // Opening it again says nothing about the schema.
        let (_s, lines) = open_logged(&tmp.path());
        assert!(!mentions(&lines, "Database"), "{lines:?}");
    }

    /// Every stored type and size, with every FOptsLen and CRC status, goes
    /// through the SQL classification; the verdict must be the decoder's.
    #[test]
    fn the_classification_agrees_with_the_decoder_rules() {
        use crate::core::decoder::{crc_failed, length_fits};

        let mtypes = [
            MType::JoinRequest,
            MType::JoinAccept,
            MType::UnconfirmedDataUp,
            MType::UnconfirmedDataDown,
            MType::ConfirmedDataUp,
            MType::ConfirmedDataDown,
            MType::RejoinRequest,
            MType::Proprietary,
            MType::Unknown,
        ];
        let sizes: Vec<i64> = (0..=40).chain([255]).collect();
        let f_opts_lens: Vec<Option<i64>> =
            std::iter::once(None).chain((0..=15).map(Some)).collect();
        let crcs = [
            None,
            Some(CrcStatus::Ok),
            Some(CrcStatus::Fail),
            Some(CrcStatus::NoCrc),
        ];

        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V2_0_0).unwrap();
        conn.execute_batch(SCHEMA_V2_1_0_UPGRADE).unwrap();
        conn.execute_batch(SCHEMA_2_LABELS).unwrap();

        // Rows go in one after the other, so reading them back by row number
        // pairs each with the inputs that made it.
        let mut inputs = Vec::new();
        {
            let tx = conn.transaction().unwrap();
            for mtype in &mtypes {
                for &size in &sizes {
                    for &f_opts_len in &f_opts_lens {
                        for crc in &crcs {
                            let id = format!("row-{}", inputs.len());
                            let row = OldRow::data(&id, mtype_str(mtype), size)
                                .crc(crc.as_ref().map(crc_str))
                                .f_opts(f_opts_len);
                            insert_old(&tx, &row);
                            inputs.push((mtype.clone(), size, f_opts_len, crc.clone()));
                        }
                    }
                }
            }
            tx.commit().unwrap();
        }
        assert_eq!(inputs.len(), 9 * 42 * 17 * 4);

        init(&mut conn).unwrap();

        let stored: Vec<Option<Malformed>> = conn
            .prepare("SELECT malformed FROM messages ORDER BY rowid")
            .unwrap()
            .query_map([], |r| {
                Ok(r.get::<_, Option<String>>(0)?
                    .as_deref()
                    .and_then(parse_malformed))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(stored.len(), inputs.len());

        let (mut crc_rows, mut length_rows, mut valid_rows) = (0, 0, 0);
        for ((mtype, size, f_opts_len, crc), got) in inputs.iter().zip(&stored) {
            // The stored form has no FOptsLen on a 2.0.0 row, so the data
            // rule sees 0 there.
            let expected = if crc_failed(crc.as_ref()) {
                Some(Malformed::Crc)
            } else if !length_fits(mtype, *size as usize, f_opts_len.unwrap_or(0) as u8) {
                Some(Malformed::Length)
            } else {
                None
            };
            assert_eq!(
                *got, expected,
                "{mtype:?}, {size} bytes, FOptsLen {f_opts_len:?}, CRC {crc:?}"
            );
            match expected {
                Some(Malformed::Crc) => crc_rows += 1,
                Some(_) => length_rows += 1,
                None => valid_rows += 1,
            }
        }
        // The comparison above would pass trivially if one class were empty.
        assert!(crc_rows > 0 && length_rows > 0 && valid_rows > 0);

        // Whatever was marked has lost its body fields, and nothing else has.
        let cleared_wrongly: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE malformed IS NOT NULL AND (
                    dev_addr IS NOT NULL OR dev_eui IS NOT NULL OR join_eui IS NOT NULL
                    OR dev_nonce IS NOT NULL OR fcnt IS NOT NULL OR fport IS NOT NULL
                    OR adr IS NOT NULL OR adr_ack_req IS NOT NULL OR ack IS NOT NULL
                    OR class_b IS NOT NULL OR f_pending IS NOT NULL OR f_opts_len IS NOT NULL)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cleared_wrongly, 0);
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE malformed IS NULL AND dev_addr = '01ABCDEF'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, valid_rows);
    }

    /// Two copies of a schema 2 database with a label on a DevAddr seen only
    /// in a CRC-failed row, one on a DevAddr seen in a valid row.
    fn labelled_schema_2_database(path: &str) {
        schema_2_database(
            path,
            &[
                OldRow::data("noise", "UnconfirmedDataUp", 23)
                    .crc(Some("fail"))
                    .dev_addr("6874FA22")
                    .at(1),
                OldRow::data("real", "UnconfirmedDataUp", 23).at(2),
            ],
        );
        let conn = Connection::open(path).unwrap();
        for (device, label) in [("6874FA22", "noise"), ("01ABCDEF", "control")] {
            conn.execute(
                "INSERT INTO device_labels (device, label, updated_at) VALUES (?1, ?2, ?3)",
                params![device, label, "2026-03-01T10:00:00+00:00"],
            )
            .unwrap();
        }
    }

    #[test]
    fn a_label_on_an_identifier_seen_only_in_malformed_rows_follows_the_lifetime_rule() {
        let long_ago = days_ago(36_500);

        // Under `Keep` both labels stay, whatever became of their rows.
        let tmp = TempDb::new("label-keep");
        labelled_schema_2_database(&tmp.path());
        let s = Storage::open(&tmp.path()).unwrap();
        assert!(records(&s)["noise"].malformed.is_some());
        let pruned = s.delete_older_than(long_ago, OrphanPolicy::Keep).unwrap();
        assert_eq!(pruned.messages, 0);
        assert!(pruned.labels.is_empty());
        assert_eq!(s.labels().unwrap().len(), 2);

        // Under a rule that accepts no orphan, the device that now has no
        // stored identifier loses its label; the one on a valid row does not.
        let tmp = TempDb::new("label-prune");
        labelled_schema_2_database(&tmp.path());
        let s = Storage::open(&tmp.path()).unwrap();
        let pruned = s
            .delete_older_than(long_ago, OrphanPolicy::KeepIf(&|_| false))
            .unwrap();
        assert_eq!(pruned.messages, 0);
        assert_eq!(pruned.labels, ["6874FA22"]);
        let labels = s.labels().unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels["01ABCDEF"], "control");

        // Nothing is left to label with the fake identifier.
        assert_eq!(
            s.set_label("6874FA22", "again", false, 10).unwrap(),
            LabelWrite::UnknownDevice
        );
    }

    // -----------------------------------------------------------------------
    // Rows an older version writes after the upgrade
    // -----------------------------------------------------------------------

    /// Rows an older version records, `copies` of each kind: a CRC-failed
    /// uplink carrying the DevAddr of the lns01 noise, a 9-byte uplink with a
    /// DevAddr, and a valid uplink.
    fn old_rows(prefix: &str, copies: usize) -> Vec<(OldRow, Option<Malformed>)> {
        let mut rows = Vec::new();
        for n in 0..copies {
            rows.push((
                OldRow::data(&format!("{prefix}-{n}-crc"), "UnconfirmedDataUp", 23)
                    .crc(Some("fail"))
                    .dev_addr("6874FA22")
                    .at(1000 + n as i64),
                Some(Malformed::Crc),
            ));
            rows.push((
                OldRow::data(&format!("{prefix}-{n}-short"), "UnconfirmedDataUp", 9)
                    .dev_addr("0BADADD0")
                    .at(2000 + n as i64),
                Some(Malformed::Length),
            ));
            rows.push((
                OldRow::data(&format!("{prefix}-{n}-valid"), "UnconfirmedDataUp", 23)
                    .at(500 + n as i64),
                None,
            ));
        }
        rows
    }

    fn plain_rows(old: &[(OldRow, Option<Malformed>)]) -> Vec<OldRow> {
        old.iter().map(|(row, _)| row.clone()).collect()
    }

    /// Every row of `old` is in `s`, with the verdict it should have, the
    /// body fields of a malformed one gone and those of a valid one intact.
    /// And no row in the table is left unchecked.
    fn assert_checked(s: &Storage, old: &[(OldRow, Option<Malformed>)]) {
        let all = records(s);
        for (row, reason) in old {
            let got = &all[&row.id];
            assert_eq!(got.malformed, *reason, "row {}", row.id);
            assert_eq!(got.mtype, mtype_of(row.mtype), "row {}", row.id);
            assert_eq!(got.payload_size as i64, row.payload_size, "row {}", row.id);
            match reason {
                Some(_) => crate::core::metadata::assert_body_cleared(got),
                None => {
                    assert_eq!(got.dev_addr, row.dev_addr, "row {}", row.id);
                    assert_eq!(got.fcnt.map(i64::from), row.fcnt, "row {}", row.id);
                }
            }
        }
        assert_eq!(unchecked_rows(s), 0);
        assert!(index_exists(s));
    }

    fn mtype_of(text: &str) -> MType {
        [
            MType::JoinRequest,
            MType::JoinAccept,
            MType::UnconfirmedDataUp,
            MType::UnconfirmedDataDown,
            MType::ConfirmedDataUp,
            MType::ConfirmedDataDown,
            MType::RejoinRequest,
            MType::Proprietary,
            MType::Unknown,
        ]
        .into_iter()
        .find(|m| mtype_str(m) == text)
        .unwrap()
    }

    /// A database this version has opened and filled with `rows` records of
    /// its own, closed again.
    fn current_database(tmp: &TempDb, rows: usize) {
        let s = Storage::open(&tmp.path()).unwrap();
        for n in 0..rows {
            store_record(&s, &format!("{:08X}", 0x0A00_0000 + n), days_ago(1));
        }
    }

    #[test]
    fn rows_recorded_by_an_older_version_are_classified_on_the_next_start() {
        let tmp = TempDb::new("rollback");
        let path = tmp.path();
        current_database(&tmp, 4);

        let old = old_rows("old", 1);
        insert_old_rows(&path, &plain_rows(&old));
        {
            // The older version also labelled a device it believed in.
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO device_labels (device, label, updated_at) VALUES ('6874FA22', 'noise', 'x')",
                [],
            )
            .unwrap();
        }

        let (s, lines) = open_logged(&path);
        assert!(
            logged(
                &lines,
                Level::Info,
                "Checking 3 stored rows recorded by an older version for malformed frames"
            ),
            "{lines:?}"
        );
        assert!(!mentions(&lines, "Checking every"), "{lines:?}");
        assert!(
            logged(
                &lines,
                Level::Info,
                "Checked 3 stored rows, 2 malformed, in"
            ),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|(l, _)| matches!(l, Level::Warn | Level::Error)),
            "{lines:?}"
        );
        assert_checked(&s, &old);
        assert_eq!(s.count().unwrap(), 7);
        assert_eq!(
            scalar(
                &s,
                "SELECT COUNT(*) FROM messages WHERE dev_addr = '6874FA22'"
            ),
            0
        );

        // The label on the fake DevAddr follows the lifetime rule now that no
        // row carries that identifier.
        let long_ago = days_ago(36_500);
        let kept = s.delete_older_than(long_ago, OrphanPolicy::Keep).unwrap();
        assert!(kept.labels.is_empty());
        assert_eq!(s.labels().unwrap().len(), 1);
        let pruned = s
            .delete_older_than(long_ago, OrphanPolicy::KeepIf(&|_| false))
            .unwrap();
        assert_eq!(pruned.labels, ["6874FA22"]);
        assert!(s.labels().unwrap().is_empty());

        // The next start has nothing to check.
        drop(s);
        let (_s, lines) = open_logged(&path);
        assert!(!mentions(&lines, "Check"), "{lines:?}");
    }

    #[test]
    fn rowid_reuse_does_not_hide_older_rows() {
        let tmp = TempDb::new("rowid-reuse");
        let path = tmp.path();
        current_database(&tmp, 10);

        // The newest rows go, and the rows the older version writes next take
        // their row numbers.  A mark that said "rows up to number 10 are
        // checked" would now hide them.
        let old = old_rows("reuse", 1);
        {
            let conn = Connection::open(&path).unwrap();
            let highest: i64 = conn
                .query_row("SELECT MAX(rowid) FROM messages", [], |r| r.get(0))
                .unwrap();
            conn.execute("DELETE FROM messages WHERE rowid > 5", [])
                .unwrap();
            for (row, _) in &old {
                insert_old(&conn, row);
            }
            let lowest_new: i64 = conn
                .query_row(
                    "SELECT MIN(rowid) FROM messages WHERE classified = 0",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(lowest_new <= highest, "{lowest_new} > {highest}");
        }

        let (s, lines) = open_logged(&path);
        assert!(mentions(&lines, "Checking 3 stored rows"), "{lines:?}");
        assert_checked(&s, &old);
        assert_eq!(s.count().unwrap(), 8);
    }

    #[test]
    fn an_emptied_table_does_not_hide_older_rows() {
        let tmp = TempDb::new("emptied");
        let path = tmp.path();
        current_database(&tmp, 5);

        // Retention removed everything; the older version then refills the
        // table past the size it had.
        let old = old_rows("refill", 3);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute("DELETE FROM messages", []).unwrap();
            for (row, _) in &old {
                insert_old(&conn, row);
            }
        }
        assert!(old.len() > 5);

        let (s, lines) = open_logged(&path);
        assert!(mentions(&lines, "Checking 9 stored rows"), "{lines:?}");
        assert_checked(&s, &old);
        assert_eq!(s.count().unwrap(), 9);
    }

    #[test]
    fn vacuum_does_not_hide_older_rows() {
        let tmp = TempDb::new("vacuum");
        let path = tmp.path();
        current_database(&tmp, 10);

        let old = old_rows("vacuum", 2);
        {
            let conn = Connection::open(&path).unwrap();
            // Gaps in the row numbers, so that VACUUM has something to move.
            conn.execute("DELETE FROM messages WHERE rowid IN (2, 3, 5, 8)", [])
                .unwrap();
            for (row, _) in &old {
                insert_old(&conn, row);
            }
            conn.execute_batch("VACUUM").unwrap();
        }

        let (s, lines) = open_logged(&path);
        assert!(mentions(&lines, "Checking 6 stored rows"), "{lines:?}");
        assert_checked(&s, &old);
        assert_eq!(s.count().unwrap(), 12);
    }

    #[test]
    fn repeated_rollbacks() {
        let tmp = TempDb::new("repeated");
        let path = tmp.path();
        current_database(&tmp, 6);

        // First rollback.
        let first = old_rows("first", 2);
        insert_old_rows(&path, &plain_rows(&first));
        let (s, lines) = open_logged(&path);
        assert!(mentions(&lines, "Checking 6 stored rows"), "{lines:?}");
        assert_checked(&s, &first);

        // A retention pass removes the oldest rows, which are the valid ones
        // of the first rollback, among them the one with the highest row
        // number.  The next rows the older version writes take that number.
        let cutoff = DateTime::parse_from_rfc3339("2026-03-01T10:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::seconds(800);
        let pruned = s.delete_older_than(cutoff, OrphanPolicy::Keep).unwrap();
        assert!(pruned.messages > 0);
        let survivors: Vec<_> = first
            .iter()
            .filter(|(row, _)| row.timestamp.as_str() >= cutoff.to_rfc3339().as_str())
            .cloned()
            .collect();
        drop(s);

        // Second rollback.
        let second = old_rows("second", 3);
        insert_old_rows(&path, &plain_rows(&second));
        let (s, lines) = open_logged(&path);
        assert!(mentions(&lines, "Checking 9 stored rows"), "{lines:?}");
        assert_checked(&s, &second);
        // The rows the first round already settled are as they were.
        assert!(!survivors.is_empty());
        assert_checked(&s, &survivors);
    }

    #[test]
    fn a_start_after_a_crash_has_nothing_to_check() {
        let tmp = TempDb::new("crash");
        let path = tmp.path();
        let s = Storage::open(&path).unwrap();
        s.insert(&sample_record()).unwrap();
        // Bad header and wrong length at once: the header is the reason.
        let mut rec = malformed_record("mhdr-and-short", Utc::now(), Malformed::Mhdr);
        rec.payload_size = 9;
        s.insert(&rec).unwrap();
        s.insert(&malformed_record("crc", Utc::now(), Malformed::Crc))
            .unwrap();

        // Copy the files while the sniffer still has the database open: what
        // a power cut would leave, with the rows only in the write-ahead log.
        let crashed = tmp.dir.join("crashed.db");
        let wal = format!("{path}-wal");
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        std::fs::copy(&path, &crashed).unwrap();
        std::fs::copy(&wal, tmp.dir.join("crashed.db-wal")).unwrap();

        let (c, lines) = open_logged(&crashed.to_string_lossy());
        assert_eq!(
            c.count().unwrap(),
            3,
            "the rows were recovered from the log"
        );
        assert!(!mentions(&lines, "Check"), "{lines:?}");
        let all = records(&c);
        assert_eq!(all["mhdr-and-short"].malformed, Some(Malformed::Mhdr));
        assert_eq!(all["crc"].malformed, Some(Malformed::Crc));
        assert_eq!(unchecked_rows(&c), 0);

        // The count that decides whether to check anything reads the index,
        // not the table.
        let conn = c.conn.lock().unwrap();
        let plan = query_plan(&conn, COUNT_UNCLASSIFIED_SQL, &[]);
        assert!(
            plan.iter().any(|l| l.contains("messages_unclassified")),
            "plan: {plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|l| l.contains("SCAN messages") && !l.contains("INDEX messages_unclassified")),
            "plan: {plan:?}"
        );
    }

    // -----------------------------------------------------------------------
    // A pass that fails
    // -----------------------------------------------------------------------

    /// Rows for the failure tests: some of every verdict, with fake
    /// identifiers in the ones that must lose them.
    fn failure_fixture_rows() -> Vec<(OldRow, Option<Malformed>)> {
        let mut rows = old_rows("fx", 4);
        rows.push((
            OldRow::join("fx-join-20", 20).at(4000),
            Some(Malformed::Length),
        ));
        rows
    }

    /// The pass failed and was rolled back whole: the database is open at the
    /// latest schema, no row has been touched, and there is no index.
    fn assert_untouched_after_failed_pass(
        s: &Storage,
        lines: &Lines,
        before: &BTreeMap<String, MessageRecord>,
        error_text: &str,
    ) {
        assert_eq!(user_version(s), MIGRATIONS.len() as i64);
        assert!(
            logged(
                lines,
                Level::Error,
                "Could not check stored rows for malformed frames"
            ),
            "{lines:?}"
        );
        assert!(logged(lines, Level::Error, error_text), "{lines:?}");
        assert!(
            logged(lines, Level::Error, "will retry at the next start"),
            "{lines:?}"
        );
        assert!(!mentions(lines, "Checked"), "{lines:?}");

        assert_eq!(s.count().unwrap(), before.len() as i64);
        assert_eq!(
            scalar(s, "SELECT COUNT(*) FROM messages WHERE classified = 0"),
            before.len() as i64
        );
        assert_eq!(
            scalar(
                s,
                "SELECT COUNT(*) FROM messages WHERE malformed IS NOT NULL"
            ),
            0
        );
        let after = records(s);
        for (id, rec) in before {
            assert_eq!(json(&after[id]), json(rec), "row {id}");
        }
        assert!(
            before
                .values()
                .any(|r| r.dev_addr.as_deref() == Some("6874FA22")),
            "the fixture must hold the fake identifier"
        );
        assert!(!index_exists(s));
    }

    #[test]
    fn a_failed_classification_is_retried_at_the_next_start() {
        // ---- A statement that aborts ---------------------------------------
        let cases = failure_fixture_rows();
        let rows = plain_rows(&cases);
        let tmp = TempDb::new("retry-abort");
        let path = tmp.path();
        schema_2_database(&path, &rows);
        let before = records_at_schema_2(&path);
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_update BEFORE UPDATE ON messages WHEN old.rowid = 3
                 BEGIN SELECT RAISE(ABORT, 'test abort'); END;",
            )
            .unwrap();

        let (s, lines) = open_logged(&path);
        assert_untouched_after_failed_pass(&s, &lines, &before, "test abort");
        drop(s);

        // The same database without the obstacle is classified on the next start.
        Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TRIGGER fail_update")
            .unwrap();
        let (s, lines) = open_logged(&path);
        assert!(mentions(&lines, "Checking every stored row"), "{lines:?}");
        assert!(
            !lines
                .iter()
                .any(|(l, _)| matches!(l, Level::Warn | Level::Error)),
            "{lines:?}"
        );
        assert_checked(&s, &cases);
        assert_eq!(s.count().unwrap(), rows.len() as i64);

        // ---- A database that cannot grow ------------------------------------
        // The state a failed pass leaves: migration 3 is in, every row reads
        // unchecked, there is no index.  The limit comes after the migration
        // because adding the columns rewrites the schema on the first page,
        // and a migration that fails is fatal by design.
        let tmp = TempDb::new("retry-full");
        let path = tmp.path();
        schema_2_database(&path, &rows);
        let before = records_at_schema_2(&path);
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATIONS[2]).unwrap();
            tx.pragma_update(None, "user_version", 3).unwrap();
            tx.commit().unwrap();
            // Packed pages and no free page: nothing is left to hand out.
            conn.execute_batch("VACUUM").unwrap();
            let free: i64 = conn
                .query_row("PRAGMA freelist_count", [], |r| r.get(0))
                .unwrap();
            assert_eq!(free, 0);
        }
        let pages: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        let limit = |conn: &Connection| {
            conn.pragma_update(None, "max_page_count", pages).unwrap();
        };

        // The error is SQLite's "full", not just some error.
        {
            let mut conn = Connection::open(&path).unwrap();
            limit(&conn);
            match classify_unchecked(&mut conn) {
                Err(rusqlite::Error::SqliteFailure(e, _)) => {
                    assert_eq!(e.code, rusqlite::ErrorCode::DiskFull, "{e:?}")
                }
                other => panic!("expected a full database, got {other:?}"),
            }
        }

        let (s, lines) = log_capture::capture(|| Storage::open_with(&path, limit));
        let s = s.unwrap();
        assert_untouched_after_failed_pass(&s, &lines, &before, "database or disk is full");
        drop(s);

        // Without the limit the next start does the work.
        let (s, lines) = open_logged(&path);
        assert!(mentions(&lines, "Checking every stored row"), "{lines:?}");
        assert!(
            !lines
                .iter()
                .any(|(l, _)| matches!(l, Level::Warn | Level::Error)),
            "{lines:?}"
        );
        assert_checked(&s, &cases);
        assert_eq!(s.count().unwrap(), rows.len() as i64);
    }

    #[test]
    fn a_failed_wal_truncation_keeps_the_classification() {
        let rows = [
            OldRow::data("noise", "UnconfirmedDataUp", 23)
                .crc(Some("fail"))
                .dev_addr("6874FA22")
                .at(1),
            OldRow::data("real", "UnconfirmedDataUp", 23).at(2),
        ];
        let tmp = TempDb::new("wal-busy");
        let path = tmp.path();
        schema_2_database(&path, &rows);
        let wal = format!("{path}-wal");

        // A second connection holds a read snapshot from before the pass, so
        // the log cannot be folded into the database and truncated.
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let seen: i64 = reader
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seen, 2);

        let started = Instant::now();
        let (s, lines) = open_logged(&path);
        // The truncation does not wait for the reader: connections wait five
        // seconds for a lock by default.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );

        assert!(
            logged(
                &lines,
                Level::Warn,
                "Checked the stored rows, but could not shrink the write-ahead log"
            ),
            "{lines:?}"
        );
        assert!(
            logged(
                &lines,
                Level::Warn,
                "it is removed when the sniffer next stops cleanly"
            ),
            "{lines:?}"
        );
        assert!(!mentions(&lines, "will retry"), "{lines:?}");
        assert!(!lines.iter().any(|(l, _)| *l == Level::Error), "{lines:?}");

        let all = records(&s);
        assert_eq!(all["noise"].malformed, Some(Malformed::Crc));
        crate::core::metadata::assert_body_cleared(&all["noise"]);
        assert_eq!(all["real"].malformed, None);
        assert_eq!(all["real"].dev_addr.as_deref(), Some("01ABCDEF"));
        assert_eq!(unchecked_rows(&s), 0);
        assert_eq!(
            scalar(&s, "SELECT COUNT(*) FROM messages WHERE classified = 1"),
            2
        );
        assert!(index_exists(&s));
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);

        // Once the reader is gone, closing the last connection removes the log.
        reader.execute_batch("COMMIT").unwrap();
        drop(reader);
        drop(s);
        assert!(
            std::fs::metadata(&wal)
                .map(|m| m.len() == 0)
                .unwrap_or(true),
            "the log is still there"
        );

        // Nothing is checked again.
        let (s, lines) = open_logged(&path);
        assert!(!mentions(&lines, "Check"), "{lines:?}");
        assert_eq!(records(&s)["noise"].malformed, Some(Malformed::Crc));
        assert_eq!(unchecked_rows(&s), 0);
    }

    #[test]
    fn truncating_the_log_leaves_the_busy_timeout_as_it_was() {
        let tmp = TempDb::new("busy-timeout");
        let s = Storage::open(&tmp.path()).unwrap();
        let conn = s.conn.lock().unwrap();
        let wait = |c: &Connection| -> i64 {
            c.query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                .unwrap()
        };
        let before = wait(&conn);
        truncate_wal(&conn).unwrap();
        assert_eq!(wait(&conn), before);
        assert!(before > 0, "connections wait for a lock by default");
    }

    // -----------------------------------------------------------------------
    // Query plans
    // -----------------------------------------------------------------------

    /// The label queries must use the partial indexes on `messages`; a full
    /// scan of that table would hold the storage lock for as long as it takes
    /// to read the whole database.
    fn assert_uses_both_device_indexes(plan: &[String]) {
        let text = plan.join("\n");
        assert!(text.contains("idx_messages_dev_addr"), "plan:\n{text}");
        assert!(text.contains("idx_messages_dev_eui"), "plan:\n{text}");
        assert!(
            !plan.iter().any(|line| line.starts_with("SCAN messages")),
            "plan scans messages:\n{text}"
        );
    }

    #[test]
    fn the_orphan_query_uses_both_device_indexes() {
        let s = Storage::open_in_memory().unwrap();
        let conn = s.conn.lock().unwrap();
        assert_uses_both_device_indexes(&query_plan(&conn, ORPHAN_LABELS_SQL, &[]));
    }

    #[test]
    fn the_stored_record_probes_use_their_index() {
        let s = Storage::open_in_memory().unwrap();
        let conn = s.conn.lock().unwrap();
        let addr = query_plan(&conn, HAS_DEV_ADDR_SQL, &[&HAVANA]);
        assert!(
            addr.iter().any(|l| l.contains("idx_messages_dev_addr")),
            "plan: {addr:?}"
        );
        assert!(
            !addr.iter().any(|l| l.starts_with("SCAN messages")),
            "plan: {addr:?}"
        );
        let eui = query_plan(&conn, HAS_DEV_EUI_SQL, &[&QINGPING_EUI]);
        assert!(
            eui.iter().any(|l| l.contains("idx_messages_dev_eui")),
            "plan: {eui:?}"
        );
        assert!(
            !eui.iter().any(|l| l.starts_with("SCAN messages")),
            "plan: {eui:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Large database
    // -----------------------------------------------------------------------

    /// Name of the `d`-th synthetic device: every fourth is a DevEUI, the
    /// rest are DevAddrs, so both identifier indexes carry weight.
    fn large_device(d: u32) -> String {
        if d.is_multiple_of(4) {
            Eui64((0x70B3_D57E_D000_0000u64 + d as u64).to_be_bytes()).to_string()
        } else {
            format!("{:08X}", 0x0100_0000 + d)
        }
    }

    /// Time a retention sweep over a million messages and 10,000 labels.
    ///
    /// Run on its own, in release mode:
    /// `cargo test --release -- --ignored large_db --nocapture`.
    ///
    /// The message deletion and the label step are timed separately by running
    /// them one after the other on one copy of the database (the second sweep
    /// then has no messages left to expire), and the combined single-
    /// transaction sweep is timed on an identical copy.
    ///
    /// The same message deletion takes very different times on byte-identical
    /// copies (about 1.5 s to 7 s on the development machine, with no pattern
    /// in which copy is slow), so the message-only figure is taken twice and
    /// the label step is compared with the faster one.  The label step itself
    /// is a few milliseconds either way.
    #[test]
    #[ignore = "builds a million-row database; run with --release"]
    fn large_db_retention_sweep() {
        use crate::core::labels::MAX_LABELS;
        use crate::core::seen::{SeenDevices, MAX_SEEN};
        use std::time::{Duration, Instant};

        const MESSAGES: u32 = 1_000_000;
        // Distinct devices that have messages.
        const DEVICES: u32 = 100_000;
        // Labelled devices 0..5_000 keep recent messages and so survive.
        const STORED: u32 = 5_000;
        // Labelled devices 5_000..7_500 have only expired messages.
        const EXPIRED_ONLY: u32 = 2_500;
        // Labelled devices DEVICES..DEVICES+2_500 never had a message.
        const ABSENT: u32 = 2_500;
        // Every device sends 10 messages, in 10 passes; in the first 6 passes
        // the messages are old enough to expire.
        const OLD_PASSES: u32 = 6;

        let tmp = TempDb::new("large");
        let path_a = tmp.path();
        let cutoff = Utc::now() - chrono::Duration::days(30);
        let names: Vec<String> = (0..DEVICES + ABSENT).map(large_device).collect();

        // ---- Build ---------------------------------------------------------
        let t = Instant::now();
        let s = Storage::open(&path_a).unwrap();
        let mut expected_expired = 0usize;
        {
            let old_ts: Vec<String> = (0..3600)
                .map(|k| {
                    (cutoff - chrono::Duration::hours(1) - chrono::Duration::seconds(k))
                        .to_rfc3339()
                })
                .collect();
            let new_ts: Vec<String> = (0..3600)
                .map(|k| {
                    (cutoff + chrono::Duration::hours(1) + chrono::Duration::seconds(k))
                        .to_rfc3339()
                })
                .collect();
            let mut conn = s.conn.lock().unwrap();
            let tx = conn.transaction().unwrap();
            {
                let mut insert = tx
                    .prepare(
                        "INSERT INTO messages (
                            id, timestamp, direction, status, source_protocol, freq_mhz,
                            datarate, mtype, dev_addr, dev_eui, payload_size
                        ) VALUES (?1, ?2, 'up', 'forwarded', 'udp', 868.1, 'SF7BW125', ?3, ?4, ?5, 23)",
                    )
                    .unwrap();
                for i in 0..MESSAGES {
                    let d = i % DEVICES;
                    let pass = i / DEVICES;
                    let expired = (STORED..STORED + EXPIRED_ONLY).contains(&d) || pass < OLD_PASSES;
                    if expired {
                        expected_expired += 1;
                    }
                    let ts = if expired { &old_ts } else { &new_ts };
                    let name = &names[d as usize];
                    let (mtype, addr, eui) = if d.is_multiple_of(4) {
                        ("JoinRequest", None, Some(name))
                    } else {
                        ("UnconfirmedDataUp", Some(name), None)
                    };
                    insert
                        .execute(params![
                            format!("m{i}"),
                            ts[(i % 3600) as usize],
                            mtype,
                            addr,
                            eui
                        ])
                        .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let build_messages = t.elapsed();

        // ---- Labels, through the public API at the real limit ---------------
        let t = Instant::now();
        let labelled: Vec<u32> = (0..STORED + EXPIRED_ONLY)
            .chain(DEVICES..DEVICES + ABSENT)
            .collect();
        assert_eq!(labelled.len(), MAX_LABELS);
        for &d in &labelled {
            let name = &names[d as usize];
            assert_eq!(
                s.set_label(name, &format!("device {d}"), true, MAX_LABELS)
                    .unwrap(),
                LabelWrite::Set
            );
        }
        assert_eq!(
            s.set_label("FFFFFFFF", "one too many", true, MAX_LABELS)
                .unwrap(),
            LabelWrite::LimitReached
        );
        // Replacing at the limit still works.
        assert_eq!(
            s.set_label(&names[0], "device 0", true, MAX_LABELS)
                .unwrap(),
            LabelWrite::Set
        );
        let build_labels = t.elapsed();
        assert_eq!(s.count().unwrap(), MESSAGES as i64);
        assert_eq!(expected_expired, 610_000);

        // Fold the write-ahead log into the main file and copy it, so that
        // every sweep starts from a byte-identical database.
        s.conn
            .lock()
            .unwrap()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        drop(s);
        let path_b = format!("{path_a}.b");
        let path_c = format!("{path_a}.c");
        std::fs::copy(&path_a, &path_b).unwrap();
        std::fs::copy(&path_a, &path_c).unwrap();
        let size_mb = std::fs::metadata(&path_a).unwrap().len() as f64 / 1_048_576.0;

        // The rule: a live list as full as it can get, none of it labelled
        // orphans, so every orphan is rejected.
        let seen = SeenDevices::new();
        for d in (STORED + EXPIRED_ONLY)..DEVICES {
            seen.note(&names[d as usize]);
        }
        let mut extra = 0u32;
        while seen.len() < MAX_SEEN {
            seen.note(&format!("EE{extra:06X}"));
            extra += 1;
        }
        assert_eq!(seen.len(), MAX_SEEN);
        let window = Duration::from_secs(3600);
        let rule = |d: &str| seen.seen_within(d, window);

        let remaining = (MESSAGES as usize - expected_expired) as i64;
        let orphans = (EXPIRED_ONLY + ABSENT) as usize;
        let survivors: Vec<String> = {
            let mut v: Vec<String> = (0..STORED).map(|d| names[d as usize].clone()).collect();
            v.sort();
            v
        };
        let check_end_state = |store: &Storage| {
            assert_eq!(store.count().unwrap(), remaining);
            assert_eq!(
                store.labels().unwrap().into_keys().collect::<Vec<_>>(),
                survivors
            );
        };

        // ---- Copy 1: messages first, then the label step on its own ---------
        let one = Storage::open(&path_a).unwrap();
        let t = Instant::now();
        let first = one.delete_older_than(cutoff, OrphanPolicy::Keep).unwrap();
        let messages_only = t.elapsed();
        assert_eq!(first.messages, expected_expired);
        assert!(first.labels.is_empty());
        assert_eq!(one.count().unwrap(), remaining);

        // The orphan query alone, and its plan, on the post-deletion table.
        let (orphan_query, plan) = {
            let conn = one.conn.lock().unwrap();
            let t = Instant::now();
            let n = conn
                .prepare(ORPHAN_LABELS_SQL)
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .count();
            let took = t.elapsed();
            assert_eq!(n, orphans);
            (took, query_plan(&conn, ORPHAN_LABELS_SQL, &[]))
        };
        assert_uses_both_device_indexes(&plan);

        let t = Instant::now();
        let second = one
            .delete_older_than(cutoff, OrphanPolicy::KeepIf(&rule))
            .unwrap();
        let labels_only = t.elapsed();
        assert_eq!(second.messages, 0);
        assert_eq!(second.labels.len(), orphans);
        check_end_state(&one);

        // ---- Copy 2: both steps in one transaction --------------------------
        let two = Storage::open(&path_b).unwrap();
        let t = Instant::now();
        let both = two
            .delete_older_than(cutoff, OrphanPolicy::KeepIf(&rule))
            .unwrap();
        let combined = t.elapsed();
        assert_eq!(both.messages, expected_expired);
        assert_eq!(both.labels.len(), orphans);
        check_end_state(&two);

        // ---- Copy 3: messages only again ------------------------------------
        // The first sweep of a process pays for growing the page cache and the
        // write-ahead log, so the message-only figure is taken a second time
        // after the combined sweep; the two are shown side by side.
        let three = Storage::open(&path_c).unwrap();
        let t = Instant::now();
        let repeat = three.delete_older_than(cutoff, OrphanPolicy::Keep).unwrap();
        let messages_only_again = t.elapsed();
        assert_eq!(repeat.messages, expected_expired);
        assert_eq!(three.count().unwrap(), remaining);

        // Both ways of labelling ended on the same labels.
        let mut removed_split = second.labels.clone();
        let mut removed_combined = both.labels.clone();
        removed_split.sort();
        removed_combined.sort();
        assert_eq!(removed_split, removed_combined);

        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        println!();
        println!(
            "large_db: {MESSAGES} messages, {MAX_LABELS} labels ({STORED} stored, {EXPIRED_ONLY} expired-only, \
             {ABSENT} absent), {DEVICES} devices with messages, live list holding {} identifiers",
            seen.len()
        );
        println!(
            "large_db: database file {size_mb:.0} MiB; build: messages {:.1} s, labels {:.1} s",
            build_messages.as_secs_f64(),
            build_labels.as_secs_f64()
        );
        println!(
            "large_db: delete {} expired messages only (OrphanPolicy::Keep): {:.0} ms on one copy, {:.0} ms on another",
            first.messages,
            ms(messages_only),
            ms(messages_only_again)
        );
        println!(
            "large_db: label step alone (KeepIf, nothing left to expire): {:.1} ms, removed {} labels \
             (orphan query alone: {:.1} ms)",
            ms(labels_only),
            second.labels.len(),
            ms(orphan_query)
        );
        println!(
            "large_db: both steps in one transaction on a second copy: {:.0} ms (messages {}, labels {})",
            ms(combined),
            both.messages,
            both.labels.len()
        );
        println!(
            "large_db: label step is {:.2}% of the combined sweep and {:.2}% of the message deletion",
            100.0 * ms(labels_only) / ms(combined),
            100.0 * ms(labels_only) / ms(messages_only.min(messages_only_again))
        );
        println!("large_db: EXPLAIN QUERY PLAN for the orphan query:");
        for line in &plan {
            println!("large_db:   {line}");
        }
    }

    // -----------------------------------------------------------------------
    // Large database: the first start after the upgrade
    // -----------------------------------------------------------------------

    /// Row `i` of the million-row fixture, and the verdict it should get.
    ///
    /// Every hundred rows hold the same mix, shuffled by a step that is
    /// coprime with 100, so that neighbouring rows differ and the rows to be
    /// marked sit on every page of the table.  Of every hundred: 80 valid
    /// uplinks, 5 JoinRequests, 3 downlinks, 8 with a failed CRC (of mixed
    /// types and sizes), 2 nine-byte uplinks, 1 twenty-byte JoinRequest and
    /// 1 ten-byte JoinAccept.
    fn large_row(i: u32) -> (OldRow, Option<Malformed>) {
        let id = format!("{:08x}-0000-4000-8000-{:012x}", i, u64::from(i) * 7919);
        let at = i64::from(i / 10);
        let addr = format!("{:08X}", 0x2600_0000u32.wrapping_add(i % 50_000));
        match (i * 37) % 100 {
            0..=79 => (
                OldRow::data(&id, "UnconfirmedDataUp", 23)
                    .dev_addr(&addr)
                    .at(at),
                None,
            ),
            80..=84 => (OldRow::join(&id, 23).at(at), None),
            85..=87 => (
                OldRow::data(&id, "UnconfirmedDataDown", 13)
                    .dev_addr(&addr)
                    .downlink()
                    .at(at),
                None,
            ),
            slot @ 88..=95 => {
                let row = match slot - 88 {
                    0 => OldRow::data(&id, "ConfirmedDataDown", 1).dev_addr(&addr),
                    1 => OldRow::bare(&id, "Proprietary", 22),
                    2 => OldRow::data(&id, "UnconfirmedDataUp", 238).dev_addr(&addr),
                    3 => OldRow::data(&id, "UnconfirmedDataUp", 23).dev_addr(&addr),
                    4 => OldRow::join(&id, 23),
                    5 => OldRow::join(&id, 19),
                    6 => OldRow::bare(&id, "JoinAccept", 17),
                    _ => OldRow::bare(&id, "Unknown", 1),
                };
                (row.crc(Some("fail")).at(at), Some(Malformed::Crc))
            }
            96..=97 => (
                OldRow::data(&id, "UnconfirmedDataUp", 9)
                    .dev_addr(&addr)
                    .at(at),
                Some(Malformed::Length),
            ),
            98 => (OldRow::join(&id, 20).at(at), Some(Malformed::Length)),
            _ => (
                OldRow::bare(&id, "JoinAccept", 10).at(at),
                Some(Malformed::Length),
            ),
        }
    }

    const LARGE_ROWS: u32 = 1_000_000;

    /// Environment variable that carries the database path to the child.
    const LARGE_DB_ENV: &str = "LORAWAN_SNIFFER_LARGE_DB";

    /// Prefix of the one line the child prints.
    const LARGE_DB_REPORT: &str = "large_db_open_child: ";

    /// The process's peak resident set so far, in kB (Linux).
    fn peak_rss_kb() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }

    fn file_size(path: &str) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    /// Open the database named by the environment, the way the sniffer does at
    /// start, twice, and report what it cost as one JSON line.
    ///
    /// This is the child of `large_db_migration_classifies_a_million_rows`.
    /// It is a separate process because the peak resident set of a process
    /// only ever grows: the process that built the fixture could not show
    /// what opening it needs.
    #[test]
    #[ignore = "child of large_db_migration_classifies_a_million_rows"]
    fn large_db_open_child() {
        let Ok(path) = std::env::var(LARGE_DB_ENV) else {
            println!("{LARGE_DB_ENV} is not set; nothing to open");
            return;
        };
        let wal = format!("{path}-wal");

        let rss_before = peak_rss_kb();
        let t = Instant::now();
        let s = Storage::open(&path).unwrap();
        let first = t.elapsed();
        let rss_after = peak_rss_kb();
        // Still open: what the log weighs for as long as the sniffer runs.
        let wal_after_first_open = file_size(&wal);
        drop(s);

        let t = Instant::now();
        let s = Storage::open(&path).unwrap();
        let second = t.elapsed();
        let wal_after_second_open = file_size(&wal);
        drop(s);

        println!(
            "{LARGE_DB_REPORT}{}",
            serde_json::json!({
                "first_open_ms": first.as_secs_f64() * 1000.0,
                "second_open_ms": second.as_secs_f64() * 1000.0,
                "peak_rss_before_kb": rss_before,
                "peak_rss_after_kb": rss_after,
                "wal_after_first_open_bytes": wal_after_first_open,
                "wal_after_second_open_bytes": wal_after_second_open,
            })
        );
    }

    /// Open a million-row database written by the previous version, in a
    /// fresh process, and check what it did.
    ///
    /// Run on its own, in release mode, on a real disk (`/tmp` may be memory):
    /// `TMPDIR=<dir> cargo test --release -- --ignored large_db_migration --nocapture`.
    ///
    /// It asserts the result, not the time: the figures it prints are what the
    /// user documentation quotes.
    #[test]
    #[ignore = "builds a million-row database; run with --release"]
    fn large_db_migration_classifies_a_million_rows() {
        use std::io::Read;
        use std::process::{Command, Stdio};

        let tmp = TempDb::new("large-migration");
        let path = tmp.path();
        let wal = format!("{path}-wal");
        let mb = |bytes: u64| bytes as f64 / 1_048_576.0;

        // ---- Build the fixture ---------------------------------------------
        let t = Instant::now();
        let (mut crc_rows, mut length_rows, mut valid_rows) = (0u32, 0u32, 0u32);
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V2_0_0).unwrap();
            conn.execute_batch(SCHEMA_V2_1_0_UPGRADE).unwrap();
            conn.execute_batch(SCHEMA_2_LABELS).unwrap();
            let tx = conn.transaction().unwrap();
            for i in 0..LARGE_ROWS {
                let (row, reason) = large_row(i);
                match reason {
                    Some(Malformed::Crc) => crc_rows += 1,
                    Some(_) => length_rows += 1,
                    None => valid_rows += 1,
                }
                insert_old(&tx, &row);
            }
            tx.commit().unwrap();
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .unwrap();
        }
        let build = t.elapsed();
        assert_eq!(
            (crc_rows, length_rows, valid_rows),
            (80_000, 40_000, 880_000)
        );
        let db_bytes = file_size(&path);
        assert_eq!(file_size(&wal), 0);

        // What a plain pass over the table costs, for comparison.
        let (page_size, page_count, cache_size, floor_cold, floor_warm) = {
            let conn = Connection::open(&path).unwrap();
            let pragma = |name: &str| -> i64 {
                conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
                    .unwrap()
            };
            let scan = || {
                let t = Instant::now();
                let n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM messages WHERE payload_size < 0",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(n, 0);
                t.elapsed()
            };
            let cold = scan();
            let warm = scan();
            (
                pragma("page_size"),
                pragma("page_count"),
                pragma("cache_size"),
                cold,
                warm,
            )
        };

        // ---- Open it in a fresh process, watching the log -------------------
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "core::storage::tests::large_db_open_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(LARGE_DB_ENV, &path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let mut peak_wal = 0u64;
        let status = loop {
            peak_wal = peak_wal.max(file_size(&wal));
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        let wall = started.elapsed();
        let mut output = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        assert!(status.success(), "child failed:\n{output}");
        let report: serde_json::Value = output
            .lines()
            // The test harness puts its own "test ... " text in front of it.
            .find_map(|l| l.split_once(LARGE_DB_REPORT).map(|(_, json)| json))
            .map(|json| serde_json::from_str(json).unwrap())
            .unwrap_or_else(|| panic!("the child reported nothing:\n{output}"));
        let final_wal = file_size(&wal);
        let after_bytes = file_size(&path);

        // ---- Check the result -------------------------------------------------
        let conn = Connection::open(&path).unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(count("SELECT COUNT(*) FROM messages"), LARGE_ROWS as i64);
        let crc = count("SELECT COUNT(*) FROM messages WHERE malformed = 'crc'");
        let length = count("SELECT COUNT(*) FROM messages WHERE malformed = 'length'");
        let valid = count("SELECT COUNT(*) FROM messages WHERE malformed IS NULL");
        assert_eq!((crc, length, valid), (80_000, 40_000, 880_000));
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM messages WHERE malformed IS NOT NULL AND (
                    dev_addr IS NOT NULL OR dev_eui IS NOT NULL OR join_eui IS NOT NULL
                    OR dev_nonce IS NOT NULL OR fcnt IS NOT NULL OR fport IS NOT NULL
                    OR adr IS NOT NULL OR adr_ack_req IS NOT NULL OR ack IS NOT NULL
                    OR class_b IS NOT NULL OR f_pending IS NOT NULL OR f_opts_len IS NOT NULL)"
            ),
            0
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM messages WHERE classified = 1"),
            LARGE_ROWS as i64
        );
        assert_eq!(count(COUNT_UNCLASSIFIED_SQL), 0);
        assert_eq!(count(UNCLASSIFIED_INDEX_EXISTS_SQL), 1);
        // A sample of rows, valid and marked, is what the generator made.
        let mut sampled = 0;
        for i in (0..LARGE_ROWS).step_by(997) {
            let (row, reason) = large_row(i);
            let got = conn
                .query_row(
                    &format!("SELECT {SELECT_COLS} FROM messages WHERE id = ?1"),
                    params![row.id],
                    row_to_record,
                )
                .unwrap();
            assert_eq!(got.malformed, reason, "row {i}");
            assert_eq!(got.payload_size as i64, row.payload_size, "row {i}");
            match reason {
                None => {
                    assert_eq!(got.dev_addr, row.dev_addr, "row {i}");
                    assert_eq!(got.dev_eui, row.dev_eui, "row {i}");
                    assert_eq!(got.fcnt.map(i64::from), row.fcnt, "row {i}");
                    assert_eq!(got.f_opts_len.map(i64::from), row.f_opts_len, "row {i}");
                }
                Some(_) => crate::core::metadata::assert_body_cleared(&got),
            }
            sampled += 1;
        }
        assert!(sampled > 1000);

        // ---- The figures ----------------------------------------------------
        let number = |key: &str| report[key].as_f64().unwrap_or(f64::NAN);
        let ratio = peak_wal as f64 / db_bytes as f64;
        let headroom = ((ratio * 1.5) * 10.0).ceil() / 10.0;
        let rss_after_mb = number("peak_rss_after_kb") / 1024.0;
        println!();
        println!(
            "large_db_migration: {LARGE_ROWS} rows built in {:.1} s; \
             mix per hundred: 80 valid uplinks, 5 JoinRequests, 3 downlinks, 8 CRC-failed, \
             2 nine-byte uplinks, 1 twenty-byte JoinRequest, 1 ten-byte JoinAccept",
            build.as_secs_f64()
        );
        println!(
            "large_db_migration: database {:.1} MiB before ({db_bytes} bytes, {page_count} pages of \
             {page_size} bytes), {:.1} MiB after; cache_size {cache_size} (negative is kibibytes)",
            mb(db_bytes),
            mb(after_bytes)
        );
        println!(
            "large_db_migration: plain full scan (SELECT COUNT(*) ... WHERE payload_size < 0): \
             {:.0} ms first, {:.0} ms again",
            floor_cold.as_secs_f64() * 1000.0,
            floor_warm.as_secs_f64() * 1000.0
        );
        println!(
            "large_db_migration: first open (migration 3 and the first pass) {:.0} ms; \
             second open {:.2} ms; child ran {:.1} s end to end",
            number("first_open_ms"),
            number("second_open_ms"),
            wall.as_secs_f64()
        );
        println!(
            "large_db_migration: write-ahead log: peak {:.1} MiB ({peak_wal} bytes) = {ratio:.2} x the \
             database size before; {} bytes right after the first open returned, {} bytes after the \
             second, {final_wal} bytes once the child had exited",
            mb(peak_wal),
            number("wal_after_first_open_bytes"),
            number("wal_after_second_open_bytes")
        );
        println!(
            "large_db_migration: free space to advise: peak ratio x 1.5, rounded up to one decimal = \
             {headroom:.1} x the database size"
        );
        println!(
            "large_db_migration: child peak RSS (VmHWM): {} kB before the first open, {} kB after \
             ({rss_after_mb:.1} MiB)",
            report["peak_rss_before_kb"], report["peak_rss_after_kb"]
        );
        println!(
            "large_db_migration: rows by verdict: crc {crc}, length {length}, not malformed {valid}; \
             {sampled} sampled rows checked against the generator"
        );

        // The plans do not depend on the size of the table, so they are shown
        // on an empty database: before the index exists (the first pass) and
        // after (every later start).
        let mut plans = Connection::open_in_memory().unwrap();
        plans.execute_batch(INIT_SQL).unwrap();
        migrate(&mut plans).unwrap();
        for (when, with_index) in [
            ("first pass, no index yet", false),
            ("later starts, with the index", true),
        ] {
            if with_index {
                plans.execute_batch(UNCLASSIFIED_INDEX_SQL).unwrap();
            }
            for (name, sql) in [
                ("CLASSIFY_MALFORMED_SQL", CLASSIFY_MALFORMED_SQL),
                ("CLASSIFY_REST_SQL", CLASSIFY_REST_SQL),
                ("COUNT_UNCLASSIFIED_SQL", COUNT_UNCLASSIFIED_SQL),
            ] {
                println!(
                    "large_db_migration: EXPLAIN QUERY PLAN ({when}) {name}: {}",
                    query_plan(&plans, sql, &[]).join(" | ")
                );
            }
        }
    }
}
