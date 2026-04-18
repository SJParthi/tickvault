//! Runbook cross-link guard — every `docs/runbooks/*.md` file the
//! index claims exists MUST actually exist, and every path it
//! references (`.claude/rules/...`, `scripts/...`, `deploy/...`,
//! `crates/...`) must resolve on disk.
//!
//! Prevents runbook rot — the #1 failure mode for ops docs is stale
//! links that look authoritative but point at renamed/deleted paths.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const RUNBOOK_DIR: &str = "docs/runbooks";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn runbook_paths() -> Vec<PathBuf> {
    let dir = workspace_root().join(RUNBOOK_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect()
}

#[test]
fn runbook_dir_has_at_least_readme_and_two_deep_dives() {
    let files: HashSet<String> = runbook_paths()
        .iter()
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()).map(String::from))
        .collect();

    for required in ["README.md", "auth.md", "zero-tick-loss.md", "dashboards.md"] {
        assert!(
            files.contains(required),
            "docs/runbooks/ missing {required}"
        );
    }
}

/// Pre-existing broken runbook links we don't own in this session.
/// Every entry must have a justification comment + TODO marker so the
/// allowlist stays shrinking.
const KNOWN_BROKEN_ALLOWLIST: &[&str] = &[
    // aws-deploy.md references a GitHub OIDC setup runbook that was
    // never authored. TODO: write docs/runbooks/github-oidc-setup.md
    // or remove the reference.
    "docs/runbooks/github-oidc-setup.md",
    // daily-operations.md references an override-log template that
    // was never authored. TODO: write docs/templates/override_log.md
    // or remove the reference.
    "docs/templates/override_log.md",
];

#[test]
fn every_repo_path_referenced_in_runbooks_resolves() {
    // Scan every runbook file for inline paths starting with
    // `.claude/`, `scripts/`, `deploy/`, `crates/`, `docs/`.
    // Every such path must resolve on disk. Misses catch stale
    // references before they end up in a Telegram alert body.
    let root = workspace_root();
    let allow: std::collections::HashSet<&'static str> =
        KNOWN_BROKEN_ALLOWLIST.iter().copied().collect();
    let mut missing: Vec<String> = Vec::new();

    for runbook in runbook_paths() {
        let text = fs::read_to_string(&runbook).unwrap_or_default();
        let name = runbook.file_name().and_then(|s| s.to_str()).unwrap_or("?");

        for path in extract_repo_paths(&text) {
            if allow.contains(path.as_str()) {
                continue;
            }
            let full = root.join(&path);
            if !full.exists() {
                missing.push(format!(
                    "  {} references `{}` — does not exist on disk ({})",
                    name,
                    path,
                    full.display()
                ));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "runbooks reference paths that don't exist:\n{}\n\n\
         Either create the referenced file or update the runbook.",
        missing.join("\n")
    );
}

/// Extract `.claude/...`, `scripts/...`, `deploy/...`, `crates/...`,
/// `docs/...` paths from runbook text. Strips common markdown
/// decoration (backticks, brackets, parens) and trailing punctuation.
fn extract_repo_paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let prefixes = [".claude/", "scripts/", "deploy/", "crates/", "docs/"];

    for line in text.lines() {
        // Skip URLs — we only care about repo-relative paths.
        for prefix in &prefixes {
            let mut cursor = 0;
            while let Some(idx) = line[cursor..].find(prefix) {
                let absolute = cursor + idx;
                // Must be at a word boundary.
                if absolute > 0 {
                    let prev = line.as_bytes()[absolute - 1];
                    if prev.is_ascii_alphanumeric() || prev == b'/' || prev == b'-' || prev == b'_'
                    {
                        cursor = absolute + prefix.len();
                        continue;
                    }
                }
                let tail = &line[absolute..];
                let end = tail.find(|c: char| {
                    !(c.is_ascii_alphanumeric() || c == '/' || c == '.' || c == '_' || c == '-')
                });
                let path_str = match end {
                    Some(e) => &tail[..e],
                    None => tail,
                };
                // Skip wildcards and patterns — they're intentionally
                // not filesystem paths.
                if path_str.contains('*') || path_str.ends_with('/') || path_str.is_empty() {
                    cursor = absolute + prefix.len();
                    continue;
                }
                // Skip trailing dot (sentence terminator) or comma.
                let cleaned = path_str.trim_end_matches(['.', ',', ':', ';']);
                if !cleaned.is_empty() {
                    out.push(cleaned.to_string());
                }
                cursor = absolute + prefix.len();
            }
        }
    }

    // De-duplicate.
    let set: HashSet<String> = out.into_iter().collect();
    set.into_iter().collect()
}

#[test]
fn extract_repo_paths_handles_markdown_decoration() {
    let text = r#"
        Read `crates/common/src/error_code.rs` first.
        Runbook: `.claude/rules/project/gap-enforcement.md`
        Dashboard: `deploy/docker/grafana/dashboards/operator-health.json`
    "#;
    let paths = extract_repo_paths(text);
    assert!(paths.iter().any(|p| p == "crates/common/src/error_code.rs"));
    assert!(
        paths
            .iter()
            .any(|p| p == ".claude/rules/project/gap-enforcement.md")
    );
    assert!(
        paths
            .iter()
            .any(|p| p == "deploy/docker/grafana/dashboards/operator-health.json")
    );
}

#[test]
fn extract_repo_paths_skips_wildcards() {
    let text = "see .claude/rules/*.md for all rules";
    let paths = extract_repo_paths(text);
    assert!(paths.is_empty(), "wildcards should be skipped: {paths:?}");
}
