# Implementation Plan: Late-tick re-folds its OWN minute's high/low (Option B, no grace timer)

**Status:** APPROVED
**Date:** 2026-06-05
**Approved by:** Parthiban — chose "Build Option B (re-write the minute)"; confirmed the exact case ("timestamp 9:15:59, received_at 9:16:00.000"). Supersedes locked decision L-C3 ("no silent merge") with this dated authorization.
**Crate(s) touched:** `trading`, `app`, `common`

## Context (verified by hostile-agent end-to-end validation, no illusion)

A tick's `exchange_timestamp` (LTT) decides its minute. Today the aggregator
seals minute M the instant the first M+1 tick arrives; a tick with LTT in M that
arrives a hair late (LTT 9:15:59, received 9:16:00.000) is `DiscardLate`d, so M's
candle misses its true high/low (the 23440 case).

**Option B:** the late tick re-folds into ITS OWN minute (M) and we re-emit M's
candle; QuestDB UPSERT replaces the row in place. No grace timer — timestamp
alone decides the minute.

Agent-verified facts:
- **UPSERT linchpin works:** `candles_<tf>` DEDUP `(ts, security_id, segment)`
  (`DEDUP_KEY_CANDLES`, shadow_persistence.rs); re-emit for the same bucket `ts`
  REPLACES the row (shadow_persistence.rs idempotent-re-flush contract).
- **Cross-verify (15:31 1m exact-match) IMPROVES** — our high/low matches Dhan
  better, never worse.
- **Conservation ledger UNAFFECTED** — aggregator is a downstream broadcast
  subscriber, not a ticks-table-path ledger term.
- **CRITICAL to fix (§4b):** `force_seal` (IST-midnight / post-15:30) must NOT
  leave a previous-day bucket amendable, or a stray in-session late tick could
  UPSERT a cross-day candle. Mitigation: `force_seal` CLEARS `last_sealed[tf]`.

## Plan Items

- [x] Item 1 — `AggregatorCell.last_sealed: [Mutex<LiveCandleState>; TF_COUNT]`
  - Files: `crates/trading/src/candles/aggregator_cell.rs`
  - Tests: `test_amend_late_into_just_sealed_bucket_updates_high_low`,
    `test_amend_late_does_not_touch_open_close_volume`,
    `test_late_into_older_than_last_sealed_still_discards`,
    `test_force_seal_clears_last_sealed_no_cross_day_amend`
- [x] Item 2 — `ConsumeOutcome::AmendedLate` + `fold_late_high_low` (max/min/tick_count only)
  - Files: `crates/trading/src/candles/aggregator_cell.rs`
- [x] Item 3 — store sealed_state into `last_sealed` on intraday seal; CLEAR on force_seal (§4b)
  - Files: `crates/trading/src/candles/aggregator_cell.rs`
- [x] Item 4 — driver: `amended_count` + `AmendedLate` → `on_seal`; update 3 early-return literals
  - Files: `crates/trading/src/candles/multi_tf_aggregator.rs`
  - Tests: `test_amended_late_routes_to_on_seal` (rename of `..._no_silent_merge`)
- [x] Item 5 — observability: `tv_aggregator_amended_ticks_total` + heartbeat field
  - Files: `crates/app/src/main.rs`, `crates/trading/src/candles/heartbeat.rs`
- [x] Item 6 — reframe superseded L-C3 / AGGREGATOR-LATE-01 docs (cite 2026-06-05)
  - Files: `aggregator_cell.rs` + `multi_tf_aggregator.rs` docs, `.claude/rules/project/wave-6-error-codes.md`

## Design

1. New field `last_sealed: [Mutex<LiveCandleState>; TF_COUNT]` on **`AggregatorCell`**
   (NOT on `LiveCandleState` — the 120-byte size assert must hold), init `empty()`.
2. `fold_late_high_low(&mut self, tick)`: `high = max(high, price)`,
   `low = min(low, price)`, `tick_count += 1`. Leaves open/close/volume/oi — a
   late tick's intra-minute order is unknown; only extremes are order-independent.
   Price already passed the #1027 `is_valid_ltp` ceiling before broadcast.
3. `consume_tick`:
   - intraday seal arm (`bucket_start > open`): after `mem::replace`, store the
     `sealed_state` into `last_sealed[tf]` (lock order: slot guard → last_sealed,
     consistent everywhere → no deadlock).
   - late arm (`bucket_start < open`): if `bucket_start == last_sealed[tf].bucket_start_ist_secs`
     → `fold_late_high_low` + return `AmendedLate { amended_state }`; else `DiscardLate`.
4. `force_seal`: set `last_sealed[tf] = empty()` (the §4b cross-day fix — only the
   bucket just sealed by a newer in-session tick is amendable).
5. Driver `MultiTfAggregator::consume_tick`: add `amended_count: u8` to `ConsumeStats`;
   `AmendedLate { amended_state } => { amended_count += 1; on_seal(tf, amended_state); }`;
   update the 3 early-return `ConsumeStats { .. }` literals.
6. main.rs seal callback: route `AmendedLate` through the same `on_seal` (transparent
   to the writer — UPSERT) + add a `stats.amended_count` counter block.

## Edge Cases

- Late tick into the just-sealed bucket → high/low corrected, UPSERT replaces. ✅
- Late tick older than `last_sealed` (≥2 minutes late) → `DiscardLate` (bounded:
  only the single most-recent sealed bucket is amendable).
- Post-`force_seal` (IST-midnight / 15:30) → `last_sealed` cleared → a cross-day
  late tick finds no amendable bucket → `DiscardLate`. Regression-tested.
- D1: `force_seal_all` drops D1 seals (1d is historical-only per live-feed-purity
  rule 10); D1 amends are likewise dropped — intended, documented.

## Failure Modes

- Amend can only ever RAISE high or LOWER low (max/min) — it can never erase a real
  extreme or fabricate one outside the observed prices. UPSERT replaces in place,
  no duplicate rows.
- O(1)/zero-alloc late path: one extra mutex lock + scalar max/min. RAM +~670 KB
  total (Mutex<LiveCandleState> × 21 × ~250 SIDs) — negligible on 8 GiB.

## Test Plan

- `cargo test -p tickvault-trading candles` — new + updated aggregator tests green.
- `cargo test -p tickvault-trading --lib`, `-p tickvault-common --lib` green.
- `cargo clippy -p tickvault-trading -p tickvault-app -p tickvault-common -- -D warnings -W clippy::perf` clean.
- `bash .claude/hooks/banned-pattern-scanner.sh` exit 0.
- `cargo test -p tickvault-common error_code_rule_file_crossref` — AGGREGATOR-LATE-01 still rule-mentioned.
- design-first wall (impl crates trading/app/common + this APPROVED plan).

## Rollback

Revert the `last_sealed` field + `AmendedLate` arm. No schema change (candle tables
unchanged); reverting just restores `DiscardLate`. Already-UPSERT-amended candles
stay corrected (harmless — they're more accurate).

## Observability

New `tv_aggregator_amended_ticks_total` counter + heartbeat `amended` field — an
amend is a candle mutation and must be visible (audit Rule 11 / 7-layer mandate).
`tv_aggregator_late_ticks_discarded_total` now counts only truly-too-late ticks
(older than the last sealed bucket).

## Per-Item Guarantee Matrix (cross-reference)

Bound by the 15-row 100% guarantee matrix + 7-row resilience matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`.

**Honest envelope:** "100% inside the tested envelope" = the new/updated aggregator
unit tests + the agent-verified UPSERT/cross-verify/ledger analysis. This makes a
late tick land in its OWN minute's high/low (operator's timestamp-only rule),
exact-match-safe against Dhan. It corrects high/low ONLY (a late tick's intra-minute
order is unknown); open/close/volume keep the in-order pass's values. Live
confirmation that a real 23440-class late tick is now captured remains pending a
market session — stated, not assumed.
