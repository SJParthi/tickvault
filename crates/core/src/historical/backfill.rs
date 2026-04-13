//! A3: Live intraday backfill worker for zero-tick-loss.
//!
//! When `TickGapTracker` emits an ERROR-level gap (configurable threshold,
//! default 30s), this module provides a backfill pipeline that pulls the
//! missing time window from Dhan's historical intraday API, synthesises
//! `ParsedTick` records from the minute OHLCV candles, and feeds them back
//! through the normal `append_tick` path. QuestDB DEDUP ensures any
//! overlapping ticks from a delayed WebSocket reconnect are merged
//! idempotently.
//!
//! # Design
//!
//! The module is split into:
//!
//! 1. [`GapBackfillRequest`] — event emitted by the gap tracker
//! 2. [`synthesize_ticks_from_minute_candles`] — pure fn, testable in isolation
//! 3. [`BackfillWorker`] — task loop that fetches + synthesises + appends.
//!
//! The worker accepts a boxed async fetcher closure so tests can substitute
//! a mock without spinning up the real Dhan REST client. Production wiring
//! passes a closure that delegates to `candle_fetcher::fetch_intraday_with_retry`.
//!
//! # Timestamp conversion
//!
//! **Rule:** Historical REST API returns UTC epoch seconds. WebSocket ticks
//! carry IST epoch seconds. Synthesised ticks must match the IST convention
//! so `append_tick` stores them compatibly. We add `IST_UTC_OFFSET_SECONDS`
//! (+19800) to the UTC candle timestamp before writing it to
//! `ParsedTick.exchange_timestamp`. This is the ONLY place in the codebase
//! that adds the IST offset to tick data, and it's covered by unit tests
//! named `test_backfill_timestamp_*` to make regressions loud.

use std::sync::atomic::{AtomicU64, Ordering};

use dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS_I64;
use dhan_live_trader_common::tick_types::{HistoricalCandle, ParsedTick};

// ---------------------------------------------------------------------------
// Event type emitted by TickGapTracker → backfill worker
// ---------------------------------------------------------------------------

/// A3: Request sent from the tick processor to the backfill worker when a
/// gap exceeding the ERROR threshold is detected. Pure data — contains only
/// what the worker needs to fetch the missing window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapBackfillRequest {
    /// Dhan security identifier.
    pub security_id: u32,
    /// Exchange segment code (propagated to synthesised ticks).
    pub exchange_segment_code: u8,
    /// Last IST epoch second we received a live tick for, BEFORE the gap.
    pub from_ist_secs: u32,
    /// Current IST epoch second when the gap was detected, AFTER the gap.
    pub to_ist_secs: u32,
}

impl GapBackfillRequest {
    /// Returns the gap duration in seconds. Never panics — saturates at 0
    /// if timestamps are out of order.
    pub fn gap_secs(&self) -> u32 {
        self.to_ist_secs.saturating_sub(self.from_ist_secs)
    }
}

// ---------------------------------------------------------------------------
// Pure: candle → tick synthesis
// ---------------------------------------------------------------------------

/// A3: Converts a batch of 1-minute historical candles into synthetic
/// `ParsedTick` records, one per candle. Pure function, zero I/O, trivially
/// testable.
///
/// For each candle:
/// - `last_traded_price` = candle close (most recent price in the bar)
/// - `exchange_timestamp` = candle timestamp + IST offset (seconds)
/// - `volume` = candle volume (cumulative up to this minute)
/// - `open_interest` = candle OI (for F&O)
/// - `day_high` / `day_low` / `day_open` = candle's H/L/O (best-effort
///   reconstruction; only day_close remains unknown at backfill time)
/// - Greeks = NaN (not available from historical candles)
///
/// The synthesised ticks are NOT bit-identical to live ticks — they carry
/// minute-resolution timestamps, not sub-second. This is by design: they
/// are placeholders to close the gap, and QuestDB DEDUP will replace them
/// if the live WebSocket later delivers the real ticks.
///
/// # Panics / Errors
///
/// Never panics. Skips any candle whose conversion would overflow `u32` for
/// timestamps or quantities (extremely unlikely with real Dhan data).
///
/// # Performance
///
/// O(n) in the number of candles. Cold path — runs once per detected gap,
/// not per tick. No allocation beyond the returned Vec.
pub fn synthesize_ticks_from_minute_candles(
    candles: &[HistoricalCandle],
    received_at_nanos: i64,
) -> Vec<ParsedTick> {
    let mut ticks = Vec::with_capacity(candles.len());
    for candle in candles {
        // UTC epoch seconds (REST) → IST epoch seconds (ParsedTick convention).
        // Per data-integrity rules this is the ONLY place we add the offset
        // to tick data.
        let ist_secs_i64 = candle
            .timestamp_utc_secs
            .saturating_add(IST_UTC_OFFSET_SECONDS_I64);
        if ist_secs_i64 < 0 || ist_secs_i64 > i64::from(u32::MAX) {
            tracing::warn!(
                security_id = candle.security_id,
                timestamp_utc_secs = candle.timestamp_utc_secs,
                "backfill: skipping candle with out-of-range timestamp"
            );
            continue;
        }
        // Clippy: saturating_as pattern — we just range-checked above, so
        // `as u32` is safe.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // APPROVED: range-checked above
        let exchange_timestamp = ist_secs_i64 as u32;

        // Volume / OI from i64 → u32. Saturate on overflow (Dhan's volume
        // field can exceed u32 for nifty on high-vol days).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // APPROVED: intentional saturate
        let volume = candle.volume.clamp(0, i64::from(u32::MAX)) as u32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // APPROVED: intentional saturate
        let open_interest = candle.open_interest.clamp(0, i64::from(u32::MAX)) as u32;

        // Close price fits f32 for Dhan equity/FNO range (max ~100k rupees).
        #[allow(clippy::cast_possible_truncation)] // APPROVED: Dhan prices fit f32
        let last_traded_price = candle.close as f32;
        #[allow(clippy::cast_possible_truncation)] // APPROVED: Dhan prices fit f32
        let day_open = candle.open as f32;
        #[allow(clippy::cast_possible_truncation)] // APPROVED: Dhan prices fit f32
        let day_high = candle.high as f32;
        #[allow(clippy::cast_possible_truncation)] // APPROVED: Dhan prices fit f32
        let day_low = candle.low as f32;

        ticks.push(ParsedTick {
            security_id: candle.security_id,
            exchange_segment_code: candle.exchange_segment_code,
            last_traded_price,
            last_trade_quantity: 0,
            exchange_timestamp,
            received_at_nanos,
            average_traded_price: last_traded_price,
            volume,
            total_sell_quantity: 0,
            total_buy_quantity: 0,
            day_open,
            day_close: 0.0, // unknown at backfill time — filled by QuestDB DEDUP if live tick arrives
            day_high,
            day_low,
            open_interest,
            oi_day_high: 0,
            oi_day_low: 0,
            iv: f64::NAN,
            delta: f64::NAN,
            gamma: f64::NAN,
            theta: f64::NAN,
            vega: f64::NAN,
        });
    }
    ticks
}

// ---------------------------------------------------------------------------
// BackfillWorker — consumes gap events, fetches + synthesises + appends
// ---------------------------------------------------------------------------

/// A3: Statistics reported by the worker for metrics and tests.
#[derive(Debug, Default)]
pub struct BackfillStats {
    /// Number of gap events received from the tracker.
    events_received: AtomicU64,
    /// Number of events where the fetcher returned zero candles.
    events_empty: AtomicU64,
    /// Number of events where the fetcher returned an error.
    events_errored: AtomicU64,
    /// Number of events where the fetcher succeeded.
    events_succeeded: AtomicU64,
    /// Total synthetic ticks pushed back into the pipeline.
    ticks_synthesised: AtomicU64,
}

impl BackfillStats {
    /// Returns a snapshot of the counters. Cold path — used by tests and
    /// `/api/stats` only.
    pub fn snapshot(&self) -> BackfillStatsSnapshot {
        BackfillStatsSnapshot {
            events_received: self.events_received.load(Ordering::Acquire),
            events_empty: self.events_empty.load(Ordering::Acquire),
            events_errored: self.events_errored.load(Ordering::Acquire),
            events_succeeded: self.events_succeeded.load(Ordering::Acquire),
            ticks_synthesised: self.ticks_synthesised.load(Ordering::Acquire),
        }
    }
}

/// A3: Immutable counters snapshot for inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackfillStatsSnapshot {
    pub events_received: u64,
    pub events_empty: u64,
    pub events_errored: u64,
    pub events_succeeded: u64,
    pub ticks_synthesised: u64,
}

/// A3: Background worker that consumes `GapBackfillRequest` events and
/// turns them into synthetic ticks. The worker is generic over an async
/// fetcher closure so tests can mock the Dhan REST call.
///
/// # Wiring (production)
///
/// ```ignore
/// let (gap_tx, gap_rx) = tokio::sync::mpsc::channel(64);
/// let fetcher = |req: GapBackfillRequest| async move {
///     candle_fetcher::fetch_intraday_window(req.security_id, req.from_ist_secs, req.to_ist_secs).await
/// };
/// let worker = BackfillWorker::new(gap_rx, tick_sender, fetcher);
/// tokio::spawn(worker.run());
/// ```
pub struct BackfillWorker<F, Fut>
where
    F: Fn(GapBackfillRequest) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Vec<HistoricalCandle>, String>> + Send,
{
    gap_rx: tokio::sync::mpsc::Receiver<GapBackfillRequest>,
    tick_tx: tokio::sync::mpsc::Sender<ParsedTick>,
    fetcher: F,
    stats: std::sync::Arc<BackfillStats>,
}

impl<F, Fut> BackfillWorker<F, Fut>
where
    F: Fn(GapBackfillRequest) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Vec<HistoricalCandle>, String>> + Send,
{
    /// Creates a new backfill worker.
    pub fn new(
        gap_rx: tokio::sync::mpsc::Receiver<GapBackfillRequest>,
        tick_tx: tokio::sync::mpsc::Sender<ParsedTick>,
        fetcher: F,
    ) -> Self {
        Self {
            gap_rx,
            tick_tx,
            fetcher,
            stats: std::sync::Arc::new(BackfillStats::default()),
        }
    }

    /// Returns a cloneable handle to the worker's stats counter. Callers
    /// can snapshot this for metrics or tests.
    pub fn stats_handle(&self) -> std::sync::Arc<BackfillStats> {
        std::sync::Arc::clone(&self.stats)
    }

    /// Runs the worker loop. Consumes `self`. Returns when the gap
    /// receiver is closed (pipeline shutdown).
    pub async fn run(mut self) {
        while let Some(request) = self.gap_rx.recv().await {
            self.stats.events_received.fetch_add(1, Ordering::AcqRel);
            tracing::info!(
                security_id = request.security_id,
                gap_secs = request.gap_secs(),
                "A3: processing gap backfill request"
            );

            let candles = match (self.fetcher)(request.clone()).await {
                Ok(c) => c,
                Err(err) => {
                    self.stats.events_errored.fetch_add(1, Ordering::AcqRel);
                    tracing::warn!(
                        security_id = request.security_id,
                        error = %err,
                        "A3: backfill fetch failed — gap remains"
                    );
                    continue;
                }
            };

            if candles.is_empty() {
                self.stats.events_empty.fetch_add(1, Ordering::AcqRel);
                tracing::debug!(
                    security_id = request.security_id,
                    "A3: backfill fetch returned zero candles (market closed or no data)"
                );
                continue;
            }

            let now_nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            let ticks = synthesize_ticks_from_minute_candles(&candles, now_nanos);
            let tick_count = ticks.len() as u64;

            let mut pushed = 0_u64;
            for tick in ticks {
                if self.tick_tx.send(tick).await.is_err() {
                    tracing::warn!(
                        security_id = request.security_id,
                        "A3: tick pipeline closed — aborting backfill"
                    );
                    return;
                }
                pushed = pushed.saturating_add(1);
            }

            self.stats.events_succeeded.fetch_add(1, Ordering::AcqRel);
            self.stats
                .ticks_synthesised
                .fetch_add(pushed, Ordering::AcqRel);
            tracing::info!(
                security_id = request.security_id,
                synthesised = pushed,
                requested = tick_count,
                "A3: backfill completed"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test arithmetic
mod tests {
    use super::*;

    fn make_candle(ts_utc: i64, close: f64, volume: i64) -> HistoricalCandle {
        HistoricalCandle {
            timestamp_utc_secs: ts_utc,
            security_id: 1333,
            exchange_segment_code: 1,
            timeframe: "1m",
            open: close - 0.5,
            high: close + 1.0,
            low: close - 1.0,
            close,
            volume,
            open_interest: 0,
        }
    }

    // --- GapBackfillRequest ---

    #[test]
    fn test_gap_request_gap_secs_normal() {
        let req = GapBackfillRequest {
            security_id: 1333,
            exchange_segment_code: 1,
            from_ist_secs: 1_700_000_000,
            to_ist_secs: 1_700_000_065,
        };
        assert_eq!(req.gap_secs(), 65);
    }

    #[test]
    fn test_gap_request_gap_secs_out_of_order_saturates() {
        let req = GapBackfillRequest {
            security_id: 1333,
            exchange_segment_code: 1,
            from_ist_secs: 1_700_000_100,
            to_ist_secs: 1_700_000_050,
        };
        assert_eq!(
            req.gap_secs(),
            0,
            "out-of-order timestamps must saturate to 0, not panic"
        );
    }

    // --- synthesize_ticks_from_minute_candles ---

    #[test]
    fn test_backfill_empty_candles_produces_empty_ticks() {
        let ticks = synthesize_ticks_from_minute_candles(&[], 0);
        assert!(ticks.is_empty());
    }

    #[test]
    fn test_backfill_timestamp_adds_ist_offset() {
        // A candle at UTC midnight 2024-01-01 → 1704067200 UTC.
        // IST offset is +19800s → 1704087000 IST.
        let candle = make_candle(1_704_067_200, 24_000.0, 1_000);
        let ticks = synthesize_ticks_from_minute_candles(&[candle], 42);
        assert_eq!(ticks.len(), 1);
        assert_eq!(
            ticks[0].exchange_timestamp,
            1_704_067_200 + 19_800,
            "LOCKED FACT: backfill MUST add IST offset when converting REST candles to ticks"
        );
    }

    #[test]
    fn test_backfill_close_becomes_last_traded_price() {
        let candle = make_candle(1_700_000_000, 24_350.75, 5_000);
        let ticks = synthesize_ticks_from_minute_candles(&[candle], 0);
        assert_eq!(ticks.len(), 1);
        let t = &ticks[0];
        assert!((t.last_traded_price - 24_350.75).abs() < 0.01);
        assert!((t.average_traded_price - 24_350.75).abs() < 0.01);
        assert_eq!(t.volume, 5_000);
    }

    #[test]
    fn test_backfill_multiple_candles_preserves_order() {
        let candles = vec![
            make_candle(1_700_000_000, 100.0, 10),
            make_candle(1_700_000_060, 101.0, 20),
            make_candle(1_700_000_120, 102.0, 30),
        ];
        let ticks = synthesize_ticks_from_minute_candles(&candles, 0);
        assert_eq!(ticks.len(), 3);
        assert!((ticks[0].last_traded_price - 100.0).abs() < 0.01);
        assert!((ticks[1].last_traded_price - 101.0).abs() < 0.01);
        assert!((ticks[2].last_traded_price - 102.0).abs() < 0.01);
        assert_eq!(
            ticks[1].exchange_timestamp - ticks[0].exchange_timestamp,
            60,
            "minute spacing preserved"
        );
    }

    #[test]
    fn test_backfill_greeks_are_nan() {
        let candle = make_candle(1_700_000_000, 100.0, 10);
        let ticks = synthesize_ticks_from_minute_candles(&[candle], 0);
        assert!(ticks[0].iv.is_nan());
        assert!(ticks[0].delta.is_nan());
        assert!(ticks[0].gamma.is_nan());
        assert!(ticks[0].theta.is_nan());
        assert!(ticks[0].vega.is_nan());
    }

    #[test]
    fn test_backfill_volume_saturates_on_overflow() {
        let mut candle = make_candle(1_700_000_000, 100.0, 0);
        candle.volume = i64::MAX;
        let ticks = synthesize_ticks_from_minute_candles(&[candle], 0);
        assert_eq!(
            ticks[0].volume,
            u32::MAX,
            "volume above u32::MAX must saturate, not wrap"
        );
    }

    #[test]
    fn test_backfill_negative_volume_clamped_to_zero() {
        let mut candle = make_candle(1_700_000_000, 100.0, 0);
        candle.volume = -500;
        let ticks = synthesize_ticks_from_minute_candles(&[candle], 0);
        assert_eq!(ticks[0].volume, 0);
    }

    // --- BackfillWorker ---

    #[tokio::test]
    async fn test_backfill_worker_happy_path() {
        let (gap_tx, gap_rx) = tokio::sync::mpsc::channel(4);
        let (tick_tx, mut tick_rx) = tokio::sync::mpsc::channel(16);
        let fetcher = |_req: GapBackfillRequest| async move {
            Ok(vec![
                make_candle(1_700_000_000, 100.0, 10),
                make_candle(1_700_000_060, 101.0, 20),
            ])
        };
        let worker = BackfillWorker::new(gap_rx, tick_tx, fetcher);
        let stats = worker.stats_handle();
        let handle = tokio::spawn(worker.run());

        gap_tx
            .send(GapBackfillRequest {
                security_id: 1333,
                exchange_segment_code: 1,
                from_ist_secs: 1_700_000_000,
                to_ist_secs: 1_700_000_120,
            })
            .await
            .unwrap();

        // Expect 2 synthesised ticks.
        let tick1 = tick_rx.recv().await.expect("tick1");
        let tick2 = tick_rx.recv().await.expect("tick2");
        assert!((tick1.last_traded_price - 100.0).abs() < 0.01);
        assert!((tick2.last_traded_price - 101.0).abs() < 0.01);

        drop(gap_tx);
        handle.await.unwrap();

        let snap = stats.snapshot();
        assert_eq!(snap.events_received, 1);
        assert_eq!(snap.events_succeeded, 1);
        assert_eq!(snap.events_errored, 0);
        assert_eq!(snap.events_empty, 0);
        assert_eq!(snap.ticks_synthesised, 2);
    }

    #[tokio::test]
    async fn test_backfill_worker_handles_empty_fetch() {
        let (gap_tx, gap_rx) = tokio::sync::mpsc::channel(4);
        let (tick_tx, mut tick_rx) = tokio::sync::mpsc::channel(16);
        let fetcher = |_req: GapBackfillRequest| async move { Ok(Vec::new()) };
        let worker = BackfillWorker::new(gap_rx, tick_tx, fetcher);
        let stats = worker.stats_handle();
        let handle = tokio::spawn(worker.run());

        gap_tx
            .send(GapBackfillRequest {
                security_id: 1333,
                exchange_segment_code: 1,
                from_ist_secs: 1_700_000_000,
                to_ist_secs: 1_700_000_060,
            })
            .await
            .unwrap();

        drop(gap_tx);
        handle.await.unwrap();

        // No ticks sent.
        assert!(tick_rx.try_recv().is_err());
        let snap = stats.snapshot();
        assert_eq!(snap.events_received, 1);
        assert_eq!(snap.events_empty, 1);
        assert_eq!(snap.events_succeeded, 0);
        assert_eq!(snap.ticks_synthesised, 0);
    }

    #[tokio::test]
    async fn test_backfill_worker_handles_fetch_error() {
        let (gap_tx, gap_rx) = tokio::sync::mpsc::channel(4);
        let (tick_tx, _tick_rx) = tokio::sync::mpsc::channel(16);
        let fetcher =
            |_req: GapBackfillRequest| async move { Err("network unreachable".to_string()) };
        let worker = BackfillWorker::new(gap_rx, tick_tx, fetcher);
        let stats = worker.stats_handle();
        let handle = tokio::spawn(worker.run());

        gap_tx
            .send(GapBackfillRequest {
                security_id: 1333,
                exchange_segment_code: 1,
                from_ist_secs: 1_700_000_000,
                to_ist_secs: 1_700_000_060,
            })
            .await
            .unwrap();

        drop(gap_tx);
        handle.await.unwrap();

        let snap = stats.snapshot();
        assert_eq!(snap.events_received, 1);
        assert_eq!(snap.events_errored, 1);
        assert_eq!(snap.events_succeeded, 0);
    }

    #[tokio::test]
    async fn test_backfill_worker_aborts_on_tick_pipeline_closed() {
        let (gap_tx, gap_rx) = tokio::sync::mpsc::channel(4);
        let (tick_tx, tick_rx) = tokio::sync::mpsc::channel::<ParsedTick>(1);
        drop(tick_rx); // Close downstream immediately.
        let fetcher = |_req: GapBackfillRequest| async move {
            Ok(vec![make_candle(1_700_000_000, 100.0, 10)])
        };
        let worker = BackfillWorker::new(gap_rx, tick_tx, fetcher);
        let handle = tokio::spawn(worker.run());

        gap_tx
            .send(GapBackfillRequest {
                security_id: 1333,
                exchange_segment_code: 1,
                from_ist_secs: 1_700_000_000,
                to_ist_secs: 1_700_000_060,
            })
            .await
            .unwrap();

        // Worker must return when the tick receiver is gone, not hang.
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("worker must exit within 2s when pipeline closes")
            .unwrap();
    }
}
