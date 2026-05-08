# Design Plan: In-Memory Store (§17) + AWS Instance Re-evaluation (§18 v2)

**Status:** DRAFT v6 — 52 design decisions LOCKED 2026-05-08 (L1–L52 across §K + §Q + §R). 2 open verification gates (`OPEN-VERIFY-1`, `OPEN-VERIFY-2`). No implementation contract until both gates close + operator approves all sections → APPROVED status.
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

## §J) Operator answers (Q1–Q15 collapsed 2026-05-08)

| Question | Answer |
|---|---|
| Q1 (consumer N) | Per-bar % fields collapse this — `MoversEngine` is a sort over latest-bar map; no fixed N |
| Q2 (cold-boot mid-market) | Full-day RAM populated as ticks arrive; first 0–105 min after boot reads RAM with growing-window data |
| Q3 (runtime-tunable cap) | YES — TF list is `config/base.toml [engine.timeframes]` boot-time array (hot-reload deferred) |
| Q4 (historical scope) | Current trading day only in RAM; historical via QuestDB matview (NOT on live trading path) |
| Q5 (RI tier) | 1-yr Standard RI (operator confirmed in §K-L1 as floor for cost) |
| Q6 (PR ordering) | Sequential #504 → 505 → 507 (PR #506 KILLED — see §K-L14) |
| Q7 (Wave-4-E1 reserved) | Deferred follow-up — `tv_subsystem_memory_bytes` to ship as #504 prereq instead (§K-L18) |
| Q8 (OS tunes) | Land in #504 deploy commit (`vm.swappiness=0`, cgroup `memory.high`) |
| Q9 (corruption rebuild) | YAGNI — rebuild on next IST midnight rollover or app restart |
| Q10 (eviction counter) | YES — `tv_in_mem_evictions_total{tf}` (§K-L18 observability bundle) |
| Q11 (ARM yes/no) | YES — Graviton4 |
| Q12 (Graviton3 vs 4) | Graviton4 (c8g family) |
| Q13 (Mumbai availability verify) | OPEN — operator to run `aws ec2 describe-instance-types --region ap-south-1` before Terraform commit |
| Q14 (benchmark re-baseline) | YES — `quality/benchmark-budgets.toml` re-baselined on aarch64 in #504 boot smoke commit |
| Q15 (multi-arch CI) | aarch64 only at first (saves CI minutes); add x86 matrix in Wave-6 if fallback needed |

## §K) Locked design decisions (L1–L18, 2026-05-08)

### Hardware + infrastructure

| # | Locked | Detail |
|---|---|---|
| L1 | AWS instance | **c8g.xlarge** (Graviton4, 4 vCPU, 8 GB), 1-yr Standard RI no-upfront, ap-south-1 (Mumbai). Architecture = aarch64. |
| L2 | Docker memory budget (Trim Path A) | QuestDB 4G→2G, Valkey 1G→384M, Grafana 1G→512M, Prometheus 512M→256M, Alertmanager 256M, Traefik 512M, Valkey-exporter 128M. **Total ~3.5 GB.** |
| L3 | Free RAM after Docker + OS for app | ~4.35 GB (8 GB − 3.5 GB Docker − 0.15 GB OS) |
| L4 | OS tunes (#504 commit) | `vm.swappiness=0`, `vm.overcommit_memory=2`, cgroup `memory.high=7.5G` |

### Data scope

| # | Locked | Detail |
|---|---|---|
| L5 | Subscribed universe (Wave 5 default) | 3 indices IDX_I + ~26 display indices + ~216 cash equities + ~10,789 index F&O = **~11,034 instruments** |
| L6 | Timeframes (NEW — drop seconds) | **21 TFs**: 1m, 2m, 3m, 4m, 5m, 6m, 7m, 8m, 9m, 10m, 11m, 12m, 13m, 14m, 15m, 30m, 1h, 2h, 3h, 4h, 1d |
| L7 | Dropped TFs | 1s, 3s, 5s, 10s, 15s, 30s — DELETED (no consumer; raw ticks already provide sub-minute) |
| L8 | TF list source | `config/base.toml [engine.timeframes].list` — runtime-tunable boot-time array |

### In-memory store

| # | Locked | Detail |
|---|---|---|
| L9 | Full current-day RAM scope | ALL ticks + ALL bars across all 21 TFs for ALL ~11K instruments |
| L10 | Tick storage | `papaya<(security_id, exchange_segment), Vec<ParsedTick>>` — full day, IST 09:15 reset, ~880 MB |
| L11 | Bar storage | `papaya<(security_id, exchange_segment), CandleEngineMap<TF>>` per-TF, full day, 21 TFs, ~1.21 GB |
| L12 | New per-Bar fields | `cumulative_volume` (running daily sum at seal), `close_pct_from_prev_day`, `prev_day_oi`, `oi_pct_from_prev_day`, `volume_pct_from_prev_day` (5 new fields) |
| L13 | % field stamping | At bar SEAL only (NOT per tick); on-demand `~50 µs` scan for between-seal "right now" % queries |

### Pipeline + APIs

| # | Locked | Detail |
|---|---|---|
| L14 | Movers replacement | `MoversEngine` becomes ~50 LoC façade over latest-bar map; **PR #506 cohort SQL migration KILLED**; depth-dynamic selector reads `MoversEngine.top_k_by(VolumeDesc, 250)` |
| L15 | DB write cadence | Continuous ILP for ticks (current architecture, no change); seal-time write for bars |
| L16 | Trading-path reads | RAM only — never touch DB |
| L17 | 1s engine | DELETED — ticks feed minute engines directly |

### Observability prerequisites

| # | Locked | Detail |
|---|---|---|
| L18 | `tv_subsystem_memory_bytes` | NEW Prometheus gauge per-component (rescue ring, tick map, bar map per-TF, registry, baseline) — ships as #504 PREREQ so we can prove RAM cap holds in real-time |

### Volume semantic + reset window

| # | Locked | Detail |
|---|---|---|
| L19 | Volume field interpretation | Quote bytes 22-25 + Full bytes 22-25 = `u32` LE = cumulative day volume (running sum since 09:15 IST). Confirmed by `live-market-feed.md` rules 8+10 + draft Dhan support `2026-05-01-volume-semantic-clarification.md` Q1-Q5 |
| L20 | Cumulative volume reset | 09:15 IST market open (matches Dhan wire semantic exactly) |
| L21 | `MoversEngine` Top Volume ranking | Sort by `cumulative_volume` field on latest bar DESC. **Until OPEN-VERIFY-1 closes, this is "best-effort matched to Dhan's `/api/movers` UI" — not bit-for-bit guaranteed** |

## §L) OPEN verification gate (must close before §K becomes APPROVED)

| # | Gate | Action | Owner | Blocker for |
|---|---|---|---|---|
| **OPEN-VERIFY-1** | Confirm Dhan's `/api/movers` "Top Volume" ranks by the same cumulative-since-09:15 field | (a) Send `docs/dhan-support/2026-05-01-volume-semantic-clarification.md` to `apihelp@dhan.co` with Q6 added (UI movers source mapping); (b) live test 09:14:55/09:15:00/09:15:30 IST tomorrow capturing volume field for one liquid instrument; (c) cross-check `/v2/marketfeed/quote` REST volume vs WebSocket Quote vs UI Top Volume rank for same instrument at 14:30 IST | Operator (Gmail) + Claude (live test setup) | full APPROVED status of §K-L21 |
| **OPEN-VERIFY-2** | Confirm Mumbai availability + lock real prices for c8g.xlarge | `aws ec2 describe-instance-types --region ap-south-1 --filters Name=instance-type,Values=c8g.xlarge` from operator AWS account; lock 1-yr Standard RI quote | Operator | Terraform commit in #504 |

## §M) Implementation contract (after §L gates close)

### PR sequence

| # | PR | Scope |
|---|---|---|
| 1 | **#504a** prerequisite — Observability | Implement `tv_subsystem_memory_bytes` per-component gauge; add Grafana panel; add `tv_in_mem_evictions_total{tf}` counter; add `tv-rss-per-subsystem-high` alert |
| 2 | **#504b** schema + struct | Add 5 new Bar fields (cumulative_volume, close_pct, prev_day_oi, oi_pct, volume_pct); ALTER candles_* matviews; add new fields to ILP writer |
| 3 | **#504c** TF list config | `config/base.toml [engine.timeframes]` array; remove 1s/3s/5s/10s/15s/30s engines from `CascadeFanout`; ratchet test for TOML-driven TF list |
| 4 | **#504d** in-memory store | Tick ring per instrument (full day); bar storage per (instrument, TF); IST 09:15 reset wiring; runtime-tunable cap |
| 5 | **#504e** % stamping at seal | `close_pct_from_prev_day`, `oi_pct_from_prev_day`, `volume_pct_from_prev_day` computed in `seal_bar` path |
| 6 | **#505** read flip | `/api/movers/v2` reads `MoversEngine` (façade over latest-bar map); 7 sort categories supported |
| 7 | **#506** ~~cohort SQL migration~~ | **KILLED — depth-dynamic selector reads RAM directly per L14** |
| 8 | **#507** Phase 4b drops | DROP `stock_movers` + `option_movers` tables (already wired dormant); 30-day clean window per `docs/runbooks/phase-4b-movers-cleanup.md` |
| 9 | **#508** AWS deploy | Terraform `instance_type = c8g.xlarge`; Docker mem_limits per L2; ARM AMI; aarch64 cross-compile CI; bench-budget re-baseline |

### 7-layer observability per new component (mandatory)

| Component | Prom counter | Prom gauge | Tracing span | Loki log | Telegram | Grafana panel | Audit table |
|---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| Tick ring per instrument | drops, evictions | size in bytes | ✅ | ✅ | on RSS alert | NEW panel | n/a (live data) |
| Bar storage per TF | seals, ticks-fed | bars-resident, bytes | ✅ | ✅ | on cap breach | NEW panel | n/a |
| `MoversEngine` | top-K queries served | latency p99 | ✅ | ✅ | on swap-error | NEW panel | n/a |
| % field stamping | bars stamped | latency p99 | ✅ | ✅ | n/a | n/a | n/a |
| `tv_subsystem_memory_bytes` | n/a | per-component RSS | n/a | ✅ at threshold | ✅ HIGH MEMORY | NEW panel | n/a |

### Honest 100% claim (refreshed for c8g.xlarge + ARM)

> 100% inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤2,000,000-tick ring buffer capacity;
> bench-gated O(1) hot path **(re-baselined on aarch64 c8g.xlarge in PR #508)**;
> composite-key uniqueness;
> chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake;
> **per-subsystem RSS accounting via `tv_subsystem_memory_bytes`**;
> **full-day in-memory store sized at ≤2.1 GB on 11,034 instruments × 21 TFs (`test_in_mem_store_total_under_2_5gb` ratchet)**;
> **volume field semantic confirmed by Dhan support ticket #2026-05-01-volume-semantic-clarification (cumulative since 09:15 IST, monotonic, resets at 09:15 IST next day)**.
> Beyond the envelope, DLQ NDJSON catches every payload as recoverable text.
> Outstanding: Wave-6 (>65h holiday-weekend dormant sleep test), OPEN-VERIFY-1 (`/api/movers` UI Top Volume mapping confirmation).

---

### Static universe simplification (NEW 2026-05-08)

| # | Locked | Detail |
|---|---|---|
| L22 | Static-subscribe universe principle | Subscribe ONCE at boot for the full Wave 5 universe (~11K instruments). NEVER re-subscribe based on pre-open data. No 09:00/09:08/09:12 IST subscription gates. |
| L23 | Trading window precise | **09:15:00 IST (inclusive) → 15:30:00 IST (exclusive)**. Tick capture starts at 09:15:00 IST when Dhan stream begins. After 15:30:00 IST, no new ticks expected; tick processing path drains the buffer. |
| L24 | Boot timing requirement | All reference data (`prev_day_close`, `prev_day_oi`, `prev_day_total_volume`, instrument master, JWT token, WS pool, ILP socket, in-memory store init) READY before 09:15:00 IST. Boot complete by 09:00:00 IST gives 15-min headroom. |
| L25 | Phase 2 readiness check (simplified role) | Stays at 09:13:01 IST as positive operator visibility signal (PHASE2-READY-01). NO LONGER drives subscription planning under indices-only scope. Pre-open buffer + REST fallback + 09:13 dispatch machinery becomes dormant under Wave 5. |
| L26 | Pre-market window observability | Capture data starting whenever Dhan emits it (typically 09:14:30 onward). Pre-open auction prices flow via existing pre-open buffer wiring as informational; not used for subscription planning. |
| L27 | Post-market window | Tick stream stops at 15:30:00 IST. Persist any in-flight rescue ring entries; in-memory store remains queryable for end-of-day backtests until app restart. |

### Top-K filter dimensions (NEW 2026-05-08)

| # | Locked | Detail |
|---|---|---|
| L28 | Top-N calculation primitive | **Method on candle storage map**: `candle_storage.top_n_by(category, expiry, scope, n) -> Vec<Bar>`. **No separate `MoversEngine` struct.** No hard cap on N. |
| L29 | Filter dimensions (Dhan-parity, 3 only) | (1) **Category** = sort field + direction (7 values mapping to Dhan UI tabs); (2) **Expiry** = date label (e.g. "26 MAY", "26 JUN", "26 JUL"); (3) **Scope** = mutually-exclusive 5-way enum (All / Index / Stock / NSE / BSE). **DROPPED:** option type (CE/PE), underlying, strike range — Dhan UI doesn't filter on these, neither do we. |
| L30 | 7 categories → (sort_field, direction) | Highest OI: `open_interest DESC` / OI Gainers: `oi_pct_from_prev_day DESC` / OI Losers: `oi_pct_from_prev_day ASC` / Top Volume: `cumulative_volume DESC` / Top Value: `(close × cumulative_volume) DESC` / Price Gainers: `close_pct_from_prev_day DESC` / Price Losers: `close_pct_from_prev_day ASC` |
| L31 | Scope enum mapping | `All` = no filter / `Index` = instrument_type IN (Index, IndexFuture, IndexOption) / `Stock` = instrument_type IN (Equity, StockFuture, StockOption) / `NSE` = segment IN (NSE_EQ, NSE_FNO, IDX_I) / `BSE` = segment IN (BSE_EQ, BSE_FNO) |
| L32 | No N restriction | N is operator-controlled per call. Latency ~50 µs scales linearly with universe size, not N. |
| L33 | Composite ranking (two-stage, optional) | For depth-dynamic cohort selection: primary filter (top-K by volume) → secondary sort (by close_pct_from_prev_day) — implemented as compose of two `top_n_by` calls when needed |
| L34 | Reuse for depth-20 / depth-200 | Same `candle_storage.top_n_by` method: depth-20 = `top_n_by(category=TopVolume, expiry=current, scope=NSE, n=250)`; depth-200 = same with n=5. **No separate selector code.** |
| L35 | NO `MoversEngine` abstraction | Architecture has only **TWO** in-memory data structures: (a) **tick map** `papaya<(security_id, exchange_segment), Vec<ParsedTick>>`, (b) **candle map** per TF `papaya<(security_id, exchange_segment), CandleEngineMap<TF>>`. Top-N is a query method on (b). **No separate movers engine, no separate writer, no separate aggregation pipeline.** |
| L36 | `/api/movers/v2` endpoint | URL stays for Dhan UI parity. Internally a ~30 LoC handler: parse query params (category, expiry, scope, n) → call `candle_storage.top_n_by(...)` → serialize bars to JSON. **No business logic — just a thin REST wrapper.** |

## §N) Static-subscribe model — what we DROPPED from the design

The shift to static universe + indices-only scope retires several design elements:

| Dropped element | Rationale | What replaces it |
|---|---|---|
| 09:00–09:12 IST pre-open buffer for STOCK F&O ATM strike resolution | Stock F&O is gated OFF under Wave 5 indices-only scope. No ATM resolution needed for static universe. | nothing — instruments are pre-known |
| Phase 2 dispatch at 09:13:01 IST (subscription dispatcher) | No subscription planning needed at 09:13. The full universe is already subscribed since boot. | Phase 2 readiness check stays as observability/visibility ping (PHASE2-READY-01) |
| REST `/marketfeed/ltp` fallback at 09:12:55 | Was a belt-and-suspenders for stock F&O pre-open close discovery. N/A under static universe. | nothing |
| Mid-market boot mode resolver (live-tick ATM resolver) | Was for mid-day boot under stock F&O scope. Indices-only static doesn't need it. | static subscribe at boot regardless of clock |
| Pre-open buffer 09:00–09:12 widened window | Original purpose: capture stock pre-open closes for F&O ATM math. | dormant code; can be deleted in future cleanup PR |

**These were correct designs for the prior stock-F&O-included universe. Under Wave 5 indices-only static universe, they become dead weight.** A separate cleanup PR after #504 lands can DELETE the dormant code (live-tick-atm-resolver, mid-market boot mode detection beyond simple `is_within_market_hours`, REST `/marketfeed/ltp` fallback module).

## §O) Updated PR sequence under static-universe simplification

| # | PR | Scope | Touches |
|---|---|---|---|
| 1 | **#504a** Observability prereq | `tv_subsystem_memory_bytes` per-component gauge + Grafana panel + alert | crates/app, observability |
| 2 | **#504b** Schema + struct | 5 new Bar fields + ALTER candles_* matviews + ILP writer | crates/storage, common |
| 3 | **#504c** TF list config | `[engine.timeframes]` array + remove seconds engines | crates/trading, config, app |
| 4 | **#504d** In-memory store | Tick ring (full day) + bar storage per (instrument, TF) + IST 09:15 reset | crates/trading, app |
| 5 | **#504e** % stamping at seal | close_pct, oi_pct, volume_pct computed in `seal_bar` | crates/trading |
| 6 | **#505** Read flip | `/api/movers/v2` is a ~30 LoC REST wrapper calling `candle_storage.top_n_by(category, expiry, scope, n)`. **No separate `MoversEngine` struct.** 7 categories × 3 expiry × 5 scope (Dhan UI parity per L29) | crates/api, trading |
| 7 | **#507** Phase 4b drops | `DROP TABLE stock_movers, option_movers` (operator runbook gate) | runbook, sql |
| 8 | **#508** AWS deploy | Terraform `instance_type = c8g.xlarge` + Docker mem_limits per L2 + ARM AMI + aarch64 cross-compile CI + bench-budget re-baseline + OS tunes per L4 | deploy/aws/terraform, deploy/docker, .github/workflows |
| 9 | **#509** Static-universe cleanup | DELETE live-tick-atm-resolver, mid-market boot mode resolver beyond simple market-hours check, REST `/marketfeed/ltp` fallback, pre-open buffer ATM-strike machinery | crates/core/instrument |

PR #506 (cohort SQL migration) — KILLED per L14 (depth-dynamic reads from RAM `MoversEngine`).

---

## §P) Architecture simplification — only TWO data structures (NEW 2026-05-08)

Operator clarification 2026-05-08: "remove all movers related infrastructure entirely. We have ticks and candles alone — that's it."

The system has **exactly two in-memory data structures**:

| # | Structure | Type | Holds | Reset |
|---|---|---|---|---|
| 1 | **Tick map** | `papaya::HashMap<(security_id, exchange_segment), Vec<ParsedTick>>` | full day raw ticks per instrument | 09:15 IST |
| 2 | **Candle map (per TF)** | `papaya::HashMap<(security_id, exchange_segment), CandleEngineMap<TF>>` × 21 TFs | full day sealed bars per (instrument, TF) with all 5 % fields | 09:15 IST |

**Everything else is a query method on these two structures.**

| Old concept | New reality |
|---|---|
| `MoversEngine` separate struct | DELETED — top-N is `candle_storage.top_n_by(...)` method |
| `stock_movers` / `option_movers` QuestDB tables | DROPPED at Phase 4b |
| `OptionMoversWriter` / `StockMoversWriter` | DELETED PR #494 |
| `movers_1s` / `movers_1m` matviews | DROPPED at Phase 4b |
| Movers aggregation pipeline | NONE — reads happen at query time |
| Depth-dynamic separate selector | DELETED — calls `candle_storage.top_n_by(category=TopVolume, scope=NSE, n=250)` |
| `/api/movers/v2` business logic | ~30 LoC REST wrapper — no logic, just query forwarding |

### Filter dimensions match Dhan UI exactly (3, not 6)

| Dim | Values | Source in Dhan UI |
|---|---|---|
| **Category** | 7 enum values: HighestOI / OIGainers / OILosers / TopVolume / TopValue / PriceGainers / PriceLosers | top tab buttons |
| **Expiry** | date label, e.g. "26 MAY", "26 JUN", "26 JUL" | top-right dropdown |
| **Scope** | 5 mutually-exclusive: All / Index / Stock / NSE / BSE | far-right dropdown |

**Dropped from earlier draft (Dhan UI doesn't filter on these, neither do we):**

| Dropped | Why |
|---|---|
| Option type (CE/PE/nil) | not in Dhan UI |
| Underlying (NIFTY / BANKNIFTY / SENSEX) | not in Dhan UI as a primary filter |
| Strike range (ATM±25) | not in Dhan UI as a query filter (it's a subscription concept, not a display filter) |

### What this delivers

| Goal | How achieved |
|---|---|
| Movers infra removed entirely | only ticks + candles in memory; no separate engine; no separate writer; no separate matviews |
| Operator's "ticks and candles alone" demand | satisfied literally — just two data structures |
| Dhan UI parity | exact match on category × expiry × scope filter dimensions |
| Top-N latency | ~50 µs regardless of N (linear in universe size) |
| Depth-dynamic cohort | reuses same `candle_storage.top_n_by` — no separate selector code |
| Architectural simplification vs prior `MoversEngine` design | drops a struct, drops a façade, drops a separate file |

---

## §Q) Depth-20 + Depth-200 dynamic resubscribe (diff-based, NEW 2026-05-08)

Operator clarification 2026-05-08: "every 1 minute resubscribe — but ONLY the instruments that move OUT or IN of the top-N. Never resubscribe the whole cohort every cycle."

### Locked design — L37–L46

| # | Locked | Detail |
|---|---|---|
| **L37** | Re-evaluation cadence | **Once per 1m bar seal** — same trigger as candle seal. NOT a separate 60s wall-clock timer. Aligns naturally with the candle pipeline; no new clocks. |
| **L38** | Trigger source | At each `CandleEngine<1m>::seal_bar()` completion, the cascade emits a `BarSealedEvent`. The depth-cohort recomputer subscribes to this event and runs once per minute. |
| **L39** | Diff algorithm | `to_remove = current_set − new_set`; `to_add = new_set − current_set`. Empty diff = NO-OP (no wire frames sent). |
| **L40** | Cohort source | `candle_storage.top_n_by(category=TopVolume, expiry=current, scope=NSE, n=K)` — same primitive as `/api/movers`. K=250 for depth-20, K=5 for depth-200. |
| **L41** | SID → connection allocation | **Sticky allocation.** Each connection maintains its own `current_set`. (a) For removed SIDs: send `RemoveSubscription` frame on the connection that currently owns the SID. (b) For added SIDs: send `AddSubscription` frame on the least-loaded connection (fewest current SIDs). This minimizes churn — SIDs only move when they exit the cohort. |
| **L42** | Per-connection diff frame batching | All adds/removes for a single connection go into ONE frame batch (Dhan max 100 SIDs per JSON message per WS-GAP-02). Worst case 250 SIDs spread across 5 conns = 50 SIDs per conn, 1 frame each. |
| **L43** | Empty cohort handling | If `top_n_by` returns < K SIDs (e.g. early market hours, bear day), keep current subscriptions live (no removes). Edge-trigger Telegram **Severity::High** `DEPTH-DYN-V2-01 cohort_below_capacity` — but only on rising edge, not every cycle. |
| **L44** | Channel-broken handling | If `DepthCommand::AddSubscriptions20`/`RemoveSubscriptions20`/`Add200`/`Remove200` send fails (receiver task exited): Critical Telegram `DEPTH-DYN-02 swap_channel_broken`; pool supervisor (`respawn_dead_connections_loop`, WS-GAP-05) re-spawns within 5s. |
| **L45** | Audit | Each cycle writes one row to `depth_dynamic_diff_audit` table with `(ts, feed, adds_count, removes_count, current_set_size, target_set_size, latency_ms)` + DEDUP UPSERT KEYS `(ts, feed)`. SEBI-relevant. |
| **L46** | Telegram severity policy | Routine cycle (any non-zero diff): **Severity::Low** coalesced via 60s coalescer (Wave 3 Item 11). Empty diff: no Telegram. Cohort-empty / channel-broken: **Severity::High/Critical** edge-triggered. |

### What lands on the wire — typical and worst case

| Scenario | Frames sent per cycle (across all 5 conns) | Wire impact |
|---|---:|---|
| Steady state, no churn | 0 | NO-OP (no frames at all) |
| Typical morning (5–20 SIDs churn) | 5–20 add + 5–20 remove = 10–40 | minimal; ~5 ms total wire time |
| High churn (50 SIDs change between minutes) | 50 add + 50 remove = 100 | ~10 ms total |
| Worst case (full cohort change, all 250 swap) | 250 + 250 = 500 | ~50 ms; rare, only on volume regime shifts |
| Cold boot first cycle | 250 add (no removes) | ~25 ms |

### Why this beats "resubscribe everything every minute"

| Approach | Frames/cycle | Wire time | Dhan rate-limit risk |
|---|---:|---|---|
| Full re-subscribe of all 250 SIDs each minute | 500 (250 unsub + 250 sub) | ~50 ms | within limits but wasteful |
| **Diff-based (this design)** | typically 10–40 | ~5 ms | well within limits, ~10× less wire churn |

### Existing wiring (already designed in `active-plan-depth-dynamic-redesign.md`)

| Component | Status |
|---|---|
| `DepthCommand::Add/RemoveSubscriptions20` enum variants | ✅ shipped (PR-B/C1 of depth-dynamic redesign) |
| `DepthCommand::Add/RemoveSubscriptions200` | ✅ shipped |
| `DynamicSubscriptionState` diff state machine | ✅ shipped |
| `depth_dynamic_pipeline_v2` dead-code scaffold | ✅ shipped |
| `depth_dynamic_diff_audit` table DDL | ✅ shipped |
| `tv_depth_dynamic_diff_adds_total{feed}` Prom counter | ✅ shipped |
| `tv_depth_dynamic_diff_removes_total{feed}` | ✅ shipped |
| `tv_depth_dynamic_set_size{feed}` gauge | ✅ shipped |
| `tv-depth-dynamic-set-size-low` Prom alert | ✅ shipped |
| Grafana panels 47+48 (operator-health) | ✅ shipped |
| **Cutover to live wire (PR-C2)** | ⏳ PENDING operator approval |

### Sequence (1m bar seal → diff → wire)

```
T+0.000 ms — 1m bar seals (e.g. 09:16:00 IST)
T+0.005 ms — BarSealedEvent fires
T+0.010 ms — depth_dynamic_pipeline_v2 receives event
T+0.060 ms — candle_storage.top_n_by(TopVolume, NSE_FNO, current_expiry, 250) returns new_set
T+0.080 ms — DynamicSubscriptionState computes to_add (~10 SIDs) + to_remove (~10 SIDs)
T+0.085 ms — for each connection, dispatch AddSubscriptions20 / RemoveSubscriptions20 frames
T+0.090 ms — connection read-loop's select! receives DepthCommand and serializes to JSON
T+0.150 ms — TLS write completes, Dhan ACKs
T+5.000 ms — Dhan starts streaming for new SIDs; old SIDs stop
T+0.000 ms (next minute, 09:17:00 IST) — cycle repeats
```

### What this gives us

| Goal | Mechanism |
|---|---|
| Resub only SIDs moving IN/OUT | diff algorithm L39, sticky allocation L41 |
| 1-minute cadence | 1m bar seal trigger L37/L38 |
| Zero unnecessary wire churn | L39 empty-diff NO-OP |
| Safety on cohort empty | L43 + Severity::High edge-triggered |
| Safety on channel broken | L44 + supervisor respawn within 5s |
| SEBI audit trail | L45 `depth_dynamic_diff_audit` table |
| Operator visibility | L46 Severity policy + 7-layer observability |

### Blocking gate

PR-C2 (live wire cutover) of `active-plan-depth-dynamic-redesign.md` is **pending operator approval** because it touches `crates/app/src/main.rs` boot sequence. Folding this into PR #504d (in-memory store) means:
- L37–L46 land as part of the in-memory store wiring
- PR-C2 of the prior redesign plan becomes obsolete (rolled into #504d)

---

## §R) Dead-code removal + table-drop guarantee (NEW 2026-05-08)

Operator demand 2026-05-08: "ensure dead code removal and table drops precisely with guarantee — nothing left behind."

### Locked principles — L47–L52

| # | Locked | Detail |
|---|---|---|
| **L47** | Every removed item has a mechanical guard | banned-pattern hook prevents re-introduction; commit blocked at pre-commit gate |
| **L48** | Every dropped table has a runtime ratchet | test scans codebase + DDL for any reference to dropped table; build fails if found |
| **L49** | Every retired ErrorCode has a cross-ref guard | `error_code_rule_file_crossref.rs` + `error_code_tag_guard.rs` both must accept the deletion |
| **L50** | Every removed Prom metric has a dashboard cleanup | Grafana JSON files scanned for orphaned references; CI fails if found |
| **L51** | Operator runbook + verification SQL | step-by-step `DROP TABLE` commands + post-drop `SELECT count(*) FROM tables() WHERE name = ...` verification |
| **L52** | Audit trail for the cleanup | each dropped item logged in `cleanup_audit_log` table with `(ts, item_kind, item_name, sha_of_removing_pr)` for SEBI traceability |

### §R.1 — Complete inventory of dead code to remove (in PR #509)

| File / module | Reason | Action | Verification |
|---|---|---|---|
| `crates/core/src/instrument/live_tick_atm_resolver.rs` | Stock F&O ATM resolution; dead under indices-only | DELETE entirely | `test_live_tick_atm_resolver_file_does_not_exist` |
| `crates/core/src/instrument/preopen_rest_fallback.rs` | Stock F&O pre-open close fallback; dead under static universe | DELETE entirely | `test_preopen_rest_fallback_file_does_not_exist` |
| `crates/core/src/instrument/preopen_price_buffer.rs` | Stock F&O 09:00-09:12 buffer; dead under static universe | DELETE entirely | `test_preopen_price_buffer_file_does_not_exist` |
| `crates/core/src/instrument/boot_mode.rs::detect_boot_mode` | 4-mode resolver; only `is_within_market_hours_ist` is needed | SIMPLIFY — keep only the helper function; delete `BootMode` enum + `detect_boot_mode` + `MidMarketBootComplete` event | `test_boot_mode_enum_does_not_exist` |
| `crates/core/src/instrument/phase2_dispatcher.rs` (if separate) | Phase 2 subscription dispatcher; readiness check stays | DELETE dispatcher; KEEP only `phase2_readiness_check.rs` | `test_phase2_dispatcher_file_does_not_exist` |
| `crates/app/src/main.rs` boot wiring | calls to deleted resolvers | REMOVE corresponding boot calls; keep readiness check spawn | `test_main_does_not_call_live_tick_atm_resolver` |
| 1s engine in `CandleEngineMap` | seconds TFs dropped | REMOVE `1s` from `[engine.timeframes].list`; `CascadeFanout` no longer instantiates 1s engine | `test_no_seconds_engines_in_cascade` |
| 3s/5s/10s/15s/30s engines | seconds TFs dropped | same as above | same |
| Phase2 unified dispatcher hand-off (if any code path) | no dispatcher under static | confirm by grep — should be zero refs | `test_no_phase2_dispatcher_call_in_main` |
| `Bar` struct fields removed (none) | All 5 new fields are ADDS, no removals | n/a | n/a |
| `MoversEngine` struct (if present in any file) | replaced by `candle_storage.top_n_by` | confirm absent — should never have been a struct | `test_no_movers_engine_struct_in_codebase` |

### §R.2 — Tables to DROP (QuestDB)

| Table | Reason | When | Operator runbook step |
|---|---|---|---|
| `stock_movers` | Phase 4b — replaced by RAM | PR #507 | `DROP TABLE IF EXISTS stock_movers` (after 30-day clean window per existing runbook) |
| `option_movers` | Phase 4b — replaced by RAM | PR #507 | `DROP TABLE IF EXISTS option_movers` (same window) |
| `candles_1s` materialized view | seconds TF dropped | PR #504c | `DROP MATERIALIZED VIEW IF EXISTS candles_1s` |
| `candles_3s` matview | same | PR #504c | same |
| `candles_5s` matview | same | PR #504c | same |
| `candles_10s` matview | same | PR #504c | same |
| `candles_15s` matview | same | PR #504c | same |
| `candles_30s` matview | same | PR #504c | same |
| `movers_1s` matview (if exists) | already DROPPED at Phase 4b | confirm | `SELECT count(*) FROM tables() WHERE name LIKE 'movers%'` should return 0 |
| `movers_5s` ... `movers_1d` matviews | already DROPPED at Phase 4b | confirm | same |

### §R.3 — Verification SQL (mandatory in operator runbook)

```sql
-- After all DROPs, these queries MUST return zero rows:
SELECT count(*) FROM tables() WHERE name = 'stock_movers';      -- expect: 0
SELECT count(*) FROM tables() WHERE name = 'option_movers';     -- expect: 0
SELECT count(*) FROM tables() WHERE name LIKE 'movers%';        -- expect: 0
SELECT count(*) FROM tables() WHERE name LIKE 'candles_1s%';    -- expect: 0
SELECT count(*) FROM tables() WHERE name LIKE 'candles_3s%';    -- expect: 0
SELECT count(*) FROM tables() WHERE name LIKE 'candles_5s%';    -- expect: 0
SELECT count(*) FROM tables() WHERE name LIKE 'candles_10s%';   -- expect: 0
SELECT count(*) FROM tables() WHERE name LIKE 'candles_15s%';   -- expect: 0
SELECT count(*) FROM tables() WHERE name LIKE 'candles_30s%';   -- expect: 0

-- Surviving candles matviews (21 expected):
SELECT name FROM tables() WHERE name LIKE 'candles_%' ORDER BY name;
-- expect: candles_1d, candles_1h, candles_1m, candles_2m, ..., candles_15m, candles_30m, candles_2h, candles_3h, candles_4h
```

### §R.4 — Mechanical guards (banned-pattern hook additions)

Add to `.claude/hooks/banned-pattern-scanner.sh` category 7 (NEW):

| Pattern | Block reason | Where |
|---|---|---|
| `MoversEngine\s*\{` (struct definition) | replaced by query method on candle storage | any prod `*.rs` |
| `OptionMoversWriter\b\|StockMoversWriter\b` | deleted at PR #494 | any prod `*.rs` |
| `live_tick_atm_resolver\b` | deleted at PR #509 | any prod `*.rs` |
| `preopen_rest_fallback\b` | deleted at PR #509 | any prod `*.rs` |
| `BootMode::(PreMarket\|MidPreMarket\|MidMarket\|PostMarket)` | enum dropped | any prod `*.rs` |
| `detect_boot_mode\s*\(` | function dropped | any prod `*.rs` |
| `Phase2Dispatcher\b` | dispatcher dropped | any prod `*.rs` |
| `"1s"\|"3s"\|"5s"\|"10s"\|"15s"\|"30s"` in `[engine.timeframes]` context | seconds TFs dropped | `config/*.toml` |
| `candles_1s\b\|candles_3s\b\|candles_5s\b\|candles_10s\b\|candles_15s\b\|candles_30s\b` | matview names dropped | any `*.rs`, `*.sql`, `*.json` |
| `stock_movers\b\|option_movers\b` (writes only — reads in audit/migration scripts allowed under `// APPROVED:` comment) | tables dropped | any prod `*.rs`, `*.sql` |

### §R.5 — Ratchet tests (mandatory in PR #509 + #504c + #507)

| Test | What it asserts | File |
|---|---|---|
| `test_live_tick_atm_resolver_deleted` | file does not exist | `crates/core/tests/static_universe_cleanup_guard.rs` |
| `test_preopen_rest_fallback_deleted` | file does not exist | same |
| `test_preopen_price_buffer_deleted` | file does not exist | same |
| `test_boot_mode_enum_deleted` | grep returns no match | same |
| `test_phase2_dispatcher_deleted` | file does not exist | same |
| `test_no_seconds_engines_in_cascade` | `CascadeFanout::engines()` does not contain 1s/3s/5s/10s/15s/30s | `crates/trading/tests/cascade_seconds_dropped_guard.rs` |
| `test_no_movers_engine_struct` | grep `crates/**/src` returns no `pub struct MoversEngine` | `crates/storage/tests/no_movers_engine_guard.rs` |
| `test_dropped_tables_not_referenced` | scan all `.rs` + `.sql` + `.json` for `stock_movers`/`option_movers`; only `// APPROVED:` annotated lines pass | `crates/storage/tests/dropped_tables_reference_guard.rs` |
| `test_dropped_matviews_not_in_ddl` | scan storage DDL strings for `candles_1s`/`3s`/`5s`/`10s`/`15s`/`30s`; expect 0 | `crates/storage/tests/dropped_matviews_ddl_guard.rs` |
| `test_grafana_dashboards_no_orphan_metrics` | scan `deploy/docker/grafana/dashboards/*.json` for `tv_movers_writer_*`/`candles_1s_*` panels; expect 0 | `crates/storage/tests/grafana_orphan_metric_guard.rs` |
| `test_error_code_movers_retired` | `MOVERS-01`/`02`/`03` not in `ErrorCode` enum | already exists in `crates/common/src/error_code.rs` ratchet |

### §R.6 — Operator runbook (NEW file)

A new runbook `docs/runbooks/dead-code-and-tables-removal.md` will land alongside PR #509 with:
- Step-by-step DROP commands in execution order
- Pre-conditions (Phase 4a/4b clean window confirmation)
- Verification SELECTs after each DROP
- Rollback plan (none — DROPs are irreversible; backups via S3 archive of partitions taken in `s3_archive` Wave 2 Item 9.4)
- 30-day post-DROP clean-window confirmation gate (per existing `phase-4b-movers-cleanup.md`)

### §R.7 — Cleanup audit log table (NEW)

```sql
CREATE TABLE IF NOT EXISTS cleanup_audit_log (
    ts TIMESTAMP,
    item_kind SYMBOL CAPACITY 16,           -- 'table_drop' | 'matview_drop' | 'file_delete' | 'metric_remove' | 'errorcode_retire' | 'guard_add'
    item_name SYMBOL CAPACITY 256,
    pr_sha SYMBOL CAPACITY 64,
    operator_initials SYMBOL CAPACITY 8,
    notes STRING
) timestamp(ts) PARTITION BY MONTH WAL DEDUP UPSERT KEYS(ts, item_kind, item_name);
```

Every cleanup PR (#504c, #507, #509) writes one row per dropped item to this table at deploy time. SEBI-relevant trail.

### §R.8 — Updated PR sequence (cleanup-aware)

| # | PR | Cleanup scope |
|---|---|---|
| #504a | observability prereq | none — adds `tv_subsystem_memory_bytes` |
| #504b | schema + struct | none — adds 5 new Bar fields |
| #504c | TF list config | **DROP `candles_1s/3s/5s/10s/15s/30s` matviews + remove seconds engines from CascadeFanout + add ratchet `test_no_seconds_engines_in_cascade`** |
| #504d | in-memory store | none — adds tick + bar storage |
| #504e | % stamping | none |
| #505 | read flip | none — adds REST wrapper |
| #507 | Phase 4b drops | **DROP `stock_movers` + `option_movers` per existing operator runbook + add `test_dropped_tables_not_referenced`** |
| #508 | AWS deploy | none — infra commit |
| **#509 (NEW)** | **static-universe cleanup** | **DELETE `live_tick_atm_resolver.rs` + `preopen_rest_fallback.rs` + `preopen_price_buffer.rs` + simplify `boot_mode.rs` + remove main.rs boot calls + add 5 ratchet tests + banned-pattern category 7** |

### What this guarantees

| Goal | Mechanism |
|---|---|
| Every dead file deleted | per-file ratchet test asserts non-existence |
| Every dropped table gone | post-DROP SELECT verification + ratchet scanning DDL |
| No re-introduction possible | banned-pattern category 7 blocks at pre-commit gate |
| No dangling Prom/Grafana refs | dashboard scanner ratchet |
| SEBI audit trail | `cleanup_audit_log` table per item |
| Operator can verify | runbook with SQL post-drop verification |

---

**End of DRAFT v6.** Awaiting OPEN-VERIFY-1 + OPEN-VERIFY-2 closure for APPROVED status.
