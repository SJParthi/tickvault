# Plan: Dhan order-update WS paper-mode re-wire (default-OFF)

**Status:** APPROVED
**Date:** 2026-07-16
**Approved by:** Parthiban (operator), 2026-07-16 (events 38df2073-eecb-43cf-876d-a4a809dde269 + 157f7cd0-dfdf-4c4e-b93a-9f9aff3317c2)

**Directive (verbatim):** "Build real-time order, position and trade-update WebSockets for both Dhan and Groww, paper mode / off by default, no live orders yet. Edit the scope-lock rule files to allow it and use the socket-token the Groww channel needs. Everything's staged on branch claude/groww-order-position-push and PR #1597 — continue from there."

Guarantee matrices: per `.claude/rules/project/per-wave-guarantee-matrix.md` (cross-referenced).

## Design
Re-wire the dormant, zero-caller `crates/core/src/websocket/order_update_connection.rs` (unchanged) into `crates/app/src/dhan_rest_stack.rs` Phase 5a, gated on a new `[dhan_order_push] enabled` flag (serde default false). When enabled, spawn `run_order_update_connection` with `notifier: None` (Telegram-silent), `wal_spill: None`, url = the existing `wss://api-order-update.dhan.co` constant, and a `broadcast::channel::<OrderUpdate>(1024)`. A new supervised consumer `crates/app/src/dhan_order_push_observability.rs` maps each `OrderUpdate` to an `order_audit` row `feed='dhan'`/`mode='paper'` via the EXISTING `OrderAuditWriter` (no storage edits; best-fit event mapping, no new `OrderAuditEvent` variants). Receive-only; no live orders; `dry_run` untouched.
- Files: `crates/common/src/config.rs` (`DhanOrderPushConfig`), `config/base.toml`, `crates/app/src/dhan_rest_stack.rs`, `crates/app/src/dhan_order_push_observability.rs`, `crates/app/src/lib.rs`, `crates/app/tests/ws_event_audit_boot_guard.rs`.
- Tests: `dhan_order_push_config_defaults_off`, `ws_event_audit_boot_guard` (gated-wiring + `notifier: None` stub-guard).

## Edge Cases
Disabled (default): no spawn, one `info!` "dhan order push disabled (config)". Token not yet ready / Dhan RST of an idle socket: the module's own reconnect ladder handles it. Unknown order-status string: mapped to counter label "other", event best-fit to Placed. No live order events exist while `dry_run=true` and no orders are placed — the channel is genuinely receive-only.

## Failure Modes
Consumer task death → supervised respawn, `tv_dhan_order_push_respawn_total{reason}`. order_audit persist failure → existing AUDIT-06 `error!` stage pattern (mirror `order_observability.rs`), no new ErrorCode. WS disconnect → reconnect ladder in the dormant module (unchanged). No Telegram on any path (noise lock).

## Test Plan
`cargo fmt`; `cargo clippy -p tickvault-app -p tickvault-common -- -D warnings`; `cargo test -p tickvault-common` (config default); `cargo test -p tickvault-app` (consumer + boot guard); the `order_update_connection` module's own core tests; `cargo test --workspace` once (crates/common changed).

## Rollback
Config flip (`enabled = false`) fully disables at runtime with zero spawn. Per-commit `git revert`. No schema/storage change to undo (reuses the existing `order_audit` table).

## Observability
Counters `tv_dhan_order_updates_total{status}` (bounded labels) + `tv_dhan_order_push_respawn_total{reason}`; `ws_event_audit` rows for the connection lifecycle; `order_audit` rows feed='dhan'/mode='paper'. No Telegram, no new CloudWatch alarm (the 2 deleted order-update alarms stay deleted per the noise lock).
