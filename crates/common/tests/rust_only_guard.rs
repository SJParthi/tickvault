//! RUST-ONLY FOREVER-GUARD — Phase 3 tracked-python allowlist ratchet.
//!
//! Operator directive (2026-07-18, relayed via the coordinator session):
//! the tickvault runtime is RUST-ONLY FOREVER. This guard lands EARLY —
//! ahead of the final zero-python PR — with a SHRINKING allowlist of the
//! python that exists on `main` TODAY, so that:
//!
//! 1. NO NEW tracked `.py` file can ever land (`no_python_outside_allowlist`).
//! 2. Every python DELETION forces the allowlist to shrink in the SAME PR
//!    (`allowlist_shrinks_monotonically` fails on ghost entries) — the
//!    designed friction that ratchets the tree toward zero python.
//! 3. NO NEW python-invocation SITE can appear in shell scripts, workflow
//!    yml/yaml, Makefiles, `.mcp.json`, or terraform templates
//!    (`no_new_python_invocations`, file-level allowlist, same shrink rule).
//!
//! Design: house pure-core + thin-shell pattern. All classification logic is
//! pure functions over `Vec<String>` / `&str` inputs (self-tested with
//! synthetic fixtures in `guard_self_test`); the real tests feed them actual
//! `git ls-files` output + on-disk file contents from THIS checkout, so the
//! guard is green on its own merge base by construction.
//!
//! HONEST LIMITATIONS (house source-scan conventions — stated, not hidden):
//! - Comment awareness is LINE-level only: a line whose first non-whitespace
//!   char is `#` is skipped — EXCEPT a shebang (`#!...`), which is executable
//!   interpreter selection, not a comment, and is scanned like any code line
//!   (hostile review round 2: a pure-python file whose only python token was
//!   `#!/usr/bin/env python3` previously passed GREEN). A trailing same-line
//!   comment (`cmd  # python`) on a code line COUNTS as a hit; heredoc bodies
//!   and yml block scalars are scanned as ordinary lines. Prose mentions of
//!   "python" inside string literals of scanned file types therefore count —
//!   deliberate fail-loud direction (a false positive is a visible allowlist
//!   edit, never a silent miss).
//! - The invocation allowlist is FILE-level: an already-allowlisted file can
//!   gain an additional python invocation undetected until the file goes
//!   fully clean (at which point the shrink rule forces its removal). Net
//!   direction is still monotonic toward zero sites.
//! - Scope excludes `.py` files themselves (covered by the tracked-file
//!   allowlist) and `docs/**/*.md` prose (docs are not runtime surfaces).
//! - `*.rs`/`*.toml` are not scanned here — a Rust-side python spawn would be
//!   a reviewed code change; extending the scan is the final zero-python
//!   PR's business.
//! - Hardened 2026-07-18 (hostile review round 1): the invocation token
//!   matches `python` with ANY single optional ASCII digit suffix
//!   (`python`, `python2`, `python3`, ... — not just `3`); the tracked,
//!   extension-less `scripts/git-hooks/*` bash scripts are IN the scan
//!   scope; and path enumeration is NUL-delimited (`git ls-files -z`), so
//!   non-ASCII paths can never be silently mangled by git's `"..."` quoting.
//!
//! Cross-PR note: sibling deletion PRs (#1637 dead-python, #1645 aws-lambdas)
//! will make `allowlist_shrinks_monotonically` FAIL on their restack until
//! they shrink these allowlists — BY DESIGN. The fix is always mechanical:
//! delete the corresponding entries below in the same PR as the deletion.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Every tracked `.py` file on `main` as of 2026-07-18 (`git ls-files '*.py' | sort`).
/// ADDITIONS ARE FORBIDDEN FOREVER (rust-only operator directive 2026-07-18).
/// Deletions MUST remove the entry in the same PR (shrinking ratchet).
const TRACKED_PY_ALLOWLIST: &[&str] = &[
    ".claude/skills/dhanhq/examples/fetch_option_chain.py",
    ".claude/skills/dhanhq/examples/gtt_forever_order.py",
    ".claude/skills/dhanhq/examples/historical_data_analysis.py",
    ".claude/skills/dhanhq/examples/iron_condor.py",
    ".claude/skills/dhanhq/examples/live_feed_setup.py",
    ".claude/skills/dhanhq/examples/margin_check.py",
    ".claude/skills/dhanhq/examples/order_management.py",
    ".claude/skills/dhanhq/examples/place_equity_order.py",
    ".claude/skills/dhanhq/examples/place_fno_order.py",
    ".claude/skills/dhanhq/examples/portfolio_summary.py",
    ".claude/skills/dhanhq/examples/super_order_with_sl.py",
    ".claude/skills/dhanhq/scripts/dhan_helpers.py",
    ".claude/skills/dhanhq/scripts/resolve_security.py",
    ".claude/skills/dhanhq/scripts/trade_logger.py",
    ".claude/skills/dhanhq/scripts/validate_order.py",
    "deploy/aws/lambda/deploy-watchdog/handler.py",
    "deploy/aws/lambda/deploy-watchdog/test_handler.py",
    "deploy/aws/lambda/hard-stop-guard/handler.py",
    "deploy/aws/lambda/hard-stop-guard/test_handler.py",
    "deploy/aws/lambda/market-open-readiness/handler.py",
    "deploy/aws/lambda/market-open-readiness/test_handler.py",
    "deploy/aws/lambda/operator-control/handler.py",
    "deploy/aws/lambda/operator-control/test_handler.py",
    "deploy/aws/lambda/questdb-console-front/handler.py",
    "deploy/aws/lambda/questdb-console-front/test_handler.py",
    "deploy/aws/lambda/questdb-console-proxy/handler.py",
    "deploy/aws/lambda/questdb-console-proxy/test_handler.py",
    "deploy/aws/lambda/start-watchdog/handler.py",
    "deploy/aws/lambda/start-watchdog/test_handler.py",
    "deploy/aws/lambda/telegram-webhook/handler.py",
    "deploy/aws/lambda/telegram-webhook/test_handler.py",
    "docs/incidents/2026-07-06-questdb-console-shell-hang/raw_socket_probe.py",
    "docs/incidents/2026-07-06-questdb-console-shell-hang/repro_backlambda.py",
    "scripts/mcp-servers/tickvault-logs/server.py",
    "scripts/mcp-servers/tickvault-logs/test_placeholder_fallback.py",
];

/// Files (non-`.py`, non-docs) that carry a python invocation on a
/// NON-COMMENT line as of 2026-07-18. File-level allowlist. Same ratchet:
/// no additions ever; a file that goes python-clean (or is deleted) MUST
/// have its entry removed in the same PR.
const INVOCATION_SITE_ALLOWLIST: &[&str] = &[
    ".github/workflows/ci.yml",
    ".mcp.json",
    "Makefile",
    "scripts/bench-gate.sh",
    "scripts/validate-automation.sh",
];

// ============================ PURE CORE ============================

/// Tracked `.py` paths NOT covered by the allowlist (must be empty).
fn py_files_not_in_allowlist(tracked_py: &[String], allowlist: &[&str]) -> Vec<String> {
    let allowed: BTreeSet<&str> = allowlist.iter().copied().collect();
    tracked_py
        .iter()
        .filter(|p| !allowed.contains(p.as_str()))
        .cloned()
        .collect()
}

/// Allowlist entries whose file is no longer tracked (must be empty —
/// the shrinking ratchet: deletions force allowlist shrink).
fn stale_entries(allowlist: &[&str], tracked: &[String]) -> Vec<String> {
    let tracked: BTreeSet<&str> = tracked.iter().map(String::as_str).collect();
    allowlist
        .iter()
        .filter(|e| !tracked.contains(**e))
        .map(|e| (*e).to_string())
        .collect()
}

/// Is this tracked path in scope for the invocation scan?
/// Shell scripts, workflow/config yml+yaml, Makefiles, `.mcp.json`,
/// terraform templates, plus the extension-less tracked bash scripts under
/// `scripts/git-hooks/` (pre-push / pre-commit / commit-msg — hostile
/// review round 1). `.py` and `.md` are excluded by construction.
fn is_invocation_scan_target(path: &str) -> bool {
    path.ends_with(".sh")
        || path.ends_with(".yml")
        || path.ends_with(".yaml")
        || path.ends_with(".tftpl")
        || path == ".mcp.json"
        || path == "Makefile"
        || path.ends_with("/Makefile")
        || path.starts_with("scripts/git-hooks/")
}

/// Whole-line comment: first non-whitespace char is `#` — but a shebang
/// (`#!`) is NOT a comment: it selects the interpreter that EXECUTES the
/// file, so it must be scanned for the python token like any code line
/// (hostile review round 2, MED: `#!/usr/bin/env python3` previously
/// slipped through as a "comment").
fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') && !t.starts_with("#!")
}

/// Word-boundary match for `python` / `python[0-9]` (widened 2026-07-18 from
/// the original `python3?` grep pattern
/// `(^|[^[:alnum:]_.-])python[0-9]?([^[:alnum:]_-]|$)` so `python2`-class
/// tokens are also caught): the char before must not be alnum/`_`/`.`/`-`;
/// the char after the token (with one optional trailing ASCII digit) must
/// not be alnum/`_`/`-`.
fn line_has_python_token(line: &str) -> bool {
    let bytes = line.as_bytes();
    let needle = b"python";
    let mut start = 0usize;
    while let Some(rel) = line[start..].find("python") {
        let i = start + rel;
        let before_ok = i == 0 || {
            let c = bytes[i - 1] as char;
            !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
        };
        let mut end = i + needle.len();
        if end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let after_ok = end >= bytes.len() || {
            let c = bytes[end] as char;
            !(c.is_ascii_alphanumeric() || c == '_' || c == '-')
        };
        if before_ok && after_ok {
            return true;
        }
        start = i + needle.len();
    }
    false
}

/// Does this file content carry a python token on any non-comment line?
fn content_has_python_invocation(content: &str) -> bool {
    content
        .lines()
        .any(|l| !is_comment_line(l) && line_has_python_token(l))
}

/// Given (path, content) pairs already scoped by `is_invocation_scan_target`,
/// return the paths that hit but are NOT in the site allowlist.
fn new_invocation_sites(files: &[(String, String)], allowlist: &[&str]) -> Vec<String> {
    let allowed: BTreeSet<&str> = allowlist.iter().copied().collect();
    files
        .iter()
        .filter(|(p, c)| !allowed.contains(p.as_str()) && content_has_python_invocation(c))
        .map(|(p, _)| p.clone())
        .collect()
}

/// Site-allowlist entries that no longer hit (deleted OR gone python-clean)
/// — must be removed from the allowlist (shrinking ratchet, site half).
fn stale_invocation_sites(files: &[(String, String)], allowlist: &[&str]) -> Vec<String> {
    allowlist
        .iter()
        .filter(|e| {
            !files
                .iter()
                .any(|(p, c)| p == *e && content_has_python_invocation(c))
        })
        .map(|e| (*e).to_string())
        .collect()
}

/// Pure parse of `git ls-files -z` stdout: NUL-delimited bytes -> sorted
/// path list. Extracted from the shell so the parse contract is
/// unit-fixtured (hostile review round 2, LOW): trailing NUL never yields
/// an empty entry, and a C-quoted (`"`-leading) path PANICS — with `-z`
/// no path may ever arrive C-quoted; a leading `"` means the enumeration
/// contract broke, and we fail LOUD rather than scan a mangled path list.
fn parse_nul_delimited_paths(bytes: &[u8]) -> Vec<String> {
    let mut files: Vec<String> = String::from_utf8_lossy(bytes)
        .split('\0')
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect();
    if let Some(quoted) = files.iter().find(|p| p.starts_with('"')) {
        panic!(
            "rust_only_guard: `git ls-files -z` returned a C-quoted path `{quoted}` — \
             NUL-delimited enumeration must emit paths verbatim; refusing to scan a \
             mangled path list"
        );
    }
    files.sort();
    files
}

fn assert_sorted_unique(allowlist: &[&str], name: &str) {
    for w in allowlist.windows(2) {
        assert!(
            w[0] < w[1],
            "{name} must stay sorted + deduplicated: `{}` >= `{}`",
            w[0],
            w[1]
        );
    }
}

// ============================ THIN SHELL ============================

fn repo_root() -> PathBuf {
    // crates/common -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("rust_only_guard: cannot canonicalize repo root")
}

fn git_ls_files(pathspecs: &[&str]) -> Vec<String> {
    let root = repo_root();
    let mut cmd = Command::new("git");
    // `-z` = NUL-delimited output: non-ASCII paths are emitted VERBATIM
    // instead of C-quoted (`"..."`), which would silently defeat the
    // extension/prefix checks (hostile review round 1, fix 3).
    cmd.arg("ls-files")
        .arg("-z")
        .arg("--")
        .args(pathspecs)
        .current_dir(&root);
    let out = cmd
        .output()
        .expect("rust_only_guard: failed to run `git ls-files` (guard requires a git checkout)");
    assert!(
        out.status.success(),
        "rust_only_guard: `git ls-files` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Parse via the pure, self-tested NUL-parse core (fixtures in
    // `guard_self_test` cover trailing-NUL + the C-quote fail-loud panic).
    parse_nul_delimited_paths(&out.stdout)
}

/// All tracked invocation-scan targets, loaded as (path, content).
fn load_invocation_scan_files() -> Vec<(String, String)> {
    let root = repo_root();
    git_ls_files(&["."])
        .into_iter()
        .filter(|p| is_invocation_scan_target(p))
        .map(|p| {
            let content = std::fs::read_to_string(root.join(&p))
                .unwrap_or_else(|e| panic!("rust_only_guard: cannot read `{p}`: {e}"));
            (p, content)
        })
        .collect()
}

// ============================ REAL-TREE TESTS ============================

/// (a) NO NEW tracked `.py` — the rust-only forever-guard.
#[test]
fn no_python_outside_allowlist() {
    assert_sorted_unique(TRACKED_PY_ALLOWLIST, "TRACKED_PY_ALLOWLIST");
    let tracked_py = git_ls_files(&["*.py"]);
    let new = py_files_not_in_allowlist(&tracked_py, TRACKED_PY_ALLOWLIST);
    assert!(
        new.is_empty(),
        "RUST-ONLY VIOLATION: new tracked .py file(s) {new:?}. The rust-only operator \
         directive (2026-07-18) forbids ANY new python in this repo, forever. This test \
         (crates/common/tests/rust_only_guard.rs) is the gate: do NOT extend \
         TRACKED_PY_ALLOWLIST — port the logic to Rust instead."
    );
}

/// (b) The shrinking ratchet: every allowlist entry must still be tracked.
/// A deleted .py MUST have its entry removed in the SAME PR.
#[test]
fn allowlist_shrinks_monotonically() {
    let tracked_py = git_ls_files(&["*.py"]);
    let stale = stale_entries(TRACKED_PY_ALLOWLIST, &tracked_py);
    assert!(
        stale.is_empty(),
        "SHRINK THE RATCHET: these TRACKED_PY_ALLOWLIST entries point at files no longer \
         tracked: {stale:?}. Whoever deleted them must REMOVE the entries from \
         crates/common/tests/rust_only_guard.rs in the same PR — the allowlist only ever \
         shrinks (rust-only operator directive 2026-07-18)."
    );
}

/// (c) NO NEW python-invocation site in .sh / .yml / .yaml / .tftpl /
/// Makefile / .mcp.json / scripts/git-hooks/* (non-comment lines;
/// file-level allowlist), and the site allowlist shrinks when a file goes
/// python-clean or is deleted.
#[test]
fn no_new_python_invocations() {
    assert_sorted_unique(INVOCATION_SITE_ALLOWLIST, "INVOCATION_SITE_ALLOWLIST");
    let files = load_invocation_scan_files();
    let new = new_invocation_sites(&files, INVOCATION_SITE_ALLOWLIST);
    assert!(
        new.is_empty(),
        "RUST-ONLY VIOLATION: new python invocation site(s) {new:?} (non-comment `python`/\
         `python[0-9]` token). The rust-only operator directive (2026-07-18) forbids new \
         python invocations; this test is the gate. Do NOT extend INVOCATION_SITE_ALLOWLIST."
    );
    let stale = stale_invocation_sites(&files, INVOCATION_SITE_ALLOWLIST);
    assert!(
        stale.is_empty(),
        "SHRINK THE RATCHET: these INVOCATION_SITE_ALLOWLIST entries no longer carry a \
         non-comment python token (file cleaned or deleted): {stale:?}. Remove the entries \
         from crates/common/tests/rust_only_guard.rs in the same PR."
    );
}

// ============================ SELF-TESTS (fixtures) ============================

/// (d) The scanner detects a synthetic NEW .py / stale entry / new site —
/// proving the guard is non-vacuous (injected-list pure-fn design).
#[test]
fn guard_self_test() {
    // New .py detection.
    let tracked = vec![
        "deploy/aws/lambda/claude-triage/handler.py".to_string(),
        "scripts/evil_new_script.py".to_string(),
    ];
    let allow = ["deploy/aws/lambda/claude-triage/handler.py"];
    assert_eq!(
        py_files_not_in_allowlist(&tracked, &allow),
        vec!["scripts/evil_new_script.py".to_string()],
        "self-test: a new .py outside the allowlist must be detected"
    );

    // Stale-entry (shrink) detection.
    let tracked = vec!["scripts/kept.py".to_string()];
    let allow = ["scripts/deleted.py", "scripts/kept.py"];
    assert_eq!(
        stale_entries(&allow, &tracked),
        vec!["scripts/deleted.py".to_string()],
        "self-test: a ghost allowlist entry must be detected"
    );

    // Scan-target scoping: .py and .md are OUT; sh/yml/Makefile/.mcp.json in.
    assert!(is_invocation_scan_target("scripts/foo.sh"));
    assert!(is_invocation_scan_target(".github/workflows/ci.yml"));
    assert!(is_invocation_scan_target("deploy/aws/prometheus.yaml"));
    assert!(is_invocation_scan_target(
        "deploy/aws/terraform/user-data.sh.tftpl"
    ));
    assert!(is_invocation_scan_target("Makefile"));
    assert!(is_invocation_scan_target("sub/dir/Makefile"));
    assert!(is_invocation_scan_target(".mcp.json"));
    // Extension-less git-hook bash scripts are IN scope (fix 2, 2026-07-18).
    assert!(is_invocation_scan_target("scripts/git-hooks/pre-push"));
    assert!(is_invocation_scan_target("scripts/git-hooks/pre-commit"));
    assert!(is_invocation_scan_target("scripts/git-hooks/commit-msg"));
    assert!(!is_invocation_scan_target("scripts/foo.py"));
    assert!(!is_invocation_scan_target("docs/runbooks/foo.md"));
    assert!(!is_invocation_scan_target("crates/common/src/lib.rs"));

    // Token boundaries.
    assert!(line_has_python_token("python3 scripts/foo.py"));
    assert!(line_has_python_token("\tpython -m json.tool"));
    assert!(line_has_python_token("exec /usr/bin/python3.11 x"));
    assert!(line_has_python_token("\"command\": \"python3\","));
    // pythonN widening (fix 1, 2026-07-18): any single digit suffix matches.
    assert!(line_has_python_token("python2 legacy/x.py"));
    assert!(line_has_python_token("/usr/bin/python2.7 y"));
    assert!(line_has_python_token("python9 z"));
    assert!(
        !line_has_python_token("mypython3 x"),
        "prefix-joined must not match"
    );
    assert!(
        !line_has_python_token("pythonic naming"),
        "suffix-joined must not match"
    );
    assert!(
        !line_has_python_token("apt install python3-pip"),
        "pkg-name suffix `-` excluded"
    );
    assert!(
        !line_has_python_token("server.python x"),
        "dot-joined prefix excluded"
    );

    // Comment-awareness (line-level).
    assert!(is_comment_line("  # python3 old note"));
    assert!(!is_comment_line("run python3  # trailing note"));
    let commented_only = "# python3 was here\n  # python legacy\necho rust only\n";
    assert!(!content_has_python_invocation(commented_only));
    let live = "# header\npython3 scripts/x.py\n";
    assert!(content_has_python_invocation(live));

    // Shebang rule (MED fix, 2026-07-18): `#!` is interpreter selection,
    // NOT a comment — a python shebang alone must be a hit.
    assert!(
        !is_comment_line("#!/usr/bin/env python3"),
        "a shebang line must not be treated as a comment"
    );
    assert!(
        content_has_python_invocation("#!/usr/bin/env python3\nimport os\n"),
        "a python shebang must be detected as an invocation"
    );
    assert!(
        !content_has_python_invocation("#!/bin/bash\necho ok\n"),
        "a bash shebang must not false-positive"
    );
    assert!(
        !content_has_python_invocation("#!/usr/bin/env bash\necho ok\n# python3 in a comment\n"),
        "ordinary `#` comment skipping must be unchanged by the shebang rule"
    );

    // NUL-parse fixtures (LOW fix, 2026-07-18): the pure `git ls-files -z`
    // stdout parse. Normal NUL-joined input parses + sorts.
    assert_eq!(
        parse_nul_delimited_paths(b"b.sh\0a.sh"),
        vec!["a.sh".to_string(), "b.sh".to_string()],
        "NUL-joined input must parse and sort"
    );
    // Trailing NUL (git's actual output shape) yields NO empty entry.
    assert_eq!(
        parse_nul_delimited_paths(b"only.sh\0"),
        vec!["only.sh".to_string()],
        "trailing NUL must not produce an empty entry"
    );
    // A C-quoted (`"`-leading) entry breaks the -z contract and must PANIC.
    assert!(
        std::panic::catch_unwind(|| parse_nul_delimited_paths(b"\"mangled\\303\\244.sh\"\0"))
            .is_err(),
        "a C-quoted path must panic loudly, never be scanned"
    );

    // New-site + stale-site detection over synthetic files.
    let files = vec![
        ("scripts/allowed.sh".to_string(), "python3 x\n".to_string()),
        (
            "scripts/new_site.sh".to_string(),
            "  python3 y\n".to_string(),
        ),
        (
            "scripts/clean.sh".to_string(),
            "# python retired\necho ok\n".to_string(),
        ),
    ];
    let site_allow = ["scripts/allowed.sh", "scripts/went_clean.sh"];
    assert_eq!(
        new_invocation_sites(&files, &site_allow),
        vec!["scripts/new_site.sh".to_string()],
        "self-test: a new invocation site must be detected"
    );
    assert_eq!(
        stale_invocation_sites(&files, &site_allow),
        vec!["scripts/went_clean.sh".to_string()],
        "self-test: a cleaned/deleted site entry must be detected as stale"
    );
}
