# WebSocket Lifecycle Audit — Error Codes (AUDIT-WS-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C > this file.
> **Operator directive (2026-06-12):** *"everything should be entirely logged monitored
> tracked captured ... if i establish 5 live market feed + 5 depth-20 + 5 depth-200 + 1
> order-update ... everything needs to be tracked."*
> **Companion code:** `crates/storage/src/ws_event_audit_persistence.rs`,
> `crates/common/src/ws_event_types.rs` (`WsType` / `WsEventKind`),
> `crates/common/src/error_code.rs::ErrorCode::AuditWs01EventWriteFailed`.
> **Companion plan:** `.claude/plans/active-plan-ws-event-audit-table.md`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this file to
> mention every `AuditWs01*` variant.

---

## §0. Why this exists

The 6 WebSocket lifecycle events (Connected / Disconnected / DisconnectedOffHours / Reconnected /
SleepEntered / SleepResumed) reach Telegram + (after #1111) CloudWatch logs — but logs roll off.
The `ws_event_audit` QuestDB table is the durable, queryable, SEBI-retentioned system-of-record:
"show me every disconnect on every connection this month, with source + downtime".

The table + append API are **`ws_type` + `connection_index` aware** so the SAME code path tracks
the current 2 connections (1 main-feed + 1 order-update) AND a future 16 (5 main-feed + 5 depth-20 +
5 depth-200 + 1 order-update) with **zero schema change** — a future depth connection just calls
`append_ws_event(WsType::Depth20, index, ...)`. This does NOT lift the 2-WebSocket runtime lock
(`websocket-connection-scope-lock.md`); it only makes the tracking ready for that expansion.

Composite-unique key per I-P1-11 discipline (extended to WS streams):
`(trading_date_ist, ws_type, connection_index, event_kind, ts)`.

---

## §1. AUDIT-WS-01 — ws_event_audit row write failed

**Severity:** Medium. **Auto-triage safe:** Yes (best-effort forensic write; never gates recovery).

**Trigger:** `WsEventAuditWriter::append_ws_event` could not persist a WS lifecycle row via QuestDB
ILP (ILP TCP down, QuestDB unreachable, disk full). The WS reconnect/sleep logic is UNAFFECTED — the
audit is a forensic side-record, never on the data-correctness or recovery path. Supersedes the
never-shipped AUDIT-03 `ws_reconnect_audit` concept (this table covers all 6 event kinds, not just
reconnect).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — look for `AUDIT-WS-01`; the payload carries `ws_type`,
   `connection_index`, `event_kind`.
2. `tv_ws_event_audit_write_errors_total` rate non-zero → QuestDB ILP degraded; run `make doctor`
   (cross-check BOOT-01/BOOT-02 if it coincides with boot).
3. The WS events themselves are still in CloudWatch logs (#1111) + Telegram, so no operator-visible
   event is lost — only the queryable forensic row for that one event is missing until QuestDB
   recovers. The next event re-appends normally.

**Honest envelope:** the audit is best-effort. A sustained ILP outage loses forensic rows for the
outage window (the live logs/Telegram still fired); it never affects tick capture, order placement,
or reconnect behaviour.

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::AuditWs01EventWriteFailed`
- `crates/storage/src/ws_event_audit_persistence.rs`
- Append sites (LIVE today): the WS lifecycle points in
  `crates/core/src/websocket/connection.rs` (main-feed) — initial connect,
  disconnect (all 4 paths via the `record_disconnect` choke point), reconnect,
  sleep-entered, sleep-resumed — each calls `emit_ws_audit`.
- Append site (NEXT sub-PR, NOT yet wired): `order_update_connection.rs`
  (`WsType::OrderUpdate`). The `OrderUpdate` enum variant + the storage append
  path are already built + tested, so wiring the order-update connection is a
  thin follow-up that emits with the same `emit_ws_audit` pattern. Until it
  lands, the order-update connection produces NO `ws_event_audit` rows.

---

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `AuditWs01*` variant)
- `crates/common/src/ws_event_types.rs`
- `crates/storage/src/ws_event_audit_persistence.rs`
- Any file containing `AUDIT-WS-01`, `AuditWs01`, `ws_event_audit`, `WsEventKind`, or `WsType`
