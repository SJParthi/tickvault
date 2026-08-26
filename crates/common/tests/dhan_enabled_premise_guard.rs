//! Source comments may not assert a value for `dhan_enabled` that the shipped
//! config contradicts.
//!
//! WHY THIS EXISTS. On 2026-08-11 the operator flipped
//! `config/base.toml [feeds] dhan_enabled` from `false` to `true`. That one
//! edit invalidated a present-tense premise in six source files at once, and
//! nothing anywhere noticed:
//!
//! | file | what it said |
//! |---|---|
//! | `app/src/main.rs` | "`dhan_enabled=true` is an ILLEGAL post-retirement config … no Dhan WebSocket exists on any path" |
//! | `app/src/order_observability.rs` | "DORMANT while `feeds.dhan_enabled = false` (today's prod default)" |
//! | `storage/src/pnl_audit_persistence.rs` | the same, on a Rule-11 heartbeat contract |
//! | `app/src/spot_1m_rest_boot.rs` | "`dhan_enabled = false` (now the locked default)" |
//! | `api/src/feed_state.rs` | "retired by config (`dhan_enabled = false` in base + production)" |
//! | `app/src/dhan_feed_stack.rs` | "`dhan_enabled = false` in BOTH config/base.toml and config/production.toml" |
//!
//! They failed in BOTH directions, which is what makes the class dangerous
//! rather than merely untidy. `main.rs` told a reader the live lane cannot
//! exist while it dials sixteen sockets. `order_observability` and
//! `pnl_audit_persistence` did the opposite: their stated rule, applied to
//! the new flag value, says a subsystem went live — and each sits directly
//! above a promise that its own silence is a Rule-11 detector. Believing
//! them turns "nothing ran at all" into "nothing went wrong".
//!
//! The diagnosis is already written in this repository, at the `main.rs`
//! `feed_stack_gate` call site, by whoever fixed the ERROR message there and
//! missed the block 640 lines above it:
//!
//!   "The message survived the revival because nothing tied it to the flag
//!    it described."
//!
//! That is the whole argument for this file. Correcting the six comments
//! fixes today; only a test that READS the flag stops the seventh. This one
//! parses the real value out of `config/base.toml` and fails the build if a
//! comment still claims the opposite as a current fact.
//!
//! DELIBERATELY NARROW. It flags only CURRENT-STATE claims — a line that
//! mentions the flag, mentions `false`, and carries a present-tense marker
//! ("today", "prod default", "prod reality", "locked default", …). It does
//! NOT flag the many legitimate descriptions of the dhan-OFF *code path*
//! ("on a dhan-off boot", "the dhan-off arm"), which stay true whatever the
//! flag says, nor dated history that names when it changed. A guard whose
//! first act is a false positive teaches people to allowlist it.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common -> crates -> repo root")
        .to_path_buf()
}

/// The real `dhan_enabled` under `[feeds]` in `config/base.toml`.
///
/// Section-scoped on purpose: `dhan_enabled` also appears in comments and in
/// other sections, and reading the wrong one would make this guard confidently
/// wrong about the very fact it exists to pin.
fn shipped_dhan_enabled(root: &Path) -> bool {
    let toml = std::fs::read_to_string(root.join("config/base.toml"))
        .unwrap_or_else(|e| panic!("cannot read config/base.toml: {e}"));
    let mut in_feeds = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_feeds = t == "[feeds]";
            continue;
        }
        if !in_feeds || t.starts_with('#') {
            continue;
        }
        if let Some(rhs) = t.strip_prefix("dhan_enabled") {
            let v = rhs.trim_start().trim_start_matches('=').trim();
            let v = v.split('#').next().unwrap_or(v).trim();
            return v == "true";
        }
    }
    panic!("no `dhan_enabled` under [feeds] in config/base.toml — this guard cannot do its job");
}

/// Present-tense markers. A line needs one of these to be read as a claim
/// about the CURRENT config rather than about a code path or dated history.
const CURRENT_STATE_MARKERS: &[&str] = &[
    "today's prod",
    "today’s prod",
    "prod default",
    "prod reality",
    "locked default",
    "the shipped config",
    "in base + production",
    "in BOTH config",
];

fn scan(dir: &Path, out: &mut Vec<String>, files: &mut usize) {
    // Anti-vacuity: a silent early return here would make the whole guard
    // pass having read nothing — the exact false-OK it is written against.
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("guard corpus unreadable {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            scan(&path, out, files);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        // The guard file itself QUOTES the stale wording, because that table is
        // the evidence for why this test exists — the same carve-out
        // `rust_only_guard` takes: a guard has to name the thing it bans, and
        // removing the words would delete the argument, not the risk.
        if path
            .file_name()
            .is_some_and(|n| n == "dhan_enabled_premise_guard.rs")
        {
            continue;
        }
        *files += 1;
        for (i, line) in src.lines().enumerate() {
            let t = line.trim();
            if !t.starts_with("//") {
                continue;
            }
            if !t.contains("dhan_enabled") || !t.contains("false") {
                continue;
            }
            if CURRENT_STATE_MARKERS.iter().any(|m| t.contains(m)) {
                out.push(format!("{}:{}: {}", path.display(), i + 1, t));
            }
        }
    }
}

#[test]
fn no_comment_claims_dhan_enabled_is_false_while_the_config_says_true() {
    let root = repo_root();
    let shipped = shipped_dhan_enabled(&root);

    let mut offenders = Vec::new();
    let mut files = 0usize;
    scan(&root.join("crates"), &mut offenders, &mut files);

    // Non-vacuity: the workspace is ~300 .rs files across six crates plus the
    // lambdas. If this collapses, the walk broke and the guard is enforcing
    // nothing while reporting green.
    assert!(
        files > 100,
        "premise guard read only {files} .rs files — it is enforcing NOTHING"
    );

    if shipped {
        assert!(
            offenders.is_empty(),
            "config/base.toml carries `dhan_enabled = true`, but these comments \
             still state `false` as a CURRENT fact:\n  {}\n\n\
             This is the 2026-08-11 class: one config flip invalidated a \
             present-tense premise in six files and nothing noticed, because \
             nothing tied the prose to the flag. Either reword to describe the \
             dhan-OFF CODE PATH (always true) or date it as history.",
            offenders.join("\n  ")
        );
    }
    // If the operator ever flips the flag back to false, these comments become
    // true again and the guard correctly says nothing. It pins agreement with
    // the config, not one particular value.
}

#[test]
fn guard_self_test() {
    let root = repo_root();
    // The parser must find the key at all — a silent `false` from a missed
    // section would disable the check above without failing anything.
    let shipped = shipped_dhan_enabled(&root);
    assert!(
        shipped,
        "base.toml [feeds] dhan_enabled parsed as false. If the operator really \
         flipped it back, delete this assertion in the same change; if not, the \
         section-scoped parser is broken and the main test above is inert."
    );

    // The marker set must actually match the shape it was written for.
    let sample = "//! it is DORMANT while `feeds.dhan_enabled = false` (today's prod default)";
    assert!(
        CURRENT_STATE_MARKERS.iter().any(|m| sample.contains(m)),
        "the marker set no longer matches the exact wording this guard was \
         written to catch"
    );
    // …and must NOT match a plain code-path description.
    let code_path = "//! - Spawned ONLY from `dhan_rest_stack` Phase 5b (the dhan-OFF arm)";
    assert!(
        !CURRENT_STATE_MARKERS.iter().any(|m| code_path.contains(m)),
        "the marker set is over-broad: it flags a dhan-off CODE PATH \
         description, which stays true whatever the flag says"
    );
}
