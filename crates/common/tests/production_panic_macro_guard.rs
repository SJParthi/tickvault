//! A panic macro on a production path is a process ABORT, not an error.
//!
//! The release profile sets `panic = "abort"`, so `unreachable!()`,
//! `panic!()`, `todo!()` and `unimplemented!()` do not unwind, do not log,
//! and do not degrade one lane. They end the trading session.
//!
//! Every one of them is written for a case the author believed impossible.
//! That belief comes in two very different strengths, and the difference is
//! the whole reason this guard exists:
//!
//! - **Compiler-guaranteed** — an outer pattern or a `const` evaluation
//!   makes the arm structurally unreachable. A bad edit is a build error.
//! - **Caller convention** — a comment asserts a value never arrives. The
//!   type system does not agree, and a refactor, a new enum variant or a
//!   test helper can prove it wrong at 09:15.
//!
//! On 2026-08-26 the cadence runner held five of the SECOND kind (`Feed`
//! is a plain enum; `Completion.lane` carries any variant) and the seal
//! ring held one more. All six are now fail-closed. This guard stops the
//! next one arriving unannounced.
//!
//! It is a SHRINKING allowlist, keyed by FILE and not by line: line numbers
//! rot on the first edit above them, and this repository has a documented
//! history of exactly that. Adding a site to a file already listed fails
//! the build; removing the last site from a listed file also fails it, so
//! the allowlist can never outlive what it excuses.

use std::path::{Path, PathBuf};

/// Files permitted to hold production panic macros, with the count each may
/// hold and the reason it is compiler-guaranteed rather than convention.
///
/// The first two are the FIRST kind — neither is an exception granted on
/// a promise, each is one the compiler actually keeps. The third is not
/// production code at all and says so.
const ALLOWLIST: &[(&str, usize, &str)] = &[
    (
        "crates/app/src/dhan_feed_stack.rs",
        1,
        "the enclosing arm binds `Ok(parsed @ (ParsedFrame::Tick(_) | \
         ParsedFrame::TickWithDepth(..)))`, so a new ParsedFrame variant \
         cannot reach the inner match at all — the pattern, not a comment, \
         is what excludes it",
    ),
    (
        "crates/tickvault-logs-mcp/src/tools.rs",
        1,
        "inside a `const` initializer: `core::str::from_utf8` on six ASCII \
         bytes is evaluated at COMPILE time, so a bad edit is a build error \
         and there is no runtime path to the panic",
    ),
    (
        "crates/storage/src/ws_frame_spill.rs",
        1,
        "a `#[cfg(test)]` fn — deliberate panic injection that exercises the \
         spill supervisor's catch-and-respawn path (WS-SPILL-01). It is not \
         compiled into a release binary at all; it is listed rather than \
         skipped because it sits ABOVE `mod tests`, and a boundary loose \
         enough to skip it would stop counting the rest of the file",
    ),
];

/// Files whose panic macros are not production code at all.
///
/// `tv_guarantees.rs` names these macros as SEARCH NEEDLES — it is the
/// binary that counts them, and a scanner that counts its own needles
/// reports a number about itself. `strategy/tests.rs` is a test module that
/// happens to live in `src/`.
const NOT_PRODUCTION: &[&str] = &["tv_guarantees.rs", "tests.rs"];

const NEEDLES: [&str; 4] = ["unreachable!(", "panic!(", "todo!(", "unimplemented!("];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common has a grandparent")
        .to_path_buf()
}

/// Count panic macros in a file's PRODUCTION half.
///
/// Stops at `mod tests` — this repository puts the test module last, and a
/// test asserting on a panic is not a production abort. Skips `//`-comment
/// lines too, because a doc comment that merely NAMES the macro
/// (`exit_rules.rs` explains it never uses one) is prose, not a code path.
///
/// The boundary is deliberately `mod tests` and NOT the first `#[cfg(test)]`:
/// that attribute also marks scattered test-only HELPERS, so truncating at
/// the first one silently stops counting the rest of the file. Over-counting
/// a risk is the safe direction; a `#[cfg(test)]` helper above `mod tests`
/// earns an explicit allowlist entry instead of a blind spot.
fn production_panic_sites(text: &str) -> usize {
    text.lines()
        .take_while(|l| !l.trim_start().starts_with("mod tests"))
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| NEEDLES.iter().any(|n| l.contains(n)))
        .count()
}

fn tracked_rust_sources(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    for crate_dir in std::fs::read_dir(root.join("crates"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let src = crate_dir.path().join("src");
        if src.is_dir() {
            walk(&src, &mut out);
        }
    }
    out.sort();
    out
}

#[test]
fn no_new_panic_macro_on_a_production_path() {
    let root = repo_root();
    let mut offenders = Vec::new();

    for path in tracked_rust_sources(&root) {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| NOT_PRODUCTION.contains(&n))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let found = production_panic_sites(&text);
        if found == 0 {
            continue;
        }
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        match ALLOWLIST.iter().find(|(f, _, _)| *f == rel) {
            Some((_, allowed, _)) if found <= *allowed => {}
            Some((_, allowed, _)) => offenders.push(format!(
                "{rel}: {found} panic macros, allowed {allowed} — a NEW one was added \
                 to an already-excused file"
            )),
            None => offenders.push(format!(
                "{rel}: {found} panic macro(s) on a production path, not allowlisted"
            )),
        }
    }

    assert!(
        offenders.is_empty(),
        "panic macros on production paths are process ABORTS under `panic = \"abort\"`.\n\
         Make the case fail CLOSED instead — count it, log it with a code, and drop the \
         one unit of work — so the out-of-box case costs one message rather than the \
         trading session.\n\
         If the arm is genuinely compiler-guaranteed (an outer pattern excludes it, or it \
         is `const`-evaluated), add it to ALLOWLIST with the reason the COMPILER keeps it \
         — never a comment that merely promises it.\n\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_allowlist_cannot_outlive_what_it_excuses() {
    let root = repo_root();
    let mut stale = Vec::new();

    for (rel, allowed, _) in ALLOWLIST {
        let path = root.join(rel);
        let Ok(text) = std::fs::read_to_string(&path) else {
            stale.push(format!("{rel}: allowlisted but the file no longer exists"));
            continue;
        };
        let found = production_panic_sites(&text);
        if found == 0 {
            stale.push(format!(
                "{rel}: allowlisted for {allowed} but has NONE left — delete the entry"
            ));
        } else if found < *allowed {
            stale.push(format!(
                "{rel}: allowlisted for {allowed} but has {found} — lower the entry"
            ));
        }
    }

    assert!(
        stale.is_empty(),
        "the allowlist may only SHRINK. A stale entry silently re-opens the budget it \
         was granted, which is how every allowlist in this repository has drifted.\n\n{}",
        stale.join("\n")
    );
}

#[test]
fn guard_self_test_distinguishes_code_from_prose_and_tests() {
    // A real site counts.
    assert_eq!(production_panic_sites("fn f() { unreachable!() }"), 1);
    // Prose naming the macro does not — `exit_rules.rs` explains at length
    // that it never uses one, and flagging that would teach the reader the
    // cheapest fix is an allowlist entry.
    assert_eq!(
        production_panic_sites("//! never `unreachable!()` and never a panic."),
        0
    );
    // A trailing test-module panic is not a production abort.
    assert_eq!(
        production_panic_sites("fn f() {}\n#[cfg(test)]\nmod tests { fn g() { panic!() } }"),
        0
    );
    // All four macros are seen.
    assert_eq!(
        production_panic_sites("todo!();\nunimplemented!();\npanic!();\nunreachable!();"),
        4
    );
    // BITE: the shape this guard was written for — a caller-convention arm
    // on a plain enum, exactly what the cadence runner held.
    assert_eq!(
        production_panic_sites(
            "match feed {\n  Feed::Dhan => a,\n  // never happens\n  \
             Feed::Truedata => unreachable!(\"no lane\"),\n}"
        ),
        1,
        "the guard must see the arm it exists for"
    );
}
