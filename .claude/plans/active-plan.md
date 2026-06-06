# Implementation Plan: NIFTY Total Market + index→stocks subscription expansion

**Status:** APPROVED (Sub-PR #1 = this docs/authorization record; each later code
sub-PR re-enters DRAFT→APPROVED on its own)
**Date:** 2026-06-06
**Approved by:** Parthiban — AskUserQuestion decisions 2026-06-06 (subscribe all
~750 NTM stocks; map-only for the other 32; raise cap to 1200; niftyindices.com
source; F&O stocks separately extractable).
**Crate(s) touched (whole feature):** common, core, storage, app (each in its own
sub-PR). Sub-PR #1 touches docs/rules only.

## Context

Operator directive 2026-06-06 (decisions captured via AskUserQuestion):
1. Add a 33rd index — **NIFTY Total Market** — to the tracked set (today: 31 NSE
   allowlist + 1 BSE SENSEX = 32).
2. Build an **index → its constituent stocks** mapping for every tracked index.
3. **Subscribe ALL NIFTY Total Market constituent stocks (~750)** to the live feed
   (Quote mode, the existing single main-feed WebSocket).
4. Other 32 indices: **map only** — the live subscription is the **NTM union**
   (NTM is the broadest basket and already contains their members).
5. **F&O stocks remain separately extractable** — each subscribed stock carries a
   role tag (`fno_underlying` and/or `index_constituent`); "F&O only" = an O(1)
   filter, NOT a second download or WebSocket.
6. Raise `MAX_DAILY_UNIVERSE_SIZE` 400 → **1200**.
7. Constituency source: **niftyindices.com** (rebuild the deleted downloader).

## Hard constraints (verified against current `main`, 2026-06-06)

- 2-WebSocket lock UNCHANGED — ~1,000 SIDs fit on the one main-feed conn (Dhan cap
  5,000/conn). NO new WebSocket.
- Quote mode for every SID (daily-universe lock §8).
- `(security_id, exchange_segment)` composite uniqueness + DEDUP UPSERT KEYS (I-P1-11).
- RAM on m8g.large (8 GiB): ~1,000 SIDs × 21 TFs ≈ ~3.2 GB working set vs ~3.9 GB
  headroom — MUST be measured (Sub-PR #6), not assumed.
- Each step is its own serial PR (operator-charter §H) with the 15+7 guarantee
  matrix + adversarial 3-agent review.

## Sub-PR sequence (each its own design-first plan + merge before the next)

- [x] **Sub-PR #1 — Authorization record + this master plan** (docs/rules only)
  - Files: `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
    (new dated §31 authorization section), `.claude/plans/active-plan.md`
  - Tests: none (docs); does not break any guard (additive rule section only)
- [ ] **Sub-PR #2 — Raise cap + add NIFTY Total Market to the index allowlist**
  - Files: `crates/common/src/constants.rs` (`MAX_DAILY_UNIVERSE_SIZE` 400→1200),
    `crates/core/src/instrument/index_extractor.rs` (`NSE_INDEX_ALLOWLIST` += NTM)
  - Tests: update `crates/storage/tests/daily_universe_scope_guard.rs`,
    `crates/trading/tests/aggregator_daily_universe_scale_guard.rs`,
    `index_extractor` allowlist tests
- [ ] **Sub-PR #3 — Rebuild the index-constituency downloader**
  - Files: `crates/core/src/instrument/index_constituency/*` (downloader + parser
    + cache, niftyindices.com), reuse `INDEX_CONSTITUENCY_*` constants
  - Tests: downloader hardening (redirect-none, body cap, content-type — mirror
    §18 CSV hardening), parser robustness, cache round-trip
- [ ] **Sub-PR #4 — index→stocks mapping + NTM extraction + role tagging**
  - Files: extractor that resolves NTM constituent symbols → NSE_EQ `security_id`
    via the Detailed master; `role` on the lifecycle row
    (`fno_underlying`/`index_constituent`); `instrument_lifecycle` schema `ALTER ADD`
  - Tests: symbol→SID resolution, dangling-reference reject, role union, I-P1-11
- [ ] **Sub-PR #5 — wire NTM stocks into the live subscription**
  - Files: `DailyUniverse::subscription_targets` = union(indices, F&O underlyings,
    NTM constituents) deduped; batched dispatch (100/msg)
  - Tests: union dedup, batch count, Quote-mode routing, cap bound
- [ ] **Sub-PR #6 — persistence + observability + RAM validation**
  - Files: index_constituency QuestDB table + role column; counters
    (`tv_universe_size{kind}`), Telegram on fetch failure; memory probe
  - Tests: DEDUP keys, metric presence, m8g.large RAM measurement evidence
- [ ] **Sub-PR #7 — end-to-end validation**
  - Full workspace test, 3-agent adversarial review, chaos + perf baseline,
    honest Current-vs-Proposed comparison table with measured numbers

## Design (Sub-PR #1 only)

Sub-PR #1 ships ZERO code. It records the operator's authorization in the
authoritative rule file (so the later code sub-PRs have a dated mandate the
design-first wall + reviewers can cite) and pins the sub-PR sequence here. The
cap constant, allowlist, downloader, mapping, and subscription wiring all land in
later sub-PRs, each updating its own guards.

## Edge Cases (Sub-PR #1)

- The rule edit is purely additive (a new dated section) — it does not alter the
  pinned strings (`daily_universe_scope_guard.rs` scans for URL/DEDUP/Quote/I-P1-11
  presence, not a file hash), so no guard breaks.
- `MAX_DAILY_UNIVERSE_SIZE` is NOT changed in this sub-PR — so the scale guard
  (still 400) stays green; the raise is Sub-PR #2 where the guard updates in lockstep.

## Failure Modes (Sub-PR #1)

- Docs-only; cannot affect runtime or build. Worst case: a typo in the rule file.

## Test Plan (Sub-PR #1)

- `cargo test -p tickvault-storage --test daily_universe_scope_guard` stays green
  (no pinned string changed).
- `cargo build --workspace` unaffected (no code change).

## Rollback (Sub-PR #1)

- `git revert` removes the rule section + plan. Zero runtime impact.

## Observability (Sub-PR #1)

- N/A — docs only. Runtime observability (counters/Telegram/audit) is specified for
  Sub-PR #6 where the live subscription size changes.

## Per-Item Guarantee Matrix

| Demand | Sub-PR #1 |
|---|---|
| 100% testing | N/A (docs); later sub-PRs carry the 22-category coverage |
| O(1)/perf | design pins O(1) role filter + composite-key dedup; measured in #5/#6 |
| zero-loss/WS/QuestDB | unchanged — still 2 WS, Quote mode, DEDUP keys |
| uniqueness/dedup | `(security_id, exchange_segment)` union dedup (I-P1-11) — enforced in #4/#5 |
| honest envelope | RAM at ~1,000 SIDs is a MEASURED gate in #6, not a promise |
| scenarios | constituency fetch failure / dangling symbol / cap bound each get a sub-PR test |
