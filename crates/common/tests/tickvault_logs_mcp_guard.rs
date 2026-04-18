//! Phase 7.2 guard — `scripts/mcp-servers/tickvault-logs/server.py` must
//! keep its five pinned tools so Claude Code's triage flow doesn't break
//! when the MCP server silently drops a capability.
//!
//! This is a source-scan guard — dep-free, fast, runs in-process. It does
//! NOT invoke the server (that would require a Python subprocess and
//! parsing JSON-RPC). What it asserts:
//!
//! 1. The `server.py` + `README.md` files exist at the pinned paths.
//! 2. `server.py` registers every required ToolSpec name.
//! 3. `server.py` exposes the `main()` entry + `_run_stdio_loop()`.
//! 4. `.mcp.json` registers the `tickvault-logs` server block.
//!
//! If any assertion fails, the build fails — future sessions can't
//! accidentally remove a tool Claude depends on.

use std::fs;
use std::path::{Path, PathBuf};

const SERVER_PY: &str = "scripts/mcp-servers/tickvault-logs/server.py";
const README_MD: &str = "scripts/mcp-servers/tickvault-logs/README.md";
const MCP_JSON: &str = ".mcp.json";

/// The five tools Claude Code's loop prompt and the error-triage hook
/// rely on. Removing any one would silently break the zero-touch chain.
const REQUIRED_TOOL_NAMES: &[&str] = &[
    "tail_errors",
    "list_novel_signatures",
    "summary_snapshot",
    "triage_log_tail",
    "signature_history",
    // Phase 7.2 expansion — live-query tools for Claude co-work
    "prometheus_query",
    "find_runbook_for_code",
    "list_active_alerts",
    // Phase 7.2 workspace-wide expansion — SQL / grep / doctor / git
    "questdb_sql",
    "grep_codebase",
    "run_doctor",
    "git_recent_log",
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_text(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn tickvault_logs_mcp_server_py_exists() {
    let path = workspace_root().join(SERVER_PY);
    assert!(
        path.exists(),
        "{} missing — the log-query MCP server is a Phase 7.2 dependency",
        path.display()
    );
}

#[test]
fn tickvault_logs_mcp_readme_exists() {
    let path = workspace_root().join(README_MD);
    assert!(path.exists(), "{} missing", path.display());
}

#[test]
fn tickvault_logs_mcp_server_registers_every_required_tool() {
    let src = load_text(SERVER_PY);
    let mut missing: Vec<&'static str> = Vec::new();
    for tool in REQUIRED_TOOL_NAMES {
        // Check the ToolSpec entry — must have `name="<tool>"`.
        let needle = format!("name=\"{tool}\"");
        if !src.contains(&needle) {
            missing.push(tool);
        }
    }
    assert!(
        missing.is_empty(),
        "tickvault-logs MCP server is missing required ToolSpec entries:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn tickvault_logs_mcp_server_has_stdio_loop_entry() {
    let src = load_text(SERVER_PY);
    assert!(
        src.contains("def _run_stdio_loop"),
        "server.py missing _run_stdio_loop — MCP transport would not work"
    );
    assert!(src.contains("def main("), "server.py missing main() entry");
}

#[test]
fn tickvault_logs_mcp_server_handles_required_jsonrpc_methods() {
    let src = load_text(SERVER_PY);
    for method in ["initialize", "tools/list", "tools/call"] {
        assert!(
            src.contains(&format!("\"{method}\"")),
            "server.py does not handle JSON-RPC method `{method}`"
        );
    }
}

#[test]
fn mcp_json_registers_tickvault_logs_server() {
    let src = load_text(MCP_JSON);
    assert!(
        src.contains("\"tickvault-logs\""),
        ".mcp.json missing `tickvault-logs` server registration"
    );
    assert!(
        src.contains("scripts/mcp-servers/tickvault-logs/server.py"),
        ".mcp.json `tickvault-logs` entry does not point at the right path"
    );
}

#[test]
fn tickvault_logs_mcp_server_is_stdlib_only() {
    // Dep-free requirement — no pip install. If an import line appears
    // that isn't in the stdlib allow-list, fail.
    let src = load_text(SERVER_PY);
    // Allow-list: every top-level import used by the current server.py.
    // Add to this list DELIBERATELY if you're approving a new stdlib
    // module; never add a pip-installable module.
    let stdlib_allowlist = [
        "from __future__",
        "import hashlib",
        "import json",
        "import os",
        "import sys",
        "from dataclasses",
        "from datetime",
        "from pathlib",
        "from typing",
        // Added for Phase 7.2 expansion (prometheus_query, list_active_alerts)
        "import urllib.parse",
        "import urllib.request",
        // Phase 7.2 workspace-wide (grep_codebase, run_doctor, git_recent_log)
        "import re",
        "import fnmatch",
        "import subprocess",
    ];
    for line in src.lines() {
        let trimmed = line.trim_start();
        if !(trimmed.starts_with("import ") || trimmed.starts_with("from ")) {
            continue;
        }
        // Skip indented imports inside functions (they're stdlib too).
        if line != trimmed {
            continue;
        }
        let matches_allowlist = stdlib_allowlist.iter().any(|a| trimmed.starts_with(a));
        assert!(
            matches_allowlist,
            "server.py has a non-stdlib top-level import: `{trimmed}`. \
             The tickvault-logs MCP server MUST be pip-dep-free. Add to \
             the allow-list in this test ONLY if the new import is stdlib."
        );
    }
}
