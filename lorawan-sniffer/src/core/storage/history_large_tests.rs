//! A million-row measurement of history queries.
//!
//! Run on its own, in release mode, on a real disk (`/tmp` is memory on many
//! systems and would flatter every number):
//!
//! ```text
//! TMPDIR=/path/on/disk cargo test --release -- --ignored large_db_history --nocapture
//! ```
//!
//! `HISTORY_ROWS` and `HISTORY_DAYS` change the size (the default is one
//! million rows over seven days; 4,300,000 over 30 days is the default
//! retention at the same rate).  `HISTORY_SUSTAINED_SECS` (60),
//! `HISTORY_QUIET_SECS` (10) and `HISTORY_CONTRAST_SECS` (20) set how long the
//! writer runs with the queries, without them, and with the queries on the
//! writer's own connection.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use super::history_tests::{flag, plan, plan_text};
use super::tests::TempDb;
use super::*;
use crate::core::eui::Eui64;
use crate::core::history::HistoryFilter;
use crate::core::labels::MAX_LABELS;

// ---------------------------------------------------------------------------
// The data
// ---------------------------------------------------------------------------

const GATEWAYS: [&str; 3] = [
    "AA:BB:CC:DD:00:11:22:01",
    "AA:BB:CC:DD:00:11:22:02",
    "AA:BB:CC:DD:00:11:22:03",
];
const DATARATES: [&str; 6] = [
    "SF7BW125",
    "SF8BW125",
    "SF9BW125",
    "SF10BW125",
    "SF11BW125",
    "SF12BW125",
];
const DEV_ADDRS: u64 = 10_000;
const DEV_EUIS: u64 = 2_500;
const QUIET_ADDR: &str = "26F00001";
const QUIET_EUI: &str = "70:B3:D5:7E:D0:FF:00:01";

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A small, fast, repeatable generator: the data should be the same on every
/// run, so that two runs can be compared.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn percent(&mut self, p: u64) -> bool {
        self.below(100) < p
    }
}

fn dev_addr(d: u64) -> String {
    format!("{:08X}", 0x2600_0000 + d)
}

fn dev_eui(e: u64) -> String {
    Eui64((0x70B3_D57E_D000_0000u64 + e).to_be_bytes()).to_string()
}

/// A record as a gateway fleet produces them: mostly unconfirmed uplinks, some
/// joins and downlinks, about a tenth malformed, and the optional fields
/// filled the way real records fill them.
fn large_record(rng: &mut Rng, at: DateTime<Utc>) -> MessageRecord {
    let roll = rng.below(100);
    let status = match rng.below(100) {
        0..=84 => Status::Forwarded,
        85..=94 => Status::Blocked,
        _ => Status::Captured,
    };
    let mut r = MessageRecord {
        id: MessageRecord::new_id(),
        timestamp: at,
        direction: Direction::Up,
        blocked_by: (status == Status::Blocked).then(|| "rule-1".to_string()),
        status,
        source_protocol: Protocol::Udp,
        freq_mhz: 867.1 + 0.2 * rng.below(8) as f64,
        datarate: DATARATES[rng.below(6) as usize].to_string(),
        rssi: None,
        snr: None,
        crc_status: None,
        mtype: MType::UnconfirmedDataUp,
        dev_addr: None,
        dev_eui: None,
        fcnt: None,
        fport: None,
        payload_size: 13 + rng.below(40) as u16,
        toa_ms: Some(40.0 + rng.below(1500) as f64 / 10.0),
        gateway_eui: Some(GATEWAYS[rng.below(3) as usize].to_string()),
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

    let uplink_signal = |r: &mut MessageRecord, rng: &mut Rng, crc: CrcStatus| {
        r.rssi = Some(-120 + rng.below(80) as i16);
        r.snr = Some(rng.below(300) as f32 / 10.0 - 15.0);
        r.crc_status = Some(crc);
    };
    let data_fields = |r: &mut MessageRecord, rng: &mut Rng| {
        r.dev_addr = Some(dev_addr(rng.below(DEV_ADDRS)));
        r.fcnt = Some(rng.below(65_536) as u32);
        r.fport = rng.percent(80).then(|| 1 + rng.below(220) as u8);
        r.f_opts_len = Some(if rng.percent(95) {
            0
        } else {
            1 + rng.below(15) as u8
        });
    };

    match roll {
        0..=4 => {
            // The radio reported a failed CRC: nothing of the body is trusted.
            r.mtype = [MType::UnconfirmedDataUp, MType::JoinRequest, MType::Unknown]
                [rng.below(3) as usize]
                .clone();
            r.payload_size = rng.below(60) as u16;
            uplink_signal(&mut r, rng, CrcStatus::Fail);
            r.malformed = Some(Malformed::Crc);
        }
        5..=8 => {
            r.mtype = MType::UnconfirmedDataUp;
            r.payload_size = 5 + rng.below(7) as u16;
            uplink_signal(&mut r, rng, CrcStatus::Ok);
            r.malformed = Some(Malformed::Length);
        }
        9 => {
            r.mtype = MType::Unknown;
            uplink_signal(&mut r, rng, CrcStatus::Ok);
            r.malformed = Some(Malformed::Mhdr);
        }
        10..=69 => {
            uplink_signal(&mut r, rng, CrcStatus::Ok);
            data_fields(&mut r, rng);
            r.adr = Some(rng.percent(70));
            r.adr_ack_req = Some(false);
            r.ack = Some(rng.percent(5));
            r.class_b = Some(false);
        }
        70..=77 => {
            r.mtype = MType::ConfirmedDataUp;
            uplink_signal(&mut r, rng, CrcStatus::Ok);
            data_fields(&mut r, rng);
            r.adr = Some(rng.percent(70));
            r.adr_ack_req = Some(false);
            r.ack = Some(false);
            r.class_b = Some(false);
        }
        78..=85 => {
            r.mtype = MType::JoinRequest;
            r.payload_size = 23;
            uplink_signal(&mut r, rng, CrcStatus::Ok);
            r.dev_eui = Some(dev_eui(rng.below(DEV_EUIS)));
            r.join_eui = Some("00:00:00:00:00:00:00:01".to_string());
            r.dev_nonce = Some(rng.below(65_536) as u16);
        }
        86..=93 => {
            r.mtype = if roll < 91 {
                MType::UnconfirmedDataDown
            } else {
                MType::ConfirmedDataDown
            };
            r.direction = Direction::Down;
            data_fields(&mut r, rng);
            r.adr = Some(rng.percent(70));
            r.ack = Some(rng.percent(30));
            r.f_pending = Some(rng.percent(5));
        }
        94..=96 => {
            r.mtype = MType::JoinAccept;
            r.direction = Direction::Down;
            r.payload_size = 17;
        }
        _ => {
            r.mtype = [MType::Proprietary, MType::RejoinRequest, MType::Unknown]
                [rng.below(3) as usize]
                .clone();
            uplink_signal(&mut r, rng, CrcStatus::Ok);
        }
    }
    r
}

/// The fixture's size and time span.
struct Fixture {
    rows: u64,
    days: u64,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

/// Build the database at `path`: `rows` records evenly over `days` days ending
/// now, a quiet DevAddr and a quiet DevEUI with three records each in the
/// oldest hour, and 10,000 labels.
fn build(path: &str, rows: u64, days: u64) -> Fixture {
    let started = Instant::now();
    let end = Utc::now();
    let span_ns = days as i64 * 86_400 * 1_000_000_000;
    let start = end - chrono::Duration::nanoseconds(span_ns);
    let s = Storage::open(path).unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    {
        let mut conn = s.conn.lock().unwrap();
        let tx = conn.transaction().unwrap();
        for i in 0..rows {
            let at = start
                + chrono::Duration::nanoseconds(
                    (i as i128 * span_ns as i128 / rows as i128) as i64,
                );
            insert_record(&tx, &large_record(&mut rng, at)).unwrap();
        }
        for k in 1..=3 {
            let at = start + chrono::Duration::minutes(10 * k);
            let mut data = large_record(&mut rng, at);
            data.malformed = None;
            data.mtype = MType::UnconfirmedDataUp;
            data.direction = Direction::Up;
            data.dev_addr = Some(QUIET_ADDR.to_string());
            data.dev_eui = None;
            insert_record(&tx, &data).unwrap();
            let mut join = data.clone();
            join.id = MessageRecord::new_id();
            join.timestamp = at + chrono::Duration::minutes(5);
            join.mtype = MType::JoinRequest;
            join.dev_addr = None;
            join.dev_eui = Some(QUIET_EUI.to_string());
            insert_record(&tx, &join).unwrap();
        }

        // 10,000 labels: 9,000 devices share a word, one is the quiet
        // DevAddr, and the rest are DevEUIs.
        let label = |device: &str, text: &str| {
            tx.execute(
                UPSERT_LABEL_SQL,
                rusqlite::params![device, text, Utc::now().to_rfc3339()],
            )
            .unwrap();
        };
        for d in 0..9_000 {
            label(&dev_addr(d), &format!("Havana sensor {d}"));
        }
        label(QUIET_ADDR, "Ñandú quiet sensor");
        for e in 0..(MAX_LABELS as u64 - 9_001) {
            label(&dev_eui(e), &format!("Lisbon roof {e}"));
        }
        tx.commit().unwrap();
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
    }
    let labels: i64 = s
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM device_labels", [], |r| r.get(0))
        .unwrap();
    assert_eq!(labels as usize, MAX_LABELS);
    drop(s);
    println!(
        "built {} rows over {} days and {} labels in {:.1} s; database {:.0} MiB",
        rows + 6,
        days,
        labels,
        started.elapsed().as_secs_f64(),
        std::fs::metadata(path).unwrap().len() as f64 / 1_048_576.0
    );
    Fixture {
        rows,
        days,
        start,
        end,
    }
}

// ---------------------------------------------------------------------------
// The queries
// ---------------------------------------------------------------------------

fn query(lower: Option<LowerBound>, change: impl FnOnce(&mut HistoryFilter)) -> HistoryQuery {
    let mut filter = HistoryFilter::default();
    change(&mut filter);
    HistoryQuery {
        lower,
        upper: None,
        filter,
        limit: MAX_HISTORY_ROWS,
        newest: true,
        count: true,
    }
}

struct Shape {
    name: String,
    query: HistoryQuery,
    /// Whether the SQL-aggregate alternative applies (a row-filtered query).
    compare: bool,
}

fn shapes(f: &Fixture) -> Vec<Shape> {
    let whole = Some(LowerBound::From(f.start));
    // The range the WebUI's long presets reach: seven days, or all there is.
    let week_start = f.end - chrono::Duration::days(7);
    let week = Some(LowerBound::From(week_start.max(f.start)));
    let day = Some(LowerBound::From(f.end - chrono::Duration::hours(24)));
    let week_name = if f.days <= 7 {
        format!("{} days (the whole fixture)", f.days)
    } else {
        "last 7 days".to_string()
    };
    let mut v = Vec::new();
    let mut add = |name: String, query: HistoryQuery, compare: bool| {
        v.push(Shape {
            name,
            query,
            compare,
        });
    };

    add(
        format!("time only, {week_name}, malformed included, with malformed count"),
        query(week, |_| {}),
        false,
    );
    add("time only, last 24 h".into(), query(day, |_| {}), false);
    add("time only, all time".into(), query(None, |_| {}), false);
    add(
        "old shape: limit=500".into(),
        HistoryQuery {
            limit: 500,
            count: false,
            ..query(None, |_| {})
        },
        false,
    );
    add(
        "old shape: since the start, oldest 500".into(),
        HistoryQuery {
            limit: 500,
            count: false,
            newest: false,
            ..query(Some(LowerBound::After(f.start)), |_| {})
        },
        false,
    );
    add(
        "quiet DevAddr (whole fixture)".into(),
        query(whole, |h| h.device = Some(QUIET_ADDR.to_lowercase())),
        true,
    );
    add(
        "quiet DevEUI fragment (whole fixture)".into(),
        query(whole, |h| h.device = Some("d0:ff:00".to_string())),
        true,
    );
    add(
        "label fragment ñandú (whole fixture)".into(),
        query(whole, |h| h.device = Some("ÑANDÚ".to_string())),
        true,
    );
    add(
        format!("needle that matches nothing, {week_name}"),
        query(week, |h| h.device = Some("zzzz".to_string())),
        true,
    );
    add(
        "needle that matches nothing, all time".into(),
        query(None, |h| h.device = Some("zzzz".to_string())),
        true,
    );
    add(
        format!("label fragment matching 9,000 devices, {week_name}"),
        query(week, |h| h.device = Some("havana".to_string())),
        true,
    );
    add(
        format!("mtype=JoinRequest, {week_name}"),
        query(week, |h| h.mtypes = vec![MType::JoinRequest]),
        true,
    );
    add(
        format!("status=blocked, {week_name}"),
        query(week, |h| h.status = Some(Status::Blocked)),
        true,
    );
    add(
        format!("one gateway, {week_name}"),
        query(week, |h| h.gateway = Some(GATEWAYS[1].to_string())),
        true,
    );
    add(
        format!("hide_malformed alone, {week_name}"),
        query(week, |h| h.hide_malformed = true),
        true,
    );
    add(
        "hide_malformed alone, last 24 h".into(),
        query(day, |h| h.hide_malformed = true),
        true,
    );
    add(
        "hide_malformed alone, all time".into(),
        query(None, |h| h.hide_malformed = true),
        true,
    );
    add(
        format!("label + type + status + hide_malformed, {week_name}"),
        query(week, |h| {
            h.device = Some("havana".to_string());
            h.mtypes = vec![MType::UnconfirmedDataUp, MType::ConfirmedDataUp];
            h.status = Some(Status::Forwarded);
            h.hide_malformed = true;
        }),
        true,
    );
    v
}

/// The same answer by the other way of counting: the totals by an SQL
/// aggregate over the range, then a separate fetch that stops at the cap.
fn sql_aggregate(conn: &Connection, q: &HistoryQuery) -> HistoryPage {
    let tx = conn.unchecked_transaction().unwrap();
    let ids = q
        .filter
        .device_text()
        .map(|text| matching_label_ids(&tx, text).unwrap());
    let c = Conditions::new(q, ids);
    let (count, malformed): (i64, i64) = tx
        .query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(malformed IS NOT NULL), 0) {RANGE_FROM}{}",
                c.where_sql(&[])
            ),
            &*c.bindings(),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let total = if q.filter.hide_malformed {
        count - malformed
    } else {
        count
    };

    let extra: &[&str] = if q.filter.hide_malformed {
        &["malformed IS NULL"]
    } else {
        &[]
    };
    let limit = q.limit as i64;
    let mut bindings = c.bindings();
    bindings.push((":limit", &limit));
    let mut stmt = tx
        .prepare(&format!(
            "SELECT rowid {RANGE_FROM}{} ORDER BY timestamp DESC LIMIT :limit",
            c.where_sql(extra)
        ))
        .unwrap();
    let chosen: Vec<i64> = stmt
        .query_map(&*bindings, |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    drop(stmt);
    let mut records = Vec::new();
    if !chosen.is_empty() {
        let ids = serde_json::to_string(&chosen).unwrap();
        let mut stmt = tx.prepare(&fetch_by_rowid_sql()).unwrap();
        let mut rows = stmt.query(rusqlite::params![ids]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            records.push(row_to_record(row).unwrap());
        }
    }
    HistoryPage {
        records,
        total: Some(total as u64),
        malformed: Some(malformed as u64),
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `min / median / max` of a few timings, in milliseconds.
fn spread(times: &[Duration]) -> String {
    let mut t = times.to_vec();
    t.sort();
    format!(
        "{:8.1} / {:8.1} / {:8.1}",
        ms(t[0]),
        ms(t[t.len() / 2]),
        ms(t[t.len() - 1])
    )
}

fn median(times: &[Duration]) -> Duration {
    let mut t = times.to_vec();
    t.sort();
    t[t.len() / 2]
}

// ---------------------------------------------------------------------------
// The writer
// ---------------------------------------------------------------------------

#[derive(Default)]
struct WriterTimes {
    waits: Vec<Duration>,
    writes: Vec<Duration>,
}

/// Insert one record every 2 ms until `stop`, timing how long the connection's
/// lock took to get and how long the write took, apart.
fn spawn_writer(s: &Storage, stop: Arc<AtomicBool>) -> JoinHandle<WriterTimes> {
    let s = s.clone();
    std::thread::spawn(move || {
        let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
        let mut times = WriterTimes::default();
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let record = large_record(&mut rng, Utc::now());
            let asked = Instant::now();
            let conn = s.conn.lock().unwrap();
            let waited = asked.elapsed();
            let began = Instant::now();
            insert_record(&conn, &record).unwrap();
            drop(conn);
            times.waits.push(waited);
            times.writes.push(began.elapsed());

            next += Duration::from_millis(2);
            // A stall does not make up for itself with a burst: the gateways
            // keep their own pace, and what they send meanwhile is queued.
            if next < Instant::now() {
                next = Instant::now();
            }
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
        times
    })
}

struct Summary {
    n: usize,
    p50: Duration,
    p99: Duration,
    max: Duration,
}

fn summarize(times: &[Duration]) -> Summary {
    let mut t = times.to_vec();
    t.sort();
    let at = |p: f64| t[((t.len() as f64 * p) as usize).min(t.len() - 1)];
    Summary {
        n: t.len(),
        p50: at(0.5),
        p99: at(0.99),
        max: t[t.len() - 1],
    }
}

fn report_writer(label: &str, w: &WriterTimes) -> (Summary, Summary) {
    let wait = summarize(&w.waits);
    let write = summarize(&w.writes);
    println!(
        "  {label:<28} {:>6} inserts | lock wait ms: p50 {:7.3} p99 {:8.3} max {:9.3} | write ms: p50 {:7.3} p99 {:8.3} max {:9.3}",
        wait.n,
        ms(wait.p50),
        ms(wait.p99),
        ms(wait.max),
        ms(write.p50),
        ms(write.p99),
        ms(write.max),
    );
    (wait, write)
}

fn wal_size(path: &str) -> u64 {
    std::fs::metadata(format!("{path}-wal"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// How many frames the log holds now, read from the shared-memory index the
/// way a reader would.  The file keeps its size when the log is restarted;
/// this number goes back to zero.
fn wal_frames(path: &str) -> Option<u32> {
    use std::io::Read;
    let mut header = [0u8; 24];
    std::fs::File::open(format!("{path}-shm"))
        .ok()?
        .read_exact(&mut header)
        .ok()?;
    // `mxFrame` of the first copy of the index header, in the machine's own
    // byte order, as SQLite writes it.
    Some(u32::from_ne_bytes(header[16..20].try_into().unwrap()))
}

/// Sample the log's size every 100 ms until `stop`.
fn spawn_wal_sampler(path: String, stop: Arc<AtomicBool>) -> JoinHandle<Vec<(Duration, u64)>> {
    std::thread::spawn(move || {
        let began = Instant::now();
        let mut samples = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            samples.push((began.elapsed(), wal_size(&path)));
            std::thread::sleep(Duration::from_millis(100));
        }
        samples
    })
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

/// The queries the WebUI's long ranges send, back to back for `duration`, on
/// `target`.  Returns every query's time.
fn hammer(target: &Storage, shapes: &[&HistoryQuery], duration: Duration) -> Vec<Duration> {
    let began = Instant::now();
    let mut times = Vec::new();
    let mut k = 0;
    while began.elapsed() < duration {
        let q = shapes[k % shapes.len()];
        k += 1;
        let t = Instant::now();
        target.query_history(q, flag()).unwrap();
        times.push(t.elapsed());
    }
    times
}

fn peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn verdict(ok: bool) -> &'static str {
    if ok {
        "PASS"
    } else {
        "FAIL"
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// Time every history query shape on a large database, then show what a
/// running writer sees while they run.
#[test]
#[ignore = "builds a million-row database and runs for minutes; run with --release"]
fn large_db_history_queries() {
    let rows = env_number("HISTORY_ROWS", 1_000_000);
    let days = env_number("HISTORY_DAYS", 7);
    let quiet_secs = env_number("HISTORY_QUIET_SECS", 10);
    let sustained_secs = env_number("HISTORY_SUSTAINED_SECS", 60);
    let contrast_secs = env_number("HISTORY_CONTRAST_SECS", 20);

    let tmp = TempDb::new("history-large");
    let path = tmp.path();
    println!("database: {path}");
    println!("sqlite {}", rusqlite::version());
    let fixture = build(&path, rows, days);

    // ---- Queries ------------------------------------------------------------
    let s = Storage::open(&path).unwrap();
    assert!(s.reader.is_some());
    let shapes = shapes(&fixture);

    println!("\n== First query after opening the database (cold SQLite page cache, warm OS cache)");
    let first = &shapes[0];
    let t = Instant::now();
    let page = s.query_history(&first.query, flag()).unwrap();
    println!(
        "  {:<75} {:8.1} ms  ({} rows, total {:?}, malformed {:?})",
        first.name,
        ms(t.elapsed()),
        page.records.len(),
        page.total,
        page.malformed
    );

    println!("\n== Every shape, 3 runs each, milliseconds min / median / max");
    println!("  newest {MAX_HISTORY_ROWS} with the count; 'one pass' is the kept method (rowids and both counts in one scan),");
    println!(
        "  'aggregate' is COUNT(*) and SUM(malformed) in SQL, then a fetch that stops at the cap"
    );
    let reader_conn = s.reader.as_ref().unwrap();
    let mut one_pass_total = Duration::ZERO;
    let mut aggregate_total = Duration::ZERO;
    let mut one_pass_wins = 0;
    let mut checkpoint: Option<(String, Vec<Duration>)> = None;
    let weekly_hide = format!(
        "hide_malformed alone, {}",
        if days <= 7 {
            format!("{days} days (the whole fixture)")
        } else {
            "last 7 days".to_string()
        }
    );
    for shape in &shapes {
        let mut times = Vec::new();
        let mut page = None;
        for _ in 0..3 {
            let t = Instant::now();
            page = Some(s.query_history(&shape.query, flag()).unwrap());
            times.push(t.elapsed());
        }
        let page = page.unwrap();
        println!("  {}", shape.name);
        println!(
            "      one pass  {} ms  | rows {:>4}, total {:?}, malformed {:?}",
            spread(&times),
            page.records.len(),
            page.total,
            page.malformed
        );
        if shape.name == weekly_hide {
            checkpoint = Some((shape.name.clone(), times.clone()));
        }
        if shape.compare {
            let mut alt_times = Vec::new();
            let mut alt = None;
            for _ in 0..3 {
                let conn = reader_conn.lock().unwrap();
                let t = Instant::now();
                alt = Some(sql_aggregate(&conn, &shape.query));
                alt_times.push(t.elapsed());
            }
            let alt = alt.unwrap();
            assert_eq!(alt.total, page.total, "{}", shape.name);
            assert_eq!(alt.malformed, page.malformed, "{}", shape.name);
            assert_eq!(
                alt.records.iter().map(|r| &r.id).collect::<Vec<_>>(),
                page.records.iter().map(|r| &r.id).collect::<Vec<_>>(),
                "{}",
                shape.name
            );
            println!(
                "      aggregate {} ms  | same rows and counts",
                spread(&alt_times)
            );
            one_pass_total += median(&times);
            aggregate_total += median(&alt_times);
            if median(&times) <= median(&alt_times) {
                one_pass_wins += 1;
            }
        }
    }
    let compared = shapes.iter().filter(|s| s.compare).count();
    println!(
        "\n  counting methods over the {compared} filtered shapes: one pass {:.0} ms, aggregate {:.0} ms (sum of medians); one pass was faster or equal on {one_pass_wins}",
        ms(one_pass_total),
        ms(aggregate_total)
    );

    println!("\n== Plans");
    {
        let conn = reader_conn.lock().unwrap();
        for shape in &shapes {
            let ids = shape
                .query
                .filter
                .device_text()
                .map(|text| matching_label_ids(&conn, text).unwrap());
            let c = Conditions::new(&shape.query, ids);
            let limit = shape.query.limit as i64;
            let mut with_limit = c.bindings();
            with_limit.push((":limit", &limit));
            println!("  {}", shape.name);
            if shape.query.filter.has_row_filter() {
                println!(
                    "      scan:  {}",
                    plan_text(&plan(&conn, &c.scan_sql(true), &c.bindings()))
                );
            } else {
                println!(
                    "      count: {}",
                    plan_text(&plan(&conn, &c.count_sql(), &c.bindings()))
                );
                if shape.query.count {
                    println!(
                        "      malformed count: {}",
                        plan_text(&plan(&conn, &c.malformed_count_sql(), &c.bindings()))
                    );
                }
                println!(
                    "      page:  {}",
                    plan_text(&plan(&conn, &c.page_sql(true), &with_limit))
                );
            }
        }
        println!(
            "  fetch by rowid: {}",
            plan_text(&plan(&conn, &fetch_by_rowid_sql(), &[]))
        );
    }

    // ---- Cancelling ------------------------------------------------------------
    // The flag is set a quarter of the way through an uncancelled scan, so the
    // scan is certainly still running, at any size of database.
    let scan = query(None, |h| h.device = Some("zzzz".to_string()));
    let t = Instant::now();
    s.query_history(&scan, flag()).unwrap();
    let full = t.elapsed();
    let set_after = full / 4;
    println!(
        "\n== A cancelled scan (needle that matches nothing, all time: {:.0} ms when left alone; flag set {:.0} ms in)",
        ms(full),
        ms(set_after)
    );
    let mut stops = Vec::new();
    for _ in 0..3 {
        let cancel = flag();
        let setter = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(set_after);
                cancel.store(true, Ordering::Relaxed);
                Instant::now()
            })
        };
        let result = s.query_history(&scan, cancel);
        let returned = Instant::now();
        let set_at = setter.join().unwrap();
        assert!(matches!(result, Err(SnifferError::Cancelled)), "{result:?}");
        stops.push(returned.saturating_duration_since(set_at));
    }
    println!(
        "  time from setting the flag to the return, ms min / median / max: {}",
        spread(&stops)
    );
    let cancel_stop = *stops.iter().max().unwrap();

    // ---- The writer ------------------------------------------------------------
    let slow: Vec<&HistoryQuery> = shapes
        .iter()
        .filter(|s| s.compare || s.query.lower.is_some())
        .map(|s| &s.query)
        .collect();
    println!(
        "\n== The record writer: one insert every 2 ms; {} queries in rotation (all the shapes above that scan or count a range)",
        slow.len()
    );

    // No queries.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = spawn_writer(&s, stop.clone());
    std::thread::sleep(Duration::from_secs(quiet_secs));
    stop.store(true, Ordering::Relaxed);
    let alone = writer.join().unwrap();
    let wal_alone = wal_size(&path);
    println!("  A: {quiet_secs} s with no queries");
    let (alone_wait, alone_write) = report_writer("no queries", &alone);
    println!(
        "  log at the end of A: {:.2} MiB file, {:?} frames",
        mib(wal_alone),
        wal_frames(&path)
    );

    // Sustained queries on the reader, then the recovery.
    let sustained = sustained_phase("B", &s, &path, &slow, sustained_secs);

    // The same again with a limit on the log file's size, to show what the
    // setting would do.  The log is emptied first so both start alike.
    s.conn
        .lock()
        .unwrap()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    s.conn
        .lock()
        .unwrap()
        .query_row("PRAGMA journal_size_limit = 8388608", [], |_| Ok(()))
        .unwrap();
    println!("  (not a pass mark) the same with PRAGMA journal_size_limit = 8 MiB on the writer");
    let limited = sustained_phase("B with limit", &s, &path, &slow, sustained_secs);
    s.conn
        .lock()
        .unwrap()
        .query_row("PRAGMA journal_size_limit = 8388608", [], |_| Ok(()))
        .unwrap();

    // The contrast: the same queries on the writer's own connection.
    let on_writer = s.on_writer_only();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = spawn_writer(&s, stop.clone());
    let contrast_times = hammer(&on_writer, &slow, Duration::from_secs(contrast_secs));
    stop.store(true, Ordering::Relaxed);
    let contrast = writer.join().unwrap();
    println!("  C: {contrast_secs} s of the same queries on the writer's connection (contrast)");
    let (con_wait, _) = report_writer("queries on the writer", &contrast);
    let q = summarize(&contrast_times);
    println!(
        "  {} queries ran: p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
        q.n,
        ms(q.p50),
        ms(q.p99),
        ms(q.max)
    );

    // ---- Verdicts --------------------------------------------------------------
    let rss = peak_rss_kb().unwrap_or(0);
    println!("\n== Pass marks");
    let lock_ok = sustained.wait.max < Duration::from_millis(10);
    println!(
        "  {}  the writer's longest lock wait with queries on the reader under 10 ms: {:.3} ms (no queries: {:.3} ms)",
        verdict(lock_ok),
        ms(sustained.wait.max),
        ms(alone_wait.max)
    );
    let write_ok = sustained.write.max < Duration::from_secs(1);
    println!(
        "  {}  the longest write under 1 s: {:.3} ms with queries (no queries: {:.3} ms)",
        verdict(write_ok),
        ms(sustained.write.max),
        ms(alone_write.max)
    );
    let final_wal = sustained.after.last().map(|a| a.size).unwrap_or(0);
    // The writer truncates the log file back to its size limit
    // after a restart, so that size counts as near the start too.
    let near = wal_alone
        .saturating_mul(2)
        .max(wal_alone + 4 * 1_048_576)
        .max(super::WAL_SIZE_LIMIT_BYTES as u64);
    let wal_ok = final_wal <= near;
    println!(
        "  {}  the log file back near its starting size ({:.2} MiB at the end of A; near means at most {:.2} MiB) 10 s after the queries: {:.2} MiB (peak {:.2} MiB)",
        verdict(wal_ok),
        mib(wal_alone),
        mib(near),
        mib(final_wal),
        mib(sustained.wal_peak)
    );
    println!(
        "        frames in the log 10 s after: {:?} (the log is restarted when the queries stop; the file keeps its size)",
        sustained.after.last().and_then(|a| a.frames)
    );
    let limited_ok = limited
        .after
        .last()
        .map(|a| a.size <= 8 * 1_048_576 + 4 * 1_048_576)
        .unwrap_or(false);
    println!(
        "  info  with journal_size_limit = 8 MiB: log file {:.2} MiB 10 s after the queries (peak {:.2} MiB), {}; longest lock wait {:.3} ms, longest write {:.3} ms",
        mib(limited.after.last().map(|a| a.size).unwrap_or(0)),
        mib(limited.wal_peak),
        verdict(limited_ok),
        ms(limited.wait.max),
        ms(limited.write.max)
    );
    let contrast_ok = con_wait.max > Duration::from_millis(100);
    println!(
        "  {}  the contrast run shows a stall: longest lock wait {:.1} ms with the queries on the writer's connection",
        verdict(contrast_ok),
        ms(con_wait.max)
    );
    println!(
        "  info  a cancelled scan stopped {:.1} ms after the flag was set (worst of 3)",
        ms(cancel_stop)
    );
    println!(
        "  info  peak resident set of this test process: {:.0} MiB",
        rss as f64 / 1024.0
    );
    if let Some((name, times)) = checkpoint {
        let worst = times.iter().max().copied().unwrap();
        println!(
            "  {}  checkpoint: {name}: {:.0} ms worst of 3, {:.0} ms median (limit 1000 ms on 1,000,000 rows); a user decision, not a failure of the test",
            if worst > Duration::from_secs(1) {
                "OVER "
            } else {
                "within"
            },
            ms(worst),
            ms(median(&times))
        );
    }
    println!(
        "  fixture: {} rows over {} days, database {:.0} MiB",
        fixture.rows + 6,
        fixture.days,
        mib(std::fs::metadata(&path).unwrap().len())
    );

    assert!(
        lock_ok,
        "the writer waited {:?} for its lock",
        sustained.wait.max
    );
    assert!(write_ok, "a write took {:?}", sustained.write.max);
    assert!(wal_ok, "the log file is still {:.2} MiB", mib(final_wal));
    assert!(contrast_ok, "the contrast run did not stall the writer");
}

/// The log after a phase: when, its file size, and the frames it holds.
struct LogSample {
    size: u64,
    frames: Option<u32>,
}

struct Phase {
    wait: Summary,
    write: Summary,
    wal_peak: u64,
    /// The log 1, 3 and 10 seconds after the queries ended.
    after: Vec<LogSample>,
}

/// `secs` seconds of queries back to back on the reader while the writer
/// inserts, then ten seconds of the writer alone, with the log's size sampled
/// throughout.
fn sustained_phase(
    name: &str,
    s: &Storage,
    path: &str,
    slow: &[&HistoryQuery],
    secs: u64,
) -> Phase {
    let wal_stop = Arc::new(AtomicBool::new(false));
    let sampler = spawn_wal_sampler(path.to_string(), wal_stop.clone());
    let stop = Arc::new(AtomicBool::new(false));
    let writer = spawn_writer(s, stop.clone());
    let query_times = hammer(s, slow, Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    let during = writer.join().unwrap();
    let ended = Instant::now();
    println!("  {name}: {secs} s of queries back to back on the reader");
    let (wait, write) = report_writer("queries on the reader", &during);
    let q = summarize(&query_times);
    println!(
        "  {} queries ran: p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
        q.n,
        ms(q.p50),
        ms(q.p99),
        ms(q.max)
    );

    // The writer goes on; the queries stop.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = spawn_writer(s, stop.clone());
    let mut after = Vec::new();
    for secs in [1u64, 3, 10] {
        let until = ended + Duration::from_secs(secs);
        std::thread::sleep(until.saturating_duration_since(Instant::now()));
        after.push(LogSample {
            size: wal_size(path),
            frames: wal_frames(path),
        });
    }
    stop.store(true, Ordering::Relaxed);
    let recovery = writer.join().unwrap();
    wal_stop.store(true, Ordering::Relaxed);
    let samples = sampler.join().unwrap();
    println!("  after {name}: the writer alone for 10 s");
    report_writer("after the queries", &recovery);
    let wal_peak = samples.iter().map(|(_, size)| *size).max().unwrap_or(0);
    println!(
        "  log file: peak {:.2} MiB; after the queries ended: {}",
        mib(wal_peak),
        ["+1 s", "+3 s", "+10 s"]
            .iter()
            .zip(&after)
            .map(|(when, a)| format!("{when} {:.2} MiB ({:?} frames)", mib(a.size), a.frames))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut every_second: Vec<(u64, u64)> = Vec::new();
    for (at, size) in &samples {
        if every_second.last().is_none_or(|(t, _)| at.as_secs() > *t) {
            every_second.push((at.as_secs(), *size));
        }
    }
    println!(
        "  log file MiB each second: {}",
        every_second
            .iter()
            .map(|(t, size)| format!("{t}:{:.1}", mib(*size)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    Phase {
        wait,
        write,
        wal_peak,
        after,
    }
}
