//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the daily
//! 15:47 IST Dhan↔Groww spot cross-broker comparator (operator 2026-07-17;
//! SPOT-XVERIFY-01/02) is wired into main.rs and that the boot module is a
//! real implementation, not a stub. Mirrors `tf_consistency_wiring_guard.rs`
//! — reads SOURCE text, so it runs on the default build independent of any
//! feature flag.

use std::fs;
use std::path::PathBuf;

fn app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_spawn_spot_crossverify_tasks_is_wired_into_main() {
    let src = app_src("src/main.rs");
    // The fn is DEFINED in spot_crossverify_boot.rs, so every main.rs
    // mention is a call site. Exactly one on the process-global prefix.
    let call_count = src.matches("spawn_spot_crossverify_tasks(").count();
    assert_eq!(
        call_count, 1,
        "main.rs must call spawn_spot_crossverify_tasks(...) EXACTLY once \
         (the process-global boot prefix); found {call_count} — zero kills \
         the 15:47 comparator."
    );
}

#[test]
fn test_spawn_spot_crossverify_tasks_threads_config_calendar_notifier() {
    let src = app_src("src/main.rs");
    let Some(rel) = src.find("spawn_spot_crossverify_tasks(") else {
        panic!("spawn_spot_crossverify_tasks( not found in main.rs");
    };
    let window = &src[rel..(rel + 400).min(src.len())];
    for needle in ["&config", "&trading_calendar", "&notifier"] {
        assert!(
            window.contains(needle),
            "spawn_spot_crossverify_tasks call must pass {needle} within its \
             argument window."
        );
    }
}

#[test]
fn test_boot_module_is_a_real_impl_not_a_stub() {
    let src = app_src("src/spot_crossverify_boot.rs");
    for needle in [
        "pub fn spawn_spot_crossverify_tasks(",
        "pub async fn run_spot_crossverify(",
        "pub fn compare_day(",
        "ensure_spot_crossverify_tables",
        "SPOT_XVERIFY_TRIGGER_SECS_OF_DAY_IST",
    ] {
        assert!(
            src.contains(needle),
            "spot_crossverify_boot.rs must contain {needle} (real impl, not a stub)."
        );
    }
    // 15:47 IST slot.
    assert!(
        src.contains("15 * 3600 + 47 * 60"),
        "the trigger must be the 15:47 IST slot."
    );
}
