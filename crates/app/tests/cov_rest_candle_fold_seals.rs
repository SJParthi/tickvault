//! Coverage tests for `rest_candle_fold` seal emission with a REAL global seal
//! sender installed (a test-only `SealWriterRunner` with a 4-seal queue that
//! is never drained): delivered, full (live drop and paced catch-up give-up)
//! and closed (writer gone) arms of both `emit_seals` and `emit_seals_paced`.
//!
//! The global sender is first-install-wins for the process, so the phases run
//! in ONE sequential test in this binary of their own.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::NaiveDate;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde_json::{Value, json};
use tickvault_app::rest_candle_fold::{
    ConfirmedBar, RestCandleFoldParams, SharedFoldReceiverSlot, day_start_nanos,
    run_rest_candle_fold, today_ist,
};
use tickvault_common::config::{QuestDbConfig, RestCandleFoldConfig};
use tickvault_common::feed::Feed;
use tickvault_storage::seal_writer_runner::{SealWriterRunner, set_global_seal_sender};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

fn harness() -> &'static PrometheusHandle {
    static H: OnceLock<PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::TRACE)
            .try_init();
        PrometheusBuilder::new()
            .install_recorder()
            .expect("install the prometheus recorder")
    })
}

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
// Canned /exec server: discovery + bars by (sid, day start)
// ---------------------------------------------------------------------------

type Bars = Arc<HashMap<(i64, i64), Value>>;

fn between<'a>(q: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = q.find(start)? + start.len();
    let rest = &q[i..];
    Some(&rest[..rest.find(end)?])
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn serve_one(mut s: TcpStream, discovery: Arc<Value>, bars: Bars) {
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
    let url = reqwest::Url::parse(&format!("http://mock{target}")).expect("target");
    let q = url
        .query_pairs()
        .find(|(k, _)| k == "query")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    let body = if q.starts_with("SELECT DISTINCT") {
        if q.contains("feed = 'dhan'") {
            (*discovery).clone()
        } else {
            json!({"dataset": []})
        }
    } else {
        let sid = between(&q, "security_id = ", " ").and_then(|v| v.parse::<i64>().ok());
        let start = between(&q, "ts >= ", " ").and_then(|v| v.parse::<i64>().ok());
        match (sid, start) {
            (Some(sid), Some(start)) => bars
                .get(&(sid, start * 1_000))
                .cloned()
                .unwrap_or_else(|| json!({"dataset": []})),
            _ => json!({"dataset": []}),
        }
    }
    .to_string();
    let out = format!(
        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(out.as_bytes()).await;
    let _ = s.shutdown().await;
}

async fn spawn_mock(discovery: Value, bars: HashMap<(i64, i64), Value>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let discovery = Arc::new(discovery);
    let bars: Bars = Arc::new(bars);
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(serve_one(s, Arc::clone(&discovery), Arc::clone(&bars)));
        }
    });
    port
}

/// A fresh directory under the system temp dir, unique per process, instant
/// and call (pid + nanos + counter), removed on drop, so a panicking test
/// leaves nothing behind.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("tv-cov-{tag}-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SESSION_OPEN_SOD: i64 = 33_300;

fn minute_nanos(date: NaiveDate, minute: i64) -> i64 {
    day_start_nanos(date) + (SESSION_OPEN_SOD + minute * 60) * 1_000_000_000
}

fn bars_json(date: NaiveDate, minutes: &[i64]) -> Value {
    let ds: Vec<Value> = minutes
        .iter()
        .map(|m| json!([minute_nanos(date, *m), 10.0, 11.0, 9.0, 10.5, 0]))
        .collect();
    json!({ "dataset": ds })
}

fn live_bar(sid: u64, date: NaiveDate, minute: i64) -> ConfirmedBar {
    ConfirmedBar {
        feed: Feed::Dhan,
        security_id: sid,
        exchange_segment_code: 0,
        minute_ts_ist_nanos: minute_nanos(date, minute),
        open: 10.0,
        high: 11.0,
        low: 9.0,
        close: 10.5,
        volume: 0,
    }
}

async fn run_fold(port: u16, catchup_days: u32, live: &[ConfirmedBar]) {
    let (tx, rx) = mpsc::channel(16);
    let slot: SharedFoldReceiverSlot = Arc::new(Mutex::new(Some(rx)));
    let task = tokio::spawn(run_rest_candle_fold(RestCandleFoldParams {
        config: RestCandleFoldConfig {
            enabled: true,
            catchup_days,
        },
        questdb: QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: 1,
            ilp_port: 1,
        },
        receiver_slot: slot,
    }));
    for b in live {
        tx.send(*b).await.expect("send");
    }
    drop(tx);
    tokio::time::timeout(Duration::from_secs(600), task)
        .await
        .expect("the fold task exits once the channel closes")
        .expect("no panic");
}

#[tokio::test(start_paused = true)]
async fn test_rest_candle_fold_seal_emission_delivered_full_and_closed_arms() {
    harness();
    // Declared before the runner, so it is dropped after it (removed last).
    let dir = TempDir::new("rest-candle-fold-seals");
    let runner =
        SealWriterRunner::for_test(dir.path().join("spill"), dir.path().join("dlq"), 64, 4, 64);
    assert!(
        set_global_seal_sender(runner.sender()),
        "first install wins"
    );

    let today = today_ist();
    let yesterday = today.pred_opt().expect("yesterday");

    // ── Phase 1+2: a 4-slot queue that nobody drains.
    let delivered_before = counter("tv_rest_candle_fold_seals_total", "feed=\"dhan\"");
    let full_before = counter("tv_rest_candle_fold_dropped_total", "seal_channel_full");
    let waits_before = counter("tv_rest_candle_fold_paced_waits_total", "");
    let mut bars = HashMap::new();
    // Today: two minutes -> two 1-minute seals (delivered).
    bars.insert((501, day_start_nanos(today)), bars_json(today, &[0, 1]));
    // Yesterday: one minute plus the force-sealed open buckets — more than
    // the two slots left, so the paced path waits out its budget and drops.
    bars.insert(
        (501, day_start_nanos(yesterday)),
        bars_json(yesterday, &[0]),
    );
    let port = spawn_mock(json!({"dataset": [[501, "IDX_I"]]}), bars).await;
    // The live minute seals again into the full queue (non-paced drop).
    run_fold(port, 2, &[live_bar(501, today, 2)]).await;

    assert_eq!(
        runner.queued_seals(),
        4,
        "the queue holds exactly its capacity"
    );
    assert_eq!(
        counter("tv_rest_candle_fold_seals_total", "feed=\"dhan\""),
        delivered_before + 4.0
    );
    assert!(
        counter("tv_rest_candle_fold_dropped_total", "seal_channel_full") >= full_before + 2.0,
        "the paced give-up and the live drop are both counted"
    );
    assert!(
        counter("tv_rest_candle_fold_paced_waits_total", "") > waits_before,
        "a full queue is waited on before the catch-up gives up"
    );

    // ── Phase 3: the writer is gone; every send reports a closed channel.
    drop(runner);
    let closed_before = counter("tv_rest_candle_fold_dropped_total", "seal_channel_closed");
    let mut bars = HashMap::new();
    bars.insert((502, day_start_nanos(today)), bars_json(today, &[0, 1]));
    let port = spawn_mock(json!({"dataset": [[502, "IDX_I"]]}), bars).await;
    run_fold(port, 1, &[live_bar(502, today, 2)]).await;
    assert!(
        counter("tv_rest_candle_fold_dropped_total", "seal_channel_closed") >= closed_before + 2.0,
        "the paced catch-up and the live emission both count closed drops"
    );
    assert_eq!(
        counter("tv_rest_candle_fold_seals_total", "feed=\"dhan\""),
        delivered_before + 4.0,
        "nothing is delivered to a closed queue"
    );
}
