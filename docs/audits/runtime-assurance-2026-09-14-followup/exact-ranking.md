# Exact top-volume ranking correction

Status: implemented in the working tree, not compiled, not deployed, and not measured on AWS. Only the explicitly labeled Python arithmetic model and `git diff --check` were executed here.

## Concrete defect and changed behavior

The previous rank key was `floor(delta_units * 1000 / lot_size)`. It is monotone but loses information. Distinct true ratios inside one milli-lot bin were treated as ties and sorted by contract id. It also discarded positive quantities below one milli-lot.

| Case | Previous behavior | Candidate behavior |
|---|---|---|
| Contract 1: 2001/2000 lots; contract 2: 2000/1999 lots | Both display 1000 milli-lots; id 1 wins | Id 2 wins because its exact ratio is greater by one cross-product unit |
| Contract 1: 1/4000 lots; contract 2: 1/2000 lots | Both display zero milli-lots and are discarded | Both remain; id 2 wins |
| Ratios 2001/2000 and 4002/4000 | Rounded tie | True exact tie; id then segment decide deterministically |
| Actual delta zero | Omitted | Omitted |
| Missing or zero lot size | Omitted | Omitted |
| Top-k cut inside a rounded tie | Could discard the more precise winner | Applied after exact ratio ordering |

The score definition is unchanged: `(delta_units / lot_size - 1) * 100`, a percentage relative to **one lot**. This is not a percentage change from the prior candle or prior ranking window. Underlying price gain remains an eligibility input, not a volume-ranking key.

## Fields and ordering

No public field or stored schema is added by this work. `RankedContract` still carries security id, segment, underlying id, cumulative volume, window milli-lots, exact window delta, and lot size.

`window_lots_milli` remains `floor(delta_units*1000/lot_size)`. The existing percentage remains `window_lots_milli/10 - 100`. Those are quantized display values. For example, 1/2000 lots is exactly 0.0005 lots and -99.95% relative to one lot, but existing display fields read 0 milli-lots and -100%. Exact ranking uses the numerator/denominator, so a displayed tie can have a precise internal order.

Within each family and supported cadence, order is:

1. Exact `delta_units / lot_size`, descending.
2. Security id, ascending, only for a true ratio tie.
3. Segment enum ordinal, ascending, only if ids also tie.

The producer still supports the same four ranking cadences: 1 second, 3 seconds, 5 seconds, and 1 minute, separately for stock and index options. This patch adds no new cadence, candle period, event-time bucketing, or order submission.

## Functions changed or added

| Function | Purpose and behavior |
|---|---|
| `VolumeLeaderboard::rank` | Keeps every eligible positive delta with a valid lot size, calculates display fields once, calls exact sorter, then truncates to k |
| `compare_ranked_contracts(&RankedContract, &RankedContract) -> Ordering` | Shared exact DESC comparator using u128 cross-products, with deterministic identity ties; root uses it in RAM publication validation |
| `exact_fractional_lots_key` | Encodes the fractional remainder of milli-lots into an order-preserving 64-bit key |
| `sort_by_exact_volume_ratio` | Reuses existing radix sorting for coarse bins and fractional refinement; restores every public display field before returning |
| `optimization_tests::reference_sort` | Independent cross-product oracle, without calling the production comparator |
| `optimization_tests::reference_rank` | Independent reference now omits actual zero deltas, not positive values rounded to zero |

`compare_ranked_contracts` defensively places a zero-denominator row after valid rows, and uses identity order for two invalid rows. Production `rank` excludes such rows, and root's RAM publisher refuses them. The comparator cannot turn a divide-by-zero input into an infinite top winner.

The existing `zero_lot_window` counter label is retained, but now counts only exact-zero window deltas. Positive sub-milli-lot quantities do not increment a refusal counter.

## Exactness and overflow proof

Let `n` be positive u32 delta, `d` positive u32 lot, `q = floor(1000*n/d)`, and `r = (1000*n) mod d`.

Different q values already have the correct ratio order. For equal q, compare `r/d`. The implemented key is `floor((r*2^64)/d)`.

For unequal fractions `r1/d1` and `r2/d2`, the cross-product difference is a nonzero integer, so their distance is at least `1/(d1*d2)`. Both denominators are at most `2^32-1`, therefore `d1*d2 < 2^64`. Their distance is strictly greater than `2^-64`; scaling by `2^64` and flooring cannot merge them. Equal fractions retain equal keys. Thus the integer coarse key followed by the 64-bit fractional key is exactly equivalent to comparing n/d over the complete positive u32 domain.

Every intermediate is bounded:

- `n*1000 < 2^42`.
- `r < d <= 2^32-1`.
- `r << 64 < 2^96`, safe in u128.
- Fractional key `< 2^64`, so conversion to u64 is exact.
- Comparator cross-products are at most `(2^32-1)^2 < 2^64`, computed in u128.

The scratch field is temporarily replaced only after a complete equal-q range has been identified. No callback, I/O, or publication happens while it contains the fractional key. Sorting swaps whole rows; the group is restored to its original common q before the next range is examined or the result is returned. Groups with only one true ratio skip the second sort and preserve the identity order already established by the first sort.

## Complexity and performance limits

| Operation | Bound relevant to this correction |
|---|---|
| One exact ratio comparison | O(1) for fixed u32/u128 operands |
| Steady-state observation | Unchanged expected O(1) hash lookup, not a worst-case constant-time hash guarantee |
| Changed-row sweep | O(D) for D dirty contracts |
| Complete ranking | O(R) for R rows and fixed-width radix keys, including refinement |
| Scratch allocation | No new heap buffer in sorting; existing row vector is reused |
| Sort stack | Bounded by the existing 17-byte radix recursion; the two sorts run sequentially |
| Reading the first published RAM row | Unchanged O(1) with respect to board length |
| Publishing or returning all rows | O(R); a complete variable-size output is not O(1) |

Small comparison-sort groups are bounded by the existing fixed cutoffs. The extra scan and u128 remainder/division calculations cost real CPU time. There is no new latency measurement, tail bound, or nanosecond guarantee. Prior AWS ranking and publication numbers do not certify this changed implementation.

## Validation evidence

New Rust tests are present but **not executed**:

- `exact_ratio_order_refines_milli_lot_ties_in_every_family_and_cadence` — both families and every cadence, exact and rounded ties.
- `exact_ratio_ranking_retains_sub_milli_lots_but_omits_true_zero_and_invalid_lots` — positive tiny quantities, zero quantity, zero lot, baseline consumption.
- `exact_ratio_ranking_applies_top_k_after_resolving_rounded_ties` — a one-row cut retains the higher exact ratio.
- `exact_ratio_comparator_handles_extreme_cross_products_and_invalid_denominators` — near-u32 fractions differing below f64 precision, maximum numerator, invalid denominator and segment tie.
- `exact_ratio_radix_sort_matches_cross_products_and_preserves_every_payload` — 44 size/shape combinations including radix thresholds and 4096 rows, compared to independent full cross-product sorting; complete payload and public-field restoration checked.
- `optimized_exact_ratio_reference_retains_sub_milli_lots_and_extreme_close_fractions` — regular public-API regression ensures the independent optimization oracle itself rejects old rounded ordering across both families and all cadences.

Executed locally: a **Python arithmetic model**, not the Rust or radix implementation, matched independent Python Fraction order for 8 cases totaling 12,305 rows. Cases include one rounded bin with 4096 rows, all 4096 small positive fractions, 4096 deterministic u32 pairs, extreme denominators/numerators, equal ratios, and positive sub-milli ratios. Bounds on every modeled intermediate were asserted. Evidence is `/workspace/scratch/81296451ef3b/tickvault-audit/docs/audits/runtime-assurance-2026-09-14-followup/ranking-arithmetic-results.json`; executable model is `/tmp/p3-ranking-arithmetic-check.py`.

`git diff --check` passed for the two edited Rust files. Rust compiler, rustfmt, real regression execution and AWS benchmarks remain required.

The build-review agent independently reviewed the fraction proof, full-u64 radix support, fixed group boundaries, field restoration, and production zero-denominator/zero-milli filtering. It reported no concrete defect. Its separate Python Fraction check covered 26,246 ratios with no order or collision mismatch. That peer check is additional mathematical evidence, not compiled Rust or target-hardware verification.

## Read-only peer review of root integration

Root's SQL helper uses an integer quotient plus 64 fractional bits split into 31 + 31 + 2 bit limbs. Its largest remainder multiplication is `(2^32-2)*2^31 = 2^63-2^32`, within signed LONG. Each remainder is reduced before the next limb, and domain guards substitute safe operands for invalid inputs. This is mathematically equivalent to exact ratio ordering and the RAM key, although the SQL uses the fraction of lots rather than the fraction of milli-lots. Live QuestDB parsing and execution remain a release gate; SQLite arithmetic-model success is not QuestDB validation.

Root's RAM publisher validates positive delta, valid denominator through display recomputation, and exact adjacent ordering. This accepts positive sub-milli rows while rejecting inconsistent display values and invalid operands. It does not certify upstream completeness, identities, or per-instrument freshness.

## Remaining limits

Correct ranking cannot establish that vendor Volume is day-cumulative in the same units as lot size. Counter relatch policies, delayed packets, partial first windows, timer scheduling, missing observations, and upstream feed completeness remain distinct concerns. This patch supplies no guarantee against lost upstream ticks, disconnects, process crashes, disk failures, stale signals, or millions of clients. It does not make the full workspace Rust-only or turn full-board output, sorting, persistence, or network delivery into O(1).
