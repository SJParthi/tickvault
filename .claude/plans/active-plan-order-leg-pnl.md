# Implementation Plan: Option-Contract-Leg P&L (dry-run order runtime)

**Status:** APPROVED
**Date:** 2026-07-19
**Approved by:** Parthiban (operator) — standing GO 2026-07-19 ("just goa head with everythgin dude okay?", relayed via the coordinator session). Recorded per the operator's ZERO-TOUCH governance pre-authorization: plan Status is APPROVED the moment it exists for operator-ordered work.

> Scope: per-leg realized/unrealized P&L for OPTION CONTRACT legs in the dry-run order runtime, built on the PR #1649 option-contract mark tap. **Dry-run only — no live-order behavior change.** Design selected by a 3-designer panel + judge (winner: MVP-in-RiskEngine, with identity day-stamping + event_seq idempotency grafted from the risk lens and the tripwire-ordering argument from the persist lens).

## Design

**One P&L state, one formula source.** `RiskEngine` (`crates/trading/src/risk/engine.rs`) stays the ONLY P&L state — `record_fill` (:171) already implements open/add/weighted-avg/reduce/flip/close with finiteness + lot_size guards. We add a pure `PositionInfo::unrealized_at(mark: f64) -> f64` in `crates/trading/src/risk/types.rs` and make `RiskEngine::total_unrealized_pnl` (engine.rs:393) delegate to it, so exactly one unrealized formula exists (COMMON). No parallel ledger, no reimplemented math.

**Two emission seams in `crates/app/src/order_runtime.rs`.** A `Copy` struct `LegPnlEvent { ts_utc_ns: i64, sid: u64, segment_code: u8, event_kind: LegPnlKind /* Mark|Fill */, net_lots: i32, lot_size: u32, avg_entry_price: f64, mark_price: f64, realized_pnl: f64, unrealized_pnl: f64 }` is emitted:
1. after `apply_fill` → `risk.record_fill` (order_runtime.rs:822) — one `fill` row per applied paper fill;
2. after `risk.update_market_price` inside `process_mark` (order_runtime.rs:1208) — one `mark` row per applied option mark on a leg with an open position or pending order (`process_mark` already filters, :1204-1207).
Both seams sit AFTER `book.tripwire_ok(sid, segment_code)` (:819 / :1175), so cross-segment token collisions are refused upstream and rows are never misattributed even though `RiskEngine` keys sid-only (an acknowledged interim; the composite `(security_id, exchange_segment)` RiskEngine rewrite is its own pre-live follow-up, NOT this PR). Only FNO segment codes (2 = NSE_FNO, 8 = BSE_FNO) emit — IDX_I spot marks never produce rows. Every event carries `segment_code` (free: `MarkUpdate` (:135) and `FillEvent` (`crates/trading/src/oms/types.rs:103`) both already carry it), so every NEW collection/row is composite from day one.

**Leg identity, resolved consumer-side.** A reverse index `HashMap<(u64, u8), OptionLegIdentity { underlying: Arc<str>, expiry: NaiveDate, strike_paise: i64, option_type: OptionType }>` is built beside `build_contract_mark_index` in `crates/app/src/groww_cadence_executor.rs` (:1290-1308) — once per day-cache build, zero extra REST, behind the same `is_some()`-style gate — and published via `Arc<ArcSwapOption<HashMap<...>>>` (arc-swap 1.9.0, already pinned), day-stamped and replaced on each daily build. The persistence CONSUMER resolves identity per row at append time, so a late day-cache self-heals: pre-publish rows persist honest sentinels (`underlying="n/a"`, `strike_paise=-1`, `option_type="n/a"` — the order_update_events precedent) and are counted via `tv_order_leg_pnl_identity_unresolved_total`. Rejected alternatives (recorded): a parallel `LegPnlBook` in the risk crate (double bookkeeping vs the COMMON rule, would need a divergence tripwire); an app-side tracker hand-mirroring the engine formula (silent-drift class); extending `PositionInfo` with identity (bloats a Copy type on the live pre-trade path); extending `FillEvent` with identity (touches all fill sites; consumer-side per-row resolution is smaller AND self-heals).

**New table `order_leg_pnl`** (PARTITION BY DAY; registered in `crates/storage/src/partition_manager.rs` `DAY_PARTITIONED_TABLES`, :55). Columns: `ts` TIMESTAMP (designated), `trading_date_ist` SYMBOL, `feed` SYMBOL (`'groww'`), `security_id` LONG, `segment` SYMBOL, `event_seq` LONG (reuses `broker_order_events::next_event_seq` — ILP-retry idempotency), `event_kind` SYMBOL (`'mark'`|`'fill'`), `underlying` SYMBOL, `expiry` SYMBOL, `strike_paise` LONG, `option_type` SYMBOL, `net_lots` INT, `lot_size` INT, `avg_entry_price` DOUBLE, `mark_price` DOUBLE, `realized_pnl` DOUBLE (cumulative-day snapshot at emit time), `unrealized_pnl` DOUBLE, `mode` SYMBOL (`'paper'`).
`const DEDUP_KEY_ORDER_LEG_PNL = "ts, trading_date_ist, feed, security_id, segment, event_seq"` — **feed IS in the key per the I-P1-11 extension**, and `segment` sits beside `security_id` per `dedup_segment_meta_guard`. New file `crates/storage/src/order_leg_pnl_persistence.rs` mirrors the house exemplar `order_update_events_persistence.rs` step-for-step: TABLE + DEDUP consts → idempotent DDL (CREATE → ALTER ADD COLUMN IF NOT EXISTS → DEDUP ENABLE) → lazy ILP-over-HTTP writer (`retry_timeout=0;request_timeout=5000`; non-finite clamp counted; flush failure → `discard_pending` counted).

**Writer off the select loop.** New `crates/app/src/order_leg_pnl_boot.rs` (the `order_update_events_boot` pattern): the runtime producer does a bounded `try_send` of the ~72-byte Copy struct into an mpsc channel (capacity from config, default 2048); a supervised consumer task resolves identity from the day Arc, builds strings consumer-side, appends + flushes. Drops counted `tv_order_leg_pnl_dropped_total{reason="full"|"closed"}` with an edge-latched coded error per episode. The runtime select loop never blocks and never allocates for P&L.

**Config + gate.** `OrderLegPnlConfig { enabled: bool, channel_capacity: usize }` beside `OrderRuntimeConfig` in `crates/common/src/config.rs`; `#[serde(default)]` + manual `Default { enabled: false, channel_capacity: 2048 }` (an ABSENT section is OFF — fail-safe); `config/base.toml` gains `[order_leg_pnl] enabled = true`. Effective gate = `order_runtime.enabled && order_leg_pnl.enabled`, resolved ONCE at the `crates/app/src/dhan_rest_stack.rs` spawn; pnl-on/runtime-off logs one honest boot line. OFF ⇒ the runtime receives `None` for the sender and is byte-identical to today.

**Error code.** ONE new variant `ErrorCode::OrderPnl01PersistFailed` (`code_str() == "ORDER-PNL-01"`) in `crates/common/src/error_code.rs` (~5 match sites), `stage ∈ {ensure_client_build, ensure_ddl, append, flush, sink_drop}` (the ORDER-EVT-01 template). LOG-SINK-ONLY (noise-lock posture — no Telegram, no CloudWatch alarm). New rule file `.claude/rules/project/order-leg-pnl-error-codes.md` carries the variant + code string verbatim (crossref guard) and is its own `runbook_path` target.

**Hot-path honesty (verbatim, binding):** DHAT deliberately NOT claimed — over-claiming on a minute-cadence cold path; the genuinely hot MarkForwarder tap is byte-untouched and keeps its existing `dhat_mark_forward.rs` proof. This item is NOT the tick hot path (no live WS exists; marks arrive at minute cadence after the option_chain_1m flush ACK, fills are rare paper events). Per event: O(1) TIME (one HashMap get + ~6 f64 ops + one bounded try_send) and O(1) SPACE (bounded channel; overflow drops counted, never grows). Zero producer-side allocation — all strings are resolved consumer-side from the day Arc.

## Edge Cases

1. **16:00 daily reset × cumulative realized:** `realized_pnl` is a cumulative-DAY snapshot of the engine's per-leg realized at emit time. The runtime's daily reset zeroes engine state; rows before/after are separated by `trading_date_ist`, and a post-reset row legitimately restarts at 0.0 — never back-adjusted. Documented in the rule file.
2. **15:30 close:** NO synthetic close-sweep row — the day's final minute-mark row is the closing unrealized snapshot (decided: last-mark semantics).
3. **IDX_I spot legs:** never emit (segment gate admits only 2|8).
4. **Same numeric token on NSE_FNO and BSE_FNO:** distinct rows — `segment` is in the DEDUP key AND in the identity-index key (I-P1-11).
5. **Identity miss** (day cache not yet published at boot, or unknown contract): sentinel row persisted + `tv_order_leg_pnl_identity_unresolved_total`; later rows self-heal once the index publishes.
6. **Non-finite values:** producers are already clean (`f32_to_f64_clean` on marks; `record_fill` finiteness guards); the writer additionally clamps non-finite doubles with a counted clamp (exemplar pattern) — defense in depth.
7. **Mark with no open position and no pending order:** `process_mark` already filters (:1204-1207) — no event, no row.
8. **lot_size 0:** treated as 1 (mirrors the existing `record_fill` guard); `unrealized_at` mirrors it, boundary-tested.
9. **Flip through zero in one fill:** `record_fill` realizes the closed portion and re-anchors `avg_entry_price`; the single `fill` row captures the post-flip state.
10. **ILP retry duplicates:** identical `event_seq` collapses under DEDUP; the first-boot pre-DEDUP-ensure window (auto-created table without DEDUP until a later boot's ensure) is a documented bounded dup window (exemplar precedent).
11. **Channel full:** `try_send` drop counted; edge-latched coded error once per episode; the runtime never blocks.
12. **Aggregates:** no per-leg CloudWatch gauges — log-sink-only + `questdb_sql` is the operator surface (noise-lock).

## Failure Modes

- **QuestDB down:** append/flush fails → ORDER-PNL-01 `stage=append|flush`, pending rows DISCARDED (poisoned-buffer defense) + counted; the runtime is unaffected. Rows for the outage window are lost — best-effort forensics; RiskEngine state remains the decision truth (stated plainly, never camouflaged).
- **DDL ensure fails at boot:** `stage=ensure_ddl`; the writer continues — the first ILP write may auto-create the table WITHOUT DEDUP keys until a later boot's ensure succeeds (honest residual, documented in the rule file).
- **Consumer task death:** release builds abort on panic (`panic = "abort"`); unwind (dev/test) builds self-heal via the supervised respawn counter `tv_order_leg_pnl_task_respawn_total` (house pattern).
- **Day-index publish failure:** identity stays unresolved → sentinel rows + counter; marks/fills are never blocked.
- **Config OFF / runtime OFF:** zero events, zero channel, runtime byte-identical (sender `None`).
- **Future second feed:** `feed` in every row + key — rows can never collide across feeds.

## Test Plan

Scoped per testing-scope.md: `cargo test -p tickvault-trading -p tickvault-app -p tickvault-storage`.

- **trading:** financial boundary tests on `unrealized_at` (zero lots, short position, lot_size 0→1, non-finite mark refused/propagated per guard, large-magnitude values) + a delegation-equivalence pin (`total_unrealized_pnl` == Σ `unrealized_at` over positions) — satisfies the financial-test guard for new P&L math.
- **app:** state-machine walk open→add→reduce→flip→close through the `apply_fill`/`process_mark` harness asserting the emitted `LegPnlEvent` sequence + values; cross-segment same-token distinct emissions; segment gate (IDX_I emits nothing); identity hit / sentinel / late-heal / day-reset; OFF emits nothing; gate resolved once with the honest pnl-on/runtime-off boot line.
- **storage:** idempotent DDL (ensure twice); DEDUP const carries feed + segment (rides `dedup_segment_meta_guard`); non-finite clamp counted; flush-failure → `discard_pending` + counted (exemplar test shape); append-after-failure recovery.
- **Guards riding:** `dedup_segment_meta_guard`, `error_code_tag_guard`, `error_code_rule_file_crossref` (rule file carries the variant verbatim), `error_level_meta_guard` (persist failures use `error!`), `partition_retention_coverage_guard` (table registered), pub-fn test + wiring guards, test-count ratchet.
- **NO DHAT test** — deliberate (see the hot-path honesty note in Design); the existing `dhat_mark_forward.rs` proof for the genuinely hot MarkForwarder is untouched.

## Rollback

- Flip `[order_leg_pnl] enabled = false` (or delete the section — serde default is OFF): next boot passes `None` as the sender; the runtime is byte-identical to today. No restart-order hazard.
- The `order_leg_pnl` table is RETAINED (house posture — tables are never dropped); the partition manager ages it out under the Standard 90d class.
- Full revert = revert the single PR; purely additive (new table + new config section + new module files), no migration to unwind, zero live-order paths touched (`dry_run` hard-true untouched; writes only `order_leg_pnl` — live-feed purity clean).

## Observability

- **Counters (static labels only):** `tv_order_leg_pnl_rows_total{feed}` · `tv_order_leg_pnl_persist_errors_total{stage}` · `tv_order_leg_pnl_dropped_total{reason}` · `tv_order_leg_pnl_rows_discarded_total` · `tv_order_leg_pnl_identity_unresolved_total` · `tv_order_leg_pnl_task_respawn_total`.
- **Coded logs:** every failure path `error!` with `code = ErrorCode::OrderPnl01PersistFailed.code_str()` + a `stage` field; `sink_drop` edge-latched per episode (audit Rule 4).
- **Runbook:** `.claude/rules/project/order-leg-pnl-error-codes.md` — documents cumulative-realized snapshot semantics, the 16:00 reset, fill-row `mark_price` = fill price, and the pre-DEDUP-ensure dup window.
- **Delivery boundary (honest, no false-OK):** LOG-SINK-ONLY — zero new Telegram events, zero CloudWatch alarms (the Dhan/Groww noise-lock posture). Operator surface = coded logs + counters + `questdb_sql` on `order_leg_pnl`.

## Plan Items

- [ ] Item 1 — pure `PositionInfo::unrealized_at` + engine delegation
  - Files: crates/trading/src/risk/types.rs, crates/trading/src/risk/engine.rs
  - Tests: test_unrealized_at_boundaries, test_total_unrealized_delegation_equivalence
- [ ] Item 2 — `LegPnlEvent` emission at the fill + mark seams (FNO-only gate)
  - Files: crates/app/src/order_runtime.rs
  - Tests: test_leg_pnl_fill_state_walk, test_leg_pnl_mark_emission_fno_only, test_leg_pnl_cross_segment_distinct, test_leg_pnl_off_emits_nothing
- [ ] Item 3 — day identity reverse index + ArcSwap publish
  - Files: crates/app/src/groww_cadence_executor.rs
  - Tests: test_leg_identity_index_build, test_leg_identity_sentinel_then_heal
- [ ] Item 4 — `order_leg_pnl` persistence (DDL + DEDUP + writer)
  - Files: crates/storage/src/order_leg_pnl_persistence.rs, crates/storage/src/lib.rs, crates/storage/src/partition_manager.rs
  - Tests: test_order_leg_pnl_ddl_idempotent, test_order_leg_pnl_dedup_key_has_feed_and_segment, test_order_leg_pnl_flush_failure_discards, test_order_leg_pnl_nonfinite_clamp
- [ ] Item 5 — config + boot consumer + gate wiring
  - Files: crates/common/src/config.rs, config/base.toml, crates/app/src/order_leg_pnl_boot.rs, crates/app/src/dhan_rest_stack.rs, crates/app/src/main.rs
  - Tests: test_order_leg_pnl_config_default_off, test_boot_gate_resolved_once
- [ ] Item 6 — `ErrorCode::OrderPnl01PersistFailed` + rule file
  - Files: crates/common/src/error_code.rs, .claude/rules/project/order-leg-pnl-error-codes.md
  - Tests: rides error_code_tag_guard, error_code_rule_file_crossref, all-variants match tests

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | mark on open long leg | one `mark` row; unrealized = (mark − avg_entry) × net_lots × lot_size |
| 2 | fill flips long→short | one `fill` row; realized snapshot includes closed-portion P&L; avg re-anchored |
| 3 | same token on NSE_FNO + BSE_FNO | two distinct rows (segment in key + identity key) |
| 4 | identity cache not yet built | sentinel row + unresolved counter; later rows resolved |
| 5 | channel full | drop counted; runtime unaffected |
| 6 | QuestDB down during flush | pending discarded + counted; ORDER-PNL-01 stage=flush |
| 7 | pnl enabled, runtime disabled | one honest boot line; zero events |
| 8 | 16:00 reset then new fill | realized restarts from the fresh engine state; trading_date_ist separates days |

## Per-Item Guarantee Matrix

15-row 100% Guarantee Matrix (per `.claude/rules/project/per-wave-guarantee-matrix.md`, item-specific):

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | ratcheted per-crate floors (trading/storage/app) only move up; `scripts/coverage-gate.sh` | post-merge llvm-cov | PR adds tests for every new pub fn |
| 100% audit coverage | `order_leg_pnl` table with DEDUP UPSERT KEYS (feed + segment in key) | `mcp__tickvault-logs__questdb_sql` | table registered in partition_manager |
| 100% testing coverage | unit + integration + financial-boundary categories declared in Test Plan | scoped `cargo test` green | Test Plan section above |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan | pre-push mandatory | all gates green before PR |
| 100% code performance | O(1) time+space per event; zero producer-side alloc; DHAT deliberately NOT claimed (minute-cadence cold path; MarkForwarder keeps `dhat_mark_forward.rs`, byte-untouched) | n/a — not the tick hot path | honesty note in Design |
| 100% monitoring | 6 `tv_order_leg_pnl_*` counters | log sink + questdb_sql | Observability section |
| 100% logging | every failure `error!` with `code = ORDER-PNL-01` | `error_level_meta_guard` | `error_code_tag_guard` |
| 100% alerting | LOG-SINK-ONLY by contract (noise-lock) — deliberately no new alarm | N/A — documented no-alert row | rule file records the delivery boundary |
| 100% security | no secrets, no external input, no new endpoint; security-reviewer pass | `cargo audit` in CI | 3-agent review |
| 100% security hardening | zero new attack surface (internal channel + ILP writer only) | N/A — declared | review confirms |
| 100% bugs fixing | adversarial 3-agent review BEFORE and AFTER impl | pre-PR + post-impl passes | 2 consecutive clean refuter rounds |
| 100% scenarios covering | Edge Cases 1–12 each test-pinned | scoped tests | Scenarios table above |
| 100% functionalities covering | every new pub fn has call site + matching test | pre-push gates 6+11 | pub-fn guards |
| 100% code review | 3-agent on this plan AND on the final diff | per-PR | recorded in the PR body |
| 100% extreme check | dedup_segment_meta_guard + error_code crossref + partition guard fail the build on regression | every commit | ratchet guards ride |

7-row Resilience Demand Matrix:

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | N/A-class for this item — P&L rows are best-effort forensics; RiskEngine state is the decision truth; drops counted, never silent | no new tick-drop path; runtime byte-identical when OFF |
| WS never disconnects | N/A — no WebSocket touched (REST-only runtime) | zero WS code in the diff |
| Never slow/locked/hanged | bounded `try_send` producer; consumer runs off the select loop | no hot-path allocation; no blocking I/O in the runtime |
| QuestDB never fails | ABSORB: append/flush failures discard pending + counted; runtime unaffected | flush-failure test |
| O(1) latency | O(1) time + space per event (one HashMap get + ~6 f64 ops + try_send of a Copy struct) | test-pinned; identity resolution is one consumer-side hash lookup |
| Uniqueness + dedup | key = `ts, trading_date_ist, feed, security_id, segment, event_seq` | dedup_segment_meta_guard + idempotency test |
| Real-time proof | counters + coded logs; log-sink-only delivery documented | Observability section |

**Honest 100% claim:** 100% inside the tested envelope, with ratcheted regression coverage: per-leg P&L rows are emitted at the two tripwire-gated runtime seams and persisted best-effort through a bounded channel (default capacity 2048, drops counted) into `order_leg_pnl` with DEDUP-idempotent replay; the RiskEngine remains the single P&L state and decision truth. Beyond the envelope: a QuestDB outage loses forensic rows for the outage window only (counted, never silent); identity misses persist honest sentinels.
