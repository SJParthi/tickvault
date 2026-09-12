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
| cost of a sweep | avoided | ⚠ **CORRECTED 2026-09-12.** This read "**900 µs MEASURED** … a **0.018% duty cycle** at 5 s". That figure is WITHDRAWN — the harness seeded every contract's volume equal to its own baseline, so every row was skipped before the comparator and the sort it timed was a sort of an EMPTY slice. Re-measured at the same 20,220-contract ceiling: **~2.9–3.3 ms** when every contract traded (**0.29 % duty at 1 s**), **123 µs** at the realistic 2,000-traded shape. **~3.3× worse, not better** |
| code | heap + slot index + re-sift + tie policy | one scan into a reusable buffer |

The heap was premature optimisation against a cost that measurement shows is free,
and it bought the single worst failure mode in the whole design. It is not built.

*(The verdict SURVIVES its own evidence being corrected, and that is worth stating
rather than quietly re-asserting: at 0.29 % duty in a market that cannot occur and
0.012 % in the one that does, the sweep is still far cheaper than the failure mode
the heap introduced — a threshold that one garbage value freezes for the session.
The conclusion was right for a reason the number never carried.)*

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
- ~~`a_falling_volume_is_refused_and_the_high_is_retained`~~ → shipped as
  **`non_monotonic_refusals_retain_the_stored_high_when_volume_falls`** — the wrap case
- `a_wrapped_counter_is_refused_by_the_same_gate_that_catches_replay`
- ~~`the_map_refuses_a_new_instrument_at_the_cap_and_keeps_serving_the_tracked_ones`~~ →
  shipped as **`capacity_refusals_refuse_new_contracts_but_keep_serving_the_tracked_ones`**
- `index_and_stock_options_are_ranked_in_separate_leaderboards`
- `a_blended_ranking_would_return_zero_stock_options` — the negative control that proves the split earns its place
- ~~`a_non_finite_previous_close_never_reaches_the_comparator`~~ → shipped as
  **`eligible_gain_pct_refuses_a_non_finite_or_zero_previous_close`**
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
| `tv_volume_leaderboard_tracked` | instruments currently tracked, per family AND cadence |
| `tv_volume_leaderboard_ranked` | size of the last materialised set, per family AND cadence — the cadence label arrived 2026-09-12 with the third and fourth cadences; without it four arms wrote one series and the reading was whichever fired last. Costs $0.00: neither gauge appears in any file under `deploy/`, so both are local-`/metrics` only and unalarmable |
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
    ~~`rebaseline_all_makes_the_next_window_measure_only_live_trading`~~ (⚠ **2026-09-12:**
    no such test, and no such function — `rebaseline_all` was REMOVED and is now
    BANNED by a source guard: `dhan_feed_stack.rs` asserts the body does not contain
    it, because a post-replay rebaseline seeds from the same stale value it was meant
    to correct. The line named a delivered test for a function the tree forbids),
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
  - Tests (⚠ **2026-09-12:** the two `rank_distinct_underlying_*` names below were
    RENAMED when that method was replaced by the free function `distinct_underlying_over`
    on 2026-09-08 — corrected here so the list names tests that exist):
    `distinct_over_the_full_rank_takes_the_heaviest_contract_per_name`,
    `multiple_strikes_of_one_symbol_collapse_to_one_and_the_next_names_fill_in`,
    `distinct_over_the_full_rank_returns_fewer_than_k_rather_than_repeating_a_name`,
    `the_five_second_pass_publishes_the_depth200_steering_candidates`, the
    `plan_ranked_minute` suite in `depth200_ranked_steer.rs`
  - **Landed 2026-09-08** (PR #1890 wired the swap engine; the gainer filter, the
    lots-in-window key and the zero-lot exclusion followed in the depth-20 PR)
- [x] Item 4 — boot seed from the previous close, depth-200 hysteresis band, ghost-instrument redial, per-cadence top-volume views (2026-09-08 THIRD)
  - Files: crates/app/src/depth_seed.rs (NEW), crates/app/src/depth_rebalance.rs, crates/app/src/dhan_feed_stack.rs, crates/app/src/depth200_candidates.rs, crates/app/src/depth200_ranked_steer.rs, crates/app/src/depth_subscription_view.rs, crates/core/src/websocket/pool_supervisor.rs, crates/storage/src/console_views.rs
  - Tests: apply_depth_seed tests (8), entry_set_is_the_first_budget_rows_and_the_rest_is_the_band, a_held_contract_inside_the_band_is_kept_and_not_swapped, band_contracts_are_never_placed_into_a_socket, the_loop_writes_tomorrows_seed_at_the_capture_window_close, publish_depth20_at_marks_a_dropped_instrument_recently_dropped_inside_the_grace_and_ghost_after_it, an_instrument_never_held_is_unknown_never_ghost, a_live_socket_delivering_a_ghost_instrument_is_redialled_with_the_ghost_reason, request_ghost_redial_is_refused_inside_the_cooldown_and_allowed_after_it, test_top_volume_cadence_view_ddl_filters_on_the_two_stored_cadences

- [ ] Item 5 — 2026-09-10 open-item sweep, ordered by the operator ("Fix and resolve everything I don't want any open items dude okay?", given against the enumerated live-health report of 2026-09-10)
  - Files: `crates/common/src/constants.rs` (`FEED_UNSUBSCRIBE_TWENTY_DEPTH` 25 → 24 — Dhan ignored every code-25 unsubscribe live on 2026-09-10; dated record in `websocket-connection-scope-lock.md` "2026-09-10 — THE DEPTH UNSUBSCRIBE REQUESTCODE IS SETTLED LIVE"), `crates/core/src/websocket/subscription_builder.rs`, `crates/common/tests/schema_validation.rs`, `crates/core/tests/subscription_builder_properties.rs`; `crates/app/src/contract_underlying_map.rs` (`order_selected_first` — the 25,000-entry map is filled from the SUBSCRIBED legs first, not artifact order), `crates/app/src/dhan_contract_universe.rs` (publish AFTER selection), `crates/app/tests/contract_map_selected_first_guard.rs`; `crates/app/src/dhan_feed_stack.rs` (`report_dead_classes` judged only inside the continuous session — the 08:33 IST RISK-GAP-03 false page); `crates/aws-lambdas/src/dhan_token_minter.rs` (ONE in-invocation retry with the next TOTP step on a TOTP rejection, `MAX_TOTP_ATTEMPTS = 2`), `deploy/aws/terraform/dhan-token-minter-lambda.tf` (timeout 60 → 120 s for the retry budget); `crates/storage/src/ws_frame_spill.rs` (`refusal_line_due` — the three WAL refusal `error!` lines ran once PER REFUSED FRAME on the socket reader task; 2,000,238 on 2026-09-04; now the 1st and every power of two, counters unchanged, the WS-SPILL-02 filter still fires at 1 — fast-lane attack sweep finding 1)
  - Tests (⚠ **CORRECTED 2026-09-12** — the first four names were the CODE-24 era and
    are stale in the DANGEROUS direction: code 24 was proven ignored on the wire on
    2026-09-11 and the constant was restored to the vendor-documented **25** the same
    day, so a reader following these names would go looking for tests that assert the
    value the tree deliberately abandoned. Renamed here to what shipped):
    `test_depth_unsubscribe_code_is_the_value_dhan_documents`,
    `test_twenty_depth_unsubscribe_is_the_documented_code_25`,
    `test_two_hundred_depth_unsubscribe_is_the_documented_code_25`,
    `schema_feed_unsubscribe_is_subscribe_plus_one_except_depth`;
    `order_selected_first_keeps_subscribed_contracts_ahead_and_stable`, `order_selected_first_matches_the_composite_key_never_the_id_alone`, `a_subscribed_contract_listed_last_in_the_artifact_is_mapped_before_the_cap`, `the_map_is_published_after_the_selection_with_selected_legs_first`, `the_pre_selection_rationale_is_gone`; `ist_secs_of_day_from_millis_is_total_and_ist_shifted`, `the_dead_class_verdict_is_deferred_until_the_continuous_session`, `leaving_the_session_clears_the_dead_class_latch`; `mint_token_retries_once_with_the_next_step_after_a_totp_rejection`, `mint_token_does_not_retry_a_pin_rejection`, `mint_token_maps_a_repeated_totp_rejection_to_dhan_rejected`, `test_is_totp_rejection_matches_dhans_wording_and_nothing_else`, `test_secs_to_sleep_for_next_totp_step_always_crosses_the_boundary`, `test_max_totp_attempts_is_exactly_one_retry`; `refusal_line_due_logs_the_first_and_every_power_of_two`
  - Honest envelope: code 24 is itself UNVERIFIED-LIVE until a session reads `ghost = 0` with `unsubscribed_grace > 0`; the map reorder does not raise the 25,000 cap (subscribed option legs ≈ 21.5k, so every one fits today); the session gate defers the dead-class verdict, it does not weaken it; the minter retry is exactly one, so a wrong secret still fails the invocation and still pages.

- [x] Item 6 — the top-volume board shows the LOTS and BOTH percentages, ordered by the operator on 2026-09-12 ("see if you match this claucaltion precilsey then go ahead with the same implementation espeiclaly for top volume atbels dude okay? ... we need to add these lots also dude in our top volumes table ... meanwhiel clealry note to have the percnetge change also dude okay? see which is net volume percnetge change and normal movers percnetage change dude okay?"), given with a RapidTables screenshot pinning 200 -> 3200 = 1500%
  - Decision (settled here, Rule 15): STORE the two inputs, DERIVE the three percentages. `delta_units` and `lot_size` are NOT recoverable from a stored row — `window_lots_milli = delta * 1000 / lot_size` is one equation in two unknowns, and consecutive rows cannot rescue it because a contract only appears while it is inside `TOP_VOLUME_RANK_PER_FAMILY`, so its `volume` series has holes exactly where it stopped being ranked. `window_lots`, `net_volume_chg_pct` and `underlying_chg_pct` ARE pure functions of stored columns, so they live in the view: zero bytes and no way to drift from their inputs. `lot_size` is stored rather than joined from `instrument_lifecycle` because that table is overwrite-on-UPSERT current-state (daily-universe lock §5), so a join would return TODAY's lot against a HISTORICAL snapshot and the row's own arithmetic would stop closing.
  - Files: `crates/app/src/volume_leaderboard.rs` (`RankedContract` gains `delta_units`/`lot_size`; `rank` splits the `and_then` into two `let-else` steps so the lot size survives the expression and is recorded from the SAME probe the division used), `crates/app/src/top_volume_snapshot.rs` (projection carries both, lossless `i64::from`), `crates/app/src/dhan_feed_stack.rs` + `crates/app/src/depth200_candidates.rs` (construction sites), `crates/storage/src/top_volume_rank_persistence.rs` (module `## Schema` block + CREATE DDL + `TOP_VOLUME_RANK_COLUMNS` ALTER manifest + `write_row` — all four in lockstep), `crates/storage/src/console_views.rs` (the three derived columns; `gain_pct` aliased `underlying_chg_pct` on the display surface only, the base table keeps its name)
  - Tests: `the_stored_inputs_reproduce_the_stored_rank_key`, `every_declared_column_actually_reaches_the_wire`, `the_ddl_column_parser_reads_the_whole_list_and_not_a_prefix`, `every_created_column_is_also_in_the_self_heal_manifest` (REWRITTEN — see below), `the_derived_percentages_cast_before_dividing_a_long`, `the_top_volume_views_expose_the_inputs_and_the_two_percentages`, `net_volume_chg_pct_is_a_change_from_one_lot_not_a_ratio_of_one_lot`, `rank_records_the_numerator_and_denominator_it_divided`, `delta_units_measures_one_window_even_after_a_sweep_that_ranked_nothing`, `project_snapshot_carries_the_rank_inputs_per_contract`
  - Two defects found by attacking, both PRE-EXISTING and both fixed here: (a) `every_created_column_is_also_in_the_self_heal_manifest` checked the OPPOSITE of its own name — it iterated the manifest and looked in the CREATE, so a column added to the CREATE and forgotten in the ALTER manifest passed green, which is the direction that bites (a box that already holds the table never receives the column, and the first ILP write auto-creates it at an inferred type). Its type check was near-vacuous too: `ddl.contains("LONG")` is true for any table with one LONG column anywhere. Both directions are now checked against types parsed from each column's own position, and bite-proven in both directions. (b) `append_row_carries_every_symbol_and_column` is additive `.contains()`, so a column dropped from `write_row` stays green; `every_declared_column_actually_reaches_the_wire` now derives its expectation from the manifest.
  - Honest envelope: the percentage is a CHANGE from one lot (`milli/10 - 100`), not a ratio of one lot (`milli/10`); the two differ by exactly 100 on every row, so the board ORDER is byte-for-byte identical and this changes what is READ, never what is ranked. QuestDB's LONG/DOUBLE promotion is UNVERIFIED in this repo — no other SQL string in the tree divides a LONG by a decimal literal — so both derived percentages `cast(... AS DOUBLE)` first, which is correct under either semantics and matches `docs/analysis/obi-backtest-queries.md`, whose ratio of two integer columns casts both sides. +16 B/row, ~970 MB -> ~1.19 GB/session, +0.07 points of the ~307 GB session burn; zero new metrics, zero new alarms, zero AWS cost; nothing added to the per-tick path (both fields are set on the 1s/5s rank sweep). NOT claimed: any of this verified on the box — rows written before the ALTER read NULL for both columns, exactly as they did when `window_lots_milli` was added on 2026-09-09.


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
cycle**; this one MEASURES **123 µs at the assumed realistic shape (0.012%)** and
**2.95 ms where every contract traded (0.295%)** — still cheaper than the sweep
already running beside it, though by less than the withdrawn 900 µs implied.

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
construction (⚠ `distinct_underlying_over` DOES allocate two `Vec`s per call — the
depth-200 path, once per cadence, not per tick; an earlier version of this line said
"allocation-free after construction" without that qualifier, and the version before
2026-09-12 named `rank_distinct_underlying`, which was DELETED on 2026-09-08 —
`distinct_underlying_over` is the surviving function and carries the same
allocation), and cannot be
poisoned by a single bad value because it holds no threshold. **NOT claimed:** that
stock-option 200-level books are worth capturing — the 2026-08-26 evidence (800
rows/minute against 100,800, and 112/210 redials against 19) says many will be
near-empty. **NOT claimed:** that this improves capture on Monday; 2026-09-04 lost
2,000,238 frames before the write-ahead log because the disk was full, which sits
upstream of everything here.

---

# WAVE 2 — 2026-09-12: four cadences, every traded contract, volume-percentage key, O(1) in universe size

**Status:** APPROVED
**Date:** 2026-09-12
**Approved by:** Parthiban — verbatim, in-session: *"go ahead with this ank on volume-percentage alone for all tehe ntire options contartcs enitlrey that too for every 1s,3s,5s and even 1m inclduign as well dude okay? taht too achieveign this O(1) dude okay?"*
**Authority:** `.claude/rules/project/websocket-connection-scope-lock.md` § "2026-09-12 — FOUR CADENCES, EVERY TRADED OPTION CONTRACT, RANKED ON VOLUME-PERCENTAGE CHANGE" (landed FIRST, per the rule-file-first law).
**Crates touched:** `crates/storage`, `crates/app`, `crates/common`.

## Design

> **⚠ CORRECTED 2026-09-12 — this section was written BEFORE the work and two of
> its mechanisms are not what shipped.** Corrections are inline below rather than
> a rewrite, so the plan-vs-outcome delta stays readable.

One declaration list becomes the single source for the cadence set.
~~A `macro_rules!` block generates~~ **a plain `#[repr(u8)]` enum with EXPLICIT
discriminants declares** `SnapshotCadence`'s variants, its `ALL` array, its
`COUNT`, its wire label, its interval seconds and its view name;
~~`#[repr(usize)]` plus `self as usize`~~ **`#[repr(u8)]` plus a `slot()`
accessor** supplies the per-cadence array index, so the index can never disagree
with the declaration order. *(Why no macro: the enum has four variants and six
accessors. A declarative macro would have hidden every one of them from `grep`
and from rust-analyzer's go-to-definition, to save writing four lines. The
ordering property the macro was for is instead pinned by a const assertion that
`ALL` is in discriminant order — the same guarantee, visible in the source.
`TfIndex` in the trading crate is the precedent followed.)*
The second cadence enum in `console_views` is DELETED and the view DDL is driven
from `SnapshotCadence`, which removes the drift rather than testing for it.

~~The ranking gains a persisted integer column carrying the volume-percentage
change — `window_lots_milli * 100 - 100_000`, in milli-percent, exactly the figure
the view already derives.~~ **NO column was added, and that resolves W2-2 rather
than skipping it.** The figure the operator asked to rank on,
`net_volume_chg_pct = window_lots_milli / 10 - 100`, is a strictly increasing
affine transform of the integer already stored, so the two orderings are IDENTICAL
byte for byte — ranking "on volume-percentage" and ranking on `window_lots_milli`
are the same ranking, and the percentage is already exposed to the operator by
`console_views` as a derived view column (`net_volume_chg_pct`, alongside
`window_lots` and `underlying_chg_pct`). Storing it again would be a second copy
of a derived value that can drift from its own inputs, at ~8 B × every row of the
largest observability table in the process. **The comparator is unchanged and
still sorts the integer lots**, because this repo bans a float in a sort key.
`window_lots_milli` and `gain_pct` both remain columns (operator Quote D).

The per-family persistence cut moves off `TOP_VOLUME_RANK_PER_FAMILY` — which is
also `DEPTH20_ENTRY_RANKS` and must stay pinned at 250 — onto its own constant,
raised so that every contract that traded is persisted.

The sweep stops walking the tracked population. Each window gets a pre-sized dirty
buffer and a per-contract bit; `observe` marks on the arms where volume actually
advances, and `rank` drains that window's buffer instead of iterating the map.
Per-sweep cost becomes Θ(traded) instead of Θ(tracked) — the honest form of the
operator's O(1) ask, since Θ(traded) is a hard floor when every traded contract is
an output row.

## Edge Cases

- A contract that trades in one window but not another: marked per-window, so each
  window's buffer is independent.
- A contract already marked for this window: the bitmask makes the push idempotent,
  which is what bounds the buffer at one entry per contract and lets it be pre-sized.
- Zero delta: still dropped before the sort, unchanged.
- Lot size zero or missing: still refused, unchanged.
- Less than one lot traded: the new percentage column is NEGATIVE by design — the
  change form's zero means exactly one lot.
- Out-of-window sweeps: drain and clear the window's buffer, rolling only those
  baselines; an untraded contract's baseline is already correct.
- Daily reset and per-family clear must empty the buffers and the bits.
- First sweep after a restart: baselines seed to the current volume, so the delta is
  zero and nothing is emitted — unchanged.
- WAL replay: the observer is skipped for the backlog, unchanged.
- The 1-minute boundary: quantisation against the capture window is accepted and
  recorded in the rule file rather than fixed here.

## Failure Modes

- A cadence added to the enum but not to `ALL` — made impossible: one macro list
  generates both.
- A cadence with no timer arm — caught by a new source guard asserting one arm per
  cadence.
- A cadence with no view — made impossible: the view set is generated from the same
  list.
- The zero-delta invariant broken by an unrelated edit, silently dropping contracts
  from the board — caught by a new test that pins it directly.
- A batch too wide for the write path: rows are DROPPED with no spill tier, so the
  producer byte budget is re-derived for the new row rate in the same change.
- Overflow computing the percentage column: checked arithmetic, saturating, with the
  bound asserted at compile time.

## Test Plan

- The macro generates a cadence whose label, interval and index agree — pinned for
  every cadence by iterating the generated `ALL`.
- Adding a variant cannot under-size the baseline array — asserted at compile time.
- One timer arm per cadence — source guard.
- One view per cadence — generated, plus a test that the generated set matches.
- The zero-delta invariant: a contract that does not trade is not emitted, and its
  baseline is unchanged after a sweep that skips it.
- Dirty-set equivalence: a randomised sequence of observes produces the identical
  ranked output under the dirty sweep and a full-scan reference sweep.
- The percentage column equals the view's derived expression for every row.
- The percentage column's order is identical to the lots order, ties included.
- The repaired cost harness ranks a non-empty set and uses a real lot lookup.
- Daily reset and family clear empty the buffers.
- DHAT: the per-tick path still allocates nothing with the marking added.

## Rollback

Every change is additive or behind a constant. Reverting the commit restores two
cadences, the 250 cut and the full-scan sweep; the new column remains in the table
and is simply not written, which QuestDB tolerates. No data is destroyed and no
schema is dropped.

## Observability

The tracked and ranked gauges gain a cadence label so four cadences stop aliasing
into one series. The existing counters are unchanged. **No new CloudWatch metric
name and no new alarm** — this pipeline reaches zero deployment surfaces today and
closing that gap needs a lever per the noise lock, so it is recorded in the rule
file as an open item rather than closed here.

## Plan Items (Wave 2)

- [x] W2-1 Single-source cadence declaration; delete the second enum; add 3s and 1m
  - Files: `crates/storage/src/top_volume_rank_persistence.rs`, `crates/storage/src/console_views.rs`
  - Tests: cadence label/interval/index agreement, generated view set, compile-time index bound
- [x] W2-2 Volume-percentage column — RESOLVED BY NOT STORING ONE
  - Files: `crates/app/src/volume_leaderboard.rs` (the proof, not a column)
  - Tests: `ranking_by_volume_percentage_is_the_same_order_as_ranking_by_lots`
  - `net_volume_chg_pct = window_lots_milli / 10 - 100` is a strictly increasing
    affine transform of the stored key, so the two produce the SAME sequence row
    for row including every tie. Storing it would add a second source of truth
    that disagrees with the view at the ±0 crossing (the integer form `100x -
    100_000` gives -100 where the float gives -99.99999999999432), and ~656 MB a
    session of redundancy on a box whose disk burn cost 2026-09-04 an entire
    trading day. Equivalence is PROVEN instead; the view computes the percentage.
- [x] W2-3 Persistence cut onto its own constant; depth constant untouched
  - Files: `crates/common/src/constants.rs`, `crates/app/src/dhan_feed_stack.rs`
  - Tests: depth entry/exit unchanged, persistence bound separate
- [x] W2-4 Per-window dirty sets; sweep becomes Θ(traded)
  - Files: `crates/app/src/volume_leaderboard.rs`
  - Tests: `a_contract_that_traded_is_never_skipped_by_the_sweep_that_follows`
    (the invariant, over every state `observe` can leave behind, on every
    cadence), `the_dirty_sweep_ranks_exactly_what_a_full_walk_would` (byte-for-byte
    board equivalence), `the_work_list_has_one_producer_and_the_gauges_are_per_cadence`
    (source guard: one push site, bit-guarded; two drains, each clearing its bit)
  - Bite-proven in SIX directions. Two of the six exposed gaps in the tests
    themselves — an unconditional push passed until the test covered advancing
    from a PARTIALLY marked state (the state production is in almost always,
    right after a 1 s sweep), and deleting the reset's list-clear passed until
    the fixture left work pending at reset. Both closed, then both bit.
- [x] W2-5 Timer arms for 3s and 1m; one-arm-per-cadence source guard; cadence-labelled gauges
  - Files: `crates/app/src/dhan_feed_stack.rs`, `crates/app/src/volume_leaderboard.rs`
  - Tests: source guard asserts an arm per cadence
- [x] W2-6 Repair the cost harness so it ranks a non-empty set with a real lot lookup
  - Files: `crates/app/src/volume_leaderboard.rs`
  - Tests: the harness ASSERTS it filled the board (`worst_ranked == DEPTH_20`)
    and that every realistic round ranked more than zero
  - MEASURED, release, x86 dev container, 20,220 tracked: 2.95 ms where every
    contract traded, 123 µs at an assumed-realistic 2,000, 28.7 µs at 500,
    6.4 µs at 100. The withdrawn "900 µs" was optimistic by 3.3x — it timed an
    empty sort under a constant lot stub. Both defects fixed; the lot lookup is
    now a real hash probe, which alone accounts for 1.78 ms -> 2.95 ms.

## Z+ 15-row and 7-row guarantee matrices

Carried by reference from this plan's Wave 1 section above; every row applies
unchanged to Wave 2, with these deltas: **code performance** — the DHAT gate covers
the new per-tick marking, and the repaired cost harness replaces a measurement that
measured nothing; **monitoring** — the gauges gain a cadence label, and the absence
of any CloudWatch surface for this pipeline is recorded as an open item rather than
claimed closed; **scenarios** — the dirty/full-scan equivalence test is the new
extreme-case gate.

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the per-tick
path is O(1) and allocation-free under a build-failing DHAT gate; the cadence set is
generated from one declaration so its labels, intervals, array index and views
cannot drift; the zero-delta invariant is pinned by its own test; the ranking
comparator stays integer-only. NOT claimed: per-sweep O(1), which is arithmetically
impossible when every traded contract is an output row — the honest floor is
Θ(traded). NOT claimed: any duty-cycle figure from the old harness. NOT claimed:
measured uncapped row counts at 3s, 5s or 1m. NOT claimed: that this pipeline is
observable outside the box.
