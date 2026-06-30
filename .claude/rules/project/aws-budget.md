# AWS Budget Enforcement — t4g.medium LOCKED ~₹1,022/mo

> **⚠ SUPERSEDED 2026-05-27 by [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md):** instance upgraded t4g.medium → t4g.large (8 GiB), bill ~₹1,022/mo → ~₹1,514/mo, cron 08:00 → 08:30 IST. Contents below retained as 2026-05-18 historical audit; current effective contract lives in the superseding file.
>
> **⚠ FURTHER SUPERSEDED → r8g.large 2026-06-30 (operator Quote 7 in [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md) §7):** instance upgraded m8g.large → **r8g.large** (Graviton4, 2 vCPU / 16 GiB), bill → ~₹2,919/mo incl GST (270 hrs, 30 GB EBS, +EIP kept). The current effective instance lock lives in that file's §7.
>
> **Authority:** Parthiban (architect). Non-negotiable.
> **Ground truth:** `docs/architecture/aws-indices-only-locked-architecture.md` §5 (instance lock 2026-05-18) and the 2026-05-20 CloudWatch-only decision below.
> **Scope:** Any file touching AWS deployment, infrastructure, Docker config, or cost-impacting changes.

## OPERATOR DECISION 2026-05-20 — Observability stack → CloudWatch-only

> **Operator (Parthiban), 2026-05-20:** "except questdb app and cloud
> watch we planned to remove everything."

The runtime is being narrowed to **THREE components only**:

| Keep | Role |
|---|---|
| **QuestDB** | the single data plane — 24-table KEEP set |
| **tickvault app** | the host process |
| **AWS CloudWatch** | the entire observability layer — metrics, logs, alarms |

**REMOVED:** Grafana, Prometheus, Alertmanager, **Valkey**. The frontend / portal /
TradingView terminal and Jaeger / Loki / Alloy / Traefik were already removed in earlier PRs.

**Execution is staged, NOT done here.** This is a multi-PR program —
see `.claude/plans/active-plan-observability-cloudwatch-only.md`.
**Valkey is load-bearing** (dual-instance lock + token cache); its
removal is the highest-risk PR and must not be half-shipped.

## OPERATOR DECISION 2026-05-18 — Instance LOCKED to t4g.medium

> See `docs/architecture/aws-indices-only-locked-architecture.md` §5
> "The instance — t4g.medium LOCKED 2026-05-18 (FINAL, NO COMPARISONS)".

**Why it shrank from c7i.xlarge → t4g.medium:**

- Universe narrowed to 4 IDX_I SIDs (`LOCKED_UNIVERSE` — NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21)
- Rescue ring trimmed 5M → 100K ticks (~980 MB freed)
- Depth-20 / depth-200 / Phase 2 / movers / greeks pipelines all deleted (PRs #2-#6b)
- App working set dropped to ~280-700 MB; total host need ~2 GB

## Authoritative Bill — t4g.medium, every day 08:00–17:00 IST

| Line | Spec | Unit Price | Monthly ₹ |
|---|---|---|---|
| EC2 t4g.medium | ARM Graviton 2 vCPU, 4 GiB, on-demand, 9hr × 30 days | $0.0224/hr | ₹514 |
| EIP (24/7) | 1 static IP — Dhan static-IP mandate | $0.005/hr × 720h | ₹306 |
| EBS gp3 | 10 GB (tight — 4-SID dataset is small) | $0.0912/GB-mo | ₹78 |
| S3 cold | Tiny dataset (4 SIDs) — Intelligent-Tier → Glacier auto | $0.025→$0.002/GB-mo | ₹15 |
| CloudWatch | 10 custom metrics + 5GB logs (free tier) + 18 alarms (13 app + 5 infra; first 10 free, 8 over @ $0.10) | ~$0.80 | ₹68 |
| SNS SMS | ~100 India SMS/mo | $0.00278/msg | ₹24 |
| SNS Email / HTTPS / Lambda | free tier | $0 | ₹0 |
| Data transfer | ~10 GB outbound | ~$0.01/GB | ₹85 |
| **TOTAL** | | | **~₹1,022/mo** |

**Honest envelope:** ₹1,022 is ~₹22 over the <₹1,000 target. Operator locked Option A — accept ₹22 overage for 7-day weekend availability (BRUTEX work).

**Pricing correction (kept for record):** Mumbai t4g.medium = $0.0224/hr per the operator's AWS console screenshot. An earlier research agent quoted $0.0392/hr (43% high) — that error would have pushed the bill to ~₹1,400/mo.

## Instance Schedule (LOCKED)

- **Start:** 08:00 IST every day Mon–Sun (EventBridge cron `cron(30 2 * * ? *)`)
- **Stop:** 17:00 IST every day Mon–Sun (EventBridge cron `cron(30 11 * * ? *)`)
- **Manual:** start/stop anytime via `aws ec2 start-instances` / Console
- **EIP:** stays associated 24/7 — Dhan static IP has 7-day cooldown on modify; never release
- **Cost per running hour:** **₹1.90** ($0.0224 × ₹85)
- **Cost per stopped hour (EBS + EIP only):** **₹0.51**

## Mechanical Rules

1. **Instance type is t4g.medium. PERIOD.** Going larger requires:
   - Operator explicit approval with dated quote
   - Update to `docs/architecture/aws-indices-only-locked-architecture.md` §5
   - Update to this file
   - Ratchet test pinning the new spec
2. **EBS stays at 10 GB.** 4-SID dataset is tiny. If it grows, partition manager → S3 cold tier; do NOT enlarge the volume.
3. **NEVER add paid AWS services** (RDS, ElastiCache, NAT Gateway, ALB, etc.) without budget review.
4. **CloudWatch is MANDATORY and always enabled** — within free tier (10 metrics + 10 alarms + 5GB logs).
5. **S3 lifecycle: auto-tier** Standard → Intelligent-Tiering @ 90d → Glacier Deep Archive @ 365d. SEBI 5y retention at ₹0.17/GB/mo.
6. **Host memory budget for t4g.medium (4 GiB total) — POST CloudWatch-only migration:**
   - QuestDB: ~1.5 GB (write-mostly, 4 SIDs, 80% lower write pressure post Wave-6)
   - Tickvault app: ~700 MB actual / 1.5 GB cap (today + yesterday sealed bars + indicator state + 100K rescue ring)
   - OS + FS cache: ~400 MB (tracing log writes, audit flush bursts, kernel TCP buffers)
   - **Total used: ~2.6 GB**
   - **Headroom: ~1.4 GB** — above the 1 GB Linux kswapd floor
7. **Pre-migration (Valkey/Prom/Alertmanager still running) is OVER-BUDGET on t4g.medium 4 GiB.** The CloudWatch-only migration plan must complete BEFORE the prod instance flips to t4g.medium. Until then, dev runs locally on Mac and prod stays unprovisioned.
8. **NO manual configuration on AWS deployment** — every setting (memory, schedule, alarms, audits) lives in version-controlled config files. `git clone` + `docker compose up -d` reproduces the runtime identically on Mac dev and AWS prod.
9. **RAM-first hot path (mandatory):** tick → strategy decision must read indicator state, today's sealed bars, yesterday's sealed bars, and prev_day_OI cache from RAM only. QuestDB is for: persistence, audit, cross-verify (cold path), boot rehydration. Banned-pattern guard blocks any indicator/strategy code path that issues SELECT against QuestDB during market hours.
10. **Alerting:** CloudWatch alarm → SNS → 4-channel fan-out (SMS + Telegram via Lambda webhook + Email + Connect outbound call). Standard pattern documented in `aws-indices-only-locked-architecture.md` §6.

## What This Prevents

- Accidentally provisioning anything other than t4g.medium (next step up t4g.large = ~₹1,262/mo, +₹240)
- EBS growing unbounded (10 GB → 100 GB = ₹702/mo extra; 4-SID workload doesn't need it)
- Adding CloudWatch beyond free tier (₹0.67/GB log ingestion adds up fast)
- Forgetting Elastic IP charges when instance is stopped (₹306/mo regardless of running state)
- Adding managed services that balloon the bill
- OOM killer striking under burst load (1+ GB headroom prevents this on 4 GiB)
- DB-dependent hot-path latency (RAM-first rule prevents this)

## Risks Accepted (t4g.medium lock)

| Risk | Mitigation |
|---|---|
| 4 GiB RAM has ~1.4 GB headroom after slimmed stack | CloudWatch `MemoryUtilization` alarm at 75%; ample for ~2.6 GB working set |
| Burstable CPU could exhaust under 09:15:30 IST bursts | Audit: 4 SIDs at ~20 ticks/sec = trivial. Baseline 40% × 2 vCPU = 0.8 vCPU effective; cumulative CPU credits handle any spike. |
| Single-AZ — InsufficientInstanceCapacity | Manual fallback to alternate AZ per `aws-capacity-error.md`. 99.99% region SLA ≈ 4.3 min/mo expected downtime. |
| ap-south-1 region outage | Accept envelope. Documented as honest-envelope limit. |
| Future BRUTEX strategy load growth >10× | t4g.medium still fits; t4g.large is the rip-cord (~₹1,262/mo) |

## RAM-First Architecture (mandatory)

### Hot Path (RAM ONLY — no DB hits)

| Operation | Source |
|---|---|
| Tick → indicator update | RAM (live aggregator + today/yesterday sealed bars + running state) |
| Indicator → strategy decision | RAM (everything resident) |
| Strategy decision → order construction | RAM (token, instrument cache) |
| Risk check (margin, exposure) | RAM (OMS state, prev_day_OI cache) |
| Order out → wire | RAM → TLS → Dhan |

### Cold Path (DB allowed)

| Operation | DB allowed |
|---|---|
| Tick persistence | ✅ async ILP write |
| Candle seal flush | ✅ async ILP write |
| Audit trail INSERT | ✅ async ILP write |
| Boot rehydration (one-time) | ✅ SELECT today's bars |
| Post-market cross-verify | ✅ SELECT all 1m bars |
| Operator query (manual debugging) | ✅ QuestDB Web Console |

### Banned Pattern (CI-enforced)

```
banned-pattern-scanner.sh blocks:
- SELECT inside crates/trading/src/strategy/*
- SELECT inside crates/trading/src/indicator/*
- SELECT inside crates/core/src/pipeline/tick_processor.rs
- SELECT inside crates/trading/src/oms/risk_check.rs
- Any code path between WS read and order out that hits QuestDB
```

## 100% Coverage Verification (POST CloudWatch-only migration)

| Need | Tool | Status |
|---|---|---|
| Tracking | QuestDB audit tables (15+) | ✅ KEEP |
| Logging | tracing → CloudWatch Logs (`/tickvault/prod/app`, 14d retention) | ✅ migrate from local errors.jsonl |
| Monitoring | CloudWatch custom metrics (10 free tier) | ✅ replaces Prometheus |
| Alerting | CloudWatch alarms → SNS → 4-channel fan-out | ✅ replaces Alertmanager |
| Auditing | QuestDB audit tables + S3 cold archive | ✅ KEEP |
| Capturing | QuestDB ticks + candles | ✅ KEEP |
| Visualizing | QuestDB Web Console (Grafana retired) | ⚠ scoped — operator-only, not 24/7 |
| Dashboards | CloudWatch Dashboards (3 free tier) | ✅ replaces Grafana operator-health page |
| Distributed tracing | CloudWatch X-Ray (optional, free tier 100K traces/mo) | ⚠ optional |
| Zero tick loss envelope | 100K rescue ring + spill + DLQ | ✅ 60s+ absorbed at 4-SID rates |
| RAM-first hot path | today + yesterday sealed bars in RAM | ✅ enforced by banned-pattern |

## Common Runtime / Dynamic / Scalable / Automated Charter

Operator demand 2026-05-10: "extremely common runtime dynamic scalable approach,
fully comprehensively automated, logged, tracked, captured, visualized, alerted,
notified on Telegram, no manual inputs."

| Demand | Mechanical enforcement |
|---|---|
| Common runtime | Same `docker-compose.yml` Mac dev = AWS prod (rule 8) |
| Dynamic | EventBridge auto-start/stop, dynamic SLO score 10s |
| Scalable | Bounded mpsc + spill ring + DLQ; QuestDB partition manager prunes old data automatically; t4g.large is the rip-cord for >10× growth |
| Automated logging | tracing macros mandatory; ERROR routes to CloudWatch Logs; hourly local errors.jsonl + 48h retention sweep (transitional) |
| Automated tracking | 15+ audit tables auto-INSERT on every typed event with DEDUP UPSERT KEYS |
| Automated capturing | every tick auto-persists to QuestDB; spill NDJSON catches overflow; auto-replays on rehydration |
| Automated visualizing | CloudWatch Dashboards auto-provisioned via Terraform / aws CLI |
| Automated alerting | CloudWatch alarms evaluate every 30s; SNS auto-routes by severity to 4 channels |
| Automated notifications | SNS → Lambda → Telegram webhook; `Severity::High`/`Critical` auto-page operator |
| No manual inputs | Boot sequence fully automatic: bootstrap.sh pulls SSM → docker compose up → app self-tests via `make doctor` → 3-tier fallback (cache → SSM → TOTP) |
| RAM-first hot path | banned-pattern guard blocks DB queries from indicator/strategy/risk paths |

## Trigger

This rule activates when editing files matching:
- `deploy/docker/docker-compose.yml`
- `deploy/aws/*`
- `scripts/aws-*`
- `crates/app/src/infra.rs`
- `crates/common/src/constants.rs` (`TICK_BUFFER_CAPACITY`)
- `crates/trading/src/indicator/*` (RAM-first guard)
- `crates/trading/src/strategy/*` (RAM-first guard)
- Any file containing `t4g.medium`, `t4g`, `c7i`, `c8g`, `mem_limit`, `EBS`, `gp3`, `instance_type`, `aws_region`, `TICK_BUFFER_CAPACITY`
