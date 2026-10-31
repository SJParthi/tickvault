# HANDOFF — tickvault, 31 Oct 2026 (Claude Code coordinator → Codex)

> Written by the Claude Code coordinator session at a safe checkpoint. Nothing in this branch changes code; it only adds this file.
> Repo: `SJParthi/tickvault`. Owner: Parthiban. Read `CLAUDE.md` and `.claude/rules/project/*.md` first: they are binding (rule-file-first law, dated owner quotes, REJECT lists).
> Main at handoff: `49dd25239` (#2168). Every claim below is labelled **Verified** (seen in git, CI or a tool result) or **Reported** (relayed by a worker session, not re-checked).

---

## 1. What the owner asked for (standing directives)

- Every **in-memory step a tick passes through under 1 microsecond at the slowest 1% (p99)**, with real-time proof from the live server. Network, disk, database and AWS are physically slower and are kept off the tick path; say so plainly, never claim them.
- O(1) per tick everywhere; anything not O(1) is flagged honestly (see the O(1) table in `CLAUDE.md`).
- 100% test coverage as the target, with every permutation, extreme and out-of-the-box case; honest numbers (floors in `quality/crate-coverage-thresholds.toml` are ratchets, not 100%).
- Everything saved to the database, audited, logged and visible on the dashboard.
- Rust only (frontend excepted), same code on Mac and AWS, everything behind config.
- Write units as words ("microseconds", "milliseconds"), never symbols, in anything for the owner.
- Merge every PR only after `All Green` on its exact head (repo auto-merge flow); never merge or arm by hand; nothing deploys 09:00–15:45 IST.
- Usage: Opus only; pace parallel work (owner chose **"Pace them"**, 30 Oct); weekly limit was ~80% on 31 Oct, resets **Fri 6 Nov 10:30 IST**.
- When a cloud session finishes, archive it.
- Video artifact (kid-friendly animated comparison tables, narrated): https://claude.ai/artifact/HdgmZrDZ91UV5EDahpRySh (owner-only link; last version 64, 31 Oct 09:30 IST). Not reachable from Codex; keep its content in mind if the owner asks for updates.

## 2. Completed — merged to `main` since Fri 30 Oct 05:00 UTC (Verified, `git log origin/main`)

| PR | Commit | What |
|---|---|---|
| #2141 | ba707f2f9 | F&O stocks price movers page on the dashboard |
| #2147 | b40b78f5c | Faster per-tick row build, byte-identical ILP |
| #2142 | 56dd73f5f | Rust-only cleanup (2 orphan shell viewers deleted, shell budget shrunk) |
| #2151 | f0d384f8a | Candle fold 14–38% faster per tick, identical bars |
| #2150 | 8a6d3de19 | Hot-thread core placement behind four default-off switches |
| #2148 | 3eec6a40a | Per-tick side stores benched + p99-gated |
| #2154 | 0c373ac21 | Muhurat Sun 8 Nov: calendar session times as one source; deploy/stop guards and liveness arming |
| #2153 | b8564637d | Socket read path ~0.25 microseconds per frame; budget 5 → 1 microsecond |
| #2156 | a7cb68c9c | Own-core placement **ON in production** for the Mon 2 Nov measured day |
| #2146 | b2a59ddb8 | Live proof tables `hot_path_latency_minute`, `host_capacity_minute`, `capacity_verdict_daily` + weekend `silence_calendar` test fix |
| #2160 | 79362cc0f | Order decision path under 1 microsecond p99 (placement 0.9, fill 0.4, rate limiter 0.07, breaker 0.05) |
| #2157 | 5227afec4 | Combined-fault chaos tests + count a lost-then-reused-then-shed frame |
| #2143 | a511a5f9d | Sealed-candle per-seal path, bench + p99 gate, zero allocation; adds `seal_path` = 1000 and `seal_ilp` = 2500 budgets |
| #2163 | 2db658abc | **Our own ILP row writer** for ticks, switch `[tick_storage] own_ilp_encoder` **ships OFF** |
| #2155 | 53282f8c0 | Frame drain counts per frame, skips no-op lag maxima |
| #2145 | 08752492a | Memory watchdog + background-task heartbeats, **both OFF** |
| #2167 | 170166d80 | Candle rows through our own ILP encoder (same switch, OFF) |
| #2166 | 713e7a9dd | Shared bench p99 gate defers a miss past the nightly baseline save |
| #2168 | 49dd25239 | Longer nextest timeout for one slow WAL proptest only (300 s × 2) |

**Deployed** (Verified, `deploy-aws.yml` runs): `a7cb68c9c` succeeded 30 Oct 18:57 UTC; `79362cc0f` succeeded 30 Oct ~22:46 UTC (includes #2146, #2156, #2160). Later merges (#2157 onward) are deployed by `postmerge-catchup.yml` (every 30 min cron; GitHub sometimes drops slots — it skipped ~4 h on 30 Oct). **Check before Mon 2 Nov 08:30 IST** that the newest successful deploy is for current main.

## 3. Partial — open draft PRs (paused to save usage; each session was told on 31 Oct to commit + push and reply)

| PR | Branch | Head at handoff | Owner session | State |
|---|---|---|---|---|
| #2144 | claude/feed-hardening-items-2-3 | 38898defa | session_01MdGCF2S6vB6dDX77uy7nU3 | Feed hardening items 2/3b (boot limit checks, subscribe reconcile); needs main merged in, CI |
| #2149 | perf/sub-us-depth | 0e1f0f60f | session_01MdGCF41HixERheZSrCNBgs | Depth rows pre-checked ILP names (531 → 366 nanoseconds per level, Reported) |
| #2152 | claude/speed-health-dashboard | 2ec6b8f0f | session_01MdGCF6U65bH6HGAhcQLvGZ | Dashboard speed and health panel; second hostile review round pending; JS budget raised by owner (§0.15 Rust-only lock) |
| #2158 | claude/clippy-all-targets | 2b6279bac | session_01MdGCF5NtXKBYs1HtmKcvQD | Plan 12b: clippy on test targets for five crates |
| #2159 | claude/adversarial-input-coverage | 968fd6066 | session_01MdGCF44nZmAAUkmmZztukd | Fuzz targets + property tests for every external input; two suspected-bug tests to confirm or clear |
| #2161 | claude/app-coverage | 09bfaea6b | session_01MdGCF6ixHeYBoBzJhiGJ3F | App crate coverage (measured 81.31%, floor 72.6%); raise the floor in the same PR |
| #2162 | claude/o1-per-event-sweep | 1d37c7e70 | session_01MdGCF4hRToaLtAmfWiyNm8 | Per-event O(1) sweep + CLAUDE.md O(1) table rows |
| #2164 | claude/xverify-51efg | 57de60ae7 | session_01MdGCF2RwF2NsoRqpGZkW4r | Cross-verify 51e/51f/51g; the noise-lock rule note (four `xverify_failed` reasons) is committed on this branch |

Branch heads may move once the sessions push their checkpoint; the replies are appended in §10 if they arrive before this file is final. **Always fetch the branch, do not trust the hashes above.**

Other paused jobs with no PR (each told to push to `handoff/<topic>` if it has anything): coverage core, storage (baseline 92.587%, earlier unpushed work was lost to a container reset), trading/api/common, lambdas/logs-mcp; audit-fix triage parts 1 and 2; feed hardening item 52; plan leftovers (W4, M6, EX1, S1, DT-small — the owner had declined that session's commit earlier; it was asked to push only to `handoff/plan-leftovers`, never a PR).

## 4. Pending — not started or waiting

1. **Mon 2 Nov 16:15 IST live verdict** (scheduled task paused at handoff, see §8): read the live proof tables (`hot_path_latency_minute`, `host_capacity_minute`, `capacity_verdict_daily`; runbook `docs/runbooks/hot-path-capacity-proof.md`), give the owner per-step typical / slowest 1% / slowest 0.1% / worst in microseconds, minutes with any tick over 1 millisecond, CPU wait, DB backlog, compare with 29 Oct (2–16 millisecond pauses), then an honest 4-core verdict. Change no setting; ask whether the own-core switch stays on.
2. **1 Nov AWS budget move** (scheduled task paused): native `limit_amount` $185 → $75 on or after 1 Nov (Quotes 37/45 in `daily-universe-scope-expansion` rule). Files: `deploy/aws/terraform/budget.tf`, `budget-guards.tf`, `budget_digest` BUDGET_USD, `hard_stop_guard` DEFAULT_BUDGET_KILL_USD → 75 (OCTOBER_2026_KILL_USD stays), update `budget_ceiling_lockstep_guard`, run aws-lambdas tests, PR, merge outside market hours, confirm with describe-budgets.
3. **Muhurat Sun 8 Nov**: code merged (#2154); terraform apply runs on the weekday post-close schedule. Verify it applied before 8 Nov; nothing deploys on 8 Nov. Owner still has to confirm the session times **14:05–15:40 IST** (assumed from press reports, not the NSE circular), whether end-of-day checks run that date, and whether the 14:05–14:20 pre-open goes into candles.
4. **Still over 1 microsecond** (test computer): database row with our writer 0.86–1.19 slowest 1% (switch off); waking a sleeping drain 16–24 microseconds (never-sleeping drain built, off, needs a spare core); live 2–16 millisecond pauses (Monday measures the own-core fix).
5. **Two evening catch-up bugs** found by #2157 (tests kept, ignored in `ALLOWED_IGNORED`): state file has no checksum; a torn file disables the evening drain on every later evening. Proposed fixes in the #2157 body. Waiting for owner OK (recommended: fix both after Monday).
6. **ws_frame_spill limitation** (#2157 defect 4): a torn tail in a non-newest WAL segment gets no gap flag; proposal in #2157 body.

## 5. Important decisions (owner, dated; record any new one in the rule files first)

| Date | Decision (owner's card or words) |
|---|---|
| 30 Oct | "Yes, raise it (Recommended)" — dashboard JS budget 235→301 lines (Rust-only lock §0.15) |
| 30 Oct | "Yes, add the 4 reasons (Recommended)" — xverify_failed reasons coverage_short, coverage_unknown, fallback_universe, worker_never_reported (noise-lock note on #2164's branch) |
| 30 Oct | "Yes, build our own (Recommended)" — own ILP row writer (built, merged, OFF) |
| 30 Oct | "Build it, switch off" — never-sleeping drain built and off |
| 30 Oct | "Yes, fixed counters (Recommended)" / "Write it after the order (Recommended)" — order path |
| 30 Oct | "Yes, Monday live test (Recommended)" — own core ON for Mon 2 Nov only, never-sleeping drain OFF |
| 30 Oct | "until measuring everything with a real time proof we don't decide on 4 cores" |
| 30 Oct | "Pace them (Recommended)" — start waiting jobs only as slots free and the 5-hour limit < 60% |
| 30 Oct | "After Monday (Recommended)" — memory watchdog and heartbeats stay off; trim/allocator, 80% restart, alarm decided after Monday |
| 31 Oct | "always i need less than one microsecond" — every in-memory tick step |

**Open owner decisions**: turn on `[tick_storage] own_ilp_encoder` (recommended Tuesday after Monday's numbers); keep `seal_ilp` = 2500 for the library path while the switch is off; the two evening bugs; memory watchdog options; dependabot #2079 (aws-actions/configure-aws-credentials 4.3.1 → 6.3.0, major); Muhurat questions above.

## 6. Relevant files

- Plans: `.claude/plans/active-plan-feed-hardening.md` (ITEM DT and sub-items DT-S0/S1/SOCK/FOLD/ROWS/S5), `.claude/plans/active-plan-audit-2026-10-20.md`.
- Budgets: `quality/benchmark-budgets.toml` (34 keys after #2143; CLAUDE.md "BENCHMARK BUDGETS" line states the count and is pinned by `crates/common/tests/benchmark_budget_reaches_a_bench_guard.rs`), `crates/core/benches/support/tail_gate.rs`.
- Live proof: `docs/runbooks/hot-path-capacity-proof.md`; tables written by the storage crate (`hot_path_latency_minute`, `host_capacity_minute`, `capacity_verdict_daily`).
- Own-core switches: `deploy/systemd/tickvault.service` (`TICKVAULT_DRAIN_THREAD=on`, `TICKVAULT_BACKGROUND_CORE=0`), `deploy/systemd/tickvault-host-tuning.service` (`TV_QDB_CORE_ISOLATION=1`). To switch off: delete those three lines in an after-close update.
- Own writer switch: `[tick_storage] own_ilp_encoder` (default false).
- Muhurat: `config/base.toml` `[[trading.muhurat_trading_dates]]`, terraform `variables.tf` (pinned equal by a guard).
- Nextest: `.config/nextest.toml` (per-test override for the WAL lifetime proptest).
- Merge flow: `.github/workflows/ci.yml` (`all-green`, `enable-auto-merge`), `postmerge-catchup.yml`, `deploy-aws.yml`, `deploy-aws-after-close.yml`, `terraform-apply.yml`.

## 7. Setup commands

```bash
git clone https://github.com/SJParthi/tickvault && cd tickvault
rustup toolchain install 1.95.0            # rust-version pinned in every crate
cargo build --workspace
cargo test -p tickvault-<crate>            # scoped by default (testing-scope.md); workspace only for crates/common changes
cargo fmt --check && cargo clippy --workspace --no-deps -- -D warnings -W clippy::perf
bash .claude/hooks/banned-pattern-scanner.sh && bash .claude/hooks/pub-fn-test-guard.sh && bash .claude/hooks/plan-gate.sh .
cargo test -p tickvault-common --test benchmark_budget_reaches_a_bench_guard --test bench_budget_elements_guard --test ignored_test_guard
make doctor                                 # health; needs Docker (QuestDB)
```

Notes: commit with `git commit -F <file>` (double-quoted `-m` mangles `§` citations); never `--no-verify`, never force-push, never `cargo update` (banned). GitHub's update-branch API returned HTTP 500 all of 30–31 Oct; merge `origin/main` into a branch locally instead. When resolving conflicts in `quality/benchmark-budgets.toml`, `crates/*/Cargo.toml` `[[bench]]`, `ignored_test_guard.rs`, `ci.yml` DHAT lists or plan files, keep both sides and fix the CLAUDE.md key count (`grep -cE '^[a-z_0-9]+ *=' quality/benchmark-budgets.toml`). `ci.yml` has a shell-line ceiling (293) enforced by `shell_budget_guard`.

## 8. Tests, failures and known CI issues

- Main at `49dd25239`: last PR runs All Green (Verified for #2168's head).
- **Weekend date bug** (fixed by #2146): four `silence_calendar` tests marked "today" a holiday; on a Saturday/Sunday the calendar refuses it. Fixed on main; watch for similar date-dependent tests.
- **Slow WAL proptest** `wal_unapplied_ranges_properties::proptest_wal_lifetimes_never_skip_or_prune_an_unapplied_frame` (~205–326 s locally): now has a 600 s per-test limit (#2168); whether the override actually kicks in on a slow runner is not yet observed.
- CI runner flakes seen 30 Oct: linker "Bus error" on Test (app) (re-run passed); treat as runner, not code, only after one re-run.
- Ignored tests to un-ignore when fixed: the two evening-drain defect tests (#2157).
- Coverage measured 30 Oct (Reported): app 81.31, core 93.71, storage 92.587, trading 97.11, api 98.66, common 99.47, lambdas 86.36, logs-mcp 92.29.

## 9. Running jobs at handoff

- **Scheduled tasks (all paused at handoff, re-enable or redo in your own tooling)**: `trig_01UBdPyTCUrevq3aKiJRswKi` Sunday 1 Nov 12:30 UTC deploy check; `trig_01NvggPm2b3tsBYP8Xim3Gez` 1 Nov 04:00 UTC $75 budget PR; `trig_015QXFhheXKxvJX6ouJ5159c` Mon 2 Nov 10:45 UTC live verdict.
- **Cloud sessions**: 16 non-archived Claude sessions were told on 31 Oct ~06:35 UTC to stop at a safe checkpoint, commit + push, and not merge, deploy or open PRs. No session is merging anything.
- **Repo automation**: `postmerge-catchup.yml` (every 30 min) and the after-close deploy/terraform crons keep running on GitHub; they deploy main outside market hours.
- **Live server**: r8g.large (2 vCPU), starts 08:30 IST, stops 16:30 IST weekdays; Monday 2 Nov runs the own-core placement.

## 10. Session checkpoint replies

Replies from the worker sessions, received 31 Oct 06:35–06:36 UTC (Reported by each session; branch heads Verified on `origin` where marked). All sessions have now replied; the last (plan leftovers) pushed `handoff/plan-leftovers` at 07:33 UTC. Every worker session is stopped and will not push again.

### #2149 depth row path — branch `perf/sub-us-depth`, head `0e1f0f60f` (draft)
- **CI**: All Green on `0e1f0f6`, every job green; Enable Auto-Merge skipped only because the PR is a draft.
- **Done**:
  - Depth ILP names are pre-checked (`ColumnName`/`TableName::new_unchecked`, with a `debug_assert` on QuestDB's own check). Bytes are unchanged, proven by a proptest and an edge test.
  - `feed_drain` per level: depth20 531 → 366 nanoseconds, depth200 638 → 437 nanoseconds.
  - New bench `crates/storage/benches/depth_row_append.rs` with a p99 gate: median about 390 nanoseconds, p99 671–866 nanoseconds.
  - Budgets: `feed_drain_depth` 1500 → 1000, new `depth_row_append` = 1000. CLAUDE.md key count 30 → 32.
  - Plan line DT-DEPTH added.
- **Pending**: merge main in (main moved), mark ready.
- **Notes**:
  - Two source-scan guards cut `depth_persistence.rs` at the first `#[cfg(test)]`; never put that string above production code.
  - The app bench needs `CARGO_BUILD_JOBS=1` on a 15 GB box.
  - `parser/depth.rs` is unchanged.

### #2152 speed and health panel — branch `claude/speed-health-dashboard`, head `2ec6b8f0f` (draft)
- **Done**:
  - Bearer-gated `GET /api/debug/speed-health` plus a dashboard panel. Script 275 lines / 10,953 bytes; the guard ceilings were lowered to match.
  - `ingest_shed.rs` publish stamp.
  - Five hostile review rounds; the last two were clean.
  - Local: api lib 428 passed.
- **Failing**: CI run 41099003362, Coverage & Perf: `api 98.55% (threshold 98.6%)`.
- **Fix**: add api tests for the uncovered branches in `speed_health.rs`:
  - a metrics body over 16 MiB;
  - a non-200 response;
  - `metrics_scrape_url` unset;
  - `read_minute_table` with no designated column.
  - Never lower the floor.
- **Then**: All Green, mark ready.

### #2161 app coverage — branch `claude/app-coverage`, head `09bfaea6b` (draft)
- **Done**:
  - 16 new `crates/app/tests/*` files.
  - App coverage 81.31% → 86.29% locally (3,757 tests passed).
  - One hostile round; every finding fixed.
- **Failing on `09bfaea`**, CI run 41101268515:
  - (1) DHAT runner out of disk before any test ran: re-run once.
  - (2) Coverage & Perf `-p tickvault-core --lib` failed. This is not this diff; read job 124235413500's full log for the test name.
- **Pending**:
  - Raise the app floor (72.6) only after a green CI coverage figure.
  - Second refuter round.
  - Fill in the PR body.
- **Notes**:
  - Run `cargo clean --profile dev` before coverage, and use `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.
  - `dhat_idle_seed_step.rs::test_depth_row_buffer_presize_holds_inline_book_rows_without_regrowing` fails under llvm-cov locally (its name lacks `dhat_`); it passed in CI.

### #2162 O(1) per-event sweep — branch `claude/o1-per-event-sweep`, head `1d37c7e` (draft)
- **Done**:
  - `seal_absorption` once-per-episode failure logs.
  - The seal runner keeps its rescue buffer across cycles.
  - The coalescer builds an alert body only when it keeps it.
  - The middleware borrows the request path.
  - 9 CLAUDE.md O(1) rows; plan item O1S.
  - Three review rounds; the 2-clean-rounds bar is not met.
- **Failing**: Coverage & Perf (job 124225167770), `-p tickvault-core --lib` under llvm-cov. Plain Test (core) passed on the same head.
- **Next**: find "FAILED" in that log, check whether the test touches `coalescer.rs` or `order_update_connection.rs`, and re-run once if it is timing-sensitive. The session has unsubscribed, so the next engineer owns it.
- **Note**: the same `-p tickvault-core --lib` coverage failure also hit #2161. It may be one shared core test failing under instrumentation. Check both logs together.

### #2164 cross-verify 51e/51f/51g — branch `claude/xverify-51efg`, head `57de60ae7` (draft)
- **Done**:
  - Per-IST-day target record, coverage hold, past-day catch-up, and a 16:24 supervisor.
  - Rule §12.15.13–15, runbook §6, CLAUDE.md row, plan ticks.
  - The noise-lock note (four `xverify_failed` reasons) is commit 6122eacbc.
  - Review rounds 1–3 fixed. Round 4 is not valid: its refuters died on the usage limit.
- **Failing (caused by this PR, small fixes)**:
  1. `error_level_meta_guard.rs:427`: 4 new `warn!` sites pairing "write" with a failure need `REVIEWED_WARN_SITES` entries, with the reason that the failure is not a loss:
     - `dhan_live_crossverify_boot.rs` :1965, :2286, :2975;
     - `dhan_live_universe.rs:961`.
     - Do not make them `error!` without a noise-lock row.
  2. `per_entity_ceiling_lockstep_guard.rs:302`: write `DAY_RECORD_MAX_INSTRUMENTS` as `= MAX_DAILY_UNIVERSE_SIZE`.
  3. Coverage & Perf: re-check after 1 and 2.
- **Pending**: two clean hostile rounds, then mark ready.
- **Owner items** (in the PR body): 51i/51j alarms need dated noise-lock sections; a proposal to record the widen's actually-placed set.

### Core coverage (no PR) — branch `claude/core-coverage`, head `c65ac9f0d`
- **Done**:
  - Baseline: core 93.71% (49,960 / 53,311).
  - Two test files: `pool_supervisor/cov_part1_tests.rs` and `order_update_connection_run_loop_cov_tests.rs`; 66 passed. Not reviewed yet.
- **Pending**:
  - pool_supervisor part 2, plus about 30 more files.
  - Owned files covered only through new `crates/core/tests/cov_*.rs`.
  - Re-measure, raise the floor (91.6), draft PR.

### Storage coverage (no PR, nothing preserved)
- **State**: no branch. The unpushed WIP tests were lost when the container reset on 30 Oct with a full disk.
- **Baseline**: storage 92.587% (59,101 / 63,833).
- **Top uncovered files**:
  - tick_spill_replay 285
  - partition_archive 245
  - wal_suspension_watcher 208
  - seal_spill 195
  - seal_writer_task 182
  - contract_name_repair 143
  - partition_manager 137
  - raw_frame_upload 119
  - s3_cold 116
  - shadow_persistence 113
  - audit_spill 103
  - seal_writer_runner 98
  - seal_dlq 90
  - fresh_start_reset 89
  - depth_spill_frame 86
- **Pending**:
  - Write every test, as new `crates/storage/tests/cov_<stem>.rs` files.
  - Raise the floor (90.1) and update the two 90.1 boundary tests in `coverage_gate_guard.rs`.

### Trading / api / common coverage (no PR) — branch `claude/coverage-trading-api-common`, head `f67a7912a`
- **Done**:
  - `crates/api/tests/coverage_gaps_api.rs` (7 passed)
  - `crates/common/tests/coverage_gaps_common.rs` (17 passed)
  - `crates/trading/tests/coverage_gaps_brutex_host.rs` (31 passed)
  - Not reviewed yet.
- **Baseline**: common 99.47, api 98.66, trading 97.11.
- **Pending**:
  - oms/api_client (~192 lines), oms/engine (~148), other oms (~120), risk/in_mem/orphan watchdog (~75), strategy/indicator through tests only (~58).
  - Reviews, re-measure, floor raise, PR.
- **Note**: 5 active plans are at the plan-gate cap, so tests go only in new `tests/` files.

### Audit-fix triage part 1 (no PR) — branch `claude/audit-plan-first34`, head `e9c5082`
- **Done**:
  - All 34 items checked; plan notes for 22 (commit ad1f3fd).
  - PR13 and PR24 ticked done with evidence.
- **Partial (NOT compiled, tested or reviewed)**:
  - storage: PR16 boot_drain off_worker, PR31c, PR32 (half-finished).
  - app: PR5, D3c, D11 Muhurat session_day, PR19.
  - **Run `cargo check -p tickvault-storage -p tickvault-app` first.**
- **Pending code**:
  - D1 rate_limiter comment
  - D4 counter
  - PR19 debug.rs
  - logs-mcp grep path refusal
  - PR27 publisher timeout
  - PR26 `maximum_retry_attempts=0` on both minters
  - PR25 delete `max_position_lots` and the `[cross_verify]` stub
  - PR9 fuzz target
- **Then**: notes, review, gates, PR.

### #2144 feed hardening items 2 and 3b — branch `claude/feed-hardening-items-2-3`, head `38898defa` (draft)
- **CI**: run 41092332796 on `38898de` succeeded, All Green included.
- **Done**:
  - Item 2: `host_limits` also checks `vm.max_map_count` and the soft open-file limit (warn only); `host_limits_lockstep_guard` pins both.
  - Item 3b: new `crates/app/src/subscribe_reconcile.rs`.
    - Re-subscribes main-feed instruments still silent 120 s after subscribe, in place, never by a redial.
    - At most 3 rounds per instrument per day, 120 s spacing.
    - Switch `[dhan_universe] subscribe_reconcile_auto_resubscribe`, default false (count only).
  - Four review rounds (10 defects fixed); the last two clean.
  - 57 tests passed.
- **Partial**: not wired into the drain. `dhan_feed_stack.rs` is owned by another session; the PR body's "Wiring proposal" lists the exact call sites.
- **Pending**: mark ready, then tick the plan boxes after merge.
- **Needs the owner**:
  - make the boot refuse a limit mismatch;
  - turn on auto-resubscribe after one shadow session;
  - a page for instruments still silent after 3 rounds (needs a noise-lock row).

### Audit-fix triage part 2 (no PR) — branch `claude/audit-plan-tail`, head `b471896fe`
- **Done**:
  - All 34 tail items of `active-plan-audit-fixes-2026-09-26.md` checked, each with a dated note.
  - Ticked done: DA-13, DA-13b, DA-13c, R3-13. Ticked superseded: PR52.
  - PR44 and PR48 left unticked: inert while depth and console wipes are off.
- **Partial (unreviewed WIP, 8 files)**:
  - `aws-control.yml`: market-hours guard and restart-questdb gating.
  - `emergency-fs-recover.yml`: re-enable the app on exit.
  - terraform `main.tf`: scheduler PassRole scope.
  - common guards: `aws_infra_wiring.rs`, `github_workflow_guard.rs`.
  - core: `token_cache.rs` 0o600 create_new, the `order_update_connection.rs` timeouts, an `instance_lock.rs` comment.
  - Only `cargo check -p tickvault-core -p tickvault-common --tests` was run; no tests, clippy, fmt or gates.
- **Pending (approved, not started)**:
  - PR33 depth_seed NSE-only index
  - PR34 and PR19 infra timeouts
  - PR50 calendar coverage per year
  - PR37 doc
  - PR39 literal error-code guard
  - row 110 seal stall throttle
  - PR38 OMS expiry vs IST date
  - DA-17/PR36 qdb_console_proxy repeated param (its plan note already names this fix, so land it or correct the note)

### Lambdas + logs-mcp coverage (no PR) — branch `claude/lambdas-mcp-coverage`, head `c4bde5b19`
- **Done**:
  - `crates/aws-lambdas/src/test_support.rs` (cfg(test) only): a loopback AWS endpoint that drives the real AWS SDK, plus env guards.
  - 44 new aws-lambdas tests, all passing at c92447a: minter, start_watchdog, hard_stop_guard and budget_spend paths. No PIN or token appears in any error.
  - 15 new logs-mcp tests; logs-mcp coverage 89.27 → 92.29%.
  - `.untested-pubfn-baseline` 311 → 309.
- **Partial**: the CloudWatch rpcv2Cbor extension in c4bde5b is committed but **not yet compiled**. Run `cargo test -p tickvault-aws-lambdas --lib` first.
- **Pending**:
  - alarm_gate, market_hours_gate and budget_killswitch `handle()` tests.
  - An October $185 / November $75 clamp boundary test.
  - Re-measure (aws-lambdas baseline 86.36%) and raise the floors (aws-lambdas 81.1, logs-mcp 87.3).
  - Review, draft PR.
- **Note**: the secret scanner rejects 10-digit client-id literals in tests.

### #2159 adversarial input coverage — branch `claude/adversarial-input-coverage`, head `fd6ff357f` (draft, Verified on origin)
- **CI**: red on `968fd60`; CI on `fd6ff357f` was still queued at handoff. All Green is red.
  - Test (common): `assertion_free_test_ratchet::assertion_free_tests_may_only_shrink` fails on the new assertion-free test `crates/trading/tests/adversarial_strategy_recipe_properties.rs::prop_recipe_with_one_byte_removed` (it only calls `parse_recipe`). Fix: assert the result is `Ok(same recipe)` or a typed refusal variant.
  - Coverage & Perf failed on the same head; log not read (likely the same test).
- **Done** (each file run locally, green; clippy `-D warnings` clean):
  - New property test files: core `adversarial_feed_frames_properties.rs` (14), `adversarial_order_update_properties.rs` (15), `adversarial_master_csv_properties.rs` (18); common `adversarial_config_toml_properties.rs` (7), `adversarial_segment_query_properties.rs` (3); storage `adversarial_spill_record_properties.rs` (10), `adversarial_wal_segment_properties.rs` (5), `adversarial_applied_watermark_properties.rs` (5); trading `adversarial_strategy_recipe_properties.rs` (18); app `adversarial_intraday_properties.rs` (6).
  - New fuzz targets (in `fuzz/Cargo.toml` and the `fuzz.yml` matrix), each run 60 s locally with 0 crashes: `main_feed_stacked_frame` (305,771 runs), `order_update_json` (3,165,233), `instrument_master_csv` (1,069,528), `seal_spill_record` (5,841,519), `strategy_recipe_files` (840,570), `segment_query` (27,725,308). `dhan_intraday_json` builds but has NOT been run. `fuzz/Cargo.toml` adds `tickvault-trading`, `tickvault-app` (path) and `serde_json =1.0.154`.
  - Bug fixed: `crates/core/src/instrument/master_csv.rs` `parse_lot_size("75.000000000000001")` returned 75; `decimal_text_is_whole` now reads the text exactly (regression test, exact-reference proptest, fuzz oracle).
  - Hostile review round 1 (3 reviewers); its MEDIUM/LOW findings in these files fixed.
- **Pending**:
  - Fix the ratchet failure above, re-check Coverage & Perf, drive All Green, mark ready.
  - Run `dhan_intraday_json` for 60 s; update the PR body table (it predates the intraday and watermark files).
  - Hostile review round 2 (two clean rounds were requested).
  - LOW gaps: DLQ NDJSON and audit spill readers, TrueData touchline/timestamp parsers, other boot state files (`boot-committed.summary`, `evening-drain.state`, depth seed, backup history).
- **Needs the owner**:
  - Real bug: an order-update `Quantity` of `100.00000000000001` decodes as `Ok(100)` (serde_json without `float_roundtrip`). Pinned by `test_known_limit_fractional_json_number_on_int_field_reads_as_integer` (asserts today's behaviour; fails once fixed). Fix needs either an edit to `crates/core/src/parser/order_update.rs` (refuse an integer field whose raw text has a non-zero fraction digit) or enabling serde_json `float_roundtrip` (a dependency feature change).
  - Optional: `parse_strike_paise` rounds sub-paisa text (`"0.005"` becomes 1 paisa); refuse or round?
- **Notes**: the container disk fills fast (`cargo clean` freed 22 GB; builds used `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`). Running targets needs cargo-fuzz and `nightly-2026-03-15`. The ignored-test guard forbids new `#[ignore]`; the assertion-free ratchet forbids tests with no assertion.
- **CI update (07:27 UTC, head `fd6ff357f`, CI complete)**: three checks red, two causes, both in this PR. (1) Test (common) and Coverage & Perf: the only failing target is `-p tickvault-common --test assertion_free_test_ratchet`, which flags `prop_recipe_with_one_byte_removed`; adding an assertion clears both. Every other suite passed. (2) Commit Lint: the handoff commit's type `wip` is not allowed; reword it, or merge with a proper squash title. All Green is red only because of these.

### Plan leftovers W4/M6/EX1/S1/DT-small (no PR) — branch `handoff/plan-leftovers`, head `2a4a91610` (Verified on origin, 07:40 UTC)
- **Commits**: `cedab1a3c` W4 test fixes (also on `claude/audit-five-items`; its type `wip` would fail Commit Lint in a PR), `3a5938254` W4 review fix, `3e7c98e6b` test pinning `take_forgot_untrusted`, `2a4a91610` handoff note `.claude/plans/handoff-plan-leftovers-2026-10-31.md` (draft PR body and the M6 proposal).
- **Done**:
  - W4 is built, with its review fixes. The HIGH finding is fixed: a table the lag tracker forgets while untrusted is no longer read as a clean probe.
  - Storage lib: 2,260 passed / 0 failed before the last commit; after it only the new test was re-run (1 passed). The 6 related storage integration tests pass; clippy is clean on the storage lib.
  - EX1, S1 and DT-small were already on main (#2134, #2135) and are ticked in the plan. S1's 2 core tests pass.
- **Partial**: the app-crate tests for EX1, S1 and DT-small were not re-run locally (disk full), so CI must run them. W4's second review round never ran. The live measurement W4 asks for was not done (no AWS access).
- **Pending**: M6 is not built (`ws_frame_spill.rs` belonged to another session). The proposal is in `.claude/plans/handoff-plan-leftovers-2026-10-31.md` on that branch: a settable frame cap in `ws_frame_spill.rs`, `evening_drain::frame_cap_for` / `peek_frame_cap`, and a `main.rs` call before STAGE-C.
- **Known**: no failing test reported. `clippy --all-targets` fails only in older untouched test files (`partition_archive`, `seal_dlq`, `seal_writer_task`, `spill_ceiling_is_free_space_based_guard`).

### #2158 test-target clippy (12b) — branch `claude/clippy-all-targets`, head `2b6279bac` (draft)
- **CI**: All Green passed on `2b6279bac`, but the branch is now behind main.
- **Done**:
  - A new Build & Verify step runs `cargo clippy -p tickvault-common -p tickvault-storage -p tickvault-api -p tickvault-aws-lambdas -p tickvault-logs-mcp --all-targets --no-deps -- -D warnings`.
  - 234 of 276 test-code findings fixed; production code untouched.
  - Guards: `rust_only_guard::ci_clippy_gate_stays_armed` and `clippy_local_parity_guard`; `scripts/clippy-local.sh` (`make clippy-ci`).
  - Five review rounds; the last two were clean.
- **Partial**: app, core and trading are not gated yet. Their 42 findings are all in owned files; the PR body lists them per file.
- **Pending**:
  - Merge main in.
  - Re-run the gate command plus `clippy_local_parity_guard`, `rust_only_guard`, `shell_budget_guard` and `source_size_ratchet_guard`.
  - Push, wait for All Green, mark ready.
  - Follow-up: fix the 42 findings, then add the three crates to the step and to `CLIPPY_ALL_TARGETS_CRATES`.
- **Notes**:
  - Once this is on main, storage test code is under `-D warnings`, so a merge of main can turn Build & Verify red.
  - `pool_supervisor.rs` is at 26,182 lines against its pin of 26,184.
  - `ci.yml` is at its 293-line shell ceiling.

### Feed hardening 52 (no PR) — branch `claude/fl52-measurements`, head `ac5e44019`
- **Done**: 52b is already on main (#2057); only its plan tick remains.
- **Partial, UNVERIFIED**:
  - 52j phase A: `/proc/net/netstat` TcpExt counters plus the guard `kernel_tcp_ext_local_only_guard.rs`. `cargo clippy -p tickvault-app --all-targets -D warnings` reports 122 errors; re-check first.
  - 52c: DHAT reader-memory harness, unfinished.
  - 52f: new ErrorCode HOT-PATH-04 (log-only) plus moving the telemetry block after subscriber init; confirm the move, and write `hot_path_telemetry_boot_wiring_guard.rs`.
- **Pending**: 52o, a default-off NDJSON WAL replay log (design in the session's lost workflow output).
- **Needs the owner**:
  - 52k/l/m/n cards;
  - 52j phase B (unsafe allowlist);
  - a TLS test key;
  - the watchdog-beacon spawn failure tagged BOOT-02.
