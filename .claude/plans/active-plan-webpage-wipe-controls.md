# Implementation Plan: webpage data-wipe controls (per-scope, off-market, type-WIPE confirm)

**Status:** APPROVED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — this session, verbatim demand: "especially from the webpage whenever I click all kinds of removal wipe removal means it should work"; scope answered via AskUserQuestion 2026-07-02: ALL FOUR scopes (market data ticks+candles, Groww sidecar files, spill/DLQ/WAL buffers, EVERYTHING incl. audit tables) + "Off-market hours only + confirm". **The EVERYTHING selection is the operator's dated override of the SEBI-retention charter rows for manually-invoked wipes** (`instrument_lifecycle` NEVER-DELETE + 5y audit retention still bind all AUTOMATED paths; only this explicit operator-confirmed button may cross them, and every use writes a forensic row first).

## Design

Add a **Wipe Controls** section to the existing `/feeds` control page (same
bearer-token auth, same page) with four buttons, each backed by a new POST
endpoint under `/api/wipe/*` in `crates/api`:

| Button | Endpoint | What it deletes |
|---|---|---|
| Wipe market data | `POST /api/wipe/market-data` `{feed: "dhan"\|"groww"\|"all", day: "YYYY-MM-DD"\|"all"}` | `ticks` + every `candles_*` table, filtered by feed (+ optional single IST day) via QuestDB HTTP `/exec` DELETE-equivalent (QuestDB: `ALTER TABLE … DROP PARTITION` for day-scoped, `TRUNCATE TABLE`-equivalent recreate for all). **RESURRECT-PROOF (operator incident 2026-07-02: a DB wipe removed Dhan rows but Groww rows CAME BACK — the bridge re-tails `data/groww/live-ticks.ndjson` from byte 0 and re-persists everything):** when `feed` includes groww this endpoint ALSO truncates the NDJSON capture file + status file and resets the bridge read-offset (feed already disabled per gate 5, so no writer races); when `feed` includes dhan it ALSO clears residual WAL segments so boot replay cannot re-inject. Wipe means GONE for BOTH feeds, identically. |
| Wipe Groww sidecar files | `POST /api/wipe/groww-files` | `data/groww/live-ticks.ndjson`, `groww-status.json`, bridge offset state (in-process reset signal) — sidecar/bridge resume cleanly on next wake |
| Wipe buffers | `POST /api/wipe/buffers` | `data/spill/*`, `data/dlq/*`, WAL segment files (`ws_frame_spill` dir) |
| WIPE EVERYTHING | `POST /api/wipe/everything` | All of the above + audit tables + `instrument_lifecycle` (+ its audit) — the factory reset. Requires the extra charter-override confirm string |

**Safety chain (every endpoint, server-side enforced — not just UI):**
1. Bearer auth (existing middleware).
2. **Market-hours gate:** refuse with 409 during 09:00–15:30 IST on trading
   days (`is_within_market_hours_ist`).
3. **Type-to-confirm:** request body must carry `confirm: "WIPE"` (and for
   everything: `confirm: "WIPE-EVERYTHING-I-OVERRIDE-RETENTION"`). Wrong/missing
   → 400 with the exact expected string named.
4. **Forensic audit row FIRST** (`wipe_audit` table: ts, scope, feed, day,
   requester note, rows/files affected, outcome) — written BEFORE the deletion
   executes (§24 audit-first ordering); the everything-wipe writes its audit
   row to a fresh post-wipe table write if the audit table itself was wiped
   (write-after semantics documented honestly).
5. **Feed pause requirement:** market-data + everything wipes require the
   affected feed(s) runtime-DISABLED first (409 otherwise) — never wipe tables
   a live pipeline is writing.
6. One wipe at a time (process-global `AtomicBool` in-flight lock; 409 on
   concurrent).

**Page UX:** four buttons with per-scope description, a text input for the
confirm string, disabled during market hours (server still re-checks), result
banner showing rows/files removed + the audit-row id.

## Edge Cases
- Wipe during market hours via curl (bypassing UI) → 409 from the server gate.
- Wipe while feed enabled → 409 naming the feed to disable first.
- Concurrent double-click → in-flight lock 409.
- Day-scoped wipe of a day with no partition → 200 with `rows: 0` (idempotent).
- QuestDB down mid-wipe → 502 with the partial-progress list in the response +
  audit row outcome=`partial`; re-run is idempotent.
- Everything-wipe then boot → tables recreated by the boot DDL self-heal;
  lifecycle rebuilt from the next CSV fetch; day-1 bootstrap path (§10) covers.
- Sidecar files wiped while sidecar running → gated on Groww disabled (rule 5).

## Failure Modes
- Deletion partially applied → audit row `partial` + response lists per-table
  outcomes; idempotent re-run completes.
- Audit write fails (QuestDB down) → wipe REFUSES to proceed (audit-first,
  fail-closed) except for the buffers scope (disk-only, logged to errors.jsonl).
- A future PR weakening the gates → ratchet test pins market-hours check,
  confirm strings, feed-disabled requirement, and the audit-first ordering.

## Test Plan
- Pure fns unit-tested: confirm-string validation, scope parsing, market-hours
  gate decision, wipe-plan builder (tables/paths per scope+feed+day).
- Handler tests (axum test harness): 400/409/200 matrix per gate.
- Source-scan ratchet: gates present in every handler; audit-first ordering.
- e2e (live QuestDB): day-scoped market-data wipe removes exactly that day's
  feed-tagged rows and leaves the other feed intact.

## Rollback
`git revert` removes the endpoints + page section; no schema migration beyond
the additive `wipe_audit` table (retained — audit history is never deleted by
rollback).

## Observability
- `wipe_audit` QuestDB table (DEDUP `(ts, scope)`), Severity::Critical
  Telegram on every executed wipe (operator sees every destructive act),
  `tv_wipe_operations_total{scope, outcome}` counter, `error!` with code on
  failures.

## Per-Item Guarantee Matrix

Every item in this plan carries the 15-row 100% Guarantee Matrix and the 7-row
Resilience Demand Matrix by cross-reference to the canonical rule —
`.claude/rules/project/per-wave-guarantee-matrix.md` (Item 22 / Item 24 style):
coverage via the unit + handler-gate + ratchet tests listed per item; audit via
the new `wipe_audit` table (DEDUP keys, audit-first ordering); monitoring via
`tv_wipe_operations_total{scope, outcome}`; logging via `error!` on failures;
alerting via the Severity::Critical Telegram on every executed wipe; security
via the 5-gate server-side chain (auth, market-hours, confirm, feed-disabled,
in-flight lock); extreme check via `wipe_gates_guard.rs` failing the build if
any gate or the audit-first ordering regresses. Honest envelope: wipes are
operator-invoked destructive actions — the guarantee is that they cannot fire
accidentally (5 gates), cannot run mid-market, cannot race a live writer, and
are always forensically recorded; the EVERYTHING scope crosses the SEBI
retention rows ONLY under the operator's recorded 2026-07-02 override.

## Plan Items
- [ ] `wipe_audit` persistence + wipe-plan pure builder
  - Files: crates/storage/src/wipe_audit_persistence.rs, crates/api/src/handlers/wipe.rs
  - Tests: test_wipe_plan_builder_scopes, test_wipe_audit_ddl
- [ ] Four POST endpoints + gates (auth, market-hours, confirm, feed-disabled, in-flight lock)
  - Files: crates/api/src/handlers/wipe.rs, crates/api/src/lib.rs (routes), crates/api/src/state.rs
  - Tests: test_wipe_rejects_during_market_hours, test_wipe_requires_confirm_string, test_wipe_requires_feed_disabled
- [ ] /feeds page Wipe Controls section
  - Files: crates/api/src/handlers/feeds_page.rs
  - Tests: test_feeds_page_renders_wipe_controls
- [ ] Telegram + counter + rule file (wipe-controls lock with the operator's override quote)
  - Files: crates/core/src/notification/events.rs, .claude/rules/project/webpage-wipe-controls-2026-07-02.md
  - Tests: test_wipe_event_severity_critical
- [ ] Ratchet: gates + audit-first pinned
  - Files: crates/api/tests/wipe_gates_guard.rs
  - Tests: wipe_handlers_carry_all_five_gates

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Click "Wipe market data (groww, all days)" off-market, feed disabled, confirm WIPE | 200, rows removed, audit row, Telegram Critical |
| 2 | Same during market hours | 409 market-hours gate |
| 3 | Same with feed still enabled | 409 naming the feed |
| 4 | Wrong confirm string | 400 naming the expected string |
| 5 | WIPE EVERYTHING with the override string | full reset + audit + Telegram; boot self-heals schemas |
