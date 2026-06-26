# Implementation Plan: ONE common feed engine (live → 1m → parity), feed = only the fetch/pull

**Status:** APPROVED
**Date:** 2026-06-22
**Approved by:** Parthiban — verbatim 2026-06-22: "only feed live ticks will be fetched and pulled right dude then from there everything is same … if we add other feeds also it should never ever be disturbed … make everything as common runtime dynamic scalable approach"; AskUserQuestion answer "One common engine in one shot"; + Groww-second-feed lock §0 Quote 1 (live↔backtest 1m parity is the locked deliverable).

## Design — the ONE principle

**Feed-specific = ONLY two small adapters: (1) the live-tick producer (the feed's wire protocol), (2) the historical/backtest fetcher (the feed's history API). EVERYTHING else is ONE common, feed-parameterized runtime.** Adding a future feed = implement those 2 adapters + add a `Feed` enum variant. The shared pipeline is never touched.

```
 FEED-SPECIFIC (2 tiny adapters/feed)        COMMON RUNTIME (shared, feed = a parameter)
 ┌──────────────────────────────┐     ┌────────────────────────────────────────────────┐
 │ producer: wire→ParsedTick     │────►│ WAL→ring→spill→DLQ→tick processor→               │
 │   Dhan=binary WS              │     │ 1-min fold cell→candles_1m (feed column)→        │
 │   Groww=sidecar NDJSON        │     │ generic exact comparator→feed_parity_1m_audit→   │
 │ BacktestSource: history→Candle│────►│ CSV→Telegram. ALL generic; `feed` is just a label│
 │   Dhan=REST /charts/intraday  │     │                                                  │
 │   Groww=growwapi historical   │     │                                                  │
 └──────────────────────────────┘     └────────────────────────────────────────────────┘
```

Research (2 parallel agents, 2026-06-22, file:line-cited) confirmed:
- `candles_1m` is ALREADY one shared table with a `feed` SYMBOL column + DEDUP `(ts, security_id, segment, feed)` (`shadow_persistence.rs:114`). Dhan+Groww rows coexist. The table is already common.
- The 1-minute fold cell, the exact-match comparator, and the audit-table schema are ALREADY structurally identical across Dhan and Groww — they were just copy-forked. They unify cleanly.
- The only genuinely feed-specific code is the wire producer + the historical adapter.

### What moves where (no circular deps; flow common ← core ← trading ← storage ← api ← app)

| Common piece | Target crate | Action |
|---|---|---|
| `Feed` enum + `as_str`/`parse` + ONE label source | **common** (`common::feed`) | MOVE from `api::feed_state`; kill 3 scattered label consts (`CANDLE_FEED_DHAN`, `GROWW_FEED_LABEL`, `TICK_FEED_*`); `api` re-exports for `FeedRuntimeState` |
| `Candle1m` type + generic exact comparator + `Mismatch` | **common** | EXTRACT from `parity_1m.rs::compare_groww_1m` + `cross_verify_1m_boot.rs::diff_minute_candles` (identical algos) |
| 1-minute fold cell (per `(security_id, ExchangeSegment)`) | **trading** (`candles/`) | GENERIFY the inner cell; Groww reuses it; Dhan's 21-TF papaya container keeps using it. Common key `(i64, ExchangeSegment)`, common late-tick = re-fold (`AmendedLate`) |
| `GenericCandle1mWriter(feed)` | **storage** | MERGE `ShadowCandleWriter` + `GrowwCandle1mWriter` into one feed-param writer |
| `feed_parity_1m_audit` (ONE table, `feed` in DEDUP) | **storage** | MERGE `cross_verify_1m_audit` + `groww_cross_verify_1m_audit` (byte-identical schemas) → one table, DEDUP `(trading_date_ist, security_id, segment, minute_ts_ist, field, feed)` |
| `trait BacktestSource { async fn fetch_1m(...) -> Vec<Candle1m> }` + per-feed impls | **core** | NEW trait; Dhan REST impl MOVES from `app`; Groww sidecar impl lives here. Cold path → `dyn` OK |
| Generic parity orchestrator `run_parity_1m(feed, &dyn BacktestSource)` | **app** | GENERIFY `cross_verify_1m_boot.rs`; one scheduler runs it per enabled feed |
| Error codes `PARITY-01`/`PARITY-02` (feed label in payload) | **common** | GENERIFY `CROSS-VERIFY-1M-*`; keep old as aliases or migrate |

**Delete/merge:** `core/feed/groww/parity_1m.rs`, `storage/groww_cross_verify_audit_persistence.rs`, the `GrowwCandle1mWriter` half of `groww_candle_persistence.rs`, `Groww1mAggregator` (reuse the common cell), and the Dhan-specific diff/intraday parse inside `cross_verify_1m_boot.rs` (extract→common, move REST→core).

**Volume stays COMMON, parameterized:** the comparator can compare volume; whether to is a per-feed *capability flag* (`compares_volume: bool`), NOT per-feed code. Groww live carries no volume → its flag is false (OHLC-only); Dhan → true. One comparator, one config knob. Honest envelope preserved without a code fork.

### ALL timeframes from ticks, for EVERY feed (operator lock 2026-06-22: "from ticks always generate all candle timeframes")
From ticks the common engine generates **ALL 21 timeframes** for every feed — NOT just 1-minute. Today Dhan already does this (21-TF `MultiTfAggregator` → `candles_*` shadow tables); Groww only does 1m. The convergence makes BOTH feeds run the SAME full 21-TF aggregator, every TF tagged `feed`. The 1-minute is merely the slice cross-verified vs backtest (the §0 deliverable); GENERATION is all-TF. So Groww moves to the full `MultiTfAggregator`, and EVERY `candles_<tf>` shadow table gets the `feed` column + `feed` in its DEDUP key (the NULL-backfill template applied to ALL TF tables, not just 1m).

**Aggregator unification (danger zone — gated by golden tests):** Dhan's aggregator is the 21-TF concurrent `papaya` engine (hot path); Groww's is single-TF `std::HashMap`. Make Groww reuse the FULL `MultiTfAggregator`, parameterized by per-feed `FeedStrategy { late_policy, volume_width:i64, baseline, key_width }`. Groww's path is lower-rate (sidecar NDJSON) so feeding the 21-TF container stays O(1)/tick (hot-path agent confirmed sharing is alloc-free). Golden test: Groww's 1m seal through the shared engine == its current 1m candle (no silent change); its other 20 TFs are now produced + persisted. Cum-volume-delta + key-width(i64) verified through the shared engine, never assumed.

## Edge Cases
- New feed added: implement producer + BacktestSource + add `Feed` variant + register in `FeedRuntimeState`. Pipeline untouched. (The whole point.)
- Feed disabled at runtime: orchestrator skips that feed; no fetch, no compare.
- Feed with no live volume (Groww): `compares_volume=false`; volume reported informational, never a mismatch.
- Empty/partial live or backtest set: false-OK guard → "Blind/Degraded", never a false "all match" (audit Rule 11).
- Migration: the two old audit tables → one. New table `CREATE IF NOT EXISTS`; old data stays queryable; no destructive drop.

## Failure Modes
- BacktestSource fetch fails (REST 5xx / sidecar crash): `PARITY-02` degrade, partial coverage, never blocks, next day re-runs.
- QuestDB unreachable: degrade + emit, no panic (mirror existing classification).
- Aggregator cell convention drift: caught by the pre-switchover Groww volume/seal parity test.
- Layering violation introduced: caught at compile time (common can't see core/api).

## Test Plan
- `common::feed::Feed`: round-trip `as_str`/`parse`, every variant.
- common comparator: OHLC exact mismatch flagged; volume gated by capability flag; `compared>0` false-OK guard.
- generic fold cell: Groww ticks through the shared cell produce identical seals to the old `Groww1mAggregator` (golden test before deleting it).
- generic candle writer: Dhan row + Groww row both land in `candles_1m` with correct `feed` + DEDUP.
- one audit table: Dhan + Groww mismatches coexist, DEDUP includes `feed`.
- BacktestSource: Dhan REST parse test (moved), Groww sidecar parse test.
- orchestrator: schedule decision + run-status classification per feed.
- `cargo test --workspace` (common change → escalate per testing-scope).
- Adversarial 3-agent review (hot-path + security + hostile) before AND after.

## Rollback
Staged sub-PRs, each additive + gated. The `Feed` move is a re-export (api unchanged externally). The unified writer/audit are behind the same boot wiring; reverting any sub-PR restores the prior siloed module with zero data loss (shared table unchanged). `feeds.groww_enabled` default OFF.

## Observability
`PARITY-01`/`PARITY-02` (feed in payload) → Telegram + `feed_parity_1m_audit` + CSV. Counters `tv_feed_parity_mismatches_total{feed}`, gauge `tv_feed_parity_compared_minutes{feed}`. Per-feed per-day Telegram summary. Mismatch-count TREND is the signal.

## Plan Items (serial sub-PRs — one finished+merged before the next)
- [x] SP1 — `common::feed::Feed` (move from api, re-export, kill scattered label consts) + `Feed::ALL` single source (MED finding) + `FeedStrategy` type skeleton. Files: `crates/common/src/feed.rs`, `crates/common/src/lib.rs`, `crates/api/src/feed_state.rs`, `crates/api/src/handlers/feeds.rs`, `feeds_page.rs`, the 3 label-const sites. Tests: feed round-trip; every list built from `Feed::ALL`; exhaustive `match` (no `_`). **MERGED #1176.**
- [x] SP2 — `common` generic `Candle1m` + exact comparator + `Mismatch` with `count_live_only` + `compares_volume` capability flags (CRIT+HIGH findings). Files: `crates/common/src/feed_parity.rs`. Tests: Dhan "ignore live-only" golden; OHLC mismatch flagged; volume gated by flag; positive-signal (non-zero price) guard so all-zero is NOT a false Pass; `compared>0` false-OK guard. **MERGED #1177.**
- [x] SP3 — generify the 1-minute fold cell with per-feed `FeedStrategy { late_policy: Refold|Discard, volume_width: i64, baseline (required arg) }` (3 CRIT + 2 HIGH/LOW findings); Groww reuses it; delete `Groww1mAggregator`. BLOCKERS: golden test Groww candle UNCHANGED vs today; Groww-cum-volume(i64)-through-shared-cell test; key-width guard (Groww token > u32::MAX must not alias — verify ≤u32 OR widen key to `(i64,ExchangeSegment)`); non-zero baseline-carry test. Files: `crates/trading/src/candles/aggregator_cell.rs`, `crates/core/src/feed/groww/aggregator_1m.rs` (delete), `groww_bridge.rs`. **MERGED #1179 (SP3a fold cell) + #1180 (SP3b Groww delegates); golden test `crates/trading/tests/groww_one_candle_engine_golden.rs` GREEN (6 passed).**
- [x] SP4 — `GenericCandle1mWriter(feed)` merging the two writers. Files: `crates/storage/src/shadow_candle_writer.rs`, `crates/storage/src/groww_candle_persistence.rs` (merge). **MERGED #1204** (`crates/storage/src/generic_candle_writer.rs` present on base).
- [ ] SP5 — ONE `feed_parity_1m_audit` table + writer (feed in DEDUP), merging the two audit modules. MANDATORY migration (HIGH finding): `UPDATE feed='dhan' WHERE feed IS NULL` BEFORE `DEDUP ENABLE UPSERT KEYS(...,feed)` — mirror the candle-table NULL-backfill so no cross-feed overwrite. Files: `crates/storage/src/feed_parity_1m_audit_persistence.rs`, delete `groww_cross_verify_audit_persistence.rs` + `cross_verify_1m_audit_persistence.rs`. Tests: migration backfill + Dhan/Groww coexist without overwrite.
- [ ] SP6 — `trait BacktestSource` + Dhan REST impl (moved from app) + Groww sidecar impl. Files: `crates/core/src/feed/backtest_source.rs`, `crates/core/src/feed/dhan/intraday.rs`, `crates/core/src/feed/groww/backtest.rs`, `scripts/groww-sidecar/groww_backtest_fetch.py`.
- [ ] SP7 — generic parity orchestrator `run_parity_1m(feed, BacktestSource)` + boot wiring per enabled feed + generic `PARITY-01/02` error codes + rule file; delete the Groww/Dhan silos. Files: `crates/app/src/feed_parity_1m_boot.rs`, `crates/app/src/main.rs`, `crates/common/src/error_code.rs`, `.claude/rules/project/feed-parity-1m-error-codes.md`.

## Guarantee matrix
Carries the 15-row + 7-row matrix by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`. Proof per sub-PR: pure unit tests (comparator, fold cell golden, feed round-trip); audit via the unified DEDUP-keyed table; logging/alerting via `PARITY-01/02` + Telegram + CSV; performance via O(1) per-minute compare + the hot-path fold cell unchanged in complexity (DHAT on the Dhan path must stay green); honest envelope: OHLC-only where a feed lacks live volume, degrade-not-block on fetch failure, never a false "all match". Adversarial 3-agent before+after each hot-path-touching sub-PR (SP3 especially).

## Adversarial deep-research findings (2026-06-22, 3-agent panel) — MANDATORY guards

The hostile agent proved the convergence is NOT "two identical copies" — three per-feed
POLICIES were being treated as identical and are not. **Resolution: the engine CODE is
common; per-feed BEHAVIOUR is a small typed `FeedStrategy` parameter (data, not forked
code).** This is MORE common-runtime/scalable, not less — a future feed supplies a
`FeedStrategy` value, never new engine code. Every finding below is a build-gated guard.

| Sev | Finding | Mandatory resolution (per-feed strategy param + golden test) | Sub-PR |
|---|---|---|---|
| CRIT | Comparator direction differs: Dhan IGNORES live-only minutes (tape authoritative); Groww counts both `missing_in_backtest` + `missing_in_live`. Merging blindly = false alarms on every Dhan run. | comparator takes `count_live_only: bool` capability (Dhan=false, Groww=true). Golden test reproduces Dhan's "ignore live-only" rule. | SP2 |
| CRIT | Late-tick policy differs: Dhan RE-FOLDS a 1-bucket-late tick (`AmendedLate`); Groww DISCARDS. Forcing Groww onto re-fold CHANGES its candle + breaks the parity vs Groww backtest. | cell gains a per-feed `late_policy: {Refold, Discard}` (Dhan=Refold, Groww=Discard). Golden test asserts Groww candle UNCHANGED vs today; SP3's golden test must NOT bless a silent behaviour change. | SP3 |
| CRIT | Volume convention + WIDTH clash: Dhan = `u32` volume, caller-passed bucket baseline; Groww = `i64` cumulative (exceeds u32 intraday for liquid stocks), self-managed inter-minute baseline. Shared cell would truncate/overflow Groww volume + lose its baseline logic. | shared cell volume field widened to `i64`; baseline-carry is a REQUIRED typed arg (no default) with a per-feed strategy; Groww-cum-volume-through-shared-cell test is a SP3 BLOCKER (not "before switchover"). | SP3 |
| HIGH→**RESOLVED** | Key-type truncation: Dhan papaya key `(u32,u8)`; Groww token width unknown. | **MEASURED from the live Groww instrument master (`https://growwapi-assets.groww.in/instruments/instrument.csv`, 164,144 rows, 2026-06-22): MAX `exchange_token` = 1,175,236 ≪ u32::MAX (4,294,967,295). Groww tokens FIT u32 — no truncation, no I-P1-11 collision.** Keep the `(u32, ExchangeSegment)` shared key. Defense-in-depth guard test retained: a token > u32::MAX must reject loudly (never silent `as u32`), so a future Groww schema change can't regress silently. | SP3 |
| HIGH | Audit-table merge DEDUP overwrite: both old tables DEDUP on 5 cols WITHOUT `feed`; a NULL-feed Dhan row + a `feed='groww'` row collide → silent cross-feed overwrite until the re-key migration runs. The candle table got a NULL-backfill; the audit plan did not. | SP5 MUST mirror the candle migration: `UPDATE feed='dhan' WHERE feed IS NULL` BEFORE `DEDUP ENABLE UPSERT KEYS(...,feed)`. Migration test. | SP5 |
| HIGH | `compares_volume=false` false-OK: an all-zero (0.0) Groww candle "matches" a 0.0 backtest → reported clean while a real feed problem hides. | capability flag gates ONLY the volume field, NEVER the false-OK guard; ADD a positive-signal guard (non-zero price present) → all-zero "match" classified Blind/Degraded, not Pass. | SP2 |
| MED | Hardcoded 2-feed enumerations (`[Feed::Dhan, Feed::Groww]`, `vec![...as_str()]`, JS `FEEDS=[...]`, Dhan-special `can_disable_dhan`) — the SAME NTM 2→3-role panic class. feed#3 would be silently dropped. | add `Feed::ALL: &[Feed]` single source; build every list from it; keep all `match feed` exhaustive (no `_`) so feed#3 forces compile errors everywhere. | SP1 |
| MED | Groww backtest timezone unverified: Dhan REST is UTC→IST; Groww live is IST; Groww backtest (growwapi) tz is undocumented. Wrong tz → 330-min minute shift → 100% false mismatch. | SP6 pins Groww backtest tz with a parse test against a known candle; assert `minute_ts_ist_nanos == live floor` for a fixture. No assume-parity-with-Dhan. | SP6 |
| LOW | Baseline-source moves cell-internal→caller; a caller forgetting Groww's prior cumulative → `volume = full_cumulative` (PREVOI-01 "minus zero" class). | baseline = required typed arg, no default; non-zero baseline-carry test. | SP3 |

**Genuinely fine (evidence-checked):** `floor_to_minute_nanos` uses `rem_euclid` (correct for pre-epoch); `candles_1m` shared table + `feed` DEDUP + NULL-backfill is the SAFE template SP4 follows; both false-OK guards (`compared_minutes>0` / `BLIND`) exist and must be preserved, not regressed to one.

**Reframed design law:** common ENGINE + per-feed `FeedStrategy { late_policy, volume_width/baseline, count_live_only, compares_volume, backtest_tz }`. SP3 (cell unification) is the danger zone — it is gated behind these strategy params + golden tests; nothing merges until the golden tests prove each feed's candle is UNCHANGED.

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | Add future feed#3 | Write producer + BacktestSource + Feed variant + a `FeedStrategy` value; engine code untouched; `Feed::ALL` forces every list/match to include it (compile error if missed) |
| 2 | Dhan + Groww both run | Both flow the SAME engine; rows tagged `feed`; one audit table |
| 3 | Groww live (no volume) vs backtest | OHLC compared exact; volume not counted |
| 4 | Backtest fetch fails for a feed | PARITY-02 degrade; partial coverage; never false OK |
| 5 | SP3 cell switchover | Groww seals identical to old aggregator (golden test) before delete |
| 6 | Revert any sub-PR | Prior siloed module restored; shared table + data intact |
