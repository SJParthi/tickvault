//! Ratchet (BP-08, audit 2026-07-01): the RESOURCE-01/02/03 process-level
//! resource monitor MUST be spawned under its supervisor in `main.rs`, NOT
//! fire-and-forget — otherwise a monitor panic silently kills the fd / RSS /
//! spill-free early-warning signals with zero operator visibility.
//!
//! Mirrors `disk_watcher_supervisor_guard.rs` / the OOM-monitor wiring: reads
//! `main.rs` SOURCE text so it runs on the default build independent of any
//! feature flag. Fails the build if a future refactor removes the supervised
//! spawn from the boot call site.

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
fn test_main_uses_supervised_resource_monitor() {
    let main_rs = read_main_rs();
    assert!(
        main_rs.contains("spawn_supervised_resource_monitor"),
        "BP-08 ratchet: main.rs must spawn the resource monitor via \
         `spawn_supervised_resource_monitor` so a monitor panic respawns + \
         alerts (RESOURCE-01/02/03) instead of vanishing silently."
    );
}

#[test]
fn test_resource_monitor_uses_platform_default_paths() {
    // The production spawn must use the platform defaults bundle (real /proc +
    // cgroup + data/spill), not test fixtures.
    let main_rs = read_main_rs();
    assert!(
        main_rs.contains("ResourceMonitorPaths::platform_defaults"),
        "BP-08 ratchet: the production spawn must read the real /proc + cgroup \
         + data/spill sources via ResourceMonitorPaths::platform_defaults."
    );
}
