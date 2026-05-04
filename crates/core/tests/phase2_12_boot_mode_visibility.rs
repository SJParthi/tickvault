//! Phase 2.12 — hostile bug-hunt L1 fix: fast/slow boot mode
//! visibility via Prometheus gauge.
//!
//! What was missing: fast boot passes `None` for `tick_enricher` (no
//! lifecycle column population — recovery path). Slow boot passes
//! `Some(enricher)` (full lifecycle population). If an operator
//! switches between modes via process restart, the change was only
//! visible in the boot log lines — not in Prometheus history,
//! not in dashboards.
//!
//! The fix: emit `tv_lifecycle_enricher_attached{boot_mode="fast"|"slow"}`
//! gauge at boot:
//!   - fast boot: value 0.0 (lifecycle columns NOT populated)
//!   - slow boot: value 1.0 (lifecycle columns populated)
//!
//! Operators can:
//!   - Chart `tv_lifecycle_enricher_attached` over time to see mode
//!     transitions
//!   - Alert on transitions: a fast boot during business hours is a
//!     warning signal (recovery mode active)
//!   - Correlate with QuestDB column population — value=0 + 0 lifecycle
//!     rows is the expected fast-boot contract; value=1 + 0 lifecycle
//!     rows is a real bug
//!
//! Plus structured INFO log lines at both sites for human readability.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let me = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    me.parent()
        .expect("tickvault root")
        .parent()
        .expect("workspace root")
        .to_path_buf()
        .join("crates")
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|err| panic!("read {p:?}: {err}"))
}

#[test]
fn phase2_12_main_rs_emits_lifecycle_enricher_attached_gauge() {
    let src = read("app/src/main.rs");
    assert!(
        src.contains("tv_lifecycle_enricher_attached"),
        "main.rs must emit tv_lifecycle_enricher_attached gauge at both boot sites"
    );
}

#[test]
fn phase2_12_fast_boot_sets_gauge_to_zero() {
    let src = read("app/src/main.rs");
    assert!(
        src.contains("\"boot_mode\" => \"fast\"") && src.contains(".set(0.0)"),
        "fast boot must set gauge to 0.0 with boot_mode=fast label"
    );
}

#[test]
fn phase2_12_slow_boot_sets_gauge_to_one() {
    let src = read("app/src/main.rs");
    assert!(
        src.contains("\"boot_mode\" => \"slow\"") && src.contains(".set(1.0)"),
        "slow boot must set gauge to 1.0 with boot_mode=slow label"
    );
}

#[test]
fn phase2_12_fast_boot_logs_enricher_attached_false() {
    let src = read("app/src/main.rs");
    assert!(
        src.contains("boot_mode = \"fast\"") && src.contains("enricher_attached = false"),
        "fast boot must log structured boot_mode=fast + enricher_attached=false"
    );
}

#[test]
fn phase2_12_slow_boot_logs_enricher_attached_true() {
    let src = read("app/src/main.rs");
    assert!(
        src.contains("boot_mode = \"slow\"") && src.contains("enricher_attached = true"),
        "slow boot must log structured boot_mode=slow + enricher_attached=true"
    );
}

#[test]
fn phase2_12_fast_boot_log_explains_lifecycle_columns_will_not_be_populated() {
    let src = read("app/src/main.rs");
    assert!(
        src.contains("lifecycle columns will NOT be")
            || src.contains("lifecycle columns will not be populated"),
        "fast boot log must clearly state that lifecycle columns will NOT be populated \
         (operator-visible context for the recovery path)"
    );
}

#[test]
fn phase2_12_slow_boot_log_lists_attached_subsystems() {
    let src = read("app/src/main.rs");
    // Slow boot's structured log line names the subsystems that get
    // wired in: TickEnricher, prev_oi_cache, BootOrderingGate,
    // midnight rollover, periodic refresh.
    assert!(
        src.contains("TickEnricher attached"),
        "slow boot log must mention TickEnricher attached"
    );
    assert!(
        src.contains("prev_oi_cache loaded"),
        "slow boot log must mention prev_oi_cache loaded"
    );
    assert!(
        src.contains("BootOrderingGate authorized"),
        "slow boot log must mention BootOrderingGate authorized"
    );
}
