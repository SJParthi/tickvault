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
/// initial floor per the standing formula = 64.9 - 0.1 = 64.8 (the misses are the
/// thin [[bin]] glue + SDK client construction + live-AWS async glue —
/// structurally unexercisable without a live AWS account).
/// 2026-07-20: 64.8 -> 81.1 - the pin ratcheted to the raised TOML floor
/// (first CI Coverage & Perf measurement 81.40 on runs 29739772946 +
/// 29745302463; PR #1694).
///
/// tickvault-logs-mcp pin added 2026-07-18 (rust-only phase 2c): initial
/// CONSERVATIVE floor 80.0 from a dev-sandbox measurement of 85.83%
/// (cargo llvm-cov, all targets incl. the parity harness subprocess
/// coverage); ratchet to the standing measured-minus-0.1 formula after
/// the first CI Coverage & Perf measurement.
const PINNED_DEFAULT_FLOOR: f64 = 63.0;
const PINNED_CRATE_FLOORS: &[(&str, f64)] = &[
    // common pin 99.5 -> 99.4 (2026-08-18): AUTHORING-ERROR correction, not a
    // ratchet bypass. The standing formula is measured-rounded-down-minus-0.1;
    // the 2026-06-10 baseline measured 99.57, so the floor should have been
    // 99.5 - 0.1 = 99.4. The minus-0.1 step was skipped for common alone —
    // every other floor in the file obeys the formula. The cost is measured:
    // CI run 32109620629 (Coverage & Perf job 95626308078) reported
    // `PASS: common 99.52% (threshold: 99.5%)` — a 0.02-point margin against a
    // documented 0.07-0.13 band, so any line drifting in a shared dependency
    // turns the post-merge gate red for a reason no PR author can act on.
    // 99.4 leaves common the highest floor in the file and sits 0.12 under the
    // current measured value. Editing this pin in the same PR IS the visible
    // review this test exists to force. Up-only resumes from here.
    ("common", 99.4),
    // B3 (2026-11-13): core, trading, storage, app, aws-lambdas and
    // tickvault-logs-mcp raised to the minimum gate figure of four green main
    // CI runs (43319207393, 43255665898, 43124168055, 43070508690), rounded
    // down to one decimal, minus 0.3. Figures in the TOML header.
    ("core", 94.5),
    ("trading", 97.1),
    ("storage", 93.7),
    ("api", 98.6),
    // app pin 68.3 -> 72.6 (2026-09-06): up-only ratchet from TWO independent
    // CI Coverage & Perf measurements on different trees — main @ e1a1584cd
    // (run 34016077307) 73.07% and PR #1874 @ d20e43313 (run 34027561018)
    // 72.92%. Min 72.92; 72.6 leaves 0.32 of margin rather than the formula's
    // 0.12, matching what the 2026-07-20 app ratchet actually did and what the
    // `common` correction above argues for. Editing this pin in the same PR IS
    // the visible review this test exists to force. Rationale in the TOML.
    // B3 (2026-11-13): 72.6 -> 86.4 (min 86.77 over the four runs above).
    ("app", 86.4),
    ("aws-lambdas", 90.5),
    // 2026-07-20: 80.0 -> 87.3 - the ratchet promised above (first CI Coverage & Perf measurement 87.64 on runs 29739772946 + 29745302463; PR #1694).
    ("tickvault-logs-mcp", 92.0),
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
#[allow(clippy::assertions_on_constants)] // APPROVED: this guard pins a relation between compile-time constants and is meant to fail when one is edited.
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

/// B3 (2026-11-13): production-only floors (the `[production]` table, read
/// and enforced by `coverage_prod_report` in the Coverage & Perf job). Same
/// up-only rule as `PINNED_CRATE_FLOORS`: the four-run minimum of the
/// production-only figure, rounded down to one decimal, minus 0.3.
const PINNED_PRODUCTION_FLOORS: &[(&str, f64)] = &[
    ("api", 96.6),
    ("app", 71.4),
    ("aws-lambdas", 77.8),
    ("common", 99.2),
    ("core", 88.4),
    ("storage", 87.8),
    ("tickvault-logs-mcp", 88.1),
    ("trading", 94.9),
];

/// Every problem with the `[production]` table of `content`: a missing or
/// empty table, an unparseable line, a crate written twice (quotes trimmed,
/// so `app` and `"app"` are one crate), a crate with no pin, a lost pin, or
/// a floor below its pin. Every `[production]` line is checked, in every
/// `[production]` header the file carries.
///
/// B3 review (2026-11-13): the first version read only the FIRST line for
/// each pinned crate, while `coverage_prod_report` kept the LAST, so a
/// second `app = 1.0` under the pinned line lowered the gate to 1% with this
/// test still green.
fn production_floor_violations(content: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut in_production = false;
    let mut found: Vec<(String, f64)> = Vec::new();
    for line in canonical_lines(content) {
        if line.starts_with('[') {
            in_production = line == "[production]";
            continue;
        }
        if !in_production {
            continue;
        }
        let Some((name, value)) = parse_threshold_line(&line) else {
            problems.push(format!("unparseable [production] line: {line}"));
            continue;
        };
        let name = name.trim_matches('"').to_string();
        if found.iter().any(|(n, _)| *n == name) {
            problems.push(format!(
                "[production] writes `{name}` twice; the report tool would gate the last line"
            ));
            continue;
        }
        found.push((name, value));
    }
    if found.is_empty() {
        problems.push(
            "the [production] table is missing or empty, so production-only coverage is no \
             longer floored"
                .to_string(),
        );
        return problems;
    }
    for (name, value) in &found {
        if !PINNED_PRODUCTION_FLOORS.iter().any(|(p, _)| p == name) {
            problems.push(format!(
                "[production] `{name}` has no pin in PINNED_PRODUCTION_FLOORS"
            ));
        }
        if !value.is_finite() {
            problems.push(format!("[production] `{name}` = {value} is not a number"));
        }
    }
    for (name, floor) in PINNED_PRODUCTION_FLOORS {
        match found.iter().find(|(n, _)| n == name) {
            None => problems.push(format!("[production] lost the {name} floor")),
            Some((_, value)) if *value < *floor => problems.push(format!(
                "[production] {name} = {value} is BELOW the pinned {floor}; floors only move up"
            )),
            Some(_) => {}
        }
    }
    problems
}

/// Regression (B3): the gate counted test code inside src as covered and
/// nothing floored production coverage. The `[production]` table must exist,
/// list every pinned crate once, and keep each at or above its pin, so
/// deleting the table (which turns the second check off) fails here.
#[test]
fn coverage_lockdown_production_floors_present_and_at_or_above_pins() {
    let content = read_thresholds_toml();
    let problems = production_floor_violations(&content);
    assert!(problems.is_empty(), "B3: {problems:#?}");
}

#[test]
fn production_floor_checker_refuses_duplicates_unpinned_and_lowered_floors() {
    let real = read_thresholds_toml();
    assert!(production_floor_violations(&real).is_empty());

    let insert_after_app =
        |extra: &str| real.replacen("app = 71.4\n", &format!("app = 71.4\n{extra}\n"), 1);
    for planted in ["app = 1.0", "\"app\" = 1.0", "app = 99.0"] {
        let text = insert_after_app(planted);
        assert_ne!(text, real, "the plant must land in the real file");
        let problems = production_floor_violations(&text);
        assert!(
            problems.iter().any(|p| p.contains("twice")),
            "{planted:?} -> {problems:?}"
        );
    }
    let second_table = format!("{real}\n[production]\napp = 1.0\n");
    assert!(
        production_floor_violations(&second_table)
            .iter()
            .any(|p| p.contains("twice"))
    );
    let unpinned = insert_after_app("ghost = 1.0");
    assert!(
        production_floor_violations(&unpinned)
            .iter()
            .any(|p| p.contains("no pin"))
    );
    let lowered = real.replacen("app = 71.4\n", "app = 71.3\n", 1);
    assert!(
        production_floor_violations(&lowered)
            .iter()
            .any(|p| p.contains("BELOW"))
    );
    assert!(
        production_floor_violations("default = 63.0\n[crates]\napp = 1.0\n")
            .iter()
            .any(|p| p.contains("missing or empty"))
    );
}

/// Every problem with a production pin list: not exactly one pin per crate
/// pinned in `crate_pins` (by name, in any order), a pin out of (0, 100], or
/// a pin under `default_floor`.
///
/// B3 review (2026-11-13): `production_floor_violations` only compares the
/// toml with the pins, so dropping one crate from BOTH places, or a typo
/// such as 7.14 for 71.4 written in both, passed every test. This mirrors
/// `coverage_lockdown_pinned_floors_are_sane` for the `[crates]` pins.
fn production_pin_problems(
    pins: &[(&str, f64)],
    crate_pins: &[(&str, f64)],
    default_floor: f64,
) -> Vec<String> {
    let mut problems = Vec::new();
    if pins.len() != crate_pins.len() {
        problems.push(format!(
            "expected {} production pins (one per pinned crate), found {}",
            crate_pins.len(),
            pins.len()
        ));
    }
    for (name, _) in crate_pins {
        let count = pins.iter().filter(|(p, _)| p == name).count();
        if count != 1 {
            problems.push(format!(
                "crate {name} has {count} production pins; it needs exactly one"
            ));
        }
    }
    for (name, floor) in pins {
        if !crate_pins.iter().any(|(c, _)| c == name) {
            problems.push(format!("production pin {name} names no pinned crate"));
        }
        if !(*floor > 0.0 && *floor <= 100.0) {
            problems.push(format!("production pin for {name} out of range: {floor}"));
        } else if *floor < default_floor {
            problems.push(format!(
                "production pin for {name} ({floor}) is under the default floor \
                 ({default_floor}); a typo such as 7.14 for 71.4 looks like this"
            ));
        }
    }
    problems
}

#[test]
fn coverage_lockdown_production_pins_are_sane() {
    let problems = production_pin_problems(
        PINNED_PRODUCTION_FLOORS,
        PINNED_CRATE_FLOORS,
        PINNED_DEFAULT_FLOOR,
    );
    assert!(problems.is_empty(), "B3: {problems:#?}");
    assert_eq!(PINNED_PRODUCTION_FLOORS.len(), 8);
}

#[test]
fn production_pin_sanity_bites_on_a_dropped_or_mistyped_pin() {
    let real = PINNED_PRODUCTION_FLOORS;
    let dropped: Vec<(&str, f64)> = real.iter().copied().filter(|(n, _)| *n != "app").collect();
    let problems = production_pin_problems(&dropped, PINNED_CRATE_FLOORS, PINNED_DEFAULT_FLOOR);
    assert!(
        problems.iter().any(|p| p.contains("crate app has 0")),
        "{problems:?}"
    );
    let typo: Vec<(&str, f64)> = real
        .iter()
        .map(|(n, f)| if *n == "app" { (*n, 7.14) } else { (*n, *f) })
        .collect();
    let problems = production_pin_problems(&typo, PINNED_CRATE_FLOORS, PINNED_DEFAULT_FLOOR);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("under the default floor")),
        "{problems:?}"
    );
    let out_of_range: Vec<(&str, f64)> = real
        .iter()
        .map(|(n, f)| if *n == "core" { (*n, 101.0) } else { (*n, *f) })
        .collect();
    let problems =
        production_pin_problems(&out_of_range, PINNED_CRATE_FLOORS, PINNED_DEFAULT_FLOOR);
    assert!(
        problems.iter().any(|p| p.contains("out of range")),
        "{problems:?}"
    );
    let renamed: Vec<(&str, f64)> = real
        .iter()
        .map(|(n, f)| if *n == "api" { ("apii", *f) } else { (*n, *f) })
        .collect();
    let problems = production_pin_problems(&renamed, PINNED_CRATE_FLOORS, PINNED_DEFAULT_FLOOR);
    assert!(
        problems.iter().any(|p| p.contains("names no pinned crate")),
        "{problems:?}"
    );
}
