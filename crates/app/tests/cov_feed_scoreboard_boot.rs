//! Coverage for `tickvault_app::feed_scoreboard_boot` — the two QuestDB
//! orchestrations (`run_feed_scoreboard`, `reconcile_process_death_episodes`,
//! plus `day_already_scored_complete`) and the pure helpers' rarely taken
//! branches.
//!
//! The orchestrations talk to QuestDB over HTTP only (`/exec` reads and DDL,
//! ILP-over-HTTP `/write` for rows, `/settings` for the ILP protocol probe),
//! so each test starts a tiny fake QuestDB on `127.0.0.1:0` in its own OS
//! thread and answers by SQL shape. Nothing leaves the machine:
//! `TV_ENVIRONMENT` is set to an invalid name, so the deploy-sha SSM read
//! fails closed before it builds an AWS client.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use tickvault_app::feed_scoreboard_boot::{
    ExistingDailyCoverage, FeedDayNumbers, JsonlCodeEvent, WsAuditEventLite,
    collect_correlation_evidence, day_already_scored_complete, fold_episode_readback_rows,
    fold_existing_coverage_keep_better, parse_existing_daily_coverage, parse_existing_daily_lag,
    parse_existing_episode_partiality, parse_prom_counter_sum, parse_ws_events,
    reconcile_process_death_episodes, run_feed_scoreboard, scan_errors_jsonl_for_correlation,
    synthesize_process_death_episodes,
};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::feed_blame::{BlameClass, EPISODE_KIND_PROCESS_DEATH};
use tickvault_storage::feed_episode_audit_persistence::FeedEpisodeAuditRow;
use tickvault_storage::feed_scoreboard_persistence::CoverageSource;

// ---------------------------------------------------------------------------
// Process setup
// ---------------------------------------------------------------------------

fn setup_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let existing = std::env::var("NO_PROXY").unwrap_or_default();
        let no_proxy = if existing.is_empty() {
            "127.0.0.1".to_string()
        } else {
            format!("127.0.0.1,{existing}")
        };
        // SAFETY: set once, before any HTTP client in this binary is built.
        unsafe {
            // Invalid characters: `resolve_environment` refuses it, so the
            // deploy-sha read returns None without touching AWS.
            std::env::set_var("TV_ENVIRONMENT", "not a valid env!");
            std::env::set_var("NO_PROXY", &no_proxy);
            std::env::set_var("no_proxy", &no_proxy);
        }
    });
}

// ---------------------------------------------------------------------------
// The fake QuestDB
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    sql: Option<String>,
    body: String,
}

/// Canned answers, chosen by request shape.
#[derive(Clone)]
struct Responses {
    ws: (u16, String),
    boot: (u16, String),
    partiality: (u16, String),
    episode_day: (u16, String),
    daily: (u16, String),
    dhan_ticks: (u16, String),
    dhan_instruments: (u16, String),
    dhan_minutes: (u16, String),
    other_ticks: (u16, String),
    other_instruments: (u16, String),
    other_minutes: (u16, String),
    metrics: (u16, String),
    write_status: u16,
    default: (u16, String),
}

fn ok(body: &str) -> (u16, String) {
    (200, body.to_string())
}

fn status(code: u16) -> (u16, String) {
    (code, "{\"error\":\"nope\"}".to_string())
}

const EMPTY: &str = "{\"dataset\":[]}";

impl Responses {
    fn all(answer: (u16, String), write_status: u16) -> Self {
        Self {
            ws: answer.clone(),
            boot: answer.clone(),
            partiality: answer.clone(),
            episode_day: answer.clone(),
            daily: answer.clone(),
            dhan_ticks: answer.clone(),
            dhan_instruments: answer.clone(),
            dhan_minutes: answer.clone(),
            other_ticks: answer.clone(),
            other_instruments: answer.clone(),
            other_minutes: answer.clone(),
            metrics: answer.clone(),
            write_status,
            default: answer,
        }
    }

    fn pick(&self, path: &str, sql: Option<&str>) -> (u16, String) {
        if path.starts_with("/settings") {
            return (404, String::new());
        }
        if path.starts_with("/write") {
            return (self.write_status, String::new());
        }
        if path.starts_with("/metrics") {
            return self.metrics.clone();
        }
        let Some(sql) = sql else {
            return self.default.clone();
        };
        let dhan = sql.contains("feed = 'dhan'");
        if sql.contains("from ws_event_audit") {
            self.ws.clone()
        } else if sql.contains("detector = 'boot_reconciled'") {
            self.boot.clone()
        } else if sql.contains("run_partial") && sql.contains("from feed_episode_audit") {
            self.partiality.clone()
        } else if sql.contains("from feed_episode_audit") {
            self.episode_day.clone()
        } else if sql.contains("from feed_scoreboard_daily") {
            self.daily.clone()
        } else if sql.starts_with("select count() from ticks") {
            if dhan {
                self.dhan_ticks.clone()
            } else {
                self.other_ticks.clone()
            }
        } else if sql.starts_with("select count() from (select distinct") {
            if dhan {
                self.dhan_instruments.clone()
            } else {
                self.other_instruments.clone()
            }
        } else if sql.contains("date_trunc('minute', ts) from ticks") {
            if dhan {
                self.dhan_minutes.clone()
            } else {
                self.other_minutes.clone()
            }
        } else {
            self.default.clone()
        }
    }
}

struct FakeQuestDb {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeQuestDb {
    fn start(responses: Responses) -> Self {
        Self::start_with_hook(responses, None)
    }

    /// `on_write` fires (from the server thread) on every `/write` request.
    fn start_with_hook(responses: Responses, on_write: Option<Arc<tokio::sync::Notify>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake QuestDB");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_srv = Arc::clone(&seen);
        let responses = Arc::new(responses);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let r = Arc::clone(&responses);
                let s = Arc::clone(&seen_srv);
                let hook = on_write.clone();
                std::thread::spawn(move || serve_one(stream, &r, &s, hook.as_deref()));
            }
        });
        Self { port, seen }
    }

    fn questdb(&self) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: self.port,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen").clone()
    }

    fn writes(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter(|s| s.path.starts_with("/write"))
            .map(|s| s.body)
            .collect()
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_chunked(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = find(raw, b"\r\n") {
        let size_line = std::str::from_utf8(&raw[..eol]).unwrap_or("0");
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("0").trim(), 16)
            .unwrap_or(0);
        raw = &raw[eol + 2..];
        if size == 0 || raw.len() < size {
            break;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[(size + 2).min(raw.len())..];
    }
    out
}

fn serve_one(
    mut stream: TcpStream,
    responses: &Responses,
    seen: &Mutex<Vec<Seen>>,
    on_write: Option<&tokio::sync::Notify>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 16 * 1024];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut rest = buf[head_end + 4..].to_vec();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut content_length = 0_usize;
    let mut chunked = false;
    let mut expect_continue = false;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        } else if lower.starts_with("expect:") && lower.contains("100-continue") {
            expect_continue = true;
        }
    }
    if expect_continue {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let body = if chunked {
        while find(&rest, b"0\r\n\r\n").is_none() {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => rest.extend_from_slice(&tmp[..n]),
            }
        }
        decode_chunked(&rest)
    } else {
        while rest.len() < content_length {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => rest.extend_from_slice(&tmp[..n]),
            }
        }
        rest
    };
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let sql = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("query="))
        .map(percent_decode);
    let (code, payload) = responses.pick(&path, sql.as_deref());
    seen.lock().expect("seen").push(Seen {
        path: path.clone(),
        sql,
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    if path.starts_with("/write")
        && let Some(n) = on_write
    {
        n.notify_one();
    }
    let reason = match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let response = if code == 204 {
        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_string()
    } else {
        format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        )
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// A port with nothing listening (bound, then released).
fn dead_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A fixed IST day number (2024-10-04): far outside the errors.jsonl
/// evidence horizon, so every run's evidence is partial.
const DAY: u64 = 20_000;

fn ts_micros(secs_of_day: i64) -> i64 {
    (i64::try_from(DAY).expect("day") * 86_400 + secs_of_day) * 1_000_000
}

fn ts_nanos(secs_of_day: i64) -> i64 {
    ts_micros(secs_of_day) * 1_000
}

fn hms(h: i64, m: i64) -> i64 {
    h * 3_600 + m * 60
}

#[allow(clippy::too_many_arguments)]
fn ws_row(
    secs: i64,
    feed: &str,
    ws_type: &str,
    conn: i64,
    kind: &str,
    source: &str,
    code: i64,
    market_hours: bool,
) -> serde_json::Value {
    serde_json::json!([
        ts_micros(secs),
        feed,
        ws_type,
        conn,
        kind,
        source,
        code,
        0,
        market_hours
    ])
}

fn dataset(rows: Vec<serde_json::Value>) -> String {
    serde_json::json!({ "dataset": rows }).to_string()
}

fn boot_row(secs: i64, conn: i64) -> FeedEpisodeAuditRow {
    FeedEpisodeAuditRow {
        ts_ist_nanos: ts_nanos(secs),
        trading_date_ist_nanos: ts_nanos(0),
        feed: "dhan".to_string(),
        ws_type: "main_feed".to_string(),
        connection_index: conn,
        episode_kind: EPISODE_KIND_PROCESS_DEATH,
        blame: BlameClass::Ours,
        blame_reason: "process_restart",
        source: "boot_reconciled".to_string(),
        dhan_code: -1,
        detector: "boot_reconciled",
        down_secs: 0,
        market_hours: true,
        evidence: "test".to_string(),
        run_partial: false,
    }
}

/// The rich, fully answering day: disconnects (one shadowed by an
/// evidence-backed existing row), an off-hours drop, a reconnect, an
/// order-update drop, malformed rows, a boot-reconciled restart, ticks and
/// minutes for Dhan, a silent second feed, and an existing daily row that is
/// `degraded` with a measured lag and registry coverage.
fn rich_responses(dropped_metric: u64) -> Responses {
    let disc_secs = hms(10, 0);
    let ws = dataset(vec![
        ws_row(
            hms(8, 34),
            "dhan",
            "main_feed",
            0,
            "connected",
            "boot",
            -1,
            false,
        ),
        ws_row(
            disc_secs,
            "dhan",
            "main_feed",
            0,
            "disconnected",
            "x",
            805,
            true,
        ),
        ws_row(
            hms(10, 1),
            "dhan",
            "main_feed",
            0,
            "reconnected",
            "x",
            -1,
            true,
        ),
        ws_row(
            hms(11, 0),
            "dhan",
            "main_feed",
            1,
            "disconnected",
            "y",
            807,
            true,
        ),
        ws_row(
            hms(16, 0),
            "dhan",
            "main_feed",
            0,
            "disconnected_off_hours",
            "z",
            -1,
            false,
        ),
        ws_row(
            hms(12, 0),
            "dhan",
            "order_update",
            0,
            "disconnected",
            "o",
            -1,
            true,
        ),
        serde_json::json!(["not-a-ts", "dhan"]),
        serde_json::json!([
            "not-a-ts",
            "dhan",
            "main_feed",
            0,
            "connected",
            "",
            0,
            0,
            false
        ]),
    ]);
    let partiality = dataset(vec![
        serde_json::json!([
            "dhan",
            "main_feed",
            0,
            "disconnect",
            ts_micros(disc_secs),
            false,
            "ours",
            true
        ]),
        serde_json::json!([
            "dhan",
            "main_feed",
            "bad-conn",
            "disconnect",
            1,
            false,
            "x",
            true
        ]),
    ]);
    let boot = dataset(vec![
        // The same key as the in-memory boot row below: deduped.
        serde_json::json!([
            "dhan",
            "process_death",
            "ours",
            true,
            "main_feed",
            2,
            ts_micros(hms(13, 0))
        ]),
        // An earlier boot's restart, read back and folded.
        serde_json::json!([
            "dhan",
            "process_death",
            "ours",
            true,
            "main_feed",
            3,
            ts_micros(hms(12, 30))
        ]),
        serde_json::json!(["short"]),
    ]);
    let daily = dataset(vec![
        serde_json::json!([
            "dhan",
            "degraded",
            10,
            20,
            30,
            100,
            -1,
            -1,
            5,
            1,
            300,
            "in_memory"
        ]),
        serde_json::json!([
            "truedata",
            "feed_off",
            -1,
            -1,
            -1,
            -1,
            -1,
            -1,
            -1,
            -1,
            -1,
            "sql_backfill"
        ]),
        serde_json::json!([5]),
    ]);
    let minutes = dataset(vec![
        serde_json::json!(["2024-10-04T09:15:00.000000Z"]),
        serde_json::json!(["2024-10-04T09:16:00.000000Z"]),
        serde_json::json!(["2024-10-04T15:39:00.000000Z"]),
    ]);
    Responses {
        ws: ok(&ws),
        boot: ok(&boot),
        partiality: ok(&partiality),
        episode_day: ok(EMPTY),
        daily: ok(&daily),
        dhan_ticks: ok("{\"dataset\":[[1234]]}"),
        dhan_instruments: ok("{\"dataset\":[[50]]}"),
        dhan_minutes: ok(&minutes),
        other_ticks: ok("{\"dataset\":[[0]]}"),
        other_instruments: ok("{\"dataset\":[[0]]}"),
        other_minutes: ok(EMPTY),
        metrics: ok(&format!(
            "# TYPE tv_ws_event_audit_dropped_total counter\n\
             tv_ws_event_audit_dropped_total{{ws_type=\"main_feed\"}} {dropped_metric}\n"
        )),
        write_status: 204,
        default: ok(EMPTY),
    }
}

// ---------------------------------------------------------------------------
// run_feed_scoreboard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_feed_scoreboard_run_rich_day_keeps_degraded_lag_coverage_and_feed_off() {
    setup_env();
    let server = FakeQuestDb::start(rich_responses(0));
    let boot_rows = vec![boot_row(hms(13, 0), 2), {
        // A row for another day: filtered out of this day's fold.
        let mut r = boot_row(hms(13, 0), 4);
        r.ts_ist_nanos += 86_400 * 1_000_000_000;
        r
    }];
    let summary = run_feed_scoreboard(
        &server.questdb(),
        server.port,
        DAY,
        "2024-10-04".to_string(),
        true,
        true,
        true,
        1,
        &boot_rows,
        Some((true, true)),
    )
    .await
    .expect("a day with answers is measured");

    let d = summary.dhan;
    assert_eq!(summary.trading_date_ist, "2024-10-04");
    assert_eq!(d.ticks, 1234);
    assert_eq!(d.instruments, 50);
    assert_eq!(d.streaming_minutes, 3);
    assert_eq!(d.reconnects, 1, "one market-data reconnect");
    // 10:00 (kept existing verdict) + 11:00 on main_feed; the order-update
    // drop is not a headline disconnect.
    assert_eq!(d.disconnects_market, 2);
    assert_eq!(d.disconnects_off_hours, 1);
    // This boot's row (in memory) + an earlier boot's read-back row; the
    // duplicate read-back of this boot's row is not double-counted.
    assert_eq!(d.restarts, 2);
    // Lag: this process measured nothing, so the existing row's
    // distribution is kept.
    assert_eq!(
        (d.lag_p50_ms, d.lag_p99_ms, d.lag_max_ms, d.lag_samples),
        (10, 20, 30, 100)
    );
    // Coverage: the existing registry-derived columns outrank SQL.
    assert_eq!(d.mapped_instruments, 5);
    assert_eq!(d.unmapped_instruments, 1);
    assert_eq!(d.covered_instrument_minutes, 300);
    assert_eq!(d.coverage_source, CoverageSource::InMemory);
    // The existing 'degraded' verdict is never erased by a rerun.
    assert!(summary.degraded);
    assert!(summary.early_run);
    assert!(summary.restart_partial);
    assert!(!summary.partial_coverage, "every source answered");
    assert!(!summary.dhan_feed_off);

    let writes = server.writes().join("\n");
    assert!(writes.contains("feed_scoreboard_daily"), "{writes}");
    assert!(writes.contains("outcome=degraded"), "{writes}");
    // The second feed measured no ticks and was already 'feed_off'.
    assert!(writes.contains("outcome=feed_off"), "{writes}");
    assert!(writes.contains("feed_episode_audit"), "{writes}");
    // The suppressed 10:00 row is not rewritten; the 11:00 one is.
    assert!(writes.contains("dhan_code=807i"), "{writes}");
    assert!(!writes.contains("dhan_code=805i"), "{writes}");
}

#[tokio::test]
async fn test_feed_scoreboard_run_audit_drops_mark_day_degraded_and_backfill_infers_feed_off() {
    setup_env();
    let mut r = rich_responses(3);
    r.daily = ok(EMPTY);
    let server = FakeQuestDb::start(r);
    let summary = run_feed_scoreboard(
        &server.questdb(),
        server.port,
        DAY,
        "2024-10-04".to_string(),
        false,
        false,
        true,
        0,
        &[],
        None,
    )
    .await
    .expect("measured");
    assert!(summary.degraded, "3 dropped audit rows degrade the day");
    assert!(!summary.early_run);
    // No existing row to keep: the lag stays unmeasured.
    assert_eq!(summary.dhan.lag_samples, -1);
    assert_eq!(summary.dhan.coverage_source, CoverageSource::SqlBackfill);
    let writes = server.writes().join("\n");
    assert!(writes.contains("outcome=degraded"), "{writes}");
    // With no runtime flags the silent second feed is inferred off.
    assert!(writes.contains("outcome=feed_off"), "{writes}");
}

#[tokio::test]
async fn test_feed_scoreboard_run_every_source_down_returns_err() {
    setup_env();
    let server = FakeQuestDb::start(Responses::all(status(500), 400));
    let err = run_feed_scoreboard(
        &server.questdb(),
        server.port,
        DAY,
        "2024-10-04".to_string(),
        false,
        false,
        false,
        0,
        &[],
        Some((true, false)),
    )
    .await
    .expect_err("nothing measured");
    assert!(err.contains("every data source was unreachable"), "{err}");
    // The daily rows were still attempted (and refused).
    assert!(!server.writes().is_empty());
}

#[tokio::test]
async fn test_feed_scoreboard_run_garbage_bodies_returns_err() {
    setup_env();
    let server = FakeQuestDb::start(Responses::all(ok("not json at all"), 204));
    let err = run_feed_scoreboard(
        &server.questdb(),
        server.port,
        DAY,
        "2024-10-04".to_string(),
        false,
        false,
        false,
        0,
        &[],
        None,
    )
    .await
    .expect_err("nothing parsable");
    assert!(err.contains("nothing measured"), "{err}");
    let asked: HashSet<String> = server
        .seen()
        .into_iter()
        .filter_map(|s| s.sql)
        .filter(|s| s.contains("from feed_episode_audit"))
        .collect();
    assert!(
        asked.iter().any(|s| s.contains("run_partial")),
        "partial evidence reads the existing rows first: {asked:?}"
    );
}

#[tokio::test]
async fn test_feed_scoreboard_run_ws_read_failed_falls_back_to_episode_readback() {
    setup_env();
    let mut r = Responses::all(ok(EMPTY), 204);
    r.ws = status(500);
    r.partiality = status(500);
    r.daily = status(500);
    r.episode_day = ok(&dataset(vec![
        serde_json::json!([
            "dhan",
            "disconnect",
            "broker",
            true,
            "main_feed",
            0,
            ts_micros(hms(10, 0))
        ]),
        serde_json::json!([
            "dhan",
            "disconnect",
            "broker",
            true,
            "order_update",
            0,
            ts_micros(hms(10, 0))
        ]),
    ]));
    r.dhan_ticks = ok("{\"dataset\":[[10]]}");
    r.dhan_instruments = ok("{\"dataset\":[[2]]}");
    let server = FakeQuestDb::start(r);
    let summary = run_feed_scoreboard(
        &server.questdb(),
        server.port,
        DAY,
        "2024-10-04".to_string(),
        false,
        false,
        false,
        0,
        &[],
        Some((true, true)),
    )
    .await
    .expect("the fallback read-back answered");
    assert!(summary.partial_coverage, "the ws read failed");
    assert_eq!(summary.dhan.ticks, 10);
    assert_eq!(summary.dhan.disconnects_market, 1, "order-update excluded");
    assert_eq!(
        summary.dhan.reconnects, -1,
        "reconnects are unmeasured when the ws read failed"
    );
    assert_eq!(summary.dhan.streaming_minutes, 0);
}

#[tokio::test]
async fn test_feed_scoreboard_run_unreadable_readbacks_mark_partial() {
    setup_env();
    let mut r = rich_responses(0);
    r.boot = ok("garbage");
    r.partiality = ok("garbage");
    r.daily = ok("garbage");
    r.dhan_minutes = status(500);
    r.write_status = 400;
    let server = FakeQuestDb::start(r);
    let summary = run_feed_scoreboard(
        &server.questdb(),
        server.port,
        DAY,
        "2024-10-04".to_string(),
        false,
        false,
        false,
        0,
        &[],
        Some((true, false)),
    )
    .await
    .expect("ticks and ws events answered");
    assert!(summary.partial_coverage);
    assert!(!summary.degraded, "no existing degraded row was readable");
    // Without the keep-better read the 10:00 row is classified afresh.
    assert_eq!(summary.dhan.disconnects_market, 2);
    assert_eq!(summary.dhan.streaming_minutes, -1);

    // Boot read-back answers 500: same partial outcome.
    let mut r = rich_responses(0);
    r.boot = status(500);
    let server = FakeQuestDb::start(r);
    let summary = run_feed_scoreboard(
        &server.questdb(),
        server.port,
        DAY,
        "2024-10-04".to_string(),
        false,
        false,
        false,
        0,
        &[],
        Some((true, false)),
    )
    .await
    .expect("measured");
    assert!(summary.partial_coverage);
    assert_eq!(summary.dhan.restarts, 0, "no boot rows were readable");
}

// ---------------------------------------------------------------------------
// day_already_scored_complete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_feed_scoreboard_day_already_scored_complete_all_shapes() {
    setup_env();
    let complete = FakeQuestDb::start(Responses::all(
        ok(&dataset(vec![
            serde_json::json!(["dhan", "complete"]),
            serde_json::json!(["truedata", "feed_off"]),
        ])),
        204,
    ));
    assert!(day_already_scored_complete(&complete.questdb(), DAY).await);

    let partial = FakeQuestDb::start(Responses::all(
        ok(&dataset(vec![
            serde_json::json!(["dhan", "partial"]),
            serde_json::json!(["truedata", "feed_off"]),
        ])),
        204,
    ));
    assert!(!day_already_scored_complete(&partial.questdb(), DAY).await);

    let garbage = FakeQuestDb::start(Responses::all(ok("nope"), 204));
    assert!(!day_already_scored_complete(&garbage.questdb(), DAY).await);

    let down = FakeQuestDb::start(Responses::all(status(500), 204));
    assert!(!day_already_scored_complete(&down.questdb(), DAY).await);

    let dead = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: dead_port(),
        pg_port: 1,
        ilp_port: 1,
    };
    assert!(!day_already_scored_complete(&dead, DAY).await);
}

// ---------------------------------------------------------------------------
// reconcile_process_death_episodes
// ---------------------------------------------------------------------------

fn reconcile_ws(connect_secs: i64) -> String {
    dataset(vec![
        ws_row(
            hms(8, 34),
            "dhan",
            "main_feed",
            0,
            "connected",
            "boot",
            -1,
            false,
        ),
        ws_row(
            connect_secs,
            "dhan",
            "main_feed",
            0,
            "connected",
            "boot",
            -1,
            false,
        ),
    ])
}

#[tokio::test]
async fn test_feed_scoreboard_reconcile_in_market_death_is_synthesized_and_flushed() {
    setup_env();
    let mut r = Responses::all(ok(EMPTY), 204);
    r.ws = ok(&reconcile_ws(hms(11, 5)));
    let server = FakeQuestDb::start(r);
    let rows =
        reconcile_process_death_episodes(&server.questdb(), DAY, ts_nanos(0), ts_nanos(hms(11, 0)))
            .await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.ts_ist_nanos, ts_nanos(hms(11, 5)));
    assert_eq!(row.trading_date_ist_nanos, ts_nanos(0));
    assert_eq!(row.feed, "dhan");
    assert_eq!(row.episode_kind, EPISODE_KIND_PROCESS_DEATH);
    assert_eq!(row.blame, BlameClass::Ours);
    assert_eq!(row.blame_reason, "process_restart");
    assert_eq!(row.detector, "boot_reconciled");
    assert_eq!(row.down_secs, 0);
    assert!(row.market_hours);
    assert!(!row.run_partial);
    assert!(row.evidence.contains("upper bound"), "{}", row.evidence);
    assert!(!row.evidence.contains("post-close"));
    let writes = server.writes();
    assert_eq!(writes.len(), 1);
    assert!(writes[0].contains("feed_episode_audit"), "{}", writes[0]);
}

#[tokio::test]
async fn test_feed_scoreboard_reconcile_post_close_with_hole_before_close_is_real_death() {
    setup_env();
    let mut r = Responses::all(ok(EMPTY), 204);
    r.ws = ok(&reconcile_ws(hms(15, 55)));
    r.dhan_minutes = ok(&dataset(vec![serde_json::json!([
        "2024-10-04T14:00:00.000000Z"
    ])]));
    let server = FakeQuestDb::start(r);
    let rows = reconcile_process_death_episodes(
        &server.questdb(),
        DAY,
        ts_nanos(0),
        ts_nanos(hms(15, 50)),
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].market_hours,
        "a 14:00 last minute is a hole before the close"
    );
    assert_eq!(rows[0].blame_reason, "process_restart");
    assert!(!rows[0].evidence.contains("post-close"));
}

#[tokio::test]
async fn test_feed_scoreboard_reconcile_post_close_minutes_unreadable_keeps_carve_out() {
    setup_env();
    for minutes in [status(500), ok("garbage")] {
        let mut r = Responses::all(ok(EMPTY), 204);
        r.ws = ok(&reconcile_ws(hms(15, 55)));
        r.dhan_minutes = minutes;
        let server = FakeQuestDb::start(r);
        let rows = reconcile_process_death_episodes(
            &server.questdb(),
            DAY,
            ts_nanos(0),
            ts_nanos(hms(15, 50)),
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].market_hours);
        assert_eq!(rows[0].blame_reason, "post_close_restart");
        assert!(
            rows[0].evidence.contains("post-close"),
            "{}",
            rows[0].evidence
        );
    }

    // A feed that streamed through the close keeps the carve-out too.
    let mut r = Responses::all(ok(EMPTY), 204);
    r.ws = ok(&reconcile_ws(hms(15, 55)));
    r.dhan_minutes = ok(&dataset(vec![serde_json::json!([
        "2024-10-04T15:39:00.000000Z"
    ])]));
    let server = FakeQuestDb::start(r);
    let rows = reconcile_process_death_episodes(
        &server.questdb(),
        DAY,
        ts_nanos(0),
        ts_nanos(hms(15, 50)),
    )
    .await;
    assert_eq!(rows[0].blame_reason, "post_close_restart");
}

#[tokio::test]
async fn test_feed_scoreboard_reconcile_clean_boot_returns_no_rows() {
    setup_env();
    // Pre-boot: connected then disconnected (down when the process died).
    let mut r = Responses::all(ok(EMPTY), 204);
    r.ws = ok(&dataset(vec![
        ws_row(
            hms(8, 34),
            "dhan",
            "main_feed",
            0,
            "connected",
            "",
            -1,
            false,
        ),
        ws_row(
            hms(9, 0),
            "dhan",
            "main_feed",
            0,
            "disconnected",
            "",
            -1,
            true,
        ),
        ws_row(
            hms(11, 5),
            "dhan",
            "main_feed",
            0,
            "connected",
            "",
            -1,
            false,
        ),
    ]));
    let server = FakeQuestDb::start(r);
    let rows =
        reconcile_process_death_episodes(&server.questdb(), DAY, ts_nanos(0), ts_nanos(hms(11, 0)))
            .await;
    assert!(rows.is_empty());
    assert!(server.writes().is_empty(), "nothing to persist");
}

#[tokio::test(start_paused = true)]
async fn test_feed_scoreboard_reconcile_every_poll_fails_returns_no_rows() {
    setup_env();
    let dead = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: dead_port(),
        pg_port: 1,
        ilp_port: 1,
    };
    let rows =
        reconcile_process_death_episodes(&dead, DAY, ts_nanos(0), ts_nanos(hms(11, 0))).await;
    assert!(rows.is_empty());
}

/// Every flush is refused: the rows are retried three times and still
/// returned (the deaths happened), never reported as persisted. The 60 s
/// gaps between retries run on a clock paused once the first refusal lands.
#[tokio::test]
async fn test_feed_scoreboard_reconcile_flush_refused_retries_and_still_returns_rows() {
    setup_env();
    let mut r = Responses::all(ok(EMPTY), 400);
    r.ws = ok(&reconcile_ws(hms(11, 5)));
    let first_write = Arc::new(tokio::sync::Notify::new());
    let server = FakeQuestDb::start_with_hook(r, Some(Arc::clone(&first_write)));
    let pauser = tokio::spawn(async move {
        first_write.notified().await;
        tokio::time::pause();
    });
    let rows =
        reconcile_process_death_episodes(&server.questdb(), DAY, ts_nanos(0), ts_nanos(hms(11, 0)))
            .await;
    pauser.await.expect("pauser task");
    assert_eq!(rows.len(), 1, "the death is returned even unpersisted");
    assert_eq!(server.writes().len(), 3, "three flush attempts");
}

// ---------------------------------------------------------------------------
// Pure helpers: rarely taken branches
// ---------------------------------------------------------------------------

fn numbers(source: CoverageSource, covered: i64) -> FeedDayNumbers {
    FeedDayNumbers {
        ticks: 1,
        instruments: 1,
        streaming_minutes: 1,
        unique_win_minutes: -1,
        both_minutes: -1,
        lag_p50_ms: -1,
        lag_p99_ms: -1,
        lag_max_ms: -1,
        lag_samples: -1,
        disconnects_market: 0,
        disconnects_off_hours: 0,
        reconnects: 0,
        stalls: 0,
        blame_broker: 0,
        blame_ours: 0,
        blame_indeterminate: 0,
        restarts: 0,
        mapped_instruments: 9,
        unmapped_instruments: 9,
        covered_instrument_minutes: covered,
        coverage_source: source,
    }
}

#[test]
fn test_feed_scoreboard_fold_existing_coverage_better_new_source_wins() {
    let existing = ExistingDailyCoverage {
        unique_win_minutes: 1,
        both_minutes: 1,
        mapped_instruments: 1,
        unmapped_instruments: 1,
        covered_instrument_minutes: 1,
        coverage_source: "sql_backfill".to_string(),
    };
    let mut n = numbers(CoverageSource::InMemory, 5);
    assert!(!fold_existing_coverage_keep_better(&mut n, Some(&existing)));
    assert_eq!(
        n.mapped_instruments, 9,
        "the better source keeps its values"
    );
    // Equal rank, this run measured zero, the existing row measured some:
    // the existing 'mixed' row folds forward.
    let mixed = ExistingDailyCoverage {
        coverage_source: "mixed".to_string(),
        covered_instrument_minutes: 40,
        ..existing
    };
    let mut z = numbers(CoverageSource::Mixed, 0);
    assert!(fold_existing_coverage_keep_better(&mut z, Some(&mixed)));
    assert_eq!(z.covered_instrument_minutes, 40);
    assert_eq!(z.coverage_source, CoverageSource::Mixed);
}

#[test]
fn test_feed_scoreboard_parsers_skip_malformed_rows() {
    // Daily lag/coverage: a non-array row and a row whose feed is not a
    // string are skipped; a short row reads sentinels.
    let body = dataset(vec![
        serde_json::json!(5),
        serde_json::json!([7, "x"]),
        serde_json::json!(["dhan", "complete"]),
    ]);
    let lag = parse_existing_daily_lag(&body).expect("parsable");
    assert_eq!(lag.len(), 1);
    assert_eq!(lag["dhan"].samples, -1);
    let cov = parse_existing_daily_coverage(&body).expect("parsable");
    assert_eq!(cov.len(), 1);
    assert_eq!(cov["dhan"].coverage_source, "");
    assert!(parse_existing_daily_lag("nope").is_none());
    assert!(parse_existing_daily_coverage("{\"dataset\":7}").is_none());

    // ws events: a row with a non-integer ts is skipped.
    let ws = dataset(vec![
        serde_json::json!(["x", "dhan", "main_feed", 0, "connected", "", 0, 0, false]),
        serde_json::json!([1, "dhan", "main_feed", 0, "connected", "", 0, 0, false]),
    ]);
    let rows = parse_ws_events(&ws).expect("parsable");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ts_ist_nanos, 1_000);

    // Episode read-back: a short row is skipped, a non-market-data row is
    // not folded.
    let mut tallies = BTreeMap::new();
    let back = dataset(vec![
        serde_json::json!(["dhan", "disconnect"]),
        serde_json::json!(["dhan", "disconnect", "broker", true, "order_update"]),
        serde_json::json!(["dhan", "disconnect", "broker", true, "main_feed"]),
    ]);
    assert_eq!(
        fold_episode_readback_rows(&mut tallies, &back, &HashSet::new()),
        Some(1)
    );
    assert_eq!(tallies["dhan"].disconnects_market, 1);

    // Partiality: a row with a non-integer connection index is skipped.
    let part = dataset(vec![
        serde_json::json!([
            "dhan",
            "main_feed",
            "x",
            "disconnect",
            1,
            false,
            "ours",
            true
        ]),
        serde_json::json!(["dhan", "main_feed", 0, "disconnect", 1, "?", "ours", true]),
    ]);
    let map: HashMap<_, _> = parse_existing_episode_partiality(&part).expect("parsable");
    assert_eq!(map.len(), 1);
    let only = map.values().next().expect("one row");
    assert!(only.run_partial, "a non-bool run_partial reads as partial");
}

#[test]
fn test_feed_scoreboard_parse_prom_counter_sum_skips_bad_lines() {
    let body = "\
# HELP x\n\
lonely_token\n\
tv_x_total notanumber\n\
tv_x_total -3\n\
tv_x_total NaN\n\
tv_x_total{a=\"b\"} 4\n\
tv_x_total_other 9\n\
tv_x_total 2.0\n";
    assert_eq!(parse_prom_counter_sum(body, "tv_x_total"), Some(6));
    assert_eq!(parse_prom_counter_sum("lonely\n", "tv_x_total"), None);
    assert_eq!(
        parse_prom_counter_sum("tv_x_total -1\n", "tv_x_total"),
        None
    );
}

#[test]
fn test_feed_scoreboard_collect_correlation_evidence_every_code_arm() {
    let ts = ts_nanos(hms(10, 0));
    let ev = |code: &str, reason: Option<&str>, stage: Option<&str>| JsonlCodeEvent {
        code: code.to_string(),
        reason: reason.map(str::to_string),
        stage: stage.map(str::to_string),
        ts_ist_nanos: ts,
    };
    let events = vec![
        ev("WS-GAP-09", Some("bare_dhan_reset"), None),
        ev("WS-GAP-09", Some("in_window_429_ride_out"), None),
        ev("WS-GAP-09", Some("something_else"), None),
        ev("WS-GAP-09", None, None),
        ev("PROC-01", None, None),
        ev("RESOURCE-02", None, None),
        ev("RESILIENCE-01", None, Some("ssm_transport")),
        ev("UNRELATED-01", None, None),
        JsonlCodeEvent {
            ts_ist_nanos: ts_nanos(0) - 1,
            ..ev("RESILIENCE-03", None, None)
        },
    ];
    let c = collect_correlation_evidence(&events, DAY, true);
    assert_eq!(c.ws_gap9_bare_ts_nanos, vec![ts]);
    assert_eq!(c.ws_gap9_429_ts_nanos, vec![ts]);
    assert_eq!(c.resource_ts_nanos, vec![ts, ts]);
    assert!(
        !c.resilience_same_day,
        "an SSM-reachability stage and an out-of-day line are not peer evidence"
    );
    assert!(c.scan_complete);
}

#[test]
fn test_feed_scoreboard_scan_errors_jsonl_skips_foreign_bare_blank_and_unopenable_files() {
    use std::os::unix::ffi::OsStrExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "tv-cov-scoreboard-jsonl-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    // 10:00 IST on DAY = 04:30 UTC.
    let line = serde_json::json!({
        "code": "PROC-01",
        "timestamp": "2024-10-04T04:30:00Z",
    })
    .to_string();
    std::fs::write(
        dir.join("errors.jsonl.2024-10-04-04"),
        format!("\n   \n{line}\nnot json\n"),
    )
    .expect("hourly file");
    // The bare compat name is skipped (would double-count).
    std::fs::write(dir.join("errors.jsonl"), format!("{line}\n")).expect("bare file");
    // A foreign file is ignored.
    std::fs::write(dir.join("app.log"), format!("{line}\n")).expect("foreign file");
    // A file name that is not UTF-8 is ignored.
    std::fs::write(
        dir.join(std::ffi::OsStr::from_bytes(b"errors\xff.jsonl")),
        "x",
    )
    .expect("non-utf8 name");

    let c = scan_errors_jsonl_for_correlation(&dir, DAY);
    assert!(c.scan_complete);
    assert_eq!(c.resource_ts_nanos, vec![ts_nanos(hms(10, 0))]);

    // A dangling symlink with an hourly name cannot be opened: incomplete.
    std::os::unix::fs::symlink(
        dir.join("missing-target"),
        dir.join("errors.jsonl.dangling"),
    )
    .expect("symlink");
    let c = scan_errors_jsonl_for_correlation(&dir, DAY);
    assert!(
        !c.scan_complete,
        "an unopenable file makes the scan partial"
    );
    assert_eq!(c.resource_ts_nanos.len(), 1);

    std::fs::remove_dir_all(&dir).expect("cleanup");
    let gone = scan_errors_jsonl_for_correlation(&dir, DAY);
    assert!(!gone.scan_complete, "a missing folder is never complete");
}

#[test]
fn test_feed_scoreboard_synthesize_out_of_order_rows_pick_last_pre_and_earliest_post() {
    let boot = ts_nanos(hms(11, 0));
    let row = |secs: i64, kind: &str| WsAuditEventLite {
        ts_ist_nanos: ts_nanos(secs),
        feed: "dhan".to_string(),
        ws_type: "main_feed".to_string(),
        connection_index: 0,
        event_kind: kind.to_string(),
        source: String::new(),
        dhan_code: -1,
        down_secs: 0,
        market_hours: true,
    };
    // Rows arrive out of order: the LAST pre-boot row (10:00 connected) wins
    // over the earlier 09:30 disconnect read after it, and the EARLIEST
    // post-boot up row (11:05) wins over 11:20.
    let rows = vec![
        row(hms(10, 0), "connected"),
        row(hms(9, 30), "disconnected"),
        row(hms(11, 20), "reconnected"),
        row(hms(11, 5), "connected"),
        row(hms(11, 10), "connected"),
    ];
    let deaths = synthesize_process_death_episodes(&rows, boot);
    assert_eq!(deaths.len(), 1);
    assert_eq!(deaths[0].ts_ist_nanos, ts_nanos(hms(11, 5)));
    assert_eq!(deaths[0].gap_upper_bound_secs, hms(11, 5) - hms(10, 0));
    assert!(deaths[0].market_hours);
    assert!(!deaths[0].post_close_restart);
}
