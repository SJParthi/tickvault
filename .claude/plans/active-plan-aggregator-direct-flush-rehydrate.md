# Implementation Plan: Multi-TF In-Memory Aggregator + Direct Flush + Rehydrate-from-Ticks + Post-Market Cross-Verify

**Status:** APPROVED (operator confirmed 2026-05-10, this revision merges discussion deltas)
**Date:** 2026-05-10
**Approved by:** Parthiban (operator)
**Wave:** 6 (post Wave-5 indices-only scope)
**Branch:** to be created from `main` AFTER PR #548 merges (e.g. `claude/wave-6-pr1-multi-tf-aggregator`)
**Scope size:** Multi-PR (4 sub-PRs), 2-3 weeks calendar, ~3000-5000 LoC net
**Schedule assumption:** AWS instance runs **08:30 → 17:30 IST** (operator decision 2026-05-10; `aws-budget.md` to be updated separately if this becomes the new permanent schedule)

---

## 🧒 Plain-English One-Liner (kid-friendly)

> Stop building candles by chaining 9 database "views" on top of each other (slow + fragile). Instead, build all 9 candle sizes (1m → 1d) in RAM directly from live ticks. Flush each finished candle straight to its own table at seal time (no cascade). On boot, replay missing ticks to catch up. After market close, fetch Dhan's authoritative 1m data for all ~11,045 instruments, zero-tolerance per-timestamp per-field cross-verify against our live aggregator, self-heal any mismatches via UPSERT, extract prev-day OHLCVOI from the last 1m candle. Persistent retry queue + 7-trading-day escalation cap means we **never silently drop an instrument**.

---

## 🎯 The 4 Sub-PRs (build order)

| # | Sub-PR | Purpose | Branch | After |
|---|---|---|---|---|
| 1 | **Aggregator engine** | RAM-based 9 TFs, bounded mpsc, IST midnight + Muhurat boundary timer | `claude/wave-6-pr1-multi-tf-aggregator` | PR #548 merged |
| 2 | **Rehydration** | Boot replays missing ticks, race-free buffer gate, fail-closed HALT | `claude/wave-6-pr2-rehydration` | PR1 merges |
| 3 | **Post-market 1m fetch + cross-verify + self-heal** | Single big job at 15:30 IST: fetch → aggregate → verify → heal → extract prev-day | `claude/wave-6-pr3-postmarket-1m` | PR2 merges |
| 4 | **Promotion** | Drop `candles_1s` + 9 mat views + LiveCandleWriter, rename shadow tables | `claude/wave-6-pr4-promote` | PR3 + 1 trading week zero-diff + 2 days clean |

---

## 📅 Final Daily Schedule

| Time IST | Action | Notes |
|---|---|---|
| **08:30** | AWS auto-starts → app boots | EventBridge |
| 08:31 | Token load, QuestDB DDL, universe load | 1 min |
| 08:33 | Resume retry queue (if non-empty from yesterday) | reads `cross_verify_retry_queue` |
| 08:33 → 09:13 | Retry queue drain + aggregator rehydration | 40 min carryover window |
| 09:13 | Phase 2 dispatcher fires | Wave 5 indices-only scope |
| 09:15 | Market opens — live aggregator runs in RAM | hot path |
| 09:15 → 15:30 | Live trading + RAM aggregator + direct-flush at seal | — |
| **15:30:00** | **🚀 Post-market 1m fetch fires immediately** | Tokio scheduled task |
| 15:30 → 16:07 | 11,045 calls @ 5/sec rate limit | 37 min, 11% of daily Dhan quota |
| 16:07 → 17:00 | Slow retry every 5 min for failed instruments | 53 min retry window |
| 17:00 → 17:15 | Cross-verify + self-heal + prev_day OHLCVOI extract | 15 min |
| 17:15 → 17:25 | Persist retry queue snapshot + write CSV mismatch file | 10 min |
| 17:25 | Telegram daily summary | — |
| **17:30** | AWS auto-stops | shutdown |
| (overnight 15h) | OFF — queue + audit safe in QuestDB + S3 | — |

---

## 🌐 Universe Coverage (~11,045 instruments)

The single Dhan REST endpoint `/v2/charts/intraday` covers ALL segments. Same fetcher code path, just different `exchangeSegment` + `instrument` params per call.

| Segment | Approx count | Instrument types | OI meaningful? | Volume meaningful? |
|---|---|---|---|---|
| **IDX_I** (indices) | ~29 | NIFTY=13, BANKNIFTY=25, SENSEX=51 + 26 display indices | ❌ Always 0 | ⚠️ Synthetic |
| **NSE_EQ** (cash equities) | ~216 | F&O underlying stock cash | ❌ Always 0 | ✅ Real |
| **NSE_FNO** (NIFTY+BANKNIFTY all expiries) | ~7,500 | FUTIDX + OPTIDX | ✅ Real | ✅ Real |
| **BSE_FNO** (SENSEX all expiries) | ~3,300 | FUTIDX + OPTIDX | ✅ Real | ✅ Real |
| **TOTAL** | **~11,045** | | | |

**Stock F&O:** NOT subscribed under Wave 5 indices-only scope. **BSE_EQ:** NOT subscribed today (Wave 7 if added). The fetcher is segment-agnostic — adding new segments later requires zero code change.

---

## 🔑 Honest 100% Charter (operator-locked, mandatory PR-body wording)

> **"100% inside the tested envelope, with ratcheted regression coverage:**
>
> - ≤60s QuestDB outage absorbed by rescue→spill→DLQ
> - ≤2,000,000-tick ring buffer (`TICK_BUFFER_CAPACITY` ratcheted by `zero_tick_loss_alert_guard.rs`)
> - bench-gated O(1) hot path (DHAT zero-alloc + Criterion p99 ≤100ns + ≤5% regression)
> - composite-key uniqueness `(security_id, exchange_segment)` per I-P1-11
> - chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake
> - post-market 1m fetch + **zero-tolerance per-timestamp per-field cross-verify** + self-heal + 7-day retry escalation cap
> - 7-layer observability per item (Prom counter+gauge + tracing + Loki + Telegram + Grafana + audit table)
> - 22-category test ratchet + 100% line coverage threshold + adversarial 3-agent review
> - SLO-01/02 real-time guarantee score @ 10s
>
> **Beyond envelope** (honest physics):
>
> - TCP can RST → we DETECT in ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close
> - Disk full → DLQ NDJSON catches every payload as recoverable text
> - Dhan REST outage > 7 trading days → operator manual action (POSTMARKET-FATAL HALT)
> - Contracts expiring instantly at 15:30:00 IST may move to Dhan's expired-options endpoint (Wave 7 fix pending Dhan support clarification)
> - Outside envelope = honest gap equals duration of condition
>
> **Anything weaker than mechanical-ratchet enforcement = NOT 100%. Aspirational claims = REJECTED IN REVIEW."**

---

## 🛡️ The 15-Row 100% Mechanical Proof Matrix (every Wave 6 item carries this)

| # | 100% demand | Mechanical proof artifact | Real-time check | Per-item gate |
|---|---|---|---|---|
| 1 | 100% code coverage | `quality/crate-coverage-thresholds.toml` 100% min/crate; `scripts/coverage-gate.sh` | post-merge llvm-cov | item PR includes coverage delta |
| 2 | 100% audit coverage | `<event>_audit` per typed event with DEDUP UPSERT KEYS | `mcp__tickvault-logs__questdb_sql` | item adds/extends audit table |
| 3 | 100% testing coverage | 22 test categories per `testing.md` | `cargo test --workspace` green | item declares which 22 |
| 4 | 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + 8 pre-commit gates | pre-push mandatory | all gates green |
| 5 | 100% performance | DHAT zero-alloc + Criterion p99 budgets + bench-gate ≤5% | `cargo bench` + `bench-gate.sh` | DHAT test if hot path |
| 6 | 100% monitoring | 7-layer telemetry (Prom counter+gauge + tracing span + Loki log + Telegram event + Grafana panel + audit table) | `mcp__tickvault-logs__run_doctor` | 9-box completes 7 layers |
| 7 | 100% logging | tracing macros mandatory; ERROR → Telegram; hourly errors.jsonl rotation | tag-guard meta-test | every error has `code` field |
| 8 | 100% alerting | `alerts.yml` Prom rule + `resilience_sla_alert_guard.rs` ratchet | `mcp__tickvault-logs__list_active_alerts` | item adds alert for new failure |
| 9 | 100% security | banned-pattern + secret-scan + `Secret<T>` + security-reviewer agent | `cargo audit` post-deploy | item runs security-reviewer |
| 10 | 100% security hardening | static IP + secret scan + `unused_must_use` lint | post-deploy IP verify | item declares attack-surface delta |
| 11 | 100% bugs fixing | adversarial 3-agent review (proven 4-bug catch rate per PR #393) | pre-PR + post-impl | item runs all 3 agents |
| 12 | 100% scenarios | 9-box + chaos test for new failure mode | chaos suite | item declares scenarios |
| 13 | 100% functionalities | every pub fn has call site + test (`pub-fn-wiring-guard.sh` + `pub-fn-test-guard.sh`) | pre-push gates 6+11 | tests for every new pub fn |
| 14 | 100% code review | adversarial 3-agent on diff before AND after impl | per-PR | item PR includes both passes |
| 15 | 100% extreme check | all of above + ratchet tests fail build on regression | every commit | item adds ratchet test |

---

## 🛡️ The 7-Row Resilience Demand Matrix (honest envelope)

| Demand | Honest envelope | What we cannot promise (physics) | Per-item proof |
|---|---|---|---|
| Zero ticks lost | BOUNDED zero loss inside chaos envelope: ring 2M → spill NDJSON → DLQ | Disk-full unbounded → spill grows → eventual drop | item must not introduce new tick-drop path |
| WS never disconnects | DETECT ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close | TCP RST anytime; OS preempt; Dhan can drop | item must not break SubscribeRxGuard or pool watchdog |
| Never slow/locked/hanged | DHAT ≤4 alloc/8KB across 10K calls; Criterion p99 ≤100ns; tick-gap >30s Telegram | OS scheduler can preempt → we DETECT, we don't PREVENT | item must not add hot-path allocation |
| QuestDB never fails | ABSORB ≤60s outage via 3-tier rescue→spill→DLQ + schema self-heal | Remote process; cannot prevent its failures | item must not break self-heal |
| O(1) latency | `from_le_bytes` + `papaya` + `Arc<HashMap>` + SPSC bounded; bench-gate ≤5% | Drift returns within 1 month without ratchet | item adds Criterion bench if hot path |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS + meta-guard | Without composite key, 2026-04-17 production bug recurs | item DEDUP key includes segment |
| Real-time proof | 7-layer telemetry + SLO-01/02 @ 10s + market-open self-test @ 09:16:30 IST | None — fully mechanical | item ratchet pins all 7 layers |

---

## 🤖 Automation Charter (every Claude session inherits)

| Need | Auto-available tool | When |
|---|---|---|
| Health check | `mcp__tickvault-logs__run_doctor` | session start, before code |
| Tail recent ERRORs | `mcp__tickvault-logs__tail_errors` | investigating any failure |
| Find runbook for ErrorCode | `mcp__tickvault-logs__find_runbook_for_code` | emitting/referencing an ERROR |
| Live Prometheus query | `mcp__tickvault-logs__prometheus_query` | verifying counter increments |
| Live QuestDB SQL | `mcp__tickvault-logs__questdb_sql` | verifying table writes / DEDUP |
| Live Grafana panel | `mcp__tickvault-logs__grafana_query` | verifying dashboard renders |
| Detect novel errors | `mcp__tickvault-logs__list_novel_signatures` | after ERROR-emitting change |
| Source-grep | `mcp__tickvault-logs__grep_codebase` | faster than spawning agent |
| Latest summary | `mcp__tickvault-logs__summary_snapshot` | session start |
| Active Prom alerts | `mcp__tickvault-logs__list_active_alerts` | before opening PR |
| Local app log tail | `mcp__tickvault-logs__app_log_tail` | debugging runtime issues |
| Triage log tail | `mcp__tickvault-logs__triage_log_tail` | reviewing auto-fix outcomes |
| Docker container status | `mcp__tickvault-logs__docker_status` | when service OFFLINE |
| AWS access | `aws` CLI + SSM `/tickvault/<env>/<key>` | on-demand |

**Mandatory automation steps at session start (BEFORE TodoWrite, BEFORE code):**
1. Run `mcp__tickvault-logs__run_doctor` — capture current health
2. Run `mcp__tickvault-logs__summary_snapshot` — read recent ERROR signatures
3. Run `mcp__tickvault-logs__list_active_alerts` — fail loudly if anything red
4. Run `bash .claude/hooks/session-auto-health.sh` if present
5. Read `data/logs/session-auto-health.latest.txt` if present

---

## 🧪 Adversarial 3-Agent Review Protocol (mandatory per sub-PR)

For ANY decision touching > 3 crates or > 1,000 LoC:

| Agent | Role | Report cap |
|---|---|---|
| `hot-path-reviewer` | Hot-path violations: `.clone`, `Vec::new`, `format!`, `Box`, `dyn`, unbounded channels | under 400 words |
| `security-reviewer` | Secret exposure in logs/labels, ILP injection, path traversal, unsafe blocks, missing input sanitization | under 400 words |
| `general-purpose` (hostile bug-hunt) | Race conditions, daily-reset collisions, missing market-hours gate, missing edge-trigger, flush/persist using `warn!` instead of `error!`, counters shown raw without `increase()`, pub fn defined-but-never-called, false-OK class bugs | under 600 words |

Wait for ALL THREE reports. Synthesize into verdict table: CRITICAL / HIGH / MEDIUM / LOW / FALSE-POSITIVE. Fix every CRITICAL and HIGH inline BEFORE opening the PR. After implementation lands green, run the SAME 3 agents again as adversarial review on the diff.

---

## 📦 Per-Item 12-Box Done Matrix (every Wave 6 item must tick all 12)

| Box | Meaning | Proof |
|---|---|---|
| 1. Implemented | Code merged | git SHA |
| 2. Unit tested | `#[test]` happy + edge cases | test names |
| 3. Integration tested | end-to-end flow | test file |
| 4. Property tested | `proptest` input space (where applicable) | proptest name |
| 5. DHAT zero-alloc | hot path 0 alloc | dhat test (hot path only) |
| 6. Criterion bench | p99 budget pinned | budget toml entry (hot path only) |
| 7. Chaos tested | failure-mode coverage | chaos file name |
| 8. Prom counter + Grafana panel | telemetry visible | metric + panel name |
| 9. Telegram event + alert rule | operator paged | event variant + alert |
| 10. Audit table written | SEBI trail | table + DEDUP key |
| 11. `make doctor` green | real-time check | doctor section |
| 12. Adversarial 3-agent review | hot-path + security + bug-hunt | review SHA |

**Status sticker per item:** 🟢 12/12 · 🟡 partial · 🔴 not started

---

## 🏗️ Sub-PR #1 — Aggregator Engine

> **Pre-impl 3-agent verdict (2026-05-10):** 4 CRITICAL + 11 HIGH + 5 MEDIUM + 2 LOW + 1 FALSE-POSITIVE.
> Operator unblocked impl with all fixes folded in. Locked design decisions
> below override the original abstract bullets.

### Locked design decisions (per pre-impl 3-agent review)

| Lock | Decision | Replaces |
|---|---|---|
| **L-C1** | **Ring→spill→DLQ pattern** for sealed candles, mirroring `tick_persistence.rs` (`SEAL_BUFFER_CAPACITY = 600_000`, watermark, ring spill to `data/spill/seals-YYYYMMDD.bin`, NDJSON DLQ on dual failure). NOT a simple bounded mpsc with drop-newest. The IST-midnight burst of ~99K seals MUST NOT silently drop. | item 1.2 (bounded mpsc 65,536 drop-newest) |
| **L-C2** | **`AtomicCell<LiveCandleState>`-per-TF** stored eagerly in `papaya::HashMap<(u32,u8), Arc<[AtomicCell<LiveCandleState>; 9]>>`. Lock-free hot-path mutation. Pre-populated at boot from `InstrumentRegistry::iter()` (composite key) BEFORE WS subscribe. NO `Mutex`, NO `compute()`-closure overhead, NO `unsafe`. | item 1.1 ambiguous "papaya holds `[LiveCandle;9]`" |
| **L-C3** | **Late ticks discarded with `error!` + counter** `tv_aggregator_late_tick_total{action="discard"}`. NO silent merge across buckets. `seal_in_progress` epoch fence per cell. ErrorCode `AGGREGATOR-LATE-01`. | item 1.1/1.3 silent on race |
| **L-C4** | **Muhurat code + ratchet test #5 DEFERRED to Wave 7** (AWS auto-stop 17:30 IST hard-conflicts with Muhurat 17:30–18:30; needs EventBridge override + ratchet test #5 removed from Sub-PR #1). Plan tracker line 482 backlog updated. | item 1.3 Muhurat clause + ratchet test #5 |
| **L-H6** | Wave-5 pct-stamping reads in-memory `Arc<HashMap<(u32,u8), PrevDayRefs>>` populated at boot via `prev_day_cache_loader::populate_prev_day_cache_at_boot()` (existing PREVCLOSE-04 path). Refreshed at IST-midnight boundary timer. NEVER QuestDB read on hot path. PREVCLOSE-04 fires once-per-process if cache empty. | unspecified sync vs async |
| **L-H7** | Boundary timer derives `trading_date_ist` from `(exchange_timestamp + IST_OFFSET).date()` (the WS LTT field), NOT `Utc::now()`. tokio `Instant` only for sleep duration. Missed-boundary detection (`last_seen_minute < expected_minute - 1`) → BOUNDARY-01 catch-up seal. | wall-clock dependence + DST risk |
| **L-H8** | Two budget rows: `consume_tick_no_seal ≤ 100ns p99` (fast path), `consume_tick_with_seal ≤ 1µs p99` (cold path). | single conflated p99 budget |
| **L-H9** | Dropped-seal log uses `error!(code = ErrorCode::AggregatorDrop01.code_str(), ...)` per `error_level_meta_guard.rs` Rule 5. | unspecified |
| **L-H10** | All 9 `DEDUP_KEY_CANDLES_*M_SHADOW` constants live in `crates/storage/src/shadow_persistence.rs` (so existing `dedup_segment_meta_guard.rs` scans them). Each contains literal substring `exchange_segment`. Meta-guard `len() >= 7` bumped to `>= 16`. | undefined location |
| **L-H11** | Pre-condition fix: `candle_persistence.rs:636` `warn!` → `error!` with `code` field BEFORE shadow writer is modeled on this file. | existing bug carry-over |
| **L-H12** | Aggregator seal path propagates `Option<u8>` for segment. Unknown segment → `None` pct, fall back to `0.0` per PREVCLOSE-04. NEVER substitutes `0u8` (which would map to IDX_I and silently look up NIFTY/BANKNIFTY pct → 2026-04-17 I-P1-11 bug class). | bug class re-introduction risk |
| **L-H13** | Boundary-fire gated by `is_within_market_hours_ist()` AND per-cell `tick_count > 0` check. NO empty seals after 15:30 close polluting shadow tables overnight. | nightly 99K zero-volume rows |
| **L-H14** | PREVCLOSE-04 once-per-boot WARN gate (using `Once` or atomic boot flag) when `prev_day_cache.is_empty()` at first seal. | silent 0.0 stamping |
| **L-H15** | Per-minute aggregator heartbeat: `NotificationEvent::AggregatorMinuteSealBurst { seals_emitted, seals_dropped }` (Severity::Info, edge-coalesced 60s) — positive false-OK avoidance signal. | no positive signal until PR3 |
| **L-M16** | Sub-PR #1 ships `aggregator_seal_audit` table NOW (not deferred to PR3). DEDUP UPSERT KEYS `(trading_date_ist, security_id, exchange_segment, timeframe, candle_ts)`. Box 10 of 12-box matrix turns GREEN for PR1. | "audit deferred to PR3" risk |
| **L-M17** | `#[instrument(skip_all, level = "trace")]` on hot-path entry, with the `tracing` static `LevelFilter` at the binary level set to `info` so trace-level spans are compile-time disabled (zero alloc). DHAT test asserts. | tracing alloc conflict |
| **L-M18** | Drop alert `tv-multi-tf-writer-dropping`: `for: 5m` AND midnight-suppression window (00:00–00:01 IST excluded via `absent_over_time` recording rule). | nightly burst false-pages |
| **L-M19** | Cell array eagerly pre-populated at boot via `InstrumentRegistry::iter()` (composite-key iteration). Ratchet test `test_aggregator_zero_alloc_on_first_tick` uses DHAT. | first-tick alloc spike risk |
| **L-M20** | All free-text ILP `STRING` columns (e.g. `last_error` in retry queue, symbol display labels) pass through `sanitize_audit_string` (added to `crates/common/src/sanitize.rs` if not present). All `Buffer::symbol(...)` calls pass through `sanitize_ilp_symbol`. Negative fuzz test added. | ILP injection risk |
| **L-L21** | Branch retained at system-assigned `claude/aggregator-engine-pN23a` (system git rules override plan). Plan branch name `claude/wave-6-pr1-multi-tf-aggregator` is informational only. | branch mismatch |
| **L-L22** | Per-item 12-box stickers added under each item (1.1–1.8) below in addition to the wave-level sticker. | misleading hook signal |

### Items (revised)

| # | Item | Detail | Box |
|---|---|---|---|
| 1.1 | RAM-based 9-TF aggregator (L-C2) | `Arc<[AtomicCell<LiveCandleState>; 9]>` per `(security_id, exchange_segment)` in `papaya::HashMap`. Eager pre-populate at boot from registry composite-key iter. Lock-free mutation. | 🔴 0/12 |
| 1.2 | Ring→spill→DLQ for sealed candles (L-C1) | `SEAL_BUFFER_CAPACITY = 600_000`, fixed-record disk spill `data/spill/seals-YYYYMMDD.bin`, NDJSON DLQ on dual failure. ErrorCode `AGGREGATOR-DROP-01` `error!`. | 🔴 0/12 |
| 1.3 | Boundary timer — IST midnight only (L-C4 deferred Muhurat) | Exchange-ts derived `trading_date_ist` (L-H7). Missed-boundary catch-up via BOUNDARY-01. Market-hours + tick_count gate (L-H13). | 🔴 0/12 |
| 1.4 | Direct-flush at seal | 9 shadow tables `candles_{1m,5m,15m,30m,1h,2h,3h,4h,1d}_shadow` with DEDUP UPSERT KEYS `(ts, security_id, exchange_segment)` (L-H10). | 🔴 0/12 |
| 1.5 | Wave-5 pct-stamping (in-memory L-H6) | Hot-path read of `Arc<HashMap<(u32,u8), PrevDayRefs>>`. Boot-load via `prev_day_cache_loader`. Once-per-boot WARN if empty (L-H14). | 🔴 0/12 |
| 1.6 | DHAT + Criterion budgets (L-H8 split) | `consume_tick_no_seal ≤ 100ns p99` + `consume_tick_with_seal ≤ 1µs p99` in `quality/benchmark-budgets.toml`. | 🔴 0/12 |
| 1.7 | Property tests | proptest 1m × N → Nm, all 9 TFs covered. | 🔴 0/12 |
| 1.8 | Per-minute heartbeat (L-H15) | `AggregatorMinuteSealBurst` (Info, coalesced 60s) — positive signal. | 🔴 0/12 |
| 1.9 | Aggregator seal audit table (L-M16) | `aggregator_seal_audit` — DEDUP UPSERT KEYS `(trading_date_ist, security_id, exchange_segment, timeframe, candle_ts)`. | 🔴 0/12 |

**Wave-level done matrix:** 🔴 0/12

---

## 🏗️ Sub-PR #2 — Rehydration On Boot

| # | Item | Detail |
|---|---|---|
| 2.1 | Boot-time replay | reads latest sealed timestamp per `(security_id, segment, TF)` from shadow tables, replays missing ticks |
| 2.2 | Race-free buffer gate | live ticks buffered in mpsc until rehydration completes; gate flipped before WS subscribe |
| 2.3 | Fail-closed HALT | if rehydration fails after 3 retries → app refuses to start; Telegram CRITICAL |
| 2.4 | 60s rehydration budget | bench-gated via `tv_aggregator_rehydration_seconds` Prom histogram |
| 2.5 | Idempotent on multiple boots | rehydration is pure function of ticks table state |
| 2.6 | Tracing span per phase | `#[instrument]` on rehydration entry + per-instrument span |

**Done matrix:** 🔴 0/12

---

## 🏗️ Sub-PR #3 — Post-Market 1m Fetch + Cross-Verify + Self-Heal (THE BIG ONE)

| # | Item | Detail |
|---|---|---|
| 3.1 | Trigger at 15:30:00 IST sharp | Tokio scheduled task (uses `TradingCalendar` to skip non-trading days) |
| 3.2 | Rate-limited fetcher | 5 calls/sec via `governor` GCRA; respects Dhan Data API limit |
| 3.3 | Endpoint locked | `POST /v2/charts/intraday` with `interval="1"`, `oi=true`, `fromDate=today`, `toDate=today` (NOT 5/15/25/60 — banned) |
| 3.4 | 11K calls in ~37 min best case | 11,045 / 5 = ~2209s = ~37 min |
| 3.5 | Per-instrument retry strategy | 6 quick retries (1s → 32s exponential) → slow retry every 5 min until 17:00 IST |
| 3.6 | **Persistent retry queue** | failed instruments → `cross_verify_retry_queue` QuestDB table with DEDUP UPSERT KEYS `(trading_date_ist, security_id, exchange_segment)` |
| 3.7 | **Resume on every boot** | 08:33 next day reads queue, resumes retry; 1h 27min before 09:13 Phase 2 |
| 3.8 | **7-trading-day hard escalation cap** | if same instrument unfetched after 7 trading days → POSTMARKET-FATAL → HALT next boot, operator manual |
| 3.9 | Dhan error code routing | DH-904/805/807/901 retried with backoff; DH-905/906/907 escalated immediately (our bug, never silent retry) |
| 3.10 | Idempotent re-run | killed mid-fetch → restart from last completed instrument via queue state |
| 3.11 | Local aggregation 1m → 5m/15m/30m/1h/2h/3h/4h/1d | deterministic rollup; we DO NOT fetch higher TFs from Dhan |
| 3.12 | **Zero-tolerance per-timestamp per-field cross-verify** | for each `(security_id, segment, candle_ts)` × 6 fields (O/H/L/C/V/OI) → exact compare; ANY delta = mismatch |
| 3.13 | Mismatch detection scope | **1m candles only** (vs Dhan 1m fetch); higher TFs by-construction correct after self-heal |
| 3.14 | Self-heal | UPSERT Dhan's value into shadow tables; re-aggregate 1m → 5m/15m/etc. for affected timestamps |
| 3.15 | Extract prev_day OHLCVOI from last 1m candle | `previous_day` table populated for tomorrow's pct-stamping |
| 3.16 | **Four-sink output design** | (1) Telegram summary (~25 lines, references CSV path) (2) CSV at `/data/cross-verify/{date}-mismatches.csv` (3) `cross_verify_mismatch_audit` QuestDB table (4) errors.jsonl + Loki for MCP access |
| 3.17 | Telegram daily summary | "Fetched X/11045, Mismatches Y, Self-healed Z, CSV: path, Top 5: ..." |
| 3.18 | Two distinct checks | (a) Fetch Completeness (X/11045 succeeded) (b) Mismatch Detection (Y per-field deltas found) — never conflated |

**Done matrix:** 🔴 0/12

---

## 🏗️ Sub-PR #4 — Promotion (Atomic Cutover)

| # | Item | Detail |
|---|---|---|
| 4.1 | Drop `candles_1s` base table | banned-pattern guard added |
| 4.2 | Drop 9 cascading materialized views | dead code |
| 4.3 | Drop `LiveCandleWriter` | dead code |
| 4.4 | Rename shadow tables to canonical | `candles_1m_shadow` → `candles_1m`, etc. |
| 4.5 | Migrate Grafana panels | point to renamed tables |
| 4.6 | Banned-pattern: `candles_1s` reference | `banned-pattern-scanner.sh` blocks any future re-add |
| 4.7 | Pre-promotion gate | 1 trading week of zero-diff cross-verify + 2 days clean |

**Done matrix:** 🔴 0/12

---

## 🗄️ NEW QuestDB Tables (2 new, this plan)

### `cross_verify_retry_queue`

```
ts TIMESTAMP,
trading_date_ist DATE,
security_id INT,
exchange_segment SYMBOL,
attempts INT,
last_error STRING,
last_attempt_ts TIMESTAMP,
status SYMBOL  -- 'pending' | 'succeeded' | 'escalated'
DEDUP UPSERT KEYS(trading_date_ist, security_id, exchange_segment)
```

Survives reboots. Resumed on every boot. SEBI 5y retention via S3 cold tier.

### `cross_verify_mismatch_audit`

```
ts TIMESTAMP,                          -- when verify ran
trading_date_ist DATE,
security_id INT,
exchange_segment SYMBOL,
candle_ts TIMESTAMP,                   -- the 1m bucket where mismatch found
timeframe SYMBOL,                      -- '1m' (only TF cross-verified vs Dhan)
field SYMBOL,                          -- 'open'|'high'|'low'|'close'|'volume'|'open_interest'
our_value DOUBLE,
dhan_value DOUBLE,
delta DOUBLE,
healed_at TIMESTAMP                    -- when self-healed via UPSERT
DEDUP UPSERT KEYS(trading_date_ist, security_id, exchange_segment, candle_ts, timeframe, field)
```

Per-timestamp per-field forensic record. Grafana panel + MCP query support. SEBI 5y retention.

---

## 📂 NEW File / Path Layout

| Path | Purpose | Retention |
|---|---|---|
| `/data/cross-verify/{YYYY-MM-DD}-mismatches.csv` | Per-day forensic CSV (operator + Claude session offline review) | 90d local → S3 cold (5y SEBI) |
| `/data/cross-verify/latest.csv` | Symlink to most-recent CSV | always |
| `cross_verify_retry_queue` (QuestDB) | Pending retries | until empty + 7d archive |
| `cross_verify_mismatch_audit` (QuestDB) | Structured mismatch record | 90d hot → S3 cold (5y) |

**CSV schema:** `trading_date_ist, security_id, exchange_segment, symbol, candle_ts, field, our_value, dhan_value, delta, healed_at, heuristic_cause`

---

## 📱 Telegram Summary Template (compact, ~25 lines, ~600 chars)

```
🔍 Cross-Verify Report — 2026-05-08

✅ Fetched: 11,045 / 11,045 instruments (100%)
✅ Cross-verified: 1m candles only (zero tolerance)
⚠️ Comparisons: 24,756,750 (375 bars × 6 fields × 11,045)
🟡 Mismatches: 1,247 detected & healed
🩹 Self-healed: 1,247 / 1,247 (100%)

📋 Full CSV: /data/cross-verify/2026-05-08-mismatches.csv
🗄️ SQL: SELECT * FROM cross_verify_mismatch_audit WHERE trading_date_ist='2026-05-08'

Top 5 by mismatch count:
  • SBIN NSE_EQ — 78
  • HDFCBANK NSE_EQ — 62
  • RELIANCE NSE_EQ — 51
  • NIFTY26MAY24500CE NSE_FNO — 28
  • BANKNIFTY26MAY54000PE BSE_FNO — 19

Likely cause (heuristic): WS disconnect 14:23-14:26 IST
```

---

## 🚨 ErrorCodes (14 total — pinned in `wave-6-error-codes.md` runbook section)

| Code | Severity | Trigger |
|---|---|---|
| REHYDRATE-01 | High | Rehydration query failed |
| REHYDRATE-02 | High | Rehydration timeout (>60s) |
| REHYDRATE-03 | Critical | Rehydration retry exhausted → HALT |
| REHYDRATE-04 | High | Buffer gate stuck |
| REHYDRATE-05 | Critical | Tick replay corrupted |
| BUFFER-GATE-01 | High | Live tick buffer overflow during rehydration |
| BUFFER-GATE-02 | High | Buffer drain timeout |
| POSTMARKET-01 | High | 15:30 fetch incomplete → queue persisted |
| POSTMARKET-03 | Medium | Retry queue carrying over to next boot |
| POSTMARKET-04 | High | Queue still pending at 09:13 next day |
| POSTMARKET-FATAL | **Critical** | 7-trading-day cap exceeded → operator manual |
| VERIFY-01 | Info | Mismatches detected & self-healed (normal) |
| VERIFY-02 | High | Mismatch count > 10K threshold (systemic problem) |
| VERIFY-03 | Medium | Same instrument > 3 days (parser bug suspect) |

---

## 🚫 Banned Patterns (added to `banned-pattern-scanner.sh`)

| Pattern | Why banned |
|---|---|
| `interval = "5"` / `"15"` / `"25"` / `"60"` in cross-verify code | Only 1m allowed (lower granularity loses last-bucket data) |
| `/v2/charts/historical` (daily endpoint) | Not needed — derive from 1m |
| `/v2/charts/expired` | Wave 7 only (Dhan support clarification pending) |
| `nsearchives.nseindia.com` (bhavcopy) | Wave 7 only (deferred) |
| `bseindia.com/download/Bhavcopy` | Wave 7 only (deferred) |
| Groww unofficial API | Permanently banned (no SLA, ToS risk) |
| `candles_1s` reference (post-PR4) | Table dropped, banned in code |
| `LiveCandleWriter` reference (post-PR4) | Dead code, banned |

---

## 🛡️ Real-Time Observability Per Item (7 layers, mandatory)

For EACH new code path in this plan, ALL SEVEN layers fire.

| # | Layer | Mechanism | Verification |
|---|---|---|---|
| 1 | Prom counter | `metrics::counter!("tv_<name>_total", ...)` static labels | `mcp__tickvault-logs__prometheus_query` |
| 2 | Prom gauge | `metrics::gauge!("tv_<name>", ...)` | dashboard panel cites it |
| 3 | Tracing span | `#[instrument]` on hot-path entry fns | `error_code_tag_guard` |
| 4 | Loki structured log | `error!`/`warn!`/`info!` with `code = ErrorCode::X.code_str()` | `mcp__tickvault-logs__tail_errors` |
| 5 | Telegram event | `NotificationEvent::*` typed variant | notification_service routes Severity::High/Critical |
| 6 | Grafana panel | wraps counter in `increase()`/`rate()` per audit-findings Rule 12 | `mcp__tickvault-logs__grafana_query` |
| 7 | Audit table | INSERT into `<event>_audit` table with DEDUP UPSERT KEYS | `mcp__tickvault-logs__questdb_sql` |

---

## 📊 New Prometheus Metrics (this plan adds 14)

| Metric | Type | Purpose |
|---|---|---|
| `tv_multi_tf_aggregator_ticks_consumed_total` | counter | hot-path tick consumption |
| `tv_multi_tf_aggregator_seals_total{tf}` | counter | per-TF seal count |
| `tv_multi_tf_writer_dropped_total{reason}` | counter | bounded channel drops |
| `tv_multi_tf_writer_spill_bytes_total` | counter | NDJSON spill volume |
| `tv_multi_tf_pending_in_channel` | gauge | mpsc occupancy |
| `tv_aggregator_rehydration_total{outcome}` | counter | boot replay outcomes |
| `tv_aggregator_rehydration_seconds` | histogram | replay latency |
| `tv_aggregator_rehydration_ticks_replayed` | counter | replay tick count |
| `tv_live_tick_buffer_gate_buffered` | gauge | gate occupancy |
| `tv_postmarket_fetch_completeness_pct` | gauge | fetch success % |
| `tv_postmarket_fetch_attempts_total{outcome}` | counter | per-call outcome |
| `tv_cross_verify_comparisons_total` | counter | 24.75M/day comparisons |
| `tv_cross_verify_mismatches_total{field}` | counter | per-field mismatch count |
| `tv_cross_verify_retry_queue_size` | gauge | persistent queue size |

---

## 🚨 New Prometheus Alerts (this plan adds 5)

| Alert | Threshold | Severity |
|---|---|---|
| `tv-multi-tf-writer-dropping` | `rate(...dropped_total[1m]) > 0` | High |
| `tv-aggregator-rehydration-failed` | `tv_aggregator_rehydration_total{outcome="failed"} > 0` | Critical |
| `tv-aggregator-rehydration-slow` | `tv_aggregator_rehydration_seconds > 60` | High |
| `tv-postmarket-fetch-incomplete` | `tv_postmarket_fetch_completeness_pct < 100` for 30min | High |
| `tv-cross-verify-mismatches-high` | `tv_cross_verify_mismatches_total > 10000` for 1 run | High |

All wired into `crates/storage/tests/resilience_sla_alert_guard.rs` ratchet.

---

## 🧪 Ratchet Tests Added (20+)

| # | Test | Pins |
|---|---|---|
| 1 | `test_aggregator_purity` | aggregator is pure function of input ticks |
| 2 | `test_bounded_channel_capacity` | mpsc cap = 65536 |
| 3 | `test_drop_newest_policy` | overflow drops newest, preserves order |
| 4 | `test_boundary_timer_at_ist_midnight` | seal fires at 00:00 IST exactly |
| 5 | _(deferred to Wave 7)_ | originally `test_muhurat_session_inclusion` — see Wave 7 backlog |
| 6 | `test_rehydrate_before_ws_subscribe` | boot order race-free |
| 7 | `test_rehydration_fail_closed_halt` | 3 retry exhaustion = HALT |
| 8 | `test_buffer_gate_drains_in_order` | FIFO during gate-flip |
| 9 | `test_aggregator_dhat_zero_alloc` | hot path 0 alloc / 10K calls |
| 10 | `test_aggregator_criterion_p99_lt_100ns` | bench-gate ≤5% regression |
| 11 | `test_dedup_keys_include_segment_for_all_9_tfs` | I-P1-11 enforced |
| 12 | `test_no_candles_1s_after_pr4` | banned-pattern post-promotion |
| 13 | `test_audit_tables_exist` | `cross_verify_*` tables present |
| 14 | `test_wave5_pct_columns_stamped_at_seal` | seal-time pct works |
| 15 | `test_prev_day_cache_loaded_before_aggregator_spawn` | boot order |
| 16 | `test_cross_verify_uses_only_1m_interval` | banned 5/15/25/60 |
| 17 | `test_retry_queue_persists_across_reboot` | queue survives shutdown |
| 18 | `test_retry_queue_7_day_escalation_cap` | POSTMARKET-FATAL fires at day 8 |
| 19 | `test_zero_tolerance_no_epsilon_anywhere` | tolerance constant = 0.0 |
| 20 | `test_csv_mismatch_file_written_with_correct_schema` | CSV format pinned |

---

## 🔥 Chaos Tests Added (6)

| Test | Failure mode |
|---|---|
| `chaos_aggregator_crash_every_minute` | rehydration recovers within 60s |
| `chaos_questdb_timeout_mid_rehydration` | rescue ring + spill catches |
| `chaos_ticks_corruption_during_replay` | parser rejects, no panic |
| `chaos_30m_seal_burst` | 44K rows in 1 second handled |
| `chaos_muhurat_day_seal` | non-standard market hours |
| `chaos_65h_idle_weekend` | sleep-until-open + boot rehydration |

---

## 🛡️ Known Limitation (documented honestly)

| What | Why | Impact | Workaround |
|---|---|---|---|
| Contracts expiring instantly at 15:30:00 IST | If Dhan moves them to `/v2/charts/expired` AT EXACTLY 15:30:00, our `/v2/charts/intraday` fetch returns empty | Those specific contracts may lack prev-day data on their final expiry day | Wave 7: dual-path fallback to `/v2/charts/expired` (pending Dhan support clarification) |
| ~10-50 contracts on weekly expiry day | Acceptable today (those contracts don't trade after expiry anyway) | ✅ Accept | — |

---

## 📅 Wave 7 Backlog (deferred from this plan)

| Item | Why deferred |
|---|---|
| Expiry-day fallback (`/v2/charts/expired` dual-path) | Pending Dhan support ticket clarification on roll-over timing |
| Bhavcopy triangulation (NSE+BSE 4-file fetch) | 1m fetch covers all primary needs; bhavcopy = nice-to-have 3rd source |
| BSE_EQ if added to subscription universe | Currently not subscribed; fetcher already segment-agnostic |
| NSE indices bhavcopy (`ind_close_all`) | Dhan WS code-6 already covers IDX_I prev_close + prev_OI |
| **Muhurat session boundary inclusion (deferred from Sub-PR #1 L-C4)** | AWS auto-stop 17:30 IST hard-conflicts with Muhurat 17:30–18:30. Needs EventBridge override + Muhurat-aware `TradingCalendar` boundary code + ratchet test (originally Sub-PR #1 ratchet #5). Once-a-year event, low risk to defer. |

---

## ✅ Status Tracker (one-glance)

| Sub-PR | Boxes done | Status |
|---|---|---|
| #1 Aggregator engine | 0/12 | 🔴 not started |
| #2 Rehydration | 0/12 | 🔴 not started |
| #3 Post-market 1m fetch + cross-verify + self-heal | 0/12 | 🔴 not started |
| #4 Promotion | 0/12 | 🔴 not started |

---

## 🔗 Cross-References

- `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row matrices, mandatory)
- `.claude/rules/project/wave-4-shared-preamble.md` (Section 7 honest 100% charter)
- `.claude/rules/project/observability-architecture.md` (5-sink + ErrorCode taxonomy)
- `.claude/rules/project/disaster-recovery.md` (boot modes + recovery primitives)
- `.claude/rules/project/historical-candles-cross-verify.md` (existing cross-verify infrastructure)
- `.claude/rules/project/security-id-uniqueness.md` (I-P1-11 composite key)
- `docs/dhan-ref/05-historical-data.md` (Dhan REST historical contract)
- `docs/dhan-ref/03-live-market-feed-websocket.md` (live-feed parsing)
- `docs/dhan-ref/08-annexure-enums.md` (ExchangeSegment + InstrumentType + rate limits)
- `quality/benchmark-budgets.toml` (Criterion p99 budgets)
- `quality/crate-coverage-thresholds.toml` (100% min per crate)

---

## 🎬 Next Action

When operator says "start sub-PR #1": next session creates branch `claude/wave-6-pr1-multi-tf-aggregator` from latest `main`, implements aggregator engine per items 1.1–1.7, runs adversarial 3-agent review (pre + post), fills 12-box done matrix, opens draft PR.

**This plan is the contract. Every sub-PR delivers against it. No silent scope creep.**
