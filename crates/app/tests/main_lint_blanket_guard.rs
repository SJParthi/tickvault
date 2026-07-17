//! Source-scan ratchet — the `main` binary crate root carries the SAME
//! restriction-lint deny blanket as `crates/app/src/lib.rs`.
//!
//! **The gap this pins closed (confirmed-LOW, 2026-07-17):** `lib.rs`
//! denies `clippy::unwrap_used` / `clippy::expect_used` (both
//! `cfg_attr(not(test))`) and `clippy::print_stdout` / `print_stderr` /
//! `dbg_macro` at its crate root — but the SEPARATE `main` bin crate root
//! (`crates/app/src/main.rs`) is its own compilation unit and did NOT
//! inherit those attributes. Production code living directly in `main.rs`
//! (the boot sequence) was therefore un-linted for the exact
//! silent-panic / stray-print class the lib blanket exists to forbid. The
//! three inner attributes are now present in `main.rs`; this ratchet fails
//! the build if any of them is removed.
//!
//! Comment lines (`//` / `///`) are stripped before matching so a
//! commented-out attribute can never vacuously satisfy a pin (house
//! pattern of `systemd_boot_notify_guard.rs`). `#`-prefixed lines are
//! deliberately NOT stripped here — in Rust source `#![...]` / `#[...]`
//! ARE the code being pinned, not comments.

use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/app") // APPROVED: test
}

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Strip only `//`-comment lines (incl. `///` docs) so every needle must
/// match a real Rust attribute, never prose. `#![...]` attribute lines are
/// preserved (they start with `#`, not `//`).
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Non-vacuity self-test: a commented-out attribute must NOT survive into
/// the scanned text, but a real `#![...]` attribute line must.
#[test]
fn test_comment_stripper_removes_commented_attrs_keeps_real_ones() {
    let sample = concat!(
        "// #![cfg_attr(not(test), deny(clippy::unwrap_used))]\n",
        "/// #![deny(clippy::print_stdout)]\n",
        "#![cfg_attr(not(test), deny(clippy::expect_used))]\n",
    );
    let stripped = strip_line_comments(sample);
    assert!(
        !stripped.contains("unwrap_used"),
        "commented-out unwrap_used attr must be stripped"
    );
    assert!(
        !stripped.contains("print_stdout"),
        "commented-out print_stdout doc attr must be stripped"
    );
    assert!(
        stripped.contains("deny(clippy::expect_used)"),
        "real #![...] attribute line must survive the stripper"
    );
}

const UNWRAP_ATTR: &str = "#![cfg_attr(not(test), deny(clippy::unwrap_used))]";
const EXPECT_ATTR: &str = "#![cfg_attr(not(test), deny(clippy::expect_used))]";
const PRINT_DBG_ATTR: &str =
    "#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]";

/// The `main` binary crate root MUST carry the three restriction-lint
/// deny attributes that `lib.rs` carries, so production boot-sequence code
/// in `main.rs` is linted for the same silent-panic / stray-print class.
#[test]
fn test_main_rs_carries_lib_restriction_lint_blanket() {
    let main_rs = strip_line_comments(&read("crates/app/src/main.rs"));

    for needle in [UNWRAP_ATTR, EXPECT_ATTR, PRINT_DBG_ATTR] {
        assert!(
            main_rs.contains(needle),
            "crates/app/src/main.rs MUST carry the restriction-lint attribute `{needle}` \
             (the same deny blanket as lib.rs) — production code in the bin crate root \
             must be linted for unwrap/expect/print/dbg. See main_lint_blanket_guard.rs."
        );
    }
}

/// The blanket must be REAL in `lib.rs` too (so the two roots stay in
/// lockstep — a future PR that weakens lib.rs would otherwise silently
/// diverge the two crate roots).
#[test]
fn test_lib_rs_still_carries_the_same_blanket() {
    let lib_rs = strip_line_comments(&read("crates/app/src/lib.rs"));

    for needle in [UNWRAP_ATTR, EXPECT_ATTR, PRINT_DBG_ATTR] {
        assert!(
            lib_rs.contains(needle),
            "crates/app/src/lib.rs MUST keep the restriction-lint attribute `{needle}` — \
             main.rs mirrors it; the two crate roots stay in lockstep."
        );
    }
}
