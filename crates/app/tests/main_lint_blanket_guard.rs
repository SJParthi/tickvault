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

/// Every crate root in the workspace: `lib.rs`, `main.rs`, and each
/// `src/bin/*.rs`.
///
/// Discovered, never listed. A hardcoded list is how this guard spent a
/// year covering ONE file: it pinned `crates/app/src/main.rs` by name while
/// twenty-five other roots — nine lambda binaries, three CLI tools, seven
/// libraries and a second `main.rs` — went unchecked. The rule was never
/// app-specific; its SCOPE was. Exactly the shape found in
/// `dashboard_live_lane_visibility_guard` a day earlier, which held for one
/// metric prefix and drifted silently for the other eight.
fn crate_roots() -> Vec<String> {
    let root = workspace_root();
    let mut out = Vec::new();
    let Ok(crates) = fs::read_dir(root.join("crates")) else {
        return out;
    };
    for c in crates.flatten() {
        let src = c.path().join("src");
        for name in ["lib.rs", "main.rs"] {
            if src.join(name).is_file() {
                out.push(format!(
                    "crates/{}/src/{name}",
                    c.file_name().to_string_lossy()
                ));
            }
        }
        if let Ok(bins) = fs::read_dir(src.join("bin")) {
            for b in bins.flatten() {
                if b.path().extension().is_some_and(|e| e == "rs") {
                    out.push(format!(
                        "crates/{}/src/bin/{}",
                        c.file_name().to_string_lossy(),
                        b.file_name().to_string_lossy()
                    ));
                }
            }
        }
    }
    out.sort();
    out
}

/// EVERY crate root must make an EXPLICIT decision about silent panics.
///
/// `deny` is the default posture. `allow` with an `// APPROVED:` reason is
/// legitimate for a diagnostic CLI whose whole job is to fall over loudly
/// on a broken workspace — `tv_doctor`, `tv_guarantees` and `smoke_test`
/// all carry one, and that is a decision on the record.
///
/// What fails is SILENCE. A root that says nothing has not chosen the
/// permissive posture; it has failed to choose at all, and the compiler
/// resolves that to "allowed" without anyone noticing. On 2026-08-26 one
/// root was in exactly that state — `crates/tickvault-logs-mcp/src/main.rs`,
/// which this guard could not see because it was looking at one hardcoded
/// path. It held zero unwraps, so nothing was broken; what was missing was
/// the thing that would have said so if a future edit added one.
#[test]
fn every_crate_root_decides_about_silent_panics() {
    let mut silent = Vec::new();

    for rel in crate_roots() {
        let src = strip_line_comments(&read(&rel));
        for lint in ["clippy::unwrap_used", "clippy::expect_used"] {
            let denied = src.contains(&format!("deny({lint})"));
            let allowed = src.contains(&format!("allow({lint})"));
            if !denied && !allowed {
                silent.push(format!("{rel}: says nothing about {lint}"));
            }
        }
    }

    assert!(
        silent.is_empty(),
        "a crate root that says nothing about unwrap/expect has not chosen the permissive \
         posture — it has failed to choose, and the compiler reads silence as permission.\n\
         Add the deny blanket, or `#![allow(...)]` with an `// APPROVED: <reason>` comment \
         so the decision is on the record.\n\n{}",
        silent.join("\n")
    );
}

/// The DEFAULT posture is deny, and the crates carrying production code
/// must keep it. A diagnostic CLI may opt out with a reason; a library or a
/// service binary may not.
#[test]
fn production_crate_roots_keep_the_deny_blanket() {
    // Every root that is NOT a hand-run diagnostic tool.
    let exempt = ["tv_doctor.rs", "tv_guarantees.rs", "smoke_test.rs"];
    let mut weakened = Vec::new();

    for rel in crate_roots() {
        if exempt.iter().any(|e| rel.ends_with(e)) {
            continue;
        }
        let src = strip_line_comments(&read(&rel));
        for needle in [UNWRAP_ATTR, EXPECT_ATTR] {
            if !src.contains(needle) {
                weakened.push(format!("{rel}: missing `{needle}`"));
            }
        }
    }

    assert!(
        weakened.is_empty(),
        "these roots carry production code and must deny silent panics. A release build \
         sets `panic = \"abort\"`, so an `unwrap` on a None is not an error — it ends the \
         trading session.\n\n{}",
        weakened.join("\n")
    );
}

/// `crates/app` specifically: `main.rs` and `lib.rs` stay in lockstep on
/// all THREE attributes including the print/dbg blanket.
///
/// The original 2026-07-17 pin, kept verbatim in substance: the boot
/// sequence lives directly in `main.rs`, so a stray `println!` there is an
/// operator-facing surface that bypasses the structured log sink entirely.
#[test]
fn test_main_rs_carries_lib_restriction_lint_blanket() {
    for rel in ["crates/app/src/main.rs", "crates/app/src/lib.rs"] {
        let src = strip_line_comments(&read(rel));
        for needle in [UNWRAP_ATTR, EXPECT_ATTR, PRINT_DBG_ATTR] {
            assert!(
                src.contains(needle),
                "{rel} MUST carry the restriction-lint attribute `{needle}` — the two app \
                 crate roots stay in lockstep. See main_lint_blanket_guard.rs."
            );
        }
    }
}

#[test]
fn guard_self_test_discovers_more_than_one_root() {
    let roots = crate_roots();
    assert!(
        roots.len() > 20,
        "discovery is the point of this guard: it found only {} root(s), which means the \
         walk broke and every assertion below it passes vacuously",
        roots.len()
    );
    assert!(roots.iter().any(|r| r == "crates/app/src/main.rs"));
    assert!(roots.iter().any(|r| r.contains("aws-lambdas/src/bin/")));
    assert!(
        roots
            .iter()
            .any(|r| r == "crates/tickvault-logs-mcp/src/main.rs"),
        "the root this guard could not see until 2026-08-26 must be in the set"
    );
}
