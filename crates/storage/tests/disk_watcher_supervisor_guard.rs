//! Ratchet (zero-tick-loss PR-5, G3): the spill disk-health watcher MUST be
//! spawned under its supervisor in `main.rs`, NOT fire-and-forget.
//!
//! Before PR-5 the watcher handle was bound to `_disk_health_watcher_handle`
//! via the bare `spawn_spill_disk_health_watcher(...)` — a panic in the
//! watcher killed disk-free monitoring (the "disk full + QuestDB down" early
//! warning) with zero signal. PR-5 routes it through
//! `spawn_supervised_spill_disk_health_watcher`, which respawns + logs
//! `DISK-WATCHER-01` + increments `tv_disk_watcher_respawn_total` on every
//! watcher death.
//!
//! This guard fails the build if a future refactor reverts `main.rs` to the
//! bare (unsupervised) spawn at the boot call site.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/storage") // APPROVED: test
}

fn read_main_rs() -> String {
    let p = workspace_root().join("crates/app/src/main.rs");
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_main_uses_supervised_disk_health_watcher() {
    let main_rs = read_main_rs();
    assert!(
        main_rs.contains("spawn_supervised_spill_disk_health_watcher"),
        "G3 ratchet: main.rs must spawn the disk-health watcher via \
         `spawn_supervised_spill_disk_health_watcher` so a watcher panic \
         respawns + alerts (DISK-WATCHER-01) instead of vanishing silently."
    );
}

#[test]
fn test_main_does_not_use_bare_unsupervised_disk_watcher_spawn() {
    let main_rs = read_main_rs();
    // The bare module-qualified call. The supervised call is
    // `::spawn_supervised_spill_disk_health_watcher(` — its prefix after
    // `::spawn_` is `supervised_`, so the bare needle below is NOT a
    // substring of it.
    assert!(
        !main_rs.contains("::spawn_spill_disk_health_watcher("),
        "G3 ratchet: main.rs must NOT call the bare (unsupervised) \
         `spawn_spill_disk_health_watcher` at the boot call site — route it \
         through `spawn_supervised_spill_disk_health_watcher` instead so a \
         watcher panic is respawned + alerted."
    );
}
