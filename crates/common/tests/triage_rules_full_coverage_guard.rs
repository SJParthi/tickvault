//! Meta-guard — every `ErrorCode` variant has a rule in
//! `.claude/triage/error-rules.yaml`.
//!
//! Milestone 2 of `.claude/plans/autonomous-operations-100pct.md`.
//!
//! Without this guard a new ErrorCode variant added to the enum would
//! silently bypass the autonomous-ops classifier — resulting in no
//! triage decision, no auto-fix, no Telegram on that code. This test
//! fails the build if ANY variant lacks a matching rule.

use std::path::PathBuf;

use tickvault_common::error_code::ErrorCode;

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn load_rules_yaml() -> String {
    let path = repo_root().join(".claude/triage/error-rules.yaml");
    std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            ".claude/triage/error-rules.yaml missing or unreadable: {err}\n\
             Path: {}",
            path.display()
        )
    })
}

#[test]
fn every_error_code_variant_has_a_triage_rule() {
    let yaml = load_rules_yaml();
    let mut missing: Vec<&'static str> = Vec::new();
    for code in ErrorCode::all() {
        let needle = format!("code: {}", code.code_str());
        // Require the exact pattern `code: <CODE>` to appear somewhere,
        // either on its own line or followed by a whitespace + comment.
        // The YAML structure is `      code: <CODE>` so the leading
        // spaces and trailing context are validated by the schema test
        // below, not here.
        if !yaml_contains_code(&yaml, &needle) {
            missing.push(code.code_str());
        }
    }
    assert!(
        missing.is_empty(),
        "The following ErrorCode variants have no rule in \
         .claude/triage/error-rules.yaml:\n  {}\n\n\
         Every variant MUST have a triage decision (silence / \
         auto_restart / auto_fix / escalate). See \
         .claude/plans/autonomous-operations-100pct.md milestone 2.",
        missing.join("\n  ")
    );
}

/// Match `code: <NEEDLE>` at the start of a whitespace-only indented
/// token stream. We accept the same-line comment form (`code: X  #
/// placeholder...`) but reject partial matches like `code: DH-90` when
/// looking for `code: DH-9` (unlikely here but worth guarding).
fn yaml_contains_code(yaml: &str, needle: &str) -> bool {
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(needle)
            && (rest.is_empty()
                || rest.starts_with(' ')
                || rest.starts_with('\t')
                || rest.starts_with('#'))
        {
            return true;
        }
    }
    false
}

#[test]
fn rule_count_matches_or_exceeds_variant_count() {
    let yaml = load_rules_yaml();
    let rule_count = yaml
        .lines()
        .filter(|l| l.trim_start().starts_with("- name:"))
        .count();
    let variant_count = ErrorCode::all().len();
    assert!(
        rule_count >= variant_count,
        "Rule count {rule_count} < ErrorCode variant count {variant_count}. \
         Every variant must have at least one rule; multiple rules for the \
         same code are permitted (e.g. fan-out by message_contains)."
    );
}

#[test]
fn every_rule_has_a_required_action_field() {
    let yaml = load_rules_yaml();
    // Walk through rule blocks; each `- name:` must be followed within
    // its block by one of the 4 allowed action values.
    let allowed_actions = ["silence", "auto_restart", "auto_fix", "escalate"];
    let mut rule_names: Vec<&str> = Vec::new();
    let mut current_rule: Option<String> = None;
    let mut current_has_action = false;
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        if let Some(name) = trimmed.strip_prefix("- name: ") {
            // Finish previous rule
            if let Some(rn) = current_rule.take() {
                assert!(
                    current_has_action,
                    "Rule '{rn}' has no action field. Must be one of: {}",
                    allowed_actions.join(" / ")
                );
            }
            let name_owned = name.trim().to_string();
            // Leak intentional — test-only, tiny, vector holds the &str
            // for the duration of the test so diagnostics can print it.
            let name_ref = Box::leak(name_owned.clone().into_boxed_str());
            rule_names.push(name_ref);
            current_rule = Some(name_owned);
            current_has_action = false;
        } else if let Some(action) = trimmed.strip_prefix("action: ") {
            // Strip inline comments from action value (yaml allows "action: silence  # ...")
            let action_clean = action.split('#').next().unwrap_or(action).trim();
            assert!(
                allowed_actions.contains(&action_clean),
                "Rule '{}' has invalid action '{action_clean}'. \
                 Allowed: {}",
                current_rule.as_deref().unwrap_or("<unknown>"),
                allowed_actions.join(" / ")
            );
            current_has_action = true;
        }
    }
    // Final rule
    if let Some(rn) = current_rule {
        assert!(
            current_has_action,
            "Rule '{rn}' has no action field (last rule in file)."
        );
    }
    assert!(!rule_names.is_empty(), "No rules parsed from yaml");
}

#[test]
fn critical_severity_codes_must_all_escalate() {
    // Safety invariant: ErrorCode::is_auto_triage_safe() returns false
    // for Critical severity. This test walks the yaml and asserts that
    // every rule whose code maps to a Critical variant uses
    // `action: escalate` — NEVER silence / auto_restart / auto_fix.
    let yaml = load_rules_yaml();
    let critical_codes: Vec<&'static str> = ErrorCode::all()
        .iter()
        .filter(|c| !c.is_auto_triage_safe())
        .map(|c| c.code_str())
        .collect();

    // For each critical code, find its rule block(s) and verify action.
    for code in &critical_codes {
        let mut in_block_for_code = false;
        let mut block_action: Option<String> = None;
        let mut saw_any = false;
        for line in yaml.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("- name: ") {
                // End of previous block — check if it was for our code
                if in_block_for_code {
                    saw_any = true;
                    let action = block_action.as_deref().unwrap_or("<missing>");
                    assert_eq!(
                        action, "escalate",
                        "Critical code {code} must have action: escalate, \
                         got '{action}'. Critical severity codes MUST NEVER \
                         auto-action per is_auto_triage_safe()."
                    );
                }
                in_block_for_code = false;
                block_action = None;
            } else if let Some(found) = trimmed.strip_prefix("code: ") {
                let value = found.split_whitespace().next().unwrap_or("");
                if value == *code {
                    in_block_for_code = true;
                }
            } else if let Some(action) = trimmed.strip_prefix("action: ") {
                let clean = action.split('#').next().unwrap_or(action).trim();
                block_action = Some(clean.to_string());
            }
        }
        // Final block
        if in_block_for_code {
            saw_any = true;
            let action = block_action.as_deref().unwrap_or("<missing>");
            assert_eq!(
                action, "escalate",
                "Critical code {code} must have action: escalate, got '{action}'"
            );
        }
        assert!(
            saw_any,
            "Critical code {code} has no rule block — other test catches this \
             but surfacing here for clarity."
        );
    }

    // Sanity: at least some critical codes exist
    assert!(
        !critical_codes.is_empty(),
        "Expected at least one Critical-severity ErrorCode variant"
    );
}

#[test]
fn auto_fix_actions_must_reference_an_existing_script() {
    // Every `auto_fix` or `auto_restart` rule must reference a real
    // executable script in the repo. Prevents rules from pointing at
    // non-existent scripts (which would fail silently at triage time).
    let yaml = load_rules_yaml();
    let root = repo_root();
    let mut current_action: Option<String> = None;
    let mut current_script: Option<String> = None;
    let mut current_name: Option<String> = None;
    let mut errors: Vec<String> = Vec::new();

    fn check(
        errors: &mut Vec<String>,
        root: &std::path::Path,
        name: Option<&str>,
        action: Option<&str>,
        script: Option<&str>,
    ) {
        let Some(action) = action else {
            return;
        };
        if action != "auto_fix" && action != "auto_restart" {
            return;
        }
        let Some(script) = script else {
            errors.push(format!(
                "Rule '{}' has action '{}' but no auto_fix_script field",
                name.unwrap_or("<unknown>"),
                action
            ));
            return;
        };
        let path = root.join(script);
        if !path.is_file() {
            errors.push(format!(
                "Rule '{}' points at auto_fix_script '{}' which does not exist",
                name.unwrap_or("<unknown>"),
                script
            ));
        }
    }

    for line in yaml.lines() {
        let trimmed = line.trim_start();
        if let Some(name) = trimmed.strip_prefix("- name: ") {
            check(
                &mut errors,
                &root,
                current_name.as_deref(),
                current_action.as_deref(),
                current_script.as_deref(),
            );
            current_name = Some(name.trim().to_string());
            current_action = None;
            current_script = None;
        } else if let Some(action) = trimmed.strip_prefix("action: ") {
            let clean = action.split('#').next().unwrap_or(action).trim();
            current_action = Some(clean.to_string());
        } else if let Some(script) = trimmed.strip_prefix("auto_fix_script: ") {
            current_script = Some(script.trim().to_string());
        }
    }
    check(
        &mut errors,
        &root,
        current_name.as_deref(),
        current_action.as_deref(),
        current_script.as_deref(),
    );

    assert!(
        errors.is_empty(),
        "auto_fix/auto_restart rule errors:\n  {}",
        errors.join("\n  ")
    );
}

#[test]
fn coverage_summary_prints_breakdown() {
    // Informational test — always passes. Prints a coverage table so
    // developers can see at a glance which codes have which actions.
    // Useful on local test runs; ignored in CI output by name filter.
    let yaml = load_rules_yaml();
    let total_variants = ErrorCode::all().len();
    let rule_count = yaml
        .lines()
        .filter(|l| l.trim_start().starts_with("- name:"))
        .count();
    let critical_count = ErrorCode::all()
        .iter()
        .filter(|c| !c.is_auto_triage_safe())
        .count();

    eprintln!(
        "\n  Triage rule coverage summary:\n    \
         ErrorCode variants: {total_variants}\n    \
         Triage rules:       {rule_count}\n    \
         Critical codes:     {critical_count} (all must escalate)\n    \
         All-variant guard:  every_error_code_variant_has_a_triage_rule"
    );
}
