# Implementation Plan: Spot 1m live-minute window hotfix + backfill + chain enable

**Status:** APPROVED
**Date:** 2026-07-13
**Approved by:** Operator standing directive 2026-07-13 (relayed via the coordinator session) — live-failure hotfix: the per-minute spot 1m REST fetcher (PR #1490) failed its FIRST live session (SPOT1M-01 every minute from 09:16 IST, `ok=0/errors=0/empty=3`); fix before close today + flip the option chain on (entitlement probe PASSED 08:31:49 IST).

## Design

Root cause (evidence-first, CloudWatch `/tickvault/prod/app`): the fetcher's
same-date 60-second request window (`fromDate = minute open, toDate =
open+60s`, `crates/app/src/spot_1m_rest_boot.rs`) is answered `2xx` WITHOUT
the target candle for every session minute, while the timestamp MATCHER is
verified correct — it produces the identical IST-as-epoch key the SHARED
parser `parse_intraday_1m_candles` (`crates/app/src/cross_verify_1m_boot.rs`)
emits, and that parser matches candles live daily at 15:31 with the
day-granular window. Fix:

1. **Window** — each per-minute fire now sends the ONLY live-proven window
   shape: `fromDate = D 00:00:00, toDate = D+1 00:00:00` (delegating to the
   shared `intraday_request_body`), filtered client-side to the exact
   minute (`spot_1m_day_request_body`). Over-delivery ≈ 375 candles ≈
   20 KB per response — inside the 2 MiB cap; request cadence unchanged.
2. **Previous-minute backfill sweep** — the full-day response covers earlier
   minutes, so every fire also persists the previous minute when it was not
   successfully persisted: per-SID in-memory `PersistTracker` (watermark
   committed only after a confirmed ILP flush), `backfill_minute_nanos`
   pure decision (one-minute lookback, session-gated), backfill candle
   extracted from the SAME response in every ladder outcome arm
   (`SidFetchOutcome::{Found,Empty,Failed}` all carry it). DEDUP keys make
   re-appends idempotent. Ladder bounds unchanged (per-SID 20 s budget).
3. **Option-chain enable** (`config/base.toml [option_chain_1m].enabled =
   true`) — the first-live-boot entitlement probe PASSED 08:31:49 IST
   2026-07-13; the dated record required by §8.5 is appended as
   `no-rest-except-live-feed-2026-06-27.md` §8.7 FIRST in the same PR;
   runbook `rest-1m-pipeline-error-codes.md` wording synced. The chain's
   2.5 s fallback timer (already tested arms a–e of
   `test_wait_until_chain_fire_signal_wakes_early_and_fallback_bounds` +
   the wiring-guard pin on `tx.send_replace(Some(fire))`) guarantees a
   failing/hanging spot leg never silences the chain leg.

## Edge Cases

- First session fire (09:16 → 09:15 candle): previous minute is pre-open →
  `backfill_minute_nanos` returns None (session-first gate).
- Multi-minute gaps: lookback is exactly ONE minute (spec) — older gaps
  stay absent, loudly counted by the existing boundary/edge accounting.
- Flush failure: staged watermarks are NOT committed (discarded buffer must
  not advance the tracker); the next fire re-backfills.
- Double persist of the same minute: tracker max-merge + QuestDB DEDUP →
  idempotent.
- Month/day boundary in `toDate`: `succ_opt` (cross-verify semantics),
  tested at 2026-07-31 → 2026-08-01.
- Vendor lateness > ladder: the own-minute fire stays `empty` (honest edge
  failure) but the NEXT fire's backfill repairs the row with a real
  (> 60 s) `close_to_data_ms` stamp.

## Failure Modes

- Dhan still answers without candles under the day window → unchanged loud
  path: `outcome="empty"` counters + SPOT1M-01 minute_failed + 3-minute
  escalation page (edge semantics untouched).
- Backfill append/flush failure → SPOT1M-02 (append/flush stages), counted;
  tracker not advanced; DEDUP-idempotent retry next fire.
- Chain leg entitlement regressing server-side → CHAIN-01/02 paths
  (unchanged); rollback switch = `[option_chain_1m] enabled = false`.

## Test Plan

- `cargo test -p tickvault-app --lib spot_1m_rest_boot` — new/updated:
  `test_regression_spot_1m_day_request_body_uses_proven_day_window`,
  `test_spot_1m_day_request_body_month_boundary`,
  `test_parse_intraday_columnar_utc_epoch_fixture_matches_ist_minute`
  (REAL UTC-epoch fixture 1783914300 = 2026-07-13 09:15 IST),
  `test_parse_intraday_columnar_for_minutes_backfill_hit`,
  `test_parse_for_minutes_malformed_short_and_mismatched_bodies_are_none`,
  `test_backfill_minute_nanos_hit_and_not_needed`,
  `test_backfill_minute_nanos_first_session_minute_has_no_backfill`,
  `test_persist_tracker_commit_max_merge_and_double_persist_idempotent`,
  `test_backfill_never_flips_edge_accounting`.
- `cargo test -p tickvault-app --lib option_chain_1m_boot` (fallback arms).
- Wiring guards: `spot_1m_rest_wiring_guard`, `option_chain_1m_wiring_guard`.
- `cargo test -p tickvault-storage --lib` + `-p tickvault-common --lib`
  (config/constants untouched semantics).
- fmt + clippy `-D warnings -W clippy::perf` + plan-gate PASS.

## Rollback

- Window/backfill: `git revert` of the code commit restores the (broken)
  minute-window build — no schema change, no config change on the spot half
  (`[spot_1m_rest] enabled = false` remains the kill switch).
- Chain: flip `config/base.toml [option_chain_1m] enabled = false` (the
  probe-and-report path resumes automatically as the fallback canary).

## Observability

- Existing: `tv_spot1m_fetch_total{outcome}`, `tv_spot1m_close_to_data_ms`
  (own-fire retrievals only — documented), `tv_spot1m_persist_errors_total`,
  SPOT1M-01/02 coded logs + edge page/recovery Telegram.
- New: `tv_spot1m_backfilled_total` counter + one `info!` per backfilled
  minute; backfilled rows carry the honest (> 60 s) `close_to_data_ms`
  column value.
- Chain: existing CHAIN-01..04 coded logs + counters activate with the
  enable flip; the boot probe verdict line remains the entitlement canary
  on any rollback.

## Per-Item Guarantee Matrix

See `per-wave-guarantee-matrix.md` — the full 15-row 100% Guarantee Matrix
and the 7-row Resilience Demand Matrix apply to EVERY item in this plan
(cross-referenced per the canonical rule instead of inlined). Item-specific
notes: no hot-path involvement (cold-path REST, once per minute + one
post-session sweep), so the DHAT/Criterion rows are N/A — cold path,
flagged honestly; every other row is proven by the tests named per item
below, the wiring guards, and the honest-envelope wording in the PR body
(any "100%" claim is qualified inside the tested envelope, with ratcheted
regression coverage).

## Plan Items

- [x] Diagnose live failure — verdict written (window bug; matcher verified
  correct against the shared parser + docs convention)
  - Files: scratchpad hotfix-verdict.md (evidence record, not committed)
  - Tests: n/a (evidence phase)
- [x] Proven day-granular request window + client-side minute filter
  - Files: crates/app/src/spot_1m_rest_boot.rs
  - Tests: test_regression_spot_1m_day_request_body_uses_proven_day_window, test_spot_1m_day_request_body_month_boundary, test_parse_intraday_columnar_utc_epoch_fixture_matches_ist_minute
- [x] Previous-minute backfill sweep (PersistTracker + ladder backfill
  extraction + honest close_to_data_ms + edge semantics unchanged)
  - Files: crates/app/src/spot_1m_rest_boot.rs
  - Tests: test_parse_intraday_columnar_for_minutes_backfill_hit, test_backfill_minute_nanos_hit_and_not_needed, test_backfill_minute_nanos_first_session_minute_has_no_backfill, test_persist_tracker_commit_max_merge_and_double_persist_idempotent, test_backfill_never_flips_edge_accounting
- [x] Post-session sweep (M1, review 2026-07-13) — one bounded ~15:31 IST
  fire repairs every session minute still missing above the per-SID
  watermark (gives the final 15:29 candle its repair path; also closes the
  L2/L4 gaps: ≥2-fire-old absences and flush-failed backfill rows)
  - Files: crates/app/src/spot_1m_rest_boot.rs
  - Tests: test_sweep_recovers_vendor_late_1529_minute, test_sweep_missing_minutes_noop_when_complete, test_sweep_missing_minutes_full_session_and_tail_gap
- [x] Option-chain enable flip + §8.7 dated rule note (verbatim relayed authorization quoted) + runbook sync + CHAIN-03 day-1 DEDUP checklist (M2/M3)
  - Files: config/base.toml, .claude/rules/project/no-rest-except-live-feed-2026-06-27.md, .claude/rules/project/rest-1m-pipeline-error-codes.md
  - Tests: existing test_wait_until_chain_fire_signal_wakes_early_and_fallback_bounds (fallback arms a–e) + option_chain_1m_wiring_guard (send_replace pin)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Minute M candle available at fire M+60 | Found via day window, persisted, ok=3, histogram sampled |
| 2 | Minute M sealed late (> 6.3 s) | M's fire empty (honest failure); M repaired by fire M+120's backfill with real delay stamp |
| 2b | 15:29 sealed late (no next fire) | Repaired by the ~15:31 post-session sweep with real delay stamp |
| 3 | Flush failure mid-fire | Watermark not advanced; SPOT1M-02; next fire re-backfills |
| 4 | Chain leg with spot leg hung | 2.5 s fallback timer fires the chain (tested arms a–e) |
| 5 | Chain entitlement regresses | CHAIN-01/02 loud; rollback = config flip |
