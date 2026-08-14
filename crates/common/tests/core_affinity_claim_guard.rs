//! Guard: CPU-pinning must be WIRED or ABSENT — never declared-but-dead.
//!
//! ## Why this exists
//!
//! From Wave 5 (2026-05-01) until 2026-08-10 the `core_affinity` crate was
//! declared in BOTH the root `Cargo.toml` and `crates/app/Cargo.toml`, and the
//! operator charter's resilience matrix advertised **"core_affinity Core 0"** as
//! a delivered guarantee under "Never slow/locked/hanged".
//!
//! It had **zero call sites**. No thread was ever pinned to any core. The only
//! occurrence of the name anywhere in `crates/*/src` was inside a comment.
//!
//! That is the most dangerous shape a guarantee can take: a dependency in the
//! manifest makes it *look* implemented to anyone auditing the build, and the
//! charter row makes it look *verified* to anyone auditing the guarantees — so
//! neither reader goes looking for the call site that does not exist.
//!
//! ## What this guard enforces
//!
//! The dependency and a real call site must rise and fall together:
//!
//! - declared in a manifest **and** called in source → OK (pinning is real)
//! - declared in **no** manifest and called nowhere    → OK (honestly absent)
//! - declared but never called                         → **FAIL** (the dead claim)
//!
//! The reverse case (called but not declared) cannot occur — it fails to compile
//! long before this test runs — so it is deliberately not asserted here.
//!
//! ## If you are re-introducing CPU pinning
//!
//! Add the dependency and the call site in the SAME PR, and do not pin to core 0:
//! that core services network interrupts, so pinning a decode thread there puts
//! it in contention with the softirq work it depends on. Also confirm the
//! instance actually has cores to spare — the box was t4g.medium (2 vCPU) when
//! this guard was written, where pinning buys little and can cost.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The crate whose wiring this guard tracks.
const PINNING_CRATE: &str = "core_affinity";

fn repo_root() -> PathBuf {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("core_affinity_claim_guard: `git rev-parse` failed (needs a git checkout)");
    assert!(
        out.status.success(),
        "core_affinity_claim_guard: not a git checkout"
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Tracked files matching the pathspecs, NUL-safe so odd paths cannot slip through.
fn git_ls(root: &Path, pathspecs: &[&str]) -> Vec<String> {
    let out = Command::new("git")
        .arg("ls-files")
        .arg("-z")
        .arg("--")
        .args(pathspecs)
        .current_dir(root)
        .output()
        .expect("core_affinity_claim_guard: `git ls-files` failed");
    assert!(
        out.status.success(),
        "core_affinity_claim_guard: `git ls-files` failed"
    );
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Strip `//` line comments so a comment mentioning the crate is never mistaken
/// for a call site. This is the exact trap that hid the dead claim for months:
/// `crates/common/src/error_code.rs` carries the name inside a comment.
///
/// Deliberately simple: `//` outside a string literal starts a comment. Rust
/// string literals containing `//` (e.g. a URL) would be over-trimmed, which is
/// SAFE here — over-trimming can only cause a false FAIL that a human then
/// inspects, never a false PASS that hides a dead dependency.
fn strip_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True iff any manifest declares the pinning crate.
fn declared_in_any_manifest(root: &Path) -> Vec<String> {
    git_ls(root, &["Cargo.toml", "crates/*/Cargo.toml"])
        .into_iter()
        .filter(|rel| {
            std::fs::read_to_string(root.join(rel))
                .map(|body| {
                    strip_line_comments(&body)
                        .lines()
                        .any(|l| l.trim_start().starts_with(PINNING_CRATE))
                })
                .unwrap_or(false)
        })
        .collect()
}

/// Files under `crates/*/src` that actually reference the crate in real code.
fn call_sites(root: &Path) -> Vec<String> {
    git_ls(root, &["crates/*/src/*.rs", "crates/*/src/**/*.rs"])
        .into_iter()
        .filter(|rel| {
            std::fs::read_to_string(root.join(rel))
                .map(|body| strip_line_comments(&body).contains(PINNING_CRATE))
                .unwrap_or(false)
        })
        .collect()
}

#[test]
fn core_affinity_is_wired_or_absent_never_declared_but_dead() {
    let root = repo_root();
    let manifests = declared_in_any_manifest(&root);
    let sites = call_sites(&root);

    assert!(
        !(!manifests.is_empty() && sites.is_empty()),
        "DEAD CPU-PINNING CLAIM: `{PINNING_CRATE}` is declared in {manifests:?} but has \
         ZERO call sites in any crate's src. This is exactly the state the repo sat in \
         from 2026-05-01 to 2026-08-10, while the operator charter advertised \
         \"core_affinity Core 0\" as a delivered guarantee — a dependency in the manifest \
         makes it look implemented, so nobody went looking for the call site that did not \
         exist.\n\n\
         Fix it in ONE of two ways, in this PR:\n\
         (a) add the real call site that pins a thread — NOT to core 0, which services \
         network interrupts and would contend with the very softirq work a decode thread \
         depends on; or\n\
         (b) remove the dependency from the manifest(s) above.\n\n\
         Do not silence this test."
    );
}

#[test]
fn guard_ignores_comment_only_mentions() {
    // The failure this guard was built for hinged on a comment mention being
    // mistaken for real usage. Prove the stripper handles it.
    let commented = "// we removed core_affinity pinning here\nlet x = 1;\n";
    assert!(
        !strip_line_comments(commented).contains(PINNING_CRATE),
        "a comment mentioning the crate must NOT count as a call site"
    );

    let real = "let ids = core_affinity::get_core_ids();\n";
    assert!(
        strip_line_comments(real).contains(PINNING_CRATE),
        "real code referencing the crate MUST count as a call site"
    );

    // Trailing comment after real code: the code half must survive.
    let mixed = "core_affinity::set_for_current(id); // pin the decode thread\n";
    assert!(
        strip_line_comments(mixed).contains(PINNING_CRATE),
        "a call site with a trailing comment must still count"
    );
}

#[test]
fn guard_self_test_manifest_detection_is_prefix_anchored() {
    // A dependency line starts at the beginning of the (trimmed) line. This
    // prevents a substring elsewhere in a manifest from reading as a
    // declaration and producing a false FAIL.
    let decl = "core_affinity = \"0.8.3\"";
    assert!(decl.trim_start().starts_with(PINNING_CRATE));

    let not_decl = "# core_affinity REMOVED 2026-08-10 — see the charter note";
    assert!(
        !not_decl.trim_start().starts_with(PINNING_CRATE),
        "a comment line in a manifest must not read as a declaration"
    );
}
