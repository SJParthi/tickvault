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
//! `taiki-e/install-action` `tool:` entry in `.github/`, `scripts/` and the
//! `Makefile` carries an exact version (`--version X.Y.Z`, `--vers X.Y.Z` or
//! `name@X.Y.Z`). Lines that do not are counted per file against a
//! SHRINK-ONLY budget: the rows below are the ones outside the change that
//! added this guard, pinned so they cannot grow and so removing one forces
//! the row down.
//!
//! HONEST LIMITS: a version carried in a shell variable (`cargo install
//! "$name"`) counts as unversioned even if the variable holds `x@1.2.3`;
//! that errs toward failing. A multi-line `tool: |` block is counted as one
//! unversioned entry (fails closed). An install written another way (a
//! downloaded release binary, `curl | tar`) is not seen here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `(path, unversioned install lines)` measured 2026-10-26. Shrink-only.
const UNVERSIONED_INSTALL_BUDGET: &[(&str, usize)] = &[
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
            .args(["--", ".github", "scripts", "Makefile"])
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

/// True when `spec` (one `name@version` token) carries an exact version:
/// `@` followed by a digit and only digits and dots after it.
fn is_exact_at_version(spec: &str) -> bool {
    spec.rsplit_once('@').is_some_and(|(name, v)| {
        !name.is_empty()
            && v.starts_with(|c: char| c.is_ascii_digit())
            && v.chars().all(|c| c.is_ascii_digit() || c == '.')
            && !v.ends_with('.')
    })
}

/// Unversioned `cargo install` / `cargo binstall` invocations on a line.
///
/// Each invocation's arguments run to the next shell separator. It is
/// versioned when they hold `--version`/`--vers` followed by an exact
/// version, or a `name@X.Y.Z` token.
fn unversioned_cargo_installs(line: &str) -> usize {
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut hits = 0usize;
    let mut i = 0usize;
    while i + 1 < words.len() {
        let is_install = words[i].trim_start_matches(['(', '`', '"', '\'']) == "cargo"
            && matches!(words[i + 1], "install" | "binstall");
        if !is_install {
            i += 1;
            continue;
        }
        let mut versioned = false;
        let mut j = i + 2;
        while j < words.len() && !matches!(words[j], "&&" | "||" | ";" | "|" | "then") {
            let w = words[j].trim_matches(|c| matches!(c, '"' | '\'' | ')' | '`' | ';'));
            if (w == "--version" || w == "--vers")
                && words.get(j + 1).is_some_and(|v| {
                    let v = v.trim_matches(|c| matches!(c, '"' | '\'' | ')'));
                    v.starts_with(|c: char| c.is_ascii_digit())
                        && v.chars().all(|c| c.is_ascii_digit() || c == '.')
                })
            {
                versioned = true;
            }
            if w.starts_with("--version=") || w.starts_with("--vers=") {
                let v = w.split_once('=').map_or("", |(_, v)| v);
                if v.starts_with(|c: char| c.is_ascii_digit()) {
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
        i = j.max(i + 2);
    }
    hits
}

/// Unversioned entries on a `tool:` line: every comma-separated tool must be
/// `name@X.Y.Z`. A block scalar (`tool: |`) or an empty value counts as one
/// unversioned entry, so a form this parser cannot read fails closed.
fn unversioned_tool_entries(line: &str) -> usize {
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
    if value.is_empty() || value.starts_with(['|', '>']) {
        return 1;
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !is_exact_at_version(s))
        .count()
}

fn unversioned_installs(text: &str) -> usize {
    text.lines()
        .filter(|l| !is_comment_line(l))
        .map(|l| unversioned_cargo_installs(l) + unversioned_tool_entries(l))
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
}
