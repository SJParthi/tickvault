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
//!    goes down in the same PR. Since 2026-10-20 (audit plan D1) every
//!    `*.service` file in the repo is scanned, not only `deploy/systemd/`
//!    (`every_systemd_unit_is_scanned`), and docker-compose shell
//!    healthchecks carry ceilings too (`compose_healthchecks_are_budgeted`).
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
//! - ~~Shell inside other files is NOT budgeted here yet: workflow `run:`
//!   steps, the Makefile, SSM command strings inside Rust (`operator_control`
//!   and friends), and `Command::new("sh")` spawns. Those are later D6 steps,
//!   recorded in `.claude/plans/active-plan-audit-fixes-2026-09-26.md`.~~
//!   CORRECTED 2026-10-12 (deep audit RUST-6 / RUST-7): every one of those is
//!   budgeted now. The console's SSM shell and the workflow SSM payloads
//!   (`console_embedded_shell_never_grows`, `ssm_workflow_shell_never_grows`,
//!   2026-10-04), workflow `run:` steps (`workflow_run_lines_never_grow`) and
//!   Makefile recipes (`makefile_recipe_lines_never_grow`) below, and Rust
//!   that starts a shell (`rust_shell_spawns_are_allowlisted` in
//!   `rust_only_guard.rs`, one dev-only row: the MCP doctor).
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
    ("scripts/bench-gate.selftest.sh", 208),
    ("scripts/bench-gate.sh", 440),
    ("scripts/bootstrap.sh", 323),
    ("scripts/claude-session-bootstrap.sh", 199),
    ("scripts/clippy-local.sh", 89),
    ("scripts/count-current-expiry-universe.sh", 128),
    ("scripts/coverage-gate.sh", 372),
    ("scripts/diagnose-write-amplification.sh", 99),
    ("scripts/doctor.sh", 298),
    ("scripts/ensure-aws-cli.sh", 71),
    ("scripts/ensure-ready.sh", 312),
    ("scripts/flaky-detect.sh", 93),
    ("scripts/mcp-doctor.sh", 97),
    ("scripts/mcp-servers/tickvault-logs-launch.sh", 33),
    ("scripts/measure-ram-residency-cost.sh", 113),
    ("scripts/notify-telegram.sh", 84),
    ("scripts/provision-infra-secrets.sh", 101),
    ("scripts/quality-full.sh", 137),
    ("scripts/questdb-console-gate-matrix.sh", 265),
    ("scripts/questdb-init.sh", 190),
    ("scripts/questdb-tunnel.sh", 304),
    ("scripts/register-dhan-ip.sh", 316),
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

/// Read a file the listing found and a budget measures. 2026-10-20 (review
/// of audit plan D1): the budget tests read these with `unwrap_or_default()`,
/// so a listed unit, compose file or workflow that was not a readable file
/// (a dangling symlink, a directory named `*.service`) measured as empty, i.e.
/// zero shell, and passed. It now fails.
fn read_listed(path: &str) -> String {
    read_text(path).unwrap_or_else(|| {
        panic!("shell_budget_guard: listed path `{path}` is not a readable file")
    })
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

/// Every systemd unit in the repo, wherever it lives. Until 2026-10-20 the scan
/// read `deploy/systemd/` only, so `scripts/tv-tunnel/tickvault-tunnel.service`
/// (run as root on the server) could gain a shell `Exec*=` line unseen (audit
/// plan D1). The listing is `all_files()`: tracked plus untracked-not-ignored,
/// so `target/`, `.git/` and other sessions' ignored worktrees are skipped.
fn systemd_units() -> Vec<String> {
    all_files()
        .into_iter()
        .filter(|p| p.ends_with(".service"))
        .collect()
}

#[test]
fn every_systemd_unit_is_scanned() {
    let units = systemd_units();
    for want in [
        "deploy/systemd/tickvault.service",
        "scripts/tv-tunnel/tickvault-tunnel.service",
    ] {
        assert!(
            units.iter().any(|u| u == want),
            "systemd scan did not find `{want}` (found {units:?}); the scan is broken"
        );
    }
}

#[test]
fn systemd_units_never_add_shell() {
    let units = systemd_units();
    assert!(
        !units.is_empty(),
        "no systemd units found; the scan is broken"
    );
    let budget_names: Vec<&str> = SYSTEMD_SHELL_EXEC_BUDGET.iter().map(|(p, _)| *p).collect();
    assert_sorted_unique(&budget_names, "SYSTEMD_SHELL_EXEC_BUDGET");
    let mut drift = Vec::new();
    for unit in &units {
        let text = read_listed(unit);
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

// 2026-10-29: the one-off wipe's SSM shell row (rust-only lock §0.14) and its
// test were DELETED with the tool after the 2026-10-28 run;
// `retired_one_off_tools_stay_deleted` keeps the files out.

/// Per workflow file: `(path, SSM command elements, bytes)` ceilings,
/// measured 2026-10-04. A workflow absent here may send no SSM shell.
/// 2026-10-20: `deploy-aws.yml` 95 / 11,980 → 94 / 11,907 (measured exactly):
/// its two `systemctl enable` elements became one that also names
/// `tickvault-holiday-gate`, which the deploy copied but never enabled
/// (−1 element, −1 byte from the measured 11,979; the old row carried one
/// byte of slack), and that unit's copy lost its `if [ -f … ]`
/// wrapper so a missing file fails the deploy instead of silently failing
/// the combined enable (−71 bytes).
const SSM_WORKFLOW_SHELL_BUDGET: &[(&str, usize, usize)] = &[
    (".github/workflows/aws-control.yml", 22, 1486),
    (".github/workflows/deploy-aws.yml", 94, 11907),
    (".github/workflows/downsize-instance.yml", 43, 3767),
    (".github/workflows/grow-ebs-volume.yml", 9, 353),
];

/// Per file: awk/jq word-occurrence ceilings, measured 2026-10-04. A file
/// absent here may use neither.
/// 2026-10-09: the `emergency-fs-recover.yml` row (1) is removed; its one
/// `awk` built the market-data drop list, which went with stress-audit RO-1.
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
    ("scripts/setup-observability.sh", 6),
    ("scripts/setup-secrets.sh", 1),
    ("scripts/test-register-dhan-ip.sh", 1),
    ("scripts/triage/verify.sh", 2),
    ("scripts/tv-tunnel/doctor.sh", 1),
    ("scripts/tv-tunnel/install-aws.sh", 1),
    ("scripts/tv-tunnel/install-mac.sh", 1),
    ("scripts/validate-deploy-ssm-quoting.sh", 2),
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
    let mut payloads = Vec::new();
    let mut i = 0usize;
    while i < c.len() {
        // SCOPE FIX (2026-10-26, audit plan item J5): the JSON form
        // `--parameters '{"commands":["…"]}'` is a payload too, not only the
        // shorthand `commands=[…]`. `payload_open` accepts both, with
        // whitespace around the `:` or `=`.
        let Some(open) = payload_open(&c, i) else {
            i += 1;
            continue;
        };
        let mut k = open;
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

/// When `commands` starts at `c[i]` and is followed by an optional closing
/// `"`, optional whitespace, `=` or `:`, optional whitespace and `[`, the
/// index just past that `[`. The word must start a token (`subcommands=[` is
/// not a payload).
///
/// HONEST LIMIT: the JSON form escaped inside a double-quoted shell string
/// (`"{\"commands\":[…]}"`) does not parse; it then counts as a send with no
/// payload, which fails the budget test (closed, not open).
fn payload_open(c: &[char], i: usize) -> Option<usize> {
    let word: Vec<char> = "commands".chars().collect();
    if *c.get(i..i + word.len())? != word[..] {
        return None;
    }
    if i > 0 && (c[i - 1].is_alphanumeric() || c[i - 1] == '_') {
        return None;
    }
    let mut k = i + word.len();
    if c.get(k) == Some(&'"') {
        k += 1;
    }
    while c.get(k).is_some_and(|ch| *ch == ' ' || *ch == '\t') {
        k += 1;
    }
    if !matches!(c.get(k), Some('=') | Some(':')) {
        return None;
    }
    k += 1;
    while c.get(k).is_some_and(|ch| ch.is_whitespace()) {
        k += 1;
    }
    (c.get(k) == Some(&'[')).then_some(k + 1)
}

/// Text with every backslash-continued line joined to the next, comment
/// lines dropped first.
///
/// SCOPE FIX (2026-10-26, audit plan item J5): `aws ssm \` on one line and
/// `send-command …` on the next is one command, and the per-line scan
/// counted zero sends for it.
fn join_continuations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if is_comment_line(line) {
            out.push('\n');
            continue;
        }
        match line.trim_end().strip_suffix('\\') {
            Some(head) => {
                out.push_str(head);
                out.push(' ');
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// `ssm send-command` invocations on non-comment lines, after joining
/// continued lines and collapsing whitespace, so `aws ssm \` + `send-command`
/// and `aws  ssm   send-command` each count once.
fn count_ssm_send_commands(text: &str) -> usize {
    join_continuations(text)
        .lines()
        .map(|l| {
            l.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .matches("ssm send-command")
                .count()
        })
        .sum()
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

/// One-off tools that ran and were deleted, per rust-only lock §0.13 and
/// §0.14 ("removed after the run"): the 2026-10-22 rebuild and the market-data
/// wipe both ran on 2026-10-28, and each tool, box subcommand, SSM shell,
/// workflow (and the wipe's IAM role file) was deleted in the next PR. With
/// them went the storage crate's rebuild copy module, which only they used.
/// Each path here must stay absent, and `crates/app/Cargo.toml` must declare
/// no bin by either tool's name, so a deleted SSM sender cannot come back
/// unbudgeted. Re-adding one needs a fresh dated quote in the rust-only lock.
const RETIRED_ONE_OFF_PATHS: &[&str] = &[
    ".github/workflows/rebuild-2026-10-22.yml",
    ".github/workflows/wipe-2026-10-31.yml",
    "crates/app/src/bin/rebuild_2026_10_22.rs",
    "crates/app/src/bin/wipe_2026_10_31.rs",
    "crates/app/src/rebuild_2026_10_22.rs",
    "crates/app/src/rebuild_box.rs",
    "crates/app/src/rebuild_ssm.rs",
    "crates/app/src/wipe_2026_10_31.rs",
    "crates/app/src/wipe_box.rs",
    "crates/app/src/wipe_ssm.rs",
    "crates/storage/src/rebuild_copy.rs",
    "deploy/aws/terraform/wipe-2026-10-31.tf",
];

#[test]
fn retired_one_off_tools_stay_deleted() {
    assert_sorted_unique(RETIRED_ONE_OFF_PATHS, "RETIRED_ONE_OFF_PATHS");
    let present: Vec<&str> = RETIRED_ONE_OFF_PATHS
        .iter()
        .copied()
        .filter(|p| repo_root().join(p).exists())
        .collect();
    assert!(
        present.is_empty(),
        "deleted one-off rebuild/wipe files are back: {present:?}. Rust-only \
         lock §0.13/§0.14 removed them after the 2026-10-28 runs; re-adding \
         needs a fresh dated quote there first."
    );
    let cargo = read_text("crates/app/Cargo.toml").expect("crates/app/Cargo.toml must exist");
    for bin in ["tv-rebuild-2026-10-22", "tv-wipe-2026-10-31"] {
        assert!(
            !cargo.contains(bin),
            "crates/app/Cargo.toml declares the deleted `{bin}` bin again"
        );
    }
    let lib = read_text("crates/app/src/lib.rs").expect("crates/app/src/lib.rs must exist");
    for module in [
        "rebuild_2026_10_22",
        "rebuild_box",
        "rebuild_ssm",
        "wipe_2026_10_31",
        "wipe_box",
        "wipe_ssm",
    ] {
        assert!(
            !lib.contains(&format!("pub mod {module};")),
            "crates/app/src/lib.rs declares the deleted module `{module}` again"
        );
    }
    let storage_lib =
        read_text("crates/storage/src/lib.rs").expect("crates/storage/src/lib.rs must exist");
    assert!(
        !storage_lib.contains("pub mod rebuild_copy;"),
        "crates/storage/src/lib.rs declares the deleted module `rebuild_copy` again"
    );
}

/// Production Rust files that may SEND SSM shell, or build it for a sender.
///
/// SCOPE FIX (2026-10-26, audit plan item J5). The three ceilings above
/// bound the shell inside files someone already listed; a NEW production
/// file calling `send_command` with `AWS-RunShellScript` tripped none of
/// them. Every production file carrying an [`SSM_SENDER_MARKERS`] marker,
/// in code or in a comment, must be here. SHRINK-ONLY: a listed file that no
/// longer carries a marker must leave in the same PR, and adding a file needs
/// a dated operator quote in the rust-only lock first. Measured 2026-10-26.
const SSM_SENDER_FILES: &[&str] = &["crates/aws-lambdas/src/operator_control.rs"];

/// What marks a file as an SSM shell sender: the two shell document names,
/// the SDK call, the CLI verb, and the API action name.
///
/// HONEST LIMIT: a document name assembled from pieces, or a call through a
/// trait with another name, is not seen; `send_command` and `document_name(`
/// are the SDK spellings every sender in this tree uses.
const SSM_SENDER_MARKERS: &[&str] = &[
    "AWS-RunShellScript",
    "AWS-RunPowerShellScript",
    "send_command",
    "send-command",
    "SendCommand",
    "document_name(",
];

fn is_ssm_sender(src: &str) -> bool {
    SSM_SENDER_MARKERS.iter().any(|m| src.contains(m))
}

#[test]
fn ssm_shell_senders_are_enumerated_and_only_shrink() {
    assert_sorted_unique(SSM_SENDER_FILES, "SSM_SENDER_FILES");
    let found: Vec<String> = all_files()
        .into_iter()
        .filter(|p| is_production_rust(p))
        .filter(|p| read_text(p).is_some_and(|s| is_ssm_sender(&s)))
        .collect();
    assert!(
        found
            .iter()
            .any(|p| p == "crates/aws-lambdas/src/operator_control.rs"),
        "the SSM sender scan did not find the console Lambda; the scan is broken"
    );
    let new: Vec<&String> = found
        .iter()
        .filter(|p| !SSM_SENDER_FILES.contains(&p.as_str()))
        .collect();
    let stale: Vec<&&str> = SSM_SENDER_FILES
        .iter()
        .filter(|p| !found.iter().any(|f| f == **p))
        .collect();
    assert!(
        new.is_empty(),
        "NEW SSM SHELL SENDER(S): {new:?}. A production Rust file that sends \
         `AWS-RunShellScript` puts shell on the box outside every budget in this \
         file. Do the step in a Rust subcommand on the box instead; adding a \
         sender needs a dated quote in the rust-only lock first."
    );
    assert!(
        stale.is_empty(),
        "SHRINK THE LIST: {stale:?} no longer send SSM shell; remove them from \
         SSM_SENDER_FILES in the same PR."
    );
}

#[test]
fn ssm_sender_discovery_self_test() {
    assert!(is_ssm_sender(".document_name(\"AWS-RunShellScript\")"));
    assert!(is_ssm_sender("client.send_command().instance_ids(id)"));
    assert!(is_ssm_sender(
        "Command::new(\"aws\").args([\"ssm\", \"send-command\"])"
    ));
    assert!(is_ssm_sender("\"Action\": \"ssm:SendCommand\""));
    assert!(is_ssm_sender("doc = \"AWS-RunPowerShellScript\""));
    assert!(!is_ssm_sender("client.get_parameter().name(path)"));
    assert!(!is_ssm_sender("let commands = vec![];"));
    assert!(is_production_rust("crates/app/src/new_sender.rs"));
    assert!(is_production_rust("crates/app/build.rs"));
    assert!(!is_production_rust("crates/app/tests/x.rs"));
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
        let text = read_listed(wf);
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
    // SCOPE FIX (2026-10-26, J5): `aws ssm \` + `send-command` on the next
    // line is one send; extra spaces do not hide it; a commented continuation
    // does not count.
    assert_eq!(
        count_ssm_send_commands("  aws ssm \\\n    send-command --instance-ids i \\\n"),
        1
    );
    assert_eq!(count_ssm_send_commands("aws   ssm\tsend-command x\n"), 1);
    assert_eq!(
        count_ssm_send_commands("# aws ssm \\\n# send-command x\n"),
        0
    );
    assert_eq!(
        count_ssm_send_commands("aws ssm send-command a; aws ssm send-command b\n"),
        2
    );
    // The JSON parameter form parses as a payload, with or without spaces.
    assert_eq!(
        ssm_payloads("--parameters '{\"commands\":[\"echo a\",\"echo b\"]}'").expect("json"),
        vec![vec!["echo a".to_string(), "echo b".to_string()]]
    );
    assert_eq!(
        ssm_payloads("--parameters '{\"commands\" : [ \"x\" ]}'").expect("json spaced"),
        vec![vec!["x".to_string()]]
    );
    assert_eq!(
        ssm_payloads("--parameters 'commands = [\"y\"]'").expect("spaced shorthand"),
        vec![vec!["y".to_string()]]
    );
    // Not a payload: a longer word ending in `commands`.
    assert_eq!(
        ssm_payloads("subcommands=[\"z\"]").expect("terminated"),
        Vec::<Vec<String>>::new()
    );
    // A split send with only a JSON payload: one send, one payload, so the
    // budget test sees both instead of neither.
    let split =
        "  aws ssm \\\n    send-command \\\n    --parameters '{\"commands\":[\"rm -rf /x\"]}'\n";
    assert_eq!(count_ssm_send_commands(split), 1);
    assert_eq!(ssm_payloads(split).expect("terminated").len(), 1);

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

// ============================ WORKFLOW `run:` SHELL (RUST-7) ============================
//
// 2026-10-12, deep audit RUST-7. Inline `run:` shell in `.github/workflows/`
// was the largest body of shell in the repo (about 4,200 lines on the audit
// date) and had no ceiling at all, while several of those workflows act on
// production (deploy, terraform apply, downsize, EBS growth). The Makefile
// recipes (about 300 lines) had none either.
//
// `workflow_run_lines_never_grow`: per workflow file, the count of non-blank,
// non-comment lines inside `run:` steps (a one-line `run: cmd` counts 1, a
// `run: |` / `run: >` block counts its body) is held at or below its ceiling.
// A workflow absent from the table may carry NO `run:` shell; a row whose
// file is gone or now measures zero must be removed. A ceiling may be lowered
// freely and must never be raised: new logic goes into a Rust bin the step
// calls.
//
// `makefile_recipe_lines_never_grow`: the same for Makefile recipe lines
// (tab-led, non-blank, not a `#` comment).
//
// HONEST LIMIT: lines, not logic, as with the ops-script ceilings above. A
// flow-style `run: "a; b; c"` counts as one line.

/// Per workflow: ceiling on non-blank, non-comment `run:` shell lines,
/// measured 2026-10-12 after the RUST-8 port and the SEC-7 / PROOF-5 edits.
const WORKFLOW_RUN_LINE_BUDGET: &[(&str, usize)] = &[
    (".github/workflows/auto-merge.yml", 25),
    (".github/workflows/aws-autopilot.yml", 7),
    (".github/workflows/aws-control.yml", 264),
    (".github/workflows/bench.yml", 19),
    (".github/workflows/chaos-nightly.yml", 64),
    (".github/workflows/ci.yml", 293),
    (".github/workflows/claude-integration.yml", 17),
    (".github/workflows/claude-mobile-command.yml", 2),
    (".github/workflows/claude-triage-loop.yml", 6),
    (".github/workflows/dep-freshness-nightly.yml", 82),
    (".github/workflows/deploy-aws-after-close.yml", 58),
    // 2026-10-20: 643 → 642, the two deploy `systemctl enable` lines became one;
    // then 642 → 640: the holiday-stop skip before the self-start added 4 lines,
    // paid by merging the two 4-line instance-state branches into one line each.
    (".github/workflows/deploy-aws.yml", 640),
    (".github/workflows/downsize-instance.yml", 425),
    (".github/workflows/emergency-fs-recover.yml", 126),
    (".github/workflows/full-test-nightly.yml", 83),
    (".github/workflows/fuzz.yml", 107),
    (".github/workflows/grow-ebs-volume.yml", 157),
    (".github/workflows/local-branch-sync.yml", 34),
    (".github/workflows/mutation.yml", 79),
    (".github/workflows/postmerge-catchup.yml", 75),
    (".github/workflows/safety.yml", 410),
    (".github/workflows/secret-scan.yml", 29),
    (".github/workflows/security-audit.yml", 2),
    // 2026-10-26 (audit PR L4): 287 → 286. The apply step re-checks IST
    // right before `terraform apply` (+2), paid by joining the guard job's
    // four clock reads into one line (−3).
    (".github/workflows/terraform-apply.yml", 286),
    (".github/workflows/wipe-log-streams.yml", 115),
];

/// Makefile recipe-line ceiling, measured 2026-10-12.
/// 2026-10-30: 301 → 295. The `cross-verify-show`, `cross-verify-now` and
/// `prev-day-show` targets went with their two shell scripts: both features
/// were retired 2026-07-14, nothing writes the files they showed, and
/// nothing reads `TICKVAULT_CROSS_VERIFY_NOW` (the kept cross-verify files
/// are served by `GET /api/debug/cross-verify/latest`, in Rust).
const MAKEFILE_RECIPE_LINE_CEILING: usize = 295;

/// Indentation (in columns) of a line.
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Non-blank, non-comment shell lines inside `run:` steps of one workflow.
fn count_workflow_run_lines(text: &str) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let mut total = 0_usize;
    let mut i = 0_usize;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let after_dash = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        let Some(value) = after_dash.strip_prefix("run:") else {
            i += 1;
            continue;
        };
        if trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        // Column of the `run` key: the block body must be indented past it.
        let key_col = indent_of(line) + (trimmed.len() - after_dash.len());
        let value = value.trim();
        let is_block = value.starts_with('|') || value.starts_with('>');
        if !is_block {
            if !value.is_empty() && !value.starts_with('#') {
                total += 1;
            }
            i += 1;
            continue;
        }
        i += 1;
        while i < lines.len() {
            let body = lines[i];
            if body.trim().is_empty() {
                i += 1;
                continue;
            }
            if indent_of(body) <= key_col {
                break;
            }
            if !body.trim_start().starts_with('#') {
                total += 1;
            }
            i += 1;
        }
    }
    total
}

/// Non-blank, non-comment Makefile recipe lines (tab-led).
fn count_makefile_recipe_lines(text: &str) -> usize {
    text.lines()
        .filter_map(|l| l.strip_prefix('\t'))
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .count()
}

#[test]
fn workflow_run_lines_never_grow() {
    let names: Vec<&str> = WORKFLOW_RUN_LINE_BUDGET.iter().map(|r| r.0).collect();
    assert_sorted_unique(&names, "WORKFLOW_RUN_LINE_BUDGET");
    let mut measured: Vec<(String, usize)> = Vec::new();
    for wf in all_files().into_iter().filter(|p| {
        p.starts_with(".github/workflows/") && (p.ends_with(".yml") || p.ends_with(".yaml"))
    }) {
        let n = count_workflow_run_lines(&read_listed(&wf));
        if n > 0 {
            measured.push((wf, n));
        }
    }
    let total: usize = measured.iter().map(|r| r.1).sum();
    assert!(
        measured.len() > 10 && total > 1_000,
        "only {} workflows / {total} run lines measured; the counter is broken",
        measured.len()
    );
    let report: Vec<String> = measured
        .iter()
        .map(|(p, n)| format!("    (\"{p}\", {n}),"))
        .collect();
    let over = over_budget(&measured, WORKFLOW_RUN_LINE_BUDGET);
    let stale = stale_budget_rows(&measured, WORKFLOW_RUN_LINE_BUDGET);
    assert!(
        over.is_empty() && stale.is_empty(),
        "workflow `run:` shell budget broken. Over ceiling (path, lines, ceiling): \
         {over:?}. Stale rows (file gone or no run shell left): {stale:?}. Put new \
         logic in a Rust bin and call it from the step; lower a ceiling freely, \
         never raise one. Measured now:\n{}",
        report.join("\n")
    );
}

#[test]
fn makefile_recipe_lines_never_grow() {
    let text = read_text("Makefile").expect("Makefile present");
    let n = count_makefile_recipe_lines(&text);
    assert!(
        n > 50,
        "only {n} Makefile recipe lines; the counter is broken"
    );
    assert!(
        n <= MAKEFILE_RECIPE_LINE_CEILING,
        "Makefile recipe lines grew: {n} > ceiling {MAKEFILE_RECIPE_LINE_CEILING}. \
         Put the logic in Rust (a `tv_*` bin) and call it; lower the ceiling freely."
    );
}

#[test]
fn workflow_run_line_counter_bites() {
    let wf = concat!(
        "jobs:\n",
        "  a:\n",
        "    steps:\n",
        "      - run: echo one\n",
        "      - name: block\n",
        "        run: |\n",
        "          set -euo pipefail\n",
        "\n",
        "          # a comment\n",
        "          echo two\n",
        "            echo nested\n",
        "      - name: folded\n",
        "        run: >-\n",
        "          echo three\n",
        "        env:\n",
        "          X: y\n",
        "      - name: not a run\n",
        "        with:\n",
        "          script: |\n",
        "            run: not counted\n",
        "      # - run: commented out\n",
    );
    // echo one (1) + block body 3 non-comment lines + folded 1 = 5, plus the
    // `run: not counted` line, a one-line `run:` key inside a `script:` block
    // that does count 1 (the counter errs toward counting more) = 6.
    assert_eq!(count_workflow_run_lines(wf), 6);
    assert_eq!(count_workflow_run_lines("on: push\n"), 0);
    let mk = "all:\n\techo a\n\t# c\n\n\techo b\nvar = 1\n";
    assert_eq!(count_makefile_recipe_lines(mk), 2);
}

// ===================== COMPOSE HEALTHCHECK SHELL (2026-10-20) =====================
//
// 2026-10-20, audit plan item D1. A docker-compose healthcheck is shell the
// container runtime runs every few seconds, on the production server too, and
// it is neither a shell file nor a `run:` step, so nothing above counted it:
// `["CMD-SHELL", "…"]` hands the string to `/bin/sh -c`, and a plain-string
// `test:` means the same in the compose format. On 2026-10-20 the one compose
// file held three: QuestDB's `bash -c` TCP probe, and a `wget` probe each for
// Loki and Alloy.
//
// `compose_healthchecks_are_budgeted`: per compose file and service, the count
// of shell healthchecks and the bytes of their commands are ceilings. A
// service absent from the table may carry NO shell healthcheck; a row whose
// service is gone or no longer runs one must be removed; a `test:` the scan
// cannot read fails rather than being skipped. A ceiling may be lowered
// freely and never raised (rust-only lock §0.10).
//
// HONEST LIMITS: a compose file is found by name (`docker-compose*` or
// `compose*`, `.yml`/`.yaml`); `["CMD", "<binary>", …]` naming a non-shell
// binary is not counted (no shell runs); bytes are counted after unquoting,
// and a YAML escape is decoded only as `\x` -> `x`.
//
// 2026-10-20 (hostile review of D1): the first parser read YAML shapes it
// does not understand as something harmless. `test: >-` measured as the
// 2-byte command `>-` (the folded command on the next lines unseen); an
// alias (`healthcheck: *hc`, `test: *cmd`), a flow mapping
// (`healthcheck: {test: …}`) or a `<<:` merge was skipped or undercounted.
// Each of those now fails the scan with its line number instead: a block
// scalar, alias, anchor, tag or flow mapping as a `test:` value or item, a
// `healthcheck:` or service line carrying an inline value, and any `<<:`
// line anywhere in a compose file. Write the healthcheck out in full as a
// flow sequence (`test: ["CMD", …]`) or a block sequence of plain scalars.

/// Per compose file and service: (file, service, shell healthchecks, bytes of
/// their commands). Ceilings, sorted by file then service. Measured 2026-10-20.
const COMPOSE_SHELL_HEALTHCHECK_BUDGET: &[(&str, &str, usize, usize)] = &[
    ("deploy/docker/docker-compose.yml", "tv-alloy", 1, 57),
    ("deploy/docker/docker-compose.yml", "tv-loki", 1, 54),
    ("deploy/docker/docker-compose.yml", "tv-questdb", 1, 106),
];

/// True when `path` names a compose file by its basename.
fn is_compose_file(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    (base.starts_with("docker-compose") || base.starts_with("compose"))
        && (base.ends_with(".yml") || base.ends_with(".yaml"))
}

/// First characters of an unquoted YAML value this scan does not interpret: a
/// block scalar (`|`, `>`), alias (`*`), anchor (`&`), tag (`!`) or nested
/// flow collection (`{`, `[`). Reading any of them as a plain string would
/// measure the wrong command.
const UNREADABLE_YAML_STARTS: [char; 7] = ['|', '>', '*', '&', '!', '{', '['];

/// True when an unquoted YAML value starts with one of `UNREADABLE_YAML_STARTS`.
fn starts_unreadable_yaml(value: &str) -> bool {
    value.trim_start().starts_with(UNREADABLE_YAML_STARTS)
}

/// Elements of a YAML flow sequence (`[a, "b c", 'd']`), unquoted. `None`
/// when the brackets or a quote do not close, or an unquoted element is not a
/// plain scalar (`starts_unreadable_yaml`).
fn flow_sequence_items(value: &str) -> Option<Vec<String>> {
    let inner = value.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut items = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&quote) = chars.peek() else {
            break;
        };
        if quote != '"' && quote != '\'' {
            if UNREADABLE_YAML_STARTS.contains(&quote) {
                return None;
            }
            let mut item = String::new();
            for c in chars.by_ref() {
                if c == ',' {
                    break;
                }
                item.push(c);
            }
            items.push(item.trim().to_string());
            continue;
        }
        chars.next();
        let mut item = String::new();
        let mut closed = false;
        while let Some(c) = chars.next() {
            if c == '\\' && quote == '"' {
                item.push(chars.next()?);
            } else if c == '\'' && quote == '\'' && chars.peek() == Some(&'\'') {
                chars.next();
                item.push('\'');
            } else if c == quote {
                closed = true;
                break;
            } else {
                item.push(c);
            }
        }
        if !closed {
            return None;
        }
        items.push(item);
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.next() {
            None => break,
            Some(',') => {}
            Some(_) => return None,
        }
    }
    Some(items)
}

/// One YAML scalar, unquoted. `None` when a quote does not close, or an
/// unquoted value is not a plain scalar (`starts_unreadable_yaml`).
fn unquote_scalar(s: &str) -> Option<String> {
    let s = s.trim();
    if !(s.starts_with('"') || s.starts_with('\'')) {
        return (!starts_unreadable_yaml(s)).then(|| s.to_string());
    }
    let mut items = flow_sequence_items(&format!("[{s}]"))?;
    (items.len() == 1).then(|| items.remove(0))
}

/// The shell command a healthcheck `test` runs, or `None` when it runs none.
/// A list is `CMD-SHELL` + command, `CMD` + argv, or `NONE`.
fn healthcheck_shell_command(items: &[String]) -> Option<String> {
    let (head, rest) = items.split_first()?;
    let runs_shell = match head.as_str() {
        "CMD-SHELL" => true,
        "CMD" => rest.first().is_some_and(|p| {
            let base = p.rsplit('/').next().unwrap_or(p.as_str());
            SHELL_RUNTIMES.contains(&base)
        }),
        _ => false,
    };
    runs_shell.then(|| rest.join(" "))
}

/// The inline value of a `key:` mapping line (`trimmed` has no indent), `""`
/// when nothing or only a comment follows the colon. `None` when the line is
/// not that key.
fn inline_value<'a>(trimmed: &'a str, key: &str) -> Option<&'a str> {
    let rest = trimmed.strip_prefix(key)?.strip_prefix(':')?;
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let rest = rest.trim();
    Some(if rest.starts_with('#') { "" } else { rest })
}

/// True for a YAML merge-key line: `<<: *x`, `- <<: *x`, `<< : *x`.
fn is_merge_key(trimmed: &str) -> bool {
    let key = trimmed.strip_prefix('-').map_or(trimmed, str::trim_start);
    key.strip_prefix("<<")
        .is_some_and(|rest| rest.trim_start().starts_with(':'))
}

/// Every healthcheck `test:` under `services:` in one compose file, as
/// `(service, shell command or None)`. A plain-string `test:` is the compose
/// shorthand for `CMD-SHELL`. `Err` names a `test:` the scan cannot read.
fn compose_healthcheck_tests(text: &str) -> Result<Vec<(String, Option<String>)>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut in_services = false;
    let mut service_indent: Option<usize> = None;
    let mut service = String::new();
    let mut healthcheck_indent: Option<usize> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        i += 1;
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // A merge key copies keys in from an anchor elsewhere in the file, so
        // a healthcheck could arrive unseen; anywhere in a compose file it fails.
        if is_merge_key(trimmed) {
            return Err(format!(
                "line {i}: `<<:` merge key; write the keys out in full"
            ));
        }
        // An inline healthcheck (`healthcheck: *hc`, `healthcheck: {test: …}`,
        // `healthcheck: &hc`) hides or shares its `test:`; anywhere it fails.
        if let Some(v) = inline_value(trimmed, "healthcheck")
            && !v.is_empty()
        {
            return Err(format!(
                "line {i}: `healthcheck:` with an inline value `{v}`; write it as \
                 a block mapping with its own `test:`"
            ));
        }
        let indent = indent_of(line);
        if indent == 0 {
            let services = inline_value(trimmed, "services");
            if services.is_some_and(|v| !v.is_empty()) {
                return Err(format!("line {i}: `services:` with an inline value"));
            }
            in_services = services.is_some();
            service_indent = None;
            healthcheck_indent = None;
            continue;
        }
        if !in_services {
            continue;
        }
        if healthcheck_indent.is_some_and(|h| indent <= h) {
            healthcheck_indent = None;
        }
        if indent == *service_indent.get_or_insert(indent) {
            // A service given inline (`db: *base`, `db: {…}`) could carry a
            // healthcheck this scan never reads.
            let (name, rest) = trimmed.split_once(':').unwrap_or((trimmed, ""));
            let rest = rest.trim();
            if !(rest.is_empty() || rest.starts_with('#')) {
                return Err(format!(
                    "line {i}: service `{name}` with an inline value `{rest}`"
                ));
            }
            service = name.to_string();
            continue;
        }
        if inline_value(trimmed, "healthcheck").is_some() {
            healthcheck_indent = Some(indent);
            continue;
        }
        let Some(value) = inline_value(trimmed, "test") else {
            continue;
        };
        if healthcheck_indent.is_none() {
            continue;
        }
        let at = i;
        // A block scalar (`test: >-`, `test: |`) puts the command on the
        // lines below; an alias, anchor, tag or flow mapping is not a command
        // this scan can measure. Only a flow sequence (`[`) is read inline.
        if !value.starts_with('[') && starts_unreadable_yaml(value) {
            return Err(format!(
                "line {at}: healthcheck `test: {value}` is a block scalar, alias, \
                 anchor, tag or flow mapping; write it as a sequence of plain or \
                 quoted strings"
            ));
        }
        let mut value = value.to_string();
        let items = if value.starts_with('[') {
            // A flow sequence may run over several lines.
            while !value.ends_with(']') && i < lines.len() {
                value.push(' ');
                value.push_str(lines[i].trim());
                i += 1;
            }
            flow_sequence_items(&value)
        } else if value.is_empty() {
            // A block sequence: `- item` lines indented under `test:`.
            let mut items = Vec::new();
            let mut readable = true;
            while i < lines.len() && indent_of(lines[i]) > indent {
                let item = lines[i].trim();
                i += 1;
                if item.is_empty() || item.starts_with('#') {
                    continue;
                }
                match item.strip_prefix('-').and_then(unquote_scalar) {
                    Some(s) => items.push(s),
                    None => readable = false,
                }
            }
            (readable && !items.is_empty()).then_some(items)
        } else {
            unquote_scalar(&value).map(|s| vec!["CMD-SHELL".to_string(), s])
        };
        let items = items.ok_or_else(|| format!("line {at}: unreadable healthcheck `test:`"))?;
        out.push((service.clone(), healthcheck_shell_command(&items)));
    }
    Ok(out)
}

#[test]
fn compose_healthchecks_are_budgeted() {
    let keys: Vec<String> = COMPOSE_SHELL_HEALTHCHECK_BUDGET
        .iter()
        .map(|r| format!("{} {}", r.0, r.1))
        .collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    assert_sorted_unique(&key_refs, "COMPOSE_SHELL_HEALTHCHECK_BUDGET");
    let files: Vec<String> = all_files()
        .into_iter()
        .filter(|p| is_compose_file(p))
        .collect();
    assert!(
        files
            .iter()
            .any(|f| f == "deploy/docker/docker-compose.yml"),
        "compose scan found {files:?} without the main compose file; the scan is broken"
    );
    // (file, service, shell healthchecks, bytes of their commands).
    let mut measured: Vec<(String, String, usize, usize)> = Vec::new();
    for file in &files {
        let text = read_listed(file);
        let tests = compose_healthcheck_tests(&text).unwrap_or_else(|e| panic!("`{file}`: {e}"));
        for (svc, cmd) in tests {
            let Some(cmd) = cmd else {
                continue;
            };
            match measured.iter_mut().find(|r| r.0 == *file && r.1 == svc) {
                Some(row) => {
                    row.2 += 1;
                    row.3 += cmd.len();
                }
                None => measured.push((file.clone(), svc, 1, cmd.len())),
            }
        }
    }
    // Anti-vacuity: the scan must see the healthchecks it is guarding.
    assert!(
        measured.len() >= 3,
        "only {} services with a shell healthcheck measured; the parser is broken",
        measured.len()
    );
    let report: Vec<String> = measured
        .iter()
        .map(|(f, s, n, b)| format!("    (\"{f}\", \"{s}\", {n}, {b}),"))
        .collect();
    let mut over = Vec::new();
    for (f, s, n, b) in &measured {
        let (cap_n, cap_b) = COMPOSE_SHELL_HEALTHCHECK_BUDGET
            .iter()
            .find(|r| r.0 == f.as_str() && r.1 == s.as_str())
            .map_or((0, 0), |r| (r.2, r.3));
        if *n > cap_n || *b > cap_b {
            over.push((format!("{f} {s}"), *n, cap_n, *b, cap_b));
        }
    }
    let stale: Vec<String> = COMPOSE_SHELL_HEALTHCHECK_BUDGET
        .iter()
        .filter(|r| !measured.iter().any(|m| m.0 == r.0 && m.1 == r.1))
        .map(|r| format!("{} {}", r.0, r.1))
        .collect();
    assert!(
        over.is_empty() && stale.is_empty(),
        "compose shell healthcheck budget broken. Over ceiling (service, checks, \
         ceiling, bytes, ceiling): {over:?}. Stale rows (service gone or no shell \
         healthcheck left): {stale:?}. Use `[\"CMD\", \"<binary>\", …]` with no shell \
         or a Rust probe; lower a ceiling freely, never raise one. Measured now:\n{}",
        report.join("\n")
    );
}

#[test]
fn compose_healthcheck_scan_bites() {
    assert!(is_compose_file("deploy/docker/docker-compose.yml"));
    assert!(is_compose_file("x/docker-compose.override.yaml"));
    assert!(is_compose_file("compose.yml"));
    assert!(!is_compose_file("deploy/docker/loki/loki-config.yml"));
    assert!(!is_compose_file("docker-compose.md"));

    let compose = concat!(
        "networks:\n",
        "  n:\n",
        "services:\n",
        "  # a comment\n",
        "  db:\n",
        "    image: x\n",
        "    healthcheck:\n",
        "      test: [\"CMD-SHELL\", \"bash -c 'true' || exit 1\"]\n",
        "      interval: 5s\n",
        "    environment:\n",
        "      test: not a healthcheck\n",
        "  web:\n",
        "    healthcheck:\n",
        "      test: curl -f http://web/ || exit 1\n",
        "  bin:\n",
        "    healthcheck:\n",
        "      test: [CMD, /usr/bin/probe, --ready]\n",
        "  wrapped:\n",
        "    healthcheck:\n",
        "      test: [\"CMD\", \"/bin/sh\", \"-c\", \"true\"]\n",
        "  block:\n",
        "    healthcheck:\n",
        "      test:\n",
        "        - CMD-SHELL\n",
        "        - 'wget -q x'\n",
        "  off:\n",
        "    healthcheck:\n",
        "      test: [\"NONE\"]\n",
        "  split:\n",
        "    healthcheck:\n",
        "      test: [\"CMD-SHELL\",\n",
        "             \"echo a, b\"]\n",
        "volumes:\n",
        "  v:\n",
        "    healthcheck:\n",
        "      test: [\"CMD-SHELL\", \"outside services\"]\n",
    );
    let shell = |s: &str| Some(s.to_string());
    assert_eq!(
        compose_healthcheck_tests(compose).expect("readable"),
        vec![
            ("db".to_string(), shell("bash -c 'true' || exit 1")),
            ("web".to_string(), shell("curl -f http://web/ || exit 1")),
            ("bin".to_string(), None),
            ("wrapped".to_string(), shell("/bin/sh -c true")),
            ("block".to_string(), shell("wget -q x")),
            ("off".to_string(), None),
            ("split".to_string(), shell("echo a, b")),
        ]
    );
    // An unreadable `test:` fails, never skips.
    let open = "services:\n  a:\n    healthcheck:\n      test: [\"CMD-SHELL\", \"open\n";
    assert!(compose_healthcheck_tests(open).is_err());
    let empty = "services:\n  a:\n    healthcheck:\n      test:\n    image: x\n";
    assert!(compose_healthcheck_tests(empty).is_err());
    // 2026-10-20 (hostile review of D1): every YAML shape the scan cannot
    // measure fails with its line; before, a block scalar read as the 2-byte
    // command `>-` and an alias, flow mapping or merge read as nothing.
    let svc = |body: &str| format!("services:\n  a:\n    image: x\n{body}");
    let hc = |test: &str| svc(&format!("    healthcheck:\n{test}      interval: 5s\n"));
    let unreadable: &[(&str, String)] = &[
        (
            "folded test",
            hc("      test: >-\n        bash -c 'curl x'\n"),
        ),
        (
            "literal test",
            hc("      test: |\n        bash -c 'curl x'\n"),
        ),
        ("alias test", hc("      test: *probe\n")),
        (
            "anchor test",
            hc("      test: &probe [\"CMD-SHELL\", \"x\"]\n"),
        ),
        (
            "tagged test",
            hc("      test: !!seq [\"CMD-SHELL\", \"x\"]\n"),
        ),
        ("flow-mapping test", hc("      test: {cmd: x}\n")),
        (
            "alias flow item",
            hc("      test: [\"CMD-SHELL\", *probe]\n"),
        ),
        (
            "alias block item",
            hc("      test:\n        - CMD-SHELL\n        - *probe\n"),
        ),
        (
            "folded block item",
            hc("      test:\n        - CMD-SHELL\n        - >-\n          curl x\n"),
        ),
        ("alias healthcheck", svc("    healthcheck: *hc\n")),
        (
            "flow-mapping healthcheck",
            svc("    healthcheck: {test: [\"CMD-SHELL\", \"curl x\"]}\n"),
        ),
        (
            "anchored healthcheck",
            svc("    healthcheck: &hc\n      test: [\"CMD-SHELL\", \"x\"]\n"),
        ),
        ("merge in a service", svc("    <<: *base\n")),
        (
            "merge outside services",
            format!("x-base: &base\n  <<: *other\n{}", svc("")),
        ),
        ("merge as a list item", svc("    - <<: *base\n")),
        (
            "inline service",
            "services:\n  a: *base\n  b:\n    image: x\n".to_string(),
        ),
        ("inline services", "services: {a: {image: x}}\n".to_string()),
    ];
    for (shape, doc) in unreadable {
        assert!(
            compose_healthcheck_tests(doc).is_err(),
            "{shape} must fail, not pass:\n{doc}"
        );
    }
    // A trailing comment is not an inline value: these still read in full.
    let commented = concat!(
        "services: # the stack\n",
        "  a: # one service\n",
        "    healthcheck: # probe\n",
        "      test: # block below\n",
        "        - CMD-SHELL\n",
        "        - wget -q x\n",
    );
    assert_eq!(
        compose_healthcheck_tests(commented).expect("readable"),
        vec![("a".to_string(), shell("wget -q x"))]
    );
    // Bite: the real QuestDB probe measures exactly its pinned 106 bytes, so
    // one byte more breaks the ceiling.
    let questdb = concat!(
        "services:\n  tv-questdb:\n    healthcheck:\n",
        "      test: [\"CMD-SHELL\", \"bash -c 'cat < /dev/null > /dev/tcp/localhost/9000 ",
        "&& cat < /dev/null > /dev/tcp/localhost/9009' || exit 1\"]\n",
    );
    let got = compose_healthcheck_tests(questdb).expect("readable");
    let cmd = got[0].1.as_deref().expect("shell");
    let cap = COMPOSE_SHELL_HEALTHCHECK_BUDGET
        .iter()
        .find(|r| r.1 == "tv-questdb")
        .map(|r| r.3);
    assert_eq!(Some(cmd.len()), cap);
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
