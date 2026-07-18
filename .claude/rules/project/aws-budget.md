# AWS Budget Enforcement — t4g.medium LOCKED ~₹1,022/mo

> **⚠ SUPERSEDED 2026-05-27 by [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md):** instance upgraded t4g.medium → t4g.large (8 GiB), bill ~₹1,022/mo → ~₹1,514/mo, cron 08:00 → 08:30 IST. Contents below retained as 2026-05-18 historical audit; current effective contract lives in the superseding file.
>
> **⚠ FURTHER SUPERSEDED → r8g.large 2026-06-30 (operator Quote 7 in [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md) §7):** instance upgraded m8g.large → **r8g.large** (Graviton4, 2 vCPU / 16 GiB), bill → ~₹2,919/mo incl GST (270 hrs, 30 GB EBS, +EIP kept). The current effective instance lock lives in that file's §7.
>
> **⚠ RE-SUPERSEDED → t4g.medium 2026-07-15 (operator Quote 8 in [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md) §7):** instance DOWNSIZED r8g.large → **t4g.medium** (Graviton2, 2 vCPU / 4 GiB), QuestDB QDB_MEM_LIMIT 4g → 1g. INTERIM bill → ~₹1,471/mo incl GST at 270 hrs with the live 50 GB root (gp3 cannot shrink; the 20 GB fresh-volume recreate — an executor pre-stage, NOT operator-quoted — drops it to ~₹1,197/mo; ~₹986/mo requires BOTH the ~176-hr auto-schedule basis AND the post-recreate 20 GB volume — on the live 50 GB root the ~176-hr figure is ~₹1,260, and ~₹986 is never the 270-hr one). EIP kept. The current effective instance lock lives in that file's §7. This file's original t4g.medium tables below remain 2026-05-18 historical audit (different universe/stack — do not reuse the ₹1,022 figure).
>
> **Authority:** Parthiban (architect). Non-negotiable.
> **Ground truth:** `docs/architecture/aws-indices-only-locked-architecture.md` §5 (instance lock 2026-05-18) and the 2026-05-20 CloudWatch-only decision below.
> **Scope:** Any file touching AWS deployment, infrastructure, Docker config, or cost-impacting changes.

## COST NOTE 2026-07-17 — dashboard tidy (−~$0.70/mo + 1 free-tier dashboard slot)

The dashboard-tidy PR (cleanup wave, Track B) retired the dead Dhan-lag
observability chain and the scoreboard dashboard (Verified against the
terraform diff in this PR; billing magnitudes Assumed at CloudWatch list
rates — active-series-hours were already $0-decaying since the producers
died with the live-WS retirements):

- **−1 alarm ≈ −$0.10/mo (Verified):** dhan-exchange-lag-p99-high
  (silent-feed-alarms.tf S3) — its only publisher
  (`run_dhan_lag_publisher`, feed_lag_monitor.rs) lost its spawn site +
  tick source with the Dhan live-WS lane deletion (PR-C2, 2026-07-13) and
  is deleted in this PR; a permanently-missing-data dead monitor (the
  groww-exchange-lag S4 precedent, 2026-07-15). Window-gate ALARM_NAMES
  trimmed 4 → 3 in lockstep.
- **−2 EMF allowlist series ≈ −$0.60/mo (names Verified; billing
  Assumed):** tv_dhan_exchange_lag_p99_seconds +
  tv_dhan_lag_samples_excluded_total (cloudwatch-agent.json +
  user-data.sh.tftpl, 19 → 17 names).
- **−1 CloudWatch dashboard: ₹0 (Verified):** `tv-<env>-scoreboard`
  (dashboard.tf) — its Dhan-vs-Groww lag-trend widgets charted only the
  dead lag gauges; frees dashboard slot #2 of the 3-slot free tier.
- Dead-widget trim on the KEPT `tv-<env>-operator` dashboard (₹0):
  WebSocket-health / spill-dropped / DLQ-ticks widgets removed — their
  metrics have ZERO producers post live-WS retirements; their app-alarms.tf
  alarms are deliberately NOT touched here (flagged follow-up, dated notes
  in dashboard.tf).

Net ≈ **−$0.70/mo pre-GST (~−₹70/mo incl. 18% GST at ₹85/$)** — the real
gain is the freed dashboard slot + ~730 LoC of dead monitoring code.

## COST NOTE 2026-07-17 — dead live-WS sweep stage 1 (−~$0.10/mo)

The stage-1 zero-wiring dead-module sweep (operator directive 2026-07-17
via coordinator) removed the `ws-reinject-01` errcode log-filter alarm
(−1 alarm ≈ −$0.10/mo, Verified against the terraform diff in this PR):
its ONLY emit site (`crates/app/src/wal_reinject.rs`, retained un-consumed
since PR-C2 "pending the Phase C module cleanup") was deleted in that
cleanup, so the filter could never match again (the ws-gap-07 /
feed-stall-01 dead-filter precedent). Dated notes in
`error-code-alarms.tf` + `observability-architecture.md`. No other
alarm/metric/dashboard change in this sweep.

## COST NOTE 2026-07-15 — Groww live-feed retirement (Trap-A lockstep; net reduction)

The Groww live feed (sidecar + bridge + stall watchdog + lag publisher) is
retired (operator 2026-07-15: "remove the whole Groww live feed; keep only
spot 1m and option chain for both brokers; go"). Alarm/metric deltas
(Verified against the terraform diff in this PR; billing magnitudes Assumed
at CloudWatch list rates — active-series-hours decay to $0 once producers
stop publishing):

- **−3 alarms ≈ −$0.30/mo (Verified):** groww-ws-inactive +
  groww-stall-restart-storm (app-alarms.tf) + groww-exchange-lag-p99-high
  (silent-feed-alarms.tf S4).
- **−1 alarm ≈ −$0.10/mo (Verified, same-PR fix round):**
  aggregator-no-seals (app-alarms.tf section 9) — its metric lost its last
  live producer with the bridge deletion (Dhan broadcast publisher-less
  since PR-C2); a permanently-dead monitor the window gate kept arming
  (window-gate ALARM_NAMES trimmed 5 → 4 in the same edit).
- **−1 alarm + its fallback log metric filter ≈ −$0.10/mo + one sparse
  derived series (Verified):** the tv-<env>-feed-stall-restarts counter
  pager (feed-stall-restart-alarm.tf deleted whole).
- **−1 errcode alarm ≈ −$0.10/mo (Verified):** the "feed-stall-01"
  error_code_alerts entry (its ERROR-level emit site died with the stall
  watchdog; observability-architecture.md paging list updated in lockstep).
- **−4 EMF allowlist names ≈ −$1.20/mo at full in-session density (names
  Verified; billing Assumed):** tv_groww_ws_active,
  tv_feed_last_tick_age_seconds, tv_feed_sidecar_stall_restart_total,
  tv_groww_exchange_lag_p99_seconds.
- **+1 EMF name ≈ +$0.30/mo (Assumed):** tv_rest_1m_fire_heartbeat — the
  per-fire liveness gauge replacing the lag gauge **1:1 under the EXISTING
  tv-<env>-market-hours-liveness-missing alarm** (metric_name-only swap;
  0 new alarms; treat_missing_data="breaching" + the 09:20–15:35 IST window
  gate unchanged).

Net ≈ **−$0.50/mo alarms/filters + ≈ −$0.90/mo series (Assumed)** — inside
the $35/mo pre-GST budget alarm ceiling; the real saving is the ~30K-LoC
delete, not dollars. Honest residual (design Assumed sound, not
live-simulated): the heartbeat is deliberately NOT pre-registered at boot —
the first set at the 09:16:01 IST fire is the session-start signal; a day
where BOTH per-minute REST spot legs are disabled/dead pages the liveness
alarm ~09:25 IST — the designed loud outcome (zero in-session capture), not
a false page.

## COST NOTE 2026-07-14 — REST-audit alarm gaps (GAP-01/03/05, +~$0.60/mo)

The 2026-07-14 REST-pipeline adversarial audit
(`docs/audits/2026-07-14-rest-pipeline-adversarial-audit.md`) found the
REST-leg paging chain was app-emitted Telegram ONLY (GAP-01/GAP-03) with no
alarm on Telegram drops themselves (GAP-05). Added:

- **+5 errcode log-filter alarms ≈ $0.50/mo** (`error-code-alarms.tf`):
  `auth-gap-05-remint-failed` (mint-FAILURE arm only — `$.cooldown_skip
  IS FALSE` scoped; excludes the noise-lock H3 non-terminal
  cooldown-skip lines), `spot1m-01-escalation` + `chain-02-escalation`
  (`stage="escalation"` once-per-episode edges only), `chain-01`,
  `chain-04-warmup`. Their log-derived metrics are sparse/dimensionless
  (billed only in hours a code fires — near-free).
- **+1 counter-delta alarm ≈ $0.10/mo** (`telegram-drop-alarm.tf`):
  `tv-<env>-telegram-drops` on `tv_telegram_dropped_total` (Sum ≥ 3 per
  900s, metrics-log delta-extraction house pattern). The derived metric is
  sparse until the flagged crates-side pre-registration lands (near-free).

Total **≈ $0.60/mo pre-GST (~₹60/mo incl. 18% GST at ₹85/$)** — inside the
$35/mo pre-GST budget alarm ceiling and the ~₹3,101/mo envelope.

## COST NOTE 2026-07-06 — Silent-feed alerting hardening (+~$1.50/mo)

The 2026-07-06 incident (Dhan feed degraded ALL day — lag p99 46s/max 199s,
29-67 of 776 instruments silent every minute, 125 SLO crossings in the
0.94-0.95 band, 9k-11.5k BOUNDARY-01 catch-up seals/10min — with ZERO pages)
added, per `deploy/aws/terraform/silent-feed-alarms.tf` +
`deploy/aws/terraform/app-alarms.tf` (tick-gap retune 100 → 40 PROVISIONAL —
round-3 correction 2026-07-08: 25 sat below the documented ~33 always-silent
healthy floor and would have paged every healthy day):

- **+4 custom-metric series ≈ $1.20/mo:** 2× `tv_boundary_catchup_total`
  under `[host, feed]` (dhan + groww, second EMF declaration), 1×
  `tv_dhan_exchange_lag_p99_seconds`, 1× `tv_dhan_lag_samples_excluded_total`.
- **+3 alarms ≈ $0.30/mo:** realtime-guarantee-degraded (0.80-0.95 dead-band),
  boundary-catchup-storm-dhan (PROVISIONAL 2000/5m ×2 — re-ratchet after one
  observed trading week), dhan-exchange-lag-p99-high (>10s ×10min). All
  market-hours-gated (09:20-15:35 IST window Lambda), all
  `treat_missing_data = notBreaching`.

Total **≈ $1.50/mo pre-GST (~₹150/mo incl. 18% GST at ₹85/$)** — inside the
$35/mo pre-GST budget alarm ceiling and the ~₹2,919/mo envelope.

## COST NOTE 2026-07-11 — Groww exchange-lag visibility (scoreboard PR-C, +~$0.40/mo)

The dual-feed scoreboard PR-C added, per `deploy/aws/terraform/silent-feed-alarms.tf` S4:

- **+1 custom-metric series ≈ $0.30/mo:** `tv_groww_exchange_lag_p99_seconds`
  (the Groww mirror of the Dhan lag gauge — its OWN EMF name, the 27th
  allowlist entry; the Groww exclusion/clamp counters stay /metrics-only, ₹0).
- **+1 alarm ≈ $0.10/mo:** groww-exchange-lag-p99-high (>5s ×10min,
  window-gated 09:20-15:35 IST like the Dhan one; the window-gate Lambda now
  arms 12 alarms).
- **+1 CloudWatch dashboard: ₹0** (slot 2 of the 3 free dashboards —
  `tv-<env>-scoreboard`, Dhan-vs-Groww lag trends).

Total **≈ $0.40/mo pre-GST (~₹40/mo incl. 18% GST at ₹85/$)** — inside the
$35/mo pre-GST budget alarm ceiling and the ~₹2,919/mo envelope.

## COST NOTE 2026-07-14 — PR-C3 tick-gap retirement (−~$0.40/mo)

PR-C3 (tick-gap detector deletion, operator Q4-ii 2026-07-13) removed the
`tv-<env>-tick-gap-instruments-silent` alarm (−1 alarm ≈ −$0.10/mo) and the
`tv_tick_gap_instruments_silent` custom-metric series from the EMF allowlist
(−1 series ≈ −$0.30/mo) — the gauge producer was deleted with the Dhan WS
lane, so both would have been dead monitors. Dated notes in
`deploy/aws/terraform/app-alarms.tf` + `market-hours-liveness-alarm.tf`.

## COST NOTE 2026-07-13 — EBS 30→50 GB (+~₹170/mo incl GST)

Prod disk-pressure remediation (operator pre-approved 2026-07-13): the root fs
hit **82%** on 2026-07-13, growing ~2.5–3.6 GB/trading-day with ZERO
reclamation (partition manager `detached=0` every run; S3 archive leg never
fired) — the 50 GB gp3 grow is the pressure-relief backstop alongside the
code retention fixes. EBS line $2.74 → $4.56 ($0.0912 × 50), bill ~₹2,919 →
**~₹3,101/mo incl GST** — still under the $35/mo pre-GST budget alarm. The
box's S3 write for Groww-capture archival needed NO IAM change (the instance
role already has Put/Get/List on the whole `tv-<env>-cold` bucket). The
effective contract lives in `daily-universe-scope-expansion-2026-05-27.md` §7
(Mechanical Rule 3); the live grow is `scripts/aws-upgrade-instance.sh
--ebs-size 50` (online) — terraform's `ebs_gp3_size_gb=50` documents
fresh-provision intent only (`volume_size` is in `lifecycle.ignore_changes`).

## COST NOTE 2026-07-14 — Order-side observability, cluster C (+~$0.60/mo now, ~$1.20/mo ceiling at Phase-1)

Order-side audit tables + alert-sink wiring + alarms (order_audit/pnl_audit rebuild, OMS→Telegram
bridge, orders-placed storm pager, arm-on-arrival fill-lag/daily-loss alarms), per
`deploy/aws/terraform/order-side-alarms.tf`:

- **+1 custom-metric series ≈ $0.30/mo:** `tv_orders_placed_delta_total` (derived, metrics-log
  filter on `/tickvault/<env>/metrics` — dense from the main.rs pre-registrations; the log filter
  itself is free). NO new EMF-published series bill today: the 2 new allowlist names
  (`tv_daily_pnl`, `tv_order_fill_lag_seconds`) are DORMANT — their emit sites ship with
  cluster A / Phase-1, so zero datapoints = $0.00 until then, then ≈ +$0.60/mo (noted here in
  advance so that PR needs no new cost note for them).
- **+3 alarms ≈ $0.30/mo:** orders-placed-storm (armed), daily-loss-breach (armed, structurally
  silent in dry-run — missing gauge + notBreaching), order-fill-lag-high (actions_enabled = false
  until Phase-1 arming). The pre-existing orders-rejected alarm is fixed at $0 (ok_actions
  removed + counter pre-registered — it was dead for single-rejection sessions).
- **Dashboard: ₹0** — one widget row appended to the EXISTING `tv-<env>-operator` dashboard;
  free-tier dashboard slot 3 deliberately NOT consumed.
- **Log-ingestion delta:** 3 newly-dense counter series ≈ a few hundred bytes/min into the
  metrics log group — noise inside the 5 GB free tier.

Total **≈ $0.60/mo pre-GST now (~₹51/mo incl. 18% GST at ₹85/$), ≈ $1.20/mo at Phase-1** —
inside the $35/mo pre-GST budget alarm ceiling and the ~₹3,101/mo envelope.

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
