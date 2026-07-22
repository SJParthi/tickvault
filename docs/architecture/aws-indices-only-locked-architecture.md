# AWS Indices-Only Architecture — LOCKED 2026-05-18

> **⚠ §5 SUPERSEDED 2026-05-27 by [`.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`](../../.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md):** instance upgraded t4g.medium → t4g.large (8 GiB), bill ~₹1,022/mo → ~₹1,514/mo, cron 08:00 → 08:30 IST, universe 4 IDX_I → ~250 daily-fetched SIDs (all NSE indices + 1 BSE SENSEX + unique F&O underlyings, all Quote mode). Other sections retain 2026-05-18 lock.
>
> **⚠ §5 FURTHER SUPERSEDED → r8g.large 16 GiB 2026-06-30 (operator Quote 7 in [`.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`](../../.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md) §7):** instance upgraded m8g.large → **r8g.large** (Graviton4, 2 vCPU / **16 GiB**, memory-optimized — DOUBLED RAM), bill → ~₹2,919/mo incl GST (270 hrs, 30 GB EBS, +EIP kept; r8g.large @ $0.08258/hr). Host memory budget recomputed for 16 GiB (~8.2 GB used, ~7.8 GB headroom at the current ~250–1000-SID universe). EIP kept because a stop/modify/start instance-type flip leaves no ephemeral public IP. The current effective instance lock lives in that file's §7; the t4g.medium spec table below is 2026-05-18 historical audit only.
>
> **⚠ §5 RE-SUPERSEDED → t4g.medium 2026-07-15 (operator Quote 8 in [`.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`](../../.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md) §7):** instance DOWNSIZED r8g.large → **t4g.medium** (Graviton2, 2 vCPU / 4 GiB), QuestDB QDB_MEM_LIMIT 4g → 1g. INTERIM bill → ~₹1,471/mo incl GST at 270 hrs with the live 50 GB root (gp3 cannot shrink; drops to ~₹1,197/mo only after the 20 GB fresh-volume recreate — an executor pre-stage, not operator-quoted; ~₹986/mo requires BOTH the ~176-hr auto-schedule basis AND the post-recreate 20 GB volume, the live-50-GB ~176-hr figure being ~₹1,260). EIP kept. The current effective instance lock lives in that file's §7; the t4g.medium spec table below remains 2026-05-18 historical audit (different universe — its ₹514/₹1,022 figures do not apply to the 2026-07-15 lock).
>
> **§5 RE-RE-SUPERSEDED → m8g.large 2026-07-21 (operator Quote 11):** the instance lock moves t4g.medium → **m8g.large** (2 vCPU / 8 GiB, $0.06416/hr; QDB_MEM_LIMIT 2g; ≈₹2,420/mo @ 270 h / ≈₹1,815 @ ~176 h) — prepared PR, physical flip operator-run post-EAP. Full contract: `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §0 Quote 11 + §7.
>
> **⚠ §5 LIVE-VOLUME CORRECTION 2026-07-19:** the banner above's "live 50 GB root" premise was WRONG — live `describe-volumes` on `vol-073ccaa417a0f344b` (2026-07-19, coordinator session) returned **30 GiB gp3** (3000 IOPS / 125 MiB/s, in-use, attached to `i-0b956d0209231a48b` since 2026-05-24): the 2026-07-13 approved 30→50 grow was recorded but never physically applied. Corrected interim bill: subtotal $12.85 ($6.05 EC2 + $3.60 EIP + $2.74 EBS@30 + $0.18 S3 + $0.28 SMS) → ₹1,092 → ×1.18 GST = **~₹1,289/mo** at 270 hrs (was stated ~₹1,471; ~176-hr figure ~₹1,077, was ~₹1,260; post-recreate ~₹1,197/~₹986 unchanged). FLAGGED FOLLOW-UP: the disk-pressure grow is UNAPPLIED — operator/infra decision pending. Authority: `daily-universe-scope-expansion-2026-05-27.md` §7 2026-07-19 correction note.
>
> **⚠ §5 OPERATOR RULING 2026-07-19 (Quote 9 — same day as the correction above):** verbatim: *"just 30 gn enough and onl yt4g medium as of now espeicall yentirkey it hsodul be kless than 1k per month dude oikay?"* — (a) the **30 GB root is formally ACCEPTED**, the 2026-07-13 30→50 grow **CANCELLED** (resolves the pending decision above); (b) t4g.medium re-affirmed as-of-now; (c) **NEW HARD BUDGET TARGET < ₹1,000/mo incl GST** — the corrected ~₹1,289 (270 hrs) / ~₹1,077 (~176 hrs) bills do NOT meet it; the itemized lever path (no non-operator-gated combination reaches <₹1,000) + the budget kill-ceiling step $55 → $25 (ratchet ladder toward $10) live in `.claude/rules/project/aws-budget.md` "OPERATOR RULING 2026-07-19" + `daily-universe-scope-expansion-2026-05-27.md` §0 Quote 9.
>
> **Status:** LOCKED design (no code shipped). Supersedes `aws-cost-floor-analysis.md` §6 choice — operator picked a path none of the canned options offered. This doc captures it.
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file > `aws-budget.md` (which now needs major revision per these locks).
> **Created:** 2026-05-18.
> **Companion:** `aws-daily-lifecycle.md` (12 worst cases — still applies, with §4-replacement noted below).

---

## §0. Auto-driver one-liner (insta-reel)

> "Sir, the new shop has ONE worker (tickvault app) and ONE storeroom (QuestDB). No fancy CCTV monitor (Grafana out). No fancy alarm box (Prometheus out). No mini-warehouse (Valkey out). Instead, the AWS bank itself watches everything for free — when something goes wrong, AWS rings your phone via 4 different bells: phone log entry + SMS + Telegram + email. The shop only watches 4 prices: Nifty, Bank Nifty, Sensex, India VIX. All calculations live in worker's head (RAM). Only after closing time does worker write notes to storeroom. New monthly bill: under ₹1,000. Done."

```
   OLD                              NEW
   ─────                            ─────
   ┌─────────────┐                  ┌─────────────┐
   │ 8 Docker    │                  │ 2 Docker    │
   │ containers  │                  │ containers  │
   │ - QuestDB   │                  │ - QuestDB   │
   │ - Grafana   │ ───── drop ────► │ - tickvault │
   │ - Prom      │                  └─────────────┘
   │ - Valkey    │                         │
   │ - Loki      │                         ▼
   │ - Alloy     │                  ┌─────────────┐
   │ - Alertmgr  │                  │ CloudWatch  │
   │ - Traefik   │                  │ free tier   │
   └─────────────┘                  │ Logs+Metrics│
        │                           │ +Alarms→SNS │
        ▼                           └──────┬──────┘
   ₹4,981/mo t4g.medium                    │
   ~11K SIDs                       ┌───────┼───────┬───────┐
                                   ▼       ▼       ▼       ▼
                                  Log    SMS    Telegram  Email
                                  ─────────────────────────────
                                  4-channel deadman
                                  
                                  t4g.small 2GB ARM
                                  4 SIDs only
                                  ~₹920/mo ✓ <₹1K
```

---

## §1. Locks captured 2026-05-18 (this session)

| # | Lock | Operator's words → engineering reading |
|---|---|---|
| L1 | Universe = 4 SIDs only | NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21 (all IDX_I, Ticker mode) |
| L2 | Drop ALL stock F&O scaffolding | Delete subscription planner stock branch, Phase 2 scheduler, depth-20/200 pipelines, greeks pipeline, movers pipeline, bhavcopy, prev_OI loader, ~218 NSE_EQ stocks |
| L3 | Drop ALL local observability tools except QuestDB | Out: Grafana, Prometheus, Alertmanager, Valkey, Loki, Alloy, Traefik. In: CloudWatch Logs + Metrics + Alarms |
| L4 | Notification = logging + SMS + call + Telegram | "the 4-channel deadman" via CloudWatch Alarm → SNS fan-out. ("call" interpreted as a fallback if SMS/Telegram fail — see §6) |
| L5 | RAM-first hot path | All timeframes, indicator state, option-chain snapshots in RAM. Periodic async flush to QuestDB. Trading decisions NEVER touch DB. DB is for replay + historical only. |
| L6 | Timeframes initially | **TICKS (raw, persisted) + 1m, 3m, 5m, 15m, 1h, 1d** = 1 raw + 6 derived TFs. Expandable to 1m/2m/.../15m/30m/1h/2h/3h/4h/1d post-backtest. Ticks land in `ticks` table via ring-buffered ILP flush; sealed candles land in `candles_<tf>` tables. |
| L7 | Option chain REST every 50s | 3 underlyings (NIFTY/BANKNIFTY/SENSEX) nearest-expiry only. Concurrent calls OK per Dhan rule (1 unique req per 3s). RAM cache + DB flush. |
| L8 | WebSockets | 1 main-feed (4 SIDs) + 1 order-update. Operator-charter §I unchanged. |
| L9 | AWS instance budget | <₹1K/mo HARD priority. Mumbai network MUST not be compromised. |
| L10 | RAM honesty | Be explicit about what fits where. No hand-waving. |
| L11 | Schedule (UPDATED 2026-05-18) | **08:00 / 17:00 IST EVERY DAY (Mon-Sun, ~30 days/mo)** — operator clarified "every day starting every entire month 8 am to 5 pm". Includes weekends for backtest / BRUTEX / strategy tuning work. Replaces earlier weekday-only lock. |
| L12 | Common runtime / dynamic / scalable / automated | All operator-charter §A demands carried forward |

---

## §2. The new RAM budget (4 SIDs + 2-container stack)

| Component | RAM | Source / math |
|---|---|---|
| QuestDB container | **1024 MB** | Trimmed from 1.5GB (Wave 7-A4 baseline); 4 SIDs × tiny write volume |
| Tickvault app process | **~280 MB** | See breakdown below |
| OS + Linux kernel + filesystem cache | **~300 MB** | Amazon Linux 2023 minimal |
| Docker daemon + containerd overhead | **~80 MB** | Docker 24.x baseline |
| Headroom (kswapd floor) | **~316 MB** | Min 200MB free for kswapd to behave |
| **TOTAL** | **~2,000 MB** | Fits **t4g.small (2GB)** with 0 headroom OR **t4g.medium (4GB)** with 2GB headroom |

### Tickvault app process breakdown (4 SIDs)

| Subcomponent | Math | Bytes |
|---|---|---|
| Sealed bars: 4 SIDs × 13 TFs × 2 days × 375 bars/day max × 64B | | **2.5 MB** |
| Live aggregator cells: 4 SIDs × 13 TFs × 80B | | 4 KB |
| Indicator state (RSI/MACD/BB/EMA/SMA × 13 TFs × 4 SIDs × 500B avg) | | **130 KB** |
| Option chain RAM: 3 underlyings × ~100 strikes × 2 sides × 200B | | **120 KB** |
| Option chain history: last ~720 snapshots (1hr at 50s cadence) × 240KB | | **170 MB** |
| Rescue ring (RESIZED): 100K × 200B (was 5M × 200B = 1GB) | | **20 MB** |
| Instrument registry (4 SIDs) + token cache | | 5 MB |
| WebSocket buffers (2 conns × 4MB) | | 8 MB |
| Tokio runtime + 4 thread stacks | | 20 MB |
| HTTP client pool (reqwest) for option chain REST | | 10 MB |
| Heap fragmentation @ 15% | | ~37 MB |
| **APP TOTAL** | | **~283 MB** |

**Key change vs old design:** rescue ring dropped from 5M→100K. At ~20 ticks/sec peak (4 SIDs × 5 ticks/sec each), 100K = 5000 seconds = **83 minutes of buffer** — way more than needed for any realistic QuestDB outage. Frees ~1GB of RAM.

**Honest envelope claim update:** the rescue-ring constant `TICK_BUFFER_CAPACITY` needs to be re-pinned at 100,000 (`crates/common/src/constants.rs:1568`), and the operator-charter §F honest-100% claim re-anchored at 100K (not 5M). This is a deliberate downward adjustment proportional to the universe size.

---

## §3. The new Docker stack (2 containers)

```
docker-compose.yml (post-deletion)
├── tv-app (the tickvault binary)
│   - port 3001 (HTTP /health)
│   - logs → CloudWatch Logs agent → /tickvault/prod/app log group
│   - metrics → CloudWatch PutMetricData via SDK
└── tv-questdb (1024 MB)
    - port 9000 (HTTP / Web UI)
    - port 9009 (ILP TCP for tick writes)
    - port 8812 (PG-wire for SELECT)
    - 10GB EBS gp3 volume mount
```

**Deleted containers (vs current 8):** Grafana, Prometheus, Alertmanager, Valkey, Loki, Alloy. Saves ~1.9 GB RAM + complexity.

**Effects of dropping Valkey:**
- Token cache moves to **in-memory `Arc<Secret<String>>` + disk SSM fallback** (token already lives in SSM as truth)
- Instrument cache stays on **disk rkyv** (already there for cold start)
- No real loss — Valkey was a nice-to-have layer over SSM

**Effects of dropping Prometheus + Grafana + Loki + Alloy + Alertmanager:**
- All metrics → **CloudWatch PutMetricData** via `aws-sdk-cloudwatch` (already in dependency tree)
- All logs → **CloudWatch Logs agent** on the host (tails `/var/log/tickvault/*.log`)
- All alerts → **CloudWatch Alarms** on those metrics + log filters → SNS fan-out

---

## §4. The new observability surface — CloudWatch is the bus

```
                  ┌──────────────────────────┐
                  │  tickvault app (Rust)    │
                  └───┬──────────────────┬───┘
                      │                  │
              metrics │                  │ logs
                      ▼                  ▼
              ┌─────────────────┐  ┌──────────────────┐
              │ CloudWatch      │  │ CloudWatch Logs  │
              │ Metrics (custom)│  │ /tickvault/prod  │
              │ (10 free max)   │  │ (5GB free/mo)    │
              └────────┬────────┘  └────────┬─────────┘
                       │                    │
                       │            log filter
                       ▼                    ▼
              ┌──────────────────────────────────┐
              │   CloudWatch Alarms (10 free)    │
              └────────────────┬─────────────────┘
                               │ state-change
                               ▼
              ┌──────────────────────────────────┐
              │   SNS topic: tv-prod-alerts      │
              └─┬──────────┬──────────┬──────────┘
                │          │          │
                ▼          ▼          ▼
              SMS       Telegram     Email
            (DLT India  (Lambda      (operator
             ~₹0.24/    →bot.send    inbox)
             msg)      Message)
```

### The 10 CloudWatch metrics that survive

| # | Metric | Purpose | Free tier ok? |
|---|---|---|---|
| 1 | `tv_boot_complete` (gauge 1/0) | Boot succeeded | yes |
| 2 | `tv_websocket_connections_active` (gauge) | 1 main + 1 order-update = 2 expected | yes |
| 3 | `tv_tick_rate_per_sec` (gauge) | Sanity: ~4-20 expected | yes |
| 4 | `tv_questdb_disconnected_seconds` (gauge) | Tick rescue ring filling | yes |
| 5 | `tv_option_chain_fetch_failures_total` (counter) | DH-904 / network failures | yes |
| 6 | `tv_app_alive_heartbeat` (gauge 1, updated 30s) | Mid-day crash detector | yes |
| 7 | `tv_token_remaining_hours` (gauge) | <4h → alert | yes |
| 8 | `tv_orders_placed_total` (counter) | When live trading active | yes |
| 9 | `tv_daily_pnl_inr` (gauge) | EOD digest | yes |
| 10 | `tv_kill_switch_state` (gauge 0/1) | Risk halt visibility | yes |

### The 10 CloudWatch alarms (each routes to SNS)

| # | Alarm | Trigger | Severity |
|---|---|---|---|
| 1 | `tv-boot-not-complete` | metric 1 missing 240s after instance running | Critical |
| 2 | `tv-ws-pool-zero` | metric 2 == 0 for 60s during market hours | Critical |
| 3 | `tv-tick-flow-stalled` | metric 3 == 0 for 60s during market hours | Critical |
| 4 | `tv-questdb-outage` | metric 4 > 30 | High |
| 5 | `tv-option-chain-failing` | metric 5 increase > 3 in 5min | High |
| 6 | `tv-app-heartbeat-missing` | metric 6 missing 60s | Critical |
| 7 | `tv-token-expiring` | metric 7 < 4 | High |
| 8 | `tv-questdb-disk-full` | EBS gp3 > 80% | High |
| 9 | `tv-instance-state-not-running-by-T+5` | Outside expected schedule | Critical |
| 10 | `tv-eventbridge-rule-missing-invocation` | rule fired < 1× in 10min window | Critical |

All within CloudWatch free tier (10 alarms + 10 metrics + 5GB logs).

### Logs

Tickvault writes JSON-structured logs to disk → CloudWatch Logs agent ships to `/tickvault/prod/app` log group. 14-day retention. Log subscription filter on `level=ERROR` → SNS for unknown error escalation.

---

## §5. The instance — t4g.medium LOCKED 2026-05-18 (FINAL, NO COMPARISONS)

**Single spec, no alternatives, no comparison tables:**

| Spec | Value |
|---|---|
| Instance | **t4g.medium** — ARM Graviton, 2 vCPU, 4 GiB RAM |
| Region | ap-south-1 (Mumbai) |
| Tenancy | Default (Shared) — Dedicated would add ₹1,500/mo for zero benefit |
| Pricing | On-demand $0.0224/hr — no Reserved / Savings Plan / Spot for now |
| Schedule | 08:00–17:00 IST every day Mon–Sun |
| EBS | gp3 10 GB |
| EIP | 1 (24/7, Dhan static-IP mandate) |
| Network | ENA enabled by default |

### Cost breakdown — t4g.medium on-demand, every day 08:00–17:00 IST

```
EC2 t4g.medium:   $0.0224/hr × 9hr × 30 days × ₹85   = ₹514   ← every day, not just weekday
EIP (24/7):       $0.005/hr × 24h × 30d × ₹85        = ₹306   ← dominant fixed cost
EBS gp3 10GB:     $0.0912 × 10 × ₹85                 = ₹78
S3 cold storage:  (tiny dataset, 4 SIDs)             = ₹15
CloudWatch:       Free tier (10/10/5GB)              = ₹0
SNS SMS (India):  100 msgs × $0.00278 × ₹85          = ₹24
SNS Email:        free                               = ₹0
SNS HTTPS Telegram: free                             = ₹0
Lambda (sns→tg):  free tier (96 invokes/day)         = ₹0
Data transfer:    ~10GB outbound                     = ₹85
──────────────────────────────────────────────────────────
TOTAL:                                                ~₹1,022/mo  ✘ ₹22 over <₹1K target
                                                                  ✓ Z+ safe — 2GB RAM headroom
                                                                  ✓ Future-proof for BRUTEX (§13)
                                                                  ✓ Includes weekends for BRUTEX work
```

**Honest envelope:** ₹1,022/mo is ~₹22 over the <₹1K target with every-day schedule. **LOCKED 2026-05-18: Operator picked Option A** — accept ₹22 overage for weekend BRUTEX availability. The <₹1K was a target, not a hard wall; ₹22 is rounding noise on the principle of having the instance available 7 days/week.

| Option | Action | Cost |
|---|---|---|
| **A (LOCKED)** Accept ₹22 over budget | Honest acceptance for weekend BRUTEX availability | **~₹1,022/mo** |
| ~~B~~ Drop EBS 10GB→5GB | (not selected) | ~₹983/mo |
| ~~C~~ Shorten hours 9hr→8hr (08:00–16:00) | (not selected) | ~₹965/mo |

**Pricing correction note (kept for record):** the operator's AWS console screenshot revealed real Mumbai t4g.medium = $0.0224/hr (not $0.0392 quoted by my earlier research agent — 43% error). Without that correction we'd be looking at ~₹1,400/mo.

### Risks accepted (LOCKED t4g.medium)

| Risk | Mitigation |
|---|---|
| 4GB RAM has 2GB headroom after slimmed stack | Monitor `MemoryUtilization` CW alarm at 75%; ample for current ~2GB working set + future strategy load |
| Burstable CPU could exhaust under 09:15:30 IST bursts | Audit: 4 SIDs at 20 ticks/sec + 10 future strategies × 21 TFs ≈ 0.5% of 1 vCPU. Burstable trivially handles it. Baseline 40% × 2 vCPU = 0.8 vCPU effective. |
| Single-AZ — InsufficientInstanceCapacity | Manual fallback to alternate AZ per `aws-capacity-error.md`. 99.99% region SLA = ~4.3min/mo expected downtime. |
| ap-south-1 region outage | Accept envelope. Documented in §B honest envelope. |
| Future BRUTEX strategy load growth >10× expected | See §13 — t4g.medium still fits, t4g.large is the rip-cord if needed (~₹1,262/mo) |

---

## §6. The 4-channel deadman notification (operator's "logging + SMS + call + Telegram")

Operator listed 4 channels. Mapped to AWS primitives:

| Operator's channel | AWS primitive | Cost | Notes |
|---|---|---|---|
| **Logging** | CloudWatch Logs `/tickvault/prod/app` | free 5GB | accessible via console + CLI |
| **SMS** | SNS → SMS subscription, India DLT route | ~₹0.24/msg | sender ID registration required (LOCK-4 pending) |
| **Telegram** | SNS → Lambda → Telegram bot.sendMessage | free tier | same chat as in-band Telegram from app |
| **Call** | NOT in scope for v1 — Amazon Connect adds ~₹500/mo | — | operator-charter §H LOCK-5 chose "stop at SNS 3-leg." If operator now WANTS phone call, see §A below |

**Open question:** the operator said "logging sms call telegram" — does "call" mean a phone CALL via Amazon Connect (adds cost), or is it shorthand for the SNS-call-back-via-Lambda mechanism (already covered by Telegram)? See §A.

---

## §7. The option chain REST loop (LOCKED design per Dhan API)

Per Dhan `option-chain.md` ref:
- Endpoint: `POST https://api.dhan.co/v2/optionchain`
- Headers: `access-token` + `client-id` (BOTH required)
- Body: `{ "UnderlyingScrip": <int>, "UnderlyingSeg": "IDX_I", "Expiry": "YYYY-MM-DD" }`
- Rate limit: **1 unique request per 3 seconds** (per Dhan docs explicit). Different underlyings can be fetched CONCURRENTLY within the 3s window.
- Returns: `data.last_price`, `data.oc[strike]` map with `.ce` and `.pe`, each containing OI / greeks / IV / LTP / volume / top bid+ask / previous_close / previous_oi / previous_volume / security_id.

### The 50s cycle (concurrent fan-out)

```
t=0:    spawn 3 concurrent POST /optionchain calls
        ├── NIFTY (UnderlyingScrip=13, IDX_I, current expiry)
        ├── BANKNIFTY (UnderlyingScrip=25, IDX_I, current expiry)
        └── SENSEX (UnderlyingScrip=51, IDX_I, current expiry)
        (all 3 are DIFFERENT underlyings → Dhan rule satisfied)
t=2s:   responses arrive, parse, merge into RAM cache
t=2s:   async flush to QuestDB `option_chain_snapshots` table
t=2s:   strategy evaluator reads RAM cache (NO DB hit)
t=2s—50s: sleep
t=50s:  next cycle
```

**Auxiliary calls (low frequency):**
- `POST /optionchain/expirylist` at boot + once daily at 15:31 IST (post-close) to detect Thursday rollover.

### Edge cases (locked from hot-path agent audit)

| Edge case | Behavior |
|---|---|
| DH-904 rate limit | exponential backoff per `dhan-annexure-enums.md` rule 11 (10→20→40→80s); skip cycle; alert via Prom counter |
| Cycle > 50s | mutex guard; skip-next-cycle policy (no overlap) |
| Thursday 15:30 rollover | post-close `/expirylist` refetch; rebuild RAM cache for new nearest expiry |
| New strikes mid-day | RAM cache uses `HashMap<f64, OptionData>` — dynamic resize OK |
| Partial CE/PE | `Option<OptionData>` per Dhan rule 7; downstream consumer handles `None` |

---

## §8. Trading decision RAM-first invariant (no DB ever on hot path)

The hot-path agent flagged this is currently HALF-ENFORCED. Required:

1. **Banned-pattern guard fix:** `.claude/hooks/banned-pattern-scanner.sh` covers wrong file path (`oms/risk_check.rs` vs actual `risk/engine.rs`). PR needed to fix.
2. **Scanner extension:** add regex `(execute|exec|query|prepare|SELECT|select)` to banned-paths list (currently path-scoped only).
3. **Hot-path constants captured outside loop:** `tick_processor.rs:1013-1037` fetches `tickvault_storage::global_questdb_config()` inside the loop — lift outside.

These are 3 small fixes in a follow-up plan. The bigger work is wiring the **bar_cache** (already scaffolded at `crates/trading/src/in_mem/bar_cache.rs` per Wave 7-A4) into the indicator engine. Adversarial agent confirmed scaffold exists but is not yet wired.

---

## §9. What gets DELETED (the destruction surface)

The Explore agent is still mapping precise files in background. High-level list (will be sharpened to exact files in plan):

| Surface | Examples |
|---|---|
| Crates code | subscription_planner stock branch, Phase 2 scheduler, depth-20/200 pipelines, depth_strike_selector, depth_rebalancer, depth_dynamic_top_volume_selector, depth_20_dynamic_subscriber, bhavcopy_cross_check, prev_oi loader, live_tick_atm_resolver, market_open_self_test stock sub-checks |
| Greeks pipeline | `crates/trading/src/greeks/` (full module deletion) |
| Movers pipeline | `option_movers` table, `movers_1m` mat view, MoversWriter, movers_pipeline task |
| Storage tables | `option_movers`, `phase2_audit`, `depth_rebalance_audit` (depth-related), all stock-F&O ILP writers |
| Notification events | `Phase2Complete`, `Phase2Failed`, `DepthRebalanced`, `DepthRebalanceFailed`, `Depth20DynamicSwap20`, `Depth20DynamicTopSetEmpty`, all greeks events, all movers events |
| ErrorCode variants | Phase2-*, Depth*-*, Greeks-*, Movers-*, Depth20Dyn-*, Depth200Dyn-* |
| Rule files | `depth-subscription.md`, phase2 runbooks |
| Plans archived | Wave 5 indices-only is already what we want — much of Wave 5 work survives; Wave 4 depth-dynamic / Wave 6 cascade get DELETED |
| Docker services | docker-compose entries: grafana, prometheus, alertmanager, valkey, loki, alloy, traefik |
| Grafana dashboards | entire `deploy/docker/grafana/dashboards/` directory |
| Prometheus alerts | entire `deploy/docker/prometheus/alerts.yml` |

**Estimated deletion size:** ~15K LoC + ~20 audit/ratchet tests + ~30 rule/runbook files + 6 Docker services. **This is the largest single deletion in tickvault history.** Plan it carefully as a multi-PR campaign.

---

## §10. Common-runtime / dynamic / scalable / automated (operator-charter §A)

The operator demanded these are NEVER compromised. Checking at 4-SID scope:

| Demand | At 4 SIDs | Preserved? |
|---|---|---|
| Common runtime Mac=AWS | Same docker-compose.yml, same config TOML, no `#[cfg(target_os)]` divergence | ✅ |
| Dynamic | Subscription scope config-driven, option chain cadence config-driven, schedule via EventBridge config-driven | ✅ |
| Scalable | If operator un-locks the universe later, the same code can scale back up. Bounded mpsc + rescue ring scale via constants. | ✅ — design preserves the dial |
| Automated logging | tracing macros + CloudWatch Logs agent | ✅ |
| Automated tracking | QuestDB audit tables (slimmer set: `order_audit`, `boot_audit`, `auth_renewal_audit`, `selftest_audit`) | ✅ |
| Automated capturing | Tick → QuestDB → S3 cold (5y SEBI) | ✅ |
| Automated visualizing | CloudWatch Dashboards (free tier 3) replaces Grafana | ✅ |
| Automated alerting | CW Alarms → SNS → 3 legs | ✅ |
| Automated notifications | SMS + Telegram + Email | ✅ |
| No manual inputs | EventBridge auto-start, SSM credential fetch at boot, CW agent auto-ships logs, SNS auto-routes | ✅ |
| RAM-first hot path | bar_cache + indicator engine + option chain RAM cache | ✅ (with §8 fixes) |

---

## §A. Three remaining questions for operator

1. **"Call" channel — phone CALL via Amazon Connect, OR is the word a synonym for one of SMS/Telegram?**
   - Amazon Connect outbound call costs ~$0.003/min in ap-south-1 ≈ ₹0.25/call. 10 calls/mo ≈ ₹3.
   - If yes, adds Item AWS-10 back to mandatory. Cost still <₹1K total.
   - If no, current 3-leg is enough.

2. **Instance: t4g.small (2GB, zero headroom, ~₹838/mo) OR t4g.medium (4GB, 2GB headroom, ~₹1,160/mo)?**
   - t4g.small is THE candidate that meets <₹1K.
   - t4g.medium is safer but breaks the budget.
   - Recommendation: **start with t4g.small + CloudWatch MemoryUtilization alarm at 85%.** If alarms fire, upgrade.

3. **EBS volume size — 10GB (locked above) OR 20GB (more growth room)?**
   - 10GB = ₹78/mo. With QuestDB 4-SID data growth of ~50MB/day, 10GB = ~6 months of data before S3 lifecycle migration.
   - 20GB = ₹155/mo. Same idea, 1 year of data.
   - Recommendation: **10GB + aggressive S3 lifecycle (transition to cold at 14 days instead of 30).**

---

## §B. Honest 100% claim (mandatory per operator-charter §F)

> "100% inside the tested envelope of the 4-SID indices-only scope on AWS t4g.small ap-south-1:
> - Zero data loss bounded by the 200,000-seal ring (`SEAL_BUFFER_CAPACITY`) → NDJSON spill → DLQ _(2026-07-18: the 100K tick rescue ring was deleted with the dead tick writer, stage-4 sweep — the seal chain is the live absorption tier)_.
> - WebSocket detect-and-reconnect ≤5s; sleep-until-open post-close; SubscribeRxGuard preserves subscriptions across reconnects.
> - QuestDB outage absorbed up to ring capacity; CloudWatch alarm fires within 30s of disconnect; operator paged via SMS+Telegram+Email simultaneously.
> - Hot path O(1) per yata indicator update; bench-gated ≤100ns p99.
> - Composite (security_id, exchange_segment) uniqueness preserved (degenerate at 4 IDX_I SIDs but invariant holds).
> - CloudWatch detection latency ≤60s for all 12 worst-case scenarios in `aws-daily-lifecycle.md` §3.
> - AWS ap-south-1 99.99% regional SLA = ~4.3 minutes/month expected downtime envelope.
> - **NOT promised:** survival of full ap-south-1 region outage, human acknowledgment within any specific window, sub-60s end-to-end SMS/Telegram delivery (carriers don't publish SLAs)."

---

## §C. Operator-charter §A-§G compliance check

| Demand from operator's restated charter (4th system reminder) | Met by | Status |
|---|---|---|
| 100% code coverage | Existing `crates/storage/tests/zero_tick_loss_alert_guard.rs` + scoped per crate | preserved |
| 100% audit coverage | 4 audit tables (order/boot/auth/selftest) | preserved at slimmer scope |
| 100% testing coverage | 22 test categories per `testing.md` | preserved |
| 100% monitoring | CloudWatch 10 metrics + 10 alarms + 5GB logs | replaces Prom/Grafana |
| 100% logging | tracing → CloudWatch Logs agent | replaces Loki/Alloy |
| 100% alerting | CW Alarms → SNS → 3 legs (SMS+TG+Email) | replaces Alertmanager |
| 100% security | banned-pattern + secret-scan + Secret<T> + cargo-audit | preserved |
| 100% bug fixing | 3-agent adversarial review (this session ran them) | preserved |
| 100% scenarios | 12 worst cases doc + 7 runbooks (this session) | NEW DELIVERABLE |
| 100% functionalities | pub-fn-test + pub-fn-wiring guards | preserved |
| 100% code review | charter §E 3-agent on every PR | preserved |
| 100% extreme check | ratchet tests fail build on regression | preserved |
| Zero ticks loss | bounded envelope (100K ring) | preserved at right size |
| WS never disconnect | SubscribeRxGuard + watchdog | preserved |
| QuestDB never fail | rescue→spill→DLQ + CloudWatch alarm | preserved |
| O(1) latency | bench-gated | preserved |
| Real-time proof | CW metrics + alarms | NEW path |
| Table+diagram format | this doc | preserved |
| Auto-driver clarity | §0 of this doc | preserved |
| No hallucination | honest envelope claim in §B | preserved |
| Mumbai network never compromised | ap-south-1 single-AZ, 99.99% SLA accepted as envelope | NEW: documented honestly |

---

## §11. Full AWS Support Surface — NOWHERE COMPROMISED (operator demand 2026-05-18)

> Operator demand verbatim: *"See entire AWS Monitoring logging support recapturing retriggering entire AWS support needed to be entirely taken care dude okay? Nowhere this should be compromised dude?"*

Reading: every dimension of AWS-side operational support must be wired, not aspirational. Below is the complete coverage matrix.

### §11.1 The 7-dimension AWS support surface (every cell must be filled)

| Dimension | AWS primitive | Status today | Cost impact | Z+ layer |
|---|---|---|---|---|
| **1. Monitoring** — see what's running | CloudWatch Metrics + Dashboards | 10 custom metrics free tier; 3 dashboards free | ₹0 | L1 DETECT |
| **2. Logging** — see what happened | CloudWatch Logs + Logs Insights | 5GB free/mo; 14-day retention | ₹0 | L5 AUDIT |
| **3. Alerting** — wake operator | CloudWatch Alarms → SNS | 10 alarms free | ₹0 | L1+L2 DETECT+VERIFY |
| **4. Recapture** — replay missed events | CloudWatch Events archive + replay; SQS DLQ; SNS retry-policy 50× | Native AWS feature | ₹0 | L6 RECOVER |
| **5. Retrigger** — re-fire failed alarms | CloudWatch Alarm `INSUFFICIENT_DATA` → re-evaluate; SNS DLQ + redrive | Native | ₹0 | L6 RECOVER |
| **6. Auto-remediation** — self-heal | SSM Automation documents; systemd Restart=on-failure | Partial (systemd ✓; SSM playbooks aspirational) | ₹0 | L6 RECOVER |
| **7. Audit / forensics** — post-mortem | CloudTrail (all AWS API calls) + S3 archive | CloudTrail management events FREE; data events extra | ₹0 baseline | L5 AUDIT |

### §11.2 The "Recapture" lock — every lost event must be replayable

| What can be lost | Recapture mechanism |
|---|---|
| SNS message to a subscriber (Telegram webhook, etc.) | **SNS Dead-Letter Queue** (SQS standard). Failed deliveries → DLQ → operator can redrive |
| EventBridge invocation (the 08:00 IST cron) | **EventBridge DLQ** (SQS) + `RetryPolicy.MaximumRetryAttempts=3` + `MaximumEventAgeInSeconds=900` — Item AWS-2 in plan |
| CloudWatch Alarm state-change | Alarm history retained 90 days; queryable via API. Re-fires automatically when condition re-triggers |
| Lambda invocation (SNS→Telegram bridge) | Lambda async invocation has **2 automatic retries + on-failure DLQ** |
| EC2 stop/start API call | CloudTrail records the API call regardless of success/failure |
| CloudWatch metric publish | Tickvault keeps a local circular buffer; on CW outage, retries on next interval. If sustained, falls back to disk log → CW Logs |
| Tick from Dhan WS | Operator-charter §F: **bounded zero loss** via the seal chain — 200K seal ring → NDJSON spill → DLQ _(2026-07-18: the 100K tick rescue ring retired with the dead tick writer; the Dhan live WS itself retired 2026-07-13)_ |

### §11.3 The "Retrigger" lock — failed alarms re-fire

| Failure mode | Retrigger behavior |
|---|---|
| Alarm goes `INSUFFICIENT_DATA` (metric missing) | TreatMissingData=`breaching` → fires Critical SNS; no manual re-fire needed |
| SNS subscription HTTP webhook returns 5xx | SNS retries 50× over ≤3600s automatically |
| SNS SMS carrier reject | Logged in SNS delivery status; CloudWatch alarm `tv-sms-delivery-failed` fires on > 0 rejects → escalates to Email + Telegram |
| Lambda fails 3× | Async DLQ captures payload; CloudWatch alarm on DLQ depth > 0 fires |
| Tickvault crashes during alarm processing | tickvault is NOT in the alarm path (operator demand: OOB). CloudWatch keeps firing independently |
| Operator missed primary alert | Alarm stays in `ALARM` state until condition clears → operator sees it whenever they wake up. SNS delivery status confirms whether SMS/Email reached carrier |

### §11.4 The "AWS support" lock — operator-facing AWS console access

| Resource | Access path | Mobile-accessible? |
|---|---|---|
| EC2 console (start/stop, view state) | AWS Console mobile app + web | ✅ yes |
| CloudWatch Logs Live Tail | AWS Console web + `aws logs tail --follow` CLI | ✅ via CLI on phone hotspot |
| CloudWatch Alarms console (ack, edit) | Console + CLI `aws cloudwatch put-metric-alarm` | ✅ |
| SSM Session Manager (SSH-less shell to instance) | Console "Connect" button + `aws ssm start-session` | ✅ |
| SNS topic publish (manual alert acknowledgment) | `aws sns publish ...` from phone hotspot | ✅ |
| QuestDB console | Port 9000 via SSM port-forward (NOT public) | ✅ via SSM tunnel |
| Application HTTP (`/health`, etc.) | Port 3001 via SSM port-forward | ✅ |

**No public ingress.** Operator reaches the box ONLY via SSM (no SSH keys, no bastion). Reduces attack surface to zero open ports beyond Dhan's outbound WebSocket.

### §11.5 What CloudWatch CANNOT do (honest envelope)

| Capability | Why CloudWatch can't | Workaround |
|---|---|---|
| Real-time custom dashboards with sub-minute granularity | CW custom metrics min resolution = 1s (paid: standard 60s default) | Use Logs Insights for sub-minute log analysis |
| Cross-region failover automatically | CW alarms are regional | Item AWS-12 (deferred per LOCK-7) |
| Complex query language like PromQL | CW Insights uses its own dialect | Document query templates |
| Long-term metric retention > 15 months | CW retention caps | Export to S3 via metric stream + Athena |
| Per-instrument granularity at 4 SIDs (only 4 dimensions) | Free tier has 10 metric × dimension combos | Fine at 4 SIDs × 10 metrics; would break at scale |

**Verdict:** at 4-SID indices-only scope, CloudWatch covers 100% of monitoring/logging/alerting/recapture/retrigger demands. The previous Grafana/Prom/Loki/Alertmanager stack provided sub-second granularity for ~11K instruments which we no longer need.

### §11.6 The 6 alarms that MUST exist (Z+ L1 detect layer)

Per operator demand "nowhere should be compromised":

1. `tv-instance-not-running-by-T+5` — EventBridge fired but EC2 not running
2. `tv-boot-not-complete-T+240` — instance running but tickvault app silent
3. `tv-app-heartbeat-missing-60s` — mid-day crash
4. `tv-ws-pool-zero-60s` — WebSocket pool collapsed (during market hours)
5. `tv-questdb-disconnected-30s` — DB outage
6. `tv-token-remaining-lt-4hr` — auth window closing

Each one fires SNS → 3-leg fan-out (SMS + Telegram + Email). Each one is `TreatMissingData=breaching`. Each one is automatically self-retriggering until the condition clears.

### §11.7 The boot-time AWS support self-test

At 08:01 IST (1 minute after instance running), tickvault must:

1. PUT a heartbeat metric to CloudWatch → confirms IAM PutMetricData permission
2. Publish a startup notification to SNS → confirms IAM Publish permission
3. Write a "boot starting" log line → confirms CloudWatch Logs agent up
4. Read its own JWT secret from SSM → confirms IAM SSM GetParameter permission
5. Read a known S3 bucket → confirms IAM s3:GetObject if needed

If ANY of the 5 fails → halt boot, fire CloudWatch `boot-iam-failure` alarm via the SNS publish that DID work (or via host-level CloudWatch agent). Operator sees the failure within 30s.

**Status:** items 1, 2, 3, 4 already exist in `crates/app/src/main.rs` boot Step 6-8. Item 5 conditional on S3 usage. Self-test wrapper to surface them as a coherent "AWS support OK" Telegram message is NEW — add as Item AWS-13 in plan.

---

## §14. Daily data-integrity cross-verify — "without this we never know if ticks were missed"

> Operator demand 2026-05-18 verbatim: *"every day after precisely 3:31 pm our cross verification of live candle 1m, 5m, 15m, 1h vs historical data of 1m, 5m, 15m and 1h should be cross-verified... only then we will always get to know our live ticks captured the real precise timeframe OHLCV for that particular timestamp or not... without that we will never know what is the gap missing or any ticks missed or any mismatch."*

This is the Z+ L3 RECONCILE layer for the WHOLE pipeline. Without it, missed ticks are invisible.

### §14.1 The 15:31 IST same-day cross-verify (intraday)

```
   15:30:00 IST — NSE closes
       │
       ▼ wait 60 seconds (let Dhan publish final 15:29-15:30 bar)
   15:31:00 IST — scheduler fires
       │
       ▼ for each (instrument, timeframe) in cross product:
       │     instruments: [NIFTY, BANKNIFTY, SENSEX, INDIA VIX]
       │     timeframes:  [1m, 5m, 15m, 1h]
       │     → 4 × 4 = 16 verifications
       │
       ▼ For each pair:
       │   1. Fetch live: SELECT * FROM candles_<tf> WHERE security_id = X AND ts BETWEEN today_0915_ist AND today_1530_ist
       │   2. Fetch historical: POST /v2/charts/intraday {securityId, IDX_I, "1"/"5"/"15"/"60", today, tomorrow}
       │   3. Compare candle-by-candle: OHLCV must match EXACTLY (zero tolerance per historical-candles-cross-verify.md)
       │
       ▼ Verdict:
       │   ✅ All match → emit Severity::Info Telegram
       │      "Cross-match OK | TFs: 4 | Candles: <N> | All OHLCV exact"
       │   ✘ Mismatches → emit Severity::Critical Telegram
       │      "Cross-match FAILED | Compared: <N> | Mismatches: <M>"
       │      "RELIANCE NSE_EQ 1m 2026-05-18 10:15 — open: hist=24710.50 live=24710.00"
       │      "+ N more mismatches"
       │
       ▼ Audit:
       │   INSERT into candle_cross_verify_audit table
       │   (date, instrument, timeframe, candles_compared, mismatches, ts_run)
       │
       ▼ S3 archive after 14d via partition manager lifecycle
```

### §14.2 The morning 1d cross-check (overnight rollover integrity)

```
   ~07:55 IST every trading day (before 08:00 IST EventBridge start)
       │ wait — 08:00 IST is when instance boots. So this runs at ~08:30 IST
       │ AFTER boot is done. New scheduler step:
       ▼
   08:05 IST (configurable) — fire morning_1d_cross_check
       │
       ▼ for each instrument in [NIFTY, BANKNIFTY, SENSEX, INDIA VIX]:
       │   1. Read our derived 1d candle: SELECT * FROM candles_1d WHERE security_id = X AND ts = yesterday_ist
       │   2. Fetch authoritative: POST /v2/charts/historical {securityId, IDX_I, fromDate=yesterday, toDate=today}
       │      → response has columnar OHLCV arrays for yesterday's daily bar
       │   3. Compare OHLCV exactly (zero tolerance)
       │
       ▼ Verdict:
       │   ✅ All 4 match → Severity::Info Telegram "Morning 1d cross-check OK"
       │   ✘ Mismatch → Severity::Critical Telegram + strategy halt for the day until operator OKs
       │
       ▼ Audit:
           INSERT into morning_1d_cross_check_audit table
```

### §14.3 What this catches (the bugs that would otherwise be invisible)

| Bug class | How cross-verify catches it |
|---|---|
| Single missed tick during a 1m window | 1m OHLCV will differ; H/L/V most likely |
| WebSocket silently dropped a packet | Same — H/L/V diverge |
| Aggregator boundary off by 1ms | First/last candle of session will mismatch |
| Clock skew on host (BOOT-03) | All candles offset by the skew amount |
| Dhan-side bar published late | Historical may match our LATER bar's ts — flagged as off-by-one |
| Parser regression (byte-offset bug) | OHLCV values garbled — flagged as severe mismatch |
| Strike/expiry routed to wrong instrument | Cross-check finds the candle for security_id X has wrong data |
| Yesterday's 1d derivation used stale ticks | Morning 1d check catches it before market open |
| QuestDB write reordered ticks | Same — affects candle aggregation |

**Without these cross-checks, ALL of these bugs silently corrupt the data we trade on.**

### §14.4 The 4 new ErrorCode variants

| Code | Severity | Auto-triage? |
|---|---|---|
| `CROSS-VERIFY-01-1531-MISMATCH` | Critical | No (data integrity — operator review) |
| `CROSS-VERIFY-02-1531-HIST-UNREACHABLE` | High | Yes (transient; retry next day) |
| `CROSS-VERIFY-03-MORNING-1D-MISMATCH` | Critical | No (block trading until operator OKs) |
| `CROSS-VERIFY-04-MORNING-1D-HIST-UNREACHABLE` | High | Yes (transient; allow trading with warning) |

### §14.5 The 3 new audit tables (DEDUP UPSERT KEYS)

| Table | Columns | DEDUP key |
|---|---|---|
| `candle_cross_verify_audit` | trading_date_ist, instrument_id, exchange_segment, timeframe, candles_compared, mismatches_count, outcome (Passed/Failed/HistUnreachable), ts_run | `(trading_date_ist, instrument_id, exchange_segment, timeframe)` |
| `candle_cross_verify_mismatches` | trading_date_ist, instrument_id, timeframe, candle_ts, field (open/high/low/close/volume), live_value, hist_value | `(trading_date_ist, instrument_id, timeframe, candle_ts, field)` |
| `morning_1d_cross_check_audit` | trading_date_ist, instrument_id, exchange_segment, outcome, ts_run, hist_open, hist_high, hist_low, hist_close, hist_volume, live_open, live_high, live_low, live_close, live_volume | `(trading_date_ist, instrument_id, exchange_segment)` |

### §14.6 The 4 new CloudWatch alarms

| Alarm | Trigger | Severity | Routing |
|---|---|---|---|
| `tv-cross-verify-1531-not-fired` | Counter `tv_cross_verify_1531_runs_total` did NOT increment between 15:31:00 and 15:35:00 IST | Critical | SNS 3-leg |
| `tv-cross-verify-1531-mismatch` | `tv_cross_verify_mismatches_total > 0` for any tf+instrument | Critical | SNS 3-leg |
| `tv-cross-verify-morning-not-fired` | Counter `tv_cross_verify_morning_runs_total` did NOT increment between 08:05:00 and 08:15:00 IST | Critical | SNS 3-leg |
| `tv-cross-verify-morning-mismatch` | `tv_cross_verify_morning_mismatches_total > 0` | Critical + auto-halt strategy | SNS + kill switch |

### §14.7 Where existing code already covers this (KEEP, narrow scope to 4 SIDs)

| Existing file | What it does today | Action for indices-only scope |
|---|---|---|
| `crates/core/src/historical/cross_verify.rs` | Already runs zero-tolerance cross-match between live `candles_*` and historical fetch | **KEEP** — narrow scope to 4 SIDs only |
| `crates/core/src/historical/candle_fetcher.rs` | Fetches 90-day history + intraday | **KEEP + MODIFY** — only fetch for 4 SIDs; drop the 218 stock fetch loop |
| `crates/core/src/notification/events.rs::CandleCrossMatchPassed` / `Failed` | Existing Telegram variants | **KEEP** |
| `historical-candles-cross-verify.md` rule file | Already documents zero-tolerance + ZERO `CROSS_MATCH_PRICE_EPSILON` | **KEEP + ADD morning 1d section** |

**This means cross-verify is ~80% already built.** What's missing:
1. The 15:31 IST scheduler hookup (today it runs post-market historical fetch, but not pinned to 15:31)
2. The morning 1d cross-check scheduler (NEW)
3. The 3 new audit tables (NEW)
4. The 4 new CloudWatch alarms (NEW)
5. Scope narrowing from ~11K instruments down to 4 SIDs

Plan effort: 1 medium PR (~300 LoC + 4 ratchet tests + 4 alarm Terraform stanzas).

### §14.8 Strategy fail-closed integration

The morning 1d cross-check is a GATE on the trading day:

```
   08:05 IST: morning cross-check fires
       │
       ├─ All 4 match → strategy_armed = true → market open at 09:15 → trading allowed
       │
       └─ Mismatch → strategy_armed = false → Critical Telegram
              "Morning 1d cross-check FAILED for NIFTY: hist=24710.50 live=24710.00"
              "Trading HALTED for today. Operator must investigate + manual /strategy-arm to resume."
              │
              └─ Operator reviews:
                   - SSM session into instance
                   - Read candle_cross_verify_mismatches table
                   - If our derivation was wrong: manual recompute or accept Dhan's value
                   - Run `aws sns publish --topic tv-prod-ack --message strategy-arm`
                   - Strategy resumes
```

This is the Z+ L4 PREVENT layer — refuses to start trading on suspect data.

### §14.9 Honest envelope claim

> "Inside the tested envelope: every trading day at 15:31 IST, 16 cross-verify pairs run (4 SIDs × 4 TFs); zero tolerance OHLCV match required. Every morning at 08:05 IST, 4 daily cross-checks run before strategy arms. Failure → Critical Telegram via 3 SNS legs within 60s + audit row + strategy halt. Beyond envelope (Dhan historical API extended outage, ~24h+): operator manual decision whether to trade without same-day verification."

---

## §13. Future BRUTEX strategy load — DOES t4g.medium STILL FIT?

> Operator demand 2026-05-18: *"even in the future after finalising the strategies and indicators from BRUTEX then we need to run those strategies or indicators also right dude so consider this also."*

After BRUTEX (drill-down brute-force backtesting) selects winning strategies + indicators, those need to RUN LIVE on the same instance. This section sizes for that future load.

### §13.1 Estimated future load (after BRUTEX selects winners)

Assumption set (defensible upper bound):

| Future component | Quantity | Per-unit cost |
|---|---|---|
| Finalised strategies running concurrently | **10** (likely 3-5 in v1; sized for 10 as headroom) | — |
| Underlyings traded | **4** (NIFTY/BANKNIFTY/SENSEX + INDIA VIX as regime filter) | — |
| Timeframes per strategy | **6** (1m/3m/5m/15m/1h/1d initially) → up to **13** (after backtest discovery) | — |
| Strategy evaluation cadence | Per tick OR per-minute boundary (strategy-dependent) | — |
| Indicators per strategy | ~5 (RSI/MACD/BB/SMA/EMA via `yata`) | O(1) update, ~50ns |

### §13.2 The hot-path math (live trading)

Per-tick path (~20 ticks/sec peak from 4 SIDs):

```
20 ticks/sec
  × 4 instruments (each tick = 1 instrument, but aggregator updates ALL TFs)
  × 13 TFs aggregator updates  (parallel; vectorisable)
  × 5 indicators per TF
  × 50ns per indicator update (yata O(1))
═══════════════════════════════════════════════════════════
= 20 × 13 × 5 × 50ns = 65 µs/sec aggregate
= 0.0065% of 1 vCPU
```

Strategy evaluation (per-minute boundary):

```
10 strategies × 4 instruments × 21 TFs × 10 µs eval each
= 2.4 ms / minute
= 0.004% of 1 vCPU
```

OMS order placement (when live trading active) — HONEST LATENCY BUDGET:

| Stage | Latency (typical) | Latency (p99) | We control? |
|---|---|---|---|
| Tick → decision → order constructed → bytes on TCP socket | **<1 ms** | <2 ms | **YES (locked target)** |
| TCP packet → Dhan API ingress (AWS Mumbai → AWS Mumbai) | 0.5–2 ms | 5 ms | network (mostly us) |
| Dhan API validation + OMS routing + exchange forwarding | 5–20 ms | 50–100 ms | **NO (Dhan-side)** |
| Exchange ACK back to us | included above | included | NO |
| **Total wire-to-ACK** | **5–30 ms** | **50–100 ms** | mixed |

**The <1ms lock applies to OUR hot path** (the part we engineer). Dhan-side round-trip is bounded by Dhan's server, NOT something AWS Mumbai or t4g.medium can accelerate further. The AWS Mumbai choice already minimizes the network leg (Dhan's api.dhan.co is also AWS ap-south-1 — same region, ~0.5–2ms RTT).

Adversarial sanity check:
```
Worst case 100 orders/day × 30 ms each = 3 seconds/day = 0.003% sustained CPU
Even with 500 orders/day × 50 ms each = 25 sec/day = 0.03% — still trivial
```

**Slippage envelope (honest, per operator-charter §F):**
> "<1 ms decision-to-wire on our side; 5–30 ms wire-to-Dhan-ACK typical (50–100 ms p99). AWS Mumbai chosen specifically to minimize network leg. Beyond envelope (Dhan-side outage or congestion): order may queue server-side; we detect via Order Update WebSocket timeout and re-place if no ACK within 5 s."

**Combined future CPU usage: <1% of 1 vCPU on t4g.medium (2 vCPU available).**

### §13.3 The RAM impact

| Component | Bytes |
|---|---|
| Strategy state (10 strats × 4 SIDs × 13 TFs × 200B) | **104 KB** |
| Strategy FSM transition tables (10 strats × ~30 states × 200B) | 60 KB |
| Backtested parameter tables (10 strats × 50 params × 50B) | 25 KB |
| Position tracker (open positions × ~500B) | ~5 KB |
| Risk engine state (P&L tracker, exposure caps, kill switch) | ~10 KB |
| Order book (last N orders, ~100 × 500B) | 50 KB |
| **Total additional vs base** | **~250 KB** |

**Negligible — adds 0.25 MB to app's ~280 MB working set.** Still comfortable in t4g.medium 4GB.

### §13.4 What about deep-future expansion (20+ strategies, 13 TFs each)?

| Scenario | CPU | RAM additional | Still fits t4g.medium? |
|---|---|---|---|
| Current (no live strategies) | <0.1% | 0 | ✅ yes, ample |
| Phase 1 (3-5 strategies, 21 TFs) | <0.3% | ~50 KB | ✅ yes |
| Phase 2 BRUTEX-finalised (10 strategies, 21 TFs) | <0.5% | ~250 KB | ✅ yes |
| Stress case (20 strategies, 13 TFs) | <2% | ~500 KB | ✅ yes |
| Theoretical (50 strategies, 13 TFs) | <5% | ~2 MB | ✅ yes (CPU + RAM both fine) |
| Pathological (100+ strategies) | ~10% | ~5 MB | ✅ yes; rip-cord to t4g.large only if order placement throughput becomes the bottleneck |

**Verdict: t4g.medium handles 5× growth from BRUTEX-finalised strategies without breaking a sweat.** The 4GB RAM ceiling is dominated by QuestDB (1GB) + OS + headroom, NOT by strategy state. The 2 vCPU ceiling is dominated by burstable baseline (40%×2=0.8 vCPU effective), and live trading workload uses <2% of that even at 20 strategies.

### §13.5 When would we need to upgrade?

| Trigger | Action |
|---|---|
| `MemoryUtilization > 75%` for 1 hr | Upgrade to t4g.large (8GB, +₹377/mo) |
| `CPUCreditBalance < 50` for 1 hr | Upgrade to c7g.large non-burstable (+₹1,000/mo) |
| OMS placing > 500 orders/day | Stay on t4g.medium; bottleneck is Dhan REST not us |
| QuestDB disk usage > 80% of 10GB EBS | Expand EBS to 20GB (+₹78/mo) — separate from instance |
| BRUTEX-finalised strategy count > 30 | Review carefully; likely still fits but design 3-agent re-audit |

**All upgrades are non-destructive: stop instance → resize → start. ~5min downtime, happens outside market hours.**

### §13.6 Z+ guarantee for future load

Per operator-charter §F honest envelope:

> "100% inside the tested envelope of {4 SIDs × ≤20 strategies × ≤13 TFs × 4GB t4g.medium ARM ap-south-1}: hot-path latency O(1) per indicator update, strategy eval ≤2.4ms per minute aggregate, RAM working set ≤300MB inclusive of all future strategy state. Headroom ratchet test pins the working set; bench-gate fails build on >5% regression. Beyond the envelope, MemoryUtilization alarm fires at 75% and operator decides to upgrade instance."

### §13.7 The 3 ratchet tests this section implies

When the BRUTEX work lands, add these tests:

1. `crates/trading/tests/strategy_load_ratchet.rs` — measures total RAM with 20 mock strategies; fails build if > 5 MB
2. `crates/trading/benches/strategy_eval_p99.rs` — Criterion bench; per-eval p99 ≤ 100 µs; fails build at >5% regression
3. `crates/storage/tests/cpu_credit_budget_alarm_guard.rs` — pins the `tv-cpu-credit-low` CloudWatch alarm name + threshold (=50)

---

## §12. Reading guide — which doc do you open when?

| Question | Open this file |
|---|---|
| What does 8 AM auto-start do? | `aws-daily-lifecycle.md` |
| What happens when [worst case]? | `aws-daily-lifecycle.md` §3 + the per-case runbook |
| What instance do we use? | this file §5 |
| Where does monitoring live? | this file §4 + §11 |
| What gets deleted in the scope reduction? | `.claude/plans/aws-lifecycle/deletion-surface-map.md` |
| What's the schedule? | this file §1 L11 (08:00/17:00 IST) |
| Can we hit <₹1K/mo? | `aws-cost-floor-analysis.md` + this file §5 (answer: no, ₹1,168 is the safe floor) |

---

## §15. Market hours + pre-open + open price data (operator question 2026-05-18)

### NSE / BSE market session timing (IST, all in `crates/common/src/trading_calendar.rs`)

| Window | IST | Activity for our 4 SIDs |
|---|---|---|
| 08:00 IST | Instance boots, tickvault starts | Step 1-8 boot per §1 L11 |
| 08:05 IST | **Morning 1d cross-check fires** | §14.2 — yesterday's daily candle verified |
| 09:00–09:08 IST | **Pre-open session** (order entry, modify, cancel) | We capture pre-open ticks if Dhan emits |
| 09:08–09:15 IST | **Final pre-open** (order matching, no new entries) | We capture |
| 09:15:00 IST | **Market opens** — continuous trading begins | Live ticks flow; strategy can arm |
| 09:15:30 IST | `MarketOpenStreamingConfirmation` Telegram fires | Positive signal — boot+WS healthy |
| 09:16:30 IST | `SelfTestPassed` Telegram fires | Z+ L1 daily heartbeat |
| 12:00–12:35 IST | (No lunch break on NSE/BSE since 2010 — continuous) | Continuous |
| 15:00 IST | Last 30 min: order book aggressively volatile | Strategy may de-risk |
| 15:30:00 IST | **Market closes** — no new orders matched | Live tick flow stops |
| 15:31:00 IST | **Daily intraday cross-verify fires** | §14.1 — 16 cross-checks |
| 15:45 IST | EOD digest Telegram (P&L, order count, alerts) | Operator daily summary |
| 17:00 IST | Instance auto-stops (EventBridge) | No further activity |

### Pre-open price data capture (existing infra, narrowed to 4 SIDs)

The existing pre-open buffer per `live-market-feed-subscription.md` 2026-04-22 Updates §5 captures 09:00–09:12 IST close prices into an in-RAM `PREOPEN_INDEX_UNDERLYINGS` map. For our 4-SID universe, this captures:

| SID | Symbol | Pre-open data | Used for |
|---|---|---|---|
| 13 | NIFTY | yes (IDX_I emits) | Indicator warm-up; first 1m candle accuracy |
| 25 | BANKNIFTY | yes | Same |
| 51 | SENSEX | yes | Same |
| 21 | INDIA VIX | yes | Volatility regime detection |

### Open price data — capturing the 09:15:00 IST open candle precisely

This is the heart of the 15:31 cross-verify check (§14):
- Our derived `candles_1m[09:15:00–09:15:59]` MUST match Dhan REST `/v2/charts/intraday` interval=1 for the same timestamp
- Zero tolerance match
- If mismatch → CROSS-VERIFY-01 Critical Telegram + audit row naming the field that differs (open/high/low/close/volume)

**Without this, we'd never know if we missed the opening tick.** The cross-verify is the proof.

### "Will market hours code still work after the slim-down?"

YES. `crates/common/src/trading_calendar.rs::is_within_market_hours_ist()` is scope-agnostic — it works the same whether we have 4 SIDs or 11,000. KEEP this module.

---

## §16. TOTP — extreme complete automated authentication

Operator question 2026-05-18: *"what about TOTP extreme complete automated authentication?"*

### What exists today (works identically in indices-only scope)

`crates/core/src/auth/token_manager.rs` (KEEP — unchanged by scope reduction):

| Step | Action | Source |
|---|---|---|
| Boot Step 3 | Check in-memory token cache (was Valkey — now `Arc<Secret<String>>`) | `token_manager::get_or_refresh()` |
| If miss / expired | Fetch TOTP secret from AWS SSM `/tickvault/prod/dhan_totp_secret` | `secret_manager::fetch_totp_secret()` |
| Generate code | `totp-rs` crate, RFC 6238, 6 digits, 30s window | `totp_generator::generate_now()` |
| Build URL | `POST https://auth.dhan.co/app/generateAccessToken?dhanClientId=...&pin=...&totp=<code>` | `auth::generate_access_token()` |
| Cache JWT in memory | `Arc<ArcSwap<Secret<String>>>` for lock-free reads | hot-path safe |
| Schedule renewal | 23h after issue (1h before expiry) | tokio interval task |
| AUTH-GAP-03 wake | On WS reconnect after sleep, if token < 4h remaining → force-renew BEFORE reconnect attempt | already shipped Wave-2 |

### The TOTP automation chain (zero operator action after bootstrap)

```
   Operator action ONCE during bootstrap:
       ─ paste TOTP secret to scripts/aws-bootstrap.sh
       ─ script writes to AWS SSM /tickvault/prod/dhan_totp_secret as SecureString
       ─ script exits

   FROM THAT MOMENT FOREVER:
       ─ Every day at 08:00 IST tickvault boots
       ─ Boot Step 3 reads SSM, generates TOTP, gets JWT
       ─ Operator never sees / never touches TOTP again
       ─ 23h proactive renewal continues automatically
       ─ Wake-from-sleep force-renewal continues automatically
```

### Failure modes already covered

| Failure | Detection | Recovery |
|---|---|---|
| TOTP secret rotated externally (operator regen via Dhan UI without updating SSM) | DH-901 on auth call | AUTH-GAP-04 ErrorCode → Critical Telegram → operator updates SSM via 1 command, instance restarts |
| SSM unreachable | timeout | retry 3× with 5s backoff; HALT if persistent |
| TOTP code rejected (clock skew vs Dhan server) | DH-901 from token endpoint | BOOT-03 clock-skew probe catches this BEFORE auth attempt (already shipped Wave-2-C) |
| JWT expires mid-session | 24h watchdog | token-manager 23h pre-emptive renewal |

### "Is this extreme complete automation?"

Yes. After bootstrap, operator NEVER manually generates a TOTP code. The system handles:
- Daily token generation
- 23h proactive refresh
- Wake-from-sleep force-refresh
- Error recovery (cache → SSM → TOTP cascade)

The only manual action is the one-time SSM secret seed during `aws-bootstrap.sh`.

---

## §17. The simple operator promise — "8 AM to 5 PM, no issues, track + log + monitor + alert"

Operator verbatim 2026-05-18: *"starting 8 am till 5 pm without any issues outage reconnect disconnect it should work always track log monitor alert telegrams sms call everything dude okay?"*

### What we promise (plain English, no jargon)

| Operator demand | What we promise | Honest envelope |
|---|---|---|
| 08:00–17:00 IST every day | Instance auto-starts at 08:00 IST every day (Mon-Sun), auto-stops at 17:00 IST | If EventBridge fails, operator paged within 7 min via 3-leg SNS + phone call |
| "No issues" | Boot complete + BootReadyConfirmation Telegram by 08:03 IST | If boot fails by 08:04 IST, alarm fires within 60s, 4-channel notify |
| "No outage" | Single instance ap-south-1 99.99% region SLA = ~4.3 min/mo expected | If outage > 4 min, alarm fires, operator paged. Beyond envelope: accept |
| "No reconnect / disconnect" | WS reconnects with SubscribeRxGuard ≤5s; sleep-until-open post-close | SEBI 24h JWT forces ≥1 reconnect/day BY LAW. We make it invisible (≤5s) but cannot eliminate |
| "Tracked always" | Every tick → ticks table; every order → order_audit; every alarm → SNS topic | 7 audit tables; SEBI 5y retention for order_audit |
| "Logged always" | CloudWatch Logs 14d retention + S3 lifecycle for 5y | Every `error!` carries `code = ErrorCode::X.code_str()` field |
| "Monitored always" | 10 CloudWatch metrics, scrape every 60s | Free tier; CloudWatch agent auto-installed |
| "Alerted always" | 10 CloudWatch alarms; TreatMissingData=breaching | Edge-triggered; coalesced; never silent |
| "Telegram + SMS + Call everything" | SNS topic → SMS leg + Telegram-bot-Lambda leg + Email leg + Amazon Connect outbound voice for Critical | 4-channel deadman; one fails, others still fire |

### The 4 daily positive Telegram pings (proof of life)

Each is operator-readable on phone in 5 seconds:

| IST time | Telegram | What it proves |
|---|---|---|
| 08:03 | `✅ tickvault started \| boot_id: <X>` | Boot completed; all 18 gates passed |
| 09:15:30 | `✅ Streaming live \| WS:1/1 \| Order-WS:1/1 \| SIDs:4/4 \| Token:23h45m` | Market opened, feed flowing |
| 15:31 | `✅ Cross-match OK \| TFs: 4 \| Candles: <N> \| All OHLCV exact` | Same-day data integrity proven |
| 15:45 | EOD digest: P&L + order count + alarms summary + tomorrow's expiry | Day complete |

**If any of these 4 is missing on a trading day → operator MUST investigate.** They are the positive heartbeat per audit-findings Rule 11 (no false-OK).

### What we cannot promise (per operator-charter §F honest envelope)

| Literal demand | Physical truth | What we substitute |
|---|---|---|
| "Never disconnect" | SEBI 24h JWT forces reconnect | Invisible reconnect ≤5s |
| "Never outage" | AWS region 99.99% SLA = 4.3min/mo possible | Telegram + SMS + Call within 60s of detection |
| "Never miss a tick" | Network packets can drop | 200K seal ring → NDJSON spill → DLQ _(2026-07-18: tick rescue ring deleted with the dead tick writer; the seal chain is the live tier)_ |
| "Never fail" | QuestDB is third-party | Absorbs via 3-tier; alarm within 30s |
| "100% guarantee no hallucination" | Words are not proof | Every claim has a ratchet test that fails build on regression |

**This is the honest 100%.** Anything stronger would be a lie.

---

## §D. Trigger / auto-load paths

This doc auto-loads when editing:
- `deploy/docker/docker-compose.yml`
- `deploy/aws/terraform/*.tf`
- `crates/common/src/constants.rs` (TICK_BUFFER_CAPACITY)
- `crates/common/src/config.rs` (SubscriptionScope)
- `crates/app/src/main.rs` (boot sequence)
- Any file containing `tv-app`, `tv-questdb`, `t4g.small`, `CloudWatch`, `IndicesUnderlyingsOnly`
