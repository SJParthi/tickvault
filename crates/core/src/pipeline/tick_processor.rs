//! Main tick processing loop — the heart of the pipeline.
//!
//! Consumes raw WebSocket binary frames, parses them, stores ticks,
//! aggregates candles, and broadcasts finalized candles to API clients.
//!
//! # Performance
//! - O(1) per tick per interval
//! - Zero heap allocation on hot path (ArrayVec for candles)
//! - Batched QuestDB writes (flush every 1000 rows or 1 second)

use std::time::Instant;

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, trace, warn};

use dhan_live_trader_common::tick_types::{CandleBroadcastMessage, TickInterval, Timeframe};

use crate::candle::CandleManager;
use crate::parser::dispatch_frame;
use crate::parser::types::ParsedFrame;

use dhan_live_trader_storage::tick_persistence::TickPersistenceWriter;

/// Runs the tick processing pipeline until the frame receiver closes.
///
/// This is designed to run as a single `tokio::spawn` task.
///
/// # Arguments
/// * `frame_receiver` — raw binary frames from WebSocket pool
/// * `candle_broadcast` — broadcast sender for finalized candles (to API WebSocket clients)
/// * `tick_writer` — batched QuestDB ILP writer (None if QuestDB unavailable)
/// * `timeframes` — active time-based intervals
/// * `tick_intervals` — active tick-count-based intervals
pub async fn run_tick_processor(
    mut frame_receiver: mpsc::Receiver<Vec<u8>>,
    candle_broadcast: broadcast::Sender<CandleBroadcastMessage>,
    mut tick_writer: Option<TickPersistenceWriter>,
    timeframes: &[Timeframe],
    tick_intervals: &[TickInterval],
) {
    let mut candle_manager = CandleManager::new(timeframes, tick_intervals);

    let mut frames_processed: u64 = 0;
    let mut ticks_processed: u64 = 0;
    let mut candles_finalized: u64 = 0;
    let mut parse_errors: u64 = 0;
    let mut storage_errors: u64 = 0;
    let mut last_flush_check = Instant::now();

    info!(
        time_intervals = timeframes.len(),
        tick_intervals = tick_intervals.len(),
        "tick processor started"
    );

    while let Some(raw_frame) = frame_receiver.recv().await {
        frames_processed += 1;
        let received_at_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

        // Parse the binary frame
        let parsed = match dispatch_frame(&raw_frame, received_at_nanos) {
            Ok(frame) => frame,
            Err(err) => {
                parse_errors += 1;
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
            ParsedFrame::Tick(tick) | ParsedFrame::TickWithDepth(tick, _) => {
                ticks_processed += 1;

                // 1. Persist tick to QuestDB
                if let Some(ref mut writer) = tick_writer
                    && let Err(err) = writer.append_tick(&tick)
                {
                    storage_errors += 1;
                    if storage_errors <= 100 {
                        warn!(
                            ?err,
                            security_id = tick.security_id,
                            total_errors = storage_errors,
                            "failed to append tick to QuestDB"
                        );
                    }
                }

                // 2. Feed tick to candle aggregator
                let finalized_candles = candle_manager.process_tick(&tick);

                // 3. Broadcast finalized candles to API subscribers
                for candle in &finalized_candles {
                    candles_finalized += 1;
                    // Determine the interval label from the candle.
                    // The CandleManager doesn't track interval labels, so we
                    // use security_id lookup. For now, broadcast with "auto" label.
                    // The API layer handles interval filtering per subscriber.
                    let msg = CandleBroadcastMessage {
                        interval: String::new(), // populated by API layer based on subscription
                        candle: *candle,
                    };
                    // broadcast::send only fails if there are no receivers — that's fine.
                    let _ = candle_broadcast.send(msg);
                }

                trace!(
                    security_id = tick.security_id,
                    ltp = tick.last_traded_price,
                    candles = finalized_candles.len(),
                    "tick processed"
                );
            }
            ParsedFrame::OiUpdate {
                security_id,
                open_interest,
                ..
            } => {
                debug!(security_id, open_interest, "OI update received");
            }
            ParsedFrame::PreviousClose {
                security_id,
                previous_close,
                ..
            } => {
                debug!(security_id, previous_close, "previous close received");
            }
            ParsedFrame::MarketStatus {
                exchange_segment_code,
                security_id,
            } => {
                info!(exchange_segment_code, security_id, "market status update");
            }
            ParsedFrame::Disconnect(code) => {
                warn!(?code, "disconnect frame received from Dhan");
            }
        }

        // Periodic flush check (every ~100ms worth of frames)
        if last_flush_check.elapsed().as_millis() > 100 {
            if let Some(ref mut writer) = tick_writer
                && let Err(err) = writer.flush_if_needed()
            {
                warn!(?err, "periodic tick flush failed");
            }
            last_flush_check = Instant::now();
        }
    }

    // Channel closed — final flush
    if let Some(ref mut writer) = tick_writer
        && let Err(err) = writer.force_flush()
    {
        error!(?err, "final tick flush failed");
    }

    info!(
        frames_processed,
        ticks_processed, candles_finalized, parse_errors, storage_errors, "tick processor stopped"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::{
        HEADER_OFFSET_EXCHANGE_SEGMENT, HEADER_OFFSET_MESSAGE_LENGTH, HEADER_OFFSET_RESPONSE_CODE,
        HEADER_OFFSET_SECURITY_ID, TICKER_OFFSET_LTP, TICKER_OFFSET_LTT, TICKER_PACKET_SIZE,
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
        let (candle_tx, mut candle_rx) = broadcast::channel(100);

        let timeframes = vec![Timeframe::S1];
        let tick_intervals = vec![TickInterval::T1];

        // Spawn processor
        let handle = tokio::spawn(async move {
            run_tick_processor(
                frame_rx,
                candle_tx,
                None, // no QuestDB writer
                &timeframes,
                &tick_intervals,
            )
            .await;
        });

        // Send a ticker frame
        let frame = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(frame).await.unwrap();

        // T1 should produce a candle
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), candle_rx.recv())
            .await
            .expect("should receive candle within 2s")
            .expect("broadcast recv should not fail");

        assert_eq!(msg.candle.security_id, 13);
        assert!((msg.candle.open - 24500.0).abs() < 0.1);

        // Close channel → processor exits
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_invalid_frame() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let (candle_tx, _candle_rx) = broadcast::channel(100);

        let timeframes = vec![Timeframe::M1];
        let tick_intervals = vec![];

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, candle_tx, None, &timeframes, &tick_intervals).await;
        });

        // Send a too-short frame
        frame_tx.send(vec![0u8; 4]).await.unwrap();

        // Send a valid frame after — should still work
        let valid_frame = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(valid_frame).await.unwrap();

        // Give processor time to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_empty_channel_exits_cleanly() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let (candle_tx, _) = broadcast::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, candle_tx, None, &[], &[]).await;
        });

        // Close immediately
        drop(frame_tx);
        let _ = handle.await; // should not hang
    }
}
