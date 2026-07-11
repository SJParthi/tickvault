# Implementation Plan: Issue #858 remaining gap — IDX_I prev-day close backstop (code-6 cache → tick stream)

**Status:** APPROVED
**Date:** 2026-07-11
**Approved by:** Parthiban (operator) — issue #858 (2026-05-28 WS-sourced prev-close directive) + operator directive relayed via coordinator session 2026-07-11 ("resolve the open issues; implement #858 per the issue")

## Design

Issue #858 asked to restore the `*_pct_from_prev_day` candle columns using the
live Quote/Full packet `close` field as the prev-day source. Most of it shipped
in PRs #860/#861: `close_pct_from_prev_day` is stamped end-to-end —
`ParsedTick.day_close` (Quote bytes 38-41 / Full bytes 50-53) →
`LiveCandleState.prev_day_close` (`crates/trading/src/candles/aggregator_cell.rs`,
last-non-zero-wins) → `route_seal` override (`crates/app/src/seal_routing.rs`) →
`stamp_seal_pct_fields` → ILP write, ratcheted by `candle_pct_column_guard.rs` +
`close_pct_realtime_proof_guard.rs`.

**The one remaining gap: IDX_I indices.** Per Dhan Ticket #5525125
(`live-market-feed.md` rule 10), indices receive prev-day close ONLY via the
standalone code-6 PrevClose packet — the Quote-mode `close` field for IDX_I is
UNVERIFIED-LIVE and observed `0.0`. The tick processor in **crates/core**
(`crates/core/src/pipeline/tick_processor.rs`) already maintains an
`index_prev_close_cache` from code-6 packets (local `HashMap<u64, f32>` owned by
the tick-processor loop, mirrored to a file cache for mid-day restart survival)
but NEVER routes it back into the tick stream — so index candles' `close_pct`
depended entirely on the unverified Quote field and read 0.

**The fix (smallest correct diff, option "a"):** a pure `#[inline(always)]`
helper `idx_prev_close_backstop(exchange_segment_code, day_close, &cache,
security_id) -> Option<f32>` in `tick_processor.rs`, called in the
`ParsedFrame::Tick(mut tick)` arm right after the day_close baseline counter and
before the ingestion gates / `consume_feed_tick`. When the tick is IDX_I AND its
packet-provided `day_close` is not a usable positive finite value AND the
code-6 cache has a positive finite entry for the SID, the cached value is
stamped into `tick.day_close`. Downstream then works unchanged: aggregator_cell
captures `prev_day_close` from `tick.day_close` (last-non-zero-wins), route_seal
stamps `close_pct_from_prev_day`.

**Cache read structure decision:** NO new structure, NO lock, NO papaya mirror.
The code-6 insert arm and the Tick arm are two arms of the SAME `match` inside
the SAME single-task loop, and `index_prev_close_cache` is a loop-local
`HashMap` — the read is one O(1) `HashMap::get` with zero allocation and zero
contention (the cheapest correct design consistent with `hot-path.md`). The
two cheap gates (segment compare + finite-positive check) run BEFORE the hash,
so non-IDX_I ticks pay 1 u8 compare. The security_id-only key is I-P1-11-safe
because both the insert arm and the read gate are segment-gated to IDX_I.

## Edge Cases

- **Quote-provided value wins:** `day_close > 0.0 && is_finite` → backstop
  returns `None`; if Dhan ever populates Quote close for IDX_I, that value is
  never overridden (fill-only semantics).
- **Cache miss** (code-6 not yet received this session, no file-cache row) →
  `None`; the tick flows unchanged with `day_close = 0.0` (pre-fix behaviour).
- **Corrupted file cache** (NaN/Inf/≤0 loaded from a damaged
  `index-prev-close` JSON) → filtered, never stamped.
- **Unusable NON-zero packet value** (NaN / negative day_close from a mangled
  frame) → treated as a gap and backstopped — a strict superset of the `== 0.0`
  case, strictly safer.
- **Non-IDX_I segment with a numerically colliding SID** (I-P1-11 class, e.g.
  NSE_FNO SID 13) → `None`; the segment gate prevents cross-segment stamping.
- **Full-packet (TickWithDepth) arm NOT touched:** IDX_I subscribes Quote mode
  only (daily-universe lock §8); Full packets for indices cannot occur under
  the 2-WS lock, so the stamp lives only in the Quote/Ticker `Tick` arm.
- **day_close_baseline_count semantics preserved:** the stamp sits AFTER the
  baseline counter, so the PROOF counter keeps meaning "packet-provided
  day_close values only".
- **Persisted `ticks.close` side effect (deliberate):** stamped IDX_I ticks
  persist the code-6 prev-close in their `close` column instead of 0 — a live
  WS-sourced value into a live tick (no synthesized ticks; `live-feed-purity.md`
  untouched), making the stored row MORE truthful per Ticket #5525125.

## Failure Modes

- **Code-6 packet never arrives AND no file cache** (Dhan-side omission on a
  fresh boot): backstop is inert; index `close_pct_from_prev_day` reads 0 —
  identical to pre-fix behaviour, never worse. The existing
  `tv_prev_close_updates` counter + per-segment PrevClose summary log remain
  the diagnostic surface.
- **Stale file cache across days:** the file cache is rewritten on every code-6
  arrival and the code-6 burst re-fires at each session's subscription; a
  mid-day restart reads today's values back. A stale value would only survive
  if TODAY's code-6 burst never arrived (see above — and then a slightly stale
  prev-close is stamped, matching the pre-existing documented file-cache
  design; no new failure mode is introduced by this PR).
- **Hot-path regression risk:** bounded to 1 u8 compare per non-IDX tick; the
  pure fn allocates nothing (comparisons + `HashMap::get`). No `.clone()`, no
  `format!`, no `Vec::new()`, no lock.
- **Ratchet interaction:** the code-6 arm (`test_prev_close_arm_has_no_sync_fs_calls`
  markers) is untouched — the diff adds no code inside that arm.

## Test Plan

Unit tests (pure fn, `crates/core/src/pipeline/tick_processor.rs::tests`):
- `test_idx_prev_close_backstop_stamps_zero_day_close_on_cache_hit`
- `test_idx_prev_close_backstop_never_overrides_nonzero_day_close`
- `test_idx_prev_close_backstop_cache_miss_returns_none`
- `test_idx_prev_close_backstop_non_idx_segment_returns_none` (I-P1-11)
- `test_idx_prev_close_backstop_rejects_junk_cached_values`
- `test_idx_prev_close_backstop_fills_non_finite_packet_value`

Suites: `cargo test -p tickvault-core --lib` (block-scoped per
`testing-scope.md`), tick_processor integration tests
(`prev_close_routing_5525125_guard`, `parser_pipeline`,
`websocket_protocol_e2e`), DHAT zero-alloc suite (`dhat_allocation`,
`dhat_prev_close_writer`, `dhat_feed_consumer`). Gates: `cargo fmt --check`,
`cargo clippy -p tickvault-core --features daily_universe_fetcher -- -D warnings
-W clippy::perf`, banned-pattern scanner (pre-commit).

Live proof surfaces (CI-blind by design, per issue #858's own note): post-boot
`SELECT close_pct_from_prev_day FROM candles_1m WHERE security_id = 13` non-zero
check + the `tv_aggregator_close_pct_nonzero_total` counter — pending the next
live QuestDB boot.

## Rollback

Single-file, additive change: `git revert <commit>` restores the exact pre-fix
behaviour (index ticks flow with `day_close = 0.0`; candles' close_pct reads 0).
No schema change, no config change, no new dependency, no state migration — the
file cache and code-6 arm are byte-identical to main.

## Observability

- Existing counters unchanged and still meaningful:
  `tv_prev_close_updates_total` (code-6 arrivals), the per-segment PrevClose
  summary `info!`, `day_close_baseline_count` PROOF log (packet-provided only).
- Downstream proof: `tv_aggregator_close_pct_nonzero_total` (the
  `close_pct_realtime_proof_guard` surface) now expected non-zero for IDX_I
  SIDs after the first code-6 packet; QuestDB
  `candles_1m.close_pct_from_prev_day` for SID 13/25/51/21 is the live check.
- No new metric added: the backstop is a deterministic pure fill whose effect
  is fully visible through the existing close_pct proof chain; adding a
  per-stamp counter would be a hot-path cost with no operator decision
  attached.

## Plan Items

- [x] Pure backstop fn + call site in the Tick arm
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: test_idx_prev_close_backstop_stamps_zero_day_close_on_cache_hit, test_idx_prev_close_backstop_never_overrides_nonzero_day_close, test_idx_prev_close_backstop_cache_miss_returns_none, test_idx_prev_close_backstop_non_idx_segment_returns_none, test_idx_prev_close_backstop_rejects_junk_cached_values, test_idx_prev_close_backstop_fills_non_finite_packet_value

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | IDX_I Quote tick, day_close=0, code-6 cached 23146.45 | tick.day_close stamped 23146.45 → candle close_pct non-zero |
| 2 | IDX_I Quote tick, day_close=23200 (Dhan populated) | untouched — packet value wins |
| 3 | IDX_I tick, cache miss | untouched (pre-fix behaviour) |
| 4 | NSE_FNO tick with colliding SID 13 | untouched (segment gate, I-P1-11) |
| 5 | Corrupted file cache (NaN/0/-1) | never stamped |
| 6 | Mid-day restart | file cache reloads at boot → backstop works before the next code-6 burst |

## Per-Item Guarantee Matrix

The 15-row "100% everything" matrix + the 7-row resilience demand matrix apply
per `.claude/rules/project/per-wave-guarantee-matrix.md` (cross-referenced in
full; compact block per the hook's accepted form):

- 15-row matrix: coverage (6 new unit tests, coverage delta ≥ 0), audit
  (N/A — no new event; existing candle chain audited), testing (unit +
  integration + DHAT categories named above), code checks (all pre-commit/pre-
  push gates), performance (zero-alloc pure fn, DHAT suite green, no bench
  budget touched), monitoring/logging/alerting (existing prev-close +
  close_pct proof chain, no new failure mode → no new alert), security
  (no input surface change; security-reviewer N/A — 40-line pure-fn diff),
  bugs/scenarios/functionality (6 scenarios table above; pure fn has tests +
  call site), review (adversarial review at PR stage), extreme check (existing
  ratchets `candle_pct_column_guard.rs` + `close_pct_realtime_proof_guard.rs` +
  `test_prev_close_arm_has_no_sync_fs_calls` all stay green).
- 7-row resilience matrix: zero ticks lost (no new drop path — fill-only field
  stamp), WS untouched (no SubscribeRxGuard/watchdog change), never
  slow/locked (O(1), zero-alloc, no lock), QuestDB self-heal untouched, O(1)
  latency (1 u8 compare per non-IDX tick; 1 HashMap get per gap-IDX tick),
  uniqueness+dedup (segment-gated read preserves I-P1-11; DEDUP keys
  untouched), real-time proof (`tv_aggregator_close_pct_nonzero_total` +
  QuestDB close_pct SELECT — live-boot verification pending, stated honestly).

Honest 100% claim: 100% inside the tested envelope, with ratcheted regression
coverage — the backstop fills IDX_I `day_close` only from a positive finite
code-6-sourced value, never overrides a packet value, and is inert on cache
miss; the live Quote-cadence behaviour of Dhan for IDX_I remains
UNVERIFIED-LIVE and the first live boot is the proof (CI-blind by design).
