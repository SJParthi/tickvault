# Step 1 — The Honest Guarantee Envelope (CANONICAL REFERENCE)

**Created:** 2026-05-11 21:10 IST (CLI continuation of web 400'd session)
**Status:** LOCKED — operator approved Mon 20:55–21:03 IST
**Authority:** `wave-4-shared-preamble.md` §8 + this file
**Used by:** any future Claude session opening cold MUST read this first to avoid re-debating closed items.

---

## Operator's verbatim charter (re-stated 2026-05-11 21:00 IST + 21:30 IST)

> "Zero ticks loss and nowhere websocket should get disconnected or reconnect, not even a single tick should be missed. Meanwhile our entire web sockets and connections should never ever become slow or locked or hanged or stuck. QuestDB should never ever fail or always disconnect. I need guarantee for both of these always.
>
> Always extreme comprehensive automation monitoring alerting notifying on telegram capturing tracking auditing debugging detailing visualizing checking everything.
>
> Always activate and use all the agents and all subagents as well to do a deep thorough research.
>
> Always it should be common runtime dynamic scalable incremental approach.
>
> Always ensure to maintain O(1) latency and uniqueness guarantee and deduplication.
>
> Real-time checks and guarantee and assurance and proof as well.
>
> 100 percentage [extensive, comprehensive, intensive, deep thorough code coverage, code scanning, code duplication, functionality missing, scenario missing, bug fixing, issues finding, testing types, testing coverage, extensive comprehensive real time testing, code performance, uniqueness, deduplication, O(1) latency always]
>
> Every new or existing Claude Code session or every new Claude Cowork task should automatically do everything as an automated process always. Access logs queries dbs project product everything entirely accessible as an automated process either in local or AWS as a common runtime dynamic scalable automated extreme automation approach."

This charter is the operator's CONSTANT demand across every session. Every plan item must satisfy it.

---

## The honest verdict (no hallucination)

**The literal claims are PHYSICALLY IMPOSSIBLE.** The honest envelope is "100% inside the tested envelope with ratcheted regression coverage." Why:

| Literal claim | Why impossible | Whose fault |
|---|---|---|
| WebSocket never disconnects | SEBI mandates JWT 24h validity → ≥1 reconnect/day by LAW | Government regulation |
| Zero ticks lost | TCP RST can happen on public internet | Physics + Dhan's network |
| QuestDB never fails | Remote process not in our control | Could be Docker, kernel OOM, disk-full |
| Never slow/locked | OS scheduler can preempt any thread | Linux kernel |

**Promising the literal = AUDIT TRAP.** SEBI audit will ask for proof; "we said never" without an envelope is a lie.

---

## The Charter vs Reality Table (locked answer)

| Operator demand (literal) | Why literal is impossible | What we DO mechanically guarantee | Where the proof lives | Ratchet test |
|---|---|---|---|---|
| **Never miss a single tick** | TCP RST + SEBI 24h JWT = ≥1 reconnect/day unavoidable | Bounded zero loss: 5M-tick rescue ring → NDJSON spill → DLQ NDJSON | `crates/common/src/constants.rs::TICK_BUFFER_CAPACITY` | `crates/storage/tests/zero_tick_loss_alert_guard.rs` |
| **WS never disconnects** | Dhan kicks socket on JWT renew (legal mandate) | Detect ≤5s + reconnect with `SubscribeRxGuard` + sleep-until-open post-close | `crates/core/src/websocket/connection.rs`, `connection_pool.rs` | `test_subscribe_rx_guard_reinstalls_on_drop` + `test_pool_watchdog_task_accepts_health_status` |
| **WS never slow/locked** | OS scheduler can preempt any thread | DHAT ≤4 alloc blocks / 8KB across 10K calls + Criterion p99 ≤100ns + Core 0 pinned via `core_affinity` | `quality/benchmark-budgets.toml` | `scripts/bench-gate.sh` 5% regression gate |
| **QuestDB never fails** | Remote process; not in my control | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal via `ALTER ADD COLUMN IF NOT EXISTS` | `crates/storage/src/instrument_persistence.rs::ensure_instrument_tables` | `crates/storage/tests/resilience_sla_alert_guard.rs` |
| **O(1) latency** | Hash collisions can degrade | `from_le_bytes` + `papaya` lock-free map + SPSC bounded ≤50ns Criterion budget | `crates/core/src/parser/*` | `scripts/bench-gate.sh` ≤5% regression |
| **Uniqueness + dedup** | `security_id` alone collides cross-segment (I-P1-11 production bug 2026-04-17) | Composite key `(security_id, exchange_segment)` everywhere + DEDUP UPSERT KEYS | `.claude/rules/project/security-id-uniqueness.md` | `crates/storage/tests/dedup_segment_meta_guard.rs` |
| **Real-time proof** | Without a positive signal, you only know via absence | 7-layer telemetry (Prom counter + gauge + tracing + Loki + Telegram + Grafana + audit table) + SLO score every 10s + market-open self-test at 09:16:30 IST | `.claude/rules/project/observability-architecture.md` | `error_code_tag_guard.rs` + `operator_health_dashboard_guard.rs` |

---

## The 5-Layer Defence Diagram (dumb-kid / auto-driver friendly)

```
                  💥 DISCONNECT / FAILURE HITS HERE
                          │
                          ▼
   ┌──────────────────────────────────────────────────────┐
   │ LAYER 1 ─── 🛡️ DETECT (≤5 sec)                       │
   │   Pool watchdog ticks every 5s, sees state ≠ Connected │
   │   Telegram: WebSocketDisconnected (in-market = HIGH)  │
   └──────────────────────────────────────────────────────┘
                          │
                          ▼
   ┌──────────────────────────────────────────────────────┐
   │ LAYER 2 ─── 🪣 BUFFER (5,000,000 ticks in RAM)        │
   │   Rescue ring keeps streaming ticks while we reconnect│
   │   ~30 sec of full universe pressure absorbed silently │
   └──────────────────────────────────────────────────────┘
                          │
                          ▼
   ┌──────────────────────────────────────────────────────┐
   │ LAYER 3 ─── 💾 SPILL (NDJSON file on disk)            │
   │   If ring fills, spill to data/spill/seals-*.bin      │
   │   Survives process restart                            │
   └──────────────────────────────────────────────────────┘
                          │
                          ▼
   ┌──────────────────────────────────────────────────────┐
   │ LAYER 4 ─── 📦 DLQ (data/dlq/*.ndjson)                │
   │   Catch-all: every payload as recoverable text        │
   │   SEBI audit-grade                                    │
   └──────────────────────────────────────────────────────┘
                          │
                          ▼
   ┌──────────────────────────────────────────────────────┐
   │ LAYER 5 ─── 🔍 POST-MARKET CROSS-VERIFY               │
   │   16:00 IST: pull Dhan REST candles vs our live ticks │
   │   ANY mismatch → Telegram FAILED + full diff list     │
   │   ZERO tolerance (no epsilon, no ±10%)                │
   └──────────────────────────────────────────────────────┘
                          │
                          ▼
                  ✅ NO TICK LOST (or you find out within 30 min)
```

---

## The auto-driver / lazy-kid explanation

> "Imagine you run a juice shop with one cash register. Customer comes, you bill, money in register, done.
>
> **What if the register breaks for 60 seconds?**
> - Layer 1: You SEE it broke within 5 sec (you didn't hear the beep)
> - Layer 2: Customer's note goes into your shirt pocket (5M-tick rescue ring)
> - Layer 3: Pocket full? Put notes in the drawer under counter (spill file)
> - Layer 4: Drawer full? Stack them by the door in a labeled bag (DLQ NDJSON)
> - Layer 5: After shutters close, count every note vs every customer slip — Telegram me if even ₹1 is off
>
> Was the register down? YES. Did you lose a single rupee? NO. Did you tell the boss within 5 seconds? YES.
>
> THAT is the honest guarantee. Not 'register will never break'. That promise is a lie."

---

## Phase 0.5 — Disconnect Resilience Hardening (LOCKED Mon 21:00 IST)

22-scenario disconnect-cause matrix enumerated → 8 gaps identified → Phase 0.5 closes all 8.

| Item | What | Status | Task file |
|---|---|---|---|
| 0.5.1 | NET-01 IP drift detector promotion | RESERVED | `tasks/T05-01-*.md` (Wed) |
| 0.5.2 | NET-02 DNS resolution failure cascade | RESERVED | `tasks/T05-02-*.md` (Wed) |
| 0.5.3 | PROC-01 OOM kill detector | RESERVED | `tasks/T05-03-*.md` (Wed) |
| 0.5.4 | DH-911 Dhan silent black-hole detector | RESERVED | `tasks/T05-04-*.md` (Wed) |
| 0.5.5 | WS-BACKPRESSURE-01 `tv_ws_recv_window_bytes` gauge | NEW | `tasks/T05-05-*.md` (Wed) |
| 0.5.6 | TCP SO_KEEPALIVE on every WS socket | NEW | `tasks/T05-06-*.md` (Wed) |
| 0.5.7 | WS-BACKPRESSURE-02 SO_RCVBUF tuning | NEW | `tasks/T05-07-*.md` (Wed) |
| 0.5.8 | RESOURCE-03 spill-file size guard | RESERVED | `tasks/T05-08-*.md` (Wed) |
| 0.5.9 | Wave 5 core_pinning wiring verification | VERIFICATION | `tasks/T05-09-*.md` (Wed) |
| 0.5.10 | **NEW Mon eve** — consolidated `PreMarketReady` Telegram | NEW | `tasks/T05-10-*.md` (Wed) |

**Core-pinning design (locked Mon 21:03 IST):** ONE core for ALL 5 WS conns (async multiplex). Same scheme Mac dev = AWS prod. 4-core layout: Core 0 = WS recv, Core 1 = parser, Core 2 = ILP writer, Core 3 = other. Mac has 14 cores (10 idle, no harm); AWS c8g.xlarge uses all 4.

---

## What is ALREADY DONE (production today, Mon May 11 2026)

Telegram screenshot Mon 9:04 PM IST confirms LIVE system streaming with:
- `[LOW] Boot complete — 5/5 main feeds DEFERRED until 09:00 IST` (post-market sleep working ✅)
- `[LOW] Instruments OK: Derivatives 97,422 / Underlyings 218` (universe loaded ✅)
- `[LOW] Auth OK — Dhan JWT acquired` (3-tier auth working ✅)
- `[LOW] Order Update WS connected` (order WS up ✅)
- `[HIGH] NSE bhavcopy cross-check FAILED — Reason: questdb_query_failed` (Phase 0.5 NET-02/DH-911 will catch upstream causes)
- `[MEDIUM] Shutdown initiated 07:18 PM` (clean shutdown working ✅)
- `[LOW] Order Update WS reconnected — Recovered after 1 consecutive failures 07:02 PM` (reconnect working ✅)

| ✅ Done | What | Where |
|---|---|---|
| 5-layer absorbing rescue+spill+DLQ | Tick path absorption | `crates/storage/src/tick_persistence.rs` |
| 7-layer telemetry | 53 ErrorCode variants, 21 ratchet tests | `crates/common/src/error_code.rs` + `error_code_*_guard.rs` |
| Composite-key uniqueness (I-P1-11) | `(security_id, exchange_segment)` everywhere | `instrument_registry.rs::by_composite` |
| SLO score every 10s + market-open self-test 09:16:30 IST | Real-time guarantee score | `slo_score.rs`, `market_open_self_test.rs` |
| SubscribeRxGuard reconnect persistence (PR #337) | Subs survive every reconnect | `connection.rs` |
| Pool supervisor 5s respawn (WS-GAP-05) | Dead-task respawn within 5s | `connection_pool.rs::spawn_pool_supervisor_task` |
| Post-market 1-day cross-verify | Dhan REST vs live ticks, ZERO tolerance | `crates/core/src/historical/cross_verify.rs` |
| Schema self-heal at boot | ALTER ADD COLUMN IF NOT EXISTS pattern | `instrument_persistence.rs::ensure_instrument_tables` |
| 15+ audit tables with DEDUP UPSERT KEYS | SEBI-grade audit trail | `crates/storage/src/*_audit_persistence.rs` |
| Telegram coalescer (60s buckets) | No alert storms | `crates/core/src/notification/coalescer.rs` |
| 7-section `make doctor` health check | One-command audit | `scripts/doctor.sh` |
| Boot-time 09:13 IST Phase 2 dispatcher | Stock F&O subscribe at pre-open finalize | `phase2_scheduler.rs` |
| 09:13 / 09:15:30 IST positive Telegram pings | Operator's "am I streaming" answer | `MarketOpenDepthAnchor` + `MarketOpenStreamingConfirmation` |
| 4 audit tables: phase2_audit / depth_rebalance_audit / ws_reconnect_audit / boot_audit / selftest_audit / order_audit | Forensic reconstruction | Wave-2-D Item 9 |

---

## Real-time proof tools (what every session can use RIGHT NOW)

| Operator question | Tool that answers, NO guessing | Returns |
|---|---|---|
| "Is anything broken now?" | `mcp__tickvault-logs__run_doctor` | 7-section pass/fail snapshot |
| "What errors fired this hour?" | `mcp__tickvault-logs__tail_errors` | `data/logs/errors.jsonl.*` |
| "Which alerts are firing?" | `mcp__tickvault-logs__list_active_alerts` | Prometheus Alertmanager state |
| "Is QuestDB writing?" | `mcp__tickvault-logs__questdb_sql "select count(*) from ticks where ts > now() - 1m"` | Live SQL result |
| "What's the SLO score right now?" | `mcp__tickvault-logs__prometheus_query "tv_realtime_guarantee_score"` | Updates every 10 sec |
| "Did Phase 2 fire today?" | `mcp__tickvault-logs__questdb_sql "select * from phase2_audit where trading_date_ist = today"` | SEBI-retained audit table |
| "What's the latest novel error signature?" | `mcp__tickvault-logs__list_novel_signatures` | Unseen-before fingerprints |
| "What's the runbook for code X?" | `mcp__tickvault-logs__find_runbook_for_code X` | Returns `.claude/rules/*.md` path |

**Every new Claude Code session auto-runs these at SessionStart per `.claude/settings.json` lines 168-181.**

---

## The Honest 100% Claim — PR-body wording (mandated by charter §8)

Every PR description mentioning "100% guarantee" MUST use this exact wording:

> "100% inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤5,000,000-tick ring buffer capacity (constant `TICK_BUFFER_CAPACITY`, `crates/common/src/constants.rs`, ratcheted by `crates/storage/tests/zero_tick_loss_alert_guard.rs`);
> bench-gated O(1) hot path;
> composite-key uniqueness;
> chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake (`crates/core/tests/ws_sleep_resilience.rs`).
> Beyond the envelope, DLQ NDJSON catches every payload as recoverable text.
> Outstanding (Wave-6 backlog): >65h holiday-weekend dormant sleep test (W6-2)."

Anything stronger ("WebSocket never disconnects", "QuestDB never fails") without the envelope qualifier = REJECT IN REVIEW.

---

## Per-item 15-row + 7-row guarantee matrix (MANDATORY for EVERY plan item)

Every Phase 0.5 item AND every Step 2 / Step 3 item AND every Friday task MUST carry:

### 15-row Coverage Matrix
| Demand | Mechanical proof | Per-item gate |
|---|---|---|
| 100% code coverage | `quality/crate-coverage-thresholds.toml` per crate | PR includes coverage delta |
| 100% audit coverage | `<event>_audit` table with DEDUP UPSERT KEYS | item adds/extends audit table |
| 100% testing coverage | 22 test categories per `testing.md` | item declares which 22 it covers |
| 100% code checks | banned-pattern + pub-fn-test + plan-verify + secret-scan + 8 pre-commit gates | all gates green |
| 100% code performance | DHAT zero-alloc + Criterion p99 + bench-gate ≤5% regression | DHAT test if hot path |
| 100% monitoring | 7-layer telemetry (Prom + tracing + Loki + Telegram + Grafana + audit + alert) | 9-box completes 7 layers |
| 100% logging | tracing macros mandatory; ERROR → Telegram | every error uses `error!` with `code` |
| 100% alerting | `alerts.yml` Prom rule + `resilience_sla_alert_guard.rs` ratchet | item adds alert for new failure mode |
| 100% security | banned-pattern + secret-scan + `Secret<T>` + security-reviewer agent | item runs security-reviewer |
| 100% security hardening | static IP enforcement + secret scan + `unused_must_use` lint | item declares attack-surface delta |
| 100% bug fixing | adversarial 3-agent review (proven 4-bug catch rate) | item runs all 3 agents |
| 100% scenarios covering | 9-box + chaos test for new failure mode | item declares scenarios in ratchet |
| 100% functionalities covering | every pub fn has call site + test | item adds tests for every new pub fn |
| 100% code review | adversarial 3-agent on diff before AND after impl | item PR includes both passes |
| 100% extreme check | all of above + ratchet test fails build on regression | item adds ratchet test |

### 7-row Resilience Matrix
| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | Bounded zero loss in chaos envelope | item must not introduce new tick-drop path |
| WS never disconnects | DETECT ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close | item must not break `SubscribeRxGuard` or pool watchdog |
| Never slow/locked/hanged | DHAT ≤4 alloc blocks/8KB across 10K calls; Criterion p99 ≤100ns; tick-gap >30s Telegram; core_affinity Core 0 | item must not add hot-path allocation |
| QuestDB never fails | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal | item must not break self-heal |
| O(1) latency | `from_le_bytes` + `papaya` + `Arc<HashMap>` + SPSC bounded; bench-gate ≤5% regression | item adds Criterion bench if hot path |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS + meta-guard | item DEDUP key includes segment |
| Real-time proof | 7-layer telemetry + SLO-01/SLO-02 @ 10s + market-open self-test @ 09:16:30 IST | item ratchet pins all 7 layers |

**Mechanical enforcement:** `.claude/hooks/per-item-guarantee-check.sh` + `make wave-guarantee-check` block PRs that lack the matrices. Already wired (Wave 5 Item 22).

---

## What this file is for (purpose)

1. **Canonical reference** for any future Claude session opening cold — read this FIRST to avoid re-debating closed items.
2. **Audit trail** for SEBI / future operator handoff — the honest envelope is documented with proof citations.
3. **Charter persistence** — operator's verbatim demand survives `/compact`, web 400s, session boundaries.
4. **Plan item checklist** — every Phase 0.5 / Step 2 / Step 3 / Friday task must satisfy the 15-row + 7-row matrices above.

**Authority chain (for resolving conflicts):**
1. `CLAUDE.md` — top-level project charter (3 principles, BANNED list, workflow)
2. `wave-4-shared-preamble.md` — sub-PR charter (this file's source)
3. `per-wave-guarantee-matrix.md` — the matrix rule
4. THIS file — the operator's verbatim charter + locked verdict
5. Step files (this directory)
6. Decisions log (this directory)
