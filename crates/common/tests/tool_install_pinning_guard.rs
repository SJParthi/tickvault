//! Every tool CI and the scripts install names an exact version.
//!
//! Plan item J5 (2026-10-26 audit). `CLAUDE.md`'s third principle is "every
//! version pinned", and `Cargo.toml`, `deny.toml` and the action SHAs are
//! pinned. The tools installed beside them were not: `ci.yml` ran
//! `cargo install --locked cargo-llvm-cov` and `tool: cargo-nextest`, so each
//! run took whatever crates.io or the action's manifest served as latest that
//! day. A new release of the coverage tool could change the numbers the merge
//! gate reads without any change in this repository.
//!
//! The rule: every `cargo install` / `cargo binstall` and every
//! `taiki-e/install-action` `tool:` entry in `.github/`, `.claude/hooks/`,
//! `deploy/`, `scripts/` and the `Makefile` carries an exact version
//! (`--version X.Y.Z`, `--vers X.Y.Z` or `name@X.Y.Z`: three numbers, an
//! optional pre-release suffix; `@0.9`, `1.*` and `latest` are ranges).
//! Lines that do not are counted per file against a SHRINK-ONLY budget: the
//! rows below are the ones outside the change that added this guard, pinned
//! so they cannot grow and so removing one forces the row down.
//!
//! HONEST LIMITS: a version carried in a shell variable (`cargo install
//! "$name"`) counts as unversioned even if the variable holds `x@1.2.3`;
//! that errs toward failing. A `tool: |` block is read line by line; an
//! empty one counts as one unversioned entry (fails closed). An install
//! written another way (a downloaded release binary, `curl | tar`) is not
//! seen here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `(path, unversioned install lines)` measured 2026-10-26. Shrink-only.
const UNVERSIONED_INSTALL_BUDGET: &[(&str, usize)] = &[
    // "install: cargo install …" hints in `echo` lines of the local hooks.
    (".claude/hooks/pre-commit-gate.sh", 1),
    (".claude/hooks/pre-push-gate.sh", 1),
    (".github/workflows/fuzz.yml", 1),     // tool: cargo-fuzz
    (".github/workflows/mutation.yml", 1), // tool: cargo-mutants
    (".github/workflows/safety.yml", 1),   // tool: cargo-careful
    // `cargo install "$cargo_name"` (the name list carries no versions) and
    // its "install manually" hint.
    ("scripts/bootstrap.sh", 2),
    ("scripts/ensure-ready.sh", 1),
    // A "(install: cargo install cargo-llvm-cov)" hint in an echo.
    ("scripts/quality-full.sh", 1),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("tool_install_pinning_guard: cannot canonicalize repo root")
}

/// Tracked and untracked-not-ignored files under `.github/`, `scripts/`, plus
/// the Makefile.
fn scanned_files() -> Vec<String> {
    let root = repo_root();
    let mut out = Vec::new();
    for extra in [&[][..], &["--others", "--exclude-standard"][..]] {
        let o = Command::new("git")
            .arg("ls-files")
            .arg("-z")
            .args(extra)
            .args([
                "--",
                ".github",
                ".claude/hooks",
                "deploy",
                "scripts",
                "Makefile",
            ])
            .current_dir(&root)
            .output()
            .expect("tool_install_pinning_guard: `git ls-files` failed to run");
        assert!(o.status.success(), "`git ls-files` failed");
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

fn read_lossy(path: &str) -> String {
    let bytes = std::fs::read(repo_root().join(path)).unwrap_or_else(|e| {
        panic!("tool_install_pinning_guard: cannot read `{path}`: {e}; a listed file must be read")
    });
    String::from_utf8_lossy(&bytes).into_owned()
}

fn is_comment_line(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// True for an EXACT version: three dot-separated numbers, optionally followed
/// by a `-pre.release` suffix of ASCII letters, digits, `.` and `-`. `0.9`,
/// `1`, `1.*`, `^1.2.3` and `latest` are ranges or aliases, not pins.
fn is_exact_version(v: &str) -> bool {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && pre.is_none_or(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        })
}

/// True when `spec` (one `name@version` token) carries an exact version.
fn is_exact_at_version(spec: &str) -> bool {
    spec.rsplit_once('@')
        .is_some_and(|(name, v)| !name.is_empty() && is_exact_version(v))
}

fn strip_arg_quotes(w: &str) -> &str {
    w.trim_matches(|c| matches!(c, '"' | '\'' | ')' | '`' | ';'))
}

/// Unversioned `cargo install` / `cargo binstall` invocations on a line,
/// including `cargo +toolchain install`.
///
/// Each invocation's arguments run to the next shell separator or a `#`
/// comment. It is versioned when they hold `--version`/`--vers` (space or
/// `=`) with an exact version, or a `name@X.Y.Z` token.
fn unversioned_cargo_installs(line: &str) -> usize {
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut hits = 0usize;
    let mut i = 0usize;
    while i + 1 < words.len() {
        let is_cargo = words[i].trim_start_matches(['(', '`', '"', '\'']) == "cargo";
        // `cargo +nightly install x`: skip one `+toolchain` word.
        let verb_at = if is_cargo && words[i + 1].starts_with('+') {
            i + 2
        } else {
            i + 1
        };
        let is_install = is_cargo
            && words
                .get(verb_at)
                .is_some_and(|w| matches!(*w, "install" | "binstall"));
        if !is_install {
            i += 1;
            continue;
        }
        let mut versioned = false;
        let mut j = verb_at + 1;
        while j < words.len() && !matches!(words[j], "&&" | "||" | ";" | "|" | "then") {
            if words[j].starts_with('#') {
                // A trailing comment: its words are not arguments.
                j = words.len();
                break;
            }
            let w = strip_arg_quotes(words[j]);
            if (w == "--version" || w == "--vers")
                && words
                    .get(j + 1)
                    .is_some_and(|v| is_exact_version(strip_arg_quotes(v)))
            {
                versioned = true;
            }
            if let Some(v) = w
                .strip_prefix("--version=")
                .or_else(|| w.strip_prefix("--vers="))
            {
                if is_exact_version(v) {
                    versioned = true;
                }
            }
            if is_exact_at_version(w) {
                versioned = true;
            }
            if words[j].ends_with(';') {
                break;
            }
            j += 1;
        }
        if !versioned {
            hits += 1;
        }
        i = j.max(verb_at + 1);
    }
    hits
}

/// Unversioned entries in comma-separated tool specs.
fn unversioned_specs(value: &str) -> usize {
    value
        .split(',')
        .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\''))
        .filter(|s| !s.is_empty())
        .filter(|s| !is_exact_at_version(s))
        .count()
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Unversioned entries on the `tool:` line at `lines[at]`: every
/// comma-separated tool must be `name@X.Y.Z`. A block scalar (`tool: |`)
/// reads every following line indented deeper than the key, each line one
/// or more comma-separated specs; an empty value, or a block with no specs,
/// counts as one unversioned entry, so a form this parser cannot read fails
/// closed.
fn unversioned_tool_entries(lines: &[&str], at: usize) -> usize {
    let line = lines[at];
    let t = line.trim_start().trim_start_matches("- ");
    let Some(value) = t.strip_prefix("tool:") else {
        return 0;
    };
    let value = value
        .split(" #")
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '\'');
    if value.starts_with(['|', '>']) {
        // The key's column: its indent plus any `- ` list marker before it.
        let key_indent = indent_of(line) + (line.trim_start().len() - t.len());
        let mut entries = 0usize;
        let mut unversioned = 0usize;
        for next in &lines[at + 1..] {
            if next.trim().is_empty() {
                continue;
            }
            if indent_of(next) <= key_indent {
                break;
            }
            if is_comment_line(next) {
                continue;
            }
            entries += 1;
            unversioned += unversioned_specs(next);
        }
        return if entries == 0 { 1 } else { unversioned };
    }
    if value.is_empty() {
        return 1;
    }
    unversioned_specs(value)
}

fn unversioned_installs(text: &str) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    (0..lines.len())
        .filter(|&n| !is_comment_line(lines[n]))
        .map(|n| unversioned_cargo_installs(lines[n]) + unversioned_tool_entries(&lines, n))
        .sum()
}

#[test]
fn every_installed_tool_names_an_exact_version() {
    let names: Vec<&str> = UNVERSIONED_INSTALL_BUDGET.iter().map(|r| r.0).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        names, sorted,
        "UNVERSIONED_INSTALL_BUDGET must be sorted and unique"
    );
    let budget: BTreeMap<&str, usize> = UNVERSIONED_INSTALL_BUDGET.iter().copied().collect();

    let files = scanned_files();
    assert!(
        files.iter().any(|f| f == ".github/workflows/ci.yml"),
        "the scan did not list ci.yml; it would pass vacuously"
    );
    let mut versioned_seen = 0usize;
    let mut problems = Vec::new();
    for path in &files {
        let text = read_lossy(path);
        versioned_seen += text
            .lines()
            .filter(|l| !is_comment_line(l))
            .filter(|l| l.contains("cargo install") || l.trim_start().starts_with("tool:"))
            .count();
        let n = unversioned_installs(&text);
        let allowed = budget.get(path.as_str()).copied().unwrap_or(0);
        if n > allowed {
            problems.push(format!(
                "  {path}: {n} unversioned install(s), budget {allowed}"
            ));
        } else if n < allowed {
            problems.push(format!(
                "  SHRANK: {path}: {n} unversioned install(s) against {allowed}; lower the budget"
            ));
        }
    }
    for name in &names {
        if !files.iter().any(|f| f == name) {
            problems.push(format!("  STALE: {name} is gone; remove its row"));
        }
    }
    assert!(
        versioned_seen > 5,
        "found only {versioned_seen} install lines; the scan is broken"
    );
    assert!(
        problems.is_empty(),
        "UNPINNED TOOL INSTALL:\n{}\n\nEvery version pinned (CLAUDE.md principle 3): write \
         `cargo install --locked --version X.Y.Z <tool>` or `tool: <tool>@X.Y.Z`. Do NOT \
         raise UNVERSIONED_INSTALL_BUDGET.",
        problems.join("\n")
    );
}

#[test]
fn tool_install_pinning_self_test() {
    // Versioned forms.
    for ok in [
        "cargo install --locked --version 0.9.1 cargo-llvm-cov",
        "        run: cargo install --locked --version 0.9.1 cargo-llvm-cov",
        "command -v x || cargo install cargo-lambda@1.9.1 --locked",
        "cargo install --vers 1.2.3 foo",
        "cargo install --version=1.2.3 foo",
        "cargo binstall foo@2.0.0",
        "          tool: cargo-nextest@0.9.148",
        "          tool: cargo-audit@0.22.2,cargo-deny@0.19.9",
        "echo nothing to install here",
        "# cargo install foo (comment)",
    ] {
        assert_eq!(unversioned_installs(ok), 0, "self-test: {ok:?} is pinned");
    }
    // Unversioned forms.
    for (bad, n) in [
        ("cargo install --locked cargo-llvm-cov", 1),
        ("        run: cargo install --locked cargo-llvm-cov", 1),
        ("cargo install foo && cargo install bar --version 1.0.0", 1),
        ("cargo install foo; cargo install bar", 2),
        (
            "if cargo install \"$cargo_name\" --quiet 2>/dev/null; then",
            1,
        ),
        ("cargo binstall foo", 1),
        ("cargo install foo@latest", 1),
        ("cargo install --version ^1.2 foo", 1),
        ("          tool: cargo-nextest", 1),
        ("          tool: cargo-audit@0.22.2,cargo-deny", 1),
        ("          tool: cargo-a, cargo-b", 2),
        ("          tool: |", 1),
        ("          tool:", 1),
        ("echo \"(install: cargo install cargo-llvm-cov)\"", 1),
    ] {
        assert_eq!(unversioned_installs(bad), n, "self-test: {bad:?}");
    }
    assert!(is_exact_at_version("x@1.2.3"));
    assert!(!is_exact_at_version("x@1.2."));
    assert!(!is_exact_at_version("x@latest"));
    assert!(!is_exact_at_version("@1.2.3"));

    // Review round 1 (2026-10-26): each row was a false pass in the first
    // version of this parser.
    for (bad, n) in [
        // A partial version is a range: the action resolves `@0.9` to the
        // newest 0.9.x.
        ("          tool: cargo-nextest@0.9", 1),
        ("          tool: cargo-nextest@1", 1),
        ("cargo install foo@1.2", 1),
        // `--version=1.*` is a range too.
        ("cargo install --version=1.* foo", 1),
        ("cargo install --version 1.* foo", 1),
        // `cargo +toolchain install` is an install.
        ("cargo +nightly install cargo-fuzz", 1),
        ("cargo +nightly install --locked cargo-fuzz", 1),
        // A trailing comment's words are not arguments.
        ("cargo install foo # pinned as bar@1.2.3", 1),
        ("cargo install foo # --version 1.2.3", 1),
    ] {
        assert_eq!(unversioned_installs(bad), n, "self-test: {bad:?}");
    }
    for ok in [
        "cargo +nightly install --locked --version 0.13.1 cargo-fuzz",
        "cargo install foo --version 1.2.3-beta.1",
        "cargo install foo@1.2.3-rc.2",
    ] {
        assert_eq!(unversioned_installs(ok), 0, "self-test: {ok:?} is pinned");
    }
    // A `tool: |` block counts EVERY unpinned spec in it, not one.
    let block = "      with:\n        tool: |\n          cargo-a\n          cargo-b@1.2.3, cargo-c\n      - name: next\n";
    assert_eq!(unversioned_installs(block), 2);
    let pinned_block =
        "        tool: |\n          cargo-a@1.0.0\n          cargo-b@2.0.0\n        next: x\n";
    assert_eq!(unversioned_installs(pinned_block), 0);
    // A block with nothing in it still fails closed.
    assert_eq!(
        unversioned_installs("        tool: |\n        next: x\n"),
        1
    );
    assert!(!is_exact_version("0.9"));
    assert!(!is_exact_version("^1.2.3"));
    assert!(!is_exact_version("1.2.3-"));
    assert!(is_exact_version("0.9.148"));
}
