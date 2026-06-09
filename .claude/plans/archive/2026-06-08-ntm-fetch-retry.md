# Implementation Plan: niftyindices NTM fetch — 5× retry + exponential backoff

**Status:** APPROVED
**Date:** 2026-06-08
**Approved by:** Parthiban — "retry at least five times" + "fix everything" (2026-06-08).
**Crate(s) touched:** `tickvault-common` (constant), `tickvault-core` (fetch loop).

## Context

The NTM constituency download (`build_constituency_map`) did a **single** `fetch_slug` per list
with no retry. A transient niftyindices.com timeout / 5xx / connection-reset on that one attempt
drops the whole NTM list for the day (degrade to core universe via NTM-CONSTITUENCY-01). The retry
constants already existed (`INDEX_CONSTITUENCY_RETRY_*`) but were never wired, and
`INDEX_CONSTITUENCY_RETRY_MAX_TIMES` was only 2. (Note: today's NTM miss was the ISIN-dangling
threshold, fixed in #1053 — this PR is separate forward hardening for the *fetch* path.)

## Design

1. **`crates/common/src/constants.rs`** — `INDEX_CONSTITUENCY_RETRY_MAX_TIMES` 2 → 5 (operator
   "at least five times").
2. **`crates/core/src/instrument/index_constituency/mod.rs`**:
   - NEW pure `constituency_retry_delay_secs(attempt)` — exponential backoff from
     `INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS` (1s), doubling, capped at
     `INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS` (10s); overflow-safe.
   - NEW private `fetch_slug_with_retry(downloader, name, slug)` — up to MAX_TIMES attempts,
     `warn!` per failed attempt with the next delay, sleeps between, returns the last `Err` only on
     exhaustion. The per-attempt read timeout stays the generous 60s
     (`INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS`) + 10s connect.
   - `build_constituency_map` calls `fetch_slug_with_retry` instead of `fetch_slug`.

No change to the matching/threshold logic (that's #1053), the 2-WebSocket lock, or the
degrade-by-design fallback (an exhausted ladder still degrades to core universe, just far less often).

## Edge Cases

- attempt=1 → MIN delay; huge/`usize::MAX` attempt → capped at MAX, no overflow (try_from + shift cap).
- MAX_TIMES floored at 1 (a misconfig to 0 still does one attempt).
- All attempts fail → returns the last `Err`; caller logs + degrades exactly as before (no new panic).
- Success on attempt N → returns immediately (no extra delay).

## Failure Modes

- Worst-case added boot latency if niftyindices is fully down: 1+2+4+8 = 15s of backoff across 5
  attempts (cold path, before market open) — acceptable; bounded.
- No new hot-path code (cold boot path only); no new alloc on any tick path.

## Test Plan

`cargo test -p tickvault-core --features daily_universe_fetcher --lib index_constituency::tests`:
- `retry_delay_first_attempt_is_min`
- `retry_delay_grows_exponentially_then_caps` (1,2,4,8,10)
- `retry_delay_never_exceeds_max_even_for_huge_attempt` (incl. `usize::MAX`, no panic)
- `retry_max_times_is_at_least_five`
- existing 4 index_constituency tests still pass. (8/8 green locally.)

## Rollback

Revert the constant + the two functions + the one call-site change. Pure cold-path logic; no schema,
wire, or data change. `git revert`-clean.

## Observability

- Each failed attempt logs `warn!(index, attempt, max, delay_secs, err, "…retrying")` so the operator
  sees the retry ladder in the app log / CloudWatch. `NTM-CONSTITUENCY-01` still fires (Critical) only
  if all 5 attempts fail AND the resulting set degrades — i.e. a genuinely sustained outage.

## Per-item guarantee matrix

All 15 "100% everything" rows + 7 resilience rows from
`.claude/rules/project/per-wave-guarantee-matrix.md` apply to every item in this plan. Hot-path rows
(DHAT/Criterion) are N/A — cold boot path only, no per-tick code; the pure backoff helper is
unit-tested (incl. overflow), and the retry is bounded (≤15s) so it cannot hang boot.

## Plan Items

- [x] Raise `INDEX_CONSTITUENCY_RETRY_MAX_TIMES` 2 → 5
  - Files: crates/common/src/constants.rs
  - Tests: retry_max_times_is_at_least_five
- [x] Wire 5× retry + exponential backoff into the NTM fetch
  - Files: crates/core/src/instrument/index_constituency/mod.rs
  - Tests: retry_delay_first_attempt_is_min, retry_delay_grows_exponentially_then_caps, retry_delay_never_exceeds_max_even_for_huge_attempt
