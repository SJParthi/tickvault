# TickVault phase 2: verification obligations and gate integrity

Read-only review of `/workspace/scratch/81296451ef3b/tickvault-audit` at `6637b52ec8d2604ba06b83b3f898f79aa7a85943`, 2026-09-14. No repository files were changed, remote commands issued, or production services started. Rust and Cargo are absent in this local worker. The only executed gate reproductions were a missing-directory benchmark check and a harmless Bash pipeline experiment. Other commands below are recommendations, not reported passes.

## What the prior evidence establishes

`/workspace/scratch/81296451ef3b/aws-top-volume-final-evidence.json` records 760 passing targeted tests in 13 groups. The four library executables cover app, core, storage and trading; the two separately built core integration targets are `stress_chaos_core` and `chaos_ws_e2e_wal_durability`. Some unchanged component results were explicitly reused. Exact component binary hashes and ten source-file hashes are present.

This is useful changed-path evidence. It does not establish a successful production `tickvault` link, a full workspace test run, coverage, DHAT, Loom, fuzzing, or a deployed version matching the inspected commit. Read-only SQL statements succeeded, but the inspected tick, candle and top-volume query datasets were empty; empty results establish query acceptance, not numerical parity under real data. The host observation subsequently records the EC2 instance stopped by its hard-stop guard.

The earlier `aws-top-volume-results.json` contains Rust 1.95.0, Graviton4/r8g.xlarge, four vCPUs, CPU 3 pinning, `aarch64-unknown-linux-musl`, and `target_cpu=neoverse-n1`. That file describes an earlier source tree. The final-evidence JSON does not contain full baseline/candidate compiler invocations, complete resolved feature graphs, or all profile overrides. Do not silently transfer every earlier metadata field to the final run; obtain the actual final build manifests before claiming exact production-configuration equivalence. There is no demonstrated feature mismatch in this review, only missing equivalence evidence.

## Required build shape

Source: `Cargo.toml`, `rust-toolchain.toml`, `crates/app/Cargo.toml`, `.github/workflows/deploy-aws.yml:120-244`.

| Property | Deployed application build | Prior targeted test campaign |
|---|---|---|
| Toolchain | Rust 1.95.0 | Earlier metadata says 1.95.0; final commands should be retained |
| Executables | `tickvault` plus `smoke_test` | Four library test executables plus two integration executables |
| Target | `aarch64-unknown-linux-musl` | Final artifact paths do not independently prove target |
| Cargo features | Default app features, explicitly an empty default set | No complete resolved feature report in final JSON |
| Release profile | overflow checks, thin LTO, one codegen unit, symbols stripped, panic abort | `--release` tests use the test harness; that does not validate production panic-abort behavior |
| Provenance | `TICKVAULT_BUILD_GIT_SHA` embeds the deployment commit | Source hashes are useful but do not substitute for final shipped-binary provenance |
| Linker/C compiler | Native ARM64 `musl-gcc` for both | Must be separately recorded |

Production build command, on an isolated **native ARM64** runner already provisioned with the toolchain, musl compiler and locked dependencies:

```bash
TV_VERIFY_SHA=$(git rev-parse HEAD)
timeout --kill-after=15s 3600s env \
  CARGO_BUILD_JOBS=2 \
  CC_aarch64_unknown_linux_musl=musl-gcc \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
  TICKVAULT_BUILD_GIT_SHA="$TV_VERIFY_SHA" \
  cargo +1.95.0 build --locked --offline --release \
  --target aarch64-unknown-linux-musl -p tickvault-app \
  --bin tickvault --bin smoke_test --message-format=json
```

Clear or record task-inherited `RUSTFLAGS`, encoded Rust flags, target CPU flags and release-profile environment overrides before this build. Do not disable LTO or change codegen units solely to get a faster pass while describing it as the deployment-equivalent build. Native x86_64 `musl-gcc` is not an ARM64 cross compiler.

After successful link, inspect `file`, `readelf -h`, `readelf -l`, `stat -c%s`, and SHA-256 for both artifacts. Check ARM64, static linking, and the workflow's binary-size gate. Do not launch the application as a build smoke test; its normal startup can touch external services. Building `smoke_test` does not require running its health probes.

Proposed resource envelope: one Cargo build process, two compiler jobs, at least 8 GiB available RAM and 12 GiB available disk before starting, with a stop if the runner reaches its actual memory/disk limit. These are admission limits, not guaranteed sufficient cold-build capacity. Build timeout/OS kill/missing offline dependency means **incomplete validation**, not a passing test or proof of a code defect. Keep build time separate from test execution time.

## Finite additional test campaign

Use an isolated checkout without production credentials, mounts or Docker socket. For a full-suite run, allow loopback mocks and block non-loopback egress at the runner boundary: `CARGO_NET_OFFLINE` prevents Cargo downloads, not HTTP calls made by tests. Keep source-review work parallel; run allocation measurements and performance timers without competing work. Do not run multiple Cargo processes against one target directory.

| Priority | Check | Why it adds evidence beyond the 760 | Compile/runtime ceiling |
|---|---|---|---|
| 1 | Production `tickvault` and `smoke_test` build | Compiles and links actual shipped entrypoints under deployment settings | 60 min build; no runtime |
| 1 | Full default-feature workspace target check | Finds bin, integration and benchmark target compilation drift | 45 min, two compiler jobs |
| 1 | Full default-feature unit/integration suite | Covers six additional crate dimensions and many untouched integration seams | 45 min build, 30 min runtime campaign; one test thread per test executable or bounded nextest workers |
| 1 | App DHAT live-ingest target | Exercises the ranker's real ingest seam, including quote/full/depth/refusal paths | 30 min cold compile; 120 s binary runtime |
| 1 | Storage/trading/core DHAT targets | Checks allocation budgets across decoder, depth writer, aggregator and RAM arena | 30 min cold compile per feature group; 120 s per binary |
| 2 | Core/trading Loom with explicit feature | Runs separate model code that default features do not execute | 30 min cold compile per group; 300 s per binary; no reduced model bounds reported as exhaustive |
| 2 | Focused mutation of changed pure helpers and repaired assertions | Demonstrates that regression tests actually reject semantic changes | 30 min total initial budget; 120 s per mutant; report completed/missed/unviable/timed out separately |
| 2 | Isolated fuzz targets with explicit bounded runtime | Tests malformed packets/configuration beyond predetermined examples | 600 s per target; 4 GiB RSS each; one target at a time on a four-core host |
| 3 | Coverage with existing per-crate thresholds | Quantifies reached lines; does not prove behavior or all branches | 60 min including instrumentation; DHAT separate |

Default-feature target compilation, matching the existing full-test-nightly scope:

```bash
timeout --kill-after=15s 2700s env CARGO_BUILD_JOBS=2 \
  cargo +1.95.0 check --locked --offline --workspace --tests --all-targets
timeout --kill-after=15s 2700s env CARGO_BUILD_JOBS=2 \
  cargo +1.95.0 test --locked --offline --workspace --no-run --message-format=json
```

If nextest is already provisioned, use the repository's `ci` profile to bound individual test processes:

```bash
timeout --kill-after=15s 1800s env CARGO_BUILD_JOBS=2 \
  cargo +1.95.0 nextest run --locked --offline --workspace \
  --profile ci --no-fail-fast --test-threads 2 --retries 0
```

The prebuild avoids treating cold compile time as a runtime failure. If the chosen nextest release still rebuilds target forms, report compile and execution separately. The ordinary CI profile retries once; the explicit zero retry preserves first-attempt evidence. No `--ignored` or `--include-ignored` belongs in this full-suite command. Without nextest, enumerate actual test executables from Cargo JSON and run each with a 180-second external timeout and `--test-threads=1`, retaining each result; a single `cargo test` timeout does not cap each test separately.

DHAT compile groups (each under a 1,800-second build timeout, then execute each produced integration binary under 120 seconds with `--test-threads=1`):

```bash
cargo +1.95.0 test --locked --offline -p tickvault-app \
  --test dhat_live_ingest_seam --test dhat_mark_forward --test dhat_ws_lag \
  --no-run --message-format=json
cargo +1.95.0 test --locked --offline -p tickvault-storage \
  --test dhat_depth_append_zero_alloc --no-run --message-format=json
cargo +1.95.0 test --locked --offline -p tickvault-trading \
  --test dhat_multi_tf_fold --test dhat_risk_engine --test dhat_tick_ram_arena \
  --no-run --message-format=json
cargo +1.95.0 test --locked --offline -p tickvault-core --features dhat \
  --test dhat_allocation --test dhat_cadence_decide \
  --test dhat_depth_packet_zero_alloc --test dhat_instrument_registry \
  --test dhat_moneyness --test dhat_telegram_dispatcher \
  --test dhat_token_handle --test dhat_truedata_decode_zero_alloc \
  --test dhat_ws_reader_zero_alloc --no-run --message-format=json
```

There are 16 DHAT integration target files in the CI list: 9 core, 1 storage, 3 trading and 3 app. The app `dhat_live_ingest_seam` file has six tests, so the old CI comment that each DHAT target has exactly one test is stale. Its allocation budget is 256 blocks for 10,000 folds and 500 blocks for the full-frame mode; a passing result supports the specified budget, not literally zero allocation everywhere. One core target, `dhat_telegram_dispatcher`, has a file-level `cfg(feature = "dhat")`; omitting that feature can produce zero tests. Inspect executed counts, not only executable names.

Loom compile groups (same build timeout, then 300-second timeout per produced executable, `--test-threads=1`):

```bash
cargo +1.95.0 test --locked --offline -p tickvault-core --features loom \
  --test loom_activity_watchdog --test loom_tick_dedup --test loom_ws_decoupling \
  --no-run --message-format=json
cargo +1.95.0 test --locked --offline -p tickvault-trading --features loom \
  --test loom_circuit_breaker --no-run --message-format=json
```

Default-feature versions of these files now contain standard stress tests; they are not all empty, despite historical CI comments. Running their default-feature targets is still not running their Loom models. Even feature-enabled passes cannot be called exhaustive production interleaving proofs: the models replace the actual channels/Notify with simplified types, and circuit-breaker tests explicitly use uninstrumented production `std` atomics in two models.

Fuzz targets declared and listed in the workflow: `tick_parser`, `config_parser`, `ws_frame_wal_replay`, `dhan_depth_packet`, `truedata_frame_decoder`. Fuzzing is excluded from the Cargo workspace. Pin uses `nightly-2026-03-15`, which must be shown to build the locked dependencies before a campaign is described as running. After a successful separate build, prefer libFuzzer `-max_total_time=600` and an outer 660-second timeout, instead of both limits at 600: this distinguishes normal campaign completion from a hung harness. Use `-rss_limit_mb=4096` and retain execution count/corpus/crash artifacts. A timeout with no proof of inputs executed is incomplete evidence. Do not launch the workflow itself as part of this read-only task.

## Concrete gate weaknesses and misleading claims

| Finding | Source and observed evidence | Consequence | Bounded repair recommendation |
|---|---|---|---|
| Missing Criterion directory passes | `scripts/bench-gate.sh:117-120`; local `bash scripts/bench-gate.sh /tmp/tickvault-phase2-known-nonexistent-criterion` exited 0 and printed `skipping bench gate` | No measurements can be reported as a successful gate; its header says no measurements must return 3 | Return 3 for missing directory, add one self-test for that distinct case |
| Fuzz pipelines lack explicit pipefail | `.github/workflows/fuzz.yml:126,147`: `cargo/timeout ... | tee ... || handler`, no explicit shell/defaults or `set -o pipefail` | Under Bash's ordinary `-e` behavior, tee success masks Cargo/fuzzer failure and bypasses the handler | Set `shell: bash` plus `set -euo pipefail`, test handler using deterministic failing stand-ins; verify hosted runner default shell semantics before describing an observed hosted failure |
| Fuzz timeout equated with success | Same workflow: exit 124 is printed as `no crashes found`; inner/outer time budgets equal | A run that never began useful fuzzing or stopped before reporting final status can look complete | Add outer grace interval; require explicit libFuzzer execution evidence and completed build metadata |
| Benchmark baseline can advance after empty/malformed measurement | `.github/workflows/bench.yml:180-184` saves whenever gate code differs from `2`, including code `3` and an absent output | An incomplete measurement can become the comparison baseline | Allow only recognized completed outcomes 0/1; an operator reset should still require valid measured data |
| Hardware drift heuristic suppresses broad real regressions too | `scripts/bench-gate.sh:90-97` and its hardware-drift logic: at least 5 regressions, >=70% share, IQR <=15 points disables relative arm | Uniform compiler/library/common-path slowdown has the same statistical shape as a host shift; this is a heuristic, not a causal proof | Retain absolute guard, expose ambiguous result, and confirm with a same-host baseline/candidate alternation before accepting drift |
| Watchdog model has no behavioral assertion | `crates/core/tests/loom_activity_watchdog.rs:106-111`: `if fired { let _ = exited; }` | Named invariant can fail conceptually without failing the test | Assert a meaningful modeled state/event relationship or rename/remove the claimed invariant until a valid model exists |
| Boolean tautologies in Loom | `loom_activity_watchdog.rs:192`; `loom_tick_dedup.rs:70,92`: `assert!(x || !x)` | These assertions cannot detect any Boolean logic bug | Replace with expected counter/state/history relationships and demonstrate a faulty model fails |
| Circuit-breaker production atomics uninstrumented | `crates/trading/tests/loom_circuit_breaker.rs:7-13` documents it; two tests use production struct | Model cannot explore interleavings at those internal std-atomic operations | Report limited shape/stress evidence; instrument real primitives for a stronger claim |
| Live chaos test self-skips | `crates/core/tests/chaos_cascade_triple_failure.rs:337-341` returns if Docker stack unavailable | Explicitly requested live test can return success having performed no live outage | Fail or produce separately counted unavailable state when deliberately selected; exclude from this campaign |
| Saturation test name overstates behavior | `crates/storage/src/ws_frame_spill.rs:6715-6730` appends one frame and asserts zero drops | It does not test a full channel or an increment | Rename to single-frame admission/initial-zero semantics; keep actual full-ring and dead-writer tests as evidence |
| OBI finiteness property admits positive infinity | `crates/trading/tests/proptest_trading.rs:385`: `spread >= 0.0 || spread.is_finite()` with message `Spread must be finite` | `+infinity` satisfies the first operand | Assert finiteness; separately specify whether a crossed orderbook's negative spread is valid |
| Quality wrapper is not CI equivalent | `scripts/quality-full.sh` skips missing audit/deny/cov, never invokes bench-gate, uses different clippy and 99% coverage flags, then can print `Code is production-ready` | Local green does not imply CI green or production readiness | Use actual CI commands and explicit unavailable results; remove absolute readiness statement |
| Quality documentation claims obsolete gates | `docs/standards/quality-gates.md:27-42` says 99% coverage, tests+ignored, release/cross-compile as an all-or-nothing six-stage gate | Documentation overstates what the current merge gate executes | Sync to workflow facts and per-crate threshold file |
| Timeout documentation understates configured window | `.config/nextest.toml` CI is period90s, terminate-after2; CI comment says capped90s | Configured repeated-period kill threshold is 180s, not90s; top comment's ten-second grace claim is also inconsistent | Correct prose or intentionally change the configured threshold with a hung-test check |

Harmless pipeline reproduction performed locally:

```bash
bash -e -c '(exit 77) | tee /dev/null || { printf "failure handler called\n"; exit 1; }'
```

Observed exit 0, with no failure-handler output. This proves the shell failure-masking mechanism. It does not claim an actual remote fuzz run crashed unnoticed.

The assertion-free scanner itself acknowledges that `.unwrap()/.expect()` count as assertions and tautologies pass its syntax check (`crates/common/tests/assertion_free_test_ratchet.rs`). Test counts and source-text wiring guards provide limited regression protection; they are not semantic proof. Source-text citation guards can also force obsolete wording to remain until adjusted together.

## Current CI coverage and exclusions

The `All Green` fan-in requires build/lint, eight default-feature crate suites, security, coverage, repo guards, DHAT and Loom. Actual ARM64 musl application builds live in the deploy workflow. Benchmarks, fuzz, mutation, full-test-nightly and live Docker chaos are separate workflows, not entries in `All Green`'s needs list. Their existence does not show completion on this commit. Mutation currently scopes only core/trading/common, so an app-only ranking change does not reach that mutation lane.

Coverage thresholds are floors, not measured current coverage: common99.4%, core91.6%, trading96.9%, storage90.1%, api98.6%, app72.6%, aws-lambdas81.1%, logs-mcp87.3%, default63.0%. The coverage run skips `dhat_` tests; dedicated DHAT is therefore an independent obligation. `cargo clippy --workspace --no-deps -- -D warnings` is the actual production lint gate. `--all-targets` was deliberately excluded; comments record 186 historical test-scoped findings, which should not be presented as a fresh measurement.

Thirteen explicit ignored tests were found at this source revision:

| Category | Count | Treatment |
|---|---:|---|
| App rank/gainer/runtime/depth timer harnesses | 6 | Exact on-demand measurements only; current final campaign already ran matrix and runtime-read harnesses |
| Core silence-scan timer | 1 | Optional characterization, not assurance gate |
| Trading fold/catch-up timers | 2 | Optional characterization, not assurance gate |
| Docker live-cascade outage | 1 | Exclude; mutates Docker service state and can self-skip |
| Multi-gigabyte spill memory experiment | 1 | Exclude from initial campaign; requires deliberate isolated disk/resource budget |
| Reporting/re-bless helper | 2 | Do not count as validation |

Never run all ignored tests as a generic completeness step. The multi-gigabyte spill experiment has a meaningful recovery/count/memory assertion and can be worthwhile later on an isolated disk; it is a separate resource-heavy experiment, not an ordinary broad test run.

## Benchmark interpretation boundaries

The new optimization matrix is appreciably stronger than a timing-only test: it asserts actual output count and compares full rows to an independent sorting oracle each sample. It covers both families, all four cadences, dense dirty sets, wide IDs/ties, populations100/2000/20220 and stock25000. It excludes update work, fixture creation, gain/lot lookup variability, publication, serialization, database/queue waits and network. No Top Volume Criterion target or corresponding latency-budget entry exists, so this matrix is a characterization rather than an automatic timing regression gate.

`top_volume_runtime` read output explicitly reports percentiles of 10,000-read **batch averages**, not individual-read latency percentiles. Publication explicitly reports O(rows) and excludes the input Vec copy. These labels are honest; retain them in the comparison table. The 100-sample maximum is an observed maximum, not a worst-case bound. A strictly O(1) whole-board or whole-workspace claim is unsupported by either the algorithms or these measurements.

For release signoff, report each obligation as passed, failed, unavailable, or not run with exact source/binary identity and nonzero work evidence. A bounded campaign can substantiate the specific tested envelope. It cannot guarantee every input, every schedule, future dependency/runtime behavior, or all external-service conditions.

## Authorized finite repairs after the initial review

The coordinator subsequently authorized edits to the identified assurance gates and two misleading storage test targets. This appendix supersedes the initial read-only status for the listed files; the earlier findings table remains the pre-fix evidence.

Modified `scripts/100pct-audit.sh`, `scripts/bench-gate.sh`, `scripts/bench-gate.selftest.sh`, `.github/workflows/fuzz.yml`, `crates/core/tests/loom_activity_watchdog.rs`, `crates/core/tests/loom_tick_dedup.rs`, `crates/storage/tests/chaos_disk_full_ulimit.rs`, and `crates/storage/tests/chaos_ws_frame_wal_replay.rs`. Added `scripts/fuzz-gate.selftest.sh` and invoked it inside the existing fuzz drift job.

- Missing Criterion directory now exits3. Ten benchmark self-test cases passed locally, including that previously false-green branch.
- Official GitHub documentation confirms unspecified Linux shell is `bash -e`, while explicit `shell: bash` adds pipeline failure propagation: [GitHub workflow shell reference](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepsshell). Both fuzz build/run steps now explicitly enable Bash and pipefail. The outer timeout has sixty seconds of reporting grace, outer timeout fails as incomplete, and a successful exit also requires a nonzero `#N DONE` execution record.
- Twelve cases executing the actual fuzz step bodies with deterministic fake Cargo/timeout tools passed locally. Cases cover real-success fixture, build failure, crash, timeout, killed process, empty/zero-execution output, invalid/zero/oversized durations and decimal leading zeroes. This is gate validation, not an actual fuzzer campaign.
- The audit tracker now reports present source/configuration as SKIP with explicit unverified execution, removes obsolete Prometheus probes and absolute coverage/complexity assertions, checks nonzero successful test results with bounded execution, retains separate logs, and exits nonzero when required evidence is missing. Actual local rerun produced0PASS/0GAP/37SKIP/3ABS and exit1, because Rust is unavailable and artifacts were not executed.
- The file-size-limit test now proves EFBIG activation in a child that ignores SIGXFSZ, checks actual surviving WAL payload integrity, rejects every nonzero child exit, and rejects zero-work output. Its deadline is15seconds. It makes no ENOSPC/zero-loss guarantee. The first child marker has a leading newline so libtest's progress prefix cannot hide it from exact-line verification.
- The former simulated-SIGKILL replay test is honestly named clean shutdown, explicitly calls bounded shutdown, and checks full payload bytes and repeated unconfirmed replay. The coordinator's other agent owns the separate real-SIGKILL experiment.
- Watchdog Loom tests now establish their stated before/after premises and assert real outcomes; unconstrained retired-gate Boolean races are labeled completion-only tests without tautologies. Production Tokio scheduling remains outside the model.

Bash syntax, fuzz YAML parsing and `git diff --check` passed locally. No Rust compilation/runtime verification was available here. Coordinator received the frozen sources and these exact test groups for AWS:

```bash
cargo test --locked --release -p tickvault-storage --test chaos_disk_full_ulimit --test chaos_ws_frame_wal_replay --no-run
cargo test --locked --release -p tickvault-core --features loom --test loom_activity_watchdog --test loom_tick_dedup --no-run
```

Run storage binaries individually under30seconds with `--test-threads=1`; expected3tests in ulimit,4in replay. Run each Loom binary under300seconds with one test thread. The source still needs the pinned Rust formatter and compilation on the coordinator's runner; report those separately when available. Remaining broader gates and heuristic weaknesses were left unresolved rather than expanding scope after the requested source freeze.
