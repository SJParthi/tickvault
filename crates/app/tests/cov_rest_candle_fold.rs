//! Coverage tests for `rest_candle_fold`: the fold task
//! (`run_rest_candle_fold` → boot catch-up, live batch loop, debounced
//! past-day refold queue) against a canned QuestDB `/exec` on 127.0.0.1,
//! the process-global handoff (`send_confirmed_bars`), and the pure
//! engine / slot-table edge arms.
//!
//! This binary installs NO global seal sender, so every seal emission takes
//! the `no_seal_sender` arm; the RAM spot-bar store IS installed, so the
//! fold output is asserted there. A Prometheus recorder is installed so the
//! loss counters can be asserted, and a sink tracing subscriber so the
//! coded log arms are evaluated.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::NaiveDate;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde_json::{Value, json};
use tickvault_app::rest_candle_fold::{
    ConfirmedBar, DeferredBarAction, FOLD_MAX_RESPONSE_BYTES, FOLD_MAX_SLOTS,
    FOLD_REFOLD_MAX_ATTEMPTS, FoldOutcome, FoldSlots, LiveBarAction, RestCandleFoldParams,
    SharedFoldReceiverSlot, SidDayFold, SidFoldState, apply_bar_batch, day_start_nanos,
    note_unfoldable_identity, run_rest_candle_fold, send_confirmed_bars,
    set_global_fold_bar_sender, spawn_supervised_rest_candle_fold, today_ist,
};
use tickvault_common::config::{QuestDbConfig, RestCandleFoldConfig};
use tickvault_common::feed::Feed;
use tickvault_trading::candles::TfIndex;
use tickvault_trading::in_mem::spot_bar_store::{SlotKey, install_spot_bar_store, spot_bar_store};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Process-global harness: recorder, tracing sink, RAM store
// ---------------------------------------------------------------------------

fn harness() -> &'static PrometheusHandle {
    static H: OnceLock<PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::TRACE)
            .try_init();
        install_spot_bar_store(3);
        PrometheusBuilder::new()
            .install_recorder()
            .expect("install the prometheus recorder")
    })
}

/// Serialises the tests that move `tv_rest_candle_fold_dropped_total
/// {reason="bad_identity"}`. That series has no label but `reason`, so two
/// tests running in parallel would each see the other's increments; holding
/// this lock lets each assert an exact delta.
fn bad_identity_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Sum of every series of `name` whose label set contains `label`.
fn counter(name: &str, label: &str) -> f64 {
    harness()
        .render()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter(|l| {
            l.starts_with(name)
                && l[name.len()..].starts_with(['{', ' '])
                && (label.is_empty() || l.contains(label))
        })
        .filter_map(|l| l.rsplit(' ').next().and_then(|v| v.parse::<f64>().ok()))
        .sum()
}

// ---------------------------------------------------------------------------
// Canned HTTP server
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Resp {
    status: u16,
    body: Vec<u8>,
    declared_len: Option<usize>,
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
    fn raw(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            body: body.to_vec(),
            declared_len: None,
            omit_len: false,
        }
    }
}

type Handler = Arc<dyn Fn(&str) -> Resp + Send + Sync>;

struct Mock {
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    fn requests(&self) -> Vec<String> {
        self.log.lock().expect("log lock").clone()
    }
    fn count(&self, needle: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.contains(needle))
            .count()
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn serve_one(mut s: TcpStream, handler: Handler, log: Arc<Mutex<Vec<String>>>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = vec![0u8; 16 * 1024];
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
    let target = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let url = reqwest::Url::parse(&format!("http://mock{target}")).expect("request target");
    let query = url
        .query_pairs()
        .find(|(k, _)| k == "query")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    log.lock().expect("log lock").push(query.clone());
    let resp = handler(&query);
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

fn qcfg(port: u16) -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: port,
        pg_port: 1,
        ilp_port: 1,
    }
}

/// Routes: discovery per feed, bars per (feed, sid, day start nanos).
#[derive(Clone, Default)]
struct Fixture {
    discovery: HashMap<&'static str, Resp>,
    bars: HashMap<(&'static str, i64, i64), Resp>,
}

fn between<'a>(q: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = q.find(start)? + start.len();
    let rest = &q[i..];
    Some(&rest[..rest.find(end)?])
}

fn feed_of(q: &str) -> &'static str {
    if q.contains("feed = 'truedata'") {
        "truedata"
    } else {
        "dhan"
    }
}

fn handler_for(fx: Fixture) -> Handler {
    Arc::new(move |q: &str| {
        let feed = feed_of(q);
        if q.starts_with("SELECT DISTINCT security_id, exchange_segment") {
            return fx
                .discovery
                .get(feed)
                .cloned()
                .unwrap_or_else(|| Resp::json(&json!({"dataset": []})));
        }
        let sid = between(q, "security_id = ", " ").and_then(|s| s.parse::<i64>().ok());
        let start_micros = between(q, "ts >= ", " ").and_then(|s| s.parse::<i64>().ok());
        if let (Some(sid), Some(start)) = (sid, start_micros) {
            return fx
                .bars
                .get(&(feed, sid, start * 1_000))
                .cloned()
                .unwrap_or_else(|| Resp::json(&json!({"dataset": []})));
        }
        Resp::raw(404, b"{}")
    })
}

// ---------------------------------------------------------------------------
// Bar builders
// ---------------------------------------------------------------------------

const IDX_I: u8 = 0;
const SESSION_OPEN_SOD: i64 = 33_300;

fn minute_nanos(date: NaiveDate, minute: i64) -> i64 {
    day_start_nanos(date) + (SESSION_OPEN_SOD + minute * 60) * 1_000_000_000
}

fn bar(feed: Feed, sid: u64, date: NaiveDate, minute: i64, close: f64) -> ConfirmedBar {
    ConfirmedBar {
        feed,
        security_id: sid,
        exchange_segment_code: IDX_I,
        minute_ts_ist_nanos: minute_nanos(date, minute),
        open: close - 1.0,
        high: close + 2.0,
        low: close - 2.0,
        close,
        volume: 0,
    }
}

fn bars_json(date: NaiveDate, minutes: &[i64], close: f64) -> Resp {
    let ds: Vec<Value> = minutes
        .iter()
        .map(|m| {
            json!([
                minute_nanos(date, *m),
                close - 1.0,
                close + 2.0,
                close - 2.0,
                close,
                0
            ])
        })
        .collect();
    Resp::json(&json!({ "dataset": ds }))
}

fn discovery(pairs: &[(i64, &str)]) -> Resp {
    let ds: Vec<Value> = pairs.iter().map(|(s, g)| json!([s, g])).collect();
    Resp::json(&json!({ "dataset": ds }))
}

fn days_ago(n: u64) -> NaiveDate {
    today_ist()
        .checked_sub_days(chrono::Days::new(n))
        .expect("date")
}

fn key(feed: Feed, sid: u64) -> SlotKey {
    SlotKey {
        feed,
        security_id: sid,
        exchange_segment_code: IDX_I,
    }
}

fn bucket_secs(date: NaiveDate, minute: i64) -> u32 {
    u32::try_from(minute_nanos(date, minute) / 1_000_000_000).expect("fits u32")
}

fn params(port: u16, catchup_days: u32, slot: &SharedFoldReceiverSlot) -> RestCandleFoldParams {
    RestCandleFoldParams {
        config: RestCandleFoldConfig {
            enabled: true,
            catchup_days,
        },
        questdb: qcfg(port),
        receiver_slot: Arc::clone(slot),
    }
}

fn slot_with(rx: mpsc::Receiver<ConfirmedBar>) -> SharedFoldReceiverSlot {
    Arc::new(Mutex::new(Some(rx)))
}

// ---------------------------------------------------------------------------
// The fold task
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rest_candle_fold_run_without_receiver_returns_immediately() {
    harness();
    let slot: SharedFoldReceiverSlot = Arc::new(Mutex::new(None));
    let before = counter("tv_rest_candle_fold_errors_total", "receiver_lost");
    // Port 1: never contacted — the task returns before building a client.
    tokio::time::timeout(
        Duration::from_secs(5),
        run_rest_candle_fold(params(1, 1, &slot)),
    )
    .await
    .expect("returns at once with an empty slot");
    assert!(slot.lock().expect("slot").is_none());
    assert!(counter("tv_rest_candle_fold_errors_total", "receiver_lost") >= before + 1.0);
}

#[tokio::test(start_paused = true)]
async fn test_rest_candle_fold_run_catchup_live_batch_and_past_day_refold() {
    harness();
    let today = today_ist();
    let y = days_ago(1);
    let y2 = days_ago(2);
    let mut fx = Fixture::default();
    fx.discovery
        .insert("dhan", discovery(&[(101, "IDX_I"), (102, "IDX_I")]));
    fx.discovery.insert("truedata", Resp::raw(500, b"down"));
    fx.bars.insert(
        ("dhan", 101, day_start_nanos(today)),
        bars_json(today, &[0, 1, 2, 3, 4], 200.0),
    );
    fx.bars.insert(
        ("dhan", 101, day_start_nanos(y)),
        bars_json(y, &[0, 1, 2], 150.0),
    );
    // A row with too few columns: the day is skipped (catchup_parse).
    fx.bars.insert(
        ("dhan", 101, day_start_nanos(y2)),
        Resp::json(&json!({"dataset": [[1]]})),
    );
    let mock = spawn_mock(handler_for(fx)).await;

    let before_future = counter("tv_rest_candle_fold_dropped_total", "future_dated");
    let before_oos = counter("tv_rest_candle_fold_dropped_total", "out_of_session");
    let before_dup = counter("tv_rest_candle_fold_duplicate_bars_total", "");
    let before_queued = counter("tv_rest_candle_fold_refold_queued_total", "");
    let before_refolds = counter("tv_rest_candle_fold_day_refolds_total", "feed=\"dhan\"");
    let before_no_sender = counter("tv_rest_candle_fold_dropped_total", "no_seal_sender");
    let before_parse = counter("tv_rest_candle_fold_errors_total", "catchup_parse");

    let (tx, rx) = mpsc::channel(64);
    let slot = slot_with(rx);
    let task = tokio::spawn(run_rest_candle_fold(params(mock.port, 3, &slot)));

    // Wait until the boot catch-up has read all three days.
    for _ in 0..200 {
        if mock.count("security_id = 101 ") >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let in_order = bar(Feed::Dhan, 101, today, 5, 201.0);
    let batch = [
        in_order,
        // A value repair of an already-folded minute (refold from RAM).
        bar(Feed::Dhan, 101, today, 2, 999.0),
        in_order,
        // 08:00 IST: out of session.
        ConfirmedBar {
            minute_ts_ist_nanos: day_start_nanos(today) + 8 * 3600 * 1_000_000_000,
            ..in_order
        },
        bar(Feed::Dhan, 101, today.succ_opt().expect("tomorrow"), 0, 1.0),
        // A repair of yesterday: queued for the debounced /exec refold.
        bar(Feed::Dhan, 101, y, 1, 151.0),
        // A brand-new identity seen only live.
        bar(Feed::Dhan, 103, today, 0, 50.0),
    ];
    for b in batch {
        tx.send(b).await.expect("send");
    }
    // Let the debounce (5 s) fire and the refold read yesterday again.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let y_reads = mock
            .requests()
            .iter()
            .filter(|r| {
                r.contains("security_id = 101 ")
                    && r.contains(&format!("ts >= {} ", day_start_nanos(y) / 1_000))
            })
            .count();
        if y_reads >= 2 {
            break;
        }
    }
    drop(tx);
    tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .expect("task exits once the channel closes")
        .expect("task did not panic");

    let store = spot_bar_store().expect("store installed");
    // Catch-up: today's sealed minutes and yesterday's force-sealed day.
    assert!(
        store
            .bar_at(key(Feed::Dhan, 101), TfIndex::M1, bucket_secs(today, 0))
            .is_some()
    );
    let y_m1 = store
        .bar_at(key(Feed::Dhan, 101), TfIndex::M1, bucket_secs(y, 0))
        .expect("yesterday folded at catch-up");
    assert_eq!(y_m1.close, 150.0);
    assert!(
        store
            .bar_at(key(Feed::Dhan, 101), TfIndex::M3, bucket_secs(y, 0))
            .is_some(),
        "a past day force-seals its open buckets"
    );
    // The live repair refolded today from RAM.
    let repaired = store
        .bar_at(key(Feed::Dhan, 101), TfIndex::M1, bucket_secs(today, 2))
        .expect("repaired minute resident");
    assert_eq!(repaired.close, 999.0);
    assert_eq!(
        store
            .bar_at(key(Feed::Dhan, 101), TfIndex::M1, bucket_secs(today, 5))
            .expect("live minute")
            .close,
        201.0
    );
    assert!(
        store
            .bar_at(key(Feed::Dhan, 103), TfIndex::M1, bucket_secs(today, 0))
            .is_some(),
        "a live-only identity gets its own slot"
    );
    assert!(
        store
            .bar_at(key(Feed::Dhan, 101), TfIndex::M1, bucket_secs(y2, 0))
            .is_none(),
        "an unparseable day is never folded"
    );
    let reqs = mock.requests();
    assert!(reqs.iter().any(|r| r.contains("feed = 'truedata'")));
    assert!(counter("tv_rest_candle_fold_dropped_total", "future_dated") >= before_future + 1.0);
    assert!(counter("tv_rest_candle_fold_dropped_total", "out_of_session") >= before_oos + 1.0);
    assert!(counter("tv_rest_candle_fold_duplicate_bars_total", "") >= before_dup + 1.0);
    assert!(counter("tv_rest_candle_fold_refold_queued_total", "") >= before_queued + 1.0);
    assert!(
        counter("tv_rest_candle_fold_day_refolds_total", "feed=\"dhan\"") >= before_refolds + 1.0
    );
    assert!(counter("tv_rest_candle_fold_dropped_total", "no_seal_sender") > before_no_sender);
    assert!(counter("tv_rest_candle_fold_errors_total", "catchup_parse") > before_parse);
}

#[tokio::test(start_paused = true)]
async fn test_rest_candle_fold_run_past_day_refold_retries_until_exhausted() {
    harness();
    let today = today_ist();
    let y = days_ago(1);
    let mut fx = Fixture::default();
    fx.discovery
        .insert("dhan", discovery(&[(201, "IDX_I"), (202, "IDX_I")]));
    // 201's yesterday never shows the triggering minute (stale read);
    // 202's yesterday read fails outright.
    fx.bars
        .insert(("dhan", 201, day_start_nanos(y)), bars_json(y, &[0], 10.0));
    fx.bars
        .insert(("dhan", 202, day_start_nanos(y)), Resp::raw(500, b"x"));
    let mock = spawn_mock(handler_for(fx)).await;
    let before_stale = counter("tv_rest_candle_fold_errors_total", "refold_stale_read");

    let (tx, rx) = mpsc::channel(8);
    let slot = slot_with(rx);
    let task = tokio::spawn(run_rest_candle_fold(params(mock.port, 1, &slot)));
    for _ in 0..200 {
        if mock.count("security_id = 202 ") >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tx.send(bar(Feed::Dhan, 201, y, 30, 11.0))
        .await
        .expect("send");
    tx.send(bar(Feed::Dhan, 202, y, 30, 11.0))
        .await
        .expect("send");
    let y_filter = format!("ts >= {} ", day_start_nanos(y) / 1_000);
    let reads = |sid: u64| {
        mock.requests()
            .iter()
            .filter(|r| r.contains(&format!("security_id = {sid} ")) && r.contains(&y_filter))
            .count()
    };
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if reads(201) >= FOLD_REFOLD_MAX_ATTEMPTS as usize
            && reads(202) >= FOLD_REFOLD_MAX_ATTEMPTS as usize
        {
            break;
        }
    }
    // A further window must not read again: the retries are exhausted.
    tokio::time::sleep(Duration::from_secs(30)).await;
    drop(tx);
    tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .expect("task exits")
        .expect("no panic");
    assert_eq!(reads(201), FOLD_REFOLD_MAX_ATTEMPTS as usize);
    assert_eq!(reads(202), FOLD_REFOLD_MAX_ATTEMPTS as usize);
    assert!(
        counter("tv_rest_candle_fold_errors_total", "refold_stale_read")
            >= before_stale + f64::from(FOLD_REFOLD_MAX_ATTEMPTS)
    );
    // The stale read never re-emitted yesterday's repaired minute.
    let store = spot_bar_store().expect("store");
    assert!(
        store
            .bar_at(key(Feed::Dhan, 201), TfIndex::M1, bucket_secs(y, 30))
            .is_none()
    );
    let _ = today;
}

#[tokio::test(start_paused = true)]
async fn test_rest_candle_fold_run_discovery_and_body_failures_fold_nothing() {
    harness();
    let today = today_ist();
    let mut fx = Fixture::default();
    // Dhan: 65 rows > the 64-row cap — the whole feed's catch-up is skipped.
    let truncated: Vec<(i64, &str)> = (0..65).map(|i| (400 + i, "IDX_I")).collect();
    fx.discovery.insert("dhan", discovery(&truncated));
    // Truedata: an unrepresentable id, a poisoned segment, one real id.
    fx.discovery.insert(
        "truedata",
        discovery(&[(-5, "IDX_I"), (301, "EVIL'"), (302, "IDX_I")]),
    );
    let mut declared = Resp::raw(200, b"");
    declared.declared_len = Some(FOLD_MAX_RESPONSE_BYTES + 1);
    fx.bars
        .insert(("truedata", 302, day_start_nanos(today)), declared);
    let mut streamed = Resp::raw(200, b"");
    streamed.body = vec![b' '; FOLD_MAX_RESPONSE_BYTES + 1];
    streamed.omit_len = true;
    fx.bars
        .insert(("truedata", 302, day_start_nanos(days_ago(1))), streamed);
    fx.bars.insert(
        ("truedata", 302, day_start_nanos(days_ago(2))),
        Resp::raw(200, &[0xff, 0xfe, 0xfd]),
    );
    let too_many: Vec<i64> = (0..=500).collect();
    let mut rows: Vec<Value> = Vec::new();
    for m in &too_many {
        rows.push(json!([
            minute_nanos(days_ago(3), m % 300),
            1.0,
            1.0,
            1.0,
            1.0,
            0
        ]));
    }
    fx.bars.insert(
        ("truedata", 302, day_start_nanos(days_ago(3))),
        Resp::json(&json!({ "dataset": rows })),
    );
    let mock = spawn_mock(handler_for(fx)).await;
    let _bad_identity_guard = bad_identity_lock().lock().await;
    let before_trunc = counter("tv_rest_candle_fold_errors_total", "discovery_truncated");
    let before_bad = counter(
        "tv_rest_candle_fold_dropped_total",
        "reason=\"bad_identity\"",
    );
    let before_query = counter("tv_rest_candle_fold_errors_total", "catchup_query");

    let (tx, rx) = mpsc::channel(4);
    let slot = slot_with(rx);
    drop(tx);
    tokio::time::timeout(
        Duration::from_secs(60),
        run_rest_candle_fold(params(mock.port, 4, &slot)),
    )
    .await
    .expect("returns once the channel is closed");

    let reqs = mock.requests();
    assert!(
        !reqs.iter().any(|r| r.contains("security_id = 400 ")),
        "a truncated discovery never folds a subset"
    );
    assert!(!reqs.iter().any(|r| r.contains("EVIL")));
    assert!(!reqs.iter().any(|r| r.contains("security_id = -5")));
    assert_eq!(
        mock.count("security_id = 302 "),
        4,
        "four catch-up days read"
    );
    let store = spot_bar_store().expect("store");
    for n in 0..4 {
        assert!(
            store
                .bar_at(
                    key(Feed::Truedata, 302),
                    TfIndex::M1,
                    bucket_secs(days_ago(n), 0)
                )
                .is_none(),
            "a failed day read folds nothing (day -{n})"
        );
    }
    assert!(counter("tv_rest_candle_fold_errors_total", "discovery_truncated") > before_trunc);
    // Exactly one unrepresentable id (-5) in the truedata discovery; the
    // poisoned segment is skipped without counting and the truncated dhan
    // discovery is skipped before its rows are read.
    assert_eq!(
        counter(
            "tv_rest_candle_fold_dropped_total",
            "reason=\"bad_identity\""
        ),
        before_bad + 1.0
    );
    assert!(counter("tv_rest_candle_fold_errors_total", "catchup_query") >= before_query + 2.0);
    // The receiver is re-parked for a respawn.
    assert!(slot.lock().expect("slot").is_some());
}

#[tokio::test(start_paused = true)]
async fn test_rest_candle_fold_run_discovery_unparseable_and_oversize_skip_feeds() {
    harness();
    let mut fx = Fixture::default();
    fx.discovery
        .insert("dhan", Resp::raw(200, b"definitely not json"));
    let mut declared = Resp::raw(200, b"");
    declared.declared_len = Some(FOLD_MAX_RESPONSE_BYTES + 1);
    fx.discovery.insert("truedata", declared);
    let mock = spawn_mock(handler_for(fx)).await;
    let (tx, rx) = mpsc::channel(4);
    let slot = slot_with(rx);
    drop(tx);
    run_rest_candle_fold(params(mock.port, 2, &slot)).await;
    let reqs = mock.requests();
    assert_eq!(
        reqs.len(),
        2,
        "two discovery reads and nothing else: {reqs:?}"
    );
    assert!(reqs.iter().all(|r| r.starts_with("SELECT DISTINCT")));
}

#[tokio::test(start_paused = true)]
async fn test_rest_candle_fold_spawn_supervised_stops_on_clean_exit() {
    harness();
    let mock = spawn_mock(handler_for(Fixture::default())).await;
    let (tx, rx) = mpsc::channel::<ConfirmedBar>(4);
    drop(tx);
    let handle = spawn_supervised_rest_candle_fold(
        RestCandleFoldConfig {
            enabled: true,
            catchup_days: 1,
        },
        qcfg(mock.port),
        rx,
    );
    tokio::time::timeout(Duration::from_secs(60), handle)
        .await
        .expect("the supervisor stops after a clean exit")
        .expect("no panic");
    assert!(
        mock.count("SELECT DISTINCT") >= 2,
        "the incarnation ran its catch-up"
    );
}

// ---------------------------------------------------------------------------
// The global handoff
// ---------------------------------------------------------------------------

#[test]
fn test_rest_candle_fold_send_confirmed_bars_full_then_closed_counts_drops() {
    harness();
    let (tx, mut rx) = mpsc::channel::<ConfirmedBar>(1);
    assert!(set_global_fold_bar_sender(tx), "first install wins");
    let today = today_ist();
    let b = bar(Feed::Dhan, 900, today, 0, 1.0);
    let full_before = counter(
        "tv_rest_candle_fold_dropped_total",
        "reason=\"channel_full\"",
    );
    let closed_before = counter(
        "tv_rest_candle_fold_dropped_total",
        "reason=\"channel_closed\"",
    );

    send_confirmed_bars(&[]);
    assert!(rx.try_recv().is_err(), "an empty slice sends nothing");

    send_confirmed_bars(&[b, b, b]);
    assert_eq!(rx.try_recv().expect("one fits"), b);
    assert!(rx.try_recv().is_err());
    assert_eq!(
        counter(
            "tv_rest_candle_fold_dropped_total",
            "reason=\"channel_full\""
        ),
        full_before + 2.0
    );

    drop(rx);
    send_confirmed_bars(&[b, b]);
    assert_eq!(
        counter(
            "tv_rest_candle_fold_dropped_total",
            "reason=\"channel_closed\""
        ),
        closed_before + 2.0
    );
}

/// The src unit test calls this with no recorder installed; what is new here
/// is the counter: every call counts, past the one-shot warn latch.
#[test]
fn test_rest_candle_fold_note_unfoldable_identity_counts_every_occurrence() {
    harness();
    let _guard = bad_identity_lock().blocking_lock();
    let before = counter(
        "tv_rest_candle_fold_dropped_total",
        "reason=\"bad_identity\"",
    );
    note_unfoldable_identity(Feed::Dhan, -1);
    note_unfoldable_identity(Feed::Truedata, i64::MIN);
    assert_eq!(
        counter(
            "tv_rest_candle_fold_dropped_total",
            "reason=\"bad_identity\""
        ),
        before + 2.0
    );
}

// ---------------------------------------------------------------------------
// Engine / slot-table edge arms
// ---------------------------------------------------------------------------

#[test]
fn test_rest_candle_fold_apply_live_bar_deferred_unrepresentable_minutes_are_out_of_session() {
    harness();
    let today = today_ist();
    let mut slot = SidDayFold::new(Feed::Dhan, 1, IDX_I);
    let base = bar(Feed::Dhan, 1, today, 0, 10.0);
    let negative = ConfirmedBar {
        minute_ts_ist_nanos: -1_000_000_000,
        ..base
    };
    assert!(matches!(
        slot.apply_live_bar_deferred(&negative, today),
        DeferredBarAction::OutOfSession
    ));
    let pre_open = ConfirmedBar {
        minute_ts_ist_nanos: day_start_nanos(today) + 9 * 3600 * 1_000_000_000,
        ..base
    };
    assert!(matches!(
        slot.apply_live_bar_deferred(&pre_open, today),
        DeferredBarAction::OutOfSession
    ));
    assert!(matches!(
        slot.apply_live_bar(&pre_open, today),
        LiveBarAction::OutOfSession
    ));
    assert_eq!(slot.current_day(), None, "a refused bar adopts no day");
    assert_eq!(slot.day_map_len(), 0);
}

#[test]
fn test_rest_candle_fold_fold_bar_beyond_u32_seconds_is_out_of_session() {
    harness();
    let mut e = SidFoldState::new(Feed::Dhan, 2, IDX_I);
    let mut b = bar(Feed::Dhan, 2, today_ist(), 0, 10.0);
    b.minute_ts_ist_nanos = (i64::from(u32::MAX) + 1) * 1_000_000_000;
    assert!(matches!(e.fold_bar(&b), FoldOutcome::OutOfSession));
    assert_eq!(e.open_bucket_count(), 0);
}

/// The src unit test `test_volume_saturates_never_tears_the_fold` covers the
/// saturated bucket values; it runs with no recorder, so the counter is the
/// only new assertion here: one increment per open bucket whose sum
/// overflowed, derived from the timeframe grid rather than hard-coded.
#[test]
fn test_rest_candle_fold_volume_saturation_counts_each_overflowed_bucket() {
    harness();
    let before = counter("tv_rest_candle_fold_volume_saturated_total", "");
    let today = today_ist();
    let mut e = SidFoldState::new(Feed::Dhan, 3, IDX_I);
    let mut first = bar(Feed::Dhan, 3, today, 0, 10.0);
    first.volume = i64::MAX;
    let mut second = bar(Feed::Dhan, 3, today, 1, 10.0);
    second.volume = 1;
    assert!(matches!(e.fold_bar(&first), FoldOutcome::Folded(_)));
    assert!(matches!(e.fold_bar(&second), FoldOutcome::Folded(_)));
    let secs = |b: &ConfirmedBar| -> u32 {
        u32::try_from(b.minute_ts_ist_nanos / 1_000_000_000).expect("fits u32")
    };
    let shared = TfIndex::ALL
        .iter()
        .filter(|tf| {
            !tf.is_second_scale() && tf.bucket_start(secs(&first)) == tf.bucket_start(secs(&second))
        })
        .count();
    assert!(shared > 0, "minutes 0 and 1 share at least one bucket");
    assert_eq!(
        counter("tv_rest_candle_fold_volume_saturated_total", ""),
        before + shared as f64
    );
}

#[test]
fn test_rest_candle_fold_apply_bar_batch_classifies_every_refusal() {
    harness();
    let today = today_ist();
    let mut engines = FoldSlots::new();
    let first = bar(Feed::Dhan, 11, today, 0, 10.0);
    let out = apply_bar_batch(&mut engines, &[first], today);
    assert_eq!(out.emissions.len(), 1);
    let batch = [
        first,
        ConfirmedBar {
            minute_ts_ist_nanos: day_start_nanos(today),
            ..first
        },
        bar(Feed::Dhan, 11, today.succ_opt().expect("tomorrow"), 0, 1.0),
        bar(Feed::Dhan, 11, days_ago(1), 0, 1.0),
    ];
    let out = apply_bar_batch(&mut engines, &batch, today);
    assert_eq!(out.duplicates, 1);
    assert_eq!(out.out_of_session, 1);
    assert_eq!(out.future_dated, 1);
    assert_eq!(out.past_day.len(), 1);
    assert_eq!(out.past_day[0], batch[3]);
    assert_eq!(out.slot_exhausted, 0);
    assert!(out.emissions.is_empty());
}

/// The fail-closed behaviour itself is pinned by the src unit tests
/// `test_fold_slots_capacity_exhaustion_fails_closed` and
/// `test_apply_bar_batch_counts_slot_exhaustion_and_drops_the_bar`, which
/// run with no recorder. What is new here: each refusal, through each of the
/// three entry points, counts exactly once on BOTH counters, and a lookup or
/// an existing identity at capacity counts nothing.
#[test]
fn test_rest_candle_fold_fold_slots_capacity_refusals_count_exactly() {
    harness();
    let mut slots = FoldSlots::new();
    for sid in 0..FOLD_MAX_SLOTS as u64 {
        assert!(slots.slot_mut(Feed::Dhan, sid, IDX_I).is_some());
    }
    let exhausted = || counter("tv_rest_candle_fold_slots_exhausted_total", "");
    let dropped = || {
        counter(
            "tv_rest_candle_fold_dropped_total",
            "reason=\"slot_capacity\"",
        )
    };
    let (exhausted_before, dropped_before) = (exhausted(), dropped());
    let newcomer = FOLD_MAX_SLOTS as u64 + 7;
    // Neither a read-only lookup nor an already-known identity counts.
    assert!(slots.lookup(Feed::Dhan, newcomer, IDX_I).is_none());
    assert!(slots.slot_mut(Feed::Dhan, 5, IDX_I).is_some());
    assert_eq!(exhausted(), exhausted_before);
    // One refusal through each entry point.
    assert!(slots.slot_mut(Feed::Dhan, newcomer, IDX_I).is_none());
    assert!(!slots.replace_or_insert(SidDayFold::new(Feed::Dhan, newcomer, IDX_I)));
    let today = today_ist();
    let out = apply_bar_batch(
        &mut slots,
        &[bar(Feed::Truedata, newcomer, today, 0, 1.0)],
        today,
    );
    assert_eq!(out.slot_exhausted, 1);
    assert_eq!(exhausted(), exhausted_before + 3.0);
    assert_eq!(dropped(), dropped_before + 3.0);
}

/// I-P1-11 + feed-in-key: one batch carrying the same security id in two
/// segments and two feeds folds four separate instruments, never one merged
/// bar. Each emission carries its own identity and its own close.
#[test]
fn test_rest_candle_fold_apply_bar_batch_keeps_composite_identities_apart() {
    harness();
    const SID: u64 = 77;
    const SEG_B: u8 = 1;
    let today = today_ist();
    let identities = [
        (Feed::Dhan, IDX_I, 10.0),
        (Feed::Dhan, SEG_B, 20.0),
        (Feed::Truedata, IDX_I, 30.0),
        (Feed::Truedata, SEG_B, 40.0),
    ];
    let batch: Vec<ConfirmedBar> = identities
        .iter()
        .map(|&(feed, segment, close)| ConfirmedBar {
            exchange_segment_code: segment,
            ..bar(feed, SID, today, 0, close)
        })
        .collect();
    let mut slots = FoldSlots::new();
    let out = apply_bar_batch(&mut slots, &batch, today);
    assert_eq!(out.duplicates, 0, "no identity is mistaken for another");
    assert_eq!(slots.len(), 4, "one slot per (feed, id, segment)");
    assert_eq!(out.emissions.len(), 4, "one emission per identity");
    let mut indices = std::collections::HashSet::new();
    for (emission, &(feed, segment, close)) in out.emissions.iter().zip(identities.iter()) {
        assert_eq!(
            (emission.feed, emission.security_id, emission.segment_code),
            (feed, SID, segment)
        );
        assert!(!emission.is_refold);
        let m1: Vec<f64> = emission
            .sealed
            .iter()
            .filter(|s| s.tf == TfIndex::M1)
            .map(|s| s.bucket.close)
            .collect();
        assert_eq!(m1, [close], "{feed:?}/{segment}: its own minute, unmerged");
        indices.insert(slots.lookup(feed, SID, segment).expect("slot allocated"));
    }
    assert_eq!(indices.len(), 4, "four distinct slots");
}
