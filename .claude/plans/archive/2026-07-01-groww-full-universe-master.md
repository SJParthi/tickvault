# Implementation Plan: Groww full-universe master tables + raised subscribe default

**Status:** APPROVED
**Date:** 2026-06-28
**Approved by:** operator (task brief, 2026-06-28)
**Crates touched:** `core` (`crates/core/src/feed/groww/{instruments.rs,shared_master_writer.rs}`), `app` (`crates/app/src/groww_activation.rs`)

## Design

### Root cause (verified on origin/main)
The Groww feed resolves ~742 NTM stocks + ~25 indices (~767 deduped) but the
shared master tables only receive 60 lifecycle + 35 constituency rows tagged
`feed='groww'`, and only 60 instruments stream live. Two coupled facts:

1. `assemble_watch_set` (`crates/core/src/feed/groww/instruments.rs:395`) dedups
   the resolved index+stock entries into `deduped` (~767), then at
   `instruments.rs:450-452` applies `deduped.truncate(cap)` where
   `cap = max_subscribe.unwrap_or(GROWW_MAX_SUBSCRIPTIONS).min(GROWW_MAX_SUBSCRIPTIONS)`.
   With the boot default `GROWW_DEFAULT_MAX_SUBSCRIBE = 60`
   (`instruments.rs:468`, threaded via `crates/app/src/main.rs:336-341`), `cap=60`,
   so `GrowwWatchSet.entries` holds only 60 entries. The full pre-cap set survives
   ONLY as the integer counts `resolved_stocks` / `indices`
   (`GrowwWatchSet`, `instruments.rs:99-110`).
2. The shared-master writer iterates `set.entries`:
   `build_groww_lifecycle_rows` (`shared_master_writer.rs:165`) and
   `build_groww_constituency_rows` (`shared_master_writer.rs:235`). So it can only
   emit ≤ `entries.len()` (=60) lifecycle rows and ≤ 60 constituency rows
   (35 after the `kind == Ltp && symbol_name.is_some()` filter, since 25 of the 60
   are indices). Dhan is correct because it persists the full `DailyUniverse`
   (total_count) independent of the subscribe set.

### Fix A — decouple the master from the subscribe cap (structural, the real fix)
- Add `pub master_entries: Vec<WatchEntry>` to `GrowwWatchSet`: the FULL deduped
  set BEFORE the `truncate`. Populate it in `assemble_watch_set` by cloning
  `deduped` immediately before the truncate. `entries` stays the capped
  live-subscribe set; the sidecar watch-file contract is unchanged.
- Point `build_groww_lifecycle_rows` and `build_groww_constituency_rows` at
  `set.master_entries` instead of `set.entries`. Everything else (row shaping,
  `feed='groww'`, DEDUP keys `(security_id, exchange_segment, feed)` per I-P1-11,
  degrade-safe path) is unchanged.
- Log `master_entries=<len>` on the `groww watch: watch file written` line
  (`instruments.rs:679`) and the `[feeds] Groww watch-list ready` mirror
  (`groww_activation.rs:332`) so the operator sees full-universe coverage vs the
  capped subscribe count.

### Fix B — raise the live-subscribe default to the full universe
- `GROWW_DEFAULT_MAX_SUBSCRIBE` 60 → `GROWW_MAX_SUBSCRIPTIONS` (1000): no artificial
  sub-cap below the Groww hard cap. The deduped ~767 set fits under 1000, so
  `entries == master_entries` in production. The `GROWW_MAX_SUBSCRIBE` env override
  remains intact (operator can still cap). The Groww 1000 hard cap still clamps in
  `assemble_watch_set` (`cap.min(GROWW_MAX_SUBSCRIPTIONS)`).
- HONEST envelope: ~767 LIVE subscriptions is the INTENDED config, unverified
  until Monday market open (today is Sunday 2026-06-28). It is config, not a
  proven-at-scale claim.

### Why O(1)/perf rows are honest N/A
This is entirely COLD-PATH (daily watch-set build + cold-path master persist,
O(n) over ~767 instruments). No per-tick path, no hot-path allocation. `WatchEntry`
is already `Clone` (`instruments.rs:63`); `master_entries` is one extra owned Vec
clone per daily build — irrelevant cost on the cold path.

## Edge Cases
- master_entries < entries can never happen (master is the pre-cap superset).
- When cap ≥ deduped.len() (the new default), entries == master_entries — both
  carry the full ~767; tested.
- A small `GROWW_MAX_SUBSCRIBE` env override (e.g. 60) still caps `entries` to 60
  while `master_entries` stays full — the decoupling proof; tested.
- Dedup runs BEFORE master_entries is captured, so master_entries is already
  deduped (no double-count).
- index entries carry no ISIN → constituency builder filters them out
  (`kind == Ltp && symbol_name.is_some()`); master_entries change does not alter
  that filter, only its input size.

## Failure Modes
- A QuestDB outage still only loses best-effort `feed='groww'` rows (degrade-safe
  `persist_groww_instruments`, `GROWW-MASTER-01`); unchanged.
- No new failure path is introduced — the change is which Vec the builders iterate.
- master_entries adds memory (~767 WatchEntry structs, owned Strings) held for the
  cold-path build only; bounded by `GROWW_MAX_UNIVERSE = 1200`.

## Test Plan
Unit tests (in `instruments.rs` + `shared_master_writer.rs`, feature-gated where the
storage row type requires `daily_universe_fetcher`):
1. `assemble_watch_set` with a resolved set > a small cap → `master_entries.len()`
   == full resolved count AND `entries.len()` == cap (decoupling proof).
2. `build_groww_lifecycle_rows` over a set with master_entries=N, entries=cap →
   N rows (not cap rows).
3. `build_groww_constituency_rows` similarly → full stock count (not capped).
4. No env override (new default) → cap is the 1000 hard cap, ~767 NOT truncated
   (entries == master_entries).
Adapt existing tests: `assemble_*` / `build_groww_watch_from_csvs_*` tests must set
`master_entries` consistently; the `set_of` test helper in `shared_master_writer.rs`
must populate `master_entries` (= entries) so existing row-builder tests stay valid.
Verification commands per STEP 5 (pub-fn-test-guard, fmt, scoped cargo test, clippy,
build). New struct fields only (no new pub fn) → pub-fn ratchet stays ≤ baseline.

## Rollback
- Single commit on a feature branch; `git revert` restores the 60-default +
  entries-iterating writer. No data migration (DEDUP UPSERT keys unchanged — same
  `(security_id, exchange_segment, feed)`; raising the row count only inserts the
  previously-missing rows, idempotently).
- `GROWW_MAX_SUBSCRIBE=60` env var reproduces the old subscribe cap at runtime
  without a code change.

## Observability
- `groww watch: watch file written` + `[feeds] Groww watch-list ready` logs gain
  `master_entries` so the operator sees full coverage vs capped subscribe count.
- Existing `tv_groww_master_persist_errors_total{stage}` counter + `GROWW-MASTER-01`
  unchanged; lifecycle/constituency persist info logs now report the full row counts.
- No new ErrorCode (no cross-ref test impact).

## Plan Items
- [x] Fix A: add `master_entries` field to `GrowwWatchSet` + populate pre-truncate
  - Files: crates/core/src/feed/groww/instruments.rs
  - Tests: test_assemble_master_entries_full_when_capped, test_assemble_master_equals_entries_when_uncapped
- [x] Fix A: point both row builders at `set.master_entries`
  - Files: crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: test_build_groww_lifecycle_rows_uses_master_entries_not_capped, test_build_groww_constituency_rows_uses_master_entries_not_capped
- [x] Fix A: log master_entries in both watch-ready log lines
  - Files: crates/core/src/feed/groww/instruments.rs, crates/app/src/groww_activation.rs
  - Tests: TEST-EXEMPT (log-only orchestration)
- [x] Fix B: raise GROWW_DEFAULT_MAX_SUBSCRIBE 60 → GROWW_MAX_SUBSCRIPTIONS
  - Files: crates/core/src/feed/groww/instruments.rs
  - Tests: test_default_max_subscribe_is_groww_hard_cap (~767 not truncated)
- [x] Adapt existing tests + `set_of` helper for master_entries
  - Files: crates/core/src/feed/groww/instruments.rs, crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: existing suite stays green

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | New default, ~767 deduped | entries==master_entries==767; 767 lifecycle + 742 constituency rows |
| 2 | GROWW_MAX_SUBSCRIBE=60 override | entries=60, master_entries=767; master tables still get full 767/742 |
| 3 | QuestDB down during persist | GROWW-MASTER-01 logged, feed unaffected (unchanged) |
| 4 | Dhan + Groww same id+segment | distinct rows under (security_id,exchange_segment,feed) (unchanged) |

## Per-Item Guarantee Matrix

The full 15-row 100% Guarantee Matrix and the 7-row Resilience Demand Matrix
from `per-wave-guarantee-matrix.md` apply to every item in this plan. See
`See per-wave-guarantee-matrix.md` (Item 22 / Item 24 canonical form). All
15 + 7 rows apply to each item above: cold-path master persistence reuses the
existing DEDUP `(security_id, exchange_segment, feed)` composite-key uniqueness
(I-P1-11), the `GROWW-MASTER-01` typed ErrorCode + runbook covers logging /
alerting / audit, and no hot path or tick-drop path is touched.
