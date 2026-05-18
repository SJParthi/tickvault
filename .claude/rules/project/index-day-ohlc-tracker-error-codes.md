# Index Day OHLC Tracker — Error Codes

> **Status:** PR #2.5 of AWS-lifecycle 14-PR sequence. Production wiring lands in PR #8 (option_chain module) when the aggregator integration completes.
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Companion code:** `crates/trading/src/in_mem/day_ohlc_tracker.rs`.
> **Companion docs:**
> - `docs/architecture/aws-indices-only-locked-architecture.md` §15 (pre-market + 09:15 open)
> - `docs/architecture/tick-to-multi-tf-aggregator.md`
> - `.claude/rules/project/live-market-feed-subscription.md` "IDX_I Special Case"
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this file to mention every `IndexOhlc*` variant.

---

## §0. Why this exists (the locked design)

For the 4 IDX_I SIDs (NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21):

| Constraint | Source |
|---|---|
| Ticker mode subscription LOCKED | operator-charter — small bandwidth, no REST polling |
| Dhan ignores Quote/Full for IDX_I anyway | `live-market-feed-subscription.md` "IDX_I Special Case" |
| Ticker packet (16 bytes) carries only LTP + LTT | `dhan-ref/03-live-market-feed-websocket.md` rule 5 |
| No `day_open`, `day_high`, `day_low`, `day_volume` in Ticker | Same — those fields exist in Quote (50 bytes) + Full (162 bytes) |
| Volume NOT tracked | operator-locked 2026-05-18: Dhan historical has no volume for indices; BRUTEX doesn't use it |
| 09:15:00 IST open price MUST = NSE equilibrium open | operator-locked 2026-05-18: NOT the first post-open tick LTP |

**Therefore:** we EXPLICITLY track day OHLC ourselves from the Ticker LTP stream:

| Field | Source | When updated |
|---|---|---|
| `day_open` | `preopen_price_buffer.backtrack_latest()` (the pre-market finalised close) | ONCE at 09:15:00 IST boundary via `arm()` |
| `day_high` | `max(day_high, last_price)` on every tick | Every tick after 09:15:00 IST |
| `day_low` | `min(day_low, last_price)` on every tick | Every tick after 09:15:00 IST |
| `day_close` | `last_price` of most recent tick | Every tick after 09:15:00 IST |
| Volume | NOT tracked | Operator-locked out of scope |

---

## §1. INDEX-OHLC-01 — pre-market buffer empty at 09:15:00 IST

**Severity:** Critical.
**Auto-triage:** No (operator paged via 4-channel SNS).
**Trigger:** at the 09:15:00 IST boundary the aggregator calls `preopen_price_buffer.backtrack_latest()` for an IDX_I SID and gets `None` — meaning no tick landed in any pre-market minute slot (09:00 to 09:12) for that SID. Possible causes:
- WebSocket disconnect during pre-market and the SID never got a tick into the 13-slot buffer
- The SID was not subscribed during pre-market (bootstrap race)
- Dhan upstream did not emit any pre-market tick for that SID
- The pre-market buffer's segment lookup map was not initialised at boot

**Fallback behavior:** the aggregator uses the FIRST post-open tick's `last_price` as `cell.open` for the 09:15 1m bar (degraded — this is the first TRADED price, not the NSE equilibrium open). The 15:31 IST cross-verify will CROSS-VERIFY-01 FAIL on the OPEN field for that SID.

**Triage:**
1. Inspect `tv_preopen_buffer_size_per_sid` Prometheus gauge — was it 0 for the entire pre-market window?
2. Inspect `tv_websocket_connections_active{kind="main"}` — was the connection alive at 09:00-09:15 IST?
3. If WS was alive but buffer empty, the SID may have been added to `PREOPEN_INDEX_UNDERLYINGS` after the boot cycle that subscribed — restart the app to re-subscribe.
4. If Dhan upstream is the cause (no pre-market activity), the day's open price for that SID will require manual override OR accept the first-trade fallback.

**Source:** `crates/trading/src/in_mem/day_ohlc_tracker.rs::DayOhlcTracker::arm_sid` callers.

---

## §2. INDEX-OHLC-02 — daily reset failed at IST midnight

**Severity:** High.
**Auto-triage:** Yes (transient — next day's first tick re-arms via `arm_sid`).
**Trigger:** the daily reset task in `reset_scheduler.rs` calls `DayOhlcTracker::reset_daily_all()` at IST midnight. The reset internally iterates the papaya HashMap and locks each `parking_lot::Mutex<DayOhlc>` to call `reset_daily()`. Failure modes:
- `parking_lot::Mutex` poisoned by a panic in a prior tick update (rare — `update_tick` has no panic paths)
- Tracker `Arc` handle dropped before reset task could acquire it
- Reset task panicked mid-iteration

**Consequence:** day high / day low / day close from previous trading day carry over to next trading day. The first tick at 09:15:00 IST re-arms via `arm_sid` which resets all 4 fields to the new pre-market close — so the carry-over only persists between IST midnight and 09:15:00 IST. If the strategy reads OHLC between midnight and 09:15:00 IST (rare), it gets stale data.

**Triage:**
1. Inspect `tv_day_ohlc_reset_failures_total` counter — if > 0, the reset failed at least once.
2. Restart the app to re-create the tracker with default disarmed sentinel — first 09:15:00 IST tick re-arms cleanly.
3. If poisoned mutex is suspected, the next `arm_sid` call inserts a NEW slot which is unaffected.

**Source:** `crates/trading/src/in_mem/day_ohlc_tracker.rs::DayOhlcTracker::reset_daily_all` + `reset_scheduler.rs` callers.

---

## §3. The mechanical guarantee (operator-charter §F)

> "Inside the LOCKED envelope of 4 IDX_I SIDs in Ticker mode: day OHLC (open, high, low, close — NO volume) is tracked explicitly by `DayOhlcTracker`. `day_open` comes from `preopen_price_buffer.backtrack_latest()` at 09:15:00 IST (= NSE equilibrium open). `day_high/low/close` derive from the LTP stream of Ticker packets. Volume is intentionally not tracked.
>
> 15:31 IST cross-verify against Dhan REST `/v2/charts/intraday` requires ZERO tolerance match on all 4 OHLC fields. Mismatches fire CROSS-VERIFY-01 (Critical Telegram + audit row). If `arm_sid()` fails because the pre-market buffer was empty, INDEX-OHLC-01 fires and the fallback first-trade open will cause cross-verify to fail on the open field — the operator knows immediately and can investigate."

---

## §4. Cross-reference

The 4 IDX_I subjects in `config/base.toml` `[cross_verify.subjects]` are the same 4 SIDs tracked by `DayOhlcTracker`. The pre-market buffer `PREOPEN_INDEX_UNDERLYINGS` was extended in this PR from 2 SIDs (NIFTY, BANKNIFTY) to 4 SIDs (adds SENSEX, INDIA VIX) so all subjects have a pre-market source for `day_open`.

| Component | File |
|---|---|
| Tracker module | `crates/trading/src/in_mem/day_ohlc_tracker.rs` |
| Pre-market buffer | `crates/core/src/instrument/preopen_price_buffer.rs` |
| 4-SID constant | `PREOPEN_INDEX_UNDERLYINGS` in same file |
| Cross-verify subjects | `config/base.toml` `[[cross_verify.subjects]]` |
| Cross-verify scheduler (lands PR #9) | `crates/core/src/cross_verify/` (not yet shipped) |

---

## §5. Trigger / auto-load

This rule activates when editing:
- `crates/trading/src/in_mem/day_ohlc_tracker.rs`
- `crates/core/src/instrument/preopen_price_buffer.rs`
- `crates/core/src/pipeline/candle_aggregator.rs` (when wiring at 09:15 boundary)
- Any file containing `IndexOhlc01`, `IndexOhlc02`, `DayOhlcTracker`, `PREOPEN_INDEX_UNDERLYINGS`
