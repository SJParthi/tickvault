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
#   1. A prebuilt release binary launches instantly, but ONLY when no
#      input it was built from is newer than it (the crate's own files,
#      the workspace Cargo.toml and Cargo.lock; it depends on no other
#      workspace crate). Until 2026-09-27 a stale binary was launched
#      as-is, so a checked-out fix (the PR29 read-only SQL gate, PR29b)
#      did not run until someone rebuilt by hand. A stale binary now
#      falls through to step 2, which rebuilds it.
#   2. Fallback: `cargo run --release -q -p tickvault-logs-mcp`
#      (build-on-first-use; build noise goes to stderr, never the MCP
#      stdout wire).
#
# All arguments (e.g. --self-test) pass through to the binary.
set -euo pipefail
cd "$(dirname "$0")/../.."

BIN="${CARGO_TARGET_DIR:-target}/release/tickvault-logs-mcp"
if [ -x "$BIN" ] \
    && [ -z "$(find crates/tickvault-logs-mcp Cargo.toml Cargo.lock -type f -newer "$BIN" -print -quit 2>/dev/null)" ]; then
    exec "$BIN" "$@"
fi
exec cargo run --release -q -p tickvault-logs-mcp -- "$@"
echo "tickvault-logs-launch: FATAL - no launch path executed" >&2; exit 1
