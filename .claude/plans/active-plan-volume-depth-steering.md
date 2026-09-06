# Implementation Plan: Volume-ranked depth steering, stock options only

**Status:** APPROVED
**Date:** 2026-09-06
**Approved by:** Parthiban — verbatim, in-session: *"See whatever I mentioned go ahead with that dude okay?"*, given in direct response to an enumerated list of exactly this blocked work.
**Authority:** `.claude/rules/project/websocket-connection-scope-lock.md` § "2026-09-06 — DEPTH IS STOCK OPTIONS ONLY, RANKED BY VOLUME, WITH THE TWO OPTION FAMILIES RANKED SEPARATELY" (landed first, per the rule-file-first law).

---

## Design

A pure ranking engine in `crates/app/src/volume_leaderboard.rs`, sibling to the
existing pure `movers.rs`, feeding the existing `depth20_layout` in place of its
percent-change ranking.

**The per-tick path is a single map update. There is no heap and no threshold.**

The first design pitched to the operator kept an incremental min-heap of the top 250
and compared each tick against its weakest member. **The hostile review killed it**,
and the replacement is both simpler and safer:

| | incremental heap | O(n) sweep at the cadence |
|---|---|---|
| per-tick work | 1 compare, or ~8 ops on a re-sift, plus a side index from instrument to slot | 1 map update |
| poisoned by one bad value | **yes, permanently** — a garbage volume near `u32::MAX` pins the threshold and nothing can ever beat it again; the set freezes for the session, green | **no** — the sweep recomputes from current state, so one bad value affects one instrument and self-corrects on its next good tick |
| cost of a sweep | avoided | **~60 µs measured-class** (2.8 ns/instrument × 20,220), a 0.0012% duty cycle at 5 s |
| code | heap + slot index + re-sift + tie policy | one scan into a reusable buffer |

The heap was premature optimisation against a cost that measurement shows is free,
and it bought the single worst failure mode in the whole design. It is not built.

**The monotonicity gate is also the overflow guard.** `ParsedTick.volume` is `u32`
and the wire value wraps at the vendor, which we cannot observe directly — but a wrap
manifests as cumulative volume *falling*, and the gate refuses exactly that. One
mechanism, both defects.

**Family split.** Two independent leaderboards keyed by option family. A blended list
returns 250 index strikes and zero stock options; the split is what makes a
stock-option ranking exist.

**Gainers are an eligibility filter, not the sort key**, so the ordered set stays
monotonic and therefore stable — which is what keeps the re-subscribe delta small.

## Edge Cases

| Case | Handling |
|---|---|
| Pre-open, volume 0 for everything | The ranking is meaningless on an all-zero field. `top_k` returns EMPTY when no instrument has a non-zero volume; the caller holds the previous layout rather than subscribing an arbitrary set. Never an empty depth pool reported as enabled. |
| Ties at zero | Cannot arise — zero-volume instruments are excluded from the ranking entirely, so the tie policy never decides a live set. |
| Ties at a non-zero volume | Broken deterministically by the composite key, so the same input always yields the same order. A ranking that reorders on identical input would churn the subscription for nothing. |
| Volume falls (wrap, WAL replay, vendor consumer-skip, 09:00 reset) | REFUSED, counted, throttled-log. The stored high is retained — better to hold the last good value than to rank the most liquid contract at near-zero. |
| A wrapped instrument that keeps trading | Stays refused for the session, because post-wrap values remain below the pre-wrap high. Correct and deliberate; visible via the counter. |
| Previous close is NaN or 0 | Gated before the comparator. A non-finite value never enters the ordering — §28.4's poisoning class, one module over. |
| Map full (>25,000 instruments) | Fail-closed on the INSERT path: a new instrument is refused with a counter and a coded error. Already-tracked instruments are unaffected. |
| Session boundary | `reset_daily()` clears both leaderboards. A missed reset ranks on yesterday, which the monotonicity gate would then mask — so the reset is asserted, not assumed. |
| Frozen contract (heavy then dead) | Known and NOT fixed here. Cumulative volume never falls, so it holds a slot. Recorded in the envelope; a liveness decay is a separate change. |

## Failure Modes

1. **Threshold poisoning** — eliminated by construction (no threshold exists).
2. **Blended ranking returns zero stock options** — prevented by the family split, pinned by a test that feeds a realistic index/stock volume mix and asserts the stock list is non-empty.
3. **Non-finite comparator corrupts the ordering wholesale** — prevented by the finite gate; a sort comparator never sees a NaN.
4. **Unbounded per-instrument map** — prevented by the fail-closed cap, matching the `day_ohlc_tracker` house pattern (refuse, count, log; never evict).
5. **Subscription churn** — the ranking is delta-compared by the caller; this module returns a set and performs no I/O, so it cannot itself cause a swap.
6. **Silent staleness** — a refusal is always counted and logged, never dropped.

## Test Plan

- `monotonic_volume_updates_are_accepted_and_ranked`
- `a_falling_volume_is_refused_and_the_high_is_retained` — the wrap case
- `a_wrapped_counter_is_refused_by_the_same_gate_that_catches_replay`
- `the_map_refuses_a_new_instrument_at_the_cap_and_keeps_serving_the_tracked_ones`
- `index_and_stock_options_are_ranked_in_separate_leaderboards`
- `a_blended_ranking_would_return_zero_stock_options` — the negative control that proves the split earns its place
- `a_non_finite_previous_close_never_reaches_the_comparator`
- `top_k_returns_empty_when_every_volume_is_zero` — the pre-open rule
- `ties_break_deterministically_on_the_composite_key`
- `reset_daily_clears_both_families`
- `the_sweep_allocates_nothing_after_construction` — DHAT
- Bite-proofs: remove the monotonicity gate → the wrap test fails; remove the split → the negative control fails; remove the finite gate → the NaN test fails.

## Rollback

The module is additive and the wiring is one call site. Reverting the commit restores
percent-change ranking exactly; no schema, no config, no AWS surface, no data
migration. Nothing on disk or in QuestDB changes shape, so a rollback needs no
backfill and no table repair.

## Observability

| Signal | Meaning |
|---|---|
| `tv_volume_leaderboard_refused_total{reason}` | a tick refused — `non_monotonic` (wrap/replay/reset) or `capacity` |
| `tv_volume_leaderboard_tracked` | instruments currently tracked, per family |
| `tv_volume_leaderboard_ranked` | size of the last materialised set, per family |
| Coded log | `VOLUME-MONO-01` on a non-monotonic refusal, power-of-two throttled per family so a storm cannot flood the sink |

Counters are seeded at construction with `increment(0)` so the CloudWatch agent's
first-sample rule cannot swallow the very first refusal — the lesson from the
2026-08-28 depth-discriminator incident, where a drop counter shipped without its
rescue sibling and 104,540 rows became permanently unclassifiable.

**Not claimed:** no EMF selector entry and no alarm ship with this. The September
forecast read live today is $142.24 against an automatic `STOP_EC2_INSTANCES` line at
$135.00, and the noise lock's standing rule is that the next metric comes with a
lever, not a cost note. These are countable locally and not pageable; that is stated
rather than hidden.

---

## Plan Items

- [ ] Item 1 — `crates/app/src/volume_leaderboard.rs`: capped map, monotonicity gate, family split, O(n) top-K into a reusable buffer
  - Files: `crates/app/src/volume_leaderboard.rs`, `crates/app/src/lib.rs`
  - Tests: the eleven named above
- [ ] Item 2 — wire it as the ranking source for the depth-20 stock sockets
  - Files: `crates/app/src/depth20_layout.rs`, `crates/app/src/depth_rebalance.rs`
  - Tests: layout tests updated to assert volume ordering
- [ ] Item 3 — depth-200 top-5 with the distinct-underlying constraint
  - Files: `crates/app/src/depth200_atm.rs`
  - Tests: distinct-underlying selection, and the down-rank case the thin-book evidence warns about

## Z+ 15-row and 7-row guarantee matrices

Both carried by reference from `.claude/rules/project/per-wave-guarantee-matrix.md`.
Per-item specifics: coverage — every new pub fn has a test and a call site (Items 1–2
land together, so no skeleton); performance — DHAT zero-alloc on the sweep, and the
per-tick path is a single map update with no allocation; monitoring — the three
counters above; recovery — refusals are counted and the previous layout is held;
uniqueness — the composite `(security_id, exchange_segment)` key throughout, never a
bare id.

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the ranking is
pure, allocation-free after construction, fail-closed at its cap, and cannot be
poisoned by a single bad value because it holds no threshold. **NOT claimed:** that
stock-option 200-level books are worth capturing — the 2026-08-26 evidence (800
rows/minute against 100,800, and 112/210 redials against 19) says many will be
near-empty. **NOT claimed:** that this improves capture on Monday; 2026-09-04 lost
2,000,238 frames before the write-ahead log because the disk was full, which sits
upstream of everything here.
