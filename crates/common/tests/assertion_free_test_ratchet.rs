//! Shrink-only ratchet on tests that CANNOT FAIL.
//!
//! ADDED 2026-09-05.
//!
//! # What this catches
//!
//! A `#[test]` whose body contains no assertion of any kind proves exactly one
//! thing: the code under it did not panic. That is sometimes the real property
//! — and where it is, the test's NAME says so (`*_no_panic`,
//! `*_degrades_without_panic`, `*_never_panics`), which is how this ratchet
//! tells the two apart.
//!
//! Where the name promises a check the body never performs, the test is worse
//! than absent: it occupies the slot where a real check would go, and it
//! reports green while doing so. Measured examples found the day this landed
//! and fixed in the same PR:
//!
//! * five CORS tests that ended `let _cors = build_cors_layer(&origins);` —
//!   `CorsLayer` exposes no getter, so they could observe nothing at all, on a
//!   port that is publicly funnelled;
//! * four tests named `..._rejects_zero/negative/nan/infinity` in
//!   `mutation_killer.rs` — the file whose entire purpose is killing mutants —
//!   that pushed a bad mark price and ended in a COMMENT where the assertion
//!   belonged. Deleting the guard they exist to protect left **all four
//!   passing**.
//!
//! # Why a budget rather than fixing all of them
//!
//! There were 152 suspect cases workspace-wide when this landed. Fixing each
//! needs a judgement about what the test MEANT to assert, and a 152-file PR
//! would get one reviewer's attention divided 152 ways. The budget stops the
//! number growing while the backlog is worked down, and it can only shrink —
//! the `assert_eq!` below is exact, so leaving it above the truth fails just
//! as loudly as exceeding it.
//!
//! # Honest limits, stated so nobody mistakes this for more than it is
//!
//! 1. It counts SYNTAX, not meaning. A test with `assert!(true)` passes here.
//! 2. Body extraction is indentation-based, which is safe only because
//!    `cargo fmt --check` gates every PR. It cannot see tests emitted by a
//!    `macro_rules!`.
//! 3. `.unwrap()` and `.expect()` COUNT as assertions — they fail the test on
//!    `Err`/`None`, which is a real check even if a blunt one.
//! 4. An assertion can be SYNTACTICALLY present and SEMANTICALLY vacuous, and
//!    this scanner cannot tell. Found by bite-proof on 2026-09-05:
//!    `assert!(snapshot.vwap.is_finite())` on an `IndicatorSnapshot` can NEVER
//!    fail, because `IndicatorSnapshot::sanitize_nan_inf` clamps every
//!    non-finite field to `0.0` before the caller sees it. Two tests were
//!    written that way, counted as asserting, and passed with the guard they
//!    existed to protect deleted. The rule that follows: on any value that
//!    passes through a sanitizer, assert the VALUE (`> 0.0`, `== 104.0`), not
//!    its finiteness — `0.0` is what a sanitized NaN looks like, so zero is
//!    the failure signal.
use std::path::{Path, PathBuf};

use tickvault_common::source_scan::strip_rust_comments;

/// How many assertion-free tests whose name does NOT advertise "did not panic"
/// may still exist.
///
/// MEASURED 2026-09-05 by this scanner on the tree it shipped with. It moves
/// DOWN as the backlog is worked and never up: raising it is how a ratchet
/// stops ratcheting. If a legitimate new test genuinely proves only absence of
/// a panic, NAME it so (`..._no_panic`, `..._never_panics`,
/// `..._degrades_without_panic`) and this scanner will not count it — which is
/// the point: the name is what tells the next reader what was proven.
const ASSERTION_FREE_BUDGET: usize = 183;

/// Substrings whose presence means the body asserts something.
const ASSERTION_MARKERS: [&str; 12] = [
    "assert!",
    "assert_eq!",
    "assert_ne!",
    "assert_matches!",
    "debug_assert",
    ".unwrap()",
    ".expect(",
    "panic!",
    "unreachable!",
    "todo!",
    "unimplemented!",
    "assert_",
];

/// A test whose NAME says its property is "it did not panic" is legitimately
/// assertion-free and is not counted.
const NO_PANIC_NAME_MARKERS: [&str; 8] = [
    "no_panic",
    "not_panic",
    "never_panic",
    "doesnt_panic",
    "does_not_panic",
    "without_panic",
    "panic_safety",
    "_is_noop",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolvable")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Returns `(test_name, is_assertion_free)` for every `#[test]` /
/// `#[tokio::test]` in `src`, which must already be comment-stripped.
pub(crate) fn classify_tests(src: &str) -> Vec<(String, bool)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t != "#[test]" && t != "#[tokio::test]" {
            continue;
        }
        // Walk forward to the `fn` line, tolerating further attributes.
        let mut j = i + 1;
        let mut fn_line = None;
        // A `#[should_panic]` sits BETWEEN `#[test]` and `fn`, so it never
        // reaches the body string. It is a real assertion — the strongest one
        // in the file when it carries `expected = ".."` — and a scanner that
        // counted such a test as assertion-free would open with a false
        // positive, which is how a guard gets allowlisted instead of obeyed.
        let mut should_panic = false;
        while j < lines.len() && j <= i + 15 {
            let ft = lines[j].trim();
            if ft.starts_with("fn ") || ft.starts_with("async fn ") {
                fn_line = Some(j);
                break;
            }
            if ft.starts_with("#") && ft.contains("should_panic") {
                should_panic = true;
            }
            if !ft.starts_with('#') && !ft.is_empty() {
                break;
            }
            j += 1;
        }
        let Some(start) = fn_line else { continue };

        let raw = lines[start];
        let indent = raw.len() - raw.trim_start().len();
        let after_fn = raw
            .trim_start()
            .trim_start_matches("async ")
            .trim_start_matches("fn ");
        let name: String = after_fn
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }

        // Body runs to the first line that is exactly `<indent>}`.
        let closer = format!("{}}}", " ".repeat(indent));
        let mut body = String::new();
        let mut k = start + 1;
        let mut terminated = false;
        while k < lines.len() {
            if lines[k] == closer {
                terminated = true;
                break;
            }
            body.push_str(lines[k]);
            body.push('\n');
            k += 1;
        }
        if !terminated {
            // Unterminated means the extraction is unreliable for this one;
            // treat it as asserting rather than manufacture a finding.
            continue;
        }

        let asserts = should_panic
            || ASSERTION_MARKERS.iter().any(|m| body.contains(m))
            || body.contains("?;");
        out.push((name, !asserts));
    }
    out
}

fn name_advertises_no_panic(name: &str) -> bool {
    NO_PANIC_NAME_MARKERS.iter().any(|m| name.contains(m))
}

#[test]
fn assertion_free_tests_may_only_shrink() {
    let root = repo_root();
    let crates_dir = root.join("crates");
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("cannot read {crates_dir:?}: {e}"))
        .flatten()
    {
        let p = entry.path();
        for sub in ["src", "tests"] {
            let d = p.join(sub);
            if d.is_dir() {
                dirs.push(d);
            }
        }
    }
    assert!(
        dirs.len() >= 8,
        "discovery found only {} crate source/test dir(s) — the scan would \
         enforce almost nothing",
        dirs.len()
    );

    let mut files = Vec::new();
    for d in &dirs {
        rust_files(d, &mut files);
    }
    assert!(
        files.len() > 200,
        "scanned only {} .rs file(s) — the walk collapsed. This assert exists \
         because sibling guards in this repo have twice passed while reading \
         almost nothing.",
        files.len()
    );

    let mut total_tests = 0usize;
    let mut offenders: Vec<String> = Vec::new();
    for path in &files {
        let body =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {path:?}: {e}"));
        let stripped = strip_rust_comments(&body);
        for (name, assertion_free) in classify_tests(&stripped) {
            total_tests += 1;
            if assertion_free && !name_advertises_no_panic(&name) {
                offenders.push(format!("  {}::{name}", path.display()));
            }
        }
    }

    assert!(
        total_tests > 2000,
        "found only {total_tests} tests — the classifier stopped matching"
    );

    let count = offenders.len();
    offenders.sort();

    assert!(
        count <= ASSERTION_FREE_BUDGET,
        "{count} test(s) assert NOTHING and their names do not say so, over \
         the budget of {ASSERTION_FREE_BUDGET}.\n\n\
         A test with no assertion proves only that the code did not panic. \
         Where that IS the property, say so in the name (`..._no_panic`, \
         `..._never_panics`, `..._degrades_without_panic`) and this ratchet \
         will not count it. Where it is not, the test occupies the slot a real \
         check would fill while reporting green — four tests named \
         `..._rejects_zero/negative/nan/infinity` passed with the guard they \
         protect DELETED.\n\n\
         Do NOT raise the budget.\n\nTests:\n{}",
        offenders.join("\n")
    );

    assert_eq!(
        count, ASSERTION_FREE_BUDGET,
        "the budget is {ASSERTION_FREE_BUDGET} but only {count} assertion-free \
         test(s) remain. Lower it to {count} in this same change — a ratchet \
         allowed to sit above the truth is a ceiling somebody padded once, and \
         it stops ratcheting the moment it does."
    );
}

/// Scanner self-test: the classifier must call an asserting test asserting and
/// an empty one assertion-free, and must not be fooled by a `#[test]` that
/// appears only inside a comment (comments are stripped before it runs).
#[test]
fn classifier_self_test() {
    let src = "\
mod t {
    #[test]
    fn asserts_something() {
        assert_eq!(1, 1);
    }

    #[test]
    fn asserts_nothing() {
        let _x = f();
    }

    #[tokio::test]
    async fn unwrap_counts_as_an_assertion() {
        f().await.unwrap();
    }

    #[test]
    #[should_panic(expected = \"must be > 0\")]
    fn should_panic_is_an_assertion() {
        let _ = f(0);
    }
}
";
    let got = classify_tests(src);
    assert_eq!(
        got,
        vec![
            ("asserts_something".to_string(), false),
            ("asserts_nothing".to_string(), true),
            ("unwrap_counts_as_an_assertion".to_string(), false),
            ("should_panic_is_an_assertion".to_string(), false),
        ],
        "classifier must distinguish asserting from assertion-free, and must \
         treat .unwrap() and a #[should_panic] attribute as assertions"
    );

    assert!(
        name_advertises_no_panic("open_all_dashboards_no_panic"),
        "a name that advertises the no-panic property must be exempt"
    );
    assert!(
        !name_advertises_no_panic("test_build_router_with_custom_origins"),
        "a name that promises a behaviour must NOT be exempt"
    );
}
