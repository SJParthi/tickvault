//! `[groww_universe]` — process-global daily Groww watch-set + shared-master
//! rider (2026-07-15 Groww live-feed retirement, plan
//! `.claude/plans/active-plan-groww-live-off.md`).
//!
//! Re-home (C1-style), NOT a redesign: the deleted live-lane activation
//! watcher (`groww_activation.rs`) owned the daily
//! `build_and_write_groww_watch` pull-until-success loop and the SOLE
//! `persist_groww_instruments` call. Two KEEP consumers still need that
//! daily work with the live lane gone:
//!
//! 1. The daily watch FILE `data/groww/groww-watch-<date>.json` — the spot
//!    leg's VIX resolver reads it every fire until resolved
//!    (`groww_spot_1m_boot.rs`; without a fresh daily build VIX fail-softs
//!    daily as `vix_unresolved`).
//! 2. The SEBI `feed='groww'` master continuity — `instrument_lifecycle` +
//!    `index_constituency` + the lifecycle audit chain via
//!    `persist_groww_instruments` (GROWW-MASTER-01 degrade contract,
//!    `groww-shared-master-error-codes.md`).
//!
//! Gated on `[groww_universe] enabled` (serde default OFF — the house
//! fail-safe; `config/base.toml` opts in). COLD PATH only: one build per IST
//! day + a fire-and-forget persist; never the tick hot path, never a
//! WebSocket, never an order path. Failures are coded `error!` reusing the
//! EXISTING `GROWW-MASTER-01` code with a `stage = "watch_build"` field for
//! the rider's own build loop (no new ErrorCode variant — banners-only
//! variant policy 2026-07-15); never a panic; bounded per-attempt backoff
//! mirroring the source loop (`min(10 * 2^attempt, 300)`s).

use std::time::Duration;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tracing::{error, info, warn};

use crate::groww_watch_paths::secs_until_day_roll;

/// Today's IST date as `YYYY-MM-DD`. Computed per attempt (never frozen at
/// spawn) — a build loop crossing IST midnight must name the watch file and
/// select FUTIDX with the NEW date (the activation watcher's hostile-review
/// round-3 rule, re-homed verbatim). Pure wall-clock read.
fn today_ist_date() -> String {
    (chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .format("%Y-%m-%d")
    .to_string()
}

/// Resolve the max-subscribe cap exactly as the retired activation spawn site
/// did (`GROWW_MAX_SUBSCRIBE` env override, else the pinned default). The cap
/// bounds the watch-set assembly; the live-subscribe consumer is gone, but the
/// watch FILE the REST legs read keeps the same envelope.
fn resolve_max_subscribe() -> Option<usize> {
    std::env::var("GROWW_MAX_SUBSCRIBE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .or(Some(
            tickvault_core::feed::groww::instruments::GROWW_DEFAULT_MAX_SUBSCRIBE,
        ))
}

/// Respawn backoff for a dead rider task (mirrors the house supervised
/// pattern — disk_health_watcher / groww_spot_1m siblings).
const GROWW_UNIVERSE_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Spawn the SUPERVISED process-global daily rider. Call ONLY when
/// `[groww_universe] enabled = true` — the config gate is the kill switch and
/// the serde default is OFF. Fix-round-1b hardening (2026-07-15): the rider
/// is an infinite loop, so ANY resolution of the inner `JoinHandle` is
/// abnormal — the supervisor logs a coded error (GROWW-MASTER-01,
/// `stage = "task_respawn"`), increments
/// `tv_groww_universe_respawn_total{reason}`, backs off, and respawns so the
/// daily watch build can never die silently (unwind-build self-heal only;
/// release panics abort per `panic = "abort"` — the TICK-FLUSH-01 honesty).
// TEST-EXEMPT: tokio supervisor wiring over an infinite daily control-plane
// loop driving live network/QuestDB I/O; the pure primitives it composes
// (secs_until_day_roll, is_valid_trading_date, build_and_write_groww_watch's
// parse/assemble chain, persist_groww_instruments' builders,
// classify_join_exit) are unit-tested in their own modules; the spawn site +
// call chain are pinned by crates/app/tests/groww_universe_wiring_guard.rs.
pub fn spawn_groww_universe_rider(questdb: QuestDbConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let inner_questdb = questdb.clone();
            let inner = tokio::spawn(run_groww_universe_rider(inner_questdb));
            let result = inner.await;
            if let Err(join_err) = &result {
                if join_err.is_cancelled() {
                    // Graceful shutdown teardown — not an abort.
                    return;
                }
            }
            let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&result);
            metrics::counter!("tv_groww_universe_respawn_total", "reason" => reason).increment(1);
            error!(
                code = ErrorCode::GrowwMaster01PersistFailed.code_str(),
                stage = "task_respawn",
                reason,
                "[groww_universe] daily rider task died — respawning after \
                 backoff so the watch build + master continuity keep firing"
            );
            tokio::time::sleep(Duration::from_secs(GROWW_UNIVERSE_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

/// The rider loop body (supervised above): one build per IST day, forever.
// TEST-EXEMPT: infinite daily control-plane loop (network + fs + QuestDB);
// pure primitives unit-tested in their own modules; wiring pinned by
// crates/app/tests/groww_universe_wiring_guard.rs.
async fn run_groww_universe_rider(questdb: QuestDbConfig) {
    let max_subscribe = resolve_max_subscribe();
    info!(
        ?max_subscribe,
        "[groww_universe] daily watch-set + shared-master rider started \
         (process-global; live lane retired 2026-07-15)"
    );
    loop {
        build_watch_for_today(&questdb, max_subscribe).await;
        // Sleep to the NEXT IST midnight + grace (the same 120s grace the
        // watch-file READERS use), then rebuild for the new date. A build
        // that itself crossed midnight is harmless: the per-attempt date
        // re-derivation already built the new day's file, and the rebuild
        // is idempotent per date (atomic overwrite + DEDUP master rows).
        let sleep_secs = secs_until_day_roll(chrono::Utc::now().timestamp());
        info!(
            sleep_secs,
            "[groww_universe] day build done — sleeping to next IST midnight + grace"
        );
        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
    }
}

/// One day's build: pull-until-success `build_and_write_groww_watch` (the
/// re-homed activation loop, per-attempt date re-derivation + capped
/// exponential backoff), then a fire-and-forget `persist_groww_instruments`
/// spawn (SEBI master continuity — the sole caller repo-wide).
// TEST-EXEMPT: network download + filesystem write + QuestDB persist
// orchestration re-homed verbatim from the activation watcher; every pure
// primitive it composes is unit-tested in instruments.rs / shared_master_writer.rs.
async fn build_watch_for_today(questdb: &QuestDbConfig, max_subscribe: Option<usize>) {
    let cache_dir = std::path::PathBuf::from("data/groww");
    // Fail-closed date guard (re-homed): a permanently-broken clock would make
    // the pull-until-success loop below spin forever on an invalid date —
    // bail loudly for this day instead; the day-roll timer retries tomorrow.
    let entry_date = today_ist_date();
    if !tickvault_core::instrument::instrument_snapshot::is_valid_trading_date(&entry_date) {
        error!(
            code = ErrorCode::GrowwMaster01PersistFailed.code_str(),
            stage = "watch_build",
            entry_date = %entry_date,
            "[groww_universe] daily build aborted — computed watch date is not \
             a valid YYYY-MM-DD; refusing to enter the build loop (would never \
             succeed); next day-roll retries"
        );
        return;
    }
    let mut attempt: u32 = 0;
    loop {
        attempt = attempt.saturating_add(1);
        // Per-attempt date re-derivation (re-homed hostile-review round 3,
        // 2026-07-08): a retry loop that crosses IST midnight must select
        // FUTIDX (and name the watch file) with the NEW date. A per-attempt
        // invalid date falls back to the validated entry value so the loop
        // never spins on a transient clock glitch.
        let attempt_watch_date = {
            let d = today_ist_date();
            if tickvault_core::instrument::instrument_snapshot::is_valid_trading_date(&d) {
                d
            } else {
                entry_date.clone()
            }
        };
        match tickvault_core::feed::groww::instruments::build_and_write_groww_watch(
            &cache_dir,
            &attempt_watch_date,
            max_subscribe,
        )
        .await
        {
            Ok(set) => {
                let index_futures = set
                    .entries
                    .iter()
                    .filter(|e| e.segment.eq_ignore_ascii_case("FNO"))
                    .count();
                info!(
                    entries = set.entries.len(),
                    master_entries = set.master_entries.len(),
                    indices = set.indices,
                    resolved_stocks = set.resolved_stocks,
                    index_futures,
                    unresolved = set.unresolved_stocks.len(),
                    watch_date = %attempt_watch_date,
                    "[groww_universe] Groww watch-list ready (file written for \
                     the REST legs' VIX/contract resolution)"
                );
                // Persist the Groww instrument set into the SHARED
                // `instrument_lifecycle` (+ `index_constituency`) master
                // tables tagged `feed='groww'` (re-homed PR-A spawn — the
                // sole caller repo-wide). Fire-and-forget + degrade-safe: a
                // persist failure logs GROWW-MASTER-01 and returns; it never
                // blocks the rider. Persist under the date the set was BUILT
                // for (the per-attempt date). No dry-run universe ⇒ false.
                let persist_questdb = questdb.clone();
                let persist_date = attempt_watch_date.clone();
                tokio::spawn(async move {
                    tickvault_core::feed::groww::shared_master_writer::persist_groww_instruments(
                        &persist_questdb,
                        &set,
                        &persist_date,
                        false,
                    )
                    .await;
                });
                break;
            }
            Err(err) => {
                let backoff = std::cmp::min(10u64.saturating_mul(1u64 << attempt.min(5)), 300);
                if attempt <= 3 {
                    warn!(
                        ?err,
                        attempt,
                        backoff_secs = backoff,
                        "[groww_universe] Groww watch-list build failed — retrying"
                    );
                } else {
                    error!(
                        code = ErrorCode::GrowwMaster01PersistFailed.code_str(),
                        stage = "watch_build",
                        ?err,
                        attempt,
                        backoff_secs = backoff,
                        "[groww_universe] Groww watch-list build still failing — \
                         pull-until-success (spot-leg VIX resolution + master \
                         continuity degrade until this succeeds)"
                    );
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
            }
        }
    }
}
