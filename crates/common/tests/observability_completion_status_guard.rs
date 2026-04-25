//! Real-time guard — every file path cited as "shipped" in the Completion
//! status block of `.claude/rules/project/observability-architecture.md` MUST
//! actually exist on disk.
//!
//! Why this exists: on 2026-04-25 a stale audit revealed the doc claimed
//! Phase 9.1, 10.2, 10.3, 11.1, 11.2, 11.3 were "still pending" — but every
//! one had already shipped (operator-health.json, depth_sequence_tracker.rs,
//! chaos_zero_tick_loss.rs, chaos_questdb_docker_pause.rs,
//! chaos_rescue_ring_overflow.rs, chaos_valkey_kill.rs). Future Claude
//! sessions reading the stale block would re-implement work that's already
//! done. This test fails the build the moment a `[x]` line cites a path
//! that doesn't exist, so the doc cannot drift from reality.
//!
//! What it scans:
//! - Every `[x]` checkbox line inside the "## Completion status" section
//! - Backtick-quoted tokens that look like repo paths
//!   (`crates/...`, `deploy/...`, `quality/...`, `scripts/...`,
//!   `.github/workflows/...`, `Makefile`)
//! - Asserts each path exists relative to workspace root.
//!
//! What it deliberately does NOT do:
//! - Does not police prose claims ("6 tests", "5 min", "100%")
//! - Does not parse `[ ]` (pending) or `[~]` (partial) lines — those are
//!   allowed to cite future paths.

use std::fs;
use std::path::{Path, PathBuf};

const DOC_REL_PATH: &str = ".claude/rules/project/observability-architecture.md";
const COMPLETION_HEADER: &str = "## Completion status of the Zero-Touch plan";

/// Path-like tokens look like one of:
///   crates/<...>.rs   |   crates/<...>.toml
///   deploy/<...>.json |   deploy/<...>.yml
///   quality/<...>.toml
///   scripts/<...>.sh  |   scripts/<...>.py
///   .github/workflows/<...>.yml
///   Makefile
/// Anything that doesn't look like a path (e.g. `Severity::High`, `cargo`,
/// `tv_websocket_connections_active`) is ignored.
fn looks_like_repo_path(token: &str) -> bool {
    let stripped = token.trim();
    if stripped.is_empty() {
        return false;
    }

    let allowed_prefixes = [
        "crates/", "deploy/", "quality/", "scripts/", ".github/", "data/", "docs/", "fuzz/",
    ];
    if allowed_prefixes.iter().any(|p| stripped.starts_with(p)) {
        return true;
    }

    matches!(stripped, "Makefile" | "Cargo.toml" | "rust-toolchain.toml")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Extract backtick-quoted tokens from a line. We do not import a markdown
/// crate — a tiny state machine is enough and keeps the test self-contained.
fn extract_backtick_tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut inside = false;
    for ch in line.chars() {
        if ch == '`' {
            if inside && !buf.is_empty() {
                out.push(buf.clone());
                buf.clear();
            }
            inside = !inside;
        } else if inside {
            buf.push(ch);
        }
    }
    out
}

/// A path token may carry a trailing `:line` or `:range` suffix
/// (e.g. `mutation.yml:103-113`) — strip those before checking the FS.
fn strip_line_suffix(token: &str) -> &str {
    match token.find(':') {
        Some(idx) => &token[..idx],
        None => token,
    }
}

fn read_completion_status_section(doc_text: &str) -> &str {
    let start = match doc_text.find(COMPLETION_HEADER) {
        Some(idx) => idx,
        None => return "",
    };
    let after = &doc_text[start..];
    // Section ends at the next `## ` heading.
    if let Some(next) = after[COMPLETION_HEADER.len()..].find("\n## ") {
        &after[..COMPLETION_HEADER.len() + next]
    } else {
        after
    }
}

#[test]
fn every_shipped_phase_cites_paths_that_exist_on_disk() {
    let root = workspace_root();
    let doc_path = root.join(DOC_REL_PATH);
    let doc = fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", doc_path.display()));

    let section = read_completion_status_section(&doc);
    assert!(
        !section.is_empty(),
        "could not find '{}' section in {}",
        COMPLETION_HEADER,
        doc_path.display()
    );

    // A `[x]` claim may span multiple lines (the doc indents continuation
    // lines). We only care about backtick-quoted repo paths anywhere within
    // the bullet, so we accumulate lines until the next bullet starts.
    let mut current_bullet = String::new();
    let mut current_is_shipped = false;
    let mut missing: Vec<(String, String)> = Vec::new();

    let flush =
        |bullet: &str, is_shipped: bool, missing: &mut Vec<(String, String)>, root: &Path| {
            if !is_shipped || bullet.is_empty() {
                return;
            }
            for token in extract_backtick_tokens(bullet) {
                if !looks_like_repo_path(&token) {
                    continue;
                }
                let path_only = strip_line_suffix(&token);
                let abs = root.join(path_only);
                if !abs.exists() {
                    missing.push((token.clone(), bullet.to_string()));
                }
            }
        };

    for line in section.lines() {
        let trimmed = line.trim_start();
        let starts_bullet = trimmed.starts_with("- [");
        if starts_bullet {
            // Flush the previous bullet before starting a new one.
            flush(&current_bullet, current_is_shipped, &mut missing, &root);
            current_bullet.clear();
            current_is_shipped = trimmed.starts_with("- [x]");
        }
        if starts_bullet || !current_bullet.is_empty() {
            current_bullet.push_str(line);
            current_bullet.push('\n');
        }
    }
    flush(&current_bullet, current_is_shipped, &mut missing, &root);

    if !missing.is_empty() {
        let mut report =
            String::from("stale citations in observability-architecture.md Completion status:\n\n");
        for (token, bullet) in &missing {
            report.push_str(&format!(
                "  missing path `{}` cited in bullet:\n    {}\n",
                token,
                bullet.trim().replace('\n', "\n    ")
            ));
        }
        report.push_str(
            "\nFix: either create the file, correct the citation, or move the bullet \
             from `[x]` to `[ ]`/`[~]`.\n",
        );
        panic!("{report}");
    }
}

#[test]
fn completion_status_section_exists_and_is_non_empty() {
    let root = workspace_root();
    let doc_path = root.join(DOC_REL_PATH);
    let doc = fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", doc_path.display()));
    let section = read_completion_status_section(&doc);
    assert!(
        section.len() > 200,
        "Completion status section is suspiciously short ({} chars) — guard would not detect drift",
        section.len()
    );
}

#[test]
fn extract_backtick_tokens_round_trips_simple_input() {
    let line = "- [x] **Phase X** — `crates/foo/src/bar.rs` and `deploy/baz.json`";
    let tokens = extract_backtick_tokens(line);
    assert_eq!(
        tokens,
        vec![
            "crates/foo/src/bar.rs".to_string(),
            "deploy/baz.json".to_string(),
        ]
    );
}

#[test]
fn looks_like_repo_path_recognises_expected_prefixes() {
    assert!(looks_like_repo_path("crates/core/src/lib.rs"));
    assert!(looks_like_repo_path("deploy/docker/grafana/x.json"));
    assert!(looks_like_repo_path("quality/benchmark-budgets.toml"));
    assert!(looks_like_repo_path("scripts/coverage-gate.sh"));
    assert!(looks_like_repo_path(".github/workflows/ci.yml"));
    assert!(looks_like_repo_path("Makefile"));

    // Non-paths that show up inside backticks in the doc.
    assert!(!looks_like_repo_path("Severity::High"));
    assert!(!looks_like_repo_path("error!"));
    assert!(!looks_like_repo_path("FUZZ_SECS: 3600"));
    assert!(!looks_like_repo_path("tv_websocket_connections_active"));
    assert!(!looks_like_repo_path("not(test)"));
}

#[test]
fn strip_line_suffix_removes_colon_line_numbers() {
    assert_eq!(
        strip_line_suffix(".github/workflows/mutation.yml:103-113"),
        ".github/workflows/mutation.yml"
    );
    assert_eq!(
        strip_line_suffix("crates/core/src/lib.rs"),
        "crates/core/src/lib.rs"
    );
}
