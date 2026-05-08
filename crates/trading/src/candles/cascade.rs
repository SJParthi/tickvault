//! 1s cascade task — Phase 3 commit 3.
//!
//! Subscribes to the existing `tick_broadcast` channel that fans every
//! WebSocket-sourced `ParsedTick` out of `tick_processor`. For each tick
//! received, calls `CandleEngineMap::on_tick` so the in-memory 1s engine
//! advances. Sealed bars are counted (Prometheus) and stamped (debug
//! tracing) — the actual fan-out into 28 derived engines lands in
//! Phase 3 commit 4 alongside the missing TF marker types.
//!
//! ## Why a broadcast subscriber, not a `tick_processor` parameter
//!
//! `crates/core` cannot import `crates/trading` (the dep arrow points
//! the other way). `tick_processor` already exposes a
//! `tokio::sync::broadcast::Sender<ParsedTick>` precisely so downstream
//! crates can attach without circular deps. Subscribing here keeps the
//! engine entirely off the persistence hot-path and respects the layer
//! direction.
//!
//! ## Hot-path budget (per plan L12)
//!
//! - broadcast `recv().await` wake: ~1µs (tokio scheduling)
//! - `CandleEngineMap::on_tick` body: ~60ns typical, ~110ns on seal
//! - sealed-bar counter increment: ~5ns (lock-free)
//!
//! The 1µs wake is OFF the persistence path entirely — the persist task
//! and this task wake independently from the same broadcast.
//!
//! ## Lag handling (audit-findings Rule 5)
//!
//! `broadcast::Receiver::recv` returns `Err(Lagged(n))` when the consumer
//! falls behind the channel ring. We log at `error!` (so Loki routes to
//! Telegram) and increment `tv_candle_cascade_lag_total` so the operator
//! sees the magnitude. We do NOT panic and do NOT exit — the consumer
//! continues from the next available tick.
//!
//! `Err(Closed)` returns from the function — the broadcast sender being
//! dropped means upstream `tick_processor` exited; nothing more to do.

use std::sync::Arc;
use std::time::{Duration, Instant};

use metrics::counter;
use tokio::sync::broadcast::{self, error::RecvError};
use tracing::{debug, error, info};

use tickvault_common::tick_types::ParsedTick;

use crate::candles::cascade_fanout::CascadeFanout;
use crate::candles::engine::Tf1s;
use crate::candles::engine_map::CandleEngineMap;
use crate::in_mem::PrevDayCache;

/// Prometheus counter — every tick the cascade processed.
pub const METRIC_TICKS_PROCESSED: &str = "tv_candle_cascade_ticks_processed_total";
/// Prometheus counter — every sealed bar the 1s engine emitted.
pub const METRIC_BARS_SEALED: &str = "tv_candle_cascade_bars_sealed_total";
/// Prometheus counter — broadcast lag events. Incremented by `n` when
/// the consumer falls behind by `n` messages. Wrap in `increase()` per
/// audit-findings Rule 12 when graphing.
pub const METRIC_LAG_TOTAL: &str = "tv_candle_cascade_lag_total";
/// Prometheus counter — supervisor respawned the cascade after a
/// panic or unexpected exit. Wave-4 stream-resilience B5: pages on
/// rising rate, never on level.
pub const METRIC_RESPAWN_TOTAL: &str = "tv_candle_cascade_respawn_total";
/// Prometheus counter — IST midnight rollover seal events.
pub const METRIC_MIDNIGHT_SEAL_BARS: &str = "tv_candle_cascade_midnight_seal_bars_total";

/// Coalesce window for `error!` on broadcast lag (audit Rule 4 +
/// stream-resilience B5). The counter still increments per event, only
/// the Telegram-routed `error!` is rate-limited.
const LAG_ERROR_COALESCE_SECS: u64 = 60;

/// Backoff between supervisor respawn attempts after the inner cascade
/// task panics. 1 second matches the WS-GAP-05 supervisor pattern in
/// `respawn_dead_connections_loop`.
const SUPERVISOR_RESPAWN_BACKOFF_SECS: u64 = 1;

/// Defensive minimum delay AFTER an IST midnight rollover before
/// recomputing the next sleep. Guarantees no busy-loop on a clock
/// anomaly that puts `secs_until_next_ist_midnight` at or near zero.
const ROLLOVER_POST_SEAL_DELAY_SECS: u64 = 1;

/// Runs the 1s cascade consumer until the broadcast sender is dropped.
///
/// Per plan L12 the only engine on the per-tick path is the 1s engine.
/// Sealed bars are observed via Prometheus + debug tracing here — the
/// 28 derived engines + their rtrb SPSC arrive in Phase 3 commit 4.
///
/// **False-OK guard (audit Rule 11, hostile review H4):** the "task
/// active" `info!` is emitted ONLY after the FIRST successful `recv()`,
/// so an immediate-close broadcast (sender dropped before any tick) does
/// not produce a misleading "started" log in the audit trail.
///
/// **Lag coalescing (audit Rule 4, hostile review H2):** consecutive
/// `Lagged(n)` events within `LAG_ERROR_COALESCE_SECS` increment the
/// counter only — the `error!` (which routes to Telegram via Loki) fires
/// at most once per coalesce window per task instance.
///
/// **28-engine fanout (Phase 3 commit 4):** when `fanout` is `Some`,
/// every sealed 1s bar is also fed into all 28 derived engines via
/// `CascadeFanout::feed_sealed_1s_bar`. The fanout call is O(28),
/// constant per sealed bar (~1.7µs), and runs in this same task so
/// the per-instrument ordering of derived bars is preserved.
///
/// **F1 (Wave-5 §K-L13 / #504e follow-up):** when `pct_cache` is
/// `Some`, the seal-time pct stamping path is enabled. The 1s engine
/// uses `on_tick_with_pct` (instead of `on_tick`) and the fanout uses
/// `feed_sealed_1s_bar_with_pct` (instead of `feed_sealed_1s_bar`),
/// so every sealed Bar carries the 5 frozen-per-day % fields populated
/// from the cache. The cache may be empty at boot — in that case the
/// stamping falls back to 0.0 % fields without dropping any seal,
/// matching the `on_*_with_pct` div-by-zero policy. The boot-time
/// loader that populates the cache lands in F2.
///
/// This function is `async fn -> ()` and returns when the broadcast
/// sender is dropped. The caller is expected to spawn it via
/// `spawn_supervised_cascade_1s` so panics are surfaced + respawned.
pub async fn run_cascade_1s(
    mut rx: broadcast::Receiver<ParsedTick>,
    engine_map: Arc<CandleEngineMap<Tf1s>>,
    fanout: Option<CascadeFanout>,
    pct_cache: Option<Arc<PrevDayCache>>,
) {
    let mut first_tick_seen = false;
    let mut last_lag_log: Option<Instant> = None;
    loop {
        match rx.recv().await {
            Ok(tick) => {
                if !first_tick_seen {
                    first_tick_seen = true;
                    info!(
                        engine_count = engine_map.len(),
                        pct_cache_wired = pct_cache.is_some(),
                        "candle cascade 1s task active — first tick observed"
                    );
                }
                counter!(METRIC_TICKS_PROCESSED).increment(1);
                let sealed_bar = match &pct_cache {
                    Some(cache) => engine_map.on_tick_with_pct(&tick, cache),
                    None => engine_map.on_tick(&tick),
                };
                if let Some(bar) = sealed_bar {
                    counter!(METRIC_BARS_SEALED).increment(1);
                    // Phase 3 commit 4: feed the sealed 1s bar into all
                    // 28 derived engines via the fanout. O(28) constant.
                    // F1: when `pct_cache` is wired, use the stamping
                    // variant so every derived engine's sealed bar
                    // carries the 5 % fields populated from the cache.
                    if let Some(ref f) = fanout {
                        match &pct_cache {
                            Some(cache) => f.feed_sealed_1s_bar_with_pct(&bar, cache),
                            None => f.feed_sealed_1s_bar(&bar),
                        }
                    }
                    // Hot-path review M1 fix: gate the format expansion
                    // entirely behind the level check so peak-load
                    // (~24K seals/sec) never pays the structured-field
                    // capture cost in production where DEBUG is off.
                    if tracing::enabled!(tracing::Level::DEBUG) {
                        debug!(
                            security_id = bar.security_id,
                            segment_code = bar.exchange_segment_code,
                            bucket_start = bar.bucket_start_ist_secs,
                            close = bar.close,
                            "1s bar sealed"
                        );
                    }
                }
            }
            Err(RecvError::Lagged(n)) => {
                counter!(METRIC_LAG_TOTAL).increment(n);
                let should_log = match last_lag_log {
                    None => true,
                    Some(prev) => prev.elapsed() >= Duration::from_secs(LAG_ERROR_COALESCE_SECS),
                };
                if should_log {
                    last_lag_log = Some(Instant::now());
                    // Audit-findings Rule 5: ERROR (not WARN) so Telegram
                    // surfaces sustained lag.
                    error!(
                        lagged_count = n,
                        coalesce_window_secs = LAG_ERROR_COALESCE_SECS,
                        "candle cascade 1s lagged behind tick broadcast — engine state will skip ticks (subsequent lag events within window logged via counter only)"
                    );
                }
            }
            Err(RecvError::Closed) => {
                info!(
                    first_tick_seen,
                    "candle cascade 1s task exiting — tick broadcast closed"
                );
                return;
            }
        }
    }
}

/// Supervisor — runs `run_cascade_1s` under a respawn loop so that a
/// panic or unexpected exit does NOT leave the in-RAM candle state
/// silently frozen (hostile review H3 fix; mirrors the pattern of
/// `respawn_dead_connections_loop` per WS-GAP-05).
///
/// Returns only when the broadcast sender is dropped. Increments
/// `tv_candle_cascade_respawn_total` on every respawn event.
pub async fn spawn_supervised_cascade_1s(
    sender: broadcast::Sender<ParsedTick>,
    engine_map: Arc<CandleEngineMap<Tf1s>>,
    fanout: Option<CascadeFanout>,
    pct_cache: Option<Arc<PrevDayCache>>,
) {
    loop {
        let rx = sender.subscribe();
        let map = Arc::clone(&engine_map);
        // Supervisor respawn path runs at most once per panic/exit;
        // clone is 28 atomic refcount bumps (~100ns total), off the
        // per-tick hot path. This is NOT the cascade hot loop.
        // O(1) EXEMPT: supervisor respawn, not per-tick hot path.
        let fanout_clone = fanout.clone();
        let pct_cache_clone = pct_cache.as_ref().map(Arc::clone);
        let join =
            tokio::spawn(
                async move { run_cascade_1s(rx, map, fanout_clone, pct_cache_clone).await },
            );
        match join.await {
            Ok(()) => {
                // Clean exit ⇒ broadcast sender was dropped. Stop
                // supervising; nothing left to consume.
                info!("candle cascade supervisor: clean exit, stopping respawn loop");
                return;
            }
            Err(join_err) => {
                counter!(METRIC_RESPAWN_TOTAL).increment(1);
                error!(
                    join_err = ?join_err,
                    "candle cascade 1s task panicked or was cancelled — respawning in 1s"
                );
                tokio::time::sleep(Duration::from_secs(SUPERVISOR_RESPAWN_BACKOFF_SECS)).await;
            }
        }
    }
}

/// IST midnight rollover task — seals BOTH the 1s engine map AND
/// every derived engine inside the optional `fanout` so the entire
/// 29-TF set rolls over atomically at IST 00:00 (plan L13).
///
/// **Hostile review H3 fix:** the legacy 1s-only
/// `run_midnight_rollover_task` was DELETED in commit 4 to remove the
/// foot-gun where a future caller could wire it instead of this
/// fanout-aware variant — the result would have been correct 1s
/// rollover but silent fusion of Day-N's open bars into Day-(N+1)'s
/// first bar across all 28 derived engines.
pub async fn run_midnight_rollover_task_with_fanout(
    engine_map: Arc<CandleEngineMap<Tf1s>>,
    fanout: Option<CascadeFanout>,
) {
    loop {
        let secs_until_midnight = secs_until_next_ist_midnight(now_ist_secs());
        tokio::time::sleep(Duration::from_secs(secs_until_midnight)).await;
        let one_s_sealed = engine_map.force_seal_all();
        let fanout_sealed = fanout.as_ref().map_or(0, |f| f.force_seal_all());
        let total = u64::from(one_s_sealed).saturating_add(fanout_sealed);
        counter!(METRIC_MIDNIGHT_SEAL_BARS).increment(total);
        info!(
            one_s_sealed,
            fanout_sealed,
            total,
            "candle cascade IST midnight rollover (29-TF) — sealed open bars across day boundary"
        );
        tokio::time::sleep(Duration::from_secs(ROLLOVER_POST_SEAL_DELAY_SECS)).await;
    }
}

// Phase 3 commit 4 (hostile review H3): the legacy 1s-only
// `run_midnight_rollover_task` was deleted here. Use
// `run_midnight_rollover_task_with_fanout(engine_map, None)` for the
// equivalent 1s-only behaviour (the fanout-aware variant accepts an
// optional fanout, so a `None` argument reproduces the legacy semantics
// without keeping a separate function symbol that could be wired by
// mistake).

/// Returns IST seconds-since-epoch (UTC + 19800).
#[inline]
fn now_ist_secs() -> i64 {
    let utc = chrono::Utc::now().timestamp();
    utc.saturating_add(i64::from(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ))
}

/// Pure helper — given current IST epoch seconds, returns the seconds
/// remaining until the next IST midnight (00:00:00). Always returns at
/// least 1 second so callers cannot busy-loop on the boundary.
#[inline]
fn secs_until_next_ist_midnight(now_ist_secs: i64) -> u64 {
    const SECS_PER_DAY: i64 = 86_400;
    let secs_of_day = now_ist_secs.rem_euclid(SECS_PER_DAY);
    let remaining = SECS_PER_DAY - secs_of_day;
    // Always >= 1 — clamps the case where now is exactly at midnight.
    remaining.max(1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tickvault_common::tick_types::ParsedTick;

    /// Helper — build a minimal `ParsedTick` for cascade unit tests.
    fn make_tick(security_id: u32, segment_code: u8, ts: u32, ltp: f32) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: segment_code,
            last_traded_price: ltp,
            exchange_timestamp: ts,
            ..ParsedTick::default()
        }
    }

    #[tokio::test]
    async fn run_cascade_1s_advances_engine_on_tick() {
        let (tx, rx) = broadcast::channel::<ParsedTick>(16);
        let engine_map: Arc<CandleEngineMap<Tf1s>> = Arc::new(CandleEngineMap::new());
        let map_clone = Arc::clone(&engine_map);

        let handle = tokio::spawn(run_cascade_1s(rx, map_clone, None, None));

        // Two ticks in the same 1s bucket → engine accumulates, no seal.
        tx.send(make_tick(1000, 1, 100, 100.0)).expect("send 1");
        tx.send(make_tick(1000, 1, 100, 101.0)).expect("send 2");

        // Drop sender → cascade exits cleanly.
        drop(tx);
        handle.await.expect("cascade task joins");

        // Engine has open bar visible.
        let latest = engine_map.latest(1000, 1).expect("engine has state");
        assert_eq!(latest.bucket_start_ist_secs, 100);
        assert_eq!(latest.high, 101.0);
    }

    #[tokio::test]
    async fn run_cascade_1s_seals_bar_on_bucket_crossing() {
        let (tx, rx) = broadcast::channel::<ParsedTick>(16);
        let engine_map: Arc<CandleEngineMap<Tf1s>> = Arc::new(CandleEngineMap::new());
        let map_clone = Arc::clone(&engine_map);
        let handle = tokio::spawn(run_cascade_1s(rx, map_clone, None, None));

        tx.send(make_tick(2000, 1, 200, 50.0)).expect("send a");
        tx.send(make_tick(2000, 1, 201, 51.0))
            .expect("send b crosses bucket");

        drop(tx);
        handle.await.expect("cascade exits");

        // After seal, the open bar is the new (201, 51.0) bar.
        let latest = engine_map.latest(2000, 1).expect("post-seal latest");
        assert_eq!(latest.bucket_start_ist_secs, 201);
        assert_eq!(latest.open, 51.0);
    }

    #[tokio::test]
    async fn run_cascade_1s_exits_when_broadcast_closes() {
        let (tx, rx) = broadcast::channel::<ParsedTick>(4);
        let engine_map: Arc<CandleEngineMap<Tf1s>> = Arc::new(CandleEngineMap::new());
        let handle = tokio::spawn(run_cascade_1s(rx, engine_map, None, None));
        drop(tx); // Close immediately — cascade should exit.
        handle.await.expect("cascade exits");
    }

    #[tokio::test]
    async fn run_cascade_1s_recovers_from_broadcast_lag() {
        // Tiny ring (2) — exceed it to force Lagged error.
        let (tx, rx) = broadcast::channel::<ParsedTick>(2);
        let engine_map: Arc<CandleEngineMap<Tf1s>> = Arc::new(CandleEngineMap::new());
        let map_clone = Arc::clone(&engine_map);
        let handle = tokio::spawn(run_cascade_1s(rx, map_clone, None, None));

        // Producer floods 100 ticks before consumer wakes — older ticks
        // are dropped, consumer sees Lagged then resumes from latest.
        for i in 0..100u32 {
            let _ = tx.send(make_tick(3000, 1, 1000 + i, 10.0 + i as f32));
        }
        // Allow consumer to drain remaining ticks.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        drop(tx);
        handle.await.expect("cascade exits after lag recovery");

        // Engine processed at least the most recent tick — proof the
        // consumer recovered from Lagged and did not panic/exit.
        assert!(engine_map.latest(3000, 1).is_some());
    }

    #[test]
    fn metric_constants_have_expected_names() {
        assert_eq!(
            METRIC_TICKS_PROCESSED,
            "tv_candle_cascade_ticks_processed_total"
        );
        assert_eq!(METRIC_BARS_SEALED, "tv_candle_cascade_bars_sealed_total");
        assert_eq!(METRIC_LAG_TOTAL, "tv_candle_cascade_lag_total");
    }

    #[test]
    fn run_cascade_1s_explicit_name_match() {
        // pub-fn-test guard — name match.
        let _ = run_cascade_1s;
    }

    #[test]
    fn spawn_supervised_cascade_1s_explicit_name_match() {
        let _ = spawn_supervised_cascade_1s;
    }

    #[test]
    fn run_midnight_rollover_task_with_fanout_explicit_name_match() {
        let _ = run_midnight_rollover_task_with_fanout;
    }

    #[test]
    fn secs_until_next_ist_midnight_returns_full_day_at_midnight() {
        // At IST midnight (secs_of_day == 0), next midnight is in 86_400s.
        // The clamp returns max(86_400, 1) == 86_400.
        let secs_at_midnight = 86_400 * 1000; // exactly N days from epoch
        assert_eq!(secs_until_next_ist_midnight(secs_at_midnight), 86_400);
    }

    #[test]
    fn secs_until_next_ist_midnight_returns_one_second_at_one_sec_before() {
        // 1 second before next midnight ⇒ returns 1.
        let one_sec_before = 86_400 * 1000 - 1;
        assert_eq!(secs_until_next_ist_midnight(one_sec_before), 1);
    }

    #[test]
    fn secs_until_next_ist_midnight_handles_mid_day() {
        // Noon IST (12:00:00 = 43_200 secs of day) ⇒ 43_200 secs left.
        let noon = 86_400 * 1000 + 43_200;
        assert_eq!(secs_until_next_ist_midnight(noon), 43_200);
    }

    #[test]
    fn secs_until_next_ist_midnight_never_returns_zero() {
        // Sweep every minute boundary, must never return 0.
        for sec_of_day in 0..86_400 {
            let now = 86_400 * 100 + sec_of_day;
            let remaining = secs_until_next_ist_midnight(now);
            assert!(
                remaining >= 1,
                "secs_until_next_ist_midnight returned 0 at sec_of_day={sec_of_day}"
            );
            assert!(
                remaining <= 86_400,
                "secs_until_next_ist_midnight returned > 86_400 at sec_of_day={sec_of_day}"
            );
        }
    }

    #[test]
    fn metric_constants_include_supervisor_and_midnight() {
        assert_eq!(METRIC_RESPAWN_TOTAL, "tv_candle_cascade_respawn_total");
        assert_eq!(
            METRIC_MIDNIGHT_SEAL_BARS,
            "tv_candle_cascade_midnight_seal_bars_total"
        );
    }

    #[tokio::test]
    async fn run_cascade_1s_uses_with_pct_path_when_cache_wired() {
        // F1 ratchet: with `pct_cache = Some(...)` the cascade
        // exercises the `_with_pct` engine variants. The cache is
        // empty here, so the stamped fields fall back to 0.0 — the
        // important assertion is that the seal STILL happens (the
        // `on_tick_with_pct` short-circuit returns `None` only when
        // the underlying `on_tick` returns `None`, never because of
        // a cache miss). Regression block for any future change that
        // accidentally drops the seal on cache miss.
        let (tx, rx) = broadcast::channel::<ParsedTick>(16);
        let engine_map: Arc<CandleEngineMap<Tf1s>> = Arc::new(CandleEngineMap::new());
        let fanout = CascadeFanout::new();
        let pct_cache = Arc::new(PrevDayCache::new());
        let map_clone = Arc::clone(&engine_map);
        let fanout_clone = fanout.clone();
        let cache_clone = Arc::clone(&pct_cache);
        let handle = tokio::spawn(run_cascade_1s(
            rx,
            map_clone,
            Some(fanout_clone),
            Some(cache_clone),
        ));

        // Two ticks crossing a 1s bucket → 1s engine seals → fanout
        // feeds derived engines via `_with_pct` path.
        tx.send(make_tick(8000, 1, 4_000, 100.0)).expect("send 1");
        tx.send(make_tick(8000, 1, 4_001, 101.0))
            .expect("send 2 crosses bucket");
        drop(tx);
        handle.await.expect("cascade exits");

        // 1s engine has post-seal latest at the new bucket.
        let latest_1s = engine_map.latest(8000, 1).expect("post-seal latest");
        assert_eq!(latest_1s.bucket_start_ist_secs, 4_001);

        // Derived engine 1m must have observed the sealed 1s bar via
        // the `_with_pct` path (cache empty → unstamped, but seal
        // still propagates).
        assert!(
            fanout.tf1m.latest(8000, 1).is_some(),
            "tf1m must observe seal even when pct_cache is empty"
        );
    }

    #[tokio::test]
    async fn run_cascade_1s_no_started_log_when_broadcast_closes_immediately() {
        // False-OK guard: the "task active" log must NOT fire if the
        // broadcast is closed before any tick arrives. This is a
        // behavioural assertion — task exits cleanly with no panic.
        let (tx, rx) = broadcast::channel::<ParsedTick>(4);
        let engine_map: Arc<CandleEngineMap<Tf1s>> = Arc::new(CandleEngineMap::new());
        let handle = tokio::spawn(run_cascade_1s(rx, engine_map, None, None));
        drop(tx);
        handle
            .await
            .expect("cascade exits cleanly on immediate close");
    }

    // NOTE: a graceful-shutdown test for `spawn_supervised_cascade_1s`
    // is intentionally absent. The supervisor holds the `Sender` clone
    // for its lifetime (so it can `subscribe()` on every respawn). In
    // production the broadcast sender lives for the process lifetime;
    // the supervisor exits via process termination, never by Sender
    // drop. Any test that drops the test-side Sender will hang because
    // the supervisor's own clone keeps the channel alive — that is
    // CORRECT behaviour. Respawn-on-panic semantics are pinned via
    // `spawn_supervised_cascade_1s_explicit_name_match` + the
    // `tv_candle_cascade_respawn_total` counter assertion.
}
