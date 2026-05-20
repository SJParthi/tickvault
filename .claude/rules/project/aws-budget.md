# AWS Budget Enforcement — ₹5,000/mo Hard Cap

> **Authority:** Parthiban (architect). Non-negotiable.
> **Scope:** Any file touching AWS deployment, infrastructure, Docker config, or cost-impacting changes.

## Budget: ₹5,000/month MAXIMUM (all AWS services included)

### Approved AWS Bill (verified from AWS Pricing API, ap-south-1, 2026-04-08)

| Component | Spec | Unit Price (USD) | Monthly ₹ |
|-----------|------|-----------------|-----------|
| EC2 (weekdays) | c7i.xlarge, on-demand, 9hr × 22 weekdays | $0.1785/hr | ₹3,530 |
| EC2 (weekends) | c7i.xlarge, on-demand, 5hr × 8 weekends | $0.1785/hr | ₹607 |
| EBS | gp3, 100GB (hot data: last 30-60 days) | $0.0912/GB-mo | ₹775 |
| S3 | Up to 500GB cold data (lifecycle tiered) | $0.025→$0.002/GB-mo | ₹333 |
| Elastic IP | 1 static IP (mandatory for Dhan Order API) | $0.005/hr idle | ₹152 |
| CloudWatch | ALWAYS ON: 7 metrics + 5 alarms + 2GB logs | FREE (within free tier) | ₹0 |
| SNS | ~100 SMS alerts/month (India) | $0.00278/msg | ₹25 |
| Data Transfer | ~10GB outbound | ~$0.01/GB | ₹85 |
| **TOTAL** | | | **₹4,981** |
| **Buffer** | | | **₹19** |

> Worst case with 500GB S3 cold data. Actual S3 cost starts at ₹106 (50GB)
> and grows ~₹10/mo. Takes 4-5 years to reach 500GB. Budget holds.

### Pricing Type: ON-DEMAND (not reserved, not spot)
- No upfront commitment. Pay only when instance is running.
- Weekdays: 8:30 AM – 5:30 PM IST (9hr, auto via EventBridge Scheduler).
- Weekends: 8:30 AM – 1:30 PM IST (5hr, auto via EventBridge Scheduler).
- Manual: start/stop anytime via AWS Console or CLI for extra checks.
- Cost per hour: **₹15.17** ($0.1785 × ₹85).

## Data Lifecycle: Hot (EBS) → Cold (S3) — Up to 500GB

| Data Age | Storage Tier | Size | ₹/GB-mo | ₹/mo |
|----------|-------------|------|---------|------|
| 0-60 days | EBS gp3 (100GB) | ~100GB | ₹7.75 | ₹775 |
| 60-365 days | S3 Intelligent-Tiering | ~150GB | ₹1.17 | ₹176 |
| 1-5 years | S3 Glacier Deep Archive | ~300GB | ₹0.17 | ₹51 |
| **Total (worst case)** | | **500GB** | | **₹333/mo on S3** |

S3 lifecycle policy (auto, no code needed):
- 0-90 days after upload: S3 Standard ($0.025/GB)
- 90-365 days: S3 Intelligent-Tiering ($0.0138/GB) — auto
- >365 days: Glacier Deep Archive ($0.002/GB) — auto
- SEBI 5-year retention: satisfied at ₹0.17/GB/mo

Partition manager (already implemented) detaches QuestDB partitions older than retention_days.
S3 archival exports detached partitions before removal (Plan Item 7, needs aws-sdk-s3).

## Mechanical Rules

1. **NEVER provision a larger instance than c7i.xlarge** without Parthiban's approval.
2. **NEVER increase EBS beyond 100GB** without Parthiban's approval.
   If 100GB fills up, partition manager detaches old data → export to S3 → free space.
3. **NEVER add paid AWS services** (RDS, ElastiCache, NAT Gateway, etc.) without budget review.
   AWS ALB allowed within free tier (750 hrs/mo) replacing Traefik.
4. **CloudWatch is MANDATORY and always enabled** — monitoring, logging, alerting are non-negotiable.
   Free tier covers: 10 custom metrics, 10 alarms, 5GB logs, 3 dashboards. Stay within free tier.
5. **S3 lifecycle policy is MANDATORY** — auto-tier to Intelligent-Tiering (90d) → Glacier (365d).
   Keeps 500GB cold data under ₹333/mo instead of ₹1,063/mo.
6. **Host memory budget for c7i.xlarge / c8g.xlarge (8GB)** — Wave 7-A4 (2026-05-10):
   - **Docker containers (5 services, ~3.42 GB):**
     - QuestDB: **1.5GB** (Wave 7-A4: trimmed from 2GB; reduced workload post Wave-6 cascade removal allows tighter cap)
     - Grafana: 768MB
     - Valkey: 512MB
     - Prometheus: 384MB (7d retention)
     - Alertmanager: 256MB (KEPT — independent process for app-crash alerts)
   - **Host process — Tickvault app: 2.0 GB cap** (today's + yesterday's sealed bars in RAM + indicator state + 100K tick rescue ring; RAM-only hot path for trading decisions — actual use ~700 MB after the 2026-05-20 ring trim)
   - **OS + filesystem cache: 600 MB** (tracing log writes, audit flush bursts, kernel TCP buffers)
   - **Total used: ~6.0 GB**
   - **Headroom (HARD FLOOR): 2.0 GB** — Linux kswapd needs ≥1 GB free; 2 GB prevents OOM under bursts
   - **REMOVED in Wave 7-A:** Traefik (use AWS ALB free tier), valkey-exporter (not queried).
   - **Already removed:** Jaeger, Loki, Alloy (saves 2.5GB RAM).
7. **Manual starts budgeted at 20hr/month max.** If consistently exceeding, review schedule.
8. **HTTP gateway:** Use AWS ALB (free tier 750 hrs/mo) for HTTPS termination, or app on port 3001 directly behind security group for internal-only access.
9. **Alert routing:** Prometheus → Alertmanager → Telegram (standard pattern). Alertmanager runs as an independent container so app-crash alerts still reach the operator. Routing alerts through the app itself is a single-point-of-failure anti-pattern (rejected 2026-05-10).
10. **NO manual configuration on AWS deployment** — every setting (memory, schedule, alerts, dashboards, audits) lives in version-controlled config files. `git clone` + `docker compose up -d` reproduces full stack identically on Mac dev and AWS prod.
11. **Headroom floor is non-negotiable: 2 GB minimum free RAM at all times** (Wave 7-A2). Below this, Linux kswapd thrashes, OOM killer becomes aggressive, latency jitter spikes. CI ratchet `test_total_host_memory_below_6_gb_ceiling` enforces it.
12. **RAM-first architecture (Wave 7-A4): NO DB queries on hot path.** Tick → strategy decision must read indicator state, today's sealed bars, yesterday's sealed bars, and prev_day_OI cache from RAM only. QuestDB is for: persistence, audit, cross-verify (cold path), boot rehydration. Banned-pattern guard blocks any indicator/strategy code path that issues SELECT against QDB during market hours.

## What This Prevents

- Accidentally provisioning c7i.2xlarge (doubles EC2 cost to ₹6,008)
- EBS growing unbounded (200GB = ₹1,550, 500GB = ₹3,876 — blows budget)
- Adding CloudWatch Logs beyond free tier (₹0.67/GB adds up fast)
- Forgetting Elastic IP charges when instance is stopped
- Adding managed services that balloon the bill
- S3 costs exploding (Intelligent-Tiering + Glacier keep cold data cheap)
- App-crash alerts vanishing (Alertmanager independence prevents this)
- Manual config drift between Mac and AWS (single compose file)
- OOM killer striking under burst load (2 GB headroom floor prevents this)
- DB-dependent hot-path latency (RAM-first rule prevents this)

## Instance Schedule (Wave 7-A — 2026-05-10 update)

- **Weekday Start:** 8:30 AM IST Mon-Fri (EventBridge: `cron(0 3 ? * MON-FRI *)`)
- **Weekday Stop:** 5:30 PM IST Mon-Fri (EventBridge: `cron(0 12 ? * MON-FRI *)`)
- **Weekend Start:** 8:30 AM IST Sat-Sun (EventBridge: `cron(0 3 ? * SAT,SUN *)`)
- **Weekend Stop:** 1:30 PM IST Sat-Sun (EventBridge: `cron(0 8 ? * SAT,SUN *)`)
- **Manual Start:** Anytime — `aws ec2 start-instances --instance-ids i-xxx` or AWS Console
- **Manual Stop:** Anytime — `aws ec2 stop-instances --instance-ids i-xxx` or AWS Console
- **Static IP:** Elastic IP stays associated 24/7 (never release — Dhan static IP has 7-day cooldown)
- **Skip weekends to save:** Each skipped weekend day saves ₹76 (5hr × ₹15.17)

> **Note:** Schedule shifted from 7:45→17:15 to 8:30→17:30 (still 9 hours, ~₹170/mo savings). Aligns with Wave 6 plan post-market 1m fetch window: market closes 15:30, fetch + cross-verify + persist by 17:25, AWS auto-stops 17:30 with 5 min buffer.

## Cost Per Hour Reference

| Action | Cost |
|--------|------|
| Instance running | ₹15.17/hr ($0.1785) |
| Instance stopped (EBS + EIP) | ₹1.43/hr ($0.0168) |
| 1 weekend day (8hr manual) | ₹121 |
| 1 quick check (2hr manual) | ₹30 |

## Host Memory Budget — Wave 7-A4 Locked (2026-05-10)

**c8g.xlarge / c7i.xlarge — 8 GB total:**

| Component | RAM | Type | Notes |
|---|---|---|---|
| **Tickvault app** | **2.0 GB cap** | **Host process** | **Today + Yesterday sealed bars in RAM + indicator state + 100K rescue ring + RAM-only hot path (actual ~700 MB)** |
| QuestDB | 1.5 GB | Docker | Time-series DB (cold-path persistence + cross-verify) |
| Grafana | 768 MB | Docker | Dashboards |
| **OS + FS cache** | **600 MB** | **Host kernel** | **tracing log writes + audit bursts** |
| Valkey | 512 MB | Docker | Token + instrument cache |
| Prometheus | 384 MB | Docker | 7d retention |
| Alertmanager | 256 MB | Docker | Independent alert routing |
| **TOTAL USED** | **~6.0 GB** | | |
| **HEADROOM (hard floor)** | **2.0 GB** | | OOM safety margin |

### Tickvault App (2.0 GB) Memory Breakdown — Wave 7-A4

| Component | Math | RAM |
|---|---|---|
| Live aggregator (current bar per TF) | 11,045 × 9 × 80B | 8 MB |
| **Today's sealed bars (compact 32B, all 9 TFs)** | up to 5.5M × 32B | **176 MB** |
| **Yesterday's sealed bars (compact 32B, all 9 TFs)** | 5.5M × 32B | **176 MB** |
| **prev_day OHLCVOI cache** | 11,045 × 16B | **180 KB** |
| Indicator running state (RSI/MACD/BB/EMA/SMA) | 99,405 × 500B | 50 MB |
| SPSC channels (5 writers × 65K slots × 200B) | | 65 MB |
| Token + instrument cache | 10K entries × 1KB | 10 MB |
| WebSocket buffers (5 conns × 4MB recv) | 5 × 4 MB | 20 MB |
| OMS + Greeks pipeline | misc | 10 MB |
| Tracing + log writer queues | bounded | 5 MB |
| Tokio runtime + 4 thread stacks | 4 × 2 MB + internal | 20 MB |
| **WORKING SET SUBTOTAL** | | **~540 MB** |
| **Rescue ring buffer (100K ticks × 200 bytes)** | 100K × 200 | **20 MB** |
| Heap fragmentation (~25% w/ jemalloc tuning) | | 140 MB |
| **APP TOTAL** | | **~700 MB** |
| **App safety margin inside 2 GB cap** | | **~1.35 GB** ✅ |

> **2026-05-20 — rescue ring rightsized 5M → 100K.** The universe
> narrowed to 4 IDX_I SIDs (~15-20 tps); a 5M ring (1.0 GB) buffered
> ~130 hours for a feed that needs minutes. Trimmed to 100K (~20 MB),
> freeing ~980 MB. The 2.0 GB app cap is now generously loose — a full
> re-budget is a separate follow-up.

### Why 2 GB Host Headroom Is Non-Negotiable

| Reason | What goes wrong without it |
|---|---|
| Linux kswapd needs ≥1 GB free | thrashes < 1 GB → page cache evictions |
| Kernel TCP buffers under burst | Direct Connect throughput dips |
| Docker daemon overhead | metrics scrape, log rotation, healthchecks |
| Service start/stop transients | restarts spike memory briefly |
| OOM killer behavior | aggressive < 1 GB free, kills random processes |
| NUMA + swap behavior | latency jitter spikes |

**Below 2 GB headroom → Linux gets twitchy. Above 2 GB → smooth operation.**

### Why QuestDB 1.5 GB Is Acceptable Post-Wave-6

| QuestDB workload | Pre-Wave-6 | Post-Wave-6 | RAM impact |
|---|---|---|---|
| `candles_1s` ingestion | 25M rows/day | REMOVED | -25M rows/day |
| Cascading mat views (9) | rebuilds on every 1s candle | REMOVED | -9× compounding |
| Sealed candle ingestion | — | 5.4M rows/day | small |
| Cross-verify reads | — | post-market only | cold path |
| **Net write pressure** | high | **80% lower** | 1.5 GB sufficient |

**QuestDB is now a write-mostly + cold-path-read store. 1.5 GB is enough; 2 GB was over-allocated.**

## RAM Trim Audit (Wave 7-A → Wave 7-A4 trajectory)

| Service | Pre-Wave-7 | Wave 7-A | Wave 7-A2 | **Wave 7-A4** | Final |
|---|---|---|---|---|---|
| QuestDB | 4 GB | 2 GB | 2 GB | **1.5 GB** | 1.5 GB |
| App | 500 MB | 500 MB | 1.5 GB | **2.0 GB** ⬆️ | 2.0 GB |
| Grafana | 1 GB | 768 MB | 768 MB | 768 MB | 768 MB |
| OS + FS cache | 100 MB | 200 MB | 600 MB | 600 MB | 600 MB |
| Valkey | 1 GB | 512 MB | 512 MB | 512 MB | 512 MB |
| Prometheus | 512 MB | 384 MB | 384 MB | 384 MB | 384 MB |
| Alertmanager | 256 MB | 256 MB | 256 MB | 256 MB | 256 MB |
| Traefik | 512 MB | REMOVED | — | — | — |
| Valkey-exporter | 128 MB | REMOVED | — | — | — |
| **Total Used** | ~7.9 GB | ~4.6 GB | ~6.0 GB | **~6.02 GB** | **~6.0 GB** |
| **Headroom** | ~100 MB ⚠️ | ~3.4 GB | ~2.0 GB | **~2.0 GB** ✅ | **2 GB (locked)** |

## RAM-First Architecture (Wave 7-A4 mandatory)

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
| Operator dashboard | ✅ Grafana SQL |

### Banned Pattern (CI-enforced)

```rust
// banned-pattern-scanner.sh blocks:
// - SELECT inside crates/trading/src/strategy/*
// - SELECT inside crates/trading/src/indicator/*
// - SELECT inside crates/core/src/pipeline/tick_processor.rs
// - SELECT inside crates/trading/src/oms/risk_check.rs
// - Any code path between WS read and order out that hits QuestDB
```

## 100% Coverage Verification (after Wave 7-A4 trim)

| Need | Tool | Status |
|---|---|---|
| Tracking | QuestDB audit tables (15+) | ✅ KEEP |
| Logging | tracing → errors.jsonl + CloudWatch Logs | ✅ KEEP local + add CW |
| Monitoring | Prometheus (14 new metrics in Wave 6 plan) | ✅ KEEP |
| Alerting | Prometheus → Alertmanager → Telegram (standard pattern) | ✅ KEEP (independent process) |
| Auditing | QuestDB audit tables + S3 cold archive | ✅ KEEP |
| Capturing | QuestDB ticks + candles | ✅ KEEP |
| Visualizing | Grafana | ✅ KEEP |
| Dashboards | Grafana operator-health single page | ✅ KEEP |
| HTTP gateway | AWS ALB (free tier) | ✅ replaces Traefik |
| Distributed tracing | CloudWatch X-Ray (optional, free tier 100K traces/mo) | ⚠️ optional |
| Zero tick loss envelope | Rescue ring 100K ticks (4-SID rightsized) | ✅ 60 sec+ absorbed at 4-SID rates |
| **RAM-first hot path** | **today + yesterday sealed bars in RAM** | ✅ Wave 7-A4 locks |

## Common Runtime / Dynamic / Scalable / Automated Charter (mandatory)

Operator demand 2026-05-10: "extremely common runtime dynamic scalable approach,
fully comprehensively automated, logged, tracked, captured, visualized, alerted,
notified on Telegram, no manual inputs."

| Demand | Mechanical enforcement |
|---|---|
| Common runtime | Same `docker-compose.yml` Mac dev = AWS prod (rule 10) |
| Dynamic | EventBridge auto-start/stop, dynamic depth-20/200 selectors, dynamic Phase 2 dispatch, dynamic SLO score 10s |
| Scalable | Bounded mpsc + spill ring + DLQ; horizontal: depth-20 5×50, depth-200 5×1; QuestDB partition manager prunes old data automatically |
| Automated logging | tracing macros mandatory; ERROR auto-routes to Telegram via Alertmanager; hourly errors.jsonl rotation; 48h retention sweep auto |
| Automated tracking | 15+ audit tables auto-INSERT on every typed event with DEDUP UPSERT KEYS |
| Automated capturing | every tick auto-persists to QuestDB; spill NDJSON catches overflow; auto-replays on rehydration |
| Automated visualizing | Grafana dashboards auto-provisioned via `grafana/provisioning/`; operator-health single-page renders without setup |
| Automated alerting | Prometheus alert rules in `alerts.yml` evaluate every 30s; Alertmanager auto-routes by severity to Telegram |
| Automated notifications | teloxide Telegram client + Alertmanager webhook; `Severity::High`/`Critical` auto-page operator |
| No manual inputs | Boot sequence is fully automatic: bootstrap.sh pulls SSM → Docker compose up → app self-tests via `make doctor` → 3-tier fallback (cache → SSM → TOTP) → market-open self-test at 09:16:30 IST |
| **RAM-first hot path** | banned-pattern guard blocks DB queries from indicator/strategy/risk paths |

## Wave 7-A3 Follow-Up (Rust code, deferred to Mac commit)

| Change | File | Status |
|---|---|---|
| `TICK_BUFFER_CAPACITY` rightsized 5M → 100K (4-SID universe) | `crates/common/src/constants.rs` | ✅ done 2026-05-20 |
| Today + yesterday sealed bar cache implementation | `crates/trading/src/indicator/bar_cache.rs` (new) | ⏳ pending Wave 6 sub-PR #1 |
| Boot-time rehydration of today's bars | `crates/core/src/boot/historical_loader.rs` (new) | ⏳ pending Wave 6 sub-PR #2 |
| Banned-pattern: SELECT from hot path | `.claude/hooks/banned-pattern-scanner.sh` | ⏳ pending Mac commit |
| DHAT zero-alloc verify with 100K ring + bar cache | `crates/core/tests/dhat_*.rs` | ⏳ pending |
| Adversarial 3-agent review | hot-path + security + bug-hunt | ⏳ pending Wave 6 sub-PR rollout |

## Trigger

This rule activates when editing files matching:
- `deploy/docker/docker-compose.yml`
- `deploy/aws/*`
- `scripts/aws-*`
- `crates/app/src/infra.rs`
- `crates/common/src/constants.rs` (TICK_BUFFER_CAPACITY)
- `crates/trading/src/indicator/*` (RAM-first guard)
- `crates/trading/src/strategy/*` (RAM-first guard)
- Any file containing `c7i`, `c8g`, `mem_limit`, `EBS`, `gp3`, `instance_type`, `aws_region`, `TICK_BUFFER_CAPACITY`
