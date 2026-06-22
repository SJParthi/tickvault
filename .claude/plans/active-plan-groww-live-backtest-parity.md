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

### Aggregator caveat (the one real risk — flagged, not hand-waved)
Dhan's aggregator is a 21-timeframe concurrent `papaya` engine (hot path); Groww's is a single-TF `std::HashMap`. We generify ONLY the inner per-instrument 1-minute fold cell (already pure, `Copy`, ≤96B) and have Groww reuse it; we do NOT merge the 21-TF concurrent container. Common key = `(i64, ExchangeSegment)`, common late-tick policy = re-fold (`AmendedLate`). Groww's cumulative-volume-delta convention is verified against the shared cell in a test before switchover, never assumed.

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
- [ ] SP1 — `common::feed::Feed` (move from api, re-export, kill scattered label consts). Files: `crates/common/src/feed.rs`, `crates/common/src/lib.rs`, `crates/api/src/feed_state.rs`, the 3 label-const sites. Tests: feed round-trip.
- [ ] SP2 — `common` generic `Candle1m` + exact comparator + `Mismatch`. Files: `crates/common/src/feed_parity.rs`. Tests: OHLC/volume-gated compare + false-OK guard.
- [ ] SP3 — generify the 1-minute fold cell; Groww reuses it (golden seal-parity test) ; delete `Groww1mAggregator`. Files: `crates/trading/src/candles/aggregator_cell.rs`, `crates/core/src/feed/groww/aggregator_1m.rs` (delete), `groww_bridge.rs`.
- [ ] SP4 — `GenericCandle1mWriter(feed)` merging the two writers. Files: `crates/storage/src/shadow_candle_writer.rs`, `crates/storage/src/groww_candle_persistence.rs` (merge).
- [ ] SP5 — ONE `feed_parity_1m_audit` table + writer (feed in DEDUP), merging the two audit modules. Files: `crates/storage/src/feed_parity_1m_audit_persistence.rs`, delete `groww_cross_verify_audit_persistence.rs` + `cross_verify_1m_audit_persistence.rs`.
- [ ] SP6 — `trait BacktestSource` + Dhan REST impl (moved from app) + Groww sidecar impl. Files: `crates/core/src/feed/backtest_source.rs`, `crates/core/src/feed/dhan/intraday.rs`, `crates/core/src/feed/groww/backtest.rs`, `scripts/groww-sidecar/groww_backtest_fetch.py`.
- [ ] SP7 — generic parity orchestrator `run_parity_1m(feed, BacktestSource)` + boot wiring per enabled feed + generic `PARITY-01/02` error codes + rule file; delete the Groww/Dhan silos. Files: `crates/app/src/feed_parity_1m_boot.rs`, `crates/app/src/main.rs`, `crates/common/src/error_code.rs`, `.claude/rules/project/feed-parity-1m-error-codes.md`.

## Guarantee matrix
Carries the 15-row + 7-row matrix by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`. Proof per sub-PR: pure unit tests (comparator, fold cell golden, feed round-trip); audit via the unified DEDUP-keyed table; logging/alerting via `PARITY-01/02` + Telegram + CSV; performance via O(1) per-minute compare + the hot-path fold cell unchanged in complexity (DHAT on the Dhan path must stay green); honest envelope: OHLC-only where a feed lacks live volume, degrade-not-block on fetch failure, never a false "all match". Adversarial 3-agent before+after each hot-path-touching sub-PR (SP3 especially).

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | Add future feed#3 | Write producer + BacktestSource + Feed variant; pipeline untouched |
| 2 | Dhan + Groww both run | Both flow the SAME engine; rows tagged `feed`; one audit table |
| 3 | Groww live (no volume) vs backtest | OHLC compared exact; volume not counted |
| 4 | Backtest fetch fails for a feed | PARITY-02 degrade; partial coverage; never false OK |
| 5 | SP3 cell switchover | Groww seals identical to old aggregator (golden test) before delete |
| 6 | Revert any sub-PR | Prior siloed module restored; shared table + data intact |
