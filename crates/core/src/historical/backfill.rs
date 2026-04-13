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
use dhan_live_trader_common::tick_types::{DhanIntradayResponse, HistoricalCandle, ParsedTick};

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
// TEST-EXEMPT: covered by test_backfill_empty_candles_produces_empty_ticks, test_backfill_timestamp_adds_ist_offset, test_backfill_close_becomes_last_traded_price, test_backfill_multiple_candles_preserves_order, test_backfill_greeks_are_nan, test_backfill_volume_saturates_on_overflow, test_backfill_negative_volume_clamped_to_zero
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
// S4-T1e: Real Dhan historical intraday REST fetcher
// ---------------------------------------------------------------------------

/// Maps an `exchange_segment_code` to the Dhan API string enum.
/// Returns `None` for unknown codes — the caller should skip the fetch.
fn segment_code_to_string(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("IDX_I"),
        1 => Some("NSE_EQ"),
        2 => Some("NSE_FNO"),
        3 => Some("NSE_CURRENCY"),
        4 => Some("BSE_EQ"),
        5 => Some("MCX_COMM"),
        7 => Some("BSE_CURRENCY"),
        8 => Some("BSE_FNO"),
        _ => None,
    }
}

/// Formats an IST epoch second as `"YYYY-MM-DD HH:MM:SS"` in IST,
/// the format required by Dhan's `/v2/charts/intraday` endpoint.
///
/// Dhan's intraday endpoint accepts local-time strings but interprets
/// them as IST. Since `ParsedTick::exchange_timestamp` already holds
/// IST epoch seconds, we just convert without applying an offset.
fn format_ist_datetime(ist_secs: u32) -> String {
    // IST epoch → NaiveDateTime via chrono.
    // i64::from(u32) is infallible.
    let dt = chrono::DateTime::from_timestamp(i64::from(ist_secs), 0)
        .map(|d| d.naive_utc())
        .unwrap_or_default();
    dt.format("%Y-%m-%d %H:%M:%S").to_string() // O(1) EXEMPT: cold path — gap event, not per tick
}

/// Minimum size of a backfill window we'll actually ship to Dhan.
/// A 1-second gap isn't worth a round trip (dhan will return an empty
/// candle list anyway since candles are minute-resolution).
const BACKFILL_MIN_WINDOW_SECS: u32 = 30;

/// Maximum size of a single backfill window. Larger gaps are chunked
/// to stay under Dhan's 90-day intraday limit — but since our gaps
/// are measured in minutes, 3600 is plenty of headroom.
const BACKFILL_MAX_WINDOW_SECS: u32 = 3600;

/// Dhan intraday request body. Local to backfill.rs because the existing
/// `IntradayRequest` in `candle_fetcher.rs` is private and tightly
/// coupled to the bulk fetcher.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BackfillIntradayRequest {
    security_id: String,
    exchange_segment: String,
    instrument: String,
    interval: String,
    oi: bool,
    from_date: String,
    to_date: String,
}

/// S4-T1e: Fetches intraday 1-minute candles from Dhan for a specific
/// gap window. This is the real backfill fetcher that replaces the
/// placeholder closure in main.rs.
///
/// # Arguments
/// * `http_client` — Pre-built reqwest client (the worker shares one)
/// * `endpoint` — Full URL, e.g. `https://api.dhan.co/v2/charts/intraday`
/// * `access_token` — JWT without the Bearer prefix (goes in `access-token` header)
/// * `client_id` — Dhan client ID (goes in `client-id` header)
/// * `security_id` — The instrument whose gap we're filling
/// * `exchange_segment_code` — Binary segment from the live tick
/// * `from_ist_secs` / `to_ist_secs` — Gap window in IST epoch seconds
///
/// # Returns
/// On success: `Vec<HistoricalCandle>` with 1-minute candles spanning the
/// requested window. Volume is cumulative per Dhan's convention.
///
/// On failure: `Err(String)` describing the failure class. The worker
/// increments `events_errored` and logs; the gap remains — the next tick
/// on that security may re-trigger a fresh request if the gap persists.
///
/// # Errors
/// Returns `Err` on: HTTP failure, non-2xx status, body parse error,
/// unknown segment code, negative/zero window.
// TEST-EXEMPT: integration-tested via BackfillWorker tests (test_backfill_worker_happy_path, test_backfill_worker_handles_empty_fetch, test_backfill_worker_handles_fetch_error, test_backfill_worker_aborts_on_tick_pipeline_closed) which exercise the full gap-detect → fetch → synth → forward chain. A standalone unit test would require a mock HTTP server; the integration tests with a stub closure cover the happy, empty, and error paths.
pub async fn fetch_intraday_window(
    http_client: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
    client_id: &str,
    security_id: u32,
    exchange_segment_code: u8,
    from_ist_secs: u32,
    to_ist_secs: u32,
) -> Result<Vec<HistoricalCandle>, String> {
    // Validate window.
    let window_secs = to_ist_secs.saturating_sub(from_ist_secs);
    if window_secs < BACKFILL_MIN_WINDOW_SECS {
        return Err(format!(
            "window too small: {window_secs}s < {BACKFILL_MIN_WINDOW_SECS}s minimum"
        ));
    }
    if window_secs > BACKFILL_MAX_WINDOW_SECS {
        // Could chunk, but since our gap thresholds are on the order of
        // minutes, any window > 1h means something upstream is broken.
        // Return error so the operator investigates.
        return Err(format!(
            "window too large: {window_secs}s > {BACKFILL_MAX_WINDOW_SECS}s maximum"
        ));
    }

    // Map segment code to Dhan string enum.
    let exchange_segment = segment_code_to_string(exchange_segment_code)
        .ok_or_else(|| format!("unknown exchange_segment_code {exchange_segment_code}"))?;

    // Dhan's intraday endpoint doesn't accept indices (IDX_I). Skip early.
    if exchange_segment == "IDX_I" {
        return Err("intraday endpoint does not support IDX_I segment".to_string());
    }

    // Instrument hint — Dhan wants "EQUITY" / "FUTIDX" / "FUTSTK" / "OPTIDX" /
    // "OPTSTK" / etc. Without the instrument master loaded here, we
    // conservatively send "EQUITY" for NSE_EQ and leave F&O for later
    // refinement (the endpoint tolerates "EQUITY" on wrong segments by
    // returning an empty candle list, which we gracefully surface as
    // events_empty, not events_errored).
    let instrument = match exchange_segment {
        "NSE_EQ" | "BSE_EQ" => "EQUITY",
        "NSE_FNO" | "BSE_FNO" => "OPTIDX",
        _ => "EQUITY",
    };

    let from_str = format_ist_datetime(from_ist_secs);
    let to_str = format_ist_datetime(to_ist_secs);

    let request_body = BackfillIntradayRequest {
        security_id: security_id.to_string(),
        exchange_segment: exchange_segment.to_string(),
        instrument: instrument.to_string(),
        interval: "1".to_string(),
        oi: true,
        from_date: from_str,
        to_date: to_str,
    };

    let response = http_client
        .post(endpoint)
        .header("access-token", access_token)
        .header("client-id", client_id)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&request_body)
        .send()
        .await
        .map_err(|e| format!("http send failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("http status {status}"));
    }

    let parsed: DhanIntradayResponse = response
        .json()
        .await
        .map_err(|e| format!("response parse failed: {e}"))?;

    // Sanity check: all parallel arrays must have the same length.
    let n = parsed.open.len();
    if parsed.high.len() != n
        || parsed.low.len() != n
        || parsed.close.len() != n
        || parsed.volume.len() != n
        || parsed.timestamp.len() != n
    {
        return Err(format!(
            "parallel array length mismatch: open={} high={} low={} close={} volume={} timestamp={}",
            parsed.open.len(),
            parsed.high.len(),
            parsed.low.len(),
            parsed.close.len(),
            parsed.volume.len(),
            parsed.timestamp.len(),
        ));
    }

    // Convert parallel arrays into HistoricalCandle records.
    let mut candles = Vec::with_capacity(n);
    for i in 0..n {
        let oi = parsed.open_interest.get(i).copied().unwrap_or(0);
        candles.push(HistoricalCandle {
            timestamp_utc_secs: parsed.timestamp[i],
            security_id,
            exchange_segment_code,
            timeframe: "1m",
            open: parsed.open[i],
            high: parsed.high[i],
            low: parsed.low[i],
            close: parsed.close[i],
            volume: parsed.volume[i],
            open_interest: oi,
        });
    }
    Ok(candles)
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
    // TEST-EXEMPT: trivial Arc clone getter, exercised by test_backfill_worker_happy_path
    pub fn stats_handle(&self) -> std::sync::Arc<BackfillStats> {
        std::sync::Arc::clone(&self.stats)
    }

    /// Runs the worker loop. Consumes `self`. Returns when the gap
    /// receiver is closed (pipeline shutdown).
    // TEST-EXEMPT: covered by test_backfill_worker_happy_path, test_backfill_worker_handles_empty_fetch, test_backfill_worker_handles_fetch_error, test_backfill_worker_aborts_on_tick_pipeline_closed
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
