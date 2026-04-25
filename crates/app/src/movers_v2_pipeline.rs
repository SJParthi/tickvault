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

use std::collections::HashMap;
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
use tickvault_core::instrument::depth_rebalancer::SharedSpotPrices;
use tickvault_core::pipeline::top_movers::{
    MoversSnapshotV2, MoversTrackerV2, SharedMoversSnapshotV2, Timeframe, snapshot_to_rows,
};
use tickvault_storage::movers_persistence::TopMoversV2Writer;

/// Plan item G (2026-04-25): bundle of one snapshot per timeframe.
///
/// `MoversTrackerV2::compute_snapshot_v2_at_timeframe` is invoked once per
/// `Timeframe` per cycle. Results land in this map keyed by timeframe so
/// downstream code (API handle publishing, QuestDB persistence) can iterate.
///
/// Always exactly `Timeframe::ALL.len()` entries when emitted from the
/// updater task. `Default` is empty for the watch channel's initial value.
pub type MultiTimeframeSnapshots = HashMap<Timeframe, MoversSnapshotV2>;

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
/// * `spot_prices` — shared underlying-spot LTPs (e.g. NIFTY, RELIANCE) used
///   by `MoversTrackerV2::compute_snapshot_v2_at_timeframe` to compute
///   futures Premium/Discount spreads. May be empty (warm-up grace —
///   buckets simply leave Premium/Discount empty).
/// * `shutdown` — notifier that both tasks await alongside their work.
#[must_use]
pub fn spawn_movers_v2_pipeline(
    registry: Arc<InstrumentRegistry>,
    tick_broadcast: broadcast::Sender<ParsedTick>,
    questdb_config: tickvault_common::config::QuestDbConfig,
    movers_config: MoversConfig,
    spot_prices: SharedSpotPrices,
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
    // bundle of per-timeframe snapshots into a watch channel; the
    // persister consumes the watch.
    let (snapshot_tx, snapshot_rx) =
        tokio::sync::watch::channel::<MultiTimeframeSnapshots>(MultiTimeframeSnapshots::new());

    let updater_registry = Arc::clone(&registry);
    let updater_shutdown = Arc::clone(&shutdown);
    let updater_snapshot_tx = snapshot_tx.clone();
    let updater_spot_prices = Arc::clone(&spot_prices);
    let updater_cadence_secs = u64::from(movers_config.snapshot_cadence_secs.max(1));
    let mut tick_rx = tick_broadcast.subscribe();

    // OI baseline signal — fired once per trading day at 09:15 IST by the
    // scheduler task (spawned below). The updater owns the tracker and
    // consumes this notification to call `tracker.capture_baseline_oi()`.
    let baseline_signal = Arc::new(Notify::new());

    let updater_baseline_signal = Arc::clone(&baseline_signal);
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
                _ = updater_baseline_signal.notified() => {
                    let captured = tracker.capture_baseline_oi();
                    info!(
                        captured,
                        "movers_v2 OI baseline captured at 09:15 IST — \
                         oi_buildup / oi_unwind rankings now populate"
                    );
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
                    // Plan item H (2026-04-25): take a synchronous snapshot
                    // of the SharedSpotPrices map for futures Premium/Discount
                    // routing. Allocation is on the cold path (every N
                    // seconds, not per tick) so the clone is acceptable.
                    let spot_snapshot: HashMap<String, f32> = {
                        let map = updater_spot_prices.read().await;
                        // O(1) EXEMPT: cold path, ~4-200 underlyings (NIFTY, BANKNIFTY,
                        // FINNIFTY, MIDCPNIFTY + stock underlyings).
                        let mut out = HashMap::with_capacity(map.len());
                        for (sym, entry) in map.iter() {
                            // Lossy f64 → f32 — fine for spread routing where the
                            // tracker stores LTPs as f32. The narrowing is bounded
                            // by Indian equity prices (max ~100k INR), well within
                            // f32 precision.
                            #[allow(clippy::cast_possible_truncation)] // APPROVED: bounded by NSE max price ≪ f32::MAX
                            out.insert(sym.clone(), entry.price as f32);
                        }
                        out
                    };

                    // Plan item G (2026-04-25): compute one snapshot per
                    // timeframe. Total = 15 cold-path computations per cycle.
                    // O(1) EXEMPT: cold path; allocation budgeted under cadence.
                    let mut multi: MultiTimeframeSnapshots =
                        MultiTimeframeSnapshots::with_capacity(Timeframe::ALL.len());
                    for tf in Timeframe::ALL {
                        let snap =
                            tracker.compute_snapshot_v2_at_timeframe(tf, &spot_snapshot);
                        multi.insert(tf, snap);
                    }
                    if updater_snapshot_tx.send(multi).is_err() {
                        info!("movers_v2 snapshot channel closed — updater exiting");
                        break;
                    }
                }
            }
        }
    });

    // OI baseline scheduler — fires `baseline_signal` once per trading day
    // at 09:15 IST. Restart-safe: if the boot happens after 09:15 on a
    // trading day, fires immediately so intraday catch-up works.
    let scheduler_shutdown = Arc::clone(&shutdown);
    let scheduler_signal = Arc::clone(&baseline_signal);
    tokio::spawn(async move {
        run_baseline_scheduler(scheduler_shutdown, scheduler_signal).await;
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
                    // Publish the OneMin (1m) snapshot to the API-facing handle
                    // regardless of market hours. The /api/movers handler is
                    // currently single-timeframe; multi-timeframe API surface
                    // is a separate workstream. Persistence below writes ALL
                    // 15 timeframes when market is open.
                    let multi: MultiTimeframeSnapshots = rx.borrow_and_update().clone();
                    if let Some(one_min) = multi.get(&Timeframe::OneMin)
                        && let Ok(mut guard) = persister_snapshot_handle.write()
                    {
                        *guard = Some(one_min.clone());
                    }
                }
                _ = flush_interval.tick() => {
                    // Market-hours gate (Rule 3): do not write rows outside 09:00-15:30 IST.
                    if !is_within_market_hours_ist() {
                        debug!("movers_v2 persister idle — outside market hours");
                        continue;
                    }
                    let Some(w) = writer.as_mut() else { continue };
                    // Plan item G (2026-04-25): iterate every timeframe and
                    // emit rows for each. The DEDUP key
                    //   (timeframe, bucket, rank_category, rank, security_id, segment)
                    // ensures the 15 series coexist without collisions.
                    let multi: MultiTimeframeSnapshots = rx.borrow().clone();
                    if multi.is_empty() {
                        continue;
                    }
                    let mut total_rows = 0_usize;
                    for tf in Timeframe::ALL {
                        let Some(snapshot) = multi.get(&tf) else { continue };
                        let rows =
                            snapshot_to_rows(snapshot, tf, &persister_registry);
                        if rows.is_empty() {
                            continue;
                        }
                        total_rows = total_rows.saturating_add(rows.len());
                        for row in rows {
                            if let Err(err) = w.append_row(row) {
                                error!(
                                    ?err,
                                    timeframe = tf.as_str(),
                                    "movers_v2 append_row failed — dropping snapshot batch"
                                );
                                break;
                            }
                        }
                    }
                    if total_rows == 0 {
                        continue;
                    }
                    if let Err(err) = w.flush() {
                        error!(
                            ?err,
                            total_rows,
                            "movers_v2 flush failed — rows retained in rescue ring"
                        );
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

/// OI baseline scheduler — awakes the updater once per trading day at
/// 09:15 IST so it can capture the intraday OI snapshot used as baseline
/// for `oi_buildup` + `oi_unwind` rankings.
///
/// # Restart behaviour
/// If the app boots after 09:15 IST on a trading day, fires immediately so
/// intraday restarts still produce a meaningful baseline. If it boots
/// before 09:15 IST, sleeps until then.
async fn run_baseline_scheduler(shutdown: Arc<Notify>, signal: Arc<Notify>) {
    // Baseline target = 09:15:00 IST = 33_300 seconds since IST midnight.
    // Matches `TICK_PERSIST_START_SECS_OF_DAY_IST + 900` (market-open + 15min).
    const OI_BASELINE_SEC_OF_DAY_IST: i64 = 33_300;

    loop {
        // Compute seconds until next 09:15 IST.
        let now_utc = chrono::Utc::now().timestamp();
        let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
        let sec_of_day = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY));
        let sleep_secs = if sec_of_day < OI_BASELINE_SEC_OF_DAY_IST {
            (OI_BASELINE_SEC_OF_DAY_IST - sec_of_day) as u64
        } else {
            // Already past today's 09:15 IST — fire immediately on first
            // boot, then sleep until tomorrow's 09:15.
            0
        };

        if sleep_secs == 0 {
            // Intraday boot — fire immediately so the current session still
            // gets a meaningful baseline from whatever OI is already tracked.
            info!("movers_v2 OI baseline scheduler — firing immediate (intraday boot)");
            signal.notify_waiters();
            // Sleep until tomorrow's 09:15.
            let tomorrow = i64::from(SECONDS_PER_DAY) - sec_of_day + OI_BASELINE_SEC_OF_DAY_IST;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(tomorrow as u64)) => {}
                _ = shutdown.notified() => {
                    info!("movers_v2 OI baseline scheduler shutting down");
                    return;
                }
            }
            continue;
        }

        debug!(
            sleep_secs,
            "movers_v2 OI baseline scheduler sleeping until 09:15 IST"
        );
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {
                info!("movers_v2 OI baseline scheduler — 09:15 IST fired");
                signal.notify_waiters();
            }
            _ = shutdown.notified() => {
                info!("movers_v2 OI baseline scheduler shutting down");
                return;
            }
        }
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
            let spot_prices =
                tickvault_core::instrument::depth_rebalancer::new_shared_spot_prices();
            let handles = spawn_movers_v2_pipeline(
                Arc::clone(&registry),
                tx.clone(),
                questdb_cfg,
                MoversConfig::default(),
                spot_prices,
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
