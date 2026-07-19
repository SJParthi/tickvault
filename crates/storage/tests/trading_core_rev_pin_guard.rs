//! Build-failing guard: the bruteX `trading-core` git dependency stays a
//! READ-ONLY, exact-rev-pinned, dev-only consumable.
//!
//! Operator lock (2026-07-18): tickvault-side access to `SJParthi/bruteX`
//! is READ-ONLY forever — see
//! `.claude/rules/project/brutex-readonly-lock-2026-07-18.md`. The crate is
//! compiled from a pinned commit at BUILD time; nothing may fetch bruteX at
//! RUNTIME, and the pin may never float on a branch or tag.

use std::fs;
use std::path::{Path, PathBuf};

const BRUTEX_URL: &str = "https://github.com/SJParthi/bruteX";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/storage has a workspace root two levels up")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The single root-manifest dependency line pinning trading-core (comment
/// lines and unrelated lines are ignored).
fn trading_core_dep_line() -> String {
    let root = read("Cargo.toml");
    root.lines()
        .find(|l| l.trim_start().starts_with("trading-core") && l.contains(BRUTEX_URL))
        .unwrap_or_else(|| panic!("root Cargo.toml must pin trading-core to {BRUTEX_URL}"))
        .to_string()
}

fn pinned_rev() -> String {
    let line = trading_core_dep_line();
    let start = line
        .find("rev = \"")
        .expect("dep line carries rev = \"...\"")
        + 7;
    let rest = &line[start..];
    let end = rest.find('"').expect("rev closes its quote");
    rest[..end].to_string()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[test]
fn test_trading_core_rev_is_exact_pinned() {
    let line = trading_core_dep_line();
    let rev = pinned_rev();
    assert_eq!(
        rev.len(),
        40,
        "rev must be a full 40-char commit sha: {rev}"
    );
    assert!(
        rev.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "rev must be lowercase hex: {rev}"
    );
    assert!(!line.contains("branch"), "no branch float allowed: {line}");
    assert!(!line.contains("tag"), "no tag float allowed: {line}");
}

#[test]
fn test_trading_core_pyo3_feature_disabled() {
    let line = trading_core_dep_line();
    assert!(
        line.contains("default-features = false"),
        "pyo3 must stay off — the pin needs default-features = false: {line}"
    );
    assert!(
        !line.contains("features = ["),
        "no feature opt-ins allowed on the trading-core pin: {line}"
    );
}

/// A `trading-core` dep line is OK only under exactly `[dev-dependencies]`.
/// Any OTHER dependency table — `[dependencies]`, `[build-dependencies]`,
/// `[target.'cfg(...)'.dependencies]`, or any future `*dependencies` section —
/// naming trading-core FAILS this guard: each would pull bruteX code into a
/// build/runtime graph.
#[test]
fn test_consumer_uses_workspace_pin_as_dev_dependency_only() {
    let manifest = read("crates/trading/Cargo.toml");
    let mut section = String::new();
    let mut dev_ok = false;
    let mut offending_section: Option<String> = None;
    for raw in manifest.lines() {
        let l = raw.trim();
        if l.starts_with('[') {
            section = l.to_string();
            continue;
        }
        if !l.starts_with("trading-core") {
            continue;
        }
        if section == "[dev-dependencies]" {
            assert!(
                l.contains("workspace = true"),
                "the dev-dependency must use the workspace pin: {l}"
            );
            dev_ok = true;
        } else if section.contains("dependencies") {
            offending_section = Some(section.clone());
        }
    }
    assert!(
        dev_ok,
        "crates/trading must consume trading-core as a dev-dependency"
    );
    assert!(
        offending_section.is_none(),
        "trading-core may ONLY appear under [dev-dependencies]; found it under \
         {} — that would pull bruteX code into a build/runtime graph",
        offending_section.as_deref().unwrap_or("?")
    );
}

#[test]
fn test_lockfile_matches_manifest_rev() {
    let rev = pinned_rev();
    let lock = read("Cargo.lock");
    let expected = format!("git+{BRUTEX_URL}?rev={rev}#{rev}");
    assert!(
        lock.contains(&expected),
        "Cargo.lock must lock the exact manifest rev: {expected}"
    );
}

#[test]
fn test_no_runtime_fetch_of_brutex_in_sources() {
    let mut files = Vec::new();
    walk(&repo_root().join("crates"), &mut files);
    assert!(
        !files.is_empty(),
        "crates/ walk found nothing — guard broken"
    );
    let offenders: Vec<String> = files
        .iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
        .filter(|p| p.components().any(|c| c.as_os_str() == "src"))
        .filter(|p| {
            fs::read_to_string(p)
                .map(|body| body.to_lowercase().contains("sjparthi/brutex"))
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "runtime sources must never reference the bruteX repo — consumption is \
         compile-time-only via the pinned dev-dependency: {offenders:?}"
    );
}

#[test]
fn test_deny_toml_scopes_the_git_source() {
    let deny = read("deny.toml");
    assert!(
        deny.contains(BRUTEX_URL),
        "deny.toml must allow-list the bruteX git source"
    );
    assert!(
        deny.contains("unknown-git = \"deny\""),
        "unknown git sources must stay denied"
    );
    assert!(
        deny.contains("crate = \"trading-core\""),
        "the license clarify/exception must name trading-core"
    );
    let lic_start = deny
        .find("[licenses]")
        .expect("deny.toml has a [licenses] section");
    let body = &deny[lic_start..];
    let lic_end = body[1..].find("\n[").map(|i| i + 1).unwrap_or(body.len());
    let licenses_body = &body[..lic_end];
    assert!(
        !licenses_body.contains("LicenseRef-Proprietary"),
        "LicenseRef-Proprietary must stay scoped to the trading-core \
         clarify/exception blocks, never the global [licenses] allow list"
    );
    assert!(
        deny.contains("[[licenses.exceptions]]"),
        "deny.toml must carry a [[licenses.exceptions]] block for trading-core"
    );
    assert!(
        deny.contains(r#"allow = ["LicenseRef-Proprietary"]"#),
        "deny.toml licenses exception must allow exactly LicenseRef-Proprietary"
    );
}
