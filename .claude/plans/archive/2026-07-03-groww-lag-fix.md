# Implementation Plan: Groww sidecar snapshot-walk coalescing + watermark-lag stall + received_at stamping

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — live-lag directive 2026-07-03 ("build the groww-lag fix PR from today's forensics")

> Forensics: scratchpad `groww-lag-forensics-2026-07-03.md` (2026-07-03,
> 10:36–10:50 IST session, post-#1343). VERIFIED root cause (Measured): every
> NATS callback in `scripts/groww-sidecar/groww_sidecar.py` walks the ENTIRE
> 768-entry `get_ltp()`/`get_index_value()` snapshot (~42 full walks/sec ≈
> 32,500 record decodes/sec of pure Python on one core), starving the SDK's
> NATS consumer, so the SDK snapshot — our ONLY data source — falls behind
> wall-clock UNBOUNDED (measured 8s → 428s over 13 min; exchange-time consumed
> at 0.53× real-time). The #1343 change-dedup removed the junk NDJSON rows but
> not the decode WORK. Secondary blind spot: micro watermark advances (+2 ms)
> reset the 30s/5s timestamp-advancing stall clocks, so a 7-minute drift never
> fires FEED-STALL-01. Secondary gap: `received_at` is NULL for `feed='groww'`
> rows, so per-feed pipeline lag is unmeasurable in SQL.

## Design

Three minimal, independent fixes. No change to the candle engine, the WAL
chain, the dedup, the DEDUP keys, or any Dhan path.

1. **Coalesce snapshot walks** (`scripts/groww-sidecar/groww_sidecar.py` —
   the cure): the NATS callbacks (`on_ltp` / `on_index`) no longer walk the
   snapshot; each does ONLY an O(1) `threading.Event().set()` on a per-kind
   dirty flag (`_LTP_DIRTY` / `_INDEX_DIRTY`) and returns, so the SDK's NATS
   consumer is never blocked by our decode work. A dedicated daemon walker
   thread (`_snapshot_walker_loop`, armed once per process alongside the
   existing watchdogs) drains each dirty snapshot at most once per
   `WALK_INTERVAL_MS` (default 200 ms, env `GROWW_WALK_INTERVAL_MS`, clamped
   to [20, 5000] by the pure `resolve_walk_interval_ms`). Clear-flag-THEN-walk
   ordering makes the dirty-flag race benign by design: an SDK update landing
   during/after the walk re-marks dirty and is drained on the next interval —
   never lost, at worst one redundant walk that the existing change-dedup
   collapses. Net effect: ≤ 2×5 walks/sec instead of ~42 (≈90% CPU freed for
   the NATS consumer), watermark lag bounded to ≈ walk interval + drain
   cadence. The walk bodies (`emit_ltp_records`/`emit_index_records`), the
   dedup, the stall detector, and the status-file writes are UNCHANGED.
2. **Watermark-lag stall criterion** (`scripts/groww-sidecar/groww_sidecar.py`):
   the timestamp-ADVANCING liveness (both the sidecar's 5s
   `should_force_reconnect` and the Rust 30s FEED-STALL-01) has a blind spot —
   a +2 ms micro-advance resets the clock while the watermark drifts minutes
   behind wall-clock. New pure decision `watermark_lag_stalled(now_ms,
   max_ts_millis, market_open, lag_threshold_ms)`: True only when the market
   is open, the watermark is known (> 0), and `now_ms − max_ts_millis` is
   STRICTLY greater than `WATERMARK_MAX_LAG_MS` (default 120_000, env
   `GROWW_WATERMARK_MAX_LAG_MS`). `_stall_recovery_loop` counts CONSECUTIVE
   stalled verdicts (1s poll); at `WATERMARK_LAG_CONSECUTIVE_CHECKS` (3) it
   fires the SAME restart path as the frozen-watermark criterion
   (`_force_close_nats_socket` → SDK `consume()` returns → reconnect +
   re-subscribe). A pure `watermark_lag_should_fire(consecutive, needed,
   cooldown_remaining_secs)` gates re-fires behind
   `WATERMARK_LAG_REFIRE_COOLDOWN_SECS` (180s) so a backlog that survives one
   reconnect refires at a bounded ~3-min cadence, never a 3s kill storm.
3. **Stamp `received_at` for groww rows** (`crates/storage/src/groww_persistence.rs`
   + `crates/app/src/groww_bridge.rs`): `GrowwLiveTickRow` gains
   `received_at_ist_nanos: Option<i64>`; `append_row` passes it through to the
   shared `RawTickFields` builder (which already OMITS the column on `None` —
   NULL-not-0 contract preserved). The bridge stamps the per-WAKE receipt
   clock (`wake_receipt_ist_nanos`, one clock read per wake — O(1) preserved),
   gated by `GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS` so a broken clock writes
   NULL, never a ~1970 timestamp. `received_at` stays OUT of the DEDUP key
   (data-integrity.md — stored column only), so replay idempotency is
   untouched.
4. **DEFERRED (follow-up, documented in the PR body):** watermark-aware
   per-minute catch-up seal — a naive wall-clock force-seal would
   mass-discard backlogged ticks under the GROWW Discard late-policy; it
   needs its own design keyed on the feed's OWN watermark, after fixes 1–2
   bound the backlog.

Guarantee matrices: this plan cross-references the canonical 15-row + 7-row
matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` (Per-Item
Guarantee Matrix); the resilience rows relevant here: no new tick-drop path
(coalescing drains the SAME latest-per-instrument SDK snapshot the callbacks
read — the snapshot never held intra-interval history, and capture-at-receipt
+ fsync per emitted record is unchanged), O(1) callbacks, bounded walker
cadence, WAL/ring/spill/DLQ untouched, `ticks` DEDUP keys untouched.

## Edge Cases

- Callback fires while the walker is mid-walk → flag re-set after clear →
  next interval re-walks (never lost; dedup collapses the overlap).
- Reconnect cycle swaps the `GrowwFeed` handle → walker reads the live handle
  from the shared `_CURRENT_FEED` cell every iteration; a walk against a
  freshly-connected pre-subscribe feed sees an empty tree (emits nothing).
- Snapshot dict mutates during iteration (SDK consumer writes while walker
  reads) → possible `RuntimeError: dictionary changed size during iteration`
  → caught by the walker's broad except; next interval retries; dedup makes
  the retry idempotent. Post-warmup the key set is stable (leaf value updates
  do not resize the exchange/segment/token dicts), so this is rare.
- `GROWW_WALK_INTERVAL_MS` garbage/absent/out-of-range → pure clamp to
  default/[20, 5000] — a 0/negative interval can never spin-loop the walker.
- Watermark unknown (0 — cold pre-first-record) → lag criterion never fires
  (the silent-feed diagnostic + Rust process-kill backstop own that case).
- Off-market huge lag (overnight: watermark = yesterday 15:29) → market-hours
  gate → never fires (mirrors every other stall layer).
- Lag exactly at threshold (120.000s) → strict `>` → not stalled; 119s no,
  121s yes (boundary-pinned tests).
- Reconnect completes and fresh records arrive → watermark jumps to ≈ now →
  verdict False → consecutive counter resets (healthy recovery re-arms).
- Backlog survives the reconnect (micro-advances persist) → refire only after
  the 180s cooldown — bounded cadence, no storm.
- Clock-read failure in the bridge (`receipt_ist_nanos` ≈ 1970) → gated by
  `GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS` → `received_at` NULL, never garbage.
- Bridge replay (byte-0 re-tail) re-appends rows with a FRESH `received_at` →
  DEDUP key excludes `received_at` → rows collapse idempotently (the newer
  stamp upserts in place — same semantics as Dhan, documented in
  data-integrity.md).

## Failure Modes

- Walker thread dies (unhandled BaseException) → per-iteration broad except
  makes this practically unreachable; the backstop is honest: no walks → no
  emits → watermark freezes → the EXISTING 5s frozen-watermark criterion
  force-closes, and the Rust FEED-STALL-01 process-kill is the outer backstop.
- Walker starves the status-file heartbeat? No: status writes are driven by
  `note_emit` inside the walk (1/s throttle unchanged) and the stall detector
  + reject poller run on their own threads. Worst added latency to the first
  `streaming` status = one walk interval (≤ 200 ms default).
- Lag criterion wrong-way (too eager) → worst case is a bounded
  force-close→reconnect (re-auth reuses the cached token; re-subscribe is
  idempotent) at most every 180s — the same self-heal path FEED-STALL-01
  already exercises.
- `received_at` plumbing wrong-way → the column is observability-only (not in
  any DEDUP key, not read by any hot path); worst case is a wrong lag reading
  in SQL, never data loss.

## Test Plan

- Python (`scripts/groww-sidecar/test_dedup.py`, stubbed `growwapi`, runnable
  via `python3 -m unittest test_dedup -v`):
  - `ResolveWalkIntervalTests`: default on absent/garbage, clamp low (0/−5 →
    20), clamp high (99999 → 5000), passthrough (200 → 200).
  - `WatermarkLagStalledTests`: 119s no, 121s yes, exactly-120s no (strict >),
    off-market never, unknown watermark (0/negative) never.
  - `WatermarkLagShouldFireTests`: below-consecutive no, at-consecutive yes,
    cooldown suppresses, cooldown-expired fires.
- Python `--selftest` harness: `_selftest_coalesce_and_watermark_lag` added
  alongside the existing three (deployed-environment runnable).
- Rust (`cargo test -p tickvault-storage --lib` + `-p tickvault-app --lib`):
  - `test_append_row_writes_received_at_when_stamped` (wire bytes carry
    `received_at=`),
  - `test_append_row_omits_received_at_when_none` (NULL-not-0 preserved),
  - `test_live_tick_row_maps_all_fields` updated for the new field,
  - `test_live_tick_row_received_at_gated_by_plausibility` (broken clock →
    None).
- Scoped per `testing-scope.md`: `cargo test -p tickvault-storage` +
  `-p tickvault-app` + fmt + scoped clippy. CI runs the full battery
  post-push. The Python unittest file is NOT wired into CI (no Python harness
  in CI today) — run locally + via `--selftest`; noted in the PR body.

## Rollback

Single revert of this PR's squash commit restores the pre-fix sidecar +
bridge + writer. No schema change (the `received_at` TIMESTAMP column already
exists in the shared `ticks` DDL — groww rows simply return to NULL), no
config change, no migration. The walker/dirty flags and the lag counters are
process-local state; the `ticks` DEDUP keys make any re-emission after
rollback idempotent.

## Observability

- Walker health: walk failures print capped stderr samples (first 5, then
  every 100th) — routed by the Rust supervisor to CloudWatch like every other
  sidecar line; a fully-dead walker surfaces as the existing frozen-watermark
  FEED STALLED diagnostic + FEED-STALL-01.
- Watermark-lag fires print a dedicated `FEED STALLED — watermark lag`
  stderr diagnostic carrying `lag_ms`, `max_ts_millis`, and the live
  emitted/deduped/dropped counters; the restart path increments the existing
  `tv_feed_sidecar_stall_restart_total{feed}` (FEED-STALL-01,
  `feed-stall-watchdog-error-codes.md`) — no new ErrorCode needed.
- `received_at` for `feed='groww'` rows makes per-feed pipeline lag
  measurable in SQL (forensics §7 query B stops returning NULLs):
  `SELECT feed, avg(datediff('s', ts, received_at)) FROM ticks … GROUP BY feed`.

## Plan Items

- [x] Item 1 — Coalesced snapshot walker: O(1) dirty-flag callbacks + interval walker thread + pure interval clamp
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: ResolveWalkIntervalTests (test_dedup.py), _selftest_coalesce_and_watermark_lag
- [x] Item 2 — Watermark-lag stall criterion wired into _stall_recovery_loop (same force-close restart path)
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: WatermarkLagStalledTests, WatermarkLagShouldFireTests (test_dedup.py)
- [x] Item 3 — Stamp received_at for groww rows (bridge → row → shared builder)
  - Files: crates/app/src/groww_bridge.rs, crates/storage/src/groww_persistence.rs
  - Tests: test_append_row_writes_received_at_when_stamped, test_append_row_omits_received_at_when_none, test_live_tick_row_received_at_gated_by_plausibility

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | ~42 NATS callbacks/sec across 768 SIDs | ≤5 walks/sec/kind; SDK consumer keeps up; watermark lag ≈ sub-second steady-state (live-verified after merge) |
| 2 | Watermark drifts 2+ min behind wall-clock via +2 ms micro-advances | lag criterion fires after 3 consecutive 1s checks → force-close → reconnect resets the backlog |
| 3 | Overnight idle (watermark = yesterday 15:29) | market-hours gate → no fire |
| 4 | Reconnect does not clear the backlog | refire bounded to one per 180s cooldown, never a kill storm |
| 5 | QuestDB query of per-feed lag | `received_at − ts` now non-NULL for feed='groww' |
| 6 | Broken wall clock in the bridge | received_at NULL (plausibility gate), never a 1970 stamp |
