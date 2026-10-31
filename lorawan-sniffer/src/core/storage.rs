//! SQLite persistence layer for captured LoRaWAN messages.
//!
//! ## Design
//!
//! `Storage` wraps a `rusqlite::Connection` behind an `Arc<Mutex<>>` so it
//! can be cheaply cloned and shared across tokio tasks via
//! `tokio::task::spawn_blocking`.
//!
//! All timestamps are persisted as RFC 3339 text (`2026-02-23T10:30:00.123Z`),
//! which is lexicographically sortable and understood by SQLite's built-in date
//! functions.
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
//!   device identifier.
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
//! ## Concurrency note
//!
//! `rusqlite::Connection` is `Send` but not `Sync`.  `Arc<Mutex<Connection>>`
//! is both `Send` and `Sync`, so `Storage` can be cloned freely and passed
//! into `spawn_blocking` closures.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use log::warn;
use rusqlite::{params, Connection, TransactionBehavior};

use crate::core::error::SnifferError;
use crate::core::metadata::{CrcStatus, Direction, MType, MessageRecord, Protocol, Status};

const SELECT_COLS: &str = "
    id, timestamp, direction, status, blocked_by, source_protocol,
    freq_mhz, datarate, rssi, snr, crc_status,
    mtype, dev_addr, dev_eui, fcnt, fport, payload_size, toa_ms,
    gateway_eui, join_eui, dev_nonce,
    adr, adr_ack_req, ack, class_b, f_pending, f_opts_len
";

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

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
/// SQLite adds a nullable column without rewriting the table, so upgrading a
/// large database is fast.  Booleans are stored as INTEGER 0/1.  A new table
/// does not touch `messages` at all.
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
];

const INSERT_SQL: &str = "
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
    conn: Arc<Mutex<Connection>>,
}

impl Storage {
    /// Open (or create) the database at `path`, applying WAL mode and schema.
    ///
    /// Parent directories are created automatically.
    pub fn open(path: &str) -> Result<Self, SnifferError> {
        let p = Path::new(path);
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut conn = Connection::open(p)?;
        init(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory database (useful for tests).
    pub fn open_in_memory() -> Result<Self, SnifferError> {
        let mut conn = Connection::open_in_memory()?;
        init(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Persist a `MessageRecord` to the database.
    ///
    /// Uses `INSERT OR REPLACE` so duplicate IDs are handled gracefully.
    pub fn insert(&self, rec: &MessageRecord) -> Result<(), SnifferError> {
        let conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
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
            ],
        )?;
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

    /// Return the most recent `limit` messages, ordered by timestamp ascending.
    pub fn query_recent(&self, limit: i64) -> Result<Vec<MessageRecord>, SnifferError> {
        let conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
        let sql = format!(
            "SELECT {cols} FROM messages ORDER BY timestamp DESC LIMIT ?1",
            cols = SELECT_COLS
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![limit])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(row_to_record(row)?);
        }
        // Return in ascending order (oldest first).
        records.reverse();
        Ok(records)
    }

    /// Return up to `limit` messages with timestamp strictly after `since`,
    /// ordered by timestamp ascending (oldest first).
    pub fn query_since(
        &self,
        since: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<MessageRecord>, SnifferError> {
        let conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
        let sql = format!(
            "SELECT {cols} FROM messages WHERE timestamp > ?1 ORDER BY timestamp ASC LIMIT ?2",
            cols = SELECT_COLS
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![since.to_rfc3339(), limit])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(row_to_record(row)?);
        }
        Ok(records)
    }
}

// ---------------------------------------------------------------------------
// Schema initialisation
// ---------------------------------------------------------------------------

fn init(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(INIT_SQL)?;
    migrate(conn)
}

/// Apply every entry of [`MIGRATIONS`] newer than the database's
/// `user_version`.
///
/// Each migration runs in its own transaction together with the version bump,
/// so a failure partway through leaves the database at the previous version
/// instead of half-migrated (SQLite DDL and `user_version` are both
/// transactional).
fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
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
mod tests {
    use super::*;
    use crate::core::eui::Eui64;
    use crate::core::metadata::{CrcStatus, Direction, MType, MessageRecord, Protocol, Status};
    use chrono::Utc;

    fn sample_record() -> MessageRecord {
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
        }
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
    struct TempDb {
        dir: std::path::PathBuf,
    }

    impl TempDb {
        fn new(label: &str) -> Self {
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

        fn path(&self) -> String {
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
        ] {
            assert!(columns.iter().any(|c| c == new), "missing column {new}");
        }
        assert_eq!(columns.len(), 27);

        // The labels table arrives with the same upgrade, empty and usable.
        assert!(s.labels().unwrap().is_empty());
        assert_eq!(
            s.set_label("01ABCDEF", "upgraded", false, 10).unwrap(),
            LabelWrite::Set
        );
        assert!(s.delete_label("01ABCDEF").unwrap());

        // Old rows survive intact; every new field reads as absent.
        let rows = s.query_recent(10).unwrap();
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
        let back = s
            .query_recent(10)
            .unwrap()
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
        assert_eq!(column_names(&s).len(), 27);
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

        let rows = s.query_recent(10).unwrap();
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
        let rows = s.query_since(since, 10).unwrap();
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
        assert_eq!(user_version(&s), 2);
        assert_eq!(MIGRATIONS.len(), 2);

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
        let rows = s.query_recent(10).unwrap();
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
        assert_eq!(user_version(&s), 2);
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
}
