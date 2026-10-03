//! Per-call latency of the database write path, as a DISTRIBUTION (p50 / p99 /
//! p99.9 / max). Companion to the decode, fold and WAL hand-off harnesses in
//! `tickvault-trading` and `tickvault-core` (`hot_path_latency`).
//!
//! Two harnesses:
//!
//! 1. [`ilp_append_cost_per_row`] — `TickWriter::append_tick_with_seq` and
//!    `DepthWriter::append_row` into the ILP buffer. Pure CPU, no database.
//!    This is the per-row work the frame drain does.
//! 2. [`ilp_flush_and_visibility_latency_live_questdb`] — a real
//!    `TickWriter::new` / `DepthWriter::new` against a running QuestDB, timing
//!    the synchronous `flush()` (returns on the server's acknowledgement, the
//!    WAL commit) and then how long until the rows are visible to a query
//!    (applied). Runs only when `TV_QDB_HTTP=host:port` is set; otherwise it
//!    prints why and returns.
//!
//! Both are `#[ignore]`d: a wall-clock figure must never gate CI. Run:
//!
//! ```text
//! cargo test --release -p tickvault-storage --test ilp_write_latency -- --ignored --nocapture
//! TV_QDB_HTTP=127.0.0.1:9000 cargo test --release -p tickvault-storage \
//!     --test ilp_write_latency -- --ignored --nocapture
//! ```
//!
//! What a figure here is NOT: production disk, production QuestDB tuning, or
//! the Graviton host. It is the cost of the code and of one local QuestDB.

use std::hint::black_box;
use std::time::{Duration, Instant};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::depth_persistence::{
    DEPTH_KIND_200, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthRow, DepthWriter,
    ensure_market_depth_table,
};
use tickvault_storage::tick_persistence::{TickWriter, ensure_ticks_table};

/// An IST trading day's midnight in IST-epoch seconds (same day the trading
/// harnesses use).
const DAY: u32 = 1_779_321_600;
/// 10:00:00 IST, inside the continuous session, so no row is refused by the
/// session-window gate.
const TEN: u32 = DAY + 36_000;
const IST_OFFSET_SECS: i64 = 19_800;

struct Dist {
    p50: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

fn dist(mut ns: Vec<u64>) -> Dist {
    ns.sort_unstable();
    let at = |q: f64| ns[((ns.len() as f64 - 1.0) * q) as usize];
    Dist {
        p50: at(0.50),
        p99: at(0.99),
        p999: at(0.999),
        max: ns[ns.len() - 1],
    }
}

fn report(stage: &str, d: &Dist) {
    eprintln!(
        "LATENCY | {stage:<46} | p50 {:>9} ns | p99 {:>9} ns | p99.9 {:>9} ns | max {:>10} ns",
        d.p50, d.p99, d.p999, d.max
    );
}

/// The i-th tick: a distinct (security_id, second) pair so no two rows share a
/// dedup key and every row is counted once.
fn tick(i: u64) -> ParsedTick {
    let secs = TEN + (i / 5_000) as u32;
    ParsedTick {
        security_id: 100_000 + (i % 5_000),
        exchange_segment_code: 2,
        last_traded_price: 100.0 + (i % 50) as f32 * 0.05,
        last_trade_quantity: 1,
        exchange_timestamp: secs,
        received_at_nanos: (i64::from(secs) - IST_OFFSET_SECS) * 1_000_000_000,
        volume: 1 + i as u32,
        ..ParsedTick::default()
    }
}

/// The i-th depth row of a depth-200 stream: 400 rows per update (200 per
/// side), each update one second later than the previous for the same
/// instrument.
fn depth_row(i: u64) -> DepthRow {
    let in_update = i % 400;
    let update = i / 400;
    let (side, level) = if in_update < 200 {
        (DEPTH_SIDE_BID, in_update as i64 + 1)
    } else {
        (DEPTH_SIDE_ASK, in_update as i64 - 199)
    };
    let secs = i64::from(TEN) + (update / 50) as i64;
    DepthRow {
        security_id: 200_000 + (update % 50) as i64,
        segment: "NSE_FNO",
        depth_kind: DEPTH_KIND_200,
        side,
        level,
        price: 25_000.5 + level as f64,
        quantity: 100 * level,
        orders: level,
        capture_seq: i as i64 + 1,
        ts_nanos: secs * 1_000_000_000,
    }
}

#[test]
#[ignore = "wall-clock harness; run with --release -- --ignored --nocapture"]
fn ilp_append_cost_per_row() {
    const ROUNDS: u64 = 50;
    const ROWS: u64 = 10_000;

    let mut ns = Vec::with_capacity((ROUNDS * ROWS) as usize);
    for round in 0..ROUNDS {
        // A fresh writer per round so the ILP buffer does not grow without
        // bound; never flushed (a writer with no sender would spill to disk).
        let mut w = TickWriter::for_test(Feed::Dhan);
        for i in 0..ROWS {
            let t = tick(round * ROWS + i);
            let start = Instant::now();
            let r = w.append_tick_with_seq(black_box(&t), (round * ROWS + i + 1) as i64);
            ns.push(start.elapsed().as_nanos() as u64);
            black_box(r.ok());
        }
        assert_eq!(w.pending(), ROWS as usize, "every tick must be buffered");
    }
    report("tick row append into the ILP buffer", &dist(ns));

    let mut ns = Vec::with_capacity((ROUNDS * ROWS) as usize);
    for round in 0..ROUNDS {
        let mut w = DepthWriter::for_test(Feed::Dhan);
        for i in 0..ROWS {
            let r = depth_row(round * ROWS + i);
            let start = Instant::now();
            let out = w.append_row(black_box(&r));
            ns.push(start.elapsed().as_nanos() as u64);
            black_box(out.ok());
        }
        assert_eq!(
            w.pending(),
            ROWS as usize,
            "every depth row must be buffered"
        );
    }
    report("depth row append into the ILP buffer", &dist(ns));
}

fn qdb_config() -> Option<QuestDbConfig> {
    let addr = std::env::var("TV_QDB_HTTP").ok()?;
    let (host, port) = addr.rsplit_once(':')?;
    Some(QuestDbConfig {
        host: host.to_string(),
        http_port: port.parse().ok()?,
        pg_port: 8812,
        ilp_port: 9009,
    })
}

async fn count(client: &reqwest::Client, cfg: &QuestDbConfig, table: &str) -> Option<u64> {
    let url = format!("http://{}:{}/exec", cfg.host, cfg.http_port);
    let body: serde_json::Value = client
        .get(url)
        .query(&[("query", format!("select count() from {table}"))])
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    body.get("dataset")?.get(0)?.get(0)?.as_u64()
}

/// Polls until `table` holds at least `want` rows; returns the time it took,
/// or `None` after 60 s.
async fn wait_visible(
    client: &reqwest::Client,
    cfg: &QuestDbConfig,
    table: &str,
    want: u64,
) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        if count(client, cfg, table).await.is_some_and(|n| n >= want) {
            return Some(start.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a running QuestDB (TV_QDB_HTTP=host:port); wall-clock harness"]
async fn ilp_flush_and_visibility_latency_live_questdb() {
    let Some(cfg) = qdb_config() else {
        eprintln!("LATENCY | skipped: set TV_QDB_HTTP=host:port to a running QuestDB");
        return;
    };
    assert!(
        ensure_ticks_table(&cfg).await,
        "ticks table must be created"
    );
    assert!(
        ensure_market_depth_table(&cfg).await,
        "market_depth table must be created"
    );
    let client = reqwest::Client::new();
    let ticks_before = count(&client, &cfg, "ticks").await.unwrap_or(0);
    let depth_before = count(&client, &cfg, "market_depth").await.unwrap_or(0);

    // Ticks: 200 batches of 1,000 rows, each flushed synchronously.
    const BATCHES: u64 = 200;
    const TICK_BATCH: u64 = 1_000;
    let mut writer = TickWriter::new(&cfg, Feed::Dhan);
    let mut flush_ns = Vec::with_capacity(BATCHES as usize);
    let mut next: u64 = ticks_before * 7 + 1;
    for _ in 0..BATCHES {
        for _ in 0..TICK_BATCH {
            writer
                .append_tick_with_seq(&tick(next), next as i64)
                .expect("append"); // APPROVED: test-only
            next += 1;
        }
        let start = Instant::now();
        let r = tokio::task::block_in_place(|| writer.flush());
        flush_ns.push(start.elapsed().as_nanos() as u64);
        r.expect("tick flush must be acknowledged"); // APPROVED: test-only
    }
    report(
        "tick flush, 1,000 rows, until acknowledged",
        &dist(flush_ns),
    );
    let want = ticks_before + BATCHES * TICK_BATCH;
    match wait_visible(&client, &cfg, "ticks", want).await {
        Some(d) => eprintln!(
            "LATENCY | ticks: last acknowledgement -> all {} rows visible: {} ms",
            BATCHES * TICK_BATCH,
            d.as_millis()
        ),
        None => eprintln!("LATENCY | ticks: rows NOT all visible within 60 s"),
    }

    // Per-batch visibility: one batch at a time, acknowledgement -> visible.
    let mut vis_ns = Vec::with_capacity(50);
    let mut have = count(&client, &cfg, "ticks").await.unwrap_or(0);
    for _ in 0..50 {
        for _ in 0..TICK_BATCH {
            writer
                .append_tick_with_seq(&tick(next), next as i64)
                .expect("append"); // APPROVED: test-only
            next += 1;
        }
        let start = Instant::now();
        tokio::task::block_in_place(|| writer.flush()).expect("flush"); // APPROVED: test-only
        have += TICK_BATCH;
        if wait_visible(&client, &cfg, "ticks", have).await.is_some() {
            vis_ns.push(start.elapsed().as_nanos() as u64);
        }
    }
    if !vis_ns.is_empty() {
        report("tick batch: flush start -> rows visible", &dist(vis_ns));
    }

    // Depth: 100 batches of 10,000 rows (25 depth-200 updates each).
    const DEPTH_BATCH: u64 = 10_000;
    let mut dw = DepthWriter::new(&cfg, Feed::Dhan);
    let mut flush_ns = Vec::with_capacity(100);
    let mut next: u64 = depth_before * 3;
    for _ in 0..100 {
        for _ in 0..DEPTH_BATCH {
            dw.append_row(&depth_row(next)).expect("append"); // APPROVED: test-only
            next += 1;
        }
        let start = Instant::now();
        let r = tokio::task::block_in_place(|| dw.flush());
        flush_ns.push(start.elapsed().as_nanos() as u64);
        r.expect("depth flush must be acknowledged"); // APPROVED: test-only
    }
    report(
        "depth flush, 10,000 rows, until acknowledged",
        &dist(flush_ns),
    );
}
