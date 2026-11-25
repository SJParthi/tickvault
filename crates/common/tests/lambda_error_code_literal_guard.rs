//! Every `code = "..."` literal in `crates/aws-lambdas` must name a real
//! `ErrorCode` variant.
//!
//! ADDED 2026-09-05, with the `LAMBDA-*` family.
//!
//! # Why a guard rather than the enum
//!
//! `crates/aws-lambdas` does NOT depend on `tickvault-common` (verified in its
//! `Cargo.toml` on the day this landed), so its 43 newly-coded `error!` sites
//! spell their codes as string literals — matching the one site that already
//! did (`operator_control.rs`, `HTTP-CLIENT-01`). Adding the dependency was
//! the alternative and was rejected: it changes the dependency graph of the
//! Lambda binaries for a compile-time constant they use once each.
//!
//! The cost of literals is that nothing type-checks them. A typo
//! (`LAMBDA-AWS-1`, `LAMBDA-NOTIFY-O1`) compiles, ships, and produces a log
//! line whose `code` field matches no metric filter and no triage rule — which
//! is indistinguishable from the uncoded state the family was created to end,
//! while LOOKING coded to any reader. `error_code_rule_file_crossref` cannot
//! catch it: that guard walks the ENUM, and a typo'd literal is not in it.
//!
//! So this guard closes the loop from the other side: it reads the literals
//! out of the Lambda sources and requires each to be a code the enum actually
//! defines.
//!
//! # Honest limit
//!
//! This proves every literal names a REAL code. It does NOT prove the code is
//! the RIGHT one for that site — a `LAMBDA-AWS-01` on a mutating call would
//! pass here. Choosing the right code is a review judgement, and the runbook
//! states the read/write split precisely so a reviewer can make it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tickvault_common::error_code::ErrorCode;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolvable")
}

/// Every `code = "LITERAL"` string in the given directory tree, with the
/// file:line it came from.
fn code_literals(dir: &Path, out: &mut Vec<(String, String)>, files: &mut usize) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {dir:?}: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            code_literals(&path, out, files);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let body =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path:?}: {e}"));
        *files += 1;
        for (idx, line) in body.lines().enumerate() {
            // Comments are stripped BEFORE matching. This repository has three
            // separate recorded incidents of a source scan tripping on its own
            // doc comment; a guard that reads prose as code is a guard that
            // gets weakened to shut it up.
            let code_only = match line.find("//") {
                Some(i) => &line[..i],
                None => line,
            };
            let mut rest = code_only;
            while let Some(at) = rest.find("code = \"") {
                let after = &rest[at + "code = \"".len()..];
                let Some(end) = after.find('"') else { break };
                out.push((
                    after[..end].to_string(),
                    format!("{}:{}", path.display(), idx + 1),
                ));
                rest = &after[end + 1..];
            }
        }
    }
}

#[test]
fn every_lambda_code_literal_names_a_real_error_code_variant() {
    let root = repo_root();
    let src = root.join("crates/aws-lambdas/src");
    assert!(
        src.is_dir(),
        "guard is BLIND: {src:?} is not a directory. If the crate moved, \
         repoint this guard; do not delete it."
    );

    let known: BTreeSet<&'static str> = ErrorCode::all().iter().map(|c| c.code_str()).collect();
    assert!(
        known.len() > 100,
        "the ErrorCode set collapsed to {} entries — this guard would accept \
         almost anything. A sibling guard in this repo once passed while \
         reading zero.",
        known.len()
    );

    let mut literals: Vec<(String, String)> = Vec::new();
    let mut files = 0usize;
    code_literals(&src, &mut literals, &mut files);

    assert!(
        files > 5,
        "scanned only {files} file(s) under {src:?} — the walk collapsed"
    );

    // Anti-vacuity: 43 sites were coded on 2026-09-05, plus the pre-existing
    // HTTP-CLIENT-01. A scan that finds far fewer has stopped seeing them.
    assert!(
        literals.len() >= 44,
        "found only {} `code = \"...\"` literal(s) in {src:?}, expected at \
         least 44 (43 coded 2026-09-05 + the pre-existing HTTP-CLIENT-01). \
         Either the codes were removed — which puts those failures back \
         outside every metric filter — or this scanner stopped matching them.",
        literals.len()
    );

    let unknown: Vec<String> = literals
        .iter()
        .filter(|(code, _)| !known.contains(code.as_str()))
        .map(|(code, at)| format!("  {at}: `{code}`"))
        .collect();

    assert!(
        unknown.is_empty(),
        "these `code = \"...\"` literals name no ErrorCode variant:\n{}\n\n\
         A code that does not exist is WORSE than no code: every CloudWatch \
         metric filter and every triage rule matches on the exact string, so \
         the line reads as coded to a human and matches nothing mechanically. \
         Fix the spelling, or add the variant to `ErrorCode` (which also needs \
         a rule-file mention and a triage rule — see \
         `docs/error-runbooks/lambda-ops-error-codes.md`).",
        unknown.join("\n")
    );
}

/// Scanner self-test: prove the extractor finds a literal and ignores one that
/// is only mentioned in a comment.
#[test]
fn literal_scanner_reads_code_and_skips_comments() {
    let dir = std::env::temp_dir().join(format!(
        "tv-lambda-literal-guard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(
        dir.join("fixture.rs"),
        "error!(code = \"LAMBDA-AWS-01\", \"real\");\n\
         // error!(code = \"LAMBDA-TYPO-99\", \"in a comment\");\n\
         let x = 1; // trailing code = \"ALSO-COMMENT\"\n",
    )
    .expect("write fixture");

    let mut out = Vec::new();
    let mut files = 0usize;
    code_literals(&dir, &mut out, &mut files);
    let _ = std::fs::remove_dir_all(&dir);

    let found: Vec<&str> = out.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(
        found,
        vec!["LAMBDA-AWS-01"],
        "the extractor must read the real literal and NOT the two in comments"
    );
    assert_eq!(files, 1);
}
