# Plan: WebSocket lifecycle AUDIT TABLE — every connect/disconnect/reconnect/sleep tracked, future-proof for 5+5+5+1 connections

**Status:** APPROVED
**Date:** 2026-06-12
**Approved by:** Parthiban (2026-06-12 — "everything should be entirely logged monitored tracked
captured ... if i establish 5 live market feed + 5 depth-20 + 5 depth-200 + 1 order-update ...
everything needs to be tracked ... add this into the queue plan then go ahead")
**Branch:** claude/nice-cerf-hemuvn
**Changed crates:** `storage` (new `ws_event_audit` table + persistence), `common` (typed `WsType`/
`WsEventKind` enums + an `AUDIT-WS-01` ErrorCode), `core` (wire the append at the 6 WS notify sites),
`app` (boot-time `ensure_ws_event_audit_table` + writer wiring)

## Why (operator request, 2026-06-12)
The 6 WS lifecycle events (Connected/Disconnected/DisconnectedOffHours/Reconnected/SleepEntered/
SleepResumed) currently reach Telegram + (after #1111) CloudWatch logs — but there is NO durable,
queryable, SEBI-retentioned forensic record. Logs roll off; the operator's "everything tracked/
captured" demand needs a QuestDB audit table answering "show me every disconnect on every connection
this month, with source + downtime". CONFIRMED GAP: no `ws_*_audit` table exists in `crates/storage`.

**Future-proofing (operator's explicit 2026-06-12 scenario):** the runtime is LOCKED to 2 WebSockets
today (1 main-feed + 1 order-update — `websocket-connection-scope-lock.md`). The operator asked that
IF they later run 5 main-feed + 5 depth-20 + 5 depth-200 + 1 order-update (= 16 connections), every
one is tracked with NO schema change. This plan makes the audit schema + append API `ws_type` +
`connection_index` aware from day one so that expansion is a no-op for tracking. This plan does NOT
add any depth connection (that still requires a separate operator-approved rule-file edit per the
scope lock); it ONLY makes the tracking generic.

## Design
1. **New typed enums in `common`** (compile-time exhaustive, future WS types just add a variant):
   - `WsType { MainFeed, Depth20, Depth200, OrderUpdate }` → `as_str()` wire labels
     `main_feed`/`depth_20`/`depth_200`/`order_update`.
   - `WsEventKind { Connected, Disconnected, DisconnectedOffHours, Reconnected, SleepEntered, SleepResumed }`
     → `as_str()` wire labels.
2. **New `ws_event_audit` QuestDB table** (`crates/storage/src/ws_event_audit_persistence.rs`,
   following the canonical 8-element audit template in `phase-0-architecture.md` / mirroring
   `tick_conservation_audit_persistence.rs`):
   | Column | Type | Note |
   |---|---|---|
   | `ts` | TIMESTAMP | designated, IST nanos→micros |
   | `trading_date_ist` | TIMESTAMP | partition/dedup |
   | `ws_type` | SYMBOL | main_feed/depth_20/depth_200/order_update |
   | `connection_index` | LONG | 0..pool_size-1 (0 for order_update) |
   | `pool_size` | LONG | configured conns of this ws_type (future-proof: 1 today, 5 later) |
   | `event_kind` | SYMBOL | connected/disconnected/.../sleep_resumed |
   | `source` | SYMBOL | classifier label (dhan_too_many/token_expired/network_reset/...) |
   | `reason` | STRING | REDACTED via `sanitize::redact_url_params` (no JWT) |
   | `dhan_code` | LONG | 805/807/... ; `-1` sentinel = none |
   | `down_secs` | LONG | reconnect only (0 otherwise) |
   | `attempts` | LONG | reconnect only |
   | `market_hours` | BOOLEAN | was the event inside [09:00,15:30) IST |
   - **DEDUP UPSERT KEYS:** `(trading_date_ist, ws_type, connection_index, event_kind, ts)`.
     `ws_type`+`connection_index` make it I-P1-11-style composite-unique across the 16 future streams;
     `ts` lets two same-kind events on the same conn in the same second both survive (template rule 4).
3. **`WsEventAuditWriter`** — async ILP writer (ring→retry), `append_ws_event(WsEventAuditRow)`,
   `ensure_ws_event_audit_table` (idempotent CREATE + ALTER ADD COLUMN IF NOT EXISTS), nanos→micros.
4. **Wiring (core/app):** at the 6 WS notify sites in `connection.rs` (main-feed) the code already
   builds reason/source/down_secs/attempts/dhan_code — pass the SAME values to
   `append_ws_event(WsType::MainFeed, connection_index, pool_size=effective_main_feed_pool_size, kind,
   ...)`. Order-update site (`order_update_connection.rs`) appends with `WsType::OrderUpdate`. The
   append helper takes `ws_type` as a parameter, so a FUTURE depth connection calls the identical
   helper with `WsType::Depth20`/`Depth200` — zero new audit code needed for expansion.
5. **`AUDIT-WS-01` ErrorCode** (`common`) — append failure (ILP down / disk full); `error!` with
   `code` + rule-file mention (cross-ref test). Mirrors the existing AUDIT-NN family.

## Edge Cases
- Order-update has 1 conn → `connection_index=0`, `pool_size=1`. Depth-200 future = 1 SID/conn but
  still 5 conns → `pool_size=5`, indices 0–4.
- `dhan_code` absent (transport error) → `-1` sentinel (INT column, never NULL).
- Two disconnects same conn same second (807 then retry-fail) → both rows survive (`ts` in DEDUP key).
- Sleep events have no reason/source → `source="n/a"`, `reason=""`, `dhan_code=-1`.
- IST-midnight boundary: `trading_date_ist` derives from the event ts, so a 23:59→00:00 disconnect/
  reconnect pair lands on the correct two dates.
- Append must NOT block the reconnect path → async writer, cold path (per-disconnect, never per-tick).

## Failure Modes
- ILP write fails → `AUDIT-WS-01` `error!` + counter `tv_ws_event_audit_write_errors_total`; the WS
  reconnect logic is unaffected (audit is best-effort, never gates recovery).
- QuestDB down at boot → `ensure_ws_event_audit_table` retried in the existing boot DDL join; the
  BOOT-01/02 gate already covers QuestDB readiness.
- Reason accidentally carries a token → redacted via `redact_url_params` (now wss-aware after #1111).
- A future 6th WS type added without an enum variant → won't compile (exhaustive match) — forces the
  author to extend tracking, which is the point.

## Test Plan
- `ws_event_audit_persistence::tests`: DDL contains all columns; DEDUP key includes
  `ws_type`+`connection_index`+`ts` (dedup-segment meta-guard parity); nanos→micros; `WsType`/
  `WsEventKind` wire labels stable; `-1` dhan sentinel; row builder for each of the 6 event kinds.
- `common`: `WsType`/`WsEventKind` `as_str` round-trip + exhaustiveness tests; `AUDIT-WS-01`
  severity + runbook-path + rule-file cross-ref.
- `core`: source-scan guard that the 6 notify sites also call `append_ws_event` (so a future edit
  can't silently drop audit) + that the append is ws_type-parameterized (future-proof pin).
- Redaction: the audit `reason` goes through `redact_url_params` (assert no raw token path).
- Scoped: `cargo test -p tickvault-storage -p tickvault-common -p tickvault-core`; common change
  escalates to the downstream consumers; fmt + clippy; banned-pattern; 3-agent review before+after.

## Rollback
Additive: new table (idempotent CREATE), new enums, new append calls that are best-effort. Revert =
`git revert`; the audit is never on a data-correctness path, so removing it loses only the forensic
record, not ticks/orders. No migration (QuestDB ignores a dropped append).

## Observability
- Layer 6 (audit table): `ws_event_audit` — the system-of-record this slice adds.
- Layer 1 (counter): `tv_ws_event_audit_rows_total{ws_type,event_kind}` + `..._write_errors_total`.
- Layer 4 (logs): unchanged #1111 warn!/info! per event (already redacted + sourced).
- Layer 5 (Telegram): unchanged 6 variants.
- Operator query: `select ws_type, connection_index, event_kind, source, down_secs, ts from
  ws_event_audit where trading_date_ist = today() order by ts desc` — every WS lifecycle event,
  per connection, for any of the current 2 or future 16 connections, identical query.

## Per-Item Guarantee Matrix
Carries the mandatory 15-row + 7-row matrix by reference — `.claude/rules/project/per-wave-guarantee-matrix.md`.
Slice specifics: 100% audit coverage (new `<event>_audit` table w/ DEDUP keys); 100% uniqueness
(composite `(trading_date_ist, ws_type, connection_index, event_kind, ts)` — I-P1-11 discipline
extended to WS streams); 100% security (reason redacted); honest envelope: tracking is generic over
ws_type so the future 5+5+5+1 expansion needs ZERO audit-code change — but the expansion itself
still requires a separate operator-approved scope-lock rule edit (this plan does not lift the 2-WS lock).

## Plan Items
- [x] `WsType` + `WsEventKind` typed enums + wire labels + tests
  - Files: crates/common/src/ws_event_types.rs (new), crates/common/src/lib.rs
  - Tests: ws_event_types unit tests (as_str round-trip, exhaustiveness)
- [x] `AUDIT-WS-01` ErrorCode variant + rule-file mention
  - Files: crates/common/src/error_code.rs, .claude/rules/project/ws-event-audit-error-codes.md (new)
  - Tests: error_code_rule_file_crossref, severity/runbook tests
- [x] `ws_event_audit` table + `WsEventAuditWriter` (8-element template)
  - Files: crates/storage/src/ws_event_audit_persistence.rs (new), crates/storage/src/lib.rs
  - Tests: ddl columns, DEDUP key (ws_type+connection_index+ts), nanos→micros, row builders
- [x] Wire the main-feed connection lifecycle sites (ws_type-parameterized) via `emit_ws_audit` at the
  `record_disconnect` choke point (all 4 disconnect paths) + reconnect + both sleep sites; pool attaches
  the channel with `pool_size = num_connections` (future-proof for 5 main-feed conns)
  - Files: crates/core/src/websocket/connection.rs, crates/core/src/websocket/connection_pool.rs
  - Tests: ws_event_audit_wiring_guard (3 source-scan tests)
  - NOTE: `order_update_connection.rs` (WsType::OrderUpdate) is the documented NEXT sub-PR (same
    `emit_ws_audit` pattern, different WsType). The OrderUpdate variant + storage append path are
    already built + tested here, so the follow-up is a thin wiring change.
- [x] Boot wiring: `ensure_ws_event_audit_table` in DDL join + spawn writer
  - Files: crates/app/src/main.rs
  - Tests: source-scan that ensure_ws_event_audit_table is in the boot DDL join

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | main-feed conn 0 disconnects (807) | row: ws_type=main_feed, index=0, kind=disconnected, source=token_expired, dhan_code=807 |
| 2 | main-feed reconnects | row: kind=reconnected, down_secs+attempts populated, source from threaded code |
| 3 | order-update conn sleeps post-close | row: ws_type=order_update, index=0, kind=sleep_entered |
| 4 | FUTURE: depth-20 conn 3 disconnects | row: ws_type=depth_20, index=3, pool_size=5 — SAME code path, no schema change |
| 5 | two disconnects same conn same second | both rows survive (ts in DEDUP key) |
| 6 | ILP write fails | AUDIT-WS-01 error! + counter; reconnect unaffected |
