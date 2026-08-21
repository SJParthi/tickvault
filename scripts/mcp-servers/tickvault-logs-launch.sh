#!/usr/bin/env bash
# tickvault-logs MCP launcher (rust-only phase 2c cutover, 2026-07-18).
#
# Launches the Rust MCP server (crates/tickvault-logs-mcp) for the
# `.mcp.json` `tickvault-logs` entry. The legacy interpreted server it
# replaces is DELETED from git and nothing resurrects it: the parity harness
# that used to re-materialize it from pinned history was itself retired on
# 2026-08-01 (rust-only-forever-lock §0, second pass), so no test writes it
# back to disk either. Rust-only: no interpreter fallback.
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
echo "tickvault-logs-launch: FATAL - no launch path executed" >&2; exit 1
