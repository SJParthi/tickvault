//! `docs/architecture/guarantees.md` must cite proofs that EXIST.
//!
//! ## Why this file exists
//!
//! That document opens by promising:
//!
//! > "If the proof file goes missing or the guard regresses,
//! > `make validate-automation` fails and no PR can merge."
//!
//! **Nothing enforced that.** Verified 2026-08-29: `validate-automation.sh`
//! contains no reference to the document, and the only test that touched it
//! checked two words about coverage. The promise was the single largest
//! false-OK surface in the repository — a document whose entire purpose is to
//! be the operator's proof-of-guarantees, asserting its own enforcement, with
//! none.
//!
//! What that cost, measured the same day: **six cited files and five cited
//! tests did not exist**, including all three the document calls its headline
//! zero-tick-loss proofs (`chaos_questdb_full_session.rs`, `chaos_disk_full.rs`,
//! `chaos_sigkill_replay.rs` — deleted, still cited as live evidence). An
//! operator reading that table would have believed the strongest claim in the
//! system was proven by tests that had been gone for months.
//!
//! This guard makes the document's own sentence true.
//!
//! ## What it checks
//!
//! Every `crates/**/*.rs` path cited in the document resolves to a real file,
//! and every backticked `test_*` / `chaos_*` identifier is defined somewhere
//! under `crates/`. A citation that cannot be resolved fails the build.
//!
//! ## What it deliberately does NOT check
//!
//! That a cited test PROVES the claim beside it. No mechanical check can read
//! a table row and judge whether the test underneath it is evidence for that
//! sentence — that stays a human obligation, and saying so here is the honest
//! boundary. What this guard removes is the weaker and much more common
//! failure: a citation pointing at nothing at all.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common -> repo root")
        .to_path_buf()
}

fn doc() -> String {
    let p = repo_root().join("docs/architecture/guarantees.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Pull every `crates/...rs` path out of the prose.
fn cited_files(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (i, _) in src.match_indices("crates/") {
        let rest = &src[i..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || "_/.-".contains(c)))
            .unwrap_or(rest.len());
        let cand = &rest[..end];
        if cand.ends_with(".rs") {
            out.insert(cand.to_string());
        }
    }
    out
}

/// Pull every backticked `test_*` / `chaos_*` identifier out of the prose.
///
/// Only bare identifiers: a `path.rs::name` citation is covered by the file
/// check plus this, because the `name` half still appears as an identifier.
fn cited_tests(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for chunk in src.split('`').skip(1).step_by(2) {
        let name = chunk.rsplit("::").next().unwrap_or(chunk).trim();
        let looks_like_test = name.starts_with("test_") || name.starts_with("chaos_");
        let is_ident = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if looks_like_test && is_ident {
            out.insert(name.to_string());
        }
    }
    out
}

fn crate_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                // This guard names identifiers in its own prose and carries a
                // deliberately-absent sentinel; including itself would let it
                // resolve names against its own text. Found by the negative
                // control below on its first run, which is what that control
                // is for.
                if p.file_name()
                    .is_some_and(|n| n == "guarantees_doc_citations_guard.rs")
                {
                    continue;
                }
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn every_cited_proof_file_exists() {
    let root = repo_root();
    let missing: Vec<String> = cited_files(&doc())
        .into_iter()
        .filter(|f| !root.join(f).is_file())
        .collect();

    assert!(
        missing.is_empty(),
        "guarantees.md cites {} file(s) that do NOT exist:\n  {}\n\n\
         The document promises that a missing proof file blocks every PR. \
         Either re-point the row at the test that actually proves it, or mark \
         the claim UNPROVEN — but it may not keep citing a deleted file as \
         live evidence. That is exactly how three deleted chaos tests stayed \
         listed as the headline zero-tick-loss proofs.",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn every_cited_proof_test_is_defined_somewhere() {
    let root = repo_root();
    let haystack: String = crate_sources(&root)
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();

    let missing: Vec<String> = cited_tests(&doc())
        .into_iter()
        .filter(|t| !haystack.contains(&format!("fn {t}")))
        .collect();

    assert!(
        missing.is_empty(),
        "guarantees.md names {} proof test(s) that are defined NOWHERE under \
         crates/:\n  {}\n\n\
         A named test that does not exist is worse than no citation: it reads \
         as proof and is not.",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn the_guard_is_not_vacuous() {
    // Both checks above pass trivially if the extractors return nothing.
    // A guard that cannot see its subject is the false-OK it was written to
    // remove, so pin that it actually finds citations.
    let d = doc();
    let files = cited_files(&d);
    let tests = cited_tests(&d);
    assert!(
        files.len() >= 20,
        "expected the document to cite many source files, found {} — the \
         extractor is probably broken, and a broken extractor makes both \
         checks above pass vacuously",
        files.len()
    );
    assert!(
        tests.len() >= 5,
        "expected the document to name several proof tests, found {}",
        tests.len()
    );
    // And prove the resolver can FAIL: a name that cannot exist must not
    // resolve, otherwise `contains` is matching something it should not.
    let haystack: String = crate_sources(&repo_root())
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    assert!(
        !haystack.contains("fn test_this_name_is_deliberately_absent_zzz"),
        "the resolver matched a name that cannot exist"
    );
}
