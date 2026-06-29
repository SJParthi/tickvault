//! Tick enrichment orchestrator.
//!
//! Sits between the parser and the persistence writer. Computes the four
//! Phase 2 lifecycle values for every tick:
//!
//! | Output | Source |
//! |---|---|
//! | `volume_delta` | `VolumeDeltaTracker::record_tick(sid, seg, current_volume)` |
//! | `prev_day_close` | `PrevDayCloseStamper::get_or_stamp(sid, seg, packet.day_close)` |
//! | `prev_day_oi` | `PrevOiCache::get(sid, seg)` (boot-loaded from candles_1d) |
//! | `phase` | `compute_phase(now_ist_secs_of_day, segment)` pure fn |
//!
//! ## IST midnight rollover (L13)
//!
//! `MidnightRolloverTask::run_once()` performs the atomic phase transition
//! described in the plan §3:
//!
//! 1. Snapshot `last_seen[security]` map → discard
//!    (`VolumeDeltaTracker::clear_for_new_day`).
//! 2. Reload `prev_oi_cache` from `candles_1d`
//!    (`PrevOiCache::load_from_questdb`).
//! 3. Reset `prev_day_close` first-seen stamper
//!    (`PrevDayCloseStamper::clear_for_new_day`).
//! 4. Set phase = PREMARKET (handled by `compute_phase` automatically
//!    once the wall clock crosses 00:00:00 IST).
//! 5. VOLUME-MONO-01 SUPPRESSED until phase reaches OPEN — caller logic
//!    (not enforced here; the consumer of `EnrichedTick` decides).
//! 6. Engine state TTL sweep — drop entries with no tick for >7 days
//!    (deferred to a follow-up commit; this enricher handles 1–4 today).
//!
//! ## Hot-path guarantee
//!
//! `enrich_tick` is O(1) per call. Two papaya `get` calls (cache + stamper),
//! one papaya `insert` (delta tracker), one branch-on-int (phase). Zero
//! allocation, no syscalls, no time-of-day call (caller passes
//! `now_ist_secs_of_day` so the wall-clock read is shared across the
//! batch — typically once per processor loop iteration).

use tracing::info;

use tickvault_common::phase::{Phase, compute_phase_by_segment_code};
use tickvault_common::tick_types::ParsedTick;

use crate::pipeline::prev_day_close_stamper::PrevDayCloseStamper;
use crate::pipeline::prev_oi_cache::PrevOiCache;
use crate::pipeline::volume_delta_tracker::{DeltaOutcome, VolumeDeltaTracker};

/// Lightweight flag bundle extracted from `EnrichedTick` so callers
/// can pass the suppression/state signals around without holding the
/// `'a` lifetime borrow on `ParsedTick`. Used by the tick-processor
/// hot loop to gate VOLUME-MONO-01 (Phase 2.7 hostile-bug-hunt
/// CRITICAL C2 fix).
///
/// All three fields are `Copy`; the struct is `Copy` itself.
#[derive(Clone, Copy, Debug)]
pub struct EnrichedTickFlags {
    pub volume_is_first_seen: bool,
    pub volume_is_regression: bool,
    pub phase: Phase,
}

/// Output of `enrich_tick`. Bundles the original tick with the four
/// Phase 2 lifecycle values plus regression flags from VOLUME-MONO-01.
#[derive(Clone, Copy, Debug)]
pub struct EnrichedTick<'a> {
    pub tick: &'a ParsedTick,
    /// Per-tick incremental volume (signed — can be negative on a
    /// day rollover before VOLUME-MONO-01 suppression kicks in).
    pub volume_delta: i64,
    /// Frozen previous-day close. Equals `tick.day_close` for the first
    /// tick of the day; subsequent ticks reuse the stamped value.
    /// `0.0` when no valid candidate has been observed yet (Ticker
    /// packet only — no prev_close field).
    pub prev_day_close: f32,
    /// Boot-loaded previous-day OI from `candles_1d`. `0` when the
    /// instrument has no prior-day record (new listing, fresh deploy).
    pub prev_day_oi: i64,
    /// Trading-day phase computed from `now_ist_secs_of_day`.
    pub phase: Phase,
    /// `true` when `volume_delta < 0`. Caller (the writer) decides
    /// whether to escalate VOLUME-MONO-01 — the rule is to suppress
    /// during PREMARKET/PREOPEN, escalate during OPEN.
    pub volume_is_regression: bool,
    /// `true` when this is the first tick of the day for this
    /// instrument. Caller suppresses VOLUME-MONO-01 on `is_first_seen`
    /// regardless of phase.
    pub volume_is_first_seen: bool,
}

/// Tick enricher — owns the four state holders that combine to produce
/// the Phase 2 lifecycle columns.
///
/// Cloneable: each field is internally Arc-shared, so cloning is cheap
/// and produces independent handles that operate on the same state.
/// Multiple consumers (the tick processor + the midnight rollover task)
/// hold their own clone.
#[derive(Clone, Default)]
pub struct TickEnricher {
    pub prev_oi_cache: PrevOiCache,
    pub prev_day_close_stamper: PrevDayCloseStamper,
    pub volume_delta_tracker: VolumeDeltaTracker,
}

impl TickEnricher {
    /// Constructs an enricher with empty/unloaded state. The caller MUST
    /// call `prev_oi_cache.load_from_questdb` and verify
    /// `is_loaded()` before subscribe fires (boot-ordering gate L14).
    pub fn new() -> Self {
        Self::default()
    }

    /// Enrich a single tick with the four lifecycle values.
    ///
    /// O(1), zero-alloc, hot-path-safe.
    ///
    /// `now_ist_secs_of_day` should be precomputed once per processor
    /// loop (or once per batch) so this fn is a pure transformation
    /// of `(tick, time)`. The caller owns the wall-clock read.
    #[inline]
    pub fn enrich_tick<'a>(
        &self,
        tick: &'a ParsedTick,
        now_ist_secs_of_day: u32,
    ) -> EnrichedTick<'a> {
        let sid = tick.security_id;
        let seg = tick.exchange_segment_code;

        // Volume delta (record + classify).
        let DeltaOutcome {
            delta,
            is_regression,
            is_first_seen,
        } = self.volume_delta_tracker.record_tick(sid, seg, tick.volume);

        // Previous-day close (first-seen stamp).
        let prev_day_close = self
            .prev_day_close_stamper
            .get_or_stamp(sid, seg, tick.day_close);

        // Previous-day OI (boot-loaded; defaults to 0 if missing).
        let prev_day_oi = self.prev_oi_cache.get(sid, seg).unwrap_or(0);

        // Trading-day phase (pure-fn classifier). Phase 2.13 hot-path
        // M2 fix: use the segment-code variant directly to skip the
        // dead `ExchangeSegment::from_byte().unwrap_or(IdxI)` conversion
        // (the enum result was ignored by compute_phase anyway).
        let phase = compute_phase_by_segment_code(now_ist_secs_of_day, seg);

        EnrichedTick {
            tick,
            volume_delta: delta,
            prev_day_close,
            prev_day_oi,
            phase,
            volume_is_regression: is_regression,
            volume_is_first_seen: is_first_seen,
        }
    }

    /// IST midnight rollover. Call once when the wall clock crosses
    /// 00:00:00 IST. Performs steps 1–3 of L13 atomically (single
    /// thread). Step 2 (prev_oi_cache reload from candles_1d) is
    /// invoked by the caller — pass through `prev_oi_cache.load_from_questdb`.
    ///
    /// Step 4 (phase=PREMARKET) is implicit — `compute_phase` will return
    /// `Phase::Premarket` automatically once the clock crosses midnight.
    /// Step 5 (VOLUME-MONO-01 suppression) is enforced by the caller
    /// based on `enriched.phase` + `enriched.volume_is_first_seen`.
    /// Step 6 (TTL sweep) is a follow-on enhancement (engine state
    /// expiry), not implemented here.
    pub fn rollover_for_new_day(&self) {
        // Step 1: clear last_seen volume baselines.
        self.volume_delta_tracker.clear_for_new_day();
        // Step 3: clear prev_day_close stamps.
        self.prev_day_close_stamper.clear_for_new_day();
        info!(
            "tick_enricher rollover for new day: volume_delta + prev_day_close cleared (prev_oi_cache reload is caller responsibility)"
        );
    }
}

/// Spawns the IST-midnight rollover task per L13 (29-tf engine plan).
///
/// At each IST 00:00:00 boundary the task:
///   1. Calls `enricher.rollover_for_new_day()` — clears
///      volume_delta baselines + prev_day_close stamps.
///   2. Calls `enricher.prev_oi_cache.load_from_questdb(&questdb_config)` —
///      reloads yesterday's OI baseline from the just-closed `candles_1d`
///      partition.
///   3. Sleeps until the next IST midnight.
///
/// Phase 2.7 hostile bug-hunt CRITICAL C1 fix: without this task
/// wired into the slow boot sequence, the volume_delta tracker
/// accumulates baselines across day boundaries and the first ~24,300
/// ticks at 09:15 IST on Day N+1 trigger false VOLUME-MONO-01 alerts.
///
/// O(1) per IST midnight (1 sweep + 1 HTTP call + 86,400 idle
/// seconds). Cancelable by dropping the returned `JoinHandle`.
pub fn spawn_midnight_rollover_task(
    enricher: std::sync::Arc<TickEnricher>,
    questdb_config: tickvault_common::config::QuestDbConfig,
) -> tokio::task::JoinHandle<()> {
    use tickvault_common::market_hours::secs_until_next_ist_midnight;

    tokio::spawn(async move {
        loop {
            let secs = secs_until_next_ist_midnight();
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;

            // L13 step 1 + 3: clear local enricher state.
            enricher.rollover_for_new_day();
            metrics::counter!("tv_midnight_rollover_total").increment(1);

            // L13 step 2: reload prev_oi_cache from yesterday's candles_1d.
            // QuestDB's daily candle for the just-closed day is finalized
            // at the partition boundary; the LATEST ON ts query returns
            // the last_oi value of that candle.
            match enricher
                .prev_oi_cache
                .load_from_questdb(&questdb_config)
                .await
            {
                Ok(count) => info!(
                    entries = count,
                    "midnight rollover: prev_oi_cache reloaded from candles_1d (L13 step 2)"
                ),
                Err(err) => tracing::warn!(
                    ?err,
                    "midnight rollover: prev_oi_cache reload failed; enricher continues with previous cache state (graceful degradation)"
                ),
            }
        }
    })
}

/// Polling interval for `spawn_prev_oi_cache_refresh_task` — 5 minutes.
/// Bounds operator-visible OI staleness on a fresh-deploy boot to one
/// 5-minute window past the time `candles_1d` first becomes populated.
pub const PREV_OI_CACHE_REFRESH_INTERVAL_SECS: u64 = 300;

/// Spawns the periodic prev_oi_cache refresh task per Phase 2.11
/// (hostile bug-hunt M2 + M4 fixes).
///
/// This task addresses TWO scenarios that the boot-time + midnight
/// load do NOT cover:
///
/// **M2 — boot at 23:50 IST window:** the boot-time load reads
/// "today's" `candles_1d` row briefly before midnight; minutes
/// later that row becomes "yesterday's" without an immediate
/// reload. The midnight task fires at 00:00 IST and reloads, but
/// the cache was correct anyway in this case.
///
/// **M4 — fresh deploy with empty `candles_1d`:** boot-time load
/// returns `Ok(count=0)` (legitimate — first trading day on this
/// instance). The cache stays empty until the next IST midnight,
/// which means OI Change panels read 0% for the entire first day.
/// Periodic refresh closes that gap once the matview pipeline
/// produces its first daily candle.
///
/// **M4 — QuestDB recovers post-boot:** if `candles_1d` was
/// unavailable at boot time but Phase 2.9's hard-fail didn't fire
/// (e.g. matview chain still building), this task's poll picks up
/// the data once available.
///
/// ## Behavior
///
/// - Polls every `PREV_OI_CACHE_REFRESH_INTERVAL_SECS` (5 min).
/// - Only retries when `cache.is_empty() == true`. Once the cache
///   has any entries, it's considered hot and the midnight task
///   owns subsequent reloads.
/// - On Ok with non-zero count: logs `info!`, increments
///   `tv_prev_oi_cache_refresh_total{outcome="populated"}`, and
///   the task EXITS (the midnight task takes over from here).
/// - On Ok with count=0: continues polling (cache still empty).
/// - On Err: logs `warn!`, continues polling.
///
/// Cancelable by dropping the returned `JoinHandle`.
pub fn spawn_prev_oi_cache_refresh_task(
    enricher: std::sync::Arc<TickEnricher>,
    questdb_config: tickvault_common::config::QuestDbConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Skip if cache already has data (boot-time load succeeded
        // with non-empty result) — this task only handles the
        // empty-cache recovery path.
        if !enricher.prev_oi_cache.is_empty() {
            info!(
                entries = enricher.prev_oi_cache.len(),
                "prev_oi_cache already populated at boot — refresh task exiting (midnight task owns subsequent reloads)"
            );
            return;
        }

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(
                PREV_OI_CACHE_REFRESH_INTERVAL_SECS,
            ))
            .await;

            if !enricher.prev_oi_cache.is_empty() {
                // Either midnight task or another path populated the
                // cache — we're done.
                info!(
                    entries = enricher.prev_oi_cache.len(),
                    "prev_oi_cache populated by another path — refresh task exiting"
                );
                metrics::counter!(
                    "tv_prev_oi_cache_refresh_total",
                    "outcome" => "external"
                )
                .increment(1);
                return;
            }

            match enricher
                .prev_oi_cache
                .load_from_questdb(&questdb_config)
                .await
            {
                Ok(0) => {
                    // candles_1d still empty — keep polling.
                    metrics::counter!(
                        "tv_prev_oi_cache_refresh_total",
                        "outcome" => "still_empty"
                    )
                    .increment(1);
                }
                Ok(count) => {
                    info!(
                        entries = count,
                        "prev_oi_cache populated by periodic refresh — task exiting (midnight rollover task takes over)"
                    );
                    metrics::counter!(
                        "tv_prev_oi_cache_refresh_total",
                        "outcome" => "populated"
                    )
                    .increment(1);
                    return;
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        "prev_oi_cache periodic refresh failed; will retry in {}s",
                        PREV_OI_CACHE_REFRESH_INTERVAL_SECS
                    );
                    metrics::counter!(
                        "tv_prev_oi_cache_refresh_total",
                        "outcome" => "err"
                    )
                    .increment(1);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tick(security_id: u64, segment_code: u8, volume: u32, day_close: f32) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: segment_code,
            volume,
            day_close,
            ..ParsedTick::default()
        }
    }

    /// Pre-open seconds-of-day (09:00 IST = 32400). Used in test fixtures
    /// without depending on the exact constant module.
    const TEST_PREOPEN_SECS: u32 = 9 * 3600;
    /// Continuous-trading seconds-of-day (09:30 IST).
    const TEST_OPEN_SECS: u32 = 9 * 3600 + 30 * 60;
    /// Pre-market seconds-of-day (07:00 IST).
    const TEST_PREMARKET_SECS: u32 = 7 * 3600;

    #[test]
    fn test_enrich_tick_first_call_marks_first_seen() {
        let e = TickEnricher::new();
        let t = make_tick(1234, 1, 5_000, 100.0);
        let r = e.enrich_tick(&t, TEST_OPEN_SECS);
        assert!(r.volume_is_first_seen);
        assert_eq!(r.volume_delta, 5_000);
        assert!(!r.volume_is_regression);
        assert_eq!(r.prev_day_close, 100.0);
        // No prev_oi loaded — defaults to 0.
        assert_eq!(r.prev_day_oi, 0);
        assert_eq!(r.phase, Phase::Open);
    }

    #[test]
    fn test_enrich_tick_second_call_returns_increment() {
        let e = TickEnricher::new();
        let t1 = make_tick(1234, 1, 5_000, 100.0);
        e.enrich_tick(&t1, TEST_OPEN_SECS);

        let t2 = make_tick(1234, 1, 7_500, 105.0); // day_close changed but stamper holds 100.0
        let r = e.enrich_tick(&t2, TEST_OPEN_SECS);
        assert!(!r.volume_is_first_seen);
        assert_eq!(r.volume_delta, 2_500);
        assert!(!r.volume_is_regression);
        // First-seen stamp held — second tick's day_close ignored.
        assert_eq!(r.prev_day_close, 100.0);
    }

    #[test]
    fn test_enrich_tick_regression_flag() {
        let e = TickEnricher::new();
        let t1 = make_tick(1, 1, 100_000, 50.0);
        e.enrich_tick(&t1, TEST_OPEN_SECS);
        // Cumulative volume regression.
        let t2 = make_tick(1, 1, 50_000, 50.0);
        let r = e.enrich_tick(&t2, TEST_OPEN_SECS);
        assert!(r.volume_is_regression);
        assert_eq!(r.volume_delta, -50_000);
    }

    #[test]
    fn test_enrich_tick_uses_prev_oi_cache() {
        let e = TickEnricher::new();
        e.prev_oi_cache.insert(99, 2, 12_345_678);
        let t = make_tick(99, 2, 1, 100.0);
        let r = e.enrich_tick(&t, TEST_OPEN_SECS);
        assert_eq!(r.prev_day_oi, 12_345_678);
    }

    #[test]
    fn test_enrich_tick_phase_routes_correctly() {
        let e = TickEnricher::new();
        let t = make_tick(1, 1, 1, 100.0);
        assert_eq!(
            e.enrich_tick(&t, TEST_PREMARKET_SECS).phase,
            Phase::Premarket
        );
        assert_eq!(e.enrich_tick(&t, TEST_PREOPEN_SECS).phase, Phase::Preopen);
        assert_eq!(e.enrich_tick(&t, TEST_OPEN_SECS).phase, Phase::Open);
    }

    /// I-P1-11 ratchet: same security_id under different segments
    /// enriches independently — the volume_delta tracker, stamper,
    /// and OI cache all use the composite key.
    #[test]
    fn test_enrich_tick_composite_key_isolation() {
        let e = TickEnricher::new();
        e.prev_oi_cache.insert(13, 0, 1_000_000); // NIFTY IDX_I
        e.prev_oi_cache.insert(13, 1, 50_000); // hypothetical NSE_EQ same id

        let t_idx = make_tick(13, 0, 1_000, 22500.0);
        let t_eq = make_tick(13, 1, 100, 1500.0);
        let r_idx = e.enrich_tick(&t_idx, TEST_OPEN_SECS);
        let r_eq = e.enrich_tick(&t_eq, TEST_OPEN_SECS);

        assert_eq!(r_idx.prev_day_oi, 1_000_000);
        assert_eq!(r_eq.prev_day_oi, 50_000);
        assert_eq!(r_idx.prev_day_close, 22500.0);
        assert_eq!(r_eq.prev_day_close, 1500.0);
        // Both first-seen for the volume tracker.
        assert!(r_idx.volume_is_first_seen);
        assert!(r_eq.volume_is_first_seen);
    }

    /// L13 step 1 + step 3: midnight rollover clears volume baselines
    /// and prev_day_close stamps. Step 2 (OI reload) is caller's
    /// responsibility — verified separately in prev_oi_cache tests.
    #[test]
    fn test_rollover_for_new_day_clears_volume_and_close_stamps() {
        let e = TickEnricher::new();
        let t = make_tick(1, 1, 5_000, 100.0);
        e.enrich_tick(&t, TEST_OPEN_SECS);
        assert!(!e.volume_delta_tracker.is_empty());
        assert!(!e.prev_day_close_stamper.is_empty());

        e.rollover_for_new_day();

        assert!(
            e.volume_delta_tracker.is_empty(),
            "volume tracker must be empty after rollover (L13 step 1)"
        );
        assert!(
            e.prev_day_close_stamper.is_empty(),
            "prev_day_close stamper must be empty after rollover (L13 step 3)"
        );

        // Next tick of the new day starts fresh.
        let t_new = make_tick(1, 1, 50, 105.0);
        let r = e.enrich_tick(&t_new, TEST_PREMARKET_SECS);
        assert!(r.volume_is_first_seen, "post-rollover tick is first-seen");
        assert_eq!(r.prev_day_close, 105.0, "new prev_day_close stamps");
        assert_eq!(r.phase, Phase::Premarket);
    }

    #[test]
    fn test_clone_shares_state() {
        let e1 = TickEnricher::new();
        let e2 = e1.clone();
        let t = make_tick(42, 1, 1_000, 200.0);
        e1.enrich_tick(&t, TEST_OPEN_SECS);
        // Clone sees the same state.
        assert_eq!(e2.volume_delta_tracker.get_last_seen(42, 1), Some(1_000));
        assert_eq!(e2.prev_day_close_stamper.get(42, 1), Some(200.0));
    }

    #[test]
    fn test_new_returns_empty_state() {
        // pub-fn-test guard name match.
        let e = TickEnricher::new();
        assert!(e.volume_delta_tracker.is_empty());
        assert!(e.prev_day_close_stamper.is_empty());
        assert!(!e.prev_oi_cache.is_loaded());
    }

    #[test]
    fn test_enrich_tick_unknown_segment_falls_back_safely() {
        // Code 6 has no enum variant per dhan-ref/08-annexure-enums.md.
        // Should fall back to IdxI for phase computation rather than
        // panic.
        let e = TickEnricher::new();
        let t = make_tick(1, 6, 100, 50.0);
        let r = e.enrich_tick(&t, TEST_OPEN_SECS);
        assert_eq!(r.phase, Phase::Open);
        // Volume / prev_close still tracked by the (sid, segment_code) key,
        // even though segment_code=6 has no enum.
        assert_eq!(r.volume_delta, 100);
    }

    /// Phase 2.11 ratchet: the refresh poll interval must be 5 minutes
    /// (300 seconds). Drift here changes operator-visible OI staleness
    /// recovery latency for fresh deploys.
    #[test]
    fn test_phase2_11_prev_oi_cache_refresh_interval_is_5_minutes() {
        assert_eq!(
            super::PREV_OI_CACHE_REFRESH_INTERVAL_SECS,
            300,
            "PREV_OI_CACHE_REFRESH_INTERVAL_SECS must be 300 (5 minutes) — \
             bounds first-day OI staleness recovery"
        );
    }

    /// Phase 2.11 ratchet: name-matched explicit test for
    /// `spawn_prev_oi_cache_refresh_task` so the pub-fn-test guard
    /// sees a matching `#[test] fn test_*spawn_prev_oi_cache_refresh_task*`.
    /// Same body as the early-exit test below — both exercise the
    /// already-populated short-circuit path.
    #[tokio::test]
    async fn test_spawn_prev_oi_cache_refresh_task_exits_when_cache_already_hot() {
        let enricher = std::sync::Arc::new(TickEnricher::new());
        enricher.prev_oi_cache.insert(7777, 1, 1_234);
        let cfg = tickvault_common::config::QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let handle = super::spawn_prev_oi_cache_refresh_task(std::sync::Arc::clone(&enricher), cfg);
        let r = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        assert!(r.is_ok());
    }

    /// Phase 2.11 ratchet: when the cache is already populated at the
    /// task's start, the refresh task exits immediately (no work to do).
    /// We exercise this by pre-populating the cache and asserting the
    /// task's JoinHandle completes quickly.
    #[tokio::test]
    async fn test_phase2_11_refresh_task_exits_early_when_cache_already_populated() {
        let enricher = std::sync::Arc::new(TickEnricher::new());
        // Pre-populate so the task's early-return path fires.
        enricher.prev_oi_cache.insert(1234, 1, 50_000);
        assert!(!enricher.prev_oi_cache.is_empty());

        let cfg = tickvault_common::config::QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1, // unreachable — irrelevant since task exits early
            pg_port: 1,
            ilp_port: 1,
        };
        let handle = super::spawn_prev_oi_cache_refresh_task(std::sync::Arc::clone(&enricher), cfg);

        // Task should complete within 1s — no I/O happens because the
        // early-return path fires before the first sleep.
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        assert!(
            result.is_ok(),
            "refresh task must exit immediately when cache is pre-populated"
        );
    }
}
