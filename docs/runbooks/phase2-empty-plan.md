# Phase 2 Empty-Plan Runbook

> **Trigger:** Telegram alert `[HIGH] Phase 2 FAILED after N attempts` with
> body containing `Empty plan at trigger`.
> **Shipped:** 2026-04-22, commit `4aaa0fb`.

## What the alert means

At 09:13:00 IST the Phase 2 scheduler woke up to subscribe stock F&O
derivatives using the 09:12 close of each stock. The computed instrument
plan ended up **empty** (zero instruments to subscribe), so the scheduler
fired `Phase2Failed` with the diagnostic breakdown instead of the old
silent `Phase2Complete { added_count: 0 }`.

Stock F&O will remain unsubscribed for the rest of the day — strategies
that need those derivatives will see zero data.

## What the diagnostic fields tell you

The alert body looks like this:

```
Empty plan at trigger. Preopen buffer entries: X.
Skipped no-price: Y. Skipped no-expiry: Z.
Sample: [RELIANCE (no-price), TCS (no-price), ...].
```

Decision tree on the three numbers:

### Case 1: `Preopen buffer entries: 0`

The pre-open snapshotter received **zero** ticks during 09:08-09:12 IST.

**Causes (most → least likely):**

1. **Main-feed WebSocket not connected during the window.** Check commit
   `996b0cc` — off-hours disconnects downgraded to LOW severity but the
   underlying disconnect may still have delayed reconnection into the
   09:08-09:12 window. Query Prometheus:
   ```
   tv_websocket_connections_active{connection_type="live-feed"}
   ```
   Must be `5` during 09:00-09:15 IST.

2. **Snapshotter task panicked before 09:08.** Grep app log for
   `"Phase 2 pre-open snapshotter started"` — must appear at boot. If
   absent, the task never spawned.

3. **Tick broadcast channel closed early.** Check for
   `RecvError::Closed` in app log around 09:00-09:12. If present, the
   live feed shut down before pre-open.

**Fix:** redeploy, ensure main-feed is up before 09:00. If redeploy is
mid-day, the Phase 2 `RunImmediate` path fires with the current spot as
fallback (documented in `phase2_scheduler.rs`).

### Case 2: `Skipped no-price: N > 0, Skipped no-expiry: 0`

Stocks had NO tick in their 09:08-09:12 minute buckets. This is the
**most common** failure mode on slow pre-market days.

**Diagnostic query (Prometheus):**
```
tv_phase2_snapshotter_ticks_filtered_total{reason="wrong_minute"}
```

If this is high, tick LTT (exchange_timestamp) was outside the 09:08-09:12
window. Dhan may have sent the pre-open equilibrium tick with LTT=09:08:00
that arrived AFTER the window wall-clock. Pre-open auction behaviour
changes daily.

**Counter-measure:** none in code today. Tomorrow, if `skipped_no_price`
is consistently > 50% of F&O stocks, file a Dhan support ticket asking
why equilibrium ticks have lagging LTTs.

### Case 3: `Skipped no-expiry: N > 0`

Stocks had prices but no eligible F&O expiry (strictly > 2 trading days
remaining). This is **expected on Thursdays** — the current week's expiry
has ≤ 1 trading day left.

Phase 2 selects `select_stock_expiry(&expiry_calendar, today)` which
skips anything too close. If Thursday, Phase 2 SHOULD subscribe the
next-week expiry instead. Check:
```sql
SELECT underlying_symbol, expiry_date
FROM derivative_contracts
WHERE underlying_symbol = 'RELIANCE' AND status = 'active'
ORDER BY expiry_date ASC LIMIT 5;
```

Verify the universe has next-week expiries loaded. If not, the CSV
download missed them — rerun `/api/instruments/rebuild`.

### Case 4: All three non-zero

Mixed failure — some stocks have no price, others no expiry. Triage
both independently using cases 2 + 3.

## Recovery options

1. **Restart app.** The Phase 2 `RunImmediate` crash-recovery path fires
   after restart (scheduler `minutes_late: N` branch) using either:
   - Today's on-disk snapshot from PROMPT A (if present).
   - Current live LTP (fallback — no 09:12 close).

2. **Manual instrument subscribe via API.** (Not implemented yet — would
   require extending `/api/instruments/*` endpoints.)

3. **Wait for tomorrow.** If this is a one-day failure, Phase 2 will run
   normally tomorrow using tomorrow's 09:12 close.

## Related

- `.claude/rules/project/live-market-feed-subscription.md` — Phase 2 spec
- `.claude/rules/project/depth-subscription.md` — depth + pre-open buffer
- `crates/core/src/instrument/phase2_scheduler.rs` — source
- `crates/core/src/instrument/preopen_price_buffer.rs` — buffer source
