//! S3-7: Coverage threshold lockdown — RATCHET FLOORS edition.
//!
//! Reads `quality/crate-coverage-thresholds.toml` and enforces that:
//!
//! 1. The file exists and is non-empty
//! 2. The `default` threshold is at or above the PINNED floor below
//! 3. Every declared crate is at or above its PINNED floor below
//! 4. All seven workspace crates are listed (aws-lambdas joined
//!    2026-07-18, rust-only phase 2b-1)
//!
//! History: this test originally demanded exactly `100.0` everywhere.
//! PR #1079 (P0 of the coverage-gaps plan, operator-approved 2026-06-10)
//! replaced the fictional, never-enforced 100.0 values with REAL measured
//! ratchet floors and made `scripts/coverage-gate.sh` fail-closed — but
//! missed updating this lockdown, leaving `Test (common)` latently red
//! (first exposed by PR #1083, the next change to touch crates/common).
//! This edition keeps the lockdown's actual purpose intact: LOWERING any
//! floor still requires editing the pinned table below in the same PR,
//! which forces a visible review. RAISING a floor in the TOML is always
//! allowed (ratchet rule: floors only move up; re-pin here in the same PR
//! per the policy header in the TOML).
//!
//! 100% stays the documented TARGET; the floors are the enforced minimum.

use std::path::Path;

/// Pinned ratchet floors — must match or under-cut
/// `quality/crate-coverage-thresholds.toml` (P0 baseline, main @ a6fe35c4:
/// common 99.57 | api 98.71 | trading 97.02 | storage 91.35 | core 90.38 |
/// app 63.43, each floored ~0.1 below measured). Lowering a TOML value
/// below its pin fails this test; raising it is always allowed.
///
/// storage pin 91.2 -> 90.1 (2026-07-17, stage-2 dead-WS sweep, PR #1631):
/// TRUTHFUL RE-BASELINE, not a ratchet bypass — the sweep deleted ~12K LoC
/// of heavily-unit-tested DEAD tick-chain code from crates/storage, which
/// mechanically lowered the crate's covered/total ratio to a measured
/// 90.26% (CI run 29590767611). Every deleted storage test exercised ONLY
/// the deleted modules (verified via origin/main imports), so no surviving
/// code lost coverage. New floor per the standing formula: 90.2 - 0.1 =
/// 90.1. This edit is exactly the visible-review act this test exists to
/// force; the up-only ratchet resumes from 90.1.
/// aws-lambdas pin added 2026-07-18 (rust-only phase 2b-1): the NEW
/// crates/aws-lambdas (4 Lambda bins + shared lib). MEASURED-then-ratcheted:
/// local `cargo llvm-cov -p tickvault-aws-lambdas` measured 64.92% lines;
/// floor per the standing formula = 64.9 - 0.1 = 64.8 (the misses are the
/// thin [[bin]] glue + SDK client construction + live-AWS async glue —
/// structurally unexercisable without a live AWS account).
///
/// tickvault-logs-mcp pin added 2026-07-18 (rust-only phase 2c): initial
/// CONSERVATIVE floor 80.0 from a dev-sandbox measurement of 85.83%
/// (cargo llvm-cov, all targets incl. the parity harness subprocess
/// coverage); ratchet to the standing measured-minus-0.1 formula after
/// the first CI Coverage & Perf measurement.
const PINNED_DEFAULT_FLOOR: f64 = 63.0;
const PINNED_CRATE_FLOORS: &[(&str, f64)] = &[
    ("common", 99.5),
    ("core", 90.2),
    ("trading", 96.9),
    ("storage", 90.1),
    ("api", 98.6),
    ("app", 63.3),
    ("aws-lambdas", 64.8),
    ("tickvault-logs-mcp", 80.0),
];

fn read_thresholds_toml() -> String {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common"); // APPROVED: test
    let path = workspace_root
        .join("quality")
        .join("crate-coverage-thresholds.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "S3-7: quality/crate-coverage-thresholds.toml not found at {} — \
             did the file get deleted? Error: {e}",
            path.display()
        )
    })
}

/// Strips whitespace and comments to produce a line we can match on.
fn canonical_lines(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|l| match l.find('#') {
            Some(idx) => l[..idx].to_string(),
            None => l.to_string(),
        })
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Parse `<name> = <value>` returning the f64 value, or None.
fn parse_threshold_line(line: &str) -> Option<(String, f64)> {
    let (name, value) = line.split_once('=')?;
    let value: f64 = value.trim().parse().ok()?;
    Some((name.trim().to_string(), value))
}

#[test]
fn coverage_lockdown_file_exists() {
    let content = read_thresholds_toml();
    assert!(
        !content.is_empty(),
        "S3-7: coverage threshold file is empty"
    );
}

#[test]
fn coverage_lockdown_default_at_or_above_pinned_floor() {
    let content = read_thresholds_toml();
    let lines = canonical_lines(&content);
    let default_line = lines
        .iter()
        .find(|l| l.starts_with("default"))
        .expect("S3-7: coverage threshold file must declare `default = ...`"); // APPROVED: test
    let (_, value) =
        parse_threshold_line(default_line).expect("S3-7: `default = <number>` must parse"); // APPROVED: test
    assert!(
        value >= PINNED_DEFAULT_FLOOR,
        "S3-7: default coverage threshold {value} is BELOW the pinned floor \
         {PINNED_DEFAULT_FLOOR}. Floors only move UP (ratchet rule, PR #1079 \
         P0 policy). Lowering requires editing PINNED_DEFAULT_FLOOR in this \
         test in the same PR — a visible, reviewable act."
    );
}

#[test]
fn coverage_lockdown_every_crate_at_or_above_pinned_floor() {
    let content = read_thresholds_toml();
    let lines = canonical_lines(&content);
    let mut in_crates_section = false;
    let mut violations: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for line in &lines {
        if line == "[crates]" {
            in_crates_section = true;
            continue;
        }
        if line.starts_with('[') {
            in_crates_section = false;
            continue;
        }
        if !in_crates_section {
            continue;
        }
        let Some((name, value)) = parse_threshold_line(line) else {
            violations.push(format!("unparseable [crates] line: {line}"));
            continue;
        };
        seen.push(name.clone());
        match PINNED_CRATE_FLOORS.iter().find(|(n, _)| *n == name) {
            Some((_, floor)) => {
                if value < *floor {
                    violations.push(format!(
                        "crate {name} is set to {value} — BELOW pinned floor \
                         {floor}. Floors only move UP; lowering requires \
                         editing PINNED_CRATE_FLOORS in this test (visible \
                         review, same PR)."
                    ));
                }
            }
            None => violations.push(format!(
                "crate {name} has no pinned floor in PINNED_CRATE_FLOORS — \
                 add the pin in this test in the same PR."
            )),
        }
    }
    for (name, _) in PINNED_CRATE_FLOORS {
        if !seen.iter().any(|s| s == name) {
            violations.push(format!(
                "pinned crate {name} missing from the [crates] section"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "S3-7: {} coverage threshold violation(s):\n{}",
        violations.len(),
        violations.join("\n")
    );
}

#[test]
fn coverage_lockdown_required_crates_are_listed() {
    let content = read_thresholds_toml();
    let required = [
        "common",
        "core",
        "trading",
        "storage",
        "api",
        "app",
        "aws-lambdas",
        // 2026-07-18 (rust-only phase 2c): the Rust MCP crate.
        "tickvault-logs-mcp",
    ];
    let mut missing = Vec::new();
    for crate_name in required {
        // Expect a line like `common  = 99.5` inside [crates].
        let looking_for = format!("{crate_name} ");
        if !content.contains(&looking_for) && !content.contains(&format!("{crate_name}=")) {
            missing.push(crate_name);
        }
    }
    assert!(
        missing.is_empty(),
        "S3-7: required crate(s) not listed in coverage thresholds: {missing:?}"
    );
}

#[test]
fn coverage_lockdown_pinned_floors_are_sane() {
    // The pins themselves must stay in (0, 100] and cover all 8 crates —
    // guards against a typo'd pin (e.g. 9.5 instead of 99.5) silently
    // weakening the lockdown. (6 -> 8 on 2026-07-18: aws-lambdas, rust-only
    // phase 2b-1 + tickvault-logs-mcp, rust-only phase 2c.)
    assert_eq!(
        PINNED_CRATE_FLOORS.len(),
        8,
        "S3-7: expected exactly 8 pinned crate floors"
    );
    assert!(PINNED_DEFAULT_FLOOR > 0.0 && PINNED_DEFAULT_FLOOR <= 100.0);
    for (name, floor) in PINNED_CRATE_FLOORS {
        assert!(
            *floor > 0.0 && *floor <= 100.0,
            "S3-7: pinned floor for {name} out of range: {floor}"
        );
        assert!(
            *floor >= PINNED_DEFAULT_FLOOR,
            "S3-7: pinned floor for {name} ({floor}) below the default floor \
             ({PINNED_DEFAULT_FLOOR}) — pins must not under-cut the default"
        );
    }
}
