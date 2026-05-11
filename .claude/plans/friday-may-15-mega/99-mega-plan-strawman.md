# MEGA PLAN — Friday May 15, 2026 — Three-Phase Sprint to Live Trading

**Status:** DRAFT
**Date:** 2026-05-11 (drafted Mon evening for Fri 06:00 IST execution start)
**Approved by:** pending — operator review required before flipping to APPROVED
**Authority chain:** CLAUDE.md > this file > per-wave-guarantee-matrix.md > defaults
**Drafted by:** Opus 4.7 (Mon May 11 18:00 IST session, after 10 parallel specialist research agents)
**Owner:** Parthiban (architect). Friday Claude Code session (builder).
**Execution window:** Fri 2026-05-15 06:00 IST → Thu 2026-05-21 23:59 IST (7 days, 168 hours wall-clock)

---

## EXECUTIVE SUMMARY (for the 1-minute reader)

This week takes the tickvault project from "live-streaming infrastructure with paper-only trading" to **"live trading capable, AWS-deployable, with the 100% guarantee envelope honestly ratcheted"**. Three phases run in strict order because each unblocks the next:

| Phase | Wall-clock | Focus | Exit gate |
|---|---|---|---|
| **Phase 0.5 — WS Disconnect Resilience Hardening** (NEW — added Mon 2026-05-11 21:00 IST) | Fri-Sat overlap | Close 8 of 22 documented disconnect scenarios. Promote 5 reserved ErrorCodes (NET-01 IP change, NET-02 DNS cascade, PROC-01 OOM, DH-911 Dhan black-hole, RESOURCE-03 disk-full) + add 3 NEW (`tv_ws_recv_window_bytes` gauge for TCP zero-window detection, TCP `SO_KEEPALIVE` enable, `tv_ws_recv_buffer_bytes` + `SO_RCVBUF` tuning). Verify 4-core pinning (Wave 5 Item 6) is wired in `main.rs`. | All 22 disconnect scenarios have detect+recover+alert+audit+ratchet coverage |
| **Phase 1 — Wave 6 close-out + 4 CRITICAL fixes** | Fri–Sat (Day 1–2) | Land PR #551, ship missing DHAT + Criterion + 6 chaos tests + 3-agent post-impl review for aggregator engine. Fix the 4 P0 silent-data-corruption bugs found by adversarial review. | Wave 6 Sub-PR #1 100% green on the 15-row guarantee matrix; 4 CRITICAL fixes merged |
| **Phase 2 — Wave 7-A4 wiring + Wave 6 Sub-PR #2 (rehydration)** | Sun–Mon (Day 3–4) | Wire BarCache into IndicatorEngine, ship boot rehydration loader, schedule midnight `clear_before`, fix `BarKey` cross-day collision, decide per-TF indicator architecture, ship live RSS gauge | Wave 7-A4 RAM-first hot path FUNCTIONAL (not aspirational); banned-pattern Cat 10 enforces against a populated cache |
| **Phase 3 — Trading observability + OMS go-live (paper→live cutover ready)** | Tue–Thu (Day 5–7) | 5 missing audit tables + 13 missing Prom alerts + 5 missing dashboards FIRST. Then wire OmsEngine in main.rs, hard-gate the 4 boot preconditions, hit `append_order_audit_row` on every state transition, add 15:30 IST cutoff + Dhan kill-switch sync. Defer Super/Forever/AMO/Slicing to Wave 8. | Mon 2026-05-25 09:15 IST first paper-equivalence live shadow session (NOT actual orders); first live order placement deferred to Wave 8 operator decision |

Concurrent track (runs alongside all phases): **AWS deployment readiness** — 4 blockers (ALB, SSM hierarchy, CloudWatch agent in user-data, Dhan `setIP` automation), Wave 6 Sub-PR #3 design decomposition into 4 slices, Wave-4-E1/E2/E3 reserved ErrorCodes promoted to shipped with chaos tests.

**Honest 100% claim (per per-wave-guarantee-matrix.md §8 mandatory wording):** This plan delivers "100% inside the tested envelope, with ratcheted regression coverage". Beyond-envelope behaviour falls back to DLQ NDJSON + kill-switch + halt. The plan does NOT promise literal "WebSocket never disconnects" or "QuestDB never fails".

---

## DEV ENVIRONMENT (operator's local Mac — same docker-compose runs on AWS)

| Spec | Value |
|---|---|
| Model | MacBook Pro (Mac16,7) Apple M4 Pro |
| CPU cores | 14 (10 Performance + 4 Efficiency) |
| RAM | 48 GB |
| Network | home WiFi (variable latency, possible IP roam) |

**Implication:** Mac OUT-SPECS the planned AWS c8g.xlarge (4 vCPU / 8GB). Mac is NOT the bottleneck for CPU/RAM. If Mac disconnects, cause is network/IP/power — fixed by AWS Mumbai EIP + UPS. Common-runtime principle (Mac dev = AWS prod) holds: same `docker-compose.yml`, same 4-core pinning scheme (Core 0 WS-recv-for-all-5-conns / Core 1 parser / Core 2 ILP writer / Core 3 other). On Mac, 10 cores stay idle (no harm). On AWS c8g.xlarge, all 4 cores used (perfect fit).

---

## PHASE 0.5 DETAIL — WS Disconnect Resilience Hardening (8 items)

(Inserted Mon 2026-05-11 21:00 IST per operator design review meeting #1 on WebSocket disconnect causes. Decisions captured in `00-decisions-log.md` entries #14-#17.)

The 22-scenario disconnect matrix has 8 unfixed gaps. The Byzantine/zombie audit (Mon 22:30 IST) added 24 more (G19-G46). The pre-market readiness flow audit (Mon 22:45 IST) confirmed PreMarketReady consolidator (0.5.10) is needed. The Telegram style audit (Mon 23:00 IST) added jargon-guard (0.5.19). Phase 0.5 grew from 9 items to **19 items**.

**Severity-sorted Phase 0.5 (19 items, ~1,770 LoC total):**

| # | Severity | Item | Existing code/reserved? | LoC | New ErrorCode |
|---|---|---|---|---|---|
| 0.5.13 | 🔥 **CRITICAL** | **Dual-instance live-lock** — boot reads `live_instance_lock` table; if other instance < 60s old → REFUSE boot (prevents 2× orders from Mac+AWS running together) | NEW (G38) | ~50 | RESILIENCE-01 |
| 0.5.1 | High (mid-market) | NET-01 promotion: IP-change-mid-session detection (60s poll vs boot baseline) | RESERVED (wave-4-error-codes.md) | ~150 | NET-01 |
| 0.5.2 | High | NET-02 promotion: DNS-cascade detection (3 consecutive failures targeting Dhan domains in 60s) | RESERVED | ~200 | NET-02 |
| 0.5.4 | High | DH-911 promotion: Dhan-side silent black-hole (subscribe accepted but zero packets in 60s) | RESERVED | ~150 | DH-911 |
| 0.5.5 | High | RESOURCE-03 promotion: disk-full pre-flight at boot AND before every spill write | partial (boot only) | ~100 | RESOURCE-03 |
| 0.5.6 | High | NEW: `tv_ws_recv_window_bytes` gauge — exposes TCP zero-window state BEFORE Dhan RSTs | not existing | ~150 | WS-BACKPRESSURE-01 |
| 0.5.9 | High | NEW (verification): confirm Wave 5 Item 6 `core_pinning::pin_workers` is WIRED in `main.rs` boot | helper exists | ~80 | CORE-PIN-01/02 |
| 0.5.14 | High | **Tokio deadlock liveness probe** — tests business logic, not just /health 200 (G19) | NEW | ~100 | LIVENESS-01 |
| 0.5.15 | High | **TCP-flow watchdog per-conn** — last-byte-age alert (G22 — catches "TCP open, zero data") | NEW | ~80 | WS-FLOW-01 |
| 0.5.16 | High | **Subscribe-ACK parser** — verify Dhan actually accepted subscribe (G24 — catches silently-lost subscribe messages) | NEW | ~120 | WS-SUB-01 |
| 0.5.17 | High | **Cross-validation (2-source agreement)** — critical metrics verified by 2 independent paths (G35) | NEW | ~150 | TELEMETRY-01 |
| 0.5.18 | High | **Mid-rebalance replay from subscription_audit** — recover unfinished Swap20/Swap200 on boot (G39) | NEW | ~100 | RESILIENCE-02 |
| 0.5.3 | Medium | PROC-01 promotion: OOM kill watcher (`/sys/fs/cgroup/.../memory.events`) | RESERVED | ~250 | PROC-01 |
| 0.5.8 | Medium | NEW: `tv_ws_recv_buffer_bytes` gauge + `SO_RCVBUF` tuning to 4 MB | not existing | ~80 | WS-BACKPRESSURE-02 |
| 0.5.11 | Medium | **Renewal-task supervisor** — wraps `spawn_renewal_task` in respawn loop (G3) | NEW | ~80 | AUTH-SUPER-01 |
| 0.5.12 | Medium | **Pool-supervisor-supervisor** — supervises the pool supervisor (G9 — paranoia tier) | NEW | ~60 | WS-SUPER-01 |
| 0.5.7 | Low | NEW: TCP `SO_KEEPALIVE` enable on every WS socket (catches NAT timeout) | not existing | ~40 | (config only) |
| 0.5.10 | Low | **PreMarketReady consolidated Telegram** — replaces 6 fragmented messages with 1 (Item 0.5.10) | NEW | ~250 | (notification only) |
| 0.5.19 | Low | **Telegram jargon guard** — banned-word scanner fails build if Telegram message contains library jargon (G47) | NEW | ~80 | (test only) |

**Sub-totals by severity:**
- 🔥 Critical: 1 item, 50 LoC
- High: 11 items, 1,380 LoC
- Medium: 4 items, 470 LoC
- Low: 3 items, 370 LoC
- **TOTAL: 19 items, ~1,770 LoC**

**Each item gets its own task file (`tasks/T05-NN-*.md`) on Wed during execution-planning sprint.** Today (Mon) we only LOCK the scope; concrete task files written Wed once dependencies + Touches scope finalized.

**Friday distribution (5 parallel Claude sessions):**
- Session 1: 0.5.13 (CRITICAL — must close first) + 0.5.14 + 0.5.15 = 230 LoC
- Session 2: 0.5.16 + 0.5.17 + 0.5.18 = 370 LoC
- Session 3: 0.5.1 + 0.5.2 + 0.5.4 + 0.5.9 = 580 LoC
- Session 4: 0.5.3 + 0.5.5 + 0.5.6 + 0.5.11 + 0.5.12 = 540 LoC
- Session 5: 0.5.7 + 0.5.8 + 0.5.10 + 0.5.19 = 450 LoC (front-loaded with the PreMarketReady consolidator since it's user-visible)

**The 22-scenario coverage table after Phase 0.5 lands:** see `02-step-1-ws-disconnect-resilience.md` (will be created Tue when we deep-dive each scenario).

---

## TABLE OF CONTENTS

1. [Section A — Pre-flight: what the Friday session must do in the first 30 minutes](#section-a)
2. [Section B — Phase 1: Wave 6 close-out + 4 CRITICAL fixes (Fri–Sat)](#section-b)
3. [Section C — Phase 2: Wave 7-A4 wiring + Wave 6 Sub-PR #2 (Sun–Mon)](#section-c)
4. [Section D — Phase 3: Trading observability + OMS go-live (Tue–Thu)](#section-d)
5. [Section E — Concurrent track: AWS deployment readiness (all 7 days)](#section-e)
6. [Section F — Concurrent track: chaos + fuzz + mutation gaps (all 7 days)](#section-f)
7. [Section G — Per-item 9-box checklist template](#section-g)
8. [Section H — 15-row + 7-row guarantee matrix (mandatory per per-wave-guarantee-matrix.md)](#section-h)
9. [Section I — Kid-friendly system diagram (ASCII)](#section-i)
10. [Section J — Risks, blockers, rollback procedures](#section-j)
11. [Appendix A — Consolidated research findings from 10 parallel agents (2026-05-11)](#appendix-a)
12. [Appendix B — File index: every file Friday session needs to read first](#appendix-b)
13. [Appendix C — ErrorCode budget (variants to add this week)](#appendix-c)

---

<a id="section-a"></a>
## SECTION A — PRE-FLIGHT (Friday session first 30 minutes)

The Friday session MUST execute this checklist BEFORE writing any code. The Wave 4 shared preamble Section 1 mandates it, and most automation tools self-load only when called.

| # | Step | Tool | Pass criterion | If fails |
|---|---|---|---|---|
| 1 | `git pull origin claude/trading-tick-vault-BkvpS` | Bash | clean fast-forward | resolve conflicts before touching anything |
| 2 | Read CLAUDE.md, this file, `wave-4-shared-preamble.md`, `per-wave-guarantee-matrix.md` | Read | full read | n/a |
| 3 | `mcp__tickvault-logs__run_doctor` | MCP | 7 sections all GREEN | follow specific runbook per RED section |
| 4 | `mcp__tickvault-logs__summary_snapshot` lookback=1h | MCP | no novel ERROR signatures | run `find_runbook_for_code` for each novel signature |
| 5 | `mcp__tickvault-logs__list_active_alerts` | MCP | array empty `[]` | inspect each firing alert before proceeding |
| 6 | `bash .claude/hooks/session-auto-health.sh` | Bash | exit 0 | follow log path printed |
| 7 | `make validate-automation` (20 checks) | Bash | 20/20 pass | fix specific check before code |
| 8 | `git log --oneline -30 origin/main..HEAD` | Bash | confirm branch ahead of main + understand recent ratchets | n/a |
| 9 | Confirm Wave 6 PR #551 merge status via `mcp__github__pull_request_read` | MCP | merged | if still review-pending, close it out FIRST as Step 1 of Phase 1 |
| 10 | `cat .claude/plans/active-plan-friday-may-15-mega.md` (this file) | Read | Status: APPROVED (or APPROVED-WITH-CHANGES if operator amended Mon) | refuse to start if Status: DRAFT |
| 11 | Flip Status: APPROVED → IN_PROGRESS, set `Approved by:` to operator's name | Edit | n/a | n/a |
| 12 | Create todo list mirroring Section B/C/D/E/F items | TodoWrite | matches plan | n/a |

**Hard rule:** if any of steps 3–7 fails, the session HALTS and Telegrams the operator. Do NOT proceed to phase work with a RED automation chain — that is exactly the "automation-first rule" CLAUDE.md mandates.

---

<a id="section-b"></a>
## SECTION B — PHASE 1: Wave 6 close-out + 4 CRITICAL fixes (Fri–Sat, Days 1–2)

**Goal:** Wave 6 Sub-PR #1 (aggregator engine) reaches 100% on the 15-row guarantee matrix. The 4 CRITICAL bugs from adversarial review (Appendix A.10) are merged. Wave 6 Sub-PR #2 (rehydration) design is ready for Phase 2 execution.

### B.1 — Land Wave 6 PR #551 if still open (FIRST priority, may take ≤2h)

| Sub-item | Action | Files |
|---|---|---|
| B.1.a | If PR #551 is "ready for review" → run the post-impl 3-agent adversarial pass on the diff. Spawn `hot-path-reviewer`, `security-reviewer`, `general-purpose` (hostile bug-hunt). Each writes a 400-word report. Resolve every CRITICAL + HIGH inline. | n/a |
| B.1.b | After agents return, `make scoped-check` + push fixes if any. | per agent findings |
| B.1.c | Merge PR #551 via `mcp__github__merge_pull_request` | n/a |
| B.1.d | Rebase `claude/trading-tick-vault-BkvpS` onto main; resolve any conflicts | n/a |

### B.2 — FIX-NOW: 4 CRITICAL P0 bugs from adversarial review

These MUST land before any further Wave 7-A4 work because they invalidate the RAM-first guarantee. All 4 must be in ONE focused PR titled "fix(critical): 4 P0 bugs in BarCache + prev_day_cache_loader" so they merge atomically.

| # | Bug | File:line | Fix | Ratchet test |
|---|---|---|---|---|
| B.2.1 | **`BarKey` cross-day collision** — `bucket_start_ist_secs` is seconds-of-day (0..86400), NOT epoch. After midnight, today's 09:00 bar overwrites yesterday's 09:00 bar at the SAME key. Silent data loss → indicator reads YESTERDAY when querying TODAY. | `crates/trading/src/in_mem/bar_cache.rs:68` | Widen key to `BarKey { security_id, segment, tf, trading_date_ist, bucket_start_secs_of_day }`. Alternative: dual `today`/`yesterday` maps swapped atomically at midnight. | `test_bar_cache_no_cross_day_collision` — insert at 09:00 today + 09:00 tomorrow, assert both retrievable distinctly. |
| B.2.2 | **BarCache never instantiated in `main.rs`** — Wave 7-A4 hot-path is non-functional today. | `crates/app/src/main.rs` — needs new section after `populate_and_log` (~line 3219) | Wire `Arc::new(BarCache::with_capacity(...))` → pass to async loader → pass to seal closure as 2nd consumer → pass to IndicatorEngine constructor. | `test_main_boot_instantiates_bar_cache` (source-scan guard like `health_counter_fix7_guard.rs`). |
| B.2.3 | **`clear_before` never scheduled** — unbounded cache growth → OOM blows 2GB cap within a week. | `crates/app/src/main.rs` boot wiring | Spawn IST-midnight tokio task calling `bar_cache.clear_before(yesterday_start_secs)` via `spawn_blocking` (papaya scan is expensive). Cap completion ≤ 03:00 IST. | `test_clear_before_scheduled_at_ist_midnight` source-scan guard + a property test that after `clear_before(t)`, all rows have `trading_date_ist >= t`. |
| B.2.4 | **`prev_day_cache_loader.rs:90` uses UTC `now()` with `dateadd('d', -7, now())`** — comments claim IST-anchored window. Sunday 22:00 UTC = Mon 03:30 IST; host clock skew at BOOT-03's 2s threshold could miss Friday's row. | `crates/app/src/prev_day_cache_loader.rs:90` | Compute the IST-anchored 7-trading-day cutoff in Rust (use `TradingCalendar`) and pass it as a bind parameter. Reject rows older than most-recent NSE trading day. | `test_prev_day_cache_query_uses_ist_anchored_cutoff` — assert SQL contains the bind, not `now()`. |

**Per-item 9-box must be filled** (template in §G). All 4 ratchet tests must be green before merge. The PR description MUST cite the adversarial-review session and the agent's verbatim severity rating.

### B.3 — Ship Wave 6 Sub-PR #1 missing pieces (close the 15-row matrix)

| Sub-item | Item from W6 plan | Files | LoC | 9-box gate |
|---|---|---|---|---|
| B.3.1 | Item 1.6 — DHAT zero-alloc + Criterion split p99 budgets (≤100ns no-seal tick / ≤1µs with-seal tick / ≤5ms force_seal_all of 99K seals) | `crates/trading/benches/multi_tf_aggregator.rs` (new), `crates/trading/tests/dhat_multi_tf_aggregator.rs` (new), `quality/benchmark-budgets.toml` (add 3 entries) | ~250 | DHAT + Criterion both green; bench-gate ≤5% regression |
| B.3.2 | Item 1.7 — proptest 9-TF rollup (1m × N → Nm equivalence under random tick sequences) | `crates/trading/tests/proptest_multi_tf_rollup.rs` (new) | ~200 | 100 cases per TF, no shrink failures |
| B.3.3 | 6 chaos tests reserved for Sub-PR #1 — crash/timeout/corruption/30m-burst/Muhurat/65h-idle | `crates/trading/tests/chaos_*.rs` (6 new) | ~600 | each chaos test asserts functional recovery AND wall-clock RTO bound |
| B.3.4 | Update `wave-6-error-codes.md` to mark AGGREGATOR-* codes shipped + add 14 new codes for Sub-PR #2/#3 (REHYDRATE-01..05, BUFFER-GATE-01..02, POSTMARKET-*, VERIFY-*) — UPFRONT so cross-ref test passes throughout the week | `.claude/rules/project/wave-6-error-codes.md` | ~300 | cross-ref test green |
| B.3.5 | Post-impl 3-agent adversarial review on cumulative Wave 6 Sub-PR #1 diff | n/a | review only | every CRITICAL/HIGH fixed inline before close |

**Phase 1 exit gate:** Sub-PR #1 100% complete; the 15-row matrix has zero red cells for Wave 6 work; the 4 CRITICAL P0 bugs are merged; Wave 6 Sub-PR #2 design (next section) is ready.

### B.4 — Pre-design Wave 6 Sub-PR #2 (Rehydration on boot) for Phase 2 execution

| Sub-item | Action |
|---|---|
| B.4.1 | Read the Sub-PR #2 section of `active-plan-aggregator-direct-flush-rehydrate.md`. List the 6 sub-items + 7 ErrorCodes (REHYDRATE-01..05, BUFFER-GATE-01..02). |
| B.4.2 | Spawn pre-impl 3-agent design review on the proposed Sub-PR #2 scope. Each agent reports under 400 words. Surface design questions back to operator if any agent flags ambiguity. |
| B.4.3 | Decompose into 2 commits: (a) `wave_6_rehydration_loader` (boot replay + buffer gate, ~600 LoC), (b) `wave_6_rehydration_fail_closed_halt` (HALT path if rehydration incomplete by market open, ~200 LoC). |

---

<a id="section-c"></a>
## SECTION C — PHASE 2: Wave 7-A4 wiring + Wave 6 Sub-PR #2 (Sun–Mon, Days 3–4)

**Goal:** Wave 7-A4 transitions from "primitives exist" to "RAM-first hot path is REAL and tested". Wave 6 Sub-PR #2 ships boot rehydration so the cache is populated at boot.

### C.1 — Ship W7-A4.5 (DHAT zero-alloc + Criterion + IndicatorEngine wiring)

| Sub-item | Action | Files | Ratchet |
|---|---|---|---|
| C.1.1 | Add `crates/trading/benches/bar_cache.rs` measuring `BarCache::get` (≤30ns p99) + `upsert` (≤100ns p99). Add entries in `quality/benchmark-budgets.toml`. | new + edit | bench-gate ≤5% regression |
| C.1.2 | Add `crates/trading/tests/dhat_bar_cache.rs` asserting 0 allocs steady-state across 10K `get`/`upsert` cycles. | new | hard fail on any block |
| C.1.3 | Wire `Arc<BarCache>` into `IndicatorEngine::new` constructor. Add `IndicatorEngine::update_from_bar(tf: TfIndex, bar: &CompactBar)` invoked on seal events. | `crates/trading/src/indicator/engine.rs`, `crates/trading/src/aggregator/multi_tf.rs` | new unit test asserts indicator state updates when seal hook fires |
| C.1.4 | Add `crates/trading/benches/indicator_update.rs` for the new bar-driven path. ≤200ns p99 per `update_from_bar` call. | new | bench-gate |
| C.1.5 | Post-impl 3-agent review on W7-A4.5 + B.2 cumulative diff. | n/a | every CRITICAL/HIGH fixed |

### C.2 — Decide per-TF indicator architecture (BLOCKING for any timeframe-aware strategy)

Two options. Friday session MUST present both to operator via `AskUserQuestion` and proceed only after answer.

| Option | Description | Pros | Cons |
|---|---|---|---|
| **A** — `update_from_bar(tf, bar)` on single shared engine | Engine carries per-(security_id, tf) state internally | Simpler memory layout; one update call per seal | State map keyed by `(security_id, tf)` — extra hash op per seal |
| **B** — `[IndicatorEngine; 9]` array indexed by `TfIndex` | One engine per TF | Zero per-tick hash collision risk | 9× memory baseline; harder hot reload |

**Recommended: Option A** (lower memory, hot-reload-friendly). The hash op is `papaya.get` which is 50ns budget — already paid for in registry lookups.

### C.3 — Live RSS gauge + alert (Wave 7-A4 final ratchet)

| Sub-item | Action | Files |
|---|---|---|
| C.3.1 | Add Prom gauge `tv_app_rss_bytes` (read from `/proc/self/statm` every 30s; cold path) | `crates/app/src/main.rs` background task |
| C.3.2 | Add gauge `tv_questdb_rss_bytes` (scrape Docker stats API every 30s) | same |
| C.3.3 | Add alert `tv-app-rss-above-2gb` (`> 2_000_000_000` for 5m) + `tv-questdb-rss-above-1500mb` (`> 1_500_000_000` for 5m) | `deploy/docker/grafana/provisioning/alerting/alerts.yml` |
| C.3.4 | Add panel to `operator-health.json` showing both gauges + 2GB / 1.5GB ceiling lines | edit |
| C.3.5 | Ratchet test in `resilience_sla_alert_guard.rs` pinning both alert rules | edit |

### C.4 — Wave 6 Sub-PR #2 execution (boot rehydration loader + fail-closed HALT)

| Sub-item | Action | Files | LoC est |
|---|---|---|---|
| C.4.1 | Async wrapper around `bar_cache_loader::merge_into_cache` — issues actual HTTP GET against QuestDB `/exec` for the 9 shadow tables UNION ALL (most-recent 2 trading days), populates BarCache. | new fn in `bar_cache_loader.rs` + boot wiring in `main.rs` | ~250 |
| C.4.2 | Buffer-gate primitive — ticks arriving during rehydration window land in a bounded buffer; gate releases when rehydration completes. | `crates/trading/src/aggregator/buffer_gate.rs` (new) | ~200 |
| C.4.3 | Fail-closed HALT — if rehydration NOT complete by 09:00 IST, app HALTS (refuses to start the trading pipeline). | `crates/app/src/main.rs` boot wiring | ~50 |
| C.4.4 | 7 new ErrorCodes added to `wave-6-error-codes.md`: REHYDRATE-01..05, BUFFER-GATE-01..02. Severity assignments per the runbook stubs already drafted in B.3.4. | edit | n/a |
| C.4.5 | 5 unit + 3 integration + 1 chaos test (mid-boot crash recovery) | `crates/trading/tests/wave_6_rehydration_*.rs` (new) | ~400 |
| C.4.6 | Pre-impl + post-impl 3-agent review | n/a | n/a |

**Phase 2 exit gate:** BarCache is wired end-to-end (loader → cache → seal closure → indicator); `make doctor` reports BarCache populated within 60s of boot; live RSS gauge proves app stays under 2GB across a full market day simulation; Wave 6 Sub-PR #2 merged.

---

<a id="section-d"></a>
## SECTION D — PHASE 3: Trading observability + OMS go-live (Tue–Thu, Days 5–7)

**Goal:** The trading-critical observability gaps close BEFORE the OMS engine is instantiated in `main.rs`. Then OMS goes live behind a `dry_run=false` config flip with 4 hard-gated boot preconditions. First live order is NOT placed this week — Mon 2026-05-25 09:15 IST is the earliest shadow-equivalence session (paper-vs-live diff with `dry_run=true` still set). Actual live order placement is deferred to Wave 8 operator decision.

### D.1 — Trading observability sprint (Tue, Day 5) — MUST land before D.2

| # | Audit table | DEDUP key | SEBI retention | Trigger | File |
|---|---|---|---|---|---|
| D.1.1 | `risk_decision_audit` | `(correlation_id, check_name, ts)` | 5y | every pre-trade check arm | new `crates/storage/src/risk_decision_audit_persistence.rs` (~250 LoC) |
| D.1.2 | `pnl_snapshot_audit` | `(trading_date_ist, ts)` | 5y | per-minute P&L | new `crates/storage/src/pnl_snapshot_audit_persistence.rs` (~200 LoC) |
| D.1.3 | `kill_switch_audit` | `(event_ts, trigger)` | 5y | activate/deactivate | new `crates/storage/src/kill_switch_audit_persistence.rs` (~150 LoC) |
| D.1.4 | `strategy_decision_audit` | `(strategy_id, ts)` | 5y | every FSM signal | new `crates/storage/src/strategy_decision_audit_persistence.rs` (~200 LoC) |
| D.1.5 | `position_lifecycle_audit` | `(correlation_id, transition)` | 5y | open/modify/close | new `crates/storage/src/position_lifecycle_audit_persistence.rs` (~250 LoC) |

**Each audit table** must have: schema-self-heal `ALTER TABLE ADD COLUMN IF NOT EXISTS`; `dedup_segment_meta_guard.rs` scan passes; new ErrorCode `AUDIT-07..11` registered in `wave-8-error-codes.md` (new file) with runbook stubs.

| # | Prometheus alert | Expression | Severity | File |
|---|---|---|---|---|
| D.1.6 | `tv-orders-place-failure-rate-high` | `rate(tv_orders_failed_total[1m]) > 0.0167` (1/min) | High | alerts.yml |
| D.1.7 | `tv-risk-pre-trade-rejection-storm` | `rate(tv_risk_rejections_total[5m]) > 0.1` | High | alerts.yml |
| D.1.8 | `tv-daily-loss-limit-breach` | `tv_daily_pnl < -tv_daily_loss_limit` | Critical | alerts.yml |
| D.1.9 | `tv-kill-switch-tripped` | `tv_kill_switch_active == 1` | Critical | alerts.yml |
| D.1.10 | `tv-position-limit-breach` | `tv_position_count > tv_position_count_limit` | High | alerts.yml |
| D.1.11 | `tv-margin-shortfall-detected` | `tv_margin_insufficient_total > 0` increase[1m] | High | alerts.yml |
| D.1.12 | `tv-static-ip-orders-not-allowed` | `tv_ip_orders_allowed == 0` for 1m | Critical | alerts.yml |
| D.1.13 | `tv-oms-illegal-state-transition` | `increase(tv_oms_illegal_transitions_total[1m]) > 0` | Critical | alerts.yml |
| D.1.14 | `tv-oms-reconciliation-drift` | `tv_oms_reconciliation_drift > tv_oms_reconciliation_epsilon` | High | alerts.yml |
| D.1.15 | `tv-circuit-breaker-open` | `tv_oms_circuit_breaker_state == 2` for 30s | High | alerts.yml |
| D.1.16 | `tv-idempotency-key-collision` | `increase(tv_oms_idempotency_collisions_total[1m]) > 0` | Critical | alerts.yml |
| D.1.17 | `tv-strategy-hot-reload-failed` | `increase(tv_strategy_reload_failures_total[5m]) > 0` | High | alerts.yml |
| D.1.18 | `tv-pnl-exit-config-missing` | `tv_pnl_exit_configured == 0` for 1m during market hours | High | alerts.yml |

All 13 alerts pinned by `resilience_sla_alert_guard.rs`.

| # | Dashboard | Panels |
|---|---|---|
| D.1.19 | `trading-pnl.json` | realized P&L, unrealized P&L, equity curve, daily loss vs limit, per-strategy P&L breakdown |
| D.1.20 | `order-book.json` | open orders by state, modify count distribution, order-age histogram, reject reason pie chart |
| D.1.21 | `risk-engine.json` | pre-trade pass/fail rate per check arm, kill-switch state, position count by symbol, margin headroom |
| D.1.22 | `strategy-decisions.json` | signals/min per strategy, FSM state distribution, hot-reload count, dry-run vs live mode indicator |
| D.1.23 | `position-tracker.json` | open positions table, net qty per symbol, MTM, carry-forward |

All 5 dashboards pinned by extending `operator_health_dashboard_guard.rs` into a new `trading_dashboards_guard.rs`.

| # | Tracing `#[instrument]` additions | Files |
|---|---|---|
| D.1.24 | 6 parser fns | `crates/core/src/parser/{ticker,quote,full,depth,oi,prev_close}.rs` |
| D.1.25 | `pipeline/tick_processor.rs` hot loop entry | edit |
| D.1.26 | Every OMS state transition arm | `crates/trading/src/oms/state_machine.rs` |
| D.1.27 | Every risk check arm | `crates/trading/src/risk/engine.rs` |
| D.1.28 | Strategy FSM evaluator | `crates/trading/src/strategy/evaluator.rs` |
| D.1.29 | Meta-guard: ratchet test that fails build if listed hot-path fns drop their span | new `tracing_span_coverage_guard.rs` |

### D.2 — OMS engine integration (Wed, Day 6)

| Sub-item | Action | Files | LoC est |
|---|---|---|---|
| D.2.1 | Wire `OmsEngine::new` in `main.rs` between greeks pipeline + API server, behind single `[strategy].dry_run = false` switch. Strategy signal → RiskEngine.check_order → OmsEngine.place_order → audit. | `crates/app/src/main.rs` | ~400 |
| D.2.2 | Hard-gate the 4 boot preconditions: (a) `ip_verifier.get_ip` `ordersAllowed=true` (CRITICAL on false), (b) `configure_pnl_exit` succeeds, (c) margin probe with dummy request succeeds, (d) Valkey idempotency store reachable. Any fail → force `dry_run=true` + CRITICAL Telegram + halt. | `crates/app/src/main.rs` | ~150 |
| D.2.3 | Hit `append_order_audit_row` on every state transition (place, modify, cancel, OrderUpdate WS message). | `crates/trading/src/oms/engine.rs`, `crates/core/src/websocket/order_update.rs` | ~100 |
| D.2.4 | Add 15:30 IST hard cutoff in `RiskEngine::check_order` — rejects orders after 15:29:30 IST. | `crates/trading/src/risk/engine.rs` | ~30 |
| D.2.5 | Add local-halt → Dhan kill-switch sync — `manual_halt()` invokes `api_client.activate_kill_switch()` with retry. | same | ~50 |
| D.2.6 | Audit + fix daily rate limit constant — verify it's 7000 (Dhan v2.3) not 5000. Add ratchet test. | `crates/trading/src/oms/rate_limiter.rs` | ~50 |
| D.2.7 | Persist OMS idempotency UUID to Valkey BEFORE submit (not just in-memory). | `crates/trading/src/oms/engine.rs` | ~80 |

### D.3 — Pre-go-live safety harness (Wed–Thu, Day 6–7)

| Sub-item | Action | Files | LoC est |
|---|---|---|---|
| D.3.1 | Add chaos test `chaos_order_reply_loss.rs` — synthetic crash between Dhan REST send and local idempotency-key persist. Asserts UUID-based dedup prevents double-submit on restart. Pins IDEMP-01. | `crates/trading/tests/` | ~250 |
| D.3.2 | Add fuzz target `oms_state_machine` — random transition sequences must NEVER produce an illegal transition without `Err(...)` return. | `fuzz/fuzz_targets/oms_state_machine.rs` | ~100 |
| D.3.3 | Add fuzz target `ilp_line_builder` — random tick payloads must never produce a malformed ILP line (escape `"` and `\`). Closes the security-reviewer LOW finding. | `fuzz/fuzz_targets/ilp_line_builder.rs` + `crates/common/src/sanitize.rs` extension | ~150 |
| D.3.4 | Add shadow-equivalence harness — run `dry_run=true` strategy alongside `dry_run=false` (when flag flipped); diff every decision. ANY divergence → CRITICAL Telegram + halt. | `crates/trading/src/oms/shadow_diff.rs` (new) | ~300 |
| D.3.5 | Add HTTP rate limiting to API server — `tower_http::limit` or `governor` middleware on per-IP basis. Closes security-reviewer MEDIUM finding. | `crates/api/src/middleware.rs` | ~100 |

### D.4 — `set_ip` automation + S3 lifecycle policy

| Sub-item | Action | Files |
|---|---|---|
| D.4.1 | Add call site for `ip_verifier::set_ip` — invoked post-`terraform apply` via a separate `scripts/aws/set-dhan-static-ip.sh` script with 7-day cooldown awareness. | new script + integration test |
| D.4.2 | Add S3 lifecycle policy in `deploy/aws/terraform/s3-lifecycle.tf` — `order_audit` prefix → Glacier Deep Archive after 1 year, 5-year minimum retention. Per AWS budget rule 5. | new terraform file |
| D.4.3 | Add `scripts/aws/verify-sebi-retention.sh` — periodic audit script (run weekly) that lists S3 `order_audit` objects and verifies lifecycle class. | new script |

**Phase 3 exit gate (end of Thursday):** all 5 audit tables live; 13 alerts firing on synthetic test data; 5 dashboards rendered; OMS engine instantiated with `dry_run=true` (NOT live); shadow-equivalence harness ready; `set_ip` automation script tested in dev; `make doctor` reports a NEW section "live trading readiness" showing 4 boot preconditions GREEN; the cumulative 3-agent post-impl review on the Phase-3 diff is clean.

**Mon 2026-05-25 09:15 IST first action:** operator decides whether to flip `dry_run=false` for the first shadow-equivalence session (NOT actual orders). First actual live order is a Wave 8 operator decision — this plan does NOT promise it.

---

<a id="section-e"></a>
## SECTION E — CONCURRENT TRACK: AWS deployment readiness (all 7 days)

Runs in background, doesn't block Phases 1–3 but must complete by Thu EOD for Mon shadow session.

| # | Item | Files | Notes |
|---|---|---|---|
| E.1 | Add `aws_lb` + target group + listener + security group in `deploy/aws/terraform/alb.tf` (NEW). HTTPS termination via ACM cert. Free tier 750 hrs/mo. | new file | replaces Traefik per aws-budget.md rule 8 |
| E.2 | Add `aws_ssm_parameter` resources for: Dhan client-id, TOTP secret, JWT cache key, Telegram bot token + chat-id, Grafana admin pw, Valkey pw. Hierarchy: `/tickvault/<env>/<service>/<key>`. | `deploy/aws/terraform/ssm.tf` (new) | use `SecureString` for secrets |
| E.3 | Install CloudWatch agent in `user-data.sh.tftpl` — config pulls from SSM parameter `/tickvault/<env>/cloudwatch/config`. Log groups: `/tickvault/app`, `/tickvault/errors`, `/tickvault/audit`. | edit | per aws-budget.md rule 4 — free tier 5GB |
| E.4 | Write `scripts/aws/dhan-set-ip.sh` — runs after `terraform apply`, calls `POST /v2/ip/setIP` with the Elastic IP. Idempotent — checks current IP first via `getIP`. 7-day cooldown documented in script comments. | new script | per security-review MEDIUM #1 |
| E.5 | Dry-run `terraform plan` in a separate test workspace, capture output, append to plan for operator review. Do NOT `apply`. | n/a | operator approves apply Mon 2026-05-25 |
| E.6 | Add CloudWatch alarms for: `tv_app_rss_bytes > 2GB`, `tv_questdb_rss_bytes > 1.5GB`, EC2 instance status check fail, EBS gp3 IOPS exhaustion, SNS message failure rate. | `deploy/aws/terraform/alarms.tf` (extend) | already 5 alarms exist per Wave 7-A4 research |
| E.7 | Add OIDC for GitHub Actions deploy (already exists per research) — verify the policy attached covers only `tickvault-*` resources (least privilege). | `deploy/aws/terraform/oidc.tf` audit | n/a |

---

<a id="section-f"></a>
## SECTION F — CONCURRENT TRACK: chaos + fuzz + mutation gaps (all 7 days)

Runs interleaved with Phases 1–3. Each item < 1 day work.

### F.1 — Chaos tests (12 to ship this week)

| # | Chaos test | File (new) | Pins ErrorCode |
|---|---|---|---|
| F.1.1 | 92h holiday weekend (W6-2 backlog) — Fri close → Tue post-holiday open | `crates/core/tests/chaos_holiday_92h_sleep.rs` | extends ws_sleep_resilience |
| F.1.2 | Trading-layer OMS under tick storm | `crates/trading/tests/chaos_oms_tick_storm.rs` | OMS-GAP-* family |
| F.1.3 | Trading-layer risk engine under burst | `crates/trading/tests/chaos_risk_burst.rs` | RISK-GAP-* |
| F.1.4 | Trading-layer strategy decision under late ticks | `crates/trading/tests/chaos_strategy_late_ticks.rs` | new STRATEGY-01 |
| F.1.5 | Triple-failure cascade (WS + QDB + token, 60s window) | `crates/core/tests/chaos_cascade_01.rs` | CASCADE-01 (Wave-4-E3 reserved) |
| F.1.6 | Dual-instance race (same dhanClientId) | `crates/storage/tests/chaos_dual_instance.rs` | RESILIENCE-01 (Wave-4-E3 reserved) |
| F.1.7 | Mid-rebalance crash recovery — replay from `subscription_audit` | `crates/core/tests/chaos_rebalance_crash.rs` | RESILIENCE-02 |
| F.1.8 | OOM-killed mid-session (cgroup oom_kill increment) | `crates/app/tests/chaos_oom_kill.rs` | PROC-01 (Wave-4-E1 reserved) |
| F.1.9 | Container restart loop detection | `crates/app/tests/chaos_container_restart_loop.rs` | PROC-02 |
| F.1.10 | DNS cascade (3 consecutive failures targeting Dhan domains) | `crates/core/tests/chaos_dns_cascade.rs` | NET-02 |
| F.1.11 | Static IP changed mid-session | `crates/core/tests/chaos_ip_changed_mid_session.rs` | NET-01 |
| F.1.12 | Order placed but reply lost (covered separately in D.3.1) | — | IDEMP-01 |

### F.2 — Fuzz targets (3 to ship)

| # | Target | File |
|---|---|---|
| F.2.1 | OMS state machine (covered in D.3.2) | `fuzz/fuzz_targets/oms_state_machine.rs` |
| F.2.2 | ILP line builder (covered in D.3.3) | `fuzz/fuzz_targets/ilp_line_builder.rs` |
| F.2.3 | Subscription planner CSV / FnoUniverse | `fuzz/fuzz_targets/subscription_planner_csv.rs` (~150 LoC) |

### F.3 — Mutation gate extension

| # | Action |
|---|---|
| F.3.1 | One-line edit to `.github/workflows/mutation.yml` `paths:` block — add `crates/storage/**`. Closes DEDUP UPSERT KEY + 6 audit-writer mutation gap. |

### F.4 — Bench gate hardening

| # | Action |
|---|---|
| F.4.1 | Add budget entries in `quality/benchmark-budgets.toml` for `prev_close_writer` and `tick_parser` (currently bypass the gate). |
| F.4.2 | Commit `quality/benchmark-baseline.json` snapshot so regression detection works on cold CI runners. |
| F.4.3 | Promote `scripts/bench-gate.sh` from post-merge to PR-blocking for `core` + `trading` crates (touched by every Wave 6 sub-PR). |

### F.5 — Wave-4-E reserved ErrorCodes promoted to shipped (5 codes)

| # | Code | Implementation |
|---|---|---|
| F.5.1 | PROC-01 (OOM kill watcher) | new `crates/app/src/oom_monitor.rs` reading `/sys/fs/cgroup/.../memory.events` |
| F.5.2 | NET-01 (IP changed mid-session) | extend `ip_monitor.rs` with 60s poll vs baseline |
| F.5.3 | DH-911 (Dhan API silent black-hole) | per-segment tick-gap alert extending WS-GAP-06 |
| F.5.4 | RESILIENCE-01 (dual-instance lock) | new `live_instance_lock` QuestDB table with DEDUP UPSERT KEYS(client_id) |
| F.5.5 | CLOCK-DRIFT-01 (mid-session clock skew) | extend BOOT-03 with periodic 60s chrony probe |

Each promotion: ErrorCode variant + runbook section in appropriate `wave-*-error-codes.md` + chaos test (overlaps with F.1) + tag-guard meta-test passes.

---

<a id="section-g"></a>
## SECTION G — PER-ITEM 9-BOX CHECKLIST TEMPLATE (mandatory per stream-resilience.md B8)

Every plan item (B.x, C.x, D.x, E.x, F.x) MUST tick all 9 boxes BEFORE the next item starts:

```
[ ] 1. Typed NotificationEvent variant added (if user-visible)
[ ] 2. ErrorCode variant added + runbook section in wave-*-error-codes.md
[ ] 3. tracing macros use `error!`/`warn!`/`info!` with `code = ErrorCode::X.code_str()` field
[ ] 4. Prometheus counter / gauge added with static labels
[ ] 5. Grafana panel renders the counter (wrapped in increase()/rate() per Rule 12)
[ ] 6. Prometheus alert rule in alerts.yml (gated by market-hours if applicable per Rule 3, edge-triggered per Rule 4)
[ ] 7. Call site exists (pub-fn-wiring-guard.sh green) — verify via grep
[ ] 8. Triage YAML rule in .claude/triage/error-rules.yaml (auto-triage-safe? Y/N)
[ ] 9. Ratchet test fails build on regression (added to crates/*/tests/)
```

Plus the per-item 15-row + 7-row guarantee matrix from `per-wave-guarantee-matrix.md` referenced (NOT duplicated) in the item's PR description.

---

<a id="section-h"></a>
## SECTION H — 15-row + 7-row GUARANTEE MATRIX (mandatory per per-wave-guarantee-matrix.md)

This plan ELECTS to reference the canonical matrix in `.claude/rules/project/per-wave-guarantee-matrix.md` rather than duplicate. Friday session MUST verify every Phase's exit gate against ALL 15 rows + 7 rows before flipping the phase from IN_PROGRESS → COMPLETE.

Specific call-outs where this plan tightens the canonical matrix:

| Row | Tightening for this week |
|---|---|
| Row 1 (code coverage 100%) | scope-limited to changed crates per `testing-scope.md`; full workspace 100% verified post-merge in CI |
| Row 2 (audit coverage 100%) | 5 NEW audit tables in D.1; SEBI 5y retention REQUIRES S3 lifecycle (E.7) before claim |
| Row 5 (performance 100%) | bench-gate promoted to PR-blocking (F.4.3); BarCache + indicator updated paths get NEW benches |
| Row 8 (alerting 100%) | 13 NEW alerts in D.1 + 2 new RSS alerts in C.3 + 5 chaos-pinned alerts in F.5 |
| Row 11 (bug fixing 100%) | 4 P0 bugs from adversarial review fixed BEFORE Phase 2 (B.2) — non-negotiable |
| Row 12 (scenarios covering 100%) | 12 NEW chaos tests in F.1 |
| Row 15 (extreme check 100%) | every new pub fn has matching pub-fn-test + pub-fn-wiring gate pass; banned-pattern Cat 10 enforced |

7-row resilience demand matrix — UPDATED honest claim:

| Demand | Pre-this-week | Post-this-week |
|---|---|---|
| Zero ticks lost | bounded (60s QDB outage, 5M ring) | unchanged but BarCache cross-day collision fixed (was silent loss in indicator path) |
| WS never disconnects | bounded (DETECT≤5s, sleep-until-open) | unchanged |
| Never slow/locked/hanged | DHAT covered for parser/registry/auth; aggregator NOT covered | +DHAT for aggregator (B.3.1) + BarCache (C.1.2) |
| QuestDB never fails | ABSORB via 3-tier | unchanged |
| O(1) latency | bench-gated for hot paths covered | +bench for aggregator + BarCache + indicator (B.3.1, C.1.1, C.1.4) |
| Uniqueness + dedup | composite (security_id, exchange_segment) per I-P1-11 | +BarKey widened to include trading_date_ist (B.2.1) |
| Real-time proof | 7-layer for boot/WS/storage | +7-layer for trading (D.1) |

---

<a id="section-i"></a>
## SECTION I — KID-FRIENDLY SYSTEM DIAGRAM (read this if confused)

Imagine the tickvault as a **3-floor building**:

```
                    ┌─────────────────────────────────────────────────┐
                    │  3rd FLOOR — TRADING BRAIN (this week, Phase 3) │
                    │  • Strategy decides "BUY NIFTY ATM CE"          │
                    │  • Risk engine says YES/NO                      │
                    │  • OMS sends order to Dhan, gets fill back      │
                    │  • Every step recorded in audit tables (SEBI)   │
                    └────────────────────▲────────────────────────────┘
                                         │ reads bars + indicators
                    ┌────────────────────┴────────────────────────────┐
                    │  2nd FLOOR — RAM-FIRST KITCHEN (Phase 2)        │
                    │  • BarCache: today + yesterday sealed bars      │
                    │  • IndicatorEngine: RSI, MACD, BB on 9 TFs      │
                    │  • Updated on every seal from 1st floor         │
                    │  • Zero database queries while market open      │
                    └────────────────────▲────────────────────────────┘
                                         │ seal events
                    ┌────────────────────┴────────────────────────────┐
                    │  1st FLOOR — TICK FACTORY (Phase 1, already 85%)│
                    │  • 5 WebSockets pull ticks from Dhan            │
                    │  • Parser turns binary into ticks               │
                    │  • Aggregator seals candles every minute        │
                    │  • Persists to QuestDB                          │
                    └─────────────────────────────────────────────────┘
                                         │
                    ┌────────────────────▼────────────────────────────┐
                    │  BASEMENT — SAFETY NET (always on)              │
                    │  • 5M-tick rescue ring (60s QuestDB outage)    │
                    │  • Spill file → DLQ NDJSON (worst case)         │
                    │  • Telegram alerts on any RED                   │
                    └─────────────────────────────────────────────────┘
```

**The 4 CRITICAL bugs we're fixing first (Phase 1):**

```
🔴 Bug #1: After midnight, kitchen shelves get OVERWRITTEN
           yesterday's 09:00 bar → today's 09:00 bar at same slot
           Cook (indicator) reads YESTERDAY's apple when asking for TODAY's
           FIX: Add date label to every shelf slot

🔴 Bug #2: Kitchen never opened — nobody told the cook the kitchen exists
           FIX: Hand the cook the kitchen key on Friday morning

🔴 Bug #3: Kitchen shelves never cleaned — pile up forever
           After a week → kitchen explodes (OOM, 2GB cap blown)
           FIX: Janitor sweeps yesterday's shelves at midnight

🔴 Bug #4: Pantry stock-take uses UTC clock when calendar is Indian
           On Sunday night, might skip Friday's inventory
           FIX: Use Indian calendar
```

**Comparison: today vs after this week**

| Capability | Today | After this week |
|---|---|---|
| Live tick streaming | ✅ working (~11K instr) | ✅ unchanged |
| Bars sealed correctly | ⚠ Wave 6 ~85% | ✅ 100% with DHAT + chaos |
| RAM kitchen populated | ❌ kitchen empty | ✅ populated at boot + on every seal |
| RAM kitchen has DATE labels | 🔴 silent collision | ✅ fixed |
| Indicators read from RAM kitchen | ❌ not wired | ✅ wired |
| Strategy → OMS → Dhan order placement | ❌ paper only | ⚠ wired but `dry_run=true` (shadow mode) |
| SEBI audit on every order | ❌ writer exists, never called | ✅ wired, 5y retention via S3 |
| Live RSS gauge proves we fit 2GB | ❌ no proof | ✅ ratcheted alert |
| AWS deployable | ⚠ 4 blockers | ✅ ready for `terraform apply` on Mon |
| First actual live order | ❌ never | ⚠ DEFERRED to Wave 8 operator decision |

---

<a id="section-j"></a>
## SECTION J — RISKS, BLOCKERS, ROLLBACK

### J.1 — Top 5 risks for the week

| # | Risk | Probability | Impact | Mitigation |
|---|---|---|---|---|
| J.1.1 | Wave 6 PR #551 still in review state on Fri morning, blocks all Phase 1 follow-ups | MEDIUM | HIGH | Phase 1 Step 1 explicitly: close it out first, ≤2h |
| J.1.2 | B.2.1 `BarKey` widening triggers cascading test breakage across trading crate | HIGH | MEDIUM | scoped-test-runner catches early; rollback PR is single-file revert |
| J.1.3 | Wave 6 Sub-PR #2 (rehydration) blocks BarCache wiring; both are due Phase 2 | MEDIUM | HIGH | sequence inside Phase 2: ship loader async wrapper FIRST (C.4.1), then C.1.3 wiring uses it |
| J.1.4 | Trading observability sprint (D.1) is large (5 audit tables + 13 alerts + 5 dashboards) — risk of slipping into Day 6 | HIGH | MEDIUM | parallelizable via 3 agents (audit-tables agent + alerts agent + dashboards agent) each writing their slice |
| J.1.5 | OMS wiring (D.2) reveals integration bug that requires Phase 4 (next week) | MEDIUM | HIGH | shadow-equivalence harness (D.3.4) catches divergence BEFORE any live order; honest pivot if found |

### J.2 — Rollback procedures

For each phase, the rollback is a single PR revert:

| Phase | Rollback action |
|---|---|
| Phase 1 fixes (B.2) | revert the "fix(critical): 4 P0 bugs" PR; BarCache continues to be unwired (status quo) |
| Phase 2 wiring (C) | revert the Phase 2 cumulative PR; banned-pattern Cat 10 stays enforced against empty cache; system reverts to paper-only |
| Phase 3 OMS (D.2) | flip `[strategy].dry_run = true` in config; OMS engine code stays in main.rs but is gated off |
| Phase 3 audit tables (D.1) | NO rollback needed — tables are write-only additions; orphan writes are cheap |
| AWS deploy (E) | do NOT `terraform apply` this week; `plan` output reviewed by operator separately |

### J.3 — Hard STOP conditions (Friday session HALTS)

| Condition | Action |
|---|---|
| 4 CRITICAL P0 bugs (B.2) cannot all be reproduced + fixed by end of Day 2 | Halt Phase 2; Telegram CRITICAL; operator decision needed |
| Wave 6 Sub-PR #1 cumulative diff fails any of the 22 test categories | Halt; resolve before merge |
| BarCache wiring (C.1.3) cannot prove `IndicatorEngine` reads new values | Halt Phase 2; do NOT advance to Phase 3 |
| Any of 4 boot preconditions (D.2.2) cannot be made green in dev | Halt Phase 3; OMS stays paper |
| `make doctor` fails any of its 7 sections during the week | Halt current item; investigate via runbooks |

---

<a id="appendix-a"></a>
## APPENDIX A — CONSOLIDATED RESEARCH FINDINGS (10 parallel agents, 2026-05-11)

Compact synthesis of the 10 specialist agent reports that drove this plan. Friday session can re-read full reports by re-spawning agents if needed.

### A.1 — Wave 6 status (Agent 1, general-purpose)

- Wave 6 ~20-22% complete overall. Sub-PR #1 (Aggregator Engine) ~85% landed via ~30 incremental sub-commits #551–#594. Sub-PRs #2/#3/#4 are 0% started.
- BLOCKING for Friday: PR #551 still "ready for review" state. Must close out FIRST.
- Sub-PR #1 missing: DHAT + Criterion split p99 budgets, proptest 9-TF rollup, 6 chaos tests, post-impl 3-agent adversarial review.
- Sub-PR #3 estimated 2500-3500 LoC across 18 items — too big for one session. Decompose into 4 slices.
- W7-A4.5 DHAT is gated on Wave 6 Sub-PR #4 (shadow table promotion). Sequencing critical.

### A.2 — Wave 7-A4 status (Agent 2, general-purpose)

- W7-A4 ~80% complete. 4/5 sub-PRs merged (W7-A4.1–.4). BarCache (413 LoC) + boot loader (499 LoC) exist as primitives only.
- NOT WIRED: no call site in main.rs for BarCache; no IST-midnight clear scheduler; no async HTTP wrapper for the loader; no live RSS gauge.
- 4 AWS deployment blockers: missing ALB, missing SSM hierarchy, CloudWatch agent NOT installed in user-data, no Dhan `setIP` automation.
- 2.0 GB app cap is aspirational — W7-A4.5 DHAT must verify; until then unproven.

### A.3 — Live-trading gaps (Agent 3, general-purpose)

- ~35% complete overall. Plumbing exists (4965-LoC Dhan API client; state machine; risk engine; rate limiter; circuit breaker; idempotency; `order_audit` DDL).
- OMS engine NOT instantiated in main.rs — only paper pipeline runs. Strategy→OMS is a STUB.
- `append_order_audit_row` has ZERO call sites in trading crate. SEBI compliance structurally broken.
- Rate limiter likely caps at 5K/day instead of Dhan v2.3's 7K. Needs audit.
- Idempotency UUID is in-memory HashMap only, NOT Valkey-persisted. Crash = duplicate order risk.
- No 15:30 IST cutoff. No local-halt → Dhan kill-switch sync. Pre-trade margin check exists but RiskEngine never calls it.
- Conservative week-1 live scope: 1 strategy × NIFTY ATM CE/PE × 1 lot × ₹2K daily loss × 10 round-trips/day. Defer Super/Forever/AMO/Slicing to Wave 8.

### A.4 — Indicator + BarCache (Agent 4, general-purpose)

- BarCache (414 LoC) COMPLETELY UNWIRED. No seal-hook push, no boot loader async wrapper, no IST-midnight scheduler.
- NO TF wired to IndicatorEngine — tick-driven only. Strategies needing "RSI on 5m" have no production path.
- CompactBar = 56 bytes (NOT 32B as docstring claims) → actual RAM 616 MB vs 176 MB target. Documentation gap; still fits 2GB cap.
- `yata 0.7.0` workspace-declared but UNUSED. Cleanup opportunity.
- NO Criterion bench, NO DHAT for indicator hot path. Claimed ~200ns/~30ns are docstring-only.
- Per-instrument `IndicatorParams` NOT supported — single shared (violates rust-code.md).

### A.5 — Observability gaps (Agent 5, general-purpose)

- 104 ErrorCode variants (architecture doc says 53 — stale). 41 Grafana alert titles. 14 dashboards (operator-health has 137 panels). 9 audit_persistence tables. 27 `#[instrument]` macros.
- Trading-path coverage weakest: OMS engine has only 1 span; RiskEngine has 1 span; Strategy evaluator has 0 spans; position tracker has nothing.
- 5 SEBI-relevant audit tables MISSING: risk_decision, pnl_snapshot, kill_switch, strategy_decision, position_lifecycle.
- 13 Prometheus alerts MISSING for trading-path (order-place failures, kill-switch, daily-loss, margin shortfall, etc.).
- 5 dashboards MISSING: trading-pnl, order-book, risk-engine, strategy-decisions, position-tracker.
- Recommendation: trading observability MUST ship BEFORE OMS go-live (drives D.1 before D.2 in this plan).

### A.6 — Testing coverage gaps (Agent 6, general-purpose)

- 181 test files; ~7250 tests; coverage 100% per crate; 16 chaos; 5 loom; 11 dhat; 3 fuzz targets.
- Trading crate has ZERO chaos tests — biggest gap.
- 3 critical fuzz targets missing: OMS state machine, ILP line builder, subscription planner CSV.
- Storage crate NOT in mutation gate. One-line fix in `.github/workflows/mutation.yml`.
- 11 chaos scenarios reserved but unwritten (holiday-92h, triple-failure cascade, dual-instance race, etc.).

### A.7 — Hot-path performance (Agent 7, hot-path-reviewer)

- 14 bench files + ~20 budget entries. 13 DHAT zero-alloc tests.
- Wave 6 seal closure: NO bench + NO DHAT. IST midnight 99K-seal burst unverified.
- BarCache lookup + stamp_seal_pct_fields: now on hot path, but no perf contract. Banned-pattern Cat 10 is "no DB on hot path" without ≤50ns bench is just a rule, not enforced quantitatively.
- `prev_close_writer` + `tick_parser` benches exist but have NO budget entry → silently bypass the 5% regression gate.
- No version-controlled baseline JSON — regression detection only works when `criterion/previous` exists.

### A.8 — Security + compliance (Agent 8, security-reviewer)

- 0 CRITICAL, 0 HIGH, 5 MEDIUM, 4 LOW.
- `set_ip()` defined but never called. No automated Dhan static IP set path. Manual `curl` only.
- Zero HTTP rate limiting on API endpoints. Attacker could exhaust Dhan 100K/day Data API quota.
- SEBI 5y retention enforced only by code comment. No S3 lifecycle policy code anywhere.
- 4 LOW: token cache file permissions unverified; boot Telegram may leak public IP; 3 `unsafe` blocks use `rkyv::access_unchecked` skipping checksum; ILP sanitization doesn't escape `\` or `"`.

### A.9 — Disaster recovery (Agent 9, general-purpose)

- 9/15 enumerated scenarios fully pinned, 4 partial, 2 untested.
- 12 NEW scenarios needed (mostly Wave-4 reserved codes never shipped): SSM rotation, mid-session clock skew, DH-911 silent black-hole, NET-01 IP changed, PROC-01 OOM, PROC-02 container loop, order reply lost, RESILIENCE-01 dual-instance, RESILIENCE-02 rebalance crash, CASCADE-01 triple-failure.
- RTO bounds missing on some chaos tests (functional only, no `assert!(elapsed < Duration)`).
- Order reply-loss chaos test CRITICAL before live trading go-live (IDEMP-01).

### A.10 — Adversarial bug hunt (Agent 10, general-purpose) — DRIVES B.2

- **4 CRITICAL P0 bugs** (covered in B.2 above): BarKey cross-day collision; BarCache unwired in main.rs; clear_before never scheduled; prev_day_cache_loader uses UTC `now()`.
- 5 HIGH: 2× warn-not-error for audit/DDL failures (Rule 5 violation); edge-trigger pre-session gap; cast convention drift; midnight rollover race.
- 6 MEDIUM: counter-not-gauge alert risk; UTC-vs-IST query mismatch; no DHAT/Criterion ratchet on BarCache; swallowed reqwest TLS error; boot-vs-aggregator race; estimated_bytes under-reports by ~120 MB.
- WAVE 7-A4 RAM-FIRST CANNOT BE CLAIMED COMPLETE until B.2 + C.1 + C.4 all ship.

---

<a id="appendix-b"></a>
## APPENDIX B — FILE INDEX (read first, in order)

Friday session reads these in the order listed before any code:

1. `CLAUDE.md` (always)
2. `.claude/plans/active-plan-friday-may-15-mega.md` (this file)
3. `.claude/rules/project/per-wave-guarantee-matrix.md`
4. `.claude/rules/project/wave-4-shared-preamble.md`
5. `.claude/rules/project/stream-resilience.md`
6. `.claude/rules/project/observability-architecture.md`
7. `.claude/rules/project/aws-budget.md`
8. `.claude/rules/project/disaster-recovery.md`
9. `.claude/rules/project/security-id-uniqueness.md` (I-P1-11 composite key)
10. `.claude/rules/project/wave-6-error-codes.md`
11. `.claude/plans/active-plan-aggregator-direct-flush-rehydrate.md` (Wave 6 canonical)
12. `.claude/plans/active-plan-in-memory-store-aws-instance.md` (Wave 7-A canonical)
13. `crates/trading/src/in_mem/bar_cache.rs`
14. `crates/app/src/bar_cache_loader.rs`
15. `crates/app/src/prev_day_cache_loader.rs`
16. `crates/trading/src/aggregator/multi_tf.rs`
17. `crates/storage/src/shadow_persistence.rs`
18. `crates/storage/src/aggregator_seal_audit_persistence.rs`
19. `crates/trading/src/oms/engine.rs`
20. `crates/trading/src/risk/engine.rs`
21. `crates/app/src/main.rs` (boot wiring section)

---

<a id="appendix-c"></a>
## APPENDIX C — ErrorCode BUDGET (variants to add this week)

| # | Variant | code_str | Severity | Auto-triage-safe | Phase introduced |
|---|---|---|---|---|---|
| 1 | REHYDRATE-01..05 | as listed | Critical/High mix | NO | Phase 2 (C.4) |
| 2 | BUFFER-GATE-01..02 | as listed | High | YES | Phase 2 (C.4) |
| 3 | BARCACHE-COLLISION-01 (defensive guard) | "BARCACHE-COLLISION-01" | Critical | NO | Phase 1 (B.2.1) |
| 4 | AUDIT-07..11 | per audit table | High | YES | Phase 3 (D.1) |
| 5 | OMS-GAP-07..09 | per missing transition class | High | NO | Phase 3 (D.2) |
| 6 | RISK-GAP-04..06 | margin / cutoff / kill-sync | Critical | NO | Phase 3 (D.2) |
| 7 | PROC-01, PROC-02 | as listed | Critical | NO | F.5 promotion |
| 8 | NET-01 | as listed | Critical | NO | F.5 promotion |
| 9 | DH-911 | as listed | High | YES | F.5 promotion |
| 10 | RESILIENCE-01 | as listed | Critical | NO | F.5 promotion |
| 11 | CLOCK-DRIFT-01 | as listed | High | YES | F.5 promotion |
| 12 | CASCADE-01 | as listed | Critical | NO | F.1.5 |

Each variant: `error_code.rs` entry + `wave-*-error-codes.md` runbook section + rule-file cross-ref test green + tag-guard meta-test green.

---

## REVISION HISTORY

| Date | Author | Change |
|---|---|---|
| 2026-05-11 18:30 IST | Opus 4.7 (Mon session) | Initial DRAFT — synthesized from 10 parallel specialist research agents |
| (Fri 06:00 IST) | Friday session | Flip Status DRAFT → APPROVED → IN_PROGRESS, set Approved by, create todo mirror |

**END OF PLAN.** Save as: `.claude/plans/active-plan-friday-may-15-mega.md`. Once approved by operator, NO MODIFICATIONS without re-approval — only checkbox status updates per `plan-enforcement.md`. After Thu EOD push, archive to `.claude/plans/archive/2026-05-15-friday-mega-week.md`.
