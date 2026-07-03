# Implementation Plan: Close-Time Force-Seal (15:30:05 IST) — the daily lost 15:29 candle

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — dated authority 2026-07-03: "why the fuck this 3:29 full candle data is not yet generated" (grounded directive, this session)

> Guarantee matrices: cross-reference `per-wave-guarantee-matrix.md` (15-row 100% matrix + 7-row resilience matrix apply as-is; this item adds no tick-drop path, no hot-path allocation — the seal task is cold-path, once per trading day).

**Crates touched:** `crates/trading` (`crates/trading/src/candles/multi_tf_aggregator.rs`), `crates/app` (`crates/app/src/main.rs`, `crates/app/src/boot_helpers.rs`).

## Design

**Verified bug (2026-07-03):** the LAST 1-minute candle of every trading day (the 15:29 IST bucket) is lost daily. A 1m bucket seals only when (a) a later tick crosses the minute boundary in `consume_tick`, or (b) `force_seal_all` at IST midnight (`crates/app/src/main.rs` Task 3 in `spawn_engine_b_aggregator`). The session gate (`crates/trading/src/candles/multi_tf_aggregator.rs`, window [09:15, 15:30)) discards ≥15:30:00 ticks BEFORE they can roll the bucket, and the watermark catch-up seal (BOUNDARY-01) never clears the bucket end + margin because the pipeline stops at ~15:30:00.8 on the market-close signal. The midnight force-seal never rescues it in practice because the AWS box auto-stops at 16:30 IST and restarts destroy RAM state (Jul-3 00:00 midnight seal ran `sealed=0`).

**Fix:** a close-time force-seal task (Task 3b) inside `spawn_engine_b_aggregator`, mirroring the IST-midnight force-seal task (Task 3) pattern:

1. Loop: sleep until **15:30:05 IST** (`boot_helpers::compute_close_seal_sleep()` wrapping the pure `secs_until_close_seal_ist(now_secs_of_day)`; constant `CLOSE_SEAL_SECS_OF_DAY_IST = 55_805`).
2. Gate on `trading_calendar.is_trading_day_today()` — skip on weekends/NSE holidays (same gate as Task 3). Never fires mid-session: the trigger instant is fixed at 15:30:05, strictly after the session gate's [09:15, 15:30) window closes.
3. Call the NEW `MultiTfAggregator::force_seal_all_session_scoped` — identical to `force_seal_all` but **skips `always_on` instruments** (GIFT Nifty's ~21h session must NOT be prematurely sealed at NSE close; only the midnight task seals always-on cells). Route each seal through the SAME `seal_routing::route_seal` (Dhan policy: drop D1, pct-stamp) into `global_seal_sender()` → the seal-writer ring → QuestDB.
4. Do NOT reset the watermark (that stays the midnight task's cross-day duty).

**Ordering vs the 15:30:00.8 "market close → pipeline stop" (parallel timer chosen, NOT a close-sequence hook):** verified from `run_until_shutdown` (main.rs ~9007–9090): the close sequence only aborts the WS handles (after the 2s `MARKET_CLOSE_DRAIN_BUFFER_SECS`), waits `GRACEFUL_SHUTDOWN_TIMEOUT_SECS`, and aborts the trading handle — it does NOT tear down the aggregator `Arc`, the `global_seal_sender()` `OnceLock`, or the seal-writer loop; the app stays alive 24/7 until SIGTERM/16:30 instance stop. So a parallel 15:30:05 timer holding its own `Arc<MultiTfAggregator>` clone runs safely after the pipeline stop. The parallel timer is also the only choice that covers BOTH boot paths (fast boot at main.rs:2213 and `start_dhan_lane` at main.rs:5009 both call `spawn_engine_b_aggregator`), whereas hooking the close sequence would cover only `run_until_shutdown`.

**Idempotency vs the midnight seal (verified):** `AggregatorCell::force_seal` on an already-emptied slot returns `None` (pinned by `test_force_seal_after_catchup_is_idempotent` and `test_force_seal_returns_none_when_uninitialised`), so the midnight `force_seal_all` after a close-time seal double-flushes nothing; and even a duplicate row is absorbed by the candles tables' DEDUP UPSERT KEYS `(ts, security_id, segment[, feed])` — double-seal is safe.

## Edge Cases

- **15:29:59 vs 15:30:05 boundary:** `secs_until_close_seal_ist(55_799) == 6`; at exactly 55_805 the helper returns 86_400 (next day) so the task never busy-loops; 55_806 → 86_399. Unit-tested.
- **Non-trading day:** calendar gate skips (`is_trading_day_today()`), identical to the midnight task; the loop just re-sleeps to the next 15:30:05.
- **Always-on (GIFT Nifty) instruments:** skipped by `force_seal_all_session_scoped` — their ~21h session continues past NSE close; sealing them at 15:30:05 would truncate the open bucket and the DEDUP UPSERT of the later boundary re-seal would overwrite it with a bucket missing the pre-seal ticks. Unit-tested.
- **Straggler in-session tick after 15:30:05:** WS is aborted at ~15:30:02.8 (close + 2s drain), so in-flight <15:30-timestamped ticks normally fold before 15:30:05. A tick folding AFTER the seal hits the existing AGGREGATOR-LATE-01 no-amendable-bucket arm (counted discard) — same documented semantics as post-midnight-seal late ticks. Bounded, honest.
- **Boot after 15:30:05 (e.g. WAL replay of in-session ticks at 15:45):** the close-seal already elapsed for today, so replayed buckets wait for the IST-midnight force-seal — unchanged backstop behaviour; today's historical 15:29 candles remain rebuildable from the `ticks` table.
- **App restarted between 15:30:00 and 15:30:05:** RAM state lost either way (pre-existing envelope); the WAL replay + midnight backstop apply.

## Failure Modes

- **Seal-sender not installed** (`global_seal_sender()` returns `None`): log warn + `continue`, exactly like the midnight task — retried next day.
- **Seal ring full:** `route_seal` returns `DroppedFull` → counted (`tv_seal_mpsc_dropped_total`), row reaches the ring→spill→DLQ absorption chain — no new drop path.
- **Task panic:** same bare-spawn supervision level as the sibling Task 3 midnight force-seal and Task 4 catch-up driver (documented parity; a panic loses the close seal until restart, midnight/WAL backstops remain).
- **Clock skew:** bounded by BOOT-03 (±2s halt gate); a ≤2s skew keeps the trigger inside the post-close window (session gate closes at 15:30:00, WS aborted ~15:30:02.8 wall).

## Test Plan

- `crates/app/src/boot_helpers.rs::tests` — `test_secs_until_close_seal_at_152959_is_6s`, `test_secs_until_close_seal_at_exact_trigger_wraps_to_next_day`, `test_secs_until_close_seal_just_after_trigger`, `test_secs_until_close_seal_at_midnight`, `test_close_seal_constant_is_153005_ist`.
- `crates/app/src/boot_helpers.rs::tests::ratchet_main_rs_spawns_close_time_force_seal` — source-scan (include_str! main.rs) that the close-seal spawn exists: calls `compute_close_seal_sleep(`, `force_seal_all_session_scoped(`, and carries the `is_trading_day_today()` gate inside the close-seal task block.
- `crates/trading/src/candles/multi_tf_aggregator.rs::tests` — `test_force_seal_all_session_scoped_skips_always_on`, `test_force_seal_all_session_scoped_seals_regular_instruments`, `test_close_then_midnight_force_seal_is_idempotent` (second pass emits 0).
- Existing aggregator + app suites stay green: `cargo test -p tickvault-trading -p tickvault-app --lib`.

## Rollback

Single revert of the PR restores prior behaviour (15:29 bucket waits for the midnight force-seal). No schema change, no config change, no data migration — the seal path writes through the existing DEDUP UPSERT tables, so rows written by the new task are indistinguishable-safe. The trading-crate method is additive (`force_seal_all_session_scoped`); removing it removes its only call site with it.

## Observability

- `info!` line per firing: `close-time force-seal complete — final session buckets flushed` with `sealed`/`dropped` counts (mirrors the midnight task's line; greppable in `data/logs/app.*`).
- `info!` skip line on non-trading days.
- `warn!` when the seal sender is not installed (same as midnight task).
- Seals flow through the existing counters: `tv_aggregator_seals_emitted_total`, `tv_seal_mpsc_dropped_total` — visible on the existing CloudWatch panels; the 15:31 IST 1m cross-verify (CROSS-VERIFY-1M-01) independently confirms the 15:29 candle now exists.
- Doc sync: `.claude/rules/project/wave-6-error-codes.md` AGGREGATOR-LATE-01 stale "15:30 force_seal" wording updated to describe the real 15:30:05 close-time force-seal.

## Plan Items

- [x] Add `force_seal_all_session_scoped` to `MultiTfAggregator` (skips `always_on`)
  - Files: crates/trading/src/candles/multi_tf_aggregator.rs
  - Tests: test_force_seal_all_session_scoped_skips_always_on, test_force_seal_all_session_scoped_seals_regular_instruments, test_close_then_midnight_force_seal_is_idempotent
- [x] Add close-seal trigger-time helper + constant to boot_helpers
  - Files: crates/app/src/boot_helpers.rs
  - Tests: test_secs_until_close_seal_at_152959_is_6s, test_secs_until_close_seal_at_exact_trigger_wraps_to_next_day, test_secs_until_close_seal_just_after_trigger, test_secs_until_close_seal_at_midnight, test_close_seal_constant_is_153005_ist
- [x] Spawn Task 3b close-time force-seal in `spawn_engine_b_aggregator`
  - Files: crates/app/src/main.rs
  - Tests: ratchet_main_rs_spawns_close_time_force_seal
- [x] Fix stale wave-6-error-codes.md "15:30 force_seal" line
  - Files: .claude/rules/project/wave-6-error-codes.md
  - Tests: (docs-only)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Trading day, healthy run, 15:30:05 IST | 15:29 (and all TFs' final) buckets seal + flush to QuestDB before 15:31 cross-verify |
| 2 | Saturday / NSE holiday | Task logs skip, re-sleeps to next day, zero seals |
| 3 | GIFT Nifty (always-on) open bucket at 15:30:05 | NOT sealed — continues folding; midnight task still seals it |
| 4 | Midnight force-seal after close-time seal | 0 double-flushes (force_seal on emptied cells returns None); any duplicate absorbed by DEDUP |
| 5 | Seal sender not yet installed | warn + retry next day; no panic |
