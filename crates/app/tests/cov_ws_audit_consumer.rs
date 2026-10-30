//! Coverage for `tickvault_app::ws_audit_consumer`: the two spawn paths that
//! own the `ws_event_audit` / `feed_gap_audit` consumers, driven end to end
//! against a tiny in-process HTTP server on 127.0.0.1 that stands in for
//! QuestDB's `/exec` (DDL) and `/write` (ILP over HTTP) endpoints.
//!
//! Every test asserts what actually reached the stub: the DDL statements the
//! consumer issued at start, and the ILP lines (table name, symbols, the
//! `open_at_shutdown` flag) each lifecycle event produced. No real QuestDB,
//! no network beyond loopback.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tickvault_app::ws_audit_consumer::{
    spawn_live_feed_lifecycle_audit, spawn_ws_event_audit_consumer,
};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::ws_event_types::{
    WS_EVENT_NO_DHAN_CODE, WsEventAuditRow, WsEventKind, WsSubscriptionDetail, WsType,
};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::WsLifecycleEvent;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One request the stub received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    body: String,
}

/// How the stub answers an ILP `POST /write`.
#[derive(Debug, Clone, Copy)]
enum WriteMode {
    /// 204 No Content — QuestDB's success answer.
    Accept,
    /// 400 Bad Request — a non-retriable ILP refusal.
    Refuse,
    /// The FIRST write is held for [`STALL`] before its 204, then every
    /// write is accepted at once: a database that stalls and recovers.
    StallFirst,
}

/// How long [`WriteMode::StallFirst`] holds the first write.
const STALL: Duration = Duration::from_millis(1_500);

/// Set once the stalled first write has been held (one stalling test only).
static FIRST_WRITE_STALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Decodes a chunked HTTP body (enough for the questdb/reqwest clients).
fn decode_chunked(raw: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = find_subslice(&raw[pos..], b"\r\n")? + pos;
        let size_text = std::str::from_utf8(&raw[pos..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        pos = line_end + 2;
        if size == 0 {
            // trailing CRLF after the last chunk
            let end = find_subslice(&raw[pos..], b"\r\n")? + pos + 2;
            return Some((out, end));
        }
        if raw.len() < pos + size + 2 {
            return None;
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size + 2;
    }
}

/// QuestDB's `/settings` answer, enough for the ILP client's protocol probe.
const SETTINGS_BODY: &str = r#"{"config":{"release.type":"OSS","release.version":"9.3.5","line.proto.support.versions":[1,2],"ilp.proto.transports":["tcp","http"],"cairo.max.file.name.length":127},"preferences.version":0,"preferences":{}}"#;

/// Starts the stub on its OWN OS thread and runtime, so the consumers'
/// blocking ILP calls (the sender's `/settings` probe runs synchronously on
/// a runtime worker) can never starve it. Returns its port and the request log.
async fn start_stub(mode: WriteMode) -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("stub runtime");
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
            let port = listener.local_addr().expect("addr").port();
            port_tx.send(port).expect("report port");
            serve(listener, mode, log).await;
        });
    });
    let port = port_rx.recv().expect("stub port");
    (port, seen)
}

async fn serve(listener: TcpListener, mode: WriteMode, log: Arc<Mutex<Vec<Seen>>>) {
    {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    // Parse as many complete requests as the buffer holds.
                    while let Some(head_end) = find_subslice(&buf, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                        let mut lines = head.lines();
                        let request_line = lines.next().unwrap_or_default().to_owned();
                        let mut parts = request_line.split_whitespace();
                        let method = parts.next().unwrap_or_default().to_owned();
                        let target = parts.next().unwrap_or_default().to_owned();
                        let mut content_length = 0usize;
                        let mut chunked = false;
                        for line in lines {
                            let lower = line.to_ascii_lowercase();
                            if let Some(v) = lower.strip_prefix("content-length:") {
                                content_length = v.trim().parse().unwrap_or(0);
                            }
                            if lower.starts_with("transfer-encoding:") && lower.contains("chunked")
                            {
                                chunked = true;
                            }
                        }
                        let body_start = head_end + 4;
                        let (body, consumed) = if chunked {
                            match decode_chunked(&buf[body_start..]) {
                                Some((b, used)) => (b, body_start + used),
                                None => break,
                            }
                        } else {
                            if buf.len() < body_start + content_length {
                                break;
                            }
                            (
                                buf[body_start..body_start + content_length].to_vec(),
                                body_start + content_length,
                            )
                        };
                        buf.drain(..consumed);
                        let is_write = method == "POST";
                        log.lock().expect("log").push(Seen {
                            method: method.clone(),
                            target: target.clone(),
                            body: String::from_utf8_lossy(&body).to_string(),
                        });
                        if is_write
                            && matches!(mode, WriteMode::StallFirst)
                            && !FIRST_WRITE_STALLED.swap(true, std::sync::atomic::Ordering::SeqCst)
                        {
                            tokio::time::sleep(STALL).await;
                        }
                        let response = if is_write {
                            match mode {
                                WriteMode::Accept | WriteMode::StallFirst => {
                                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
                                        .to_owned()
                                }
                                WriteMode::Refuse => {
                                    let b = r#"{"code":"invalid","message":"refused by stub","line":1,"errorId":"stub-1"}"#;
                                    format!(
                                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
                                        b.len()
                                    )
                                }
                            }
                        } else {
                            let b = if target.starts_with("/settings") {
                                SETTINGS_BODY
                            } else {
                                r#"{"ddl":"OK"}"#
                            };
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
                                b.len()
                            )
                        };
                        if sock.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
            });
        }
    }
}

fn cfg_for(port: u16) -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_owned(),
        http_port: port,
        pg_port: port,
        ilp_port: port,
    }
}

fn writes_containing(seen: &Arc<Mutex<Vec<Seen>>>, needle: &str) -> Vec<String> {
    seen.lock()
        .expect("log")
        .iter()
        .filter(|s| s.method == "POST")
        .flat_map(|s| s.body.lines().map(str::to_owned).collect::<Vec<String>>())
        .filter(|l| l.contains(needle))
        .collect()
}

/// Polls until `pred` holds or `deadline` passes.
async fn wait_until(deadline: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pred()
}

fn audit_row(kind: WsEventKind, index: i64) -> WsEventAuditRow {
    WsEventAuditRow {
        event_ts_ist_nanos: 1_790_000_000_000_000_000,
        trading_date_ist_nanos: 1_789_948_800_000_000_000,
        feed: tickvault_common::feed::Feed::Dhan,
        ws_type: WsType::MainFeed,
        connection_index: index,
        pool_size: 5,
        event_kind: kind,
        source: "test_source".to_owned(),
        reason: "test_reason".to_owned(),
        dhan_code: WS_EVENT_NO_DHAN_CODE,
        down_secs: 0,
        attempts: 0,
        market_hours: true,
        subscription: WsSubscriptionDetail::NONE,
    }
}

fn lifecycle(
    endpoint: DhanEndpointType,
    index: u8,
    kind: WsEventKind,
    reason: &'static str,
    dhan_code: Option<u16>,
) -> WsLifecycleEvent {
    WsLifecycleEvent {
        endpoint,
        connection_index: index,
        kind,
        reason,
        dhan_code,
        instruments_held: 42,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ws_audit_consumer_spawn_ws_event_audit_consumer_rows_reach_ilp_write() {
    let (port, seen) = start_stub(WriteMode::Accept).await;
    let tx = spawn_ws_event_audit_consumer(cfg_for(port));
    tx.send(audit_row(WsEventKind::Connected, 2))
        .await
        .expect("send row 1");
    tx.send(audit_row(WsEventKind::Disconnected, 3))
        .await
        .expect("send row 2");

    let landed = wait_until(Duration::from_secs(4), || {
        writes_containing(&seen, "ws_event_audit").len() >= 2
    })
    .await;
    assert!(landed, "both rows must be flushed as ILP lines: {seen:?}");

    // The DDL ran first, against /exec, and names the table.
    let ddl_hits = seen
        .lock()
        .expect("log")
        .iter()
        .filter(|s| s.method == "GET" && s.target.starts_with("/exec"))
        .count();
    assert!(ddl_hits >= 1, "the consumer must ensure its table at start");

    let lines = writes_containing(&seen, "ws_event_audit");
    assert!(
        lines.iter().any(|l| l.contains("ws_type=main_feed")),
        "row carries its ws_type symbol: {lines:?}"
    );
    drop(tx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ws_audit_consumer_spawn_ws_event_audit_consumer_refused_flush_keeps_consuming() {
    let (port, seen) = start_stub(WriteMode::Refuse).await;
    let tx = spawn_ws_event_audit_consumer(cfg_for(port));
    tx.send(audit_row(WsEventKind::Connected, 0))
        .await
        .expect("send row 1");
    tx.send(audit_row(WsEventKind::Reconnected, 1))
        .await
        .expect("send row 2");

    // A refused flush is logged and the loop continues: the second row is
    // still attempted (a second POST reaches the stub).
    let attempted = wait_until(Duration::from_secs(4), || {
        seen.lock()
            .expect("log")
            .iter()
            .filter(|s| s.method == "POST")
            .count()
            >= 2
    })
    .await;
    assert!(
        attempted,
        "every row must be attempted even after a refused flush: {seen:?}"
    );
    drop(tx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ws_audit_consumer_spawn_live_feed_lifecycle_audit_gaps_close_on_connect_and_shutdown()
{
    let (port, seen) = start_stub(WriteMode::Accept).await;
    let tx = spawn_live_feed_lifecycle_audit(cfg_for(port));

    // Slot 3 (depth-20): goes dark with an 805, one failed dial, then connects.
    tx.send(lifecycle(
        DhanEndpointType::Depth20,
        3,
        WsEventKind::Disconnected,
        "pool_overflow",
        Some(805),
    ))
    .await
    .expect("send");
    tx.send(lifecycle(
        DhanEndpointType::Depth20,
        3,
        WsEventKind::DialFailed,
        "dial_failed",
        None,
    ))
    .await
    .expect("send");
    tx.send(lifecycle(
        DhanEndpointType::Depth20,
        3,
        WsEventKind::Connected,
        "connected",
        None,
    ))
    .await
    .expect("send");
    // Slot 4 (main feed): goes dark and never comes back.
    tx.send(lifecycle(
        DhanEndpointType::MainFeed,
        4,
        WsEventKind::DisconnectedOffHours,
        "idle_silence",
        None,
    ))
    .await
    .expect("send");
    // Slot 5 closes for the process stop: every open gap ends now.
    tx.send(lifecycle(
        DhanEndpointType::Depth200,
        5,
        WsEventKind::Disconnected,
        "shutdown",
        None,
    ))
    .await
    .expect("send");
    // Out-of-range slot: ignored by the gap tracker, still audited.
    tx.send(lifecycle(
        DhanEndpointType::MainFeed,
        200,
        WsEventKind::Disconnected,
        "pool_overflow",
        None,
    ))
    .await
    .expect("send");

    let gaps_landed = wait_until(Duration::from_secs(4), || {
        writes_containing(&seen, "feed_gap_audit").len() >= 2
    })
    .await;
    assert!(gaps_landed, "two gaps must be written: {seen:?}");
    let rows_landed = wait_until(Duration::from_secs(4), || {
        writes_containing(&seen, "ws_event_audit").len() >= 6
    })
    .await;
    assert!(rows_landed, "every lifecycle event is one audit row");

    let gaps = writes_containing(&seen, "feed_gap_audit");
    // The connect-closed depth-20 gap is not open at shutdown.
    assert!(
        gaps.iter()
            .any(|l| l.contains("ws_type=depth_20") && l.contains("open_at_shutdown=f")),
        "the depth-20 gap closed by its connect: {gaps:?}"
    );
    // The main-feed gap still open at the stop is closed AT the stop.
    assert!(
        gaps.iter()
            .any(|l| l.contains("ws_type=main_feed") && l.contains("open_at_shutdown=t")),
        "the main-feed gap closed by the shutdown: {gaps:?}"
    );
    // No gap for the shutdown close itself (slot 5 opens nothing).
    assert!(
        !gaps.iter().any(|l| l.contains("ws_type=depth_200")),
        "a shutdown close opens no gap: {gaps:?}"
    );

    // Dropping the last sender ends the forwarder cleanly.
    drop(tx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ws_audit_consumer_spawn_live_feed_lifecycle_audit_drop_closes_open_gaps() {
    let (port, seen) = start_stub(WriteMode::Accept).await;
    let tx = spawn_live_feed_lifecycle_audit(cfg_for(port));
    tx.send(lifecycle(
        DhanEndpointType::MainFeed,
        1,
        WsEventKind::Disconnected,
        "idle_silence",
        Some(800),
    ))
    .await
    .expect("send");
    // Wait for the row so the forwarder has processed the disconnect.
    assert!(
        wait_until(Duration::from_secs(4), || {
            !writes_containing(&seen, "ws_event_audit").is_empty()
        })
        .await
    );
    // Every producer is gone: the open gap is written closed, not lost.
    drop(tx);
    let closed = wait_until(Duration::from_secs(4), || {
        writes_containing(&seen, "feed_gap_audit")
            .iter()
            .any(|l| l.contains("open_at_shutdown=t"))
    })
    .await;
    assert!(
        closed,
        "the open gap must be closed when every socket is gone"
    );
}

// ---------------------------------------------------------------------------
// Backpressure: a stalled writer fills the row channel, rows are dropped
// (counted, logged once per episode), and the episode closes when rows land.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct GlobalCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for GlobalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for GlobalCapture {
    type Writer = GlobalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A process-wide log buffer and metrics recorder (the forwarder logs and
/// counts on runtime worker threads, which a thread-scoped subscriber misses).
fn global_observers() -> &'static (GlobalCapture, metrics_exporter_prometheus::PrometheusHandle) {
    static OBS: std::sync::OnceLock<(
        GlobalCapture,
        metrics_exporter_prometheus::PrometheusHandle,
    )> = std::sync::OnceLock::new();
    OBS.get_or_init(|| {
        let cap = GlobalCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::set_global_recorder(recorder).expect("one global recorder");
        (cap, handle)
    })
}

fn dropped_rows(handle: &metrics_exporter_prometheus::PrometheusHandle) -> f64 {
    handle
        .render()
        .lines()
        .find_map(|l| {
            l.strip_prefix("tv_ws_event_audit_dropped_total{reason=\"live_feed_forward\"} ")
        })
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_ws_audit_consumer_spawn_live_feed_lifecycle_audit_stalled_writer_drops_then_recovers()
{
    let (cap, metrics_handle) = global_observers();
    let (port, seen) = start_stub(WriteMode::StallFirst).await;
    let tx = spawn_live_feed_lifecycle_audit(cfg_for(port));

    // The first row's flush is held by the stub.
    tx.send(lifecycle(
        DhanEndpointType::MainFeed,
        7,
        WsEventKind::DialFailed,
        "first_row",
        None,
    ))
    .await
    .expect("send");
    let first_post = wait_until(Duration::from_secs(4), || {
        seen.lock().expect("log").iter().any(|s| s.method == "POST")
    })
    .await;
    assert!(first_post, "the first write reaches the stub and stalls");

    // Flood well past the 1,024-row channel while the writer is stalled.
    for _ in 0..1_200 {
        tx.send(lifecycle(
            DhanEndpointType::MainFeed,
            7,
            WsEventKind::DialFailed,
            "flood_row",
            None,
        ))
        .await
        .expect("send");
    }
    let dropped = wait_until(Duration::from_secs(4), || {
        dropped_rows(metrics_handle) >= 1.0
    })
    .await;
    assert!(
        dropped,
        "rows past the channel bound are counted as dropped"
    );

    // Once the stall clears the queue drains; the next row lands and closes
    // the drop episode.
    let drained = wait_until(Duration::from_secs(15), || {
        writes_containing(&seen, "flood_row").len() >= 900
    })
    .await;
    assert!(drained, "the queued rows are written after the stall");
    tx.send(lifecycle(
        DhanEndpointType::MainFeed,
        7,
        WsEventKind::DialFailed,
        "recovered_probe",
        None,
    ))
    .await
    .expect("send");
    let landed = wait_until(Duration::from_secs(4), || {
        !writes_containing(&seen, "recovered_probe").is_empty()
    })
    .await;
    assert!(landed, "a row sent after the episode lands");

    let logs = cap.text();
    assert!(
        logs.contains("socket-lifecycle row was DROPPED"),
        "the episode is paged once"
    );
    assert!(
        logs.contains("the drop episode is over"),
        "the recovery is said once rows land again"
    );
    // Paged ONCE per episode, not once per dropped row.
    assert_eq!(
        logs.matches("socket-lifecycle row was DROPPED").count(),
        1,
        "one line per episode"
    );
    drop(tx);
}

impl GlobalCapture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("capture")).to_string()
    }
}
