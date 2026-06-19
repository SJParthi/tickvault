//! Groww feed bridge — second feed (operator lock §32).
//!
//! The Rust consumer side of the Groww feed: reads the **capture-at-receipt
//! durable file** the Python `growwapi` sidecar appends (one NDJSON tick per
//! line), persists each raw tick to the SHARED `ticks` table (tagged
//! `feed='groww'`), folds ticks through the [`Groww1mAggregator`], and persists
//! sealed 1-minute candles to the SHARED `candles_1m` table (tagged
//! `feed='groww'`) — the "same tables + feed column" model (operator 2026-06-19).
//!
//! **Default OFF + isolated:** only runs when `feeds.groww_enabled`. It writes
//! ONLY Groww-tagged (`feed='groww'`) rows into the shared tables, distinguished
//! from Dhan rows (`feed='dhan'`) by the feed-extended DEDUP keys, and never
//! touches the Dhan write path. **Dormant until the sidecar exists:** the loop is
//! a TEST-EXEMPT I/O driver; the pure, fully-unit-tested primitives are the line
//! parser, the segment map, and the candle→row mapper. The NDJSON schema below is
//! the Python↔Rust contract (defined consumer-side; the sidecar conforms).
//!
//! ```jsonc
//! {"security_id":1333,"segment":"NSE_EQ","ts_ist_nanos":1780000020123000000,
//!  "exchange_ts_millis":1780000020123,"ltp":2847.55,"cum_volume":123456}
//! ```

use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::feed::groww::aggregator_1m::{Groww1mAggregator, Groww1mCandle, Groww1mTick};
use tickvault_storage::groww_candle_persistence::{GrowwCandle1mRow, GrowwCandle1mWriter};
use tickvault_storage::groww_persistence::{GrowwLiveTickRow, GrowwLiveTickWriter};

/// Poll interval (milliseconds) for tailing the sidecar's append-only tick file.
const GROWW_BRIDGE_POLL_MS: u64 = 500;
/// Poll interval for tailing the sidecar's append-only tick file.
const GROWW_BRIDGE_POLL: Duration = Duration::from_millis(GROWW_BRIDGE_POLL_MS);

/// Broker-source provenance label written into the `feed` column.
pub const GROWW_FEED_NAME: &str = "groww";

/// Default path of the Python sidecar's capture-at-receipt append-only tick file.
pub const GROWW_TICK_FILE_DEFAULT: &str = "data/groww/live-ticks.ndjson";

/// One capture-at-receipt tick line the Python sidecar appends. The fields are
/// the Python↔Rust contract (see module docs). `ts_ist_nanos` is already IST
/// (the sidecar converts); `cum_volume` is Groww cumulative day volume.
#[derive(Debug, Deserialize)]
struct GrowwTickLine {
    security_id: i64,
    segment: String,
    ts_ist_nanos: i64,
    exchange_ts_millis: i64,
    ltp: f64,
    cum_volume: i64,
}

/// A parsed Groww tick — the aggregator input plus the raw fields needed for the
/// shared `ticks` (feed='groww') row.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ParsedGrowwTick {
    tick: Groww1mTick,
    exchange_ts_millis: i64,
}

/// Maps the canonical segment string to [`ExchangeSegment`]. Pure + testable.
fn segment_from_str(s: &str) -> Result<ExchangeSegment> {
    Ok(match s {
        "IDX_I" => ExchangeSegment::IdxI,
        "NSE_EQ" => ExchangeSegment::NseEquity,
        "NSE_FNO" => ExchangeSegment::NseFno,
        "NSE_CURRENCY" => ExchangeSegment::NseCurrency,
        "BSE_EQ" => ExchangeSegment::BseEquity,
        "MCX_COMM" => ExchangeSegment::McxComm,
        "BSE_CURRENCY" => ExchangeSegment::BseCurrency,
        "BSE_FNO" => ExchangeSegment::BseFno,
        other => anyhow::bail!("unknown Groww segment: {other}"),
    })
}

/// Parses one NDJSON tick line. Pure + testable.
fn parse_groww_tick_line(line: &str) -> Result<ParsedGrowwTick> {
    let l: GrowwTickLine =
        serde_json::from_str(line).context("groww bridge: tick line is not valid JSON")?;
    let segment = segment_from_str(&l.segment)?;
    Ok(ParsedGrowwTick {
        tick: Groww1mTick {
            security_id: l.security_id,
            segment,
            ts_ist_nanos: l.ts_ist_nanos,
            ltp: l.ltp,
            cum_volume: l.cum_volume,
        },
        exchange_ts_millis: l.exchange_ts_millis,
    })
}

/// Builds the shared `ticks` (feed='groww') row for a parsed tick. `capture_seq`
/// is the monotonic, replay-stable dedup tiebreaker (TICK-SEQ-01). Pure + testable.
fn live_tick_row(parsed: &ParsedGrowwTick, capture_seq: i64) -> GrowwLiveTickRow {
    GrowwLiveTickRow {
        ts_ist_nanos: parsed.tick.ts_ist_nanos,
        security_id: parsed.tick.security_id,
        segment: parsed.tick.segment.as_str(),
        ltp: parsed.tick.ltp,
        volume: parsed.tick.cum_volume,
        exchange_ts_millis: parsed.exchange_ts_millis,
        capture_seq,
    }
}

/// Maps an aggregated [`Groww1mCandle`] to a persistable row for the SHARED
/// `candles_1m` table, stamping the `groww` feed-provenance label (part of the
/// shared candle DEDUP key). Pure + testable.
fn candle_row_from_aggregated(candle: &Groww1mCandle) -> GrowwCandle1mRow {
    GrowwCandle1mRow {
        ts_ist_nanos: candle.minute_start_ist_nanos,
        security_id: candle.security_id,
        segment: candle.segment.as_str(),
        feed: GROWW_FEED_NAME,
        open: candle.open,
        high: candle.high,
        low: candle.low,
        close: candle.close,
        volume: candle.volume,
        tick_count: candle.tick_count,
    }
}

/// Monotonic, replay-stable `capture_seq` source (TICK-SEQ-01): `max(prev+1,
/// wall_nanos)`. Seeded from wall-clock so it never resets below a prior value
/// across restarts within a session.
fn next_capture_seq(counter: &AtomicI64, wall_nanos: i64) -> i64 {
    let mut prev = counter.load(Ordering::Relaxed);
    loop {
        let next = (prev + 1).max(wall_nanos);
        match counter.compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(observed) => prev = observed,
        }
    }
}

/// Dormant consumer driver: tails the sidecar's append-only NDJSON tick file,
/// persists each raw tick to the SHARED `ticks` table (feed='groww'), folds them
/// through the 1m aggregator, and persists sealed candles to the SHARED
/// `candles_1m` table (feed='groww'). Runs ONLY when `feeds.groww_enabled`;
/// writes ONLY `feed='groww'`-tagged rows; never touches the Dhan write path.
/// Idles (no writes) until the Python sidecar starts appending to
/// `tick_file_path`. Loops forever; per-line/flush errors are logged, never
/// propagated (a bad line or a transient QuestDB blip must not kill the feed).
// TEST-EXEMPT: file-tail + live-QuestDB ILP I/O driver; the pure primitives it
// uses (parse_groww_tick_line, segment_from_str, live_tick_row,
// candle_row_from_aggregated, next_capture_seq) are unit-tested below.
pub async fn run_groww_bridge(qdb: QuestDbConfig, tick_file_path: PathBuf) {
    info!(
        path = %tick_file_path.display(),
        "Groww bridge started — tailing the sidecar tick file (idles until the sidecar appends)"
    );
    let mut live_writer = GrowwLiveTickWriter::new(&qdb);
    let mut candle_writer = GrowwCandle1mWriter::new(&qdb);
    let mut aggregator = Groww1mAggregator::new();
    let capture_seq = AtomicI64::new(0);
    let mut offset: u64 = 0;
    let mut residual = String::new();

    loop {
        tokio::time::sleep(GROWW_BRIDGE_POLL).await;

        let mut file = match File::open(&tick_file_path).await {
            Ok(f) => f,
            Err(_) => continue, // sidecar not started yet — idle, no writes
        };
        let len = match file.metadata().await {
            Ok(m) => m.len(),
            Err(err) => {
                warn!(?err, "groww bridge: metadata failed");
                continue;
            }
        };
        if len < offset {
            // File shrank (rotation/truncate) — restart from the top.
            offset = 0;
            residual.clear();
        }
        if len == offset {
            continue; // nothing new
        }
        if file.seek(SeekFrom::Start(offset)).await.is_err() {
            continue;
        }
        let mut buf = String::new();
        match file.read_to_string(&mut buf).await {
            Ok(n) => offset = offset.saturating_add(n as u64),
            Err(err) => {
                warn!(?err, "groww bridge: read failed");
                continue;
            }
        }

        residual.push_str(&buf);
        let mut wrote_live = false;
        let mut wrote_candle = false;
        // Process only COMPLETE lines; keep any trailing partial line buffered.
        while let Some(nl) = residual.find('\n') {
            let raw: String = residual.drain(..=nl).collect();
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let parsed = match parse_groww_tick_line(line) {
                Ok(p) => p,
                Err(err) => {
                    error!(?err, "groww bridge: skipping malformed tick line");
                    continue;
                }
            };
            let seq = next_capture_seq(&capture_seq, parsed.tick.ts_ist_nanos);
            if let Err(err) = live_writer.append_row(&live_tick_row(&parsed, seq)) {
                error!(
                    ?err,
                    "groww bridge: shared ticks (feed=groww) append failed"
                );
            } else {
                wrote_live = true;
            }
            if let Some(candle) = aggregator.on_tick(&parsed.tick) {
                if let Err(err) = candle_writer.append_row(&candle_row_from_aggregated(&candle)) {
                    error!(
                        ?err,
                        "groww bridge: shared candles_1m (feed=groww) append failed"
                    );
                } else {
                    wrote_candle = true;
                }
            }
        }
        // Flush failures are data-at-risk (ILP-buffered Groww rows may not have
        // reached QuestDB) — `error!` per audit Rule 5 / charter Rule 6 so they
        // route to Telegram via ERROR-level Loki routing, never silent `warn!`.
        if wrote_live && let Err(err) = live_writer.flush() {
            error!(?err, "groww bridge: shared ticks (feed=groww) flush failed");
        }
        if wrote_candle && let Err(err) = candle_writer.flush() {
            error!(
                ?err,
                "groww bridge: shared candles_1m (feed=groww) flush failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"security_id":1333,"segment":"NSE_EQ","ts_ist_nanos":1780000020123000000,"exchange_ts_millis":1780000020123,"ltp":2847.55,"cum_volume":123456}"#;

    #[test]
    fn test_segment_from_str_known_and_unknown() {
        assert_eq!(
            segment_from_str("NSE_EQ").unwrap(),
            ExchangeSegment::NseEquity
        );
        assert_eq!(segment_from_str("IDX_I").unwrap(), ExchangeSegment::IdxI);
        assert_eq!(
            segment_from_str("NSE_FNO").unwrap(),
            ExchangeSegment::NseFno
        );
        assert!(segment_from_str("WAT_EQ").is_err());
        assert!(segment_from_str("").is_err());
    }

    #[test]
    fn test_parse_groww_tick_line_ok() {
        let p = parse_groww_tick_line(LINE).expect("parse");
        assert_eq!(p.tick.security_id, 1333);
        assert_eq!(p.tick.segment, ExchangeSegment::NseEquity);
        assert_eq!(p.tick.ts_ist_nanos, 1_780_000_020_123_000_000);
        assert_eq!(p.tick.ltp, 2847.55);
        assert_eq!(p.tick.cum_volume, 123_456);
        assert_eq!(p.exchange_ts_millis, 1_780_000_020_123);
    }

    #[test]
    fn test_parse_groww_tick_line_rejects_garbage_and_bad_segment() {
        assert!(parse_groww_tick_line("not json").is_err());
        assert!(parse_groww_tick_line("{}").is_err());
        let bad_seg = r#"{"security_id":1,"segment":"XYZ","ts_ist_nanos":1,"exchange_ts_millis":1,"ltp":1.0,"cum_volume":1}"#;
        assert!(parse_groww_tick_line(bad_seg).is_err());
    }

    #[test]
    fn test_live_tick_row_maps_all_fields() {
        let p = parse_groww_tick_line(LINE).expect("parse");
        let row = live_tick_row(&p, 42);
        assert_eq!(row.security_id, 1333);
        assert_eq!(row.segment, "NSE_EQ");
        assert_eq!(row.ltp, 2847.55);
        assert_eq!(row.volume, 123_456);
        assert_eq!(row.exchange_ts_millis, 1_780_000_020_123);
        assert_eq!(row.capture_seq, 42);
        assert_eq!(row.ts_ist_nanos, 1_780_000_020_123_000_000);
    }

    #[test]
    fn test_candle_row_from_aggregated_stamps_groww_feed() {
        let candle = Groww1mCandle {
            security_id: 1333,
            segment: ExchangeSegment::NseEquity,
            minute_start_ist_nanos: 1_780_000_020_000_000_000,
            open: 100.0,
            high: 110.0,
            low: 99.0,
            close: 105.0,
            volume: 50,
            tick_count: 7,
        };
        let row = candle_row_from_aggregated(&candle);
        assert_eq!(row.feed, "groww");
        assert_eq!(row.segment, "NSE_EQ");
        assert_eq!(row.security_id, 1333);
        assert_eq!(row.ts_ist_nanos, 1_780_000_020_000_000_000);
        assert_eq!(row.open, 100.0);
        assert_eq!(row.close, 105.0);
        assert_eq!(row.volume, 50);
        assert_eq!(row.tick_count, 7);
    }

    #[test]
    fn test_next_capture_seq_is_monotonic() {
        let c = AtomicI64::new(0);
        let a = next_capture_seq(&c, 1_000);
        let b = next_capture_seq(&c, 1_000); // wall not advanced → prev+1
        let d = next_capture_seq(&c, 5_000); // wall jumps → takes wall
        assert_eq!(a, 1_000);
        assert_eq!(b, 1_001);
        assert_eq!(d, 5_000);
        assert!(b > a && d > b, "strictly monotonic");
    }
}
