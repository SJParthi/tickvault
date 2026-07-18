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
//!
//! 2026-07-18 (rust-only phase 2c, PRE-CUTOVER transition window): the
//! Rust port `crates/tickvault-logs-mcp` now exists with full 14-tool
//! parity. During the parallel-run window BOTH implementations are
//! pinned: every python assertion below stays untouched, and the new
//! `rust_port_*` tests pin the Rust crate. `.mcp.json` MUST keep
//! pointing at server.py until the separate cutover PR swaps it after
//! live parallel-run validation.

use std::fs;
use std::path::{Path, PathBuf};

const SERVER_PY: &str = "scripts/mcp-servers/tickvault-logs/server.py";
const README_MD: &str = "scripts/mcp-servers/tickvault-logs/README.md";
const MCP_JSON: &str = ".mcp.json";
// 2026-07-18: the Rust port (phase 2c) — dual-pinned with server.py.
const RUST_CRATE_DIR: &str = "crates/tickvault-logs-mcp";
const RUST_TOOLS_RS: &str = "crates/tickvault-logs-mcp/src/tools.rs";
const RUST_RPC_RS: &str = "crates/tickvault-logs-mcp/src/rpc.rs";

/// The five tools Claude Code's loop prompt and the error-triage hook
/// rely on. Removing any one would silently break the zero-touch chain.
const REQUIRED_TOOL_NAMES: &[&str] = &[
    "tail_errors",
    "list_novel_signatures",
    "summary_snapshot",
    "triage_log_tail",
    "signature_history",
    // Phase 7.2 expansion — live-query tools for Claude co-work.
    // `prometheus_query` + `list_active_alerts` were RETIRED in #O5
    // (2026-05-30) — they pointed at the now-removed Prometheus (#O3) +
    // Alertmanager (#O2) containers. Use questdb_sql / CloudWatch instead.
    "find_runbook_for_code",
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
        // urllib retained for tool_tickvault_api (the prometheus_query +
        // list_active_alerts tools that originally needed it were retired
        // in #O5, 2026-05-30).
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

// ---------------------------------------------------------------------------
// 2026-07-18 — Rust-port pins (phase 2c, pre-cutover transition window).
// The Rust crate `crates/tickvault-logs-mcp` must keep full tool parity
// with server.py for as long as both live side by side. These tests do
// NOT replace any python pin above — they are ADDITIVE. Deleting either
// implementation before the cutover PR fails the build.
// ---------------------------------------------------------------------------

/// The FULL 14-tool surface both implementations must expose. (The
/// python `REQUIRED_TOOL_NAMES` above is the historical 10-tool core;
/// this is the complete current set the parity harness byte-compares.)
const FULL_TOOL_SURFACE: &[&str] = &[
    "tail_errors",
    "list_novel_signatures",
    "summary_snapshot",
    "triage_log_tail",
    "signature_history",
    "find_runbook_for_code",
    "questdb_sql",
    "grep_codebase",
    "run_doctor",
    "git_recent_log",
    "tickvault_api",
    "docker_status",
    "app_log_tail",
    "cloudwatch_logs",
];

#[test]
fn rust_port_crate_exists_with_core_sources() {
    let root = workspace_root();
    for rel in [
        RUST_CRATE_DIR,
        RUST_TOOLS_RS,
        RUST_RPC_RS,
        "crates/tickvault-logs-mcp/Cargo.toml",
        "crates/tickvault-logs-mcp/src/lib.rs",
        "crates/tickvault-logs-mcp/src/main.rs",
        "crates/tickvault-logs-mcp/src/config.rs",
        "crates/tickvault-logs-mcp/src/signature.rs",
        "crates/tickvault-logs-mcp/src/sigv4.rs",
    ] {
        let path = root.join(rel);
        assert!(
            path.exists(),
            "{} missing — the Rust MCP port (phase 2c) is dual-pinned with \
             server.py until the cutover PR",
            path.display()
        );
    }
}

#[test]
fn rust_port_tools_rs_registers_the_full_14_tool_surface() {
    let src = load_text(RUST_TOOLS_RS);
    let mut missing: Vec<&'static str> = Vec::new();
    for tool in FULL_TOOL_SURFACE {
        let needle = format!("\"{tool}\"");
        if !src.contains(&needle) {
            missing.push(tool);
        }
    }
    assert!(
        missing.is_empty(),
        "crates/tickvault-logs-mcp/src/tools.rs is missing tool names \
         (14-tool parity with server.py is the phase-2c contract):\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn rust_port_python_server_covers_same_full_surface() {
    // Bidirectional parity pin: every tool the Rust crate must expose is
    // also registered in server.py (name="<tool>" ToolSpec needles). A
    // tool added to one side only fails here.
    let src = load_text(SERVER_PY);
    let mut missing: Vec<&'static str> = Vec::new();
    for tool in FULL_TOOL_SURFACE {
        let needle = format!("name=\"{tool}\"");
        if !src.contains(&needle) {
            missing.push(tool);
        }
    }
    assert!(
        missing.is_empty(),
        "server.py is missing tools the Rust port exposes — the 14-tool \
         parity surface drifted:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn rust_port_rpc_handles_required_jsonrpc_methods() {
    let src = load_text(RUST_RPC_RS);
    for method in ["initialize", "tools/list", "tools/call"] {
        assert!(
            src.contains(&format!("\"{method}\"")),
            "rpc.rs does not handle JSON-RPC method `{method}`"
        );
    }
    assert!(
        src.contains("2024-11-05"),
        "rpc.rs must pin MCP protocolVersion 2024-11-05 (server.py parity)"
    );
}

#[test]
fn rust_port_cutover_not_yet_performed() {
    // Pre-cutover invariant: .mcp.json still points at server.py and
    // does NOT yet reference the Rust binary. The cutover swap is a
    // SEPARATE later PR after live parallel-run validation (2026-07-18).
    let src = load_text(MCP_JSON);
    assert!(
        src.contains("scripts/mcp-servers/tickvault-logs/server.py"),
        ".mcp.json no longer points at server.py — the cutover must be \
         its own PR with a dated rule/guard update, not a side effect"
    );
    assert!(
        !src.contains("tickvault-logs-mcp"),
        ".mcp.json references the Rust binary — premature cutover; the \
         swap is a separate PR after live parallel-run validation"
    );
}
