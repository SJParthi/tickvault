//! RUST-ONLY FOREVER-GUARD — Phase 3 tracked-the banned interpreter allowlist ratchet.
//!
//! Operator directive (2026-07-18, relayed via the coordinator session):
//! the tickvault runtime is RUST-ONLY FOREVER. This guard lands EARLY —
//! ahead of the final zero-the banned interpreter PR — with a SHRINKING allowlist of the
//! the banned interpreter that exists on `main` TODAY, so that:
//!
//! 1. NO NEW tracked `.py` file can ever land (`no_banned_files_outside_allowlist`).
//! 2. Every the banned interpreter DELETION forces the allowlist to shrink in the SAME PR
//!    (`allowlist_shrinks_monotonically` fails on ghost entries) — the
//!    designed friction that ratchets the tree toward zero the banned interpreter.
//! 3. NO NEW the banned interpreter-invocation SITE can appear in shell scripts, workflow
//!    yml/yaml, Makefiles, `.mcp.json`, or terraform templates
//!    (`no_new_banned_invocations`, file-level allowlist, same shrink rule).
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
//!   (hostile review round 2: a pure-the banned interpreter file whose only the banned interpreter token was
//!   `#!/usr/bin/env the banned interpreter` previously passed GREEN). A trailing same-line
//!   comment (`cmd  # the banned interpreter`) on a code line COUNTS as a hit; heredoc bodies
//!   and yml block scalars are scanned as ordinary lines. Prose mentions of
//!   "the banned interpreter" inside string literals of scanned file types therefore count —
//!   deliberate fail-loud direction (a false positive is a visible allowlist
//!   edit, never a silent miss).
//! - The invocation allowlist is FILE-level: an already-allowlisted file can
//!   gain an additional the banned interpreter invocation undetected until the file goes
//!   fully clean (at which point the shrink rule forces its removal). Net
//!   direction is still monotonic toward zero sites.
//! - Scope excludes `.py` files themselves (covered by the tracked-file
//!   allowlist) and `docs/**/*.md` prose (docs are not runtime surfaces).
//! - `*.rs`/`*.toml` are not scanned here — a Rust-side the banned interpreter spawn would be
//!   a reviewed code change; extending the scan is the final zero-the banned interpreter
//!   PR's business.
//! - Hardened 2026-07-18 (hostile review round 1): the invocation token
//!   matches `the banned interpreter` with ANY single optional ASCII digit suffix
//!   (`the banned interpreter`, `the banned interpreter`, `the banned interpreter`, ... — not just `3`); the tracked,
//!   extension-less `scripts/git-hooks/*` bash scripts are IN the scan
//!   scope; and path enumeration is NUL-delimited (`git ls-files -z`), so
//!   non-ASCII paths can never be silently mangled by git's `"..."` quoting.
//!
//! Cross-PR note: sibling deletion PRs (#1637 dead-the banned interpreter, #1645 aws-lambdas)
//! will make `allowlist_shrinks_monotonically` FAIL on their restack until
//! they shrink these allowlists — BY DESIGN. The fix is always mechanical:
//! delete the corresponding entries below in the same PR as the deletion.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Every tracked `.py` file on `main` as of 2026-07-18 (`git ls-files '*.py' | sort`).
/// ADDITIONS ARE FORBIDDEN FOREVER (rust-only operator directive 2026-07-18).
/// Deletions MUST remove the entry in the same PR (shrinking ratchet).
const TRACKED_BANNED_ALLOWLIST: &[&str] = &[];

/// Files (non-`.py`, non-docs) that carry a the banned interpreter invocation on a
/// NON-COMMENT line as of 2026-07-18. File-level allowlist. Same ratchet:
/// no additions ever; a file that goes the banned interpreter-clean (or is deleted) MUST
/// have its entry removed in the same PR.
const INVOCATION_SITE_ALLOWLIST: &[&str] = &[];

/// Every tracked-file extension this guard refuses, as `git ls-files` pathspecs.
///
/// SCOPE FIX (2026-08-10): this guard previously scanned `*.py` and NOTHING
/// ELSE, so a tracked `.js` / `.ts` / `.rb` / `.pl` / `.lua` file passed GREEN —
/// and because `banned_tokens()` deliberately excludes `node`/`npx`, a committed
/// `.js` invoked via `npx` cleared BOTH real-tree tests. That is exactly the
/// failure shape this file's own header warns about ("a guard is only as good as
/// its SCOPE, and scope errors are invisible by construction: they produce
/// green, not red") — the 2026-08-01 `pip`-verb correction was the same class.
///
/// Verified at fix time: `git ls-files` returns ZERO matches for every pathspec
/// below, so widening the scope keeps both allowlists at their hard-zero floor
/// and the ratchet can never be re-grown.
/// SCOPE FIX #2 (2026-08-11): `*.py` was banned but `*.pyw` / `*.pyi` / `*.pyx`
/// were not — a ONE-CHARACTER rename evaded the primary ban entirely, and
/// `.pyw` is a directly-executable form on some platforms. The remaining
/// additions close the equivalent one-rename escape for the other interpreted
/// families already listed (`.jsx`/`.tsx`/`.mts`/`.cts` for the JS/TS pair)
/// plus the interpreted runtimes that were simply never enumerated.
///
/// Verified at fix time (`git ls-files -- <pathspec>` for EVERY entry below):
/// ZERO tracked matches, so both allowlists stay at their hard-zero floor and
/// the ratchet cannot be re-grown by this widening.
const BANNED_FILE_PATHSPECS: &[&str] = &[
    "*.py", "*.pyw", "*.pyi", "*.pyx", "*.js", "*.jsx", "*.mjs", "*.cjs", "*.es6", "*.coffee",
    "*.ts", "*.tsx", "*.mts", "*.cts", "*.rb", "*.pl", "*.php", "*.lua", "*.tcl", "*.groovy",
    "*.jl", "*.ipynb",
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
        // 2026-08-07: `.tf` and Dockerfiles ADDED. They were structurally
        // unscanned — only the `.tftpl` TEMPLATE form was covered — so a
        // terraform `local-exec` provisioner shelling into a banned installer,
        // or a `RUN <installer> install ...` line in a Dockerfile, would have
        // passed this guard GREEN. Both file classes had zero tracked matches
        // when the hole was found, so this closes a LATENT blind spot rather
        // than an active violation. That distinction matters: this guard's
        // whole job is to be true for files that do not exist yet.
        //
        // This is the same failure shape as the 2026-08-01 correction recorded
        // in `rust-only-forever-lock-2026-07-19.md` — there the token set
        // covered a runtime's own name but not its package manager; here the
        // file-type set covered a template but not the rendered form. A guard
        // is only as good as its SCOPE, and scope errors are invisible by
        // construction: they produce green, not red.
        || path.ends_with(".tf")
        || path == "Dockerfile"
        || path.ends_with("/Dockerfile")
        || path.rsplit('/').next().is_some_and(|f| f.starts_with("Dockerfile."))
        // 2026-08-11 SCOPE FIX #3 — EXECUTABLE-MANIFEST classes. Only
        // `.mcp.json` was matched, by exact path, so `.claude/settings.json`
        // (which carries the Claude Code HOOK command lines — a real
        // executable surface) was structurally unscanned, as were systemd
        // units (`ExecStart=`), launchd agents (`ProgramArguments`), IDE
        // run-configs (`.run/*.xml`), and the Alloy collector config. Each
        // class can name an interpreter as the program it launches, which is
        // precisely the invocation shape this scan exists to catch.
        //
        // Verified at fix time: all 16 tracked files across these five
        // classes (3 `.service`, 1 `.plist`, 5 `.xml`, 1 `.alloy`, 6 `.json`)
        // are token-clean, so this closes a LATENT blind spot and keeps
        // INVOCATION_SITE_ALLOWLIST empty. Same lesson as the 2026-08-07
        // `.tf`/Dockerfile row and the 2026-08-01 package-manager row: scope
        // errors are invisible by construction — they produce green, not red.
        || path.ends_with(".service")
        || path.ends_with(".plist")
        || path.ends_with(".xml")
        || path.ends_with(".alloy")
        || path.ends_with(".json")
        || path == "Makefile"
        || path.ends_with("/Makefile")
        || path.starts_with("scripts/git-hooks/")
}

/// Whole-line comment: first non-whitespace char is `#` — but a shebang
/// (`#!`) is NOT a comment: it selects the interpreter that EXECUTES the
/// file, so it must be scanned for the the banned interpreter token like any code line
/// (hostile review round 2, MED: `#!/usr/bin/env the banned interpreter` previously
/// slipped through as a "comment").
fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') && !t.starts_with("#!")
}

/// Word-boundary match for `the banned interpreter` / `the banned interpreter[0-9]` (widened 2026-07-18 from
/// the original `the banned interpreter?` grep pattern
/// `(^|[^[:alnum:]_.-])the banned interpreter[0-9]?([^[:alnum:]_-]|$)` so `the banned interpreter`-class
/// tokens are also caught): the char before must not be alnum/`_`/`.`/`-`;
/// the char after the token (with one optional trailing ASCII digit) must
/// not be alnum/`_`/`-`.
fn banned_token() -> String {
    // Assembled from bytes so the literal never appears in this repository
    // (operator directive 2026-07-31). Detection semantics are UNCHANGED.
    String::from_utf8(vec![0x70, 0x79, 0x74, 0x68, 0x6f, 0x6e]).unwrap()
}

/// Every interpreter/package-manager token that re-introduces the banned
/// runtime. 2026-08-01 (operator directive — "only Rust"): the guard used to
/// ban the interpreter's OWN name and nothing else, so ELEVEN live install
/// sites passed green for weeks — `pip3 install ziglang` in
/// `terraform-apply.yml` (which made the arm64 LINKER of every production
/// lambda an interpreter invocation) plus `pip3 install awscli` in four
/// setup scripts. Banning the runtime while permitting its package manager
/// is not a ban; these tokens close that hole.
///
/// `perl` JOINED the ban 2026-08-10. It was previously excluded because
/// `terraform-apply.yml` ran a 13-line `perl -ne` program to reject non-ASCII
/// security-group rule descriptions. That check is now
/// `crates/common/tests/sg_rule_description_ascii_guard.rs` — same semantics,
/// broader coverage (it runs in `Test (common)` on every PR, where the perl
/// step ran only on the path-filtered terraform workflow). With the last site
/// gone, the ratchet SHRINKS, which is the only direction it may move.
///
/// Deliberately NOT included (each would fail the guard TODAY and needs its
/// own decision, never a silent allowlist entry):
///   - `venv` — `deploy-aws.yml` has a `rm -rf …/venv` CLEANUP line, which
///     only DELETES; banning the token would fail on the line that removes the
///     very thing the directive objects to
///   - `node`/`npx` — `.mcp.json` dev-only MCP servers for the Claude session;
///     never deployed, never in the product path. Removing them breaks local
///     tooling and buys nothing on the box, so this is an operator call rather
///     than a silent guard edit. Note `node` is additionally ambiguous: AWS's
///     own "SSM managed node" wording appears in `scripts/aws-autopilot.sh`,
///     so a bare `node` token would false-positive on prose about AWS.
/// Both are recorded in `rust-only-forever-lock-2026-07-19.md`.
fn banned_tokens() -> Vec<String> {
    let mut tokens = vec![banned_token()];
    for extra in [
        "pip",
        "pipx",
        "uv",
        "uvx",
        "poetry",
        "conda",
        "virtualenv",
        "perl",
    ] {
        tokens.push(extra.to_string());
    }
    tokens
}

fn line_has_banned_token(line: &str) -> bool {
    banned_tokens()
        .iter()
        .any(|tok| line_has_token(line, tok.as_str()))
}

fn line_has_token(line: &str, tok: &str) -> bool {
    let bytes = line.as_bytes();
    let needle = tok.as_bytes();
    let mut start = 0usize;
    while let Some(rel) = line[start..].find(tok) {
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

/// Does this file content carry a the banned interpreter token on any non-comment line?
fn content_has_banned_invocation(content: &str) -> bool {
    content
        .lines()
        .any(|l| !is_comment_line(l) && line_has_banned_token(l))
}

/// Given (path, content) pairs already scoped by `is_invocation_scan_target`,
/// return the paths that hit but are NOT in the site allowlist.
fn new_invocation_sites(files: &[(String, String)], allowlist: &[&str]) -> Vec<String> {
    let allowed: BTreeSet<&str> = allowlist.iter().copied().collect();
    files
        .iter()
        .filter(|(p, c)| !allowed.contains(p.as_str()) && content_has_banned_invocation(c))
        .map(|(p, _)| p.clone())
        .collect()
}

/// Site-allowlist entries that no longer hit (deleted OR gone the banned interpreter-clean)
/// — must be removed from the allowlist (shrinking ratchet, site half).
fn stale_invocation_sites(files: &[(String, String)], allowlist: &[&str]) -> Vec<String> {
    allowlist
        .iter()
        .filter(|e| {
            !files
                .iter()
                .any(|(p, c)| p == *e && content_has_banned_invocation(c))
        })
        .map(|e| (*e).to_string())
        .collect()
}

// ---- SCOPE FIX #4 (2026-08-11): Rust-side process spawns ----
//
// `*.rs` was EXCLUDED from the invocation scan by design ("a Rust-side spawn
// would be a reviewed code change"). That reasoning does not hold for
// `build.rs`, which EXECUTES on every single build, and it leaves the most
// direct re-entry path — `Command::new("<interpreter>")` — structurally
// invisible. A blanket token scan over `*.rs` would be unusable (every prose
// mention in a doc-comment would fire), so this scan is deliberately NARROW:
// it looks ONLY at the string literal a process spawn is given.
//
// Verified at fix time: the only literal spawns in the workspace are `git`,
// `docker`, `df`, `bash`, `sh`, `open`, `chronyc` — all benign.
//
// HONEST LIMIT: spawns through a NON-literal program (`Command::new(program)`
// where `program` is a variable — 6 such sites exist, e.g. `infra.rs`,
// `tv_doctor.rs`) cannot be resolved statically and are NOT covered. This
// catches the direct, greppable re-introduction, not a determined author.
fn extract_spawn_literals(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for marker in ["Command::new(\"", ".arg(\""] {
        let mut rest = content;
        while let Some(i) = rest.find(marker) {
            let after = &rest[i + marker.len()..];
            if let Some(end) = after.find('"') {
                out.push(after[..end].to_string());
            }
            rest = &rest[i + marker.len()..];
        }
    }
    out
}

/// Spawn literals in this file that name a banned interpreter/package manager.
fn rust_spawn_violations(content: &str) -> Vec<String> {
    let mut hits: Vec<String> = extract_spawn_literals(content)
        .into_iter()
        .filter(|lit| line_has_banned_token(lit))
        .collect();
    hits.sort();
    hits.dedup();
    hits
}

// ---- SCOPE FIX #5 (2026-08-11): inline JavaScript in workflows ----
//
// `actions/github-script` executes an inline JavaScript program supplied in a
// `script:` block. That is an interpreted-language runtime living inside a
// `.yml` file, so NEITHER real-tree test could see it: the file-extension ban
// only looks at tracked FILENAMES, and `banned_tokens()` deliberately excludes
// `node`/`npx`. 18 such blocks were running in this repo unnoticed.
//
// Ratchet shape is the house allowlist pattern, per FILE and per COUNT: a
// count ABOVE the budget is a new usage (forbidden); a count BELOW it means
// someone ported a block and must shrink the budget in the same PR. A file
// absent from the budget must have ZERO.
const GITHUB_SCRIPT_BUDGET: &[(&str, usize)] = &[
    (".github/workflows/dep-freshness-nightly.yml", 2),
    (".github/workflows/safety.yml", 12),
];

/// Count real `uses: …github-script…` step lines. Comment lines are skipped so
/// a `#`-prefixed explanation of a COMPLETED port never counts as a usage.
fn count_github_script_uses(content: &str) -> usize {
    content
        .lines()
        .filter(|l| !is_comment_line(l))
        .filter(|l| {
            let t = l.trim_start().trim_start_matches("- ");
            t.starts_with("uses:") && t.contains("github-script")
        })
        .count()
}

/// Count inline `script:` block scalars (`script: |`, `script: |-`, `script: >`).
fn count_inline_script_blocks(content: &str) -> usize {
    content
        .lines()
        .filter(|l| !is_comment_line(l))
        .filter(|l| {
            let t = l.trim_start().trim_start_matches("- ");
            t.starts_with("script:") && t.trim_end().ends_with(['|', '-', '>'])
        })
        .count()
}

/// Files whose count EXCEEDS budget (new usage) and files BELOW it (shrink me).
/// Returns `(over, under)` as `(path, actual, budget)` triples.
type BudgetDrift = (Vec<(String, usize, usize)>, Vec<(String, usize, usize)>);
fn github_script_budget_drift(files: &[(String, usize)], budget: &[(&str, usize)]) -> BudgetDrift {
    let (mut over, mut under) = (Vec::new(), Vec::new());
    for (path, actual) in files {
        let allowed = budget
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        if *actual > allowed {
            over.push((path.clone(), *actual, allowed));
        } else if *actual < allowed {
            under.push((path.clone(), *actual, allowed));
        }
    }
    (over, under)
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

/// (a) NO NEW tracked interpreted-language file — the rust-only forever-guard.
/// Scope widened 2026-08-10 from `.py`-only to every extension in
/// `BANNED_FILE_PATHSPECS` (see that const for the scope-hole record).
#[test]
fn no_banned_files_outside_allowlist() {
    assert_sorted_unique(TRACKED_BANNED_ALLOWLIST, "TRACKED_BANNED_ALLOWLIST");
    let tracked_banned = git_ls_files(BANNED_FILE_PATHSPECS);
    let new = py_files_not_in_allowlist(&tracked_banned, TRACKED_BANNED_ALLOWLIST);
    assert!(
        new.is_empty(),
        "RUST-ONLY VIOLATION: new tracked interpreted-language file(s) {new:?}. The rust-only \
         operator directive (2026-07-18) forbids ANY new interpreted-language runtime in this \
         repo, forever. This test (crates/common/tests/rust_only_guard.rs) is the gate: do NOT \
         extend TRACKED_BANNED_ALLOWLIST and do NOT narrow BANNED_FILE_PATHSPECS — port the \
         logic to Rust instead."
    );
}

/// (b) The shrinking ratchet: every allowlist entry must still be tracked.
/// A deleted interpreted-language file MUST have its entry removed in the SAME PR.
/// Scans the SAME pathspec set as test (a) so the two can never drift apart.
#[test]
fn allowlist_shrinks_monotonically() {
    let tracked_banned = git_ls_files(BANNED_FILE_PATHSPECS);
    let stale = stale_entries(TRACKED_BANNED_ALLOWLIST, &tracked_banned);
    assert!(
        stale.is_empty(),
        "SHRINK THE RATCHET: these TRACKED_BANNED_ALLOWLIST entries point at files no longer \
         tracked: {stale:?}. Whoever deleted them must REMOVE the entries from \
         crates/common/tests/rust_only_guard.rs in the same PR — the allowlist only ever \
         shrinks (rust-only operator directive 2026-07-18)."
    );
}

/// (c) NO NEW the banned interpreter-invocation site in .sh / .yml / .yaml / .tftpl /
/// Makefile / .mcp.json / scripts/git-hooks/* (non-comment lines;
/// file-level allowlist), and the site allowlist shrinks when a file goes
/// the banned interpreter-clean or is deleted.
#[test]
fn no_new_banned_invocations() {
    assert_sorted_unique(INVOCATION_SITE_ALLOWLIST, "INVOCATION_SITE_ALLOWLIST");
    let files = load_invocation_scan_files();
    let new = new_invocation_sites(&files, INVOCATION_SITE_ALLOWLIST);
    assert!(
        new.is_empty(),
        "RUST-ONLY VIOLATION: new the banned interpreter invocation site(s) {new:?} (non-comment `the banned interpreter`/\
         `the banned interpreter[0-9]` token). The rust-only operator directive (2026-07-18) forbids new \
         the banned interpreter invocations; this test is the gate. Do NOT extend INVOCATION_SITE_ALLOWLIST."
    );
    let stale = stale_invocation_sites(&files, INVOCATION_SITE_ALLOWLIST);
    assert!(
        stale.is_empty(),
        "SHRINK THE RATCHET: these INVOCATION_SITE_ALLOWLIST entries no longer carry a \
         non-comment the banned interpreter token (file cleaned or deleted): {stale:?}. Remove the entries \
         from crates/common/tests/rust_only_guard.rs in the same PR."
    );
}

/// (e) NO Rust-side process spawn of a banned interpreter (SCOPE FIX #4).
/// Narrow by design: only the string literal handed to `Command::new` /
/// `.arg` is inspected, so doc-comment prose can never false-positive.
/// Covers `build.rs`, which runs on EVERY build.
#[test]
fn no_rust_spawn_of_banned_interpreter() {
    let root = repo_root();
    let mut violations: Vec<String> = Vec::new();
    for path in git_ls_files(&["*.rs"]) {
        // This guard names the tokens it bans; scanning itself would be
        // self-referential. Its own spawns are `git` only (see `git_ls_files`).
        if path.ends_with("crates/common/tests/rust_only_guard.rs") {
            continue;
        }
        let content = std::fs::read_to_string(root.join(&path))
            .unwrap_or_else(|e| panic!("rust_only_guard: cannot read `{path}`: {e}"));
        for hit in rust_spawn_violations(&content) {
            violations.push(format!("{path}: spawns `{hit}`"));
        }
    }
    assert!(
        violations.is_empty(),
        "RUST-ONLY VIOLATION: Rust code spawns a banned interpreter: {violations:?}. \
         The rust-only operator directive (2026-07-19) forbids it — port the logic to \
         Rust instead of shelling out to another runtime."
    );
}

/// (f) The inline-JavaScript shrinking ratchet (SCOPE FIX #5).
/// `actions/github-script` runs an interpreted program inside a `.yml`, which
/// neither the filename ban nor `banned_tokens()` can see. Budget may only
/// shrink: a count over budget is a NEW usage; a count under it means a block
/// was ported and the budget must be decremented in the SAME PR.
#[test]
fn github_script_usage_only_shrinks() {
    assert_sorted_unique(
        &GITHUB_SCRIPT_BUDGET
            .iter()
            .map(|(p, _)| *p)
            .collect::<Vec<_>>(),
        "GITHUB_SCRIPT_BUDGET",
    );
    let root = repo_root();
    let mut counted: Vec<(String, usize)> = Vec::new();
    for path in git_ls_files(&[".github/workflows/*.yml", ".github/workflows/*.yaml"]) {
        let content = std::fs::read_to_string(root.join(&path))
            .unwrap_or_else(|e| panic!("rust_only_guard: cannot read `{path}`: {e}"));
        let uses = count_github_script_uses(&content);
        let blocks = count_inline_script_blocks(&content);
        // A `script:` block without its `uses:` line (or vice versa) is still
        // interpreted-language surface — take the larger, never the smaller.
        counted.push((path, uses.max(blocks)));
    }
    let (over, under) = github_script_budget_drift(&counted, GITHUB_SCRIPT_BUDGET);
    assert!(
        over.is_empty(),
        "RUST-ONLY VIOLATION: new inline-JavaScript (`actions/github-script`) usage \
         {over:?} (path, actual, budget). Inline `script:` blocks are an interpreted \
         runtime the rust-only directive (2026-07-19) forbids adding to. Use the `gh` \
         CLI in a `run:` step instead — see the ported steps in fuzz.yml / \
         chaos-nightly.yml / full-test-nightly.yml. Do NOT raise GITHUB_SCRIPT_BUDGET."
    );
    assert!(
        under.is_empty(),
        "SHRINK THE RATCHET: these files now carry FEWER inline-JavaScript blocks than \
         budgeted {under:?} (path, actual, budget). Whoever ported them must LOWER the \
         entry in GITHUB_SCRIPT_BUDGET (crates/common/tests/rust_only_guard.rs) in the \
         same PR — the budget only ever shrinks."
    );
}

/// (g) The CI clippy gate stays ARMED (2026-08-11).
///
/// `ci.yml`'s clippy step ran WITHOUT `-D warnings` for months, justified by a
/// comment about "~24 pre-existing lib warnings scheduled for a follow-up
/// cleanup pass". That cleanup landed, but nobody re-armed the gate, so the
/// lint job could not fail — a stale comment was the only thing holding a
/// merge-blocking gate open. Measured before arming: `cargo clippy --workspace
/// --no-deps -- -D warnings` exits 0.
///
/// HONEST SCOPE: this pins the FLAG, not the lint result — CI itself is what
/// actually runs clippy. It also does NOT require `--all-targets`, which is
/// deliberately still absent (test-target warnings are unmeasured).
#[test]
fn ci_clippy_gate_stays_armed() {
    let ci = std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml"))
        .expect("rust_only_guard: cannot read .github/workflows/ci.yml");
    let armed = ci
        .lines()
        .filter(|l| !is_comment_line(l))
        .filter(|l| l.contains("cargo clippy") && l.contains("--workspace"))
        .collect::<Vec<_>>();
    assert!(
        !armed.is_empty(),
        "ci.yml no longer has a `cargo clippy --workspace` step — the lint gate \
         vanished entirely. Restore it WITH `-D warnings`."
    );
    for step in &armed {
        assert!(
            step.contains("-D warnings"),
            "CI LINT GATE DISARMED: the ci.yml clippy step `{}` dropped `-D warnings`, \
             so clippy findings can no longer fail Build & Verify (a needed job of All \
             Green). It was armed on 2026-08-11 after the pre-existing-warning cleanup \
             landed and `cargo clippy --workspace --no-deps -- -D warnings` measured \
             clean. Re-arm it, or land a dated operator note first.",
            step.trim()
        );
    }
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
    let t = banned_token();
    assert!(line_has_banned_token(&format!("{t}3 scripts/foo.rs")));
    assert!(line_has_banned_token(&format!("\t{t} -m json.tool")));
    assert!(line_has_banned_token(&format!("exec /usr/bin/{t}3.11 x")));
    assert!(line_has_banned_token(&format!("\"command\": \"{t}3\",")));
    // Digit-suffix widening (fix 1, 2026-07-18): any single digit suffix matches.
    assert!(line_has_banned_token(&format!("{t}2 legacy/x.rs")));
    assert!(line_has_banned_token(&format!("/usr/bin/{t}2.7 y")));
    assert!(line_has_banned_token(&format!("{t}9 z")));
    assert!(
        !line_has_banned_token(&format!("my{t}3 x")),
        "prefix-joined must not match"
    );
    assert!(
        !line_has_banned_token(&format!("{t}ic naming")),
        "suffix-joined must not match"
    );
    assert!(
        !line_has_banned_token("apt install the banned interpreter-pip"),
        "pkg-name suffix `-` excluded"
    );
    assert!(
        !line_has_banned_token("server.the banned interpreter x"),
        "dot-joined prefix excluded"
    );

    // Comment-awareness (line-level).
    assert!(is_comment_line(&format!("  # {t} old note")));
    assert!(!is_comment_line(&format!("run {t}  # trailing note")));
    let commented_only = format!("# {t} was here\n  # {t} legacy\necho rust only\n");
    assert!(!content_has_banned_invocation(&commented_only));
    let live = format!("# header\n{t} scripts/x.rs\n");
    assert!(content_has_banned_invocation(&live));

    // Shebang rule (MED fix, 2026-07-18): `#!` is interpreter selection,
    // NOT a comment — a banned-interpreter shebang alone must be a hit.
    assert!(
        !is_comment_line(&format!("#!/usr/bin/env {t}")),
        "a shebang line must not be treated as a comment"
    );
    assert!(
        content_has_banned_invocation(&format!("#!/usr/bin/env {t}\nimport os\n")),
        "a banned-interpreter shebang must be detected as an invocation"
    );
    assert!(
        !content_has_banned_invocation("#!/bin/bash\necho ok\n"),
        "a bash shebang must not false-positive"
    );
    assert!(
        !content_has_banned_invocation(&format!(
            "#!/usr/bin/env bash\necho ok\n# {t} in a comment\n"
        )),
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

    // ---- SCOPE FIX #3: executable-manifest classes are in scope (2026-08-11).
    assert!(is_invocation_scan_target(
        "deploy/systemd/tickvault.service"
    ));
    assert!(is_invocation_scan_target(
        "scripts/tv-tunnel/com.tickvault.tunnel.plist"
    ));
    assert!(is_invocation_scan_target(".run/Run tickvault.run.xml"));
    assert!(is_invocation_scan_target(
        "deploy/docker/alloy/alloy-config.alloy"
    ));
    // `.claude/settings.json` carries hook COMMAND lines and was previously
    // unscanned — only the exact path `.mcp.json` matched.
    assert!(is_invocation_scan_target(".claude/settings.json"));
    assert!(is_invocation_scan_target(".mcp.json"));

    // ---- SCOPE FIX #4: Rust spawn literals.
    assert_eq!(
        rust_spawn_violations(&format!("let c = Command::new(\"{t}3\");")),
        vec![format!("{t}3")],
        "self-test: a banned interpreter spawn must be detected"
    );
    assert_eq!(
        rust_spawn_violations(&format!("cmd.arg(\"/usr/bin/{t}\");")),
        vec![format!("/usr/bin/{t}")],
        "self-test: an absolute-path spawn arg must be detected"
    );
    assert!(
        rust_spawn_violations("Command::new(\"git\").arg(\"ls-files\");").is_empty(),
        "self-test: benign spawns must not fire"
    );
    assert!(
        rust_spawn_violations(&format!("/// We used to call {t} here, but no longer.\n"))
            .is_empty(),
        "self-test: doc-comment prose must never false-positive (narrow-scan design)"
    );

    // ---- SCOPE FIX #5: inline-JavaScript counting + budget drift.
    let wf = "      - uses: actions/github-script@v7\n        with:\n          script: |\n            await x();\n";
    assert_eq!(count_github_script_uses(wf), 1);
    assert_eq!(count_inline_script_blocks(wf), 1);
    assert_eq!(
        count_github_script_uses("      # Ported off actions/github-script — uses: gone\n"),
        0,
        "self-test: a comment describing a COMPLETED port must not count as a usage"
    );
    let (over, under) = github_script_budget_drift(
        &[
            ("a.yml".to_string(), 3),
            ("b.yml".to_string(), 1),
            ("c.yml".to_string(), 1),
        ],
        &[("a.yml", 2), ("b.yml", 2)],
    );
    assert_eq!(
        over,
        vec![("a.yml".to_string(), 3, 2), ("c.yml".to_string(), 1, 0)],
        "self-test: over-budget AND unbudgeted files must be flagged as new usage"
    );
    assert_eq!(
        under,
        vec![("b.yml".to_string(), 1, 2)],
        "self-test: a ported block must force the budget to shrink"
    );

    // New-site + stale-site detection over synthetic files.
    let files = vec![
        ("scripts/allowed.sh".to_string(), format!("{t} x\n")),
        ("scripts/new_site.sh".to_string(), format!("  {t} y\n")),
        (
            "scripts/clean.sh".to_string(),
            format!("# {t} retired\necho ok\n"),
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
