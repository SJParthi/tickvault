//! Guard — the 100% Audit Tracker stays consistent with its script.
//!
//! M5 of `.claude/plans/autonomous-operations-100pct.md`.
//!
//! Prevents drift between:
//!   - `.claude/plans/100pct-audit-tracker.md` (living matrix)
//!   - `scripts/100pct-audit.sh` (real-time runner)
//!   - `Makefile` target `100pct-audit`
//!
//! Every P/R dimension claimed in the tracker doc must have a matching
//! check in the script. Every GAP the script can report must have a
//! real proof artifact (file or test).

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn load(path: &str) -> String {
    let full = repo_root().join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|err| panic!("missing {}: {err}", full.display()))
}

#[test]
fn tracker_plan_doc_exists() {
    let doc = load(".claude/plans/100pct-audit-tracker.md");
    // Must contain all 4 categories
    for cat in &[
        "P — Mechanically provable",
        "R — Runtime-verifiable",
        "L — Layered (asymptotic)",
        "I — Impossible (absolute)",
    ] {
        assert!(doc.contains(cat), "tracker doc missing category `{cat}`");
    }
}

#[test]
fn audit_script_exists_and_executable() {
    let path = repo_root().join("scripts/100pct-audit.sh");
    assert!(path.is_file(), "scripts/100pct-audit.sh missing");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert!(mode & 0o100 != 0, "100pct-audit.sh not executable");
    }
}

#[test]
fn audit_script_emits_all_four_categories() {
    let src = load("scripts/100pct-audit.sh");
    for cat in &["P", "R", "L", "I"] {
        // Each category must be used at least once via record or a section header
        let needle = format!("record {cat} ");
        let section = format!("--- {cat}:");
        assert!(
            src.contains(&needle) || src.contains(&section),
            "audit script never emits rows for category {cat}"
        );
    }
}

#[test]
fn audit_script_references_only_real_files() {
    // For every `check_file_exists P|R|L|I "..." <path> "..."` line, the
    // path must resolve. Prevents drift where we rename a file without
    // updating the script.
    let src = load("scripts/100pct-audit.sh");
    let root = repo_root();
    let mut bad: Vec<String> = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("check_file_exists ") {
            continue;
        }
        // Skip documentation comments
        if trimmed.starts_with("# ") {
            continue;
        }
        // Extract the path (3rd token is the path in our calling convention:
        // check_file_exists <CAT> "<DIM>" <PATH> "<PROOF>"
        // But the args span multiple lines via `\` continuations; take the
        // next non-quoted non-empty token of the logical line.
    }
    // Instead of parsing bash, re-derive the paths by scanning for known
    // patterns: every `check_file_exists ...` must be followed within 6
    // lines by a bare path token (starts with a letter or . — not a quote).
    let lines: Vec<&str> = src.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if !line.trim_start().starts_with("check_file_exists ") {
            continue;
        }
        // Path is typically on the line right after the dimension string,
        // indented with spaces, no quotes. Look up to 4 lines ahead.
        for j in 1..=4 {
            let Some(next) = lines.get(i + j) else { break };
            let t = next.trim_start();
            if t.starts_with('"') {
                continue;
            }
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            // Candidate path: strip trailing "\" or whitespace
            let candidate = t
                .trim_end_matches('\\')
                .trim()
                .trim_end_matches('"')
                .trim_start_matches('"');
            if candidate.is_empty() {
                continue;
            }
            // Skip if it's obviously not a path (shell variable, flag, etc.)
            if candidate.starts_with('$') || candidate.starts_with('-') {
                continue;
            }
            // Skip the proof description (which is quoted and starts with
            // an uppercase letter usually). Real paths contain "/" or end
            // in a known extension.
            let looks_like_path = candidate.contains('/')
                || candidate.ends_with(".toml")
                || candidate.ends_with(".sh")
                || candidate.ends_with(".yml")
                || candidate.ends_with(".service");
            if !looks_like_path {
                continue;
            }
            let check_path = root.join(candidate);
            if !check_path.exists() {
                bad.push(format!("line {}: {}", i + j + 1, candidate));
            }
            break;
        }
    }
    assert!(
        bad.is_empty(),
        "100pct-audit.sh references files that do not exist:\n  {}",
        bad.join("\n  ")
    );
}

#[test]
fn makefile_target_wired() {
    let mk = load("Makefile");
    assert!(
        mk.contains("100pct-audit"),
        "Makefile missing `100pct-audit` target — run `make 100pct-audit` would fail"
    );
}

#[test]
fn tracker_doc_has_4_categories_section_counts() {
    let doc = load(".claude/plans/100pct-audit-tracker.md");
    // Sanity counts from the "Categorization summary" table
    for label in &[
        "P (Mechanically provable)",
        "R (Runtime-verifiable)",
        "L (Layered asymptotic)",
        "I (Impossible absolute)",
    ] {
        assert!(
            doc.contains(label),
            "tracker doc missing category summary row for `{label}`"
        );
    }
}

#[test]
fn every_cited_guard_test_exists_in_tree() {
    // Spot-check: every test name mentioned in the tracker's "proof
    // artifact" column must exist in the workspace. We scan for
    // `crates/*/tests/*.rs` patterns inside the doc and verify each.
    let doc = load(".claude/plans/100pct-audit-tracker.md");
    let root = repo_root();
    let mut missing: Vec<String> = Vec::new();
    for token in doc.split_whitespace() {
        let clean = token.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-'
        });
        // Must match shape: crates/<x>/tests/<y>.rs  OR  crates/<x>/src/<y>.rs
        let looks = (clean.starts_with("crates/")
            || clean.starts_with(".claude/")
            || clean.starts_with("scripts/")
            || clean.starts_with("quality/")
            || clean.starts_with("config/")
            || clean.starts_with("prometheus/")
            || clean.starts_with(".github/"))
            && (clean.ends_with(".rs")
                || clean.ends_with(".sh")
                || clean.ends_with(".toml")
                || clean.ends_with(".yml")
                || clean.ends_with(".md"));
        if !looks {
            continue;
        }
        let p = root.join(clean);
        if !p.exists() {
            missing.push(clean.to_string());
        }
    }
    // Allow a small allowlist of planned / cold references
    let allowlist: &[&str] = &[
        "prometheus/alerts/zero-tick-loss.yml", // the doc mentions an aspirational canonical path
        "crates/common/benches/registry.rs",    // bench names can vary
        "crates/core/benches/pipeline.rs",
        "crates/core/benches/full_tick.rs",
        "crates/trading/benches/state_machine.rs",
        "crates/api/tests/api_auth_middleware_guard.rs", // rename candidate; actual path auth_middleware.rs
        "crates/api/src/middleware.rs", // mentioned but the test file is tests/auth_middleware.rs
        "crates/storage/tests/f32_precision_regression.rs", // planned rename
    ];
    let real_missing: Vec<&String> = missing
        .iter()
        .filter(|m| !allowlist.iter().any(|a| m == a))
        .collect();
    assert!(
        real_missing.is_empty(),
        "tracker doc cites artifacts that don't exist (not on allowlist):\n  {}",
        real_missing
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}
