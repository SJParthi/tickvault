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
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use dhan_live_trader_common::constants::MINIMUM_VALID_EXCHANGE_TIMESTAMP;

use crate::parser::dispatch_frame;
use crate::parser::types::ParsedFrame;

use dhan_live_trader_storage::tick_persistence::TickPersistenceWriter;

/// Runs the tick processing pipeline until the frame receiver closes.
///
/// This is designed to run as a single `tokio::spawn` task.
///
/// # Arguments
/// * `frame_receiver` — raw binary frames from WebSocket pool
/// * `tick_writer` — batched QuestDB ILP writer (None if QuestDB unavailable)
pub async fn run_tick_processor(
    mut frame_receiver: mpsc::Receiver<Vec<u8>>,
    mut tick_writer: Option<TickPersistenceWriter>,
) {
    // Grab metric handles once before the hot loop — O(1) per tick after this.
    // These are no-ops if no metrics recorder is installed (e.g., in tests).
    let m_frames = counter!("dlt_frames_processed_total");
    let m_ticks = counter!("dlt_ticks_processed_total");
    let m_parse_errors = counter!("dlt_parse_errors_total");
    let m_storage_errors = counter!("dlt_storage_errors_total");
    let m_junk_filtered = counter!("dlt_junk_ticks_filtered_total");
    let m_tick_duration = histogram!("dlt_tick_processing_duration_ns");
    let m_pipeline_active = gauge!("dlt_pipeline_active");

    let mut frames_processed: u64 = 0;
    let mut ticks_processed: u64 = 0;
    let mut parse_errors: u64 = 0;
    let mut storage_errors: u64 = 0;
    let mut junk_ticks_filtered: u64 = 0;
    let mut last_flush_check = Instant::now();

    m_pipeline_active.set(1.0);
    info!("tick processor started");

    while let Some(raw_frame) = frame_receiver.recv().await {
        let tick_start = Instant::now();
        frames_processed += 1;
        m_frames.increment(1);
        let received_at_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

        // Parse the binary frame
        let parsed = match dispatch_frame(&raw_frame, received_at_nanos) {
            Ok(frame) => frame,
            Err(err) => {
                parse_errors += 1;
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
            ParsedFrame::Tick(tick) | ParsedFrame::TickWithDepth(tick, _) => {
                ticks_processed += 1;
                m_ticks.increment(1);

                // Filter junk ticks: initialization/heartbeat frames from Dhan
                // have LTP=0.0 and/or exchange_timestamp=0 (epoch 1970-01-01).
                // These MUST NOT be persisted.
                if tick.last_traded_price <= 0.0
                    || tick.exchange_timestamp < MINIMUM_VALID_EXCHANGE_TIMESTAMP
                {
                    junk_ticks_filtered += 1;
                    m_junk_filtered.increment(1);
                    if junk_ticks_filtered <= 10 {
                        debug!(
                            security_id = tick.security_id,
                            ltp = tick.last_traded_price,
                            exchange_timestamp = tick.exchange_timestamp,
                            total_filtered = junk_ticks_filtered,
                            "junk tick filtered — LTP or timestamp invalid"
                        );
                    }
                    continue;
                }

                // Persist tick to QuestDB
                if let Some(ref mut writer) = tick_writer
                    && let Err(err) = writer.append_tick(&tick)
                {
                    storage_errors += 1;
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

                trace!(
                    security_id = tick.security_id,
                    ltp = tick.last_traded_price,
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

        m_tick_duration.record(tick_start.elapsed().as_nanos() as f64);

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

    // Final tick flush to QuestDB
    if let Some(ref mut writer) = tick_writer
        && let Err(err) = writer.force_flush()
    {
        error!(?err, "final tick flush failed");
    }

    m_pipeline_active.set(0.0);
    info!(
        frames_processed,
        ticks_processed,
        junk_ticks_filtered,
        parse_errors,
        storage_errors,
        "tick processor stopped"
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

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });

        // Send a valid ticker frame
        let frame = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(frame).await.unwrap();

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
            run_tick_processor(frame_rx, None).await;
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
    async fn test_tick_processor_filters_zero_ltp_tick() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });

        // Send a ticker frame with LTP=0.0 — should be filtered (not crash)
        let frame = make_ticker_frame(13, 0.0, 1772073900);
        frame_tx.send(frame).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_filters_zero_timestamp_tick() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });

        // Send a ticker frame with LTT=0 (epoch 1970) — should be filtered
        let frame = make_ticker_frame(13, 24500.0, 0);
        frame_tx.send(frame).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_passes_valid_tick_after_junk() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });

        // Send junk tick first (LTP=0)
        let junk = make_ticker_frame(13, 0.0, 1772073900);
        frame_tx.send(junk).await.unwrap();

        // Then send valid tick — processor should not crash
        let valid = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(valid).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_empty_channel_exits_cleanly() {
        let (frame_tx, frame_rx) = mpsc::channel(100);

        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });

        // Close immediately
        drop(frame_tx);
        let _ = handle.await; // should not hang
    }
}
