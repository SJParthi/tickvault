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

/// Recursively collect every `Cargo.toml` in the repo, skipping build/VCS
/// caches and symlinks so the scan is deterministic and cannot loop.
fn collect_manifests(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if matches!(
                name.as_ref(),
                "target" | ".git" | ".cargo" | "node_modules" | "data"
            ) {
                continue;
            }
            collect_manifests(&entry.path(), out);
        } else if name == "Cargo.toml" {
            out.push(entry.path());
        }
    }
}

/// Depth-first walk over a parsed manifest, recording the dotted key-path of
/// every reference to the `trading-core` crate: a literal `trading-core` key
/// (plain, dotted-table, target-specific, or `[patch]` form), a rename entry
/// carrying `package = "trading-core"`, or a bare `package = "trading-core"`
/// string anywhere (arrays of tables included).
fn find_trading_core_refs(value: &toml::Value, path: &mut Vec<String>, hits: &mut Vec<String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                path.push(key.clone());
                let is_tc_key = key == "trading-core";
                let is_tc_rename = child
                    .as_table()
                    .and_then(|t| t.get("package"))
                    .and_then(toml::Value::as_str)
                    == Some("trading-core");
                let is_tc_package_str = key == "package" && child.as_str() == Some("trading-core");
                if is_tc_key || is_tc_rename || is_tc_package_str {
                    hits.push(path.join("."));
                }
                find_trading_core_refs(child, path, hits);
                path.pop();
            }
        }
        toml::Value::Array(items) => {
            for (idx, item) in items.iter().enumerate() {
                path.push(idx.to_string());
                find_trading_core_refs(item, path, hits);
                path.pop();
            }
        }
        _ => {}
    }
}

/// `trading-core` may appear in exactly two sanctioned places: the root
/// `[workspace.dependencies]` pin and the `crates/trading` `[dev-dependencies]`
/// inherit. Every other reference — dotted tables, `package = "trading-core"`
/// renames, target-specific tables, `[patch]` sections, any other manifest
/// (fuzz included) — fails the build. Manifests are PARSED (never
/// string-matched) and an unparsable manifest fails closed.
#[test]
fn test_consumer_uses_workspace_pin_as_dev_dependency_only() {
    let root = repo_root();
    let mut manifests = Vec::new();
    collect_manifests(&root, &mut manifests);
    manifests.sort();
    assert!(
        manifests.len() >= 10,
        "manifest scan looks broken: expected the >= 10 known Cargo.toml files, found {}",
        manifests.len()
    );

    let root_manifest = root.join("Cargo.toml");
    let trading_manifest = root.join("crates").join("trading").join("Cargo.toml");
    let mut root_pin_seen = false;
    let mut dev_dep_seen = false;

    for manifest in &manifests {
        let text = fs::read_to_string(manifest)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", manifest.display()));
        let parsed: toml::Table = text
            .parse()
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", manifest.display()));
        let mut hits = Vec::new();
        find_trading_core_refs(&toml::Value::Table(parsed), &mut Vec::new(), &mut hits);
        for hit in hits {
            let sanctioned_root =
                *manifest == root_manifest && hit == "workspace.dependencies.trading-core";
            let sanctioned_dev =
                *manifest == trading_manifest && hit == "dev-dependencies.trading-core";
            assert!(
                sanctioned_root || sanctioned_dev,
                "unsanctioned trading-core reference at `{hit}` in {} — bruteX trading-core is allowed ONLY as the root [workspace.dependencies] pin and the crates/trading [dev-dependencies] entry",
                manifest.display()
            );
            root_pin_seen |= sanctioned_root;
            dev_dep_seen |= sanctioned_dev;
        }
    }
    assert!(
        root_pin_seen,
        "root Cargo.toml must carry the [workspace.dependencies] trading-core pin"
    );
    assert!(
        dev_dep_seen,
        "crates/trading/Cargo.toml must declare trading-core under [dev-dependencies]"
    );

    let trading: toml::Table = read("crates/trading/Cargo.toml")
        .parse()
        .expect("failed to parse crates/trading/Cargo.toml");
    let inherits_workspace = trading
        .get("dev-dependencies")
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("trading-core"))
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("workspace"))
        .and_then(toml::Value::as_bool);
    assert_eq!(
        inherits_workspace,
        Some(true),
        "crates/trading trading-core dev-dependency must be `{{ workspace = true }}`"
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
        "runtime sources must never reference the bruteX repo — consumption is compile-time-only via the pinned dev-dependency: {offenders:?}"
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
        "LicenseRef-Proprietary must stay scoped to the trading-core clarify/exception blocks, never the global [licenses] allow list"
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
