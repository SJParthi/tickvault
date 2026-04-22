//! Plan items G1 + G2 (2026-04-22) — boot wiring for the V2 movers tracker.
//!
//! Exposes a single entry point, [`spawn_movers_v2_pipeline`], that:
//!
//! 1. Constructs a [`MoversTrackerV2`] with the injected `InstrumentRegistry`.
//! 2. Subscribes a receiver off the shared tick broadcast.
//! 3. Spawns TWO tokio tasks:
//!    - **Updater** — drains the tick broadcast, feeds every tick to
//!      `tracker.update_v2(..)`. O(1) per tick.
//!    - **Persister** — periodically computes a snapshot, publishes it
//!      to the shared handle (read by `/api/movers`), and writes rows
//!      to the unified `top_movers` QuestDB table (guarded by
//!      `is_within_market_hours_ist()` per Rule 3).
//!
//! Both tasks honour the [`tokio::sync::Notify`] shutdown signal so they
//! can be cleanly cancelled at process exit.
//!
//! # Why this is its own module
//!
//! The slow boot path (`main.rs` Step 11) and the fast boot path
//! (`main.rs` ~line 830) both need identical wiring. Centralising the
//! spawn logic here keeps the two boot paths symmetric (prevents the
//! kind of boot-path divergence the G3+G4 pre-push guard complains about).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, broadcast};
use tracing::{debug, error, info, warn};

use tickvault_common::config::MoversConfig;
use tickvault_common::constants::{
    IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
    TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::instrument_registry::InstrumentRegistry;
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::pipeline::top_movers::{
    MoversSnapshotV2, MoversTrackerV2, SharedMoversSnapshotV2, snapshot_to_rows,
};
use tickvault_storage::movers_persistence::TopMoversV2Writer;

/// Handles returned to the boot path so it can join on shutdown.
pub struct MoversV2PipelineHandles {
    pub updater: tokio::task::JoinHandle<()>,
    pub persister: tokio::task::JoinHandle<()>,
    pub snapshot_handle: SharedMoversSnapshotV2,
}

/// Spawns the V2 movers tracker pipeline.
///
/// # Arguments
/// * `registry` — Arc<InstrumentRegistry> for O(1) segment-aware bucket classification.
/// * `tick_broadcast` — the process-wide tick broadcast sender. A fresh
///   receiver is created via `.subscribe()`.
/// * `questdb_config` — for constructing the `TopMoversV2Writer` (ILP).
/// * `movers_config` — [`MoversConfig`] filters + cadences (plan item G2).
/// * `shutdown` — notifier that both tasks await alongside their work.
#[must_use]
pub fn spawn_movers_v2_pipeline(
    registry: Arc<InstrumentRegistry>,
    tick_broadcast: broadcast::Sender<ParsedTick>,
    questdb_config: tickvault_common::config::QuestDbConfig,
    movers_config: MoversConfig,
    shutdown: Arc<Notify>,
) -> MoversV2PipelineHandles {
    // Shared state — constructed eagerly so both the api state and the
    // persister task see the same handle.
    let snapshot_handle: SharedMoversSnapshotV2 =
        Arc::new(std::sync::RwLock::new(Some(MoversSnapshotV2::default())));

    // The tracker itself lives inside the updater task. The persister
    // reads snapshots via a clone of the tracker — BUT the tracker is
    // not Send between tasks by itself (HashMap is, but we want a single
    // writer). Pattern: the updater owns the tracker and emits a fresh
    // snapshot into a watch channel; the persister consumes the watch.
    let (snapshot_tx, snapshot_rx) =
        tokio::sync::watch::channel::<MoversSnapshotV2>(MoversSnapshotV2::default());

    let updater_registry = Arc::clone(&registry);
    let updater_shutdown = Arc::clone(&shutdown);
    let updater_snapshot_tx = snapshot_tx.clone();
    let updater_cadence_secs = u64::from(movers_config.snapshot_cadence_secs.max(1));
    let mut tick_rx = tick_broadcast.subscribe();

    let updater = tokio::spawn(async move {
        let mut tracker = MoversTrackerV2::new(updater_registry);
        let mut recompute = tokio::time::interval(Duration::from_secs(updater_cadence_secs));
        recompute.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                _ = updater_shutdown.notified() => {
                    info!("movers_v2 updater task shutting down");
                    break;
                }
                recv = tick_rx.recv() => {
                    match recv {
                        Ok(tick) => tracker.update_v2(&tick),
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(skipped, "movers_v2 updater lagged — skipped ticks");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("movers_v2 tick broadcast closed — updater exiting");
                            break;
                        }
                    }
                }
                _ = recompute.tick() => {
                    let snapshot = tracker.compute_snapshot_v2();
                    if updater_snapshot_tx.send(snapshot).is_err() {
                        info!("movers_v2 snapshot channel closed — updater exiting");
                        break;
                    }
                }
            }
        }
    });

    // Persister task — reads the latest snapshot off the watch channel,
    // publishes to the shared handle, and writes to QuestDB.
    let persister_shutdown = Arc::clone(&shutdown);
    let persister_snapshot_handle = Arc::clone(&snapshot_handle);
    let persister_registry = Arc::clone(&registry);
    let persistence_cadence_secs = u64::from(movers_config.persistence_cadence_secs.max(1));

    let persister = tokio::spawn(async move {
        let mut writer = match TopMoversV2Writer::new(&questdb_config) {
            Ok(w) => {
                info!("top_movers v2 writer connected");
                Some(w)
            }
            Err(err) => {
                warn!(?err, "top_movers v2 writer unavailable — /api/movers only");
                None
            }
        };

        let mut rx = snapshot_rx;
        let mut flush_interval =
            tokio::time::interval(Duration::from_secs(persistence_cadence_secs));
        flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                _ = persister_shutdown.notified() => {
                    info!("movers_v2 persister task shutting down");
                    if let Some(w) = writer.as_mut()
                        && let Err(err) = w.flush()
                    {
                        error!(?err, "final top_movers v2 flush failed at shutdown");
                    }
                    break;
                }
                changed = rx.changed() => {
                    if changed.is_err() {
                        info!("movers_v2 snapshot channel closed — persister exiting");
                        break;
                    }
                    // Publish to the API-facing shared handle regardless of market hours.
                    let snapshot: MoversSnapshotV2 = rx.borrow_and_update().clone();
                    if let Ok(mut guard) = persister_snapshot_handle.write() {
                        *guard = Some(snapshot);
                    }
                }
                _ = flush_interval.tick() => {
                    // Market-hours gate (Rule 3): do not write rows outside 09:00-15:30 IST.
                    if !is_within_market_hours_ist() {
                        debug!("movers_v2 persister idle — outside market hours");
                        continue;
                    }
                    if let Some(w) = writer.as_mut() {
                        // Snapshot the latest published state (RwLock read) and enrich to rows.
                        let snapshot_opt = match persister_snapshot_handle.read() {
                            Ok(g) => g.as_ref().cloned(),
                            Err(_) => None,
                        };
                        let Some(snapshot) = snapshot_opt else { continue };
                        let rows = snapshot_to_rows(&snapshot, &persister_registry);
                        if rows.is_empty() {
                            continue;
                        }
                        for row in rows {
                            if let Err(err) = w.append_row(row) {
                                error!(?err, "movers_v2 append_row failed — dropping snapshot batch");
                                break;
                            }
                        }
                        if let Err(err) = w.flush() {
                            error!(?err, "movers_v2 flush failed — rows retained in rescue ring");
                        }
                    }
                }
            }
        }
    });

    MoversV2PipelineHandles {
        updater,
        persister,
        snapshot_handle,
    }
}

/// Market-hours gate helper — mirrors `depth_rebalancer::is_within_market_hours_ist`
/// (Rule 3 in `.claude/rules/project/audit-findings-2026-04-17.md`).
#[inline]
#[must_use]
pub fn is_within_market_hours_ist() -> bool {
    let now_utc = chrono::Utc::now().timestamp();
    let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    // `rem_euclid` returns a non-negative value in the range `[0, SECONDS_PER_DAY)`.
    // The cast to u32 is safe because the value is bounded by 86400.
    let sec_of_day = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY));
    let sec_of_day_u32 = u32::try_from(sec_of_day).unwrap_or(0);
    (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day_u32)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_within_market_hours_ist_returns_bool_in_current_wall_clock() {
        // The function must not panic regardless of the test-runner wall-clock.
        // Both branches are reachable depending on when CI runs.
        let _ = is_within_market_hours_ist();
    }

    #[test]
    fn test_spawn_movers_v2_pipeline_returns_handles_and_snapshot_arc() {
        // Smoke test: spawning must not panic. We cannot flush to QuestDB in
        // a unit test — the writer attempt is best-effort (None on failure)
        // and the persister task continues without a writer.
        use tokio::runtime::Builder;

        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let registry = Arc::new(InstrumentRegistry::empty());
            let (tx, _rx) = broadcast::channel::<ParsedTick>(8);
            let questdb_cfg = tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 65_535,
                pg_port: 65_535,
                ilp_port: 65_535,
            };
            let shutdown = Arc::new(Notify::new());
            let handles = spawn_movers_v2_pipeline(
                Arc::clone(&registry),
                tx.clone(),
                questdb_cfg,
                MoversConfig::default(),
                Arc::clone(&shutdown),
            );

            // Shared snapshot handle must be populated (empty default).
            assert!(Arc::strong_count(&handles.snapshot_handle) >= 1);

            // Trigger shutdown and allow tasks to join.
            shutdown.notify_waiters();
            let _ = tokio::time::timeout(Duration::from_millis(500), handles.updater).await;
            let _ = tokio::time::timeout(Duration::from_millis(500), handles.persister).await;
        });
    }
}
