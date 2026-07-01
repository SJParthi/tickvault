//! BP-07 — PROC-01 OOM-kill monitor.
//!
//! Closes the audit gap (`.claude/plans/permutation-coverage-audit-2026-07-01.md`
//! §146): an OOM kill has **no dedicated signal** today. `wave-4-error-codes.md`
//! named PROC-01 "Reserved" with a `Source (planned): crates/app/src/oom_monitor.rs`
//! that never existed, and there was no `Proc01` ErrorCode variant. An OOM was
//! therefore only caught indirectly (process dies → systemd → market-hours
//! liveness page on a missing SLO), so an OOM-loop was indistinguishable from a
//! panic-loop with zero OOM attribution.
//!
//! This module reads the cgroup-v2 `memory.events` file, which the kernel
//! updates with a per-cgroup `oom_kill` counter (monotonic within the cgroup's
//! lifetime). Every 60s the monitor re-reads the counter and compares it against
//! a boot-time baseline; a positive delta means one or more processes in this
//! cgroup (tickvault itself OR a sidecar) were killed by the kernel OOM killer.
//! On a positive delta it emits:
//!   * `error!(code = "PROC-01", …)` → Telegram Critical via the 5-sink pipeline
//!   * `tv_oom_kills_total` — counter incremented by the delta
//!
//! Probe failures (cgroup-v1 host, non-Linux dev box, missing file, parse error)
//! increment `tv_oom_monitor_probe_failed_total` and log a `warn!` — an honest
//! "no signal" (the disk_health_watcher precedent), never a spurious page and
//! never a panic. The monitor is observability-only: it NEVER blocks boot or the
//! hot path.
//!
//! The module mirrors [`crate::disk_health_watcher`] exactly: a fully-unit-tested
//! pure parser + decision, a thin `spawn_*` task, and a supervised wrapper that
//! respawns on death (mirrors WS-GAP-05 / DISK-WATCHER-01) so OOM monitoring can
//! never vanish silently.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{debug, error, info, warn};

use crate::disk_health_watcher::classify_join_exit;

/// Cadence of the OOM probe. 60s matches the disk-health watcher — an OOM kill
/// is a rare, already-happened event, so a minute of latency to the alert is
/// immaterial while CPU cost stays negligible.
pub const OOM_MONITOR_POLL_INTERVAL_SECS: u64 = 60;

/// Backoff between a supervised OOM-monitor death and its respawn. Small so
/// monitoring resumes quickly, non-zero so a task that panics instantly on every
/// start cannot busy-spin the CPU — it respawns at most once per this interval,
/// and the `tv_oom_monitor_respawn_total` counter rate surfaces the flap to the
/// operator via CloudWatch. Mirrors [`crate::disk_health_watcher::DISK_WATCHER_RESPAWN_BACKOFF_SECS`].
pub const OOM_MONITOR_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Canonical cgroup-v2 memory-events path. On a cgroup-v2 host the app's own
/// cgroup exposes `memory.events` here (systemd typically bind-mounts the unit's
/// cgroup at the fs root inside the container/unit view). If this path does not
/// exist (cgroup-v1, macOS dev box), the probe fails softly.
pub const DEFAULT_CGROUP_V2_MEMORY_EVENTS_PATH: &str = "/sys/fs/cgroup/memory.events";

/// Outcome of reading + parsing `memory.events`, exposed for unit testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OomProbeOutcome {
    /// The `oom_kill` counter was read successfully.
    Ok { oom_kill_count: u64 },
    /// The file could not be read or the `oom_kill` line was missing/malformed.
    /// The baseline is left untouched; the operator has no OOM signal from this
    /// tick, which is honest about the lack of a cgroup-v2 source.
    ProbeFailed { reason: &'static str },
}

/// Result of comparing the current `oom_kill` count against the boot baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OomDelta {
    /// No new kills since the baseline (`current <= baseline`). A `current`
    /// BELOW the baseline (cgroup re-created) also maps here via saturating
    /// subtraction — it can never underflow-panic.
    NoChange,
    /// One or more new OOM kills occurred: `current - baseline == count > 0`.
    NewKills { count: u64 },
}

/// PURE parser of a cgroup-v2 `memory.events` file body. The file is a set of
/// `key value` lines, e.g.:
///
/// ```text
/// low 0
/// high 0
/// max 0
/// oom 3
/// oom_kill 2
/// oom_group_kill 0
/// ```
///
/// Returns `Some(oom_kill_value)` when the `oom_kill` line is present and its
/// value parses as `u64`; `None` when the field is missing or malformed. The
/// `oom` field (allocation failures that did NOT kill) is deliberately ignored —
/// only `oom_kill` (actual kills) triggers PROC-01.
#[must_use]
pub fn parse_oom_kill_count(memory_events: &str) -> Option<u64> {
    for line in memory_events.lines() {
        let mut parts = line.split_whitespace();
        // Match the EXACT field name `oom_kill` — NOT `oom` and NOT
        // `oom_group_kill`, both of which also start with `oom`.
        if parts.next() == Some("oom_kill") {
            return parts.next().and_then(|v| v.parse::<u64>().ok());
        }
    }
    None
}

/// PURE decision: compare the current `oom_kill` count against the boot baseline.
///
/// Uses saturating subtraction so a counter reset (the cgroup was re-created and
/// now reports fewer kills than the baseline) yields [`OomDelta::NoChange`]
/// rather than underflowing — the next successful read naturally re-baselines
/// upward at the call site.
#[must_use]
pub fn classify_oom_delta(baseline: u64, current: u64) -> OomDelta {
    match current.saturating_sub(baseline) {
        0 => OomDelta::NoChange,
        count => OomDelta::NewKills { count },
    }
}

/// One-shot probe: read `path` and parse the `oom_kill` counter. Returns the
/// outcome enum so the caller decides what to do (the production task updates the
/// baseline + emits PROC-01; tests assert on the parsed value).
#[must_use]
// This wrapper is a read + delegate to the fully-tested parser: the ProbeFailed
// branch is covered by `test_probe_against_missing_path_returns_probe_failed`,
// the Ok branch by `test_probe_against_temp_fixture_file_parses` + the pure
// `parse_oom_kill_count` tests.
// TEST-EXEMPT: thin fs-read wrapper; both branches covered by the probe tests above.
pub fn probe_oom_kill_count(path: &Path) -> OomProbeOutcome {
    match std::fs::read_to_string(path) {
        Ok(body) => match parse_oom_kill_count(&body) {
            Some(oom_kill_count) => OomProbeOutcome::Ok { oom_kill_count },
            None => OomProbeOutcome::ProbeFailed {
                reason: "oom_kill_field_missing_or_malformed",
            },
        },
        Err(_) => OomProbeOutcome::ProbeFailed {
            reason: "memory_events_read_failed",
        },
    }
}

/// Spawn the background OOM monitor. Idempotent — call once at boot. The returned
/// `JoinHandle` can be aborted on shutdown.
///
/// On the first successful probe the current `oom_kill` count becomes the
/// baseline, so a pre-existing lifetime OOM count NEVER fires a spurious page.
/// Thereafter each positive delta emits PROC-01 + `tv_oom_kills_total` and the
/// baseline advances so the same kills are not re-reported.
///
/// On a non-cgroup-v2 host (missing file / cgroup-v1 / macOS) every probe fails
/// softly: `tv_oom_monitor_probe_failed_total` increments and the task logs
/// `warn!` — no page, no panic. This is honest about the lack of a signal, the
/// same posture as the disk-health watcher on a non-POSIX host.
// TEST-EXEMPT: tokio task spawn — exercised in production by `crates/app/src/main.rs`.
// The pure `parse_oom_kill_count` / `classify_oom_delta` above are fully unit-tested;
// this wrapper is a probe loop that needs an integration harness to test usefully.
#[must_use]
pub fn spawn_oom_monitor(memory_events_path: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let m_kills = metrics::counter!("tv_oom_kills_total");
        let m_probe_failed = metrics::counter!("tv_oom_monitor_probe_failed_total");

        info!(
            path = %memory_events_path.display(),
            interval_secs = OOM_MONITOR_POLL_INTERVAL_SECS,
            "OOM monitor started (PROC-01)"
        );

        // `None` until the first successful probe establishes the baseline.
        let mut baseline: Option<u64> = None;

        let mut ticker = tokio::time::interval(Duration::from_secs(OOM_MONITOR_POLL_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match probe_oom_kill_count(&memory_events_path) {
                OomProbeOutcome::Ok { oom_kill_count } => match baseline {
                    None => {
                        baseline = Some(oom_kill_count);
                        info!(
                            path = %memory_events_path.display(),
                            baseline = oom_kill_count,
                            "OOM monitor baseline established"
                        );
                    }
                    Some(prev) => match classify_oom_delta(prev, oom_kill_count) {
                        OomDelta::NoChange => {
                            debug!(
                                path = %memory_events_path.display(),
                                oom_kill_count,
                                "OOM monitor probe ok — no new kills"
                            );
                            // Re-baseline downward on a counter reset so a later
                            // real kill above the reset value is detected.
                            if oom_kill_count < prev {
                                baseline = Some(oom_kill_count);
                            }
                        }
                        OomDelta::NewKills { count } => {
                            error!(
                                code = tickvault_common::error_code::ErrorCode::Proc01OomKillDetected
                                    .code_str(),
                                path = %memory_events_path.display(),
                                new_kills = count,
                                total_kills = oom_kill_count,
                                baseline = prev,
                                "PROC-01: OOM kill detected — a process in this cgroup was \
                                 killed by the kernel OOM killer (host out of memory)"
                            );
                            m_kills.increment(count);
                            // Advance the baseline so the same kills are not re-reported.
                            baseline = Some(oom_kill_count);
                        }
                    },
                },
                OomProbeOutcome::ProbeFailed { reason } => {
                    m_probe_failed.increment(1);
                    warn!(
                        path = %memory_events_path.display(),
                        reason,
                        "OOM monitor probe failed — no OOM-kill signal from this cgroup \
                         (cgroup-v1 / non-Linux host?)"
                    );
                }
            }
        }
    })
}

/// Supervise the OOM monitor. [`spawn_oom_monitor`] runs an infinite probe loop,
/// so its `JoinHandle` resolves ONLY on a fatal event (panic or external cancel).
/// This supervisor mirrors the WS-GAP-05 pool supervisor and the DISK-WATCHER-01
/// disk-watcher supervisor: on every monitor death it logs, increments
/// `tv_oom_monitor_respawn_total{reason}`, then respawns after
/// [`OOM_MONITOR_RESPAWN_BACKOFF_SECS`] so OOM monitoring can never vanish
/// silently. The returned `JoinHandle` is itself an infinite loop; callers bind
/// it to a `_`-prefixed name. The supervisor body has no panic path of its own
/// (pure-function classification, no `unwrap`/`expect`).
// O(1) EXEMPT: cold-path supervisor — one task per session, fires only on monitor death.
#[must_use]
pub fn spawn_supervised_oom_monitor(memory_events_path: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = spawn_oom_monitor(memory_events_path.clone());
            let join_result = handle.await;
            let reason = classify_join_exit(&join_result);
            warn!(
                reason,
                backoff_secs = OOM_MONITOR_RESPAWN_BACKOFF_SECS,
                path = %memory_events_path.display(),
                "OOM monitor exited — respawning so OOM-kill (PROC-01) monitoring continues"
            );
            metrics::counter!("tv_oom_monitor_respawn_total", "reason" => reason).increment(1);
            tokio::time::sleep(Duration::from_secs(OOM_MONITOR_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic cgroup-v2 `memory.events` fixture (note `oom` and
    // `oom_group_kill` also start with `oom` — the parser must match the EXACT
    // `oom_kill` field, not a prefix).
    const FIXTURE: &str = "low 0\nhigh 0\nmax 4\noom 3\noom_kill 2\noom_group_kill 0\n";

    #[test]
    fn test_parse_oom_kill_count_happy_path() {
        assert_eq!(parse_oom_kill_count(FIXTURE), Some(2));
    }

    #[test]
    fn test_parse_oom_kill_count_zero() {
        // A healthy cgroup with no kills reports `oom_kill 0` — Some(0), NOT None.
        let body = "low 0\nhigh 0\noom 0\noom_kill 0\n";
        assert_eq!(parse_oom_kill_count(body), Some(0));
    }

    #[test]
    fn test_parse_oom_kill_count_missing_field() {
        // cgroup-v1-ish body with no `oom_kill` line at all.
        let body = "low 0\nhigh 0\nmax 0\n";
        assert_eq!(parse_oom_kill_count(body), None);
    }

    #[test]
    fn test_parse_oom_kill_count_malformed_value() {
        let body = "oom 1\noom_kill notanumber\n";
        assert_eq!(parse_oom_kill_count(body), None);
    }

    #[test]
    fn test_parse_oom_kill_count_does_not_match_oom_group_kill() {
        // `oom_group_kill` starts with `oom` and contains `kill` — the parser
        // must NOT confuse it for the `oom_kill` field.
        let body = "oom 5\noom_group_kill 9\n";
        assert_eq!(parse_oom_kill_count(body), None);
    }

    #[test]
    fn test_parse_oom_kill_count_tolerates_other_fields_and_whitespace() {
        // Extra fields, blank lines, leading/trailing whitespace, and the
        // target field last — the parser still finds it.
        let body = "\n  low   0  \nhigh 0\n\noom 7\noom_kill   42\n";
        assert_eq!(parse_oom_kill_count(body), Some(42));
    }

    #[test]
    fn test_parse_oom_kill_count_empty_body() {
        assert_eq!(parse_oom_kill_count(""), None);
    }

    #[test]
    fn test_classify_oom_delta_no_change() {
        assert_eq!(classify_oom_delta(5, 5), OomDelta::NoChange);
    }

    #[test]
    fn test_classify_oom_delta_positive_delta() {
        assert_eq!(classify_oom_delta(5, 8), OomDelta::NewKills { count: 3 });
    }

    #[test]
    fn test_classify_oom_delta_single_new_kill() {
        assert_eq!(classify_oom_delta(0, 1), OomDelta::NewKills { count: 1 });
    }

    #[test]
    fn test_classify_oom_delta_counter_reset_saturates() {
        // cgroup re-created: current BELOW baseline. Must NOT underflow-panic;
        // maps to NoChange so the caller can re-baseline downward.
        assert_eq!(classify_oom_delta(10, 3), OomDelta::NoChange);
    }

    #[test]
    fn test_oom_poll_interval_constant_reasonable() {
        // Too short = wasted CPU; too long = operator surprised by an OOM loop.
        assert!(OOM_MONITOR_POLL_INTERVAL_SECS >= 30);
        assert!(OOM_MONITOR_POLL_INTERVAL_SECS <= 300);
    }

    #[test]
    fn test_respawn_backoff_is_small_but_nonzero() {
        assert!(OOM_MONITOR_RESPAWN_BACKOFF_SECS >= 1);
        assert!(OOM_MONITOR_RESPAWN_BACKOFF_SECS <= 30);
    }

    #[test]
    fn test_default_cgroup_path_is_v2() {
        // The cgroup-v2 unified hierarchy exposes `memory.events` at the cgroup
        // root; pin the path so a future edit does not silently point at a
        // cgroup-v1 (`memory.oom_control`) path that this parser cannot read.
        assert!(DEFAULT_CGROUP_V2_MEMORY_EVENTS_PATH.ends_with("memory.events"));
        assert!(DEFAULT_CGROUP_V2_MEMORY_EVENTS_PATH.contains("/sys/fs/cgroup"));
    }

    #[test]
    fn test_probe_against_missing_path_returns_probe_failed() {
        // A path that cannot exist on any runner → ProbeFailed with a reason.
        let outcome = probe_oom_kill_count(std::path::Path::new(
            "/nonexistent/tickvault/oom-monitor/memory.events",
        ));
        match outcome {
            OomProbeOutcome::ProbeFailed { reason } => {
                assert!(!reason.is_empty(), "failure must carry a reason string");
            }
            OomProbeOutcome::Ok { .. } => panic!("missing path must not parse as Ok"), // APPROVED: test
        }
    }

    #[test]
    fn test_probe_against_temp_fixture_file_parses() {
        // Write a real fixture file and probe it end-to-end (exercises the Ok
        // branch of the wrapper without depending on the host cgroup).
        let dir = std::env::temp_dir().join("tickvault-oom-monitor-test");
        std::fs::create_dir_all(&dir).expect("mkdir temp"); // APPROVED: test
        let path = dir.join("memory.events");
        std::fs::write(&path, FIXTURE).expect("write fixture"); // APPROVED: test
        match probe_oom_kill_count(&path) {
            OomProbeOutcome::Ok { oom_kill_count } => assert_eq!(oom_kill_count, 2),
            OomProbeOutcome::ProbeFailed { reason } => {
                panic!("real fixture must parse Ok, got ProbeFailed: {reason}") // APPROVED: test
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_spawn_oom_monitor_returns_running_join_handle() {
        // Point at a missing path so the probe loop just fails softly forever;
        // the task must be running immediately after spawn.
        let handle = spawn_oom_monitor(std::path::PathBuf::from(
            "/nonexistent/tickvault/oom-monitor/memory.events",
        ));
        tokio::task::yield_now().await;
        assert!(!handle.is_finished(), "OOM monitor task must be running");
        handle.abort();
    }

    #[tokio::test]
    async fn test_spawn_supervised_oom_monitor_keeps_running() {
        // The supervisor is an infinite loop — its JoinHandle must NOT resolve
        // in normal operation (the inner monitor loops forever). If a future
        // edit makes the supervisor return after one death, this guard fails.
        let handle = spawn_supervised_oom_monitor(std::path::PathBuf::from(
            "/nonexistent/tickvault/oom-monitor/memory.events",
        ));
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "supervisor must keep running, not exit after spawning the monitor"
        );
        handle.abort();
    }
}
