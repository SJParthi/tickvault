# Implementation Plan: clear the deleted-runtime naming residue (Rust-only intent)

**Status:** APPROVED
**Date:** 2026-08-22
**Approved by:** Parthiban (operator), 2026-08-22 in-session: "fix and resolve
ebrythgind dude okay?" — given in direct response to a report naming this as the
single OPEN finding of the seven-hunt sweep, with its exact scope (15 files,
23 symbols, 153 sites) and the note that it needs a plan before the first edit.
**Guarantee matrices:** carried by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row).

## Design

`crates/tickvault-logs-mcp/src/pycompat.rs` and its 23 `py_*` helpers reproduce the
retired reference implementation's text-handling semantics so MCP tool output stays
byte-identical. Nothing executes a second runtime — these are ordinary Rust functions.
The vocabulary, however, reads against the operator's standing directive
(`rust-only-forever-lock-2026-07-19.md` §0 Quote 2, escalated 2026-08-01).

Rename, semantics untouched:

- `pycompat.rs` -> `legacy_compat.rs`; `pub mod pycompat` -> `pub mod legacy_compat`
- 23 `py_<name>` -> `legacy_<name>` (e.g. `py_slice_chars` -> `legacy_slice_chars`)
- prose comments naming the retired file, in `crates/*/src` only, reworded to
  "the retired reference implementation"

Scope is one crate (`tickvault-logs-mcp`) plus one comment in
`crates/aws-lambdas/src/operator_control.rs`.

## Edge Cases

- **Load-bearing literals must survive.** `!src.contains("server.py")` in
  `claude_session_bootstrap_guard.rs:183` and `tickvault_logs_mcp_guard.rs:132` is the
  enforcement itself; deleting the string deletes the check. Guard tests are the
  class-1 carve-out (a guard must name what it bans) and are NOT reworded.
- `py_` is a prefix of no other identifier in the tree; a bounded whole-word rename
  cannot collide.
- `ensure_ascii` is already neutrally named and only moves file.

## Failure Modes

- A missed call site -> compile error, caught before commit. Not silent.
- Over-eager substitution touching a guard literal -> the guard's own test fails.
- Renaming a public item another crate imports -> compile error; verified the module
  is referenced only from within `tickvault-logs-mcp`.

## Test Plan

- `cargo test -p tickvault-logs-mcp` (byte-parity goldens live here)
- `cargo test -p tickvault-common --test rust_only_guard --test tickvault_logs_mcp_guard
  --test claude_session_bootstrap_guard --test claude_mcp_endpoints_config_guard`
- `cargo build --workspace` for the cross-crate comment change
- Behaviour must be unchanged: this is a rename, so every existing golden passes as-is.

## Rollback

`git revert` of the single commit. No schema, config, wire format, or runtime behaviour
changes, so revert is total and needs no coordination.

## Observability

No new counters, logs, or alarms — nothing about runtime behaviour changes. The
observable outcome is `grep -r 'py_' crates/*/src` returning empty, which the guard
suites already assert on for the banned-token class.
