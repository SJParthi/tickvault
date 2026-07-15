//! F1(iv) source-scan ratchet (2026-07-15): the cadence COMPOSITION
//! CONTRACT must stay pinned in BOTH its load-bearing places —
//!
//! 1. the dated "COMPOSITION CONTRACT (2026-07-15)" section in
//!    `.claude/rules/project/cadence-error-codes.md` (the rule text every
//!    future Dhan-firing executor session auto-loads), and
//! 2. the process-global gate handle `pub fn global_dhan_gates` in
//!    `crates/core/src/cadence/gate.rs` (the ONE shared budget the
//!    contract routes every composition through).
//!
//! Deleting/renaming either silently un-binds future executors from the
//! shared 429 budget — this guard fails the build instead. House pattern:
//! the non-vacuous self-test asserts the needles would NOT match a
//! mutated haystack, so the scan can never pass vacuously.

/// The gate module source, pinned at compile time (a moved/deleted file
/// fails the build at `include_str!`).
const GATE_RS: &str = include_str!("../src/cadence/gate.rs");

/// The rule-file path, relative to the core crate's manifest dir.
const RULE_FILE_REL: &str = "../../.claude/rules/project/cadence-error-codes.md";

const CONTRACT_HEADING_NEEDLE: &str = "COMPOSITION CONTRACT (2026-07-15)";
const GLOBAL_HANDLE_NEEDLE: &str = "pub fn global_dhan_gates";
const GLOBAL_INIT_NEEDLE: &str = "pub fn init_global_dhan_gates";

fn rule_file_text() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(RULE_FILE_REL);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "F1(iv) ratchet: cadence rule file missing/unreadable at {} — {e}. \
             The COMPOSITION CONTRACT section must live in \
             .claude/rules/project/cadence-error-codes.md",
            path.display()
        )
    })
}

#[test]
fn test_cadence_rule_file_pins_composition_contract_heading() {
    let text = rule_file_text();
    assert!(
        text.contains(CONTRACT_HEADING_NEEDLE),
        "F1(iv) ratchet: '{CONTRACT_HEADING_NEEDLE}' heading is gone from \
         cadence-error-codes.md — future Dhan-firing executors lose the \
         shared-budget composition contract. Restore the dated section \
         (or update this guard WITH a fresh dated operator note)."
    );
    // The contract must keep naming BOTH composition legs: the shared
    // Data-API limiter and the global gate handle.
    assert!(
        text.contains("dhan_data_api_limiter"),
        "F1(iv) ratchet: the composition contract no longer names the \
         shared dhan_data_api_limiter leg"
    );
    assert!(
        text.contains("global_dhan_gates"),
        "F1(iv) ratchet: the composition contract no longer names the \
         global_dhan_gates handle"
    );
}

#[test]
fn test_cadence_gate_module_keeps_global_handle_fns() {
    assert!(
        GATE_RS.contains(GLOBAL_HANDLE_NEEDLE),
        "F1(iv) ratchet: `{GLOBAL_HANDLE_NEEDLE}` was removed/renamed in \
         cadence/gate.rs — the process-global shared-budget handle is the \
         composition contract's anchor"
    );
    assert!(
        GATE_RS.contains(GLOBAL_INIT_NEEDLE),
        "F1(iv) ratchet: `{GLOBAL_INIT_NEEDLE}` was removed/renamed in \
         cadence/gate.rs — boot wiring can no longer size the shared \
         gate registry from config"
    );
}

#[test]
fn test_composition_contract_guard_needles_are_non_vacuous() {
    // Self-test (house convention): prove the needles discriminate — a
    // mutated haystack must NOT match, so the scan cannot pass vacuously.
    let mutated_rule = rule_file_text().replace(CONTRACT_HEADING_NEEDLE, "CONTRACT-DELETED");
    assert!(!mutated_rule.contains(CONTRACT_HEADING_NEEDLE));
    let mutated_gate = GATE_RS.replace("global_dhan_gates", "renamed_gates");
    assert!(!mutated_gate.contains(GLOBAL_HANDLE_NEEDLE));
    assert!(!mutated_gate.contains(GLOBAL_INIT_NEEDLE));
}
