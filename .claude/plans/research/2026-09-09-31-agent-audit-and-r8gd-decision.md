# 31-agent audit of main + PR #1901 + `claude/integration`, and the r8gd.2xlarge question (2026-09-09)

> **Status labels:** Verified = file:line or a command result seen by an agent; Assumed = derived, not observed; Unknown = could not be checked from this container.
> **Scope:** `main` @ `f8a6cf5f3`, PR #1901 head `b89f2720e` (`claude/pensive-heisenberg-u2by0j`), `origin/claude/integration` @ `1e0c2d7ee`.
> **Method:** 31 read-only agents (rust-only, O(1) table, hot path, security, storage/dedup, depth ranking, futures-ATM lock, 5 s cadence blockers, ghost/804, WAL chain, disk/retention, AWS pricing, observability, build verify, test types, audit codes, config drift, CLAUDE.md staleness, races, composite key, order side, REST/cross-verify, requirements ledger, lambdas/automation, permutations, API/frontend, candles, simplifier, deps). 17 ran to completion in round 1; a session rate-limit killed 14 mid-run and they were re-run with tighter scopes. Nothing in the repo was edited by any agent.

## 1. Executive summary

| Area | Status | Risk | Notes (plain English) |
|---|---|---|---|
| Rust-only, except frontend | ✅ | Low | Both guards green (27/27, 12/12). Zero non-Rust executables tracked; all 104 shebangs are bash; all 13 Lambdas are Rust on `provided.al2023`. Four known evasion shapes stay open by design (printf/base64 assembly, `.gitattributes` filters, proc-macro spawning at compile time, Makefile `$(shell node …)`). |
| Hot path O(1) / zero-alloc | ✅ | Low | Per-packet path clean of every banned pattern. One MEDIUM gap: the depth-frame walk (`drain_depth_frame`) has no DHAT gate of its own; parse and append are gated separately. |
| Storage uniqueness / dedup | ✅ | Low | Every per-instrument DEDUP key carries security_id + segment + feed; `market_depth` carries `depth_kind` + `capture_seq`; `top_volume_rank` keyed on `(ts, tf, family, feed, security_id, segment)` so 1s/5s and index/stock cannot collide. Guard 9/9. |
| Depth ranking pipeline (2026-09-06/07/08 locks) | ✅ | Low | 18 of 19 locked properties Verified in code with named tests. The 19th (`rebaseline_all`) was replaced by a safer mechanism; the rule text is stale. |
| Futures-as-primary ATM, frozen window (2026-09-09 lock) | ❌ | High | **Recorded-only. Zero code.** Spot-only centring; the 09:30 top-up still runs and conflicts with "frozen"; no leave-window counter. |
| 5-second apply cadence (asked 4×) | ⚠️ | High | Deliberately not shipped. Blocker #1 is real and session-ending: Dhan 804 → Fatal park → no respawn, and the ghost redial cannot reach a parked socket. Blocker #3 (`send_swap` pending) is already fixed. |
| `claude/integration` branch | ❌ | High | **Does not compile** (trading crate E0425 ×3, duplicate `use`, duplicate `fn`). Re-adds 5 orphan Groww/Python/fast-boot files and `prometheus.yml`; deletes truedata §11; changes a fire-delay constant without a dated quote. Rust-only guard fails 2/27 on that tree. |
| PR #1901 | ⚠️ | Medium | CI All Green on the exact head, but a **duplicated `#[test]` orphans an existing test** (`pre_register_contract_underlying_counters_never_panics_without_a_recorder` no longer runs). One-line fix. |
| Order/risk/OMS lockouts | ✅ | Low | All seven locks intact; `dry_run: true` hardcoded; only live setter is `#[cfg(test)]`; OrderBudget + GCRA wired on both placement paths. |
| WAL / spill / DLQ durable floor | ✅ | Medium | Replay watermark IS implemented (`wal_applied_watermark.rs`, #1864) but not yet live-measured. Three loss paths remain (refold with `lost>0` still confirms; active-dir byte prune can delete unreplayed segments; queue bounded by count not bytes). |
| Disk / retention | ⚠️ | High | Burn ~307 GB/session on a 600 GB volume; runway trigger ships INERT (`SESSION_BURN_BYTES_DEFAULT = 0`); shed thresholds still fractional. Depth partitions ARE reclaimable intra-day now (`depth_hot_hours = 4`). |
| Memory sizing | ⚠️ | Medium | `MemoryHigh=20G` + QuestDB 12 GiB + shm 0.5 + sidecars 0.64 = **33.1 GiB on a 31.3 GiB host** — over-committed by hand-sized constants. |
| Observability | ⚠️ | Medium | 88 alarms, 110 EMF names, 12 gated. **31 new metric names since 2026-09-05 reach no CloudWatch surface and have no dated rule row.** One rule-file drift: `tv_tick_spill_replayed_bytes_total` claimed shipped, not in the selector. No browser surface shows what the depth pools hold or the ranking board. |
| Audit trail (QuestDB rows) | ⚠️ | Medium | Depth swaps, ghost redials, depth-seed decisions and contract refusals have counters/logs but **no audit table rows**. Rankings do (`top_volume_rank`). |
| Test types | ⚠️ | Medium | 11 of the 12 newest modules are **outside mutation-testing scope** (`mutation.yml` lists only core/trading/common). Zero proptest in any of them. Four loom models exist, none cover the new concurrency seams. |
| Concurrency | ✅ | Low | The 2e681b9 drain-spin fix is closed. One MEDIUM: writer join at shutdown blocks a tokio worker up to 12 s with no poison flag. |
| Composite key (I-P1-11) | ✅ | Low | Every new module composite-keyed and test-pinned. One latent encoding divergence: `depth20_track.rs` uses `segment as u8` where 13 sibling sites use `binary_code()` (BSE_FNO 7 vs 8) — harmless today because depth is NSE-only. |
| Automation (no human input) | ⚠️ | Medium | 13/13 Lambdas have Errors + not-invoked alarms; lifecycle, deploy, catch-up, recovery all automated. Ten human-only steps remain by design (EIP release, github-token, TOTP rotation, setIP, IAM, limit_amount, AZ change, re-enable after budget stop, volume shrink, dated quotes). |
| CLAUDE.md accuracy | ⚠️ | Medium | 17 of 24 dependency versions wrong (redis is a phantom; totp-rs is 6.0.0), hooks are 40 not 18, `active-plan.md` does not exist, API has 15 routes not 12. **Blocking-direction stale:** operator-charter rule 13 still says "only 2 WebSocket connections EVER, never depth-20/200" with no correction marker. |
| Dependencies | ✅ | Low | No banned specifiers; 527 packages; no embedded interpreters; native build systems exactly cc/cmake/pkg-config. `cargo audit`/`deny` not installed in this container — CI's Security & Audit job is the evidence. |

## 2. Branch and PR state

| Item | Fact | Evidence |
|---|---|---|
| Designated branch | `claude/agent-activation-infra-review-23r6io` at `f8a6cf5` (== main). Only this file is added. | `git status` |
| PR #1901 | head `b89f2720e`, draft, `mergeable_state = clean`, All Green **success** on the exact head (22 checks). +170 lines: `contract_underlying_map.rs` (+161), `dhan_contract_universe.rs` (+9). | GitHub API `check-runs` on the head SHA |
| PR #1901 defect | `#[test]` duplicated at `contract_underlying_map.rs:1131-1132`; the next-following test `pre_register_contract_underlying_counters_never_panics_without_a_recorder` (branch line 1225) lost its attribute and no longer runs. rustc only warns on duplicated attributes; CI clippy is lib-only, so nothing caught it. Net tests +6, not the +7 the PR body claims. | Verified by direct `git show` on the branch |
| PR #1901 secondary | `REFUSED_COUNTER` doc says "legs refused while building a snapshot" but now also receives artifact-scan refusals under the same labels; the count runs inside the attach retry loop so it multiplies per attempt; 113k `increment(1)` calls with a non-literal label on the attach path (boot, not per-tick). | B02 |
| `claude/integration` | merge-base = main; tree diff 47 files / +22,012. **Trading crate fails to compile** (`oms/api_client.rs:131,1159,1404` E0425; `trading_pipeline.rs` duplicate `use` E0252; `oms/types.rs:3574/3965` duplicate fn E0428). Five orphan `.rs` files not declared in any `lib.rs`/`mod.rs` (groww_bridge 6805, groww_sidecar_supervisor 4190 — spawns python3/pip/venv, groww_spot_1m_boot 4701, fast_boot_validation 577, feed_gap_audit_persistence 558) + 2 orphan tests; `deploy/docker/prometheus/prometheus.yml` re-added (CloudWatch-only lock); truedata §11 deleted (annotate-never-delete); `SPOT_1M_REST_FIRE_DELAY_MS` 300→5 with no dated quote. `rust_only_guard` 25/2 FAIL, browser guard 10/2 FAIL on that tree. | A01, A03 |
| Salvageable from integration | `b89f2720e` (= PR #1901), the day-OHLC `should_route_day_ohlc_tick` extraction, `config/production.toml` restatement, `.claude/settings.json` hook, `pre-pr-gate.sh` matrix gate. Decide `ssm-params-prod.tf` separately. | A01 |

**Recommendation:** do not merge `claude/integration`. Cherry-pick the five salvageable hunks onto a fresh branch. Fix PR #1901's duplicated attribute (delete one `#[test]` at line 1131 and restore `#[test]` above `fn pre_register_contract_underlying_counters_never_panics_without_a_recorder`), re-run `cargo test -p tickvault-app --lib -- --list | grep contract_underlying_map | wc -l`, and correct the "+7 / 2122" claim in the body.

## 3. r8gd.2xlarge (local NVMe) versus bigger EBS — the comparison

Prices from the official AWS Price List API for ap-south-1 (EffectiveDate 2026-09-01, fetched 2026-09-09). Instance-store facts from the AWS EC2 User Guide.

| | **A. Today: r8g.xlarge + 600 GB gp3 (6000 IOPS / 500 MiB/s)** | **B. r8gd.2xlarge + small root gp3, data on NVMe** | **C. r8g.2xlarge + same EBS (RAM-only upgrade)** |
|---|---|---|---|
| vCPU / RAM | 4 / 32 GiB | 8 / 64 GiB | 8 / 64 GiB |
| Local NVMe | none | 1 × 475 GB, hardware AES-256 | none |
| Compute $/hr | 0.16516 | 0.38016 (2.3×) | 0.33032 |
| Compute / month (198 h) | $32.70 | $75.27 | $65.40 |
| EBS / month | **$88.92** (54.72 storage + 17.10 IOPS + 17.10 throughput) | $9.12 (100 GB baseline) | $88.92 |
| EIP | $3.60 | $3.60 | $3.60 |
| **Pre-GST total** | **$125.22** | **$87.99** | $157.92 |
| Incl. 18% GST | $147.76 | $103.83 | $186.35 |
| Delta vs today | — | **−$37.23/mo (−30%)** | +$32.70/mo |
| Fits the $135 automatic-stop line? | forecast $142.24 — **NO** (over by $7) | yes, ~$47 under | **NO** |
| Data survives the 17:30 daily stop? | yes | **NO — instance store is erased on every stop, hibernate, terminate and disk failure** (survives reboot only) | yes |
| One ~307 GB session fits? | yes (600 GB, ~293 GB spare) | yes (475 GB, ~168 GB spare) — but only for a day that starts empty | yes |
| Write throughput | 500 MiB/s provisioned (measured peak 107) | NVMe local, no provisioning, no throttling by cost | same as A |
| What must persist across the nightly stop (Verified list) | everything lives on the root EBS | QuestDB volume (SEBI tables + unarchived partitions), `data/instrument-cache` (mapping artifacts, depth seed), `data/sebi-preserve`, `data/recover`, `data/ws_wal` (active + archive), `data/spill`, `data/dlq`, `data/state` — **none of it can live on the NVMe** | same as A |
| Retention floor vs ephemeral disk | n/a | depth/ticks floor is 1 day and candles 2 days, so **at least one full session is always unarchived at 17:30**; a wipe at stop loses it unless verified S3 upload completes before the stop, which Quotes 19/20 record it does not | n/a |
| Code that must change | none | `variables.tf` validation (explicitly rejects r8gd), `instance_type_lock_guard.rs` ×6, `downsize-instance.yml` TO_TYPE + EXPECTED_EIP (recreate ⇒ new EIP), `aws-upgrade-instance.sh` allowlist, user-data has **zero** NVMe mkfs/mount/fstab lines, QuestDB docker volume path, `data/` paths, `MemoryHigh`, `QDB_MEM_LIMIT`, `AllowedCPUs`/`worker_threads`, QuestDB cpuset/cpus | `MemoryHigh`, `QDB_MEM_LIMIT`, cpuset, worker_threads, the type pins |
| Operator rules touched | none | Quote 13 (r8gd REJECTED for exactly this reason), Quote 15 (r8g.xlarge FINALISED), §7 Mechanical Rule 1 (fresh dated quote + 7 lockstep sites) | Quote 15 + Rule 1 |
| Reversible? | grow-only volume | new instance = new EIP + Dhan setIP re-whitelist before live orders | type flip, keeps volume |

### Plain-English verdict

Sir, imagine the juice shop rents a fridge. Today's fridge (EBS) is expensive to rent every month, but everything inside survives the night. The r8gd.2xlarge is a bigger shop with a free, much faster fridge built into the wall — but that fridge is **emptied every evening at 17:30 when the shop closes**, and this shop closes every single day. Everything that must be there tomorrow morning (the SEBI record books, yesterday's unarchived data, the replay log, the instrument map, the depth seed) would still need the rented fridge, so you would be paying for **both**, and any day the S3 archival fails to finish before 17:30 is a day of data lost forever. That is the exact failure this repository already suffered on 2026-09-04 with a full disk.

| Question | Answer |
|---|---|
| Is r8gd.2xlarge cheaper? | Yes, about $37/month cheaper on the list-price arithmetic, IF the small root + NVMe layout is used. |
| Does it solve disk pressure? | Only for the current-day hot set. It does **not** remove the need for EBS; it adds a second tier. |
| Does it add anything the operator asked for? | 64 GiB RAM and 8 vCPU (retires the 33.1 GiB over-commit finding) and unthrottled local write bandwidth. |
| What does it cost beyond money? | An instance recreate (new EIP, Dhan setIP re-whitelist ≥7 days before live orders), a tiered-storage design that does not exist yet (zero NVMe lines in user-data), ~11 hardcoded sizing sites, and a rule-file reversal of Quote 13's explicit r8gd rejection. |
| Recommendation | **Not now.** The measured driver is depth at ~80% of ~307 GB/session; neither option changes that. If RAM/CPU headroom is wanted, **C (r8g.2xlarge)** is a pure type flip with no storage redesign but breaches $135. If cost is the goal, the levers already on record (Quote 17 IOPS/throughput revert saves $34.20/mo on 19%/21% measured utilisation; EIP release $3.60) are cheaper than a tiered-storage rebuild. Revisit r8gd only after same-day verified S3 archival is proven to complete before 17:30 for five consecutive sessions. |

## 4. Requirements delivered vs recorded (strict: a rule-file row is not delivery)

| Requirement | Status | Evidence |
|---|---|---|
| Futures PRIMARY for ATM ±25, window frozen all day (2026-09-09) | **RECORDED-ONLY** | only commit `8e3789108 docs:`; `dhan_contract_universe.rs:1226-1234` spot-only; drain binding `dhan_feed_stack.rs:6248` still `IdxI|NseEquity|BseEquity`; 09:30 top-up live (`CONTRACT_TOPUP_CUTOFF_IST_SECS`) |
| Depth write-volume decision (Quote 21 follow-up) | **RECORDED-ONLY** | no sampling/cap constant |
| Budget lever / EIP release (Quote 10, §2.3n) | **RECORDED-ONLY** | `enable_eip = true`; Sept forecast $142.24 vs $135 action line |
| No-in-session-deploy RULE | PARTIAL | workflow guard `deploy-aws.yml:17-44` (overridden 2× on 09-03); no rule file |
| Same-day S3 archival of depth (2026-08-15 SECOND) | PARTIAL | hour partitions + `depth_hot_hours = 4`; a 4-hour floor, not same-hour verify-then-drop |
| Per-minute additive re-fit vs the new freeze | PARTIAL | re-fit exists; freeze directive unreconciled |
| 5-second APPLY cadence | EXPLICITLY DEFERRED | `REBALANCE_OFFSET_SECS = 8`, once/minute; blocker = 804 park |
| Ranked boot dial before first ranking | EXPLICITLY DEFERRED | seed appears from the 2nd session |
| Unsubscribe RequestCode 24 vs 25 | UNKNOWN | ghost counter is the probe; `ghost` outcomes are **not seeded at zero**, so the first sample would be dropped by the agent |
| Depth churn measured | UNKNOWN | counters exist, no session read |
| 16 sockets · Full mode + IDX_I Quote · 25,000 cap · NTM spot set · depth in full with `depth_kind` · stock-options-only lots-in-window ranking · 5 s ranking sweep · delta-only capped resubscribe · distinct underlyings + hysteresis · 09:00 candles · receipt-clock OHLCV · TVW3 WAL · percentage columns · 15:41 cross-verify · depth seed · ghost redial · replay watermark · 1s/5s views · `window_lots_milli` · `gain_pct` from underlying · `undo_swap` | **DELIVERED (21)** | A25 table, each with file:line |

## 5. Findings by severity

### CRITICAL (session-ending or data-losing today)
1. **`claude/integration` cannot build** and violates three permanent locks (Groww removal, Rust-only, CloudWatch-only). Do not merge.
2. **Dhan 804 parks a depth-200 socket for the session with no recovery path** (`pool_supervisor.rs:511-543, 863-869, 1247-1256, 4681`). With capacity 1 per socket, a wrong unsubscribe code produces this on the FIRST swap at any cadence. The ghost redial (`take_ghost_redial`) is consulted only while the socket is `Continue`, so it structurally cannot help. The "self-healing" claim in the 2026-09-08 THIRD rule section is withdrawn by the 2026-09-09 section — correctly.
3. **Budget:** September forecast $142.24 against the $135.00 automatic `STOP_EC2_INSTANCES` line, and both actions are `STANDBY` (armed). A stop at 14:00 IST on a trading day is now the expected outcome of doing nothing. The $150 `hard_stop_guard` line additionally disables the morning start rule (the box does not come back).

### HIGH
4. Futures-primary ATM lock: recorded-only (§4).
5. PR #1901 duplicated `#[test]` orphans an existing test (§2).
6. **Parked depth-200 socket keeps its contract "held" forever**: planner never re-homes it (`depth200_ranked_steer.rs:141-150`), the view reports `subscribed=true` (`depth_rebalance.rs:1269`), only a per-minute `channel_closed` log says otherwise.
7. **False ghost across pools:** a contract dropped by depth-20 and legitimately subscribed by depth-200 within the same minute reads as a ghost and redials the healthy depth-200 socket (`depth_subscription_view.rs:281-304`, `dhan_feed_stack.rs:5267`). Expected several times per session under normal top-5 ⊂ top-250 churn.
8. **Expired contract in a guard set = infinite 30 s redial loop** (813 is `Transient`, nothing evicts the contract; `pool_supervisor.rs:543-544`).
9. **Universe > 25,000 after a volatile expiry ⇒ the WHOLE pool is refused** and the lane falls back to 4 indices (`dhan_live_universe.rs:328`, `dhan_feed_stack.rs:8849`). Margin today ~2,004 slots; index chains are UNCAPPED by operator ruling.
10. Memory over-commit 33.1 GiB on a 31.3 GiB host (`tickvault.service:354` `MemoryHigh=20G`, compose `QDB_MEM_LIMIT 12g`, `shm 512m`, sidecars 0.64 GiB).
11. Disk runway trigger inert (`ingest_shed.rs:504` `SESSION_BURN_BYTES_DEFAULT = 0`); shed thresholds fractional (0.15/0.08/0.04 of a growing volume).

### MEDIUM
12. `drain_depth_frame` seam has no DHAT gate (`dhan_feed_stack.rs:6992`); `classify_raw` rides it ungated.
13. 31 new metric names with no CloudWatch surface and no dated rule row (B15 list); `tv_tick_spill_replayed_bytes_total` claimed shipped in §2.3c but absent from `cloudwatch-agent.json`.
14. `tv_dhan_feed_depth_total{outcome=ghost|ghost_redial|ghost_exhausted|unsubscribed_grace}` not seeded at zero (`dhan_feed_stack.rs:3681-3715`) — the CloudWatch first-sample rule drops the first event, which is the only event that verifies the unsubscribe code.
15. No audit-table rows for depth swaps, ghost redials, depth-seed decisions, contract refusals-by-reason (B18).
16. Refold with `lost > 0` still confirms and archives the segment (`dhan_feed_stack.rs:11762-11840`); active-dir byte prune can delete unreplayed segments (`ws_frame_spill.rs:3709-3719`); offload queue bounded in count not bytes.
17. Writer join at shutdown blocks a tokio worker up to 12 s with no poison flag (`dhan_feed_stack.rs:2190-2195, 6675-6720`).
18. 11/12 newest modules outside mutation scope; zero proptest; no loom on the new seams (B17).
19. Public `GET /api/feeds*` exposes `auth_rejected`, subscribed counts, drop counts on a funnelled port (`feeds.rs:35-52`).
20. `depth20_track.rs:63,1169` `segment as u8` vs `binary_code()` elsewhere (BSE_FNO 7 vs 8) — latent.
21. `volume_leaderboard.rs:550` re-latch `warn!` on the tick path is unthrottled and uncoded.
22. `dockerd` dead at a NORMAL boot is unhandled (`tickvault.service:20-21` `Wants=` only; fix exists only in `emergency-fs-recover.yml`).
23. CLAUDE.md/rule staleness in the blocking direction (operator-charter rule 13) and the dependency table (17/24 wrong).

### LOW
24. Raw reqwest error logs at `dhan_contract_universe.rs:1950-1976`; `depth_seed.rs:315-333` `from_slice` without a byte cap; `refusals()` returns 4 of 5 counters; two zero-caller pub fns in `spot_price_store.rs`; `DAY_PARTITIONED_TABLES` lists ~12 deleted tables; 46 index slugs where docs say 49; `partition_archive.rs:231` says 21 candle tables (24); `tf_index.rs:7,23` mentions a Mutex that no longer exists; `rebaseline_all` cited in two docs but asserted absent by a guard; `budget-guards.tf:15,260` comments still say $55.

## 6. What a 5-second apply cadence needs, in order (from A10)

| Step | Change | Pinning test |
|---|---|---|
| 0 | Read one live session's `ghost` vs `unsubscribed_grace` counters (after seeding them at zero) | — |
| 1 | Make an 804-caused `FatalDisconnect` eligible for ONE respawn after `GHOST_REDIAL_COOLDOWN_SECS`, replaying the guard's current set | `a_single_804_park_respawns_once_with_the_retained_set_and_a_second_804_parks_forever` |
| 2 | Move `load_depth_candidates` + `fetch_movers` (2 × 10 s QuestDB budget) to a once-a-minute arm; the 5 s arm plans from RAM only | `the_five_second_arm_makes_no_database_call` (source scan) |
| 3 | Per-socket rolling 60 s swap budget shared by both planners | `twelve_five_second_calls_never_exceed_four_swaps_per_socket_per_minute` |
| 4 | `depth_steering_stalled` threshold 180 → ≤ 6 × cadence | assert in `cloudwatch_app_alarms_wiring.rs` |
| 5 | Flip the cadence on the steering task (never the drain) | `the_frame_drain_never_sends_a_swap_or_unsubscribe` |

Worst-case wire cost with today's caps at 5 s: 300 swaps/min = 600 messages/min across 10 depth sockets.

## 7. Agent roster and honesty about coverage

| Agent | Result |
|---|---|
| A01 integration, A03 rust-only, A04 O(1) table, A05 hot path, A06 security (partial: no shell), A07 storage, A08 depth ranking, A09 futures-ATM, A10 cadence blockers, A11 ghost/804, A12 WAL, A13 disk, A14 pricing, A20 CLAUDE.md, A22 composite key, A25 requirements, A27 permutations | completed round 1 |
| B02 PR review, B15 observability, B17 test types, B18 audit codes, B19 config drift, B21 races, B23 order side, B24 REST/xverify, B26 lambdas, B28 API, B29 candles, B30 simplifier, B31 deps | completed round 2 after the rate-limit kill |
| B16 build verify | see §8 |
| Not possible from this container | `cargo audit` / `cargo deny` (not installed); live CloudWatch/QuestDB reads (tickvault-logs MCP failed to connect); the GitHub MCP had bad credentials, so PR state came from the public REST API |

## 8. Build verification on the PR #1901 head
