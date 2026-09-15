# TickVault phase-2 volume correctness review

Reviewed source: `6637b52ec8d2604ba06b83b3f898f79aa7a85943`, read-only inspection of `/workspace/scratch/81296451ef3b/tickvault-audit`.

These findings are deterministic source counterexamples, pending execution against the Rust test binary. Neither `rustc` nor `cargo` is available in the local runner. No production requests were made. Existing 760-test evidence was not relabelled as coverage of these new combinations.

## Implementation follow-up

Root authorized fixes after the read-only review. Implemented locally; root's AWS Rust verification remains pending.

Changed files:

- `crates/app/src/volume_leaderboard.rs`: `GainerDepthSelection` and shared `gainer_eligible_for_depth`; existing `gainer_eligible` delegates with zero distinct limit; fixes diagnostic wording; two `gainer_depth_bands_*` regressions. Updated the source push-count invariant from six to seven to account for the additional function-local result vector, preserving the single dirty-list producer.
- `crates/app/src/dhan_feed_stack.rs`: only `snapshot_top_volume` selection/publication and the new `depth200_reaches_distinct_gainers_beyond_the_depth20_contract_limit` integration regression. Depth-20 still publishes at most 300; depth-200 independently fills its 20-name band from the complete eligible order.
- `crates/app/src/depth200_candidates.rs`: header-only correction identifying the five-second cadence and shared selection helper.
- `crates/trading/src/candles/aggregator_cell.rs`: `rebase_open_buckets` settles pending carry before changing axes, requires an amendment callback for catch-up-sealed buckets, and rebases the available predecessor (open state or retained last seal). This additionally preserves deltas when the reset and following updates are themselves late to a drained frame. No added per-instrument state or allocation.
- `crates/trading/src/candles/multi_tf_aggregator.rs`: callback forwards amendments through the existing seal path and increments amendment statistics. Two new `all_24_frames_preserve_late_carry_*` tests: a 48-trace matrix over two late policies, two catch-up states, four reset timestamp positions, and positive/negative/unknown classification; plus one nine-tick repeated-reset/catch-up trace.

Test commands for root's isolated AWS runner:

```bash
cargo test --release -p tickvault-app --lib gainer_depth_bands_ -- --test-threads=1
cargo test --release -p tickvault-app --lib depth200_reaches_distinct_gainers_ -- --test-threads=1
cargo test --release -p tickvault-app --lib volume_leaderboard:: -- --test-threads=1
cargo test --release -p tickvault-trading --lib all_24_frames_preserve_late_carry_ -- --test-threads=1
cargo test --release -p tickvault-trading --lib candles:: -- --test-threads=1
```

Expected new-test totals: two helper tests, one production-wiring regression, two all-frame conservation tests. Expected source outcome: depth-200 20 versus prior 1; matrix gross 160 and net +160/-160/unknown according to input; repeated-reset gross/net 3,000,000,200. `git diff --check` passed locally. Compile/test execution is not claimed until root supplies its result.

## 1. Depth-200 distinct selection loses eligible names beyond depth-20's 300-contract limit

`LiveIngest::snapshot_top_volume`, `crates/app/src/dhan_feed_stack.rs`, first calls `gainer_eligible(ranked_all, DEPTH20_EXIT_RANKS, ...)`, where the limit is 300 contracts, then calls `distinct_underlying_over(&gainers, DEPTH200_EXIT_UNDERLYINGS)` on that truncated result. Depth-200's intended population is the full volume-ranked eligible stock-option population, with one highest-ranked contract per distinct underlying. See the depth-200 module header and the user quote beside `distinct_underlying_over`.

Counterexample: a volume-sorted board of 319 eligible contracts. Contracts 1..=300 all belong to underlying 10; contracts 301..=319 each belong to a different eligible underlying. Existing code publishes 300 contracts for depth-20 and only ONE distinct candidate for depth-200, though the full board supplies its entire 20-name exit band. A realistic variant with approximately 100 strikes per underlying yields three names from the first 300 and excludes the fourth/fifth name immediately below the cut. This is observable without clock faults, missing metadata, or unusual integer inputs.

Minimal regression should exercise `snapshot_top_volume` without a database writer, seed the leaderboard with carried nonzero lot sizes, and set all underlying spots to 110 and previous closes to 100. After a Stock/5s snapshot, assert depth-20 publication remains exactly 300 and depth-200 contains 20 distinct names with the heaviest contract of each, including rank 301. This verifies the production coupling instead of only unit-testing the already-correct distinct helper.

Correction: a shared eligibility walk can accumulate two bounded results concurrently: first 300 eligible contracts and first 20 distinct eligible underlyings, stopping only when both results are full or the input ends. Memoize one gainer verdict per underlying. Do not rerank/roll the same cadence twice, and do not uncap depth-20's published vector. Worst-case work is O(R) for fixed band limits and fixed-width identities, not O(1) for a full board.

## 2. Large counter restart erases pending late-volume carry before it is attributed

`AggregatorCell::rebase_open_buckets`, `crates/trading/src/candles/aggregator_cell.rs`, rebases open states and then unconditionally clears `carried_upto`, `carried_net`, and `carried_unclassified`. A late packet with a larger cumulative value can have put newly accepted volume into those fields without widening the current state yet. The restart therefore discards already-accepted volume on those timeframes, while longer timeframes that folded the same tick normally retain it.

Using the existing `multi_tf_aggregator::tests::{OPEN,tick,SEG_IDX}` helpers, send `(offset_seconds, cumulative, price)`:

| Arrival | Offset | Cumulative | Meaning |
|---:|---:|---:|---|
| 1 | 0 | 3,000,000,000 | Warm baseline |
| 2 | 60 | 3,000,000,100 | Accepted +100; M1 bucket rolls |
| 3 | 10 | 3,000,000,150 | Late but accepted +50; pending M1 carry |
| 4 | 61 | 10 | Large restart; no attributable new volume |
| 5 | 62 | 20 | Accepted +10 after restart |

Use prices 100, 101, 102, 103, 104 respectively. Call `force_seal_all` and keep only the latest emission for each `(tf,bucket_start)` to account for amendments. Expected counted increment is 160 units for every timeframe. Source execution yields 110 for M1 and 160 for D1: the outstanding +50 was reset out of M1's carry. Signed volume loses the same +50. This combines existing supported late-news and large-reset behavior, rather than imposing a new broker-unit interpretation.

Proposed direct regression in the existing test module:

```rust
#[test]
fn all_frames_keep_late_volume_carry_when_a_counter_restarts() {
    let mut agg = MultiTfAggregator::new(FeedStrategy::DEFAULT);
    let mut latest = std::collections::HashMap::new();
    for (off, cum, price) in [
        (0_u32, 3_000_000_000_u32, 100.0_f32),
        (60, 3_000_000_100, 101.0),
        (10, 3_000_000_150, 102.0),
        (61, 10, 103.0),
        (62, 20, 104.0),
    ] {
        let stats = agg.consume_tick(
            Feed::Dhan,
            &tick(77, SEG_IDX, OPEN + off, price, cum),
            None,
            |_, _, _, tf, state| {
                latest.insert((tf.as_ordinal(), state.bucket_start_ist_secs), state);
            },
        );
        assert!(!stats.refused_price && !stats.out_of_session);
    }
    agg.force_seal_all(|_, _, _, tf, state| {
        latest.insert((tf.as_ordinal(), state.bucket_start_ist_secs), state);
    });
    for tf in TfIndex::ALL {
        let gross: u64 = latest.iter()
            .filter(|((ord, _), _)| *ord == tf.as_ordinal())
            .map(|(_, state)| state.volume)
            .sum();
        assert_eq!(gross, 160, "timeframe {tf:?}");
    }
}
```

The correction must settle or preserve outstanding carry before rebasing its cumulative axis. Simply retaining the old absolute `carried_upto` is unsafe: its coordinate belongs to the old series and can inflate a new-series candle. Settling into open states before computing the new shared offset handles the simple fixture; catch-up-sealed states with no open bucket need additional treatment and a deterministic fixture so that a pending carry is either emitted as a real amendment or translated into the new axis without loss. Preserve its sign and unknown-classification flag along with its gross units.

## 3. Existing operational message offers a false unit-verification rule

The zero-milli-lot warning in `volume_leaderboard.rs` tells the operator: `delta_units a multiple of lot_size means volume arrives in LOTS and the key is inverted; unrelated small values mean the premise holds.` That inference is reversed/unsupported. Whole-lot trades expressed in raw units naturally produce deltas divisible by lot size; lot-count deltas need not be divisible. Divisibility alone cannot certify vendor units in either direction. The query also samples only persisted rows, so it cannot inspect rows that the zero-lot filter excluded.

Correction is diagnostic text only: state the observations that merit investigation, require raw-feed/unit documentation or an independent broker/exchange reference, and keep unit semantics unverified until that evidence is available. Do not tell an operator that one divisibility test settles it or change the ranking denominator on that basis.

## Reviewed boundaries with no newly demonstrated defect

- Runtime slots enforce integer lots-descending/id-ascending/enum-segment-ascending order; immutable Arcs permit concurrent readers. Publication/reset ordering deliberately requires one serialized writer, and no cross-slot atomicity is promised.
- Quantization at 0.001 lot and ID/segment tie breaks are explicit metric semantics, so an exact-rational ranking difference alone is not a defect under the current contract.
- Unknown lots and sub-milli-lot results consume baselines and suppress rows by the current contract. Equal cumulative observations do not count as volume advances.
- Zero first observations, WAL exclusion, same-boundary corrections, capture cutoffs, coarse timestamp meaning, finite storage queues, and missing completeness watermarks remain boundaries already recorded in the prior assurance documents. They were not counted as newly discovered bugs.
- The known staircase recovery ambiguity remains documented. Suppressing all sub-ceiling advances would break genuine reset recovery and was not proposed.
