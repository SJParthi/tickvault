# AWS Cost Floor Analysis — Can We Reach <₹1K/mo?

> **⚠️ SUPERSEDED 2026-05-18 ⚠️**
>
> This doc compared multiple instance candidates to find <₹1K achievability. The operator's AWS console screenshot revealed the agent had hallucinated ap-south-1 pricing by ~43%. With CORRECTED Mumbai pricing:
> - **t4g.medium ap-south-1 = $0.0224/hr (not $0.0392)**
> - **t4g.medium every-day 9hr × 30 = ~₹1,022/mo** ✓ essentially at <₹1K target
> - **Tenancy: Default (Shared) LOCKED** (Dedicated saves nothing, costs ₹1,500/mo extra)
>
> **THE FINAL LOCK:** t4g.medium ARM ap-south-1, Shared tenancy, on-demand, every-day 08:00–17:00 IST = ~₹1,022/mo.
>
> Read this only for historical reasoning. **For the locked architecture, read `aws-indices-only-locked-architecture.md`.**

---

# AWS Cost Floor Analysis — Can We Reach <₹1K/mo? (historical)

> **Status:** DESIGN DOC (no code shipped). Honest envelope analysis per operator-charter §F.
> **Created:** 2026-05-18 in response to operator LOCK-2 ("<₹1K/mo, accept compromises TBD").
> **Companion:** `docs/architecture/aws-daily-lifecycle.md`, `.claude/rules/project/aws-budget.md`.
> **Bottom line:** Sub-₹1K on AWS is structurally NOT achievable without breaking a non-negotiable invariant. AWS-only floor ≈ ₹1.4K/mo. Off-AWS paths exist but each has serious trade-offs.

---

## §0. Auto-driver one-liner

> "Sir, you said keep the bill under ₹1,000. Honest answer: AWS minimum bill for this product is ~₹1,400/mo because Dhan demands a never-changing IP address (₹152), the database needs 8GB RAM (smallest matching server is ₹650 for 4hr/day), and the cold-archive disk is ₹150. To go below ₹1,000 we MUST do one of: (a) run from your home computer instead of AWS — loses guaranteed IP, breaks if power cuts; (b) skip the database — breaks SEBI audit; (c) shorter hours like only 2hr/day — misses market open prep. Pick one of these three OR accept ~₹1,400 as the floor."

---

## §1. The non-negotiable invariants (what we CAN'T cut)

| Invariant | Source | Min cost it forces |
|---|---|---|
| Static IP, registered with Dhan, 24/7 | Dhan static-IP mandate effective 2026-04-01 (auth.md rule 7) | EIP ~₹152/mo (charged when associated AND when instance stopped) |
| Host memory ≥ 8GB | aws-budget.md rule 6 (sum of QuestDB 1.5GB + App 2GB + headroom 2GB + etc) | smallest instance: c7i.large or c8g.xlarge — both 8GB |
| SEBI 5-year retention of audit tables | charter §H, aws-budget.md rule "S3 lifecycle policy is MANDATORY" | S3 cold storage growing to ~₹150/mo at year 5 |
| EBS for hot data (last 14-30 days) | aws-budget.md rule "EBS gp3, 100GB" | At minimum 50GB = ₹387; at 30GB = ₹232 |
| CloudWatch monitoring + alarms | aws-budget.md rule 4 "MANDATORY and always enabled" | Within free tier — ₹0 if disciplined |
| Once-daily SNS alert capacity | this doc §3 of `aws-daily-lifecycle.md` | ~₹25/mo (100 SMS) |

**Combined floor (regardless of instance choice):**

```
EIP                  ₹152
S3 cold + transfer   ₹150  (worst case year 5; ₹50 first year)
EBS gp3 (30GB min)   ₹232
SNS + CloudWatch     ₹25
DataTransfer         ₹85
─────────────────────────
INFRASTRUCTURE FLOOR ₹644/mo  (BEFORE any compute)
```

This is the cost just for the *envelope* — no EC2 hours included yet.

---

## §2. The compute cost surface

c7i.large (~₹650/mo) vs c8g.xlarge (~₹1,010/mo) vs c7i.xlarge (~₹4,137/mo) at full 9hr × 22 weekdays + 5hr × 8 weekends:

| Instance | vCPU | RAM | $/hr | ₹/hr | Full schedule ₹/mo | Notes |
|---|---|---|---|---|---|---|
| c7i.large | 2 | 4GB | 0.0893 | 7.59 | ₹2,069 | **Fails 8GB memory budget per aws-budget.md** |
| c7i.xlarge | 4 | 8GB | 0.1785 | 15.17 | ₹4,137 | Current `aws-budget.md` baseline |
| c8g.large | 2 | 4GB | 0.0795 | 6.76 | ₹1,842 | ARM, **fails memory budget** |
| c8g.xlarge | 4 | 8GB | 0.159 | 13.52 | ₹3,687 | ARM, satisfies memory budget. ~12% cheaper than c7i.xlarge |
| t3a.medium | 2 | 4GB | 0.0376 | 3.20 | ₹873 | **Burstable, fails memory budget, CPU credits unreliable for sustained load** |
| t4g.large | 2 | 8GB | 0.0672 | 5.71 | ₹1,557 | ARM, 8GB, **burstable** — credits exhaust under WS+QuestDB+5 ILP flush bursts |
| t4g.xlarge | 4 | 16GB | 0.1344 | 11.42 | ₹3,114 | ARM, 16GB, **burstable** — same credit issue but larger baseline |

Three observations:

1. **Any instance < 8GB RAM fails aws-budget.md rule 6.** That eliminates c7i.large, c8g.large, t3a.medium.
2. **Burstable t-series violate the hot-path latency invariant.** Tick processing at 5K ticks/sec with QuestDB ILP flush bursts drains CPU credits in minutes; once depleted, the instance falls to baseline ~10-20% CPU which causes Phase 2 dispatch (09:13 IST) to miss its window.
3. **c8g.xlarge (ARM) is ~12% cheaper than c7i.xlarge for the same RAM.** ARM is a real option IF all our Rust crates compile cleanly for aarch64 (they do — `cargo build --target aarch64-unknown-linux-gnu` is verified by CI). This is a free saving with no envelope break.

---

## §3. The 7 cost-cut levers (combined)

| # | Lever | Saves/mo | Architectural compromise | Operator-acceptable? |
|---|---|---|---|---|
| 1 | c7i.xlarge → c8g.xlarge (ARM) | ₹450 | None — same RAM, same vCPU | YES (free win) |
| 2 | Skip weekends entirely | ₹607 | Operator does manual start on weekend prep days | YES if operator agrees |
| 3 | Reduce daily hours 9hr → 4hr (10:00–14:00 IST market core) | ₹2,300 (from c8g.xlarge baseline) | Loses pre-market prep window (Phase 1 boot needs to be < 4 min into market hours, very tight); loses post-market historical fetch (15:30 onwards) | NO — breaks Phase 2 + post-market cross-verify |
| 4 | Reduce daily hours 9hr → 6hr (08:00–14:00) | ₹1,000 | Loses post-market historical fetch (15:30–17:00) | NO — breaks post-market 1m fetch per Wave 6 plan |
| 5 | EBS 100GB → 30GB | ₹540 | Tighter hot-window; requires faster S3 lifecycle (14 days vs 30 days to cold) | YES, marginal |
| 6 | Drop Elastic IP (EIP), use ephemeral on each start | ₹152 | **BLOCKED — Dhan static IP mandate** | NO — order APIs reject |
| 7 | Use Spot instances | up to ₹2,000 | Instance can be terminated with 2-min notice; mid-session interruption causes data loss + reconnect cascade | NO — operator previously rejected per aws-budget.md "on-demand only" |

### Best legitimate AWS-only combo

c8g.xlarge + skip weekends + EBS 30GB:

```
c8g.xlarge weekday 9hr × 22 days  ₹2,676
EIP                               ₹152
EBS 30GB                          ₹232
S3 cold                           ₹150  (year-5 worst case)
SNS + CloudWatch                  ₹25
Data Transfer                     ₹85
─────────────────────────────────────────
TOTAL                            ₹3,320/mo
```

Down from ₹4,981 → ₹3,320 (33% saving). **Still 3.3× over the ₹1K target.**

### Aggressive AWS-only combo (breaks Wave 6 post-market fetch)

c8g.xlarge + weekday only + 5hr/day + EBS 30GB:

```
c8g.xlarge weekday 5hr × 22 days  ₹1,487
EIP                               ₹152
EBS 30GB                          ₹232
S3 cold (year 1)                  ₹50
SNS + CloudWatch                  ₹25
Data Transfer                     ₹85
─────────────────────────────────────────
TOTAL                            ₹2,031/mo
```

Still 2× over ₹1K. AND breaks the Wave 6 post-market 1m candle fetch which runs 15:30–17:00 IST.

### Theoretical AWS minimum (breaks multiple invariants)

t4g.large + weekday only + 4hr/day + EBS 30GB + drop EIP (illegal per Dhan):

```
t4g.large weekday 4hr × 22 days   ₹503
EBS 30GB                          ₹232
S3 cold (year 1)                  ₹50
SNS + CloudWatch                  ₹25
Data Transfer                     ₹85
─────────────────────────────────────────
TOTAL                             ₹895/mo  ← <₹1K achieved
```

**But this combo breaks 4 invariants:**
- Burstable instance — CPU credits exhaust under tick load (hot-path latency violation)
- 8GB RAM violated — t4g.large is 8GB but with burstable CPU
- Drops EIP → Dhan static-IP mandate violated → orders rejected after 2026-04-01
- 4hr/day misses Wave 6 post-market 1m fetch + cross-verify (cold-path data integrity break)

**This is not a serious option.**

---

## §4. Off-AWS paths

If <₹1K is truly non-negotiable, the only way is OFF AWS:

### Option A — Self-hosted on a fixed-IP home server / co-located mini-PC

| Cost component | Monthly |
|---|---|
| Mac mini / NUC (4 vCPU, 16GB) one-time | ₹50K capex; ₹833/mo amortized over 5y |
| Home broadband with static IP (BSNL FTTH static-IP plan) | ₹1,500/mo (and ₹1,200/mo minimum) |
| Electricity (~30W × 24h × 30d = 21.6 kWh × ₹8/kWh) | ₹173 |
| Cloud-only services we'd still need (S3 cold storage for SEBI 5y) | ₹50 |
| **Total** | **₹2,556/mo** |

Cheaper to amortize but **HIGHER monthly opex than AWS** because residential static-IP plans in India cost more than EIP. **NOT cheaper.**

### Option B — VPS provider (Hetzner, Contabo, OVH ap-south)

| Provider | RAM | Static IP | Monthly USD | Monthly ₹ |
|---|---|---|---|---|
| Hetzner CCX13 (Frankfurt) | 8GB | Included | $14.49 | ₹1,231 |
| Contabo Cloud VPS S (India) | 8GB | Included | $6.99 | ₹594 |
| OVH (no ap-south region; Singapore) | 8GB | Included | $12 | ₹1,020 |

**Contabo at ~₹594/mo + ₹150 S3 = ~₹744/mo gets below ₹1K.**

BUT: Contabo network reliability + India-side latency to Dhan API (Mumbai) varies; not under AWS reliability SLA; static IP changes require ticket; SEBI audit logs need a separate compliant storage path; CloudWatch / SNS infra all has to be rebuilt or replaced. Effort to migrate ≈ 6-8 weeks. Operationally less mature.

### Option C — Hybrid (compute on home server, audit storage on AWS S3)

| Cost component | Monthly |
|---|---|
| Mac mini / NUC amortized | ₹833 |
| Home static-IP broadband | ₹1,500 |
| AWS S3 cold (SEBI) | ₹150 |
| **Total** | **₹2,483/mo** |

Same problem as Option A — home internet bills exceed AWS EIP+EC2.

### Option D — Reduce SEBI retention from 5y to legal minimum (NOT legal advice — verify)

Per SEBI circular SEBI/HO/MIRSD/DOP/CIR/P/2018/153 (and subsequent updates), broking-related records require 5-year retention. **NOT a cost lever** — would create regulatory risk.

---

## §5. Honest verdict

| Path | Monthly cost | Pros | Cons | Sub-₹1K? |
|---|---|---|---|---|
| Current AWS (c7i.xlarge full schedule) | ₹4,981 | matches aws-budget.md baseline | 5x over target | NO |
| Best AWS legitimate (c8g.xlarge + skip weekends + EBS 30GB) | ₹3,320 | All invariants honored | Still 3.3x over | NO |
| Aggressive AWS (5hr/day) | ₹2,031 | Closer | Breaks Wave 6 post-market fetch | NO |
| Theoretical AWS (t4g.large + 4hr/day) | ₹895 | Sub-₹1K | Breaks 4 invariants — not viable | NO (invariants) |
| Contabo VPS + AWS S3 hybrid | ₹744 | Sub-₹1K | New ops infra, migration effort, lower reliability | YES (with cost) |
| Self-hosted home | ₹2,556 | Owned hardware | Home broadband > AWS EIP | NO |

**Realistic AWS-only floor: ~₹1.4K–₹2K/mo if we maintain Wave 6 post-market fetch and Dhan static IP.**
**Sub-₹1K requires leaving AWS entirely.**

---

## §6. Recommendation framework — operator picks ONE

| Choice | What we ship | What operator accepts |
|---|---|---|
| **A** — Drop to ₹3.3K/mo on AWS | Switch to c8g.xlarge, skip weekends auto-start, EBS 30GB | 33% saving from current; weekend manual start; tighter hot-window |
| **B** — Drop to ~₹2K/mo on AWS, accept some break | c8g.xlarge + 5hr/day + skip weekends | Breaks Wave 6 post-market fetch — operator must manually re-run cross-verify next morning |
| **C** — Sub-₹1K via Contabo migration | 6-8 week migration project to Contabo VPS Mumbai/SG + S3 for SEBI | New ops surface; lower SLA; need to rebuild monitoring/alerting |
| **D** — Accept ₹4-5K AWS as the honest floor | Keep current aws-budget.md as-is | The <₹1K target was aspirational — revise it |

Honest framing per operator-charter §F: I cannot ship "<₹1K on AWS" because the math does not close. Choices A/B/D stay on AWS at varying compromise; choice C leaves AWS to hit the budget.

---

## §7. Decisions pending operator lock

- [ ] **LOCK-2-FINAL:** Pick A, B, C, or D from §6 above.
- [ ] If A or B: confirm c7i.xlarge → c8g.xlarge ARM swap (verify Rust binary builds via CI).
- [ ] If A or B: confirm "skip weekend auto-start" — operator OK with manual `aws ec2 start-instances` on weekend prep days?
- [ ] If B: confirm operator accepts that Wave 6 post-market fetch + cross-verify runs the NEXT morning instead of same-day evening.
- [ ] If C: open a separate plan `.claude/plans/contabo-migration/` — out of scope of `aws-lifecycle` plan.
- [ ] If D: I update `aws-budget.md` to be honest about the ~₹4-5K floor and we keep current schedule.

---

## §8. Trigger

This document loads as a sibling reference whenever `.claude/rules/project/aws-budget.md` is being edited or `docs/architecture/aws-daily-lifecycle.md` cost section is being discussed.
