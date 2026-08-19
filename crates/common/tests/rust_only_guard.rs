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
/// 2026-08-14 SCOPE FIX #6 — the shell-variant row.
///
/// The nine-agent audit bite-tested fourteen evasion vectors and found this one
/// only PARTIALLY caught: a `.bash` / `.zsh` / `.ksh` / `.ps1` / `.bat` file was
/// caught only if it happened to carry BOTH a shebang and a banned token. A
/// PowerShell or batch script carries no shebang at all, so it was invisible —
/// the one-rename evasion that the `.pyw`/`.pyi` row already closed for the
/// interpreter's own extensions, still open for every shell variant.
///
/// This repository's shell is bash, universally: all 99 tracked shebangs are
/// `bash` or `sh`, and `.sh` is the only shell extension in use. Banning the
/// variants therefore costs nothing and removes a whole class.
///
/// Verified at fix time (`git ls-files -- <pathspec>` for EVERY entry below):
/// ZERO tracked matches for all seven, so both allowlists stay at their
/// hard-zero floor and the ratchet cannot be re-grown by this widening.
const BANNED_FILE_PATHSPECS: &[&str] = &[
    "*.py", "*.pyw", "*.pyi", "*.pyx", "*.js", "*.jsx", "*.mjs", "*.cjs", "*.es6", "*.coffee",
    "*.ts", "*.tsx", "*.mts", "*.cts", "*.rb", "*.pl", "*.php", "*.lua", "*.tcl", "*.groovy",
    "*.jl",
    "*.ipynb", // SCOPE FIX #6 — shell variants (all zero tracked; `.sh` + bash only)
    "*.bash", "*.zsh", "*.ksh", "*.ps1", "*.bat", "*.fish", "*.nu",
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
        // 2026-08-19 SCOPE FIX #8 — CONFIG FILES THAT CAN CARRY COMMANDS.
        // `.conf` was structurally invisible: not an extension here, and
        // config files carry no shebang so the first-line fallback misses
        // them too. `deploy/aws/sysctl/99-tickvault-net.conf` is inert today
        // (sysctl key=value only), but plenty of `.conf` formats — systemd
        // units, supervisor, cron.d — take an executable command line, and
        // "it happens to be inert right now" is not a scan boundary.
        //
        // `.service`/`.timer` are enumerated for the same reason: this repo
        // SHIPS `deploy/systemd/tickvault.service`, whose ExecStart is a
        // command line, and it was equally unscanned.
        || path.ends_with(".conf")
        || path.ends_with(".service")
        || path.ends_with(".timer")
        // 2026-08-14 SCOPE FIX #7 — MAKE'S OTHER NAMES. The check was
        // `path == "Makefile" || path.ends_with("/Makefile")`, case-SENSITIVE
        // and single-name. GNU make's search order is `GNUmakefile`,
        // `makefile`, `Makefile` — so a tracked `GNUmakefile` SHADOWS the
        // scanned `Makefile` entirely while being invisible here, and it
        // carries no shebang, so the first-line fallback does not catch it
        // either. `*.mk` includes are the same class. This is precisely the
        // enumerate-one-more-name failure the `has_interpreter_shebang`
        // docblock below says has already been wrong four times; the honest
        // fix for make is to enumerate the names make itself enumerates.
        || matches!(
            path.rsplit('/').next(),
            Some("Makefile" | "makefile" | "GNUmakefile")
        )
        || path.ends_with(".mk")
        // `<name>.Dockerfile` — the `docker build -f prod.Dockerfile`
        // convention, which `Dockerfile.*` does not match. Latent (zero
        // tracked Dockerfiles today), enumerated for the same reason.
        || path.ends_with(".Dockerfile")
        // Copy-into-place settings carriers. `.claude/settings.local.json`
        // itself is gitignored, but its tracked `.example`/`.template` seeds
        // carry hook COMMAND lines and end in neither `.json` nor a shebang.
        || path.ends_with(".json.example")
        || path.ends_with(".json.template")
        || path.starts_with("scripts/git-hooks/")
        // 2026-08-14 SCOPE FIX #4 — CARGO CONFIG. `.cargo/config.toml` can set
        // `[target.*] runner = …` and `linker = …`, which the toolchain
        // EXECUTES on every single build. That is the most privileged
        // invocation surface in the repo and it was structurally unscanned:
        // no `.toml` is a scan target, and `.toml` is not a banned extension
        // either, so an interpreter named as the linker would have passed
        // green while linking every production binary. This is not
        // hypothetical shape — the 2026-08-01 correction records an
        // interpreter package having ACTUALLY BEEN the arm64 linker for every
        // production Rust lambda.
        //
        // Scoped to the cargo manifests specifically rather than all `.toml`,
        // because `config/base.toml` and the rule/quality TOMLs carry English
        // prose where a bare token would false-positive.
        //
        // Verified at fix time: `.cargo/config.toml` carries only
        // `rustflags = ["-C", "target-cpu=neoverse-n1"]` — token-clean, so
        // this closes a LATENT blind spot and the allowlists stay at zero.
        || path == ".cargo/config.toml"
        || path.ends_with("/.cargo/config.toml")
        || path == "Cargo.toml"
        || path.ends_with("/Cargo.toml")
}

/// Does this file's FIRST LINE select an interpreter to execute it?
///
/// 2026-08-14 SCOPE FIX #5 — THE STRUCTURAL ONE. Every scope fix before this
/// (the `.tf`/Dockerfile row, the executable-manifest row, the cargo-config
/// row above) closed a hole by ENUMERATING one more extension. That approach
/// has now been wrong four times, and each time in the same direction: a file
/// class nobody listed was invisible, and invisibility reads as green.
///
/// The enumeration left two escapes open at once. An extension-less tracked
/// executable anywhere outside the single hardcoded `scripts/git-hooks/`
/// prefix — `tools/deploy`, `bin/run` — is neither extension-banned nor
/// invocation-scanned. And a shell script renamed `.bash`, `.zsh`, `.ksh`,
/// `.ps1` or `.bat` escapes the `.sh` check, the same one-rename evasion the
/// `.pyw`/`.pyi` additions closed for the interpreter's own extensions.
///
/// A shebang is not a naming convention — it is the kernel's instruction for
/// which interpreter runs the file. So asking the FILE what executes it,
/// rather than asking its NAME, closes both escapes at once and, more
/// importantly, closes the ones nobody has thought of yet.
///
/// Verified at fix time: all 99 tracked shebangs are `bash`, so this closes
/// LATENT blind spots and both allowlists stay at their hard-zero floor.
fn has_interpreter_shebang(content: &str) -> bool {
    content.lines().next().is_some_and(|l| l.starts_with("#!"))
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
    // 2026-08-10 — CASE-SENSITIVITY IS DELIBERATE. KEEP IT.
    //
    // An audit flagged this matcher as CRITICAL because `PIP3 install ...` slips
    // past a ban on the lowercase token. I implemented the case-insensitive fix,
    // then RAN the test, and it was wrong on both counts:
    //
    // 1. It broke the build on a FALSE POSITIVE — `.claude/hooks/
    //    banned-pattern-scanner.sh` carries a quoted human-readable message
    //    naming a vendor's SDK by language, capitalised as English prose. That is
    //    documentation, not an invocation, and the rust-only lock's own §0
    //    explicitly RETAINS vendor-reference and provenance mentions.
    //
    // 2. The bypass it defended against cannot execute. Linux filenames are
    //    case-sensitive (verified: `pip3` resolves, `PIP3` does not), so an
    //    uppercase "invocation" is simply a command-not-found, never a working
    //    evasion.
    //
    // Case-sensitivity is therefore doing useful WORK here: English prose
    // capitalises a language name, shell invocations do not. Lowercasing would
    // trade a real signal for noise. If a future reviewer re-reads that audit
    // finding, this comment is the answer — it was tested, not assumed.
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
// HONEST LIMIT 1: spawns through a NON-literal program (`Command::new(program)`
// where `program` is a variable — 6 such sites exist, e.g. `infra.rs`,
// `tv_doctor.rs`) cannot be resolved statically and are NOT covered. This
// catches the direct, greppable re-introduction, not a determined author.
//
// HONEST LIMIT 2 (2026-08-14, found by audit — recorded, NOT closed): a spawn
// routed through a WRAPPER function is invisible here, and such a wrapper
// already exists — `tickvault-logs-mcp/src/tools.rs::run_with_timeout(program,
// …)`, called with bare `"bash"` / `"git"` / `"docker"` literals that sit in
// neither marker form. Closing this needs call-graph analysis, not a string
// scan, so it is stated rather than pretended away. The shebang fallback and
// the file-extension ban both still apply to whatever such a wrapper launches.
//
// 2026-08-14 SCOPE FIX #6 — `.args([…])`. The old marker set was
// `Command::new("` and `.arg("`, and the PLURAL form does not contain the
// singular one (an `s` intervenes before the paren). So
// `Command::new("env").args(["<interpreter>", "-c", "…"])` was FULLY LITERAL
// and FULLY GREEN: the extractor saw only the benign `"env"` and never looked
// at the payload. That is not a hypothetical shape — `.args([…])` is already
// the dominant form in this workspace (20+ sites, including `build.rs`, which
// executes on every build). Same lesson as every scope row above: the hole was
// in what the scan LOOKED AT, not in what it banned.
fn extract_spawn_literals(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    // 2026-08-18 SCOPE FIX — WRAPPER FUNCTIONS (closes HONEST LIMIT 2's literal half).
    //
    // The marker set was `Command::new("` and `.arg("`, so a spawn routed through a
    // helper was invisible even when the program name was a plain literal:
    // `run_with_timeout("python3", ["-c", "…"])` contains NEITHER marker. That is
    // not hypothetical — the wrapper already exists in the MCP crate and is called
    // four times. An inline `-c` payload also dodges the file-extension ban AND the
    // shebang fallback, so this was the one shape with no backstop at all.
    //
    // Listing the wrapper by name closes the literal form. A wrapper NOT listed here
    // remains invisible — that residual is real and is why HONEST LIMIT 2 above stays
    // on the record rather than being deleted. Adding a new wrapper is now a
    // deliberate act, which is the most a string scan can honestly promise.
    for marker in ["Command::new(\"", ".arg(\"", "run_with_timeout(\""] {
        let mut rest = content;
        while let Some(i) = rest.find(marker) {
            let after = &rest[i + marker.len()..];
            if let Some(end) = after.find('"') {
                out.push(after[..end].to_string());
            }
            rest = &rest[i + marker.len()..];
        }
    }
    // Plural form: take EVERY string literal inside the `[...]` group.
    let mut rest = content;
    while let Some(i) = rest.find(".args([") {
        let after = &rest[i + ".args([".len()..];
        // Bound the scan at the closing bracket so a later, unrelated literal
        // on a following line is never attributed to this spawn.
        let group = after.find(']').map_or(after, |end| &after[..end]);
        let mut tail = group;
        while let Some(open) = tail.find('"') {
            let lit = &tail[open + 1..];
            match lit.find('"') {
                Some(close) => {
                    out.push(lit[..close].to_string());
                    tail = &lit[close + 1..];
                }
                None => break,
            }
        }
        rest = &rest[i + ".args([".len()..];
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
// ---- SCOPE FIX #9 (2026-08-15): the NODE-family runtimes ----
//
// `node` / `npx` / `npm` / `yarn` / `pnpm` / `deno` / `bun` were never banned
// tokens, and `.mcp.json` runs `npx` live. The rule file recorded that as an
// OPEN gap needing an operator ruling, because the obvious fix — adding them to
// `banned_tokens()` — would fail the build on `.mcp.json` itself, and that file
// is dev-session MCP tooling that is never deployed to the box. Breaking local
// tooling to satisfy a lock that exists to protect the RUNTIME would be the
// wrong trade.
//
// A blanket token ban is also wrong for a second reason: `node` is
// prose-ambiguous. `scripts/aws-autopilot.sh` says "SSM managed node online?"
// three times, and a word-boundary scan would flag all three. A guard whose
// first act is three false positives teaches the reader to allowlist it.
//
// So this scans COMMAND POSITION, not free text: the token must begin a
// command — at line start, after a pipe/`&&`/`;`/`$(`, or as a JSON
// `"command":` value. "managed node" fails that test; `npx -y pkg` passes it.
// Shrink-only budget, same shape as the github-script one: a NEW node-family
// invocation anywhere fails; the two existing `.mcp.json` entries are pinned so
// they cannot grow, and removing them forces the budget down in the same PR.
const NODE_RUNTIME_BUDGET: &[(&str, usize)] = &[(".mcp.json", 2)];

/// Interpreted runtimes and their package managers, as COMMAND names.
///
/// Covers the node family plus `ruby`/`gem`/`php`/`lua`. The latter four have
/// their FILE extensions banned already, so a tracked `.rb` cannot exist — but
/// an extension does not stop an inline `ruby -e '…'` in a shell script, which
/// is the same hole the 2026-08-01 correction found for `pip`: banning the
/// artifact is not banning the invocation. All four have ZERO live invocations,
/// so including them costs nothing and closes the class rather than one member
/// of it.
const NODE_FAMILY: &[&str] = &[
    "node", "npx", "npm", "yarn", "pnpm", "deno", "bun", "ruby", "gem", "php", "lua",
];

/// Does `token` start a COMMAND at byte offset `at` within `line`?
///
/// Command position is what separates an invocation from a mention. Checking it
/// is why this guard can ban a runtime that the word "node" also names in
/// ordinary English prose about AWS.
fn is_command_position(line: &str, at: usize) -> bool {
    let before = line[..at].trim_end();
    if before.is_empty() {
        return true;
    }
    // JSON `"command": "npx"` — the invocation form in `.mcp.json`.
    if before.ends_with("\"command\":") || before.ends_with('"') && before.contains("\"command\"") {
        return true;
    }
    before.ends_with('|')
        || before.ends_with("&&")
        || before.ends_with("||")
        || before.ends_with(';')
        || before.ends_with("$(")
        || before.ends_with('(')
        || before.ends_with('`')
}

/// Count node-family invocations in COMMAND POSITION on non-comment lines.
fn count_node_invocations(content: &str) -> usize {
    let mut hits = 0usize;
    for line in content.lines() {
        if is_comment_line(line) {
            continue;
        }
        for name in NODE_FAMILY {
            let mut rest = line;
            let mut base = 0usize;
            while let Some(i) = rest.find(name) {
                let at = base + i;
                let after = line[at + name.len()..].chars().next();
                // A whole word, not a prefix of `nodejs_helper` or a suffix of
                // `managed-node`.
                let word_end = after.is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '-');
                if word_end && is_command_position(line, at) {
                    hits = hits.saturating_add(1);
                }
                base = at + name.len();
                rest = &line[base..];
            }
        }
    }
    hits
}

const GITHUB_SCRIPT_BUDGET: &[(&str, usize)] = &[
    (".github/workflows/dep-freshness-nightly.yml", 2),
    (".github/workflows/safety.yml", 12),
];

// The FRONTEND carve-out is pinned by `browser_surface_and_toolchain_guard.rs`,
// NOT here (2026-08-14 merge resolution).
//
// Two sessions closed the same gap in parallel: this file briefly carried a
// FRONTEND_SCRIPT_BUDGET, and `main` landed the sibling guard. The sibling is
// strictly more thorough — it also pins tracked `.html` (frontend surface vs
// vendor docs) and the `.cargo/config.toml` runner/linker, with a
// planted-runner self-test — so the duplicate here was DELETED rather than
// kept alongside it.
//
// Keeping both would have been worse than keeping neither: two budgets
// asserting the same fact can disagree, and then an edit satisfies one guard
// while failing the other for a reason that reads as arbitrary. One fact, one
// ratchet.

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
///
/// Selection is by PATH first (`is_invocation_scan_target`) and then, for
/// everything the path rules did not already claim, by CONTENT — any tracked
/// file whose first line is a shebang is executable regardless of what it is
/// called (`has_interpreter_shebang`). See that function for why the
/// path-enumeration approach kept failing in the same direction.
///
/// Binary and unreadable files are skipped rather than fatal: `read_to_string`
/// fails on non-UTF-8, and a PNG has no shebang to find. Path-selected targets
/// keep the original fail-loud behaviour, because a scan target we cannot read
/// IS a guard failure.
fn load_invocation_scan_files() -> Vec<(String, String)> {
    let root = repo_root();
    git_ls_files(&["."])
        .into_iter()
        .filter_map(|p| {
            if is_invocation_scan_target(&p) {
                let content = std::fs::read_to_string(root.join(&p))
                    .unwrap_or_else(|e| panic!("rust_only_guard: cannot read `{p}`: {e}"));
                return Some((p, content));
            }
            // Content-selected: unreadable => not a shebang script => skip.
            let content = std::fs::read_to_string(root.join(&p)).ok()?;
            has_interpreter_shebang(&content).then_some((p, content))
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
    // 2026-08-10 ANTI-VACUITY, in the one shape that fits this test. ZERO tracked
    // interpreted-language files is the CORRECT and desired state here, so a
    // non-empty assert on the RESULT would be backwards. What must be proven
    // instead is that the LOOKUP MECHANISM still works — otherwise a broken
    // `git ls-files` returns nothing and this guard passes for the wrong reason,
    // indistinguishable from success.
    assert!(
        git_ls_files(&["."]).len() > 100,
        "RUST-ONLY GUARD IS BLIND: `git ls-files` returned almost nothing, so an \
         interpreted-language file could exist and go unseen. This guard's PASS \
         would be meaningless."
    );
    // Scope widened from `*.py` to the 9-extension BANNED_FILE_PATHSPECS by #1738,
    // landed in parallel. Kept verbatim — it is strictly broader than what this
    // test previously covered, and it composes with the assert above rather than
    // competing with it: theirs widens WHAT is looked for, mine proves the looking
    // actually happened.
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
    // 2026-08-10 ANTI-VACUITY. Every assertion in this test checks that the
    // VIOLATION set is empty; nothing checked that the SCANNED set was not. An
    // empty file list — from a narrowed glob, a renamed directory, or a broken
    // `git ls-files` — made both checks trivially true and this guard reported
    // green while enforcing NOTHING. A sibling guard in this repo was found in
    // exactly that state, and its own "proof of non-vacuity" was a tautology.
    // ~160 files match today; 50 leaves 3x headroom and still fails loudly if
    // the scan collapses.
    assert!(
        files.len() > 50,
        "RUST-ONLY GUARD IS BLIND: the invocation scan matched only {} file(s). \
         It is enforcing nothing. Expected >50. Check is_invocation_scan_target() \
         and that `git ls-files` works from the test's working directory.",
        files.len()
    );
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

/// (g3) The NODE-family runtimes only shrink (2026-08-15).
///
/// Closes the last OPEN item the rust-only lock recorded as needing a ruling.
/// The ruling it encodes: dev-session tooling that never reaches the box may
/// keep its existing invocations, pinned so they cannot grow; a NEW one
/// anywhere fails the build.
#[test]
fn node_family_invocations_only_shrink() {
    assert_sorted_unique(
        &NODE_RUNTIME_BUDGET
            .iter()
            .map(|(p, _)| *p)
            .collect::<Vec<_>>(),
        "NODE_RUNTIME_BUDGET",
    );
    let mut counted: Vec<(String, usize)> = Vec::new();
    // The SAME file set `no_new_banned_invocations` scans — so the node budget
    // inherits every scope fix that set has accumulated (shebang detection,
    // make's other names, cargo config, executable manifests) instead of
    // growing its own list to drift out of sync.
    for (path, content) in load_invocation_scan_files() {
        let n = count_node_invocations(&content);
        if n > 0 || NODE_RUNTIME_BUDGET.iter().any(|(p, _)| *p == path) {
            counted.push((path, n));
        }
    }
    let (over, under) = github_script_budget_drift(&counted, NODE_RUNTIME_BUDGET);
    assert!(
        over.is_empty(),
        "RUST-ONLY VIOLATION: new node-family invocation {over:?} (path, actual, budget). \
         The runtime is Rust-only (operator directive 2026-07-19). The ONLY tolerated \
         node-family invocations are the dev-session MCP entries in `.mcp.json`, which \
         never reach the box. If you need a tool, write it in Rust or call a real binary \
         — do NOT raise NODE_RUNTIME_BUDGET."
    );
    assert!(
        under.is_empty(),
        "SHRINK THE RATCHET: fewer node-family invocations than budgeted {under:?} \
         (path, actual, budget). Lower the entry in NODE_RUNTIME_BUDGET in the same PR — \
         the budget only ever shrinks."
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

/// 2026-08-10. The rust-only lock claims a "HARD ZERO floor" for both allowlists.
/// Until this test, that floor was enforced by REVIEWER DISCIPLINE ONLY: a PR could
/// re-add a banned file together with its own allowlist entry and stay green, because
/// every other assertion here only compares the tree against the allowlist. This makes
/// the floor mechanical — re-growing either list now fails the build.
#[test]
fn allowlists_are_pinned_at_zero() {
    assert!(
        TRACKED_BANNED_ALLOWLIST.is_empty(),
        "TRACKED_BANNED_ALLOWLIST must stay EMPTY (hard-zero floor, rust-only \
         directive 2026-07-31). It currently has {} entr(y/ies): {:?}. Port the \
         logic to Rust instead of allowlisting it.",
        TRACKED_BANNED_ALLOWLIST.len(),
        TRACKED_BANNED_ALLOWLIST
    );
    assert!(
        INVOCATION_SITE_ALLOWLIST.is_empty(),
        "INVOCATION_SITE_ALLOWLIST must stay EMPTY (hard-zero floor). It currently \
         has {} entr(y/ies): {:?}.",
        INVOCATION_SITE_ALLOWLIST.len(),
        INVOCATION_SITE_ALLOWLIST
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

    // SCOPE FIX #7 (2026-08-14) — make's OTHER names. `GNUmakefile` and
    // `makefile` are searched by GNU make BEFORE `Makefile`, so either one
    // SHADOWS the scanned file entirely. Neither carries a shebang, so the
    // first-line fallback cannot rescue them.
    assert!(is_invocation_scan_target("GNUmakefile"));
    assert!(is_invocation_scan_target("makefile"));
    assert!(is_invocation_scan_target("sub/dir/GNUmakefile"));
    assert!(is_invocation_scan_target("build/rules.mk"));
    assert!(is_invocation_scan_target("prod.Dockerfile"));
    assert!(is_invocation_scan_target(
        ".claude/settings.local.json.example"
    ));
    assert!(is_invocation_scan_target(
        ".claude/settings.local.json.template"
    ));
    // A file merely CONTAINING the word must still be out of scope, or the
    // widening becomes a false-positive engine.
    assert!(!is_invocation_scan_target(
        "docs/how-to-write-a-Makefile.md"
    ));

    // SCOPE FIX #6 (2026-08-14) — `.args([…])`. The old marker set was
    // `Command::new("` and `.arg("`; the PLURAL form contains NEITHER (an `s`
    // sits between `arg` and the paren), so a fully-literal
    // `Command::new("env").args(["<interpreter>", "-c", …])` passed GREEN with
    // only the benign `"env"` ever extracted. `.args([…])` is already the
    // dominant form in this workspace, including in `build.rs`.
    let t = banned_token();
    let plural = format!(r#"Command::new("env").args(["{t}3", "-c", "print(1)"]);"#);
    let hits = extract_spawn_literals(&plural);
    assert!(
        hits.iter().any(|h| h == &format!("{t}3")),
        "self-test: the plural .args([..]) payload must be extracted, got {hits:?}"
    );
    assert!(
        !rust_spawn_violations(&plural).is_empty(),
        "self-test: a banned interpreter inside .args([..]) must be a violation"
    );
    // The group is bounded at its closing bracket, so a later unrelated literal
    // on a following line is never attributed to this spawn.
    let bounded = format!(r#"c.args(["git", "log"]);{}let s = "{t}3";"#, '\n');
    assert!(
        rust_spawn_violations(&bounded).is_empty(),
        "self-test: extraction must stop at ']' and not swallow later literals"
    );

    // SCOPE FIX #9 — node-family COMMAND POSITION, both directions.
    //
    // The false-negative half: these are real invocations and must count.
    for invocation in [
        r#"    "command": "npx","#,
        "npx -y @scope/pkg",
        "cat x | node -e 'x'",
        "make build && yarn install",
        "$(npm bin)/tool",
        "deno run mod.ts; echo done",
    ] {
        assert_eq!(
            count_node_invocations(invocation),
            1,
            "self-test: `{invocation}` is a node-family INVOCATION and must be counted"
        );
    }

    // The false-POSITIVE half, which is why this scans command position at all.
    // `scripts/aws-autopilot.sh` says "SSM managed node" three times; a plain
    // word-boundary token scan would flag all three, and a guard whose first
    // act is three false positives teaches the reader to allowlist it.
    for prose in [
        "  # 3. SSM managed node online?",
        "  note_ok \"SSM managed node Online\"",
        "  note_issue \"SSM managed node not Online (ping=$PING)\"",
        "echo \"the node is healthy\"",
        "nodejs_helper --run",
        "kubectl get managed-node",
    ] {
        assert_eq!(
            count_node_invocations(prose),
            0,
            "self-test: `{prose}` MENTIONS a node-family word without invoking one — \
             counting it would make this guard a false-positive engine"
        );
    }

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

    // ---- SCOPE FIX (2026-08-18): spawn routed through a WRAPPER function.
    //
    // Both directions are asserted, and the second matters more than the first:
    // the wrapper's four real callers pass `bash` / `git` / `docker` / `aws`, so a
    // marker that fired on those would be deleted by the next reader as noise, and
    // the hole would reopen permanently. A guard is only kept if it is quiet when
    // it should be quiet.
    assert_eq!(
        rust_spawn_violations(&format!("let out = run_with_timeout(\"{t}3\", &args);")),
        vec![format!("{t}3")],
        "self-test: an interpreter spawned through the wrapper must be detected"
    );
    assert!(
        rust_spawn_violations("let out = run_with_timeout(\"bash\", &args);").is_empty(),
        "self-test: the wrapper's real callers must never false-positive"
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

// ============================================================================
// SCOPE FIX (2026-08-18) — the two surfaces the 2026-08-14 audit left UNDEFINED
// ============================================================================
//
// That audit closed four holes and RECORDED two it did not close. Recording a
// hole is honest, but it is not a control: both stayed invisible to every gate,
// so a new instance of either would land with no signal at all.
//
// Neither is made impossible here — a string scan cannot resolve a variable, and
// vendor CI actions are genuinely not our source. What changes is the failure
// mode: an UNDEFINED surface grows silently, a PINNED one fails the build. That
// is the whole of the claim, and it is deliberately smaller than "fixed".

/// Spawns whose program name is a VARIABLE — `Command::new(program)`.
///
/// HONEST LIMIT 1 explains why these cannot be resolved statically. What CAN be
/// done is deny them room to multiply: the exact count per file is pinned, so a
/// NEW variable-spawn site fails the build and has to be argued for in review.
/// The six that exist launch operator tooling (docker / git / aws CLIs).
const NON_LITERAL_SPAWN_BUDGET: &[(&str, usize)] = &[
    ("crates/app/src/bin/tv_doctor.rs", 1),
    ("crates/app/src/infra.rs", 4),
    ("crates/tickvault-logs-mcp/src/tools.rs", 1),
];

/// Third-party GitHub Actions — every one a `node20` JavaScript runtime.
///
/// These run in CI, not in the product, and they are vendor code rather than
/// ours, which is why the rust-only lock never banned them. But "not banned" had
/// quietly become "not looked at". Pinning the SET — never the version, since
/// tags and SHAs rotate legitimately — means a NEW vendor runtime entering CI
/// fails the build instead of arriving unannounced.
const CI_ACTION_ALLOWLIST: &[&str] = &[
    "Swatinem/rust-cache",
    "actions/cache",
    "actions/cache/restore",
    "actions/cache/save",
    "actions/checkout",
    "actions/download-artifact",
    "actions/github-script",
    "actions/upload-artifact",
    "anthropics/claude-code-action",
    "aws-actions/configure-aws-credentials",
    "dtolnay/rust-toolchain",
    "hashicorp/setup-terraform",
    "peter-evans/create-pull-request",
    "taiki-e/install-action",
];

/// Count `Command::new(<not a quote>)` — i.e. a variable program name.
/// Comment lines are excluded so prose describing the shape never counts.
fn non_literal_spawn_count(content: &str) -> usize {
    const M: &str = "Command::new(";
    content
        .lines()
        // `is_comment_line` is the SHELL/YAML `#` form and is wrong here — this
        // scans Rust. Using it silently counted every `///` line that merely
        // DESCRIBES the shape. Caught by this module's own self-test.
        .filter(|l| !l.trim_start().starts_with("//"))
        .map(|line| {
            let mut n = 0usize;
            let mut rest = line;
            while let Some(i) = rest.find(M) {
                let after = &rest[i + M.len()..];
                // A quote means the program is a LITERAL, already covered by the
                // spawn scan. The BACKSLASH-escaped form matters just as much: a
                // guard file carries `"Command::new(\""` as its own scan marker,
                // and reading that as a variable spawn would make every scanner
                // in this repo look like a violation.
                if !(after.starts_with('"') || after.starts_with("\\\"")) {
                    n += 1;
                }
                rest = after;
            }
            n
        })
        .sum()
}

/// Action names referenced by `uses:`, with the version/SHA stripped.
/// Local (`./…`) and container (`docker://…`) steps are not vendor runtimes.
fn ci_action_names(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if is_comment_line(trimmed) {
            continue;
        }
        // Both real YAML forms must count: `- uses: foo@v1` (the step's first
        // key, so it carries the list dash) and a bare `uses:` (the step began
        // with `- name:`). Handling only the second silently under-counts, and
        // an under-counting allowlist is worse than none — it reads as full
        // coverage while a whole syntax form walks past it.
        let trimmed = trimmed
            .strip_prefix("- ")
            .map_or(trimmed, |rest| rest.trim_start());
        let Some(value) = trimmed.strip_prefix("uses:") else {
            continue;
        };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        if value.is_empty() || value.starts_with("./") || value.starts_with("docker://") {
            continue;
        }
        out.push(value.split('@').next().unwrap_or(value).to_string());
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn non_literal_spawn_sites_only_shrink() {
    let root = repo_root();
    let mut counted: Vec<(String, usize)> = Vec::new();
    for path in git_ls_files(&["*.rs"]) {
        // PRODUCTION source only. Test files legitimately carry this shape as
        // DATA: scan markers in string literals, and raw-string fixtures like
        // `r#"Command::new(program)"#` that exist precisely to prove a scanner
        // ignores non-literals. Counting those would make the guards themselves
        // the repo's top violators and turn this budget into noise — and a
        // budget that reads as noise is one the next reader deletes.
        if !path.contains("/src/") {
            continue;
        }
        let content = std::fs::read_to_string(root.join(&path)).unwrap_or_default();
        let n = non_literal_spawn_count(&content);
        if n > 0 || NON_LITERAL_SPAWN_BUDGET.iter().any(|(p, _)| *p == path) {
            counted.push((path, n));
        }
    }
    let (over, under) = github_script_budget_drift(&counted, NON_LITERAL_SPAWN_BUDGET);
    assert!(
        over.is_empty(),
        "RUST-ONLY VIOLATION: a NEW variable-program spawn site appeared {over:?} as \
         (path, actual, budget). A spawn through a variable cannot be checked by any \
         string scan — it is exactly the shape that can launch another runtime \
         unseen. Prefer a literal program name; if the variable is unavoidable, \
         validate it against an allowlist AT THE CALL SITE and raise this budget in \
         the same PR with that justification."
    );
    assert!(
        under.is_empty(),
        "STALE BUDGET: fewer variable-program spawns than pinned {under:?} as \
         (path, actual, budget). Good news — lower NON_LITERAL_SPAWN_BUDGET in the \
         same PR so the ratchet keeps its grip."
    );
}

#[test]
fn ci_actions_are_pinned_to_the_allowlist() {
    assert_sorted_unique(CI_ACTION_ALLOWLIST, "CI_ACTION_ALLOWLIST");
    let root = repo_root();
    let mut seen: Vec<String> = Vec::new();
    for path in git_ls_files(&[".github/workflows/*.yml", ".github/workflows/*.yaml"]) {
        let content = std::fs::read_to_string(root.join(&path)).unwrap_or_default();
        seen.extend(ci_action_names(&content));
    }
    seen.sort();
    seen.dedup();

    let added: Vec<&String> = seen
        .iter()
        .filter(|a| !CI_ACTION_ALLOWLIST.contains(&a.as_str()))
        .collect();
    assert!(
        added.is_empty(),
        "NEW third-party CI action(s) {added:?}. Every one is a JavaScript (node20) \
         runtime executing in our CI with access to the checkout and to secrets. \
         Adding one is a supply-chain decision, not a workflow detail: pin it to a \
         full commit SHA rather than a moving tag, then add its NAME here."
    );

    let stale: Vec<&&str> = CI_ACTION_ALLOWLIST
        .iter()
        .filter(|a| !seen.iter().any(|s| s.as_str() == **a))
        .collect();
    assert!(
        stale.is_empty(),
        "STALE ALLOWLIST: pinned CI action(s) no longer used anywhere {stale:?}. \
         Remove them in the same PR — an allowlist that outlives its entries \
         silently re-opens room for a vendor runtime to return unnoticed."
    );
}

#[test]
fn scope_fix_2026_08_18_self_test() {
    // Variable-program detection: the whole point is telling the two apart.
    assert_eq!(
        non_literal_spawn_count("let c = Command::new(program);"),
        1,
        "self-test: a variable program name must count"
    );
    assert_eq!(
        non_literal_spawn_count("let c = Command::new(\"git\");"),
        0,
        "self-test: a literal program name must NOT count — it is already covered"
    );
    assert_eq!(
        non_literal_spawn_count("// we could use Command::new(program) here"),
        0,
        "self-test: prose describing the shape must never count"
    );

    // CI action extraction.
    assert_eq!(
        ci_action_names("      - uses: actions/checkout@v7.0.1\n"),
        vec!["actions/checkout".to_string()],
        "self-test: the version must be stripped so SHA rotation is not a failure"
    );
    assert_eq!(
        ci_action_names("      - uses: actions/checkout@3d3c42e5aac5\n"),
        vec!["actions/checkout".to_string()],
        "self-test: a SHA pin must resolve to the same NAME as a tag pin"
    );
    assert!(
        ci_action_names("      - uses: ./.github/actions/local\n").is_empty(),
        "self-test: a local composite action is our own code, not a vendor runtime"
    );
    assert!(
        ci_action_names("      # uses: evil/action@v1\n").is_empty(),
        "self-test: a commented-out action must never count"
    );
}
