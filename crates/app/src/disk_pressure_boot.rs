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
#[must_use]
pub fn pressure_config(cfg: &PartitionRetentionConfig) -> PartitionRetentionConfig {
    let days = pressure_hot_days(cfg);
    PartitionRetentionConfig {
        market_data_hot_days: days,
        depth_hot_days: days,
        ..cfg.clone()
    }
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

    loop {
        ticker.tick().await;
        let now_secs = started.elapsed().as_secs();

        let probe = match probe_disk_free_bytes(&data_dir) {
            DiskHealthOutcome::Ok {
                free_bytes,
                total_bytes,
            } => match used_pct_from(free_bytes, total_bytes) {
                Some(pct) => {
                    used_gauge.set(f64::from(pct));
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
                metrics::counter!("tv_disk_pressure_passes_total").increment(1);
                let dropped = run_one_pass(&questdb, &cfg).await;
                if let Some(n) = dropped {
                    metrics::counter!("tv_disk_pressure_partitions_dropped_total").increment(n);
                }
                apply_action(&mut state, action, now_secs, dropped);
            }
            PressureAction::Escalate => {
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
            info!(
                verified = summary.verified,
                dropped = summary.dropped,
                failed = summary.failed,
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
            depth_hot_days: 3,
            archive_enabled: true,
            pressure_archive_enabled: true,
            pressure_hot_days: 2,
            ..PartitionRetentionConfig::default()
        }
    }

    #[test]
    fn pressure_config_compresses_market_data_not_audit() {
        let p = pressure_config(&cfg());
        assert_eq!(p.market_data_hot_days, 2, "ticks + candles compress");
        assert_eq!(p.depth_hot_days, 2, "depth compresses — it is the heaviest");
        assert_eq!(
            p.retention_days, 90,
            "audit/daily tables are small and several are SEBI 5y — pressure comes \
             from market data, so pressure acts on market data"
        );
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
        assert_eq!(p.depth_hot_days, 2);
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
}
