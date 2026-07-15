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

Composite-unique key per I-P1-11 discipline (extended to WS streams), with
`feed` added for per-feed identity (operator 2026-06-23 — a Dhan and a Groww
connection can share `(ws_type, connection_index)`, so each feed's lifecycle
events stay distinct rows; pinned by
`dedup_segment_meta_guard::per_feed_market_data_dedup_keys_must_include_feed`):
`(ts, trading_date_ist, feed, ws_type, connection_index, event_kind)`.

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
- Append site (LIVE since #1113): `order_update_connection.rs`
  (`WsType::OrderUpdate`) — clean-close (`Ok(())`), error-disconnect (`Err`
  arm), reconnect (`connect_and_listen`), sleep-entered/resumed — each calls
  `emit_order_update_ws_audit`. Both live WebSockets now write `ws_event_audit`
  rows. A future depth pool would call the same pattern with its own `WsType`.
- *(2026-07-15: the THREE Groww append sites below — bridge lifecycle rows, the boot-time
  Connected row, and the stall-watchdog `stall_restarted` rows — are DELETED with the Groww
  live feed (writers gone with `groww_bridge.rs` / `groww_sidecar_supervisor.rs`); the
  historical `feed='groww'` rows remain readable in `ws_event_audit`. Paragraphs retained
  as historical audit.)*
- Append site (HISTORICAL — writer deleted 2026-07-15): the Groww second-feed bridge
  `crates/app/src/groww_bridge.rs` (`WsType::GrowwBridge`, `feed='groww'`) —
  `emit_groww_ws_audit` fires one row per Groww lifecycle transition (Connected
  on the first streaming rising edge, Disconnected/DisconnectedOffHours on the
  feed-disable falling edge, Reconnected on a later streaming edge), edge-latched
  so it never spams per loop turn. Closes the gap where Groww connected but wrote
  no `ws_event_audit` row even though the table is already feed-keyed. Best-effort
  `try_send` through the SAME consumer Dhan uses — an ILP failure is still
  AUDIT-WS-01 (no new ErrorCode).
- **2026-07-04 update — boot-time Groww Connected row added (operator parity
  directive: "i need the same view display everything even for groww").** The
  streaming-edge latch above meant a CLOSED-market boot wrote NO Groww row at
  all (socket connected + 768 subscriptions, zero signals). The bridge now
  ALSO emits ONE `Connected` row (source `groww_subscribed`, reason "socket
  connected + subscribed — awaiting first tick") AT SOCKET-CONNECT time — on
  the first observed sidecar `subscribed`/`streaming` status per connected
  episode, edge-latched by `GrowwAuditLatches::boot_connect_announced`
  (re-armed only on a genuine disconnect falling edge: feed disable / bridge
  death — never per poll turn or reconnect attempt). The EXISTING
  first-streaming-tick rising-edge row (source `groww_sidecar` /
  `groww_resumed`, reason "groww streaming") is KEPT UNCHANGED as the
  streaming confirmation — the boot-time row is ADDITIVE; the two are
  distinguished by `source`/`reason`, and the companion Telegram ping
  (`FeedConnectedAwaitingTicks`) keeps saying "awaiting first tick" so
  socket-connected is never presented as streaming (the
  `groww-second-feed-scope-2026-06-19.md` §5 honest-envelope split —
  CAPTURE vs UPSTREAM, connected ≠ streaming — still holds). Same
  best-effort `try_send`; a write failure is still AUDIT-WS-01.
- Append site (LIVE since scoreboard PR-B, 2026-07-10): the Groww sidecar
  stall watchdog `crates/app/src/groww_sidecar_supervisor.rs`
  (`emit_stall_ws_audit`) — ONE `WsEventKind::StallRestarted`
  (`stall_restarted`) row per stall-watchdog kill+relaunch (classic
  FEED-STALL-01 arm + the never-streamed arm), `feed='groww'` /
  `ws_type='groww_bridge'`, with a FIXED machine cause slug in `source`
  (`feed_blame::STALL_SOURCE_*` — never raw child text) and the silent
  window in `down_secs`. Same best-effort `try_send`; a failure is still
  AUDIT-WS-01. The 15:45 IST dual-feed scorecard aggregates these into
  stall episodes (`dual-feed-scoreboard-error-codes.md` §2). NOT an
  up-kind and NOT a plain disconnect — and (PR-B fix round 1, 2026-07-10
  review HIGH) the scoreboard's process-death pairing treats it as fully
  TRANSPARENT: it is never an "up" signal AND it never occupies the
  last-pre-boot slot (`synthesize_process_death_episodes` /
  `post_boot_pairing_complete` skip it when tracking the prior row — the
  parked-wake precedent). Rationale: the stall kill+relaunch restores
  streaming WITHOUT a fresh Connected/Reconnected row (the bridge's audit
  latches re-arm only on feed-disable / bridge-death falling edges), so
  letting the stall row displace the morning connect acted as a DOWN
  marker and permanently blocked the boot-only Groww process-death
  synthesis for the rest of the day. A genuine `disconnected` /
  `sleep_entered` row still occupies the slot, so the skip can never
  resurrect a genuinely-down key.

---

## §1.1. 2026-07-05 Update — loud drops + ILP-HTTP ACK + order-update Connected row

The operator caught `ws_event_audit` EMPTY on the Mac on 2026-07-05
(`select * from ws_event_audit where feed='groww'` = 0 rows) — a silent
false-OK class incident (audit-findings Rule 11). Two verified silent-loss
windows were closed, plus one missing lifecycle row:

1. **Producer drop arms are now LOUD.** The three `try_send` drop sites
   (main-feed `connection.rs::emit_ws_audit`, order-update
   `emit_order_update_ws_audit`, Groww bridge `emit_groww_ws_audit`) upgraded
   from `debug!` to `error!(code = AUDIT-WS-01)` + the NEW counter
   `tv_ws_event_audit_dropped_total{reason="full"|"closed"}` (static labels
   only; the dropped row's pre-redaction content still never reaches a log).
   Drops remain non-blocking — the WS loops are never stalled; they are just
   no longer invisible. Ratchets:
   `crates/core/tests/ws_audit_loud_drops_guard.rs` +
   `crates/app/tests/ws_audit_loud_drops_guard.rs` (no `debug!` in any drop
   arm; counter + code present).
2. **Transport is ILP-over-HTTP with a per-flush server ACK.**
   `WsEventAuditWriter::new` now builds `http::addr=host:<http_port>` (was
   `tcp::addr=host:<ilp_port>`). ILP TCP flush is fire-and-forget — a
   server-side reject (schema drift, DEDUP violation) NEVER returned `Err`, so
   the §1 AUDIT-WS-01 flush arm never paged while the table stayed empty. HTTP
   flush ACKs every request; rejects now surface as `Err` → the existing
   AUDIT-WS-01 arm pages. Cold path (a handful of rows/day) — HTTP latency is
   irrelevant. Ratchets: `test_ilp_http_conf_targets_http_port` (storage unit)
   + the app guard's `test_ws_event_audit_writer_uses_ilp_http` (no
   `tcp::addr` in the module).
3. **Order-update WS now stamps its initial `Connected` row.** Previously the
   order-update connection emitted Disconnected / DisconnectedOffHours /
   Reconnected / SleepEntered / SleepResumed but NEVER `Connected` — "Order
   Update WS connected" left no forensic row. `connect_and_listen` now emits
   `WsEventKind::Connected` (source `n/a`, reason "order-update initial
   connect / clean-close reconnect") on every successful connect that is NOT a
   failure-recovery (those keep emitting `Reconnected`). Edge semantics hold:
   exactly one row per connect episode, never per loop turn.
4. **Metric semantics note:** `tv_ws_event_audit_rows_total{ws_type,event_kind}`
   counts CLIENT-SIDE successful append+flush sends in the consumer — with the
   HTTP ACK it now implies the server accepted the flush, but it remains a
   client-side counter, not a table row count. The forensic ground truth is
   the `ws_event_audit` table itself
   (`mcp__tickvault-logs__questdb_sql "select count(*) from ws_event_audit"`).

---

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `AuditWs01*` variant)
- `crates/common/src/ws_event_types.rs`
- `crates/storage/src/ws_event_audit_persistence.rs`
- Any file containing `AUDIT-WS-01`, `AuditWs01`, `ws_event_audit`, `WsEventKind`, or `WsType`
