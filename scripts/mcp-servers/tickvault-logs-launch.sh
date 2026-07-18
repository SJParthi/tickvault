#!/usr/bin/env bash
# tickvault-logs MCP launcher (rust-only phase 2c cutover, 2026-07-18).
#
# Launches the Rust MCP server (crates/tickvault-logs-mcp) for the
# `.mcp.json` `tickvault-logs` entry. The python server.py it replaces is
# DELETED from git (the parity harness re-materializes it from pinned git
# history for regression testing only).
#
# Launch policy (coordinator decision, phase-2c open Unknown resolved):
#   1. A prebuilt release binary, if present, launches instantly.
#      HONEST CAVEAT: a prebuilt binary can be STALE relative to the
#      checked-out sources; `cargo build --release -p tickvault-logs-mcp`
#      refreshes it (cargo rebuilds only on change).
#   2. Fallback: `cargo run --release -q -p tickvault-logs-mcp`
#      (build-on-first-use; build noise goes to stderr, never the MCP
#      stdout wire).
#
# All arguments (e.g. --self-test) pass through to the binary.
set -euo pipefail
cd "$(dirname "$0")/../.."

BIN="${CARGO_TARGET_DIR:-target}/release/tickvault-logs-mcp"
if [ -x "$BIN" ]; then
    exec "$BIN" "$@"
fi
exec cargo run --release -q -p tickvault-logs-mcp -- "$@"
