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
/// §3b BODY needles (verifier nuance-a, 2026-07-15): the heading needle
/// alone let a body-hollowing (heading kept, contract rows gutted) slip —
/// and the `dhan_data_api_limiter` / `global_dhan_gates` whole-file
/// checks match OTHER sections of the rule file too. These sentences are
/// VERBATIM from the §3b contract body and appear nowhere else in it.
const CONTRACT_BODY_NEEDLES: [&str; 3] = [
    "Route every Dhan call through the shared `dhan_data_api_limiter`",
    "Record/consult the PROCESS-GLOBAL gate registry",
    "A queue delay is SELF-INFLICTED pacing",
];

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
    // Nuance-a (2026-07-15): body-hollowing defense — the §3b contract
    // BODY must keep its three load-bearing sentences verbatim (the
    // heading + whole-file substring checks above are satisfiable with
    // the section body gutted).
    for needle in CONTRACT_BODY_NEEDLES {
        assert!(
            text.contains(needle),
            "F1(iv) ratchet (nuance-a): the §3b composition-contract BODY \
             sentence '{needle}' is gone from cadence-error-codes.md — a \
             hollowed section un-binds future executors from the shared \
             budget. Restore the contract body (or update this guard WITH \
             a fresh dated operator note)."
        );
    }
}

/// The runner source, pinned at compile time (verifier L2 wiring pin).
const RUNNER_RS: &str = include_str!("../src/cadence/runner.rs");

#[test]
fn test_cadence_expiry_resolution_wiring_consults_dhan_gate() {
    // Verifier L2 wiring pin (2026-07-15, flipped POSITIVE from the
    // hostile source-scan): `resolve_broker_expiries` MUST consult the
    // shared Dhan gate budget before an expiry-list fire — pre-L2 it
    // fired UNGATED Dhan REST (worst case 8 Dhan requests in one rolling
    // second when a retry wave collided with a cycle burst).
    let start = RUNNER_RS
        .find("async fn resolve_broker_expiries")
        .expect("L2 wiring pin: resolve_broker_expiries fn is gone from runner.rs");
    // Bound the scan at the next top-level section separator after the
    // fn (the fn body ends before the following `// ----` banner).
    let body = &RUNNER_RS[start..];
    let end = body
        .find("\n// ---------------------------------------------------------------------------")
        .unwrap_or(body.len());
    let body = &body[..end];
    assert!(
        body.contains("try_acquire_expiry"),
        "L2 wiring pin: resolve_broker_expiries no longer consults \
         DhanGates::try_acquire_expiry — Dhan expiry-list fires would be \
         UNGATED Dhan REST again (verifier L2)"
    );
    assert!(
        body.contains("tv_cadence_expiry_rate_limited_total"),
        "L2 wiring pin: the expiry-leg 429 counter \
         tv_cadence_expiry_rate_limited_total was removed from \
         resolve_broker_expiries — expiry 429s go silent again"
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
    // Nuance-a body needles discriminate too (each present exactly once
    // in the rule file — a hollowed §3b cannot be satisfied by another
    // section's text), and the L2 runner needles discriminate.
    let rule = rule_file_text();
    for needle in CONTRACT_BODY_NEEDLES {
        assert_eq!(
            rule.matches(needle).count(),
            1,
            "nuance-a self-test: body needle '{needle}' must appear exactly \
             once in the rule file"
        );
        let hollowed = rule.replace(needle, "BODY-GUTTED");
        assert!(!hollowed.contains(needle));
    }
    let mutated_runner = RUNNER_RS.replace("try_acquire_expiry", "ungated_fire");
    assert!(!mutated_runner.contains("try_acquire_expiry"));
}
