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
use crate::core::metadata::{CrcStatus, Direction, MessageRecord, MType, Protocol, Status};

const SELECT_COLS: &str = "
    id, timestamp, direction, status, blocked_by, source_protocol,
    freq_mhz, datarate, rssi, snr, crc_status,
    mtype, dev_addr, dev_eui, fcnt, fport, payload_size, toa_ms
";

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

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

const INSERT_SQL: &str = "
INSERT OR REPLACE INTO messages (
    id, timestamp, direction, status, blocked_by, source_protocol,
    freq_mhz, datarate, rssi, snr, crc_status,
    mtype, dev_addr, dev_eui, fcnt, fport, payload_size, toa_ms
) VALUES (
    ?1,  ?2,  ?3,  ?4,  ?5,  ?6,
    ?7,  ?8,  ?9,  ?10, ?11,
    ?12, ?13, ?14, ?15, ?16, ?17, ?18
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
        let conn = Connection::open(p)?;
        init(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Open an in-memory database (useful for tests).
    pub fn open_in_memory() -> Result<Self, SnifferError> {
        let conn = Connection::open_in_memory()?;
        init(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
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

fn init(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(INIT_SQL)
}

// ---------------------------------------------------------------------------
// Row deserialization
// ---------------------------------------------------------------------------

fn row_to_record(row: &rusqlite::Row) -> Result<MessageRecord, rusqlite::Error> {
    let ts_str: String = row.get(1)?;
    let timestamp = DateTime::parse_from_rfc3339(&ts_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|e| {
            warn!("Failed to parse stored timestamp '{}': {} — using current time", ts_str, e);
            Utc::now()
        });

    let direction = match row.get::<_, String>(2)?.as_str() {
        "up" => Direction::Up,
        _ => Direction::Down,
    };
    let status = match row.get::<_, String>(3)?.as_str() {
        "forwarded" => Status::Forwarded,
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
    })
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
    use crate::core::metadata::{CrcStatus, Direction, MessageRecord, MType, Protocol, Status};
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
}
