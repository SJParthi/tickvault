//! Ratchets for the 2026-08-07 audit fixes.
//!
//! Every fix in this file's scope was a signal that did not mean what it said:
//! two false-OKs (checks that always passed), one false-RED (a check that
//! always failed), and one set of documentation claims the code contradicted.
//! These tests fail the build if any of them regresses.
//!
//! See `.claude/plans/archive/2026-08-07-audit-fixes.md` (or the active plan
//! while in flight) for the full evidence table.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/common
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common must have two ancestors")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// Fix 2 — the three ratchet baselines must stay tracked in git
// ---------------------------------------------------------------------------

const TRACKED_BASELINES: [&str; 3] = [
    ".claude/hooks/.test-count-baseline",
    ".claude/hooks/.untested-pubfn-baseline",
    ".claude/hooks/.financial-test-baseline",
];

#[test]
fn ratchet_baselines_exist_on_disk() {
    for rel in TRACKED_BASELINES {
        let p = repo_root().join(rel);
        assert!(
            p.is_file(),
            "{rel} must exist — a ratchet with no baseline compares nothing and \
             passes vacuously (the exact false-OK this fix closed)"
        );
        let body = std::fs::read_to_string(&p).expect("baseline readable");
        let trimmed = body.trim();
        assert!(
            !trimmed.is_empty() && trimmed.parse::<u64>().is_ok(),
            "{rel} must hold a single integer, got {trimmed:?}"
        );
    }
}

#[test]
fn ratchet_baselines_are_not_gitignored() {
    let gitignore = read(".gitignore");
    for rel in TRACKED_BASELINES {
        // Match the path as a standalone ignore line, not as a substring of a
        // comment (the .gitignore now *documents* these paths on purpose).
        let ignored = gitignore
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .any(|l| l == rel || l == format!("/{rel}"));
        assert!(
            !ignored,
            "{rel} is gitignored again — CI would then see no baseline. \
             merge-gate-lock-2026-07-04.md §3 row 6 recorded this exact false-OK; \
             do not re-introduce it"
        );
    }
}

#[test]
fn ratchet_guards_fail_closed_when_baseline_missing_in_ci() {
    for (script, needle) in [
        (
            ".claude/hooks/test-count-guard.sh",
            "test-count baseline missing in CI",
        ),
        (
            ".claude/hooks/pub-fn-test-guard.sh",
            "untested-pubfn baseline missing in CI",
        ),
        (
            ".claude/hooks/financial-test-guard.sh",
            "financial-test baseline missing in CI",
        ),
    ] {
        let body = read(script);
        assert!(
            body.contains(needle),
            "{script} lost its CI fail-closed branch (expected {needle:?}) — \
             without it a missing baseline silently auto-establishes and exits 0"
        );
        assert!(
            body.contains(r#"if [ -n "${CI:-}" ]; then"#),
            "{script} must gate the fail-closed branch on the CI env var"
        );
    }
}

// ---------------------------------------------------------------------------
// Fix 1 — bench-gate hardware-drift detection
// ---------------------------------------------------------------------------

#[test]
fn bench_gate_carries_hardware_drift_detection() {
    let body = read("scripts/bench-gate.sh");
    for needle in [
        "HARDWARE_DRIFT_MIN_COUNT=5",
        "HARDWARE_DRIFT_MIN_SHARE=0.70",
        "HARDWARE_DRIFT_SPREAD_PCT=15",
        "HARDWARE DRIFT DETECTED",
        // 2026-08-07 (second calibration): the spread estimator MUST stay
        // robust. It shipped as max-min and then failed to suppress the very
        // next rotation — one outlier sub-30ns bench (+111.6%) dragged max-min
        // to 88.0 pts vs the 15 pt bar while the other 30 sat in +23.6..+42.9%
        // (IQR 2.6 pts). Live: main @026c7b63, bench run 31182757999 attempt 2.
        "INTERQUARTILE RANGE",
    ] {
        assert!(
            body.contains(needle),
            "scripts/bench-gate.sh lost {needle:?} — the relative regression arm \
             would go back to failing on every shared-runner rotation (31/31 \
             benches 'regressed' +24..30% on main @993a144c while every absolute \
             budget passed)"
        );
    }
    // The estimator must never revert to the worst-case statistic: a single
    // noisy tail bench must not be able to veto a textbook drift shape.
    assert!(
        !body.contains("spread = hi - lo"),
        "bench-gate reverted to max-min spread — one outlier benchmark can then \
         single-handedly veto drift suppression, which is exactly the false RED \
         observed live on main @026c7b63 (IQR 2.6 pts vs max-min 88.0 pts)"
    );
    // The absolute arm must NEVER be suppressed by drift classification.
    assert!(
        body.contains("NOT suppressed: the absolute ns budgets"),
        "bench-gate must state explicitly that absolute budgets still gate during drift"
    );
}

#[test]
fn bench_gate_selftest_exists_and_covers_both_directions() {
    let body = read("scripts/bench-gate.selftest.sh");
    for needle in [
        "was false RED",
        "still FAILS (guarantee intact)",
        "still FAILS (not uniform)",
        "still FAILS (fail-safe)",
        "FAILS on absolute (not suppressed)",
        // The 2026-08-07 second incident: a real drift shape carrying ONE
        // outlier. Proven to FAIL against the pre-IQR gate and PASS after.
        "IQR ignores tails",
    ] {
        assert!(
            body.contains(needle),
            "bench-gate.selftest.sh lost the {needle:?} case — the drift detector \
             must be proven to SUPPRESS the real drift shape AND to still FAIL a \
             genuine regression, or it is just a disabled gate"
        );
    }
}

// ---------------------------------------------------------------------------
// Fix 3 — real-SSM tests are opt-in, never gated on ambient credentials
// ---------------------------------------------------------------------------

#[test]
fn real_ssm_tests_are_opt_in_not_credential_sniffed() {
    let support = read("crates/core/src/test_support.rs");
    assert!(
        support.contains("pub fn real_ssm_tests_enabled() -> bool"),
        "crates/core/src/test_support.rs must expose real_ssm_tests_enabled()"
    );
    assert!(
        support.contains("TICKVAULT_TEST_REAL_SSM"),
        "real_ssm_tests_enabled must require the explicit opt-in env var"
    );

    // The six previously-fragile call sites must not sniff credentials again.
    for rel in [
        "crates/core/src/auth/secret_manager.rs",
        "crates/core/src/network/ip_verifier.rs",
        "crates/core/src/notification/service.rs",
    ] {
        let body = read(rel);
        assert!(
            !body.contains("has_aws_credentials()"),
            "{rel} branches on has_aws_credentials() again. That asks \"are \
             credentials present?\" when the assertion needs \"is SSM \
             reachable?\" — any box with a stray AWS_ACCESS_KEY_ID and no SSM \
             route then fails. Use real_ssm_tests_enabled()"
        );
    }
}

// ---------------------------------------------------------------------------
// Fix 4 — the corrected documentation claims
// ---------------------------------------------------------------------------

#[test]
fn claude_md_no_longer_claims_registry_is_papaya() {
    let body = read("CLAUDE.md");
    assert!(
        !body.contains("`InstrumentRegistry` (papaya concurrent map)"),
        "CLAUDE.md claims InstrumentRegistry is a papaya concurrent map. It is a \
         plain HashMap keyed on (SecurityId, ExchangeSegment) — see \
         crates/common/src/instrument_registry.rs `by_composite`"
    );
}

#[test]
fn claude_md_qualifies_the_blanket_o1_principle() {
    let body = read("CLAUDE.md");
    assert!(
        body.contains("Honest scope of principle 2"),
        "CLAUDE.md must keep the honest scope note on principle 2. O(1) is proven \
         on the tick hot path but NOT universal — spot_bar_store.rs reads are \
         O(log n) and its scans are O(n). Per the operator's standing rule a \
         non-O(1) step is flagged, never relabelled"
    );
    assert!(
        body.contains("spot_bar_store.rs"),
        "the principle-2 note must name the known exception explicitly"
    );
}
