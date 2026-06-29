//! Broadcast consumer task that pushes every tick into [`TickStorage`].
//!
//! Subscribes to the same `tokio::sync::broadcast::Sender<ParsedTick>`
//! that fans the live tick stream to existing consumers (the 1s
//! candle cascade, persistence pipeline, etc.). Each `recv().await`
//! pushes the tick into the in-RAM ring.
//!
//! ## Why a broadcast consumer (not a tick_processor parameter)
//!
//! The existing `cascade::run_cascade_1s` already uses this pattern:
//! every downstream component that needs the live tick stream
//! subscribes via its own broadcast Receiver, decoupling the
//! tick_processor's hot loop from any individual consumer's latency
//! profile. Adding a `tick_storage.push(&tick)` call inline in
//! tick_processor.rs would couple that hot path to the storage's lock
//! latency; the broadcast pattern keeps the hot path lean.
//!
//! ## Lag handling
//!
//! `broadcast::Receiver::recv` returns `Err(Lagged(n))` when the
//! consumer is slower than the broadcast capacity. The
//! `tv_in_mem_tick_storage_lag_total` counter increments by `n`; the
//! `error!` path is coalesced so a single backpressure event doesn't
//! flood Telegram.

use std::sync::Arc;
use std::time::{Duration, Instant};

use metrics::counter;
use tokio::sync::broadcast::{self, error::RecvError};
use tracing::{error, info};

use tickvault_common::tick_types::ParsedTick;

use crate::in_mem::tick_storage::TickStorage;

/// Coalesce window for `error!` on broadcast lag (matches the cascade
/// task's discipline). `error!` fires AT MOST once per N seconds even
/// under sustained lag, so Telegram doesn't flood. Counter still
/// increments on every Lagged event for accurate operator dashboards.
const LAG_ERROR_COALESCE_SECS: u64 = 60;

/// Runs until the broadcast sender is dropped. The caller spawns this
/// via `tokio::spawn(...)` and stores the `JoinHandle` for shutdown
/// observability.
///
/// **First-tick log**: emits `info!` only after the FIRST successful
/// `recv()` — an immediate-close broadcast (sender dropped before any
/// tick) does NOT spam a "task active" line. Mirrors the cascade
/// task's audit-Rule-11 discipline (no false-OK signals).
pub async fn run_tick_storage_consumer(
    mut rx: broadcast::Receiver<ParsedTick>,
    storage: Arc<TickStorage>,
) {
    let mut first_tick_seen = false;
    let mut last_lag_log: Option<Instant> = None;
    loop {
        match rx.recv().await {
            Ok(tick) => {
                if !first_tick_seen {
                    first_tick_seen = true;
                    info!(
                        instruments = storage.len_instruments(),
                        "tick_storage consumer task active — first tick observed"
                    );
                }
                if !storage.push(tick) {
                    // `push` already increments
                    // `tv_in_mem_tick_storage_pushes_dropped_total{reason}`
                    // and only returns false on Mutex poison (extremely
                    // unlikely; no panic-able code in the critical
                    // section). Continue the loop — next push targets
                    // the same `Arc<Mutex>` and will succeed.
                    counter!("tv_in_mem_tick_storage_consumer_drops_total").increment(1);
                }
            }
            Err(RecvError::Lagged(n)) => {
                counter!("tv_in_mem_tick_storage_lag_total").increment(n);
                let should_log = match last_lag_log {
                    None => true,
                    Some(prev) => prev.elapsed() >= Duration::from_secs(LAG_ERROR_COALESCE_SECS),
                };
                if should_log {
                    last_lag_log = Some(Instant::now());
                    // Audit-findings Rule 5: ERROR (not WARN) so
                    // Telegram surfaces sustained lag.
                    error!(
                        lagged_count = n,
                        coalesce_window_secs = LAG_ERROR_COALESCE_SECS,
                        "tick_storage consumer broadcast lag"
                    );
                }
            }
            Err(RecvError::Closed) => {
                info!("tick_storage consumer broadcast closed — shutting down task");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tick(security_id: u64, segment: u8, ltp: f32) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: segment,
            last_traded_price: ltp,
            ..ParsedTick::default()
        }
    }

    #[tokio::test]
    async fn test_consumer_receives_and_pushes_one_tick() {
        let (tx, rx) = broadcast::channel(16);
        let storage = Arc::new(TickStorage::default());
        let storage_clone = Arc::clone(&storage);
        let join = tokio::spawn(async move {
            run_tick_storage_consumer(rx, storage_clone).await;
        });
        // Send one tick, then close the channel.
        tx.send(make_tick(1234, 1, 100.0))
            .expect("broadcast send during test");
        drop(tx);
        // Task exits when sender is dropped.
        join.await.expect("consumer task join");
        assert_eq!(storage.len_total(), 1);
    }

    #[tokio::test]
    async fn test_consumer_handles_multiple_instruments() {
        let (tx, rx) = broadcast::channel(16);
        let storage = Arc::new(TickStorage::default());
        let storage_clone = Arc::clone(&storage);
        let join = tokio::spawn(async move {
            run_tick_storage_consumer(rx, storage_clone).await;
        });
        for i in 0..5 {
            tx.send(make_tick(1234, 1, i as f32)).expect("send");
            tx.send(make_tick(5678, 2, i as f32)).expect("send");
        }
        drop(tx);
        join.await.expect("join");
        assert_eq!(storage.len_total(), 10);
        assert_eq!(storage.len_instruments(), 2);
    }

    #[tokio::test]
    async fn test_run_tick_storage_consumer_explicit_name_match() {
        let (tx, rx) = broadcast::channel::<ParsedTick>(4);
        let storage = Arc::new(TickStorage::default());
        drop(tx);
        run_tick_storage_consumer(rx, storage).await;
    }

    #[tokio::test]
    async fn test_consumer_exits_cleanly_on_closed_channel() {
        // Sender dropped before any tick → task exits without
        // emitting the false-OK "task active" line (Rule 11).
        let (tx, rx) = broadcast::channel::<ParsedTick>(16);
        let storage = Arc::new(TickStorage::default());
        drop(tx);
        // Should return without hanging.
        run_tick_storage_consumer(rx, storage).await;
    }
}
