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
         the confirm-on-channel residual, F3; same-day 15:40 IST \
         tick-conservation audit), never an ad-hoc literal"
    );
    assert!(
        src.contains("WS_WAL_ARCHIVE_PRUNE_INTERVAL_SECS"),
        "the prune task must loop on the pinned 6h cadence constant"
    );
    // The prune must read the SAME WAL dir the writer + conservation audit
    // use (the single-source-of-truth helper), so the two can never drift.
    let prune_idx = src
        .find("ws_frame_spill::prune_archived_segments(")
        .expect("prune call present");
    let window = &src[prune_idx.saturating_sub(600)..prune_idx];
    assert!(
        window.contains("tick_conservation_boot::ws_wal_dir()"),
        "the prune task must resolve the WAL dir via \
         tick_conservation_boot::ws_wal_dir() (the shared single source of \
         truth), not a hardcoded path"
    );
}

#[test]
fn test_capture_rotation_precedes_bridge_and_sidecar_spawns() {
    // Review round 1 F1 (HIGH): the bridge's one-shot archive-drain decision
    // checks archive existence ONCE at its boot resume, while a sidecar-side
    // at-open rotation renames the live file ~1s later — a boot race that
    // silently orphans yesterday's un-flushed tail. The fix: ONLY Rust
    // rotates at open, synchronously, BEFORE both the bridge task and the
    // sidecar supervisor spawn. main.rs has a single shared spawn site for
    // both (process-global boot prefix, every boot arm), so pinning source
    // order here pins the runtime order by construction.
    let src = read_main_rs();
    let rotate_idx = src.find("rotate_stale_groww_capture_at_open(").expect(
        "main.rs must call groww_bridge::rotate_stale_groww_capture_at_open \
         (the Rust-owned boot rotation — F1/F2)",
    );
    for spawn in [
        // Both bridge arms (scale shard bridges + single-conn bridge)…
        "spawn_supervised_groww_shard_bridges(",
        "spawn_supervised_groww_bridge(",
        // …and both sidecar arms (fleet + single-conn supervisor).
        "spawn_groww_scale_fleet(",
        "spawn_supervised_groww_sidecar_supervisor(",
    ] {
        let spawn_idx = src
            .find(spawn)
            .unwrap_or_else(|| panic!("main.rs must contain the {spawn} spawn site"));
        assert!(
            rotate_idx < spawn_idx,
            "rotate_stale_groww_capture_at_open must be called BEFORE {spawn} \
             in main.rs source order — the rename must complete before the \
             bridge's one-shot drain decision and before the sidecar process \
             can re-open the old inode (F1)"
        );
    }
    // Single-owner invariant: the PYTHON sidecar must never rotate at open
    // (that reintroduces the race). The sidecar-side scan lives in
    // crates/common/tests/groww_capture_archive_guard.rs; here we pin that
    // main.rs carries exactly one rotation call site — the shared pre-spawn
    // boot prefix.
    assert_eq!(
        src.matches("rotate_stale_groww_capture_at_open(").count(),
        1,
        "exactly one rotation call site — the shared pre-spawn boot prefix"
    );
}

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
