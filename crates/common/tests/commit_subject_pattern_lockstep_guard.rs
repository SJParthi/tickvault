//! One commit-subject rule, five places that enforce it, and nothing checking
//! they agree.
//!
//! Found 2026-08-22 while diagnosing why `All Green` was red on PR #1794 with
//! every code gate passing. The Commit Lint merge gate rejected three commit
//! subjects. Tracing how they were ever committed turned up five enforcement
//! points carrying **four different** patterns:
//!
//! | where | types | scope class | `[Phase N]` |
//! |---|---|---|---|
//! | `.github/workflows/ci.yml` (the merge gate) | 13 | `[a-z0-9_/,-]` | no |
//! | `CLAUDE.md` (the documented rule) | 13 | `[a-z0-9_/-]` | no |
//! | `.claude/hooks/pre-commit-gate.sh` | 13 | `[a-z0-9_/-]` | no |
//! | `.claude/hooks/pre-pr-gate.sh` | 13 | `[a-z0-9_/-]` | no |
//! | `scripts/git-hooks/commit-msg` | **8** | `[a-z0-9_/-]` | **yes** |
//!
//! Both failure directions were live at once. The git hook REJECTED five types
//! the merge gate accepts (`ci`, `build`, `style`, `bench`, `revert`) and
//! ACCEPTED a `[Phase N]` prefix the merge gate rejects, so it could both block
//! work CI would take and wave through work CI would refuse.
//!
//! Worse, the pre-commit extractor understood only `-m "subject"`. The two
//! forms `pr-completion-protocol.md` MANDATES for any body citing a section
//! sign -- heredoc and `-F <file>` -- were unreadable: `-F` extracted nothing
//! and printed SKIP, so the gate silently did not run. That skip is how the
//! three failing subjects reached the branch.
//!
//! The merge gate is the authority here, because it is the thing that actually
//! decides whether code lands. These tests read its pattern out of `ci.yml` and
//! require every local copy to match it verbatim -- and check behaviour
//! by running the repo's OWN commit-msg hook, rather than a Rust reimplementation
//! that could agree with itself while disagreeing with the shell.

use std::path::{Path, PathBuf};
use std::process::Command;

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

/// The merge gate's pattern, read out of `ci.yml` -- never restated here, so
/// this guard cannot drift away from the thing it is pinning.
fn merge_gate_pattern() -> String {
    let ci = read(".github/workflows/ci.yml");
    let line = ci
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("pattern='"))
        .expect("ci.yml must carry the Commit Lint `pattern='...'` line");
    line.trim_start_matches("pattern='")
        .trim_end_matches('\'')
        .to_string()
}

/// Every local enforcement point that must carry the merge gate's pattern.
const LOCAL_ENFORCEMENT_POINTS: &[&str] = &[
    "CLAUDE.md",
    ".claude/hooks/pre-commit-gate.sh",
    ".claude/hooks/pre-pr-gate.sh",
    "scripts/git-hooks/commit-msg",
];

#[test]
fn the_merge_gate_pattern_is_readable_and_sane() {
    let p = merge_gate_pattern();
    assert!(
        p.starts_with("^(feat|fix|"),
        "merge-gate pattern does not look like the commit-subject regex: {p}"
    );
    assert!(
        p.contains("(\\([a-z0-9_/,-]+\\))?"),
        "merge-gate pattern lost its scope group: {p}"
    );
}

#[test]
fn every_local_enforcement_point_matches_the_merge_gate_verbatim() {
    let want = merge_gate_pattern();
    for rel in LOCAL_ENFORCEMENT_POINTS {
        let body = read(rel);
        assert!(
            body.contains(&want),
            "{rel} does not carry the merge gate's commit-subject pattern verbatim.\n\
             A local copy that disagrees either blocks work CI accepts or waves \
             through work CI rejects -- both were live before this guard existed.\n\
             expected to find: {want}"
        );
    }
}

#[test]
fn the_git_hook_no_longer_accepts_a_prefix_the_merge_gate_rejects() {
    let hook = read("scripts/git-hooks/commit-msg");
    assert!(
        !hook.contains("\\[Phase [0-9]+\\]"),
        "scripts/git-hooks/commit-msg accepts a `[Phase N]` prefix. The merge \
         gate does not, so such a commit passes locally and is rejected by CI."
    );
}

/// The extractor must read all three forms a commit can arrive in. Two of them
/// are MANDATED by `pr-completion-protocol.md` for section-sign bodies.
#[test]
fn the_extractor_reads_the_two_mandated_commit_forms() {
    let hook = read(".claude/hooks/pre-commit-gate.sh");
    assert!(
        hook.contains("--file") && hook.contains("-F"),
        "pre-commit-gate.sh cannot read `git commit -F <file>`. It previously \
         extracted nothing for that form and printed SKIP, so the commit-message \
         gate silently did not run -- which is how three non-conforming subjects \
         reached PR #1794."
    );
    assert!(
        hook.contains("Form 2: heredoc"),
        "pre-commit-gate.sh lost its heredoc handling. Without it the extractor \
         returns the literal `$(cat <<` and FAILS an otherwise-valid subject."
    );
}

/// Behavioural check that runs the repo's OWN `commit-msg` hook rather than a
/// copy of its regex.
///
/// This is deliberately not a Rust reimplementation and not a bare `grep`: a
/// reimplementation can agree with itself while disagreeing with the shell, and
/// `grep` would only prove something about a pattern this test extracted. Running
/// the hook proves the thing a developer's commit actually hits. `bash` is on the
/// spawn allowlist for exactly this -- "test harnesses invoking the repo's own
/// .sh hooks".
fn hook_accepts(subject: &str) -> bool {
    let hook = repo_root().join("scripts/git-hooks/commit-msg");
    let msg = std::env::temp_dir().join(format!(
        "tv-commit-subject-{}-{}.txt",
        std::process::id(),
        subject.len()
    ));
    std::fs::write(&msg, format!("{subject}\n\nbody line\n")).expect("write temp commit message");

    let status = Command::new("bash")
        .arg(&hook)
        .arg(&msg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let _ = std::fs::remove_file(&msg);
    match status {
        Ok(s) => s.success(),
        Err(e) => panic!("could not run the commit-msg hook: {e}"),
    }
}

#[test]
fn the_hook_accepts_the_subjects_this_repo_actually_writes() {
    for subject in [
        "feat(app): add a thing",
        "fix(app,api): a multi-crate fix",
        "chore(deps): drop nine declarations",
        "docs: no scope at all",
        "perf(core,trading,app): measure two sweeps",
        "ci(workflows): a type the hook used to reject outright",
        "revert(app): another type the hook used to reject outright",
    ] {
        assert!(
            hook_accepts(subject),
            "the commit-msg hook rejects a subject the merge gate accepts: {subject}"
        );
    }
}

#[test]
fn the_hook_rejects_the_three_subjects_that_broke_the_merge_gate() {
    // Verbatim from the failing Commit Lint run on PR #1794 (2026-08-22).
    // These carry the Conventional Commits `!` breaking marker, which this
    // repo's pattern does not implement. Whether to implement it is an owner
    // decision about a merge gate, deliberately NOT taken here -- this test
    // only pins that the local hook and CI agree on rejecting them today, so
    // nobody "fixes" one side in isolation and reopens the silent-skip gap.
    for subject in [
        "refactor(api,core)!: remove the Groww feed — phase 3, api clean",
        "refactor(core)!: remove the Groww feed — phase 2, cadence lane collapse",
        "refactor(common)!: remove the Groww feed — phase 1, foundation crate",
    ] {
        assert!(
            !hook_accepts(subject),
            "the hook now accepts a subject the merge gate rejected: {subject}"
        );
    }
    assert!(!hook_accepts("merge: something"), "`merge` is not a type");
    assert!(
        !hook_accepts("[Phase 1] feat(app): thing"),
        "the merge gate rejects a [Phase N] prefix, so the hook must too"
    );
}
