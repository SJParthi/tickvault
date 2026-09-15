# Audit remediation — 13 September 2026

Base: `7f97c32c310807a2e4a3307b4b10b8463ad5d05d`. These repairs are a review
branch, not evidence that the running production binary has been replaced.

## Implemented behavior

| Area | Change | Scope |
|---|---|---|
| Candle volume | Rebase open buckets onto a representable counter axis; preserve post-reset and rollover deltas | Within-process vendor counter resets; not an unlimited integer guarantee |
| Candle direction | Reset counter and price references together; clear old direction carry | Regression tests cover reversals and flat prices |
| Coverage gate | Require present, nonnegative integer counters, safe numeric range, and covered <= total | Explicit zero/zero remains valid |
| Quote cache | Ordered expiry index replaces repeated full-map sweeps; at most one expired eviction per admission | O(log capacity) mutation; expected hash lookup; response copying remains O(bytes) |
| API diagnostics | Rejected requests log at DEBUG; counters and authentication decisions retained | Normal INFO logging no longer amplifies every 4xx rejection |
| Debug reads | Enforce actual byte limits, including files growing after metadata inspection | 10 MiB text/CSV and 1 MiB summary JSON |
| Candle migration | Refuse automatic table DROP unless a successful count explicitly reports zero | Preserve populated/unreadable tables; external writers must still be quiesced |
| SSH configuration | Default to no inbound SSH; reject unrestricted /0; CI reads optional restricted CIDR | Does not depend on a public SSH rule for ordinary SSM access |
| Evidence reporting | Distinguish evaluated STATIC CHECK predicates from unexecuted REFERENCE claims | No unconditional correctness or latency guarantee |
| Language inventory | Document production shell and external database dependencies | Rust-only operations migration is not falsely marked complete |
| Top Volume measurement | Add an ignored, bounded, in-memory latency characterization | Eight workloads, 100 samples each; no production data writes |

## Validation

Rust 1.95.0, Apple M4 Pro / arm64 / 48 GiB; isolated checkout on the operator's
Mac. No Rust application tests were run against production credentials or data.

- Candle module: 247 passed, 2 ignored, 0 failed.
- API library: 347 passed, 0 failed.
- Candle migration module: 24 passed, 0 failed.
- AWS infrastructure source guards: 35 passed; browser/toolchain guards: 12 passed.
- Deploy safety guards: 13 passed.
- Evidence-report rendering test: 1 passed.
- Top Volume latency characterization and dirty/full-walk oracle: 1 passed each
  in release mode, explicitly selected (neither was a zero-test invocation).
- Coverage gate: 19 selftests passed; benchmark gate: 9 selftests passed.
- Terraform formatting passed. Extracted actual SSH variable validation: 7/7
  valid/invalid cases matched expectations. This is not a full Terraform apply.

The first attempt selected a storage test target under the common package and
failed before executing it. The corrected package-specific command above passed.
This is recorded to avoid presenting the failed invocation as a test success.

The Top Volume ignored release test reports observe **batch-average** ns/event
and rank-call p50/p95/p99/max. Its results describe named synthetic workloads,
not production latency, individual-tick percentiles, cold insertion, persistence,
database visibility, or a Graviton service-level objective.

One release run, carried-lot path, 20,220 tracked stock contracts, 100 samples:

| Changed contracts | Rank p50 (us) | Rank p99 (us) | Observed maximum (us) |
|---:|---:|---:|---:|
| 100 | 7.916 | 8.625 | 9.375 |
| 500 | 34.917 | 40.875 | 43.375 |
| 2,000 | 96.125 | 116.708 | 131.250 |
| 20,220 | 698.042 | 1,102.833 | 1,266.041 |

The output cap is 250, but the full candidate sort precedes truncation. The
batch-average observe p50 ranged from 17.00 to 64.17 ns/event for these four
workloads. These are not individual-tick percentiles. Case order and uncontrolled
CPU/cache state prevent using this single run to declare either lot-lookup path
universally faster. Maxima observed in 100 samples are not worst-case bounds.

## Live operational evidence

The user authorized starting `i-0c3fe906dad5492fc` in `ap-south-1`. The instance
became running, SSM Online, tickvault active, and QuestDB healthy. The deployment
SHA parameter matches the base above; this is parameter evidence, not a computed
binary hash. Public TCP/22 from `0.0.0.0/0` was revoked after successful SSM
commands. TCP/9000 remains restricted to its existing source security group.

Read-only SQL confirmed `top_volume` has 15 physical columns and its four views
exist; the 1s view exposes the expected 20 columns. `ticks`, `top_volume`, and all
24 physical candle tables contained zero rows. `instrument_lifecycle` contained
140,081 rows and `table_storage_daily` contained 63 rows, so the entire database
was not empty. The expected named Docker volume is mounted.

[Earlier emergency recovery run 34755212369](https://github.com/SJParthi/tickvault/actions/runs/34755212369)
logged actual successful drops of ticks, market_depth, all 24 candles, and
candles_named at approximately 11:44:47 UTC, before this audit restart. This
explains the missing tick/candle history. It did not target top_volume; do not
claim it explains that table's empty state. This remediation did not execute the
recovery workflow or restore historical data.

## Remaining limits

No finite test run establishes all failure permutations. Output of N rows is
at least O(N), and full ranking sorts grow with their candidate count. Queue,
disk and shutdown overload policies remain bounded-loss tradeoffs, not zero-loss
proofs. The current public API rate protection is retained; raising it without
capacity evidence would weaken protection rather than establish customer scale.
Millions of users, complete operational-shell migration, general hot reload,
production latency and restoration of deleted history require separate measured
work. Do not merge/deploy while treating those items as completed.
