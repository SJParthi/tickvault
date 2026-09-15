# TickVault runtime assurance

The candidate contains corrections for volume accounting, depth selection, recovery ordering, drain scheduling, operational responses and bounded buffering. **Its Rust changes have not been compiled, formatted with rustfmt, or executed.** It is not deployed or published. Source review and passing shell fixtures are insufficient for release signoff.

| Area | Candidate correction | Current evidence | Remaining acceptance |
|---|---|---|---|
| Candles | Settle pending late gross/signed/unclassified volume before a cumulative-counter reset; rebase a retained sealed predecessor | Two new Rust tests describe 48 combination traces and one repeated-reset trace, each across 24 frames; independent source review | Execute both new tests and affected candle suites on the AWS target |
| Depth selection | Depth 200 distinct-underlying selection continues beyond depth 20's 300-contract cap | Two selector tests and a production-seam regression added; unexecuted | Full ranked population and independent caps must pass |
| Drain scheduling | After at most 64 frames, poll each ready maintenance source once; shutdown closes admission then drains queued work | Actual drain regression uses controlled virtual time and repeated nonempty backlog; unexecuted | Compile, execute and characterize actual snapshot delay under pressure |
| Capture/recovery | Preserve shed/rescue ranges; refuse incomplete deferred restore; coherent snapshot reads; conditional overflow reset | Real process-SIGKILL fixture plus deterministic concurrent regressions added; unexecuted | Run isolated recovery tests and the affected storage suite |
| Operational controls | Strict boolean force; unsupported feeds return 409; failed/pending SQL is not success; notification acknowledgements validated | Exact embedded wipe verifier passed 27 fake-response cases locally | Compile and execute Rust control/notification/API regressions |
| Bounded buffers | Latest-only configuration mailbox; bounded request/body/tail buffers; partial runbook results identified | Eleven new Rust regressions plus source review; unexecuted | Run MCP/hot-reload suites and allocation checks |
| Assurance gates | Missing measurements fail; fuzz pipeline failures propagate; evidence presence is not measured compliance | Benchmark gate 10/10 and fuzz workflow 12/12 mocked cases passed | No real benchmark/fuzz/coverage campaign is implied |

## Execution boundary

The 49 local cases are 10 benchmark-gate fixtures, 12 fuzz-workflow fixtures and 27 current wipe-verifier fixtures. They are shell/mock checks, not 49 Rust tests. Older wipe fixtures were superseded and are not added to this total.

At 2026-09-14T02:51:28.856694Z, the existing AWS instance was aarch64, the app service was active and its QuestDB container reported healthy. This is a timestamped observation, not a current-session delivery guarantee. The 731 existing AWS crate files matched the complete baseline crate-content digest `f6f5d687c7caa03c45c9016d313771d0c941734d3ed2bfab7df085d7974bb459` from commit 6637b52ec8d2604ba06b83b3f898f79aa7a85943.

A compile of that existing baseline's tickvault and smoke_test production binaries was launched in a network-isolated container with 2 CPUs, 12 GiB memory and a 3,600-second timeout. No produced executable was launched. Subsequent status requests timed out; completion is unconfirmed. This job excludes the new candidate changes.

Automatic approval review rejected an archive transfer of repository documentation and remote checkout initialization, citing lack of explicit authorization for the payload and remote state. The transfer was not retried. Candidate code was not uploaded. The next required authorization is to copy the reviewed source/test/configuration patch into an isolated AWS test directory and run the defined compile and regression checks. Documentation archives, credentials, deployment, order execution and destructive production tests are outside that validation step.

## Complexity and durability limits

Reading one already-published top row can be O(1) in row count. Updating a fixed set of 24 timeframes is bounded per accepted tick. Building a complete board still touches dirty/output rows and is O(D+R) with the fixed-width rank key; returning R rows is at least O(R). Parallelism and Rust do not remove these costs or establish a fixed elapsed-time bound.

The prior AWS 30 ns RAM result is a median of 10,000-read batch averages with a supplied clock and warm cache. It is not an individual-call tail bound or end-to-end trading latency. Two observed maximum rank latencies worsened even though every one of 28 median comparisons improved. Prior 760 passes include 132 final-source app reruns and 628 reused unchanged-component passes; none is a new candidate test pass.

WAL queue admission, buffered write, flush and completed sync are different boundaries. A process failure can destroy records still in memory. A local WAL cannot recover market events never delivered by the provider. Full 09:15 mixed-pressure pipeline behavior, real database commit/replay parity, hardware power loss and million-client capacity remain unverified.

The application backend is Rust, but the entire workspace is not exclusively Rust: operations scripts, configuration and the external QuestDB engine remain. Instrument capacities and deployed timeframe/feed policies also remain finite. A strict all-workspace Rust-only, fully dynamic, universal O(1) or zero-loss guarantee is not met.

## Evidence and reproduction

- `requirements.json` contains 67 requirement rows and 30 new findings/gaps.
- `volume.md`, `capture.md`, `complexity.md`, `controls.md`, and `verification.md` contain source references and finite test selectors.
- `wipe-verifier-results.json` contains the exact 27 local case results and source hashes.
- The interactive TickVault Runtime Assurance report separates historical AWS results from the uncompiled candidate.

## Primary references

- [Dhan market-data architecture](https://madefortrade.in/t/from-the-exchange-to-your-screen-how-dhan-built-a-fast-market-data-platform-for-retail-traders/92131), Alok Pandey, 27 July 2026: internal streaming, asynchronous storage, bounded client queues. It is not an API lossless-delivery SLA.
- [DhanHQ live market feed](https://dhanhq.co/docs/v2/live-market-feed/), retrieved 14 September 2026: connection/subscription caps, binary payload and ping/pong behavior.
- [Tokio select fairness](https://docs.rs/tokio/latest/tokio/macro.select.html), retrieved 14 September 2026: biased selection places fairness responsibility on its caller.
- [QuestDB REST compatibility API](https://questdb.com/docs/connect/compatibility/rest-api/), retrieved 14 September 2026: CSV export response and error forms.
- [GitHub Actions shell semantics](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepsshell), independently checked 14 September 2026: unspecified Bash versus explicit Bash pipefail behavior.
