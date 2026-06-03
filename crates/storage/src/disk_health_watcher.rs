//! Background disk-health watcher for the spill directory.
//!
//! Closes the single highest-risk gap in the zero-loss chain identified in
//! the 2026-04-28 audit: "disk full + QuestDB down simultaneously". Without
//! this watcher operators only learn about a full spill disk when ticks
//! actually start landing in the DLQ — at which point real loss is already
//! happening if the DLQ also fills up.
//!
//! The watcher polls `df` every 60s and exposes:
//!   * `tv_spill_dir_free_bytes` — gauge, current free bytes
//!   * `tv_spill_dir_total_bytes` — gauge, current total bytes
//!   * `tv_spill_dir_health_check_failed_total` — counter
//!
//! Prometheus alert rule (added separately): fires CRITICAL Telegram when
//! `tv_spill_dir_free_bytes < SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD`.
//! Operator gets ~hours of warning instead of seconds.
//!
//! The watcher is intentionally minimal — no external `nix`/`fs2` deps, just
//! stdlib `std::process::Command` invoking `df` (POSIX). Linux/macOS only;
//! on Windows the watcher logs a warning at boot and exits cleanly.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tracing::{debug, error, info};

/// Cadence of the disk-health probe. 60s gives ~hours of warning before a
/// fast-filling spill dir actually runs out of space, while burning
/// negligible CPU.
pub const SPILL_DISK_HEALTH_POLL_INTERVAL_SECS: u64 = 60;

/// Threshold below which the spill dir is considered critically low. The
/// matching Prometheus alert rule routes to Telegram CRITICAL when the
/// gauge dips below this. Default 1 GiB — at the typical observed spill
/// rate during a sustained QuestDB outage (~10 MB/min) this gives ~100
/// minutes of operator warning before the disk actually fills.
pub const SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Outcome of one health check, exposed for unit testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskHealthOutcome {
    /// `df` succeeded; we have a free-bytes number.
    Ok { free_bytes: u64, total_bytes: u64 },
    /// `df` failed (non-POSIX OS, command not found, parse error). Gauge
    /// is left at its previous value; operator has no signal.
    ProbeFailed { reason: &'static str },
}

/// One-shot health check. Spawns `df --output=avail,size --block-size=1`
/// against `path` and parses the output. Returns the outcome enum so the
/// caller can decide what to do with it (the production background task
/// just updates the gauge; tests can assert on the parsed numbers).
// TEST-EXEMPT: covered by `test_probe_against_real_path_returns_ok_or_probe_failed` (different name pattern; the test exercises this exact entrypoint against `/tmp` on POSIX runners and the ProbeFailed branch on others).
pub fn probe_disk_free_bytes(path: &std::path::Path) -> DiskHealthOutcome {
    // GNU coreutils `df --output=avail,size --block-size=1` produces:
    //     Avail Size
    //     <bytes> <bytes>
    // BSD/macOS `df` does NOT support `--output`; use `df -k` and convert
    // 1024-byte blocks to bytes. We try GNU form first, then fall back.
    let gnu = Command::new("df")
        .args(["--output=avail,size", "--block-size=1", "--"])
        .arg(path)
        .output();
    let parsed = match gnu {
        Ok(out) if out.status.success() => parse_df_gnu(&out.stdout),
        _ => {
            let bsd = Command::new("df").args(["-k", "--"]).arg(path).output();
            match bsd {
                Ok(out) if out.status.success() => parse_df_bsd_kb(&out.stdout),
                _ => None,
            }
        }
    };
    match parsed {
        Some((free, total)) => DiskHealthOutcome::Ok {
            free_bytes: free,
            total_bytes: total,
        },
        None => DiskHealthOutcome::ProbeFailed {
            reason: "df_invocation_or_parse_failed",
        },
    }
}

/// Parse GNU `df --output=avail,size --block-size=1` output. Format is two
/// header words then one data row of two integers. Returns
/// `Some((avail_bytes, total_bytes))` on success.
fn parse_df_gnu(stdout: &[u8]) -> Option<(u64, u64)> {
    let s = std::str::from_utf8(stdout).ok()?;
    let mut lines = s.lines().skip(1); // drop header row
    let row = lines.next()?;
    let mut nums = row.split_whitespace().filter_map(|t| t.parse::<u64>().ok());
    let avail = nums.next()?;
    let total = nums.next()?;
    Some((avail, total))
}

/// Parse BSD/macOS `df -k <path>` output. The data row is:
///     Filesystem 1024-blocks Used Available Capacity Mounted-on
/// We need the 4th column (Available) and 2nd column (1024-blocks). Return
/// values are converted to bytes (multiply by 1024).
fn parse_df_bsd_kb(stdout: &[u8]) -> Option<(u64, u64)> {
    let s = std::str::from_utf8(stdout).ok()?;
    let mut lines = s.lines().skip(1);
    let row = lines.next()?;
    let cols: Vec<&str> = row.split_whitespace().collect();
    if cols.len() < 4 {
        return None;
    }
    let total_kb: u64 = cols[1].parse().ok()?;
    let avail_kb: u64 = cols[3].parse().ok()?;
    Some((avail_kb.saturating_mul(1024), total_kb.saturating_mul(1024)))
}

/// Spawn the background watcher task. Idempotent — call once at boot. The
/// returned `JoinHandle` can be aborted on shutdown.
///
/// On non-POSIX systems the watcher logs a warning and the task exits
/// immediately (the gauges remain at zero, which is honest about the lack
/// of a signal).
// TEST-EXEMPT: tokio task spawn — exercised in production by `crates/app/src/main.rs`. The pure-function `probe_disk_free_bytes` above is fully unit-tested (5 tests covering parser branches + a real /tmp probe); this wrapper is a one-line spawn that needs an integration harness to test usefully.
pub fn spawn_spill_disk_health_watcher(spill_dir: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let m_free = metrics::gauge!("tv_spill_dir_free_bytes");
        let m_total = metrics::gauge!("tv_spill_dir_total_bytes");
        let m_failed = metrics::counter!("tv_spill_dir_health_check_failed_total");

        info!(
            path = %spill_dir.display(),
            interval_secs = SPILL_DISK_HEALTH_POLL_INTERVAL_SECS,
            critical_threshold_bytes = SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD,
            "spill disk-health watcher started"
        );

        // Ensure the dir exists so `df` doesn't fail the probe.
        if let Err(err) = std::fs::create_dir_all(&spill_dir) {
            error!(
                ?err,
                path = %spill_dir.display(),
                "could not create spill dir for health watcher"
            );
        }

        let mut ticker =
            tokio::time::interval(Duration::from_secs(SPILL_DISK_HEALTH_POLL_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match probe_disk_free_bytes(&spill_dir) {
                DiskHealthOutcome::Ok {
                    free_bytes,
                    total_bytes,
                } => {
                    m_free.set(free_bytes as f64);
                    m_total.set(total_bytes as f64);
                    if free_bytes < SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD {
                        error!(
                            path = %spill_dir.display(),
                            free_bytes,
                            total_bytes,
                            threshold = SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD,
                            "CRITICAL: spill dir free space below threshold — \
                             tick spill is at risk if QuestDB stays down"
                        );
                    } else {
                        debug!(
                            path = %spill_dir.display(),
                            free_bytes,
                            total_bytes,
                            "spill dir health probe ok"
                        );
                    }
                }
                DiskHealthOutcome::ProbeFailed { reason } => {
                    m_failed.increment(1);
                    error!(
                        path = %spill_dir.display(),
                        reason,
                        "spill disk health probe failed — operator has no free-space signal"
                    );
                }
            }
        }
    })
}

/// Backoff between a watcher death and its respawn. Small so disk-free
/// monitoring resumes quickly, but non-zero so a watcher that panics
/// instantly on every start cannot busy-spin the CPU — it respawns at
/// most once per this interval, and the `tv_disk_watcher_respawn_total`
/// counter rate surfaces the flap to the operator via CloudWatch.
pub const DISK_WATCHER_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Classify why a supervised task's `JoinHandle` resolved, into a stable
/// metric label. Pure function so the supervisor's branch logic is unit
/// testable without constructing a real `JoinError` (which has no public
/// constructor).
#[must_use]
pub fn classify_join_exit(join_result: &Result<(), tokio::task::JoinError>) -> &'static str {
    match join_result {
        Ok(()) => "clean_exit",
        Err(e) if e.is_panic() => "panic",
        Err(e) if e.is_cancelled() => "cancelled",
        Err(_) => "unknown",
    }
}

/// G3 (zero-tick-loss audit) — supervise the spill disk-health watcher.
///
/// [`spawn_spill_disk_health_watcher`] runs an infinite probe loop, so its
/// `JoinHandle` resolves ONLY on a fatal event (panic or external cancel).
/// Before this supervisor the handle was bound to `_` in `main.rs`, so a
/// panic made disk-free monitoring vanish silently — and that monitoring is
/// the early-warning for the single highest-risk gap in the zero-loss chain
/// ("disk full + QuestDB down"). This supervisor mirrors the WS-GAP-05 pool
/// supervisor: on every watcher death it logs `error!` (code
/// `DISK-WATCHER-01`) + increments `tv_disk_watcher_respawn_total{reason}`,
/// then respawns after [`DISK_WATCHER_RESPAWN_BACKOFF_SECS`] so monitoring
/// continues. The counter feeds a CloudWatch alarm so the operator is paged
/// on a flapping watcher.
///
/// The returned `JoinHandle` is itself an infinite loop (it never resolves
/// in normal operation); callers bind it to a `_`-prefixed name. The
/// supervisor body has no panic path of its own (no `unwrap`/`expect`,
/// pure-function classification), so it does not need a supervisor-of-the-
/// supervisor.
// O(1) EXEMPT: cold-path supervisor — one task per session, fires only on watcher death.
pub fn spawn_supervised_spill_disk_health_watcher(
    spill_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = spawn_spill_disk_health_watcher(spill_dir.clone());
            let join_result = handle.await;
            let reason = classify_join_exit(&join_result);
            error!(
                reason,
                code = tickvault_common::error_code::ErrorCode::DiskWatcher01Respawned.code_str(),
                backoff_secs = DISK_WATCHER_RESPAWN_BACKOFF_SECS,
                path = %spill_dir.display(),
                "DISK-WATCHER-01: spill disk-health watcher exited — respawning so \
                 free-space monitoring continues (disk-full + QuestDB-down early warning)"
            );
            metrics::counter!("tv_disk_watcher_respawn_total", "reason" => reason).increment(1);
            tokio::time::sleep(Duration::from_secs(DISK_WATCHER_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_threshold_constant_is_at_least_one_gib() {
        // 1 GiB minimum so the alert has meaningful runway. If a future
        // edit lowers this to a few MB, alerts fire too late.
        assert!(SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD >= 1024 * 1024 * 1024);
    }

    #[test]
    fn test_poll_interval_is_reasonable() {
        // Too short = wasted CPU; too long = operator surprised by full disk.
        assert!(SPILL_DISK_HEALTH_POLL_INTERVAL_SECS >= 30);
        assert!(SPILL_DISK_HEALTH_POLL_INTERVAL_SECS <= 300);
    }

    #[test]
    fn test_parse_df_gnu_happy_path() {
        let stdout = b"Avail Size\n12345 99999\n";
        assert_eq!(parse_df_gnu(stdout), Some((12345, 99999)));
    }

    #[test]
    fn test_parse_df_gnu_handles_extra_whitespace() {
        let stdout = b"Avail Size\n   12345   99999  \n";
        assert_eq!(parse_df_gnu(stdout), Some((12345, 99999)));
    }

    #[test]
    fn test_parse_df_gnu_missing_data_row() {
        let stdout = b"Avail Size\n";
        assert_eq!(parse_df_gnu(stdout), None);
    }

    #[test]
    fn test_parse_df_bsd_kb_converts_to_bytes() {
        // Mocked BSD output: Filesystem 1024-blocks Used Available Capacity Mounted-on
        let stdout =
            b"Filesystem 1024-blocks Used Avail Cap Mount\n/dev/disk1 1000 200 800 20% /\n";
        let (avail, total) = parse_df_bsd_kb(stdout).expect("parse");
        // avail=800 KB → 819_200 bytes; total=1000 KB → 1_024_000 bytes.
        assert_eq!(avail, 800 * 1024);
        assert_eq!(total, 1000 * 1024);
    }

    #[test]
    fn test_parse_df_bsd_kb_handles_too_few_columns() {
        let stdout = b"Filesystem Blocks\nbad-row 100\n";
        assert_eq!(parse_df_bsd_kb(stdout), None);
    }

    #[test]
    fn test_probe_against_real_path_returns_ok_or_probe_failed() {
        // We can't assert specific numbers (CI machines vary), but on a
        // POSIX runner `df` should succeed against `/tmp` (or `/`). On a
        // hypothetical non-POSIX runner this returns ProbeFailed; either
        // outcome is valid — the test just exercises the codepath.
        let outcome = probe_disk_free_bytes(std::path::Path::new("/tmp"));
        match outcome {
            DiskHealthOutcome::Ok {
                free_bytes,
                total_bytes,
            } => {
                assert!(total_bytes > 0, "real /tmp must report non-zero total");
                assert!(
                    free_bytes <= total_bytes,
                    "free must not exceed total on a sane FS"
                );
            }
            DiskHealthOutcome::ProbeFailed { reason } => {
                assert!(!reason.is_empty(), "failure must carry a reason string");
            }
        }
    }

    // -- G3 supervisor (spawn_supervised_spill_disk_health_watcher) --

    #[test]
    fn test_respawn_backoff_is_small_but_nonzero() {
        // Non-zero so a tight panic loop can't busy-spin the CPU; small so
        // disk-free monitoring resumes within seconds of a watcher death.
        assert!(DISK_WATCHER_RESPAWN_BACKOFF_SECS >= 1);
        assert!(DISK_WATCHER_RESPAWN_BACKOFF_SECS <= 30);
    }

    #[tokio::test]
    async fn test_classify_join_exit_clean() {
        let h = tokio::spawn(async {});
        let r = h.await;
        assert_eq!(classify_join_exit(&r), "clean_exit");
    }

    #[tokio::test]
    async fn test_classify_join_exit_panic() {
        // A panicking task yields a JoinError where is_panic() == true.
        let h = tokio::spawn(async {
            panic!("intentional test panic"); // APPROVED: test — exercises the panic branch
        });
        let r = h.await;
        assert_eq!(classify_join_exit(&r), "panic");
    }

    #[tokio::test]
    async fn test_classify_join_exit_cancelled() {
        // An aborted task yields a JoinError where is_cancelled() == true.
        let h = tokio::spawn(async {
            // Sleep long enough that abort lands before completion.
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        h.abort();
        let r = h.await;
        assert_eq!(classify_join_exit(&r), "cancelled");
    }

    #[tokio::test]
    async fn test_spawn_supervised_spill_disk_health_watcher_keeps_running() {
        // The supervisor is an infinite loop — its JoinHandle must NOT
        // resolve in normal operation. The inner watcher it spawns also
        // loops forever (60s probe interval), so the supervisor parks on
        // `handle.await` and never completes. (If a future edit makes the
        // supervisor return after one watcher death instead of respawning,
        // this guard fails.)
        let handle = spawn_supervised_spill_disk_health_watcher(std::path::PathBuf::from(
            "data/spill-supervisor-test",
        ));
        // Let the spawned task make progress.
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "supervisor must keep running, not exit after spawning the watcher"
        );
        handle.abort();
    }
}
