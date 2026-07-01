//! BP-08 (audit 2026-07-01) — process-level resource early-warning monitor.
//!
//! Closes the RESOURCE-01/02/03 gaps from the 2026-07-01 permutation audit:
//! before this, RESOURCE-01/02/03 were RESERVED stubs — there was NO
//! file-descriptor-count monitor at all (a leaked WS / QuestDB socket could
//! exhaust `LimitNOFILE` with zero signal until `connect()` starts failing),
//! and no process-level RSS-vs-cgroup or spill-vs-free early alarm distinct
//! from the host-aggregate `mem_used_high` / `disk_used_high` CloudWatch
//! alarms. This monitor pages at 80% (fd / RSS) and at <20% free (spill) so
//! the operator acts BEFORE exhaustion.
//!
//! Design mirrors [`crate::oom_monitor`] + [`crate::disk_health_watcher`]:
//!   * PURE classifiers + parsers (unit-tested truth tables);
//!   * a thin fs-read probe wrapper;
//!   * a supervised poll loop (respawn on panic, DISK-WATCHER-01 pattern).
//!
//! Linux-only signals (`/proc`, cgroup). On a non-Linux host or when a source
//! is unreadable the probe returns a `ProbeFailed`/`None` value, the gauge is
//! left honest (no false-OK), and `tv_resource_monitor_probe_failed_total`
//! increments — never a panic, never a false page.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{debug, error, info, warn};

use crate::disk_health_watcher::{DiskHealthOutcome, classify_join_exit, probe_disk_free_bytes};

/// Cadence of the resource probe. 60s matches the OOM + disk watchers; slow-
/// moving resource pressure needs no tighter sampling and the cost is
/// negligible.
pub const RESOURCE_MONITOR_POLL_INTERVAL_SECS: u64 = 60;

/// Backoff between a monitor death and its respawn (mirrors DISK-WATCHER-01 /
/// OOM monitor): small so monitoring resumes quickly, non-zero so a monitor
/// that panics instantly on every start cannot busy-spin.
pub const RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS: u64 = 5;

/// RESOURCE-01: page when open fd count reaches this fraction (percent) of
/// `LimitNOFILE`. 80% leaves headroom to act before `connect()` fails.
pub const FD_HIGH_PCT_THRESHOLD: u64 = 80;

/// RESOURCE-02: page when process VmRSS reaches this fraction (percent) of the
/// cgroup memory limit. 80% is well below the OOM-killer trip so PROC-01 is a
/// last resort, not the first signal.
pub const RSS_HIGH_PCT_THRESHOLD: u64 = 80;

/// RESOURCE-03: page when spill-dir free space drops BELOW this fraction
/// (percent) of total. 20% free is the early-warning floor.
pub const SPILL_FREE_LOW_PCT_THRESHOLD: u64 = 20;

/// Default `/proc/self/fd` directory (open file descriptors, one entry each).
pub const DEFAULT_PROC_SELF_FD_PATH: &str = "/proc/self/fd";
/// Default `/proc/self/limits` file (carries `Max open files`).
pub const DEFAULT_PROC_SELF_LIMITS_PATH: &str = "/proc/self/limits";
/// Default `/proc/self/status` file (carries `VmRSS`).
pub const DEFAULT_PROC_SELF_STATUS_PATH: &str = "/proc/self/status";
/// Default cgroup-v2 memory limit file.
pub const DEFAULT_CGROUP_V2_MEMORY_MAX_PATH: &str = "/sys/fs/cgroup/memory.max";

// ---------------------------------------------------------------------------
// PURE classifiers (truth tables)
// ---------------------------------------------------------------------------

/// PURE: is `used` at or above `pct_threshold`% of `limit`? Used for the fd
/// and RSS "high" checks. A `limit` of 0 (unknown / unlimited) returns `false`
/// (no denominator → no false page). Saturating arithmetic — never panics.
#[must_use]
pub fn is_at_or_above_pct(used: u64, limit: u64, pct_threshold: u64) -> bool {
    if limit == 0 {
        return false;
    }
    // used * 100 >= limit * pct_threshold  (avoid float; saturating so a huge
    // `used` cannot overflow-panic).
    used.saturating_mul(100) >= limit.saturating_mul(pct_threshold)
}

/// PURE: is free space BELOW `pct_threshold`% of `total`? Used for the spill
/// "low free" check. A `total` of 0 returns `false` (unknown → no page).
#[must_use]
pub fn is_free_below_pct(free: u64, total: u64, pct_threshold: u64) -> bool {
    if total == 0 {
        return false;
    }
    // free * 100 < total * pct_threshold
    free.saturating_mul(100) < total.saturating_mul(pct_threshold)
}

// ---------------------------------------------------------------------------
// PURE parsers
// ---------------------------------------------------------------------------

/// PURE parser of a `/proc/self/limits` file body for the soft `Max open
/// files` limit. The file is fixed-width columnar text:
///
/// ```text
/// Limit                     Soft Limit           Hard Limit           Units
/// Max open files            65536                65536                files
/// ```
///
/// Returns the SOFT limit (the effective `LimitNOFILE`) when the line is
/// present and parses; `None` otherwise. `unlimited` maps to `None` (no
/// denominator → no page).
#[must_use]
pub fn parse_max_open_files(limits_body: &str) -> Option<u64> {
    for line in limits_body.lines() {
        if let Some(rest) = line.strip_prefix("Max open files") {
            // The next whitespace-separated token is the soft limit.
            let soft = rest.split_whitespace().next()?;
            return soft.parse::<u64>().ok();
        }
    }
    None
}

/// PURE parser of a `/proc/self/status` file body for `VmRSS` (in bytes). The
/// line is `VmRSS:\t   12345 kB` — value in KiB. Returns bytes.
#[must_use]
pub fn parse_vmrss_bytes(status_body: &str) -> Option<u64> {
    for line in status_body.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kib = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return Some(kib.saturating_mul(1024));
        }
    }
    None
}

/// PURE parser of a cgroup-v2 `memory.max` file body. The file is a single
/// token: either a byte count (`8589934592`) or the literal `max`
/// (unlimited). Returns `Some(bytes)` for a real limit, `None` for `max` /
/// unparseable (no denominator → RESOURCE-02 skipped).
#[must_use]
pub fn parse_cgroup_memory_max_bytes(memory_max_body: &str) -> Option<u64> {
    let token = memory_max_body.trim();
    if token == "max" || token.is_empty() {
        return None;
    }
    token.parse::<u64>().ok()
}

// ---------------------------------------------------------------------------
// Probes (thin fs-read wrappers)
// ---------------------------------------------------------------------------

/// Count the entries in `/proc/self/fd` = open file descriptors. Returns
/// `None` on a non-Linux host (dir missing / unreadable).
// TEST-EXEMPT: thin fs-read wrapper; the count logic is trivial and the None
// branch is exercised by `test_probe_fd_count_missing_dir_is_none`.
#[must_use]
pub fn probe_open_fd_count(fd_dir: &Path) -> Option<u64> {
    let entries = std::fs::read_dir(fd_dir).ok()?;
    Some(entries.filter(std::result::Result::is_ok).count() as u64)
}

/// Read + parse the soft `Max open files` limit. `None` on non-Linux / missing.
// TEST-EXEMPT: thin fs-read + delegate to the fully-tested `parse_max_open_files`.
#[must_use]
pub fn probe_max_open_files(limits_path: &Path) -> Option<u64> {
    let body = std::fs::read_to_string(limits_path).ok()?;
    parse_max_open_files(&body)
}

/// Read + parse VmRSS bytes. `None` on non-Linux / missing.
// TEST-EXEMPT: thin fs-read + delegate to the fully-tested `parse_vmrss_bytes`.
#[must_use]
pub fn probe_vmrss_bytes(status_path: &Path) -> Option<u64> {
    let body = std::fs::read_to_string(status_path).ok()?;
    parse_vmrss_bytes(&body)
}

/// Read + parse the cgroup memory limit. `None` on `max` / non-Linux / missing.
// TEST-EXEMPT: thin fs-read + delegate to the fully-tested `parse_cgroup_memory_max_bytes`.
#[must_use]
pub fn probe_cgroup_memory_max_bytes(memory_max_path: &Path) -> Option<u64> {
    let body = std::fs::read_to_string(memory_max_path).ok()?;
    parse_cgroup_memory_max_bytes(&body)
}

// ---------------------------------------------------------------------------
// Paths bundle
// ---------------------------------------------------------------------------

/// The set of source paths the monitor reads. Bundled so the production spawn
/// takes the platform defaults and tests can point at fixtures.
#[derive(Debug, Clone)]
pub struct ResourceMonitorPaths {
    /// `/proc/self/fd` — open fd entries.
    pub proc_self_fd: PathBuf,
    /// `/proc/self/limits` — `Max open files` soft limit.
    pub proc_self_limits: PathBuf,
    /// `/proc/self/status` — `VmRSS`.
    pub proc_self_status: PathBuf,
    /// cgroup-v2 `memory.max` — process memory ceiling.
    pub cgroup_memory_max: PathBuf,
    /// spill directory (free-percent probe reuses the disk-health `df` probe).
    pub spill_dir: PathBuf,
}

impl ResourceMonitorPaths {
    /// Production defaults (Linux `/proc` + cgroup-v2 + `data/spill`).
    #[must_use]
    pub fn platform_defaults(spill_dir: PathBuf) -> Self {
        Self {
            proc_self_fd: PathBuf::from(DEFAULT_PROC_SELF_FD_PATH),
            proc_self_limits: PathBuf::from(DEFAULT_PROC_SELF_LIMITS_PATH),
            proc_self_status: PathBuf::from(DEFAULT_PROC_SELF_STATUS_PATH),
            cgroup_memory_max: PathBuf::from(DEFAULT_CGROUP_V2_MEMORY_MAX_PATH),
            spill_dir,
        }
    }
}

// ---------------------------------------------------------------------------
// Background task + supervisor
// ---------------------------------------------------------------------------

/// Spawn the background resource monitor. Idempotent — call once at boot. The
/// returned `JoinHandle` can be aborted on shutdown.
///
/// Every 60s it samples fd count, VmRSS, and spill free-space, updates the
/// `tv_open_fds` / `tv_process_rss_bytes` / `tv_spill_free_pct` gauges, and
/// emits RESOURCE-01/02/03 `error!` when a threshold is crossed. On a non-
/// Linux host each probe fails softly (`tv_resource_monitor_probe_failed_total`,
/// no page, no panic).
// TEST-EXEMPT: tokio task spawn — the pure classifiers + parsers are fully
// unit-tested; this wrapper is a probe loop that needs an integration harness.
#[must_use]
pub fn spawn_resource_monitor(paths: ResourceMonitorPaths) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let m_fds = metrics::gauge!("tv_open_fds");
        let m_rss = metrics::gauge!("tv_process_rss_bytes");
        let m_spill_free_pct = metrics::gauge!("tv_spill_free_pct");
        let m_probe_failed = metrics::counter!("tv_resource_monitor_probe_failed_total");

        info!(
            interval_secs = RESOURCE_MONITOR_POLL_INTERVAL_SECS,
            fd_pct = FD_HIGH_PCT_THRESHOLD,
            rss_pct = RSS_HIGH_PCT_THRESHOLD,
            spill_free_pct = SPILL_FREE_LOW_PCT_THRESHOLD,
            "resource monitor started (RESOURCE-01/02/03)"
        );

        let mut ticker =
            tokio::time::interval(Duration::from_secs(RESOURCE_MONITOR_POLL_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;

            // RESOURCE-01 — open fd count vs LimitNOFILE.
            match (
                probe_open_fd_count(&paths.proc_self_fd),
                probe_max_open_files(&paths.proc_self_limits),
            ) {
                (Some(fds), limit_opt) => {
                    m_fds.set(fds as f64);
                    if let Some(limit) = limit_opt
                        && is_at_or_above_pct(fds, limit, FD_HIGH_PCT_THRESHOLD)
                    {
                        error!(
                            code = tickvault_common::error_code::ErrorCode::Resource01FdCountHigh
                                .code_str(),
                            open_fds = fds,
                            limit,
                            pct_threshold = FD_HIGH_PCT_THRESHOLD,
                            "RESOURCE-01: open file-descriptor count at/above \
                             {FD_HIGH_PCT_THRESHOLD}% of LimitNOFILE — inspect \
                             /proc/self/fd for a socket leak before connect() fails"
                        );
                    } else {
                        debug!(open_fds = fds, "resource monitor: fd count ok");
                    }
                }
                (None, _) => {
                    m_probe_failed.increment(1);
                }
            }

            // RESOURCE-02 — VmRSS vs cgroup memory.max.
            match probe_vmrss_bytes(&paths.proc_self_status) {
                Some(rss) => {
                    m_rss.set(rss as f64);
                    if let Some(limit) = probe_cgroup_memory_max_bytes(&paths.cgroup_memory_max)
                        && is_at_or_above_pct(rss, limit, RSS_HIGH_PCT_THRESHOLD)
                    {
                        error!(
                            code =
                                tickvault_common::error_code::ErrorCode::Resource02ResidentMemoryHigh
                                    .code_str(),
                            rss_bytes = rss,
                            limit_bytes = limit,
                            pct_threshold = RSS_HIGH_PCT_THRESHOLD,
                            "RESOURCE-02: process resident memory at/above \
                             {RSS_HIGH_PCT_THRESHOLD}% of the cgroup limit — the OOM \
                             killer (PROC-01) is imminent; right-size the workload"
                        );
                    } else {
                        debug!(rss_bytes = rss, "resource monitor: RSS ok");
                    }
                }
                None => {
                    m_probe_failed.increment(1);
                }
            }

            // RESOURCE-03 — spill-dir free percent (reuse the disk-health probe).
            match probe_disk_free_bytes(&paths.spill_dir) {
                DiskHealthOutcome::Ok {
                    free_bytes,
                    total_bytes,
                } => {
                    let free_pct = if total_bytes == 0 {
                        0.0
                    } else {
                        (free_bytes as f64 / total_bytes as f64) * 100.0
                    };
                    m_spill_free_pct.set(free_pct);
                    if is_free_below_pct(free_bytes, total_bytes, SPILL_FREE_LOW_PCT_THRESHOLD) {
                        error!(
                            code = tickvault_common::error_code::ErrorCode::Resource03SpillFreeLow
                                .code_str(),
                            free_bytes,
                            total_bytes,
                            free_pct,
                            pct_threshold = SPILL_FREE_LOW_PCT_THRESHOLD,
                            path = %paths.spill_dir.display(),
                            "RESOURCE-03: spill-dir free space below \
                             {SPILL_FREE_LOW_PCT_THRESHOLD}% — the zero-loss chain is at \
                             risk if QuestDB stays down; free disk / restore the drain"
                        );
                    } else {
                        debug!(free_pct, "resource monitor: spill free ok");
                    }
                }
                DiskHealthOutcome::ProbeFailed { .. } => {
                    m_probe_failed.increment(1);
                }
            }
        }
    })
}

/// Supervise the resource monitor. [`spawn_resource_monitor`] runs an infinite
/// probe loop, so its `JoinHandle` resolves ONLY on a fatal event (panic or
/// external cancel). This supervisor mirrors the WS-GAP-05 pool supervisor +
/// the DISK-WATCHER-01 + OOM-monitor supervisors: on every monitor death it
/// logs, increments `tv_resource_monitor_respawn_total{reason}`, then respawns
/// after [`RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS`] so resource monitoring can
/// never vanish silently. The returned `JoinHandle` is itself an infinite loop;
/// callers bind it to a `_`-prefixed name. The supervisor body has no panic
/// path of its own (pure-function classification, no `unwrap`/`expect`).
// O(1) EXEMPT: cold-path supervisor — one task per session, fires only on monitor death.
#[must_use]
pub fn spawn_supervised_resource_monitor(
    paths: ResourceMonitorPaths,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = spawn_resource_monitor(paths.clone());
            let join_result = handle.await;
            let reason = classify_join_exit(&join_result);
            warn!(
                reason,
                backoff_secs = RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS,
                "resource monitor exited — respawning so RESOURCE-01/02/03 \
                 monitoring continues"
            );
            metrics::counter!("tv_resource_monitor_respawn_total", "reason" => reason).increment(1);
            tokio::time::sleep(Duration::from_secs(RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- thresholds sanity --

    #[test]
    fn test_thresholds_are_sane() {
        assert!((50..=95).contains(&FD_HIGH_PCT_THRESHOLD));
        assert!((50..=95).contains(&RSS_HIGH_PCT_THRESHOLD));
        assert!((5..=40).contains(&SPILL_FREE_LOW_PCT_THRESHOLD));
    }

    #[test]
    fn test_poll_interval_is_reasonable() {
        assert!(RESOURCE_MONITOR_POLL_INTERVAL_SECS >= 30);
        assert!(RESOURCE_MONITOR_POLL_INTERVAL_SECS <= 300);
    }

    #[test]
    fn test_respawn_backoff_is_small_but_nonzero() {
        assert!(RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS >= 1);
        assert!(RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS <= 30);
    }

    // -- is_at_or_above_pct — truth table --

    #[test]
    fn test_is_at_or_above_pct_below_threshold_is_false() {
        // 79 of 100 at 80% threshold → false.
        assert!(!is_at_or_above_pct(79, 100, 80));
    }

    #[test]
    fn test_is_at_or_above_pct_at_threshold_is_true() {
        // Exactly 80 of 100 at 80% → true (>=).
        assert!(is_at_or_above_pct(80, 100, 80));
    }

    #[test]
    fn test_is_at_or_above_pct_above_threshold_is_true() {
        assert!(is_at_or_above_pct(95, 100, 80));
        assert!(is_at_or_above_pct(100, 100, 80));
    }

    #[test]
    fn test_is_at_or_above_pct_zero_limit_is_false() {
        // Unknown/unlimited limit → no denominator → never page.
        assert!(!is_at_or_above_pct(1_000_000, 0, 80));
    }

    #[test]
    fn test_is_at_or_above_pct_realistic_fd() {
        // 52429 of 65536 = 80.0% → true; 52428 = 79.99% → false.
        assert!(is_at_or_above_pct(52_429, 65_536, 80));
        assert!(!is_at_or_above_pct(52_428, 65_536, 80));
    }

    #[test]
    fn test_is_at_or_above_pct_no_overflow_on_huge_used() {
        // Saturating: a huge `used` must not overflow-panic.
        assert!(is_at_or_above_pct(u64::MAX, 100, 80));
    }

    // -- is_free_below_pct — truth table --

    #[test]
    fn test_is_free_below_pct_above_floor_is_false() {
        // 21% free at 20% floor → NOT below → false.
        assert!(!is_free_below_pct(21, 100, 20));
    }

    #[test]
    fn test_is_free_below_pct_at_floor_is_false() {
        // Exactly 20% free → NOT strictly below → false.
        assert!(!is_free_below_pct(20, 100, 20));
    }

    #[test]
    fn test_is_free_below_pct_below_floor_is_true() {
        // 19% free → below 20% → true.
        assert!(is_free_below_pct(19, 100, 20));
        assert!(is_free_below_pct(0, 100, 20));
    }

    #[test]
    fn test_is_free_below_pct_zero_total_is_false() {
        assert!(!is_free_below_pct(0, 0, 20));
    }

    // -- parse_max_open_files --

    #[test]
    fn test_parse_max_open_files_happy_path() {
        let body = "Limit                     Soft Limit           Hard Limit           Units\n\
                    Max open files            65536                65536                files\n";
        assert_eq!(parse_max_open_files(body), Some(65536));
    }

    #[test]
    fn test_parse_max_open_files_missing_line_is_none() {
        assert_eq!(
            parse_max_open_files("Max locked memory  0  0  bytes\n"),
            None
        );
    }

    #[test]
    fn test_parse_max_open_files_unlimited_is_none() {
        // A literal `unlimited` soft limit does not parse as u64 → None.
        let body = "Max open files            unlimited            unlimited            files\n";
        assert_eq!(parse_max_open_files(body), None);
    }

    // -- parse_vmrss_bytes --

    #[test]
    fn test_parse_vmrss_bytes_happy_path() {
        // VmRSS: 12345 kB → 12345 * 1024 bytes.
        let body = "VmPeak:\t  100000 kB\nVmRSS:\t   12345 kB\nVmData:\t  5000 kB\n";
        assert_eq!(parse_vmrss_bytes(body), Some(12_345 * 1024));
    }

    #[test]
    fn test_parse_vmrss_bytes_missing_is_none() {
        assert_eq!(parse_vmrss_bytes("VmPeak:\t 100 kB\n"), None);
    }

    // -- parse_cgroup_memory_max_bytes --

    #[test]
    fn test_parse_cgroup_memory_max_bytes_numeric() {
        assert_eq!(
            parse_cgroup_memory_max_bytes("8589934592\n"),
            Some(8_589_934_592)
        );
    }

    #[test]
    fn test_parse_cgroup_memory_max_bytes_unlimited_is_none() {
        // `max` = unlimited → no denominator → RESOURCE-02 skipped.
        assert_eq!(parse_cgroup_memory_max_bytes("max\n"), None);
        assert_eq!(parse_cgroup_memory_max_bytes("  max  "), None);
    }

    #[test]
    fn test_parse_cgroup_memory_max_bytes_garbage_is_none() {
        assert_eq!(parse_cgroup_memory_max_bytes("not-a-number\n"), None);
        assert_eq!(parse_cgroup_memory_max_bytes(""), None);
    }

    // -- probe fd count (None branch) --

    #[test]
    fn test_probe_fd_count_missing_dir_is_none() {
        let out = probe_open_fd_count(std::path::Path::new(
            "/nonexistent-resource-monitor-fd-dir-xyz",
        ));
        assert_eq!(out, None);
    }

    #[test]
    fn test_platform_defaults_bundle_paths() {
        let p = ResourceMonitorPaths::platform_defaults(PathBuf::from("data/spill"));
        assert_eq!(p.proc_self_fd, PathBuf::from(DEFAULT_PROC_SELF_FD_PATH));
        assert_eq!(p.spill_dir, PathBuf::from("data/spill"));
    }

    // -- supervisor keeps running --

    #[tokio::test]
    async fn test_spawn_supervised_resource_monitor_keeps_running() {
        let handle = spawn_supervised_resource_monitor(ResourceMonitorPaths::platform_defaults(
            PathBuf::from("data/resource-monitor-test"),
        ));
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "supervisor must keep running, not exit after spawning the monitor"
        );
        handle.abort();
    }
}
