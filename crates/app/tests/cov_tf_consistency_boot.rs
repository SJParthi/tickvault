//! Coverage tests for `tf_consistency_boot`: the async orchestrator
//! (`run_tf_consistency` → private `run_tf_pass` / `http_get_text`) driven
//! against a canned QuestDB `/exec` + ILP `/write` server on 127.0.0.1, plus
//! a few pure-fn edge arms.
//!
//! Every scenario uses a PAST trading date, so the readiness wait returns at
//! once with no floor (it only waits when the date is today).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::NaiveDate;
use serde_json::{Value, json};
use tickvault_app::tf_consistency_boot::{
    CandleRow, TF_VERIFY_DISCOVERY_ROW_LIMIT, TF_VERIFY_MAX_AUDIT_ROWS_PER_RUN,
    TF_VERIFY_MAX_RESPONSE_BYTES, TF_VERIFY_TF_UNION_ROW_LIMIT, TfConsistencySummaryData,
    bucket_grid, compare_tf, ist_day_start_nanos, parse_tf_union_dataset, previous_trading_day,
    recompute_window, run_tf_consistency, tf_verify_targets,
};
use tickvault_common::config::{QuestDbConfig, TradingConfig};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_storage::tf_consistency_audit_persistence::FindingCategory;
use tickvault_trading::candles::TfIndex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// Canned HTTP server
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Resp {
    status: u16,
    body: Vec<u8>,
    /// Declare this Content-Length instead of the real one (body unsent).
    declared_len: Option<usize>,
    /// Send no Content-Length at all; the body ends at connection close.
    omit_len: bool,
}

impl Resp {
    fn json(v: &Value) -> Self {
        Self {
            status: 200,
            body: v.to_string().into_bytes(),
            declared_len: None,
            omit_len: false,
        }
    }
    fn raw(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.as_bytes().to_vec(),
            declared_len: None,
            omit_len: false,
        }
    }
}

type Handler = Arc<dyn Fn(&str, &str) -> Resp + Send + Sync>;

struct Mock {
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    fn requests(&self) -> Vec<String> {
        self.log.lock().expect("log lock").clone()
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn serve_one(mut s: TcpStream, handler: Handler, log: Arc<Mutex<Vec<String>>>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = vec![0u8; 64 * 1024];
    let header_end = loop {
        let Ok(n) = s.read(&mut tmp).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            let lower = l.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let Ok(n) = s.read(&mut tmp).await else {
            return;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let target = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let url = reqwest::Url::parse(&format!("http://mock{target}")).expect("request target");
    let path = url.path().to_string();
    let query = url
        .query_pairs()
        .find(|(k, _)| k == "query")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    log.lock()
        .expect("log lock")
        .push(format!("{path} {query}"));
    let resp = handler(&path, &query);
    let mut out = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", resp.status);
    if let Some(declared) = resp.declared_len {
        out.push_str(&format!("Content-Length: {declared}\r\n"));
    } else if !resp.omit_len {
        out.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    out.push_str("\r\n");
    let _ = s.write_all(out.as_bytes()).await;
    if resp.declared_len.is_none() {
        let _ = s.write_all(&resp.body).await;
    }
    let _ = s.flush().await;
    let _ = s.shutdown().await;
}

async fn spawn_mock(handler: Handler) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(serve_one(s, Arc::clone(&handler), Arc::clone(&log2)));
        }
    });
    Mock { port, log }
}

fn init_tracing() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::TRACE)
            .try_init();
    });
}

fn qcfg(port: u16) -> QuestDbConfig {
    init_tracing();
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: port,
        pg_port: 1,
        ilp_port: 1,
    }
}

// ---------------------------------------------------------------------------
// Scenario routing
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Scenario {
    discovery: Option<Resp>,
    count: Option<Resp>,
    one_m: HashMap<i64, Resp>,
    union: HashMap<i64, Resp>,
    write_status: Option<u16>,
}

fn sid_in(q: &str) -> Option<i64> {
    let rest = &q[q.find("security_id = ")? + "security_id = ".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn handler_for(sc: Scenario) -> Handler {
    Arc::new(move |path: &str, q: &str| {
        if path.starts_with("/write") {
            return Resp::raw(sc.write_status.unwrap_or(204), "");
        }
        if q.starts_with("SELECT DISTINCT security_id, segment") {
            return sc
                .discovery
                .clone()
                .unwrap_or_else(|| Resp::json(&json!({"dataset": []})));
        }
        if q.starts_with("SELECT count(*)") {
            return sc
                .count
                .clone()
                .unwrap_or_else(|| Resp::json(&json!({"dataset": [[0]]})));
        }
        if let Some(sid) = sid_in(q) {
            if q.starts_with("SELECT * FROM (") {
                return sc
                    .union
                    .get(&sid)
                    .cloned()
                    .unwrap_or_else(|| Resp::json(&json!({"dataset": []})));
            }
            return sc
                .one_m
                .get(&sid)
                .cloned()
                .unwrap_or_else(|| Resp::json(&json!({"dataset": []})));
        }
        // DDL (CREATE / ALTER) and anything else.
        Resp::raw(200, "{\"ddl\":\"OK\"}")
    })
}

// ---------------------------------------------------------------------------
// Data builders
// ---------------------------------------------------------------------------

/// A past Monday (a trading day; never today).
fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2025, 6, 2).expect("date")
}

fn ds() -> i64 {
    ist_day_start_nanos(day())
}

fn row(sod: i64, price: f64, volume: i64) -> CandleRow {
    CandleRow {
        ts_nanos: ds() + sod * 1_000_000_000,
        open: price,
        high: price + 0.5,
        low: price - 0.5,
        close: price + 0.25,
        volume,
        tick_count: 3,
    }
}

/// Every minute 09:15..15:40 (385 rows) with a varying price.
fn full_day_1m() -> Vec<CandleRow> {
    (0..385)
        .map(|m| row(33_300 + m * 60, 100.0 + ((m % 7) as f64) * 0.25, 10 + m))
        .collect()
}

/// The stored higher-TF rows a correct fold would write for `rows_1m`.
fn tf_rows_for(rows_1m: &[CandleRow]) -> Vec<(&'static str, CandleRow)> {
    let mut out = Vec::new();
    for tf in tf_verify_targets() {
        for w in bucket_grid(tf.seconds_per_bucket()) {
            let start = i64::from(w.start_secs_of_day);
            let end = i64::from(w.end_effective_secs_of_day);
            let members: Vec<CandleRow> = rows_1m
                .iter()
                .copied()
                .filter(|r| {
                    let sod = (r.ts_nanos - ds()) / 1_000_000_000;
                    sod >= start && sod < end
                })
                .collect();
            if let Ok(Some(rec)) = recompute_window(&members) {
                out.push((
                    tf.display_name(),
                    CandleRow {
                        ts_nanos: ds() + start * 1_000_000_000,
                        open: rec.open,
                        high: rec.high,
                        low: rec.low,
                        close: rec.close,
                        volume: rec.volume,
                        tick_count: rec.tick_count,
                    },
                ));
            }
        }
    }
    out
}

fn one_m_json(rows: &[CandleRow]) -> Resp {
    let ds: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!([
                r.ts_nanos,
                r.open,
                r.high,
                r.low,
                r.close,
                r.volume,
                r.tick_count
            ])
        })
        .collect();
    Resp::json(&json!({ "dataset": ds }))
}

fn union_json(rows: &[(&str, CandleRow)]) -> Resp {
    let ds: Vec<Value> = rows
        .iter()
        .map(|(l, r)| {
            json!([
                l,
                r.ts_nanos,
                r.open,
                r.high,
                r.low,
                r.close,
                r.volume,
                r.tick_count
            ])
        })
        .collect();
    Resp::json(&json!({ "dataset": ds }))
}

fn discovery(pairs: &[(i64, &str)]) -> Resp {
    let ds: Vec<Value> = pairs.iter().map(|(s, g)| json!([s, g])).collect();
    Resp::json(&json!({ "dataset": ds }))
}

async fn run(sc: Scenario) -> (TfConsistencySummaryData, Mock) {
    let mock = spawn_mock(handler_for(sc)).await;
    let summary = run_tf_consistency(&qcfg(mock.port), day()).await;
    (summary, mock)
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

// ---------------------------------------------------------------------------
// Orchestrator scenarios
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_unreachable_questdb_reads_blind_and_degraded() {
    let s = run_tf_consistency(&qcfg(free_port()), day()).await;
    assert_eq!(s.dhan_date_ist, "2025-06-02");
    assert_eq!(s.status_label, "blind");
    assert!(s.degraded, "unreachable reads must degrade");
    assert_eq!(s.instruments, 0);
    assert_eq!(s.buckets_compared, 0);
    assert!(!s.truncated);
    assert!(s.top_detail.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_discovery_and_count_http_500_reads_blind() {
    let sc = Scenario {
        discovery: Some(Resp::raw(500, "boom")),
        count: Some(Resp::raw(500, "boom")),
        ..Scenario::default()
    };
    let (s, mock) = run(sc).await;
    assert_eq!(s.status_label, "blind");
    assert!(s.degraded);
    assert_eq!(s.instruments, 0);
    let reqs = mock.requests();
    assert!(
        reqs.iter()
            .any(|r| r.contains("SELECT count(*) FROM ticks")),
        "empty discovery must ask the source table: {reqs:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_discovery_malformed_json_reads_blind_degraded() {
    let sc = Scenario {
        discovery: Some(Resp::raw(200, "not json at all")),
        count: Some(Resp::json(&json!({"dataset": [[0]]}))),
        ..Scenario::default()
    };
    let (s, _mock) = run(sc).await;
    assert_eq!(s.status_label, "blind");
    assert!(s.degraded);
    assert_eq!(s.instruments, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_empty_discovery_and_empty_source_reads_no_data() {
    let sc = Scenario {
        discovery: Some(discovery(&[])),
        count: Some(Resp::json(&json!({"dataset": [[0]]}))),
        ..Scenario::default()
    };
    let (s, mock) = run(sc).await;
    assert_eq!(s.status_label, "no_data");
    assert!(!s.degraded);
    assert_eq!(s.instruments, 0);
    // The ensure DDL ran against the same server.
    assert!(mock.requests().iter().any(|r| r.contains("ALTER TABLE")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_empty_discovery_with_source_rows_reads_blind() {
    let sc = Scenario {
        discovery: Some(discovery(&[])),
        count: Some(Resp::json(&json!({"dataset": [[42]]}))),
        ..Scenario::default()
    };
    let (s, _mock) = run(sc).await;
    assert_eq!(
        s.status_label, "blind",
        "ticks without candles is the dead-fold class"
    );
    assert!(!s.degraded, "the count read itself succeeded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_empty_discovery_count_malformed_degrades() {
    let sc = Scenario {
        discovery: Some(discovery(&[])),
        count: Some(Resp::json(&json!({"dataset": []}))),
        ..Scenario::default()
    };
    let (s, _mock) = run(sc).await;
    assert_eq!(s.status_label, "blind");
    assert!(
        s.degraded,
        "an unparseable count is a failed read, never zero"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_discovery_declared_oversize_is_refused() {
    let mut big = Resp::raw(200, "");
    big.declared_len = Some(TF_VERIFY_MAX_RESPONSE_BYTES + 1);
    let sc = Scenario {
        discovery: Some(big),
        count: Some(Resp::json(&json!({"dataset": [[0]]}))),
        ..Scenario::default()
    };
    let (s, _mock) = run(sc).await;
    assert_eq!(s.status_label, "blind");
    assert!(s.degraded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_discovery_streamed_oversize_body_is_refused() {
    // No Content-Length: the post-read cap is the only defense.
    let mut big = Resp::raw(200, "");
    big.body = vec![b' '; TF_VERIFY_MAX_RESPONSE_BYTES + 1];
    big.omit_len = true;
    let sc = Scenario {
        discovery: Some(big),
        count: Some(Resp::json(&json!({"dataset": [[0]]}))),
        ..Scenario::default()
    };
    let (s, _mock) = run(sc).await;
    assert_eq!(s.status_label, "blind");
    assert!(s.degraded);
    assert_eq!(s.instruments, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_consistent_instrument_reads_pass() {
    let rows = full_day_1m();
    let tf = tf_rows_for(&rows);
    let mut sc = Scenario {
        discovery: Some(discovery(&[(5001, "IDX_I")])),
        ..Scenario::default()
    };
    sc.one_m.insert(5001, one_m_json(&rows));
    sc.union.insert(5001, union_json(&tf));
    let (s, mock) = run(sc).await;
    assert_eq!(s.status_label, "pass", "summary: {s:?}");
    assert_eq!(s.instruments, 1);
    assert_eq!(s.buckets_compared, tf.len() as u64);
    assert_eq!(s.mismatches, 0);
    assert_eq!(s.missing_tf_rows, 0);
    assert_eq!(s.duplicates, 0);
    assert!(!s.degraded);
    assert!(s.top_detail.is_empty());
    let reqs = mock.requests();
    assert!(
        !reqs.iter().any(|r| r.starts_with("/write")),
        "nothing to persist on a clean pass: {reqs:?}"
    );
    assert!(reqs.iter().any(|r| r.contains("FROM candles_1m")));
}

/// Builds the mismatch fixture: a changed 5m close, an off-grid 15m row and a
/// duplicated 1m minute.
fn mismatch_scenario(sid: i64, write_status: u16) -> Scenario {
    let mut rows = full_day_1m();
    let dup = rows[0];
    rows.insert(1, dup);
    let clean: Vec<CandleRow> = full_day_1m();
    let mut tf = tf_rows_for(&clean);
    let m5 = TfIndex::ALL
        .into_iter()
        .find(|t| t.seconds_per_bucket() == 300)
        .expect("5m frame")
        .display_name();
    let idx = tf.iter().position(|(l, _)| *l == m5).expect("a 5m row");
    tf[idx].1.close += 1.0;
    let m15 = TfIndex::ALL
        .into_iter()
        .find(|t| t.seconds_per_bucket() == 900)
        .expect("15m frame")
        .display_name();
    tf.push((m15, row(33_300 + 60, 100.0, 1)));
    let mut sc = Scenario {
        discovery: Some(discovery(&[(sid, "IDX_I")])),
        write_status: Some(write_status),
        ..Scenario::default()
    };
    sc.one_m.insert(sid, one_m_json(&rows));
    sc.union.insert(sid, union_json(&tf));
    sc
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_mismatch_offgrid_duplicate_persists_findings() {
    let (s, mock) = run(mismatch_scenario(5002, 204)).await;
    assert_eq!(s.status_label, "mismatch", "summary: {s:?}");
    assert!(s.mismatches >= 1);
    assert_eq!(s.off_grid, 1);
    assert_eq!(s.duplicates, 1);
    assert!(!s.degraded, "the audit flush succeeded");
    assert!(!s.top_detail.is_empty());
    assert!(s.top_detail.iter().any(|d| d.contains("5002")));
    assert!(
        mock.requests().iter().any(|r| r.starts_with("/write")),
        "findings must be flushed to the audit table"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_audit_flush_rejected_degrades_the_run() {
    let (s, mock) = run(mismatch_scenario(5003, 500)).await;
    assert_eq!(s.status_label, "mismatch");
    assert!(s.degraded, "a refused audit flush must degrade the run");
    assert!(mock.requests().iter().any(|r| r.starts_with("/write")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_per_instrument_failure_matrix_reads_degraded() {
    let good = full_day_1m();
    let good_tf = tf_rows_for(&good);
    let mut sc = Scenario::default();
    let mut pairs: Vec<(i64, &str)> = Vec::new();

    // 6001: poisoned segment (never queried).
    pairs.push((6001, "x' OR '1'='1"));
    // 6002..=6007: out-of-session 1m row -> data-derived exclusion (6 > the
    // 5-sample cap).
    for sid in 6002..=6007 {
        pairs.push((sid, "IDX_I"));
        let mut rows = good.clone();
        rows.insert(0, row(30_000, 100.0, 1));
        sc.one_m.insert(sid, one_m_json(&rows));
    }
    // 6010: 1m truncated (501 in-session rows: duplicates allowed).
    pairs.push((6010, "IDX_I"));
    let mut trunc = good.clone();
    while trunc.len() < 501 {
        trunc.push(good[0]);
    }
    trunc.sort_by_key(|r| r.ts_nanos);
    sc.one_m.insert(6010, one_m_json(&trunc));
    // 6011: 1m HTTP 500.
    pairs.push((6011, "IDX_I"));
    sc.one_m.insert(6011, Resp::raw(500, "x"));
    // 6012: 1m malformed.
    pairs.push((6012, "IDX_I"));
    sc.one_m.insert(6012, Resp::raw(200, "{"));
    // 6013: union truncated.
    pairs.push((6013, "IDX_I"));
    sc.one_m.insert(6013, one_m_json(&good));
    let mut big_union = good_tf.clone();
    while big_union.len() <= TF_VERIFY_TF_UNION_ROW_LIMIT {
        big_union.push(good_tf[0]);
    }
    sc.union.insert(6013, union_json(&big_union));
    // 6014: union HTTP 500.
    pairs.push((6014, "IDX_I"));
    sc.one_m.insert(6014, one_m_json(&good));
    sc.union.insert(6014, Resp::raw(503, "x"));
    // 6015: union malformed.
    pairs.push((6015, "IDX_I"));
    sc.one_m.insert(6015, one_m_json(&good));
    sc.union.insert(6015, Resp::raw(200, "[]"));
    // 6016: volume overflow in the first window of every frame.
    pairs.push((6016, "IDX_I"));
    sc.one_m.insert(
        6016,
        one_m_json(&[row(33_300, 100.0, i64::MAX), row(33_360, 100.0, i64::MAX)]),
    );
    // 6017: clean.
    pairs.push((6017, "IDX_I"));
    sc.one_m.insert(6017, one_m_json(&good));
    sc.union.insert(6017, union_json(&good_tf));

    sc.discovery = Some(discovery(&pairs));
    let (s, mock) = run(sc).await;
    // Examined: 6010..=6017 (8). Excluded: 6001 (bad segment), 6002..=6007.
    assert_eq!(s.instruments, 8, "summary: {s:?}");
    assert_eq!(s.buckets_compared, good_tf.len() as u64);
    assert!(s.degraded);
    assert_eq!(s.status_label, "degraded");
    assert_eq!(s.mismatches, 0);
    let reqs = mock.requests();
    assert!(
        !reqs.iter().any(|r| r.contains("OR '1'='1")),
        "a poisoned segment must never reach a follow-up query"
    );
    for excluded in 6002..=6007 {
        assert!(
            !reqs.iter().any(|r| r.starts_with("/exec SELECT * FROM (")
                && r.contains(&format!("security_id = {excluded} "))),
            "an out-of-session instrument is never compared ({excluded})"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_audit_row_cap_truncates_and_counts_all() {
    // 40 instruments with full 1m days and NO higher-TF rows: every window is
    // a missing row, ~291 per instrument, past the 10,000-row audit cap.
    let good = full_day_1m();
    let per_sid = tf_rows_for(&good).len() as u64;
    let mut sc = Scenario::default();
    let mut pairs = Vec::new();
    for sid in 8001..8041 {
        pairs.push((sid, "IDX_I"));
        sc.one_m.insert(sid, one_m_json(&good));
    }
    sc.discovery = Some(discovery(&pairs));
    let (s, mock) = run(sc).await;
    assert_eq!(s.instruments, 40);
    assert_eq!(s.missing_tf_rows, 40 * per_sid);
    assert!(40 * per_sid > TF_VERIFY_MAX_AUDIT_ROWS_PER_RUN as u64);
    assert!(s.truncated, "past the audit cap the run reads truncated");
    assert_eq!(s.status_label, "blind", "zero buckets compared: {s:?}");
    assert!(s.top_detail.len() <= 10);
    assert!(mock.requests().iter().any(|r| r.starts_with("/write")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tf_consistency_boot_run_always_on_instrument_is_excluded() {
    let idx_code =
        tickvault_common::segment::segment_str_to_code("IDX_I").expect("IDX_I is a segment");
    let mut set = HashSet::new();
    set.insert((7001_u64, idx_code));
    tickvault_common::always_on::init_always_on_segments(set);
    let mut sc = Scenario {
        discovery: Some(discovery(&[(7001, "IDX_I")])),
        ..Scenario::default()
    };
    sc.one_m.insert(7001, one_m_json(&full_day_1m()));
    let (s, mock) = run(sc).await;
    assert_eq!(s.instruments, 0);
    assert_eq!(s.buckets_compared, 0);
    assert_eq!(s.status_label, "blind", "rows seen, nothing compared");
    assert!(
        !mock
            .requests()
            .iter()
            .any(|r| r.contains("security_id = 7001 ")),
        "an always-on instrument is never queried"
    );
}

#[tokio::test(start_paused = true)]
async fn test_tf_consistency_boot_run_discovery_truncation_tripwire_reads_degraded() {
    // LIMIT+1 rows, every segment poisoned so no follow-up query runs; the
    // politeness sleeps advance on the paused clock.
    let rows: Vec<Value> = (0..=TF_VERIFY_DISCOVERY_ROW_LIMIT as i64)
        .map(|i| json!([i + 1, "BAD"]))
        .collect();
    let sc = Scenario {
        discovery: Some(Resp::json(&json!({ "dataset": rows }))),
        ..Scenario::default()
    };
    let (s, mock) = run(sc).await;
    assert!(s.degraded);
    assert_eq!(s.instruments, 0);
    assert_eq!(s.status_label, "blind");
    assert!(
        !mock.requests().iter().any(|r| r.contains("security_id = ")),
        "no follow-up query for refused segments"
    );
}

// ---------------------------------------------------------------------------
// Pure-fn edge arms
// ---------------------------------------------------------------------------

#[test]
fn test_tf_consistency_boot_previous_trading_day_seven_closed_days_is_none() {
    // Mon 2025-06-02 .. Fri 2025-06-06 holidays; looking back from Mon
    // 2025-06-09 crosses Sun, Sat and the five holidays: seven closed days.
    let holidays: Vec<Value> = (2..=6)
        .map(|d| json!({"date": format!("2025-06-0{d}"), "name": "test"}))
        .collect();
    let cfg: TradingConfig = serde_json::from_value(json!({
        "market_open_time": "09:15:00",
        "market_close_time": "15:30:00",
        "order_cutoff_time": "15:29:00",
        "data_collection_start": "09:00:00",
        "data_collection_end": "16:00:00",
        "timezone": "Asia/Kolkata",
        "max_orders_per_second": 10,
        "nse_holidays": holidays,
    }))
    .expect("trading config");
    let cal = TradingCalendar::from_config(&cfg).expect("calendar");
    let from = NaiveDate::from_ymd_opt(2025, 6, 9).expect("date");
    assert_eq!(previous_trading_day(&cal, from), None);
    // One day later the lookback reaches Mon 2025-06-09 itself.
    let from2 = NaiveDate::from_ymd_opt(2025, 6, 10).expect("date");
    assert_eq!(previous_trading_day(&cal, from2), Some(from));
}

#[test]
fn test_tf_consistency_boot_parse_tf_union_non_string_label_row_is_skipped() {
    let label = tf_verify_targets()[0].display_name();
    let body = json!({"dataset": [
        [7, 1, 1.0, 1.0, 1.0, 1.0, 1, 1],
        [label, 1_000, 1.0, 2.0, 0.5, 1.5, 10, 2],
        "not a row"
    ]})
    .to_string();
    let (rows, truncated) = parse_tf_union_dataset(&body, 10).expect("parse");
    assert!(!truncated);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.ts_nanos, 1_000);
    assert_eq!(rows[0].1.volume, 10);
}

#[test]
fn test_tf_consistency_boot_compare_tf_skips_pre_session_1m_rows() {
    // A 1m row at 09:00 sits before every window: the cursor walks past it
    // and it lands in no bucket.
    let tf = tf_verify_targets()[0];
    let pre = row(32_400, 50.0, 999);
    let first = row(33_300, 100.0, 10);
    let (findings, counts) = compare_tf(tf, &[pre, first], &[], ds(), None);
    let missing: Vec<_> = findings
        .iter()
        .filter(|f| f.category == FindingCategory::MissingTfRow)
        .collect();
    assert_eq!(missing.len(), 1, "only the 09:15 window has a member");
    assert_eq!(missing[0].bucket_secs_of_day, 33_300);
    assert_eq!(
        missing[0].recomputed_volume, 10,
        "the 09:00 row is not counted"
    );
    assert_eq!(missing[0].one_m_rows, 1);
    assert_eq!(counts.buckets_compared, 0);
}
