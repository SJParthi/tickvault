# TickVault follow-up correctness review

Updated 2026-09-14: runtime candidate
`d54ebc68749f6ea3c097d9fe51180462df223e61` has now been built and tested on AWS.
The selected Rust campaign passed **905 tests, 0 failed, 0 ignored**. Both
production release binaries built. Seven actual QuestDB synthetic ordering
fixtures passed, and current exact-ranking/read/publication benchmarks were
recorded on AWS Linux ARM64. The candidate has **not been deployed**.

The Runtime Assurance report binds these results to source/build hashes and
retains the raw results and earlier failures. Its actual service/database
observations have explicit timestamps; they are not a continuous health check.
The initial offline review below is historical and is superseded for execution
status by the completed campaign. Implementation and production gaps remain.

The prior source candidate was `af77fb0698aeb4414136767e4907593e90ac1ddf`.
The current report records the final follow-up commit and code-patch hash.
Historical baseline measurements do not apply to the modified exact sorter,
publication validation, scheduler or recovery scan.

## Changes

- Compare exact positive `delta_units / lot_size` ratios, preserve sub-milli
  volume, and refine rounded radix groups with exact fixed-width fractional
  keys. Complete sorting remains O(rows), not O(1).
- Validate exact row order and coherent display values before publishing a
  RAM snapshot. Reconstruct exact rational order in SQL with signed-safe
  integer limbs, without restoring the removed rank column.
- Preserve the official opening price/range through pre-open candle rollovers.
- Scope watermark skipping to actual acknowledged Dhan market-data records,
  check full segment records/CRCs, refuse unreadable replay batches, and repair
  a reseed parser seek error.
- Service maintenance during both frame and seed backlogs; preserve real
  production source in source-guard tests with inline test probes.
- Verify all original/current/required reset targets using strict catalog and
  count parsing; fail on missing, malformed, nonzero and failed results.

## Completed current-source execution

| Campaign | Result | Scope |
|---|---|---|
| Production release builds | `tickvault` and `smoke_test` built | Does not execute startup, deploy or enable trading |
| Selected Rust suite | 905 passed, 0 failed, 0 ignored | Eleven groups; group subtotals are included in 905 |
| Actual QuestDB ordering | 7 fixtures passed, 20,057 rows compared | Read-only synthetic data; no populated production-table parity |
| AWS benchmarks | 162 operation/run records | Warm exact ranking, publication and prepared RAM reads; no end-to-end or concurrent-writer guarantee |

Individually timed prepared RAM reads had per-run medians of 181–189 ns,
including timer overhead. Stock/1s exact ranking of 25,000 rows had per-run
medians of 900.669–1,945.245 µs across four tested shapes. These are distinct
operations, not interchangeable latency figures. The separate timer baseline
was not subtracted. The older roughly 30 ns batch mean is not an individually
timed latency percentile.

## Historical initial offline-review evidence

| Campaign | Result | Scope |
|---|---|---|
| Benchmark gate fixtures | 10 passed | Synthetic benchmark inputs; no new benchmark |
| Fuzz workflow fixtures | 12 passed | Mocked shell outcomes; no fuzz target execution |
| Reset verifier fixtures | 199 passed | Extracted read-only shell fragments and fake transport; no wipe/HTTP/service commands |
| Ranking arithmetic model | 8 cases, 12,305 rows | Python integer keys vs independent Fraction; no Rust radix execution |
| Numeric SQL model | 7 cases, 20,023 rows | Extracted Rust SQL-builder templates run on SQLite vs Fraction; no QuestDB execution |
| Ranking peer arithmetic check | 26,246 ratios | Independent arithmetic review, not a Rust test count |
| Source and diff checks | Completed | Does not establish compilation or runtime behavior |
| New Rust tests at initial offline review | Not yet executed then | Superseded by the source-bound 905-test AWS campaign above |

The 199 reset cases supersede the previous candidate's 27 reset cases. Current
mocked shell total is 221; the 15 mathematical/SQL cases are a separate type
of evidence. Do not add historical repeated campaigns as new test coverage.

## Remaining requirements

1. Top Volume still has four cadences. All 24 candle timeframes need a shared
   bucket-based ranking architecture to meet the all-timeframe requirement.
2. Sweep deltas and publication timestamps are not exact candle intervals
   under delayed timers. Late carry can conserve totals while moving bucket
   attribution. Neither behavior is repaired by a more precise comparator.
3. RAM readers exist, but no production automated trading reader is wired.
   No order-execution behavior was added.
4. Same-day far-future timestamps can still advance the candle watermark.
   A trusted-clock/quarantine policy needs to preserve raw observations and
   avoid silently treating uncertain data as complete.
5. Raw WAL queue admission is not a durable acknowledgement. Failed writes,
   buffered records at process/power loss, corruption handling and the bounded
   restart-sequence sample remain limitations.
6. The provider interface does not establish resumable lossless exchange-event
   replay. No universal zero-loss/never-disconnect claim is supported.
7. Existing shell/configuration operations and external QuestDB remain.
   Rust-only application corrections are not a Rust-only entire infrastructure.
8. No million-client or extreme 09:15 end-to-end capacity test has run.

## Validation procedure and remaining coverage

The production builds, selected Rust tests, selected chaos tests and synthetic
QuestDB fixtures below have completed for the current runtime candidate.
Warm microbenchmarks also completed. Populated live-table parity, concurrent
reader/writer stress, full market-open pressure, real disk exhaustion and
provider/reconnect fault campaigns remain distinct, uncompleted requirements.

Use an isolated checkout on `i-0c3fe906dad5492fc` in `ap-south-1`, with the
existing aarch64 Rust toolchain. Do not execute the normal production startup
binary as a test. Preserve running service state and use finite CPU/memory/time
limits. Confirm source hashes before and after any formatting fixes.

1. Run `cargo fmt --all -- --check`; apply required formatting and bind evidence
   to the resulting source identity.
2. Compile the real `tickvault` and `smoke_test` production binaries with the
   repository's locked dependencies, release profile and aarch64 musl target.
3. Compile and execute the relevant app, trading, storage, API, Lambda, MCP and
   core test groups, including the previous candidate's changes. Exact local
   selectors are in the accompanying reviews and `build-guard-selectors.txt`.
4. Execute the isolated SIGKILL, WAL/read-failure, watermark concurrency and
   disk-limit tests; validate every exit/result rather than treating a timeout
   or missing tool as success.
5. Validate the new SQL against the pinned QuestDB engine using isolated
   fixtures and populated ordering parity checks. SQLite arithmetic evidence
   is not a substitute for this engine-specific gate.
6. Measure fresh source hashes under representative AWS load, including exact
   ratios inside shared display bins, maximum population, repeated publication,
   retention, seed/frame backlog, slow storage and reconnect. Report per-stage
   p50/p95/p99/p99.9/max with sample sizes, not a universal worst-case bound.

The current source change is reviewable. Completed selected checks do not close
the remaining architecture, input-completeness, deployment and production-load
gaps, and do not certify every possible failure combination.
