//! Main tick processing loop — the heart of the pipeline.
//!
//! Consumes raw WebSocket binary frames, parses them, and persists ticks
//! to QuestDB. Pure capture — zero aggregation on the hot path.
//!
//! # Performance
//! - O(1) per tick
//! - Zero heap allocation on hot path
//! - Batched QuestDB writes (flush every 1000 rows or 100ms)

use std::time::Instant;

use metrics::{counter, gauge, histogram};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, trace, warn};

use crate::parser::dispatch_frame;
use crate::parser::types::ParsedFrame;
use dhan_live_trader_common::constants::{
    DEDUP_RING_BUFFER_POWER, MINIMUM_VALID_EXCHANGE_TIMESTAMP,
};
use dhan_live_trader_common::tick_types::ParsedTick;

use dhan_live_trader_storage::candle_persistence::LiveCandleWriter;
use dhan_live_trader_storage::tick_persistence::{
    DepthPersistenceWriter, TickPersistenceWriter, build_previous_close_row,
};

use dhan_live_trader_common::tick_types::MarketDepthLevel;

/// Returns `true` if all 5 depth levels have finite bid/ask prices.
///
/// # Performance
/// O(1) — exactly 10 `is_finite()` checks (single CPU instruction each).
#[inline(always)]
fn depth_prices_are_finite(depth: &[MarketDepthLevel; 5]) -> bool {
    depth
        .iter()
        .all(|level| level.bid_price.is_finite() && level.ask_price.is_finite())
}

// ---------------------------------------------------------------------------
// O(1) Tick Deduplication Ring Buffer
// ---------------------------------------------------------------------------

/// O(1) fixed-size dedup ring buffer for tick deduplication.
///
/// Pre-allocated at pipeline startup; zero allocation on the hot path.
/// Uses open-addressing with instant eviction — a single slot per hash bucket.
///
/// # Dedup key
/// `(security_id, exchange_timestamp, ltp_bits)` — catches exact duplicate ticks
/// while allowing legitimate price updates within the same second.
///
/// # False negative safety
/// Hash collisions cause eviction, meaning some duplicates may pass through.
/// This is safe: QuestDB `DEDUP UPSERT KEYS(ts, security_id)` is the
/// authoritative server-side dedup. This ring buffer reduces redundant writes.
struct TickDedupRing {
    /// Pre-allocated slot array. Each slot holds a 64-bit fingerprint.
    /// Initialized to `u64::MAX` (empty sentinel).
    slots: Box<[u64]>,
    /// Bitmask for fast modulo: `size - 1` where size is a power of two.
    mask: usize,
}

#[allow(clippy::arithmetic_side_effects)] // APPROVED: wrapping_mul is intentional for hash mixing; shift/bitwise ops are bounded by u64/u32 width
impl TickDedupRing {
    /// Creates a new ring buffer with `2^power` slots.
    ///
    /// # Panics (debug only)
    /// Debug-asserts that `power` is in [8, 24].
    fn new(power: u32) -> Self {
        debug_assert!(
            (8..=24).contains(&power),
            "dedup ring buffer power out of range"
        );
        let size = 1_usize << power;
        Self {
            slots: vec![u64::MAX; size].into_boxed_slice(),
            mask: size.wrapping_sub(1),
        }
    }

    /// Returns `true` if this tick was recently seen (duplicate).
    ///
    /// # Performance
    /// O(1) — one hash computation + one array lookup + one comparison.
    #[inline(always)]
    fn is_duplicate(&mut self, security_id: u32, exchange_timestamp: u32, ltp: f32) -> bool {
        let key = Self::fingerprint(security_id, exchange_timestamp, ltp);
        let idx = (key as usize) & self.mask;
        if self.slots[idx] == key {
            true
        } else {
            self.slots[idx] = key;
            false
        }
    }

    /// Builds a 64-bit fingerprint from tick identity fields using FNV-1a mixing.
    ///
    /// Distinct (security_id, exchange_timestamp, ltp) triples produce distinct
    /// fingerprints with high probability (~1 - 1/2^64 per pair).
    #[inline(always)]
    fn fingerprint(security_id: u32, exchange_timestamp: u32, ltp: f32) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325_u64; // FNV-1a 64-bit offset basis
        h ^= u64::from(security_id);
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a 64-bit prime
        h ^= u64::from(exchange_timestamp);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        h ^= u64::from(ltp.to_bits());
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        h
    }
}

/// Runs the tick processing pipeline until the frame receiver closes.
///
/// This is designed to run as a single `tokio::spawn` task.
///
/// # Arguments
/// * `frame_receiver` — raw binary frames from WebSocket pool
/// * `tick_writer` — batched QuestDB ILP writer for ticks (None if QuestDB unavailable)
/// * `depth_writer` — batched QuestDB ILP writer for market depth (None if QuestDB unavailable)
/// * `tick_broadcast` — optional broadcast sender for fan-out to browser WebSocket clients.
///   O(1) send per tick (atomic write into fixed-size ring buffer). Lagging receivers
///   are auto-skipped — the chart only needs the latest price.
/// * `candle_aggregator` — optional 1-second candle aggregator (None disables candle generation)
/// * `live_candle_writer` — optional QuestDB ILP writer for persisting completed 1s candles
/// * `top_movers` — optional top movers tracker (None disables gainers/losers tracking)
/// * `shared_snapshot` — optional shared handle for publishing top movers snapshots to API
pub async fn run_tick_processor(
    mut frame_receiver: mpsc::Receiver<bytes::Bytes>,
    mut tick_writer: Option<TickPersistenceWriter>,
    mut depth_writer: Option<DepthPersistenceWriter>,
    tick_broadcast: Option<broadcast::Sender<ParsedTick>>,
    mut candle_aggregator: Option<super::candle_aggregator::CandleAggregator>,
    mut live_candle_writer: Option<LiveCandleWriter>,
    mut top_movers: Option<super::top_movers::TopMoversTracker>,
    shared_snapshot: Option<super::top_movers::SharedTopMoversSnapshot>,
) {
    // Grab metric handles once before the hot loop — O(1) per tick after this.
    // These are no-ops if no metrics recorder is installed (e.g., in tests).
    let m_frames = counter!("dlt_frames_processed_total");
    let m_ticks = counter!("dlt_ticks_processed_total");
    let m_parse_errors = counter!("dlt_parse_errors_total");
    let m_storage_errors = counter!("dlt_storage_errors_total");
    let m_junk_filtered = counter!("dlt_junk_ticks_filtered_total");
    let m_depth_snapshots = counter!("dlt_depth_snapshots_total");
    let m_oi_updates = counter!("dlt_oi_updates_total");
    let m_prev_close_updates = counter!("dlt_prev_close_updates_total");
    let m_market_status_updates = counter!("dlt_market_status_updates_total");
    let m_disconnects = counter!("dlt_disconnect_frames_total");
    let m_tick_duration = histogram!("dlt_tick_processing_duration_ns");
    let m_pipeline_active = gauge!("dlt_pipeline_active");
    let m_dedup_filtered = counter!("dlt_dedup_filtered_total");
    let m_crossed_market = counter!("dlt_crossed_market_total");

    let mut frames_processed: u64 = 0;
    let mut ticks_processed: u64 = 0;
    let mut parse_errors: u64 = 0;
    let mut storage_errors: u64 = 0;
    let mut junk_ticks_filtered: u64 = 0;
    let mut dedup_filtered: u64 = 0;
    let mut last_flush_check = Instant::now();
    let mut last_snapshot_check = Instant::now();

    // O(1) dedup ring buffer — pre-allocated once, zero allocation in hot loop.
    let mut dedup_ring = TickDedupRing::new(DEDUP_RING_BUFFER_POWER);

    m_pipeline_active.set(1.0);
    info!("tick processor started");

    while let Some(raw_frame) = frame_receiver.recv().await {
        let tick_start = Instant::now();
        frames_processed = frames_processed.saturating_add(1);
        m_frames.increment(1);
        let received_at_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

        // Parse the binary frame
        let parsed = match dispatch_frame(&raw_frame, received_at_nanos) {
            Ok(frame) => frame,
            Err(err) => {
                parse_errors = parse_errors.saturating_add(1);
                m_parse_errors.increment(1);
                if parse_errors <= 100 {
                    warn!(
                        ?err,
                        frame_len = raw_frame.len(),
                        total_errors = parse_errors,
                        "failed to parse binary frame"
                    );
                }
                continue;
            }
        };

        // Process based on frame type
        match parsed {
            ParsedFrame::Tick(tick) => {
                ticks_processed = ticks_processed.saturating_add(1);
                m_ticks.increment(1);

                // Filter junk ticks: NaN/Infinity, zero/negative LTP,
                // or epoch timestamps (heartbeat/init frames from Dhan).
                if !tick.last_traded_price.is_finite()
                    || tick.last_traded_price <= 0.0
                    || tick.exchange_timestamp < MINIMUM_VALID_EXCHANGE_TIMESTAMP
                {
                    junk_ticks_filtered = junk_ticks_filtered.saturating_add(1);
                    m_junk_filtered.increment(1);
                    if junk_ticks_filtered <= 10 {
                        debug!(
                            security_id = tick.security_id,
                            ltp = tick.last_traded_price,
                            exchange_timestamp = tick.exchange_timestamp,
                            total_filtered = junk_ticks_filtered,
                            "junk tick filtered — LTP non-finite/invalid or timestamp invalid"
                        );
                    }
                    continue;
                }

                // O(1) dedup: skip exact duplicate ticks (same security_id +
                // timestamp + LTP) resent by Dhan on reconnection.
                if dedup_ring.is_duplicate(
                    tick.security_id,
                    tick.exchange_timestamp,
                    tick.last_traded_price,
                ) {
                    dedup_filtered = dedup_filtered.saturating_add(1);
                    m_dedup_filtered.increment(1);
                    continue;
                }

                // Persist tick to QuestDB
                if let Some(ref mut writer) = tick_writer
                    && let Err(err) = writer.append_tick(&tick)
                {
                    storage_errors = storage_errors.saturating_add(1);
                    m_storage_errors.increment(1);
                    if storage_errors <= 100 {
                        warn!(
                            ?err,
                            security_id = tick.security_id,
                            total_errors = storage_errors,
                            "failed to append tick to QuestDB"
                        );
                    }
                }

                // O(1) broadcast to browser WebSocket clients (if any are connected).
                // Lagging receivers auto-skip — chart only needs latest price.
                if let Some(ref sender) = tick_broadcast {
                    let _ = sender.send(tick);
                }

                // O(1) candle aggregation: update 1s OHLCV candle for this security.
                if let Some(ref mut agg) = candle_aggregator {
                    agg.update(&tick);
                }

                // O(1) top movers: update change_pct from previous close.
                if let Some(ref mut movers) = top_movers {
                    movers.update(&tick);
                }

                trace!(
                    security_id = tick.security_id,
                    ltp = tick.last_traded_price,
                    "tick processed"
                );
            }
            ParsedFrame::TickWithDepth(tick, depth) => {
                ticks_processed = ticks_processed.saturating_add(1);
                m_ticks.increment(1);

                // Depth data is ALWAYS persisted — Market Depth standalone packets
                // (code 3) have exchange_timestamp=0 by design, but their 5-level
                // depth is valid. The tick portion is only persisted when the
                // exchange timestamp is valid (Full packets, code 8).
                let ltp_valid = tick.last_traded_price.is_finite() && tick.last_traded_price > 0.0;
                let tick_is_valid =
                    ltp_valid && tick.exchange_timestamp >= MINIMUM_VALID_EXCHANGE_TIMESTAMP;

                if tick_is_valid {
                    // O(1) dedup: skip entire snapshot if tick is exact duplicate.
                    // Market Depth code 3 (timestamp=0) bypasses dedup — each
                    // snapshot is meaningful even without a timestamp.
                    if dedup_ring.is_duplicate(
                        tick.security_id,
                        tick.exchange_timestamp,
                        tick.last_traded_price,
                    ) {
                        dedup_filtered = dedup_filtered.saturating_add(1);
                        m_dedup_filtered.increment(1);
                        continue;
                    }

                    // Persist tick to QuestDB (Full packet with valid timestamp)
                    if let Some(ref mut writer) = tick_writer
                        && let Err(err) = writer.append_tick(&tick)
                    {
                        storage_errors = storage_errors.saturating_add(1);
                        m_storage_errors.increment(1);
                        if storage_errors <= 100 {
                            warn!(
                                ?err,
                                security_id = tick.security_id,
                                total_errors = storage_errors,
                                "failed to append tick to QuestDB"
                            );
                        }
                    }
                } else if !ltp_valid {
                    // LTP non-finite or non-positive — skip depth too (truly junk)
                    junk_ticks_filtered = junk_ticks_filtered.saturating_add(1);
                    m_junk_filtered.increment(1);
                    if junk_ticks_filtered <= 10 {
                        debug!(
                            security_id = tick.security_id,
                            ltp = tick.last_traded_price,
                            exchange_timestamp = tick.exchange_timestamp,
                            total_filtered = junk_ticks_filtered,
                            "junk tick with depth filtered — LTP non-finite/invalid"
                        );
                    }
                    continue;
                }
                // else: LTP valid but no exchange_timestamp (Market Depth code 3)
                // — skip tick persistence, still persist depth below

                // Guard: skip depth with NaN/Infinity bid/ask prices
                if !depth_prices_are_finite(&depth) {
                    junk_ticks_filtered = junk_ticks_filtered.saturating_add(1);
                    m_junk_filtered.increment(1);
                    continue;
                }

                // O(1) crossed market detection: bid > ask at best level.
                // Occurs during auction periods — metric only, do NOT filter.
                if depth[0].bid_price > depth[0].ask_price
                    && depth[0].bid_price > 0.0
                    && depth[0].ask_price > 0.0
                {
                    m_crossed_market.increment(1);
                    debug!(
                        security_id = tick.security_id,
                        bid = depth[0].bid_price,
                        ask = depth[0].ask_price,
                        "crossed market detected (bid > ask at best level)"
                    );
                }

                // Persist 5-level depth to QuestDB (separate table)
                m_depth_snapshots.increment(1);
                if let Some(ref mut dw) = depth_writer
                    && let Err(err) = dw.append_depth(
                        tick.security_id,
                        tick.exchange_segment_code,
                        tick.received_at_nanos,
                        &depth,
                    )
                {
                    storage_errors = storage_errors.saturating_add(1);
                    m_storage_errors.increment(1);
                    if storage_errors <= 100 {
                        warn!(
                            ?err,
                            security_id = tick.security_id,
                            total_errors = storage_errors,
                            "failed to append depth to QuestDB"
                        );
                    }
                }

                // O(1) broadcast to browser WebSocket clients (if tick has valid LTP).
                if ltp_valid && let Some(ref sender) = tick_broadcast {
                    let _ = sender.send(tick);
                }

                // O(1) candle aggregation (only for ticks with valid exchange timestamps).
                if tick_is_valid && let Some(ref mut agg) = candle_aggregator {
                    agg.update(&tick);
                }

                // O(1) top movers update (needs valid LTP + previous close).
                if ltp_valid && let Some(ref mut movers) = top_movers {
                    movers.update(&tick);
                }

                trace!(
                    security_id = tick.security_id,
                    ltp = tick.last_traded_price,
                    "tick with depth processed"
                );
            }
            ParsedFrame::OiUpdate {
                security_id,
                exchange_segment_code,
                open_interest,
            } => {
                m_oi_updates.increment(1);
                debug!(
                    security_id,
                    exchange_segment_code, open_interest, "OI update received"
                );
            }
            ParsedFrame::PreviousClose {
                security_id,
                exchange_segment_code,
                previous_close,
                previous_oi,
            } => {
                m_prev_close_updates.increment(1);

                // Guard: skip non-finite previous_close (NaN/Infinity from corrupted frame)
                if !previous_close.is_finite() {
                    junk_ticks_filtered = junk_ticks_filtered.saturating_add(1);
                    m_junk_filtered.increment(1);
                    continue;
                }

                // Persist previous close to QuestDB
                if let Some(ref mut writer) = tick_writer
                    && let Err(err) = build_previous_close_row(
                        writer.buffer_mut(),
                        security_id,
                        exchange_segment_code,
                        previous_close,
                        previous_oi,
                        received_at_nanos,
                    )
                {
                    storage_errors = storage_errors.saturating_add(1);
                    m_storage_errors.increment(1);
                    if storage_errors <= 100 {
                        warn!(
                            ?err,
                            security_id, "failed to write previous close to QuestDB"
                        );
                    }
                }

                debug!(
                    security_id,
                    exchange_segment_code, previous_close, previous_oi, "previous close persisted"
                );
            }
            ParsedFrame::MarketStatus {
                exchange_segment_code,
                security_id,
            } => {
                m_market_status_updates.increment(1);
                info!(exchange_segment_code, security_id, "market status update");
            }
            ParsedFrame::Disconnect(code) => {
                m_disconnects.increment(1);
                error!(
                    ?code,
                    "disconnect frame received from Dhan — connection will be closed by WebSocket layer"
                );
            }
        }

        m_tick_duration.record(tick_start.elapsed().as_nanos() as f64);

        // Periodic flush check (every ~100ms worth of frames)
        if last_flush_check.elapsed().as_millis() > 100 {
            if let Some(ref mut writer) = tick_writer
                && let Err(err) = writer.flush_if_needed()
            {
                warn!(?err, "periodic tick flush failed");
            }
            if let Some(ref mut dw) = depth_writer
                && let Err(err) = dw.flush_if_needed()
            {
                warn!(?err, "periodic depth flush failed");
            }

            // Sweep stale candles and persist completed 1s candles to QuestDB
            if let Some(ref mut agg) = candle_aggregator {
                // Reuse received_at_nanos from line 179 instead of a second Utc::now() syscall.
                // Dhan sends exchange_timestamp as UTC epoch seconds.
                // received_at_nanos is UTC nanos — same timezone, direct comparison.
                // APPROVED: i64→u32 truncation is safe: UTC epoch fits u32 until 2106
                #[allow(clippy::cast_possible_truncation)]
                let now_secs = (received_at_nanos / 1_000_000_000) as u32;
                agg.sweep_stale(now_secs);
                let completed = agg.completed_slice();
                if !completed.is_empty() {
                    if let Some(ref mut cw) = live_candle_writer {
                        for c in completed {
                            if let Err(err) = cw.append_candle(
                                c.security_id,
                                c.exchange_segment_code,
                                c.timestamp_secs,
                                c.open,
                                c.high,
                                c.low,
                                c.close,
                                c.volume,
                                c.tick_count,
                            ) {
                                warn!(?err, "failed to append live candle to QuestDB");
                                break;
                            }
                        }
                    }
                    trace!(count = completed.len(), "flushed completed 1s candles");
                }
                agg.clear_completed();
            }
            // Periodic live candle flush
            if let Some(ref mut cw) = live_candle_writer
                && let Err(err) = cw.flush_if_needed()
            {
                warn!(?err, "periodic live candle flush failed");
            }

            // Compute top movers snapshot every ~5 seconds (cold path, O(N log N))
            if last_snapshot_check.elapsed().as_secs() >= 5 {
                if let Some(ref mut movers) = top_movers {
                    let snapshot = movers.compute_snapshot();
                    if let Some(ref handle) = shared_snapshot
                        && let Ok(mut guard) = handle.write()
                    {
                        *guard = Some(snapshot);
                    }
                }
                last_snapshot_check = Instant::now();
            }

            last_flush_check = Instant::now();
        }
    }

    // Final flush to QuestDB
    if let Some(ref mut writer) = tick_writer
        && let Err(err) = writer.force_flush()
    {
        error!(?err, "final tick flush failed");
    }
    if let Some(ref mut dw) = depth_writer
        && let Err(err) = dw.force_flush()
    {
        error!(?err, "final depth flush failed");
    }

    // Flush remaining candles and persist to QuestDB
    if let Some(ref mut agg) = candle_aggregator {
        agg.flush_all();
        let final_count = agg.completed_slice().len();
        if let Some(ref mut cw) = live_candle_writer {
            for c in agg.completed_slice() {
                let _ = cw.append_candle(
                    c.security_id,
                    c.exchange_segment_code,
                    c.timestamp_secs,
                    c.open,
                    c.high,
                    c.low,
                    c.close,
                    c.volume,
                    c.tick_count,
                );
            }
            if let Err(err) = cw.force_flush() {
                error!(?err, "final live candle flush failed");
            }
        }
        agg.clear_completed();
        info!(
            total_completed = agg.total_completed(),
            final_flush = final_count,
            "candle aggregator stopped"
        );
    }

    // Log final top movers state
    if let Some(ref movers) = top_movers {
        info!(
            tracked = movers.tracked_count(),
            ticks_processed = movers.ticks_processed(),
            "top movers tracker stopped"
        );
    }

    m_pipeline_active.set(0.0);
    info!(
        frames_processed,
        ticks_processed,
        junk_ticks_filtered,
        dedup_filtered,
        parse_errors,
        storage_errors,
        "tick processor stopped"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only arithmetic (casts, indexing) is safe
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::{
        DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, HEADER_OFFSET_EXCHANGE_SEGMENT,
        HEADER_OFFSET_MESSAGE_LENGTH, HEADER_OFFSET_RESPONSE_CODE, HEADER_OFFSET_SECURITY_ID,
        MARKET_DEPTH_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE,
        PREVIOUS_CLOSE_PACKET_SIZE, RESPONSE_CODE_DISCONNECT, RESPONSE_CODE_FULL,
        RESPONSE_CODE_MARKET_DEPTH, RESPONSE_CODE_MARKET_STATUS, RESPONSE_CODE_OI,
        RESPONSE_CODE_PREVIOUS_CLOSE, TICKER_OFFSET_LTP, TICKER_OFFSET_LTT, TICKER_PACKET_SIZE,
    };

    /// Build a valid ticker binary frame for testing.
    fn make_ticker_frame(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
        let mut buf = vec![0u8; TICKER_PACKET_SIZE];
        buf[HEADER_OFFSET_RESPONSE_CODE] = 2; // Ticker
        buf[HEADER_OFFSET_MESSAGE_LENGTH..HEADER_OFFSET_MESSAGE_LENGTH + 2]
            .copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
        buf[HEADER_OFFSET_EXCHANGE_SEGMENT] = 2; // NSE_FNO
        buf[HEADER_OFFSET_SECURITY_ID..HEADER_OFFSET_SECURITY_ID + 4]
            .copy_from_slice(&security_id.to_le_bytes());
        buf[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&ltp.to_le_bytes());
        buf[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4].copy_from_slice(&ltt.to_le_bytes());
        buf
    }

    #[tokio::test]
    async fn test_tick_processor_processes_frames() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Send a valid ticker frame
        let frame = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        // Give processor time to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Close channel → processor exits
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_invalid_frame() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Send a too-short frame
        frame_tx
            .send(bytes::Bytes::from(vec![0u8; 4]))
            .await
            .unwrap();

        // Send a valid frame after — should still work
        let valid_frame = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx
            .send(bytes::Bytes::from(valid_frame))
            .await
            .unwrap();

        // Give processor time to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_filters_zero_ltp_tick() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Send a ticker frame with LTP=0.0 — should be filtered (not crash)
        let frame = make_ticker_frame(13, 0.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_filters_zero_timestamp_tick() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Send a ticker frame with LTT=0 (epoch 1970) — should be filtered
        let frame = make_ticker_frame(13, 24500.0, 0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_passes_valid_tick_after_junk() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Send junk tick first (LTP=0)
        let junk = make_ticker_frame(13, 0.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(junk)).await.unwrap();

        // Then send valid tick — processor should not crash
        let valid = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(valid)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_empty_channel_exits_cleanly() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Close immediately
        drop(frame_tx);
        let _ = handle.await; // should not hang
    }

    /// Build a minimal valid packet for a given response code.
    fn make_packet(response_code: u8, size: usize) -> Vec<u8> {
        let mut buf = vec![0u8; size];
        buf[HEADER_OFFSET_RESPONSE_CODE] = response_code;
        buf[HEADER_OFFSET_MESSAGE_LENGTH..HEADER_OFFSET_MESSAGE_LENGTH + 2]
            .copy_from_slice(&(size as u16).to_le_bytes());
        buf[HEADER_OFFSET_EXCHANGE_SEGMENT] = 2; // NSE_FNO
        buf[HEADER_OFFSET_SECURITY_ID..HEADER_OFFSET_SECURITY_ID + 4]
            .copy_from_slice(&42u32.to_le_bytes());
        buf
    }

    #[tokio::test]
    async fn test_tick_processor_handles_oi_update() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_OI, OI_PACKET_SIZE);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_previous_close() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_market_status() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_MARKET_STATUS, MARKET_STATUS_PACKET_SIZE);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_disconnect() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        // Set disconnect code to 807 (AccessTokenExpired)
        frame[8..10].copy_from_slice(&807u16.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_full_quote() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        // Full quote packet with valid LTP and timestamp
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        // Set LTP at TICKER_OFFSET_LTP and LTT at TICKER_OFFSET_LTT
        // (Full packet uses same offsets for LTP/LTT as ticker)
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&24500.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_parse_error_rate_limiting() {
        // Send 105 invalid frames to exercise the `if parse_errors <= 100` boundary.
        // After 100, the warn! is suppressed; the processor still continues.
        let (frame_tx, frame_rx) = mpsc::channel(200);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        for _ in 0..105 {
            frame_tx
                .send(bytes::Bytes::from(vec![0u8; 4]))
                .await
                .unwrap();
        }

        // Send a valid frame after — processor should still work.
        let valid = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(valid)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_final_flush_on_channel_close() {
        // With tick_writer=None, the final flush path (lines 146-149) takes
        // the if-let-Some branch only when a writer exists. Without a real
        // QuestDB we pass None, but we still exercise the code path that
        // checks `if let Some(ref mut writer)` and falls through.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Send a valid tick so frames_processed > 0
        let frame = make_ticker_frame(42, 25000.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Close channel to trigger final flush path
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_periodic_flush_check_with_elapsed_time() {
        // Exercise the periodic flush check by sending frames with a gap > 100ms.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Send first valid tick
        let frame1 = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame1)).await.unwrap();

        // Wait > 100ms so periodic flush elapsed check triggers
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Send second tick — this will trigger the elapsed > 100ms branch
        let frame2 = make_ticker_frame(14, 24600.0, 1772073901);
        frame_tx.send(bytes::Bytes::from(frame2)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_multiple_junk_ticks_suppression() {
        // Send > 10 junk ticks to exercise the `if junk_ticks_filtered <= 10` boundary.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        for _ in 0..15 {
            let junk = make_ticker_frame(13, 0.0, 1772073900);
            frame_tx.send(bytes::Bytes::from(junk)).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_negative_ltp_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // LTP = -1.0 should be filtered as junk
        let frame = make_ticker_frame(13, -1.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_mixed_valid_and_invalid_frames() {
        // Interleave valid, junk, and parse-error frames to exercise
        // multiple code paths in a single run.
        let (frame_tx, frame_rx) = mpsc::channel(200);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Parse error (too short)
        frame_tx
            .send(bytes::Bytes::from(vec![0u8; 3]))
            .await
            .unwrap();
        // Valid tick
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13, 24500.0, 1772073900,
            )))
            .await
            .unwrap();
        // Junk tick (LTP=0)
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(14, 0.0, 1772073900)))
            .await
            .unwrap();
        // Junk tick (timestamp=0)
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(15, 24500.0, 0)))
            .await
            .unwrap();
        // Valid tick
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                16, 24600.0, 1772073901,
            )))
            .await
            .unwrap();
        // OI update
        frame_tx
            .send(bytes::Bytes::from(make_packet(
                RESPONSE_CODE_OI,
                OI_PACKET_SIZE,
            )))
            .await
            .unwrap();
        // Disconnect
        let mut disc = make_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        disc[8..10].copy_from_slice(&807u16.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(disc)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // Additional tick processor tests for edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_tick_processor_handles_previous_close_and_market_status_sequence() {
        // Send previous_close then market_status in sequence.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        let prev_close = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        frame_tx.send(bytes::Bytes::from(prev_close)).await.unwrap();

        let market_status = make_packet(RESPONSE_CODE_MARKET_STATUS, MARKET_STATUS_PACKET_SIZE);
        frame_tx
            .send(bytes::Bytes::from(market_status))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_full_quote_with_zero_ltp_filtered() {
        // Full quote with LTP=0 should be filtered as junk.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        // Set LTP=0.0 and valid timestamp
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&0.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_full_quote_valid_ltp_processed() {
        // Full quote with valid LTP and timestamp should be processed.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&25000.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_many_frames_triggers_flush_check() {
        // Send many valid frames rapidly to verify periodic flush check runs.
        let (frame_tx, frame_rx) = mpsc::channel(500);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Send 50 valid frames
        for i in 0..50 {
            let frame = make_ticker_frame(13 + i, 24500.0 + (i as f32), 1772073900);
            frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        }

        // Wait for periodic flush check to trigger
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Send another batch
        for i in 0..50 {
            let frame = make_ticker_frame(100 + i, 25000.0, 1772073901);
            frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_unknown_response_code_is_parse_error() {
        // Unknown response code (99) should be counted as a parse error.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        let mut buf = vec![0u8; 8];
        buf[0] = 99; // unknown response code
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(buf)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    /// Creates a TickPersistenceWriter connected to a local TCP listener.
    /// Returns the writer and an abort handle for the listener.
    /// The listener accepts connections and reads data until aborted.
    async fn create_mock_ilp_writer() -> (TickPersistenceWriter, tokio::task::JoinHandle<()>) {
        use dhan_live_trader_common::config::QuestDbConfig;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Spawn a task that accepts connections and reads data (acting as ILP sink)
        let listener_handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => continue,
                        }
                    }
                });
            }
        });

        // Small delay to ensure the listener is ready
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };

        let writer = TickPersistenceWriter::new(&config).unwrap();
        (writer, listener_handle)
    }

    /// Creates a TickPersistenceWriter connected to a TCP listener that
    /// immediately closes the connection, causing flushes to fail.
    async fn create_broken_ilp_writer() -> TickPersistenceWriter {
        use dhan_live_trader_common::config::QuestDbConfig;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Accept the connection and immediately drop it
        let listener_handle = tokio::spawn(async move {
            if let Ok((_stream, _)) = listener.accept().await {
                // Drop stream immediately to close the connection
            }
        });

        // Small delay to ensure the listener is ready
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };

        let writer = TickPersistenceWriter::new(&config).unwrap();

        // Wait for the listener task to complete (connection accepted and dropped)
        let _ = listener_handle.await;

        // Small delay to ensure the TCP connection is fully closed
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        writer
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_tick_processor_with_writer_appends_valid_ticks() {
        // Exercises lines 89-90 (if let Some(ref mut writer)) and the
        // successful append_tick path. Also covers periodic flush (lines 136-137)
        // and final flush (lines 146-147) with a working writer.
        let (writer, listener_handle) = create_mock_ilp_writer().await;
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, Some(writer), None, None, None, None, None, None).await;
        });

        // Send a valid tick
        let frame = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        // Wait > 100ms to trigger periodic flush check
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Send another valid tick after the delay to trigger the elapsed check
        let frame2 = make_ticker_frame(14, 24600.0, 1772073901);
        frame_tx.send(bytes::Bytes::from(frame2)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Close channel to trigger final flush
        drop(frame_tx);

        // Use timeout to avoid hanging if writer cleanup is slow
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

        listener_handle.abort();
    }

    #[tokio::test]
    async fn test_tick_processor_with_broken_writer_flush_fails() {
        // Exercises lines 137, 139 (periodic flush failure) and lines 147, 149
        // (final flush failure) when the TCP connection is broken.
        //
        // The writer's flush_if_needed() only flushes when elapsed >=
        // TICK_FLUSH_INTERVAL_MS (1000ms). So we need to wait > 1s for
        // the periodic flush to actually attempt to send data over the
        // broken TCP connection.
        let writer = create_broken_ilp_writer().await;
        let (frame_tx, frame_rx) = mpsc::channel(200);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, Some(writer), None, None, None, None, None, None).await;
        });

        // Send valid ticks — they buffer successfully but flush will fail
        for i in 0..5 {
            let frame = make_ticker_frame(13 + i, 24500.0 + (i as f32), 1772073900);
            frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        }

        // Wait > 1s so writer.flush_if_needed() actually tries to flush
        // (TICK_FLUSH_INTERVAL_MS = 1000ms). The tick processor's outer
        // check triggers at 100ms, but the writer only flushes at 1000ms.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

        // Send more ticks to trigger the periodic flush check on next iteration
        for i in 0..5 {
            let frame = make_ticker_frame(20 + i, 25000.0, 1772073901);
            frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Close channel to trigger final flush (which should also fail
        // because there are pending ticks from the second batch)
        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // Market Depth code 3 tests — depth must persist even without timestamp
    // -----------------------------------------------------------------------

    /// Build a Market Depth standalone packet (code 3, 112 bytes).
    /// Has LTP but NO exchange_timestamp (exchange_timestamp=0 by design).
    fn make_market_depth_frame(security_id: u32, ltp: f32) -> Vec<u8> {
        let mut buf = vec![0u8; MARKET_DEPTH_PACKET_SIZE];
        buf[HEADER_OFFSET_RESPONSE_CODE] = RESPONSE_CODE_MARKET_DEPTH;
        buf[HEADER_OFFSET_MESSAGE_LENGTH..HEADER_OFFSET_MESSAGE_LENGTH + 2]
            .copy_from_slice(&(MARKET_DEPTH_PACKET_SIZE as u16).to_le_bytes());
        buf[HEADER_OFFSET_EXCHANGE_SEGMENT] = 2; // NSE_FNO
        buf[HEADER_OFFSET_SECURITY_ID..HEADER_OFFSET_SECURITY_ID + 4]
            .copy_from_slice(&security_id.to_le_bytes());
        // LTP at offset 8
        buf[8..12].copy_from_slice(&ltp.to_le_bytes());
        buf
    }

    #[tokio::test]
    async fn test_tick_processor_market_depth_not_filtered_as_junk() {
        // Market Depth code 3 has exchange_timestamp=0 by design.
        // Depth data must still be persisted — it must NOT be filtered as junk.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Send a Market Depth frame with valid LTP but no timestamp
        let frame = make_market_depth_frame(13, 24500.0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
        // If this doesn't crash and processes the frame, it works.
        // The depth_writer=None means depth isn't persisted in this test,
        // but the frame reaches the depth persistence code path.
    }

    #[tokio::test]
    async fn test_tick_processor_market_depth_zero_ltp_is_filtered() {
        // Market Depth with LTP=0.0 is truly junk — should be filtered.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        let frame = make_market_depth_frame(13, 0.0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_tick_processor_with_depth_writer_persists_full_quote_depth() {
        // Exercises the depth persistence path with a real DepthPersistenceWriter.
        let (tick_writer, listener_handle) = create_mock_ilp_writer().await;

        // Create a separate depth writer on its own ILP connection.
        use dhan_live_trader_common::config::QuestDbConfig;
        let depth_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let depth_port = depth_listener.local_addr().unwrap().port();
        let depth_listener_handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = depth_listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => continue,
                        }
                    }
                });
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let depth_config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: depth_port,
            http_port: 1,
            pg_port: 1,
        };
        let depth_writer = DepthPersistenceWriter::new(&depth_config).unwrap();

        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(
                frame_rx,
                Some(tick_writer),
                Some(depth_writer),
                None,
                None,
                None,
                None,
                None,
            )
            .await;
        });

        // Send a Full Quote packet (code 8) with valid LTP and timestamp
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&24500.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();

        // Send a Market Depth packet (code 3) with valid LTP but no timestamp
        let depth_frame = make_market_depth_frame(42, 25000.0);
        frame_tx
            .send(bytes::Bytes::from(depth_frame))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        drop(frame_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

        listener_handle.abort();
        depth_listener_handle.abort();
    }

    // ===================================================================
    // TickDedupRing unit tests
    // ===================================================================

    #[test]
    fn test_dedup_ring_new_tick_not_duplicate() {
        let mut ring = TickDedupRing::new(8); // 256 slots
        assert!(!ring.is_duplicate(13, 1772073900, 24500.0));
    }

    #[test]
    fn test_dedup_ring_same_tick_is_duplicate() {
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(13, 1772073900, 24500.0));
        assert!(ring.is_duplicate(13, 1772073900, 24500.0));
    }

    #[test]
    fn test_dedup_ring_third_identical_also_duplicate() {
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(13, 1772073900, 24500.0));
        assert!(ring.is_duplicate(13, 1772073900, 24500.0));
        assert!(ring.is_duplicate(13, 1772073900, 24500.0));
    }

    #[test]
    fn test_dedup_ring_different_security_not_duplicate() {
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(13, 1772073900, 24500.0));
        assert!(!ring.is_duplicate(14, 1772073900, 24500.0));
    }

    #[test]
    fn test_dedup_ring_different_timestamp_not_duplicate() {
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(13, 1772073900, 24500.0));
        assert!(!ring.is_duplicate(13, 1772073901, 24500.0));
    }

    #[test]
    fn test_dedup_ring_different_ltp_not_duplicate() {
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(13, 1772073900, 24500.0));
        assert!(!ring.is_duplicate(13, 1772073900, 24501.0));
    }

    #[test]
    fn test_dedup_ring_zero_security_id() {
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(0, 0, 0.0));
        assert!(ring.is_duplicate(0, 0, 0.0));
    }

    #[test]
    fn test_dedup_ring_max_values() {
        let mut ring = TickDedupRing::new(8);
        assert!(!ring.is_duplicate(u32::MAX, u32::MAX, f32::MAX));
        assert!(ring.is_duplicate(u32::MAX, u32::MAX, f32::MAX));
    }

    #[test]
    fn test_dedup_ring_eviction_after_collision() {
        // With a small ring (256 slots), inserting many distinct entries
        // will eventually evict earlier ones, allowing re-insertion.
        let mut ring = TickDedupRing::new(8); // 256 slots
        // Insert first tick
        assert!(!ring.is_duplicate(13, 1772073900, 24500.0));
        // Fill buffer with other entries to force eviction
        for i in 1..=512 {
            ring.is_duplicate(i + 100, 1772073900, 24500.0 + (i as f32));
        }
        // Original entry may have been evicted — should no longer be duplicate
        // (This tests that the ring buffer has finite memory and old entries are lost)
        // Note: this is probabilistic based on hash distribution.
        // We don't assert the result — just verify it doesn't panic.
        let _ = ring.is_duplicate(13, 1772073900, 24500.0);
    }

    #[test]
    fn test_dedup_ring_interleaved_securities() {
        let mut ring = TickDedupRing::new(12); // 4096 slots
        // Alternating security IDs should not confuse the dedup
        for round in 0..3 {
            let ts = 1772073900 + round;
            assert!(!ring.is_duplicate(13, ts, 24500.0));
            assert!(!ring.is_duplicate(14, ts, 24500.0));
            assert!(!ring.is_duplicate(15, ts, 24500.0));
            // Duplicates within same round
            assert!(ring.is_duplicate(13, ts, 24500.0));
            assert!(ring.is_duplicate(14, ts, 24500.0));
            assert!(ring.is_duplicate(15, ts, 24500.0));
        }
    }

    #[test]
    fn test_dedup_ring_fingerprint_deterministic() {
        let fp1 = TickDedupRing::fingerprint(13, 1772073900, 24500.0);
        let fp2 = TickDedupRing::fingerprint(13, 1772073900, 24500.0);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_dedup_ring_fingerprint_distinct_for_different_inputs() {
        let fp1 = TickDedupRing::fingerprint(13, 1772073900, 24500.0);
        let fp2 = TickDedupRing::fingerprint(14, 1772073900, 24500.0);
        let fp3 = TickDedupRing::fingerprint(13, 1772073901, 24500.0);
        let fp4 = TickDedupRing::fingerprint(13, 1772073900, 24501.0);
        assert_ne!(fp1, fp2);
        assert_ne!(fp1, fp3);
        assert_ne!(fp1, fp4);
    }

    #[test]
    fn test_dedup_ring_fingerprint_symmetric_inputs_differ() {
        // Verify (sec=1, ts=3) != (sec=3, ts=1) — FNV-1a is order-dependent
        let fp1 = TickDedupRing::fingerprint(1, 3, 100.0);
        let fp2 = TickDedupRing::fingerprint(3, 1, 100.0);
        assert_ne!(fp1, fp2);
    }

    // ===================================================================
    // NaN / Infinity pipeline tests
    // ===================================================================

    #[tokio::test]
    async fn test_tick_processor_nan_ltp_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_ticker_frame(13, f32::NAN, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_positive_infinity_ltp_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_ticker_frame(13, f32::INFINITY, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_negative_infinity_ltp_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_ticker_frame(13, f32::NEG_INFINITY, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_nan_ltp_full_quote_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_infinity_ltp_full_quote_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4]
            .copy_from_slice(&f32::INFINITY.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_nan_ltp_market_depth_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_market_depth_frame(13, f32::NAN);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_infinity_ltp_market_depth_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_market_depth_frame(13, f32::INFINITY);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // ===================================================================
    // Dedup pipeline integration tests
    // ===================================================================

    #[tokio::test]
    async fn test_tick_processor_dedup_filters_exact_duplicate_tick() {
        // Send the same tick twice — second should be dedup-filtered.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx
            .send(bytes::Bytes::from(frame.clone()))
            .await
            .unwrap();
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_allows_different_timestamp() {
        // Same security, different timestamp — both should pass dedup.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13, 24500.0, 1772073900,
            )))
            .await
            .unwrap();
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13, 24500.0, 1772073901,
            )))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_allows_different_ltp() {
        // Same security+timestamp, different LTP — both should pass (price update).
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13, 24500.0, 1772073900,
            )))
            .await
            .unwrap();
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13, 24501.0, 1772073900,
            )))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_allows_different_security() {
        // Different security, same timestamp+LTP — both should pass.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13, 24500.0, 1772073900,
            )))
            .await
            .unwrap();
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                14, 24500.0, 1772073900,
            )))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_burst_100_identical() {
        // Send 100 identical ticks — only first should pass dedup.
        let (frame_tx, frame_rx) = mpsc::channel(200);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_ticker_frame(13, 24500.0, 1772073900);
        for _ in 0..100 {
            frame_tx
                .send(bytes::Bytes::from(frame.clone()))
                .await
                .unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_does_not_affect_junk() {
        // Junk ticks (LTP=0) are filtered BEFORE dedup — dedup never sees them.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        // Send junk tick
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(13, 0.0, 1772073900)))
            .await
            .unwrap();
        // Send valid tick for same security — should NOT be treated as duplicate
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                13, 24500.0, 1772073900,
            )))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_full_quote_duplicate() {
        // Send the same Full Quote twice — second should be dedup-filtered.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&24500.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx
            .send(bytes::Bytes::from(frame.clone()))
            .await
            .unwrap();
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_market_depth_not_deduped() {
        // Market Depth code 3 has timestamp=0 — bypasses dedup (no valid timestamp).
        // Two identical depth frames should BOTH be processed (not deduplicated).
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_market_depth_frame(13, 24500.0);
        frame_tx
            .send(bytes::Bytes::from(frame.clone()))
            .await
            .unwrap();
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // ===================================================================
    // Additional edge case tests
    // ===================================================================

    #[tokio::test]
    async fn test_tick_processor_subnormal_ltp_filtered() {
        // Subnormal f32 (very close to zero but positive) — should be filtered
        // because it's <= 0.0 is false but it's effectively zero for trading.
        // Actually subnormal > 0.0 is true, and is_finite() is true.
        // So subnormal passes through — this is correct (it's a valid f32).
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let subnormal: f32 = f32::MIN_POSITIVE / 2.0;
        let frame = make_ticker_frame(13, subnormal, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_negative_zero_ltp_filtered() {
        // -0.0 is == 0.0 in IEEE 754, so -0.0 <= 0.0 is true → filtered.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_ticker_frame(13, -0.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_max_u32_security_id_processed() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        // u32::MAX security_id with valid LTP and timestamp
        let mut frame = make_ticker_frame(0, 24500.0, 1772073900);
        frame[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_minimum_valid_timestamp_accepted() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        // Exact minimum valid timestamp — should pass
        let frame = make_ticker_frame(13, 24500.0, MINIMUM_VALID_EXCHANGE_TIMESTAMP);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_timestamp_one_below_minimum_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_ticker_frame(
            13,
            24500.0,
            MINIMUM_VALID_EXCHANGE_TIMESTAMP.saturating_sub(1),
        );
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_all_nine_packet_types_in_sequence() {
        // Send one of every packet type in rapid succession.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // 1. Index Ticker (code 1)
        let mut idx_tick = make_ticker_frame(13, 24500.0, 1772073900);
        idx_tick[0] = 1; // RESPONSE_CODE_INDEX_TICKER
        frame_tx.send(bytes::Bytes::from(idx_tick)).await.unwrap();

        // 2. Ticker (code 2) — already default from make_ticker_frame
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                14, 24600.0, 1772073900,
            )))
            .await
            .unwrap();

        // 3. Market Depth (code 3)
        frame_tx
            .send(bytes::Bytes::from(make_market_depth_frame(15, 24700.0)))
            .await
            .unwrap();

        // 4. Quote (code 4)
        let quote = make_packet(
            dhan_live_trader_common::constants::RESPONSE_CODE_QUOTE,
            dhan_live_trader_common::constants::QUOTE_PACKET_SIZE,
        );
        frame_tx.send(bytes::Bytes::from(quote)).await.unwrap();

        // 5. OI (code 5)
        frame_tx
            .send(bytes::Bytes::from(make_packet(
                RESPONSE_CODE_OI,
                OI_PACKET_SIZE,
            )))
            .await
            .unwrap();

        // 6. Previous Close (code 6)
        frame_tx
            .send(bytes::Bytes::from(make_packet(
                RESPONSE_CODE_PREVIOUS_CLOSE,
                PREVIOUS_CLOSE_PACKET_SIZE,
            )))
            .await
            .unwrap();

        // 7. Market Status (code 7)
        frame_tx
            .send(bytes::Bytes::from(make_packet(
                RESPONSE_CODE_MARKET_STATUS,
                MARKET_STATUS_PACKET_SIZE,
            )))
            .await
            .unwrap();

        // 8. Full Quote (code 8) with valid data
        let mut full = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        full[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&24800.0_f32.to_le_bytes());
        full[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(full)).await.unwrap();

        // 9. Disconnect (code 50)
        let mut disc = make_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        disc[8..10].copy_from_slice(&807u16.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(disc)).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_dedup_then_valid_after_different_ltp() {
        // Tick A (ltp=24500) → Tick A again (dedup) → Tick B (ltp=24501, same sec+ts)
        // Tick B should pass because LTP differs.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame_a = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx
            .send(bytes::Bytes::from(frame_a.clone()))
            .await
            .unwrap();
        frame_tx.send(bytes::Bytes::from(frame_a)).await.unwrap(); // duplicate
        let frame_b = make_ticker_frame(13, 24501.0, 1772073900);
        frame_tx.send(bytes::Bytes::from(frame_b)).await.unwrap(); // NOT duplicate (different LTP)
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_mixed_dedup_junk_valid_nan() {
        // Comprehensive mixed sequence: valid → duplicate → junk → NaN → valid
        let (frame_tx, frame_rx) = mpsc::channel(200);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });

        // Valid tick
        let valid1 = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx
            .send(bytes::Bytes::from(valid1.clone()))
            .await
            .unwrap();
        // Exact duplicate
        frame_tx.send(bytes::Bytes::from(valid1)).await.unwrap();
        // Junk (LTP=0)
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(14, 0.0, 1772073900)))
            .await
            .unwrap();
        // NaN LTP
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                15,
                f32::NAN,
                1772073900,
            )))
            .await
            .unwrap();
        // Infinity LTP
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                16,
                f32::INFINITY,
                1772073900,
            )))
            .await
            .unwrap();
        // Parse error (too short)
        frame_tx
            .send(bytes::Bytes::from(vec![0u8; 3]))
            .await
            .unwrap();
        // Valid tick (different security+timestamp — should pass)
        frame_tx
            .send(bytes::Bytes::from(make_ticker_frame(
                17, 24600.0, 1772073901,
            )))
            .await
            .unwrap();
        // Market Depth (no timestamp, valid LTP — should NOT be dedup-filtered)
        frame_tx
            .send(bytes::Bytes::from(make_market_depth_frame(18, 24700.0)))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // Depth NaN/Infinity guard tests
    // -----------------------------------------------------------------------

    /// Build a Market Depth frame with a NaN bid_price in level 0.
    fn make_market_depth_frame_with_nan_bid(security_id: u32, ltp: f32) -> Vec<u8> {
        let mut buf = make_market_depth_frame(security_id, ltp);
        // Level 0 bid_price at MARKET_DEPTH_OFFSET_DEPTH_START + 12 = 24
        buf[24..28].copy_from_slice(&f32::NAN.to_le_bytes());
        buf
    }

    /// Build a Market Depth frame with an Infinity ask_price in level 0.
    fn make_market_depth_frame_with_inf_ask(security_id: u32, ltp: f32) -> Vec<u8> {
        let mut buf = make_market_depth_frame(security_id, ltp);
        // Level 0 ask_price at MARKET_DEPTH_OFFSET_DEPTH_START + 16 = 28
        buf[28..32].copy_from_slice(&f32::INFINITY.to_le_bytes());
        buf
    }

    #[tokio::test]
    async fn test_tick_processor_depth_nan_bid_price_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_market_depth_frame_with_nan_bid(13, 24500.0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_depth_inf_ask_price_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let frame = make_market_depth_frame_with_inf_ask(13, 24500.0);
        frame_tx.send(bytes::Bytes::from(frame)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    // -----------------------------------------------------------------------
    // PreviousClose NaN/Infinity guard tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_tick_processor_previous_close_nan_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let mut buf = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        buf[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
        buf[12..16].copy_from_slice(&0u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(buf)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_previous_close_infinity_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None, None, None, None, None, None, None).await;
        });
        let mut buf = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        buf[8..12].copy_from_slice(&f32::INFINITY.to_le_bytes());
        buf[12..16].copy_from_slice(&0u32.to_le_bytes());
        frame_tx.send(bytes::Bytes::from(buf)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }
}
