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
/// Files DELIBERATELY excluded from the invocation scan.
///
/// # Why this list is an exclusion list and not an inclusion list
///
/// This guard decided for a year by asking "is this one of the file types we
/// listed?". That question has now been wrong SIX times, always the same way:
/// `.cargo/config.toml` (the linker), `pip` as an installer rather than the
/// interpreter, `GNUmakefile` shadowing `Makefile`, `.args([..])` spawn form,
/// `.config/nextest.toml` (bite-proven), and `.githooks/` (a hooks path
/// `core.hooksPath` makes executable, outside the enumerated
/// `scripts/git-hooks/` prefix).
///
/// Each fix added one more name. Each time, the NEXT unlisted class stayed
/// invisible. Six repetitions of the same failure is not six mistakes — it is
/// one wrong question, asked six times.
///
/// So the question is inverted. The scan now covers EVERY tracked file, and
/// this list is the small, named, justified set that opts out. A new file
/// class arriving in the repo is scanned by default rather than ignored by
/// default, which is the only shape that can be right about files nobody has
/// thought of yet.
///
/// # What is safe to exclude, and why
///
/// * **Prose** (`.md`) — 1,084 files that legitimately DISCUSS banned runtimes:
///   migration provenance, vendor API references, dated audit history. The
///   rust-only lock's own §0 names these as deliberately retained. Scanning
///   them would produce a thousand false positives on day one and the guard
///   would be disabled within a week, which is worse than the gap.
/// * **Rust** (`.rs`) — not unscanned, scanned DIFFERENTLY: the spawn-literal
///   scan covers `Command::new`/`.arg`/`.args`/`run_with_timeout` there, and a
///   plain token scan would fire on this very file, which must name the tokens
///   in order to ban them.
/// * **Lockfiles** (`.lock`) — machine-generated dependency graphs. Package
///   NAMES legitimately contain banned substrings; nothing in them executes.
/// * **This guard's own sources** — they must contain the tokens to ban them.
///
/// Everything else — every extension, every extension-less file, every
/// directory, including ones that do not exist yet — is scanned.
fn is_excluded_from_invocation_scan(path: &str) -> bool {
    // Prose. See the docblock: a thousand legitimate mentions.
    if path.ends_with(".md") {
        return true;
    }
    // Rust is scanned by the spawn-literal pass instead.
    if path.ends_with(".rs") {
        return true;
    }
    // Machine-generated dependency graphs. Excluded from the TOKEN scan
    // because a lockfile is not a script — but see
    // `native_build_toolchain_only_shrinks` below, which scans it for a
    // different and real hazard.
    //
    // CORRECTED 2026-08-20 (SCOPE FIX #8). This exclusion used to say
    // "nothing executes from them", and that was false in exactly the way the
    // 2026-08-01 `pip` miss was false: the lockfile does not execute, but it
    // NAMES crates whose `build.rs` does. `aws-lc-sys` declares `cmake` as a
    // build dependency, so a clean build drives CMake — a separate scripting
    // language — to compile a C library. A comment asserting a hazard cannot
    // exist is how the previous five misses each survived.
    if path.ends_with(".lock") {
        return true;
    }
    // Binary or opaque payloads — a token match would be meaningless.
    for ext in [
        ".png", ".jpg", ".jpeg", ".gif", ".ico", ".pdf", ".woff", ".woff2", ".ttf", ".otf", ".zip",
        ".gz", ".bin", ".wasm",
    ] {
        if path.ends_with(ext) {
            return true;
        }
    }
    false
}

/// LEGACY inclusion predicate, retained ONLY as the positive half of the
/// self-test: every type it ever listed must still be scanned under the
/// inversion above. If a future edit narrows the exclusion list wrongly, the
/// self-test catches it by replaying the historical inclusion set.
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
        // 2026-08-19 SCOPE FIX #9 — TOOL CONFIGS UNDER `.config/`, PROVEN
        // EXPLOITABLE, NOT THEORETICAL.
        //
        // The row above scoped to the cargo manifests "specifically rather
        // than all `.toml`". Sound reasoning, incomplete enumeration: it
        // listed the two TOMLs its author knew executed something.
        // `.config/nextest.toml` is a third. nextest supports
        // `[script.setup] command = ["...", "-c", "..."]`, which the test
        // runner EXECUTES on every `cargo nextest` invocation — including in
        // CI, on every PR.
        //
        // This was demonstrated, not reasoned about: adding
        // `[script.setup] command = ["python3", "-c", "print(1)"]` to
        // `.config/nextest.toml` left this guard reporting 12/12 GREEN. An
        // interpreter could have run on every test invocation in the repo
        // whose single loudest rule forbids exactly that.
        //
        // `.config/` is enumerated as a DIRECTORY rather than by filename,
        // because the failure mode being closed is precisely that filenames
        // get enumerated one at a time — `.config/` is where tools put
        // configs, so the next tool that lands there is covered on arrival
        // instead of after the next audit. Prose TOMLs live in `config/` and
        // `quality/`, not `.config/`, so the false-positive concern the row
        // above raises does not apply here.
        || path.starts_with(".config/")
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

/// Strips shell quoting so a token split across quotes still matches.
///
/// 2026-08-19 — EVASION HOLE, found by planting it. `I=pyt"hon"3` followed by
/// `$I -c ...` executes the banned runtime, and the guard passed 12/12 on it:
/// the file WAS scanned, but the literal never appears contiguously, so string
/// matching cannot see it.
///
/// This is a different class from scope holes #1-#5. Those were files the
/// guard never opened; this one it read and could not recognise. Removing `"`
/// and `'` before matching closes the whole family of split-literal forms
/// (`pyt"hon"3`, `'py'thon3`, `py""thon3`) for one pass over the line.
fn strip_shell_quotes(line: &str) -> String {
    line.chars().filter(|c| *c != '"' && *c != '\'').collect()
}

/// Does this line invoke a banned runtime?
///
/// # Threat model — stated, because an unstated one gets assumed to be total
///
/// This guard PREVENTS: accidental reintroduction, a dependency quietly
/// pulling an interpreter in, a vendored script arriving with the rest of a
/// change, and now split-literal obfuscation.
///
/// It does NOT prevent: a determined author who wants the runtime anyway.
/// `$(echo cHl0aG9uMw== | base64 -d)`, `${X}${Y}` assembled from two
/// variables, or a name read from a file all execute the same runtime and no
/// static string scan can decide the general case — that reduces to knowing
/// what a shell expands to, which is undecidable.
///
/// Saying so is the point. A guard whose limits are unstated gets read as
/// airtight, and "the guard is green" then stands in for "the codebase is
/// clean" — which is precisely the false-OK this repo forbids everywhere
/// else. The guard is a ratchet against drift, not a sandbox.
fn line_has_banned_token(line: &str) -> bool {
    let unquoted = strip_shell_quotes(line);
    banned_tokens()
        .iter()
        .any(|tok| line_has_token(line, tok.as_str()) || line_has_token(&unquoted, tok.as_str()))
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
    // 2026-08-19 SCOPE FIX #7 — THE SLICE-LITERAL WRAPPER (the fifth miss).
    //
    // HONEST LIMIT 2 above says a wrapper not named in the marker list "remains
    // invisible", and calls that residual theoretical. It was not. A SECOND
    // wrapper exists and has the whole time:
    //
    //     // crates/app/src/bin/tv_doctor.rs
    //     fn run_cmd(args: &[&str]) -> Result<String, String> {
    //         let output = Command::new(args[0]).args(&args[1..])
    //
    // called five times as `run_cmd(&["curl", "-s", …])`. That shape matches
    // NOTHING: not `Command::new("` (the program is `args[0]`, a variable), not
    // `.arg("`, not `.args([` (the slice is a bare `&[…]` ARGUMENT, not a
    // `.args([…])` call), not `run_with_timeout("`. It also adds no new
    // non-literal spawn site, so `NON_LITERAL_SPAWN_BUDGET` stays put and its
    // shrink-only test still passes. `run_cmd(&["python3", "-c", "…"])` was
    // fully green, and an inline `-c` payload dodges BOTH remaining backstops
    // (the file-extension ban and the shebang fallback).
    //
    // The fix does NOT enumerate one more wrapper name — that is what has been
    // wrong five times running. It scans EVERY `(&[…])` slice-literal group in
    // the file. That is deliberately broader than "spawns": a bare
    // `&["python3", …]` string literal has no legitimate purpose anywhere in
    // this workspace, so failing on it regardless of the surrounding call is
    // the stronger and simpler guarantee.
    // HOLE EIGHT, closed 2026-08-29, and it is the same shape one step out.
    //
    // `.args(vec!["python3", "-c", "print(1)"])` matched NOTHING: `vec!` sits
    // between the `(` and the `[`, so neither `.args([` nor `(&[` fires, the
    // program literal is the benign `"env"` so `Command::new("` sees nothing
    // wrong, and an inline `-c` payload means no banned file extension and no
    // shebang ever exist. Zero backstops. It was found by PLANTING the form in
    // a tracked file and watching all twenty tests pass green, with a positive
    // control in the same directory failing — the only way to tell a scanner
    // that is clean from one that is blind.
    //
    // Adding `vec![` rather than `.args(vec![` on purpose: the narrow form
    // would leave `.args(&vec![`, `let a = vec![…]; .args(a)`, and every other
    // arrangement open, which is exactly the enumerate-one-more-shape habit
    // that has now been wrong six times. A bare `vec!["python3", …]` has no
    // legitimate purpose anywhere in this workspace.
    for marker in [".args([", "(&[", "vec!["] {
        let mut rest = content;
        while let Some(i) = rest.find(marker) {
            let after = &rest[i + marker.len()..];
            out.extend(literals_in_group(after));
            rest = &rest[i + marker.len()..];
        }
    }
    out
}

/// Every string literal in a `[...]` group, stopping at the group's closing
/// bracket.
///
/// The bracket search is STRING-AWARE, which the previous inline version was
/// not: it bounded the group at the first `]` anywhere, so a payload
/// containing one — `.args(["sh", "-c", "a[1]"])` — truncated the group early
/// and silently dropped every literal after it. A scanner that stops reading
/// halfway through the exact payload an attacker controls is worse than no
/// scanner, because it reports clean.
fn literals_in_group(after: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = after.char_indices();
    let mut in_string = false;
    let mut lit_start = 0_usize;
    let mut escaped = false;
    for (idx, ch) in chars {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                out.push(after[lit_start..idx].to_string());
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                lit_start = idx + 1;
            }
            // Only a bracket OUTSIDE a string ends the group.
            ']' => break,
            _ => {}
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

/// Words that INTRODUCE a command without being one, so whatever follows them
/// is still in command position.
///
/// `sudo`/`exec`/`env`/`command`/`time`/`nohup`/`xargs` are shell wrappers;
/// `RUN`/`CMD`/`ENTRYPOINT`/`SHELL` are Dockerfile verbs. `echo` is
/// deliberately ABSENT — `echo "SSM managed node"` is prose, and treating a
/// printer as a wrapper is how this check would start reporting sentences.
const COMMAND_INTRODUCERS: &[&str] = &[
    "sudo",
    "exec",
    "env",
    "command",
    "time",
    "nohup",
    "xargs",
    "RUN",
    "CMD",
    "ENTRYPOINT",
    "SHELL",
];

/// Does `token` start a COMMAND at byte offset `at` within `line`?
///
/// Command position is what separates an invocation from a mention. Checking it
/// is why this guard can ban a runtime that the word "node" also names in
/// ordinary English prose about AWS.
///
/// # Why this PARSES the prefix instead of listing suffixes
///
/// It used to ask whether the text before the token ENDED WITH one of nine
/// strings (`|`, `&&`, `||`, `;`, `$(`, `(`, backtick, `"command":`, `"`).
/// That is the same enumerate-the-known-shapes design that this lock's §0.1
/// and §0.2 record failing six times, and it failed here too: `run: npx …`
/// (the dominant single-line CI form), `RUN npm ci` (Dockerfile),
/// `ExecStart=/usr/bin/node …` (systemd), and `sudo`/`exec`/`env` prefixes all
/// end with none of the nine — so eleven runtimes whose ONLY detector this is
/// were invisible in every one of those forms.
///
/// So the question changed from "does the prefix end with a known separator?"
/// to "is the prefix ENTIRELY made of things that precede a command?". The
/// second question has a bounded answer; the first has an endless list.
///
/// Consumed, repeatedly, until nothing is left (⇒ command position) or
/// something unrecognised is (⇒ prose):
/// - a YAML list marker (`- `)
/// - a key ending in `:` — `run:`, `command:`, `"command":`, `entrypoint:` —
///   rejecting anything containing `//` so a URL is never mistaken for a key
/// - an assignment ending in `=` — `ExecStart=`, `FOO=bar`
/// - an opening quote
/// - a [`COMMAND_INTRODUCERS`] word
/// - a trailing PATH (`/usr/bin/`, `./node_modules/.bin/`) — an absolute or
///   relative path to a binary is an invocation, not a mention
fn is_command_position(line: &str, at: usize) -> bool {
    let before = line[..at].trim_end();
    if before.is_empty() {
        return true;
    }

    // Start from the LAST shell separator: everything after it is a fresh
    // command, whatever came before.
    // `=` is a separator so `ExecStart=/usr/bin/node` resolves: the systemd
    // form puts the binary after an assignment, and everything after the last
    // `=` is a fresh command line.
    //
    // HONEST LIMIT: an env-var PREFIX (`FOO=bar node app.js`) is NOT covered —
    // after the split the remainder is `bar`, a bare word, and accepting bare
    // words would make `managed node` a hit. A miss here is a false negative;
    // accepting it would be a false-positive engine, and this guard survives
    // only while its first act is never a false positive.
    let sep_end = ["|", "&&", "||", ";", "$(", "(", "`", "=", "\n"]
        .iter()
        .filter_map(|s| before.rfind(s).map(|i| i + s.len()))
        .max()
        .unwrap_or(0);
    let mut seg = before[sep_end..].trim();

    // Bounded: every arm strictly shortens `seg`, and the loop returns the
    // moment it cannot.
    loop {
        if seg.is_empty() {
            return true;
        }
        let next = if let Some(rest) = seg.strip_prefix("- ") {
            rest
        } else if seg.ends_with('"') || seg.ends_with('\'') {
            // The token's OWN opening quote: `"command": "` before `npx`.
            &seg[..seg.len() - 1]
        } else if let Some(rest) = seg.strip_prefix(['"', '\'']) {
            rest
        } else if seg.ends_with(':') && !seg.contains("//") {
            ""
        } else if seg.ends_with('=') {
            ""
        } else if seg.ends_with('/') && seg.starts_with(['/', '.', '~']) {
            // A path to a binary: `/usr/bin/`, `./node_modules/.bin/`.
            ""
        } else if let Some(word) = COMMAND_INTRODUCERS
            .iter()
            // `r.is_empty()` matters: `before` is trim_end()'d, so a wrapper
            // that is the WHOLE prefix — `RUN` in `RUN npm ci`, `sudo` in
            // `sudo npm install` — arrives with no trailing space.
            .find(|w| {
                seg.strip_prefix(**w)
                    .is_some_and(|r| r.is_empty() || r.starts_with(' '))
            })
        {
            &seg[word.len()..]
        } else {
            return false;
        };
        let trimmed = next.trim();
        if trimmed.len() >= seg.len() {
            // Defensive: never spin on an arm that failed to shorten.
            return false;
        }
        seg = trimmed;
    }
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

/// Strip Rust comments, string-literal aware.
///
/// Used by the `Command::new` spelling scan so a comment placed INSIDE the
/// path (`Command::/*x*/new(`) cannot hide a spawn. String-aware because a
/// `//` inside a literal is not a comment, and treating it as one would
/// truncate real code and produce false FAILURES.
fn strip_rs_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b as char);
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            // A space, so `Command::/*x*/new(` does NOT silently become the
            // canonical spelling here — normalisation removes whitespace
            // afterwards, and the count comparison is what flags it.
            out.push(' ');
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

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
            // INVERTED 2026-08-19: scan everything tracked unless it is on the
            // named exclusion list. Previously this asked "is p one of the
            // types we listed?", which was wrong six times in the same
            // direction. A file class nobody thought of is now scanned by
            // default rather than ignored by default.
            if is_excluded_from_invocation_scan(&p) {
                return None;
            }
            // Unreadable => binary or vanished => nothing to scan. Never a
            // panic: the tree legitimately contains binary assets, and a guard
            // that crashes on one is a guard someone disables.
            let content = std::fs::read_to_string(root.join(&p)).ok()?;
            Some((p, content))
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

/// (d2) NO tracked EXECUTABLE may be excluded from the invocation scan.
///
/// 2026-08-19, found by the COMPILER: `has_interpreter_shebang` — SCOPE FIX #5,
/// described above as "THE STRUCTURAL ONE", the fix that was supposed to stop
/// this guard enumerating extensions forever — had **zero call sites** and was
/// a `dead_code` warning.
///
/// It was not silently broken; it was SUPERSEDED. The same-day inversion of
/// `load_invocation_scan_files` ("scan everything tracked unless excluded")
/// covers extension-less executables by default, which is strictly stronger
/// than asking each file for its shebang. So the enforcement was real and the
/// function was redundant.
///
/// But a dead function whose doc-comment describes live enforcement is exactly
/// the class this repo has recorded twice (`scan_silence` as a "cold-path
/// sweep" with no callers; a "boot HALTS" that could not fire). Deleting it
/// would also throw away the one guarantee the inversion does NOT make: the
/// inversion is only as good as its EXCLUSION list, and nothing stops that
/// list growing to cover a directory that holds an executable.
///
/// So the function gets the job the inversion cannot do — proving the
/// exclusion list never hides something the kernel would execute. Adding
/// `tools/` to `is_excluded_from_invocation_scan` while `tools/deploy` starts
/// `#!/usr/bin/env <interpreter>` fails HERE, and nowhere else.
#[test]
fn every_tracked_executable_is_inside_the_invocation_scan() {
    let root = repo_root();
    let mut hidden: Vec<String> = Vec::new();
    let mut executables = 0_usize;
    for path in git_ls_files(&["."]) {
        // Unreadable => binary => not a shebang script.
        let Ok(content) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        if !has_interpreter_shebang(&content) {
            continue;
        }
        executables += 1;
        if is_excluded_from_invocation_scan(&path) {
            hidden.push(path);
        }
    }
    // Anti-vacuity, the shape this test needs: if NOTHING has a shebang the
    // loop never runs and the emptiness assert below is trivially true. The
    // tree has ~99 shebang files; 20 leaves headroom and still fails loudly
    // if the enumeration collapses.
    assert!(
        executables > 20,
        "RUST-ONLY GUARD IS BLIND: only {executables} tracked file(s) carry a shebang.          Expected >20. `git ls-files` or the read is broken, and this test is          enforcing nothing."
    );
    assert!(
        hidden.is_empty(),
        "RUST-ONLY VIOLATION: tracked executable(s) {hidden:?} carry a `#!` line but are          EXCLUDED from the invocation scan. The kernel will run them and this guard          cannot see what they invoke. Narrow is_excluded_from_invocation_scan() rather          than adding an allowlist entry."
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
/// The inversion must keep scanning everything the ENUMERATION ever listed.
///
/// This replays the historical inclusion set against the new exclusion
/// predicate. If someone later widens the exclusions — "`.toml` is all config,
/// let's skip it" — this fails, because every one of those types was added to
/// the old list only after a real blind spot was found in it.
///
/// It also pins the classes that were blind at each of the six holes, so a
/// regression to any individual past failure is caught by name.
#[test]
fn inversion_still_scans_everything_the_enumeration_ever_listed() {
    for path in [
        // the original enumerated set
        "scripts/x.sh",
        ".github/workflows/x.yml",
        "deploy/x.yaml",
        "deploy/aws/terraform/user-data.sh.tftpl",
        "deploy/aws/terraform/main.tf",
        "Makefile",
        "GNUmakefile",
        "makefile",
        "build/x.mk",
        "Cargo.toml",
        ".cargo/config.toml",
        "deploy/x.conf",
        "deploy/systemd/tickvault.service",
        "deploy/systemd/tickvault.timer",
        "scripts/tv-tunnel/com.tickvault.tunnel.plist",
        "x.json",
        "x.xml",
        // the six holes, by the exact path class each was found in
        ".config/nextest.toml", // #5, bite-proven
        ".githooks/pre-commit", // #6, bite-proven
        // classes NEVER enumerated — the whole point of inverting
        "Justfile",
        "Taskfile.yml",
        ".envrc",
        ".tool-versions",
        "lefthook.yml",
        ".pre-commit-config.yaml",
        "tools/deploy", // extension-less executable
        "quality/x.toml",
        "deploy/schema.sql",
    ] {
        assert!(
            !is_excluded_from_invocation_scan(path),
            "`{path}` must be SCANNED. Excluding it re-opens the \
             decide-by-filename failure that has now been wrong six times."
        );
    }
}

/// The exclusions are deliberate and must stay small. Each entry here is
/// justified in `is_excluded_from_invocation_scan`'s docblock; this pins that
/// the set has not silently grown.
#[test]
fn only_the_justified_classes_are_excluded() {
    for path in [
        "docs/anything.md",
        "crates/common/src/lib.rs",
        "Cargo.lock",
        "assets/logo.png",
    ] {
        assert!(
            is_excluded_from_invocation_scan(path),
            "`{path}` is a justified exclusion and must stay excluded"
        );
    }
    // And the inverse: a `.md`-LOOKING path that is not prose must not slip
    // through on a suffix match alone.
    assert!(
        !is_excluded_from_invocation_scan("scripts/build.md.sh"),
        "suffix matching must not be fooled by a compound name"
    );
}

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

    // SCOPE FIX #10 (2026-08-21) — the six forms the SUFFIX LIST could not see.
    //
    // `is_command_position` used to ask whether the prefix ENDED WITH one of
    // nine strings. Every form below ends with none of them, so eleven runtimes
    // whose only detector this is were invisible in the DOMINANT CI form, the
    // Dockerfile form, and the systemd form simultaneously. Each was verified
    // to count 0 before the parser replaced the list, and 1 after.
    for invocation in [
        "        run: npx -y @scope/pkg",
        "        run: node build.js",
        "      - run: npm ci",
        "RUN npm ci",
        "CMD node server.js",
        "ExecStart=/usr/bin/node /opt/app.js",
        "sudo npm install -g pkg",
        "exec node app.js",
        "env node app.js",
        "  command: yarn start",
    ] {
        assert_eq!(
            count_node_invocations(invocation),
            1,
            "self-test: `{invocation}` is a node-family INVOCATION in command \
             position and must be counted — this is the class the old suffix \
             list missed for eleven runtimes at once"
        );
    }

    // The false-POSITIVE half of the SAME fix. Widening command position is
    // exactly how a guard starts reporting sentences, and a guard whose first
    // act is a false positive teaches the reader to allowlist it. `echo` is
    // deliberately not a command introducer for this reason.
    for prose in [
        "echo \"SSM managed node online\"",
        "  printf 'the node is healthy'",
        "# ExecStart would run node here",
        "  description: the node pool scales",
        "  see https://example.com/node/docs",
        "let x = deno_ish_variable;",
        // `node_modules` is not `node`: the whole-word check must reject a
        // path that merely CONTAINS the runtime name. This fixture started
        // life on the must-count list by mistake, and the guard was right.
        "./node_modules/.bin/tool",
    ] {
        assert_eq!(
            count_node_invocations(prose),
            0,
            "self-test: `{prose}` MENTIONS a node-family word without invoking \
             one — the parser must not widen into prose"
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

    // ---- SCOPE FIX #8 (2026-08-29): `vec![]` argument groups.
    //
    // The evading form, bite-proven by planting it in a tracked file and
    // watching every test pass. Both directions asserted: the banned name must
    // fire in each `vec!` arrangement, and an ordinary `vec!` of harmless
    // strings must stay silent — a marker that shouted at every vector in the
    // workspace would be deleted within a week and the hole would reopen.
    assert_eq!(
        rust_spawn_violations(&format!(
            "Command::new(\"env\").args(vec![\"{t}3\", \"-c\", \"print(1)\"]);"
        )),
        vec![format!("{t}3")],
        "self-test: .args(vec![…]) must be detected -- HOLE EIGHT"
    );
    assert_eq!(
        rust_spawn_violations(&format!("let a = &vec![\"{t}3\", \"-c\"];")),
        vec![format!("{t}3")],
        "self-test: &vec![…] must be detected too -- the narrow fix would miss it"
    );
    assert!(
        rust_spawn_violations("let names = vec![\"alpha\", \"beta\"];").is_empty(),
        "self-test: an ordinary vec! of harmless strings must stay silent"
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
/// Crates in the resolved graph whose `build.rs` drives a NON-RUST build
/// system during `cargo build`.
///
/// SCOPE FIX #8 (2026-08-20). Every prior fix asked "what does this repo's own
/// source execute?". None asked what our DEPENDENCIES execute while being
/// compiled. `aws-lc-sys` — reached through `aws-lc-rs`, the TLS provider
/// CLAUDE.md mandates — declares `cmake` as a build dependency and drives
/// CMake (and, on some targets, NASM) to compile the AWS-LC **C** library on
/// every clean build, including the `aarch64-unknown-linux-musl` deploy
/// cross-compile.
///
/// # This is a BUILD-time surface, not a runtime one
///
/// Nothing non-Rust runs in production: all thirteen Lambdas declare
/// `provided.al2023` with a `bootstrap` handler, and the systemd unit runs
/// `/opt/tickvault/bin/tickvault`. So this is bounded rather than banned —
/// banning it would mean dropping the mandated TLS provider.
///
/// # Why a budget rather than a ban
///
/// Same shape as `CI_ACTION_ALLOWLIST`: the set may only SHRINK. A NEW native
/// build system entering the dependency graph — a vendored C++ library, a Go
/// toolchain, an autotools crate — now fails the build instead of arriving
/// unannounced in a lockfile nobody reads.
const NATIVE_BUILD_TOOLCHAIN_BUDGET: &[&str] = &["cc", "cmake", "pkg-config"];

/// Package names declared in `Cargo.lock`, in file order.
fn locked_package_names(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("name = \"")
            && let Some(name) = rest.strip_suffix('"')
        {
            out.push(name.to_string());
        }
    }
    out
}

/// BITE-PROOF for SCOPE FIX #8.
///
/// The real-tree test below cannot be bite-proven by planting a package in
/// `Cargo.lock`: `cargo test` validates and rewrites the lockfile before the
/// test binary runs, so the plant is gone by the time it is read. Discovered
/// by trying it. So the DETECTION LOGIC is proven here against fixtures
/// instead — which is stronger anyway, since it pins the parser too.
#[test]
fn native_build_toolchain_self_test() {
    // The parser must find names, and only names.
    let lock = "\
[[package]]\n\
name = \"serde\"\n\
version = \"1.0.0\"\n\
\n\
[[package]]\n\
name = \"cmake\"\n\
version = \"0.1.57\"\n";
    let names = locked_package_names(lock);
    assert_eq!(names, vec!["serde".to_string(), "cmake".to_string()]);

    // A version line must never be mistaken for a name.
    assert!(
        !locked_package_names("version = \"cmake\"\n")
            .iter()
            .any(|n| n == "cmake"),
        "only `name = ` lines are package names"
    );

    // THE BITE. A new native build system in the graph must be detected as
    // new — this is the assertion that would have caught `cmake` arriving
    // unannounced, and it is what the real-tree test does against the live
    // lockfile.
    let present = ["cc", "cmake", "bindgen"];
    let new: Vec<&str> = present
        .iter()
        .copied()
        .filter(|b| !NATIVE_BUILD_TOOLCHAIN_BUDGET.contains(b))
        .collect();
    assert_eq!(
        new,
        vec!["bindgen"],
        "a native build system outside the budget must be reported as NEW"
    );

    // And the shrink half: a budget entry that has left the graph must be
    // reported, so the ratchet cannot outlive what it bounds.
    let present_after_removal = ["cc", "pkg-config"];
    let gone: Vec<&str> = NATIVE_BUILD_TOOLCHAIN_BUDGET
        .iter()
        .copied()
        .filter(|b| !present_after_removal.contains(b))
        .collect();
    assert_eq!(
        gone,
        vec!["cmake"],
        "a budget entry no longer in the graph must be reported as stale"
    );
}

/// EVERY tracked `Cargo.lock` in the repository, as `(path, package names)`.
///
/// # SCOPE FIX #13 (2026-09-01) — the twelfth hole, and it is the same shape
/// as the eleven before it
///
/// Both lockfile guards used to read `root.join("Cargo.lock")` — one file.
/// But the root manifest carries `exclude = ["fuzz"]`, so `fuzz/` is a
/// SEPARATE cargo workspace with its own `fuzz/Cargo.lock` (a real, tracked,
/// several-hundred-package graph), and `.github/workflows/fuzz.yml` runs
/// `cargo fuzz build` against it every week in CI.
///
/// That file was invisible to all three mechanisms at once: the interpreter
/// guard and the native-builder guard both read only the root path, and the
/// token scan excludes `.lock` outright. A `pyo3`, `mlua`, `v8` or `cmake`
/// arriving in the fuzz graph would have been green forever.
///
/// LATENT, not live — the fuzz graph is clean today. It is fixed anyway,
/// because "clean today" is a measurement and this file exists precisely
/// because measurements go stale while the guard around them does not.
///
/// The durable lesson, now recorded for the twelfth time: every one of these
/// holes was in what the scan LOOKED AT, never in the tokens it banned. So
/// this asks git which lockfiles exist rather than naming one.
fn all_locked_graphs() -> Vec<(String, Vec<String>)> {
    let root = repo_root();
    let paths = git_ls_files(&["*Cargo.lock"]);
    assert!(
        !paths.is_empty(),
        "RUST-ONLY GUARD IS BLIND: `git ls-files -- *Cargo.lock` matched nothing. \
         At minimum the workspace root lockfile is tracked, so an empty result \
         means the enumeration broke and both lockfile guards are enforcing \
         nothing."
    );
    paths
        .into_iter()
        .map(|p| {
            let lock = std::fs::read_to_string(root.join(&p))
                .unwrap_or_else(|e| panic!("rust_only_guard: cannot read {p}: {e}"));
            let names = locked_package_names(&lock);
            // Anti-vacuity, PER FILE: a parser that returns nothing makes every
            // assertion downstream trivially true, which is exactly how a guard
            // reports green while enforcing nothing. Every real lockfile in this
            // repository resolves hundreds of packages.
            assert!(
                names.len() > 100,
                "RUST-ONLY GUARD IS BLIND: parsed only {} package name(s) from \
                 {p}. The lockfile format changed or the read failed, and this \
                 guard is enforcing nothing.",
                names.len()
            );
            (p, names)
        })
        .collect()
}

/// (h) The native build toolchain the LOCKFILES pull in may only shrink.
#[test]
fn native_build_toolchain_only_shrinks() {
    assert_sorted_unique(
        NATIVE_BUILD_TOOLCHAIN_BUDGET,
        "NATIVE_BUILD_TOOLCHAIN_BUDGET",
    );
    // Union across every tracked lockfile — a build script that executes in
    // the weekly fuzz lane executes just as truly as one in the main graph.
    let graphs = all_locked_graphs();
    let names: Vec<String> = graphs.iter().flat_map(|(_, n)| n.iter().cloned()).collect();

    // Every native-build-system driver we know how to name. Absence from this
    // list is not safety — it is the reason the list is a LIST and not a
    // guess, and adding to it is how the next one gets caught.
    const KNOWN_NATIVE_BUILDERS: &[&str] = &[
        "autotools",
        "bindgen",
        "cc",
        "cmake",
        "cxx-build",
        "meson-next",
        "nasm-rs",
        "pkg-config",
        "system-deps",
        "vcpkg",
    ];

    let mut present: Vec<&str> = KNOWN_NATIVE_BUILDERS
        .iter()
        .copied()
        .filter(|b| names.iter().any(|n| n == b))
        .collect();
    present.sort_unstable();

    let new: Vec<&str> = present
        .iter()
        .copied()
        .filter(|b| !NATIVE_BUILD_TOOLCHAIN_BUDGET.contains(b))
        .collect();
    assert!(
        new.is_empty(),
        "RUST-ONLY VIOLATION: new non-Rust build system(s) {new:?} entered the \
         dependency graph. Their build scripts EXECUTE during `cargo build`, \
         including the deploy cross-compile. Remove the dependency, or record \
         it in NATIVE_BUILD_TOOLCHAIN_BUDGET with the reason it is unavoidable."
    );

    let gone: Vec<&str> = NATIVE_BUILD_TOOLCHAIN_BUDGET
        .iter()
        .copied()
        .filter(|b| !present.contains(b))
        .collect();
    assert!(
        gone.is_empty(),
        "SHRINK THE RATCHET: {gone:?} no longer appear in Cargo.lock. Remove \
         them from NATIVE_BUILD_TOOLCHAIN_BUDGET in the same PR — a budget that \
         outlives what it bounds is a permission nobody is using."
    );
}

/// (i) SCOPE FIX #9 — the EMBEDDED-INTERPRETER class in the locked graph.
///
/// Every prior scope fix in this file closed a hole in what the guard LOOKED
/// AT, never in its token list, and this is the ninth of exactly that shape.
/// `native_build_toolchain_only_shrinks` above taught the guard to read
/// `Cargo.lock` — but only for native BUILD systems. Nothing anywhere asked
/// whether the graph contains a scripting RUNTIME.
///
/// That gap is reachable without tripping a single existing guard. A crate
/// declaring `rhai = "1"` carries no banned file extension (the manifest is
/// `.toml`), no shebang, no spawn literal, and no invocation token — and
/// `.lock` is explicitly excluded from the token scan. The script itself
/// travels as an `include_str!` string, which is `.rs` and therefore outside
/// the invocation scan by design. Green everywhere, with a full interpreter
/// compiled into the binary.
///
/// VERIFIED ABSENT when this landed (2026-08-24): none of the names below
/// appears in the graph.
///
/// `wasm-bindgen`/`js-sys`/`web-sys` are deliberately NOT on this list. They
/// ARE in the lockfile today, pulled transitively by chrono, getrandom, uuid,
/// reqwest and opentelemetry — but every one of those declares them under
/// `[target.'cfg(target_arch = "wasm32")'.dependencies]`, so they are never
/// compiled for this system's targets. Banning them would fail the build over
/// packages that do not exist in any artifact we ship. The lockfile is
/// target-agnostic; that is a property of the FILE, not a breach.
#[test]
fn embedded_interpreters_are_absent_from_the_locked_graph() {
    // Every tracked lockfile, not just the root — see `all_locked_graphs`
    // for SCOPE FIX #13. An interpreter embedded in the fuzz graph runs in
    // CI just as truly as one in the main graph, and until 2026-09-01 it was
    // invisible to this guard, to its native-builder sibling, and to the
    // token scan simultaneously. Per-file anti-vacuity lives in the helper.
    let graphs = all_locked_graphs();
    let names: Vec<String> = graphs.iter().flat_map(|(_, n)| n.iter().cloned()).collect();
    assert!(
        !names.is_empty(),
        "RUST-ONLY GUARD IS BLIND: no package names across any tracked \
         lockfile. Per-file anti-vacuity lives in `all_locked_graphs`; this \
         is the belt-and-braces check that the UNION is non-empty, because a \
         filter below over an empty list would report clean while enforcing \
         nothing."
    );

    // Scripting/bytecode runtimes that would execute non-Rust code from
    // inside a Rust binary. Absence from this list is not safety — it is why
    // the list is a LIST, and adding to it is how the next one gets caught.
    const EMBEDDED_INTERPRETERS: &[&str] = &[
        "boa_engine",
        "deno_core",
        "duktape",
        "hematita",
        "mlua",
        "neon",
        "pyo3",
        "quick-js",
        "quickjs-rs",
        "rhai",
        "rlua",
        "rquickjs",
        "rustpython-vm",
        "v8",
        "wasmer",
        "wasmi",
        "wasmtime",
    ];
    assert_sorted_unique(EMBEDDED_INTERPRETERS, "EMBEDDED_INTERPRETERS");

    let found: Vec<&str> = EMBEDDED_INTERPRETERS
        .iter()
        .copied()
        .filter(|i| names.iter().any(|n| n == i))
        .collect();

    assert!(
        found.is_empty(),
        "RUST-ONLY BREACH — an embedded scripting runtime entered the dependency \
         graph: {found:?}. The operator's standing rule is Rust only, except the \
         enumerated frontend surfaces (rust-only-forever-lock-2026-07-19.md). A \
         crate that embeds an interpreter ships one INSIDE the binary, where no \
         file-extension ban, shebang check, or spawn scan can see it. If this is \
         deliberate, it needs a fresh dated operator quote in that lock file \
         FIRST — then remove the name here in the same PR, so the exemption is a \
         visible diff and not an accident."
    );
}

/// BITE-PROOF for SCOPE FIX #9.
///
/// The real-tree test above cannot be bite-proven by planting a package in
/// `Cargo.lock` — `cargo test` validates and rewrites the lockfile before the
/// test binary reads it, so the plant is gone by the time it matters (the
/// sibling test's own doc records discovering this the hard way). The
/// detection logic is therefore proven against fixtures, which is stronger
/// anyway: it pins the parser as well as the rule.
#[test]
fn embedded_interpreter_detection_self_test() {
    let clean = "[[package]]\nname = \"serde\"\n[[package]]\nname = \"tokio\"\n";
    let dirty = "[[package]]\nname = \"serde\"\n[[package]]\nname = \"rhai\"\n";

    let clean_names = locked_package_names(clean);
    let dirty_names = locked_package_names(dirty);
    assert_eq!(
        clean_names,
        vec!["serde", "tokio"],
        "parser must read names"
    );
    assert_eq!(dirty_names, vec!["serde", "rhai"], "parser must read names");

    let hits = |names: &[String]| -> usize {
        ["pyo3", "rhai", "mlua"]
            .iter()
            .filter(|i| names.iter().any(|n| n == *i))
            .count()
    };
    assert_eq!(hits(&clean_names), 0, "a clean graph must not match");
    assert_eq!(
        hits(&dirty_names),
        1,
        "an interpreter in the graph MUST be detected — if this fails, the \
         real-tree test above is reporting green while enforcing nothing"
    );
}

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

/// BITE-PROOF for SCOPE FIX #7 (2026-08-19) — the slice-literal wrapper.
///
/// A scope fix that is not bite-proven is a claim. Every assertion below FAILS
/// against the pre-fix extractor and PASSES against the current one, so this
/// test is the difference between "we widened the scan" and "we widened the
/// scan and it actually catches the shape".
///
/// The banned token is assembled at runtime rather than written as a literal,
/// because this file is scanned by its own siblings and a literal here would
/// make the guard fail on itself.
#[test]
fn scope_fix_2026_08_19_self_test() {
    let banned = banned_token();

    // (1) THE MISS. `run_cmd(&["<interpreter>", "-c", "…"])` — the real shape
    // in `tv_doctor.rs`. Matches no marker the pre-fix extractor had: the
    // program is `args[0]` (a variable, so `Command::new("` never sees it),
    // and the slice is a bare `&[…]` ARGUMENT, not a `.args([…])` call.
    let src = format!(r#"run_cmd(&["{banned}", "-c", "print(1)"])"#);
    assert!(
        !rust_spawn_violations(&src).is_empty(),
        "self-test: a slice-literal wrapper call must be CAUGHT — this is the \
         exact shape that passed green until 2026-08-19"
    );

    // (2) It generalises. The fix scans EVERY `(&[…])` group, not one more
    // wrapper name, so a wrapper nobody has written yet is already covered.
    let unknown = format!(r#"some_future_helper(&["{banned}", "script.x"])"#);
    assert!(
        !rust_spawn_violations(&unknown).is_empty(),
        "self-test: the scan must not depend on the wrapper's NAME — enumerating \
         names is what has been wrong five times running"
    );

    // (3) No false positive on a benign slice.
    assert!(
        rust_spawn_violations(r#"run_cmd(&["df", "-h", "/data"])"#).is_empty(),
        "self-test: a benign slice must stay clean, or the first response to \
         this guard will be to disable it"
    );

    // (4) The string-aware bracket bound. The old scanner stopped at the FIRST
    // `]` anywhere, so a payload containing one truncated the group and
    // silently dropped every literal after it — reporting clean on exactly the
    // bytes an attacker controls.
    let nested = format!(r#".args(["sh", "-c", "a[1]", "{banned}"])"#);
    assert!(
        !rust_spawn_violations(&nested).is_empty(),
        "self-test: a `]` INSIDE a string must not end the group early"
    );

    // (5) An escaped quote must not end a literal early either.
    let escaped = format!(r#".args(["say \"hi\"", "{banned}"])"#);
    assert!(
        !rust_spawn_violations(&escaped).is_empty(),
        "self-test: an escaped quote inside a literal must not desynchronise \
         the scanner and hide the literal after it"
    );
}

/// Markers that RENAME `std::process::Command`, defeating the spawn scan.
///
/// The spawn scan is marker-driven on the literal `Command::new("`. A single
/// renamed import makes every spawn in that file invisible to it.
const COMMAND_ALIAS_MARKERS: &[&str] = &[
    ", Command as ",
    "= std::process::Command;",
    "process::Command as ",
    "{Command as ",
];

/// Spellings of `Command::new(` that the spawn scan's literal marker misses.
///
/// The scan matches the exact substring `Command::new("`. Rust accepts several
/// other spellings of the same call, and each one is invisible to it.
///
/// Scoped to `Command` DELIBERATELY. The obvious wider ban — any `>::new(` —
/// was measured against this checkout and matches FOUR legitimate sites, all
/// `Vec::<&str>::new()` / `Vec::<String>::new()` turbofish. A guard whose first
/// act is four false positives on idiomatic Rust teaches the next reader that
/// the cheapest fix is an allowlist, which is how three anchors in this file's
/// own history were weakened. The narrow form costs nothing and stays credible.
// Byte-sorted, as `assert_sorted_unique` requires. The grouping by KIND is in
// the trailing comments rather than the order, because ' ' < ':' < '>' puts the
// whitespace and qualified forms either side of each other.
const COMMAND_SPELLING_MARKERS: &[&str] = &[
    "Command ::new(",  // whitespace before `::`
    "Command >::new(", // fully-qualified, spaced
    "Command:: new(",  // whitespace after `::`
    "Command::new (",  // whitespace before `(`
    "Command::new(r",  // raw string: `r"python3"` / `r#"python3"#`
    "Command>::new(",  // fully-qualified: `<std::process::Command>::new("…")`
];

/// SCOPE FIX #15 (2026-09-01) — the fourteenth hole, LATENT, closed before use.
///
/// Every previous fix in this file was written after a real class went
/// unscanned. This one is written while the tree is CLEAN: all six spellings
/// below have **zero occurrences** across `crates/` and `fuzz/`, measured on
/// this checkout before the guard was added.
///
/// That is the point. The pattern this file records fourteen times is that a
/// class nobody enumerated is invisible, and invisibility reads as green — so
/// the cheapest moment to close a spelling hole is while nothing is using it
/// and the ban therefore costs nothing to adopt.
///
/// | spelling | matches `Command::new("`? |
/// |---|---|
/// | `Command::new(r"python3")` | no — `(r` breaks the adjacency |
/// | `<std::process::Command>::new("python3")` | no — `Command>::new(` |
/// | `Command ::new("python3")` | no — space before `::` |
/// | `Command:: new("python3")` | no — space after `::` |
/// | `Command::new ("python3")` | no — space before `(` |
///
/// Each is ordinary Rust the compiler accepts, and `.rs` has no backstop: it
/// is excluded from the token scan, carries no shebang, and is not a banned
/// extension. The canonical `Command::new("…")` form remains available and is
/// the one the spawn scan can see, so nothing legitimate is lost.
#[test]
fn command_new_is_never_written_in_a_spelling_the_spawn_scan_cannot_see() {
    assert_sorted_unique(COMMAND_SPELLING_MARKERS, "COMMAND_SPELLING_MARKERS");
    let root = repo_root();
    let mut violations: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    for path in git_ls_files(&["*.rs"]) {
        // This guard names the spellings it bans, so it cannot scan itself.
        if path.ends_with("rust_only_guard.rs") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        scanned += 1;
        for marker in COMMAND_SPELLING_MARKERS {
            if content.contains(marker) {
                violations.push(format!("{path}: contains `{marker}`"));
            }
        }

        // SCOPE FIX #16 (2026-09-01) — close the CLASS, not a sixth literal.
        //
        // Adversarial review defeated the list above with two spellings it
        // does not enumerate and never could, because the set is infinite:
        //
        //     std::process::Command::\n        new("python3")   // NEWLINE
        //     std::process::Command::/*x*/new("python3")      // COMMENT
        //
        // Both compile; both passed 22/22 green. Enumerating a seventh
        // literal would lose the same race again — the lesson this file
        // records sixteen times is that a class nobody enumerated is
        // invisible, and invisibility reads as green.
        //
        // So: strip comments, delete ALL whitespace, and compare COUNTS. A
        // call that only becomes visible AFTER normalisation is, by
        // definition, one the raw substring scan cannot see. Counts rather
        // than presence, so a file holding one canonical call and one
        // evasive call is still caught.
        let decommented = strip_rs_comments(&content);
        let normalised: String = decommented.chars().filter(|c| !c.is_whitespace()).collect();
        let visible = decommented.matches("Command::new(").count();
        let actual = normalised.matches("Command::new(").count();
        if actual > visible {
            violations.push(format!(
                "{path}: {} `Command::new(` call(s) written with whitespace or a \
                 comment inside the path, so the spawn scan cannot see them",
                actual - visible
            ));
        }
    }
    assert!(
        scanned > 100,
        "self-test: expected to scan many .rs files, scanned {scanned} — the \
         file list is broken and this guard is passing vacuously"
    );
    assert!(
        violations.is_empty(),
        "`Command::new` written in a spelling the spawn scan cannot see. Use \
         the canonical `Command::new(\"…\")` form so the interpreter ban still \
         applies:\n{}",
        violations.join("\n")
    );
}

/// SCOPE FIX #14 (2026-09-01) — the thirteenth hole, PROVEN BY PLANTING.
///
/// The Rust spawn scan looks for the literal `Command::new("`. Renaming the
/// type at the import site defeats it completely, and `.rs` is excluded from
/// the token scan, has no shebang, and is not a banned extension — so there is
/// no backstop whatsoever.
///
/// Measured on this checkout by planting four tracked `.rs` files:
///
/// | payload | result |
/// |---|---|
/// | `std::process::Command::new("python3")` (control) | FAILED, correctly |
/// | `use ...Command as Runner; Runner::new("python3")` | **20 passed — green** |
/// | `type Sh = std::process::Command; Sh::new("python3")` | **20 passed — green** |
/// | `use ...Command as C; C::new("node")` | **20 passed — green** |
///
/// Only the import line differs between row 1 and row 2. Reachable from any
/// compiled crate including `build.rs`, which executes on every `cargo build`,
/// and an inline `-c` payload means no interpreted-language FILE ever lands.
///
/// This bans the RENAME rather than enumerating one more alias name, because
/// enumerating the next name is the approach that has now been wrong fourteen
/// times. Zero legitimate aliased imports existed when this landed, so the ban
/// costs nothing; if one is ever genuinely needed, the canonical unaliased
/// form is always available and is the form the spawn scan can see.
#[test]
fn command_is_never_aliased_past_the_spawn_scan() {
    assert_sorted_unique(COMMAND_ALIAS_MARKERS, "COMMAND_ALIAS_MARKERS");
    let root = repo_root();
    let mut violations: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    for path in git_ls_files(&["*.rs"]) {
        // This guard names the markers it bans; scanning itself is
        // self-referential, exactly as the sibling spawn scan documents.
        if path.ends_with("crates/common/tests/rust_only_guard.rs") {
            continue;
        }
        let content = std::fs::read_to_string(root.join(&path))
            .unwrap_or_else(|e| panic!("rust_only_guard: cannot read `{path}`: {e}"));
        scanned += 1;
        for marker in COMMAND_ALIAS_MARKERS {
            if content.contains(marker) {
                violations.push(format!("{path}: renames Command via `{marker}`"));
            }
        }
    }

    // Anti-vacuity: a glob that matches nothing, or a read that fails open,
    // would make this assertion trivially true — the exact false-OK class this
    // file exists to prevent.
    assert!(
        scanned > 100,
        "RUST-ONLY GUARD IS BLIND: scanned only {scanned} .rs file(s). The \
         enumeration broke and this guard is enforcing nothing."
    );

    assert!(
        violations.is_empty(),
        "RUST-ONLY VIOLATION: `std::process::Command` is renamed, which makes \
         every spawn in that file invisible to the `Command::new(\"` spawn \
         scan: {violations:?}. Use the canonical unaliased form so the scan \
         can see it. See SCOPE FIX #14 above — this exact shape was planted \
         and passed 20/20 green before this guard existed."
    );
}
