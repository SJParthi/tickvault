# PHASE 1 — RESILIENCE + HONEST ENVELOPE LOCKED (2026-05-13)

> **Status:** LOCKED. Companion to `topic-PHASE-0-LEAN-LOCKED.md`.
> **Author:** Parthiban (operator decision after disconnect analysis 2026-05-13)
> **Scope:** What we CAN guarantee, what we CANNOT, and the Z+ 7-layer defense for every disconnect class.
> **Build day:** Friday 2026-05-15 (alongside Phase 0)
> **Live monitoring start:** Mon 2026-05-19 (22 trading days dry_run)
>
> **THE FOUNDATIONAL CONTRACT:** This document defines the **honest envelope** for tickvault. Every "100% guarantee" claim must reference this file. Anything stronger = REJECT IN REVIEW.

---

## Why Phase 1 exists (the foundational truth)

The operator's charter demands "100% guarantee, zero ticks lost, WebSocket never disconnects."

**Some of these are physically impossible to literally deliver.** SEBI law mandates 24h JWT rotation → WebSocket MUST reconnect at least once per day. Dhan's edge servers do scheduled load-balancer cycles → RST storms WILL happen. AWS networking has rare AZ blips. TCP keepalive timers will close zombie sockets.

**Phase 1 is the engineering response: define the envelope honestly, build mechanical defenses around it, ratchet everything, never lie.**

---

## The 6 disconnect classes (cannot be prevented)

| # | Class | Frequency expected | Can stop? | Why |
|---|---|---|---|---|
| 1 | **SEBI 24h JWT expiry** | **1/day MINIMUM, BY LAW** | ❌ NEVER | Regulatory mandate — token MUST rotate every 24h |
| 2 | **Dhan-side RST storm** | 1-3/day on AWS+EIP | ❌ Server-side | Dhan internal timers / load-balancer cycles |
| 3 | **AWS network/AZ blip** | ~1/month | ❌ Datacenter event | EBS micro-pauses, EC2 maintenance, AZ degradation |
| 4 | **OS TCP keepalive death** | If silent >7200s | ❌ Kernel-level | Linux closes zombie sockets after default keepalive timeout |
| 5 | **Daily 17:30 IST shutdown** | 1/day | ❌ Cost-saving design | EC2 auto-stop after market close (saves ₹2,800/mo) |
| 6 | **Code/OS patches** | ~1/week | ❌ Operational reality | Security updates, app upgrades |

**Expected disconnects per session in Phase 0 + AWS:** **2-5/day.** Each one bounded ≤2s with hardening.

---

## What we CAN guarantee (the honest envelope)

| Guarantee | Inside envelope | Mechanical proof |
|---|---|---|
| **Reconnect speed** | ≤2s with `SubscribeRxGuard` preserving subscriptions | `test_subscribe_rx_guard_reinstalls_on_drop` |
| **Detection speed** | ≤15s for IDX_I (NIFTY/BANKNIFTY/SENSEX/VIX), ≤30s for stocks | Activity watchdog config |
| **Gap-fill** | REST `/marketfeed/ltp` per-minute for 222 SIDs (~11s) covers WS outages ≤5 min | `rest_gap_fill.rs` (Friday build) |
| **Rescue ring** | 5,000,000-tick ring absorbs QuestDB outages ≤60s | `TICK_BUFFER_CAPACITY` constant, ratcheted by `zero_tick_loss_alert_guard.rs` |
| **Cross-verify** | Daily 15:31 IST exact-match vs Dhan historical for 1m candles | `cross_verify.rs` + `CROSS_MATCH_PRICE_EPSILON = 0.0` |
| **Uniqueness** | Composite key `(security_id, exchange_segment)` everywhere | `security-id-uniqueness.md` + 7-layer ratchet |
| **O(1) latency** | `from_le_bytes` + `papaya` + `arc-swap` + SPSC bounded | DHAT zero-alloc + Criterion ≤100ns |
| **No data loss inside envelope** | 3-tier rescue→spill→DLQ; DLQ NDJSON catches every payload as recoverable text | `chaos_zero_tick_loss.rs` |

---

## What we CANNOT guarantee (the truth, no hallucination)

| Operator's literal demand | Honest answer | Why physical impossibility |
|---|---|---|
| "WebSocket never disconnects" | ❌ **FALSE** | SEBI 24h JWT rotation by law + Dhan-side RST storms server-side |
| "Zero ticks lost, no envelope" | ❌ **FALSE** | Sub-second gaps during ≤2s reconnect cannot be filled (REST rate limit 1/sec) |
| "QuestDB never fails" | ❌ **FALSE** | Third-party process — OOM, disk full, network partition can happen |
| "Never slow/locked/hanged" | ⚠️ **TRUE under normal load only** | t3.medium CPU credit starvation under abnormal load could happen |
| "100% never any failure" | ❌ **FALSE** | Physical impossibility. Charter §F itself says envelope-qualified |

**The lies I refuse to tell** (per operator-charter-forever §F):

1. ❌ "WebSocket never disconnects"
2. ❌ "Zero ticks lost forever"
3. ❌ "QuestDB never fails"
4. ❌ Any "100% guarantee" without envelope qualifier

---

## Z+ 7-Layer Defense Matrix (per disconnect class)

### Class 1: SEBI 24h JWT expiry

| Layer | Mechanism | Test |
|---|---|---|
| L1 DETECT | Token-age guard, warns at 4h before expiry | `test_token_force_renewal_when_stale` |
| L2 VERIFY | RenewToken API ACK + new JWT validation | `test_renew_token_response_parse` |
| L3 RECONCILE | Daily `auth_audit` table — verify exactly 1 rotation per session | `auth_audit_persistence.rs` |
| L4 PREVENT | Pre-emptive renewal at 23h mark, before 24h hard expiry | `token_manager::force_renewal_if_stale` |
| L5 AUDIT | `auth_audit` row per renewal: timestamp, old_expiry, new_expiry | DEDUP UPSERT KEYS |
| L6 RECOVER | Force renewal → WS reconnect with fresh JWT | AUTH-GAP-03 runbook |
| L7 COOLDOWN | 30s grace after rotation before counting next disconnect | `WS_POST_RECONNECT_GRACE_SECS` |

### Class 2: Dhan-side RST storm

| Layer | Mechanism | Test |
|---|---|---|
| L1 DETECT | Activity watchdog 15s (IDX_I) / 30s (stocks) | `test_activity_watchdog_fires_on_silence` |
| L2 VERIFY | Subscribe-ACK round-trip — require frame in 5s after subscribe | `test_subscribe_ack_required` |
| L3 RECONCILE | Cross-verify 1m candles at 15:31 IST vs Dhan historical (exact match) | `cross_verify.rs` ZERO_TOLERANCE |
| L4 PREVENT | Static IP whitelist (Dhan trust), keepalive tuned to 20s | EC2 EIP + `tcp_keepalive_intvl=20` |
| L5 AUDIT | `ws_reconnect_audit` table — every reconnect logged | DEDUP `(ts, conn_id)` |
| L6 RECOVER | Instant retry once → exponential backoff (1s→2s→4s→8s) → SubscribeRxGuard restores subs | `connection.rs::run_read_loop` |
| L7 COOLDOWN | 30s grace before counting next disconnect (prevents flapping) | `WS_POST_RECONNECT_GRACE_SECS` |

### Class 3: AWS network/AZ blip

| Layer | Mechanism | Test |
|---|---|---|
| L1 DETECT | TCP RTO ~1s + watchdog 15s | OS-level |
| L2 VERIFY | Reconnect attempt with retry | Existing code |
| L3 RECONCILE | REST gap-fill if outage >60s | `rest_gap_fill.rs` |
| L4 PREVENT | Multi-AZ EBS, EC2 instance health monitor | AWS-managed |
| L5 AUDIT | `ws_reconnect_audit` with reason="aws_network" | tag classification |
| L6 RECOVER | Auto reconnect with exponential backoff | `connection.rs` |
| L7 COOLDOWN | 30s grace | `WS_POST_RECONNECT_GRACE_SECS` |

### Class 4: OS TCP keepalive death (zombie socket)

| Layer | Mechanism | Test |
|---|---|---|
| L1 DETECT | Activity watchdog 15s catches BEFORE kernel 7200s | `activity_watchdog.rs` |
| L2 VERIFY | Reconnect attempt | Existing |
| L3 RECONCILE | REST gap-fill | `rest_gap_fill.rs` |
| L4 PREVENT | Tune kernel: `tcp_keepalive_time=60`, `tcp_keepalive_intvl=20`, `tcp_keepalive_probes=3` | AWS user-data script |
| L5 AUDIT | `ws_reconnect_audit` with reason="zombie_socket" | tag |
| L6 RECOVER | Watchdog forces close + reconnect | `activity_watchdog::fire` |
| L7 COOLDOWN | 30s grace | `WS_POST_RECONNECT_GRACE_SECS` |

### Class 5: Daily 17:30 IST shutdown

| Layer | Mechanism | Test |
|---|---|---|
| L1 DETECT | Scheduled (EventBridge cron) | AWS-managed |
| L2 VERIFY | Pre-shutdown drain — flush rescue ring, close QuestDB cleanly | Graceful shutdown hook |
| L3 RECONCILE | Post-market cross-verify completes before shutdown (15:31 → 17:25 window) | Boot order check |
| L4 PREVENT | Cost-saving design — accept the ~16h gap | Operator-approved |
| L5 AUDIT | `boot_audit` table — boot ts + previous shutdown ts | DEDUP `(boot_ts)` |
| L6 RECOVER | Next-day 08:30 IST auto-boot, full pipeline restart | EventBridge cron |
| L7 COOLDOWN | n/a — daily expected event | n/a |

### Class 6: Code/OS patches

| Layer | Mechanism | Test |
|---|---|---|
| L1 DETECT | App lifecycle SIGTERM hook | Graceful shutdown |
| L2 VERIFY | Post-restart subscribe ACK on all 222 SIDs | `test_subscribe_ack_required` |
| L3 RECONCILE | **Indicator state replay from QuestDB** (recovers RSI/MACD running values) | NEW: `boot_replay.rs` (Phase 1 add) |
| L4 PREVENT | Stagger patches off-market-hours (post-15:30 IST) | Deployment policy |
| L5 AUDIT | `boot_audit` row with reason="patch" | tag |
| L6 RECOVER | systemd auto-restart on crash | `Restart=always` |
| L7 COOLDOWN | 60s warmup before strategy evaluator engages | `STRATEGY_BOOT_WARMUP_SECS` |

---

## The 15-row 100% Coverage Matrix (qualified honestly)

| Dimension | 100% inside envelope? | Beyond envelope? | Mechanical proof |
|---|---|---|---|
| Code coverage | ✅ Per-crate threshold in `quality/crate-coverage-thresholds.toml` | llvm-cov post-merge | `scripts/coverage-gate.sh` |
| Audit coverage | ✅ 15 audit tables + DEDUP UPSERT KEYS | Daily reconcile | `dedup_segment_meta_guard.rs` |
| Testing coverage | ✅ 22 test categories per `testing.md` | Mutation/fuzz weekly | `cargo test --workspace` |
| Code checks | ✅ banned-pattern + 8 pre-commit gates | Adversarial 3-agent review | `banned-pattern-scanner.sh` |
| Code performance | ✅ DHAT zero-alloc + Criterion p99 ≤100ns | bench-gate 5% regression | `scripts/bench-gate.sh` |
| Monitoring | ✅ 7-layer telemetry (audit + Telegram + QuestDB UI) | Phase 2 adds Grafana | `mcp__tickvault-logs__*` |
| Logging | ✅ tracing macros, hourly errors.jsonl, 48h retention | Loki post-Phase-2 | `error_code_tag_guard.rs` |
| Alerting | ✅ Telegram via teloxide direct from app | AWS SNS for Critical | `resilience_sla_alert_guard.rs` |
| Security | ✅ secret-scan + `Secret<T>` + static IP enforcement | cargo audit post-deploy | `banned-pattern-scanner.sh` |
| Security hardening | ✅ static IP, no .env files, `unused_must_use` lint | Post-deploy IP verify | clippy lints |
| Bugs fixing | ✅ Adversarial 3-agent review per PR (proven 4-bug catch rate) | Mutation testing | `verify-build` agent |
| Scenarios covering | ✅ Phase 1 = ~80 paths defended | Chaos tests | `chaos_*.rs` (16 today) |
| Functionalities | ✅ pub-fn-test + pub-fn-wiring guards | Every fn has test + call site | pre-push gates 6 + 11 |
| Code review | ✅ Adversarial 3-agent BEFORE + AFTER impl | Per-PR | hot-path + security + hostile agents |
| Extreme check | ✅ All ratchet tests fail build on regression | Mechanical, not aspirational | `cargo test` green |

---

## The 7-row Resilience Demand Matrix (qualified honestly)

| Demand (operator verbatim) | Honest envelope | What it means |
|---|---|---|
| **"Zero ticks lost"** | Bounded zero loss in envelope: rescue ring 5M → spill NDJSON → DLQ NDJSON | Inside ≤60s QuestDB outage + ≤5min WS outage = ZERO. Beyond = DLQ recoverable |
| **"WS never disconnects"** | **SEBI 24h JWT forces ≥1 reconnect/day BY LAW** | Cannot promise zero. Honest: ≤2s reconnect, sub-preserved, gap-filled |
| **"Never slow/locked/hanged"** | DHAT ≤4 alloc blocks/8KB across 10K calls; Criterion p99 ≤100ns | Bench-gated. Holds for 222 SIDs ticker mode |
| **"QuestDB never fails"** | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal | Cannot promise zero failure. Honest: 60s envelope, no data loss |
| **"O(1) latency"** | `from_le_bytes` + `papaya` + `Arc<HashMap>` + SPSC bounded | Bench-gate ≤5% regression on hot path |
| **"Uniqueness + dedup"** | Composite `(security_id, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS | Ratcheted by `dedup_segment_meta_guard.rs` |
| **"Real-time proof"** | 7-layer telemetry + SLO score every 10s + market-open self-test 09:16:30 IST | Mechanical, not aspirational |

---

## Session Automation Chain (always-on, per charter)

Every Claude Code / Cowork session inherits via `.mcp.json` + SessionStart hooks:

| Tool | What it does | Trigger |
|---|---|---|
| `mcp__tickvault-logs__run_doctor` | 7-section health snapshot | Session start |
| `mcp__tickvault-logs__summary_snapshot` | Last-hour ERROR signatures | Session start |
| `mcp__tickvault-logs__list_active_alerts` | Currently firing alerts | Session start |
| `mcp__tickvault-logs__tail_errors` | Recent ERROR events | On demand |
| `mcp__tickvault-logs__questdb_sql` | Run SQL on live DB | On demand |
| `mcp__tickvault-logs__app_log_tail` | Tail live app log | On demand |
| `mcp__tickvault-logs__find_runbook_for_code` | Map ErrorCode → runbook | On demand |
| `mcp__tickvault-logs__list_novel_signatures` | New error patterns | On demand |
| `mcp__tickvault-logs__signature_history` | Pattern frequency over time | On demand |
| `mcp__tickvault-logs__grep_codebase` | Source search | On demand |
| `session-auto-health.sh` | Doctor + validate-automation background | Session start |
| `session-context-brief.sh` | Active plans + open PRs | Session start |
| `session-sanity.sh` | Branch + uncommitted check | Session start |

**Local Mac dev + AWS prod = SAME tools, SAME MCP, SAME data paths.** Common runtime, dynamic, scalable, automated. Per charter §A.

---

## Monitoring Chain (5 sinks for every ERROR)

```
error!() → tracing::error → 5 sinks fanout
   ↓
   ├─ stdout / journald (host)
   ├─ data/logs/app.YYYY-MM-DD.log (daily rotation)
   ├─ data/logs/errors.log (WARN+ single file)
   ├─ data/logs/errors.jsonl.YYYY-MM-DD-HH (ERROR JSONL, 48h retention)
   └─ Telegram + AWS SNS SMS (operator notification)
                ↓
                Claude auto-triage (.claude/triage/error-rules.yaml)
                   ↓
                   known + safe → auto-fix script
                   known + Critical → escalate (Telegram + SMS + GitHub Issue)
                   novel signature → operator alert
```

**Result:** every error visible in ≥3 places. Operator cannot miss.

---

## The 53 ErrorCode Variants (100% rule-synced)

Every tracked error has:
- `code_str()` stable wire format
- `severity()`: Info < Low < Medium < High < Critical
- `runbook_path()`: file in `.claude/rules/`
- `is_auto_triage_safe()`: never true for Critical
- Cross-ref test enforces: every variant mentioned in ≥1 rule file
- Tag-guard meta-test: every `error!` carries `code = ErrorCode::X.code_str()`

**21 mechanical ratchets** (per `observability-architecture.md`) fail the build on regression.

---

## The 5 hardening changes (Friday build, universal)

| # | Change | Where | Why it matters |
|---|---|---|---|
| 1 | **Activity watchdog tighter for IDX_I** (15s) vs stocks (30s) | `activity_watchdog.rs` | Catches zombie socket on critical SIDs faster |
| 2 | **Subscribe-ACK verification** — require frame in 5s after subscribe | `connection.rs::run_read_loop` | Detects "connected but no data" — the 2026-05-12 bug |
| 3 | **REST `/marketfeed/ltp` gap-fill module** — per-minute during WS outage | new `rest_gap_fill.rs` | Fills outages up to 5 min — covers the 276s blackout |
| 4 | **Stagger initial conn 2s apart** | `connection_pool.rs` | N/A in Phase 0 (1 conn only) but ratchet retained |
| 5 | **Indicator state replay from QuestDB on boot** | new `boot_replay.rs` | Mid-day restart recovers RSI/MACD state in <60s |

**Total: ~620 LoC for Phase 1 hardening.**

---

## Mechanical Enforcement Chain (every commit)

| Gate | Catches |
|---|---|
| `pre-commit-fast-gate.sh` | fmt + banned-pattern + secret-scan + version pin + 9 invariants |
| `pre-push-gate.sh` | 12 fast gates ~35s total |
| `per-item-guarantee-check.sh` | 15-row + 7-row matrix presence in plan |
| `banned-pattern-scanner.sh` | hot-path violations |
| `pub-fn-test-guard.sh` | every pub fn has test or TEST-EXEMPT comment |
| `pub-fn-wiring-guard.sh` | every pub fn has call site |
| `plan-verify.sh` | unchecked plan items |
| `error_code_tag_guard.rs` | every error has `code =` field |
| `error_code_rule_file_crossref.rs` | every ErrorCode has rule-file mention |
| `dedup_segment_meta_guard.rs` | DEDUP keys include segment |
| `operator_health_dashboard_guard.rs` | counters have Grafana panels (Phase 2) |
| `resilience_sla_alert_guard.rs` | counters have alert rules |
| `error_level_meta_guard.rs` | flush/persist uses `error!` not `warn!` |
| `scripts/bench-gate.sh` | 5% regression budget |
| `scripts/coverage-gate.sh` | 100% line coverage per crate |
| GitHub Actions CI | full 22-test battery + mutation + fuzz |

**Anything that needs to be true is machine-checked. Hand-wave = REJECT.**

---

## Auto-driver / Insta-reel Explanation

> "Sir, ek train station chala rahe hain. **Train kabhi-kabhi 30 second late hoti hai** — hum train ko on-time chalna mandatory nahi kar sakte, train Dhan banata hai, hum nahi.
>
> **Lekin ye 6 cheezein hum mechanically guarantee kar sakte hain:**
>
> 1. Train late hone par **2 second ke andar pata chal jayega** (watchdog)
> 2. Passengers ke liye **ek bus REST se chala denge** beech ke time pe (gap-fill)
> 3. Late ki **diary me likh denge** har baar (audit table — 15 tables hain)
> 4. Aapko **Telegram pe message** jayega (alert via teloxide)
> 5. Train aate hi **passengers wahi seat pe wapas baith jayenge** (SubscribeRxGuard)
> 6. 1 ghante ke andar **panchayat ko sab gaadi ka record dikhayenge** (cross-verify at 15:31 IST)
>
> **Train kabhi disconnect nahi hogi — ye jhooth hai.** Lekin disconnect ke 30 second ke andar passenger ko bhi pata nahi chalega ki kuch hua tha. **Wo guarantee hai. Mechanical, not aspirational.** Har commit pe test fail ho jayega agar koi banda ye promise tod de."

---

## The Honest 100% Claim (mandatory PR-body wording)

When any PR / commit / Telegram / doc says "100% guarantee", it MUST be qualified exactly:

> **"100% inside the tested envelope, with ratcheted regression coverage:**
> - **≤2s WebSocket reconnect with `SubscribeRxGuard` preserving subscriptions**
> - **≤15s detection for IDX_I (NIFTY/BANKNIFTY/SENSEX/VIX), ≤30s for stocks**
> - **REST `/marketfeed/ltp` gap-fill covers WS outages up to 5 minutes for 222 SIDs**
> - **≤60s QuestDB outage absorbed by 5,000,000-tick rescue ring (constant `TICK_BUFFER_CAPACITY`, ratcheted by `zero_tick_loss_alert_guard.rs`)**
> - **Bench-gated O(1) hot path (Criterion p99 ≤100ns, 5% regression budget)**
> - **Composite-key uniqueness `(security_id, exchange_segment)` per I-P1-11**
> - **Daily 15:31 IST cross-verify vs Dhan historical, exact-match (zero tolerance)**
> - **Beyond the envelope: DLQ NDJSON catches every payload as recoverable text"**

**Promising literal "WebSocket never disconnects" or "QuestDB never fails" without envelope qualifier = REJECT IN REVIEW.**

---

## What Phase 1 dry_run (22 trading days) measures

| Metric | Target | What if not met |
|---|---|---|
| Disconnects per day | ≤5 (expected 2-3) | Escalate to Dhan support with audit data |
| Reconnect time p99 | ≤2s | Tune backoff schedule |
| Detection time (silence → alert) | ≤15s IDX_I, ≤30s stocks | Tighten watchdog |
| REST gap-fill success rate | ≥99% | Investigate Dhan REST throttling |
| Cross-verify exact-match rate | 100% on 1m candles | Investigate Dhan historical drift |
| Memory usage steady-state | ≤2.8 GB on t3.medium | Investigate leak |
| QuestDB ingestion lag | ≤100ms | Tune ILP buffer |
| Telegram alerts per day | ≤10 (operator-readable) | Improve coalescing |
| AWS instance crashes | 0 | Investigate root cause |
| Strategy signals logged | ≥5/day (dry_run, no orders) | Validate strategy logic |

**After 22 trading days:**

- **Green light** (all metrics met) → proceed to Phase 2A (1 strategy live, smallest lot)
- **Yellow light** (1-2 metrics yellow) → fix + extend dry_run 1 week
- **Red light** (>2 metrics red) → fix + restart 22-day window

---

## What Phase 1 does NOT do

- ❌ No real orders placed (dry_run = true everywhere)
- ❌ No depth-20 / depth-200 subscriptions
- ❌ No streaming Greeks
- ❌ No movers pipeline
- ❌ No dynamic selectors
- ❌ No multi-leg strategies
- ❌ No Grafana / Prometheus / Valkey (Phase 2+)
- ❌ No BSE F&O
- ❌ No promising "zero disconnect" or "100% no-envelope"

---

## Operator Sign-off

This file is locked when committed. It is the **foundational honest envelope** for tickvault. Every future Claude session must:

1. Read this file FIRST when promising any "100%" claim
2. Qualify every guarantee with the envelope wording above
3. Cite ratchet test names, not aspirations
4. Refuse to promise the 3 physical impossibilities (WS never disconnect, zero ticks lost forever, QuestDB never fails)

**Approved by Parthiban: 2026-05-13 (after live disconnect storm + honest envelope discussion).**
**Plan author: Claude (this session).**
**Branch: `claude/trading-tick-vault-BkvpS`.**

---

## Phased rollout (updated)

| Phase | When | Scope |
|---|---|---|
| **Phase 0 build** | Friday 2026-05-15 | Lean scope: 1 WS, 222 SIDs Ticker, 2 services. See `topic-PHASE-0-LEAN-LOCKED.md` |
| **Phase 1 hardening** | Friday 2026-05-15 (alongside) | This file: 5 hardening changes, Z+ 7-layer per disconnect class, mechanical ratchets |
| **Phase 1 validation** | Sat-Sun 2026-05-16/17 | Mac backtest sweep, dry_run smoke tests, CI green, all ratchets verified |
| **Phase 1 AWS deploy** | Mon 2026-05-18 | Provision t3.medium, EIP, EBS, deploy via docker-compose |
| **Phase 1 monitoring** | 22 trading days (Mon 2026-05-19 → Fri 2026-06-13) | Live ticks, dry_run=true, measure 10 metrics above |
| **Phase 1 decision gate** | 2026-06-16 | Green/Yellow/Red per metrics |
| **Phase 2A live trade** | 2 weeks (~10 trading days) | ONE strategy live, smallest lot, full audit |
| **Phase 2B expand** | TBD | Add depth/greeks/movers ONLY if data demands |

**Total: 6 weeks from now to first live trade.**
