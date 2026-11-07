//! Guard test — every node package run from `.mcp.json` or a GitHub workflow
//! names an exact `@x.y.z` version (principle 3: every version pinned).
//!
//! Found by the 2026-11-07 deep audit: `.mcp.json` ran
//! `npx -y @modelcontextprotocol/server-github` and
//! `npx -y @modelcontextprotocol/server-postgres` with no version, so every
//! session and the 10-minute triage workflow (which runs with a write-capable
//! GitHub token) fetched whatever the registry served as `latest` that minute.
//! A tag (`@latest`), a range (`@^1.2.0`, `@1.x`) or a bare name is refused.
//!
//! The count of node invocations itself is budgeted separately by
//! `rust_only_guard.rs` (`NODE_RUNTIME_BUDGET`); this guard only checks that
//! each one that remains is pinned.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or(manifest)
}

/// Node package runners whose next non-flag argument is ONE package to run.
const RUNNERS: &[&str] = &["npx", "bunx", "pnpx"];
/// `<tool> <sub>` pairs whose next non-flag argument is one package to run.
const RUNNER_SUBCOMMANDS: &[(&str, &str)] = &[
    ("npm", "exec"),
    ("npm", "x"),
    ("pnpm", "dlx"),
    ("yarn", "dlx"),
    ("bun", "x"),
];
/// `<tool> <sub>` pairs whose every following non-flag argument is a package.
const INSTALL_SUBCOMMANDS: &[(&str, &str)] = &[
    ("npm", "install"),
    ("npm", "i"),
    ("npm", "add"),
    ("pnpm", "add"),
    ("pnpm", "install"),
    ("yarn", "add"),
    ("yarn", "global"),
    ("bun", "add"),
    ("bun", "install"),
];

/// True when `spec` is `name@MAJOR.MINOR.PATCH` (scoped names allowed), with
/// no tag, range, prerelease wildcard or missing version.
fn is_exact_pin(spec: &str) -> bool {
    let (scope_prefix, rest) = match spec.strip_prefix('@') {
        Some(r) => ("@", r),
        None => ("", spec),
    };
    let Some((name, version)) = rest.rsplit_once('@') else {
        return false;
    };
    if name.is_empty() || (scope_prefix == "@" && !name.contains('/')) {
        return false;
    }
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn clean(token: &str) -> &str {
    token.trim_matches(|c| c == '"' || c == '\'' || c == ',' || c == '`')
}

/// Every package argument a command line passes to a node runner/installer.
fn package_args(tokens: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = clean(tokens[i]);
        let next = tokens.get(i + 1).map(|s| clean(s)).unwrap_or("");
        let (start, all) = if RUNNERS.contains(&t) {
            (i + 1, false)
        } else if RUNNER_SUBCOMMANDS.contains(&(t, next)) {
            (i + 2, false)
        } else if INSTALL_SUBCOMMANDS.contains(&(t, next)) {
            (i + 2, true)
        } else {
            i += 1;
            continue;
        };
        let mut j = start;
        while j < tokens.len() {
            let a = clean(tokens[j]);
            if a.is_empty() || a.starts_with('-') {
                j += 1;
                continue;
            }
            // A shell separator ends the command.
            if matches!(a, "&&" | "||" | ";" | "|") {
                break;
            }
            out.push(a.to_string());
            if !all {
                break;
            }
            j += 1;
        }
        i = j.max(i + 1);
    }
    out
}

fn mcp_json_package_args(root: &Path) -> Vec<(String, String)> {
    let path = root.join(".mcp.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let json: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path:?}: {e}"));
    let servers = json
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| panic!(".mcp.json has no mcpServers object"));
    let mut out = Vec::new();
    for (name, server) in servers {
        let mut tokens: Vec<String> = Vec::new();
        if let Some(cmd) = server.get("command").and_then(|v| v.as_str()) {
            tokens.extend(cmd.split_whitespace().map(str::to_string));
        }
        if let Some(args) = server.get("args").and_then(|v| v.as_array()) {
            tokens.extend(args.iter().filter_map(|a| a.as_str()).map(str::to_string));
        }
        let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
        for pkg in package_args(&refs) {
            out.push((format!(".mcp.json server `{name}`"), pkg));
        }
    }
    out
}

fn workflow_package_args(root: &Path) -> Vec<(String, String)> {
    let dir = root.join(".github/workflows");
    let mut out = Vec::new();
    let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yml") | Some("yaml")
        );
        if !is_yaml {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with('#') {
                continue;
            }
            let tokens: Vec<&str> = code.split_whitespace().collect();
            for pkg in package_args(&tokens) {
                out.push((format!("{}:{}", path.display(), n + 1), pkg));
            }
        }
    }
    out
}

#[test]
fn every_node_package_in_mcp_json_and_workflows_is_pinned_to_an_exact_version() {
    let root = repo_root();
    let mcp = mcp_json_package_args(&root);
    // Non-vacuity: the two node MCP servers must be seen by the scanner. If
    // they are removed, lower this together with NODE_RUNTIME_BUDGET.
    assert!(
        mcp.len() >= 2,
        "scanner found only {} node package(s) in .mcp.json: {mcp:?}",
        mcp.len()
    );
    let mut all = mcp;
    all.extend(workflow_package_args(&root));
    let unpinned: Vec<&(String, String)> = all.iter().filter(|(_, p)| !is_exact_pin(p)).collect();
    assert!(
        unpinned.is_empty(),
        "UNPINNED NODE PACKAGE (principle 3: every version pinned): {unpinned:?}. \
         Write each as `name@MAJOR.MINOR.PATCH` (no tag, no range), taking the \
         number from the npm registry."
    );
}

#[test]
fn exact_pin_check_accepts_only_exact_versions() {
    assert!(is_exact_pin("@modelcontextprotocol/server-github@2025.4.8"));
    assert!(is_exact_pin("left-pad@1.3.0"));
    for bad in [
        "@modelcontextprotocol/server-github",
        "@modelcontextprotocol/server-github@latest",
        "@modelcontextprotocol/server-github@^2025.4.8",
        "left-pad@~1.3.0",
        "left-pad@1.x",
        "left-pad@1.3",
        "left-pad@1.3.0-beta.1",
        "left-pad",
        "@scope@1.2.3",
        "@1.2.3",
    ] {
        assert!(!is_exact_pin(bad), "`{bad}` must not count as an exact pin");
    }
}

#[test]
fn package_arg_scanner_finds_runner_and_installer_packages() {
    let toks = |s: &'static str| s.split_whitespace().collect::<Vec<_>>();
    assert_eq!(
        package_args(&toks("npx -y foo@1.2.3 --flag")),
        vec!["foo@1.2.3"]
    );
    assert_eq!(package_args(&toks("run: npx foo")), vec!["foo"]);
    assert_eq!(package_args(&toks("npm exec --yes bar")), vec!["bar"]);
    assert_eq!(
        package_args(&toks("npm install -g a@1.0.0 b && echo done")),
        vec!["a@1.0.0", "b"]
    );
    assert_eq!(package_args(&toks("pnpm dlx \"baz\"")), vec!["baz"]);
    assert!(package_args(&toks("npm ci")).is_empty());
    assert!(package_args(&toks("cargo test -p tickvault-common")).is_empty());
}
