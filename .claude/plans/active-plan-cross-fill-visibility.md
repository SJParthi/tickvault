# Implementation Plan: Cross-Fill Visibility (audit table + daily digest + portal tile + feeds relabel)

**Status:** APPROVED
**Date:** 2026-07-20
**Approved by:** Parthiban (operator directive 2026-07-20 — "every cross-fill highlighted, logged, monitored, audited, visualised — every day, week, month — precisely at what time it is happening"; standing pre-authorization relayed via the central takeover task)

## Design

The cadence engine (`crates/core/src/cadence/`) already flags cross-fills
(`lane.flags.cross_fill`, counter `tv_cadence_cross_fill_total{direction}`)
and Groww fallbacks (`tv_cadence_groww_fallback_total{leg}`) but leaves NO
queryable per-event record with the precise time. This PR adds:

1. **`cross_fill_audit` QuestDB table** (`crates/storage/src/cross_fill_audit_persistence.rs`,
   modeled on the current house template `rest_fetch_audit_persistence.rs`):
   columns `ts, trading_date_ist, lane, source_lane, stage, legs, rows_filled,
   cycle_latency_ms, ladder_rung, resolution, retry_attempts,
   resolved_at_ms_after_close`; DEDUP UPSERT KEYS
   `(ts, trading_date_ist, lane, cycle_minute_ist, stage)` (designated `ts`
   FIRST per the 2026-04-28 rule; `cycle_minute_ist` added as an INT column to
   satisfy the key contract). Idempotent ensure-DDL (CREATE → ALTER ADD COLUMN
   IF NOT EXISTS → DEDUP ENABLE) joins the cadence boot DDL spawn in
   `crates/app/src/cadence_boot.rs` (the scheduler owns the table's authors).
2. **Emit sites** (`crates/core/src/cadence/audit.rs` + `runner.rs`): a
   process-global `OnceLock` mpsc sink (the `global_expiry_store` house
   pattern — core cannot depend on storage) receives one
   `CrossFillAuditEvent` per cross-fill arm firing (`finalize_if_complete`)
   and per Groww-fallback launch (`GrowwVerdict`). Fire-and-forget
   `try_send`, NEVER on the decision path; a send/append/flush failure logs
   `error!` with the NEW `ErrorCode::Cadence04AuditWriteFailed`
   (`CADENCE-04`, Severity::Medium, runbook `cadence-error-codes.md` §4).
   The app-side consumer (`crates/app/src/cross_fill_visibility.rs`) drains
   the channel into the storage writer (the `ws_audit_consumer` pattern).
3. **Daily digest**: a small scheduled task (same module) fires at 15:47 IST
   on trading days, queries the day's `cross_fill_audit` rows over QuestDB
   `/exec`, and sends ONE plain-language Telegram via the NEW
   `NotificationEvent::CrossFillDailyDigest` (Severity::Info,
   DispatchPolicy::Immediate — the scorecard precedent). Gated on the table
   being queryable: a read failure sends the honest "could not be read —
   count unknown" body, never a false "0 times ✅" (Rule 11).
4. **Portal tile**: `GET /api/board/data` gains `db.cross_fill_today`
   (count) + `db.cross_fill_events` (short "9:16 AM dhan ← groww (spot)"
   strings; `null` = unreadable); the `/board` page renders ONE new
   "Cross-fill today" tile in the Vault strip.
5. **Feeds-panel relabel** (operator-scare fix): the retired live-WS lanes'
   Disabled verdict reason becomes "live-WS lane (retired) — off by design"
   (`Feed::live_ws_retired()` + `evaluate_feed_health` post-adjust); the
   `/feeds` page gains a prominent line stating market data is captured by
   the per-minute REST cadence, and the per-feed note says the same.
6. **Runbook** `docs/runbooks/cross-fill-visibility.md`: daily/weekly/monthly
   rollup SQL + what the digest means.

Counter check (deliverable 5): `tv_cadence_cross_fill_total{direction}` and
`tv_cadence_groww_fallback_total{leg}` verified present in `runner.rs`; the
audit rows carry the same identity, so no new counter is required beyond the
write-error/discard counters of the new writer.

## Edge Cases

- Zero cross-fills on a day → digest says "Cross-fill used 0 times today ✅"
  ONLY when the table answered the query (no false-OK on unreadable table).
- QuestDB down at emit time → writer buffers/flush fails → CADENCE-04
  `error!` + discard counter; the cadence decision path is untouched.
- Channel full/closed (consumer dead) → CADENCE-04 `error!` + drop counter;
  bounded channel (1024) so the runner never blocks.
- Non-trading day → digest task sleeps to the next IST midnight, sends
  nothing (market closed — no cycles ran).
- IST midnight rollover mid-sleep → loop recomputes from the fresh date.
- Groww-fallback rows stamp `resolution='native_late_retry'` and
  `resolved_at_ms_after_close = -1` (resolution unknown at launch — the
  T+4s retry session's PR will stamp measured values); cross-fill rows stamp
  `resolution='cross_fill'` + measured `resolved_at_ms_after_close`.
- Re-run/replay of the same (minute, lane, stage) → DEDUP UPSERTs in place
  (idempotent).
- Board tile with table absent (pre-first-boot) → query fails → honest `null`
  tile ("—"), never fabricated 0.

## Failure Modes

- Audit append/flush failure → `error!(code = "CADENCE-04")` +
  `tv_cross_fill_audit_write_errors_total{stage}` + discard-pending
  (poisoned-buffer defense, the rest_fetch_audit contract). Best-effort:
  the CADENCE-01 coalesced log + Telegram + counters still carry the event.
- Digest task death → supervised-lite loop with `error!` on query failure;
  the digest read-failure body is itself the honest signal.
- Ensure-DDL failure → HTTP-CLIENT-01-class documented duplicate-row window
  (same envelope as the sibling ensures in cadence_boot).

## Test Plan

- `crates/storage`: DDL string test (columns + DEDUP key + ts-first), writer
  append/flush/discard tests (for_test constructor), dedup-key segment
  meta-guard compliance (table has no security_id — lane/source_lane are
  feed lanes, not instruments).
- `crates/core`: audit-event emit unit tests (sink installed → event
  received with correct stage/legs/rung/latency; no sink → no panic);
  existing runner tests stay green.
- `crates/common`: ErrorCode catalogue tests (roundtrip/all-list/severity)
  absorb `Cadence04AuditWriteFailed`; crossref test satisfied by the
  rule-file mention; feed_health relabel test.
- `crates/app`: pure-fn tests for digest SQL builder, dataset parser,
  12-hour IST time formatter, digest line builder, trigger decide.
- `crates/api`: board payload field tests (null contract), board_page tile
  marker test, feeds_page prominent-line test.
- Full suites: `cargo test -p tickvault-storage -p tickvault-core -p
  tickvault-api` (+ common, app targeted) + fmt + pre-push-gate.

## Rollback

Every piece is additive and best-effort: revert the PR (single squash
commit) to restore the prior behaviour byte-identically. The
`cross_fill_audit` table is forensics-only — no reader depends on it for
decisions; it can be dropped manually with no data-path impact. The digest
task and tile degrade to absence; the feeds relabel is a pure string change.

## Observability

- New table `cross_fill_audit` (SEBI-style forensic chain, DEDUP-keyed).
- New counters: `tv_cross_fill_audit_write_errors_total{stage}`,
  `tv_cross_fill_audit_rows_discarded_total`,
  `tv_cross_fill_audit_dropped_total{reason}` (channel drops).
- New coded error `CADENCE-04` (`Cadence04AuditWriteFailed`) routed through
  the 5-sink chain; runbook section in
  `.claude/rules/project/cadence-error-codes.md` §4 + operator runbook
  `docs/runbooks/cross-fill-visibility.md`.
- Daily Telegram digest (`CrossFillDailyDigest`, Info/Immediate) at 15:47
  IST; `/board` tile for at-a-glance intraday visibility; weekly/monthly
  rollup SQL documented in the runbook.

## Plan Items

- [x] `crates/storage/src/cross_fill_audit_persistence.rs` — table + writer + ensure-DDL
  - Files: crates/storage/src/cross_fill_audit_persistence.rs, crates/storage/src/lib.rs
  - Tests: test_cross_fill_audit_create_ddl_shape, test_cross_fill_audit_append_and_discard
- [x] `Cadence04AuditWriteFailed` ErrorCode + rule-file mention
  - Files: crates/common/src/error_code.rs, .claude/rules/project/cadence-error-codes.md
  - Tests: existing catalogue tests + test_cadence_04_medium_auto_triage_safe
- [x] Core emit sites + OnceLock sink
  - Files: crates/core/src/cadence/audit.rs, crates/core/src/cadence/runner.rs, crates/core/src/cadence/mod.rs
  - Tests: test_cross_fill_audit_event_channel_roundtrip
- [x] App consumer + daily digest + boot wiring
  - Files: crates/app/src/cross_fill_visibility.rs, crates/app/src/cadence_boot.rs, crates/app/src/lib.rs
  - Tests: test_digest_lines_plain_english, test_digest_sql_builders, test_format_ist_12h
- [x] Telegram event variant
  - Files: crates/core/src/notification/events.rs
  - Tests: test_cross_fill_daily_digest_message_format
- [x] Board tile + feeds relabel
  - Files: crates/api/src/handlers/board.rs, crates/api/src/handlers/board_page.rs, crates/api/src/handlers/feeds_page.rs, crates/common/src/feed_health.rs, crates/common/src/feed.rs
  - Tests: test_board_cross_fill_null_contract, test_board_page_has_cross_fill_tile, test_feeds_page_prominent_rest_line, test_disabled_retired_lane_reason
- [x] Runbook
  - Files: docs/runbooks/cross-fill-visibility.md
  - Tests: n/a (docs)

## Per-Item Guarantee Matrix

See `per-wave-guarantee-matrix.md` — the 15-row 100% Guarantee Matrix and
the 7-row Resilience Demand Matrix apply to every item of this plan; the
per-item proofs are the Test Plan + Observability sections above and the
filled matrices in the PR body (the template carries both matrices).

## Zero-Loss Guarantee Charter check

- Coverage: unit tests per module above; audit table carries DEDUP keys; no
  new pub fn without test/TEST-EXEMPT + call site.
- Zero-loss/resilience: no new tick-drop path (cadence decision path
  untouched — audit is fire-and-forget try_send off the hot path); no
  hot-path allocation (cross-fill is cold-path, once per degraded cycle);
  DEDUP composite keys per spec; O(1) per emit.
- Evidence: real test output pasted in the PR body.
