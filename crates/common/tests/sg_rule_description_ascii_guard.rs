//! Guard: security-group RULE descriptions must be pure ASCII.
//!
//! ## Why this exists
//!
//! AWS rejects non-ASCII characters in security-group **rule** descriptions.
//! Issue #229: an em-dash in `main.tf`'s B4 ingress description hard-failed
//! `terraform apply` — after the plan had passed, with the state lock taken.
//! Variable, output and alarm descriptions are unaffected; only the `ingress` /
//! `egress` rule blocks matter.
//!
//! ## Why it is Rust now
//!
//! This check used to be a 13-line `perl -ne` program embedded in
//! `.github/workflows/terraform-apply.yml`. It was the last live interpreter
//! invocation in the repository, and it sat directly against the operator's
//! standing directive that the workspace be Rust-only
//! (`rust-only-forever-lock-2026-07-19.md`). Porting it here removes the
//! interpreter and, incidentally, improves coverage: the perl step ran only on
//! the path-filtered terraform workflow, whereas this runs in `Test (common)`
//! on **every** PR through the All Green gate.
//!
//! ## Deliberate behavioural difference from the perl original
//!
//! The perl program never reset its block-tracking state between files (`$in`
//! and `$d` persisted across `ARGV`), so an unbalanced brace in one file could
//! silently change how the NEXT file was parsed. This implementation resets per
//! file. That is strictly more correct, and it is called out rather than left
//! for someone to discover as a mysterious behaviour change.

use std::path::PathBuf;
use std::process::Command;

const TF_DIR: &str = "deploy/aws/terraform";

fn repo_root() -> PathBuf {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("sg_rule_description_ascii_guard: `git rev-parse` failed");
    assert!(out.status.success(), "not a git checkout");
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// True if the line OPENS an `ingress` / `egress` block, including the
/// `dynamic "ingress" {` form.
///
/// Mirrors the perl `/^\s*(dynamic\s+")?(ingress|egress)"?\s*\{/`.
fn opens_rule_block(line: &str) -> bool {
    let t = line.trim_start();
    let rest = if let Some(after) = t.strip_prefix("dynamic") {
        // `dynamic "ingress" {`
        after.trim_start().trim_start_matches('"')
    } else {
        t
    };

    for kw in ["ingress", "egress"] {
        if let Some(after) = rest.strip_prefix(kw) {
            let after = after.trim_start().trim_start_matches('"').trim_start();
            if after.starts_with('{') {
                return true;
            }
        }
    }
    false
}

/// True if the line is a `description = ...` assignment.
fn is_description_assignment(line: &str) -> bool {
    let t = line.trim_start();
    let Some(after) = t.strip_prefix("description") else {
        return false;
    };
    after.trim_start().starts_with('=')
}

/// A finding: file, 1-based line number, and the offending line.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    file: String,
    line_no: usize,
    line: String,
}

/// Scan one file's contents for non-ASCII rule descriptions.
fn scan(file: &str, contents: &str) -> Vec<Violation> {
    let mut violations = Vec::new();
    // Per-file state — see the module note on the perl original's cross-file leak.
    let mut in_block = false;
    let mut depth: i64 = 0;

    for (idx, line) in contents.lines().enumerate() {
        if opens_rule_block(line) {
            in_block = true;
        }
        if !in_block {
            continue;
        }

        depth += line.matches('{').count() as i64;
        depth -= line.matches('}').count() as i64;

        if is_description_assignment(line) && !line.is_ascii() {
            violations.push(Violation {
                file: file.to_string(),
                line_no: idx + 1,
                line: line.trim().to_string(),
            });
        }

        if depth <= 0 {
            in_block = false;
            depth = 0;
        }
    }

    violations
}

#[test]
fn security_group_rule_descriptions_are_ascii_only() {
    let root = repo_root();
    let dir = root.join(TF_DIR);
    let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {TF_DIR}: {e}"));

    let mut violations = Vec::new();
    let mut scanned = 0_usize;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("tf") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        scanned += 1;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        violations.extend(scan(&name, &contents));
    }

    assert!(
        scanned > 0,
        "no .tf files found under {TF_DIR} — guard is vacuous"
    );

    assert!(
        violations.is_empty(),
        "NON-ASCII IN SECURITY-GROUP RULE DESCRIPTION ({} found across {scanned} .tf files).\n\
         AWS REJECTS these at apply time — after the plan passed and the state lock \
         was taken (issue #229: an em-dash in the B4 ingress description).\n\
         Use plain ASCII (a hyphen, not an em-dash) in ingress/egress description \
         values. Variable/output/alarm descriptions are unaffected.\n\n{}",
        violations.len(),
        violations
            .iter()
            .map(|v| format!("  {}:{}: {}", v.file, v.line_no, v.line))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_perl_implementation_is_gone_and_stays_gone() {
    // This test's whole purpose is to be the replacement. If the perl step comes
    // back, the repository regains an interpreter and this guard becomes a
    // redundant second opinion rather than the enforcement.
    let wf = std::fs::read_to_string(repo_root().join(".github/workflows/terraform-apply.yml"))
        .expect("terraform-apply.yml must exist");

    // Strip YAML comments before looking for an invocation. The replacement
    // note in that file NAMES perl to explain why it left, and a naive
    // substring search would trip on the very comment documenting the removal —
    // a guard that fails on its own explanation teaches people to delete the
    // explanation.
    let executable: String = wf
        .lines()
        .map(|l| match l.find('#') {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !executable.contains("perl"),
        "terraform-apply.yml invokes perl again. The ASCII SG-description check \
         lives in this Rust test now (rust-only-forever-lock-2026-07-19.md); \
         re-adding the interpreter needs a dated operator quote in that rule file."
    );
}

#[cfg(test)]
mod scanner_self_tests {
    use super::*;

    /// The exact character from issue #229. Built via `char::from_u32` rather
    /// than typed inline so this file itself stays pure ASCII — and so the
    /// fixture cannot silently become ASCII text the way a mistyped `\\u{...}`
    /// escape did on the first cut of these tests, which made them pass while
    /// asserting nothing.
    fn em_dash() -> char {
        char::from_u32(0x2014).expect("U+2014 EM DASH is a valid scalar")
    }

    fn e_acute() -> char {
        char::from_u32(0x00E9).expect("U+00E9 LATIN SMALL E WITH ACUTE is valid")
    }

    #[test]
    fn fixture_helpers_really_are_non_ascii() {
        // Guard the guard: if these ever became ASCII, every detection test
        // below would pass vacuously.
        assert!(!em_dash().is_ascii());
        assert!(!e_acute().is_ascii());
    }

    #[test]
    fn detects_non_ascii_inside_an_ingress_block() {
        let tf = format!(
            "resource \"aws_security_group\" \"x\" {{\n  ingress {{\n    \
             description = \"SSH {} admin\"\n    from_port = 22\n  }}\n}}\n",
            em_dash()
        );
        let v = scan("t.tf", &tf);
        assert_eq!(v.len(), 1, "should flag the em-dash description");
        assert_eq!(v[0].line_no, 3);
    }

    #[test]
    fn detects_inside_the_dynamic_form() {
        let tf = format!(
            "dynamic \"ingress\" {{\n  content {{\n    description = \"caf{}\"\n  }}\n}}\n",
            e_acute()
        );
        assert_eq!(scan("t.tf", &tf).len(), 1);
    }

    #[test]
    fn ignores_non_ascii_outside_rule_blocks() {
        // The exact false-positive class the perl original was scoped to avoid:
        // variable/output/alarm descriptions accept non-ASCII fine.
        let tf = format!(
            "variable \"x\" {{\n  description = \"budget {} monthly\"\n}}\n\
             resource \"aws_cloudwatch_metric_alarm\" \"a\" {{\n  \
             alarm_description = \"lag {} 5s\"\n}}\n",
            em_dash(),
            e_acute()
        );
        assert!(
            scan("t.tf", &tf).is_empty(),
            "must not flag descriptions outside ingress/egress rule blocks"
        );
    }

    #[test]
    fn ascii_descriptions_inside_rule_blocks_pass() {
        let tf = "ingress {\n  description = \"SSH - admin\"\n}\n";
        assert!(scan("t.tf", tf).is_empty());
    }

    #[test]
    fn block_closes_at_matching_brace_so_later_lines_are_not_scanned() {
        // Depth tracking must END the block; otherwise every later description
        // in the file gets scanned and unrelated non-ASCII fails the build.
        let tf = "ingress {\n  description = \"ok\"\n}\n\
                  variable \"v\" {\n  description = \"tarif \\u{e9}t\\u{e9}\"\n}\n";
        assert!(
            scan("t.tf", tf).is_empty(),
            "state leaked past the closing brace of the rule block"
        );
    }

    #[test]
    fn state_resets_between_files() {
        // The perl original leaked $in/$d across files. Prove we do not: an
        // unbalanced ingress block in one file must not arm the next.
        let unbalanced = "ingress {\n  description = \"ok\"\n";
        let next_file = "variable \"v\" {\n  description = \"caf\\u{e9}\"\n}\n";
        assert!(scan("a.tf", unbalanced).is_empty());
        assert!(
            scan("b.tf", next_file).is_empty(),
            "block state leaked from a previous file"
        );
    }

    #[test]
    fn opens_rule_block_matches_the_documented_forms() {
        assert!(opens_rule_block("  ingress {"));
        assert!(opens_rule_block("egress {"));
        assert!(opens_rule_block("  dynamic \"ingress\" {"));
        assert!(opens_rule_block("dynamic \"egress\"  {"));
        // Must NOT match look-alikes that are not rule blocks.
        assert!(!opens_rule_block("  ingress_rules = []"));
        assert!(!opens_rule_block("# ingress {"));
        assert!(!opens_rule_block("  egressive {"));
    }

    #[test]
    fn is_description_assignment_is_precise() {
        assert!(is_description_assignment("  description = \"x\""));
        assert!(is_description_assignment("description=\"x\""));
        assert!(!is_description_assignment("  alarm_description = \"x\""));
        assert!(!is_description_assignment("  descriptions = []"));
    }
}
