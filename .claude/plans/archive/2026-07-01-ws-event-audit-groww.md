# Implementation Plan: Wire the GROWW feed into the `ws_event_audit` table

**Status:** APPROVED
**Date:** 2026-06-28
**Approved by:** operator standing directive 2026-06-29 — close the one genuine
audit gap (Groww connects/disconnects but writes NO `ws_event_audit` row; only
Dhan does) and make the change merge-ready.

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C >
> `ws-event-audit-error-codes.md` (AUDIT-WS-01) > `data-integrity.md`
> (feed-in-key EVERYWHERE) > this file.
> **Scope:** ONE vertical slice — a REAL emit call from the Groww bridge into the
> SAME `ws_event_audit` table Dhan already writes, tagged `feed='groww'`. No
> skeleton, no inert pub surface (Rule 14), no schema migration.

## Verified facts (re-confirmed in code before writing this plan)

- `ws_event_audit` is multi-feed BY DESIGN. DDL + DEDUP carry `feed`:
  `crates/storage/src/ws_event_audit_persistence.rs:58-59` →
  `DEDUP_KEY_WS_EVENT_AUDIT = "ts, trading_date_ist, feed, ws_type, connection_index, event_kind"`;
  the `feed` SYMBOL column is in the CREATE DDL (`:71`) and self-heals on old
  tables via `ALTER … ADD COLUMN IF NOT EXISTS feed` + `DEDUP ENABLE UPSERT KEYS`
  (`:130-172`). `append_row` already writes `.symbol("feed", r.feed.as_str())`
  (`:254`). **No schema change is needed — the table is already feed-keyed.**
- The ONLY emit sites today are Dhan: `crates/core/src/websocket/connection.rs:672`
  (`emit_ws_audit`, stamps `feed: Feed::Dhan` at `:698`, `ws_type: self.ws_audit_type`)
  and `order_update_connection.rs`. Both feed an mpsc channel drained by
  `run_ws_event_audit_consumer` (`crates/app/src/main.rs:2971`) which calls
  `WsEventAuditWriter::{new,append_row,flush}` and increments
  `tv_ws_event_audit_write_errors_total` + `tv_ws_event_audit_rows_total`.
- `spawn_ws_event_audit_consumer(config.questdb.clone())`
  (`crates/app/src/main.rs:2952`) returns the `Sender<WsEventAuditRow>` already
  used by both Dhan paths. **It is reusable as-is for Groww.**
- The Groww bridge `run_groww_bridge` (`crates/app/src/groww_bridge.rs:682`)
  drives `feed_health` on its transitions but takes NO `ws_audit_tx` and writes NO
  audit row. Transition signals already present in the loop:
  - first `subscribed`/`streaming` (latch `connect_logged`, `:766`) +
    first parsed tick (`streaming_observed`, `:797`) → **Connected**
  - `!feed_runtime.is_enabled(Feed::Groww)` → clears `connected`, re-arms latches
    (`:750-759`) → **Disconnected / DisconnectedOffHours** (feed disabled)
  - sidecar status file absent / watcher channel closed + re-arm
    (`:731-735`) → reconnect-class signal
- `WsType` (`crates/common/src/ws_event_types.rs:28`) has exactly 4 variants
  (`MainFeed`/`Depth20`/`Depth200`/`OrderUpdate`) — **none is a sensible label for
  the Groww sidecar bridge** (it is NDJSON tailing of a Python sidecar, not a Dhan
  binary WS). `WsType::all()` (`:54`) is pinned at length 4 by
  `test_all_arrays_match_variant_counts` (`:198`).
- `WsEventKind` (`:68`) already has all 6 kinds we need: `Connected`,
  `Disconnected`, `DisconnectedOffHours`, `Reconnected`, `SleepEntered`,
  `SleepResumed`.
- `Feed::Groww` exists (`crates/common/src/feed.rs`) with `as_str() == "groww"`,
  and `WsEventAuditRow.feed: crate::feed::Feed` (`ws_event_types.rs:135`).

## Plan Items

- [x] Item 1 — Add a Groww-appropriate `WsType::GrowwBridge` variant + wire label.
  - Files: `crates/common/src/ws_event_types.rs`
  - Tests: `test_ws_type_as_str_labels_are_stable_and_unique`,
    `test_all_arrays_match_variant_counts` (updated 4→5)
- [x] Item 2 — Pass the existing `ws_audit_tx` into `run_groww_bridge` and emit a
  `WsEventAuditRow` at each Groww transition (Connected / Disconnected /
  DisconnectedOffHours / Reconnected), `feed=Feed::Groww`, mirroring `emit_ws_audit`.
  - Files: `crates/app/src/groww_bridge.rs`, `crates/app/src/main.rs`
  - Tests: `test_groww_connected_transition_builds_connected_row`,
    `test_groww_disable_builds_disconnected_offhours_or_disconnected_row`,
    `test_groww_reconnect_builds_reconnected_row`,
    `test_groww_audit_row_stamps_feed_groww_and_growwbridge_wstype`,
    `test_groww_bridge_emits_ws_event_audit_on_connect` (source-scan ratchet)
- [x] Item 3 — Confirm AUDIT-WS-01 already covers the new ILP failure path; no new
  ErrorCode. Confirm rollback is additive / self-healing.
  - Files: `.claude/rules/project/ws-event-audit-error-codes.md` (append the Groww
    bridge append-site to the "Source" list — doc only)
  - Tests: `error_code_rule_file_crossref.rs` stays green (no new variant)

## WsType DECISION (load-bearing)

**Add a new `WsType::GrowwBridge` variant (wire label `"groww_bridge"`).** Rationale:

- The 4 existing variants are all **Dhan** endpoints (`main_feed`, `depth_20`,
  `depth_200`, `order_update`) — their doc comments name `*.dhan.co` URLs
  (`ws_event_types.rs:29-36`). Re-using `MainFeed` for Groww would make a query
  like "every main_feed connect/disconnect this month" silently mix two brokers,
  and the `ws_type` SYMBOL would no longer mean "which Dhan WS". The `feed` column
  disambiguates rows in the DEDUP key, but `ws_type` is ALSO an operator filter —
  it must stay broker-true.
- The Groww transport is genuinely different (Python-sidecar NDJSON tail, not a
  binary WS), so a distinct `ws_type` is the honest label and reads cleanly in
  `questdb_sql` (`select … where feed='groww' and ws_type='groww_bridge'`).
- `connection_index = 0` (single Groww bridge, pool_size 1) — matches the existing
  single-conn convention and the DEDUP composite `(ws_type, connection_index)`.
- Cost is one enum arm + the `all()` array + its two pinned tests (length 4→5,
  label uniqueness). `WsType` is `Copy`/`Hash` and lives in `common`, so adding
  the arm is zero-risk to the hot path. Alternative considered (reuse
  `OrderUpdate`/`MainFeed`) — REJECTED: corrupts the broker-true meaning of
  `ws_type`.

## Design

The Groww bridge already computes the exact transition signals; we only need to
emit one `WsEventAuditRow` per transition into the channel that already exists.

1. **`WsType::GrowwBridge`** (`ws_event_types.rs`): add the variant, its
   `as_str() => "groww_bridge"` arm, append it to `all()` (now length 5), and
   update the two label tests. This is the ONLY `common` change.
2. **Pass the existing tx into the bridge.** `run_groww_bridge` gains an
   `Option<tokio::sync::mpsc::Sender<WsEventAuditRow>>` parameter (Option mirrors
   the Dhan `ws_audit_tx: Option<…>` convention so a None build still works). In
   `main.rs:347`, pass `Some(spawn_ws_event_audit_consumer(config.questdb.clone()))`
   — the SAME helper the Dhan/order-update paths use. The consumer drains ALL
   producers into the one `ws_event_audit` table.
3. **Emit at each transition** via a small bridge-local `emit_groww_ws_audit(kind,
   source, reason)` helper that mirrors `connection.rs:672-720`:
   - row fields: `event_ts_ist_nanos`/`trading_date_ist_nanos` from
     `Utc::now()+IST_UTC_OFFSET_NANOS` (same IST-nanos math as the Dhan helper,
     `connection.rs:688-692`); `feed: Feed::Groww`; `ws_type: WsType::GrowwBridge`;
     `connection_index: 0`; `pool_size: 1`; `event_kind`; `source`
     (`"groww_sidecar"` / `"feed_disabled"` / `"watcher_reconnect"`); `reason`
     (short, no token); `dhan_code: WS_EVENT_NO_DHAN_CODE` (Groww has no Dhan
     codes); `down_secs: 0`; `attempts: 0`; `market_hours:
     is_within_market_hours_ist()`.
   - **Connected** — fired ONCE on the rising edge into `streaming_observed`
     (gate on a `connected_audited` latch so we don't spam one row per loop turn,
     matching the edge-trigger rule, audit-findings Rule 4).
   - **Disconnected / DisconnectedOffHours** — fired ONCE on the falling edge in
     the `!feed_runtime.is_enabled(Feed::Groww)` block (`:750`), choosing
     `DisconnectedOffHours` when `!is_within_market_hours_ist()` (Low-severity,
     mirrors the Dhan off-hours convention) else `Disconnected`. Reset the
     `connected_audited` latch here so a later re-enable re-emits Connected.
   - **Reconnected** — fired ONCE when `streaming_observed` rises AGAIN after a
     prior Disconnected (i.e. the bridge resumed) — reuse the same edge latch
     distinguishing first-connect vs re-connect.
   - `try_send` is non-blocking; drop on full/closed exactly like the Dhan helper
     (`connection.rs:714-715`) so the bridge loop is never stalled by the forensic
     side-record. This is COLD PATH (once per transition, never per tick) — the two
     owned `String`s are off the tick path (`O(1) EXEMPT`, same as Dhan).
4. **Row identity / dedup.** `(ts, trading_date_ist, feed='groww',
   ws_type='groww_bridge', connection_index=0, event_kind)` — a Groww row and a
   Dhan row never collide (per-feed identity, `data-integrity.md` feed-in-key).
   `ts` (designated, nanos) lets two same-kind Groww events in the same second both
   survive.

## Edge Cases

- **Feed disabled → re-enabled.** Disable falling-edge emits Disconnected/
  DisconnectedOffHours ONCE and resets the `connected_audited` latch; the next
  streaming rising edge emits Reconnected (not a second Connected). No duplicate
  rows because the latch gates each direction.
- **Sidecar crash / respawn (watcher channel closed + re-arm, `:731-735`).** The
  bridge keeps running on the fallback poll; when streaming resumes,
  `streaming_observed` rises and we emit Reconnected. If streaming never resumes,
  no Connected row is emitted (honest — `set_connected(false)` already reflects
  this; no false-OK).
- **Market-closed connect.** `market_hours` is stamped from
  `is_within_market_hours_ist()`; an off-hours disable routes to
  `DisconnectedOffHours` (Low) so the audit mirrors the Dhan off-hours class and
  doesn't imply an in-market incident.
- **Duplicate connect events (one row per loop turn).** Prevented by the
  `connected_audited` edge latch — Connected/Reconnected fire only on the rising
  edge, never every poll. (Even if a duplicate slipped through, the DEDUP key
  collapses same-`(ts,kind)` rows.)
- **QuestDB down at emit.** Best-effort: `try_send` drops on a full/closed channel
  and `WsEventAuditWriter::flush` returns `Err` while QuestDB is unreachable
  (rows buffer, AUDIT-WS-01 Medium). This NEVER gates the Groww feed, tick capture,
  or `feed_health` — the audit is a forensic side-record only.
- **None tx (defensive / test builds).** `Option<Sender>` → emit is a no-op when
  `None`, exactly like the Dhan `emit_ws_audit` early-return (`connection.rs:681`).

## Failure Modes

- **ILP append/flush fails (QuestDB unreachable, disk full, ILP TCP down).**
  Already covered by **AUDIT-WS-01** (`ws-event-audit-error-codes.md` §1, Medium,
  auto-triage-safe) and the existing `tv_ws_event_audit_write_errors_total{stage}`
  counter in `run_ws_event_audit_consumer` (`main.rs:2992,3005`). The Groww rows
  flow through the SAME consumer, so **no new ErrorCode is required** — confirmed
  against `error_code_rule_file_crossref.rs` (adding the Groww append-site to the
  AUDIT-WS-01 "Source" list is a doc-only mention; no enum variant added, so the
  cross-ref test stays green).
- **Channel full (bridge faster than consumer).** `try_send` drops the row + the
  Groww transition is still in CloudWatch logs + `feed_health`; only the queryable
  forensic row for that one event is missing until pressure clears. Identical
  bounded behaviour to the Dhan helper.
- **Wrong feed/ws_type stamped.** Guarded by
  `test_groww_audit_row_stamps_feed_groww_and_growwbridge_wstype` (would fail the
  build if a refactor regresses `feed`/`ws_type`).

## Test Plan

Unit (in `crates/app/src/groww_bridge.rs::tests`, using `WsEventAuditWriter::for_test()`
+ a row-builder, no live QuestDB):
- `test_groww_connected_transition_builds_connected_row` — first
  streaming/parsed-tick edge → `WsEventKind::Connected`, `feed=Groww`.
- `test_groww_disable_builds_disconnected_offhours_or_disconnected_row` — disable
  falling edge → `DisconnectedOffHours` off-hours / `Disconnected` in-market.
- `test_groww_reconnect_builds_reconnected_row` — second streaming edge after a
  disable → `WsEventKind::Reconnected`.
- `test_groww_audit_row_stamps_feed_groww_and_growwbridge_wstype` — row builder
  round-trips `feed=Feed::Groww`, `ws_type=WsType::GrowwBridge`,
  `connection_index=0`, `pool_size=1`, `dhan_code=WS_EVENT_NO_DHAN_CODE`; appends
  cleanly to `WsEventAuditWriter::for_test()` (pending == 1).
- `test_groww_bridge_emits_ws_event_audit_on_connect` — **source-scan ratchet**:
  asserts `run_groww_bridge` source contains the `emit_groww_ws_audit(` call on the
  connect path (mirrors the existing bridge source-scan guards at
  `groww_bridge.rs:1113,1285,1308`), so a future refactor cannot silently remove
  the audit emit.

Common (`crates/common/src/ws_event_types.rs::tests`):
- `test_ws_type_as_str_labels_are_stable_and_unique` — extend the expected vec with
  `"groww_bridge"`, assert uniqueness.
- `test_all_arrays_match_variant_counts` — update `WsType::all().len()` 4 → 5.

Storage (already green, re-run to confirm the multi-feed append path is unchanged):
- `ws_event_audit_persistence::tests::test_append_row_each_event_kind_and_ws_type`
  (iterates `WsType::all()` × `WsEventKind::all()` — now also covers GrowwBridge).
- `dedup_segment_meta_guard::per_feed_market_data_dedup_keys_must_include_feed`
  (unchanged — `ws_event_audit` already feed-keyed).

Scope (per `testing-scope.md`): edit touches `crates/common/` (new WsType variant)
→ **escalate to `cargo test --workspace`** for this change.

## Rollback

- **Additive, no schema migration.** The `feed` column + feed-keyed DEDUP already
  exist on `ws_event_audit` (`ws_event_audit_persistence.rs:71,130-172`,
  self-healing `ADD COLUMN IF NOT EXISTS`), so this plan adds NO column and NO
  DDL — Groww simply starts writing rows the table already accepts.
- **To revert:** remove the `emit_groww_ws_audit` call(s) from `run_groww_bridge`
  and drop the `ws_audit_tx` parameter (revert `main.rs:347` to not pass the tx).
  The `WsType::GrowwBridge` variant can stay harmlessly (inert label) OR be
  reverted with its two test updates — either way no data is lost and no other
  feed is affected.
- Existing `feed='groww'` rows remain valid forever (SEBI retention; never DROP).

## Observability

- **The audit row IS the observability** — that is the whole point of this plan.
  It makes "show me every Groww connect/disconnect this month, with source +
  market-hours" a single `questdb_sql` over `ws_event_audit where feed='groww'`,
  durably + SEBI-retentioned, instead of roll-off CloudWatch logs only.
- It **complements** (does not replace) the live `feed_health` signal: `feed_health`
  answers "is Groww green RIGHT NOW?" (the `/feeds` page), while `ws_event_audit`
  answers "what was its lifecycle history?". The two are independent — the audit
  emit is best-effort and never gates `feed_health`.
- Reuses the existing counters `tv_ws_event_audit_rows_total` (rows persisted) and
  `tv_ws_event_audit_write_errors_total{stage}` (AUDIT-WS-01) — no new metric, so
  the existing dashboards/alerts cover the Groww rows automatically.
- 7-layer telemetry check: counter ✅ (rows_total/write_errors_total), tracing/log
  ✅ (`error!` in the consumer with AUDIT-WS-01 semantics), Telegram ✅ (AUDIT-WS-01
  Medium routes via the error pipeline), audit table ✅ (the row itself). Layers
  N/A for a cold control-plane event: per-tick gauge (this is once-per-transition,
  not per-tick).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Groww enabled, first tick streams | ONE `connected` row, `feed='groww'`, `ws_type='groww_bridge'`, `market_hours` true if in-session |
| 2 | Operator disables Groww in-market | ONE `disconnected` row (Low/High per off-hours), latch reset |
| 3 | Operator disables Groww off-hours | ONE `disconnected_off_hours` row (Low) |
| 4 | Re-enable Groww, streaming resumes | ONE `reconnected` row (NOT a 2nd `connected`) |
| 5 | Sidecar crash → watcher re-arm → resume | `reconnected` on the new streaming edge |
| 6 | QuestDB down at emit | row dropped/buffered, AUDIT-WS-01 Medium, feed unaffected |
| 7 | Same-second Groww + Dhan connect | TWO rows (distinct `feed`), neither collapsed |

## Per-Item Guarantee Matrix

This plan carries the 15-row + 7-row guarantee matrix by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical matrices).
Each item above maps to that matrix; the load-bearing rows for THIS
cold/control-plane + observability change are: 100% audit coverage (the new
`feed='groww'` rows in the SAME feed-keyed `ws_event_audit` table close the one
genuine gap), 100% testing coverage (Rust unit + the `WsType::all()` append-path
coverage + the existing live-QuestDB e2e), 100% logging (no new secret-bearing
log; `reason` is short + token-free and is additionally redacted at the ILP write
boundary by `WsEventAuditWriter::append_row` via `redact_url_params`), 100%
monitoring (reuses `tv_ws_event_audit_rows_total` + `tv_ws_event_audit_write_errors_total`),
100% alerting (AUDIT-WS-01 already wired — no new failure mode), 100%
functionalities (every new pub surface — the `WsType::GrowwBridge` variant + the
bridge emit helper — has a test AND a real call site, Rule 14), 100% extreme check
(round-trip row builder + a source-scan ratchet that the bridge calls the audit
append on connect). From the 7-row resilience matrix: Uniqueness+dedup (DEDUP key
already includes `feed`; a Groww row never collides with a Dhan row — I-P1-11
extended to WS streams + feed-in-key), Zero ticks lost (NO new tick-drop path; the
audit emit is `try_send` best-effort, COLD PATH, off the tick path, and never
gates the feed), O(1) latency (the emit fires once per transition, never per tick;
`O(1) EXEMPT` like the Dhan helper). The changed crates are **app**
(`groww_bridge.rs`, `main.rs` wiring), **common** (`ws_event_types.rs` — the new
`WsType::GrowwBridge` variant). **storage** is UNCHANGED (the append helper +
table already accept any `feed`/`ws_type`; no new append helper needed).

## Honest 100% claim (per operator-charter §F)

100% inside the tested envelope, with ratcheted regression coverage: the Groww
bridge now writes one `feed='groww'` row per lifecycle transition into the SAME
feed-keyed `ws_event_audit` table Dhan uses (DEDUP `ts, trading_date_ist, feed,
ws_type, connection_index, event_kind` — ratcheted by
`dedup_segment_meta_guard::per_feed_market_data_dedup_keys_must_include_feed`);
the emit is best-effort `try_send` (AUDIT-WS-01 Medium absorbs an ILP outage —
rows buffer, never gate the feed); edge-triggered so duplicate connects don't spam
(audit-findings Rule 4); no new tick-drop path and no schema migration (the table
already self-heals `feed` via `ADD COLUMN IF NOT EXISTS`). Beyond the envelope (a
sustained QuestDB outage), the forensic row for that one transition is missing
until QuestDB recovers — the live `feed_health` + CloudWatch log for the same
event still fired, so no operator-visible signal is lost.
