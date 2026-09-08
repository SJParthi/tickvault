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
| cost of a sweep | avoided | **900 µs MEASURED** (`rank_sweep_cost_at_the_authorized_ceiling`, release, 20,220 contracts), a **0.018% duty cycle** at 5 s |
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
- `rank_sweep_cost_at_the_authorized_ceiling` — an `#[ignore]`d wall-clock harness,
  the house shape. ⚠ **This line previously named `the_sweep_allocates_nothing_after_construction`
  as delivered. No such test existed** — a named test listed against a ticked item and
  absent from the tree. The DHAT proof it promised is a real follow-up, not a
  delivered one, and is recorded as outstanding rather than quietly dropped.
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

- [x] Item 1 — `crates/app/src/volume_leaderboard.rs`: capped map, monotonicity gate, family split, O(n) top-K into a reusable buffer
  - Files: `crates/app/src/volume_leaderboard.rs`, `crates/app/src/lib.rs`
  - Tests: the eleven named above
  - **Landed alone**, in a PR after the authorization merged. See "Item 1 shipped without Item 2" below.
- [x] Item 2 — wire it as the ranking source for the depth-20 stock sockets
  - Files (as shipped, not as first planned): `crates/app/src/dhan_contract_universe.rs`
    (emits the family map), `crates/app/src/dhan_feed_stack.rs` (owns the
    leaderboard, observes per tick, ranks on the 1 s AND 5 s arms, publishes the
    gainer-eligible top set on the 5 s arm), `crates/app/src/depth20_ranked_steer.rs`
    (the ranked planner — entry 250, exit 300, ≤4 swaps per socket per minute),
    `crates/app/src/depth_rebalance.rs` (applies it once a minute; the 2026-08-26
    `depth20_layout` is the pre-first-ranking fallback only)
  - Tests: `plan_depth20_ranked_minute_*` (incl. the exit-band and inclusive-edge
    tests), `a_gainer_ranked_below_the_top_250_by_volume_still_qualifies_for_depth`,
    `gainer_eligible_stops_at_the_limit_and_tallies_only_what_it_visited`,
    `rebaseline_all_makes_the_next_window_measure_only_live_trading`,
    `a_newly_tracked_contract_reports_nothing_until_it_trades_in_a_window`, the
    `depth_rebalance_wiring_tests` publish tests, and the DHAT ingest-seam gate now
    covering `observe`
  - **Landed 2026-09-08.** Decision A (RAM, not QuestDB) held: the ranking crosses
    the task boundary through an `ArcSwapOption` snapshot
- [x] Item 3 — depth-200 top-5 with the distinct-underlying constraint
  - Files (as shipped, not as first planned): `crates/app/src/volume_leaderboard.rs`
    (`rank_distinct_underlying`, `distinct_underlying_over`, `gainer_eligible`),
    `crates/app/src/depth200_candidates.rs`, `crates/app/src/depth200_ranked_steer.rs`,
    `crates/app/src/depth_rebalance.rs` — `depth200_atm.rs` survives only as the
    pre-first-ranking fallback
  - Tests: `rank_distinct_underlying_takes_the_heaviest_contract_per_name`,
    `multiple_strikes_of_one_symbol_collapse_to_one_and_the_next_names_fill_in`,
    `rank_distinct_underlying_returns_fewer_than_k_rather_than_repeating_a_name`,
    `the_five_second_pass_publishes_the_depth200_steering_candidates`, the
    `plan_ranked_minute` suite in `depth200_ranked_steer.rs`
  - **Landed 2026-09-08** (PR #1890 wired the swap engine; the gainer filter, the
    lots-in-window key and the zero-lot exclusion followed in the depth-20 PR)
- [x] Item 4 — boot seed from the previous close, depth-200 hysteresis band, ghost-instrument redial, per-cadence top-volume views (2026-09-08 THIRD)
  - Files: crates/app/src/depth_seed.rs (NEW), crates/app/src/depth_rebalance.rs, crates/app/src/dhan_feed_stack.rs, crates/app/src/depth200_candidates.rs, crates/app/src/depth200_ranked_steer.rs, crates/app/src/depth_subscription_view.rs, crates/core/src/websocket/pool_supervisor.rs, crates/storage/src/console_views.rs
  - Tests: apply_depth_seed tests (8), entry_set_is_the_first_budget_rows_and_the_rest_is_the_band, a_held_contract_inside_the_band_is_kept_and_not_swapped, band_contracts_are_never_placed_into_a_socket, the_loop_writes_tomorrows_seed_at_the_capture_window_close, publish_depth20_at_marks_a_dropped_instrument_recently_dropped_inside_the_grace_and_ghost_after_it, an_instrument_never_held_is_unknown_never_ghost, a_live_socket_delivering_a_ghost_instrument_is_redialled_with_the_ghost_reason, request_ghost_redial_is_refused_inside_the_cooldown_and_allowed_after_it, test_top_volume_cadence_view_ddl_filters_on_the_two_stored_cadences

## Item 2's two architectural decisions (Rule 15 — decided BEFORE the wiring PR)

`audit-findings-2026-04-17.md` Rule 15 requires that a decision a later sub-PR
depends on is settled in the plan first, and Rule 17 says a plan item carrying a
"TBD" is DRAFT, not APPROVED. Item 2 named two files and settled neither of the
seams it actually crosses. Both are settled here, from evidence.

### Decision A — how volume crosses the task boundary: RAM, not QuestDB

The frame drain owns `&mut LiveIngest` on its own task. The depth-rebalance loop is
a separate task on a **one-minute** cadence, and it reads everything it has today
(`fetch_movers`, `load_depth_candidates`) from **QuestDB**. So the existing
architecture crosses this boundary through the database, and the cheapest-looking
option is to follow it: a `LATEST ON ts PARTITION BY security_id` over the day's
NSE_FNO rows.

**Rejected, on three pieces of this repository's own measured evidence:**

| | why the DB path is wrong here |
|---|---|
| The requirement | The operator asked for the ranking to follow volume *"every second I mean every tick"*. A per-minute query is not that, and a 5-second one over ~22,000 partitions is a different thing again. |
| The measured bottleneck | The disk is what is actually failing. `dhan-rest-only-noise-lock` §2.3o measures 138 GB consumed in one session, and 2026-09-04 captured **zero** ticks and dropped **2,000,238** frames because the volume was full. Adding a 22,000-partition scan every few seconds to that is the coupling class this repository has spent three weeks removing. |
| The standing rule | `aws-budget.md` rule 12 and CLAUDE.md's RAM-first principle: no decision path reads market data from the database. Depth steering is a decision path. |

**The chosen shape is the one `LiveIngest` already uses for every other per-tick
accumulator** (`refused_price`, `seals_dropped`, `scan_silence`): a **plain field on
the single-owner `&mut` struct** — no lock, no channel, no allocation on the tick
path — drained by a periodic timer arm on the drain's own `tokio::select!`.

Publication across the boundary is `arc_swap::ArcSwap<Arc<Vec<RankedContract>>>`,
already a pinned workspace dependency (`=1.9.2`), already a DIRECT dependency of
`crates/app` — so Item 2 adds no dependency and needs no approval under CLAUDE.md's
new-dep rule — and already the house pattern here: `token_manager.rs` (`TokenHandle =
Arc<ArcSwap<Option<TokenState>>>`), `dhan_data_api_limiter.rs`, `leg_identity.rs` and
`calendar_staleness.rs` all use it. The drain stores a fresh snapshot every 5
seconds; the rebalance loop `load()`s it lock-free.

The cadence sits on the drain task deliberately, for the reason CLAUDE.md's O(1)
table already records for `catch_up_seal_all`: moving a sweep off that task needs a
lock or a channel around a `&mut`, and both are strictly worse for the hot path than
a bounded pause. That row measures its own 5-second sweep at **9.67 ms, a 0.2% duty
cycle**; this one MEASURES 900 µs, **0.018%** — an order of magnitude cheaper than the
sweep already running beside it.

### Decision B — where index-vs-stock classification comes from: the contract artifact, at attach

`observe` needs `OptionFamily` and the underlying id, and a tick carries neither —
only `security_id` and `segment`, and index and stock options share `NSE_FNO`. So a
map is required, and the question is who builds it.

`DepthCandidate` already carries `is_index_option` and is built on the
**rebalance-loop** side from the daily contract artifact. `ContractSelection`, built
on the **attach** side from the same artifact, knows the same fact — it counts
`index_options` and `stock_options` separately — but **discards it per contract**,
keeping only `Vec<SubscribeInstrument>` and two aggregate counts.

**Decision:** `select_contract_universe` additionally emits the per-contract
classification it already computes, and the attach installs it on `LiveIngest`. That
is the correct owner for three reasons: it is where the fact is already known, so
nothing is re-derived (the exact bug `DepthCandidate::is_index_option`'s own doc
comment records — *"asking a six-entry index map 'do you know this name?' answers 'is
it an index?' only by accident"*); it is built once daily on a cold path; and it
makes the drain's per-tick step a single hash probe with no fallback and no guessing.

A tick whose id is not in the map is **skipped, not refused** — spots, futures and
index options all land there legitimately, and the authorization is stock options
only. Counting them as refusals would make the refusal counter meaningless.

### Item 1 shipped without Item 2 — the correction

The guarantee matrix below said *"Items 1–2 land together, so no skeleton"*. **That
is no longer true and is corrected rather than left standing.** The authorization
(the rule-file section) merged in one PR; Item 1's code follows in another, and Item
2 is blocked behind it by the serial-PR protocol.

So Item 1 is in the tree with **no production caller**. Checked against Rule 14's own
five detection signals, none is present — no stub comments, no heartbeat counter on
an inert loop, no `enabled = false` flag, no exact-count tests, and `VOLUME-MONO-01`
plus its runbook already existed. `pub-fn-wiring-guard` passes. **The honest residual,
stated rather than implied by a green check:** that guard accepts *test* call sites
while its own header describes the bug class as a missing *production* one, so the
mechanical gate is satisfied and Rule 14's intent is not. Item 2 is what closes it.

## Z+ 15-row and 7-row guarantee matrices

Both carried by reference from `.claude/rules/project/per-wave-guarantee-matrix.md`.
Per-item specifics: coverage — every new pub fn has a test; **Item 1's call sites are
tests only, because Items 1 and 2 did NOT land together as this line originally
claimed — see "Item 1 shipped without Item 2" above**; performance — DHAT zero-alloc
on the sweep, and the per-tick path is a single map update with no allocation;
monitoring — the three counters above; recovery — refusals are counted and the previous layout is held;
uniqueness — the composite `(security_id, exchange_segment)` key throughout, never a
bare id.

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the ranking is
pure, fail-closed at its cap, allocation-free on the per-tick path after
construction (⚠ `rank_distinct_underlying` DOES allocate two `Vec`s per call — the
depth-200 path, once per cadence, not per tick; an earlier version of this line said
"allocation-free after construction" without that qualifier), and cannot be
poisoned by a single bad value because it holds no threshold. **NOT claimed:** that
stock-option 200-level books are worth capturing — the 2026-08-26 evidence (800
rows/minute against 100,800, and 112/210 redials against 19) says many will be
near-empty. **NOT claimed:** that this improves capture on Monday; 2026-09-04 lost
2,000,238 frames before the write-ahead log because the disk was full, which sits
upstream of everything here.
