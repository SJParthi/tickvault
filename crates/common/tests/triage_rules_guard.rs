//! Phase 6 of `.claude/plans/active-plan.md` — triage rules YAML guard.
//!
//! `.claude/triage/error-rules.yaml` is the runtime classifier used by
//! both the Claude `/loop` prompt and the shell hook
//! `.claude/hooks/error-triage.sh`. If the YAML drifts out of sync with
//! the enum OR points at non-existent scripts/runbooks, the triage
//! daemon silently does nothing — exactly the failure mode the whole
//! zero-touch plan is trying to prevent.
//!
//! This guard is dep-free (pure string scanning) and asserts:
//!
//! 1. The YAML file exists + parses at a structural level (`version:`
//!    header + `rules:` block + one or more `- name:` entries).
//! 2. Every `code:` value matches an `ErrorCode::X.code_str()` variant
//!    — no ghost codes.
//! 3. Every `action:` is one of the four valid actions
//!    (silence / auto_restart / auto_fix / escalate).
//! 4. Every `auto_fix_script:` path resolves to an executable file
//!    under the repo.
//! 5. Every `runbook_path:` resolves to an existing markdown file.
//!
//! Run: `cargo test -p tickvault-common --test triage_rules_guard`

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use tickvault_common::error_code::ErrorCode;

const RULES_FILE: &str = ".claude/triage/error-rules.yaml";
const VALID_ACTIONS: &[&str] = &["silence", "auto_restart", "auto_fix", "escalate"];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_rules_text() -> String {
    let path = workspace_root().join(RULES_FILE);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Strip trailing YAML `# comment` and surrounding whitespace from a
/// scalar-value line. Handles e.g. `code: I-P1-11       # placeholder`.
fn strip_yaml_value(s: &str) -> String {
    let without_comment = s.split('#').next().unwrap_or(s);
    without_comment.trim().trim_matches('"').to_string()
}

#[test]
fn triage_rules_file_exists_and_has_structural_markers() {
    let text = load_rules_text();
    assert!(
        text.contains("version: 1"),
        "triage rules YAML missing `version: 1` header"
    );
    assert!(
        text.contains("rules:"),
        "triage rules YAML missing `rules:` block"
    );
    assert!(
        text.contains("- name:"),
        "triage rules YAML must have at least one `- name:` entry"
    );
}

#[test]
fn every_code_in_rules_yaml_matches_an_error_code_variant() {
    let text = load_rules_text();
    let known: HashSet<&'static str> = ErrorCode::all().iter().map(|c| c.code_str()).collect();
    let mut missing: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("code:") {
            continue;
        }
        let value = strip_yaml_value(trimmed.trim_start_matches("code:"));
        if value.is_empty() {
            continue;
        }
        if !known.contains(value.as_str()) {
            missing.push(value);
        }
    }

    assert!(
        missing.is_empty(),
        "triage rules YAML references codes that don't exist as ErrorCode variants:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn every_action_in_rules_yaml_is_valid() {
    let text = load_rules_text();
    let valid: HashSet<&'static str> = VALID_ACTIONS.iter().copied().collect();
    let mut invalid: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("action:") {
            continue;
        }
        let value = strip_yaml_value(trimmed.trim_start_matches("action:"));
        if !valid.contains(value.as_str()) {
            invalid.push(value);
        }
    }

    assert!(
        invalid.is_empty(),
        "triage rules YAML has invalid `action:` values (valid: {:?}):\n  {}",
        VALID_ACTIONS,
        invalid.join("\n  ")
    );
}

#[test]
fn every_auto_fix_script_exists_and_is_executable() {
    let text = load_rules_text();
    let root = workspace_root();
    let mut missing: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("auto_fix_script:") {
            continue;
        }
        let value = strip_yaml_value(trimmed.trim_start_matches("auto_fix_script:"));
        if value.is_empty() {
            continue;
        }
        let path = root.join(&value);
        if !path.exists() {
            missing.push(format!("{value} (resolved: {})", path.display()));
            continue;
        }
        // Basic exec check: on Unix, require the user-execute bit.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match fs::metadata(&path) {
                Ok(meta) => {
                    let mode = meta.permissions().mode();
                    if mode & 0o100 == 0 {
                        missing.push(format!(
                            "{value} exists but user-exec bit not set (mode={mode:o})"
                        ));
                    }
                }
                Err(err) => missing.push(format!("{value}: stat failed: {err}")),
            }
        }
    }

    assert!(
        missing.is_empty(),
        "triage rules YAML references auto_fix_scripts with problems:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn every_runbook_path_in_rules_yaml_exists() {
    let text = load_rules_text();
    let root = workspace_root();
    let mut missing: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("runbook_path:") {
            continue;
        }
        let value = strip_yaml_value(trimmed.trim_start_matches("runbook_path:"));
        if value.is_empty() {
            continue;
        }
        let path = root.join(&value);
        if !path.exists() {
            missing.push(format!("{value} (resolved: {})", path.display()));
        }
    }

    assert!(
        missing.is_empty(),
        "triage rules YAML references runbook_paths that don't exist:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn every_confidence_value_is_in_range() {
    let text = load_rules_text();
    let mut out_of_range: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("confidence:") {
            continue;
        }
        let value = strip_yaml_value(trimmed.trim_start_matches("confidence:"));
        if value.is_empty() {
            continue;
        }
        let parsed: Result<f64, _> = value.parse();
        match parsed {
            Ok(c) if (0.0..=1.0).contains(&c) => {}
            _ => out_of_range.push(value),
        }
    }

    assert!(
        out_of_range.is_empty(),
        "triage rules YAML has confidence values outside 0.0..=1.0:\n  {}",
        out_of_range.join("\n  ")
    );
}

#[test]
fn rule_names_are_unique() {
    let text = load_rules_text();
    let mut seen: HashSet<String> = HashSet::new();
    let mut dups: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("- name:") {
            continue;
        }
        let value = strip_yaml_value(trimmed.trim_start_matches("- name:"));
        if !seen.insert(value.clone()) {
            dups.push(value);
        }
    }

    assert!(
        dups.is_empty(),
        "triage rules YAML has duplicate rule names:\n  {}",
        dups.join("\n  ")
    );
}
