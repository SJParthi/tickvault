# THE FINAL LOCKED PLAN — Tickvault Indices-Only Production Deployment

> **Status:** LOCKED 2026-05-18. Single source of truth for the next phase of work.
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file > all other docs.
> **Companion docs:** all 9 prior session design docs roll up into this plan.
> **Scope:** Production deployment of locked indices-only architecture. NOT yet covering: strategy design, BRUTEX, risk engine wiring (deferred per operator "only this is needed as of now").

---

## §0. Auto-driver one-liner

> "Sir, this is the ONE plan. We have everything designed: 4 indices in head/RAM, 21 charts updating every tick, 50-second option chain refresh, 30 days history, 4-alarm phone (SMS + Telegram + Email + Call), automatic 8 AM to 5 PM daily startup. Now we BUILD it. 14 numbered PRs in order. Each PR ships, then next starts. Total ~6 weeks. You watch Telegram, that's all."

---

## §1. The LOCKED scope (frozen 2026-05-18)

| Element | Locked value |
|---|---|
| Universe (live feed) | NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21 — **4 IDX_I SIDs only** |
| Option chain (REST every 50s) | NIFTY + BANKNIFTY + SENSEX nearest expiry only — **3 underlyings** (VIX has no options) |
| Timeframes | **21 dynamic IST wall-clock TFs** (1m..15m + 30m + 1h/2h/3h/4h + 1d) |
| Historical window in RAM | **30 trading days rolling** |
| RAM-first hot path | All TFs + indicators + option chain in RAM. DB = replay/audit ONLY. Trading decisions NEVER hit DB. |
| Materialized views | **ZERO** — tickvault aggregator computes everything in RAM, sends sealed bars directly to QuestDB |
| Pre-open data | Capture NIFTY+BANKNIFTY+SENSEX+INDIA VIX ticks during 09:00–09:15 IST (pre-open session); use for indicator warm-up |
| Live open data | 09:15:00 IST first tick → first 1m bar precision proven by 15:31 IST cross-verify (zero tolerance) |
| WebSockets | 1 main-feed (4 SIDs) + 1 order-update — **2 connections forever** |
| Cross-verify | 15:31 IST same-day (1m/5m/15m/1h × 4 SIDs = 16 pairs) + 08:05 IST morning 1d |
| Reconnect policy | **REAL reconnect = new TCP + re-auth + re-subscribe** — target ≤100 ms end-to-end (NOT literal 0 ms — physically impossible per `precision-realism-and-table-redesign.md` §1.1). Force-close old socket; verify first tick within 5s of resubscribe |
| Crash recovery target | **~3 seconds** end-to-end (systemd RestartSec=0 + tmpfs cached JWT + lazy bar_cache hydration) per `precision-realism-and-table-redesign.md` §1.2 |
| Tables | **31 locked** — fresh redesign, precise columns only; `previous_close` table DROPPED (morning 1d fetch provides it); PrevClose packet parser DROPPED |
| Tick loss policy | Bounded zero-loss inside 100K rescue ring envelope; cross-verify catches gaps; REST backfill on detected gap |
| AWS instance | t4g.medium ARM ap-south-1, Default tenancy, on-demand |
| Schedule | **08:00–16:00 IST EVERY DAY (Mon–Sun)** — UPDATED 2026-05-18 (saves ~₹57/mo; 16:00 gives 30 min buffer after 15:30 close + 15:31 cross-verify + 15:45 EOD digest) |
| Docker stack | tickvault + QuestDB only (2 containers) |
| Observability | CloudWatch (Logs + Metrics + Alarms) — single sink |
| Notification | **4-channel**: SMS + Telegram + Email + Phone CALL (Amazon Connect outbound voice for Critical) |
| Monthly cost | ~₹1,037/mo (₹1,022 base + ₹15 Connect) |
| Bootstrap path | **Cowork prompt** (LOCKED — see §10) |
| Mac dev workflow | IntelliJ → cargo on Mac → docker compose up locally → git push → GitHub Actions → SSM deploy → AWS (LOCKED) |

---

## §2. The trading session timing (pre-open + live open, locked)

```
   IST                  Activity                                   Tickvault state
   ───                  ────────                                   ──────────────
   00:00:01            IST midnight roll                          bar_cache evicts oldest day; 30d window slides
   08:00:00            EventBridge cron fires                     AWS StartInstances
   08:00:30            Instance running, cloud-init starts        systemd boots tickvault.service
   08:03:00            Boot complete, all 8 steps green           BootReadyConfirmation Telegram fires ✅
   08:05:00            Morning 1d cross-check fires              compares yesterday's derived 1d candle vs Dhan REST
   08:05:30            Match OR mismatch → both continue trading. CROSS-VERIFY-03 Critical fires on mismatch but does NOT block trading. Operator decides whether to manually pause.
   08:30:00            (operator wakes, checks Telegram)         operator scrolls phone
   09:00:00            **PRE-OPEN SESSION BEGINS**               Tickvault starts capturing pre-open ticks
   09:00:01–09:08:00   Pre-open price discovery                  Ticks captured to RAM aggregator (1m TF starts)
   09:08:01–09:14:59   Final pre-open (order matching)           Ticks continue; pre-open buffer fills
   09:13:00            (in old design Phase 2 fired here)        In new indices-only design: NOTHING (no stock F&O)
   09:14:30            Phase 2 readiness pre-check               (Wave 5 Item 9 — verify all 11 gates green pre-open)
   09:15:00            **MARKET OPEN — continuous trading**      First REAL trade tick arrives
   09:15:30            MarketOpenStreamingConfirmation           Telegram ✅ "Streaming live | WS:1/1 | SIDs:4/4"
   09:16:30            SelfTestPassed (7 sub-checks all green)   Telegram ✅
   12:00–15:30         Continuous trading (no lunch break)       Ticks flow; 21 TFs update in RAM; option chain every 50s
   15:30:00            **MARKET CLOSE**                          Live tick flow stops
   15:31:00            Daily intraday cross-verify fires         **12 pairs (4 SIDs × 3 TFs: 1m, 5m, 15m only)** zero-tolerance OHLCV match. 1-min buffer ensures the final 15:29:00–15:29:59 candle has settled in our aggregator before comparison.
   15:31:30            Cross-verify outcome → Telegram           ✅ Pass OR ✘ Fail Critical with mismatch details. **Does NOT block trading the next day** — operator decides.
   15:45:00            EOD digest Telegram                       P&L + order count + alarms summary
   16:00:00            EventBridge stops instance                AWS StopInstances; tickvault clean shutdown (UPDATED 2026-05-18)
   
   Next morning 08:05  1d cross-verify catches yesterday's full-day candle vs Dhan REST /v2/charts/historical interval=DAY
```

### Pre-open + live open precision proof (§14 of locked architecture doc)

The pre-open and live-open ticks are subject to the SAME 15:31 IST cross-verify. If we miss a pre-open tick, the 09:15 first 1m bar OHLCV will mismatch Dhan REST → CROSS-VERIFY-01 Critical fires. **This is the proof we captured the open precisely.**

---

## §3. REAL reconnect with REAL resubscribe (operator demand 2026-05-18)

Operator: *"fast boot crash or slow boot crash it can act like working but failing reconnect or disconnect should real reconnect with real resubscribe."*

### The "fake working" problem

| Problem | Detection today | Action today | NEW required action |
|---|---|---|---|
| TCP socket says ESTABLISHED but no data flows (Dhan-side silent drop) | WS-GAP-06 tick-gap detector @ 30s | logs ERROR; existing reconnect attempt | **FORCE close socket → NEW TCP connection → fresh handshake → re-auth → RE-SUBSCRIBE** |
| Process running but main feed task hung | watchdog on task completion | logs ERROR | **systemd Restart=always; supervisor respawns task within 5s** |
| Process boots but key subsystem silently dead (e.g., option chain scheduler hung) | per-subsystem heartbeat counter | varies by subsystem | **CloudWatch alarm on missing heartbeat → forced systemctl restart via SSM RunCommand** |
| WS reconnected but subscribe-command channel dropped | SubscribeRxGuard (PR #337) | guard re-installs | already correct ✅ |
| Reconnect happened but Dhan never sends ack | no current detection | none | **NEW: wait 5s for first tick post-resubscribe; if absent, RECONNECT AGAIN with fresh TCP** |

### The REAL reconnect algorithm (LOCKED)

```rust
// PSEUDOCODE — pattern locked, implementation in PR #5 below
async fn detect_and_reconnect() {
    loop {
        tokio::select! {
            _ = tick_gap_detector.wait_for_gap(Duration::from_secs(30)) => {
                if is_within_market_hours_ist() {
                    error!(code = "WS-GAP-06", "Silent socket detected — forcing real reconnect");
                    
                    // 1. Force-close existing TCP — do NOT trust polite shutdown
                    let _ = ws_writer.close_immediately().await;
                    drop(ws_reader);
                    
                    // 2. Establish NEW TCP connection
                    let new_ws = connect_with_fresh_tcp().await?;
                    
                    // 3. Re-authenticate with current JWT
                    let token = token_manager.get().await;
                    new_ws.send(build_auth_message(&token)).await?;
                    
                    // 4. RE-SUBSCRIBE all 4 SIDs (do NOT rely on session continuity)
                    for sid in [NIFTY, BANKNIFTY, SENSEX, INDIA_VIX] {
                        new_ws.send(build_subscribe_ticker(*sid)).await?;
                    }
                    
                    // 5. Verify first tick arrives within 5s — else RECONNECT AGAIN
                    match timeout(Duration::from_secs(5), new_ws.next_tick()).await {
                        Ok(Some(_)) => info!("Real reconnect confirmed — ticks flowing"),
                        _ => {
                            error!(code = "WS-GAP-07-NEW", "Reconnect succeeded but no tick — retrying");
                            continue; // tight loop with exponential backoff
                        }
                    }
                }
            }
        }
    }
}
```

### What this catches that the old code didn't

| Failure mode | Old behavior | New behavior |
|---|---|---|
| Dhan kills TCP silently | Watchdog → reconnect, but old socket lingered → race | Force-close BEFORE reconnect; no race |
| Reconnect succeeds but no resubscribe sent | Possibly missed subscribes for some SIDs | Always re-send all 4 SID subscribes |
| Reconnect happens but Dhan doesn't ack | Sit there forever pretending to work | 5s timeout → reconnect again |
| Multiple reconnects in rapid succession | Could fork connections | Single-task ownership; exponential backoff |

### 4 new ErrorCode variants

| Code | Severity | Trigger |
|---|---|---|
| `WS-GAP-07-FORCED-RECONNECT` | High | Silent socket detected, forced reconnect initiated |
| `WS-GAP-08-RESUBSCRIBE-NO-TICK` | Critical | Resubscribe sent but no tick in 5s |
| `WS-GAP-09-RECONNECT-STORM` | Critical | >5 reconnect attempts in 60s |
| `WS-GAP-10-ZOMBIE-PROCESS` | Critical | tickvault process running but main feed task missing heartbeat 60s |

---

## §4. Worst-case tick crash recovery (operator demand 2026-05-18)

Operator: *"in worst case if any ticks fail due to any crash means everything needs to be considered."*

### The crash + recovery chain

```
   tickvault crashes mid-market at T+0 (say 11:23:47 IST)
       │
       ▼
   systemd Restart=on-failure RestartSec=3 detects exit, waits 3s
       │
       ▼
   T+3s: systemd starts tickvault again
       │
       ▼
   Boot Step 1-7: config, observability, auth (TOTP from SSM), QuestDB DDL, bar_cache rehydration
       │
       ▼
   Boot Step 4a: bar_cache loads last 30 days from QuestDB (~84 SELECT queries, async parallel, ~2s)
       │
       ▼
   T+15s: bar_cache fully rehydrated INCLUDING all bars sealed before the crash today
       │
       ▼
   T+18s: Main feed WS reconnects with fresh TCP + re-auth + re-subscribe (§3 algorithm)
       │
       ▼
   T+20s: First post-crash tick arrives → aggregator resumes
       │
       ▼
   THE GAP: from crash at 11:23:47 to first new tick at 11:24:07 = 20 seconds of TICKS LOST
       │
       ▼
   15:31 IST cross-verify CATCHES this:
   - Compares our derived candles_1m for 11:23:00 and 11:24:00 vs Dhan REST
   - Mismatch detected on those 2 candles
   - CROSS-VERIFY-01 Critical Telegram fires (Phone+SMS+TG+Email)
   - Audit row in candle_cross_verify_mismatches naming exact fields that differ
       │
       ▼
   Optional recovery (NEW — see below):
   - Auto-backfill via Dhan REST /v2/charts/intraday for the gap window
   - Re-derive missed candles from REST OHLCV
   - Overwrite candles_1m for affected timestamps
   - Re-run cross-verify; should now pass
```

### The auto-backfill on crash detection (NEW design)

```rust
// PSEUDOCODE — pattern locked, implementation in PR #6 below
async fn on_boot_after_crash(crash_time: i64) {
    if was_a_crash_today(crash_time) {
        let gap_start = crash_time;
        let gap_end = first_post_crash_tick_time;
        
        // Backfill via REST for each TF
        for tf in [_1m, _5m, _15m, _1h] {
            let rest_bars = fetch_intraday_rest(NIFTY, tf, gap_start, gap_end).await?;
            for bar in rest_bars {
                bar_cache.upsert(bar);  // overwrites any partial derived bars
                questdb_writer.upsert(bar);  // overwrites candles_1m/5m/15m/1h
            }
        }
        
        // Same for BANKNIFTY, SENSEX, INDIA VIX
        // Emit Telegram: "Crash recovery: backfilled <N> bars from <gap_start> to <gap_end>"
        emit_telegram_info(format!("Crash recovery: backfilled {} bars", n));
    }
}
```

### Worst-case scenarios covered

| Scenario | RTO | Recovery mechanism |
|---|---|---|
| Process crash, systemd restart succeeds in 20s | 20s gap → 100% recovered via REST backfill | bar_cache rehydration + REST backfill |
| Process crash, systemd 3-retry exhausts | 60s+ → alarm fires (BOOT-02) | Operator manual restart; same backfill on next boot |
| EC2 instance reboot (kernel panic) | ~3 min (instance reboot + boot) | Cross-verify catches the gap; backfill runs on next boot |
| EC2 instance crash, AWS needs to reschedule | ~10 min | Same |
| QuestDB crash mid-write | Rescue ring absorbs ticks; QuestDB restart auto via Docker | No tick loss inside 100K ring envelope |
| Both tickvault + QuestDB crash simultaneously | 100K rescue ring → NDJSON spill → DLQ NDJSON | Bounded zero-loss inside spill capacity |
| Disk full (EBS gp3 hit limit) | Storage-gap-05 alarm; rescue ring fills; eventual DLQ | Operator pages, expands EBS |
| AWS region outage > 60s | Outside envelope per operator-charter §F | Accept; CloudWatch alarm fires when region returns |

---

## §5. THE 14 PR SEQUENCE (operator-charter §H serial completion)

| # | PR | What ships | Effort | Files |
|---|---|---|---|---|
| 1 | **Prep: config blocks + ErrorCode skeletons + ratchet tests** | Cement new contracts BEFORE deletions | Small | `[option_chain]`, `[cross_verify]`, `[telemetry]` TOML; 12 new ErrorCode stubs; ratchet test skeletons |
| 2 | **Delete movers pipeline + tables** | Lowest risk; already mostly retired | Small | `MoversWriter`, `option_movers`, 25 matviews |
| 3 | **Delete Greeks inline pipeline (keep black_scholes lib)** | Decoupled | Medium | `inline_computer.rs`, `aggregator.rs`, pipeline task |
| 4 | **Delete depth-20 / depth-200 dynamic infrastructure** | Big chunk; ~3K LoC | Large | All depth subdirs |
| 5 | **Delete Phase 2 scheduler + emit guard + bhavcopy + prev_OI + live_tick_atm_resolver** | Stock F&O orchestration out | Large | Phase 2 module entire |
| 6 | **Replace universe builder + CSV + rkyv cache → 4-line `LOCKED_UNIVERSE` const** | Largest LoC single chunk (~12K removed → 50 added) | Large | `instrument/` dir gutted |
| 7 | **Tighten SubscriptionScope to `Indices4Only`; drop 218 NSE_EQ + 25 sectoral display indices** | Final universe lock | Small | Config + planner |
| 8 | **Build `option_chain/` module (REST fetcher, 50s scheduler, RAM cache, full-day request+response history, audit writer)** | Heart-piece live | Medium | NEW module |
| 9 | **Build `cross_verify/` module (15:31 IST + 08:05 IST schedulers + comparator + 3 audit tables + auto-backfill)** | Z+ L3 reconcile + crash recovery | Medium | NEW module |
| 10 | **Cutover observability: delete Grafana/Prom/Alertmanager/Loki/Alloy/Traefik/Valkey from docker-compose; add CloudWatch agent config + 10 alarm Terraform stanzas** | Big visible change | Large | Docker + Terraform |
| 11 | **REAL reconnect algorithm (§3 spec) — force-close TCP, re-auth, re-subscribe, 5s tick verify** | Catches the fake-working bug | Medium | `websocket/connection.rs` |
| 12 | **Rescue ring resize 5M → 100K; per-subsystem heartbeat gauges; zombie-process detection (WS-GAP-10)** | Right-size + fake-working detection L1 | Medium | constants + observability |
| 13 | **Terraform: switch instance type to t4g.medium ARM; schedule every-day 08:00/17:00; new alarms; Amazon Connect outbound voice Lambda for Critical phone CALL** | AWS-side cutover + 4-channel notification | Large | Terraform + Lambda |
| 14 | **Mac dev parity ratchet test + GitHub Actions deploy.yml + market-hours guard + auto-rollback** | Production deploy pipeline live | Medium | `.github/workflows/deploy.yml`; ratchet test |

**Total: 14 PRs. Estimated 6-8 weeks at 1 PR/session per operator-charter §H serial completion. ~36K LoC removed, ~6K LoC added, net -30K LoC.**

---

## §6. Bootstrap path — LOCKED to Cowork prompt

Operator: *"Choose bootstrap path (script OR Cowork prompt)."*

**LOCKED: Cowork prompt.** Reasoning:

| Reason | Detail |
|---|---|
| Operator stated "I don't even have idea about how to buy or launch instance" | Cowork has Claude environment + AWS CLI + Terraform pre-installed; operator only needs AWS signup + credit card |
| `aws-bootstrap.sh` requires local Mac to have AWS CLI + Terraform + jq installed | Adds friction; Cowork avoids this |
| Cowork can pause + ask operator interactively when secrets needed | Script can prompt too, but Cowork has richer UX |
| Cowork can self-correct on errors via tool calls | Script needs operator to debug bash |
| Cowork session can hand off to long-running monitoring loop | Script exits after run |

**The Cowork prompt is in `docs/runbooks/operator-end-to-end-automation.md` §11 — paste it verbatim into a fresh Claude Cowork window.**

`scripts/aws-bootstrap.sh` design stays in the doc as a REFERENCE for what Cowork should do — Cowork can use it as a checklist OR write it on the fly.

---

## §7. Mac dev workflow — LOCKED

Operator: *"Mac dev workflow — IntelliJ → cargo → docker-compose → git push → AWS deploy."*

```
   On Mac (operator's laptop):
   ─────
   1. IntelliJ open
   2. Edit code
   3. `make docker-up` brings up tickvault + QuestDB locally
   4. `cargo test -p tickvault-<crate>` runs scoped tests
   5. `make doctor` verifies local health
   6. Commit on feature branch
   7. `git push -u origin feature-xyz`
   8. GitHub PR opened — CI runs 22-test battery
   9. CI green → operator merges to main
   ──── Mac side done. AWS side begins automatically ────
   10. GitHub Actions `deploy.yml` triggers on main merge
   11. IST clock guard: refuses 09:14–15:31 IST
   12. Outside market hours → AWS OIDC AssumeRole
   13. SSM RunCommand: stop tickvault, swap binary, start
   14. Wait 180s for BootReadyConfirmation Telegram
   15. If received: ✅ deploy success
   16. If timeout: auto-rollback to previous binary; Critical Telegram
```

### What operator NEVER needs to do

- Install AWS CLI on Mac
- Install Terraform on Mac
- SSH into AWS instance (use SSM Session Manager instead — accessed via browser or `aws ssm start-session` if installed)
- Manually start/stop EC2 (EventBridge cron does it)
- Manually run cron jobs (CloudWatch alarms fire automatically)

### What operator DOES on Mac

- Edit code in IntelliJ
- Run `cargo test` to validate locally
- `git push` to deploy
- Read Telegram for daily proof-of-life

**Total Mac toolchain: IntelliJ + Rust + Docker Desktop + Git. Nothing else.**

---

## §8. Quick-wins to do TODAY (5 actions, all <30 min total)

After AWS account is created + before bootstrap:

| # | Action | Time | Cost |
|---|---|---|---|
| 1 | Enable MFA on AWS root user | 5 min | Free |
| 2 | Create AWS billing alarm at ₹1,500/mo | 5 min | Free |
| 3 | Enable GitHub Dependabot on `SJParthi/tickvault` repo | 1 click | Free |
| 4 | Enable AWS Compute Optimizer | 1 click | Free |
| 5 | Subscribe to AWS Health Dashboard email + RSS feed | 5 min | Free |

---

## §9. The 4 daily Telegrams (proof of life — operator scrolls phone)

| IST | Telegram | What it proves | If MISSING |
|---|---|---|---|
| 08:03 | ✅ `tickvault started \| boot_id: X` | Boot complete | Investigate immediately; Phone Call already fired |
| 09:15:30 | ✅ `Streaming live \| WS:1/1 \| Order-WS:1/1 \| SIDs:4/4 \| Token:23h45m` | Market open, feed flowing | Cross-verify will catch any gap; ack alert |
| 15:31:30 | ✅ `Cross-match OK \| TFs:4 \| Candles:N \| All OHLCV exact` | Same-day data integrity proven | CRITICAL — strategy data suspect; investigate |
| 15:45 | EOD digest: P&L + order count + alarms summary | Day complete | Means EOD task didn't run; manual review |

**Total operator daily effort: ~30 seconds scrolling Telegram.**

---

## §10. Honest envelope (operator-charter §F mandatory wording)

> "100% inside the LOCKED envelope:
> - 4 SIDs (NIFTY/BANKNIFTY/SENSEX/INDIA VIX) main feed
> - 3 underlyings option chain
> - 21 timeframes IST wall-clock
> - 30 days rolling RAM history
> - 100K rescue ring + REST backfill on detected gap
> - REAL reconnect (force-close TCP, re-auth, re-subscribe, 5s tick verify)
> - 15:31 IST + 08:05 IST cross-verify zero-tolerance
> - 4-channel notification (SMS + Telegram + Email + Phone Call) for Critical
> - CloudWatch single-sink observability
> - t4g.medium ARM ap-south-1 Default tenancy on-demand
> - Schedule 08:00–17:00 IST every day Mon–Sun
> - ~₹1,037/mo
>
> Every claim above is pinned by a ratchet test that fails the build on regression.
>
> Beyond envelope: AWS region outage > 60s, simultaneous failure of all 4 SNS legs, operator phone destroyed, Dhan API extended outage > 24h. These are documented as honest limitations — not promised."

---

## §11. Dhan API v2.5 Compliance Requirements — LOCKED 2026-05-18

Per operator demand 2026-05-18 ("add all these recommendations into the plans docs architecture requirements"), the full Dhan API requirements that the locked design MUST honor:

### §11.1 Authentication compliance (Dhan v2.4 SEBI mandate)

| Requirement | Source | Our compliance |
|---|---|---|
| 24-hour Access Token max validity | v2.4 SEBI mandate Sep 2025 | `token_manager.rs` 23h pre-emptive refresh; never holds token > 23h |
| Static IP mandatory for Order APIs (effective April 1, 2026) | v2.4 | Boot Step 5.5 calls `GET /v2/ip/getIP`; gates startup on `ordersAllowed=true`; HALT + Critical Telegram if false |
| TOTP RFC 6238 6-digit, 30-second window | v2.4 | `totp-rs` crate; secret in AWS SSM SecureString `/tickvault/prod/dhan_totp_secret` |
| `access-token` header (lowercase, hyphenated) for ALL REST | v2.0 | Locked in `oms/api_client.rs` headers |
| `client-id` header REQUIRED in addition to `access-token` for Option Chain + Market Quote APIs | v2.1+ | Locked in `option_chain/` module — both headers always sent |
| Token NEVER in URL query params, NEVER in logs, NEVER in QuestDB | charter §A | `Secret<String>` wrapper; tracing fields redact |
| RenewToken extends 24h to another 24h | v2.0 | `GET /v2/RenewToken` called 1h before expiry as backup; primary path = full TOTP regen daily |

### §11.2 Rate limit compliance (Dhan published limits)

| API category | Per-sec | Per-day | Our rate at 4-SID scope | Headroom |
|---|---|---|---|---|
| Data APIs (option chain, intraday, historical) | 5 | 100,000 | ~1,455/day | 98.5% headroom |
| Quote APIs (`/v2/marketfeed/ltp` etc) | 1 | unlimited | 0 (not used) | N/A |
| Order APIs (Phase 1.5+) | 10 | 7,000 | ~50-100 orders/day expected | 98% headroom |
| Non-Trading APIs (profile, fund limit, ip) | 20 | unlimited | ~10/day | 99% headroom |
| Order modifications (per order) | — | 25 | TBD per strategy | N/A |

**Rate limiter:** GCRA algorithm via `governor` crate. Dual limits (per-sec burst + per-day cumulative). Enforced in `oms/rate_limiter.rs` when Phase 1.5 wiring lands.

### §11.3 Error handling compliance (Dhan v2.0 DH-900 series)

Every Dhan API error has 3 string fields: `errorType`, `errorCode`, `errorMessage`. Mapped to `ErrorCode` enum:

| Dhan code | Action | Severity |
|---|---|---|
| DH-901 (Invalid auth) | Force token refresh → retry ONCE → HALT if still fails | Critical |
| DH-902 (No API access) | HALT + Telegram | Critical |
| DH-903 (Account issues) | HALT + Telegram | Critical |
| DH-904 (Rate limit) | Exponential backoff 10→20→40→80s; if 80s exhausted → CRITICAL | High |
| DH-905 (Input exception) | NEVER retry; log + fix request | Medium |
| DH-906 (Order error) | NEVER retry; log + fix order | Medium |
| DH-907 (Data error) | Check params, no blind retry | Medium |
| DH-908 (Internal server) | Retry with backoff (rare) | High |
| DH-909 (Network) | Retry with backoff | High |
| DH-910 (Other) | Log + alert | Medium |
| Data 805 (Too many connections) | STOP ALL connections 60s, then reconnect one at a time | Critical |
| Data 807 (Token expired) | Trigger token refresh + retry | High |

All locked in `crates/common/src/error_code.rs` per `100-percent-compliance-audit.md` cross-ref test.

### §11.4 Endpoint URLs — exact strings (no typos)

| Purpose | URL |
|---|---|
| Generate token | `https://auth.dhan.co/app/generateAccessToken?dhanClientId={ID}&pin={PIN}&totp={TOTP}` |
| Renew token | `https://api.dhan.co/v2/RenewToken` |
| User profile | `https://api.dhan.co/v2/profile` |
| Static IP set/modify/get | `https://api.dhan.co/v2/ip/setIP` / `/modifyIP` / `/getIP` |
| Main feed WS | `wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2` |
| Order update WS | `wss://api-order-update.dhan.co` (auth via JSON MsgCode 42 SELF) |
| Option chain | `https://api.dhan.co/v2/optionchain` (POST, body has `UnderlyingScrip`/`UnderlyingSeg`/`Expiry`) |
| Option chain expiry list | `https://api.dhan.co/v2/optionchain/expirylist` |
| Intraday historical | `https://api.dhan.co/v2/charts/intraday` (interval "1"/"5"/"15"/"25"/"60") |
| Daily historical | `https://api.dhan.co/v2/charts/historical` (supports `oi` param) |
| Orders (Phase 1.5+) | `https://api.dhan.co/v2/orders` (POST/GET) `/orders/{id}` (PUT/DELETE/GET) |
| Positions (Phase 1.5+) | `https://api.dhan.co/v2/positions` (GET/DELETE) |
| Margin calc (Phase 1.5+) | `https://api.dhan.co/v2/margincalculator` (POST) |
| Fund limit (Phase 1.5+) | `https://api.dhan.co/v2/fundlimit` (GET) |
| Kill switch (Phase 1.5+) | `https://api.dhan.co/v2/killswitch` (POST/GET) |

### §11.5 Field name quirks — locked in serde

| Quirk | Dhan field | Our code |
|---|---|---|
| Fund limit typo (must keep!) | `availabelBalance` (missing 'l') | `#[serde(rename = "availabelBalance")]` |
| Option Chain JSON keys PascalCase | `UnderlyingScrip`, `UnderlyingSeg`, `Expiry` | `#[serde(rename_all = "PascalCase")]` |
| Order Update WS PascalCase top-level | `Data`, `Type` | Same |
| Order Update WS single-char codes | `Product: "C"=CNC, "I"=INTRADAY, "M"=MARGIN, "F"=MTF, "V"=CO, "B"=BO` | Mapped enum |
| Order Update WS `TxnType` | `"B"=Buy, "S"=Sell` | Mapped enum |
| Order Update WS `Source` | `"P"=API, "N"=Normal (Dhan web)` | Filter own orders by `Source="P"` |
| Postback `filled_qty` snake_case | inconsistent with rest | Exact field name preserved |
| `last_trade_time` is STRING not epoch | `"DD/MM/YYYY HH:MM:SS"` | Parse as string |
| Option chain strike keys are DECIMAL STRINGS | `"25650.000000"` | Parse to f64 |
| CE or PE may be `None` (deep OTM) | sparse map | `Option<OptionData>` |
| `crossCurrency` boolean | currency F&O only | `Option<bool>` with `#[serde(default)]` |

### §11.6 Annexure enums — exact numeric codes

| Enum | Values | Our location |
|---|---|---|
| ExchangeSegment | IDX_I=0, NSE_EQ=1, NSE_FNO=2, NSE_CURRENCY=3, BSE_EQ=4, MCX_COMM=5, **gap at 6**, BSE_CURRENCY=7, BSE_FNO=8 | `common/src/types.rs::ExchangeSegment` — at locked scope only IDX_I (0) active |
| ProductType | CNC, INTRADAY, MARGIN, MTF, CO, BO | `common/src/order_types.rs` |
| OrderStatus | TRANSIT, PENDING, CLOSED, TRIGGERED, REJECTED, CANCELLED, PART_TRADED, TRADED, EXPIRED | `common/src/order_types.rs` |
| FeedRequestCode | 11=Connect, 12=Disconnect, 15/17/21=Subscribe Ticker/Quote/Full, 16/18/22=Unsubscribe, 23=Subscribe Depth, **25=Unsubscribe Depth (NOT 24)** | `common/src/types.rs` |
| FeedResponseCode | 1=Index, 2=Ticker, 4=Quote, 5=OI, ~~6=PrevClose~~ DROPPED, 7=MarketStatus, 8=Full, 50=Disconnect | code 6 parser deleted per operator insight |
| InstrumentType | INDEX, FUTIDX, OPTIDX, EQUITY, FUTSTK, OPTSTK, FUTCOM, OPTFUT, FUTCUR, OPTCUR | `common/src/instrument_types.rs` |
| ExpiryCode | 0=Current/Near, 1=Next, 2=Far | `common/src/instrument_types.rs` |

### §11.7 Mandatory manual steps (Claude/Cowork CANNOT automate)

| Step | Why manual | When |
|---|---|---|
| AWS account creation (email + payment card) | AWS legal req; no API for individual signup | Once |
| Register EIP with Dhan via web.dhan.co | Dhan requires logged-in web session; no API for IP registration | Once + after any EIP change (7-day cooldown) |
| DLT-SMS sender ID registration (TRAI mandate for India SMS) | Carrier-side identity verification | Once, ~24-48h processing |
| Dhan TOTP setup via web.dhan.co (scan QR via Authenticator app) | Identity verification with OTP via email/mobile | Once at Dhan account setup |
| Telegram bot creation via @BotFather (operator owns token) | Telegram requires interactive bot creation | Once |
| Approve Cowork bootstrap prompt + provide secrets when asked | Operator must authorize | Once at bootstrap |

### §11.8 Phase 1.5 wiring blockers (Order APIs designed, not wired)

Per `gap-inventory-2026-05-18.md` §1 — the 11-item Phase 1.5 wiring milestone:

| # | Wire-up | Dhan API touched |
|---|---|---|
| 1 | Strategy `Signal` → Risk pre-check → OMS order construction | (internal) |
| 2 | OMS state machine 10/26 transitions implemented | (internal) |
| 3 | Order update WS events → strategy notification | `wss://api-order-update.dhan.co` |
| 4 | Position tracker hooked to order_audit | `GET /v2/positions` |
| 5 | Kill switch state shared Risk + OMS + Strategy | `POST /v2/killswitch`, `GET /v2/killswitch` |
| 6 | Daily loss aggregator | (internal, uses trade book) |
| 7 | Idempotency UUID per strategy decision | `correlationId` field in `POST /v2/orders` |
| 8 | Rate limiter wired to OMS API client | All Order APIs |
| 9 | Circuit breaker on OMS failures | DH-904/908/909 detection |
| 10 | Reconciliation against Dhan position book daily | `GET /v2/positions` + `GET /v2/trades` |
| 11 | EOD square-off path (when `dry_run=false`) | `DELETE /v2/positions` (Exit All v2.5) |

**Until Phase 1.5 wiring lands, tickvault runs in OBSERVATION mode** (subscribed to ticks + option chain + cross-verify daily; no orders placed).

### §11.9 v2.5 OPTIONAL features — deferred (operator decides if/when)

| Feature | Operator decision needed |
|---|---|
| Conditional Trigger Orders (alert-based order placement) | Useful if strategies use "RSI crosses 70 → place order"; defer until strategy design |
| P&L based exit (server-side auto-square-off at threshold) | Alternative to client-side risk engine; defer (we prefer client-side control) |
| Exit All API (`DELETE /v2/positions`) | 🟡 Phase 1.5 USE — EOD + emergency square-off |
| Programmatic token generation via TOTP | 🟢 Already locked in our design |
| Option Chain `average_price` + `security_id` per strike (v2.5) | 🟢 Already in our `option_chain_snapshots` schema |

### §11.10 APIs we explicitly DO NOT use (and why)

| API | Why not |
|---|---|
| `POST /v2/super/orders` | Phase 1.5 strategy may use; defer — we'll likely use plain orders + client-side risk |
| `POST /v2/forever/orders` (GTT) | We don't park overnight orders |
| `POST /v2/positions/convert` (intraday ↔ CNC) | We don't convert; orders stay in intraday |
| `GET /v2/holdings` | We trade options, not equity holdings |
| `GET /v2/edis/tpin` + `POST /v2/edis/form` | EDIS only needed for selling demat stocks; we don't hold |
| `GET /v2/ledger`, `GET /v2/trades/{from}/{to}/{page}` | Operator pulls from Dhan web/email if needed for tax filing |
| Partner authentication 3-step flow | We are individual trader, not partner platform |
| BSE F&O / NSE_CURRENCY / MCX_COMM / BSE_CURRENCY subscriptions | Out of locked indices-only scope |
| `POST /v2/marketfeed/ltp` / `/ohlc` / `/quote` REST | WS Live Market Feed already gives us LTP/OHLC; REST is wasteful at 4 SIDs |
| Postback URL (HTTP webhook for order updates) | We use Order Update WS — postback is backup we don't need |
| 20-level depth WS | Depth dropped per LOCK-I |
| 200-level depth WS | Same |

### §11.11 Ratchet tests pinning Dhan compliance

| Test file | Ratchets |
|---|---|
| `crates/common/tests/error_code_rule_file_crossref.rs` | Every DH-901..910 + DATA-800..814 has rule-file mention |
| `crates/common/tests/error_code_tag_guard.rs` | Every `error!` carries `code = ErrorCode::DH901.code_str()` field |
| `crates/core/tests/ws_protocol_e2e.rs` | Binary protocol byte offsets match Dhan spec (8-byte main feed header, 16-byte ticker, 50-byte quote, 162-byte full) |
| `crates/core/src/parser/types.rs::test_exchange_segment_gap_at_6` | Verifies enum NEVER assigns 6 |
| `crates/core/src/option_chain/test_client_id_header_required` | Verifies both `access-token` + `client-id` headers always sent |
| `crates/storage/tests/dedup_segment_meta_guard.rs` | Every DEDUP key includes `exchange_segment` per I-P1-11 |

### §11.12 Honest envelope on Dhan compliance

> "100% inside the Dhan v2.5 specification: we use 10 APIs daily, 11 more at Phase 1.5, skip ~30 out-of-scope APIs. Every endpoint URL exact-stringed. Every JSON field quirk (typos, PascalCase, single-char codes) preserved in serde. Every rate limit honored with GCRA + headroom. Every error code (DH-901..910 + DATA-800..814) mapped to typed handler. Every annexure enum (ExchangeSegment with gap at 6, FeedRequestCode with 25-not-24 for unsubscribe depth) locked in code with ratchet tests.
>
> Beyond envelope: Dhan ships new API endpoints in future v2.6+ — operator decides which to adopt; documented adapter pattern in `dhan-api-coverage-map.md` makes adding new endpoints a 1-PR change."

---

## §12. Trigger / auto-load

Always loaded — this is the canonical plan. Activates when editing:
- Any file under `crates/`
- `deploy/`
- `.github/workflows/`
- `.claude/plans/aws-lifecycle/`
- `config/base.toml`
