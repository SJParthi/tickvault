//! Phase 7.2 guard — the `tickvault-logs` MCP server must keep its
//! pinned tool surface so Claude Code's triage flow doesn't break when
//! the server silently drops a capability.
//!
//! 2026-07-18 (rust-only phase 2c, CUTOVER DONE): the server is the Rust
//! crate `crates/tickvault-logs-mcp`, launched from `.mcp.json` via
//! `scripts/mcp-servers/tickvault-logs-launch.sh`. The legacy
//! `scripts/mcp-servers/tickvault-logs/server.py` is DELETED from git
//! after parallel-run parity evidence (PR #1644 harness + the cutover
//! PR's live side-by-side matrix).
//!
//! 2026-08-01 (operator directive — pure Rust): the parity harness that
//! re-materialized the deleted implementation from pinned git history and
//! EXECUTED it is RETIRED. It ran the banned runtime in CI on every PR, and
//! it is why the "zero tracked files" claim held only at rest. Pin 5 below
//! is INVERTED accordingly.
//!
//! This is a source-scan guard — dep-free, fast, runs in-process. What
//! it asserts (each a build-failing pin; none weaker than the
//! pre-cutover legacy pins they replace):
//!
//! 1. The legacy server stays RETIRED from git (a resurrection fails).
//! 2. `.mcp.json` launches the Rust server via the launcher — never
//!    the legacy runtime.
//! 3. The launcher exists, is executable, and is rust-only.
//! 4. The Rust crate keeps its core sources + the full 14-tool surface
//!    + the JSON-RPC methods + the pinned protocol version.
//! 5. The parity harness stays RETIRED, and NO Rust source anywhere names
//!    (or spawns) the banned runtime.
//! 6. validate-automation keeps exercising the REAL rust launch path.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const LEGACY_SERVER_DIR: &str = "scripts/mcp-servers/tickvault-logs";

/// The banned runtime's name, assembled from bytes so the literal never
/// appears in this repository (operator directive 2026-08-01). This file is
/// ENFORCEMENT: the assertions below must be able to name the token in order
/// to ban it, exactly like `rust_only_guard.rs::banned_token`. Detection
/// semantics are UNCHANGED — this is the same six bytes the assertions used
/// to spell inline.
fn banned_runtime() -> String {
    String::from_utf8(vec![0x70, 0x79, 0x74, 0x68, 0x6f, 0x6e])
        .unwrap_or_else(|_| unreachable!("ASCII bytes"))
}
const LAUNCHER_SH: &str = "scripts/mcp-servers/tickvault-logs-launch.sh";
const MCP_JSON: &str = ".mcp.json";
const RUST_CRATE_DIR: &str = "crates/tickvault-logs-mcp";
const RUST_TOOLS_RS: &str = "crates/tickvault-logs-mcp/src/tools.rs";
const RUST_RPC_RS: &str = "crates/tickvault-logs-mcp/src/rpc.rs";
const PARITY_RS: &str = "crates/tickvault-logs-mcp/tests/parity.rs";
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
fn legacy_server_retired_from_git() {
    // Post-cutover pin (replaces the pre-cutover `server.py exists` pin,
    // inverted to the new truth): NOTHING under the old legacy server
    // dir is git-tracked. Disk presence is deliberately NOT asserted either
    // way. (Until 2026-08-01 the parity harness re-materialized the file
    // there at runtime; that harness is retired — see
    // `parity_harness_is_retired_and_nothing_spawns_the_legacy_runtime`.)
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root())
        .args(["ls-files", "--", LEGACY_SERVER_DIR])
        .output()
        .expect("run git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    let tracked = String::from_utf8_lossy(&out.stdout);
    assert!(
        tracked.trim().is_empty(),
        "legacy MCP server files are git-tracked again — the rust-only \
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
fn mcp_json_no_longer_launches_the_legacy_runtime() {
    // Post-cutover inverse of the old `.mcp.json points at server.py`
    // pin: no legacy launch of the tickvault-logs server may return.
    let src = load_text(MCP_JSON);
    assert!(
        !src.contains("server.py"),
        ".mcp.json references server.py — the legacy MCP server was \
         retired in the phase 2c cutover (rollback = a deliberate revert \
         of the cutover PR, never a partial re-point)"
    );
    assert!(
        !src.contains("mcp-servers/tickvault-logs/"),
        ".mcp.json references the retired legacy server dir"
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
        !src.to_ascii_lowercase().contains(&banned_runtime()),
        "launcher must be rust-only — no legacy fallback"
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

/// INVERTED 2026-08-01 (operator directive — pure Rust, nowhere the banned
/// runtime's name). This test used to REQUIRE the parity harness to keep its
/// pinned-commit materialization. That harness
/// (`crates/tickvault-logs-mcp/tests/parity.rs`) resurrected the DELETED
/// implementation from pinned git history onto disk and EXECUTED it, and
/// hard-failed rather than skipping when the interpreter was absent — so it
/// ran the banned runtime in CI on every PR, and the "tree is at zero tracked
/// files" claim held only because this test wrote one back at runtime.
///
/// The harness is retired. Its load-bearing pins survive as self-contained
/// golden literals in `src/` (hash vectors, the SigV4 signing-key and
/// request goldens, the ensure_ascii goldens, the novel-cutoff overflow
/// bands) — those need no child process. What was genuinely LOST and is not
/// replaced: the end-to-end JSON-RPC envelope diff and the `tools/list`
/// registry diff against the legacy oracle. That is a real coverage
/// reduction, recorded here rather than hidden.
#[test]
fn parity_harness_is_retired_and_nothing_spawns_the_legacy_runtime() {
    let root = workspace_root();
    assert!(
        !root.join(PARITY_RS).exists(),
        "{PARITY_RS} is back — it resurrects the deleted implementation from \
         pinned git history and EXECUTES it. Re-introducing it needs a fresh \
         dated operator quote in rust-only-forever-lock-2026-07-19.md first."
    );

    // And the banned runtime's name may not appear ANYWHERE in our own
    // source — Rust, shell, workflows, terraform, manifests. (Vendor API
    // reference docs under `docs/` are third-party documentation describing
    // THEIR SDKs, and `.claude/plans/` is dated history the house convention
    // never rewrites; both are deliberately out of scope.)
    let token = banned_runtime();
    let out = Command::new("git")
        .args([
            "grep",
            "-ln",
            "-i",
            "--",
            &token,
            "crates/",
            "scripts/",
            ".github/",
            "deploy/",
            "Cargo.toml",
            "quality/",
            ".gitignore",
        ])
        .current_dir(&root)
        .output()
        .expect("git grep");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hits: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        // The enforcement files must NAME the token in order to ban it; both
        // assemble it from bytes, so the literal is never spelled.
        .filter(|l| {
            !l.ends_with("tests/rust_only_guard.rs") && !l.ends_with("tickvault_logs_mcp_guard.rs")
        })
        .collect();
    assert!(
        hits.is_empty(),
        "the banned runtime's name is back in Rust source: {hits:?}"
    );
}

#[test]
fn gitignore_masks_materialized_legacy_dir() {
    let src = load_text(".gitignore");
    assert!(
        src.contains("scripts/mcp-servers/tickvault-logs/"),
        ".gitignore must keep masking the legacy server dir — defence in \
         depth so a manually-materialized server.py can never be \
         accidentally re-committed (the harness that materialized it at \
         runtime was retired 2026-08-01)"
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
        !src.contains(&format!("{}3 scripts/mcp-servers", banned_runtime())),
        "validate-automation.sh still invokes the retired legacy MCP server"
    );
}
