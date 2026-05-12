# Topic — AWS Budget Automation + Mac↔AWS Parity (Common Runtime)

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `aws-budget.md` rule 10 (common runtime) > `wave-5-error-codes.md` (CORE-PIN) > this file.
> **Trigger:** Operator demand 2026-05-12 14:10 IST: comprehensive AWS budget monitoring + Mac mirror parity + core pinning + check everything.

---

## 🚗 Auto-Driver Story (60-second read)

> Sir, our trading system runs in TWO places:
> 1. **Your MacBook** (dev / local trading)
> 2. **AWS Mumbai c8g.xlarge** (production / scheduled trading)
>
> Both must be IDENTICAL twins. Same Docker images. Same config files. Same memory layout. Same core pinning. Same indicators. Same strategies. Same dashboards. Same alerts.
>
> If they diverge even slightly → bug behaviour differs → debugging impossible.
>
> Plus: AWS costs money. Daily budget tracking. If we drift over ₹5,000/month → Telegram alert.
>
> Today: design the AUTOMATION that keeps Mac = AWS, plus the AWS budget guardrails.

---

## 📋 Section 1 — AWS Budget Automation (₹5,000/month cap)

### Current budget (per `aws-budget.md`)

| Component | ₹/month |
|---|---|
| EC2 (c7i/c8g.xlarge on-demand, ~280 hrs/mo) | 3,530 (weekdays) + 607 (weekends) = 4,137 |
| EBS gp3 100 GB | 775 |
| Elastic IP (always associated) | 152 |
| S3 (up to 500 GB cold) | 333 (worst case) |
| CloudWatch | 0 (free tier) |
| SNS | 25 (~100 SMS) |
| Data Transfer | 85 (~10 GB) |
| **TOTAL** | **₹4,981** |
| **Cap** | **₹5,000** |
| **Buffer** | **₹19** |

### Daily budget scrape design

**Source:** AWS Cost Explorer API (`get_cost_and_usage`)

**Cadence:** Once daily at 09:00 IST (after AWS instance boot)

**Telegram daily digest:**
```
💰 AWS Daily Budget — 2026-05-12

Month-to-date spend: ₹2,847 / ₹5,000 (57%)
Forecasted month-end: ₹4,890 / ₹5,000 (98%)

Breakdown:
  EC2:        ₹2,100 (66 hours running this month)
  EBS gp3:    ₹387
  EIP:        ₹76
  S3:         ₹178
  SNS + Data: ₹56
  CloudWatch: ₹0 (free tier)

Anomalies: NONE detected by AWS Cost Anomaly Detection

Status: ✅ ON TRACK
```

### Alert thresholds

| Threshold | Severity | Action |
|---|---|---|
| MTD > 70% of cap | Low (INFO Telegram) | Daily nudge |
| MTD > 90% of cap | High | Operator review needed |
| MTD > 100% of cap | Critical | HALT non-essential spend; pause AWS until next month |
| Forecast > 100% | Medium | Plan to reduce non-essential usage |
| Anomaly detected (Cost Anomaly Detection) | High | Investigate immediately |
| Single-day spend > 3x average | High | Anomalous usage; investigate |

### NEW components

| Component | Where |
|---|---|
| `crates/app/src/aws_budget_monitor.rs` | NEW file — daily Cost Explorer scrape |
| `aws_budget_audit` QuestDB table | NEW table — daily spend tracking |
| `tv_aws_monthly_spend_inr` gauge | NEW Prometheus metric |
| `tv-aws-budget-near-cap` alert | NEW alert rule |
| Telegram `NotificationEvent::AwsBudgetDigest` | NEW variant |

### Z+ 7-Layer for AWS budget

| Layer | Mechanism |
|---|---|
| L1 DETECT | Daily Cost Explorer scrape; 6 metrics |
| L2 VERIFY | Cross-check Cost Explorer vs Billing Console (weekly) |
| L3 RECONCILE | End-of-month: actual vs forecast; alert if diff > 10% |
| L4 PREVENT | Schedule reduction recommendations BEFORE cap hit |
| L5 AUDIT | `aws_budget_audit` table — every daily scrape logged |
| L6 RECOVER | If overspend: auto-stop instance for the day (operator override available) |
| L7 COOLDOWN | Between Cost Explorer API calls: 5 min (rate-limit safety) |

---

## 📋 Section 2 — AWS Instance Lifecycle Monitoring

### EventBridge schedule (`aws-budget.md` rule 7)

| Time | Event | Source |
|---|---|---|
| 08:30 IST Mon-Fri | Instance auto-start | EventBridge cron |
| 17:30 IST Mon-Fri | Instance auto-stop | EventBridge cron |
| 08:30 IST Sat-Sun | Instance auto-start | EventBridge cron |
| 13:30 IST Sat-Sun | Instance auto-stop | EventBridge cron |

### Lifecycle events to track + alert

| Event | Source | Severity | Telegram |
|---|---|---|---|
| Instance start success | CloudWatch EC2 event | Info | "🚀 AWS instance started — 08:30:14 IST" |
| Instance stop success | CloudWatch EC2 event | Info | "🛑 AWS instance stopped — 17:30:08 IST" |
| Start MISSED (still stopped at 08:35) | Lambda checker | Critical | "🆘 AWS instance NOT STARTED by 08:35 — manual intervention" |
| Stop FAILED | CloudWatch alarm | High | "⚠️ AWS instance stop failed" |
| Manual start | CloudTrail event | Info | "👤 Operator manually started AWS" |
| Manual stop | CloudTrail event | Info | "👤 Operator manually stopped AWS" |
| EIP disassociation | CloudTrail event | Critical | "🆘 EIP DISASSOCIATED — Dhan orders will fail" |
| Instance terminated | CloudTrail event | Critical | "🆘 INSTANCE TERMINATED — restart required" |
| Auto-scaling event (not used today, but defensive) | CloudWatch | High | Alert + halt |

### NEW Lambda function: `tickvault-instance-watchdog`

Cron: every 5 min during business hours (08:30-17:30 IST)

Logic:
1. Check if instance is in `running` state
2. If between 09:00-15:30 IST AND state != running → CRITICAL Telegram
3. If between 17:30-08:30 IST AND state == running → INFO Telegram ("idle running, manual override?")

Cost: ~₹0.10/mo (well under free tier of 1M invocations).

---

## 📋 Section 3 — Mac↔AWS Parity (THE BIG ONE)

### What must be IDENTICAL between Mac dev and AWS prod

| Concern | Mac path | AWS path | Parity check |
|---|---|---|---|
| Docker compose | `deploy/docker/docker-compose.yml` | same git-tracked file | `sha256sum` match |
| Config TOML | `config/base.toml` + `config/{env}.toml` | same | sha256 match |
| Indicators TOML | `config/indicators.toml` | same | sha256 match |
| Strategies TOML | `config/strategies.toml` | same | sha256 match |
| Docker image versions | digest-pinned in compose | same | digest match |
| Rust binary | built from `git rev-parse HEAD` | same git commit | commit hash match |
| QuestDB schema | DDL run at boot (idempotent) | same | schema dump diff |
| Grafana dashboards | `deploy/docker/grafana/dashboards/*.json` | same | sha256 match |
| Prometheus alerts | `deploy/docker/grafana/provisioning/alerting/alerts.yml` | same | sha256 match |
| SSM secrets | `/tickvault/dev/...` | `/tickvault/prod/...` (env-scoped) | path naming convention |
| Telegram bot token | same SSM path, different env scope | same | env-scoped |
| Core pinning | 4 Tokio threads → cores 0-3 | same code | `tv_core_pinning_workers_pinned_total == 4` |
| Memory budget | 2 GB app cap (per aws-budget) | same | `tv_app_rss_bytes < 2_000_000_000` |
| Telegram bot channel | dev channel | prod channel | separate SSM `chat_id` |

### `make parity-check` command (NEW)

Operator runs on either machine. Compares Mac state vs AWS state.

```
$ make parity-check

🔍 Mac↔AWS Parity Check — 2026-05-12 14:10 IST

[1/10] Docker compose SHA256:        ✅ match
[2/10] Config base.toml:             ✅ match
[3/10] Config indicators.toml:        ✅ match
[4/10] Config strategies.toml:        ⚠️  Mac has strategy 'bullish_breakout' enabled, AWS does not
[5/10] Docker image digests:          ✅ match (5 services)
[6/10] Rust binary git commit:        ✅ both at e63a688
[7/10] QuestDB schema:                ✅ match (37 tables, 9 mat views)
[8/10] Grafana dashboards:            ✅ match (8 JSON files)
[9/10] Prometheus alerts.yml:         ✅ match (24 rules)
[10/10] Core pinning status:          ✅ Mac=4/4, AWS=4/4

⚠️  1 drift detected — see [4/10]
Recommendation: bring AWS in sync OR disable on Mac before pushing.
```

### NEW parity audit table

```sql
CREATE TABLE IF NOT EXISTS parity_check_audit (
  ts                  TIMESTAMP,
  check_name          SYMBOL,
  mac_value           STRING,
  aws_value           STRING,
  match               BOOLEAN,
  drift_details       STRING
) TIMESTAMP(ts) PARTITION BY DAY
  DEDUP UPSERT KEYS(ts, check_name);
```

Daily run @ 23:00 IST → write all 10 check results → if any drift, Telegram WARN.

---

## 📋 Section 4 — Core Pinning Parity (Wave 5 CORE-PIN-01)

### Per `wave-5-error-codes.md`:
- 4 Tokio worker threads pinned to cores 0-3
- WS read loop, parser, ILP writer, "other"
- Drift watchdog re-pins on detection
- `core_affinity` crate handles cross-platform

### Mac dev

| Hardware | Pinning method | Notes |
|---|---|---|
| M1/M2 MacBook (4 P-cores) | `core_affinity::set_for_current(core_id)` | macOS thread_policy_set under the hood |
| M3 Max (12 P-cores) | Same — pin to first 4 P-cores | Mac dev mirror still works |

### AWS prod

| Hardware | Pinning method | Notes |
|---|---|---|
| c8g.xlarge (4 vCPUs Graviton ARM) | Same `core_affinity` crate | Linux `sched_setaffinity` under the hood |
| c7i.xlarge (4 vCPUs Intel) | Same crate | x86_64 same |

### Verification

| Check | Source |
|---|---|
| `tv_core_pinning_workers_pinned_total` gauge | Prometheus — must equal 4 |
| `CORE-PIN-01` ErrorCode | Fires at boot if any worker fails to pin |
| `CORE-PIN-02` ErrorCode | Fires when drift watchdog detects a worker moved off its pinned core |
| `make parity-check` step 10 | Compares Mac vs AWS pinning status |

### What COULD differ

| Factor | Mac | AWS | Mitigation |
|---|---|---|---|
| P-cores vs E-cores (Mac only) | Pin to P-cores (cores 0-3 on M1/M2 default) | All 4 vCPUs are uniform | Same code path; cores 0-3 work both |
| CPU governor (frequency scaling) | macOS handles automatically | Linux: ensure `performance` mode for hot path | Document in deploy/aws/setup.sh |
| Hyperthreading on x86 | N/A | c7i has SMT enabled | Pin to physical cores; verify via `lscpu` |
| Big.LITTLE on Graviton | N/A | c8g is uniform | Same code |

**Same code works on both.** No Mac-specific or AWS-specific code path.

---

## 📋 Section 5 — Comprehensive automation chain (all 5 W's)

| Concern | Mac dev | AWS prod | Automation |
|---|---|---|---|
| **TRACK** AWS budget | N/A | Daily Cost Explorer scrape | `aws-budget-monitor.rs` |
| **MONITOR** instance lifecycle | N/A | EventBridge + Lambda watchdog | Lambda function |
| **LOG** instance events | N/A | CloudTrail → CloudWatch Logs | Native AWS |
| **AUDIT** spending | N/A | `aws_budget_audit` table | QuestDB persistence |
| **CAPTURE** anomalies | N/A | AWS Cost Anomaly Detection | Native AWS (free) |
| **NOTIFY** | Local Telegram (dev channel) | Telegram (prod channel) | NotificationService |
| **TRACK** parity | Manual `make parity-check` | Daily auto-run 23:00 IST | `parity-check.sh` |
| **MONITOR** RAM | Both: `tv_app_rss_bytes` | Same | Prometheus |
| **LOG** core pinning drift | Both: `tv_core_pinning_drift_total` | Same | Tracing → JSONL |
| **AUDIT** indicator/strategy enabling | Both: `indicator_lifecycle_audit` table | Same | QuestDB |
| **CAPTURE** config changes | Both: git commit log | Same | Git |
| **NOTIFY** config drift | Both: parity-check daily | Same | Telegram |

**Bottom line:** every monitoring/logging/auditing/capturing mechanism runs ON BOTH machines. Same code, same outputs, just different SSM paths and Telegram channels.

---

## 📋 Section 6 — Common Runtime Dynamic Scalable verification

The 4-word test (per `operator-charter-forever.md` §A):

| Word | Mac dev | AWS prod | Verification |
|---|---|---|---|
| **Common** | `docker compose up -d` | `docker compose up -d` | Same yml, same commands |
| **Runtime** | Hot-reload via `notify` crate | Same | Verify `tv_config_reloads_total` increments |
| **Dynamic** | Config TOML changes apply live | Same | Same |
| **Scalable** | 11K instruments today | Same | Bounded resources |
| **Incremental** | One PR at a time | Same | Wave A/B/C/D rollout |

---

## 📋 Section 7 — The deployment workflow (Mac → AWS)

```
1. Developer (operator) on MacBook
   ↓ git commit
   ↓ git push origin <branch>
   ↓ PR opened
   ↓ CI runs full battery (22 test categories)
   ↓ PR merged to main

2. AWS deployment (manual or auto via GitHub Actions)
   ↓ git pull on AWS instance
   ↓ docker compose up -d (idempotent)
   ↓ binary rebuild from same commit
   ↓ make parity-check → confirm Mac == AWS

3. Verification
   ↓ Telegram digest: "AWS at commit <hash>; Mac at commit <hash>; match: ✅"
```

### Critical invariant

**Mac dev and AWS prod MUST be on the SAME git commit at all times during trading hours.**

If divergence detected → Telegram CRITICAL → operator reconciles.

Mechanical enforcement: `make parity-check` step [6/10] commit hash compare.

---

## 📋 Section 8 — What still differs (and is OK to differ)

Some things SHOULD differ between Mac and AWS:

| Thing | Mac | AWS | Why OK |
|---|---|---|---|
| SSM path prefix | `/tickvault/dev/...` | `/tickvault/prod/...` | Env separation; same KEYS |
| Telegram chat ID | dev channel | prod channel | Don't alert dev tests to operator's main phone |
| Static IP | optional (Mac NAT) | mandatory (Dhan allowlist) | Mac for dev; AWS for live orders |
| Trading mode | dry_run = true (recommended) | dry_run = false (live) | Mac doesn't place real orders |
| Bhavcopy download | best effort | mandatory | Mac can skip if network flaky |
| AWS budget monitor | DISABLED | ENABLED | No AWS cost on Mac |
| EventBridge schedule | N/A | enabled | Mac doesn't auto-start/stop |
| EBS snapshot | N/A | nightly | Mac uses Time Machine instead |

**These are ENVIRONMENT differences, not CODE differences.** Same code, different config values.

---

## 🛡️ Z+ 7-Layer for Mac↔AWS parity

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_parity_drift_count` gauge per check_name |
| L2 VERIFY | `make parity-check` runs all 10 comparisons |
| L3 RECONCILE | Daily 23:00 IST automated parity-check + audit |
| L4 PREVENT | Pre-deploy hook on AWS: refuse `docker compose up` if config sha256 differs from main branch |
| L5 AUDIT | `parity_check_audit` table — every check logged with diff |
| L6 RECOVER | On drift: operator pull latest from main + redeploy |
| L7 COOLDOWN | Between parity checks: 1 hour minimum |

---

## 🚨 Worst-case scenarios for Mac↔AWS parity (W171-W180, 10 NEW)

| # | Scenario | Defense |
|---|---|---|
| W171 | Operator edits config on AWS via SSH (out-of-band) | Parity check detects; operator must commit to git or revert |
| W172 | Mac and AWS on different branches | Commit hash mismatch in parity check |
| W173 | Docker image updated on AWS but not Mac (manual `docker pull latest` violation) | Digest mismatch; parity check flags |
| W174 | SSM secret value changed (e.g., token renewed) but only on prod | Both should read from same SSM env path; if scoped correctly, no issue |
| W175 | Mac has stale cached binary from old commit | Parity check detects commit hash mismatch |
| W176 | AWS instance has more cores than Mac (E-cores mismatch) | Core pinning uses first 4 cores; works on both |
| W177 | Mac uses macOS-specific Docker volume driver, AWS uses Linux | Compose file is platform-agnostic |
| W178 | Mac has Wi-Fi flaky during test runs | Tests use timeouts + retries; not a parity issue |
| W179 | AWS time zone set to UTC, Mac to Asia/Kolkata | All code uses IST epoch math; no tz dependency |
| W180 | Mac jemalloc version differs from AWS | Cargo.lock pins exact version; Docker uses pinned image |

---

## 📊 Friday LoC estimate for parity + AWS budget

| Component | LoC |
|---|---|
| AWS budget monitor (Cost Explorer scrape) | 250 |
| Lambda instance watchdog | 150 (Python) |
| `aws_budget_audit` + `parity_check_audit` tables | 80 |
| `make parity-check` script | 200 |
| Daily parity scheduler | 80 |
| Telegram NotificationEvent::AwsBudgetDigest + ParityDrift | 100 |
| Prometheus metrics (8 new) | 80 |
| Grafana panels (5 new — budget + parity) | 150 (JSON) |
| Alert rules (4 new) | 50 |
| Ratchet tests (10 cases) | 300 |
| **TOTAL** | **~1,440 LoC** |

---

## 🚗 Auto-Driver Story (extended)

> Sir, the parity check is like having TWO IDENTICAL DOCTORS in two cities. Both treat the same patient with the same medicine, same dose, same schedule. If one doctor accidentally gives a different pill, we know IMMEDIATELY because they share a daily checklist.
>
> AWS doctor (prod) tracks 6 things:
> 1. ₹ spent this month (Cost Explorer)
> 2. Instance start/stop times (EventBridge)
> 3. EIP attached (CloudTrail)
> 4. Backup snapshots (EBS)
> 5. Anomaly detection (Cost Anomaly Detection)
> 6. Manual interventions (CloudTrail)
>
> Mac doctor (dev) tracks the SAME 6 (minus AWS-specific) plus:
> 7. Pre-deploy parity check before pushing to AWS
>
> Daily 23:00 IST, both doctors compare notes. 10 items. If any disagree → Telegram alert "configs drift on item X". Operator reconciles.
>
> Cost: ₹0.10/month for Lambda + AWS Cost Anomaly Detection (FREE) + same code on both = ZERO extra infrastructure cost.

---

## 🎯 Discussion items

### D1 — Should AWS budget Telegram be daily OR weekly?

Daily = predictable but more messages. Weekly = compact but you miss daily spikes.

**My vote:** Daily during the first 3 months post-deploy (to catch drift early), then switch to weekly.

### D2 — Auto-stop instance on overspend?

If MTD spend > 100% of cap → auto-stop instance for the day?

**Pro:** prevents runaway spend
**Con:** ZERO trading possible if false-positive

**My vote:** Telegram CRITICAL only. Operator manually decides to stop. No auto-stop.

### D3 — Should Mac dev also run AWS budget monitor (as a check)?

Mac dev would query AWS Cost Explorer if AWS credentials available.

**Pro:** Mac developer sees the spend without logging into AWS console
**Con:** Adds AWS dependency to local dev

**My vote:** OPTIONAL — enable via `[features] aws_budget_monitor = true` in dev TOML.

### D4 — Daily parity check timing

23:00 IST after market close + post-market fetch complete.

**Alternative:** at boot (08:30 IST) — catches drift before trading starts.

**My vote:** BOTH. 08:30 boot check + 23:00 daily reconcile.

### D5 — What if parity check finds drift DURING market hours?

Treat as Critical and halt? OR Treat as Medium and continue?

**My vote:** Medium — continue trading; flag for operator review post-market. (Config drift mid-day shouldn't affect already-running session.)

---

## 🎤 Final summary

| Concern | Status |
|---|---|
| AWS budget monitor | ✅ designed (₹5K cap, daily scrape, 6 thresholds) |
| Instance lifecycle monitor | ✅ designed (Lambda watchdog + CloudTrail events) |
| Mac↔AWS parity (10 checks) | ✅ designed (`make parity-check`) |
| Core pinning parity | ✅ same code, same Wave 5 CORE-PIN ErrorCodes |
| Comprehensive automation chain | ✅ all 5 W's on both machines |
| Common runtime dynamic scalable verification | ✅ 4-word test passes |
| Worst cases (W171-W180) | ✅ 10 NEW defended |
| Friday LoC estimate | ~1,440 LoC |
| Z+ 7-layer applied | ✅ Yes |

**Operator confirmed: everything that runs on AWS MUST run on Mac with same configs/functions/core-pinning. Daily verification. Telegram drift alerts.**

Discussion mode continues. NO IMPLEMENTATION.
EOF
echo "AWS budget + Mac parity plan locked"