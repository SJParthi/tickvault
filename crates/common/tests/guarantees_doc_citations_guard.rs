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

// ---------------------------------------------------------------------------
// 2026-10-11 (audit TRU-1, TRU-6, TRU-7, TRU-18): the documents must not keep
// a claim the code disproves. Each check below reads the text with every
// `~~struck~~` span removed, because the house style corrects a stale claim
// by striking it and adding a dated note, never by deleting history. A claim
// left UNSTRUCK is a claim the document still makes.
// ---------------------------------------------------------------------------

/// Remove every `~~…~~` span (they may cross a line break). An unpaired
/// trailing `~~` is kept as text, so an open strike cannot hide the rest of
/// the document.
fn without_struck(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(open) = rest.find("~~") {
        let after_open = &rest[open + 2..];
        match after_open.find("~~") {
            Some(close) => {
                out.push_str(&rest[..open]);
                rest = &after_open[close + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

fn plain_english_doc() -> String {
    let p = repo_root().join("docs/architecture/what-we-guarantee-plain-english.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Every `.rs` file under `crates/*/src`, with its basename.
fn production_sources(root: &Path) -> Vec<PathBuf> {
    crate_sources(root)
        .into_iter()
        .filter(|p| {
            p.strip_prefix(root.join("crates"))
                .ok()
                .and_then(|rel| rel.components().nth(1))
                .is_some_and(|c| c.as_os_str() == "src")
        })
        .collect()
}

#[test]
fn without_struck_self_test() {
    assert_eq!(without_struck("a ~~b~~ c"), "a  c");
    assert_eq!(without_struck("a ~~b\n> c~~ d"), "a  d");
    assert_eq!(
        without_struck("a ~~b"),
        "a ~~b",
        "an open strike hides nothing"
    );
    assert_eq!(without_struck("~~x~~~~y~~z"), "z");
}

/// Regression: 2026-10-11 (audit TRU-1). The operator-facing page said a
/// price update could be lost only if memory, disk and the backup file all
/// failed "at the same instant", and that one tick is processed
/// "end-to-end" in ~5 billionths of a second. The code has five separate,
/// counted loss paths that each need only one step to fail, and the ~5 ns
/// bench times header decode and routing only.
#[test]
fn plain_english_doc_makes_no_all_at_once_loss_claim() {
    let live = without_struck(&plain_english_doc());
    for banned in [
        "all at the same instant",
        "all full at once",
        "tear at once does a fruit drop",
        "end-to-end inside the shop",
    ] {
        assert!(
            !live.contains(banned),
            "what-we-guarantee-plain-english.md still says {banned:?} unstruck. \
             A tick can be lost when ONE save step is full or stopped (spill \
             queue full, spill writer dead, spill byte cap, WAL loss after a \
             shed, rows dropped at the database writer), and the ~5 ns figure \
             is decode + routing only. Strike the claim and add a dated \
             correction."
        );
    }
    // Not vacuous: the corrected page still makes the honest claims.
    assert!(live.contains("CORRECTED 2026-10-11"));
    assert!(live.contains("rings your phone"));
}

/// Regression: 2026-10-11 (audit TRU-6). `guarantees.md` called the
/// tick-drop alarm and `tick_persistence.rs` RETIRED while both were live.
/// A row may say RETIRED only about things that are gone: no metric it names
/// may still be emitted from production source, and no `.rs` file it names
/// may still exist under `crates/*/src`.
#[test]
fn retired_rows_name_nothing_still_live() {
    let root = repo_root();
    let sources = production_sources(&root);
    assert!(
        sources.len() > 100,
        "expected many production sources, found {} — the walker is broken",
        sources.len()
    );
    let haystack: String = sources
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    let basenames: BTreeSet<String> = sources
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();

    let live = without_struck(&doc());
    let mut retired_rows = 0usize;
    let mut offenders = Vec::new();
    for line in live.lines().filter(|l| l.contains("RETIRED")) {
        retired_rows += 1;
        for chunk in line.split('`').skip(1).step_by(2) {
            if chunk.starts_with("tv_") {
                let name: String = chunk
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
                    .collect();
                if haystack.contains(&format!("\"{name}\"")) {
                    offenders.push(format!("metric `{name}` is still emitted"));
                }
            }
            if chunk.ends_with(".rs") {
                let base = chunk.rsplit('/').next().unwrap_or(chunk);
                if basenames.contains(base) {
                    offenders.push(format!("file `{base}` still exists under crates/*/src"));
                }
            }
        }
    }
    assert!(
        retired_rows >= 3,
        "expected several RETIRED rows to check, found {retired_rows}"
    );
    assert!(
        offenders.is_empty(),
        "guarantees.md marks rows RETIRED that name live code:\n  {}\n\n\
         Strike the RETIRED note and add a dated correction naming the live \
         emit site and alarm.",
        offenders.join("\n  ")
    );
}

/// Regression: 2026-10-11 (audit TRU-7). Tier 10 named the alerts
/// `HighWebSocketReconnectRate` and `QuestDbDown`, which exist in no
/// terraform, Rust or workflow file. Every backticked alert in the Tier 10
/// table must be a real CloudWatch alarm, `tv-<env>-X`, whose
/// `tv-${var.environment}-X` appears in some `deploy/aws/terraform/*.tf`.
#[test]
fn tier10_alerts_name_real_alarms() {
    let root = repo_root();
    let tf: String = std::fs::read_dir(root.join("deploy/aws/terraform"))
        .expect("read deploy/aws/terraform")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tf"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();

    let live = without_struck(&doc());
    let start = live
        .find("## Tier 10")
        .expect("guarantees.md has a Tier 10 section");
    let section = &live[start..];
    let end = section[3..].find("\n## ").map_or(section.len(), |i| i + 3);
    let section = &section[..end];

    let mut checked = 0usize;
    let mut bad = Vec::new();
    for row in section.lines().filter(|l| l.starts_with('|')) {
        let cells: Vec<&str> = row
            .split('|')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        let Some(alert) = cells.last() else { continue };
        for token in alert.split('`').skip(1).step_by(2) {
            checked += 1;
            match token.strip_prefix("tv-<env>-") {
                Some(suffix) if tf.contains(&format!("tv-${{var.environment}}-{suffix}\"")) => {}
                Some(_) => bad.push(format!("`{token}` has no matching terraform alarm")),
                None => bad.push(format!("`{token}` is not a `tv-<env>-…` alarm name")),
            }
        }
    }
    assert!(
        checked >= 2,
        "expected Tier 10 to name real alarms, found {checked}"
    );
    assert!(
        bad.is_empty(),
        "guarantees.md Tier 10 names alerts that do not exist:\n  {}",
        bad.join("\n  ")
    );
}

/// Regression: 2026-10-11 (audit TRU-18). The `pipeline` budget row read
/// "Pipeline routing ≤ 100 ns per tick", but the bench behind it loops
/// `dispatch_frame` (header decode + dispatch) and touches none of the drain.
/// While that stays true the row must say it times decode.
#[test]
fn pipeline_budget_row_says_decode_only() {
    let root = repo_root();
    let bench_path = root.join("crates/core/benches/pipeline.rs");
    let bench = std::fs::read_to_string(&bench_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", bench_path.display()));
    let drain_symbols = ["drain_main_feed_frame", "consume_tick", "append_tick"];
    if !bench.contains("dispatch_frame") || drain_symbols.iter().any(|s| bench.contains(s)) {
        // The bench now measures more than decode; this check no longer
        // applies, and the row may describe the wider path.
        return;
    }
    let live = without_struck(&doc());
    let row = live
        .lines()
        .find(|l| l.starts_with('|') && l.contains("100 ns per tick"))
        .expect("guarantees.md Tier 3 carries the 100 ns per tick budget row");
    let claim = row.trim_start_matches('|').trim_start();
    assert!(
        !claim.starts_with("Pipeline routing"),
        "the 100 ns budget row still calls itself pipeline routing: {row}"
    );
    assert!(
        claim.contains("decode"),
        "the 100 ns budget row must say it times packet decode: {row}"
    );
}
