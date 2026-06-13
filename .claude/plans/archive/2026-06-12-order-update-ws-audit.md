# Plan: order-update WebSocket → ws_event_audit recorder (completes WS lifecycle tracking)

**Status:** APPROVED
**Date:** 2026-06-12
**Approved by:** Parthiban (2026-06-12 — "meanwhile go ahead with the PRs" + standing "everything
should be entirely logged monitored tracked captured" for every WebSocket)
**Branch:** claude/nice-cerf-hemuvn
**Changed crates:** `core` (thread audit channel through `run_order_update_connection` +
`order_update_post_close_sleep` + `connect_and_listen`, emit at lifecycle points), `app` (extract a
shared `spawn_ws_event_audit_consumer` helper; wire it at the order-update spawn sites)

## Why (operator request, 2026-06-12)
#1112 added the `ws_event_audit` black-box for the MAIN-FEED connection. The order-update connection
(`wss://api-order-update.dhan.co`, the order-confirmation lifeline) is the OTHER live WebSocket — its
disconnect/reconnect/sleep events reach Telegram + CloudWatch logs but are NOT yet in the durable
`ws_event_audit` table. This completes the "every WebSocket event tracked" goal: the OrderUpdate
`WsType` variant + the storage append path are already built + tested in #1112, so this is a thin
wiring change (no new schema, no new table).

## Design
1. **Extract `spawn_ws_event_audit_consumer(questdb_cfg) -> mpsc::Sender<WsEventAuditRow>`** in
   `app/main.rs` — factor the channel-create + consumer-spawn block already living inside
   `create_websocket_pool` into a reusable helper (DRY). `create_websocket_pool` calls it; the
   order-update spawn site(s) call it too. This is the SAME self-contained pattern (no boot refactor)
   — a second consumer task writing to the same `ws_event_audit` table is fine (ILP appends are
   independent). The order-update connection is exactly 1 → `pool_size = 1`, `connection_index = 0`.
2. **Thread `ws_audit_tx: Option<mpsc::Sender<WsEventAuditRow>>`** through (last param, matches the
   `notifier` convention): `run_order_update_connection` → `order_update_post_close_sleep` (sleep
   entered/resumed) and `connect_and_listen` (reconnect).
3. **Free helper `emit_order_update_ws_audit(tx, kind, source, reason, dhan_code, down_secs, attempts)`**
   in `order_update_connection.rs` — builds a `WsEventAuditRow` with `WsType::OrderUpdate`,
   `connection_index = 0`, `pool_size = 1`, IST nanos + IST-midnight + market_hours, non-blocking
   `try_send` (never stalls the order-confirmation read loop). Cold path (`O(1) EXEMPT`).
4. **Emit at 4 lifecycle points** (mirrors the main-feed wiring):
   - Disconnect: the loop's `Err(err)` arm (`IncrementAndRetry`) → `WsEventKind::Disconnected`
     (in-market) / `DisconnectedOffHours` by `is_within_market_hours`. source via
     `classify_disconnect_cause(&err_string, None).label()`.
   - Reconnect: in `connect_and_listen` right where `OrderUpdateReconnected` fires (after a successful
     connect with `failures_before_attempt > 0`) → `WsEventKind::Reconnected` (down_secs not tracked
     for order-update → 0; attempts = failures_before_attempt).
   - Sleep entered / resumed: in `order_update_post_close_sleep` alongside the existing
     `WebSocketSleepEntered`/`WebSocketSleepResumed` notifies.

## Edge Cases
- `ws_audit_tx = None` (tests / not wired) → `emit_order_update_ws_audit` is a no-op early return.
- Order-update has no Dhan disconnect-code surface like the main feed → `dhan_code = None` (`-1`
  sentinel); source comes from the transport-error string classifier.
- `connect_and_listen` reconnect emit only fires when `failures_before_attempt > 0` (a real reconnect,
  not the first connect) — same gate the existing `OrderUpdateReconnected` Telegram uses.
- Two order-update spawn sites (normal + alt boot path) — each gets its OWN consumer via the helper;
  only one boot path runs per process, so exactly one consumer exists at runtime.

## Failure Modes
- ILP write fails in the consumer → `AUDIT-WS-01` `error!` + `tv_ws_event_audit_write_errors_total`
  (same consumer as #1112); the order-update reconnect logic is UNAFFECTED (best-effort audit).
- Audit channel full → `try_send` drops the row + `debug!` (never blocks the order-confirmation loop).
- A reason embedding a token → redacted at the ILP boundary in `WsEventAuditWriter::append_row`
  (already wss-aware after #1111).

## Test Plan
- `core`: `order_update_ws_audit_wiring_guard` source-scan — `run_order_update_connection` /
  `order_update_post_close_sleep` / `connect_and_listen` accept `ws_audit_tx`; `WsType::OrderUpdate`
  is stamped; the 4 lifecycle `WsEventKind::*` emit points are present.
- `app`: extend `ws_event_audit_boot_guard` — the order-update spawn site(s) call
  `spawn_ws_event_audit_consumer`; the extracted helper exists.
- Reuse #1112's storage tests (OrderUpdate row already covered by
  `test_append_row_each_event_kind_and_ws_type`).
- Scoped: `cargo test -p tickvault-core -p tickvault-app`; fmt + clippy; banned-pattern (the
  `O(1) EXEMPT` block covers the cold-path `.to_string()`); pub-fn guards; 3-agent review before+after.

## Rollback
Additive, best-effort (same class as #1112). Revert = `git revert`; no schema/data-path change; the
order-confirmation flow is untouched (emit is fire-and-forget `try_send`).

## Observability
- Layer 6: `ws_event_audit` rows now also carry `ws_type=order_update` (the only remaining untracked
  live WebSocket — after this, BOTH live sockets are in the black box).
- Layer 1: reuses `tv_ws_event_audit_rows_total{ws_type="order_update",event_kind}` +
  `..._write_errors_total` (no new counters).
- Layers 4/5 (logs/Telegram): unchanged existing order-update events.
- Operator query: `select * from ws_event_audit where ws_type='order_update' and trading_date_ist=today()`.

## Per-Item Guarantee Matrix
Carries the 15+7 matrix by reference — `.claude/rules/project/per-wave-guarantee-matrix.md`. Slice
specifics: 100% audit (order-update lifecycle now in the SEBI-retentioned table); 100% security
(reason redacted at the boundary); 100% logging (unchanged + now durable); honest envelope: this
completes the 2 CURRENT live sockets — the future depth pools (§ websocket-connection-scope-lock) would
each call the same `emit_*` with their `WsType`, still zero schema change.

## Plan Items
- [x] Extract `spawn_ws_event_audit_consumer` helper; `create_websocket_pool` uses it
  - Files: crates/app/src/main.rs
  - Tests: ws_event_audit_boot_guard (extended)
- [x] Thread `ws_audit_tx` + `emit_order_update_ws_audit` through the order-update connection;
  emit at disconnect/reconnect/sleep-entered/sleep-resumed
  - Files: crates/core/src/websocket/order_update_connection.rs
  - Tests: order_update_ws_audit_wiring_guard (source-scan)
- [x] Wire the order-update spawn site(s) to create a consumer + pass the tx
  - Files: crates/app/src/main.rs
  - Tests: ws_event_audit_boot_guard (extended)

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | order-update disconnects in-market | row ws_type=order_update kind=disconnected source=<network/...> |
| 2 | order-update reconnects after N failures | row kind=reconnected attempts=N |
| 3 | order-update enters post-close sleep | row kind=sleep_entered down_secs=sleep_secs |
| 4 | order-update resumes from sleep | row kind=sleep_resumed |
| 5 | audit channel full | row dropped via try_send; order-confirmation loop never blocks |
