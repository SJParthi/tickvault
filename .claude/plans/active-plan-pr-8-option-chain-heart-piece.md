# Implementation Plan: PR #8 — option_chain heart-piece + pre-open day_open wiring

**Status:** IN_PROGRESS — sub-PR split per operator-charter §H
**Date:** 2026-05-19
**Approved by:** Parthiban (split + Q5 auto-allow locked via AskUserQuestion)
**Predecessor:** PR #7 series complete (#711-#715 all merged)
**Successor in 14-PR sequence:** PR #9 — cross_verify module (15:31 IST + 08:05 IST schedulers)

## Sub-PR split (operator-locked)

- **PR #8a** (#716 MERGED) — Slice 1: DayOhlcTracker 09:15 IST arm wiring.
  Closed the pre-open equilibrium open-price gap.
- **PR #8b** (this branch `claude/aws-lifecycle-pr-8b-option-chain-heart-piece`) —
  Slice 2: option_chain heart-piece constants (5 constants, 8 ratchet tests).
  Pins the operator-locked values (50s cadence, 60s cache age, DH-904
  backoff ladder, 3-cycle stale threshold, 0.5% L2-verify tolerance).
  Mergeable on its own; unblocks every later slice.
- **PR #8c** (NEXT — needs operator awake) — Slices 3-8: the heart-piece
  behavior. Deferred from this autonomous session because:
  - Slice 4 (strategy fail-closed gate) requires wiring `SnapshotCache`
    into `StrategyInstance::evaluate()` — changes the evaluator signature
    + threads the cache through the trading pipeline = architecturally
    significant, needs operator sign-off (charter: no arch decisions
    while operator asleep).
  - Slice 3 (8 OPTION-CHAIN-* emit sites) is NOT mechanical — the
    current `snapshot_scheduler.rs` lacks the DH-904 ladder / parse-
    failure / L2-verify failure paths the 8 codes describe. Tagging
    requires implementing those paths first = real heart-piece work.
  - Slice 8 (alerts) — Prometheus vs CloudWatch is open Q1, deferred.

---

## §0. Auto-driver / Insta-reel one-liner

> "Sir, two things land in this PR.
>
> **One — pre-open + market open:** every morning at exactly 9:15 AM IST, we take the equilibrium price NSE published from the 9:00-9:08 AM pre-open auction (it's already captured minute-by-minute in our pre-open buffer) and stamp it as today's official 9:15 OPEN price for NIFTY, BANKNIFTY, SENSEX and INDIA VIX. That price matches exactly what TradingView and Dhan show — not "first traded tick" which is a few ticks different.
>
> **Two — option chain heart-piece:** every 50 seconds for NIFTY, BANKNIFTY and SENSEX we fetch the live option chain (every strike, every greek, OI, IV, top bid/ask) from Dhan REST, keep it in RAM, audit every request and full JSON response to QuestDB, and the strategy REFUSES to trade if the cache is older than 60 seconds. No stale option chain trade. Ever."

---

## §1. Why this PR exists — two coupled gaps

### Gap 1: Pre-open / market-open day_open (Z+ rule 13: defined but never called)

Per `.claude/rules/project/index-day-ohlc-tracker-error-codes.md`:

| Component | Status today |
|---|---|
| `preopen_price_buffer.rs` capturing 09:00-09:12 IST closes | ✅ Live |
| `PREOPEN_INDEX_UNDERLYINGS` contains all 4 IDX_I SIDs (NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21) | ✅ Live |
| `PreOpenCloses::backtrack_latest()` returns equilibrium price | ✅ Live |
| `DayOhlcTracker::arm_sid()` method exists | ✅ Defined |
| **09:15:00 IST boundary task wiring `arm_sid()`** | ❌ **NOT WIRED — Rule 13 anti-pattern** |
| `INDEX-OHLC-01` ErrorCode (empty buffer at 09:15) | ✅ Defined |

**Operator-locked contract** (rule file verbatim):
> "09:15:00 IST open price MUST = NSE equilibrium open — NOT the first post-open tick LTP."

**Today's behavior under current code:** `day_open` = LTP of the first WebSocket tick at or after 09:15:00 IST — ≈ first traded price, NOT the NSE-published official open. Cross-verify at 15:31 IST against Dhan REST `/v2/charts/intraday` will mismatch on the OPEN field.

### Gap 2: Option chain heart-piece (the strategy's blood supply)

Per `docs/architecture/option-chain-z-plus-heart-piece.md` §1:

> "Without a fresh option chain, every strategy decision becomes UNKNOWN. The strategy must fail-closed (not trade) — but the operator must know IMMEDIATELY that we're in fail-closed state."

Pre-existing infrastructure (already on `main`):
- `crates/core/src/option_chain/` module — 2,509 LoC (client.rs, snapshot_scheduler.rs, snapshot_cache.rs, prev_oi.rs, types.rs)
- `spawn_snapshot_scheduler` wired at `main.rs:5103` (this populates the 869-entry prev_oi overlay you see in boot logs)
- 8 `OptionChain01..08` ErrorCode variants defined in `crates/common/src/error_code.rs`
- `OPTION_CHAIN_UNDERLYINGS` const = NIFTY/BANKNIFTY/SENSEX (VIX excluded — no options)
- `OPTION_CHAIN_REQUEST_TIMEOUT_SECS = 10` constant

**What's missing per `option-chain-z-plus-heart-piece.md` §3–§8:**
1. Strategy fail-closed gate (cache_age > 60s → Critical alert + skip trade)
2. The 8 OPTION-CHAIN-* emit sites tagged with `code = ErrorCode::*.code_str()`
3. L2 VERIFY: option-chain `data.last_price` vs WS index LTP (0.5% tolerance)
4. L7 COOLDOWN: 3-failure ladder 50s → 60s → 90s → 120s
5. 3 audit tables (`option_chain_request_audit`, `dhan_option_chain_raw`, `option_chain_snapshots`)
6. 6 Prometheus alerts (CloudWatch migration is PR #10)
7. Thursday rollover handling (mid-day expirylist refresh)
8. Constants pins: `OPTION_CHAIN_FETCH_CADENCE_SECS=50`, `OPTION_CHAIN_MAX_CACHE_AGE_SECS=60`, `OPTION_CHAIN_BACKOFF_LADDER_SECS=[10,20,40,80]`, `OPTION_CHAIN_STALE_CYCLES_BEFORE_TELEGRAM=3`

---

## §2. What this PR FORBIDS forever (operator lock 2026-05-15 §I + heart-piece §11)

- Strategy emitting any trading signal while `option_chain_cache.age_seconds(any_underlying) > 60` during market hours → compile-time impossible via the fail-closed gate.
- `DayOhlcTracker::day_open` being set from the first post-open tick LTP instead of the pre-open equilibrium → compile-time impossible because the `arm_sid()` gate fires at the 09:15:00 IST boundary BEFORE any tick processor can touch `day_open`.
- Any `error!` in the option_chain module without `code = ErrorCode::OptionChainNN.code_str()` → ratcheted by `error_code_tag_guard.rs` meta-test.
- Hot path allocation in the 50s scheduler tick: bounded mpsc, pre-allocated buffers, `Arc<RwLock<OptionChainCache>>` lock held in microseconds only.

Mechanical ratchets enforce all of the above.

---

## §3. Sliced plan (8 slices — sized for 1 commit each)

### Slice 1 — Pre-open / market-open day_open wiring (the operator's question)

- [ ] Spawn 09:15:00 IST boundary task in `main.rs` boot orchestrator that runs ONCE per trading day.
  - Reads from `Arc<SharedPreOpenBuffer>` (already populated by `run_preopen_snapshot_task`).
  - Iterates `PREOPEN_INDEX_UNDERLYINGS` (the 4 IDX_I SIDs).
  - For each SID, calls `PreOpenCloses::backtrack_latest()`.
  - If `Some(equilibrium_close)` → calls `DayOhlcTracker::arm_sid(sid, IdxI, equilibrium_close)`.
  - If `None` → emits `INDEX-OHLC-01` Critical Telegram + writes `day_open=NaN` sentinel. The aggregator falls back to first-trade LTP for that SID and the operator sees the cross-verify mismatch at 15:31 IST.
- [ ] Wire `Arc<DayOhlcTracker>` into the tick processor so per-tick `day_high` / `day_low` / `day_close` updates flow.
- [ ] At 15:30:00 IST seal boundary: call `DayOhlcTracker::seal_day()` to lock `day_close` and emit a per-SID `DayOhlcSealed` event (Severity::Info) carrying OHLC for the day.
- [ ] Daily reset at IST midnight: `DayOhlcTracker::reset_daily_all()` (per `index-day-ohlc-tracker-error-codes.md` INDEX-OHLC-02).
- **Files:**
  - `crates/app/src/main.rs` (boot wiring — new spawn site at 09:15:00 boundary)
  - `crates/trading/src/in_mem/day_ohlc_tracker.rs` (already exists — add `seal_day()` + IST-midnight reset task)
  - `crates/core/src/pipeline/tick_processor.rs` (call `update_tick()` per IDX_I tick)
  - `crates/core/src/notification/events.rs` (new `DayOhlcSealed` event)
- **Tests:** `test_arm_sid_at_0915_uses_preopen_backtrack`, `test_arm_sid_empty_buffer_emits_index_ohlc_01`, `test_seal_day_at_1530_locks_close`, `test_midnight_reset_clears_all_4_sids`, source-scan guard `secret_manager.rs::tests::test_arm_sid_wired_at_0915_boundary`.

### Slice 2 — Constants + config pinned

- [ ] Add to `crates/common/src/constants.rs`:
  ```rust
  pub const OPTION_CHAIN_FETCH_CADENCE_SECS: u64 = 50;
  pub const OPTION_CHAIN_MAX_CACHE_AGE_SECS: u64 = 60;
  pub const OPTION_CHAIN_BACKOFF_LADDER_SECS: [u64; 4] = [10, 20, 40, 80];
  pub const OPTION_CHAIN_STALE_CYCLES_BEFORE_TELEGRAM: u32 = 3;
  pub const OPTION_CHAIN_L2_VERIFY_TOLERANCE_PCT: f64 = 0.5;
  ```
- [ ] `[option_chain]` section to `config/base.toml`:
  - `fetch_cadence_secs = 50` (configurable for slowdown under L7 cooldown)
  - `max_cache_age_secs = 60` (operator-locked, do not raise)
- **Tests:** `test_option_chain_constants_pinned_at_50_60_etc` (5 assertions on the constants).

### Slice 3 — 8 OPTION-CHAIN-* emit sites tagged

- [ ] Replace existing `error!`/`warn!` calls in `option_chain/snapshot_scheduler.rs` + `client.rs` + `snapshot_cache.rs` with tagged versions: `error!(code = ErrorCode::OptionChain01FetchFailed.code_str(), ...)` etc.
- [ ] Wire `OptionChain05CacheStaleHaltStrategy` to the cache-age check (Slice 4).
- [ ] Wire `OptionChain08TokenExpiredMidCycle` to token-manager force-refresh path.
- **Tests:** `error_code_tag_guard.rs::every_error_macro_tagged_with_a_known_code_carries_code_field` is the ratchet (already exists; will fail build if any of the 8 sites miss the `code =` field).

### Slice 4 — Strategy fail-closed gate (heart-piece §5)

- [ ] In `crates/trading/src/strategy/`, add `pre_check_option_chain_freshness()` that runs at the top of every signal-evaluation cycle:
  ```rust
  for underlying in OPTION_CHAIN_UNDERLYINGS {
      let age = option_chain_cache.age_seconds(underlying);
      if age > OPTION_CHAIN_MAX_CACHE_AGE_SECS {
          emit_critical(ErrorCode::OptionChain05CacheStaleHaltStrategy, underlying, age);
          metrics::counter!("tv_strategy_skipped_total", "reason" => "stale_option_chain").increment(1);
          return None;  // do NOT trade
      }
  }
  ```
- [ ] Edge-triggered: only fire Telegram on rising edge per audit-findings Rule 4.
- **Tests:** `test_strategy_fail_closed_when_cache_age_61s`, `test_strategy_fail_closed_emits_option_chain_05`, `test_edge_triggered_no_telegram_spam`.

### Slice 5 — L2 VERIFY against live WS LTP (heart-piece §3 row 2)

- [ ] Read live WS index LTP from `SharedSpotPrices` (already populated by spot updater).
- [ ] Compare option-chain response `data.last_price` against WS LTP within 0.5% tolerance.
- [ ] Mismatch → emit `OPTION-CHAIN-04` (Severity::Medium, NOT auto-triage safe).
- **Tests:** `test_l2_verify_pass_within_05_pct`, `test_l2_verify_fail_emits_option_chain_04`, `test_l2_verify_uses_idx_i_segment_lookup`.

### Slice 6 — Audit chain (3 QuestDB tables — heart-piece §6)

- [ ] `option_chain_request_audit(ts, underlying_id, expiry, http_status, latency_ms, retry_count, error_class)` with DEDUP `(trading_date_ist, ts, underlying_id, expiry)`.
- [ ] `dhan_option_chain_raw(ts, underlying_id, expiry, json_body STRING, payload_hash)` with DEDUP `(trading_date_ist, ts, underlying_id, expiry, payload_hash)`. **Table referenced in `partition_manager.rs:42`** so the table exists conceptually — slice ensures the DDL + writer are wired.
- [ ] `option_chain_snapshots(ts, underlying_id, expiry, strike, side, oi, ltp, iv, delta, theta, gamma, vega, volume, top_bid, top_ask)` with DEDUP `(trading_date_ist, ts, underlying_id, expiry, strike, side)`.
- [ ] Idempotent `CREATE TABLE IF NOT EXISTS` + `ALTER ADD COLUMN IF NOT EXISTS` per schema-self-heal pattern.
- [ ] Async ILP writer task drains a bounded mpsc — no hot-path allocation in scheduler.
- **Tests:** `test_request_audit_dedup_includes_trading_date`, `test_raw_blob_dedup_includes_payload_hash`, `test_snapshot_dedup_includes_strike_side`, `dedup_segment_meta_guard.rs` already ratchets the DEDUP key shape.

### Slice 7 — L7 COOLDOWN + Thursday rollover (heart-piece §3 row 7 + §4 row 8)

- [ ] After 3 consecutive failed cycles, scheduler cadence escalates 50s → 60s → 90s → 120s. Resets to 50s on first success.
- [ ] On L2 VERIFY fail or empty `oc` map, re-fetch expirylist via `POST /v2/optionchain/expirylist` and rebuild cache for new nearest-expiry. Emit `OPTION-CHAIN-07` (Info).
- **Tests:** `test_cooldown_ladder_50_60_90_120`, `test_cooldown_resets_on_success`, `test_thursday_rollover_refetches_expirylist`.

### Slice 8 — 6 Prometheus alerts + Grafana panels

- [ ] Add to `deploy/docker/grafana/provisioning/alerting/alerts.yml`:
  - `tv-option-chain-cache-stale-60s` (Critical, gated by `tv_market_hours_active == 1`)
  - `tv-option-chain-cycle-skips-3-in-5m` (High)
  - `tv-option-chain-dh904-sustained` (High)
  - `tv-option-chain-l2-verify-fail` (Medium)
  - `tv-strategy-skipped-due-stale-chain` (Critical)
  - `tv-option-chain-rollover-detected` (Info)
- [ ] All wrapped in `increase()` / `rate()` per audit-findings Rule 12.
- [ ] Grafana operator-health dashboard: 6 new panels mirroring the alerts + per-underlying cache-age gauge.
- [ ] CloudWatch migration deferred to PR #10 per `THE-FINAL-PLAN.md` §5 row 10.
- **Tests:** `resilience_sla_alert_guard.rs` already ratchets that every new counter has an alert rule; `operator_health_dashboard_guard.rs` ratchets the Grafana panel.

---

## §4. Z+ 15-row "100% everything" matrix

| Demand | Mechanical proof |
|---|---|
| Code coverage | Per-slice unit tests + 3 integration tests (boot wiring, cross-verify pre-stage, replay from `dhan_option_chain_raw`) |
| Audit coverage | 3 new audit tables with DEDUP UPSERT KEYS per Z+ rule + SEBI 5y retention via S3 lifecycle |
| Testing coverage | 22 test categories per `testing.md` covered for `crates/core/src/option_chain/` + `crates/trading/src/in_mem/day_ohlc_tracker.rs` (the two changed crates) |
| Code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + 9 pre-commit gates green |
| Performance | DHAT zero-alloc test on scheduler tick (no allocation per cycle); Criterion p99 ≤ 200ms per request (REST is cold path); ≤50ns per tick for `DayOhlcTracker::update_tick` (hot path) |
| Monitoring | 7-layer telemetry per `wave-4-shared-preamble.md` §4: Prom counter + gauge + tracing span + Loki log + Telegram event + Grafana panel + audit table — every emit site covers all 7 |
| Logging | All 8 OPTION-CHAIN-* + INDEX-OHLC-01/02 emit sites tagged via `error_code_tag_guard.rs` |
| Alerting | 6 Prometheus alerts in `alerts.yml`, 2 INDEX-OHLC alerts already in place, all edge-triggered |
| Security | No new attack surface — option-chain REST already authenticated; `Secret<String>` JWT plumbing unchanged |
| Security hardening | `client-id` + `access-token` headers always sent per Dhan v2.5; ratcheted by `test_client_id_header_required` (new) |
| Bug fixing | 3-agent adversarial review on Slice 1 + Slice 4 + Slice 6 diffs BEFORE merging |
| Scenarios | 8 failure modes from heart-piece §4 each have a test; pre-open empty buffer scenario tested; Thursday rollover tested |
| Functionalities | Every new `pub fn` has call site + test (gates 6+11 green); `DayOhlcTracker::arm_sid` finally has a production call site (closes Rule 13 gap) |
| Code review | Adversarial 3-agent on diff BEFORE impl (during plan review) AND AFTER impl on the diff |
| Extreme check | All ratchets (error_code_tag_guard, dedup_segment_meta_guard, operator_health_dashboard_guard, resilience_sla_alert_guard) fail build on regression |

---

## §5. Z+ 7-row Resilience matrix

| Demand | Honest envelope |
|---|---|
| Zero ticks lost | Unchanged — option-chain REST is cold path; tick rescue ring untouched |
| WS never disconnects | Unchanged — same 1 main-feed + 1 order-update WS |
| Never slow/locked | Scheduler tick is async + bounded mpsc; strategy fail-closed gate adds <1µs to signal evaluation |
| QuestDB never fails | Audit tables follow same 3-tier rescue→spill→DLQ pattern; ALTER ADD COLUMN IF NOT EXISTS schema self-heal |
| O(1) latency | `cache.age_seconds(underlying)` is O(1); papaya pin/get for DayOhlcTracker is O(1) |
| Uniqueness + dedup | Composite DEDUP `(trading_date_ist, ts, underlying_id, expiry [, strike, side])` per I-P1-11 |
| Real-time proof | 60s cache-age threshold + 50s fetch cadence = Critical Telegram within ~70s of stale-cache detection |

---

## §6. Honest 100% claim (per operator-charter §F)

> "100% inside the LOCKED envelope: option chain fetched every 50s for 3 underlyings concurrently (NIFTY=13, BANKNIFTY=25, SENSEX=51); cache freshness monitored at strategy evaluation; strategy fail-closes within 60s of staleness via the OPTION-CHAIN-05 path; every request audited via `option_chain_request_audit` and full JSON body via `dhan_option_chain_raw`. Pre-open / market-open day_open for the 4 IDX_I SIDs sourced ONCE per trading day at 09:15:00 IST from `PreOpenCloses::backtrack_latest()` (= NSE equilibrium open). Day OHLC sealed at 15:30:00 IST. Beyond the envelope (Dhan REST regional outage > 60s, simultaneous network failure, JWT compromise mid-session) strategy refuses to emit signals AND operator is paged within 60 seconds via 3 independent SNS legs.
>
> **NOT promised:** literal "never miss a fetch" — Dhan REST has no published SLA. What IS promised: detection within 30s, alert within 60s, strategy fail-closed within 90s, full audit trail for SEBI replay."

---

## §7. Scenarios covered (per `wave-4-shared-preamble.md`)

| # | Scenario | Expected behavior |
|---|---|---|
| 1 | Fresh boot 22:00 IST (off-hours) | Scheduler spawns, immediately sleeps until next market open. Cache age gauge unreported. |
| 2 | 09:00 IST market open, 4 IDX_I SIDs in pre-open buffer | At 09:15:00 IST `arm_sid()` fires for all 4 SIDs with `backtrack_latest()` value. Cross-verify at 15:31 IST matches on OPEN field. |
| 3 | Pre-open buffer empty for 1 of 4 SIDs (Dhan TCP-RST during pre-market) | `INDEX-OHLC-01` Critical Telegram for that SID. Fallback to first-trade LTP. Cross-verify MISMATCHES at 15:31 IST → `CROSS-VERIFY-01` (also Critical). Operator inspects. |
| 4 | DH-904 rate limit on NIFTY fetch | Backoff ladder 10s → 20s → 40s → 80s. On 80s exhaustion, `OPTION-CHAIN-02` Telegram. Other 2 underlyings unaffected. |
| 5 | NIFTY cache age 61s during market hours | Strategy `pre_check_option_chain_freshness()` returns None. `OPTION-CHAIN-05` Critical Telegram (edge-triggered). `tv_strategy_skipped_total` counter increments. No trade. |
| 6 | Thursday 15:30 IST expirylist rollover | Mid-day L2 VERIFY fail or empty `oc` map → re-fetch expirylist → rebuild cache → `OPTION-CHAIN-07` Info Telegram. |
| 7 | Cycle takes > 50s (overlap) | `tokio::Mutex<CycleState>` skip-next policy. Alarm fires if 2 skips in row. |
| 8 | Token expired mid-cycle (HTTP 401) | Token-manager force-refresh. Retry within same cycle. `OPTION-CHAIN-08` Telegram if refresh fails. |
| 9 | L2 VERIFY: option-chain LTP off from WS LTP by 0.7% | `OPTION-CHAIN-04` Medium Telegram. NOT auto-triage safe — operator decides parser-bug vs Dhan-data-anomaly. |
| 10 | 15:30:00 IST seal | `DayOhlcSealed` event per SID. 4 INFO Telegrams (or coalesced as one). day_close locked. |
| 11 | IST midnight reset | `reset_daily_all()` clears all 4 SID states. Next day's 09:15:00 IST re-arms cleanly. |
| 12 | Reboot at 11:30 IST mid-session (post-`arm_sid` window) | `arm_sid()` is idempotent — if `day_open` already armed in QuestDB, reads it back. Otherwise fallback to first post-reboot tick. Cross-verify catches the mismatch. |

---

## §8. Open architectural decisions (per Rule 17 — front-loaded)

| Question | Decision (operator confirm/override) |
|---|---|
| **Q1:** Prometheus alerts vs CloudWatch — which lands in PR #8? | Prometheus + Grafana for PR #8 (live infra). CloudWatch migration is PR #10 per `THE-FINAL-PLAN.md` row 10. **Recommend: confirm Prometheus.** |
| **Q2:** Should `DayOhlcTracker::arm_sid()` wiring carve out as its own PR (#8a) or stay inside #8? | Stay inside PR #8 as Slice 1 (mechanically coupled to the 09:15:00 IST boundary task that #8 introduces; sharing tests, sharing the boot orchestrator spawn site). **Recommend: keep inside.** |
| **Q3:** What's the L2 VERIFY tolerance — 0.5% (per heart-piece §3) or tighter? | Lock at **0.5%** matching the design doc. Tighter risks false positives near close. Looser hides parser bugs. |
| **Q4:** What's the post-reboot recovery for `day_open` when arming missed? | Persist `day_open` to QuestDB `day_ohlc_audit` table at 09:15:00 IST. On reboot, `arm_sid()` queries the table — if today's row exists, reuse it; else fallback to first-trade LTP + INDEX-OHLC-01 alert. |
| **Q5:** Sub-PR split if any slice grows large? | Recommend keeping #8 as ONE PR. If 3-agent review surfaces CRITICAL/HIGH that forces a Rule-14-style anti-pattern, split as #8a (Slice 1 only) + #8b (Slices 2-8). Operator confirms split if needed. |
| **Q6:** SENSEX is BSE — does `arm_sid()` use IdxI segment for cross-verify? | Yes — `PREOPEN_INDEX_UNDERLYINGS` already lists SENSEX with sid=51 and segment IdxI (it's the IDX_I value feed, not the BSE derivative segment). Cross-verify against Dhan REST `charts/intraday` already handles SENSEX as IDX_I. |
| **Q7:** Will the rescue ring buffer the 50s scheduler payload if QuestDB is down? | Yes — async ILP writer task drains via the existing rescue→spill→DLQ chain. No hot-path impact. Documented in heart-piece §6 last paragraph. |

---

## §9. Estimated commits / slices: 8

## §10. Estimated LoC delta

- ~200 lines added per slice → ~1,600 lines total
- ~300 lines deleted (refactor of existing snapshot_scheduler emit sites)
- Net ~+1,300 lines
- 3 new QuestDB tables (DDL string per table ≈ 30 LoC)
- ~30 new tests across the 8 slices

## §11. Rollout checklist (per slice)

- [ ] `cargo check -p <crate>` green
- [ ] `cargo test -p <crate>` green
- [ ] Banned-pattern scanner clean
- [ ] Pub-fn-test guard clean
- [ ] Pub-fn-wiring guard clean (every new `pub fn` has a call site)
- [ ] `error_code_tag_guard.rs` green (every `error!` mentioning a code has the `code =` field)
- [ ] `dedup_segment_meta_guard.rs` green (every new DEDUP key includes segment)
- [ ] `resilience_sla_alert_guard.rs` green (every new counter has an alert rule)
- [ ] `operator_health_dashboard_guard.rs` green (every new counter has a Grafana panel)
- [ ] Commit with conventional message
- [ ] After all 8 slices: 3-agent adversarial review on diff
- [ ] Fix every CRITICAL + HIGH inline before opening PR
- [ ] Open draft PR, enable auto-merge, subscribe to PR activity
- [ ] After CI green: 3-agent adversarial pass again on final diff

---

## §12. The 6 OPEN questions for operator (verbatim from heart-piece §13)

Same as §8 above plus operator-charter §H ack: **"start coding now, OR plan more first?"**

If you say "approved", I open Slice 1 immediately. If you want a deeper carve-out on Q4/Q5, I'll re-draft.

---

## §13. Trigger / auto-load paths

Always loaded for sessions touching:
- `crates/core/src/option_chain/`
- `crates/trading/src/in_mem/day_ohlc_tracker.rs`
- `crates/core/src/instrument/preopen_price_buffer.rs`
- `crates/storage/src/option_chain_*.rs` (new)
- `.claude/plans/active-plan-pr-8-option-chain-heart-piece.md`
