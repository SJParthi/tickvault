# Candle volume: what the implementation can and cannot assure

## Current implementation notice — September 15, 2026

The current source contract and exact AWS comparison are in [signed-bar-volume-2026-09-15.md](signed-bar-volume-2026-09-15.md). **The candidate is not deployed and its AWS latencies are unmeasured.** Candidate `2ea857464bd8550b00ab1e25382a5d9a9f7ed7e8`, tree `bbf5dcd79223779e5f9322af6d2db014d7005969`, has actual Rust and isolated QuestDB results on the native ARM64 Mac. Both exact SQL fixtures passed, one test each, on QuestDB 9.3.5 with Corretto 23. Broader Rust suites and guards have failures under repair; changed source needs retesting. Linux ARM64 release build/config validation remains pending.

| Concern | Current source behavior |
|---|---|
| Quantity | Canonical candle gross remains nonnegative. One cached signed projection uses close versus frozen previous same-timeframe close; independent tick-rule accumulation is removed. |
| Primary metric | `signed_bar_volume_vs_one_lot_v3`: `100 × (signed volume / pinned lot_size - 1)`, exact signed ratio descending. This measures against one lot, not the previous candle. |
| Presentation | Top Volume v3 API and primary views expose one signed `volume`. Physical compatibility columns and versioned legacy diagnostic formats remain; no AWS column was dropped. |
| Timeframes | Ten candle frames: 1s, 3s, 5s, 1m, 3m, 5m, 10m, 15m, 30m, 60m. Four ranked frames: 1s, 3s, 5s, 1m. Source support is not deployed availability. |
| Zero versus unknown | Equal closes produce a known zero. Missing predecessor, legacy basis, ambiguous counter or signed overflow are unavailable. Other quality flags still prevent strict ranking. |
| Conservation | Gross volume is additive across frames. Signed whole-bar volume is not; every timeframe uses its own baseline. |
| Dhan timestamps | Validated LTT selects candle buckets. The existing bounded receipt clock separately supplies observation freshness, with its original fallback when receipt evidence is unavailable or implausible. TrueData keeps its prior bucket policy. Regressing Dhan LTT cannot replace a later-event close/current price and remains marked uncertain. |
| Opening quantity | The first present counter may count its opening trade once only for Dhan NSE/BSE equity or F&O when positive cumulative equals positive last-trade quantity and both LTT and receipt fall in 09:15:00. A seen counter, later/restarted snapshot or missing evidence cannot invent a zero origin. |
| Provenance | Pinned lot, definition, underlying, family, quality, revision and explicit `volume_basis`; format-4 spill/DLQ namespaces prevent older readers silently interpreting new caches. |
| Remaining chart mismatch | Offline reconstruction with the new LTT/opening rules matches signed values at 09:15:10, :15, :20 and :25, with 163,200 gross units across the first 30 seconds. The first two chart bars still differ: reconstructed 17,400/29,400 versus chart 600/46,200. This is captured-data analysis, not Rust execution or complete chart parity. |
| Recovery | Managed boot recovery compares all 25 fields, inserts missing keys, recognizes identical rows and retains differing records as conflicts. An explicitly unknown legacy duplicate may skip only when all original 18 values agree and all seven stored extensions remain NULL. No global revision order or concurrent external-writer protection is claimed. |
| Readiness and durability | Required schema and all four primary Top Volume view acknowledgments now gate candle startup; optional console views remain best effort. Existing session/universe/bucket, closure, quality and freshness checks remain. Writer admission is not durable acknowledgment; retained recovery conflicts and upstream completeness remain separate questions. |

The recorded first Rust round has common 1,059 passed; trading 1,804 passed/3 failed/2 ignored; storage 1,425 passed/1 failed/2 ignored; app 2,408 passed/3 failed/6 ignored. The trading clock and app startup guards each have one failure, while the storage recovery integration suite passed 10 tests. This is not a clean release verdict. `evidence/volume-release-20260915/questdb-sql-round1.json` separately records both real SQL fixture passes on that source. A later candidate must carry its own evidence; historical counts, an older compilation and offline reconstruction cannot be combined into a new pass total.

## Historical source review retained for provenance

**Everything below describes the identified older source or its validation campaign. Conflicts with the current notice or runtime contract are superseded, not additional current guarantees.**

Source review: `d78c431fe5842a87ccd74a7f350dbaedb3a2e346`, September 13, 2026.
This document separates source behavior from live verification. It does not
claim that every listed harness ran, that a deployment contains this revision, or that
all possible feed failures have been tested.

## Timeframe availability

The aggregator computes all 24 variants in `TfIndex::ALL`
(`crates/trading/src/candles/tf_index.rs:409`; the consume loop is in
`multi_tf_aggregator.rs:1370`). The live drain persists a smaller set:

| Timeframes | Live candle emission in this source | Why |
|---|---|---|
| 1 s, 5 s, 10 s, 15 s, 30 s | Enabled | Requested second-scale set |
| 1m, 2m, 3m, 5m, 15m, 30m, 60m | Enabled | Requested minute-scale set |
| 2 s, 3 s, 4 s, 6 s, 7 s, 8 s, 9 s, 11 s, 12 s, 13 s, 14 s | Disabled | Fold slots exist, but emission is filtered |
| 1d | Disabled on the live-derived lane | Earlier instruction forbids deriving daily candles from this stream |

The authoritative predicate is `TfIndex::is_operator_requested`
(`tf_index.rs:627`). The drain calls it on normal, catch-up, and final-seal
paths (`crates/app/src/dhan_feed_stack.rs:2797,3374,3770`). Therefore, neither
"24 enum variants" nor "24 passing fold tests" proves 24 populated DB tables.
A request to enable more frames is a product/configuration change with real
write-volume consequences, not merely an assurance check.

## Per-column meaning

| Item | What it means | What it does not prove |
|---|---|---|
| `volume` | Gross incremental units inferred from differences between accepted day-cumulative readings | Every individual exchange trade was delivered |
| Candle `net_volume` | Signed tick-rule estimate: uptick positive, downtick negative, flat carries the prior direction when known | Actual buyer-initiated minus seller-initiated exchange trade tape |
| NULL `net_volume` | Classification unavailable, no classifiable volume, or a source/legacy record lacking the signed information | Balanced trading; zero is a separate, real result |
| `tick_count` | Number of source rows folded into the bucket | Exchange trade count or proof of deduplicated feed delivery |
| Candle `ts` | Bucket-open label on the selected IST fold clock | A UTC-normalized instant or guaranteed exchange-event-time bucketing |

`classify_tick_volume` is in `multi_tf_aggregator.rs:425`; output refusal and
clamping are in `live_candle_state.rs:327`. All frames receive the same
per-tick classification before their independent bucket folds. The persisted
column copies `seal.state.net_volume()` (`shadow_seal_columns.rs:234`).

## The opening burst and partial first windows

At the first accepted observation of an instrument, the engine seeds its
baseline to the cumulative volume it just received. That observation has
zero attributable incremental volume. This deliberately prevents a
mid-session connection from putting the entire day's prior activity into
one candle (`multi_tf_aggregator.rs`, baseline seed near 1232–1284;
`a_mid_session_slot_creation_must_not_put_a_whole_days_volume_in_one_bar`).

This also means a first observation at 09:15 with a nonzero cumulative does
not establish where or when those initial units traded. An already-observed
pre-open baseline may narrow the uncertainty; it does not turn a broker
snapshot feed into a complete trade tape. An initial cumulative total can be
recorded as a baseline/unknown quantity, but cannot honestly be assigned to
all precise first-second bars without additional source evidence.

A later monotonic cumulative reading can recover a missing packet's gross
volume delta. It cannot reconstruct the missing prices, trade order,
aggressor sides, or exact second-by-second attribution. One large aggregate
delta may be signed by the last observed price movement even if its
underlying trades went both ways.

## Scenario comparison

| Scenario | Implemented handling | Remaining boundary |
|---|---|---|
| First observation after boot | Seed cumulative; count subsequent deltas | Initial unknown volume and first partial candle remain partial |
| Normal rollover | Chain each bucket to predecessor's cumulative endpoint | Precision is limited to received snapshots and selected clock |
| Smaller out-of-order cumulative | Keep monotonic baseline and previous accepted classification price | Can amend H/L/C under policy; not a new volume delta |
| Late packet carrying larger cumulative | Carry otherwise-unattributed gross and signed delta into an available seal path | Conserves totals, but may assign volume to a later/final bucket rather than its exact original timestamp |
| One-bucket-late price | Refold most recently sealed bucket's H/L/C; emit an UPSERT | Does not rebuild unlimited older candles |
| More-than-one-bucket-late price | Refuse unavailable old bucket; count refusal | Original candle OHLC cannot be guaranteed complete |
| Large cumulative reset | Detect large drop; rebase open buckets on a shifted axis; reset direction carry | Wrapping/reset packet's own true delta is unknown; small drops are treated as stale until other policy intervenes |
| Repeated resets near zero | Preserve prior counted volume plus subsequent representable deltas | Finite integer bounds remain; no unlimited-counter guarantee |
| Same-price flow with no known direction | Return unclassified, not falsely balanced | Signed net is unavailable until direction becomes inferable |
| Catch-up seal followed by late news | Carry settlement and last-sealed amendment preserve counted gross | Exact temporal attribution still limited |
| DB unavailable | Ring/spill/replay mechanisms provide recovery paths | Durability depends on successful persistence and acknowledgment, finite capacity, disk health, and replay execution |

The reset corrections are covered by
`repeated_restarts_preserve_volume_below_equal_and_above_reset_counter`,
`a_restart_uses_its_own_price_as_the_next_ticks_reference`, and
`a_restart_does_not_invent_a_direction_for_an_unchanged_price`.

## Clock and boundary rules

`fold_clock_ist_secs` (`tf_index.rs:271` vicinity) uses receipt time when it
is between 10 seconds before and 300 seconds after the exchange timestamp.
Outside that band it falls back to exchange time. Missing receipt timestamps
also use exchange time. Both are interpreted according to this repository's
IST wall-clock epoch convention.

The candle gate is 09:00 inclusive to 15:40 exclusive. It is not identical to
the 09:15 Top Volume capture gate. Grid alignment is anchored at 09:00, so
2-minute and 30/60-minute candles can span the 09:15 regular-session opening.
Coarse candles may include auction observations. Timestamp equality alone
must not be used to equate a candle's opening label with a Top Volume
snapshot's closing/observation boundary.

A whole-second bucket may contain several updates with the same timestamp.
Close ownership follows the latest eligible fold under the chosen clock,
including equal-time arrival order. Finer ordering that the source does not
supply cannot be reconstructed by a database timestamp column.

## Existing executable evidence and its limits

| Test or group | Exercises | Scope limit |
|---|---|---|
| `all_24_frames_conserve_accepted_volume_under_reorder_and_catch_up` | New 256-trace all-24-frame check against maximum accepted cumulative minus warm baseline | Passed within the 81-test AWS candle-multi group; 256 × 64 finite synthetic updates, not every permutation |
| `all_24_frames_match_per_bucket_gross_and_net_through_full_session_boundaries` | New 23,100-delta full-session per-bucket gross/net check, real receipt timestamps, independent period arithmetic, all 24 frames | Passed within the AWS candle-multi group; 23,100 accepted deltas from 09:15 through 15:39:59 with warm 09:14:59 baseline; no missing-upstream reconstruction |
| `every_timeframe_of_one_day_must_sum_to_the_same_volume_total` | Reordered, stale and late-news gross conservation | One deterministic fixture |
| `every_sub_minute_frame_sums_to_the_same_minute_net_volume` | Mixed signs and flat carry | One minute; partial/coarse-grid endpoints require different checks |
| `hostile_fuzz_conservation_with_catch_up_seals_interleaved` | 4,000 generated reorder/catch-up traces with over/under-count ledger | Fixed seed; short traces; not a broker/WAL/DB load test |
| `hostile_fuzz_every_frame_tiles_the_day_to_one_total` | Existing source compares S1 and M1 against D1 | Despite its name, it does not currently compare all 24 frames |
| `receipt_clock_end_to_end_tests` | Receipt boundary crossing, stale snapshot and replay fallback | Synthetic clock fixtures |
| `a_replayed_spill_record_carries_the_net_volume_it_was_classified_with` | v2 seal-spill signed-flow round trip | Does not establish every historical record was v2 or recovered |
| `fold_cost_at_the_authorized_ceiling` | Ignored throughput measurement | Isolated process benchmark, not a production 09:15 SLO |
| `catch_up_seal_all_sweep_cost` | Ignored full-sweep measurement | Synthetic workload, not DB flush/recovery latency |

The fixed-seed tests are ordinary unit tests, not proptest generators.
Increasing `PROPTEST_CASES` does not increase their 4,000 cases. Repeating
the same fixed-seed binary does not add new input permutations.

## Commands for an isolated release test runner

Run in a separate test checkout/process with the intended compiler and
matching source. These commands do not launch the application or query the
live database:

```bash
cargo test --release -p tickvault-trading --lib all_24_frames_ -- --nocapture --test-threads=1
cargo test --release -p tickvault-trading --lib candles:: -- --test-threads=1
cargo test --release -p tickvault-trading --lib hostile_fuzz_ -- --nocapture --test-threads=1
cargo test --release -p tickvault-trading --lib receipt_clock_end_to_end_tests -- --nocapture
cargo test --release -p tickvault-storage --lib a_replayed_spill_record_carries_the_net_volume_it_was_classified_with -- --nocapture
cargo test --release -p tickvault-trading --lib fold_cost_at_the_authorized_ceiling -- --ignored --nocapture
cargo test --release -p tickvault-trading --lib catch_up_seal_all_sweep_cost -- --ignored --nocapture
```

Confirm a nonzero test count before treating filtered command output as
evidence. Measure benchmark resource use separately before running on a
host that also serves production traffic. Report compiler, commit, CPU,
result counts, ignored/skipped tests, and units with every timing result.

The new full-session test strengthens normal-flow coverage; combining it
with generated repeated resets and adversarial reordering remains a separate
gate. Exact per-bucket attribution is asserted only for its uninterrupted
feed with a warm baseline, not for packets whose original time is unknown. A separate pipeline fault test must track
received-frame IDs through WAL, parser, seal spill and DB acknowledgment.
Passing the pure fold tests alone cannot certify zero loss across that path.

## Completed AWS validation snapshot

The AWS targeted campaign passed 39 candle-cell and 81 candle-multi tests, with zero failures. Two candle timing tests remained ignored. These are part of the consolidated 760-test campaign, not extra tests to sum again. Passing these finite fixtures supports the tested reset, clock, conservation and late-data cases; it does not prove full-session attribution for missing broker events or a zero-loss 09:15 production pipeline.

Source implementation remains undeployed. SQL read-only checks and healthy services are distinct from applying view DDL or deploying a new executable. The test evidence file records the checked source and binary hashes; the final Top Volume cutoff retest and timing campaign completed separately at 18:57:13 UTC. The all-24-frame oracle is distinct from an unrun combined 09:15 DB/WAL-stall and host-crash experiment.
