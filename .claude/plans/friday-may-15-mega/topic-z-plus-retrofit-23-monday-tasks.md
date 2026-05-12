# Topic — Z+ Retrofit for 23 Mon-Eve Locked Tasks

> **Status:** DRAFT (discussion mode — no implementation 2026-05-12 → 2026-05-14)
> **Authority:** `z-plus-defense-doctrine.md` > this file.
> **Scope:** Apply the 7-layer Z+ defense pattern to every one of the 23 Mon-eve task briefs already in `tasks/T01-*.md` → `T23-*.md`.
> **Purpose:** Discussion + brainstorming. Each task gets its 7-layer table; gaps surface; we debate.

---

## 🚗 Auto-Driver Story

> Sir, on Monday evening we locked 23 tasks for Friday. They each have a description, a LoC estimate, and tests listed. But none of them yet have the **Z+ bodyguard chart** — the 7 commandos table. Today we go through all 23 and ask "if this task ships, what catches each kind of failure?" Like asking the PM's security team "show me your 7-layer plan for each motorcade route".

---

## 📋 The 23 Mon-Eve Tasks (recap)

(Indexed by Mon-eve plan; consolidated here for discussion-only.)

| # | Task | LoC est |
|---|---|---|
| 1 | Phase 2 readiness pre-flight gate (PHASE2-READY-02) | 200 |
| 2 | Mid-market boot orchestrator wiring | 150 |
| 3 | Sealed bar cache (today + yesterday) | 250 |
| 4 | RAM-first hot path banned-pattern guard | 80 |
| 5 | Pre-open buffer REST fallback scheduler integration | 100 |
| 6 | Stock F&O expiry rollover ratchet hardening | 50 |
| 7 | Greeks pipeline MAX_UNDERLYINGS=3 ratchet | 30 |
| 8 | Spot updater classify_tick_for_spot_update wiring | 80 |
| 9 | Bhavcopy prev-OI loader (8:15am ready orchestrator) | 300 |
| 10 | Cohort universe segment-filter config plumb | 100 |
| 11 | Telegram coalescer drop-class family alerts (4 alerts) | 60 |
| 12 | Live-tick ATM resolver Mode-C boot wiring | 200 |
| 13 | Aggregator heartbeat positive-signal | 50 |
| 14 | Operator-health Grafana dashboard panels | 100 |
| 15 | SLO score scheduler (10s) — phase2_health pinning | 80 |
| 16 | Subscribe rate-limit token bucket | 150 |
| 17 | DEDUP segment meta-guard extension | 50 |
| 18 | Z+ checklist guard hook | 200 |
| 19 | Auto-driver test guard hook | 150 |
| 20 | Run-doctor 8th section (Z+ envelope check) | 100 |
| 21 | Triage YAML for WS-FLOW-01 | 50 |
| 22 | Wave-6 sub-PR rollout doc | 100 |
| 23 | Master Z+ inheritance map | 150 |

**Total Mon-eve LoC:** ~3,330. **Plus dynamic depth (880) + WS Flow Health (1,790) = ~6,000 LoC Friday workload.**

---

## 🛡️ The Z+ Retrofit (7-layer pattern applied to each task)

### Task 1 — Phase 2 readiness pre-flight gate

| Layer | Mechanism |
|---|---|
| L1 DETECT | Pre-flight check at 09:13:01 IST emits per-check pass/fail |
| L2 VERIFY | Re-check on 60s timer until 09:14:30 IST |
| L3 RECONCILE | Daily phase2_audit row vs operator console |
| L4 PREVENT | If any check fails AT 09:13 → block depth-200 swap until clear |
| L5 AUDIT | `phase2_readiness_audit` table (NEW — DEDUP UPSERT KEYS(ts, check_name)) |
| L6 RECOVER | 3 failures → soft-restart phase2 scheduler + Telegram CRITICAL |
| L7 COOLDOWN | 60s between retries within the 09:13–09:15 window |

**Open question:** L4 PREVENT — should depth-200 swap be BLOCKED on pre-flight fail, or just emit warning and proceed? Risk of blocking: depth-200 stuck on stale anchor. Risk of proceeding: depth-200 anchored on garbage ATM.

### Task 2 — Mid-market boot orchestrator wiring

| Layer | Mechanism |
|---|---|
| L1 DETECT | Boot-mode classifier sees `MidMarket` → fires |
| L2 VERIFY | Live-tick resolver polls `SharedSpotPrices` at 5/10/15/20/25s; exit early when 100% F&O stocks resolved |
| L3 RECONCILE | Compare resolved ATM vs Dhan Option Chain REST snapshot |
| L4 PREVENT | If now() > 15:00 IST → skip mid-market boot, fall through to PostMarket mode |
| L5 AUDIT | `mid_market_boot_audit` table (NEW) |
| L6 RECOVER | If after 30s zero ticks → fall through to REST fallback → QuestDB last resort |
| L7 COOLDOWN | N/A — single-shot at boot |

**Open question:** Should we ABORT boot if >5% of stocks can't be resolved via any source, or proceed with 95% subscribed?

### Task 3 — Sealed bar cache (today + yesterday)

| Layer | Mechanism |
|---|---|
| L1 DETECT | Cache miss counter per `(security_id, segment, tf)` |
| L2 VERIFY | On miss, fall back to QuestDB SELECT, then update cache |
| L3 RECONCILE | Daily 23:00 IST — full SELECT of today's bars vs cache contents |
| L4 PREVENT | Cache hydration runs at boot BEFORE first hot-path read |
| L5 AUDIT | `bar_cache_audit` table for miss events (NEW) |
| L6 RECOVER | If cache corrupted (checksum mismatch) → full rehydrate from QuestDB |
| L7 COOLDOWN | 5s between rehydrate attempts |

**Open question:** Cache size = ~352 MB per `aws-budget.md` Wave 7-A4 math. What if it exceeds 400 MB after 6 months of new SIDs? Bound by `STOCK_OPTION_ATM_STRIKES_EACH_SIDE`?

### Task 4 — RAM-first hot path banned-pattern guard

| Layer | Mechanism |
|---|---|
| L1 DETECT | `banned-pattern-scanner.sh` category 7 (NEW) — scan for `SELECT.*FROM` inside `crates/trading/src/{strategy,indicator}/**` and `crates/core/src/pipeline/tick_processor.rs` |
| L2 VERIFY | Negative-control: insert a synthetic SELECT in a test file, ensure scanner blocks |
| L3 RECONCILE | Quarterly grep of entire workspace for false-negatives |
| L4 PREVENT | Pre-commit hook = no commit possible with violation |
| L5 AUDIT | Hook log + git commit denial reason |
| L6 RECOVER | N/A — pre-commit blocks before damage |
| L7 COOLDOWN | N/A |

**Discussion angle:** Should we also ban `tokio::spawn` from hot path? Spawning a new task takes ~3µs (allocation + scheduler entry). Could break O(1).

### Task 5 — Pre-open buffer REST fallback scheduler integration

| Layer | Mechanism |
|---|---|
| L1 DETECT | At 09:12:55 IST, scan PreOpenCloses for missing F&O stocks |
| L2 VERIFY | Call POST /v2/marketfeed/ltp with the missing SIDs |
| L3 RECONCILE | Daily phase2_audit captures `rest_fallback_used` count |
| L4 PREVENT | REST is rate-limited 1/sec → batch the call (1000 SIDs/call) |
| L5 AUDIT | Already covered by phase2_audit + `rest_fallback_invocations_total` counter |
| L6 RECOVER | REST returns empty → fall through to QuestDB previous close → if empty, skip stock |
| L7 COOLDOWN | If REST returns 805 → 60s STOP_ALL pause per Dhan rule 12 |

**Open question:** Should we PRE-EMPTIVELY call REST at 09:00 IST (early warm-up) to ensure HTTP path is healthy, even if buffer fills later?

### Task 6 — Stock F&O expiry rollover ratchet hardening

| Layer | Mechanism |
|---|---|
| L1 DETECT | Constant pin: `STOCK_EXPIRY_ROLLOVER_TRADING_DAYS = 0` (T-0 only) |
| L2 VERIFY | 7 ratchet tests in subscription_planner |
| L3 RECONCILE | Daily compare planner output vs Dhan instrument master |
| L4 PREVENT | Build fails if constant changes without operator approval |
| L5 AUDIT | Existing planner emission log |
| L6 RECOVER | If wrong expiry subscribed → daily reconcile catches; next boot fixes |
| L7 COOLDOWN | N/A |

**Discussion angle:** What if NSE introduces Tuesday expiry for stock F&O (rumored)? Constant needs to handle both Thursday + Tuesday weekly.

### Task 7 — Greeks pipeline MAX_UNDERLYINGS=3 ratchet

| Layer | Mechanism |
|---|---|
| L1 DETECT | `const MAX_UNDERLYINGS: usize = 3` |
| L2 VERIFY | Compile-time `static_assertions::const_assert_eq!(MAX_UNDERLYINGS, 3)` |
| L3 RECONCILE | Daily greeks aggregator slice length check |
| L4 PREVENT | If config tries to inject 4th underlying → panic with explicit message |
| L5 AUDIT | Greeks pipeline emission counts |
| L6 RECOVER | N/A — compile-time |
| L7 COOLDOWN | N/A |

### Task 8 — Spot updater classify_tick_for_spot_update wiring

| Layer | Mechanism |
|---|---|
| L1 DETECT | Per-segment tick counter |
| L2 VERIFY | After classification, write to `SharedSpotPrices` map; reader re-queries |
| L3 RECONCILE | Daily compare spot prices vs Dhan REST snapshot |
| L4 PREVENT | Off-segment ticks skipped silently (no panic) |
| L5 AUDIT | `tv_spot_updates_per_segment_total` counter |
| L6 RECOVER | If `SharedSpotPrices` becomes stale (no update in 60s) → mark unhealthy → reconnect |
| L7 COOLDOWN | N/A |

### Task 9 — Bhavcopy prev-OI loader (8:15am ready orchestrator)

| Layer | Mechanism |
|---|---|
| L1 DETECT | Boot scheduler checks if it's after 08:15 IST → triggers loader |
| L2 VERIFY | Download bhavcopy, parse, cross-check size + row count |
| L3 RECONCILE | Compare bhavcopy prev-OI vs Option Chain REST snapshot |
| L4 PREVENT | If bhavcopy unreachable, fall back to Option Chain REST |
| L5 AUDIT | `prev_oi_load_audit` table (NEW) |
| L6 RECOVER | If both fail → cache remains empty → PREVOI-01 fires Severity::Medium |
| L7 COOLDOWN | 5-min retry on transient failure; max 5 retries |

**Open question:** Bhavcopy CSV URL — is it stable? What if NSE redesigns their portal?

### Task 10 — Cohort universe segment-filter config plumb

| Layer | Mechanism |
|---|---|
| L1 DETECT | Config TOML reads `[depth_20.dynamic.universe] exchange_segments = ["NSE_FNO"]` |
| L2 VERIFY | Config-load schema validation |
| L3 RECONCILE | Daily emit Telegram with current config |
| L4 PREVENT | Config hot-reload guarded — can't flip during market hours |
| L5 AUDIT | Config-change audit (rare event) |
| L6 RECOVER | Invalid config → fall back to default `["NSE_FNO"]` |
| L7 COOLDOWN | N/A |

### Task 11 — Telegram coalescer drop-class family alerts (4 alerts)

| Layer | Mechanism |
|---|---|
| L1 DETECT | 4 Prom alerts already shipped (Wave 6 Sub-PR #1) |
| L2 VERIFY | Counter increment + alert fires |
| L3 RECONCILE | Weekly review of alert frequency |
| L4 PREVENT | Coalescer caps samples at 10 per 60s window |
| L5 AUDIT | Existing aggregator alert guard ratchet |
| L6 RECOVER | If drop rate > 100/min → operator manually inspects |
| L7 COOLDOWN | Alerts gated by `tv_market_hours_active == 1` |

### Tasks 12–17 — (skipping detailed retrofit for brevity; pattern identical)

Each follows the same 7-layer table. Brief one-liner per task:

| # | Task | Notable Z+ gap to discuss |
|---|---|---|
| 12 | Live-tick ATM resolver | L4 PREVENT — what if cash equity for an F&O stock is delisted? |
| 13 | Aggregator heartbeat | L1 DETECT — what if heartbeat itself dies silently? Need watchdog-watchdog. |
| 14 | Operator-health Grafana dashboard | L5 AUDIT only — Grafana itself can crash; need fallback panel screenshot? |
| 15 | SLO score scheduler | L2 VERIFY — composite score can lie if one dimension fakes-OK |
| 16 | Subscribe rate-limit token bucket | L7 COOLDOWN — what's the right refill rate? GCRA? Leaky? |
| 17 | DEDUP segment meta-guard | L4 PREVENT — already shipped; just extending to new tables |

### Tasks 18–23 — Mechanical gates + docs

| # | Task | Z+ relevance |
|---|---|---|
| 18 | Z+ checklist guard hook | THE meta-gate — enforces this doctrine |
| 19 | Auto-driver test guard hook | THE language gate — rejects jargon in operator text |
| 20 | Run-doctor 8th section (Z+ envelope) | Real-time proof of Z+ status |
| 21 | Triage YAML for WS-FLOW-01 | L6 RECOVER specification |
| 22 | Wave-6 sub-PR rollout doc | Incremental approach (charter line #7) |
| 23 | Master Z+ inheritance map | THE single-page operator visualization |

---

## 🎯 Cross-cutting discussion items

### D1 — How do we PROVE 100% coverage at the file level?

`quality/crate-coverage-thresholds.toml` says 100% per crate. But individual files within a crate aren't pinned. If `crates/trading/src/strategy/foo.rs` drops to 80% coverage but the crate average stays 100% (because `bar.rs` is 100%), the gate passes.

**Proposal:** Add per-file coverage threshold. Brutal but bulletproof.

**Counter:** Adds friction. Some files (e.g., `error_code.rs`) have unreachable arms (defensive match catch-alls). Per-file = false alarms.

### D2 — Should the Z+ doctrine REQUIRE chaos test for every layer?

L6 RECOVER says "if everything fails, fall back to X". How do we PROVE X works without an actual test that injects failures of layers 1-5?

**Proposal:** Every Z+ PR ships at least one chaos test per L6.

**Counter:** Chaos tests are slow (~30s each). 24 Friday tasks × 1 chaos test = 12 min added to CI.

### D3 — Telegram fatigue vs Z+ completeness

If every layer fires its own Telegram event, operator gets 7× messages per incident. Already saw this with depth rebalance (10-30/day fixed by suppression).

**Proposal:** Only L1 DETECT and L6 RECOVER emit Telegram. L2-L5 are silent (audit table only). L7 is logged but not Telegram'd.

**Counter:** L4 PREVENT skipping Telegram means operator doesn't know "we prevented a failure". Missing positive signal.

### D4 — Auto-restart frequency cap

L6 RECOVER for WS Flow says "soft restart after 3 failures in 5min". What if 5 different conns each fail 3× → 5 soft restarts in 25min?

**Proposal:** Global soft-restart cap = 1 per hour. After that, halt + Telegram CRITICAL.

**Counter:** If genuine systemic issue, halting means zero trading for the day.

### D5 — Test scope explosion

22 test categories × 7 layers × 24 tasks = 3,696 test concepts. We don't have time on Friday.

**Proposal:** Z+ requires 7 categories per layer (unit, property, loom-where-relevant, dhat-hot-path, chaos, mutation, fuzz). Total = 7 × 7 × 24 = 1,176 tests. Still too many.

**Counter:** Most layers reuse existing meta-guards. Real new tests per task: ~10. Total = 240 — manageable.

---

## 🔥 What Z+ EXPOSES about Mon-eve plan

| Gap | Severity |
|---|---|
| Task 11 (Telegram coalescer) — has no L3 RECONCILE | LOW (alerts visible in Grafana, daily review is informal) |
| Task 13 (Aggregator heartbeat) — no watchdog-of-watchdog | MEDIUM (if heartbeat task dies, we lose positive signal) |
| Task 14 (Grafana dashboard) — Grafana itself is L5 only | HIGH (single point of visualization failure) |
| Task 15 (SLO score) — composite can mask weakest dim | HIGH (false-OK risk per audit-findings Rule 11) |
| Task 9 (bhavcopy loader) — fallback to REST untested | MEDIUM (need explicit fallback chain test) |

**Recommendation:** Pre-Friday, write 5 explicit fixes for these gaps (each ~50 LoC).

---

## 🎤 Open for discussion

Pick any of D1-D5 to argue. Or surface a new gap. Or call BS on any of the 7-layer cells above.

3 days, no implementation, pure brainstorming. Floor's yours.
