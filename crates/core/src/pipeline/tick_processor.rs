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
    let mut frames_processed: u64 = 0;
    let mut ticks_processed: u64 = 0;
    let mut parse_errors: u64 = 0;
    let mut storage_errors: u64 = 0;
    let mut junk_ticks_filtered: u64 = 0;
    let mut last_flush_check = Instant::now();

    info!("tick processor started");

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

                // Filter junk ticks: initialization/heartbeat frames from Dhan
                // have LTP=0.0 and/or exchange_timestamp=0 (epoch 1970-01-01).
                // These MUST NOT be persisted.
                if tick.last_traded_price <= 0.0
                    || tick.exchange_timestamp < MINIMUM_VALID_EXCHANGE_TIMESTAMP
                {
                    junk_ticks_filtered += 1;
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
        DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, HEADER_OFFSET_EXCHANGE_SEGMENT,
        HEADER_OFFSET_MESSAGE_LENGTH, HEADER_OFFSET_RESPONSE_CODE, HEADER_OFFSET_SECURITY_ID,
        MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE, PREVIOUS_CLOSE_PACKET_SIZE,
        RESPONSE_CODE_DISCONNECT, RESPONSE_CODE_FULL, RESPONSE_CODE_MARKET_STATUS,
        RESPONSE_CODE_OI, RESPONSE_CODE_PREVIOUS_CLOSE, TICKER_OFFSET_LTP, TICKER_OFFSET_LTT,
        TICKER_PACKET_SIZE,
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
            run_tick_processor(frame_rx, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_OI, OI_PACKET_SIZE);
        frame_tx.send(frame).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_previous_close() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        frame_tx.send(frame).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_market_status() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });
        let frame = make_packet(RESPONSE_CODE_MARKET_STATUS, MARKET_STATUS_PACKET_SIZE);
        frame_tx.send(frame).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_disconnect() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });
        let mut frame = make_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        // Set disconnect code to 807 (AccessTokenExpired)
        frame[8..10].copy_from_slice(&807u16.to_le_bytes());
        frame_tx.send(frame).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_handles_full_quote() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });
        // Full quote packet with valid LTP and timestamp
        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        // Set LTP at TICKER_OFFSET_LTP and LTT at TICKER_OFFSET_LTT
        // (Full packet uses same offsets for LTP/LTT as ticker)
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&24500.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx.send(frame).await.unwrap();
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
            run_tick_processor(frame_rx, None).await;
        });

        for _ in 0..105 {
            frame_tx.send(vec![0u8; 4]).await.unwrap();
        }

        // Send a valid frame after — processor should still work.
        let valid = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(valid).await.unwrap();

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
            run_tick_processor(frame_rx, None).await;
        });

        // Send a valid tick so frames_processed > 0
        let frame = make_ticker_frame(42, 25000.0, 1772073900);
        frame_tx.send(frame).await.unwrap();

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
            run_tick_processor(frame_rx, None).await;
        });

        // Send first valid tick
        let frame1 = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(frame1).await.unwrap();

        // Wait > 100ms so periodic flush elapsed check triggers
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Send second tick — this will trigger the elapsed > 100ms branch
        let frame2 = make_ticker_frame(14, 24600.0, 1772073901);
        frame_tx.send(frame2).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_multiple_junk_ticks_suppression() {
        // Send > 10 junk ticks to exercise the `if junk_ticks_filtered <= 10` boundary.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });

        for _ in 0..15 {
            let junk = make_ticker_frame(13, 0.0, 1772073900);
            frame_tx.send(junk).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_negative_ltp_filtered() {
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });

        // LTP = -1.0 should be filtered as junk
        let frame = make_ticker_frame(13, -1.0, 1772073900);
        frame_tx.send(frame).await.unwrap();

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
            run_tick_processor(frame_rx, None).await;
        });

        // Parse error (too short)
        frame_tx.send(vec![0u8; 3]).await.unwrap();
        // Valid tick
        frame_tx
            .send(make_ticker_frame(13, 24500.0, 1772073900))
            .await
            .unwrap();
        // Junk tick (LTP=0)
        frame_tx
            .send(make_ticker_frame(14, 0.0, 1772073900))
            .await
            .unwrap();
        // Junk tick (timestamp=0)
        frame_tx
            .send(make_ticker_frame(15, 24500.0, 0))
            .await
            .unwrap();
        // Valid tick
        frame_tx
            .send(make_ticker_frame(16, 24600.0, 1772073901))
            .await
            .unwrap();
        // OI update
        frame_tx
            .send(make_packet(RESPONSE_CODE_OI, OI_PACKET_SIZE))
            .await
            .unwrap();
        // Disconnect
        let mut disc = make_packet(RESPONSE_CODE_DISCONNECT, DISCONNECT_PACKET_SIZE);
        disc[8..10].copy_from_slice(&807u16.to_le_bytes());
        frame_tx.send(disc).await.unwrap();

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
            run_tick_processor(frame_rx, None).await;
        });

        let prev_close = make_packet(RESPONSE_CODE_PREVIOUS_CLOSE, PREVIOUS_CLOSE_PACKET_SIZE);
        frame_tx.send(prev_close).await.unwrap();

        let market_status = make_packet(RESPONSE_CODE_MARKET_STATUS, MARKET_STATUS_PACKET_SIZE);
        frame_tx.send(market_status).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_full_quote_with_zero_ltp_filtered() {
        // Full quote with LTP=0 should be filtered as junk.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });

        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        // Set LTP=0.0 and valid timestamp
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&0.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx.send(frame).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_full_quote_valid_ltp_processed() {
        // Full quote with valid LTP and timestamp should be processed.
        let (frame_tx, frame_rx) = mpsc::channel(100);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });

        let mut frame = make_packet(RESPONSE_CODE_FULL, FULL_QUOTE_PACKET_SIZE);
        frame[TICKER_OFFSET_LTP..TICKER_OFFSET_LTP + 4].copy_from_slice(&25000.0_f32.to_le_bytes());
        frame[TICKER_OFFSET_LTT..TICKER_OFFSET_LTT + 4]
            .copy_from_slice(&1772073900_u32.to_le_bytes());
        frame_tx.send(frame).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(frame_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_tick_processor_many_frames_triggers_flush_check() {
        // Send many valid frames rapidly to verify periodic flush check runs.
        let (frame_tx, frame_rx) = mpsc::channel(500);
        let handle = tokio::spawn(async move {
            run_tick_processor(frame_rx, None).await;
        });

        // Send 50 valid frames
        for i in 0..50 {
            let frame = make_ticker_frame(13 + i, 24500.0 + (i as f32), 1772073900);
            frame_tx.send(frame).await.unwrap();
        }

        // Wait for periodic flush check to trigger
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Send another batch
        for i in 0..50 {
            let frame = make_ticker_frame(100 + i, 25000.0, 1772073901);
            frame_tx.send(frame).await.unwrap();
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
            run_tick_processor(frame_rx, None).await;
        });

        let mut buf = vec![0u8; 8];
        buf[0] = 99; // unknown response code
        buf[1..3].copy_from_slice(&8u16.to_le_bytes());
        buf[3] = 2;
        buf[4..8].copy_from_slice(&42u32.to_le_bytes());
        frame_tx.send(buf).await.unwrap();

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
            run_tick_processor(frame_rx, Some(writer)).await;
        });

        // Send a valid tick
        let frame = make_ticker_frame(13, 24500.0, 1772073900);
        frame_tx.send(frame).await.unwrap();

        // Wait > 100ms to trigger periodic flush check
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Send another valid tick after the delay to trigger the elapsed check
        let frame2 = make_ticker_frame(14, 24600.0, 1772073901);
        frame_tx.send(frame2).await.unwrap();

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
            run_tick_processor(frame_rx, Some(writer)).await;
        });

        // Send valid ticks — they buffer successfully but flush will fail
        for i in 0..5 {
            let frame = make_ticker_frame(13 + i, 24500.0 + (i as f32), 1772073900);
            frame_tx.send(frame).await.unwrap();
        }

        // Wait > 1s so writer.flush_if_needed() actually tries to flush
        // (TICK_FLUSH_INTERVAL_MS = 1000ms). The tick processor's outer
        // check triggers at 100ms, but the writer only flushes at 1000ms.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

        // Send more ticks to trigger the periodic flush check on next iteration
        for i in 0..5 {
            let frame = make_ticker_frame(20 + i, 25000.0, 1772073901);
            frame_tx.send(frame).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Close channel to trigger final flush (which should also fail
        // because there are pending ticks from the second batch)
        drop(frame_tx);
        let _ = handle.await;
    }
}
