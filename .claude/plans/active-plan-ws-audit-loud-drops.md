# Implementation Plan: ws_event_audit loud drops + ILP-over-HTTP ACK + order-update Connected row

**Status:** VERIFIED
**Date:** 2026-07-05
**Approved by:** Parthiban (operator) — 2026-07-05, this session: operator caught
`ws_event_audit` EMPTY on the Mac ("ws_event_audit where feed='groww'" = 0 rows)
— a silent false-OK class incident (audit-findings Rule 11). Root cause already
established: (1) the three producer `try_send` drop arms log at `debug!` (invisible,
no counter); (2) the writer flushes over ILP TCP (fire-and-forget — server-side
rejects never surface as `Err`, so the AUDIT-WS-01 flush arm never pages).

> Per-item guarantee matrix: cross-reference
> `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row matrices
> apply as written; this is a cold-path forensic-record fix — no hot-path change,
> no new tick-drop path, no DEDUP-key change).

## Design

Two verified silent-loss windows on the `ws_event_audit` forensic chain are
closed, plus one missing lifecycle row:

1. **Loud drop arms.** The three `ws_event_audit` producer `try_send` drop sites
   (`crates/core/src/websocket/connection.rs::emit_ws_audit`,
   `crates/core/src/websocket/order_update_connection.rs::emit_order_update_ws_audit`,
   `crates/app/src/groww_bridge.rs::emit_groww_ws_audit`) upgrade from `debug!`
   to `error!(code = ErrorCode::AuditWs01EventWriteFailed.code_str(), ...)` and
   increment a NEW counter `tv_ws_event_audit_dropped_total{reason="full"|"closed"}`
   (static labels only). The log carries NO row content — the pre-redaction
   `reason` field of the dropped row never reaches a log (existing security
   review stance preserved).
2. **ILP-over-HTTP with per-flush ACK.** `WsEventAuditWriter::new`
   (`crates/storage/src/ws_event_audit_persistence.rs`) switches its
   `Sender::from_conf` config from `tcp::addr=host:9009` to
   `http::addr=host:9000` (the `QuestDbConfig::http_port`). questdb-rs 6.1.0
   default features include `sync-sender-http`, so no Cargo change. HTTP flush
   returns a real server ACK — server-side rejects surface as `Err` from
   `flush()` → the EXISTING AUDIT-WS-01 flush arm in
   `crates/app/src/main.rs::run_ws_event_audit_consumer` pages. Cold path
   (a handful of rows/day), latency irrelevant. Conf string built by a private
   testable helper `ilp_http_conf()`.
3. **Order-update initial `Connected` row.** `connect_and_listen`
   (`order_update_connection.rs`) emits `WsEventKind::Connected` right after a
   successful socket connect when `failures_before_attempt == 0` (a
   failure-recovery connect keeps emitting `Reconnected` exactly as today) —
   mirroring the main-feed initial-connect emit (`connection.rs` ~980-987).
   Edge semantics preserved: exactly one row per connect episode
   (`connect_and_listen` connects once per invocation; no per-loop-turn spam).
4. **Rule-file note.** `.claude/rules/project/ws-event-audit-error-codes.md`
   gains a dated 2026-07-05 section documenting the loud drops, the HTTP
   transport, the order-update Connected row, and that
   `tv_ws_event_audit_rows_total` counts client-side sends (the flush that the
   HTTP ACK now verifies).

Crates touched: `tickvault-core`, `tickvault-storage`, `tickvault-app`
(files: `crates/core/src/websocket/connection.rs`,
`crates/core/src/websocket/order_update_connection.rs`,
`crates/storage/src/ws_event_audit_persistence.rs`,
`crates/app/src/groww_bridge.rs`).

## Edge Cases

- **`TrySendError::Full` vs `Closed`:** distinguished into the static `reason`
  label via an exhaustive match (no allocation, no Display of the row).
- **Consumer dead (`closed`):** every subsequent lifecycle event now emits one
  `error!` + counter increment — bounded by lifecycle-event cadence (cold path,
  at most a few per reconnect cycle), never per tick; no coalescing needed.
- **HTTP sender at construction:** `Sender::from_conf("http::addr=…")` does not
  dial at build time; an unreachable QuestDB surfaces at `flush()` as `Err`
  (the AUDIT-WS-01 arm) instead of at construction. The lazy `sender = None`
  fallback path is kept for a malformed conf.
- **Order-update first connect after a failure streak:** keeps emitting
  `Reconnected` (unchanged); `Connected` fires only on a `failures == 0`
  connect, so no double row for the same establish.
- **Clean-close → reconnect (failures reset to 0):** the fresh connect episode
  emits `Connected` — every socket establish now leaves exactly one forensic
  row (previously: none).

## Failure Modes

- **Audit channel full during a reconnect storm:** rows drop exactly as before
  (never stall the WS loop), but now LOUDLY — `error!` (AUDIT-WS-01, Medium,
  auto-triage-safe) + `tv_ws_event_audit_dropped_total{reason="full"}`.
- **Consumer task dead:** `reason="closed"` drops page the same code; the
  operator learns immediately instead of finding an empty table days later.
- **QuestDB HTTP endpoint down / server-side reject (schema drift, DEDUP
  violation):** `flush()` now returns `Err` (real ACK) → existing AUDIT-WS-01
  flush arm logs `error!` + `tv_ws_event_audit_write_errors_total{stage="flush"}`.
  Rows stay in the local buffer per existing semantics; next event retries.
- **No behavioural risk to the WS loops:** drop arms remain non-blocking
  (`try_send`); the emit is cold path; no recovery-path change.

## Test Plan

- `crates/core/src/websocket/connection.rs`: unit test
  `test_ws_audit_drop_reason_labels` for the pure `Full`/`Closed` → label
  helper (shared by both core drop sites).
- `crates/storage/src/ws_event_audit_persistence.rs`: unit test
  `test_ilp_http_conf_targets_http_port` pinning the `http::addr=host:http_port`
  conf string (ratchet b).
- `crates/core/tests/ws_audit_loud_drops_guard.rs` (NEW source-scan ratchet):
  (a) the two core drop arms carry `error!` + `tv_ws_event_audit_dropped_total`
  and contain NO `debug!`; (c) `order_update_connection.rs` contains the
  `WsEventKind::Connected` emit.
- `crates/app/tests/ws_audit_loud_drops_guard.rs` (NEW source-scan ratchet):
  the groww_bridge drop arm carries `error!` + the counter and no `debug!`;
  `crates/storage` writer uses `http::addr` (scanned via relative path).
- Scoped runs: `cargo test -p tickvault-storage ws_event`,
  `cargo test -p tickvault-core websocket` (touched module) and the new guard,
  `cargo test -p tickvault-app` groww_bridge + guard tests. `cargo fmt --check`
  + `cargo clippy` on touched crates.

## Rollback

Single revert of the one squash-merged PR commit restores: `debug!` drop arms,
TCP ILP transport, no order-update Connected row, and the pre-2026-07-05 rule
file. No schema change, no DEDUP-key change, no config change — nothing to
migrate back. The new counter simply stops being emitted.

## Observability

- NEW counter `tv_ws_event_audit_dropped_total{reason="full"|"closed"}` (static
  labels) — one increment per dropped forensic row at any of the 3 producers.
- Drop arms log `error!` with `code = AUDIT-WS-01` → 5-sink chain (errors.jsonl,
  Telegram routing per severity, CloudWatch) — the previously-silent window is
  now pageable and greppable.
- Flush failures keep `tv_ws_event_audit_write_errors_total{stage}` — now
  REAL signal because HTTP flush carries a server ACK (TCP never did).
- `tv_ws_event_audit_rows_total{ws_type,event_kind}` unchanged — counts
  client-side successful append+flush sends (noted in the rule file).
- Rule file `.claude/rules/project/ws-event-audit-error-codes.md` updated with
  the dated 2026-07-05 note (runbook stays the cross-ref target for
  `AuditWs01EventWriteFailed`).

## Plan Items

- [x] Item 1 — Loud drop arms (3 sites) + `tv_ws_event_audit_dropped_total`
  - Files: crates/core/src/websocket/connection.rs, crates/core/src/websocket/order_update_connection.rs, crates/app/src/groww_bridge.rs
  - Tests: test_ws_audit_drop_reason_labels
- [x] Item 2 — Writer ILP-over-HTTP (`http::addr=host:http_port`)
  - Files: crates/storage/src/ws_event_audit_persistence.rs
  - Tests: test_ilp_http_conf_targets_http_port
- [x] Item 3 — Order-update initial `Connected` forensic row
  - Files: crates/core/src/websocket/order_update_connection.rs
  - Tests: test_order_update_connected_emit_is_wired (source-scan)
- [x] Item 4 — Source-scan ratchets (no debug! in drop arms; http::addr; Connected emit)
  - Files: crates/core/tests/ws_audit_loud_drops_guard.rs, crates/app/tests/ws_audit_loud_drops_guard.rs
  - Tests: test_core_drop_arms_are_loud, test_groww_drop_arm_is_loud, test_ws_event_audit_writer_uses_ilp_http
- [x] Item 5 — Rule-file dated note
  - Files: .claude/rules/project/ws-event-audit-error-codes.md
  - Tests: (docs — covered by existing error_code_rule_file_crossref)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Audit channel full at a Groww lifecycle edge | `error!` AUDIT-WS-01 + `tv_ws_event_audit_dropped_total{reason="full"}`; bridge loop never stalls |
| 2 | Consumer task dead, main-feed reconnects | `reason="closed"` drops page; operator sees the dead consumer same-day |
| 3 | QuestDB rejects a row server-side (schema/DEDUP) | HTTP flush returns Err → existing AUDIT-WS-01 flush arm pages (TCP silently swallowed this) |
| 4 | Order-update WS connects at boot | one `Connected` row (`ws_type=order_update`) lands in `ws_event_audit` |
| 5 | Order-update recovers after failure streak | `Reconnected` row exactly as before — no duplicate `Connected` |
