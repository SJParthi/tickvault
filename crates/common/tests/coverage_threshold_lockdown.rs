//! S3-7: Coverage threshold lockdown (ratchet-floor edition).
//!
//! Reads `quality/crate-coverage-thresholds.toml` and enforces that:
//!
//! 1. The file exists and is non-empty
//! 2. The `default` threshold is >= the pinned ratchet floor
//! 3. Every declared crate is >= its pinned ratchet floor
//! 4. All six workspace crates are explicitly listed
//!
//! POLICY (updated 2026-06-10, P0 of the coverage-gaps plan — PR #1079):
//! 100% remains the TARGET, but the enforced values are RATCHET FLOORS set
//! from the first real measurement (main @ a6fe35c4). The previous
//! "exactly 100.0" lockdown matched a gate that passed vacuously (it
//! matched zero files — see .claude/plans/research/coverage-gaps.md §2,
//! GAP #0). Honest enforced floors replace a fictional unenforced 100.
//!
//! RATCHET RULE: floors may only move UP, never down. When real coverage
//! rises, raise the floor in `quality/crate-coverage-thresholds.toml` AND
//! raise the pinned baseline below in the SAME commit — which forces a
//! visible review, exactly as the original 100.0 lockdown intended.
//! Lowering any floor below its pinned baseline fails this test.

use std::path::Path;

/// Pinned ratchet baselines (2026-06-10, main @ a6fe35c4 — PR #1079).
/// A floor in the TOML below its baseline here = build failure.
const BASELINE_DEFAULT: f64 = 63.0;
const BASELINE_CRATES: [(&str, f64); 6] = [
    ("common", 99.5),
    ("core", 90.2),
    ("trading", 96.9),
    ("storage", 91.2),
    ("api", 98.6),
    ("app", 63.3),
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

/// Parses `<name> = <value>` lines inside the `[crates]` section into
/// `(name, value)` pairs.
fn parse_crate_floors(lines: &[String]) -> Vec<(String, f64)> {
    let mut in_crates_section = false;
    let mut floors = Vec::new();
    for line in lines {
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
        if let Some((name, value)) = line.split_once('=') {
            let name = name.trim().to_string();
            let parsed: f64 = value.trim().parse().unwrap_or_else(|e| {
                panic!("S3-7: crate {name} has non-numeric threshold {value:?}: {e}")
                // APPROVED: test
            });
            floors.push((name, parsed));
        }
    }
    floors
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
fn coverage_lockdown_default_meets_ratchet_floor() {
    let content = read_thresholds_toml();
    let lines = canonical_lines(&content);
    let default_line = lines
        .iter()
        .find(|l| l.starts_with("default"))
        .expect("S3-7: coverage threshold file must declare `default = ...`"); // APPROVED: test
    let value: f64 = default_line
        .split_once('=')
        .map(|(_, v)| v.trim())
        .expect("S3-7: default line must be `default = <number>`") // APPROVED: test
        .parse()
        .expect("S3-7: default threshold must be numeric"); // APPROVED: test
    assert!(
        value >= BASELINE_DEFAULT,
        "S3-7: default coverage floor {value} is BELOW the pinned ratchet \
         baseline {BASELINE_DEFAULT}. Floors may only move UP (PR #1079 \
         ratchet rule). To raise the baseline, edit BASELINE_DEFAULT in this \
         test in the same commit."
    );
}

#[test]
fn coverage_lockdown_every_crate_meets_ratchet_floor() {
    let content = read_thresholds_toml();
    let lines = canonical_lines(&content);
    let floors = parse_crate_floors(&lines);
    let mut violations: Vec<String> = Vec::new();
    for (name, baseline) in BASELINE_CRATES {
        match floors.iter().find(|(n, _)| n == name) {
            Some((_, value)) => {
                if *value < baseline {
                    violations.push(format!(
                        "crate {name} floor {value} is BELOW pinned ratchet \
                         baseline {baseline} — floors may only move UP"
                    ));
                }
            }
            None => violations.push(format!(
                "crate {name} missing from [crates] section — every workspace \
                 crate must carry an explicit floor"
            )),
        }
    }
    assert!(
        violations.is_empty(),
        "S3-7: {} coverage ratchet violation(s):\n{}\n\
         Per the PR #1079 ratchet rule, raising a floor requires raising the \
         pinned baseline in this test in the SAME commit (visible review).",
        violations.len(),
        violations.join("\n")
    );
}

#[test]
fn coverage_lockdown_required_crates_are_listed() {
    let content = read_thresholds_toml();
    let required = ["common", "core", "trading", "storage", "api", "app"];
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
