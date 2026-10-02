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
    ("deploy/aws/holiday-gate.sh", 132),
    ("deploy/aws/host-tuning/apply-host-tuning.sh", 242),
    ("deploy/aws/sysctl/verify-net-tuning.sh", 143),
    ("deploy/aws/terraform/user-data.sh.tftpl", 269),
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
const SYSTEMD_SHELL_EXEC_BUDGET: &[(&str, usize)] = &[
    ("deploy/systemd/tickvault-holiday-gate.service", 1),
    ("deploy/systemd/tickvault-host-tuning.service", 3),
];

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
