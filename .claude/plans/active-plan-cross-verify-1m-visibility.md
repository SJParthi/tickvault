# Implementation Plan: Make the 15:31 post-market cross-verify VISIBLE

**Status:** APPROVED
**Date:** 2026-06-10
**Approved by:** Parthiban (operator directive 2026-06-10 — "Write the 6-section APPROVED plan first", task brief authorizes implementation; confirmed live 2026-06-10: CSV written 15:33, zero mismatches, zero Telegram, nothing on portal)

> **Guarantee matrices:** this plan carries the 15-row + 7-row guarantee
> matrices by cross-reference to `per-wave-guarantee-matrix.md` (canonical).
> Per-item specifics are in the Observability + Test Plan sections below.
> Honest 100% claim wording per `operator-charter-forever.md` §F applies:
> 100% inside the tested envelope, with ratcheted regression coverage.

## Design

The 15:31:00 IST post-market 1-minute cross-verify (`run_cross_verify_1m`,
`crates/app/src/cross_verify_1m_boot.rs`) already computes a
`CrossVerify1mSummary` "for the caller to emit Telegram", but the caller
(`crates/app/src/main.rs`, the spawned task ending at the
`"PROOF: cross_verify_1m fired"` log line) drops it. Three additions:

1. **Telegram (crates/core + crates/app):**
   - New typed `NotificationEvent::CrossVerify1mSummary { trading_date_ist:
     String, instruments: usize, compared: usize, mismatches: usize,
     missing: usize, degraded: bool }` in
     `crates/core/src/notification/events.rs`. Severity is data-dependent:
     `Info` ONLY when `mismatches == 0 && missing == 0 && !degraded &&
     compared > 0`; `High` otherwise. (Hardened per pre-impl hostile review:
     C1 — `compared == 0` must NOT read as PASS [false-OK / audit Rule 11:
     Dhan serving empty 200s yields compared=0, fetch_failures=0]; H1 —
     `missing > 0` is missed live candles, the very signal this audit
     exists for, so it forces High.) `dispatch_policy()` returns
     `Immediate` for the summary (pre-impl finding M1: without it the Info
     variant coalesces for 60s and re-renders as a bucket summary).
     Message per the 10 Telegram commandments: severity
     emoji first (✅/⚠️), plain English, IST 12-hour time ("3:31 PM IST"),
     real numbers, no library names, no file paths (points at the portal
     Cross-verify card).
   - New typed `NotificationEvent::CrossVerify1mAborted { detail: String }`,
     Severity::High — fired when the spawned task dies before producing a
     summary, so absence of the daily summary is impossible to miss.
     (Pre-impl finding H2: emit ONLY on `JoinError::is_panic()` —
     cancellation during the 16:30 IST auto-stop / graceful shutdown is a
     normal teardown, not an abort, and must not page.)
   - `crates/app/src/main.rs`: restructure the cross-verify spawn into an
     INNER `tokio::spawn` (existing decision + sleep + run logic, returning
     `Option<CrossVerify1mSummary>`: `None` = legitimately skipped
     [non-trading day / past-trigger / no targets], `Some` = ran) and an
     OUTER supervisor task that awaits the inner `JoinHandle`:
     `Ok(Some(summary))` → `notifier.notify(CrossVerify1mSummary {...})`;
     `Ok(None)` → nothing (legitimate skip); `Err(join_err)` (panic/abort)
     → `notifier.notify(CrossVerify1mAborted {...})`. The emit call sites
     live in main.rs and are pinned by a source-scan ratchet.

2. **Summary artefact + API endpoint (crates/app + crates/api):**
   - `run_cross_verify_1m` additionally writes
     `data/cross-verify/cross-verify-1m-YYYY-MM-DD.summary.json` next to the
     CSV (one JSON line: trading_date, instruments_checked, compared,
     mismatches, missing_ours, fetch_failures, degraded) via a pure
     `summary_json_contents()` builder (unit-tested). Fail-soft like the CSV.
   - New `GET /api/debug/cross-verify/latest` in
     `crates/api/src/handlers/debug.rs` (same read-only pattern as
     logs/spill endpoints; dir overridable via `TV_CROSS_VERIFY_DIR`,
     default `data/cross-verify`): finds the lexically-newest file that
     BOTH `starts_with("cross-verify-1m-")` AND `ends_with(".csv")`
     (pre-impl findings H3 + S-H2: the strict suffix predicate keeps the
     sibling `.summary.json` and any foreign file out of the selection),
     returns JSON `{date, csv_file, mismatch_rows, summary: <parsed
     summary.json or null>, csv: "<content>"}` built via `serde_json`
     (pre-impl finding S-M2: never hand-escape the CSV payload); 404 with
     a GENERIC `{"error":"no cross-verify run yet"}` body — no resolved
     filesystem path disclosure (pre-impl finding S-M1).
     Registered in `crates/api/src/lib.rs` next to the other debug routes.

3. **Portal card (deploy/aws/lambda/operator-control/handler.py):**
   - New read-only POST action `cross_verify` (NOT in `_DESTRUCTIVE`): runs
     an SSM shell `curl -fsS http://127.0.0.1:3001/api/debug/cross-verify/latest`
     on the box, parsed by a pure `_parse_cross_verify()` (unit-tested).
   - New "Cross-verify — daily candle check vs exchange record" card on the
     Data tab: date + instruments + compared + mismatches + missing +
     PASS/FAIL badge (PASS = mismatches==0 && missing==0 && !degraded &&
     compared>0 — same hardened condition as the event severity), loaded by
     a `loadCrossVerify()` JS fn wired into the Data tab activation.
     ALL string fields rendered through the existing `esc()` helper
     (pre-impl finding S-H1), with a test asserting the card template
     escapes. The `cross_verify` action is read-only (NOT in
     `_DESTRUCTIVE`); bearer auth applies via the existing top-level
     `_authorized()` gate (pre-impl finding S-L1 confirmed).

## Edge Cases

- **Task panics mid-sleep or mid-run** → `JoinError` → `CrossVerify1mAborted`
  (High). tokio catches panics into the JoinHandle, so no panic escapes.
- **Legitimate skips** (non-trading day, mid-evening boot past 15:31, empty
  target list) → inner returns `None` → no event, no false alarm.
- **Degraded run** (no JWT / >10% fetch failures) → summary has
  `degraded=true` → event severity High even with 0 mismatches (false-OK
  guard, audit Rule 11).
- **No CSV yet** (fresh box, before first 15:31) → endpoint 404 with JSON
  error body; portal card shows "no run yet".
- **CSV without summary.json** (days before this change) → endpoint returns
  `summary: null`, still serves the CSV; `mismatch_rows` derived by counting
  CSV lines minus header.
- **Large mismatch CSV** → endpoint serves whole file (cold-path, operator
  on-demand; CSV is mismatch-rows-only so normally tiny). Acceptable.
- **Box stopped (portal)** → `_ssm_shell_sync` returns "" → parse yields
  empty fields → card shows "—" (same degrade pattern as storage card).
- **Boundary severity (hardened per pre-impl review):** clean run with
  compared>0 → Info; exactly one mismatch → High; degraded alone → High;
  missing>0 alone → High (H1); compared==0 (Dhan empty-200s, quiet-day
  forced run, no-JWT degraded path) → High with honest "nothing could be
  compared" wording (C1/L3). Tests pin all five.
- **Graceful shutdown mid-sleep** (16:30 IST auto-stop, `make stop`) →
  `JoinError` with `is_panic()==false` → log only, NO Aborted page (H2).
- **`FixedOffset::east_opt` failure** (compile-time-constant, effectively
  unreachable) → inner returns a Failed marker → Aborted event (L2), not a
  silent skip.
- **Forced run (`TICKVAULT_CROSS_VERIFY_NOW`)** → event fires too —
  intentional: the operator triggered it and the event is the
  proof-of-pipeline; a quiet-day forced run reads High/"nothing compared",
  never a fabricated ✅ (M3 acknowledged).
- **Double channel on failure days** — the existing CROSS-VERIFY-1M-01/02
  `error!` routes stay unchanged alongside the new High summary event;
  intentional redundancy, acknowledged (M2).
- **usize counts** never negative; merge already saturating.

## Failure Modes

- **Notifier unavailable / Telegram down** → `NotificationService::notify`
  is fail-soft (existing dispatch/coalescer handles drops with
  `tv_telegram_dropped_total`); the structured `info!`/`error!` logs and the
  CSV+summary files remain as record. No new tick-drop path; hot path
  untouched (everything here is post-market cold path).
- **summary.json write fails** (disk full/permission) → fail-soft `warn!`,
  CSV + audit table remain durable record; Telegram still fires (the event
  is built from the in-memory summary, not the file).
- **Endpoint reads while file mid-write** → tokio::fs read returns partial
  CSV at worst; next refresh corrects. Read-only, never blocks the app.
- **SSM curl fails on the box (app down at query time)** → empty output →
  portal card degrades to "—", no 500 (matches existing `_ssm_shell_sync`
  fail-soft contract).
- **JoinError on outer supervisor itself** → outer task is the last line of
  defense; its own panic would be a tokio-runtime-level failure already
  covered by process-level monitoring. Documented honest envelope.

## Test Plan

- `crates/core` (events): `cargo test -p tickvault-core --lib notification`
  — new tests: topic+severity pinned for both variants (Info-clean,
  High-on-mismatch, High-on-degraded, High-for-aborted), message contains
  ✅/⚠️ emoji-first, "3:31 PM IST", real numbers, action line on failure;
  no file paths/library names in body.
- `crates/api` (endpoint): `cargo test -p tickvault-api` — new tests in
  `handlers/debug.rs`: 404-when-missing (dir absent + dir empty),
  200-with-content (CSV present → body carries csv + date +
  mismatch_rows), summary.json merged when present, newest-file selection,
  env-override respected (under the existing ENV_LOCK).
- `crates/app` (boot helpers + ratchet): `cargo test -p tickvault-app` —
  unit tests for `summary_json_contents()` (field fidelity, degraded flag)
  + new source-scan ratchet `crates/app/tests/cross_verify_1m_visibility_guard.rs`
  pinning: (a) main.rs emits `NotificationEvent::CrossVerify1mSummary`,
  (b) main.rs emits `NotificationEvent::CrossVerify1mAborted` on JoinError,
  (c) cross_verify_1m_boot.rs writes the summary JSON.
- Lambda: `python3 -m unittest deploy/aws/lambda/operator-control/test_handler.py`
  — new tests: `_parse_cross_verify` happy path + empty; HTML contains the
  Cross-verify card; `cross_verify` action not in `_DESTRUCTIVE`.
- Block-scoped per testing-scope.md (core+app+api touched; common untouched
  → no workspace escalation). Gates: fmt, clippy -D warnings, banned-pattern,
  pub-fn-test-guard, plan-verify. All outputs pasted in the PR.

## Rollback

Single revert of the one squash-merged PR restores prior behavior exactly:
the cross-verify run itself is untouched (still writes audit table + CSV);
all additions are additive (new event variants, new endpoint, new portal
card, new summary file). No schema changes, no config changes, no
migration. The summary.json files left on disk are inert. Portal Lambda
redeploy via the existing deploy workflow picks up the reverted handler.

## Observability

- 7-layer coverage for the new path (per wave-4 preamble §4):
  structured `info!` proof line (exists today) + typed Telegram event
  (NEW — the whole point) + existing `CROSS-VERIFY-1M-01/02` error codes
  unchanged + `cross_verify_1m_audit` QuestDB table unchanged + CSV +
  summary.json artefacts + portal card (replaces the retired Grafana layer)
  + source-scan ratchets failing the build on regression.
- The Aborted event makes silence impossible: every trading-day 15:31 run
  ends in exactly one of {summary Telegram, legitimate-skip log, Aborted
  Telegram}.
- No new ErrorCode variants needed (CrossVerify1m01/02 already exist and
  keep their rule-file cross-refs in
  `.claude/rules/project/cross-verify-1m-error-codes.md`).

## Plan Items

- [ ] Item 1 — Typed events + emit wiring
  - Files: crates/core/src/notification/events.rs, crates/app/src/main.rs
  - Tests: test_cross_verify_1m_summary_clean_is_info, test_cross_verify_1m_summary_mismatch_is_high, test_cross_verify_1m_summary_degraded_is_high, test_cross_verify_1m_aborted_is_high, test_cross_verify_1m_summary_message_format
- [ ] Item 2 — summary.json artefact
  - Files: crates/app/src/cross_verify_1m_boot.rs
  - Tests: summary_json_contents_round_trips_fields, summary_json_marks_degraded
- [ ] Item 3 — GET /api/debug/cross-verify/latest
  - Files: crates/api/src/handlers/debug.rs, crates/api/src/lib.rs
  - Tests: cross_verify_latest_returns_404_when_missing, cross_verify_latest_returns_200_with_content, cross_verify_latest_merges_summary_json
- [ ] Item 4 — Portal Cross-verify card
  - Files: deploy/aws/lambda/operator-control/handler.py, deploy/aws/lambda/operator-control/test_handler.py
  - Tests: test_parse_cross_verify, test_html_has_cross_verify_card, test_cross_verify_action_is_read_only
- [ ] Item 5 — Source-scan ratchet
  - Files: crates/app/tests/cross_verify_1m_visibility_guard.rs
  - Tests: emit_call_site_for_summary_is_pinned, emit_call_site_for_aborted_is_pinned, summary_json_write_is_pinned

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Clean run, 0 mismatches | ✅ Info Telegram + portal PASS badge |
| 2 | Mismatches > 0 | ⚠️ High Telegram + portal FAIL badge |
| 3 | Degraded fetch (0 mismatches) | ⚠️ High Telegram (false-OK guard) |
| 4 | Task panics | High Aborted Telegram |
| 5 | Non-trading day / past 15:31 boot | No event (legitimate skip) |
| 6 | No CSV on box | Endpoint 404; card "no run yet" |
| 7 | Pre-change CSV without summary.json | Endpoint 200, summary null |
