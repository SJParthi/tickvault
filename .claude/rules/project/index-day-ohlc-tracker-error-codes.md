# Index Day OHLC Tracker — Error Codes

> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Companion code:** `crates/trading/src/in_mem/day_ohlc_tracker.rs`.
> **Companion docs:**
> - `.claude/rules/project/live-market-feed-subscription.md` "IDX_I Special Case"
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this file to mention every `IndexOhlc*` variant.

---

## §0. Why this exists (the locked design)

For the 4 IDX_I SIDs (NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21):

| Constraint | Source |
|---|---|
| Ticker mode subscription LOCKED | operator-charter — small bandwidth, no REST polling |
| Ticker packet (16 bytes) carries only LTP + LTT | `dhan-ref/03-live-market-feed-websocket.md` rule 5 |
| No `day_open`, `day_high`, `day_low`, `day_volume` in Ticker | Same — those fields exist in Quote (50 bytes) + Full (162 bytes) |
| Volume NOT tracked | operator-locked 2026-05-18: Dhan historical has no volume for indices; BRUTEX doesn't use it |

**Therefore:** we EXPLICITLY track day OHLC ourselves from the Ticker LTP stream:

| Field | Source | When updated |
|---|---|---|
| `day_open` | First observed live tick LTP after midnight reset | ONCE on first `update_tick` per trading day (auto-arm) |
| `day_high` | `max(day_high, last_price)` on every tick | Every tick after auto-arm |
| `day_low` | `min(day_low, last_price)` on every tick | Every tick after auto-arm |
| `day_close` | `last_price` of most recent tick | Every tick after auto-arm |
| Volume | NOT tracked | Operator-locked out of scope |

**2026-05-26 update:** the Dhan pre-market buffer module was deleted per
operator directive (alongside the entire Dhan historical fetch chain).
The previous design that armed `day_open` from `PreOpenCloses::backtrack_latest()`
at the 09:15:00 IST boundary was replaced with auto-arm-on-first-tick:
`DayOhlcTracker::update_tick` initialises all four OHLC fields on its
first call after a daily reset.

**2026-07-03 update (session-open purity, operator directive 2026-07-03):**
the day-OHLC tick consumer (`crates/app/src/day_ohlc_orchestrator.rs::spawn_day_ohlc_tick_consumer`)
is now session-gated to `[09:15:00, 15:30:00)` IST via `day_ohlc_session_accepts`
(delegates to the canonical `g1_exchange_gate_accepts`) — pre-open indicative
ticks (Dhan streams from 09:00 IST) and post-close snapshots can no longer arm
`day_open` or advance high/low/close. "First observed live tick" above now
means the first IN-SESSION tick; the 09:15:00 tick IS the day open. Skips are
counted by `tv_day_ohlc_session_gate_skipped_total`.

---

## §1. INDEX-OHLC-02 — daily reset failed at IST midnight

**Severity:** High.
**Auto-triage:** Yes (transient — the CCL-02 supervisor respawns the reset task automatically, and the next day's first tick re-arms via auto-arm on `update_tick`).
**Trigger:** the daily reset task calls `DayOhlcTracker::reset_daily_all()` at IST midnight. The reset internally iterates the papaya HashMap and locks each `parking_lot::Mutex<DayOhlc>` to call `reset_daily()`. The task is now wrapped by the CCL-02 supervisor `spawn_supervised_midnight_reset_task` (mirror WS-GAP-05 / DISK-WATCHER-01): whenever the inner reset task's `JoinHandle` resolves — a panic mid-iteration (`reason="panic"`), an external cancel (`reason="cancelled"`), or an unexpected clean return (`reason="clean_exit"`) — the supervisor logs `error!(code = "INDEX-OHLC-02", reason, ...)`, increments `tv_day_ohlc_reset_failures_total{reason}`, backs off `DAY_OHLC_RESET_RESPAWN_BACKOFF_SECS` (5s), and respawns so the midnight reset keeps firing. Failure vectors:
- `parking_lot::Mutex` poisoned by a panic in a prior tick update (rare — `update_tick` has no panic paths; parking_lot does NOT poison, so this vector is near-unreachable — but the supervisor makes it non-silent if it ever fires)
- Tracker `Arc` handle dropped before reset task could acquire it
- Reset task panicked mid-iteration

**Consequence:** if the reset ever fails to fire, day high / day low / day close from the previous trading day carry over to the next trading day. The CCL-02 supervisor respawns the reset task within ~5s; the next live tick also re-arms all 4 fields via `update_tick`'s auto-arm path, so any carry-over only persists between IST midnight and the first live tick of the new session.

**Triage:**
1. Inspect `tv_day_ohlc_reset_failures_total` counter (produced by `spawn_supervised_midnight_reset_task`) — if > 0, the reset task died at least once; the `reason` label (`panic`/`cancelled`/`clean_exit`/`unknown`) says why. A sustained non-zero rate means the reset task is flapping — inspect `data/logs/errors.jsonl.*` for the panic backtrace immediately preceding the INDEX-OHLC-02 line.
2. The supervisor already respawned; no manual action is required for a one-off. For a flapping reset, restart the app to re-create the tracker with the default disarmed sentinel — first live tick re-arms cleanly.
3. If poisoned mutex is suspected, the next `update_tick` call inserts a NEW slot which is unaffected.

**Source:** `crates/trading/src/in_mem/day_ohlc_tracker.rs::DayOhlcTracker::reset_daily_all` + `crates/app/src/day_ohlc_orchestrator.rs::spawn_supervised_midnight_reset_task` (supervisor) / `spawn_midnight_reset_task` (inner task) / `classify_reset_task_exit` (pure exit classifier). Boot wiring: `crates/app/src/main.rs` (the `_reset_supervisor_handle` spawn). Wiring ratchet: `crates/core/src/auth/secret_manager.rs::test_day_ohlc_orchestrator_is_wired_into_main`.

---

## §2. Cross-reference

| Component | File |
|---|---|
| Tracker module | `crates/trading/src/in_mem/day_ohlc_tracker.rs` |
| Boot orchestrator | `crates/app/src/day_ohlc_orchestrator.rs` |
| Tick consumer + midnight reset spawn site | `crates/app/src/main.rs` |

---

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/trading/src/in_mem/day_ohlc_tracker.rs`
- `crates/app/src/day_ohlc_orchestrator.rs`
- Any file containing `IndexOhlc02`, `DayOhlcTracker`
