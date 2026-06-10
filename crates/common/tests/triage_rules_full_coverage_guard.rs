//! Meta-guard — every `ErrorCode` variant has a rule in
//! `.claude/triage/error-rules.yaml` OR a documented exclusion line.
//!
//! Milestone 2 of `.claude/plans/autonomous-operations-100pct.md`.
//!
//! Without this guard a new ErrorCode variant added to the enum would
//! silently bypass the autonomous-ops classifier — resulting in no
//! triage decision, no auto-fix, no Telegram on that code. This test
//! fails the build if ANY variant lacks a matching rule, unless the
//! YAML carries a documented exclusion of the form
//! `# TRIAGE-EXEMPT: <CODE> — <reason>`. Exclusion hygiene is also
//! ratcheted: exemptions must name LIVE variants, carry a non-empty
//! reason, never coexist with a rule for the same code, and never be
//! used for a code whose `is_auto_triage_safe()` is false.

use std::path::PathBuf;

use tickvault_common::error_code::ErrorCode;

/// Marker for a documented exclusion line in the rules YAML.
const EXEMPT_MARKER: &str = "TRIAGE-EXEMPT:";

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

/// Parse one `# TRIAGE-EXEMPT: <CODE> — <reason>` exemption line.
///
/// A valid exemption line MUST:
/// - start with a single `#` at COLUMN 0 (no leading whitespace). Per
///   YAML, a column-0 `#` always terminates any `justification: >-`
///   block scalar and is a real comment — so string CONTENT inside a
///   justification can never parse as an exemption (the hostile-review
///   HIGH finding this guards against);
/// - carry exactly `TRIAGE-EXEMPT:` as the first token after the `#`.
///
/// The reason separator is one optional em-dash / en-dash / hyphen
/// (stripped ONCE, so a reason that itself starts with `-` survives).
/// Returns `(code, reason)`; the reason may be empty — the hygiene test
/// rejects that case with an actionable message.
fn parse_exemption_line(line: &str) -> Option<(String, String)> {
    // Column-0 single `#` only: no trim_start, and `##` is rejected.
    let after_hash = line.strip_prefix('#')?;
    if after_hash.starts_with('#') {
        return None;
    }
    let payload = after_hash.trim_start().strip_prefix(EXEMPT_MARKER)?;
    let payload = payload.trim();
    let code: String = payload.split_whitespace().next().unwrap_or("").to_string();
    let mut rest = payload.strip_prefix(&code).unwrap_or("").trim_start();
    for sep in ['—', '–', '-'] {
        if let Some(stripped) = rest.strip_prefix(sep) {
            rest = stripped;
            break;
        }
    }
    Some((code, rest.trim().to_string()))
}

/// Parse all exemption lines from the YAML.
fn parse_exemptions(yaml: &str) -> Vec<(String, String)> {
    yaml.lines().filter_map(parse_exemption_line).collect()
}

/// Heuristic for "this line was probably MEANT to be an exemption":
/// after stripping ALL leading whitespace, `#` characters and inner
/// whitespace, it case-insensitively starts with `triage-exempt`
/// (with or without the colon). Catches the near-miss forms that the
/// strict parser ignores — `## TRIAGE-EXEMPT:`, indented exemptions,
/// lowercase `# triage-exempt:`, `# TRIAGE-EXEMPT :` — so a malformed
/// or stale exemption can never sit invisibly in the file. Doc lines
/// that QUOTE the marker (e.g. `#   "# TRIAGE-EXEMPT: ..."`) do not
/// match because the leading quote breaks the prefix.
fn looks_like_exemption(line: &str) -> bool {
    let stripped = line.trim_start().trim_start_matches(['#', ' ', '\t']);
    stripped.to_ascii_lowercase().starts_with("triage-exempt")
}

#[test]
fn every_error_code_variant_has_a_triage_rule() {
    let yaml = load_rules_yaml();
    let exempt_codes: Vec<String> = parse_exemptions(&yaml)
        .into_iter()
        .map(|(code, _reason)| code)
        .collect();
    let mut missing: Vec<&'static str> = Vec::new();
    for code in ErrorCode::all() {
        let needle = format!("code: {}", code.code_str());
        // Require the exact pattern `code: <CODE>` to appear somewhere,
        // either on its own line or followed by a whitespace + comment.
        // The YAML structure is `      code: <CODE>` so the leading
        // spaces and trailing context are validated by the schema test
        // below, not here. A documented `# TRIAGE-EXEMPT: <CODE> — ...`
        // line is the only accepted alternative to a rule block.
        if !yaml_contains_code(&yaml, &needle) && !exempt_codes.iter().any(|c| c == code.code_str())
        {
            missing.push(code.code_str());
        }
    }
    assert!(
        missing.is_empty(),
        "The following ErrorCode variants have NEITHER a rule in \
         .claude/triage/error-rules.yaml NOR a documented exclusion:\n  {}\n\n\
         Every variant MUST have a triage decision (silence / \
         auto_restart / auto_fix / escalate), or — only when no triage \
         rule can apply — a `# TRIAGE-EXEMPT: <CODE> — <reason>` line. See \
         .claude/plans/autonomous-operations-100pct.md milestone 2.",
        missing.join("\n  ")
    );
}

#[test]
fn triage_exemptions_must_name_live_codes_and_carry_reasons() {
    // Stale exemptions are the same rot class as the orphaned MOVERS
    // rules cleaned up 2026-05-18: a code is deleted from the enum but
    // its YAML artifact lingers, silently misdocumenting coverage.
    let yaml = load_rules_yaml();
    let live: Vec<&'static str> = ErrorCode::all().iter().map(|c| c.code_str()).collect();
    let mut errors: Vec<String> = Vec::new();
    for (code, reason) in parse_exemptions(&yaml) {
        if code.is_empty() {
            errors.push("TRIAGE-EXEMPT line with no code".to_string());
            continue;
        }
        if !live.contains(&code.as_str()) {
            errors.push(format!(
                "TRIAGE-EXEMPT for '{code}' — not a live ErrorCode variant \
                 (stale exemption; delete the line)"
            ));
        }
        if reason.is_empty() {
            errors.push(format!(
                "TRIAGE-EXEMPT for '{code}' has no reason — exclusions must \
                 be documented: `# TRIAGE-EXEMPT: {code} — <why no rule applies>`"
            ));
        }
    }
    assert!(
        errors.is_empty(),
        "Exemption hygiene violations:\n  {}",
        errors.join("\n  ")
    );
}

#[test]
fn exempted_codes_must_not_also_have_rules() {
    // A code with BOTH a rule block and an exemption line is ambiguous:
    // is the rule live or retired? Force one or the other.
    let yaml = load_rules_yaml();
    let mut both: Vec<String> = Vec::new();
    for (code, _reason) in parse_exemptions(&yaml) {
        if code.is_empty() {
            continue; // reported by the hygiene test above
        }
        let needle = format!("code: {code}");
        if yaml_contains_code(&yaml, &needle) {
            both.push(code);
        }
    }
    assert!(
        both.is_empty(),
        "These codes have BOTH a rule block and a TRIAGE-EXEMPT line \
         (ambiguous — keep exactly one):\n  {}",
        both.join("\n  ")
    );
}

#[test]
fn non_auto_triage_safe_codes_must_never_be_exempted() {
    // Safety invariant: a code whose `is_auto_triage_safe()` is false
    // (every Critical-severity code, plus other operator-action codes)
    // ALWAYS needs an explicit `action: escalate` rule. Exempting one
    // would remove the classifier's escalate decision for exactly the
    // codes where silence is most dangerous.
    let yaml = load_rules_yaml();
    let unsafe_codes: Vec<&'static str> = ErrorCode::all()
        .iter()
        .filter(|c| !c.is_auto_triage_safe())
        .map(|c| c.code_str())
        .collect();
    let mut violations: Vec<String> = Vec::new();
    for (code, _reason) in parse_exemptions(&yaml) {
        if unsafe_codes.contains(&code.as_str()) {
            violations.push(code);
        }
    }
    assert!(
        violations.is_empty(),
        "These non-auto-triage-safe codes are TRIAGE-EXEMPT — forbidden; \
         they MUST have an `action: escalate` rule instead:\n  {}",
        violations.join("\n  ")
    );
}

/// The deliberate, test-pinned list of exempted codes. EMPTY today —
/// every live variant has a rule. Adding a `# TRIAGE-EXEMPT:` line to
/// the YAML REQUIRES adding the code here too, so swapping a rule for
/// an exemption is always a two-file, review-visible change (hostile-
/// review HIGH finding: without this pin, a rule deletion plus an
/// exemption in the same PR would pass silently for any
/// auto-triage-safe code).
const PINNED_EXEMPTIONS: &[&str] = &[];

#[test]
fn exemptions_must_match_the_pinned_allowlist() {
    let yaml = load_rules_yaml();
    let mut found: Vec<String> = parse_exemptions(&yaml)
        .into_iter()
        .map(|(code, _reason)| code)
        .collect();
    found.sort_unstable();
    found.dedup_by(|a, b| {
        assert_ne!(a, b, "duplicate TRIAGE-EXEMPT lines for '{a}' — keep one");
        false
    });
    let mut pinned: Vec<String> = PINNED_EXEMPTIONS.iter().map(|s| s.to_string()).collect();
    pinned.sort_unstable();
    assert_eq!(
        found, pinned,
        "TRIAGE-EXEMPT lines in error-rules.yaml must exactly match \
         PINNED_EXEMPTIONS in this test file (two-file change by design). \
         YAML has: {found:?}; pinned: {pinned:?}"
    );
}

#[test]
fn exemption_like_lines_must_parse_as_valid_exemptions() {
    // Near-miss lint: `## TRIAGE-EXEMPT:`, an INDENTED exemption,
    // lowercase `# triage-exempt:`, or `# TRIAGE-EXEMPT :` are all
    // ignored by the strict parser — without this lint a stale or
    // malformed exemption could sit invisibly in the file forever
    // (the exact MOVERS-rot class the hygiene test exists to prevent).
    let yaml = load_rules_yaml();
    let mut malformed: Vec<String> = Vec::new();
    for line in yaml.lines() {
        if looks_like_exemption(line) && parse_exemption_line(line).is_none() {
            malformed.push(line.to_string());
        }
    }
    assert!(
        malformed.is_empty(),
        "Lines that LOOK like exemptions but do not parse (must be a \
         single `#` at column 0 followed by `TRIAGE-EXEMPT: <CODE> — \
         <reason>`):\n  {}",
        malformed.join("\n  ")
    );
}

#[test]
fn exemption_parser_accepts_and_rejects_expected_forms() {
    // Accepted: single column-0 `#`, em-dash / en-dash / hyphen
    // separator, optional space after `#`. NOTE: explicit join (not
    // `\`-continuation string literals — those EAT leading whitespace
    // on the next line, which would silently destroy the indentation
    // this test exists to exercise).
    let accepted = [
        "# TRIAGE-EXEMPT: FOO-01 — positive lifecycle ping, no decision needed",
        "# TRIAGE-EXEMPT: BAR-02 - hyphen separator also fine",
        "#TRIAGE-EXEMPT: QUX-04 – en-dash separator, no space after hash",
        "# TRIAGE-EXEMPT: DASH-05 - -v flag broken (reason keeps its own dash)",
    ]
    .join("\n");
    let parsed = parse_exemptions(&accepted);
    assert_eq!(parsed.len(), 4, "expected 4 exemptions, got {parsed:?}");
    assert_eq!(parsed[0].0, "FOO-01");
    assert_eq!(parsed[0].1, "positive lifecycle ping, no decision needed");
    assert_eq!(parsed[1].0, "BAR-02");
    assert_eq!(parsed[1].1, "hyphen separator also fine");
    assert_eq!(parsed[2].0, "QUX-04");
    assert_eq!(parsed[2].1, "en-dash separator, no space after hash");
    assert_eq!(parsed[3].0, "DASH-05");
    assert_eq!(
        parsed[3].1, "-v flag broken (reason keeps its own dash)",
        "the separator must be stripped ONCE, not greedily"
    );

    // Rejected as exemptions: prose that mentions codes, rule fields,
    // a marker buried mid-sentence, an INDENTED marker line (could be
    // string content inside a `justification: >-` block scalar — the
    // hostile-review HIGH finding), a double-hash form, and a
    // justification continuation line carrying the marker.
    let rejected = [
        "# MOVERS-01/02/03 rules removed 2026-05-18 (PR #1 of AWS-lifecycle)",
        "      code: DH-904",
        "# the old TRIAGE-EXEMPT: style note buried mid-sentence does not count",
        "    # TRIAGE-EXEMPT: SNEAKY-01 — indented, may be block-scalar content",
        "## TRIAGE-EXEMPT: DOC-02 — double-hash documentation form",
        "      mentions # TRIAGE-EXEMPT: INSIDE-03 — justification content",
    ]
    .join("\n");
    assert!(
        parse_exemptions(&rejected).is_empty(),
        "prose / rule fields / indented / double-hash lines must never \
         parse as exemptions: {:?}",
        parse_exemptions(&rejected)
    );

    // The lint helper DOES flag the indented and double-hash near-miss
    // forms so they cannot hide (parse=None but looks_like=true).
    assert!(looks_like_exemption(
        "    # TRIAGE-EXEMPT: SNEAKY-01 — indented"
    ));
    assert!(looks_like_exemption("## TRIAGE-EXEMPT: DOC-02 — doc form"));
    assert!(looks_like_exemption(
        "# triage-exempt: LOWER-03 — lowercase"
    ));
    assert!(looks_like_exemption(
        "# TRIAGE-EXEMPT : SPACE-04 — colon gap"
    ));
    // Quoted documentation of the marker does NOT trigger the lint.
    assert!(!looks_like_exemption(
        "#   \"# TRIAGE-EXEMPT: <CODE> — <reason>\""
    ));

    // A marker line with no reason parses with an empty reason — the
    // hygiene test is what rejects it.
    let no_reason = "# TRIAGE-EXEMPT: BAZ-03\n";
    let parsed = parse_exemptions(no_reason);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].0, "BAZ-03");
    assert!(parsed[0].1.is_empty());
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
    // An exempted code legitimately has no rule, so exemptions count
    // toward coverage here — otherwise a perfectly hygienic exemption
    // would fail this test once the fan-out slack is exhausted.
    let exemption_count = parse_exemptions(&yaml).len();
    let variant_count = ErrorCode::all().len();
    assert!(
        rule_count + exemption_count >= variant_count,
        "Rule count {rule_count} + exemptions {exemption_count} < ErrorCode \
         variant count {variant_count}. Every variant must have at least one \
         rule (or a pinned TRIAGE-EXEMPT line); multiple rules for the same \
         code are permitted (e.g. fan-out by message_contains)."
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
        // Containment: the script must stay inside the repo. A plain
        // `starts_with(root)` would NOT catch `../` traversal (Path
        // prefix matching is component-wise, pre-normalization), so
        // reject absolute paths and any `..` component explicitly.
        let script_path = std::path::Path::new(script);
        if script_path.is_absolute()
            || script_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            errors.push(format!(
                "Rule '{}' auto_fix_script '{}' escapes the repo root \
                 (absolute path or `..` component) — must be repo-relative",
                name.unwrap_or("<unknown>"),
                script
            ));
            return;
        }
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
    let exemption_count = parse_exemptions(&yaml).len();

    eprintln!(
        "\n  Triage rule coverage summary:\n    \
         ErrorCode variants: {total_variants}\n    \
         Triage rules:       {rule_count}\n    \
         Exemptions:         {exemption_count} (TRIAGE-EXEMPT lines)\n    \
         Critical codes:     {critical_count} (all must escalate)\n    \
         All-variant guard:  every_error_code_variant_has_a_triage_rule"
    );
}
