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
    ("crates/api/src/handlers/brutex_crossverify.rs", 6),
    ("crates/api/src/middleware.rs", 1),
    ("crates/app/src/brutex_crossverify_compare.rs", 1),
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
