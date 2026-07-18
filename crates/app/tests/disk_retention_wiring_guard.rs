//! Source-scan ratchet — disk-retention hardening wiring (2026-07-13,
//! prod disk-full incident: 30 GB root EBS at 82%, +2.5–3.5 GB/trading-day
//! with zero reclamation).
//!
//! Pins in `main.rs` that BOTH new retention mechanisms stay wired in the
//! process-global boot prefix (they run on every boot arm — deliberately
//! NOT inside the Dhan-lane periodic health loop, which never runs on a
//! Groww-only boot):
//!
//! 1. The WAL `archive/` prune task — confirmed-replay segments older than
//!    `WS_WAL_ARCHIVE_RETENTION_SECS` (7 days, F3) are deleted; before this,
//!    `data/ws_wal/archive/` grew ~0.15–0.6 GB/day unbounded (nothing
//!    anywhere pruned it).
//! 2. The `errors.log` size cap — the single-file WARN+ append log is
//!    skipped by every retention sweeper (the `*.log`-name guard), so it
//!    was the one unbounded log file; the hourly errors.jsonl sweeper task
//!    now also caps it at `ERRORS_LOG_MAX_BYTES`.
//!
//! (Mirror of the `ws_rate_limit_cooldown_wiring_guard.rs` house pattern.)

use std::fs;
use std::path::PathBuf;

fn read_main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_main_spawns_ws_wal_archive_prune() {
    let src = read_main_rs();
    assert!(
        src.contains("ws_frame_spill::prune_archived_segments("),
        "main.rs must call ws_frame_spill::prune_archived_segments — without \
         it, confirmed-replay WAL segments accumulate unbounded in \
         data/ws_wal/archive/ (~0.15-0.6 GB/day on the prod 30 GB volume)"
    );
    assert!(
        src.contains("WS_WAL_ARCHIVE_RETENTION_SECS"),
        "the prune must use the pinned WS_WAL_ARCHIVE_RETENTION_SECS \
         retention constant (7 days — audit-safe AND long-weekend-safe for \
         the confirm-on-channel residual, F3), never an ad-hoc literal"
    );
    assert!(
        src.contains("WS_WAL_ARCHIVE_PRUNE_INTERVAL_SECS"),
        "the prune task must loop on the pinned 6h cadence constant"
    );
    // The prune must read the SAME WAL dir the writer uses (the
    // single-source-of-truth helper — relocated 2026-07-18 from the retired
    // tick_conservation_boot module into boot_helpers with the
    // tick-conservation retirement), so the two can never drift.
    let prune_idx = src
        .find("ws_frame_spill::prune_archived_segments(")
        .expect("prune call present");
    let window = &src[prune_idx.saturating_sub(600)..prune_idx];
    assert!(
        window.contains("boot_helpers::ws_wal_dir()"),
        "the prune task must resolve the WAL dir via \
         boot_helpers::ws_wal_dir() (the shared single source of \
         truth), not a hardcoded path"
    );
}

// RETIRED 2026-07-15 (Groww live-feed deletion):
// test_capture_rotation_precedes_bridge_and_sidecar_spawns died with the
// sidecar/bridge/fleet spawns + rotate_stale_groww_capture_at_open (all in
// deleted groww_bridge.rs / groww_sidecar_supervisor.rs); no capture file is
// produced anymore. The WAL archive-prune + errors-log-cap pins survive.

#[test]
fn test_main_wires_errors_log_cap() {
    let src = read_main_rs();
    assert!(
        src.contains("observability::cap_errors_log_size("),
        "main.rs must call observability::cap_errors_log_size — without it, \
         data/logs/machine/errors.log (skipped by every sweeper via the \
         *.log-name guard) grows unbounded"
    );
    assert!(
        src.contains("observability::ERRORS_LOG_MAX_BYTES"),
        "the cap must use the pinned ERRORS_LOG_MAX_BYTES constant"
    );
    assert!(
        src.contains("boot_helpers::ERROR_LOG_FILE_PATH"),
        "the cap must target the canonical ERROR_LOG_FILE_PATH constant — \
         never a duplicated path literal"
    );
}
