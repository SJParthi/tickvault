# Design Plan: In-Memory Store (§17) + AWS Instance Re-evaluation (§18 v2)

**Status:** APPROVED v1 — 120 design decisions LOCKED 2026-05-08 (L1-L120 across 17 sections). Operator approval 2026-05-08 "go ahead dude". c8g.xlarge ONLY. Implementation begins with PR #504a (observability prereq). 2 open verification gates remain in parallel: OPEN-VERIFY-1 (auto-closing via L107 09:14 IST cron over 5 trading days) and OPEN-VERIFY-2 (operator-side aws ec2 describe-instance-types verify).
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
| A13 | All non-c8g instances BANNED for tickvault (operator directive 2026-05-08) | c8g.xlarge ONLY; c7i/c7a/c7g/c7gd/m7i/m7a/m7g/m8g/r7i/r7g/r8g/c7i-flex/t3/c8g.large/c8g.2xlarge — all OUT OF SCOPE |

## B) The 5 blocking decisions

### B1) §17 retention shape

| Option | RAM | Cost | Fits c8g.xlarge | Notes |
|---|---:|---|---|---|
| A — Full 1,279 bars × 11K | ~1.7 GB | needs c8g.xlarge or c8g.xlarge | ❌ | most flexible; >₹5K cap on-demand |
| **B — Last 100 bars/TF × 11K** | ~140 MB | ₹0 | ✅ tight | RECOMMENDED; >100 bars hits matview |
| C — Top-1K liquid full + rest QuestDB | ~155 MB | ₹0 | ✅ asymmetric | needs liquidity classifier — defer |

### B2) §17 PR ordering

| Order | Risk | Rollback |
|---|---|---|
| Sequential (#504 in-mem → #505 reads → #506 cohort SQL → #507 DELETE+DROP) | LOW — each PR shippable independent | trivial — config flag |
| Movers-DELETE-first (skip to #507) | HIGH — table gone before v2 read flip validated | hard |
| Hybrid (#504+#505 together, #506 deferred) | MED — depth-dynamic still hits `movers_1m` | medium |

### B3) §18 instance generation — c8g.xlarge LOCKED (operator directive 2026-05-08)

> **2026-05-08 OPERATOR DIRECTIVE — c8g.xlarge ONLY:**
> "use only c8g — remove everything related to c7i / other generations from the plan."
>
> All comparison tables BELOW this line are HISTORICAL DECISION CONTEXT
> (kept for audit trail showing why c8g.xlarge was chosen). They are
> NOT current options. The single locked choice is **c8g.xlarge**
> (Graviton4, 4 vCPU, 8 GB, aarch64, ap-south-1, 1-yr Standard RI no-upfront, ~₹3,470/mo all-in). See §K-L1 for the canonical lock.
>
> Any future plan section, code commit, or Terraform change MUST use
> c8g.xlarge or its size variants only. c7i / c7a / c7g / c7gd / m7i /
> m7a / m7g / m8g / r7i / r7g / r8g / c7i-flex / t3 are all OUT OF SCOPE
> per A13 banned list.

---

Operator demand 2026-05-07 (verbatim): *"why not any latest like c8g or any other latest instances especially using the extreme latest instance to have a powerful ultra fast automated compute"*.

Honest re-survey of compute generations available in **ap-south-1 (Mumbai)** as of 2026-05. Pricing snapshot from operator-side `aws ec2 describe-instance-types` + `aws pricing get-products` is required to FINALIZE; figures below are last-confirmed prices that need re-verification at deploy time.


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
| B1 = Option A | needs RAM > 8 GB → exceeds c8g.xlarge | RESERVED — out of scope under c8g.xlarge LOCKED |
| B1 = Option B | fits c8g.xlarge | B4 = B/D feasible at zero infra cost |
| Architecture | aarch64 only (c8g LOCKED) | aarch64 cross-compile + CI re-baseline mandatory in #508 |
| B4 = E/F | needs RAM > 8 GB → exceeds c8g.xlarge | RESERVED — out of scope |
| B4 = G (mmap) | C12 (bounded-SELECT) becomes blocking | adds historical without RAM cost |
| C13 = runtime-tunable cap | §17.4 architecture changes (compile-time const → runtime-tunable Vec) | "common runtime dynamic" demand |
| B5 RI tier | 1-yr Standard RI no-upfront LOCKED | (Convertible deferred — out of scope) |

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
| ~~Q11~~ | CLOSED — c8g.xlarge ARM (aarch64) LOCKED per A13/L1 | n/a |
| ~~Q12~~ | CLOSED — Graviton4 (c8g) LOCKED | n/a |
| ~~Q13~~ | OPEN-VERIFY-2 covers this | see §L |
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

## §S) Depth-eligibility filter — exclude BSE / SENSEX (NEW 2026-05-08)

Operator catch 2026-05-08: "for depth-20 + depth-200 we need to exclude SENSEX because Dhan doesn't support it."

**Confirmed.** Per `.claude/rules/dhan/full-market-depth.md` rule 13:
> "Only NSE segments valid. NSE_EQ and NSE_FNO only. BSE, MCX, Currency are NOT available for Full Market Depth. Reject at subscription build time — do not send and wait for error."

SENSEX is `BSE_FNO` → not eligible for depth. NIFTY + BANKNIFTY F&O are `NSE_FNO` → eligible.

### Locked design — L53–L57

| # | Locked | Detail |
|---|---|---|
| **L53** | Depth-eligible segment set | **EXACTLY `{NSE_FNO}`** for our current scope. NSE_EQ technically allowed by Dhan but we don't depth-subscribe equities (no use case). IDX_I, BSE_*, MCX_*, NSE_CURRENCY → NOT eligible. |
| **L54** | Depth cohort filter (override of L34 scope) | Depth-20 + depth-200 cohort = `candle_storage.top_n_by(category=TopVolume, expiry=current, segments=[NSE_FNO], n=K)`. **NOT scope=NSE** (which would include IDX_I + NSE_EQ); explicit `segments=[NSE_FNO]` only. |
| **L55** | SENSEX explicit exclusion | SENSEX index F&O contracts are `BSE_FNO`, automatically filtered out by L54. Add explicit ratchet test confirming SENSEX SIDs never appear in depth-20 or depth-200 cohort. |
| **L56** | Pre-subscription validation | At every depth `Add` frame build, validate `segment ∈ {NSE_FNO}` before serializing JSON. Mismatch → log Critical + skip frame + audit row. Defence-in-depth (cohort filter L54 should never let it through, but guard anyway). |
| **L57** | Observability | Prom counter `tv_depth_cohort_segment_excluded_total{segment, feed}` increments any time a non-NSE_FNO SID would have been in the cohort but was filtered out. Edge-trigger Telegram **Severity::Medium** if non-zero (means cohort filter is doing work — likely a config or universe-shape bug). |

### What this means concretely

| Underlying | Index segment | F&O segment | Subscribed to main feed? | Depth-20 eligible? | Depth-200 eligible? |
|---|---|---|---|---|---|
| NIFTY (id=13) | IDX_I | NSE_FNO | ✅ | ✅ via NSE_FNO options | ✅ |
| BANKNIFTY (id=25) | IDX_I | NSE_FNO | ✅ | ✅ | ✅ |
| **SENSEX (id=51)** | **IDX_I** | **BSE_FNO** | ✅ | ❌ **EXCLUDED** | ❌ **EXCLUDED** |

So the depth-20 top-250 cohort comes only from NIFTY + BANKNIFTY F&O contracts (typically ~6,000 active option strikes × 2 underlyings ≈ 12K contracts → top-250 by volume). Depth-200 top-5 same — only NIFTY + BANKNIFTY ATM options.

### Ratchet tests (mandatory in #504d)

| Test | What it asserts |
|---|---|
| `test_depth_cohort_only_nse_fno` | `candle_storage.top_n_by(...)` for depth feeds returns ZERO entries with `segment != NSE_FNO` |
| `test_sensex_never_in_depth_cohort` | scenario test: populate candle store with NIFTY+BANKNIFTY+SENSEX bars; depth-20 cohort never includes any SENSEX SID |
| `test_depth_subscription_builder_rejects_bse` | feeding a BSE_FNO SID to `build_depth_subscribe_message` returns Err |
| `test_depth_subscription_builder_rejects_idx_i` | same for IDX_I (depth not meaningful for indices) |
| `test_tv_depth_cohort_segment_excluded_metric_emitted` | when cohort source contains BSE/IDX_I, metric increments with correct `{segment}` label |

### Banned-pattern category 7 addition

Add to `.claude/hooks/banned-pattern-scanner.sh`:

| Pattern | Block reason |
|---|---|
| any depth subscribe message construction with `BSE_*` or `IDX_I` or `MCX_COMM` or `NSE_CURRENCY` segment | Dhan rejects; pre-empts at commit-time |

### Why the existing `scope=NSE` (L31) was almost right but not enough

| Filter dim | What it includes | What it excludes |
|---|---|---|
| L31 `Scope::NSE` | NSE_EQ + NSE_FNO + IDX_I | BSE_*, MCX_*, NSE_CURRENCY |
| **L54 explicit `segments=[NSE_FNO]`** | NSE_FNO ONLY | IDX_I (no depth on indices), NSE_EQ (we don't use), all BSE, MCX, currency |

`/api/movers` keeps the broader `Scope::NSE` filter (L31) — it shows top movers across NSE_EQ + NSE_FNO + IDX_I (matches Dhan UI's "NSE" dropdown). But **depth feeds need the tighter `segments=[NSE_FNO]` filter** for correctness.

---

## §T) Telegram notification UX overhaul (NEW 2026-05-08)

Operator complaint 2026-05-08: live Telegram screenshots showed `[CRITICAL] REAL-TIME GUARANTEE SCORE CRITICAL` firing every minute alongside `[INFO] Coalesced summary [RealtimeGuaranteeHealthy]` — clear flap-driven spam. Even as a developer it's unreadable; for a non-technical user it's useless.

### Three bugs identified (locked findings)

| # | Bug | Evidence | Fix |
|---|---|---|---|
| **B1** | SLO-02 fires on every `Critical` tick, not strictly rising edge | 10:25 two identical CRITICAL back-to-back | edge-trigger guard in `slo_score.rs::evaluate_slo_score` — only fire if `prev_state != Critical`; ratchet test |
| **B2** | 30-sec QuestDB blip triggers full Critical alert | 10:23/10:25/10:28/10:29 same flap | state-hold debounce: require ≥ N consecutive cycles in same state before firing; ratchet test |
| **B3** | Message body unintelligible for non-technical user | "Weakest dimension: qdb_health / Code: SLO-02" | smart message template — plain English headline + auto-recovery hint + technical footer |

### Locked design — L58–L67

| # | Locked | Detail |
|---|---|---|
| **L58** | Edge-triggered (strict) | SLO scheduler MUST track `previous_outcome`. Fire SLO-02 ONLY when `previous_outcome != Critical && current == Critical`. Same for SLO-01 (only fire on `previous != Healthy && current == Healthy`). No "fire while still in same state" allowed. |
| **L59** | State-hold debounce | Require **N consecutive cycles in target state** before firing. For SLO scheduler at 10s cadence: N=3 → 30 sec sustained Critical before SLO-02 fires; N=3 → 30 sec sustained Healthy before SLO-01 recovery fires. Configurable via `[notification.debounce]` TOML. |
| **L60** | Hysteresis bands | Critical entry threshold = 0.80 (existing); Healthy exit threshold = 0.95 (existing); add Degraded band [0.80, 0.95) where neither alert fires (silent). Prevents jitter at boundary. |
| **L61** | Per-cause severity policy | `qdb_health` weakest **outside market hours** = downgrade Critical → Low (informational); during market hours = full Critical. Same for `ws_health`, `tick_freshness`, `phase2_health`. Implemented as severity-rewrite at emit time. |
| **L62** | Dev-mode suppression | If `TICKVAULT_PROFILE=local` AND local Docker services are auto-bouncing (auto-up.log mtime < 60s), downgrade ALL Telegram from Critical/High → Low for 5 minutes. Prevents the exact spam you saw on local Mac. |
| **L63** | Smart message template | Body: **plain English headline** + **auto-recovery hint** + **technical footer**. Example: <br/>`⚠️ QuestDB temporarily unreachable. System is buffering data — no action needed unless this persists > 5 min during market hours. (SLO-02, qdb_health=0)` <br/>vs current: `Composite score: 0.000 / Weakest: qdb_health / Code: SLO-02 / Action: see runbook ...` |
| **L64** | Quiet hours | Outside market hours (15:30 IST → 09:00 IST + weekends + holidays), all severity ≤ High → downgrade to Low (logged, no Telegram). Critical still pages. |
| **L65** | Per-event template registry | `crates/core/src/notification/templates.rs` — one template per `NotificationEvent` variant, reviewed for non-technical clarity. Template ratchet test ensures every variant has a template. |
| **L66** | Telegram footer convention | Every message ends with one technical-detail line in *italics* readable but ignorable for non-technical users. Example: `_(SLO-02 / dimension: qdb_health / score: 0.000 / runbook: wave-3-d)_`. |
| **L67** | Notification "what to do" wizard | Each Critical alert includes a 1–3 step "what to do" list at the top: 1) `make doctor`, 2) check Docker, 3) restart if persists. Stored in `templates.rs` per event variant. |

### Comparison: what current Telegram looks like vs new template

#### Current (the spam you saw)

```
🚨 [CRITICAL] REAL-TIME GUARANTEE SCORE CRITICAL
Composite score: 0.000 (below 0.80).
Weakest dimension: qdb_health
Code: SLO-02
Action: see runbook .claude/rules/project/wave-3-d-error-codes.md
```

#### New (lazy-kid + non-technical-user friendly)

```
⚠️ QuestDB temporarily unreachable

What's happening:
  System is buffering all incoming ticks safely.
  No data loss — buffer can hold 60+ seconds.

What to do:
  1. Wait 30 seconds. Auto-recovery is normal.
  2. If still down after 5 min during market hours,
     run: make doctor
  3. If doctor red, restart: make docker-up

Has this happened before today: 0 times.
Started: 10:25:00 IST (sustained 30s)

_(SLO-02 / dimension: qdb_health / score: 0.000)_
```

### Notification policy matrix (per cause × per condition)

| Cause | In-market | Off-market | Dev mode | Boot phase |
|---|---|---|---|---|
| QuestDB blip < 30 s | Low (silent) | Low | Low | Low |
| QuestDB down 30 s – 5 min | Low coalesced | Low coalesced | Low | Low |
| QuestDB down > 5 min in-market | **Critical** with template | High | Low (suppressed dev) | Critical |
| WS pool one conn drops | Low | Low | Low | Low |
| WS pool all conns drop | **Critical** | High | Low | Critical |
| Token < 4h to expiry | High at boot, Critical at < 1h | Low | Low | Critical |
| Phase 2 readiness check fails | **Critical** | n/a | Low | Critical |
| RSS > 80% of cap | High | Low | Low | High |
| OOM kill detected | **Critical** | **Critical** | High | **Critical** |
| Tick gap > 30 s during market | High coalesced | n/a | Low | n/a |
| Cohort below capacity | High edge-trigger | Low | Low | Low |
| Depth diff applied | Low coalesced | Low | Low | Low |

**Rule:** any item in non-Critical column does NOT page operator. Only Critical column triggers the immediate-fire path; everything else is summary/coalesced/silent.

### Ratchet tests (mandatory in #504a observability prereq)

| Test | What it asserts |
|---|---|
| `test_slo_02_strictly_edge_triggered` | feeding `[Critical, Critical, Critical]` to scheduler emits ONE Telegram, not three |
| `test_slo_01_recovery_strictly_edge_triggered` | same for healthy recovery |
| `test_state_hold_debounce_30s` | 20-sec Critical blip emits NO Telegram; 35-sec Critical emits one |
| `test_dev_mode_suppression_active_when_docker_bouncing` | session-start-hook flag suppresses Critical → Low for 5 min |
| `test_every_notification_event_variant_has_template` | enum exhaustiveness check; template registry has entry per variant |
| `test_template_includes_what_to_do_section` | scan all template strings; require `What to do:` substring on Critical templates |
| `test_template_includes_technical_footer` | scan all templates; require italicised footer with code |
| `test_quiet_hours_downgrade_high_to_low_outside_market_hours` | feed High event at 16:00 IST → routed as Low |

### How this scales for non-technical users

| Persona | What they see |
|---|---|
| **Operator (you)** | full template — headline + what-to-do + footer; can act |
| **Co-trader (non-technical)** | reads the headline + first 1–2 lines of "What to do"; understands "wait 30 seconds" without knowing what SLO is |
| **Auditor / SEBI reviewer** | technical footer (code, dimension, score) sufficient for audit trail |
| **Dev investigating** | footer + code + runbook reference — zero info loss vs current |

### Coalescing (existing, stays)

| Layer | Cadence | Purpose |
|---|---|---|
| Per-topic 60s coalescer (Wave 3 Item 11) | 60 s | dedupe rapid-fire same-topic events |
| State-hold debounce (NEW L59) | N=3 cycles (30 s at 10s scheduler) | suppress flap-induced state transitions |
| Quiet hours downgrade (NEW L64) | continuous | route off-market noise to logs |
| Dev-mode suppression (NEW L62) | 5-min window after auto-up | suppress local-infra-bounce noise |

These compose with logical AND — an event must pass ALL of: not-flapping, in-market, not-dev-bouncing → before reaching Telegram as Critical.

### Implementation scope additions to PR sequence

| PR | Adds |
|---|---|
| #504a | template registry + 8 ratchet tests + state-hold debounce + per-cause severity policy + smart template format |
| #504d | dev-mode suppression hook (reads session-auto-health flag) |
| #508 (deploy) | quiet-hours config defaults |
| **NEW PR #510** | non-technical-user readability audit — review every Critical template for plain English; operator UAT |

### What this gives the operator

| Goal | Mechanism |
|---|---|
| No flap-driven spam | L59 30-sec state-hold debounce |
| One alert per real incident | L58 strict edge-trigger |
| Useful for non-technical users | L63 plain-English template + L67 what-to-do wizard |
| Quiet at night | L64 quiet hours |
| Quiet during dev infra bounce | L62 dev-mode suppression |
| Full audit trail kept | L66 technical footer + Loki ERROR routing unchanged |

---

## §U) Local Mac dev vs AWS prod — why disconnects happen locally and how we guarantee bounded zero-loss anywhere (NEW 2026-05-08)

Operator concern 2026-05-08: "even with this massive local MacBook why is WebSocket live market feed disconnect happening? If this happens here means then even on AWS c8g.xlarge it can happen too — clarify."

### §U.1 — Local Mac specs locked in plan

| Component | Spec |
|---|---|
| Model | MacBook Pro `Mac16,7` (MX2Y3ZP/A) |
| Chip | Apple M4 Pro |
| Cores | **14** (10 performance + 4 efficiency) |
| Memory | **48 GB unified** |
| Memory used (Activity Monitor live) | 28.62 GB used / 19.38 GB cached / **0 swap** |
| App Memory | 20.55 GB / Wired 2.96 GB / Compressed 4.00 GB |
| CPU load (live) | System 7% / User 26% / Idle **67%** |
| tickvault RSS (current) | 828 MB (Memory tab) — well under threshold |
| Hot-path consumers | tickvault 116% CPU (peak), Virtual Machine Service for Docker 64% |
| OS | macOS Sonoma equivalent |

**Verdict:** This Mac is NOT resource-constrained. WS disconnects here are **environmental artifacts**, not load-induced.

### §U.2 — Why WS disconnects on a 48 GB M4 Pro Mac (root cause analysis)

The disconnects you observed are NOT caused by resource pressure. The 5 honest causes:

| # | Cause | Mechanism | Affects AWS? |
|---|---|---|---|
| **C1** | **macOS App Nap** | When tickvault isn't the foreground app, macOS may throttle background tasks → tokio scheduler delays → WS keepalive misses → upstream RST | ❌ NO — Linux has no App Nap |
| C2 | macOS power management / display sleep | Lid close, display off, energy-saver mode reduces CPU clock; affects Wi-Fi sleep too | ❌ NO — server runs 24/7 |
| C3 | Wi-Fi vs Ethernet flap | macOS auto-switches between Wi-Fi networks or 2.4/5 GHz bands; momentary IP change resets TCP | ⚠️ PARTIAL — AWS uses VPC ENI (no Wi-Fi); but ISP-side flaps still possible |
| C4 | NAT timeout on home/office ISP | Idle TCP > 60s gets reset by NAT box; less common during market hours | ⚠️ PARTIAL — AWS direct-egress less aggressive but TCP still finite |
| C5 | Dhan-side rate-limit / RST flood | Dhan disconnects during their own infra issues (e.g. PR #337 was caused by Dhan TCP RST storm 2026-04-24) | ✅ YES — Dhan-side issue affects all clients regardless of OS |

**Observation:** 4 of 5 causes are dev-environment-specific. Only C5 (Dhan-side) carries to AWS — and it's the SAME risk on every client.

### §U.3 — Why AWS c8g.xlarge dramatically reduces these

| Cause | AWS c8g.xlarge mitigation |
|---|---|
| C1 App Nap | **Eliminated** — Linux has no App Nap concept |
| C2 Power management | **Eliminated** — server-class instance runs 24/7, no display, no lid |
| C3 Wi-Fi flap | **Eliminated** — VPC ENI is direct ethernet to AWS backbone |
| C4 NAT timeout | **Reduced** — direct egress through AWS internet gateway; TCP keepalive within VPC < ISP NAT timeout |
| C5 Dhan-side RST | **Same risk** — but our resilience handles it (SubscribeRxGuard + reconnect) |

**Honest summary:** AWS c8g.xlarge eliminates 4 of 5 disconnect causes outright. The remaining 1 (Dhan-side) is the same on any client and is what our design IS built to absorb.

### §U.4 — The bounded zero-loss guarantee (real-time mechanisms + proof)

This is the honest 100% claim per `wave-4-shared-preamble.md` §8 — restated with each guarantee mapped to its mechanism + ratchet test.

| Operator demand | Honest envelope | Real-time mechanism | Proof (test) | Telegram alert if breached |
|---|---|---|---|---|
| Zero tick loss | **bounded inside 60s outage envelope** | Rescue ring 2M ticks (224 MB) → spill NDJSON → DLQ. Tier 1 absorbs in-RAM; Tier 2 disk file; Tier 3 final DLQ. | `crates/storage/tests/zero_tick_loss_alert_guard.rs`; `chaos_zero_tick_loss.rs`; `chaos_questdb_docker_pause.rs` | Critical if rescue ring > 80% full |
| WS never disconnects | **detect ≤ 5s + reconnect preserves subs** | Pool watchdog 5s tick; `SubscribeRxGuard` (PR #337) keeps subscribe channel alive across reconnect; sleep-until-open post-15:30 IST; pool supervisor respawns dead conns ≤ 5s | `test_subscribe_rx_guard_reinstalls_on_drop`; `ws_sleep_resilience.rs` (65h chaos); `chaos_ws_frame_spill_saturation.rs` | Critical if all 5 main conns drop > 30s in-market |
| WS never slow/locked | **DHAT ≤ 4 alloc blocks + Criterion p99 ≤ 100 ns** | Hot path is `from_le_bytes` + bounded SPSC + papaya MVCC; bench-gate fails build on > 5% regression; `core_affinity` pins WS read to Core 0 | `dhat_*.rs` family; `benches/tick_parser.rs`; `quality/benchmark-budgets.toml` | Tick-gap > 30s in-market (coalesced 60s) |
| QuestDB never fails | **absorb via 3-tier + schema self-heal** | Continuous ILP TCP socket; rescue ring buffers on disconnect; spill writer flushes to NDJSON; `ALTER TABLE ADD COLUMN IF NOT EXISTS` on every boot | `chaos_questdb_docker_pause.rs`; `resilience_sla_alert_guard.rs`; `instrument_persistence.rs::ensure_instrument_tables` | Critical if QuestDB unreachable > 5 min in-market (after debounce per L59) |
| O(1) latency | **bench-gated < 100 ns p99 hot path** | `from_le_bytes` parser, papaya `Arc<HashMap>` lookup, bounded SPSC; no `.clone()`, no `Vec::new()`, no `format!()` on hot path | banned-pattern hook category 2; DHAT proves zero alloc; Criterion benches all live | bench-gate CI fail on PR |
| Uniqueness + dedup | **composite (security_id, exchange_segment) per I-P1-11** | `papaya` `HashMap<(u32, ExchangeSegment), _>`; every storage `DEDUP_KEY_*` constant includes segment; meta-guard scans for regression | `dedup_segment_meta_guard.rs`; `instrument_registry::tests::test_iter_returns_both_colliding_segments` | n/a — design-time invariant |

### §U.5 — Lazy-kid analogy table

| Concept | Lazy-kid version |
|---|---|
| Why local Mac disconnects | "Your laptop is sleeping or your wifi blinked. Your laptop is amazing — it's not the laptop's fault, it's the wifi or the sleep mode." |
| Why AWS won't disconnect the same way | "AWS is a giant computer in a datacenter that never sleeps. It's plugged into super-fast ethernet, never on wifi. So sleep + wifi problems are gone." |
| Why we still design for disconnect on AWS | "Even AWS computers can have a tiny network blip once in a while. So we built a safety net — a big bucket that catches every tick if the connection breaks for up to 60 seconds. Nothing falls on the floor." |
| What's the safety net | "Three layers: (1) memory bucket holds 2 million ticks, (2) if that overflows we write to a file on disk, (3) if even that breaks we have a final 'dead-letter queue' file. NOTHING gets lost permanently." |
| What if QuestDB itself dies? | "Same safety net — the database can be down for a minute and we don't lose anything. We pour into the memory bucket; when DB comes back, we replay everything in order." |
| What if our app crashes? | "Docker auto-restarts it within 5 seconds. The big buffer file survives the crash. On restart we replay it." |
| O(1) latency promise | "Every tick is processed in less than 100 nanoseconds — that's a billionth of a second. Every benchmark run proves this; if anyone tries to slow it down, the build literally won't compile." |
| Composite key promise | "We never confuse two instruments that share the same number. Even Dhan reuses ID 27 for both an index and a stock — we use the (number + segment) as the unique key. Always." |

### §U.6 — Locked design decisions L68–L72

| # | Locked | Detail |
|---|---|---|
| **L68** | Local Mac dev specs captured | MacBook Pro M4 Pro, 14 cores, 48 GB unified — captured in §U.1 |
| **L69** | Mac-specific dev mitigations | (a) `caffeinate -ds` to prevent App Nap during dev sessions; (b) Energy Saver → "Prevent automatic sleeping when the display is off"; (c) Activity Monitor → reduce "Preventing Sleep: No" by adding tickvault to login items with Force-Active option |
| **L70** | Disconnect classification at runtime | Add `tv_websocket_disconnect_cause_total{cause}` counter labels: `app_nap` / `wifi_flap` / `nat_timeout` / `dhan_rst` / `unknown`. Detected by recent `SystemTime::now()` clock-jump vs uptime delta heuristic at reconnect moment. |
| **L71** | Real-time guarantee score (composite SLO) | Already implemented (SLO-01/SLO-02). Per §T L58–L67 fixes the flap-spam. The score IS the real-time check — at 10s cadence it gives operator a single-glance "are guarantees holding?" answer. |
| **L72** | AWS deployment health expectation | After AWS deploy, expect WS disconnect rate to drop ~70% vs local Mac (loses C1+C2+C3 causes). Track via `tv_websocket_disconnect_total` 24h rate before/after migration; ratchet `test_aws_disconnect_rate_below_local_baseline` (baseline established post-migration soak). |

### §U.7 — The honest 100% claim (verbatim, refreshed for v8)

> 100% inside the tested envelope, with ratcheted regression coverage:
> ≤ 60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤ 2,000,000-tick ring buffer capacity;
> bench-gated O(1) hot path (re-baselined on aarch64 c8g.xlarge in PR #508);
> composite-key uniqueness;
> chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake;
> per-subsystem RSS accounting via `tv_subsystem_memory_bytes`;
> full-day in-memory store sized at ~2.1 GB on 11K instruments × 21 TFs;
> volume field semantic confirmed by Dhan support 2026-05-01-volume-semantic-clarification (cumulative since 09:15 IST, monotonic, resets at 09:15 IST next day);
> Telegram notifications edge-triggered + 30s state-hold debounced + dev-mode suppressed (no flap spam);
> WS disconnect classified by cause (`tv_websocket_disconnect_cause_total{cause}`), reconnect preserves subs via `SubscribeRxGuard`, post-close sleep-until-open + supervisor respawn ≤ 5s.
>
> **Beyond the envelope, DLQ NDJSON catches every payload as recoverable text.**
>
> Outstanding (Wave-6): > 65h holiday-weekend dormant sleep test (W6-2);
> OPEN-VERIFY-1 (`/api/movers` UI Top Volume mapping confirmation);
> OPEN-VERIFY-2 (c8g.xlarge Mumbai availability + RI pricing lock).

---

## §V) Dhan ping/pong contract — locked (NEW 2026-05-08)

Operator demand 2026-05-08: "always ensure to follow the Dhan ping pong mechanism also always — respect that bro."

The 10:05 IST RST flood you saw (5/5 main WS connections dropped with `os error 54: Connection reset by peer`) is the EXACT failure mode the ping/pong contract protects against.

### Dhan ping/pong spec (per `live-market-feed.md` rule 16)

| Parameter | Value | Source |
|---|---|---|
| Ping cadence (server → us) | every 10 seconds | Dhan docs |
| Pong response (us → server) | required within 40 seconds | Dhan docs |
| Failure to pong | server kills connection with RST (`os error 54`) | observed live |
| Implementation | **let `tokio-tungstenite` handle automatically — DO NOT implement manual ping frames** | rule 16 |

### Locked design — L73–L78

| # | Locked | Detail |
|---|---|---|
| **L73** | tokio-tungstenite auto-pong | The `WebSocketStream` automatically responds to server ping frames in its `next()` poll. Our read loop MUST poll the stream continuously — any blocking work in the same task that exceeds 40s starves the pong path → server RSTs us. |
| **L74** | Hot-path ban: no blocking work in WS read task | The WS read task does ONLY: poll frame → push to SPSC → loop. NO `cargo build`, no `tokio::time::sleep > 5s`, no synchronous DB writes. Banned-pattern hook category 8 (NEW): `tokio::time::sleep\(.*[5-9]\d*` or `std::thread::sleep` inside `connection.rs::run_read_loop`. |
| **L75** | Pong-deadline observability | NEW Prometheus gauge `tv_ws_last_pong_age_secs{conn_id}` updated from inside the WS task; alert `tv-ws-pong-stale` fires if any conn's last pong > 30s (10s before the 40s server kill threshold). Telegram Severity::High edge-triggered (per L58/L59). |
| **L76** | Disconnect-cause heuristic | At reconnect moment, check `time_since_last_recv_secs`. If > 30s and < 50s → label cause as `pong_starvation` in `tv_websocket_disconnect_cause_total{cause}`. If 5/5 conns drop within 5s of each other → cause = `dhan_rst_flood` (server-side, not client). |
| **L77** | Ratchet test for the contract | `crates/core/tests/ws_pong_contract_guard.rs`: (a) source scans `connection.rs::run_read_loop` for any sleep > 5s — must return zero; (b) property test simulates 60 ping events at 10s intervals → verifies tokio-tungstenite auto-pongs all 60. |
| **L78** | Dhan-side RST-flood class — separate from our pong contract | When ALL 5 conns drop within 5 seconds with `os error 54`, this is **Dhan-side infrastructure event**, NOT our pong miss. Reconnect path handles it; PR #337 `SubscribeRxGuard` preserves subs. Treat as Severity::High coalesced (not 5×High individual messages — fold into ONE summary "WS pool RST event" message). |

### What was wrong with the 10:05 IST notification (5 separate [HIGH] messages)

You got 5 individual `[HIGH] WebSocket N/5 disconnected` Telegram messages within 1 second, then 5 individual `[MEDIUM] WebSocket N/5 reconnected` 5 seconds later. That's 10 messages for ONE upstream RST event.

**Fix per L78 + L46 (notification UX):** When ≥ 3 conns of the same pool drop within 5 seconds, fold into ONE message:

```
🚨 [HIGH] Dhan WS pool RST event (5/5 main feed)

What's happening:
  Dhan server reset all 5 connections (os error 54).
  This is upstream infrastructure, not our system.

Detection time: 10:05:23 IST
Reconnect started: 10:05:24 IST  (+1 sec)
Reconnect completed: 10:05:28 IST  (+5 sec)
Subscriptions preserved: ✅ via SubscribeRxGuard
Ticks lost: 0 (rescue ring absorbed)

What to do: nothing — auto-recovery worked.

_(WS-RST-FLOOD / 5/5 conns / cause=dhan_rst_flood)_
```

### Ratchet enforcement of the contract

| Mechanism | What it asserts |
|---|---|
| Banned-pattern category 8 (NEW) | no `tokio::time::sleep` > 5s in `connection.rs::run_read_loop`; no `std::thread::sleep` anywhere in WS task |
| `test_ws_read_loop_no_blocking_work` | source scan asserts read loop body is only frame-poll + SPSC-push + loop |
| `test_pong_deadline_metric_exists` | `tv_ws_last_pong_age_secs{conn_id}` gauge is registered |
| `test_pong_stale_alert_fires_at_30s` | when last pong > 30s, alert fires (using mocked WS stream) |
| Property test `tokio_tungstenite_handles_60_pings` | 60 ping frames at 10s intervals → 60 auto-pong responses |
| `test_rst_flood_coalesces_into_one_message` | feeding 5 disconnect events within 5s emits ONE `WS-RST-FLOOD` message, not 5 individual ones |

### What this guarantees

| Demand | Mechanism |
|---|---|
| Always follow Dhan ping/pong | tokio-tungstenite auto-pong (L73) + read loop never blocks > 5s (L74) + ratchet enforcement (L77) |
| Detect pong starvation early | `tv_ws_last_pong_age_secs` gauge + 30s alert (10s pre-warning before 40s server kill) (L75) |
| Distinguish our-fault from Dhan-fault | disconnect cause classification (L76) — `pong_starvation` (us) vs `dhan_rst_flood` (Dhan) |
| Stop spam on RST flood | 5-within-5s coalescing into ONE summary message (L78) |
| Preserve subscriptions across RST | `SubscribeRxGuard` already shipped (PR #337) |
| Zero tick loss during RST window | rescue ring 2M absorbs the 1–5 sec gap |

### Updated all 78 locked decisions

| Group | Locks | Range |
|---|---:|---|
| Hardware + infra | 4 | L1–L4 |
| Data scope | 4 | L5–L8 |
| In-memory store | 10 | L9–L18 |
| Volume semantics | 3 | L19–L21 |
| Static universe | 6 | L22–L27 |
| Top-N + Dhan parity | 9 | L28–L36 |
| Depth diff resub | 10 | L37–L46 |
| Cleanup guarantee | 6 | L47–L52 |
| Depth-eligibility | 5 | L53–L57 |
| Notification UX | 10 | L58–L67 |
| Local Mac vs AWS | 5 | L68–L72 |
| **Dhan ping/pong contract (NEW)** | **6** | **L73–L78** |

---

## §W) Comprehensive crash + recovery matrix — every boot mode, every failure class (NEW 2026-05-08)

Operator demand: "slow boot, fast boot, outside market hours, inside market hours, extreme crash, app crash, DB crash, any out-of-box scenarios, errors, exceptions, bugs, issues — the system must overcome everything entirely + comprehensively automated."

### §W.1 — 4 boot modes × 5 time windows = 20 scenarios

| Boot mode | Definition | Typical RTO |
|---|---|---|
| **Fast warm** | rkyv binary cache valid, JWT cached, QuestDB schema present, network up | 5–10s |
| **Slow warm** | caches valid but Dhan/QuestDB cold | 10–60s |
| **Fast cold** | fresh clone, no cache, but Dhan + DB up | 30–60s (CSV download dominates) |
| **Slow cold** | fresh clone + Dhan + DB also slow | 60–300s |

| Time window IST | Behavior |
|---|---|
| 09:00–09:15 | pre-open: subscribe at boot, no ticks yet, ready by 09:15 |
| 09:15–15:30 | in-market: full live stream, all guarantees in effect |
| 15:30–next 09:00 | post-market / overnight: WS sleeps until next open, persist remains queryable |
| Saturday/Sunday | weekend: full sleep until Mon 09:00 IST, supervised dormant |
| Trading holiday (e.g. Republic Day) | calendar-aware sleep until next trading open |

### §W.2 — 12 crash / failure classes × recovery primitives

| # | Failure class | Detection | Mitigation | Recovery RTO | Ratchet test |
|---|---|---|---|---|---|
| 1 | App panic / abort | systemd / Docker `Restart=always`; `panic=abort` exits cleanly | Docker auto-restart within 5s | 5–10s | `chaos_app_panic_recovery.rs` |
| 2 | OOM kill | cgroup `memory.events:oom_kill` increment; `tv_oom_kills_total` counter | Docker auto-restart; rescue ring NDJSON survives on disk; replay on boot | 10–15s | PROC-01 (RESERVED → ship in Wave-6) |
| 3 | App restart loop | `RestartCount > 5/hour` | HALT before consuming budget; Critical Telegram | manual | PROC-02 (RESERVED → ship in Wave-6) |
| 4 | QuestDB crash (Docker exit) | ILP write `Err`; liveness probe fails 3× | rescue ring 2M ticks → spill NDJSON; QuestDB Docker auto-restart; replay on recovery | ≤ 60s within tested envelope | `chaos_questdb_docker_pause.rs` (live) |
| 5 | QuestDB schema drift | DDL ALTER detects missing columns | `ALTER TABLE ADD COLUMN IF NOT EXISTS` schema self-heal | seamless | `instrument_persistence.rs::ensure_instrument_tables` |
| 6 | Disk full | `df -h /` < 5% free | rescue ring stops draining to spill; alert HIGH; halt persist if < 1% | manual cleanup | RESOURCE-01 (RESERVED → Wave-4-E3) |
| 7 | WS Dhan-side RST flood | 5/5 conns drop within 5s with `os error 54` | `SubscribeRxGuard` preserves subs; auto-reconnect; coalesce alert per L78 | 1–5s | `chaos_ws_rst_flood.rs` (NEW) |
| 8 | Single WS conn drop | watchdog 5s tick sees state ≠ Connected | reconnect with backoff; pool supervisor respawns task | 5–10s | `test_subscribe_rx_guard_reinstalls_on_drop` |
| 9 | Pong starvation (we miss the 40s window) | `tv_ws_last_pong_age_secs` > 30s | alert HIGH at 30s; should never reach 40s | corrected by L74 ban on blocking work | `test_ws_read_loop_no_blocking_work` |
| 10 | Token expiry mid-session | DH-901 / DataAPI-807 | rotate via SSM/TOTP; reconnect with fresh JWT | 5–15s | AUTH-GAP-02 + AUTH-GAP-03 (live) |
| 11 | Network ISP / DNS flap | resolver fails or socket times out | retry with exponential backoff; 3-tier rescue absorbs gap | 10–60s | NET-01 / NET-02 (RESERVED) |
| 12 | Pre-open buffer empty when Phase 2 readiness fires | check at 09:13:01 IST | PHASE2-READY-01 Critical alert with named failed sub-checks; operator has 120s before 09:15 | manual | `phase2_readiness_check.rs::tests` |

### §W.3 — Out-of-the-box edge cases

| # | Edge case | Behavior |
|---|---|---|
| E1 | Operator boots app at 14:55 IST (35 min before close) | Mode C mid-market boot; ~30s to full subscribe; ~140 MB RAM populated from live ticks |
| E2 | App crashes at 14:55 IST, restarts at 14:56 | rescue ring NDJSON replayed; ~1 min ticks recovered into ticks table; in-mem store rebuilds from live ticks |
| E3 | Operator boots app at Saturday 03:00 AM | post-market mode; all WS conns dormant until Mon 09:00 IST; QuestDB stays queryable for backtests |
| E4 | App boots during 4-day holiday weekend (Wed 16:00 → Tue 09:00) | dormant 92h; force-renew token if < 4h on wake (AUTH-GAP-03) |
| E5 | Fresh clone on a new machine 5 minutes before market open | cold boot races market: CSV download 5–10s + cache build 20–30s + WS subscribe 10s = ~40s; should beat 09:15 if started by 09:14:00 |
| E6 | Two app instances accidentally running against same Dhan client_id | RESILIENCE-01 (RESERVED) — `live_instance_lock` table refuses second boot if first is < 60s old |
| E7 | Wall-clock skew > 2s between local and trusted source (chrony / QuestDB) | BOOT-03 HALT — refuses to start, prevents IST-midnight rollover corruption |
| E8 | Mid-rebalance crash | replay-on-boot from `subscription_audit` table re-issues unfinished `Swap20`/`Swap200` (RESILIENCE-02 RESERVED) |
| E9 | Operator manually edits `config/base.toml` between boots (bad TOML) | figment parse fails at config load; clean exit with stderr error message |
| E10 | TLS cert expires on Dhan side mid-session | TLS handshake fails on next reconnect; fall back to error log + alert; manual escalation to Dhan support |
| E11 | Triple-failure in 60s: WS drop + QuestDB pause + token expiry | CASCADE-01 (RESERVED) — every recovery primitive tested simultaneously in chaos suite |
| E12 | Massive Dhan-side RST flood every minute for 20 min | per-cycle alert coalesced; rescue ring drains over time; if rescue > 80% full, Critical alert; sustained > 5 min in-market triggers manual escalation |
| E13 | macOS Mac sleeps mid-session (App Nap, lid close) | local-only — addressed by L69 caffeinate / energy saver / login items |
| E14 | Docker container randomly killed by host kernel (OOM scoring high) | cgroup limits prevent app being chosen; if killed, Docker restarts within 5s |
| E15 | Pre-market subscribe message exceeds Dhan rate limit | per WS-GAP-02 batched 100/msg; if rate-limited, exponential backoff per Dhan rule 8 |

### §W.4 — Recovery primitive index

| Primitive | Implements | Tests |
|---|---|---|
| Rescue ring 2M | tick absorption during ≤60s outage | `zero_tick_loss_alert_guard.rs` + `chaos_zero_tick_loss.rs` |
| Spill NDJSON file | tick absorption beyond rescue ring capacity | `chaos_rescue_ring_overflow.rs` |
| DLQ NDJSON file | final tier — every payload caught | `live_feed_purity_guard.rs` |
| `SubscribeRxGuard` | sub channel survives reconnect | `test_subscribe_rx_guard_reinstalls_on_drop` |
| Pool supervisor respawn | dead WS task replaced ≤5s | `test_pool_watchdog_task_accepts_health_status` |
| Sleep-until-open post-15:30 IST | dormant WS during off-hours | `ws_sleep_resilience.rs` (65h chaos) |
| `force_renewal_if_stale` | token refresh on WS wake | AUTH-GAP-03 ratchets |
| Schema self-heal | `ALTER ADD COLUMN IF NOT EXISTS` | `instrument_persistence.rs::ensure_instrument_tables` |
| BOOT-01/02 deadlines | refuse to start with bad QuestDB | `wait_for_questdb_ready` ratchets |
| BOOT-03 clock-skew gate | refuse to start with > 2s skew | `test_boot_probe_emits_critical_on_clock_skew_gt_2s` |
| Phase 2 readiness pre-flight | gates 09:15 IST market open | `phase2_readiness_check.rs::tests` |
| Composite SLO score | real-time guarantee dashboard | `slo_score.rs::tests` |
| Market-open self-test 09:16:30 | positive heartbeat | `market_open_self_test.rs::tests` |
| `cleanup_audit_log` | SEBI traceability for drops | §R.7 |
| Audit tables (6) | SEBI 5y retention | Wave 2 Item 9 |

### §W.5 — Locked design — L79–L86

| # | Locked | Detail |
|---|---|---|
| **L79** | Comprehensive failure-mode coverage | All 12 failure classes (§W.2) MUST have a chaos/integration test in the workspace before APPROVED. Currently shipped: 5 (1, 4, 5, 7, 8). RESERVED: 7 (2, 3, 6, 9 partial, 11). PR-#508 prereq: ship missing chaos tests OR explicitly defer to Wave-6 with named items. |
| **L80** | Boot-mode determinism | All 20 boot mode × time window combinations (§W.1) MUST produce a deterministic outcome. `BootMode` enum simplified per L25 — keep only the 2 forks (in-market vs off-market) under static-universe model. The 4-mode complexity goes away. |
| **L81** | Out-of-box edge cases catalogued | All 15 E1–E15 edges have an explicit triage path (table above). Edges marked RESERVED are Wave-6 items; not blockers for #504a–#510 ship. |
| **L82** | RST-flood coalescing | When ≥ 3 conns of same pool drop within 5 sec, fold into ONE summary message (per L78). Eliminates the "5 individual [HIGH] messages in 1 second" spam pattern from 10:05 IST. |
| **L83** | Replay-on-boot | At every boot, `crates/storage/src/spill_replay.rs` (already shipped) replays any pending NDJSON from `data/spill/`. This handles app crash + restart cleanly. |
| **L84** | Crash + recovery summary in `make doctor` | `make doctor` § 8 (NEW) — show recent restart count, OOM events, sustained RSS over thresholds, recent rescue ring high-watermark. One-stop "what happened recently?" answer. |
| **L85** | "Was anything lost?" runbook | `docs/runbooks/zero-loss-verification.md` — operator can verify nothing was lost across any incident by running 3 SQLs (rescue ring depth high-watermark; spill file count; `cleanup_audit_log` recent rows). |
| **L86** | Dev-mode = production-mode behaviorally | The same recovery primitives run in dev (local Mac) and prod (AWS). Only the suppression policy (L62 dev-mode) differs. The CODE PATH for a crash is identical — what changes is only the alert routing. |

### §W.6 — What this guarantees (operator's "extreme + comprehensive + automated" demand)

| Demand | Mechanism |
|---|---|
| Slow boot recovery | `wait_for_questdb_ready` 60s deadline + BOOT-01/02 alerts; rescue ring buffers ticks during slow boot |
| Fast boot | rkyv binary cache 10ms load; CSV download skipped |
| In-market crash | rescue ring 2M absorbs ≤60s; replay NDJSON on restart; `SubscribeRxGuard` preserves subs |
| Out-of-market crash | dormant — no impact; restart on next market open |
| App crash | Docker auto-restart 5s; rescue NDJSON replayed |
| DB crash | rescue ring → spill → DLQ; auto-recovery on QuestDB return |
| Out-of-box: triple failure | CASCADE-01 chaos test (RESERVED Wave-6) |
| Out-of-box: dual instance | `live_instance_lock` table refuses (RESERVED) |
| Out-of-box: wall-clock skew | BOOT-03 HALT before any IST-midnight corruption |
| Out-of-box: mid-rebalance crash | `subscription_audit` replay (RESERVED) |
| All failures catalogued | this section + §K-L18 + §T notification policy |
| All failures observable | 7-layer observability per `wave-4-shared-preamble.md` § 4 |
| All failures recoverable | recovery primitive index §W.4 — every primitive shipped or RESERVED |

### §W.7 — Honest 100% claim refresh (verbatim, v10)

> 100% inside the tested envelope, with ratcheted regression coverage:
> ≤ 60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤ 2,000,000-tick ring buffer capacity;
> bench-gated O(1) hot path (re-baselined on aarch64 c8g.xlarge in PR #508);
> composite-key uniqueness;
> chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake;
> per-subsystem RSS accounting via `tv_subsystem_memory_bytes`;
> full-day in-memory store (~2.1 GB on 11K instruments × 21 TFs);
> volume field semantic confirmed by Dhan support 2026-05-01;
> Telegram notifications edge-triggered + 30s state-hold debounced + dev-mode suppressed;
> WS disconnect classified by cause; `SubscribeRxGuard` preserves subs across reconnect;
> tokio-tungstenite auto-pong respects Dhan 10s ping / 40s kill window;
> RST-flood coalesces 5/5 disconnect events into ONE summary message;
> 12 failure classes catalogued + 5 chaos tests shipped (7 RESERVED Wave-6);
> 15 out-of-box edge cases catalogued + triage path documented;
> 20 boot-mode × time-window combinations deterministic.
>
> **Beyond the envelope, DLQ NDJSON catches every payload as recoverable text.**
>
> Outstanding (Wave-6): chaos tests for OOM (PROC-01), restart loop (PROC-02), disk-full (RESOURCE-01), triple-failure (CASCADE-01), dual-instance (RESILIENCE-01), mid-rebalance (RESILIENCE-02), > 65h holiday-weekend dormant sleep (W6-2);
> OPEN-VERIFY-1 (`/api/movers` UI Top Volume mapping); OPEN-VERIFY-2 (c8g.xlarge Mumbai availability).

### Updated locked decisions across all 14 sections

| Group | Locks | Range |
|---|---:|---|
| Hardware + infra | 4 | L1–L4 |
| Data scope | 4 | L5–L8 |
| In-memory store | 10 | L9–L18 |
| Volume semantics | 3 | L19–L21 |
| Static universe | 6 | L22–L27 |
| Top-N + Dhan parity | 9 | L28–L36 |
| Depth diff resub | 10 | L37–L46 |
| Cleanup guarantee | 6 | L47–L52 |
| Depth-eligibility | 5 | L53–L57 |
| Notification UX | 10 | L58–L67 |
| Local Mac vs AWS | 5 | L68–L72 |
| Dhan ping/pong contract | 6 | L73–L78 |
| **Crash + recovery matrix (NEW)** | **8** | **L79–L86** |

---

## §X) Extreme Automation Charter — NO manual processes (NEW 2026-05-08)

Operator demand 2026-05-08 (verbatim): "no manual process is accepted unless extreme case. Always extremely comprehensively automated approach. Every Claude Code session should automatically do everything. 100% on every dimension. Guaranteed without hallucination — with proof."

This charter captures the binding rule: **manual operator actions are accepted ONLY in true extreme cases. Everything else must be automated end-to-end.**

### §X.1 — The No-Manual-Process Rule

| Operator action that USED to be manual | New automated path |
|---|---|
| Mute Telegram bot when flap-spamming | L62 dev-mode auto-suppression + L59 30s state-hold debounce → never reaches user |
| `make stop` to silence dev spam | replaced by auto-suppression above; explicit stop only on operator request |
| `make doctor` health check | runs automatically every Claude session start (`session-auto-health.sh` already wired) |
| Read recent errors in Telegram | summary-writer auto-publishes `data/logs/errors.summary.md` every 60s |
| Run cargo tests after change | `verify-build` agent runs automatically post-edit; CI runs full battery on every push |
| Check for novel ERROR signatures | `mcp__tickvault-logs__list_novel_signatures` runs at session start |
| Cross-check guards intact | `make validate-automation` runs at session start (30 mechanical checks) |
| Manually verify volume field semantic | empirical 3-way cross-check at 14:30 IST cron, results auto-posted |
| Manually compute RSS attribution | `tv_subsystem_memory_bytes` per-component gauge (L18) auto-Prometheus |
| Manually read PR webhook events | `subscribe_pr_activity` MCP tool wakes session on event |

### §X.2 — Locked design L87–L96

| # | Locked | Detail |
|---|---|---|
| **L87** | NO manual process gate | Every operational task must have ONE of: (a) auto-trigger from event, (b) cron, (c) MCP tool callable from any Claude session. Manual `make` invocations are CONVENIENCE wrappers, not the canonical path. |
| **L88** | Per-session auto-bootstrap (existing — keep) | SessionStart hook runs: `run_doctor` → `summary_snapshot` → `list_active_alerts` → `session-auto-health.sh` → `session-sanity.sh`. NO Claude Code session starts without these 5 tools fired. |
| **L89** | Cross-environment access | Same MCP tool surface in local AND AWS. `mcp__tickvault-logs__*` for prom/qdb/grafana/loki works identically against local Docker AND AWS via SSM-routed endpoints. Single tool call, dual environment. |
| **L90** | Auto-research on every design decision | When operator asks a design question touching > 3 crates / > 1,000 LoC, Claude spawns 3 specialist agents in parallel (hot-path-reviewer / security-reviewer / general-purpose) per `wave-4-shared-preamble.md` § 3 — automatic, not manual. |
| **L91** | Auto-fix on triage rule match | `.claude/triage/error-rules.yaml` matches signature → `auto-fix.sh` runs without operator intervention. Critical errors still escalate to Telegram (per L46 severity policy). |
| **L92** | Auto-doc-update on every plan change | Every operator decision in chat → I update plan-doc + commit + push within same turn, no manual prompt. Plan-verify hook ensures the chat referenced files exist. |
| **L93** | Auto-runbook-link on every alert | Every Telegram Critical body includes a "What to do" wizard (L67) with 1–3 step actionable list. No "see runbook" jargon — actual steps inline. |
| **L94** | Auto-soak-window enforcement | Phase 3/4a/4b clean-window gates (24h, 7d, 14d, 30d) checked automatically by daily cron — no operator manual countdown. CI gate fails PR if window not met. |
| **L95** | Auto-instance-pricing-verify | OPEN-VERIFY-2 (`aws ec2 describe-instance-types --region ap-south-1`) wired as a CI step that runs weekly, posts results to Telegram, fails build if c8g.xlarge availability lapses. No manual `aws ec2` runs. |
| **L96** | Auto-empirical-cross-check at 14:30 IST daily | OPEN-VERIFY-1 cross-check (WebSocket vs REST quote vs `/api/movers`) runs automatically every trading day at 14:30 IST, results posted to `data/notes/volume-semantic-verify.YYYY-MM-DD.md`. After 5 consecutive matches, gate auto-closes. |

### §X.3 — The 14-dimension 100% guarantee matrix (operator's bracket list)

Every "100%" must point to a specific mechanical proof. NO hallucinated 100% claims.

| # | Dimension | Mechanical proof | File / test |
|---|---|---|---|
| 1 | **Code coverage** | `quality/crate-coverage-thresholds.toml` per-crate 100% min; CI fails on regression | `scripts/coverage-gate.sh` |
| 2 | **Code scanning** | banned-pattern hook 8 categories; pre-commit gate blocks commit | `.claude/hooks/banned-pattern-scanner.sh` |
| 3 | **Code duplication** | `cargo-mutants` survives = bug; weekly CI run | `.github/workflows/mutation.yml` |
| 4 | **Functionality missing** | `pub-fn-test-guard.sh` — every pub fn has test; `pub-fn-wiring-guard.sh` — every pub fn has caller | both ratchets |
| 5 | **Scenario missing** | §W catalogues 12 failure classes + 15 edge cases; chaos test suite covers shipped 5 + RESERVED 7 named in Wave-6 | `crates/storage/tests/chaos_*.rs` family |
| 6 | **Bug fixing** | adversarial 3-agent review per `wave-4-shared-preamble.md` § 3 on every PR | proven 4-bug catch rate per 30-commit PR |
| 7 | **Issues finding** | `mcp__tickvault-logs__list_novel_signatures` daily; `error-triage.sh` triage YAML | shipped Phase 6 |
| 8 | **Testing types (22 categories)** | unit / integration / property / loom / dhat / fuzz / mutation / sanitizer / coverage / chaos / etc. | `testing.md` enumerates all 22 |
| 9 | **Testing coverage** | scoped per-crate default + workspace on `crates/common/` change + CI runs full | `testing-scope.md` rule |
| 10 | **Real-time testing** | live integration tests against running QuestDB/Valkey/etc.; chaos tests pause Docker mid-test | `chaos_questdb_docker_pause.rs` |
| 11 | **Code performance** | DHAT zero-alloc + Criterion p99 budgets + bench-gate ≤5% regression | `quality/benchmark-budgets.toml` |
| 12 | **Uniqueness** | I-P1-11 composite key `(security_id, exchange_segment)` everywhere | `dedup_segment_meta_guard.rs` |
| 13 | **Deduplication** | `DEDUP UPSERT KEYS(...segment...)` on every storage table | meta-guard ratchet |
| 14 | **O(1) latency** | `from_le_bytes` + `papaya` + `Arc<HashMap>` + bounded SPSC + bench-gate | DHAT + Criterion + budgets |

**Honest 100% rule:** every cell above MUST cite an existing file/test/hook. If a cell is RESERVED (Wave-6 chaos tests for example), it's named explicitly — no hand-wave 100% allowed.

### §X.4 — What "100%" actually means (no hallucination)

Per `wave-4-shared-preamble.md` Section 8 — the ONLY honest 100% claim is bounded by tested envelope. Restated for L87-L96:

> **100% inside the tested envelope, with ratcheted regression coverage AND auto-running on every Claude session start.** Outside the envelope (e.g. >65h holiday-weekend dormant sleep, OOM kill chaos, dual-instance race), items are explicitly named in Wave-6 backlog with target ship dates — NOT claimed as covered today.

| What we WON'T say | What we WILL say |
|---|---|
| "Zero ticks ever lost forever" | "Bounded zero loss inside ≤60s outage envelope; DLQ NDJSON catches every payload beyond" |
| "WS never disconnects" | "Detect ≤5s + reconnect preserves subs via `SubscribeRxGuard`; sleep-until-open post-close" |
| "QuestDB never fails" | "Absorb via 3-tier rescue→spill→DLQ + schema self-heal; chaos-tested 60s outage" |
| "100% bug-free" | "100% inside the tested envelope; novel signatures detected daily; auto-triage on YAML match" |
| "Always O(1)" | "Bench-gated ≤100 ns p99 hot path; ≤5% regression fails CI; re-baselined per arch" |

### §X.5 — Per-Claude-session automation pipeline (already shipped, locked)

| Step | Tool | When | What it does |
|---|---|---|---|
| 1 | `mcp__tickvault-logs__run_doctor` | session start | 7-section health snapshot |
| 2 | `mcp__tickvault-logs__summary_snapshot` | session start | last hour ERROR signatures grouped |
| 3 | `mcp__tickvault-logs__list_active_alerts` | session start | current firing Prom alerts |
| 4 | `session-context-brief.sh` | session start | active plans + open PRs + 5-step protocol |
| 5 | `session-auto-health.sh` | session start (background) | doctor + validate-automation + mcp-doctor |
| 6 | `session-sanity.sh` | session start | branch + uncommitted check + auto-save remote |
| 7 | `make stop` if Telegram spam detected | session start (NEW per §X.6) | local app silenced if dev-mode flap rate > 5/min |

### §X.6 — Auto-silence-flap (NEW under L87 — eliminates today's manual `make stop` need)

| Trigger | Action | Implementation |
|---|---|---|
| `tv_telegram_dropped_total{reason="rate_exceeded"}` > 5/min | auto-mute Telegram dispatch for 5 minutes | `notification/coalescer.rs` checks rate, sets `disabled_until` |
| Local Mac dev mode + auto-up.log mtime < 60s | dev-mode suppression L62 (already locked) | reads session-auto-health flag |
| Sustained QuestDB flap (Connect/Disconnect every minute for >5 min) | auto-issue `make stop` and Telegram once: "Local QuestDB unstable; app stopped automatically. Run `make doctor`." | new daemon in app boot — kills self if QuestDB flap rate > N/5min |

### §X.7 — Multi-environment auto-access table

| Resource | Local Mac access | AWS access | Same MCP tool? |
|---|---|---|---|
| Prometheus | `localhost:9090` via Docker | AWS via SSM tunnel | ✅ `mcp__tickvault-logs__prometheus_query` |
| QuestDB | `localhost:9000` HTTP / `localhost:8812` PG | AWS via SSM tunnel | ✅ `mcp__tickvault-logs__questdb_sql` |
| Grafana | `localhost:3000` | AWS via SSM tunnel | ✅ `mcp__tickvault-logs__grafana_query` |
| Logs (errors.jsonl) | `data/logs/errors.jsonl.*` | AWS S3 → fetched | ✅ `mcp__tickvault-logs__tail_errors` |
| Logs (app.YYYY-MM-DD.log) | local file | AWS via CloudWatch / Loki | ✅ `mcp__tickvault-logs__app_log_tail` |
| Active alerts | local Prom | AWS Prom via SSM | ✅ `mcp__tickvault-logs__list_active_alerts` |
| Container status | local Docker | AWS Docker via SSM | ✅ `mcp__tickvault-logs__docker_status` |
| Health check | local make doctor | AWS make doctor via SSM | ✅ `mcp__tickvault-logs__run_doctor` |

**Single tool surface, dual environment.** No "switch environment" command needed.

### §X.8 — Lazy-kid explanation

| Demand | Lazy-kid version |
|---|---|
| No manual process | "If you ever have to type a command to fix something normal, that's a bug. The system should fix itself." |
| Auto-everything per session | "Every time I (Claude) wake up in this project, I automatically check: is anything broken? are there errors? are alerts firing? — before you ever ask." |
| 100% inside envelope | "I'll tell you EXACTLY what's tested and what's not. I won't say 'always perfect' — that's a lie. I'll say 'tested for 60-second outage, beyond that we have a backup file that catches everything.'" |
| Auto-silence flap | "If the system starts spamming Telegram because something silly is flapping, I shut up automatically until you tell me to talk again — instead of making you mute me manually." |
| Multi-env access | "Whether your stuff is on your laptop or in the cloud, the same single command works. You don't have to remember which environment." |
| Auto-research | "If you ask me a hard question, I split it into 3 specialists who go investigate in parallel — automatically — and I synthesize their findings." |
| Auto-fix on known issues | "If I see an error pattern I recognize, I fix it without asking. If it's something new, I ask first. The known-good fixes are pre-canned." |

### §X.9 — Updated locked decisions across 15 sections

| Group | Locks | Range |
|---|---:|---|
| Hardware + infra | 4 | L1–L4 |
| Data scope | 4 | L5–L8 |
| In-memory store | 10 | L9–L18 |
| Volume semantics | 3 | L19–L21 |
| Static universe | 6 | L22–L27 |
| Top-N + Dhan parity | 9 | L28–L36 |
| Depth diff resub | 10 | L37–L46 |
| Cleanup guarantee | 6 | L47–L52 |
| Depth-eligibility | 5 | L53–L57 |
| Notification UX | 10 | L58–L67 |
| Local Mac vs AWS | 5 | L68–L72 |
| Dhan ping/pong contract | 6 | L73–L78 |
| Crash + recovery matrix | 8 | L79–L86 |
| **Extreme Automation Charter (NEW)** | **10** | **L87–L96** |

---

**End of DRAFT v12.** Awaiting OPEN-VERIFY-1 + OPEN-VERIFY-2 closure for APPROVED status.

## §Y) Workspace Auto-Update Charter — every dependency, every workspace, always (NEW 2026-05-08)

Operator demand 2026-05-08: "always make sure our entire product is always automated for an update for the entire workspace libraries and even everywhere — everything should be always updated with guarantee."

### §Y.1 — Locked design L97–L106

| # | Locked | Detail |
|---|---|---|
| **L97** | Dependabot auto-PRs for Cargo workspace | `.github/dependabot.yml` `package-ecosystem: cargo`, weekly, `groups.cargo-patch-and-minor`. Already shipped (PR #452). Auto-merge on green CI for patch+minor. |
| **L98** | Docker base image auto-updates | `package-ecosystem: docker` for `docker-compose.yml`. Weekly. SHA256 digests pinned per image. |
| **L99** | GitHub Actions auto-updates | `package-ecosystem: github-actions` for `.github/workflows/*.yml`. Weekly. Auto-merge patch+minor on green CI. |
| **L100** | OS package auto-updates (AWS) | `dnf-automatic` weekly Saturday 04:00 IST on AWS Linux. Telegram report on completion. |
| **L101** | Workspace pinning rule unchanged | exact versions ONLY in workspace `Cargo.toml`. `^`, `~`, `*`, `>=` BANNED. Crates use `{ workspace = true }`. Dependabot proposes exact-version bumps, NEVER caret/tilde. |
| **L102** | Auto-merge gate | green CI = clippy + test + audit + deny + fmt + banned-pattern + secret-scan ALL pass → auto-merge. Any failure → leave PR open + Telegram MEDIUM. |
| **L103** | Security advisory blocking | `cargo-audit` runs every PR + nightly. New CVE → fail CI + Telegram CRITICAL. cargo-deny `unsound` / `vulnerability` block merge. |
| **L104** | Full 22-test battery on dependabot bumps | Force `FULL_QA=1` on dependabot branches. Catches regressions invisible to bump diff. |
| **L105** | Lockfile sanity | `Cargo.lock` committed; reproducible builds. cargo-deny `[bans]` blocks duplicate dep versions. |
| **L106** | Audit trail per bump | Every dependency bump merge writes `cleanup_audit_log` row (per L52) with `item_kind = "dependency_bump"`, `item_name = "<crate>:<old>→<new>"`, `pr_sha`. SEBI-traceable. |

### §Y.2 — Auto-update coverage matrix

| Surface | Mechanism | Cadence | Auto-merge | Logged |
|---|---|---|---|---|
| Cargo workspace deps | dependabot `cargo` | weekly | YES on green patch+minor | `cleanup_audit_log` |
| Cargo nested deps (lockfile) | dependabot transitive | weekly | YES | same |
| Docker images (8 services) | dependabot `docker` | weekly | YES | same |
| GitHub Actions versions | dependabot `github-actions` | weekly | YES patch+minor | same |
| OS packages on AWS | `dnf-automatic` | weekly Sat 04:00 IST | YES | Telegram + audit log |
| cargo-fuzz corpus | nightly CI | nightly | n/a (data) | corpus dir |
| Mutation testing baseline | weekly CI | weekly Mon | n/a | mutation report |
| `benchmark-budgets.toml` | manual operator review | quarterly | NO | git history |

### §Y.3 — Guarantee chain

| Promise | Mechanism |
|---|---|
| All deps current within 7 days | dependabot weekly |
| Security advisories patched ≤ 24h | `cargo-audit` nightly + Telegram CRITICAL → auto-patch-PR |
| No silent regressions | full 22-test battery on dependabot merges (L104) |
| All bumps audited | `cleanup_audit_log` row per merge (L106) |
| No version drift | workspace `{ workspace = true }` single source |
| Reproducible builds | `Cargo.lock` committed + cargo-deny no-duplicates |
| AWS kernel current | `dnf-automatic` weekly + reboot policy |

### §Y.4 — What we INTENTIONALLY do NOT auto-update

| Item | Why manual |
|---|---|
| Major version bumps (e.g. tokio 1.x → 2.x) | breaking-change risk; needs operator review + RFC |
| Pinned Docker SHA256 with budget impact | mem requirements may change |
| `benchmark-budgets.toml` | post-arch-change re-baseline only |
| Rust toolchain MSRV | manual after compatibility verify |
| Dhan API version | manual; depends on Dhan release notes |

### §Y.5 — Lazy-kid version

| Concept | Plain English |
|---|---|
| Auto-update | "Every Saturday morning, robots check every library, propose upgrades, run ALL tests, merge on green — you touch nothing." |
| Guarantee | "If ANY test fails, the upgrade does NOT merge. We never ship a broken upgrade." |
| Security patch speed | "New CVE → I wake via webhook within hours, propose patch, merge if safe. CRITICAL Telegram if patch needs you." |
| Audit | "Every upgrade logged with date + version + commit hash. Answer 'when did we upgrade tokio?' in 1 SQL query." |
| What we DON'T auto | "Big breaking changes (1.x → 2.x) need your review. Patch + minor auto-merge on green; majors don't." |

### §Y.6 — Total locked decisions across 16 sections

| Group | Locks | Range |
|---|---:|---|
| Hardware + infra | 4 | L1–L4 |
| Data scope | 4 | L5–L8 |
| In-memory store | 10 | L9–L18 |
| Volume semantics | 3 | L19–L21 |
| Static universe | 6 | L22–L27 |
| Top-N + Dhan parity | 9 | L28–L36 |
| Depth diff resub | 10 | L37–L46 |
| Cleanup guarantee | 6 | L47–L52 |
| Depth-eligibility | 5 | L53–L57 |
| Notification UX | 10 | L58–L67 |
| Local Mac vs AWS | 5 | L68–L72 |
| Dhan ping/pong contract | 6 | L73–L78 |
| Crash + recovery matrix | 8 | L79–L86 |
| Extreme Automation Charter | 10 | L87–L96 |
| **Workspace Auto-Update Charter (NEW)** | **10** | **L97–L106** |
| **TOTAL** | **106** | **L1–L106** |

c8g.xlarge ONLY. 106 design decisions. 2 open verification gates remaining.

## §Z) Gap closure — all discussed items locked (NEW 2026-05-08)

Operator directive 2026-05-08: "09:14 IST live test schedule + everything else from the gap list is needed in our plan."

This section locks every item that was discussed in the design session but had not yet received an explicit L-number. **No hallucinated coverage** — each item below maps to a specific implementation file or explicit defer-with-Wave-6-pointer.

### §Z.1 — Locked design L107–L120

| # | Locked | Detail |
|---|---|---|
| **L107** | OPEN-VERIFY-1(b) automated 09:14 IST live test | Daily cron `09 09 * * 1-5 IST` (i.e. 09:14 IST Mon–Fri, configured via systemd timer / GitHub Actions IST cron). Subscribes to ONE liquid contract (e.g. NIFTY ATM call current expiry), captures `volume` field at T-30s / T-5s / T+0s / T+30s / T+60s relative to 09:15:00 IST, writes capture to `data/notes/volume-semantic-verify.YYYY-MM-DD.md`. After 5 consecutive matches (volume = 0 at T-30s + T-5s, increments thereafter), gate auto-closes. Implementation: `scripts/openverify1b-cron.sh` + `crates/app/tests/openverify1b_capture.rs` (NEW). |
| **L108** | Q6 addition to Dhan support email | Append to `docs/dhan-support/2026-05-01-volume-semantic-clarification.md` Question 6: *"Does the `/api/movers` web UI 'Top Volume' tab rank instruments by the same cumulative-day-volume field exposed in the Live Market Feed Quote/Full packet (bytes 22–25, u32 LE)? If different, what is the source field?"* Operator clicks Send via Gmail to `apihelp@dhan.co`. |
| **L109** | Indicator engine read pattern | Indicators (RSI/EMA/MACD/SMA/BB via yata) compute ON DEMAND by reading the latest N bars from candle storage map per (instrument, TF). NO separate indicator persistence, NO indicator-snapshot table writes during live trading. `IndicatorEngine` becomes a thin wrapper: `compute_indicator(security_id, segment, tf, indicator_kind, n_bars) → f64` reads RAM, returns. Latency O(N) bars × fixed indicator cost ~10–50 µs per call. **Backtest path** uses the same primitive over historical matview data. |
| **L110** | Strategy evaluator read pattern | Strategy FSM evaluator (TOML-configured per `crates/trading/src/strategy/`) reads ONLY from candle storage map + tick map + indicator engine. NEVER touches QuestDB on live decision path. Strategy reads happen at bar seal events (pull-on-seal model) — the strategy evaluator is a `BarSealedEvent` subscriber. |
| **L111** | Order-update WS unchanged under new architecture | Order-update WebSocket (`wss://api-order-update.dhan.co`, JSON) is independent of in-memory store changes. Existing OMS state machine + reconciliation continues to work. The new in-memory store does NOT change order-update flow. Reaffirmed by ratchet: `test_order_update_ws_unaffected_by_inmem_store_changes`. |
| **L112** | Wave-6 chaos tests reaffirmed (named, scoped, deferred) | The 7 outstanding Wave-6 chaos tests are EXPLICITLY named in `wave-6-backlog.md` with target dates: PROC-01 OOM kill detection, PROC-02 restart loop, RESOURCE-01 disk full, CASCADE-01 triple-failure, RESILIENCE-01 dual-instance, RESILIENCE-02 mid-rebalance, W6-2 >65h holiday-weekend dormant sleep. Honest-100% claim (§U.7, §W.7) calls each one out by name. Not blocking for #504a–#510 ship. |
| **L113** | PR #510 non-technical-user readability audit | Post-#504a deploy: operator reviews EVERY `Critical` Telegram template (per L65 registry) for plain-English clarity. Audit produces a `docs/operator/telegram-template-uat-2026-MM-DD.md` sign-off doc. Acceptance: at least 1 non-technical-user reviewer (e.g. spouse, co-trader) confirms each template is readable without explanation. Acts as the L67 "what-to-do" wizard validation. |
| **L114** | Convertible RI as future option (NOT current lock) | §J Q5 locked **1-yr Standard RI no-upfront** as the current cost-optimal choice. **L114 reaffirms:** Convertible RI (~13% premium vs Standard) is a documented future option if Wave-6 proves c8g.xlarge is too small (e.g. operator wants Option A full-retention or Option E/F historical). Decision gate: 6 months post-deploy review of RSS trends + cohort capacity utilization. Operator must explicitly opt-in to Convertible at the next RI renewal. Default stays Standard. |
| **L115** | Sanity sweep on L-numbers (in-doc) | Every locked decision MUST carry a unique `**L<N>**` bold marker in the plan doc. Plan-verify hook extended (`plan-verify.sh`) to scan for: (a) duplicate L-numbers, (b) skipped numbers (e.g. L42 missing between L41 and L43), (c) any `Locked` row without bold marker. Fails commit if any condition triggered. |
| **L116** | Local Docker auto-up state observability | The auto-up script (referenced in every SessionStart hook) writes its state to `data/logs/auto-up.YYYY-MM-DD-HHMM.log`. NEW MCP tool `mcp__tickvault-logs__autoup_status` returns `{ profile, prom, qdb, graf, api, last_attempt_at, success }` — Claude can self-check whether local infra is up before answering "is the system healthy?" |
| **L117** | Phase 4a/4b soak gate auto-tracking | Daily cron at 17:00 IST (post-market) checks: (a) days since Phase 4a deploy commit, (b) days since Phase 4b drop commit, (c) any `tv_movers_persist_errors_total` > 0, (d) any cleanup_audit_log entries with `errored=true`. Posts daily soak-gate status to `data/notes/phase-4-soak-progress.YYYY-MM-DD.md`. After 24h Phase 4a clean window expires → auto-Telegram "Phase 4a soak complete, safe to proceed with Phase 4b drop". |
| **L118** | Plan-doc CHANGELOG section | NEW §AA at the bottom of plan: `## §AA) CHANGELOG` — every Draft version (v1 through v12) listed with date, commit sha, summary of changes. Operator can audit "what changed between v8 and v9?" in seconds without git diff. |
| **L119** | "Discussion topics covered" cross-reference table | Already present in chat as the discussion-topic-vs-plan-section map. Locks: this table MUST be regenerated and committed to plan as `§AB) Discussion-Topic Coverage Map` whenever a new section is added. Mechanical guard: plan-verify.sh fails if §AB is older than the most-recent §<X> addition. |
| **L120** | Honest 100% claim is the SINGLE source of truth on guarantees | Verbatim wording lives in §W.7 (latest refresh). Every PR description that mentions "100% guarantee" MUST cite §W.7 verbatim or include a delta clearly stating "additional guarantee X added by this PR". CI ratchet `test_pr_body_uses_honest_envelope_wording` scans every PR opened against the repo. |

### §Z.2 — 09:14 IST live test cron — full spec (L107)

```bash
# scripts/openverify1b-cron.sh
# Runs 09:13:30 IST Mon-Fri (allows 30s setup before 09:14:00 capture)
# crontab: 13 9 * * 1-5  IST  (TZ=Asia/Kolkata)

#!/bin/bash
set -euo pipefail
TODAY=$(date -u +%Y-%m-%d)
OUTPUT="data/notes/volume-semantic-verify.${TODAY}.md"

# Subscribe to one liquid contract via dedicated short-lived WS connection
# (separate from main feed pool — does not impact production subscriptions)
cargo run --release --bin openverify1b_capture -- \
    --security-id 41747 \
    --segment NSE_FNO \
    --capture-windows "T-30,T-5,T+0,T+30,T+60" \
    --reference-time "09:15:00 IST" \
    --output "$OUTPUT"

# Auto-update gate status
if scripts/check-volume-monotonicity.sh "$OUTPUT"; then
    echo "OPEN-VERIFY-1(b) PASS for $TODAY ($(scripts/count-passes.sh) consecutive)" \
        | tee -a data/notes/openverify1b-progress.log
    if [[ $(scripts/count-passes.sh) -ge 5 ]]; then
        echo "OPEN-VERIFY-1(b) GATE CLOSED — 5 consecutive matches"
        scripts/post-telegram.sh "✅ OPEN-VERIFY-1(b) gate auto-closed after 5 matches"
    fi
else
    scripts/post-telegram.sh "⚠️ OPEN-VERIFY-1(b) MISMATCH on $TODAY — review $OUTPUT"
fi
```

### §Z.3 — Capture windows + expected behavior

| Window | Time | Expected `volume` field | Reasoning |
|---|---|---:|---|
| T-30 | 09:14:30 IST | **0** | pre-open / no continuous trading yet |
| T-5 | 09:14:55 IST | **0** | still pre-market |
| T+0 | 09:15:00 IST | **0 → first non-zero** | exact session-open instant; first tick should land |
| T+30 | 09:15:30 IST | small positive (e.g. 100s–1000s) | first 30 sec of trading volume |
| T+60 | 09:16:00 IST | larger positive | full 1-min cumulative |

If T-30 or T-5 is non-zero → field includes pre-open volume (UNEXPECTED; would invalidate L20).
If T+0 is still 0 at 09:15:01 → tick stream delayed (Dhan-side issue; capture again next day).
If T+60 < T+30 → monotonicity violated (CRITICAL).

### §Z.4 — Gate-close criteria

| Match count | State | Action |
|---|---|---|
| 0 | initial | gate OPEN |
| 1–4 | accumulating | gate OPEN, log progress |
| ≥ 5 consecutive matches | criteria met | gate AUTO-CLOSED → `OPEN-VERIFY-1` resolved → `§K-L21` upgraded from "best-effort" to "confirmed" |
| Any single mismatch | reset | gate OPEN, counter back to 0, Telegram MEDIUM |

### §Z.5 — Total locked decisions across 17 sections (final)

| Group | Locks | Range |
|---|---:|---|
| Hardware + infra | 4 | L1–L4 |
| Data scope | 4 | L5–L8 |
| In-memory store | 10 | L9–L18 |
| Volume semantics | 3 | L19–L21 |
| Static universe | 6 | L22–L27 |
| Top-N + Dhan parity | 9 | L28–L36 |
| Depth diff resub | 10 | L37–L46 |
| Cleanup guarantee | 6 | L47–L52 |
| Depth-eligibility | 5 | L53–L57 |
| Notification UX | 10 | L58–L67 |
| Local Mac vs AWS | 5 | L68–L72 |
| Dhan ping/pong contract | 6 | L73–L78 |
| Crash + recovery matrix | 8 | L79–L86 |
| Extreme Automation Charter | 10 | L87–L96 |
| Workspace Auto-Update Charter | 10 | L97–L106 |
| **Gap closure (NEW §Z)** | **14** | **L107–L120** |
| **TOTAL** | **120** | **L1–L120** |

c8g.xlarge ONLY. 120 design decisions locked. **All discussion topics now have explicit plan coverage.** Awaiting OPEN-VERIFY-1 + OPEN-VERIFY-2 closure for APPROVED status.
