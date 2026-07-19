//! Phase 7.2 guard — the `tickvault-logs` MCP server must keep its
//! pinned tool surface so Claude Code's triage flow doesn't break when
//! the server silently drops a capability.
//!
//! 2026-07-18 (rust-only phase 2c, CUTOVER DONE): the server is the Rust
//! crate `crates/tickvault-logs-mcp`, launched from `.mcp.json` via
//! `scripts/mcp-servers/tickvault-logs-launch.sh`. The python
//! `scripts/mcp-servers/tickvault-logs/server.py` is DELETED from git
//! after parallel-run parity evidence (PR #1644 harness + the cutover
//! PR's live side-by-side matrix); the parity harness re-materializes it
//! from pinned git history (`SERVER_PY_PINNED_COMMIT` in
//! `crates/tickvault-logs-mcp/tests/parity.rs`), so the byte-parity gate
//! keeps running WITHOUT python living in the tree.
//!
//! This is a source-scan guard — dep-free, fast, runs in-process. What
//! it asserts (each a build-failing pin; none weaker than the
//! pre-cutover python pins they replace):
//!
//! 1. The python server stays RETIRED from git (a resurrection fails).
//! 2. `.mcp.json` launches the Rust server via the launcher — never
//!    python.
//! 3. The launcher exists, is executable, and is rust-only.
//! 4. The Rust crate keeps its core sources + the full 14-tool surface
//!    + the JSON-RPC methods + the pinned protocol version.
//! 5. The parity harness keeps the pinned git-history reference (the
//!    parity gate cannot be silently deleted or de-pinned).
//! 6. validate-automation keeps exercising the REAL rust launch path.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PYTHON_SERVER_DIR: &str = "scripts/mcp-servers/tickvault-logs";
const LAUNCHER_SH: &str = "scripts/mcp-servers/tickvault-logs-launch.sh";
const MCP_JSON: &str = ".mcp.json";
const RUST_CRATE_DIR: &str = "crates/tickvault-logs-mcp";
const RUST_TOOLS_RS: &str = "crates/tickvault-logs-mcp/src/tools.rs";
const RUST_RPC_RS: &str = "crates/tickvault-logs-mcp/src/rpc.rs";
const VALIDATE_SH: &str = "scripts/validate-automation.sh";

/// The FULL 14-tool surface the server must expose (the byte-parity
/// contract the cutover was validated against).
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
fn python_server_retired_from_git() {
    // Post-cutover pin (replaces the pre-cutover `server.py exists` pin,
    // inverted to the new truth): NOTHING under the old python server
    // dir is git-tracked. The path may exist ON DISK (the parity harness
    // re-materializes server.py there at runtime; the dir is gitignored)
    // — disk presence is deliberately NOT asserted either way.
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root())
        .args(["ls-files", "--", PYTHON_SERVER_DIR])
        .output()
        .expect("run git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    let tracked = String::from_utf8_lossy(&out.stdout);
    assert!(
        tracked.trim().is_empty(),
        "python MCP server files are git-tracked again — the rust-only \
         phase 2c cutover deleted them; a resurrection needs a fresh \
         dated operator decision:\n{tracked}"
    );
}

#[test]
fn mcp_json_registers_tickvault_logs_server() {
    let src = load_text(MCP_JSON);
    assert!(
        src.contains("\"tickvault-logs\""),
        ".mcp.json missing `tickvault-logs` server registration"
    );
    assert!(
        src.contains(LAUNCHER_SH),
        ".mcp.json `tickvault-logs` entry must launch the Rust server via \
         {LAUNCHER_SH}"
    );
}

#[test]
fn mcp_json_no_longer_launches_python() {
    // Post-cutover inverse of the old `.mcp.json points at server.py`
    // pin: no python launch of the tickvault-logs server may return.
    let src = load_text(MCP_JSON);
    assert!(
        !src.contains("server.py"),
        ".mcp.json references server.py — the python MCP server was \
         retired in the phase 2c cutover (rollback = a deliberate revert \
         of the cutover PR, never a partial re-point)"
    );
    assert!(
        !src.contains("mcp-servers/tickvault-logs/"),
        ".mcp.json references the retired python server dir"
    );
}

#[test]
fn launcher_exists_executable_and_rust_only() {
    let path = workspace_root().join(LAUNCHER_SH);
    assert!(path.is_file(), "{} missing", path.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "launcher must be executable (mode={mode:o})"
        );
    }
    let src = load_text(LAUNCHER_SH);
    // 2026-07-18 review MED-1 hardening: comment-strip the launcher
    // source before pinning the launch lines — the pre-hardening
    // substring asserts were satisfiable by the launcher's HEADER
    // COMMENT alone (a deleted exec line with the comment intact still
    // passed). Whole-line `#` comments are stripped; the `#!` shebang
    // is kept (it is code, not commentary).
    let code: String = src
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with('#') || t.starts_with("#!")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.contains("release/tickvault-logs-mcp"),
        "launcher must prefer the prebuilt release binary (in CODE, not a comment)"
    );
    assert!(
        code.contains("exec \"$BIN\""),
        "launcher must exec the prebuilt binary via `exec \"$BIN\"` (in CODE, \
         not a comment)"
    );
    assert!(
        code.contains("exec cargo run --release -q -p tickvault-logs-mcp"),
        "launcher must fall back to `exec cargo run --release` \
         (build-on-first-use) in CODE, not a comment"
    );
    assert!(
        code.contains("no launch path executed"),
        "launcher must keep the fail-loud safety tail (`no launch path \
         executed` + exit 1) after the final exec — it catches any future \
         deletion/regression of the exec lines"
    );
    assert!(
        !src.to_ascii_lowercase().contains("python"),
        "launcher must be rust-only — no python fallback"
    );
}

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
            "{} missing — the Rust MCP server is the ONLY tickvault-logs \
             implementation post-cutover",
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
         (the 14-tool surface is the cutover parity contract):\n  {}",
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
        "rpc.rs must pin MCP protocolVersion 2024-11-05 (the parity-frozen \
         protocol version)"
    );
}

#[test]
fn rust_port_binary_supports_self_test() {
    // validate-automation + the launcher's --self-test path depend on the
    // flag surviving (server.py --self-test parity).
    let src = load_text("crates/tickvault-logs-mcp/src/main.rs");
    assert!(
        src.contains("--self-test"),
        "main.rs must keep the --self-test flag (validate-automation runs \
         the launcher with it)"
    );
}

// The parity-harness pins (`parity_harness_pins_git_history_reference` +
// `parity_harness_keeps_the_transcript_test`) were RETIRED 2026-07-19
// (BATCH-5) with the harness itself: `crates/tickvault-logs-mcp/tests/parity.rs`
// — the test-time python3 + git-network fixture that re-materialized the
// deleted python server and byte-compared it — was deleted in the Rust-only
// purge (rust-only-forever-lock-2026-07-19.md). The `python_server_retired_from_git`
// pin above remains the standing guarantee that no python server is git-tracked.

#[test]
fn gitignore_masks_materialized_python_dir() {
    let src = load_text(".gitignore");
    assert!(
        src.contains("scripts/mcp-servers/tickvault-logs/"),
        ".gitignore must mask the runtime-materialized python dir — \
         otherwise a parity run leaves an untracked server.py that can be \
         accidentally re-committed"
    );
}

#[test]
fn validate_automation_exercises_rust_launch_path() {
    let src = load_text(VALIDATE_SH);
    assert!(
        src.contains("tickvault-logs-launch.sh --self-test"),
        "validate-automation.sh must self-test through the REAL .mcp.json \
         launch path (the launcher)"
    );
    assert!(
        src.contains("cargo test -p tickvault-logs-mcp --lib"),
        "validate-automation.sh must keep the Rust MCP unit-test check"
    );
    assert!(
        !src.contains("python3 scripts/mcp-servers"),
        "validate-automation.sh still invokes the retired python MCP server"
    );
}
