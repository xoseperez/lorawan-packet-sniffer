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
//! ## Concurrency note
//!
//! `rusqlite::Connection` is `Send` but not `Sync`.  `Arc<Mutex<Connection>>`
//! is both `Send` and `Sync`, so `Storage` can be cloned freely and passed
//! into `spawn_blocking` closures.

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use log::warn;
use rusqlite::{params, Connection};

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
/// large database is fast.  Booleans are stored as INTEGER 0/1.
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

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

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

    /// Delete all messages with a timestamp older than `cutoff`.
    ///
    /// Returns the number of rows deleted.
    pub fn delete_older_than(&self, cutoff: DateTime<Utc>) -> Result<usize, SnifferError> {
        let conn = self.conn.lock().map_err(|_| SnifferError::LockPoisoned)?;
        let n = conn.execute(
            "DELETE FROM messages WHERE timestamp < ?1",
            params![cutoff.to_rfc3339()],
        )?;
        Ok(n)
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
        let deleted = s.delete_older_than(cutoff).unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn delete_older_than_keeps_all_when_recent() {
        let s = Storage::open_in_memory().unwrap();
        s.insert(&sample_record()).unwrap();
        s.insert(&sample_record()).unwrap();

        let cutoff = Utc::now() - chrono::Duration::days(30);
        let deleted = s.delete_older_than(cutoff).unwrap();
        assert_eq!(deleted, 0);
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
        let deleted = s.delete_older_than(cutoff).unwrap();
        assert_eq!(deleted, 5);
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
}
