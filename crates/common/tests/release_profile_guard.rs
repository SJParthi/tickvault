//! The release profile is a safety property, and nothing was checking it.
//!
//! Found 2026-08-22. `CLAUDE.md` states all five release settings as fact, and
//! **18 production files reason about `panic = "abort"` in their own comments**
//! — their correctness arguments depend on it. No test asserted any of it.
//!
//! Two of the five are not build tuning; they change what the program *does*
//! when something goes wrong:
//!
//! | Setting | If it silently changed | Why that is worse than it sounds |
//! |---|---|---|
//! | `overflow-checks = true` | integer overflow **wraps silently** instead of panicking | This is financial arithmetic. A wrapped quantity or price is not a crash you can see — it is a wrong number that persists, prices a position, and is written to the audit table as if it were real. |
//! | `panic = "abort"` | a panicking task **unwinds** instead of killing the process | Eighteen files argue their own safety from this. Several say, in as many words, that a panicked bring-up task aborts the whole process so a half-initialised lane cannot keep running. Flip it and those arguments are quietly false while still being written down. |
//!
//! Neither failure announces itself. That is the whole reason to pin them: a
//! release setting is exactly the kind of fact that is true until someone
//! optimises a build and nobody notices which invariant they spent.
//!
//! The other three (`lto`, `codegen-units`, `strip`) are performance and size
//! choices. They are pinned too, but only so a change is deliberate and shows
//! up in a diff — not because a change would be unsafe.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common -> crates -> repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The `[profile.release]` block body, comments stripped.
fn release_profile_block() -> String {
    let toml = read("Cargo.toml");
    let start = toml
        .find("\n[profile.release]")
        .expect("Cargo.toml has no [profile.release] section");
    let body = &toml[start + 1..];
    let end = body[1..].find("\n[").map(|i| i + 1).unwrap_or(body.len());
    body[..end]
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every setting that must hold, with why it matters when it does not.
const PINNED: &[(&str, &str)] = &[
    (
        "overflow-checks = true",
        "integer overflow would WRAP SILENTLY instead of panicking. This is \
         financial arithmetic: a wrapped price or quantity is not a visible \
         crash, it is a wrong number that gets acted on and audited as real.",
    ),
    (
        "panic = \"abort\"",
        "18 production files argue their own safety from abort-on-panic — that \
         a panicked task kills the process rather than leaving a half-built \
         lane running. Unwinding makes every one of those arguments quietly \
         false while they stay written in the source.",
    ),
    (
        "lto = \"thin\"",
        "documented in CLAUDE.md as the shipped profile",
    ),
    (
        "codegen-units = 1",
        "documented in CLAUDE.md as the shipped profile",
    ),
    (
        "strip = \"symbols\"",
        "documented in CLAUDE.md as the shipped profile",
    ),
];

#[test]
fn the_release_profile_carries_every_setting_claude_md_promises() {
    let block = release_profile_block();
    for (setting, why) in PINNED {
        assert!(
            block.contains(setting),
            "[profile.release] no longer sets `{setting}`.\n\nWhy this matters: {why}\n\n\
             Current block:\n{block}"
        );
    }
}

#[test]
fn nothing_overrides_the_release_profile_back_down() {
    let toml = read("Cargo.toml");
    // A per-package or build-override table can weaken the parent profile for
    // some crates only -- which is strictly worse than turning it off outright,
    // because the top-level block still reads as if it holds.
    for bad in [
        "[profile.release.package",
        "[profile.release.build-override]",
    ] {
        assert!(
            !toml.contains(bad),
            "Cargo.toml carries `{bad}`, which can weaken [profile.release] for \
             part of the build while the top-level block still reads as if it \
             holds everywhere. If this is deliberate, this guard must say so \
             explicitly rather than be deleted."
        );
    }
}

#[test]
fn the_shipped_binary_is_built_with_the_release_profile() {
    let wf = read(".github/workflows/deploy-aws.yml");
    assert!(
        wf.contains("cargo build --release") && wf.contains("--bin tickvault"),
        "the deploy workflow no longer builds the app binary with --release, so \
         the pinned [profile.release] settings would not reach the box at all"
    );
}

#[test]
fn claude_md_and_cargo_toml_agree_on_the_profile() {
    let claude = read("CLAUDE.md");
    let line = claude
        .lines()
        .find(|l| l.contains("Release profile:"))
        .expect("CLAUDE.md no longer documents the release profile");
    for (setting, _) in PINNED {
        // CLAUDE.md renders them inside backticks, without the spaces around `=`
        // that Cargo.toml uses.
        let compact = setting.replace(' ', "");
        let doc_compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            doc_compact.contains(&compact),
            "CLAUDE.md's release-profile line does not mention `{setting}`, so \
             the document and the build disagree.\nLine: {line}"
        );
    }
}

#[test]
fn the_abort_on_panic_reasoning_is_not_orphaned() {
    // Not a threshold -- the point is to NAME what breaks if the setting goes.
    // A count here would be a nuisance test that fails whenever a file is added.
    let mut dependents = Vec::new();
    let mut stack = vec![repo_root().join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if !matches!(n, "target" | "tests" | "benches" | "fuzz") {
                    stack.push(p);
                }
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) == Some("rs")
                && let Ok(body) = std::fs::read_to_string(&p)
                && body.contains("panic = \"abort\"")
            {
                dependents.push(p);
            }
        }
    }
    assert!(
        !dependents.is_empty(),
        "no production file references abort-on-panic any more. Either the \
         reasoning was removed (fine — retire this test in the same change) or \
         the scanner broke (not fine — it would pass vacuously forever)."
    );
    assert!(
        release_profile_block().contains("panic = \"abort\""),
        "{} production files argue their own safety from abort-on-panic, and \
         [profile.release] no longer sets it. Those arguments are now false and \
         still written down in the source.",
        dependents.len()
    );
}

#[test]
fn guard_self_test() {
    let block = release_profile_block();
    assert!(
        block.starts_with("[profile.release]"),
        "block parser did not capture the section header: {block}"
    );
    assert!(
        !block.contains("[profile.dev]"),
        "block parser ran past the end of [profile.release] into the next \
         section, so it would pass on settings that live somewhere else: {block}"
    );
    assert!(
        !block.contains('#'),
        "comments were not stripped -- a setting mentioned only in a comment \
         would satisfy the assertions: {block}"
    );
}
