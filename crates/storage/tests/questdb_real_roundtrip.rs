//! Rows written through the REAL writers into a REAL QuestDB, read back by
//! query (audit PROOF-1, 2026-10-11).
//!
//! # Why this exists
//!
//! Before this file every storage test ran against a mock HTTP server that
//! answered with canned text, and the two real-QuestDB harnesses were
//! `#[ignore]`d and run by no workflow. So a change to a table's DDL, a DEDUP
//! key, an `ALTER ... ADD COLUMN` self-heal step or an ILP column type passed
//! CI and failed only in production, at boot or by silently merging or
//! dropping rows.
//!
//! These tests run the boot DDL functions against QuestDB, write through
//! `TickWriter`, `DepthWriter` and `ShadowCandleWriter`, and then query:
//!
//! * the DDL creates every live table, is idempotent, and leaves DEDUP on;
//! * a replayed row (same key) collapses to one row;
//! * a second capture of the same second (`capture_seq`) is kept;
//! * the same `security_id` in another segment is a separate row (I-P1-11);
//! * a candle written twice keeps the later values (UPSERT);
//! * a price survives the f32 to f64 conversion as the decimal it was;
//! * rows survive a QuestDB restart, and the DDL re-runs cleanly on the
//!   populated tables (the restart test, two phases).
//!
//! # How it runs
//!
//! `TV_QDB_REAL_HTTP=host:port` points at QuestDB's HTTP port (9000). Unset or
//! empty, each test prints why and returns, so `cargo test` on a laptop with no
//! QuestDB stays green. CI's storage test lane starts a QuestDB container and
//! also sets `TV_QDB_REAL_REQUIRED=1`, which turns "unset or unreachable" into
//! a FAILURE: a lane that silently skipped every test here would be the
//! false-OK this file exists to remove.
//!
//! The restart test has two phases. With `TV_QDB_RESTART_PHASE` unset it
//! writes its rows; with `TV_QDB_RESTART_PHASE=after` (the CI step that runs
//! after `docker restart`) it checks them. Each test uses its own
//! `security_id` range, so the tests can run in parallel against one
//! database.
//!
//! Local run:
//!
//! ```text
//! docker run -d -p 9000:9000 questdb/questdb:9.3.5
//! TV_QDB_REAL_HTTP=127.0.0.1:9000 TV_QDB_REAL_REQUIRED=1 \
//!     cargo test -p tickvault-storage --test questdb_real_roundtrip
//! ```

use std::time::{Duration, Instant};

use serde_json::Value;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::depth_persistence::{
    DEPTH_KIND_20, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthRow, DepthWriter, ensure_market_depth_table,
};
use tickvault_storage::feed_aux_persistence::ensure_feed_aux_table;
use tickvault_storage::shadow_candle_writer::ShadowCandleWriter;
use tickvault_storage::shadow_persistence::ensure_shadow_candle_tables;
use tickvault_storage::shadow_seal_columns::ShadowSealRow;
use tickvault_storage::tick_persistence::{TickWriter, ensure_ticks_table};

/// An IST trading day's midnight in IST-epoch seconds.
const DAY: u32 = 1_779_321_600;
/// 10:00:00 IST, inside the continuous session, so no row is refused by a
/// session-window gate.
const TEN: u32 = DAY + 36_000;
const IST_OFFSET_SECS: i64 = 19_800;
const NANOS: i64 = 1_000_000_000;

/// Wire segment codes (`segment_code_to_str`).
const SEG_NSE_EQ: u8 = 1;
const SEG_NSE_FNO: u8 = 2;

/// How long a test waits for QuestDB to apply its WAL after a write.
const APPLY_DEADLINE: Duration = Duration::from_secs(90);

fn required() -> bool {
    std::env::var("TV_QDB_REAL_REQUIRED").is_ok_and(|v| v == "1")
}

/// The QuestDB to test against, or `None` (test returns) when none is set and
/// none is required.
fn qdb() -> Option<QuestDbConfig> {
    let Some(addr) = std::env::var("TV_QDB_REAL_HTTP")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        assert!(
            !required(),
            "TV_QDB_REAL_REQUIRED=1 but TV_QDB_REAL_HTTP is unset: this lane \
             would skip every real-QuestDB test and still pass"
        );
        eprintln!("questdb_real_roundtrip: skipped, set TV_QDB_REAL_HTTP=host:port");
        return None;
    };
    let (host, port) = addr
        .rsplit_once(':')
        .unwrap_or_else(|| panic!("TV_QDB_REAL_HTTP must be host:port, got {addr}"));
    Some(QuestDbConfig {
        host: host.to_string(),
        http_port: port
            .parse()
            .unwrap_or_else(|_| panic!("TV_QDB_REAL_HTTP port is not a number: {addr}")),
        pg_port: 8812,
        ilp_port: 9009,
    })
}

/// Runs one SQL statement through `/exec` and returns the JSON body. Panics
/// on a transport failure or a QuestDB error, naming the statement.
async fn exec(cfg: &QuestDbConfig, sql: &str) -> Value {
    let url = format!("http://{}:{}/exec", cfg.host, cfg.http_port);
    let resp = reqwest::Client::new()
        .get(url)
        .query(&[("query", sql)])
        .send()
        .await
        .unwrap_or_else(|e| panic!("QuestDB unreachable for `{sql}`: {e}"));
    let body: Value = resp
        .json()
        .await
        .unwrap_or_else(|e| panic!("QuestDB answer to `{sql}` is not JSON: {e}"));
    assert!(
        body.get("error").is_none(),
        "QuestDB refused `{sql}`: {body}"
    );
    body
}

/// The dataset rows of a query.
async fn rows(cfg: &QuestDbConfig, sql: &str) -> Vec<Value> {
    exec(cfg, sql)
        .await
        .get("dataset")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `SELECT count() ...` as a number.
async fn count(cfg: &QuestDbConfig, sql: &str) -> u64 {
    rows(cfg, sql)
        .await
        .first()
        .and_then(|r| r.get(0))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("`{sql}` returned no count"))
}

/// Waits until QuestDB has applied every committed WAL transaction of
/// `table` (`writerTxn == sequencerTxn`), so a count reads what was written.
async fn wait_applied(cfg: &QuestDbConfig, table: &str) {
    let sql = format!("SELECT writerTxn, sequencerTxn FROM wal_tables() WHERE name = '{table}'");
    let start = Instant::now();
    loop {
        let r = rows(cfg, &sql).await;
        if let Some(row) = r.first() {
            let writer = row.get(0).and_then(Value::as_i64);
            let sequencer = row.get(1).and_then(Value::as_i64);
            if writer.is_some() && writer == sequencer {
                return;
            }
        }
        assert!(
            start.elapsed() < APPLY_DEADLINE,
            "{table}: WAL not applied within {APPLY_DEADLINE:?} (last: {r:?})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn tick(security_id: u64, segment: u8, secs: u32, ltp: f32, volume: u32) -> ParsedTick {
    ParsedTick {
        security_id,
        exchange_segment_code: segment,
        last_traded_price: ltp,
        last_trade_quantity: 1,
        exchange_timestamp: secs,
        received_at_nanos: (i64::from(secs) - IST_OFFSET_SECS) * NANOS,
        volume,
        ..ParsedTick::default()
    }
}

fn depth_row(
    security_id: i64,
    segment: &'static str,
    side: &'static str,
    level: i64,
    capture_seq: i64,
) -> DepthRow {
    DepthRow {
        security_id,
        segment,
        depth_kind: DEPTH_KIND_20,
        side,
        level,
        price: 100.0 + level as f64 * 0.05,
        quantity: 10 * level,
        orders: level,
        capture_seq,
        ts_nanos: i64::from(TEN) * NANOS,
    }
}

fn candle(security_id: i64, segment: &'static str, close: f64, volume: i64) -> ShadowSealRow {
    ShadowSealRow {
        table_name: "candles_1m",
        timestamp_ist_nanos: i64::from(TEN) * NANOS,
        security_id,
        segment,
        feed: "dhan",
        open: 100.0,
        high: close.max(100.0),
        low: close.min(100.0),
        close,
        volume,
        oi: 0,
        tick_count: 3,
        close_pct_from_prev_day: 0.0,
        open_pct: 0.0,
        change_pct: 0.0,
        open_gap_pct: 0.0,
        total_buy_qty: 0,
        total_sell_qty: 0,
        open_latency_ns: None,
        close_latency_ns: None,
        window_span_latency_ns: None,
    }
}

/// Every boot ensure the live lane runs, in boot order.
async fn ensure_all(cfg: &QuestDbConfig) {
    assert!(ensure_shadow_candle_tables(cfg).await, "candle tables");
    assert!(ensure_ticks_table(cfg).await, "ticks");
    assert!(ensure_market_depth_table(cfg).await, "market_depth");
    assert!(ensure_feed_aux_table(cfg).await, "feed_aux");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ddl_creates_every_live_table_idempotently_with_dedup_on() {
    let Some(cfg) = qdb() else { return };
    // Twice: the second run is the boot of a process whose tables exist.
    ensure_all(&cfg).await;
    ensure_all(&cfg).await;

    for table in ["ticks", "market_depth", "candles_1m", "candles_5m"] {
        let r = rows(
            &cfg,
            &format!("SELECT dedup FROM tables() WHERE table_name = '{table}'"),
        )
        .await;
        let dedup = r
            .first()
            .and_then(|row| row.get(0))
            .and_then(Value::as_bool);
        assert_eq!(
            dedup,
            Some(true),
            "{table}: missing, or DEDUP is off after the boot ensure ({r:?})"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticks_keep_distinct_captures_and_segments_and_collapse_a_replay() {
    let Some(cfg) = qdb() else { return };
    assert!(ensure_ticks_table(&cfg).await, "ticks");
    const SID: u64 = 910_001;

    let mut w = TickWriter::new(&cfg, Feed::Dhan);
    let a = tick(SID, SEG_NSE_FNO, TEN, 21_004.95, 10);
    w.append_tick_with_seq(&a, 1).expect("append"); // APPROVED: test-only
    // The same packet replayed from the WAL: same capture_seq, same key.
    w.append_tick_with_seq(&a, 1).expect("append"); // APPROVED: test-only
    // A second, byte-identical arrival in the same second: a new capture.
    w.append_tick_with_seq(&a, 2).expect("append"); // APPROVED: test-only
    // The same id in another segment is another instrument.
    let b = tick(SID, SEG_NSE_EQ, TEN, 512.30, 7);
    w.append_tick_with_seq(&b, 1).expect("append"); // APPROVED: test-only
    tokio::task::block_in_place(|| w.flush()).expect("tick flush acknowledged"); // APPROVED: test-only
    wait_applied(&cfg, "ticks").await;

    let sid_filter = format!("security_id = {SID}");
    assert_eq!(
        count(
            &cfg,
            &format!("SELECT count() FROM ticks WHERE {sid_filter}")
        )
        .await,
        3,
        "4 appends = 1 replay (collapsed) + 2 captures + 1 other segment"
    );
    assert_eq!(
        count(
            &cfg,
            &format!("SELECT count() FROM ticks WHERE {sid_filter} AND segment = 'NSE_FNO'")
        )
        .await,
        2
    );
    assert_eq!(
        count(
            &cfg,
            &format!("SELECT count() FROM ticks WHERE {sid_filter} AND segment = 'NSE_EQ'")
        )
        .await,
        1
    );
    // The designated timestamp is the exchange time as sent, no IST shift,
    // and the price reads back as the decimal Dhan sent (f32_to_f64_clean).
    let r = rows(
        &cfg,
        &format!(
            "SELECT ltp, feed, ts FROM ticks WHERE {sid_filter} AND segment = 'NSE_FNO' LIMIT 1"
        ),
    )
    .await;
    let row = r.first().expect("an NSE_FNO tick row"); // APPROVED: test-only
    assert_eq!(row.get(0).and_then(Value::as_f64), Some(21_004.95));
    assert_eq!(row.get(1).and_then(Value::as_str), Some("dhan"));
    let ts = row.get(2).and_then(Value::as_str).unwrap_or_default();
    assert!(
        ts.starts_with("2026-05-21T10:00:00"),
        "ts must be the exchange time verbatim (IST epoch), never shifted: {ts}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn depth_keeps_every_level_side_and_segment_and_collapses_a_replay() {
    let Some(cfg) = qdb() else { return };
    assert!(ensure_market_depth_table(&cfg).await, "market_depth");
    const SID: i64 = 920_001;

    let mut w = DepthWriter::new(&cfg, Feed::Dhan);
    for _pass in 0..2 {
        // Pass 1 replays pass 0 exactly: every row must collapse.
        for level in 1..=5 {
            for side in [DEPTH_SIDE_BID, DEPTH_SIDE_ASK] {
                w.append_row(&depth_row(SID, "NSE_FNO", side, level, 7))
                    .expect("append"); // APPROVED: test-only
            }
        }
    }
    // Same id, other segment: separate rows.
    for level in 1..=5 {
        w.append_row(&depth_row(SID, "NSE_EQ", DEPTH_SIDE_BID, level, 7))
            .expect("append"); // APPROVED: test-only
    }
    tokio::task::block_in_place(|| w.flush()).expect("depth flush acknowledged"); // APPROVED: test-only
    wait_applied(&cfg, "market_depth").await;

    let sid_filter = format!("security_id = {SID}");
    assert_eq!(
        count(
            &cfg,
            &format!("SELECT count() FROM market_depth WHERE {sid_filter} AND segment = 'NSE_FNO'")
        )
        .await,
        10,
        "5 levels x 2 sides, the replay collapsed"
    );
    assert_eq!(
        count(
            &cfg,
            &format!("SELECT count() FROM market_depth WHERE {sid_filter} AND segment = 'NSE_EQ'")
        )
        .await,
        5
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_candle_written_twice_keeps_the_later_values_per_segment() {
    let Some(cfg) = qdb() else { return };
    // The candle writer refuses to flush until the tables are keyed in this
    // process, exactly as at boot.
    assert!(ensure_shadow_candle_tables(&cfg).await, "candle tables");
    const SID: i64 = 930_001;

    let mut w = ShadowCandleWriter::new(&cfg).expect("candle writer"); // APPROVED: test-only
    w.append_row(&candle(SID, "NSE_FNO", 101.0, 10))
        .expect("append"); // APPROVED: test-only
    w.append_row(&candle(SID, "NSE_EQ", 99.0, 4))
        .expect("append"); // APPROVED: test-only
    tokio::task::block_in_place(|| w.flush()).expect("candle flush acknowledged"); // APPROVED: test-only
    // An amended bar for the same minute (a late tick): the same key.
    w.append_row(&candle(SID, "NSE_FNO", 102.5, 25))
        .expect("append"); // APPROVED: test-only
    tokio::task::block_in_place(|| w.flush()).expect("candle flush acknowledged"); // APPROVED: test-only
    wait_applied(&cfg, "candles_1m").await;

    let r = rows(
        &cfg,
        &format!(
            "SELECT segment, close, volume FROM candles_1m WHERE security_id = {SID} \
             ORDER BY segment"
        ),
    )
    .await;
    assert_eq!(
        r.len(),
        2,
        "one row per segment, the amendment upserted: {r:?}"
    );
    let eq = &r[0];
    let fno = &r[1];
    assert_eq!(eq.get(0).and_then(Value::as_str), Some("NSE_EQ"));
    assert_eq!(eq.get(2).and_then(Value::as_i64), Some(4));
    assert_eq!(fno.get(0).and_then(Value::as_str), Some("NSE_FNO"));
    assert_eq!(fno.get(1).and_then(Value::as_f64), Some(102.5));
    assert_eq!(
        fno.get(2).and_then(Value::as_i64),
        Some(25),
        "the later write must win (DEDUP UPSERT), not the first"
    );
}

/// Phase 1 (default) writes; phase 2 (`TV_QDB_RESTART_PHASE=after`, run by CI
/// after `docker restart`) proves the rows survived and the DDL re-runs on
/// populated tables.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_survive_a_questdb_restart() {
    let Some(cfg) = qdb() else { return };
    const SID: u64 = 940_001;
    let sid_filter = format!("security_id = {SID}");
    let after = std::env::var("TV_QDB_RESTART_PHASE").is_ok_and(|v| v == "after");

    if !after {
        assert!(ensure_ticks_table(&cfg).await, "ticks");
        assert!(ensure_shadow_candle_tables(&cfg).await, "candle tables");
        let mut tw = TickWriter::new(&cfg, Feed::Dhan);
        for seq in 1..=3 {
            tw.append_tick_with_seq(&tick(SID, SEG_NSE_FNO, TEN, 250.25, seq), i64::from(seq))
                .expect("append"); // APPROVED: test-only
        }
        tokio::task::block_in_place(|| tw.flush()).expect("tick flush"); // APPROVED: test-only
        let mut cw = ShadowCandleWriter::new(&cfg).expect("candle writer"); // APPROVED: test-only
        cw.append_row(&candle(SID as i64, "NSE_FNO", 250.25, 3))
            .expect("append"); // APPROVED: test-only
        tokio::task::block_in_place(|| cw.flush()).expect("candle flush"); // APPROVED: test-only
        wait_applied(&cfg, "ticks").await;
        wait_applied(&cfg, "candles_1m").await;
        assert_eq!(
            count(
                &cfg,
                &format!("SELECT count() FROM ticks WHERE {sid_filter}")
            )
            .await,
            3
        );
        return;
    }

    // After the restart: the boot DDL must succeed on populated tables
    // (ADD COLUMN IF NOT EXISTS, DEDUP ENABLE), and never drop a row.
    ensure_all(&cfg).await;
    assert_eq!(
        count(
            &cfg,
            &format!("SELECT count() FROM ticks WHERE {sid_filter}")
        )
        .await,
        3,
        "ticks written before the restart are gone or duplicated"
    );
    assert_eq!(
        count(
            &cfg,
            &format!("SELECT count() FROM candles_1m WHERE {sid_filter}")
        )
        .await,
        1,
        "the candle written before the restart is gone or duplicated"
    );
    // And the restarted database takes new rows.
    let mut tw = TickWriter::new(&cfg, Feed::Dhan);
    tw.append_tick_with_seq(&tick(SID, SEG_NSE_FNO, TEN + 1, 250.50, 4), 4)
        .expect("append"); // APPROVED: test-only
    tokio::task::block_in_place(|| tw.flush()).expect("tick flush after restart"); // APPROVED: test-only
    wait_applied(&cfg, "ticks").await;
    assert_eq!(
        count(
            &cfg,
            &format!("SELECT count() FROM ticks WHERE {sid_filter}")
        )
        .await,
        4
    );
}
