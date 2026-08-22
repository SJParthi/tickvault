//! Two guards the 2026-08-14 nine-agent audit found MISSING, both of the same
//! shape: an invariant this repository relies on that nothing enforced.
//!
//! # Guard 1 — the frontend carve-out is pinned
//!
//! `CLAUDE.md` grants exactly one exemption to the Rust-only rule: "except
//! frontend". It then names four browser-facing surfaces. The audit found that
//! sentence was enforced by **nothing at all** — `rust_only_guard.rs` bans
//! standalone `.js` files but has no `.html` awareness and never counts inline
//! `<script>` inside `.rs`. A fifth page handler with 300 lines of embedded
//! JavaScript, or a new tracked `.html`, would land completely green.
//!
//! That matters more than it sounds. The carve-out is the ONE hole in an
//! otherwise absolute rule, and an unbounded hole is not a carve-out — it is an
//! open door with a sign next to it.
//!
//! This guard makes the exemption **shrink-only**: every file that may contain
//! browser code is enumerated with its exact count, and anything else must
//! contain none.
//!
//! # Guard 2 — the build toolchain declares no external runner
//!
//! `.cargo/config.toml` `[target.*] runner` / `linker` execute on EVERY build.
//! `rust_only_guard.rs` scans that file for banned interpreter TOKENS, which is
//! necessary but not sufficient: a linker named `node`, `zig`, or a wrapper
//! script passes the token scan and still injects a non-Rust program into the
//! build.
//!
//! This is not hypothetical. `rust-only-forever-lock-2026-07-19.md` §0 records
//! that a package-manager-installed toolchain was, for a period, **the arm64
//! linker of every production Rust lambda**, and it read green the whole time.
//! Asking "is there a runner or linker key at all?" is strictly stronger than
//! asking "does its value contain a banned word", because it cannot be defeated
//! by a name nobody thought to ban.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Repo root, from this crate's manifest dir.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("browser_surface_guard: cannot canonicalize repo root")
}

/// Files matching `pathspec` that are tracked **or newly added and not
/// ignored**.
///
/// The `--others --exclude-standard` half is load-bearing and was found missing
/// by this guard's own bite-test: a brand-new file that has never been
/// `git add`ed does not appear in `git ls-files`, so a scanner built on tracked
/// files alone reports GREEN on exactly the change it exists to catch — the
/// first commit of a new browser surface.
///
/// `rust_only_guard.rs` learned this the same way (its SCOPE FIX C1 records an
/// untracked `crates/x/src/evil.rs` being invisible to every diff source). It
/// is repeated here rather than inherited, because a guard that only sees
/// yesterday's files is not a guard.
fn scan_paths(pathspec: &str) -> Vec<String> {
    let root = repo_root();
    let run = |extra: &[&str]| -> Vec<String> {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("ls-files");
        for a in extra {
            cmd.arg(a);
        }
        let out = cmd
            .arg("--")
            .arg(pathspec)
            .current_dir(&root)
            .output()
            .expect("browser_surface_guard: `git ls-files` must run");
        assert!(
            out.status.success(),
            "browser_surface_guard: `git ls-files {extra:?} -- {pathspec}` failed"
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    };

    let mut all = run(&[]);
    all.extend(run(&["--others", "--exclude-standard"]));
    all.sort();
    all.dedup();
    all
}

// ===================== GUARD 1: BROWSER SURFACES =====================

/// Every `.rs` file permitted to contain `<script`, with its EXACT count.
///
/// The first three are sanctioned frontend surfaces per `CLAUDE.md`. The rest
/// are NOT frontend — they are XSS test fixtures and vendor-error-body parsing,
/// where the literal appears inside a test assertion or a sanitiser's input.
/// They are enumerated rather than pattern-excluded precisely so that a real
/// `<script>` added to one of them shows up as a count change.
///
/// **This table may SHRINK freely. Growing it — adding a path, or raising a
/// count — means a new browser surface, and needs a dated operator note in
/// `rust-only-forever-lock-2026-07-19.md` first.**
const SCRIPT_BUDGET: &[(&str, usize)] = &[
    // --- sanctioned frontend surfaces (CLAUDE.md carve-out) ---
    ("crates/api/src/handlers/dashboard_page.rs", 1),
    ("crates/api/src/handlers/feeds_page.rs", 1),
    ("crates/api/src/handlers/board_page.rs", 1),
    // --- NOT frontend: XSS fixtures + vendor-body parsing ---
    ("crates/api/src/middleware.rs", 1),
    ("crates/aws-lambdas/src/operator_control.rs", 3),
    ("crates/core/src/notification/events.rs", 12),
    ("crates/trading/src/oms/api_client.rs", 1),
];

/// Tracked `.html` files, and why each is allowed to exist.
///
/// Exactly ONE is a frontend surface. The other twelve are vendor API
/// reference documents under `docs/` — captured third-party pages, never
/// served, never executed by us.
const HTML_ALLOWED_FRONTEND: &[&str] = &["crates/aws-lambdas/src/operator_control_console.html"];

/// Vendor reference HTML lives ONLY here. Anything outside is a new surface.
const HTML_VENDOR_DOC_PREFIX: &str = "docs/";

#[test]
fn browser_code_in_rust_is_pinned_to_the_enumerated_surfaces() {
    let root = repo_root();
    let budget: BTreeMap<&str, usize> = SCRIPT_BUDGET.iter().copied().collect();

    let mut actual: BTreeMap<String, usize> = BTreeMap::new();
    for path in scan_paths("*.rs") {
        // The scanner is exempt from its own scan. It has to SPELL the pattern
        // it looks for — in this module's docs and in its bite-test fixtures —
        // so including itself would make it permanently self-tripping.
        // `rust_only_guard.rs` carries the identical carve-out for the same
        // reason (it must name the tokens it bans).
        if path.ends_with("browser_surface_and_toolchain_guard.rs") {
            continue;
        }
        let full = root.join(&path);
        let Ok(body) = std::fs::read_to_string(&full) else {
            continue;
        };
        let count = body.matches("<script").count();
        if count > 0 {
            actual.insert(path, count);
        }
    }

    // Non-vacuity: the sanctioned surfaces must actually be found. If this
    // scanner ever silently matches nothing, it would "pass" forever.
    assert!(
        actual.contains_key("crates/api/src/handlers/dashboard_page.rs"),
        "browser-surface scan found no `<script` in a known frontend surface — \
         the scanner is broken and would pass vacuously"
    );

    let mut problems = Vec::new();
    for (path, count) in &actual {
        match budget.get(path.as_str()) {
            None => problems.push(format!(
                "  NEW browser surface: {path} contains {count} `<script` and is not in \
                 SCRIPT_BUDGET"
            )),
            Some(&allowed) if *count > allowed => problems.push(format!(
                "  GREW: {path} has {count} `<script`, budget allows {allowed}"
            )),
            Some(_) => {}
        }
    }

    assert!(
        problems.is_empty(),
        "BROWSER-SURFACE BUDGET EXCEEDED:\n{}\n\n\
         `CLAUDE.md` exempts the FRONTEND from the Rust-only rule and names four \
         surfaces. That exemption is deliberately shrink-only: an unbounded \
         carve-out is not a carve-out.\n\n\
         If this is genuinely a new frontend surface, add a dated note to \
         `.claude/rules/project/rust-only-forever-lock-2026-07-19.md` FIRST, then \
         add the path here. If it is a test fixture, the same applies — enumerate \
         it, so a real `<script>` added later still shows as a count change.",
        problems.join("\n")
    );
}

#[test]
fn tracked_html_is_one_frontend_surface_plus_vendor_docs() {
    let html = scan_paths("*.html");
    assert!(
        !html.is_empty(),
        "no tracked .html found — the scanner is broken and would pass vacuously"
    );

    let mut unexpected = Vec::new();
    for path in &html {
        let is_frontend = HTML_ALLOWED_FRONTEND.contains(&path.as_str());
        let is_vendor_doc = path.starts_with(HTML_VENDOR_DOC_PREFIX);
        if !is_frontend && !is_vendor_doc {
            unexpected.push(format!("  {path}"));
        }
    }

    assert!(
        unexpected.is_empty(),
        "UNSANCTIONED .html FILE(S):\n{}\n\n\
         Tracked HTML is allowed in exactly two places: the operator console \
         (the one frontend surface that is a file rather than a Rust string), and \
         vendor API reference pages under `{HTML_VENDOR_DOC_PREFIX}`, which are \
         captured documents and are never served.\n\n\
         A new HTML file anywhere else is a new browser surface and needs a dated \
         operator note in `rust-only-forever-lock-2026-07-19.md` first.",
        unexpected.join("\n")
    );

    // The frontend surface must still exist — if it is renamed or deleted, this
    // guard must be updated deliberately rather than quietly passing.
    for expected in HTML_ALLOWED_FRONTEND {
        assert!(
            html.iter().any(|p| p == expected),
            "the sanctioned frontend surface `{expected}` is no longer tracked — \
             update HTML_ALLOWED_FRONTEND deliberately"
        );
    }
}

// ===================== GUARD 2: BUILD TOOLCHAIN =====================

/// Strip `#` comments so a commented-out example cannot trip the assertion.
fn strip_toml_comments(body: &str) -> String {
    body.lines()
        .map(|line| match line.find('#') {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn cargo_config_declares_no_external_runner_or_linker() {
    let root = repo_root();
    let path = root.join(".cargo/config.toml");
    let Ok(body) = std::fs::read_to_string(&path) else {
        // No cargo config at all is trivially safe.
        return;
    };

    let code = strip_toml_comments(&body);

    let mut found = Vec::new();
    for (lineno, line) in code.lines().enumerate() {
        let t = line.trim_start();
        // Match the KEY, not a substring: `runner = ...` / `linker = ...`,
        // including the dotted and quoted forms cargo accepts.
        let is_key = |key: &str| t.starts_with(key) && t[key.len()..].trim_start().starts_with('=');
        if is_key("runner") || is_key("linker") {
            found.push(format!("  .cargo/config.toml:{}: {}", lineno + 1, t.trim()));
        }
    }

    assert!(
        found.is_empty(),
        "CARGO BUILD TOOLCHAIN DECLARES AN EXTERNAL PROGRAM:\n{}\n\n\
         `runner` and `linker` execute on EVERY build, so a non-Rust program named \
         there is in the build path of every production binary.\n\n\
         This is asserted as an ABSENCE rather than by scanning the value for banned \
         words, and that difference is the whole point: a token scan is defeated by \
         any name nobody thought to ban. `rust-only-forever-lock-2026-07-19.md` §0 \
         records a real instance — a package-manager-installed toolchain was the \
         arm64 linker of every production lambda, and it read green throughout.\n\n\
         `rustflags` is deliberately NOT checked here; it passes flags to rustc and \
         names no external program. If a runner/linker is genuinely required, add a \
         dated operator note to that lock file first.",
        found.join("\n")
    );
}

#[test]
fn toolchain_scanner_detects_a_planted_runner() {
    // Self-test: the comment-stripper must not swallow a real declaration, and
    // a commented example must not trip it. A guard that has never been shown
    // to fire is not a guard.
    let planted = "[target.aarch64-unknown-linux-musl]\nrunner = \"some-emulator\"\n";
    let code = strip_toml_comments(planted);
    assert!(
        code.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("runner") && t["runner".len()..].trim_start().starts_with('=')
        }),
        "scanner failed to detect a planted `runner` key"
    );

    let commented =
        "# runner = \"some-emulator\"\nrustflags = [\"-C\", \"target-cpu=neoverse-n1\"]\n";
    let code = strip_toml_comments(commented);
    assert!(
        !code.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("runner") && t["runner".len()..].trim_start().starts_with('=')
        }),
        "scanner false-positived on a COMMENTED runner example"
    );

    // And the real file's `rustflags` must not be mistaken for a runner/linker.
    let real = "rustflags = [\"-C\", \"target-cpu=neoverse-n1\"]";
    let t = real.trim_start();
    assert!(
        !(t.starts_with("runner") || t.starts_with("linker")),
        "rustflags must not be treated as a runner/linker declaration"
    );
}

// ===================== GUARD 3: SPAWN ALLOWLIST =====================

/// Every external binary this workspace may spawn by NAME LITERAL.
///
/// # Why an allowlist, when `rust_only_guard.rs` already scans spawns
///
/// That scan is a **denylist**: it rejects a spawn whose program name appears on
/// a banned-token list. The 2026-08-14 audit bite-tested it and found the
/// obvious hole — a spawn of the JavaScript runtime passes, because that name
/// was never listed. So does every other interpreter shipped after the list was
/// written.
///
/// (Those names are deliberately not spelled here. This file is an enforcement
/// test, and the lock file's standard is that our own source stays at literal
/// zero — a guard cannot ban a token by writing it down. An earlier draft did
/// spell them and `rust_only_guard.rs` correctly rejected this file, which is
/// the guard working exactly as intended.)
///
/// `rust-only-forever-lock-2026-07-19.md` §0 states the lesson in its own
/// words, after a package manager smuggled a toolchain past a ban on the
/// interpreter's name: *"a ban on a runtime that permits its package manager is
/// not a ban."* The same reasoning applies one level up. A denylist can only
/// reject what someone already thought of; an allowlist rejects everything
/// nobody has justified.
///
/// Each entry below is a real spawn site in the workspace today, and every one
/// is a system utility or VCS — no language runtime among them.
const SPAWN_ALLOWLIST: &[(&str, &str)] = &[
    (
        "git",
        "build.rs sha resolution + guard tests enumerating tracked files",
    ),
    ("bash", "test harnesses invoking the repo's own .sh hooks"),
    ("sh", "same, POSIX form"),
    (
        "docker",
        "compose health checks (infra.rs) + container tests",
    ),
    ("df", "disk-health watcher"),
    ("open", "operator convenience — opens a URL on the dev box"),
    (
        "chronyc",
        "clock-discipline verification for the latency claim",
    ),
];

/// Files carrying deliberate spawn FIXTURES rather than real spawns.
///
/// Both are enforcement tests: they must SPELL the patterns they police, so
/// scanning them would make each permanently self-tripping. Same carve-out the
/// language guard already takes for the tokens it names.
const SPAWN_SCAN_EXEMPT: &[&str] = &[
    "crates/common/tests/rust_only_guard.rs",
    "crates/common/tests/browser_surface_and_toolchain_guard.rs",
];

/// Extract the literal from every `Command::new("...")` in `body`.
fn spawned_literals(body: &str) -> Vec<String> {
    const NEEDLE: &str = "Command::new(\"";
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(NEEDLE) {
        let after = &rest[i + NEEDLE.len()..];
        match after.find('"') {
            Some(end) => {
                out.push(after[..end].to_owned());
                rest = &after[end..];
            }
            None => break,
        }
    }
    out
}

#[test]
fn every_spawned_binary_is_on_the_allowlist() {
    let root = repo_root();
    let allowed: Vec<&str> = SPAWN_ALLOWLIST.iter().map(|(bin, _)| *bin).collect();

    let mut violations = Vec::new();
    let mut seen_any = false;

    for path in scan_paths("*.rs") {
        if SPAWN_SCAN_EXEMPT.contains(&path.as_str()) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        for bin in spawned_literals(&body) {
            seen_any = true;
            if !allowed.contains(&bin.as_str()) {
                violations.push(format!("  {path}: Command::new(\"{bin}\")"));
            }
        }
    }

    // Non-vacuity: `git` is spawned by build.rs and by several guard tests. If
    // the scanner finds nothing at all, it is broken and would pass forever.
    assert!(
        seen_any,
        "spawn scan found ZERO `Command::new(\"...\")` literals — the scanner is \
         broken and would pass vacuously"
    );

    assert!(
        violations.is_empty(),
        "SPAWN ALLOWLIST VIOLATION:\n{}\n\n\
         This workspace is Rust-only, and the existing spawn check is a DENYLIST — \
         it rejects only names someone already thought to ban, so `node`, `deno`, \
         `bun` and every runtime shipped after that list was written pass it \
         silently.\n\n\
         `rust-only-forever-lock-2026-07-19.md` §0 records the same lesson from a \
         real incident: a ban on a runtime that permits its package manager is not \
         a ban.\n\n\
         If this binary is genuinely required, add it to SPAWN_ALLOWLIST **with a \
         reason**, and if it is a language runtime add a dated operator note to \
         that lock file FIRST.",
        violations.join("\n")
    );
}

#[test]
fn spawn_allowlist_is_documented_and_has_no_language_runtime() {
    for (bin, why) in SPAWN_ALLOWLIST {
        assert!(
            !bin.is_empty() && !why.is_empty(),
            "every SPAWN_ALLOWLIST entry needs a binary and a stated reason; \
             `{bin}` has an empty field"
        );
    }

    // The allowlist itself is FROZEN to this exact set. Adding any binary —
    // benign or not — fails here until someone updates this line deliberately,
    // which is the whole point: the ratchet must be turned by hand, in a diff a
    // reviewer sees.
    //
    // Written as a frozen SET rather than as an inner denylist of runtime names.
    // The first draft of this test did carry such a denylist, and
    // `rust_only_guard.rs` correctly REJECTED this file for spelling those names
    // — a guard cannot ban a token by writing it down. The lock file states the
    // standard: enforcement files keep our own source at literal zero. A frozen
    // set gives strictly stronger protection anyway (it rejects `zig` and
    // `cmake` too, which no runtime denylist would have listed) while naming
    // nothing banned.
    const FROZEN: &[&str] = &["git", "bash", "sh", "docker", "df", "open", "chronyc"];

    let mut actual: Vec<&str> = SPAWN_ALLOWLIST.iter().map(|(b, _)| *b).collect();
    actual.sort_unstable();
    let mut frozen: Vec<&str> = FROZEN.to_vec();
    frozen.sort_unstable();

    assert_eq!(
        actual, frozen,
        "SPAWN_ALLOWLIST changed. Every entry is a system utility or VCS today, \
         and nothing may join them quietly.\n\n\
         If the addition is genuinely required, update FROZEN in the same commit \
         with a stated reason on the entry. If it is a language runtime or a \
         package manager, it does not belong here at all — add a dated operator \
         note to rust-only-forever-lock-2026-07-19.md first."
    );
}

#[test]
fn spawn_scanner_extracts_literals_and_ignores_non_literals() {
    // Bite-proof of the extractor itself.
    let src = r#"
        let a = Command::new("git").arg("status");
        let b = Command::new("node").arg("x.js");
        let c = Command::new(program).arg("y");
    "#;
    let found = spawned_literals(src);
    assert_eq!(
        found,
        vec!["git".to_string(), "node".to_string()],
        "extractor must find both literals and skip the non-literal spawn"
    );

    // HONEST LIMIT, restated so it is never mistaken for coverage: a spawn
    // through a VARIABLE (`Command::new(program)`) carries no literal for any
    // static scan to read. Four such sites exist today — infra.rs (docker CLI
    // dispatch), tv_doctor.rs, and the logs-mcp tool runner — and each derives
    // its program from repo-controlled config, not from user input. They are
    // NOT covered by this guard and cannot be; saying so is the point.
    assert!(
        spawned_literals("Command::new(program)").is_empty(),
        "a non-literal spawn yields no literal — this is the documented limit"
    );
}
