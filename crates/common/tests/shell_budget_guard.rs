//! SHELL BUDGET GUARD — audit-plan item D6 (rust-only lock §0.10, 2026-10-01).
//!
//! The rust-only lock (`docs/claude-rules-full/project/rust-only-forever-lock-2026-07-19.md`)
//! says every executable in the product path is Rust. `rust_only_guard.rs`
//! enforces that for interpreted languages, but it allows `bash` and `sh`
//! everywhere with no limit: on 2026-10-01 the tree held 105 tracked shell
//! files (about 18,000 lines), five of them run by systemd on the production
//! server at every boot, and nothing stopped a sixth.
//!
//! This guard freezes that set and makes it shrink only:
//!
//! 1. `no_new_shell_files` — a tracked or untracked shell file that is not
//!    listed below fails the build. A shell file is any file named `*.sh`,
//!    any `*.sh.tftpl` template, or any file whose first line is a shebang
//!    naming a shell (`bash`, `sh`, `dash`, `zsh`, `ksh`).
//! 2. `shell_lists_shrink_only` — a listed file that is deleted, or no longer
//!    a shell file, fails the build until its entry is removed in the same PR.
//!    That is the designed friction that ratchets the list toward zero.
//! 3. `ops_shell_files_never_grow` — every file outside developer tooling
//!    (deploy scripts, operator scripts, CI gates) carries a line ceiling. It
//!    may stay level or shrink; growing it means porting the new logic to Rust
//!    instead. A ceiling may be lowered freely.
//! 4. `systemd_units_never_add_shell` — each unit file's count of `Exec*=`
//!    lines that run a shell (`/bin/sh`, `/bin/bash`, `sh -c`, or a `.sh`
//!    path) is pinned exactly. It may only go down, and when it does the pin
//!    goes down in the same PR.
//!
//! Developer tooling (`.claude/**`, `scripts/git-hooks/*`) is frozen by file
//! set only: those files run on a developer machine or in a Claude session,
//! never in the product path, and the lock's scope is the product path.
//! Their lines may change.
//!
//! HONEST LIMITS, stated rather than hidden:
//! - A shell program with no `.sh` name and no shebang that is run as
//!   `bash <file>` is not detected by name or content. Every such file in
//!   the tree today has a shebang; a new one would also need a caller, and
//!   callers in systemd units are checked by (4).
//! - Shell inside other files is NOT budgeted here yet: workflow `run:`
//!   steps, the Makefile, SSM command strings inside Rust (`operator_control`
//!   and friends), and `Command::new("sh")` spawns. Those are later D6 steps,
//!   recorded in `.claude/plans/active-plan-audit-fixes-2026-09-26.md`.
//! - Line ceilings count lines, not logic. A script can be rewritten at the
//!   same length; the ceiling only stops it from growing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Developer-tooling shell files: frozen set, lines free. Sorted.
/// ADDITIONS ARE FORBIDDEN. A deletion removes the entry in the same PR.
const SHELL_DEV_TOOLING_FILES: &[&str] = &[
    ".claude/hooks/auto-save-remote.sh",
    ".claude/hooks/auto-save-watchdog.sh",
    ".claude/hooks/banned-pattern-scanner.sh",
    ".claude/hooks/block-bypass.sh",
    ".claude/hooks/block-env-files.sh",
    ".claude/hooks/boot-symmetry-guard.sh",
    ".claude/hooks/data-integrity-guard.sh",
    ".claude/hooks/dedup-latency-scanner.sh",
    ".claude/hooks/dep-freshness-scanner.sh",
    ".claude/hooks/error-triage.sh",
    ".claude/hooks/financial-test-guard.sh",
    ".claude/hooks/hot-path-scanner-selftest.sh",
    ".claude/hooks/notification-log.sh",
    ".claude/hooks/per-item-guarantee-check.sh",
    ".claude/hooks/permission-audit-log.sh",
    ".claude/hooks/plan-gate.selftest.sh",
    ".claude/hooks/plan-gate.sh",
    ".claude/hooks/plan-verify.selftest.sh",
    ".claude/hooks/plan-verify.sh",
    ".claude/hooks/post-compact-recovery.sh",
    ".claude/hooks/post-edit-rustfmt.sh",
    ".claude/hooks/post-push-sync.sh",
    ".claude/hooks/pre-commit-gate.sh",
    ".claude/hooks/pre-merge-gate.sh",
    ".claude/hooks/pre-pr-gate.sh",
    ".claude/hooks/pre-push-gate.sh",
    ".claude/hooks/pre-tool-dispatch.sh",
    ".claude/hooks/pub-fn-test-guard.sh",
    ".claude/hooks/pub-fn-wiring-guard.sh",
    ".claude/hooks/recover-wip.sh",
    ".claude/hooks/scoped-test-runner.sh",
    ".claude/hooks/secret-scanner.sh",
    ".claude/hooks/session-auto-health.sh",
    ".claude/hooks/session-context-brief.sh",
    ".claude/hooks/session-end-save.sh",
    ".claude/hooks/session-sanity.sh",
    ".claude/hooks/stop-auto-verify.sh",
    ".claude/hooks/stop-failure-alert.sh",
    ".claude/hooks/stop-grill-reminder.sh",
    ".claude/hooks/test-count-guard.sh",
    ".claude/hooks/user-prompt-validate.sh",
    ".claude/plans/friday-may-15-mega/scripts/recover.sh",
    ".claude/statusline.sh",
    "scripts/git-hooks/commit-msg",
    "scripts/git-hooks/pre-commit",
    "scripts/git-hooks/pre-push",
];

/// Every other shell file, with its line ceiling as of 2026-10-01. Sorted.
/// ADDITIONS ARE FORBIDDEN and a ceiling may never rise. A deletion removes
/// the entry in the same PR; a ceiling may be lowered at any time.
const SHELL_OPS_FILES: &[(&str, usize)] = &[
    ("deploy/aws/terraform/user-data.sh.tftpl", 266),
    ("scripts/100pct-audit.sh", 378),
    ("scripts/all-green-equivalence-matrix.sh", 272),
    ("scripts/auto-fix-clear-spill-rollback.sh", 41),
    ("scripts/auto-fix-clear-spill.sh", 56),
    ("scripts/auto-fix-drain-spill-rollback.sh", 40),
    ("scripts/auto-fix-drain-spill.sh", 97),
    ("scripts/auto-fix-refresh-instruments-rollback.sh", 49),
    ("scripts/auto-fix-refresh-instruments.sh", 65),
    ("scripts/auto-fix-rotate-token-rollback.sh", 56),
    ("scripts/auto-fix-rotate-token.sh", 97),
    ("scripts/aws-autopilot.sh", 471),
    ("scripts/aws-bootstrap-state-backend.sh", 94),
    ("scripts/aws-one-shot-bootstrap.sh", 323),
    ("scripts/aws-seed-ssm-parameters.sh", 250),
    ("scripts/aws-upgrade-instance.sh", 542),
    ("scripts/bench-gate.selftest.sh", 212),
    ("scripts/bench-gate.sh", 440),
    ("scripts/bootstrap.sh", 323),
    ("scripts/claude-session-bootstrap.sh", 199),
    ("scripts/clippy-local.sh", 92),
    ("scripts/count-current-expiry-universe.sh", 128),
    ("scripts/coverage-gate.sh", 372),
    ("scripts/cross-verify-show.sh", 50),
    ("scripts/diagnose-write-amplification.sh", 99),
    ("scripts/doctor.sh", 298),
    ("scripts/ensure-aws-cli.sh", 71),
    ("scripts/ensure-ready.sh", 312),
    ("scripts/flaky-detect.sh", 93),
    ("scripts/mcp-doctor.sh", 97),
    ("scripts/mcp-servers/tickvault-logs-launch.sh", 33),
    ("scripts/measure-ram-residency-cost.sh", 113),
    ("scripts/notify-telegram.sh", 84),
    ("scripts/prev-day-show.sh", 44),
    ("scripts/provision-infra-secrets.sh", 101),
    ("scripts/quality-full.sh", 137),
    ("scripts/questdb-console-gate-matrix.sh", 265),
    ("scripts/questdb-init.sh", 190),
    ("scripts/questdb-tunnel.sh", 304),
    ("scripts/register-dhan-ip.sh", 316),
    ("scripts/setup-cloudwatch-alarms.sh", 227),
    ("scripts/setup-observability.sh", 375),
    ("scripts/setup-secrets.sh", 118),
    ("scripts/sync-to-integration.sh", 255),
    ("scripts/test-coverage-guard.sh", 310),
    ("scripts/test-register-dhan-ip.sh", 114),
    ("scripts/triage/rollback.sh", 60),
    ("scripts/triage/verify.sh", 128),
    ("scripts/tv-tunnel/doctor.sh", 160),
    ("scripts/tv-tunnel/install-aws.sh", 138),
    ("scripts/tv-tunnel/install-mac.sh", 168),
    ("scripts/validate-automation.sh", 178),
    ("scripts/validate-deploy-ssm-quoting.sh", 81),
    ("scripts/verify-option-chain-minute-snapshot.sh", 193),
    ("scripts/verify-stack.sh", 125),
];

/// Per systemd unit: how many `Exec*=` lines run a shell. Pinned EXACTLY:
/// it may only go down, and the pin moves down in the same PR.
/// Empty since 2026-10-01 (audit D6d): the last one, the QuestDB self-heal,
/// is now `tickvault-host ensure-questdb`.
const SYSTEMD_SHELL_EXEC_BUDGET: &[(&str, usize)] = &[];

/// Shells a shebang may name for the file to count as a shell script.
const SHELL_RUNTIMES: &[&str] = &["bash", "sh", "dash", "zsh", "ksh"];

// ============================ PURE CORE ============================

/// The runtime a shebang names: `#!/bin/bash` -> `bash`,
/// `#!/usr/bin/env -S bash -e` -> `bash`. `None` when line 1 is not a shebang.
fn shebang_runtime(content: &str) -> Option<String> {
    let rest = content.lines().next()?.strip_prefix("#!")?;
    let mut parts = rest.split_whitespace();
    let mut current = parts.next()?;
    loop {
        let base = current.rsplit('/').next().unwrap_or(current);
        if base == "env" || base.starts_with('-') {
            current = parts.next()?;
            continue;
        }
        return Some(base.to_string());
    }
}

/// True when `path` / `content` is a shell script by name or by shebang.
fn is_shell_file(path: &str, content: &str) -> bool {
    if path.ends_with(".sh") || path.ends_with(".sh.tftpl") {
        return true;
    }
    shebang_runtime(content).is_some_and(|r| SHELL_RUNTIMES.contains(&r.as_str()))
}

/// Shell files found that neither list names.
fn unlisted_shell_files(found: &[String], dev: &[&str], ops: &[(&str, usize)]) -> Vec<String> {
    found
        .iter()
        .filter(|p| !dev.contains(&p.as_str()) && !ops.iter().any(|(o, _)| *o == p.as_str()))
        .cloned()
        .collect()
}

/// Listed entries that are no longer shell files in the tree.
fn stale_entries(found: &[String], dev: &[&str], ops: &[(&str, usize)]) -> Vec<String> {
    dev.iter()
        .copied()
        .chain(ops.iter().map(|(p, _)| *p))
        .filter(|p| !found.iter().any(|f| f == p))
        .map(str::to_string)
        .collect()
}

/// `(path, lines, ceiling)` for every ops file over its ceiling.
fn over_ceiling(lines: &[(String, usize)], ops: &[(&str, usize)]) -> Vec<(String, usize, usize)> {
    lines
        .iter()
        .filter_map(|(p, n)| {
            let (_, cap) = ops.iter().find(|(o, _)| *o == p.as_str())?;
            (n > cap).then(|| (p.clone(), *n, *cap))
        })
        .collect()
}

/// True when one unit-file line is an `Exec*=` directive that runs a shell.
fn exec_line_runs_shell(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with('#') || !line.starts_with("Exec") {
        return false;
    }
    let Some((_, cmd)) = line.split_once('=') else {
        return false;
    };
    // Strip systemd's executable prefixes (`-`, `@`, `+`, `!`, `:`).
    let cmd = cmd.trim_start_matches(['-', '@', '+', '!', ':']);
    let mut words = cmd.split_whitespace();
    let Some(program) = words.next() else {
        return false;
    };
    let base = program.rsplit('/').next().unwrap_or(program);
    if SHELL_RUNTIMES.contains(&base) || program.ends_with(".sh") {
        return true;
    }
    // `/usr/bin/env bash …`
    if base == "env" {
        return words
            .find(|w| !w.starts_with('-'))
            .is_some_and(|w| SHELL_RUNTIMES.contains(&w.rsplit('/').next().unwrap_or(w)));
    }
    false
}

/// Shell `Exec*=` lines in one unit file. Continuation lines (`\`) are joined.
fn count_shell_exec_lines(unit: &str) -> usize {
    let mut joined: Vec<String> = Vec::new();
    let mut pending = String::new();
    for raw in unit.lines() {
        if let Some(head) = raw.strip_suffix('\\') {
            pending.push_str(head);
            pending.push(' ');
            continue;
        }
        pending.push_str(raw);
        joined.push(std::mem::take(&mut pending));
    }
    if !pending.is_empty() {
        joined.push(pending);
    }
    joined.iter().filter(|l| exec_line_runs_shell(l)).count()
}

fn assert_sorted_unique(names: &[&str], what: &str) {
    for pair in names.windows(2) {
        assert!(
            pair[0] < pair[1],
            "{what} must be sorted and unique: `{}` then `{}`",
            pair[0],
            pair[1]
        );
    }
}

// ============================ REAL-TREE PLUMBING ============================

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("shell_budget_guard: cannot canonicalize repo root")
}

/// Tracked AND untracked-not-ignored files, NUL-delimited so no path is quoted.
fn all_files() -> Vec<String> {
    let root = repo_root();
    let mut out = Vec::new();
    for extra in [&[][..], &["--others", "--exclude-standard"][..]] {
        let o = Command::new("git")
            .arg("ls-files")
            .arg("-z")
            .args(extra)
            .current_dir(&root)
            .output()
            .expect("shell_budget_guard: `git ls-files` failed to run");
        assert!(
            o.status.success(),
            "shell_budget_guard: `git ls-files` failed"
        );
        out.extend(
            o.stdout
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned()),
        );
    }
    out.sort();
    out.dedup();
    out
}

/// Read a file lossily. A listed path that cannot be read is a failure, never a skip.
fn read_text(path: &str) -> Option<String> {
    let full = repo_root().join(path);
    if !full.is_file() {
        return None;
    }
    let bytes = std::fs::read(&full)
        .unwrap_or_else(|e| panic!("shell_budget_guard: cannot read `{path}`: {e}"));
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Every shell file in the tree with its line count.
fn shell_files_with_lines() -> Vec<(String, usize)> {
    all_files()
        .into_iter()
        .filter_map(|p| {
            // Markdown can quote a shebang on line 1 of a code sample; docs
            // are never executed.
            if p.ends_with(".md") {
                return None;
            }
            let text = read_text(&p)?;
            is_shell_file(&p, &text).then(|| (p, text.lines().count()))
        })
        .collect()
}

// ============================ REAL-TREE TESTS ============================

#[test]
fn no_new_shell_files() {
    assert_sorted_unique(SHELL_DEV_TOOLING_FILES, "SHELL_DEV_TOOLING_FILES");
    let ops: Vec<&str> = SHELL_OPS_FILES.iter().map(|(p, _)| *p).collect();
    assert_sorted_unique(&ops, "SHELL_OPS_FILES");
    for p in &ops {
        assert!(
            !SHELL_DEV_TOOLING_FILES.contains(p),
            "`{p}` is in both shell lists"
        );
    }
    let found: Vec<String> = shell_files_with_lines()
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    // Anti-vacuity: the scan must see the files it is guarding.
    assert!(
        found.len() > 10,
        "shell_budget_guard found only {} shell files; the scan is broken",
        found.len()
    );
    let new = unlisted_shell_files(&found, SHELL_DEV_TOOLING_FILES, SHELL_OPS_FILES);
    assert!(
        new.is_empty(),
        "NEW shell file(s) {new:?}. The rust-only lock (§0.10) forbids adding shell: \
         write it in Rust (a subcommand of the app binary, or a test/CI step in Rust)."
    );
}

#[test]
fn shell_lists_shrink_only() {
    let found: Vec<String> = shell_files_with_lines()
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let stale = stale_entries(&found, SHELL_DEV_TOOLING_FILES, SHELL_OPS_FILES);
    assert!(
        stale.is_empty(),
        "shell list entries {stale:?} name files that are gone or no longer shell. \
         Remove them from crates/common/tests/shell_budget_guard.rs in this PR."
    );
}

#[test]
fn ops_shell_files_never_grow() {
    let over = over_ceiling(&shell_files_with_lines(), SHELL_OPS_FILES);
    assert!(
        over.is_empty(),
        "shell file(s) grew past their ceiling (path, lines, ceiling): {over:?}. \
         Put the new logic in Rust instead, or make the edit line-neutral. Raising \
         a ceiling needs a dated operator quote in the rust-only lock first."
    );
}

#[test]
fn systemd_units_never_add_shell() {
    let units: Vec<String> = all_files()
        .into_iter()
        .filter(|p| p.starts_with("deploy/systemd/") && p.ends_with(".service"))
        .collect();
    assert!(
        !units.is_empty(),
        "no systemd units found; the scan is broken"
    );
    let budget_names: Vec<&str> = SYSTEMD_SHELL_EXEC_BUDGET.iter().map(|(p, _)| *p).collect();
    assert_sorted_unique(&budget_names, "SYSTEMD_SHELL_EXEC_BUDGET");
    let mut drift = Vec::new();
    for unit in &units {
        let text = read_text(unit).unwrap_or_default();
        let got = count_shell_exec_lines(&text);
        let want = SYSTEMD_SHELL_EXEC_BUDGET
            .iter()
            .find(|(p, _)| p == unit)
            .map_or(0, |(_, n)| *n);
        if got != want {
            drift.push((unit.clone(), got, want));
        }
    }
    for (p, _) in SYSTEMD_SHELL_EXEC_BUDGET {
        if !units.iter().any(|u| u == p) {
            drift.push(((*p).to_string(), 0, usize::MAX));
        }
    }
    assert!(
        drift.is_empty(),
        "systemd shell Exec lines drifted (unit, found, pinned): {drift:?}. A rise is \
         forbidden (call a Rust subcommand instead); a fall lowers the pin in this PR; \
         a pin for a missing unit is removed."
    );
}

// ============================ SELF-TESTS ============================

#[test]
fn shell_budget_guard_self_test() {
    // is_shell_file: by name and by shebang.
    assert!(is_shell_file("a/b.sh", ""));
    assert!(is_shell_file("deploy/x.sh.tftpl", ""));
    assert!(is_shell_file(
        "scripts/git-hooks/pre-push",
        "#!/bin/bash\necho\n"
    ));
    assert!(is_shell_file("x", "#!/usr/bin/env -S bash -e\n"));
    assert!(is_shell_file("x", "#!/bin/dash\n"));
    assert!(is_shell_file("x", "#!/usr/bin/env zsh\n"));
    assert!(!is_shell_file("x.rs", "fn main() {}\n"));
    assert!(
        !is_shell_file("x", "echo\n#!/bin/bash\n"),
        "shebang must be line 1"
    );
    assert!(!is_shell_file("x.tftpl", "#cloud-config\n"));

    // unlisted / stale / ceiling.
    let found = vec!["a.sh".to_string(), "b.sh".to_string()];
    assert_eq!(
        unlisted_shell_files(&found, &["a.sh"], &[]),
        vec!["b.sh".to_string()]
    );
    assert!(unlisted_shell_files(&found, &["a.sh"], &[("b.sh", 1)]).is_empty());
    assert_eq!(
        stale_entries(&found, &["a.sh", "gone.sh"], &[]),
        vec!["gone.sh".to_string()]
    );
    assert_eq!(
        stale_entries(&found, &[], &[("b.sh", 1), ("old.sh", 1)]),
        vec!["old.sh".to_string()]
    );
    let lines = vec![("a.sh".to_string(), 10), ("b.sh".to_string(), 5)];
    assert_eq!(
        over_ceiling(&lines, &[("a.sh", 9), ("b.sh", 5)]),
        vec![("a.sh".to_string(), 10, 9)]
    );
    assert!(over_ceiling(&lines, &[("a.sh", 10)]).is_empty());

    // systemd Exec lines.
    for yes in [
        "ExecStart=/opt/x/run.sh",
        "ExecStart=-/opt/x/run.sh --flag",
        "ExecStartPre=-/bin/bash /opt/x/ensure",
        "ExecStart=-/bin/sh -c 'modprobe x'",
        "ExecStop=+/usr/bin/env bash -c true",
        "ExecStart=@/bin/sh sh -c true",
        "  ExecStartPost=/usr/bin/dash /x",
    ] {
        assert!(exec_line_runs_shell(yes), "should count: {yes}");
    }
    for no in [
        "ExecStart=/opt/tickvault/bin/tickvault",
        "ExecStart=-/sbin/ethtool -G ens34 rx 8192",
        "ExecStart=/usr/bin/install -m 0644 a.sh /etc/a.sh",
        "# ExecStart=/bin/bash x",
        "Environment=SHELL=/bin/bash",
        "ExecStart=",
    ] {
        assert!(!exec_line_runs_shell(no), "should not count: {no}");
    }
    let unit =
        "[Service]\nExecStart=/bin/sh \\\n  -c 'x'\nExecStart=/opt/bin/app\n# ExecStart=/x.sh\n";
    assert_eq!(count_shell_exec_lines(unit), 1);
}

// ======================= EMBEDDED SHELL + AWK/JQ (2026-10-04) =======================
//
// Audit findings M5 and L8 (2026-10-04). The four tests above budget shell
// FILES. Two places run root shell on the production server without being a
// shell file, and two text languages ran with no count at all:
//
// - M5a: the operator console Lambda sends shell over SSM, written as Rust
//   string constants. `console_embedded_shell_never_grows` pins the physical
//   lines and bytes of every string literal in those source files.
// - M5b: workflow steps send shell over SSM as `--parameters 'commands=[…]'`.
//   `ssm_workflow_shell_never_grows` pins, per workflow file, the number of
//   command elements and their bytes, and fails when an `ssm send-command`
//   carries no parseable `commands=[…]` payload (shell hidden in a variable
//   or a file would otherwise escape the count).
// - L8: `awk` and `jq` are deliberately not banned (`rust_only_guard.rs`
//   NODE_FAMILY docblock: jq runs the All Green verdict, merge-gate-lock
//   §5.1), but nothing counted them. `awk_jq_usage_never_grows` pins, per
//   file, the word occurrences of `awk`/`gawk`/`mawk`/`jq` (and `--jq`) on
//   non-comment lines of every non-Rust, non-doc file, and inside string
//   literals of production Rust (`crates/*/src`, `crates/*/build.rs`).
//   `all_green_jq_program_never_grows` pins the All Green jq program's lines.
//   A `*.awk` / `*.jq` program file is forbidden outright.
//
// Every number is a CEILING: it may stay level or fall, and may be lowered
// in any PR. Raising one needs a dated operator quote in the rust-only lock
// (§0.10) first. A file that leaves the tree must leave its row.
//
// HONEST LIMITS, stated rather than hidden:
// - The console shell is NOT moved to Rust here; it is frozen. Porting it is
//   the remaining work (rust-only lock §0.10).
// - `operator_control.rs` also builds a few short shell lines with
//   `.to_string()` / `format!` next to its HTML and messages; its string
//   literals are not counted here because most of them are not shell.
// - Rust literals are read up to the first `#[cfg(test)]` line; a string
//   after that line in production code would not be counted.
// - awk/jq counting is by word: `${AWK}` through a variable, or a name split
//   across quotes, is not seen. Comment lines (`#`, `//`) are skipped,
//   except a `#!` shebang on line 1.

/// Console Lambda sources whose string literals are SSM shell:
/// `(path, physical lines, bytes)` ceilings measured 2026-10-04.
const CONSOLE_SHELL_SOURCES: &[(&str, usize, usize)] = &[
    (
        "crates/aws-lambdas/src/operator_control_action_commands.rs",
        537,
        34447,
    ),
    (
        "crates/aws-lambdas/src/operator_control_commands.rs",
        27,
        1766,
    ),
];

/// Per workflow file: `(path, SSM command elements, bytes)` ceilings,
/// measured 2026-10-04. A workflow absent here may send no SSM shell.
const SSM_WORKFLOW_SHELL_BUDGET: &[(&str, usize, usize)] = &[
    (".github/workflows/aws-control.yml", 22, 1486),
    (".github/workflows/deploy-aws.yml", 95, 11980),
    (".github/workflows/downsize-instance.yml", 43, 3767),
    (".github/workflows/grow-ebs-volume.yml", 9, 353),
];

/// Per file: awk/jq word-occurrence ceilings, measured 2026-10-04. A file
/// absent here may use neither.
const AWK_JQ_BUDGET: &[(&str, usize)] = &[
    (".claude/hooks/auto-save-remote.sh", 1),
    (".claude/hooks/banned-pattern-scanner.sh", 1),
    (".claude/hooks/block-bypass.sh", 1),
    (".claude/hooks/block-env-files.sh", 2),
    (".claude/hooks/boot-symmetry-guard.sh", 1),
    (".claude/hooks/data-integrity-guard.sh", 1),
    (".claude/hooks/dedup-latency-scanner.sh", 1),
    (".claude/hooks/dep-freshness-scanner.sh", 1),
    (".claude/hooks/error-triage.sh", 2),
    (".claude/hooks/financial-test-guard.sh", 1),
    (".claude/hooks/hot-path-scanner-selftest.sh", 1),
    (".claude/hooks/notification-log.sh", 2),
    (".claude/hooks/permission-audit-log.sh", 2),
    (".claude/hooks/plan-gate.sh", 1),
    (".claude/hooks/plan-verify.sh", 1),
    (".claude/hooks/post-edit-rustfmt.sh", 1),
    (".claude/hooks/post-push-sync.sh", 5),
    (".claude/hooks/pre-commit-gate.sh", 4),
    (".claude/hooks/pre-merge-gate.sh", 4),
    (".claude/hooks/pre-pr-gate.sh", 7),
    (".claude/hooks/pre-push-gate.sh", 2),
    (".claude/hooks/pre-tool-dispatch.sh", 1),
    (".claude/hooks/scoped-test-runner.sh", 2),
    (".claude/hooks/session-auto-health.sh", 1),
    (".claude/hooks/session-context-brief.sh", 2),
    (".claude/hooks/session-sanity.sh", 3),
    (".claude/hooks/stop-failure-alert.sh", 2),
    (".claude/hooks/stop-grill-reminder.sh", 1),
    (".claude/hooks/user-prompt-validate.sh", 1),
    (".claude/plans/friday-may-15-mega/scripts/recover.sh", 3),
    (".claude/settings.json", 1),
    (".claude/statusline.sh", 7),
    (".github/workflows/auto-merge.yml", 4),
    (".github/workflows/aws-control.yml", 6),
    (".github/workflows/ci.yml", 4),
    (".github/workflows/dep-freshness-nightly.yml", 1),
    (".github/workflows/deploy-aws-after-close.yml", 2),
    (".github/workflows/deploy-aws.yml", 4),
    (".github/workflows/downsize-instance.yml", 4),
    (".github/workflows/emergency-fs-recover.yml", 1),
    (".github/workflows/fuzz.yml", 4),
    (".github/workflows/local-branch-sync.yml", 1),
    (".github/workflows/mutation.yml", 1),
    (".github/workflows/postmerge-catchup.yml", 4),
    ("Makefile", 16),
    (
        "crates/aws-lambdas/src/operator_control_action_commands.rs",
        7,
    ),
    ("crates/aws-lambdas/src/operator_control_commands.rs", 2),
    ("deploy/aws/terraform/user-data.sh.tftpl", 1),
    ("scripts/all-green-equivalence-matrix.sh", 4),
    ("scripts/aws-one-shot-bootstrap.sh", 4),
    ("scripts/aws-upgrade-instance.sh", 4),
    ("scripts/bench-gate.sh", 3),
    ("scripts/claude-session-bootstrap.sh", 3),
    ("scripts/count-current-expiry-universe.sh", 1),
    ("scripts/coverage-gate.sh", 2),
    ("scripts/diagnose-write-amplification.sh", 1),
    ("scripts/git-hooks/pre-push", 2),
    ("scripts/notify-telegram.sh", 2),
    ("scripts/questdb-console-gate-matrix.sh", 2),
    ("scripts/register-dhan-ip.sh", 9),
    ("scripts/setup-observability.sh", 8),
    ("scripts/setup-secrets.sh", 1),
    ("scripts/test-register-dhan-ip.sh", 1),
    ("scripts/triage/verify.sh", 2),
    ("scripts/tv-tunnel/doctor.sh", 1),
    ("scripts/tv-tunnel/install-aws.sh", 1),
    ("scripts/tv-tunnel/install-mac.sh", 1),
    ("scripts/validate-deploy-ssm-quoting.sh", 2),
    ("scripts/verify-option-chain-minute-snapshot.sh", 10),
];

/// Lines of the All Green jq program in `ci.yml` (between `jq -rn '` and the
/// closing `')`), measured 2026-10-04. Owner-approved (merge-gate-lock §5.1);
/// frozen here, never edited by this guard.
const ALL_GREEN_JQ_PROGRAM_LINES: usize = 21;

/// The awk/jq words counted.
const AWK_JQ_WORDS: &[&str] = &["awk", "gawk", "mawk", "jq"];

/// Only the region before the first `#[cfg(test)]` line is production code.
fn production_region(src: &str) -> &str {
    let mut offset = 0usize;
    for line in src.split_inclusive('\n') {
        if line.trim() == "#[cfg(test)]" {
            return &src[..offset];
        }
        offset += line.len();
    }
    src
}

/// The contents of every string literal in Rust source (plain, raw, byte),
/// as written. Comments and char literals are skipped. Returns `None` when a
/// literal or block comment is unterminated (fail closed).
fn rust_string_literals(src: &str) -> Option<Vec<String>> {
    let c: Vec<char> = src.chars().collect();
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < c.len() {
        let ch = c[i];
        // Comments.
        if ch == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '/' && c.get(i + 1) == Some(&'*') {
            let mut depth = 1usize;
            i += 2;
            while depth > 0 {
                match (c.get(i), c.get(i + 1)) {
                    (Some('/'), Some('*')) => {
                        depth += 1;
                        i += 2;
                    }
                    (Some('*'), Some('/')) => {
                        depth -= 1;
                        i += 2;
                    }
                    (Some(_), _) => i += 1,
                    (None, _) => return None,
                }
            }
            continue;
        }
        // Char literal or lifetime.
        if ch == '\'' {
            if c.get(i + 1) == Some(&'\\') {
                i += 2;
                while i < c.len() && c[i] != '\'' {
                    i += 1;
                }
                i += 1;
            } else if c.get(i + 2) == Some(&'\'') {
                i += 3;
            } else {
                i += 1;
            }
            continue;
        }
        // Raw string: `r#*"` (optionally `br`), not inside an identifier.
        let raw_start = ch == 'r' && {
            let prev = i.checked_sub(1).map(|p| c[p]);
            let prev_ok = match prev {
                None => true,
                Some('b') => i.checked_sub(2).map(|p| c[p]).is_none_or(|pp| !ident(pp)),
                Some(p) => !ident(p),
            };
            let mut j = i + 1;
            while c.get(j) == Some(&'#') {
                j += 1;
            }
            prev_ok && c.get(j) == Some(&'"')
        };
        if raw_start {
            let mut j = i + 1;
            let mut hashes = 0usize;
            while c[j] == '#' {
                hashes += 1;
                j += 1;
            }
            let start = j + 1;
            let mut k = start;
            loop {
                if k >= c.len() {
                    return None;
                }
                if c[k] == '"' && (1..=hashes).all(|h| c.get(k + h) == Some(&'#')) {
                    break;
                }
                k += 1;
            }
            out.push(c[start..k].iter().collect());
            i = k + 1 + hashes;
            continue;
        }
        if ch == '"' {
            let start = i + 1;
            let mut k = start;
            loop {
                match c.get(k) {
                    None => return None,
                    Some('\\') => k += 2,
                    Some('"') => break,
                    Some(_) => k += 1,
                }
            }
            out.push(c[start..k].iter().collect());
            i = k + 1;
            continue;
        }
        i += 1;
    }
    Some(out)
}

/// `(physical lines, bytes)` of the string literals in a Rust source region.
fn literal_lines_and_bytes(literals: &[String]) -> (usize, usize) {
    literals
        .iter()
        .filter(|s| !s.is_empty())
        .fold((0, 0), |(l, b), s| (l + s.split('\n').count(), b + s.len()))
}

/// True for a comment line in shell / YAML / TOML / Makefile / Rust.
fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || t.starts_with("//")
}

/// The JSON-string elements of every `commands=[…]` SSM payload, skipping
/// comment lines. `None` when a payload is unterminated (fail closed).
fn ssm_payloads(text: &str) -> Option<Vec<Vec<String>>> {
    let cleaned: String = text
        .split_inclusive('\n')
        .map(|l| if is_comment_line(l) { "\n" } else { l })
        .collect();
    let c: Vec<char> = cleaned.chars().collect();
    let needle: Vec<char> = "commands=[".chars().collect();
    let mut payloads = Vec::new();
    let mut i = 0usize;
    while i + needle.len() <= c.len() {
        if c[i..i + needle.len()] != needle[..] {
            i += 1;
            continue;
        }
        let mut k = i + needle.len();
        let mut elems = Vec::new();
        loop {
            match c.get(k) {
                None => return None,
                Some(']') => break,
                Some('"') => {
                    let start = k + 1;
                    let mut e = start;
                    loop {
                        match c.get(e) {
                            None => return None,
                            Some('\\') => e += 2,
                            Some('"') => break,
                            Some(_) => e += 1,
                        }
                    }
                    elems.push(c[start..e].iter().collect());
                    k = e + 1;
                }
                Some(_) => k += 1,
            }
        }
        payloads.push(elems);
        i = k + 1;
    }
    Some(payloads)
}

/// Non-comment lines that run `ssm send-command`.
fn count_ssm_send_commands(text: &str) -> usize {
    text.lines()
        .filter(|l| !is_comment_line(l) && l.contains("ssm send-command"))
        .count()
}

/// Whole-word awk/jq occurrences in one line (`--jq` counts; `x.jq`, `jq_x`,
/// `gawk`'s inner `awk` do not double count).
fn count_awk_jq_in_line(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut hits = 0usize;
    for word in AWK_JQ_WORDS {
        let mut from = 0usize;
        while let Some(rel) = line[from..].find(word) {
            let at = from + rel;
            let end = at + word.len();
            let after_ok = bytes
                .get(end)
                .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-'));
            let before_ok = match at.checked_sub(1).map(|p| bytes[p]) {
                None => true,
                Some(b'-') => at >= 2 && &line[at - 2..at] == "--",
                Some(b) => !(b.is_ascii_alphanumeric() || b == b'_' || b == b'.'),
            };
            if after_ok && before_ok {
                hits += 1;
            }
            from = end;
        }
    }
    hits
}

/// awk/jq occurrences in a non-Rust file: non-comment lines, plus a line-1
/// shebang.
fn count_awk_jq_text(text: &str) -> usize {
    text.lines()
        .enumerate()
        .filter(|(n, l)| !is_comment_line(l) || (*n == 0 && l.starts_with("#!")))
        .map(|(_, l)| count_awk_jq_in_line(l))
        .sum()
}

/// awk/jq occurrences inside string literals of a production Rust region.
fn count_awk_jq_rust(src: &str) -> Option<usize> {
    let lits = rust_string_literals(production_region(src))?;
    Some(
        lits.iter()
            .flat_map(|s| s.lines())
            .map(count_awk_jq_in_line)
            .sum(),
    )
}

/// Lines of the All Green jq program: the lines strictly between the one
/// holding `verdict="$(jq -rn '` and the next whose trim starts with `')`.
fn all_green_jq_program_lines(ci: &str) -> Option<usize> {
    let lines: Vec<&str> = ci.lines().collect();
    let open = lines
        .iter()
        .position(|l| l.contains("verdict=\"$(jq -rn '"))?;
    let close = lines[open + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with("')"))?;
    Some(close)
}

/// `(path, value, ceiling)` rows over their ceiling, plus rows for measured
/// files that have no ceiling (`ceiling = 0`).
fn over_budget(
    measured: &[(String, usize)],
    budget: &[(&str, usize)],
) -> Vec<(String, usize, usize)> {
    measured
        .iter()
        .filter_map(|(p, n)| {
            let cap = budget
                .iter()
                .find(|(b, _)| *b == p.as_str())
                .map_or(0, |(_, c)| *c);
            (*n > cap).then(|| (p.clone(), *n, cap))
        })
        .collect()
}

/// Budget rows whose file has gone, or now measures zero.
fn stale_budget_rows(measured: &[(String, usize)], budget: &[(&str, usize)]) -> Vec<String> {
    budget
        .iter()
        .filter(|(b, _)| !measured.iter().any(|(p, n)| p == b && *n > 0))
        .map(|(b, _)| (*b).to_string())
        .collect()
}

fn is_production_rust(p: &str) -> bool {
    p.starts_with("crates/")
        && p.ends_with(".rs")
        && (p.contains("/src/") || p.ends_with("/build.rs"))
}

#[test]
fn console_embedded_shell_never_grows() {
    let names: Vec<&str> = CONSOLE_SHELL_SOURCES.iter().map(|r| r.0).collect();
    assert_sorted_unique(&names, "CONSOLE_SHELL_SOURCES");
    assert_eq!(
        names.len(),
        2,
        "both console shell sources must stay listed"
    );
    let mut report = Vec::new();
    let mut over = Vec::new();
    for (path, max_lines, max_bytes) in CONSOLE_SHELL_SOURCES {
        let src = read_text(path)
            .unwrap_or_else(|| panic!("console shell source `{path}` is gone: remove its row"));
        let lits = rust_string_literals(production_region(&src))
            .unwrap_or_else(|| panic!("`{path}`: unterminated literal or comment"));
        let (lines, bytes) = literal_lines_and_bytes(&lits);
        // Anti-vacuity: every listed file carries shell today.
        assert!(
            lines > 10,
            "`{path}`: only {lines} literal lines; the lexer is broken"
        );
        report.push(format!("    (\"{path}\", {lines}, {bytes}),"));
        if lines > *max_lines || bytes > *max_bytes {
            over.push((path.to_string(), lines, *max_lines, bytes, *max_bytes));
        }
    }
    assert!(
        over.is_empty(),
        "console SSM shell grew (path, lines, ceiling, bytes, ceiling): {over:?}. \
         Write the new step in Rust (a `tickvault-host` subcommand) instead. \
         Measured now:\n{}",
        report.join("\n")
    );
}

#[test]
fn ssm_workflow_shell_never_grows() {
    let names: Vec<&str> = SSM_WORKFLOW_SHELL_BUDGET.iter().map(|r| r.0).collect();
    assert_sorted_unique(&names, "SSM_WORKFLOW_SHELL_BUDGET");
    let workflows: Vec<String> = all_files()
        .into_iter()
        .filter(|p| {
            p.starts_with(".github/workflows/") && (p.ends_with(".yml") || p.ends_with(".yaml"))
        })
        .collect();
    assert!(
        workflows.len() > 5,
        "found only {} workflows; the scan is broken",
        workflows.len()
    );
    let mut report = Vec::new();
    let mut problems = Vec::new();
    for wf in &workflows {
        let text = read_text(wf).unwrap_or_default();
        let payloads = ssm_payloads(&text)
            .unwrap_or_else(|| panic!("`{wf}`: unterminated `commands=[` payload"));
        let sends = count_ssm_send_commands(&text);
        if payloads.len() < sends {
            problems.push(format!(
                "`{wf}`: {sends} `ssm send-command` line(s) but {} parseable `commands=[…]` \
                 payload(s); shell sent another way escapes this budget",
                payloads.len()
            ));
        }
        let elems: usize = payloads.iter().map(Vec::len).sum();
        let bytes: usize = payloads.iter().flatten().map(String::len).sum();
        if elems > 0 {
            report.push(format!("    (\"{wf}\", {elems}, {bytes}),"));
        }
        let (cap_e, cap_b) = SSM_WORKFLOW_SHELL_BUDGET
            .iter()
            .find(|r| r.0 == wf.as_str())
            .map_or((0, 0), |r| (r.1, r.2));
        if elems > cap_e || bytes > cap_b {
            problems.push(format!(
                "`{wf}`: {elems} SSM command element(s) / {bytes} bytes over ceiling {cap_e} / {cap_b}"
            ));
        }
    }
    for name in &names {
        if !workflows.iter().any(|w| w == name) {
            problems.push(format!("`{name}` is gone: remove its row"));
        }
    }
    assert!(
        !report.is_empty(),
        "no SSM shell found in any workflow; the parser is broken"
    );
    assert!(
        problems.is_empty(),
        "SSM shell budget broken:\n{}\nPut the new step in Rust (a `tickvault-host` \
         subcommand) and call that. Measured now:\n{}",
        problems.join("\n"),
        report.join("\n")
    );
}

#[test]
fn awk_jq_usage_never_grows() {
    let names: Vec<&str> = AWK_JQ_BUDGET.iter().map(|r| r.0).collect();
    assert_sorted_unique(&names, "AWK_JQ_BUDGET");
    let mut measured: Vec<(String, usize)> = Vec::new();
    let mut program_files = Vec::new();
    for p in all_files() {
        if p.ends_with(".awk") || p.ends_with(".jq") {
            program_files.push(p.clone());
        }
        if p.ends_with(".md") || p.starts_with("docs/") {
            continue;
        }
        let full = repo_root().join(&p);
        if !full.is_file() {
            continue;
        }
        let bytes = std::fs::read(&full)
            .unwrap_or_else(|e| panic!("awk/jq budget: cannot read `{p}`: {e}"));
        if bytes.contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let n = if p.ends_with(".rs") {
            if !is_production_rust(&p) {
                continue;
            }
            count_awk_jq_rust(&text)
                .unwrap_or_else(|| panic!("`{p}`: unterminated literal or comment"))
        } else {
            count_awk_jq_text(&text)
        };
        if n > 0 {
            measured.push((p, n));
        }
    }
    assert!(
        program_files.is_empty(),
        "awk/jq program file(s) {program_files:?}: write the logic in Rust"
    );
    // Anti-vacuity: the All Green verdict is a jq program today.
    assert!(
        measured
            .iter()
            .any(|(p, _)| p == ".github/workflows/ci.yml"),
        "awk/jq scan did not see ci.yml's jq; the scan is broken"
    );
    let report: Vec<String> = measured
        .iter()
        .map(|(p, n)| format!("    (\"{p}\", {n}),"))
        .collect();
    let over = over_budget(&measured, AWK_JQ_BUDGET);
    let stale = stale_budget_rows(&measured, AWK_JQ_BUDGET);
    assert!(
        over.is_empty() && stale.is_empty(),
        "awk/jq budget broken. Over (path, found, ceiling): {over:?}. Stale rows \
         (file gone or now zero, remove them): {stale:?}. Do the text work in Rust \
         instead. Measured now:\n{}",
        report.join("\n")
    );
}

#[test]
fn all_green_jq_program_never_grows() {
    let ci = read_text(".github/workflows/ci.yml").expect("ci.yml must exist");
    let n = all_green_jq_program_lines(&ci)
        .expect("the All Green `verdict=\"$(jq -rn '` program was not found");
    assert!(
        n > 5,
        "All Green jq program measured {n} lines; the scan is broken"
    );
    assert!(
        n <= ALL_GREEN_JQ_PROGRAM_LINES,
        "the All Green jq program grew to {n} lines (ceiling {ALL_GREEN_JQ_PROGRAM_LINES}). \
         Its semantics are pinned by merge-gate-lock §5.1; growth needs a dated quote."
    );
}

#[test]
fn embedded_shell_and_awk_jq_self_test() {
    // Rust lexer: plain, raw, hashed raw, escapes, comments, chars, lifetimes.
    let src = concat!(
        "// \"not a literal\"\n",
        "/* \"nor this\" /* nested */ */\n",
        "const A: &str = \"a\\\"b\";\n",
        "const B: &str = r#\"x \"q\" y\n line2\"#;\n",
        "const C: &str = r\"raw\";\n",
        "fn f<'a>(x: &'a str) -> char { let _ = b'\"'; '\"' }\n",
        "const D: &[u8] = br##\"z\"##;\n",
        "let for_r = 1; // r\"not raw\"\n",
    );
    let lits = rust_string_literals(src).expect("terminated");
    assert_eq!(
        lits,
        vec![
            "a\\\"b".to_string(),
            "x \"q\" y\n line2".to_string(),
            "raw".to_string(),
            "z".to_string()
        ]
    );
    assert_eq!(literal_lines_and_bytes(&lits), (5, 22));
    assert!(rust_string_literals("const X: &str = \"open").is_none());
    assert!(rust_string_literals("/* open").is_none());
    assert_eq!(production_region("a\n#[cfg(test)]\nb\n"), "a\n");
    assert_eq!(production_region("a\n"), "a\n");

    // SSM payloads.
    let wf = concat!(
        "# --parameters 'commands=[\"commented\"]'\n",
        "  aws ssm send-command \\\n",
        "    --parameters 'commands=[\"echo a]\",\n",
        "      \"x=\\\"1\\\"\"\n",
        "    ]' \\\n",
        "  aws ssm send-command --parameters 'commands=[\"one\"]'\n",
    );
    let p = ssm_payloads(wf).expect("terminated");
    assert_eq!(
        p,
        vec![
            vec!["echo a]".to_string(), "x=\\\"1\\\"".to_string()],
            vec!["one".to_string()]
        ]
    );
    assert_eq!(count_ssm_send_commands(wf), 2);
    assert!(ssm_payloads("commands=[\"open").is_none());
    assert_eq!(
        count_ssm_send_commands("echo cannot send-command\n# aws ssm send-command\n"),
        0
    );

    // awk/jq words.
    assert_eq!(count_awk_jq_in_line("x | awk '{print $1}' | jq -r ."), 2);
    assert_eq!(count_awk_jq_in_line("gh run list --jq '.[0]'"), 1);
    assert_eq!(count_awk_jq_in_line("/usr/bin/gawk -f x"), 1);
    assert_eq!(count_awk_jq_in_line("$(jq -rn '"), 1);
    for no in [
        "jq_program",
        "filter.jq",
        "awkward",
        "hawk",
        "jq-1.7",
        "-jq",
        "dejq",
    ] {
        assert_eq!(count_awk_jq_in_line(no), 0, "should not count: {no}");
    }
    assert_eq!(
        count_awk_jq_text("#!/usr/bin/awk -f\n# awk in a comment\nawk 1\n"),
        2
    );
    assert_eq!(
        count_awk_jq_rust(
            "// awk\nconst A: &str = \"x | awk 1\";\n#[cfg(test)]\nconst B: &str = \"jq\";\n"
        ),
        Some(1)
    );

    // All Green program lines.
    assert_eq!(
        all_green_jq_program_lines("a\n  verdict=\"$(jq -rn '\n  .x\n  .y\n  ')\"\n"),
        Some(2)
    );
    assert_eq!(all_green_jq_program_lines("no program\n"), None);

    // Budget helpers.
    let m = vec![("a".to_string(), 3), ("b".to_string(), 1)];
    assert_eq!(
        over_budget(&m, &[("a", 2)]),
        vec![("a".to_string(), 3, 2), ("b".to_string(), 1, 0)]
    );
    assert!(over_budget(&m, &[("a", 3), ("b", 1)]).is_empty());
    assert_eq!(
        stale_budget_rows(&m, &[("a", 3), ("gone", 1)]),
        vec!["gone".to_string()]
    );
    assert!(is_production_rust("crates/app/src/main.rs"));
    assert!(is_production_rust("crates/app/build.rs"));
    assert!(!is_production_rust("crates/app/tests/x.rs"));
}

// ======================= NEXTEST FLAKY RESULT (2026-10-04) =======================
//
// Audit finding L2: `.config/nextest.toml` retried each failing test once
// in the `ci` profile, and a test that passed on the retry counted as a
// pass, so a flaky test went green silently. nextest 0.9.131 added
// `flaky-result = "fail"`; the config sets it and requires that version
// (an older nextest would ignore the key, so it must refuse to run).
// Kept here, beside the other CI-config budgets of this file, because it is
// one small pin and needs no plumbing of its own.

/// Lowest nextest version that honours `flaky-result`.
const NEXTEST_FLAKY_RESULT_MIN: (u64, u64, u64) = (0, 9, 131);

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// Problems with a nextest config's flaky handling (empty = OK).
fn nextest_flaky_problems(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => return vec![format!("nextest.toml does not parse: {e}")],
    };
    let required = match table.get("nextest-version") {
        Some(toml::Value::String(s)) => Some(s.clone()),
        Some(toml::Value::Table(t)) => t
            .get("required")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => None,
    };
    match required.as_deref().and_then(parse_version) {
        Some(v) if v >= NEXTEST_FLAKY_RESULT_MIN => {}
        _ => out.push(format!(
            "`nextest-version` must require at least {}.{}.{} (found {required:?})",
            NEXTEST_FLAKY_RESULT_MIN.0, NEXTEST_FLAKY_RESULT_MIN.1, NEXTEST_FLAKY_RESULT_MIN.2
        )),
    }
    let profiles = table.get("profile").and_then(|p| p.as_table());
    for name in ["default", "ci"] {
        let Some(profile) = profiles
            .and_then(|p| p.get(name))
            .and_then(|p| p.as_table())
        else {
            if name == "ci" {
                out.push("profile `ci` is missing".to_string());
            }
            continue;
        };
        let retries = match profile.get("retries") {
            None => 0,
            Some(toml::Value::Integer(n)) => *n,
            Some(toml::Value::Table(t)) => t.get("count").and_then(|c| c.as_integer()).unwrap_or(0),
            Some(_) => 1,
        };
        if retries > 0 && profile.get("flaky-result").and_then(|v| v.as_str()) != Some("fail") {
            out.push(format!(
                "profile `{name}` retries failing tests but lacks `flaky-result = \"fail\"`, \
                 so a test that passes on retry would go green"
            ));
        }
        if profile.contains_key("overrides") {
            out.push(format!(
                "profile `{name}` has per-test overrides; one could set `flaky-result = \"pass\"`"
            ));
        }
    }
    out
}

#[test]
fn nextest_ci_profile_fails_flaky_tests() {
    let text = read_text(".config/nextest.toml").expect(".config/nextest.toml must exist");
    let problems = nextest_flaky_problems(&text);
    assert!(
        problems.is_empty(),
        "nextest flaky handling regressed: {problems:?}"
    );
}

#[test]
fn nextest_flaky_self_test() {
    let good = "nextest-version = { required = \"0.9.131\" }\n[profile.ci]\n\
                retries = { backoff = \"fixed\", count = 1, delay = \"1s\" }\nflaky-result = \"fail\"\n";
    assert!(nextest_flaky_problems(good).is_empty());
    let no_flag = "nextest-version = \"0.9.140\"\n[profile.ci]\nretries = 1\n";
    assert_eq!(nextest_flaky_problems(no_flag).len(), 1);
    let old = "nextest-version = \"0.9.130\"\n[profile.ci]\nretries = 0\n";
    assert_eq!(nextest_flaky_problems(old).len(), 1);
    let unpinned = "[profile.ci]\nretries = 0\n";
    assert_eq!(nextest_flaky_problems(unpinned).len(), 1);
    let default_retry =
        "nextest-version = \"0.9.131\"\n[profile.default]\nretries = 2\n[profile.ci]\n";
    assert_eq!(nextest_flaky_problems(default_retry).len(), 1);
    let overrides = "nextest-version = \"0.9.131\"\n[profile.ci]\n\
                     [[profile.ci.overrides]]\nfilter = 'all()'\nflaky-result = \"pass\"\n";
    assert_eq!(nextest_flaky_problems(overrides).len(), 1);
    assert_eq!(parse_version("0.9.131"), Some((0, 9, 131)));
    assert_eq!(parse_version("0.9"), None);
    assert_eq!(parse_version("0.9.1.2"), None);
}
