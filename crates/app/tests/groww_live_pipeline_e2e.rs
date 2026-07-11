//! Groww live-feed END-TO-END proof against a REAL QuestDB (operator 2026-06-28).
//!
//! This is the hard proof — not inference — that synthetic Groww ticks fed through
//! the SAME public bridge path the Monday 09:15 sidecar will use land in the SHARED
//! `ticks` AND `candles_1m` tables tagged `feed='groww'`, and coexist with a Dhan
//! row for the same instrument+minute (feed-in-key uniqueness).
//!
//! ## What it drives (the REAL path, no re-implementation)
//! 1. Installs a real `SealWriterRunner` via `set_global_seal_sender` + spawns the
//!    seal-writer loop, so `global_seal_sender()` resolves to a writer that lands
//!    sealed candles in `candles_1m`.
//! 2. Ensures the SHARED `ticks` + 21 `candles_<tf>` tables.
//! 3. Writes synthetic NDJSON in the EXACT sidecar schema across a 1-minute
//!    boundary (so the minute-N M1 bucket seals) + the sidecar status file.
//! 4. Spawns the PUBLIC `run_groww_bridge` (event-driven `notify` tail) with the
//!    Groww feed enabled, and waits for the rows to appear.
//! 5. Asserts via QuestDB HTTP `/exec`: `ticks` has the Groww rows with the right
//!    `(security_id, segment, capture_seq, feed)`; `candles_1m` has a `feed='groww'`
//!    row with the expected OHLC; then inserts a `feed='dhan'` row for the SAME
//!    instrument+minute and asserts BOTH survive.
//!
//! ## Live-QuestDB gate
//! The non-`#[ignore]` tests SKIP cleanly (printing why) when QuestDB is not
//! reachable on 127.0.0.1:9000/9009, so `cargo test` is green in environments
//! without a DB. They do REAL work (and assert real rows) when it IS up. A skip is
//! reported as a skip — never a false pass.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tickvault_api::feed_state::{Feed, FeedRuntimeState};
use tickvault_common::config::{FeedsConfig, QuestDbConfig, TradingConfig};
use tickvault_common::feed_health::FeedHealthRegistry;
use tickvault_common::trading_calendar::TradingCalendar;

const QDB_HTTP_PORT: u16 = 9000;
const QDB_ILP_PORT: u16 = 9009;

/// A per-RUN unique SID base in the 880-million reserved range, salted from a
/// monotonic wall-clock so a re-run (or residual rows from a previous run against
/// the same QuestDB) can never satisfy a count prematurely. Bounded to keep the
/// SIDs well within the reserved 880_000_001..889_999_999 window.
fn sid_base() -> i64 {
    let salt = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
        .rem_euclid(9_000_000);
    880_000_001 + salt
}

fn test_qdb() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: QDB_HTTP_PORT,
        pg_port: 8812,
        ilp_port: QDB_ILP_PORT,
    }
}

/// A fresh Groww aggregator for the `run_groww_bridge` 7th argument (2026-07-02:
/// the aggregator + IST-midnight force-seal are owned by the SUPERVISOR wrapper
/// in prod — `spawn_supervised_groww_bridge` — so the raw bridge fn takes the
/// instance; these e2e tests drive the raw bridge and never reach a midnight
/// boundary, so a standalone instance is exactly right).
fn test_aggregator() -> tickvault_trading::candles::MultiTfAggregator {
    tickvault_trading::candles::MultiTfAggregator::with_capacity(
        tickvault_app::groww_bridge::GROWW_AGGREGATOR_CAPACITY,
    )
}

#[allow(dead_code)] // APPROVED: retained for future e2e that exercises the supervisor wrapper (needs the calendar arg)
fn test_calendar() -> Arc<TradingCalendar> {
    let trading = TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: vec![],
        muhurat_trading_dates: vec![],
        nse_mock_trading_dates: vec![],
    };
    Arc::new(TradingCalendar::from_config(&trading).expect("valid test calendar")) // APPROVED: test-only
}

/// True when the CI lane demands a real QuestDB: with `TV_REQUIRE_QDB` set,
/// an unreachable QuestDB PANICS instead of skipping, so the Groww e2e lane
/// can never vacuously pass (audit-findings Rule 11: no false-OK signals).
fn qdb_required() -> bool {
    std::env::var("TV_REQUIRE_QDB").is_ok()
}

/// Panic message for the `TV_REQUIRE_QDB` hard-fail branch (shared by all 3 tests).
const QDB_REQUIRED_PANIC: &str = "TV_REQUIRE_QDB=1 set but QuestDB not reachable on \
     127.0.0.1:9000/9009 — the CI lane must never vacuously pass (audit Rule 11)";

/// True when QuestDB's HTTP `/exec` answers 200 AND the ILP TCP port accepts a
/// connection. Used to SKIP (not fail) when no DB is available.
async fn questdb_available() -> bool {
    let http_ok = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{QDB_HTTP_PORT}/exec?query=SELECT%201"
        ))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    let ilp_ok = std::net::TcpStream::connect(("127.0.0.1", QDB_ILP_PORT)).is_ok();
    http_ok && ilp_ok
}

/// Run an arbitrary SQL via QuestDB HTTP `/exec` and return the raw JSON body.
async fn exec(sql: &str) -> String {
    let url = format!("http://127.0.0.1:{QDB_HTTP_PORT}/exec");
    reqwest::Client::new()
        .get(&url)
        .query(&[("query", sql)])
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("questdb /exec request")
        .text()
        .await
        .expect("questdb /exec body")
}

/// Scrape the single integer in a `{"dataset":[[N]],...}` count response.
fn scrape_count(body: &str) -> i64 {
    let marker = "\"dataset\":[[";
    let start = body
        .find(marker)
        .unwrap_or_else(|| panic!("response missing dataset: {body}"));
    let from = start + marker.len();
    let end = body[from..]
        .find(']')
        .unwrap_or_else(|| panic!("malformed dataset: {body}"));
    body[from..from + end]
        .trim()
        .parse::<i64>()
        .unwrap_or_else(|_| panic!("count not an int: {}", &body[from..from + end]))
}

async fn count(sql: &str) -> i64 {
    scrape_count(&exec(sql).await)
}

/// A unique temp working dir for this test run (avoids cross-run collisions).
fn unique_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("tv_groww_e2e_{}_{}_{}", tag, std::process::id(), n));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Current wall-clock as IST epoch nanos — the sidecar's `now_ist_nanos()`
/// convention (UTC epoch + 19,800s), the SAME epoch the bridge's
/// `receipt_ist_nanos` / `groww_status_is_live` freshness gate compares against.
fn now_ist_nanos() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        + tickvault_common::constants::IST_UTC_OFFSET_NANOS
}

/// Write the sidecar connect+subscribe PROOF status file EXACTLY like the real
/// sidecar's `write_status` (groww_sidecar.py): event + counts + a FRESH
/// IST-epoch `ts_ist_nanos` write stamp. The bridge's stale-status freshness
/// gate (2026-07-06 fix) accepts a record only when its stamp is at/after the
/// bridge's OWN start (the live floor) and within 120s of now — a stampless or
/// pre-spawn record is honestly rejected as a prior-incarnation fossil. Tests
/// that assert `connected` must therefore keep this record fresh, exactly like
/// the real sidecar (which rewrites the status ≤1s apart on real emits) — the
/// gate itself is never weakened here.
fn write_status_streaming(path: &std::path::Path, stocks: u64, indices: u64) {
    std::fs::write(
        path,
        format!(
            "{{\"event\":\"streaming\",\"stocks\":{stocks},\"indices\":{indices},\"total\":{},\"ts_ist_nanos\":{}}}",
            stocks + indices,
            now_ist_nanos(),
        ),
    )
    .expect("write status");
}

/// One synthetic NDJSON tick line in the EXACT sidecar schema.
fn ndjson(security_id: i64, segment: &str, ts_ist_nanos: i64, ltp: f64, cum_volume: i64) -> String {
    let exchange_ts_millis = ts_ist_nanos / 1_000_000;
    format!(
        "{{\"security_id\":{security_id},\"segment\":\"{segment}\",\"ts_ist_nanos\":{ts_ist_nanos},\"exchange_ts_millis\":{exchange_ts_millis},\"ltp\":{ltp},\"cum_volume\":{cum_volume}}}\n"
    )
}

/// Install the real seal-writer (idempotent global install) + ensure the shared
/// tables. Returns the QuestDB config. Spawns the seal loop on the current runtime.
async fn boot_shared_infra(qdb: &QuestDbConfig) {
    // Ensure both shared table families exist (idempotent CREATE + DEDUP).
    tickvault_storage::tick_persistence::ensure_tick_table_dedup_keys(qdb).await;
    tickvault_storage::shadow_persistence::ensure_shadow_candle_tables(qdb).await;

    // Install the real seal-writer runner so global_seal_sender() resolves and
    // sealed candles land in candles_1m. set_global_seal_sender is idempotent
    // (process-global OnceLock) — only the first test to call it installs; that is
    // fine, the writer is wired to the same QuestDB for every test here.
    use tickvault_storage::seal_writer_loop::{run_seal_writer_loop, seal_drain_interval};
    use tickvault_storage::seal_writer_runner::{SealWriterRunner, set_global_seal_sender};
    if let Ok(runner) = SealWriterRunner::new(qdb, 1_024)
        && set_global_seal_sender(runner.sender())
    {
        {
            let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            std::mem::forget(_cancel_tx);
            tokio::spawn(async move {
                let _ = run_seal_writer_loop(runner, seal_drain_interval(), cancel_rx).await;
            });
        }
    }
}

/// Poll a count query until it reaches `>= want` or the deadline elapses.
async fn wait_until_count(sql: &str, want: i64, deadline: Duration) -> i64 {
    let start = std::time::Instant::now();
    let mut last = 0;
    while start.elapsed() < deadline {
        last = count(sql).await;
        if last >= want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    last
}

/// THE PROOF: synthetic Groww ticks → ticks + candles_1m tagged feed='groww',
/// coexisting with a Dhan row for the same instrument+minute.
#[tokio::test]
async fn groww_ticks_and_candles_land_tagged_feed_groww() {
    if !questdb_available().await {
        assert!(!qdb_required(), "{QDB_REQUIRED_PANIC}");
        eprintln!(
            "SKIP groww_ticks_and_candles_land_tagged_feed_groww: QuestDB not reachable on \
             127.0.0.1:{QDB_HTTP_PORT}/{QDB_ILP_PORT} — run a local QuestDB to exercise this proof."
        );
        return;
    }
    let qdb = test_qdb();
    boot_shared_infra(&qdb).await;

    // Build a synthetic stream across a 1-minute boundary so the minute-N M1 bucket
    // SEALS when the minute-(N+1) tick arrives. Use a fixed plausible IST instant.
    // 2026-06-15 10:30:00 IST in epoch nanos (within the validate window 2020..2100).
    let min_n_base: i64 = 1_781_001_000_000_000_000; // floor to a second; we offset below
    let sid = sid_base();
    let seg = "NSE_EQ";
    // Three ticks in minute N (open=100, high=105, low=99, close=102), then one tick
    // in minute N+1 to force the minute-N bucket to seal.
    let s = 1_000_000_000_i64; // nanos per second
    let mut nd = String::new();
    nd.push_str(&ndjson(sid, seg, min_n_base + s, 100.0, 0));
    nd.push_str(&ndjson(sid, seg, min_n_base + 10 * s, 105.0, 0));
    nd.push_str(&ndjson(sid, seg, min_n_base + 50 * s, 99.0, 0));
    nd.push_str(&ndjson(sid, seg, min_n_base + 59 * s, 102.0, 0)); // close of minute N
    // A tick in minute N+1 (61s later) crosses the boundary → seals minute N.
    nd.push_str(&ndjson(sid, seg, min_n_base + 61 * s, 103.5, 0));

    let dir = unique_dir("happy");
    let tick_file = dir.join("live-ticks.ndjson");
    let status_file = dir.join("groww-status.json");
    std::fs::write(&tick_file, &nd).expect("write ndjson");
    // Status file: the sidecar's connect+subscribe PROOF (streaming), stamped
    // fresh in the real sidecar's format. This pre-spawn write is (correctly)
    // rejected by the freshness gate as pre-floor; the refresher task below
    // supplies the post-spawn fresh proof.
    write_status_streaming(&status_file, 5, 1);

    // Enable Groww + spawn the PUBLIC event-driven bridge against the temp files.
    let feeds = FeedsConfig {
        dhan_enabled: false,
        groww_enabled: true,
        ..Default::default()
    };
    let feed_runtime = Arc::new(FeedRuntimeState::from_config(&feeds));
    let feed_health = Arc::new(FeedHealthRegistry::new());
    let bridge = tokio::spawn(tickvault_app::groww_bridge::run_groww_bridge(
        qdb.clone(),
        tick_file.clone(),
        status_file.clone(),
        Arc::clone(&feed_runtime),
        Arc::clone(&feed_health),
        None,
        None,
        test_aggregator(),
        std::sync::Arc::new(tickvault_app::groww_bridge::GrowwAuditLatches::default()),
        std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        None,
    ));

    // Mirror the real sidecar's ≤1s status rewrite on real emits: keep the
    // streaming PROOF record FRESH (stamped at/after the bridge's live floor,
    // which is set at bridge-task start) so the stale-status freshness gate
    // can honestly latch `connected`. Without this, the single pre-spawn
    // write above is rejected as a prior-incarnation fossil — the gate's
    // exact design (2026-07-06 stale-status false-OK fix). Bounded loop
    // (60s) comfortably outlives every assertion deadline below.
    let refresher_status = status_file.clone();
    let _status_refresher = tokio::spawn(async move {
        for _ in 0..240 {
            write_status_streaming(&refresher_status, 5, 1);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });

    // --- Assert the 5 ticks landed in `ticks` tagged feed='groww' ---
    let ticks_sql =
        format!("SELECT count(*) FROM ticks WHERE security_id = {sid} AND feed = 'groww'");
    let got_ticks = wait_until_count(&ticks_sql, 5, Duration::from_secs(20)).await;
    assert_eq!(
        got_ticks, 5,
        "expected 5 Groww ticks in the shared `ticks` table tagged feed='groww', got {got_ticks}"
    );

    // The connect+subscribe PROOF must have flipped connected (streaming) + recorded
    // counts. Poll briefly: QuestDB WAL-apply can surface the row a beat before the
    // bridge's same-iteration `set_connected` is observable from this thread.
    let now_ist = now_ist_nanos;
    let connect_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < connect_deadline
        && !feed_health
            .snapshot(Feed::Groww, true, true, true, now_ist())
            .input
            .connected
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let report = feed_health.snapshot(Feed::Groww, true, true, true, now_ist());
    assert!(
        report.input.connected,
        "connected must be true once streaming"
    );
    assert_eq!(
        report.input.subscribed_stocks, 5,
        "subscribe stock count recorded"
    );
    assert_eq!(
        report.input.subscribed_indices, 1,
        "subscribe index count recorded"
    );

    // --- Assert the minute-N candle sealed into candles_1m tagged feed='groww' ---
    let candle_sql =
        format!("SELECT count(*) FROM candles_1m WHERE security_id = {sid} AND feed = 'groww'");
    let got_candles = wait_until_count(&candle_sql, 1, Duration::from_secs(20)).await;
    assert!(
        got_candles >= 1,
        "expected at least one feed='groww' candle in candles_1m, got {got_candles}"
    );
    // Verify the OHLC of the sealed minute-N bucket (open=100, high=105, low=99, close=102).
    let ohlc_body = exec(&format!(
        "SELECT open, high, low, close FROM candles_1m WHERE security_id = {sid} AND feed = 'groww' ORDER BY ts ASC LIMIT 1"
    ))
    .await;
    assert!(
        ohlc_body.contains("100"),
        "candle open=100 expected: {ohlc_body}"
    );
    assert!(
        ohlc_body.contains("105"),
        "candle high=105 expected: {ohlc_body}"
    );
    assert!(
        ohlc_body.contains("99"),
        "candle low=99 expected: {ohlc_body}"
    );
    assert!(
        ohlc_body.contains("102"),
        "candle close=102 expected: {ohlc_body}"
    );

    // --- Feed-in-key coexistence: a Dhan row for the SAME (security_id,segment,ts) ---
    // Insert one Dhan candle for the same instrument + minute and assert BOTH survive
    // (the candle DEDUP key (ts, security_id, segment, feed) keeps them distinct).
    // Use the minute-N bucket ts (floor to minute) in nanos.
    let minute_ts_nanos = (min_n_base / 60_000_000_000) * 60_000_000_000;
    // ILP line: candles_1m,feed=dhan,segment=NSE_EQ security_id=...,open=...,... ts
    let ilp = format!(
        "candles_1m,feed=dhan,segment={seg} security_id={sid}i,open=200.0,high=200.0,low=200.0,close=200.0 {minute_ts_nanos}\n"
    );
    ilp_send(&ilp).await;
    // Both feeds' candle rows for this (security_id, segment, minute) must coexist.
    let both_sql = format!(
        "SELECT count(*) FROM (SELECT DISTINCT feed FROM candles_1m WHERE security_id = {sid} AND segment = '{seg}')"
    );
    let distinct_feeds = wait_until_count(&both_sql, 2, Duration::from_secs(10)).await;
    assert_eq!(
        distinct_feeds, 2,
        "a Dhan candle and a Groww candle for the same instrument+minute must BOTH survive \
         (feed-in-key uniqueness) — got {distinct_feeds} distinct feeds"
    );

    bridge.abort();
    let _ = std::fs::remove_dir_all(&dir);

    eprintln!(
        "PROVED: {got_ticks} Groww ticks in `ticks` (feed=groww), {got_candles} candle(s) in \
         candles_1m (feed=groww) with OHLC 100/105/99/102, and {distinct_feeds} distinct feeds \
         (dhan+groww) coexisting for SID {sid}."
    );
}

/// Send one ILP line directly to QuestDB (used to inject the Dhan coexistence row).
async fn ilp_send(line: &str) {
    use std::io::Write;
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", QDB_ILP_PORT)).expect("ilp connect");
    stream.write_all(line.as_bytes()).expect("ilp write");
    stream.flush().expect("ilp flush");
    // Give QuestDB a moment to commit the WAL.
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Chaos: a malformed NDJSON line is skipped (no panic) and the valid lines still land.
#[tokio::test]
async fn malformed_ndjson_line_is_skipped_and_valid_lines_land() {
    if !questdb_available().await {
        assert!(!qdb_required(), "{QDB_REQUIRED_PANIC}");
        eprintln!("SKIP malformed_ndjson_line_is_skipped: QuestDB not reachable.");
        return;
    }
    let qdb = test_qdb();
    boot_shared_infra(&qdb).await;

    let sid = sid_base() + 100;
    let seg = "NSE_EQ";
    let s = 1_000_000_000_i64;
    let base: i64 = 1_781_002_000_000_000_000;
    let mut nd = String::new();
    nd.push_str(&ndjson(sid, seg, base + s, 50.0, 0)); // valid
    nd.push_str("this is not json at all\n"); // malformed → skipped, no panic
    nd.push_str("{\"security_id\":1,\"segment\":\"WAT\"}\n"); // bad segment → skipped
    nd.push_str(&ndjson(sid, seg, base + 2 * s, 51.0, 0)); // valid

    let dir = unique_dir("malformed");
    let tick_file = dir.join("live-ticks.ndjson");
    let status_file = dir.join("groww-status.json");
    std::fs::write(&tick_file, &nd).expect("write ndjson");
    std::fs::write(
        &status_file,
        r#"{"event":"streaming","stocks":1,"indices":0}"#,
    )
    .expect("write status");

    let feeds = FeedsConfig {
        dhan_enabled: false,
        groww_enabled: true,
        ..Default::default()
    };
    let feed_runtime = Arc::new(FeedRuntimeState::from_config(&feeds));
    let feed_health = Arc::new(FeedHealthRegistry::new());
    let bridge = tokio::spawn(tickvault_app::groww_bridge::run_groww_bridge(
        qdb.clone(),
        tick_file.clone(),
        status_file.clone(),
        Arc::clone(&feed_runtime),
        Arc::clone(&feed_health),
        None,
        None,
        test_aggregator(),
        std::sync::Arc::new(tickvault_app::groww_bridge::GrowwAuditLatches::default()),
        std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        None,
    ));

    let ticks_sql =
        format!("SELECT count(*) FROM ticks WHERE security_id = {sid} AND feed = 'groww'");
    let got = wait_until_count(&ticks_sql, 2, Duration::from_secs(20)).await;
    assert_eq!(
        got, 2,
        "the 2 valid lines must land; the malformed + bad-segment lines must be skipped (no panic), got {got}"
    );
    bridge.abort();
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("PROVED: malformed lines skipped, {got} valid Groww ticks landed for SID {sid}.");
}

/// Idempotency: re-feeding the SAME NDJSON (file shrink → offset reset) must NOT
/// create duplicate rows — `capture_seq` is seeded from the replay-stable
/// `ts_ist_nanos`, so the same tick reuses the same key and DEDUP collapses it.
#[tokio::test]
async fn replay_same_ndjson_is_idempotent_no_duplicate_rows() {
    if !questdb_available().await {
        assert!(!qdb_required(), "{QDB_REQUIRED_PANIC}");
        eprintln!("SKIP replay_same_ndjson_is_idempotent: QuestDB not reachable.");
        return;
    }
    let qdb = test_qdb();
    boot_shared_infra(&qdb).await;

    let sid = sid_base() + 200;
    let seg = "NSE_EQ";
    let s = 1_000_000_000_i64;
    let base: i64 = 1_781_003_000_000_000_000;
    let nd = {
        let mut v = String::new();
        v.push_str(&ndjson(sid, seg, base + s, 70.0, 0));
        v.push_str(&ndjson(sid, seg, base + 2 * s, 71.0, 0));
        v.push_str(&ndjson(sid, seg, base + 3 * s, 72.0, 0));
        v
    };

    let ticks_sql =
        format!("SELECT count(*) FROM ticks WHERE security_id = {sid} AND feed = 'groww'");

    // Run twice against the SAME content via two short bridge lifetimes. The second
    // run re-reads from offset 0 (fresh bridge state) — capture_seq is seeded from
    // ts_ist_nanos, so the same 3 ticks produce the same 3 keys → DEDUP collapses.
    for round in 0..2 {
        let dir = unique_dir(&format!("replay{round}"));
        let tick_file = dir.join("live-ticks.ndjson");
        let status_file = dir.join("groww-status.json");
        std::fs::write(&tick_file, &nd).expect("write ndjson");
        std::fs::write(
            &status_file,
            r#"{"event":"streaming","stocks":1,"indices":0}"#,
        )
        .expect("write status");
        let feeds = FeedsConfig {
            dhan_enabled: false,
            groww_enabled: true,
            ..Default::default()
        };
        let feed_runtime = Arc::new(FeedRuntimeState::from_config(&feeds));
        let feed_health = Arc::new(FeedHealthRegistry::new());
        let bridge = tokio::spawn(tickvault_app::groww_bridge::run_groww_bridge(
            qdb.clone(),
            tick_file.clone(),
            status_file.clone(),
            Arc::clone(&feed_runtime),
            Arc::clone(&feed_health),
            None,
            None,
            test_aggregator(),
            std::sync::Arc::new(tickvault_app::groww_bridge::GrowwAuditLatches::default()),
            std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
            None,
        ));
        let _ = wait_until_count(&ticks_sql, 3, Duration::from_secs(15)).await;
        bridge.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // After replaying the identical stream, there must still be exactly 3 rows.
    let final_count = count(&ticks_sql).await;
    assert_eq!(
        final_count, 3,
        "replaying the same NDJSON must be idempotent (capture_seq seeded from ts_ist_nanos): \
         expected exactly 3 rows, got {final_count}"
    );
    eprintln!(
        "PROVED: replaying the same 3-tick stream stayed at {final_count} rows (idempotent)."
    );
}
