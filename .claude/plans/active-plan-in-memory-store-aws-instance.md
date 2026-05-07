# Design Plan: In-Memory Store (§17) + AWS Instance Re-evaluation (§18 v2)

**Status:** DRAFT — discussion only, no implementation contract until operator answers Q1–Q15.
**Date:** 2026-05-07
**Authors:** Parthiban (architect), Claude (builder)
**Branch:** to-be-created on operator approval
**Supersedes:** `.claude/plans/active-plan-29-tf-and-movers-deletion.md` §17 + §18 (when this lands as plan revision).
**Source plans referenced:**
- `.claude/plans/active-plan-29-tf-and-movers-deletion.md` §17.1–§18.7 (current state)
- `.claude/rules/project/aws-budget.md` (₹5K/mo hard cap, banned instance list)
- `docs/operator/aws-readiness-audit-2026-05-03.md` (15 findings, 3-agent adversarial review)
- `docs/runbooks/aws-deploy.md` (first-time setup)
- `docs/runbooks/aws-disaster-recovery.md`
- `.claude/plans/active-plan-non-aws-readiness-fixes.md` (10-item post-audit work)
- `.claude/plans/active-plan-wave-6-backlog.md` W6-1 (600K→2M rescue ratchet) + W6-2 (>65h sleep)

---

## A) Locked decisions (no debate)

| # | Locked decision | Source PR / file |
|---|---|---|
| A1 | 4 ticks columns: `volume_delta`, `prev_day_close`, `prev_day_oi`, `phase` | PR #466 |
| A2 | Candle matviews 18 → 28 TFs (RAM ≡ DB parity) | PR #503 |
| A3 | `CandleEngine<TF>` generic + `CascadeFanout` 28 engines wired | PR #482/484/485/486/488 |
| A4 | Composite key `(security_id, exchange_segment)` per I-P1-11 | locked stack-wide |
| A5 | DEDUP UPSERT KEYS includes segment on every storage table | meta-guard ratchet |
| A6 | `MEMORY_RSS_ALERT_MB = 1024` (1 GB) | PR #497 |
| A7 | Tick rescue ring capacity = 2,000,000 | PR #453 |
| A8 | Phase 4b dead code DELETED (~4K LoC) | PR #494 |
| A9 | MOVERS-01/02/03 ErrorCodes RETIRED | PR #495 |
| A10 | `/api/movers/v2` DORMANT scaffold (reads `CascadeFanout`) | PR #490 |
| A11 | ₹5,000/mo hard AWS budget cap | `aws-budget.md` (Parthiban authority) |
| A12 | Non-AWS readiness fixes shipped (10 items, audit findings #2/#3 reworded honestly) | PR-set on branch `claude/non-aws-readiness-fixes-2026-05-03` |
| A13 | c7i-flex / t3 / c7i.large BANNED (burstable CPU + RAM-too-small) | `aws-budget.md` |

## B) The 5 blocking decisions

### B1) §17 retention shape

| Option | RAM | Cost | Fits c7i.xlarge | Notes |
|---|---:|---|---|---|
| A — Full 1,279 bars × 11K | ~1.7 GB | needs c7i.2xlarge or m7i.xlarge | ❌ | most flexible; >₹5K cap on-demand |
| **B — Last 100 bars/TF × 11K** | ~140 MB | ₹0 | ✅ tight | RECOMMENDED; >100 bars hits matview |
| C — Top-1K liquid full + rest QuestDB | ~155 MB | ₹0 | ✅ asymmetric | needs liquidity classifier — defer |

### B2) §17 PR ordering

| Order | Risk | Rollback |
|---|---|---|
| Sequential (#504 in-mem → #505 reads → #506 cohort SQL → #507 DELETE+DROP) | LOW — each PR shippable independent | trivial — config flag |
| Movers-DELETE-first (skip to #507) | HIGH — table gone before v2 read flip validated | hard |
| Hybrid (#504+#505 together, #506 deferred) | MED — depth-dynamic still hits `movers_1m` | medium |

### B3) §18 instance generation (NEW — operator's c8g/latest-gen reopener)

Operator demand 2026-05-07 (verbatim): *"why not any latest like c8g or any other latest instances especially using the extreme latest instance to have a powerful ultra fast automated compute"*.

Honest re-survey of compute generations available in **ap-south-1 (Mumbai)** as of 2026-05. Pricing snapshot from operator-side `aws ec2 describe-instance-types` + `aws pricing get-products` is required to FINALIZE; figures below are last-confirmed prices that need re-verification at deploy time.

| Family | Gen | CPU | xlarge $/hr | xlarge ₹/mo (full month) | Mumbai availability | Notes |
|---|---|---|---:|---:|---|---|
| **c7i** | Intel Sapphire Rapids (4th gen Xeon) | x86_64 | $0.1785 | ₹3,610 | ✅ confirmed | current baseline; 1-yr RI ₹2,275 |
| **c7a** | AMD EPYC 9xxx (Zen 4) | x86_64 | $0.1958 | ₹3,960 | ✅ likely | ~10% pricier than c7i; perf parity |
| **c7g** | Graviton3 (ARM Neoverse V1) | aarch64 | $0.1421 | ₹2,874 | ✅ confirmed | **20% cheaper**, requires ARM build |
| **c7gd** | Graviton3 + NVMe local | aarch64 | $0.1814 | ₹3,668 | ✅ likely | local NVMe = lower disk latency |
| **c8g** | Graviton4 (ARM Neoverse V2) | aarch64 | ~$0.1697 (est.) | ~₹3,432 (est.) | ⚠️ NEEDS VERIFY | **latest gen**; ~30% perf uplift over c7g; ap-south-1 listing not confirmed in our tooling |
| **m7i** | Intel Sapphire Rapids, mem-balanced | x86_64 | $0.2016 | ₹4,078 | ✅ confirmed | 16 GB RAM at xlarge |
| **m7a** | AMD EPYC, mem-balanced | x86_64 | $0.2210 | ₹4,471 | ✅ likely | |
| **m7g** | Graviton3, mem-balanced | aarch64 | $0.1632 | ₹3,302 | ✅ confirmed | 16 GB RAM @ 20% saving |
| **m8g** | Graviton4, mem-balanced | aarch64 | ~$0.1958 (est.) | ~₹3,962 (est.) | ⚠️ NEEDS VERIFY | latest gen + 16 GB |
| **r7g** | Graviton3, mem-optimized | aarch64 | $0.2117 | ₹4,282 | ✅ confirmed | 32 GB RAM |
| **r8g** | Graviton4, mem-optimized | aarch64 | ~$0.2541 (est.) | ~₹5,140 (est.) | ⚠️ NEEDS VERIFY | breaches cap on-demand; viable on RI |

Pricing is **on-demand, full month (730hr)** for direct comparison; our schedule is 238 hr/mo which scales accordingly.

### B3.1) ARM (Graviton) vs x86 — the real tradeoff

| Concern | Status / Evidence |
|---|---|
| Pinned Rust workspace | Targets `x86_64-unknown-linux-gnu`. Adding `aarch64-unknown-linux-gnu` = `cargo build --target aarch64-...` + cross-compile or remote-build. |
| `aws-lc-rs` (TLS) | ✅ Supports aarch64. Production-ready. |
| `jemalloc-sys` | ✅ Supports aarch64. |
| `tokio` + `tokio-tungstenite` | ✅ portable. |
| `questdb-rs` (ILP client) | ✅ pure Rust, portable. |
| `rkyv` zero-copy | ✅ portable; endianness already LE on both. |
| Docker images we depend on | QuestDB ✅ ARM, Valkey ✅ ARM, Prometheus ✅ ARM, Grafana ✅ ARM, Alertmanager ✅ ARM, Traefik ✅ ARM, Loki/Alloy ✅ ARM. All multi-arch on Docker Hub. |
| CI cross-compile | Currently x86 only. Need GitHub Actions matrix or AWS CodeBuild Graviton runner. ~1 day work. |
| Benchmark / DHAT parity | DHAT works on aarch64. Criterion runs portably. **But** Wave-4 budget numbers in `quality/benchmark-budgets.toml` were measured on x86 — re-baseline on the chosen arch. |
| Hot-path correctness | Endian is LE both sides. SIMD lanes (none in our code). No `_mm_*` intrinsics. ✅ neutral. |
| AWS Mumbai availability | c7g ✅ confirmed. c8g ⚠️ pending verification (operator-side `aws ec2 describe-instance-types --region ap-south-1 --filters Name=instance-type,Values=c8g.xlarge`). |

### B3.2) Re-ranked instance choice under ₹5K cap

The schedule (`aws-budget.md`: 9hr × 22 + 5hr × 8 = 238 hr/mo) gives much smaller numbers than the full-month figures above. Re-applying:

| Rank | Instance + RI tier | Total ₹/mo (incl. EBS+EIP+CW+SNS+S3+DT) | Headroom | RAM | Supports option |
|---|---|---:|---:|---|---|
| **1** | **c7g.xlarge 1-yr RI no-upfront (ARM)** | **~₹3,310** | ₹1,690 | 8 GB | B (140 MB) |
| 2 | c7i.xlarge 1-yr RI no-upfront (x86, current) | ₹3,787 | ₹1,213 | 8 GB | B |
| 3 | m7g.xlarge 1-yr RI no-upfront (ARM) | ~₹3,720 | ₹1,280 | 16 GB | A (1.7 GB), D (1d hist) |
| 4 | c8g.xlarge 1-yr RI (Graviton4) — IF available | ~₹3,470 (est.) | ~₹1,530 | 8 GB | B + ~30% perf headroom |
| 5 | m7i.xlarge 1-yr RI (x86) | ₹4,081 | ₹919 | 16 GB | A, D |
| 6 | c7i.xlarge 3-yr RI all-upfront (x86) | ₹2,967 | ₹2,033 | 8 GB | B + capital lock ₹52K |
| 7 | r7g.xlarge 1-yr RI (ARM) | ~₹4,360 (est.) | ~₹640 | 32 GB | A, D, E (7d hist) |

**Honest reading:**
- **c7g (Graviton3) saves ~₹500/mo vs c7i** at xlarge with same 8 GB RAM.
- **m7g saves ~₹360/mo vs m7i** at xlarge with 16 GB RAM (and unlocks Option A for in-mem store and Option D historical at the SAME budget headroom as c7i.xlarge today).
- **c8g (Graviton4)** if Mumbai-listed gives ~30% per-core perf at near c7i price — but tooling readiness (cross-compile, CI, benchmark re-baseline) has a one-time cost.
- The operator's "extreme latest = ultra fast" intuition lines up with **m7g 1-yr RI** (best perf/₹ in stable territory) or **m8g 1-yr RI** (latest, needs availability + tooling work).

### B4) §17 historical scope (Option B/D/E/F/G — unchanged from §18.5)

| Option | RAM | Use case | Instance impact |
|---|---:|---|---|
| B (baseline) | today only | live trading | c7g/c7i/c8g any |
| D | today + 1d | yesterday vs today | same — fits at zero cost |
| E | today + 7d | weekly volatility regime | bump to m7g/m7i/m8g |
| F | today + 30d | monthly regime / SEBI replay | bump to r7g/r8g or c7i.2xlarge |
| G | mmap historical via parquet/QuestDB partitions | selective backtest >30d | any — but RSS spikes if SELECT wide; needs bounded-SELECT guard |

### B5) Reserved-Instance lock-in flexibility

| Tier | Discount vs on-demand | Lock-in | Type-change penalty |
|---|---|---|---|
| On-demand | 0 | none | none |
| 1-yr Standard RI no-upfront | ~37% | 1 year | full forfeit on type change |
| 1-yr Convertible RI no-upfront | ~24% | 1 year | can convert to other type |
| 3-yr Standard RI all-upfront | ~60% | 3 years | full forfeit |
| 3-yr Convertible RI all-upfront | ~52% | 3 years | can convert |

If we expect to migrate Graviton3 → Graviton4 (c7g → c8g) within the year, **Convertible RI** is the safer lock-in. Costs ~13% more than Standard but preserves migration optionality.

## C) 16 deeper requirements §17/§18 don't pin down

| # | Open requirement | Why it matters | Resolution path |
|---|---|---|---|
| C1 | Cold-boot mid-market behaviour | App boots 11:00 IST → in-mem empty for 0–105 min; strategy reads "last 5m" hits matview fallback (~1ms) — acceptable or backfill at boot? | Q2 |
| C2 | Per-TF retention asymmetry | 100 bars × 1m = 100 min, 100 × 1d = 100 days. Uniform vs asymmetric? | Q1 |
| C3 | Strategy/consumer read pattern | Max-N across `MoversEngine`, indicators, strategy evaluator | Q1 |
| C4 | 3-source-of-truth parity (live tick → RAM → matview) | Currently parity only RAM ≡ DB at write boundary; no live runtime drift detector | Q3 |
| C5 | Eviction-vs-read race in `ArrayVec` | papaya pin gives MVCC of Bar slots, but inner ArrayVec is mutating; reader holding slice while writer pushes | Q3 — confirm `Mutex` vs `ArcSwap<Vec<Bar>>` vs lock-free |
| C6 | Backfill on instrument-master daily refresh | New (security_id, segment) at 00:00 IST — empty start vs matview backfill? | Q2 |
| C7 | Movers cohort SQL migration semantics (#506) | `movers_1m` had derived `volume_change_pct`, etc.; `candles_1m` has only OHLCV — re-derive in SELECT or in `MoversEngine`? | Q4 |
| C8 | RI lock-in flexibility | Standard vs Convertible — 13% premium for migration optionality | Q5 |
| C9 | OS-level memory tunes | `vm.overcommit_memory=2`, `cgroup memory.high=7.5G`, `swappiness=0` — currently aspirational, not in `deploy/aws/*` | Q8 |
| C10 | PROC-01/02 OOM detection (Wave-4-E1 RESERVED) | Container restart loop = data corruption window; not yet shipped | Q7 |
| C11 | `tv-swap-usage-nonzero` alert (RESERVED) | Unconfigured swap silently degrades latency | Q7 |
| C12 | mmap-vs-RSS bounded-SELECT enforcement | mmap pages enter RSS on access; wide SELECT spikes — needs API-layer guard | Q4 (only if B4=G chosen) |
| C13 | Runtime-tunable cap | Operator demanded "common runtime, dynamic, scalable, incremental" — `ArrayVec<Bar, 100>` is compile-time const | Q3 |
| C14 | Audit trail for in-mem evictions | `tv_in_mem_evictions_total{tf}` for visibility — nice-to-have or audit-required? | Q10 |
| C15 | Rebuild-on-corruption (single-instrument repopulate from matview) | YAGNI vs defence-in-depth | Q9 |
| C16 | Consumer migration guard | Test pinning ALL consumers go through in-mem store; none accidentally hit `candles_*` direct on hot path | Q3 |

## D) Interdependencies

| If you decide... | ...it forces | ...and unblocks |
|---|---|---|
| B1 = Option A | B3 ≥ m7g/m7i (16 GB) | full retention in RAM; D/E historical feasible |
| B1 = Option B | B3 = c7g/c7i/c8g (8 GB cheapest path) | only B4 = B/D feasible at zero infra cost |
| B3 = Graviton family (c7g/m7g/c8g/m8g) | adds aarch64 cross-compile + CI matrix work (~1 day one-time) | unlocks ~₹500/mo savings or ~30% perf uplift on c8g |
| B4 = E/F | B1 = Option A; B3 ≥ m7g/m7i; possibly r7g/r8g | weekly/monthly RAM backtest |
| B4 = G (mmap) | C12 (bounded-SELECT) becomes blocking | adds historical without RAM cost |
| C13 = runtime-tunable cap | §17.4 architecture changes (`ArrayVec<Bar, 100>` → `SmallVec` with cap or `Vec` with config) | "common runtime dynamic" demand |
| B5 = Convertible RI | +13% cost, preserves Graviton3→4 migration | safer if c8g availability/perf uplift improves |

## E) 15 questions for operator (was 10 — added 5 for AWS instance generation)

| # | Question | Why I'm asking |
|---|---|---|
| Q1 | What N do real consumers need? Max bars-back across `MoversEngine`, indicators, strategy evaluator — rough numbers per TF | C2/C3 — drives whether 100 is right or asymmetric per-TF |
| Q2 | Cold-boot mid-market — strategies tolerate matview fallback (~1ms) for first 100 min, or must we backfill at boot? | C1/C6 |
| Q3 | Cap = compile-time const (`ArrayVec<Bar, 100>`) or runtime-tunable (`config.toml: in_mem_bars_per_tf = { "1m": 200, ... }`)? | C13 — operator's "dynamic + scalable" |
| Q4 | Historical scope: B (today), D (today + 1d at no cost), E/F (forces instance bump), or G (mmap)? | B4 — biggest infra-cost lever |
| Q5 | Standard RI or Convertible RI? | B5 — migration optionality vs 13% premium |
| Q6 | PR order: sequential (#504→507) or fold #506 into #505 (3 PRs)? | B2 |
| Q7 | Ship PROC-01/02 + `tv-swap-usage-nonzero` as prerequisite of §17/§18, or accept as follow-up gap? | C10/C11 — closes "100% scenarios" or admits known-gap |
| Q8 | OS tunes (`overcommit_memory`, `cgroup memory.high`, `swappiness=0`) — prerequisite blocker or land in #504 infra commit? | C9 |
| Q9 | Per-instrument rebuild-on-corruption path — needed or YAGNI? | C15 |
| Q10 | `tv_in_mem_evictions_total{tf}` counter — audit-required or visibility-only? | C14 |
| **Q11** | **Architecture: stay x86 (c7i family) or migrate to ARM (c7g/m7g/c8g/m8g)?** | B3.1 — saves ~₹500/mo or unlocks Option A; one-time cross-compile cost |
| **Q12** | **If ARM: stable Graviton3 (c7g/m7g) or latest Graviton4 (c8g/m8g)?** | B3.2 — c8g needs Mumbai-availability verify + benchmark re-baseline |
| **Q13** | **Should we run `aws ec2 describe-instance-types --region ap-south-1` now to confirm c8g/m8g availability?** | B3 — closes the "estimated price" gap |
| **Q14** | **Auto-rebaseline benchmark budgets on chosen arch?** (`bench-gate.sh` runs after instance switch) | C4 — ensures ratchets don't false-fail on aarch64 |
| **Q15** | **Multi-arch CI matrix from day 1 (x86 + aarch64)?** | B3.1 — keeps both archs viable for fallback; doubles CI time |

## F) Existing AWS-related docs inventory (so we don't duplicate)

| Doc | Status | What it covers |
|---|---|---|
| `.claude/rules/project/aws-budget.md` | ACTIVE rule | ₹5K/mo cap; banned instance list; Docker memory budget; instance schedule (07:45–17:15 weekday, 07:45–13:15 weekend); EBS lifecycle |
| `docs/operator/aws-readiness-audit-2026-05-03.md` | ACTIVE audit | 15 findings (3 CRITICAL, 4 HIGH, 4 MEDIUM, 4 LOW) from 3-agent adversarial review; per-step boot timeout audit |
| `docs/runbooks/aws-deploy.md` | ACTIVE runbook | First-time setup; Phase 1 account creation; IAM; Terraform |
| `docs/runbooks/aws-disaster-recovery.md` | ACTIVE runbook | DR procedures |
| `.claude/plans/active-plan-non-aws-readiness-fixes.md` | IMPLEMENTED | 10-item post-audit (§8 wording reword, s3_backup deletion, etc.) |
| `.claude/plans/active-plan-wave-6-backlog.md` (W6-1 / W6-2) | BACKLOG | Rescue ring 600K→2M ratchet; 70h sleep chaos test |
| `.claude/plans/archive/2026-04-14-session-7-aws-and-tv-doctor.md` | ARCHIVED | historical context |
| `.claude/plans/archive/2026-04-06-infra-self-healing.md` | ARCHIVED | infra resilience design |
| `.claude/plans/archive/2026-03-19-fix-infra-gaps.md` | ARCHIVED | early infra audit |
| `.claude/plans/archive/2026-03-19-fix-infra-observability-automation-gaps.md` | ARCHIVED | observability gaps |

**Net:** AWS already has a rule (budget cap), a deploy runbook, a DR runbook, an audit, a post-audit fix-plan, and 6 archived design iterations. This new plan extends `aws-budget.md` with the **instance-generation re-survey** (B3) — does NOT replace any existing doc, only adds Graviton3/4 evaluation that the current `aws-budget.md` doesn't cover.

## G) Plan-doc updation contract (what changes when this plan lands)

| File | Change |
|---|---|
| `.claude/plans/active-plan-29-tf-and-movers-deletion.md` | §17 + §18 marked "SUPERSEDED by `active-plan-in-memory-store-aws-instance.md`" + cross-link |
| `.claude/rules/project/aws-budget.md` | Banned-instance list extended (or relaxed) per Q11/Q12 outcome; add Graviton row; add ARM cross-compile note |
| `.claude/plans/active-plan-wave-6-backlog.md` | New items if Q7 deferred (e.g. PROC-01/02 follow-up); new item for benchmark re-baseline if Q14 |
| `quality/benchmark-budgets.toml` | If Q14 = re-baseline: numbers updated post-instance-switch |
| `.github/workflows/*.yml` | If Q15 = multi-arch CI: matrix added |
| `Cargo.toml` | If Q11 = ARM: add `[target.'cfg(target_arch="aarch64")']` blocks if any deps need it |
| `deploy/aws/terraform/main.tf` | If new instance chosen: `instance_type` changed; AMI ID swapped (Graviton AMIs differ from x86) |
| `deploy/docker/docker-compose.yml` | If ARM: pin `platform: linux/arm64` on services or rely on multi-arch manifests |

## H) Sequence once operator answers Q1–Q15

1. Spawn 3 parallel specialist agents (per `wave-4-shared-preamble.md` §3): `hot-path-reviewer` (CandleEngine read/write race), `security-reviewer` (in-mem vs SSM secret-handling on new arch), `general-purpose` (hostile bug-hunt — boot-mid-market emptiness, eviction-vs-read race, mmap RSS spikes, ARM endian/atomic correctness).
2. Synthesize findings into CRITICAL/HIGH/MEDIUM verdict table.
3. Fix every CRITICAL+HIGH inline.
4. Convert this DRAFT to APPROVED status with answers folded in.
5. Open §17/§18 supersession PR (single doc-only commit).
6. Open implementation sub-PRs (#504a–#507) per the resolved sequence.

## I) The honest 100% claim (verbatim — `wave-4-shared-preamble.md` §8 today)

> 100% inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤2,000,000-tick ring buffer capacity (`TICK_BUFFER_CAPACITY`,
> `crates/common/src/constants.rs`, ratcheted by
> `crates/storage/tests/zero_tick_loss_alert_guard.rs`); bench-gated
> O(1) hot path; composite-key uniqueness;
> chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake
> (`crates/core/tests/ws_sleep_resilience.rs`). Beyond the envelope,
> DLQ NDJSON catches every payload as recoverable text. Outstanding
> (Wave-6): >65h holiday-weekend dormant sleep test (W6-2).

If we move to ARM, the ratchet bench numbers must be re-baselined and the claim should add: *"benchmark budgets re-baselined on aarch64 (`quality/benchmark-budgets.toml`, commit <sha>)"*.

---

**End of DRAFT.** Awaiting operator answers to Q1–Q15 before APPROVED status.
