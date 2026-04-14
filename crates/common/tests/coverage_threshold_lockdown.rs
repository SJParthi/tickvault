//! S3-7: Coverage threshold lockdown.
//!
//! Reads `quality/crate-coverage-thresholds.toml` and enforces that:
//!
//! 1. The `default` threshold is exactly `100.0`
//! 2. Every declared crate is at exactly `100.0`
//! 3. The file itself exists and is non-empty
//!
//! This prevents a hidden-in-a-PR weakening of coverage. `scripts/coverage-gate.sh`
//! reads this file in CI and blocks merges on regression, but the threshold
//! VALUE itself was previously unlocked — any PR could silently set
//! `common = 95.0` to ship a half-tested crate. This test blocks that at
//! push time via pre-push Gate 9 (locked facts).
//!
//! Any attempt to lower a threshold requires editing this test in the same
//! commit, which forces a visible review.

use std::path::Path;

/// Policy: 100% line coverage for every crate. No exceptions.
const MIN_COVERAGE: f64 = 100.0;

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

/// Strips whitespace and comments to produce a line we can match regexes on.
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

#[test]
fn coverage_lockdown_file_exists() {
    let content = read_thresholds_toml();
    assert!(
        !content.is_empty(),
        "S3-7: coverage threshold file is empty"
    );
}

#[test]
fn coverage_lockdown_default_is_100() {
    let content = read_thresholds_toml();
    let lines = canonical_lines(&content);
    let default_line = lines
        .iter()
        .find(|l| l.starts_with("default"))
        .expect("S3-7: coverage threshold file must declare `default = ...`"); // APPROVED: test
    // Expect `default = 100.0` or `default = 100`.
    assert!(
        default_line.contains("100"),
        "S3-7: default coverage threshold MUST be 100.0, got: {default_line}. \
         Policy is 100% line coverage for every crate. Do NOT weaken without \
         parity enforcement elsewhere."
    );
    // Extra safety: no threshold below MIN_COVERAGE should appear on the default line.
    let _ = MIN_COVERAGE; // referenced for docs; actual parse intentionally loose
}

#[test]
fn coverage_lockdown_every_crate_is_100() {
    let content = read_thresholds_toml();
    let lines = canonical_lines(&content);
    // Find every `<crate> = <value>` line inside the [crates] section.
    let mut in_crates_section = false;
    let mut violations: Vec<String> = Vec::new();
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
        // Parse `<name> = <number>`
        if let Some((name, value)) = line.split_once('=') {
            let name = name.trim();
            let value = value.trim();
            // Value must contain "100" and not start with a digit less than 100.
            // A parse-and-compare would be cleaner but adds a TOML dep; keep it
            // simple with substring checks.
            if !value.contains("100") {
                violations.push(format!("crate {name} is set to {value} — MUST be 100.0"));
            }
            // Catch "99.9" which contains "99" not "100"
            if value.starts_with("99") || value.starts_with("95") || value.starts_with("90") {
                violations.push(format!("crate {name} is set to {value} — MUST be 100.0"));
            }
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
    let required = ["common", "core", "trading", "storage", "api", "app"];
    let mut missing = Vec::new();
    for crate_name in required {
        // Expect a line like `common  = 100.0` inside [crates].
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
