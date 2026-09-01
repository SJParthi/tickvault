//! Pressure-triggered partition archival — the supervised loop.
//!
//! The decision lives in [`tickvault_storage::disk_pressure`] and is pure.
//! This module is the I/O half: probe the volume, ask what to do, do it.
//!
//! # The one property worth checking in review
//!
//! **This module contains no `DROP` of anything.** When the decision says to
//! act, it calls
//! [`PartitionArchiver::archive_and_drop_old_partitions`](tickvault_storage::partition_archive::PartitionArchiver::archive_and_drop_old_partitions)
//! — the unchanged, already-ratcheted path whose `VerifiedArchive` type-state
//! makes "drop without a verified S3 copy" unrepresentable. A partition that
//! could not be exported, uploaded, checksum-matched, row-count-matched and
//! audit-logged is KEPT.
//!
//! That is the whole safety argument for triggering archival mid-session
//! rather than only after the close: the trigger is new, the deletion is not.
//!
//! # Why mid-session at all
//!
//! The daily archive leg is triggered by partition AGE and runs once, after
//! market close. At the authorized scale the modelled volume fills a 200 GB
//! root well inside a session — hours before anything is age-eligible and
//! hours before that run. A full volume blocks every QuestDB write, which
//! backs the ILP flush up, which backs the frame drain up, which overflows
//! the socket receive buffer — and Dhan's own published architecture states
//! that a slow consumer is skipped forward to "the latest available state",
//! i.e. the intermediate ticks are dropped at the vendor. So a disk problem
//! becomes a tick-loss problem, and bounding the disk is part of staying a
//! fast consumer rather than a separate housekeeping errand.

use std::path::PathBuf;
use std::time::Duration;

use tickvault_common::config::{PartitionRetentionConfig, QuestDbConfig};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::ingest_shed::{
    INGEST_SHED, decide_shed_level_all_signals, free_fraction_from_used_pct, runway_sessions,
    seconds_to_disk_full,
};
use tickvault_storage::disk_health_watcher::{DiskHealthOutcome, probe_disk_free_bytes};
use tickvault_storage::disk_pressure::{
    PressureAction, PressureProbe, PressureState, apply_action, decide_pressure_action,
    pressure_hot_days, thresholds_are_sane, used_pct_from,
};
use tickvault_storage::partition_archive::PartitionArchiver;
use tracing::{error, info, warn};

/// Poll cadence. 60s matches the disk-health watcher: fast enough that a
/// filling volume is caught with room to work, slow enough that the probe
/// itself is free.
pub const DISK_PRESSURE_POLL_INTERVAL_SECS: u64 = 60;

/// Backoff before respawning the loop after an unexpected exit.
pub const DISK_PRESSURE_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Build the retention config an archive pass should use while under
/// pressure: every high-volume class compressed to the pressure window.
///
/// `retention_days` (the STANDARD class — audit and daily tables) is left
/// alone deliberately. Those tables are small, several carry SEBI 5-year
/// obligations, and compressing them would trade a forensic record for
/// megabytes. Pressure comes from market data, so pressure acts on market
/// data.
/// # Pressure may only ever SHORTEN a window (fixed 2026-08-19)
///
/// Every field below takes `min(configured, pressure_days)`. The previous
/// version assigned `pressure_days` unconditionally, which was a real defect
/// once `depth_hot_days` dropped to 1 the same day: `pressure_hot_days`
/// floors at `PRESSURE_MIN_HOT_DAYS` (2), so a disk-pressure episode
/// **RAISED** the depth window 1 -> 2 and retained an EXTRA day of the
/// single heaviest table on the box — measured at 505,807,280 rows in one
/// session. The emergency path made the emergency worse, then escalated
/// "pressure could not be relieved".
///
/// `intraday_hot_days` is now compressed too. It was carried through
/// untouched by `..cfg.clone()`, so `ticks` plus the sixteen sub-minute
/// candle tables — the heaviest class after depth — were structurally
/// invisible to disk pressure. Benign only because the value happens to be 1
/// today; at any larger value pressure could not touch them at all.
#[must_use]
pub fn pressure_config(cfg: &PartitionRetentionConfig) -> PartitionRetentionConfig {
    let days = pressure_hot_days(cfg);
    PartitionRetentionConfig {
        // min(), never a bare assignment: pressure SHORTENS or does nothing.
        market_data_hot_days: cfg.market_data_hot_days.min(days),
        depth_hot_days: cfg.depth_hot_days.min(days),
        intraday_hot_days: cfg.intraday_hot_days.min(days),
        // The marker the archiver reads to keep `market_depth` on its
        // hour-granular window while a spill replay may be in flight.
        //
        // Set ONLY here. Under sustained pressure the spill dirs are
        // non-empty BECAUSE the disk is full, so deferring to the day window
        // makes the one hourly-reclaimable table unreclaimable exactly when
        // reclaiming it is the point. The drop itself is unchanged and still
        // fail-closed: export -> recount -> HeadObject -> `VerifiedArchive`.
        under_disk_pressure: true,
        ..cfg.clone()
    }
}

/// Free-space GAIN that invalidates the burn anchor.
///
/// 8 GiB is well above poll-to-poll jitter and well below a meaningful
/// archival pass, so it separates "the volume is the one we anchored" from
/// "something gave space back" without re-anchoring on noise.
const RE_ANCHOR_ON_GAIN_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Seconds since IST midnight, right now.
///
/// The exhaustion decision needs the CAPTURE window's remaining time, and that
/// window is defined in seconds-of-day IST (`TICK_PERSIST_*`). Derived here
/// rather than threaded through the pure decision so the decision stays a
/// function of numbers only and remains testable without a clock.
fn secs_of_day_ist() -> u32 {
    let ist = chrono::Utc::now().timestamp()
        + i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS);
    // `rem_euclid` rather than `%`: a pre-1970 clock would make `%` negative,
    // and a negative seconds-of-day silently reads as "before the window
    // opened" — which HOLDS the gate rather than shedding, but only by luck.
    u32::try_from(ist.rem_euclid(86_400)).unwrap_or(0)
}

/// Spawn the supervised pressure loop.
///
/// Returns `None` when the feature is off or the thresholds are unusable —
/// the task is not spawned at all, so a disabled build is byte-identical to
/// the pre-2026-08-19 behaviour.
// Its two REFUSAL paths are directly tested below
// (`spawn_supervised_disk_pressure_loop_returns_none_when_disabled` and
// `..._refuses_unusable_thresholds`) — those are the paths that decide whether
// anything runs at all, so they are the ones worth pinning. The spawned body is
// covered indirectly: the decision it drives is unit-tested in
// `tickvault_storage::disk_pressure` (18 tests) and the config derivation by
// `pressure_config_compresses_market_data_not_audit`.
// and a negative seconds-of-day silently reads as "before the window
pub fn spawn_supervised_disk_pressure_loop(
    data_dir: PathBuf,
    questdb: QuestDbConfig,
    cfg: PartitionRetentionConfig,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.pressure_archive_enabled {
        info!("disk-pressure archival disabled — daily age-based archival only");
        return None;
    }
    if !thresholds_are_sane(&cfg) {
        // Refuse rather than silently repair: a threshold pair the operator
        // did not mean is a mistake they need to see, and quietly "fixing" it
        // would run behaviour nobody asked for.
        warn!(
            high_water_pct = cfg.pressure_high_water_pct,
            low_water_pct = cfg.pressure_low_water_pct,
            "disk-pressure archival REFUSED: thresholds are unusable (low must be \
             strictly below high, and high in 1..=100) — fix the config to enable it"
        );
        return None;
    }

    info!(
        high_water_pct = cfg.pressure_high_water_pct,
        low_water_pct = cfg.pressure_low_water_pct,
        hot_days = pressure_hot_days(&cfg),
        max_passes = cfg.pressure_max_passes,
        cooldown_secs = cfg.pressure_min_interval_secs,
        path = %data_dir.display(),
        "disk-pressure archival ARMED (archives to S3 with verification before any \
         drop; never deletes unarchived data)"
    );

    Some(tokio::spawn(async move {
        loop {
            let d = data_dir.clone();
            let q = questdb.clone();
            let c = cfg.clone();
            let handle = tokio::spawn(async move { run_disk_pressure_loop(d, q, c).await });
            match handle.await {
                Ok(()) => {
                    // The inner loop never returns on its own; reaching here
                    // means something ended it, so respawn rather than
                    // silently leaving the volume unwatched.
                    warn!("disk-pressure loop exited cleanly — respawning");
                }
                Err(err) => {
                    error!(
                        ?err,
                        "disk-pressure loop died — respawning (the volume is unwatched \
                         until it returns)"
                    );
                }
            }
            metrics::counter!("tv_disk_pressure_respawn_total").increment(1);
            tokio::time::sleep(Duration::from_secs(DISK_PRESSURE_RESPAWN_BACKOFF_SECS)).await;
        }
    }))
}

/// The loop body. Separated from the supervisor so a panic is caught by the
/// `JoinHandle` above rather than taking the whole task tree down.
async fn run_disk_pressure_loop(
    data_dir: PathBuf,
    questdb: QuestDbConfig,
    cfg: PartitionRetentionConfig,
) {
    let mut state = PressureState::default();
    let mut ticker = tokio::time::interval(Duration::from_secs(DISK_PRESSURE_POLL_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Monotonic seconds since the loop started — supplied to the pure
    // decision so the cooldown does not depend on a wall clock that can jump.
    let started = tokio::time::Instant::now();
    let used_gauge = metrics::gauge!("tv_data_disk_used_pct");
    // The burn anchor: (free_bytes, monotonic_secs) of the first trustworthy
    // reading. Everything the box knows about its own burn rate is the
    // difference between this and now, so it is deliberately NOT re-taken on
    // every poll — a rolling short window measures noise, and the whole point
    // is the session rate.
    //
    // Re-anchored on a large INCREASE in free space, because that is a
    // different volume than the one anchored: an archival pass reclaimed, the
    // operator deleted something, or the disk was resized. Continuing to
    // measure against the old anchor after a 40 GB reclaim would report a
    // burn that already happened as if it were still ahead of us.
    let mut burn_anchor: Option<(u64, u64)> = None;

    loop {
        ticker.tick().await;
        let now_secs = started.elapsed().as_secs();

        // Carried beside the probe rather than inside it: `PressureProbe` is a
        // storage-crate type whose only job is the retention decision, and the
        // runway trigger needs the RAW byte count that the percentage threw
        // away. Widening that type to serve a second consumer would couple two
        // decisions that must be able to disagree.
        let mut free_bytes_seen: Option<u64> = None;
        let probe = match probe_disk_free_bytes(&data_dir) {
            DiskHealthOutcome::Ok {
                free_bytes,
                total_bytes,
            } => match used_pct_from(free_bytes, total_bytes) {
                Some(pct) => {
                    used_gauge.set(f64::from(pct));
                    free_bytes_seen = Some(free_bytes);
                    PressureProbe::used(pct)
                }
                None => {
                    // An impossible reading is treated exactly like a failed
                    // one. Never act on a number the probe could not have
                    // meant.
                    metrics::counter!("tv_disk_pressure_probe_failed_total").increment(1);
                    PressureProbe::failed()
                }
            },
            DiskHealthOutcome::ProbeFailed { reason } => {
                metrics::counter!("tv_disk_pressure_probe_failed_total").increment(1);
                warn!(
                    reason,
                    path = %data_dir.display(),
                    "disk-pressure probe failed — treating as no-pressure this poll \
                     (a blind probe must never trigger a drop)"
                );
                PressureProbe::failed()
            }
        };

        let action = decide_pressure_action(probe, &state, &cfg, now_secs);
        let used = probe.used_pct.unwrap_or(0);

        // The SECOND lever. Everything below this line reclaims space by
        // deleting old data; this one stops writing new data, and it only
        // engages once deleting has nothing left to give.
        //
        // `Escalate` is exactly that signal: it means every partition old
        // enough to archive already is, so retention is at its floor. On a
        // poll where it does NOT fire, the decision HOLDS the current
        // level rather than lifting it — restoring is governed by the free
        // space recovering, not by a quiet poll.
        //
        // A failed probe reports `used_pct: None`, which would read as 0%
        // used = 100% free and RESTORE everything on a blind reading. So a
        // blind poll skips the decision entirely and holds, matching the
        // surrounding loop's rule that a blind probe never triggers an action.
        if let Some(pct) = probe.used_pct {
            let free = free_fraction_from_used_pct(pct);
            let retention_at_floor = matches!(action, PressureAction::Escalate);
            // The runway half, and the fallback is the load-bearing part. A
            // byte count we did not get is an UNKNOWN runway, never a zero
            // one: `runway_sessions` is inert on a zero BURN, so standing the
            // burn down leaves the fractional half deciding alone. Passing a
            // 0-byte fallback with a real burn would read as no runway at all
            // and shed everything on a poll that saw nothing — the same class
            // of blind-reading bug the `Some(pct)` binding above prevents.
            // Anchor maintenance, before the decision reads it.
            if let Some(bytes) = free_bytes_seen {
                match burn_anchor {
                    None => burn_anchor = Some((bytes, now_secs)),
                    Some((anchored, _))
                        if bytes > anchored.saturating_add(RE_ANCHOR_ON_GAIN_BYTES) =>
                    {
                        burn_anchor = Some((bytes, now_secs));
                    }
                    Some(_) => {}
                }
            }

            let (runway_free_bytes, runway_burn) = match free_bytes_seen {
                Some(bytes) => (bytes, cfg.ingest_shed_session_burn_bytes),
                None => (0, 0),
            };
            let next = decide_shed_level_all_signals(
                INGEST_SHED.level(),
                free,
                runway_free_bytes,
                runway_burn,
                retention_at_floor,
                burn_anchor,
                now_secs,
                secs_of_day_ist(),
            );
            // Published every poll, not only on a transition: a runway that
            // is quietly shortening across a session is the signal an
            // operator wants BEFORE the gate arms, and a gauge that only
            // moved at transitions could never show the approach.
            // The box.s own measurement, published so the operator can see the
            // burn it is acting on rather than only its verdict.
            let secs_to_full = burn_anchor
                .and_then(|(af, asec)| seconds_to_disk_full(af, asec, runway_free_bytes, now_secs));
            if let Some(secs) = secs_to_full {
                // The gauge is f64 and the value is at most ~86,400 seconds.
                // APPROVED: seconds within a day, far below 2^53.
                #[allow(clippy::cast_precision_loss)]
                metrics::gauge!("tv_disk_seconds_to_full").set(secs as f64);
            }

            let runway = runway_sessions(runway_free_bytes, runway_burn);
            if let Some(sessions) = runway {
                metrics::gauge!("tv_disk_runway_sessions").set(sessions);
            }
            if INGEST_SHED.set(next) {
                metrics::counter!("tv_ingest_shed_transitions_total", "level" => next.as_str())
                    .increment(1);
                warn!(
                    level = next.as_str(),
                    used_pct = pct,
                    runway_sessions = runway.unwrap_or(f64::NAN),
                    secs_to_disk_full = secs_to_full.unwrap_or(0),
                    retention_at_floor,
                    "ingest shedding CHANGED — the box is writing less to stay alive on a \
                     full disk. Ticks are never shed at any level; order-book depth is \
                     dropped first inline, then entirely, and comes back automatically \
                     once free space recovers"
                );
            }
        }

        match action {
            PressureAction::Disabled | PressureAction::Idle | PressureAction::Hold => {
                apply_action(&mut state, action, now_secs, None);
            }
            PressureAction::Cooldown => {
                info!(
                    used_pct = used,
                    "disk above high water but within the pressure cooldown — holding"
                );
                apply_action(&mut state, action, now_secs, None);
            }
            PressureAction::EndEpisode => {
                info!(
                    used_pct = used,
                    low_water_pct = cfg.pressure_low_water_pct,
                    "disk-pressure episode ENDED — volume back below low water"
                );
                apply_action(&mut state, action, now_secs, None);
            }
            PressureAction::StartEpisode | PressureAction::ContinuePass => {
                if matches!(action, PressureAction::StartEpisode) {
                    metrics::counter!("tv_disk_pressure_episodes_total").increment(1);
                    warn!(
                        used_pct = used,
                        high_water_pct = cfg.pressure_high_water_pct,
                        hot_days = pressure_hot_days(&cfg),
                        "disk-pressure episode STARTED — archiving aged partitions to S3 \
                         now (verified before every drop; today and yesterday are never \
                         eligible)"
                    );
                }
                // Wake the spill/WAL reclaim sweep NOW rather than letting it
                // wait out its 6-hourly timer (2026-09-01).
                //
                // This is the deadlock breaker. QuestDB tables suspend when
                // the volume fills, and `partition_archive` correctly refuses
                // to archive a suspended table — so the archival pass below
                // can reclaim NOTHING once the dominant table is suspended.
                // Our own capture segments are the one thing on this volume
                // that QuestDB does not own, so pruning them is the only
                // reclaim that still works in that state, and it is what lets
                // `wal_auto_resume` reach its free-space threshold.
                //
                // Requested BEFORE the pass, not after: the pass is the slow
                // part, and on a suspended table it is also the useless part.
                // Rate-floored inside the sweep, so a long episode cannot turn
                // a cold path into a hot loop.
                crate::reclaim_signal::request_reclaim();

                metrics::counter!("tv_disk_pressure_passes_total").increment(1);
                let dropped = run_one_pass(&questdb, &cfg).await;
                if let Some(n) = dropped {
                    metrics::counter!("tv_disk_pressure_partitions_dropped_total").increment(n);
                }
                apply_action(&mut state, action, now_secs, dropped);
            }
            PressureAction::Escalate => {
                // Escalation is the state where this matters MOST: the
                // archival pass reclaimed nothing, which on a full volume is
                // the signature of suspended tables. The spill sweep is then
                // the only reclaim left, so ask for it here too — the pass
                // arm above is not reached on an escalated poll.
                crate::reclaim_signal::request_reclaim();

                metrics::counter!("tv_disk_pressure_unrelievable_total").increment(1);
                error!(
                    code = ErrorCode::StorageGap05DiskPressureUnrelievable.code_str(),
                    used_pct = used,
                    high_water_pct = cfg.pressure_high_water_pct,
                    hot_days = pressure_hot_days(&cfg),
                    passes_used = state.episode.map_or(0, |e| e.passes_used),
                    last_pass_dropped = state.episode.and_then(|e| e.last_pass_dropped),
                    "disk pressure could NOT be relieved — every partition old enough to \
                     archive is already archived, or could not be verified into S3 and was \
                     correctly kept. NOTHING further will be deleted: the remaining \
                     partitions are either still being written to or have no verified \
                     copy, and both are data-loss trades that are the operator's to make. \
                     Remedy: grow the gp3 volume (online, one command, never shrinkable) \
                     or reduce ingest scope"
                );
                apply_action(&mut state, action, now_secs, None);
            }
        }
    }
}

/// One archive pass. Returns the partitions dropped, or `None` when the pass
/// could not run at all (no bucket resolvable, client build failure).
///
/// `None` and `Some(0)` are deliberately distinct: `Some(0)` means the pass
/// ran and found nothing reclaimable, which escalates; `None` means the pass
/// never happened, which should not be read as "nothing left to reclaim".
async fn run_one_pass(questdb: &QuestDbConfig, cfg: &PartitionRetentionConfig) -> Option<u64> {
    let pressure_cfg = pressure_config(cfg);
    match PartitionArchiver::new(questdb, &pressure_cfg).await {
        Ok(Some(mut archiver)) => {
            let summary = archiver.archive_and_drop_old_partitions().await;
            // The pass may have returned WITHOUT running — the daily leg holds
            // the concurrency guard, or the WAL probe failed closed. Both hand
            // back an all-zero summary, and `dropped == 0` is this function's
            // escalation signal, so reporting it as `Some(0)` would raise a
            // false STORAGE-GAP-05 Critical for a pass that never started.
            // `None` is exactly the "never happened" case this function's own
            // doc comment reserves.
            if !summary.pass_ran {
                info!(
                    "disk-pressure archive pass did not run (another pass holds the \
                     archive guard, or the WAL probe failed closed) — reporting as \
                     not-run rather than as nothing-reclaimable"
                );
                return None;
            }
            info!(
                verified = summary.verified,
                dropped = summary.dropped,
                failed = summary.failed,
                tables_wal_suspended = summary.tables_wal_suspended,
                rows_archived = summary.rows_archived,
                gzip_bytes_uploaded = summary.gzip_bytes_uploaded,
                "disk-pressure archive pass complete (every drop had a verified S3 copy)"
            );
            Some(u64::from(summary.dropped))
        }
        Ok(None) => {
            // No bucket resolvable. Archival is the ONLY way this loop frees
            // space, so without it there is nothing to do but say so — and
            // NOT report it as "nothing reclaimable", which would escalate
            // with a misleading reason.
            warn!(
                "disk-pressure pass skipped: no archive bucket resolvable (set \
                 [partition_retention] archive_bucket or TV_ENVIRONMENT) — no partition \
                 can be freed until this is fixed"
            );
            None
        }
        Err(err) => {
            error!(
                ?err,
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                "disk-pressure pass could not build the archiver — no partition touched"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_questdb() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    fn cfg() -> PartitionRetentionConfig {
        PartitionRetentionConfig {
            retention_days: 90,
            market_data_hot_days: 35,
            // 1, matching config/base.toml since 2026-08-19. The fixture used
            // 3 before, which MASKED the widening defect: with 3 > the
            // pressure floor of 2, min() and a bare assignment agree, so the
            // bug was invisible to every test that used this fixture.
            depth_hot_days: 1,
            intraday_hot_days: 1,
            archive_enabled: true,
            pressure_archive_enabled: true,
            pressure_hot_days: 2,
            ..PartitionRetentionConfig::default()
        }
    }

    #[test]
    fn pressure_config_compresses_market_data_not_audit() {
        let p = pressure_config(&cfg());
        assert_eq!(
            p.market_data_hot_days, 2,
            "minute history compresses 35 -> 2"
        );
        // 1, NOT 2. Re-blessed 2026-08-19: this line asserted 2, which was
        // the DEFECT stated as the contract — pressure raising the depth
        // window from the configured 1 to the floor of 2, retaining an extra
        // day of the heaviest table exactly when the disk is full.
        assert_eq!(
            p.depth_hot_days, 1,
            "pressure must never RAISE a window above what is configured"
        );
        assert_eq!(
            p.intraday_hot_days, 1,
            "intraday must be compressible by pressure, and never raised"
        );
        assert_eq!(
            p.retention_days, 90,
            "audit/daily tables are small and several are SEBI 5y — pressure comes \
             from market data, so pressure acts on market data"
        );
    }

    #[test]
    fn pressure_never_widens_any_window() {
        // The general invariant, asserted per class rather than per literal:
        // whatever pressure computes, no class may come back LARGER than it
        // went in. This is the test whose absence let the depth defect ship.
        for configured in [0_u32, 1, 2, 3, 15, 35, u32::MAX] {
            let mut c = cfg();
            c.market_data_hot_days = configured;
            c.depth_hot_days = configured;
            c.intraday_hot_days = configured;
            let p = pressure_config(&c);
            assert!(
                p.market_data_hot_days <= configured,
                "market_data widened {configured} -> {}",
                p.market_data_hot_days
            );
            assert!(
                p.depth_hot_days <= configured,
                "depth widened {configured} -> {}",
                p.depth_hot_days
            );
            assert!(
                p.intraday_hot_days <= configured,
                "intraday widened {configured} -> {}",
                p.intraday_hot_days
            );
        }
    }

    #[test]
    fn pressure_leaves_audit_retention_alone_at_every_input() {
        // The other half of the contract: SEBI/audit tables are never touched
        // by pressure, whatever the market-data windows are set to.
        for configured in [0_u32, 1, 35, u32::MAX] {
            let mut c = cfg();
            c.depth_hot_days = configured;
            assert_eq!(pressure_config(&c).retention_days, 90);
        }
    }

    #[test]
    fn pressure_config_cannot_reach_below_the_floor() {
        let mut c = cfg();
        c.pressure_hot_days = 0;
        let p = pressure_config(&c);
        assert_eq!(
            p.market_data_hot_days, 2,
            "today and yesterday stay untouchable even when the config asks for 0"
        );
        // The configured 1 stands: min(1, floor 2) == 1. The floor bounds how
        // far pressure may COMPRESS, never how much it may retain.
        assert_eq!(p.depth_hot_days, 1);
    }

    #[test]
    fn pressure_config_preserves_everything_else() {
        let mut c = cfg();
        c.archive_bucket = "tv-prod-cold".to_string();
        c.max_partitions_per_run = 200;
        let p = pressure_config(&c);
        assert_eq!(p.archive_bucket, "tv-prod-cold");
        assert_eq!(p.max_partitions_per_run, 200);
        assert!(p.archive_enabled, "the pass still needs the archive leg on");
    }

    #[test]
    fn spawn_supervised_disk_pressure_loop_returns_none_when_disabled() {
        let mut c = cfg();
        c.pressure_archive_enabled = false;
        assert!(
            spawn_supervised_disk_pressure_loop(PathBuf::from("/tmp"), test_questdb(), c,)
                .is_none(),
            "a disabled build must not even spawn the task"
        );
    }

    #[test]
    fn spawn_supervised_disk_pressure_loop_refuses_unusable_thresholds() {
        let mut c = cfg();
        c.pressure_low_water_pct = 90; // above the 75 high water
        assert!(
            spawn_supervised_disk_pressure_loop(PathBuf::from("/tmp"), test_questdb(), c,)
                .is_none(),
            "a threshold pair the operator did not mean must be visible, not quietly fixed"
        );
    }

    #[test]
    fn the_pressure_loop_decides_shedding_only_on_a_reading_it_actually_got() {
        // The failure this pins: `probe.used_pct` is `None` on a blind probe,
        // and `unwrap_or(0)` two lines above reads as 0% used = 100% free.
        // Feeding that to the shed decision would RESTORE every depth feed on
        // the exact poll where the loop cannot see the disk at all.
        let src = include_str!("disk_pressure_boot.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);

        assert!(
            production.contains("if let Some(pct) = probe.used_pct"),
            "the shed decision must be inside a Some(pct) binding, never on the \
             unwrap_or(0) fallback"
        );
        assert!(
            production.contains("free_fraction_from_used_pct(pct)"),
            "the loop measures USED and the gate decides on FREE — the named \
             converter is what stops the two conventions being mixed"
        );
        assert!(
            production.contains("matches!(action, PressureAction::Escalate)"),
            "shedding must be gated on retention having nothing left to reclaim \
             — reclaim first, shed second"
        );
        assert!(
            production.contains("if INGEST_SHED.set(next)"),
            "only a real transition may log — a level held for hours must not \
             repeat the same warning every poll"
        );
    }

    #[test]
    fn a_blind_probe_would_have_restored_everything_if_it_reached_the_decision() {
        // Why the guard above is worth a test rather than a comment: this is
        // the actual arithmetic of the bug it prevents.
        let blind_used = 0_u8; // what `unwrap_or(0)` produces
        let free = free_fraction_from_used_pct(blind_used);
        assert_eq!(
            tickvault_common::ingest_shed::decide_shed_level(
                tickvault_common::ingest_shed::ShedLevel::AllDepth,
                free,
                true
            ),
            tickvault_common::ingest_shed::ShedLevel::None,
            "a blind probe fed through as 0% used restores everything — which \
             is precisely why it must never reach the decision"
        );
    }

    #[test]
    fn the_pressure_loop_decides_shedding_on_runway_as_well_as_percentage() {
        // The 2026-08-28 session ended at 55% free — nowhere near the 15%
        // fractional bar — and roughly one session of writes from a full
        // disk. A loop that consults only the percentage cannot see that, so
        // this pins that it consults both.
        let src = include_str!("disk_pressure_boot.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);

        assert!(
            production.contains("decide_shed_level_all_signals("),
            "the loop must use the runway-aware decision; the percentage-only \
             gate never armed on the day this trigger was written for. \
             Superseded 2026-08-28: the call is now all_signals, which \
             SUBSUMES the configured runway and adds the box.s own measurement"
        );
        assert!(
            production.contains("cfg.ingest_shed_session_burn_bytes"),
            "the burn must come from config — a hardcoded number would decide \
             what a box captures without an operator ever setting it"
        );
        assert!(
            production.contains("tv_disk_runway_sessions"),
            "the runway must be published every poll, or an operator can only \
             ever learn about it from the transition that already happened"
        );
    }

    #[test]
    fn a_byte_count_the_probe_never_returned_stands_the_runway_down() {
        // The blind-reading bug, in its runway form. `free_bytes_seen` is set
        // in the SAME match arm as `used_pct`, so today they are `Some`
        // together — but that coupling is implicit, and a future edit that
        // moved either one would leave `0` bytes flowing into a live burn.
        // Zero free bytes against a real burn reads as ZERO runway, which
        // sheds every order-book row on a poll that saw nothing at all.
        let src = include_str!("disk_pressure_boot.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);

        assert!(
            production.contains("None => (0, 0)"),
            "an absent byte count must stand the BURN down too — passing 0 \
             bytes with a live burn is the blind-probe restore bug inverted"
        );
        assert!(
            !production.contains("free_bytes_seen.unwrap_or(0)"),
            "a bare 0-byte fallback is exactly the shape this guard forbids"
        );

        // And the arithmetic behind it, so the scan above is pinned to a real
        // failure rather than to a spelling.
        assert_eq!(
            tickvault_common::ingest_shed::decide_shed_level_with_runway(
                tickvault_common::ingest_shed::ShedLevel::None,
                0.55,            // a healthy-looking disk
                0,               // the byte count we did NOT get
                148_000_000_000, // a real measured burn
                false,
            ),
            tickvault_common::ingest_shed::ShedLevel::AllDepth,
            "0 bytes with a live burn sheds everything — which is precisely \
             why the burn is stood down when the byte count is missing"
        );
        assert_eq!(
            tickvault_common::ingest_shed::decide_shed_level_with_runway(
                tickvault_common::ingest_shed::ShedLevel::None,
                0.55,
                0, // same blind reading …
                0, // … with the burn stood down, as production does
                false,
            ),
            tickvault_common::ingest_shed::ShedLevel::None,
            "with the burn stood down a blind byte count changes nothing"
        );
    }
}

#[cfg(test)]
mod self_measured_burn_tests {
    use super::*;

    #[test]
    fn the_loop_uses_the_all_signals_decision_and_maintains_an_anchor() {
        // The whole point of this change: the box measures its own burn rather
        // than waiting for an operator to type a number in. A refactor back to
        // the configured-only decision would silently restore the human in the
        // loop that `ingest_shed`'s founding directive forbids.
        let src = include_str!("disk_pressure_boot.rs");
        let marker = concat!("#[cfg(", "test)]");
        let production = src.split(marker).next().unwrap_or(src);

        assert!(
            production.contains("decide_shed_level_all_signals("),
            "the loop must consult the self-measured signal, not only config"
        );
        assert!(
            production.contains("let mut burn_anchor: Option<(u64, u64)> = None;"),
            "the burn anchor is what makes the measurement possible"
        );
        assert!(
            production.contains("RE_ANCHOR_ON_GAIN_BYTES"),
            "free space GROWING means a different volume than the one anchored \
             — measuring against a stale anchor after a reclaim reports a burn \
             that already happened as if it were still ahead"
        );
        assert!(
            production.contains("tv_disk_seconds_to_full"),
            "the measurement must be visible, not only acted on"
        );
    }

    #[test]
    fn the_anchor_is_taken_only_from_a_reading_the_probe_actually_got() {
        // Same hazard as the percentage path: a blind probe reports no bytes,
        // and anchoring on a fabricated 0 would make every later poll compute
        // a colossal burn and shed everything.
        let src = include_str!("disk_pressure_boot.rs");
        let marker = concat!("#[cfg(", "test)]");
        let production = src.split(marker).next().unwrap_or(src);

        assert!(
            production.contains("if let Some(bytes) = free_bytes_seen {"),
            "the anchor must be taken inside a Some(bytes) binding"
        );
    }

    #[test]
    fn seconds_of_day_is_never_negative_even_on_a_pre_epoch_clock() {
        // `%` on a negative timestamp yields a negative seconds-of-day, which
        // reads as "before the window opened" — safe by luck rather than by
        // construction. `rem_euclid` makes it safe by construction.
        let src = include_str!("disk_pressure_boot.rs");
        let marker = concat!("#[cfg(", "test)]");
        let production = src.split(marker).next().unwrap_or(src);
        assert!(
            production.contains("rem_euclid(86_400)"),
            "seconds-of-day must use rem_euclid, not %"
        );

        // And the live value is in range on this machine's clock.
        let now = secs_of_day_ist();
        assert!(now < 86_400, "seconds-of-day out of range: {now}");
    }
}
