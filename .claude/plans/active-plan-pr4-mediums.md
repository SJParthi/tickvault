# Implementation Plan: PR-4 — sweep mediums (future-ts clamp, flush-ladder ratchet, holiday ratchet) + 2 doc nits

**Status:** VERIFIED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — "ok now work on prs plan and futures" (this session) + the standing fix-sequence directive; PR-4 of the adversarial-sweep queue.

> Guarantee matrices: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md`
> (15-row 100% matrix + 7-row resilience matrix apply to every item below; the
> honest-100% envelope wording per operator-charter §F is adopted verbatim).

## Design

**M-A — relative future-timestamp clamp (groww bridge).** `validate_groww_tick`
today bounds `ts_ist_nanos` only against a STATIC year-2100 max — a tick
stamped "tomorrow" (sidecar bug / SDK clock glitch) passes, poisoning a future
candle bucket and confusing the IST-midnight force-seal. Fix: pass the wake's
receipt clock into validation and REJECT any tick more than
`GROWW_FUTURE_TS_TOLERANCE_NANOS` (60s) ahead of it — generous for legitimate
exchange-vs-box skew, fatal for day-level poison. New
`GrowwTickReject::FutureTimestamp` variant flows through the existing
reject/log/skip path (error! + skip, no drop of the pipeline). The clock is
read ONCE per wake (not per line — O(1) preserved).

**M-B — ILP flush-ladder ratchet + honest envelope (storage groww writer).**
Investigation shows the flush path is ALREADY bounded: connection-class flush
errors run a 3-attempt reconnect+replay ladder with 50/100/200ms backoff
(≤350ms wall-clock), then return Err with the buffer RETAINED. The residual
unbounded piece is the OS-level TCP connect timeout inside
`Sender::from_conf` (kernel default, minutes, once per reconnect attempt) —
the questdb client does not expose a connect-timeout knob on the TCP
transport, and guessing conf-string params risks a silent `sender=None` (zero
Groww persistence). HONEST resolution per the charter: pin the bounded ladder
with a ratchet test (constants + retained-buffer contract + backoff sum
≤350ms) and document the connect-leg envelope in the writer docs; a transport
change (HTTP ILP with request timeouts) is flagged as the future lift, not
gambled into a medium fix.

**M-C — NSE holiday-calendar completeness ratchet.** A missing holiday in
`config/base.toml [[trading.nse_holidays]]` makes the system expect data on a
closed day (false Down alarms) or run boundary tasks wrongly. New build-failing
test: the config parses, holidays are valid unique dates, and the CURRENT year
has a plausible NSE count (≥10 entries) — so every January the build itself
demands the new year's calendar instead of a silent rot.

**Doc nits (from the operator's official-page verification):**
`live-order-update.md` note 15 wrongly says RefLtp/TickSize are camelCase (the
official page shows PascalCase; our parser ignores both fields so zero code
impact); `websocket-connection-scope-lock.md` cites the retired depth-200 URL
form (`/?token=`) where the official page shows `/twohundreddepth` (no code
exists — forbidden feed; citation corrected for accuracy).

## Edge Cases
- Tick exactly at now+59s → accepted (skew tolerance); now+61s → rejected.
- Clock-read failure (receipt=0) → the relative check would reject everything;
  guard: tolerance check only applies when receipt > 0 (fail-open to the
  static bound — never mass-reject on a broken clock).
- Holiday test on Jan 1 with last year's calendar → RED build = the feature.
- Weekend-only entries / duplicates in config → test rejects.

## Failure Modes
- Overly tight tolerance rejecting real ticks → 60s is ~15x the observed
  exchange-stamp skew; rejects are error!-logged with the delta so a
  misconfigured box is visible immediately.
- Future edit re-widening the flush ladder to unbounded → ratchet fails build.

## Test Plan
- validate: future-ts matrix (at/below/above tolerance, zero-clock fail-open,
  static bounds still enforced).
- storage ratchet: ladder constants pinned (3 attempts, backoff sum ≤350ms,
  buffer-retained doc contract present).
- holiday ratchet: parses base.toml, current-year count ≥10, unique + valid.
- Full `cargo test -p tickvault-app -p tickvault-common -p tickvault-storage` scoped green.

## Rollback
`git revert` — no schema/config change.

## Observability
- Rejected future ticks: existing per-reject error! path + reject reason
  (`FutureTimestamp`) named in the log; conservation ledger already counts
  junk.

## Plan Items
- [x] M-A future-ts clamp + reject variant + tests
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_validate_rejects_future_timestamp
- [x] M-B flush-ladder ratchet + envelope docs
  - Files: crates/storage/src/groww_persistence.rs
  - Tests: test_flush_reconnect_ladder_is_bounded
- [x] M-C holiday-calendar completeness ratchet
  - Files: crates/common/tests/nse_holiday_calendar_guard.rs
  - Tests: test_current_year_holidays_present_and_valid
- [x] Doc nits (RefLtp casing, depth-200 endpoint citation)
  - Files: .claude/rules/dhan/live-order-update.md, .claude/rules/project/websocket-connection-scope-lock.md
  - Tests: (doc)

### Adversarial 4-agent verification addendum (2026-07-02, post-implementation pass)

Hot-path: PASS on all 4 required checks (1 MEDIUM: bounded <=350ms thread::sleep
on the QuestDB ERROR path only — documented, default-OFF feed; 2 LOW pre-existing).
Security: token-minter path CLEAN, SQL gate no bypass, guards non-tautological
(1 MEDIUM queued to PR-5: server-side confirm tokens for the 3 legacy destructive
Lambda actions). Hostile: 3 HIGH + 1 LOW verified REAL and FIXED inline below;
2 MEDIUM (F4 rotation-spanning downtime tail, F5 wipe/app-start race) queued to PR-5.

- [x] F1 fix — cross-day offset-snapshot resume blocked (head 64→256 + ist_day field + day check)
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_resume_from_snapshot_decision_matrix
- [x] F2 fix — rotation/persist race guard (skip persist when file-by-path shorter than drained offset)
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_resume_from_snapshot_decision_matrix
- [x] F3 fix — ILP buffer bounded at 64 MiB; over cap pauses NDJSON consumption (capture file = durable spill; rising-edge error! + counter)
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_wake_read_budget_pauses_consumption_over_ilp_cap
- [x] F6 fix — future-ts clamp fails OPEN on a degenerate ~1970 clock (min-plausible receipt guard; doc corrected)
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_validate_rejects_future_timestamp

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Sidecar emits tick stamped tomorrow | rejected + error! with delta; candles unpoisoned |
| 2 | QuestDB wedged during flush | ≤350ms bounded ladder → Err with buffer retained → next wake retries |
| 3 | January arrives, calendar not updated | build goes RED demanding the new year's holidays |
