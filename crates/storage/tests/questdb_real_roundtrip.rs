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
//!   populated tables (the restart test, two phases);
//! * every tick round trip runs through BOTH tick encoders: the library one
//!   and our own (`TickIlpEncoder::Own`), which production runs
//!   (`config/production.toml` `own_ilp_encoder = true`, audit 2026-11-07);
//!   so does every candle round trip, since the same switch picks the candle
//!   writer's encoder;
//! * ticks also round-trip through the OFFLOADED writer (`split_for_offload`,
//!   producer, bounded queue, writer-thread sink), the shape the live lane
//!   runs, with both encoders;
//! * a row the server refuses (wrong column type) is classified the same by
//!   both senders, keeps its bytes for the rescue, and does not poison the
//!   connection for the next good row.
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
use tickvault_storage::tick_ilp::{
    IlpLine, RawIlpSender, TickIlpEncoder, TickIlpError, TickIlpSender, TickRowBuffer,
};
use tickvault_storage::tick_persistence::{TickWriter, ensure_ticks_table};

/// Both tick encoders, each with the offset its rows add to a test's
/// `security_id`, so the two write disjoint rows into one database.
/// Production runs `Own` (`config/production.toml` `own_ilp_encoder = true`),
/// so it is never left out of a tick round trip (audit 2026-11-07).
const ENCODERS: [(TickIlpEncoder, u64); 2] =
    [(TickIlpEncoder::Library, 0), (TickIlpEncoder::Own, 1)];

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
    for (encoder, offset) in ENCODERS {
        let sid = 910_001 + offset;
        tick_captures_and_segments_round_trip(&cfg, encoder, sid).await;
        // Same decimal back through either encoder.
        let r = rows(
            &cfg,
            &format!("SELECT ltp FROM ticks WHERE security_id = {sid} AND segment = 'NSE_EQ'"),
        )
        .await;
        assert_eq!(
            r.first().and_then(|row| row.get(0)).and_then(Value::as_f64),
            Some(512.30),
            "{encoder:?}: the NSE_EQ price did not read back as sent"
        );
    }
}

/// One encoder's half of
/// `ticks_keep_distinct_captures_and_segments_and_collapse_a_replay`.
async fn tick_captures_and_segments_round_trip(
    cfg: &QuestDbConfig,
    encoder: TickIlpEncoder,
    sid: u64,
) {
    let mut w = TickWriter::new_with_encoder(cfg, Feed::Dhan, encoder);
    let a = tick(sid, SEG_NSE_FNO, TEN, 21_004.95, 10);
    w.append_tick_with_seq(&a, 1).expect("append"); // APPROVED: test-only
    // The same packet replayed from the WAL: same capture_seq, same key.
    w.append_tick_with_seq(&a, 1).expect("append"); // APPROVED: test-only
    // A second, byte-identical arrival in the same second: a new capture.
    w.append_tick_with_seq(&a, 2).expect("append"); // APPROVED: test-only
    // The same id in another segment is another instrument.
    let b = tick(sid, SEG_NSE_EQ, TEN, 512.30, 7);
    w.append_tick_with_seq(&b, 1).expect("append"); // APPROVED: test-only
    tokio::task::block_in_place(|| w.flush()).expect("tick flush acknowledged"); // APPROVED: test-only
    wait_applied(cfg, "ticks").await;

    let sid_filter = format!("security_id = {sid}");
    assert_eq!(
        count(
            cfg,
            &format!("SELECT count() FROM ticks WHERE {sid_filter}")
        )
        .await,
        3,
        "{encoder:?}: 4 appends = 1 replay (collapsed) + 2 captures + 1 other segment"
    );
    assert_eq!(
        count(
            cfg,
            &format!("SELECT count() FROM ticks WHERE {sid_filter} AND segment = 'NSE_FNO'")
        )
        .await,
        2
    );
    assert_eq!(
        count(
            cfg,
            &format!("SELECT count() FROM ticks WHERE {sid_filter} AND segment = 'NSE_EQ'")
        )
        .await,
        1
    );
    // The designated timestamp is the exchange time as sent, no IST shift,
    // and the price reads back as the decimal Dhan sent (f32_to_f64_clean).
    let r = rows(
        cfg,
        &format!(
            "SELECT ltp, feed, ts FROM ticks WHERE {sid_filter} AND segment = 'NSE_FNO' LIMIT 1"
        ),
    )
    .await;
    let row = r.first().expect("an NSE_FNO tick row"); // APPROVED: test-only
    assert_eq!(
        row.get(0).and_then(Value::as_f64),
        Some(21_004.95),
        "{encoder:?}"
    );
    assert_eq!(
        row.get(1).and_then(Value::as_str),
        Some("dhan"),
        "{encoder:?}"
    );
    let ts = row.get(2).and_then(Value::as_str).unwrap_or_default();
    assert!(
        ts.starts_with("2026-05-21T10:00:00"),
        "{encoder:?}: ts must be the exchange time verbatim (IST epoch), never shifted: {ts}"
    );
}

/// Ticks through the OFFLOADED writer, the shape the live lane runs: the
/// producer stages rows and hands each flush to a writer thread over the
/// bounded queue, and that thread's sink makes the HTTP round trip. Both
/// encoders. Every row the sink reports landed is read back, a replayed row
/// collapses, and nothing stays pending or queued (audit 2026-11-07: the
/// real-QuestDB tick tests used only the synchronous flush).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offloaded_ticks_land_through_the_writer_thread_with_both_encoders() {
    let Some(cfg) = qdb() else { return };
    assert!(ensure_ticks_table(&cfg).await, "ticks");
    const ROWS: u32 = 40;
    for (encoder, offset) in ENCODERS {
        let sid = 915_001 + offset;
        let writer = TickWriter::new_with_encoder(&cfg, Feed::Dhan, encoder);
        let (mut producer, sink, queue) = writer.split_for_offload();
        // The writer thread, as the live lane runs it: write every batch
        // until the producer closes the queue.
        let writer_thread = std::thread::spawn(move || {
            let mut sink = sink;
            let mut landed = 0_usize;
            let mut batches = 0_usize;
            for mut batch in queue {
                landed += sink.write(&mut batch);
                batches += 1;
            }
            (landed, batches)
        });
        for seq in 1..=ROWS {
            let t = tick(sid, SEG_NSE_FNO, TEN, 300.05, seq);
            producer
                .append_tick_with_seq(&t, i64::from(seq))
                .expect("append"); // APPROVED: test-only
            if seq % 20 == 0 {
                producer.flush().expect("hand-off to the writer thread"); // APPROVED: test-only
            }
        }
        // A WAL replay of the first row: same capture_seq, same key.
        producer
            .append_tick_with_seq(&tick(sid, SEG_NSE_FNO, TEN, 300.05, 1), 1)
            .expect("append"); // APPROVED: test-only
        producer.flush().expect("hand-off to the writer thread"); // APPROVED: test-only
        assert_eq!(
            producer.pending(),
            0,
            "{encoder:?}: every row left the producer"
        );
        producer.close_offload();
        let (landed, batches) =
            tokio::task::block_in_place(|| writer_thread.join()).expect("writer thread"); // APPROVED: test-only
        assert_eq!(
            landed,
            ROWS as usize + 1,
            "{encoder:?}: the sink must report every handed-off row landed"
        );
        assert_eq!(batches, 3, "{encoder:?}: one batch per hand-off");
        wait_applied(&cfg, "ticks").await;
        assert_eq!(
            tick_count(&cfg, sid).await,
            u64::from(ROWS),
            "{encoder:?}: {ROWS} captures land, the replayed row collapses"
        );
        let r = rows(
            &cfg,
            &format!("SELECT ltp, feed FROM ticks WHERE security_id = {sid} LIMIT 1"),
        )
        .await;
        let row = r.first().expect("an offloaded tick row"); // APPROVED: test-only
        assert_eq!(
            row.get(0).and_then(Value::as_f64),
            Some(300.05),
            "{encoder:?}"
        );
        assert_eq!(
            row.get(1).and_then(Value::as_str),
            Some("dhan"),
            "{encoder:?}"
        );
    }
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
    // Both encoders, each on its own `security_id`: production runs the
    // candle writer with our own encoder (`[tick_storage] own_ilp_encoder =
    // true`, `config/production.toml`), which this test did not exercise
    // until 2026-11-07.
    for (encoder, offset) in ENCODERS {
        let sid = 930_001 + offset as i64;
        candle_upsert_round_trip(&cfg, encoder, sid).await;
    }
}

/// One encoder's half of
/// `a_candle_written_twice_keeps_the_later_values_per_segment`.
async fn candle_upsert_round_trip(cfg: &QuestDbConfig, encoder: TickIlpEncoder, sid: i64) {
    let mut w = ShadowCandleWriter::new_with_encoder(cfg, encoder).expect("candle writer"); // APPROVED: test-only
    assert!(
        w.is_connected(),
        "{encoder:?}: the production constructor builds a real sender"
    );
    w.append_row(&candle(sid, "NSE_FNO", 101.0, 10))
        .expect("append"); // APPROVED: test-only
    w.append_row(&candle(sid, "NSE_EQ", 99.0, 4))
        .expect("append"); // APPROVED: test-only
    tokio::task::block_in_place(|| w.flush())
        .unwrap_or_else(|e| panic!("{encoder:?}: candle flush not acknowledged: {e}"));
    // An amended bar for the same minute (a late tick): the same key.
    w.append_row(&candle(sid, "NSE_FNO", 102.5, 25))
        .expect("append"); // APPROVED: test-only
    tokio::task::block_in_place(|| w.flush())
        .unwrap_or_else(|e| panic!("{encoder:?}: candle flush not acknowledged: {e}"));
    wait_applied(cfg, "candles_1m").await;

    let r = rows(
        cfg,
        &format!(
            "SELECT segment, close, volume FROM candles_1m WHERE security_id = {sid} \
             ORDER BY segment"
        ),
    )
    .await;
    assert_eq!(
        r.len(),
        2,
        "{encoder:?}: one row per segment, the amendment upserted: {r:?}"
    );
    let eq = &r[0];
    let fno = &r[1];
    assert_eq!(
        eq.get(0).and_then(Value::as_str),
        Some("NSE_EQ"),
        "{encoder:?}"
    );
    assert_eq!(eq.get(1).and_then(Value::as_f64), Some(99.0), "{encoder:?}");
    assert_eq!(eq.get(2).and_then(Value::as_i64), Some(4), "{encoder:?}");
    assert_eq!(
        fno.get(0).and_then(Value::as_str),
        Some("NSE_FNO"),
        "{encoder:?}"
    );
    assert_eq!(
        fno.get(1).and_then(Value::as_f64),
        Some(102.5),
        "{encoder:?}"
    );
    assert_eq!(
        fno.get(2).and_then(Value::as_i64),
        Some(25),
        "{encoder:?}: the later write must win (DEDUP UPSERT), not the first"
    );
}

/// Phase 1 (default) writes; phase 2 (`TV_QDB_RESTART_PHASE=after`, run by CI
/// after `docker restart`) proves the rows survived and the DDL re-runs on
/// populated tables. Ticks go through both encoders, each on its own
/// `security_id`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_survive_a_questdb_restart() {
    let Some(cfg) = qdb() else { return };
    const SID: u64 = 940_001;
    let after = std::env::var("TV_QDB_RESTART_PHASE").is_ok_and(|v| v == "after");

    if !after {
        assert!(ensure_ticks_table(&cfg).await, "ticks");
        assert!(ensure_shadow_candle_tables(&cfg).await, "candle tables");
        for (encoder, offset) in ENCODERS {
            let mut tw = TickWriter::new_with_encoder(&cfg, Feed::Dhan, encoder);
            for seq in 1..=3 {
                let t = tick(SID + offset, SEG_NSE_FNO, TEN, 250.25, seq);
                tw.append_tick_with_seq(&t, i64::from(seq)).expect("append"); // APPROVED: test-only
            }
            tokio::task::block_in_place(|| tw.flush()).expect("tick flush"); // APPROVED: test-only
        }
        for (encoder, offset) in ENCODERS {
            let mut cw =
                ShadowCandleWriter::new_with_encoder(&cfg, encoder).expect("candle writer"); // APPROVED: test-only
            cw.append_row(&candle((SID + offset) as i64, "NSE_FNO", 250.25, 3))
                .expect("append"); // APPROVED: test-only
            tokio::task::block_in_place(|| cw.flush())
                .unwrap_or_else(|e| panic!("{encoder:?}: candle flush: {e}"));
        }
        wait_applied(&cfg, "ticks").await;
        wait_applied(&cfg, "candles_1m").await;
        for (encoder, offset) in ENCODERS {
            assert_eq!(tick_count(&cfg, SID + offset).await, 3, "{encoder:?}");
        }
        return;
    }

    // After the restart: the boot DDL must succeed on populated tables
    // (ADD COLUMN IF NOT EXISTS, DEDUP ENABLE), and never drop a row.
    ensure_all(&cfg).await;
    for (encoder, offset) in ENCODERS {
        assert_eq!(
            tick_count(&cfg, SID + offset).await,
            3,
            "{encoder:?}: ticks written before the restart are gone or duplicated"
        );
    }
    for (encoder, offset) in ENCODERS {
        assert_eq!(
            count(
                &cfg,
                &format!(
                    "SELECT count() FROM candles_1m WHERE security_id = {}",
                    SID + offset
                )
            )
            .await,
            1,
            "{encoder:?}: the candle written before the restart is gone or duplicated"
        );
    }
    // And the restarted database takes new rows, through either encoder.
    for (encoder, offset) in ENCODERS {
        let mut tw = TickWriter::new_with_encoder(&cfg, Feed::Dhan, encoder);
        tw.append_tick_with_seq(&tick(SID + offset, SEG_NSE_FNO, TEN + 1, 250.50, 4), 4)
            .expect("append"); // APPROVED: test-only
        tokio::task::block_in_place(|| tw.flush()).expect("tick flush after restart"); // APPROVED: test-only
    }
    wait_applied(&cfg, "ticks").await;
    for (encoder, offset) in ENCODERS {
        assert_eq!(tick_count(&cfg, SID + offset).await, 4, "{encoder:?}");
    }
}

/// `ticks` rows of one `security_id`, every segment.
async fn tick_count(cfg: &QuestDbConfig, sid: u64) -> u64 {
    count(
        cfg,
        &format!("SELECT count() FROM ticks WHERE security_id = {sid}"),
    )
    .await
}

/// A scratch table for the refusal test, one per encoder, so the two never
/// race. `v` is a LONG: a STRING value for it is a cast error the server
/// refuses (HTTP 400), the class a schema drift between a writer and its
/// table produces.
fn refusal_table(encoder: TickIlpEncoder) -> &'static str {
    match encoder {
        TickIlpEncoder::Library => "tv_qdb_rt_refusal_library",
        TickIlpEncoder::Own => "tv_qdb_rt_refusal_own",
    }
}

/// The tick writer's sender for `encoder`, built the way
/// `TickWriter::new_with_encoder` builds it (same conf string as
/// `ticks_ilp_http_conf`).
fn tick_sender(cfg: &QuestDbConfig, encoder: TickIlpEncoder) -> TickIlpSender {
    match encoder {
        TickIlpEncoder::Library => TickIlpSender::Library(
            questdb::ingress::Sender::from_conf(format!(
                "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
                cfg.host, cfg.http_port
            ))
            .expect("library sender"), // APPROVED: test-only
        ),
        TickIlpEncoder::Own => TickIlpSender::Own(
            RawIlpSender::new(&cfg.host, cfg.http_port).expect("own sender"), // APPROVED: test-only
        ),
    }
}

/// One row into `table` at 10:00 IST: `v` as a string (refused by a LONG
/// column) or as a long (accepted).
fn refusal_row(buf: &mut TickRowBuffer, table: &'static str, as_string: bool) {
    buf.table(table).expect("table"); // APPROVED: test-only
    if as_string {
        buf.column_str("v", "not-a-number").expect("column"); // APPROVED: test-only
    } else {
        buf.column_i64("v", 7).expect("column"); // APPROVED: test-only
    }
    buf.at_nanos(i64::from(TEN) * NANOS).expect("at"); // APPROVED: test-only
}

/// The server's refusal text with the per-request `id: …` part removed:
/// QuestDB stamps a fresh error id on every answer, so only the rest can be
/// compared across two requests.
fn without_error_id(message: &str) -> String {
    // QuestDB's JSON body carries it as `"errorId":"…"` (measured in CI on
    // 2026-11-07: `"errorId":"aaa2042e54ef-3"` then `…-4` on the next
    // request); drop that value and keep the key.
    const JSON_KEY: &str = "\"errorId\":\"";
    if let Some(at) = message.rfind(JSON_KEY) {
        let start = at + JSON_KEY.len();
        let end = message[start..]
            .find('"')
            .map_or(message.len(), |rel| start + rel);
        return format!("{}{}", &message[..start], &message[end..]);
    }
    // The bracketed suffix is `[id: …, code: …, line: …]`; anchor on the
    // bracket so an `id: ` inside the server's own text is never cut.
    let Some(bracket) = message.rfind("[id: ") else {
        return message.to_string();
    };
    let start = bracket + 1;
    let tail = &message[start..];
    let end = tail.find([',', ']']).unwrap_or(tail.len());
    let rest = tail[end..].trim_start_matches(',').trim_start();
    format!("{}{rest}", &message[..start])
}

#[test]
fn without_error_id_drops_only_the_per_request_id() {
    assert_eq!(
        without_error_id("Could not flush buffer: bad [id: 4f-2a, code: invalid, line: 1]"),
        "Could not flush buffer: bad [code: invalid, line: 1]"
    );
    assert_eq!(
        without_error_id("Could not flush buffer: bad [id: 4f-2a]"),
        "Could not flush buffer: bad []"
    );
    // No bracketed id: unchanged, even with an `id: ` in the server text.
    assert_eq!(
        without_error_id("column security_id: x"),
        "column security_id: x"
    );
    // Regression: 2026-11-07 — the real server's JSON body carries a fresh
    // `"errorId"` per request, so two refusals of the same row differed only
    // there and the encoder comparison failed in CI.
    let body = |id: &str| {
        format!(
            "Could not flush buffer: {{\"code\":\"invalid\",\"message\":\"cast error\",\
             \"line\":1,\"errorId\":\"{id}\"}}"
        )
    };
    assert_eq!(
        without_error_id(&body("aaa2042e54ef-3")),
        without_error_id(&body("aaa2042e54ef-4"))
    );
    assert_eq!(
        without_error_id(&body("aaa2042e54ef-3")),
        "Could not flush buffer: {\"code\":\"invalid\",\"message\":\"cast error\",\
         \"line\":1,\"errorId\":\"\"}"
    );
}

/// What one encoder's refused send looked like: whether it may be retried,
/// the HTTP status where the error carries one, and the text without its id.
#[derive(Debug, PartialEq)]
struct Refusal {
    retryable: bool,
    status: Option<u16>,
    message: String,
}

fn refusal_of(err: &TickIlpError, table: &str) -> Refusal {
    let status = match err {
        TickIlpError::Send(e) => e.status,
        TickIlpError::Library(e) => {
            // The library carries no status; a server refusal is its
            // ServerFlushError class, the one our sender maps 4xx to.
            assert_eq!(
                e.code(),
                questdb::ErrorCode::ServerFlushError,
                "library refusal is not a server flush error: {e}"
            );
            None
        }
        TickIlpError::Encoder(why) => panic!("encoder refused before the send: {why}"),
    };
    Refusal {
        retryable: err.is_retryable(),
        status,
        // Each encoder writes its own table, which the server names.
        message: without_error_id(&err.to_string()).replace(table, "<table>"),
    }
}

/// A row the server refuses (wrong column type) is classified the same by
/// both encoders' senders (not retryable, same server text), the failed send
/// keeps every byte in the buffer for the writer's rescue, nothing lands, and
/// the same sender then delivers a good row on the connection the refusal
/// used (audit 2026-11-07: production runs the own encoder, which no real
/// QuestDB test exercised).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_row_is_classified_alike_and_its_bytes_are_kept_by_both_encoders() {
    let Some(cfg) = qdb() else { return };
    let mut seen: Vec<(TickIlpEncoder, Refusal, Vec<u8>)> = Vec::new();
    for (encoder, _) in ENCODERS {
        let table = refusal_table(encoder);
        exec(
            &cfg,
            &format!(
                "CREATE TABLE IF NOT EXISTS {table} (v LONG, ts TIMESTAMP) \
                 TIMESTAMP(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS(ts)"
            ),
        )
        .await;
        let mut sender = tick_sender(&cfg, encoder);
        let mut buf = TickRowBuffer::new(encoder);
        refusal_row(&mut buf, table, true);
        let before = buf.as_bytes().to_vec();
        let err = tokio::task::block_in_place(|| sender.flush(&mut buf))
            .expect_err("a STRING for a LONG column must be refused"); // APPROVED: test-only
        let refusal = refusal_of(&err, table);
        assert!(
            !refusal.retryable,
            "{encoder:?}: a refusal is not retryable: {err}"
        );
        assert_eq!(
            buf.as_bytes(),
            before.as_slice(),
            "{encoder:?}: the failed send must leave every byte for the rescue"
        );
        assert_eq!(
            buf.row_count(),
            1,
            "{encoder:?}: the refused row is still held"
        );

        // The same sender, after the refusal: a good row lands.
        buf.clear();
        refusal_row(&mut buf, table, false);
        tokio::task::block_in_place(|| sender.flush(&mut buf))
            .unwrap_or_else(|e| panic!("{encoder:?}: good row after a refusal: {e}"));
        assert!(buf.is_empty(), "{encoder:?}: a delivered buffer is cleared");
        wait_applied(&cfg, table).await;
        let landed = rows(&cfg, &format!("SELECT v FROM {table}")).await;
        assert_eq!(
            landed.len(),
            1,
            "{encoder:?}: only the good row may land: {landed:?}"
        );
        assert_eq!(
            landed[0].get(0).and_then(Value::as_i64),
            Some(7),
            "{encoder:?}"
        );
        seen.push((encoder, refusal, before));
    }
    let [(_, lib, lib_bytes), (_, own, own_bytes)] = seen.as_slice() else {
        panic!("expected one refusal per encoder: {seen:?}");
    };
    assert_eq!(
        own.status,
        Some(400),
        "the own sender must read a refused row as HTTP 400: {own:?}"
    );
    assert_eq!(
        lib.message, own.message,
        "both encoders must report the server's refusal in the same words"
    );
    assert_eq!(lib.retryable, own.retryable);
    // Same row, same bytes, apart from the table name.
    assert_eq!(
        String::from_utf8_lossy(lib_bytes).replace("library", "X"),
        String::from_utf8_lossy(own_bytes).replace("own", "X"),
        "the two encoders wrote different bytes for the refused row"
    );
}

/// A tick flush that fails hands every pending row to the rescue tier, with
/// either encoder: none is dropped on the floor. Needs no QuestDB (the
/// writer points at a closed port), so it runs in every lane.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_tick_flush_hands_every_row_to_the_rescue_with_both_encoders() {
    let closed = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 1,
        pg_port: 8812,
        ilp_port: 9009,
    };
    for (encoder, offset) in ENCODERS {
        let mut w = TickWriter::new_with_encoder(&closed, Feed::Dhan, encoder);
        let (_rescue_sink, rescued) = w.split_rescue_offload();
        for seq in 1..=3 {
            let t = tick(950_001 + offset, SEG_NSE_FNO, TEN, 101.5, seq);
            w.append_tick_with_seq(&t, i64::from(seq)).expect("append"); // APPROVED: test-only
        }
        assert_eq!(w.pending(), 3, "{encoder:?}");
        let flushed = tokio::task::block_in_place(|| w.flush());
        assert!(flushed.is_err(), "{encoder:?}: a closed port cannot ack");
        assert_eq!(w.pending(), 0, "{encoder:?}: the buffer moved on");
        let batch = rescued
            .try_recv()
            .unwrap_or_else(|e| panic!("{encoder:?}: no rescue batch: {e}"));
        assert_eq!(batch.rows(), 3, "{encoder:?}: every row reaches the rescue");
    }
}
