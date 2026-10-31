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

(Appended as replies arrive.)
