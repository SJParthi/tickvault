//! Source-scan ratchet (AUTH-P12 / GAP-NET-01): the RUNTIME IP monitor
//! (`spawn_ip_monitor`) must stay wired into `main.rs` boot, and its
//! halt-vs-alert decision must stay gated on `dry_run`.
//!
//! Background: the permutation-coverage audit (2026-07-01, gap AUTH-P12) found
//! that `crates/core/src/network/ip_monitor.rs::spawn_ip_monitor` was defined
//! and fully unit-tested but had ZERO production call site — only the one-shot
//! boot-time `verify_public_ip` / `verify_static_ip_at_boot` were wired, so a
//! mid-session public-IP / Elastic-IP change was never re-detected. This PR
//! wires the runtime monitor. These guards fail the build if the wiring
//! regresses to dead code (audit-findings Rule 13: a fn defined + tested but
//! never called is a bug) or if the dry_run-gated halt decision is dropped.

use std::fs;
use std::path::PathBuf;

/// Read a file relative to the `tickvault-app` crate manifest dir.
fn read_app(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_spawn_ip_monitor_has_production_call_site() {
    let main_src = read_app("src/main.rs");
    assert!(
        main_src.contains("ip_monitor::spawn_ip_monitor("),
        "AUTH-P12 regression: `main.rs` no longer spawns the runtime IP \
         monitor. Without a production call site `spawn_ip_monitor` is dead \
         code and a mid-session Elastic-IP / public-IP change is never \
         re-detected — the exact gap AUTH-P12 was created to close."
    );
}

#[test]
fn test_ip_monitor_uses_for_runtime_config() {
    let main_src = read_app("src/main.rs");
    assert!(
        main_src.contains("IpMonitorConfig::for_runtime("),
        "AUTH-P12 regression: the runtime IP monitor must be built via \
         `IpMonitorConfig::for_runtime(verified_ip, dry_run)` so the halt \
         gate is derived from `dry_run`."
    );
}

#[test]
fn test_ip_monitor_halt_is_dry_run_gated() {
    let main_src = read_app("src/main.rs");
    // The wiring must feed `config.strategy.dry_run` into the monitor config
    // so the halt-vs-alert decision is gated on it (halt only when NOT dry_run).
    assert!(
        main_src.contains("for_runtime(") && main_src.contains("config.strategy.dry_run"),
        "AUTH-P12 regression: the runtime IP monitor's halt gate is no longer \
         derived from `config.strategy.dry_run`. It MUST halt only when \
         real orders are live (dry_run == false) and alert-only under dry_run."
    );
}
