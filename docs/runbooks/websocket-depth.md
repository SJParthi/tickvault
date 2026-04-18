# Runbook — WebSocket / Depth failures

**When it fires:** `WS-GAP-01`, `WS-GAP-02`, `WS-GAP-03`,
`WebSocketDisconnected`, `HighWebSocketReconnectRate`,
`Depth20Disconnected`, `Depth200Disconnected`, `NoDepthFrames`,
`DeepDepthDataLoss`, `DepthFramesDropped`, `DepthRebalancerDown`,
`Depth200ResetLoop`, `DATA-804/805/806`.

**Consequence if not resolved:** no live market data → strategy
signals stall → orders not placed → missed opportunities (or worse,
stale prices used for decisions).

## Three WebSocket pools

| Pool | URL | Max conns | Packet | Codes we expect |
|---|---|---|---|---|
| Main feed | `wss://api-feed.dhan.co` | 5 | 8-byte header, f32 prices | Ticker/Quote/Full 1–8, 50 |
| Depth 20-level | `wss://depth-api-feed.dhan.co/twentydepth` | 5 | 12-byte header, f64 prices | 41 (Bid), 51 (Ask) |
| Depth 200-level | `wss://full-depth-api.dhan.co/twohundreddepth` | 5 | 12-byte header, 1 instrument/conn | 41, 51 |

Pools are **independent** — 5 main + 5 depth-20 + 5 depth-200 = 15
usable slots total. The 200-level pool only has 2 active connections
(NIFTY CE + BANKNIFTY CE ATM) by design.

## Telegram alert → action (60-second version)

| Alert | First action |
|---|---|
| 🟠 `WebSocketDisconnected` | Check if reconnect succeeded in the next 30s. Auto-recovery usually handles. |
| 🟠 `HighWebSocketReconnectRate` | Check Dhan status page. Rate > 0.5 reconnects/min is abnormal. |
| 🔴 `Depth20Disconnected` / `Depth200Disconnected` (during market hours) | Check `tv-depth-flow` dashboard. Try `scripts/auto-fix-restart-depth.sh`. |
| 🟡 `NoDepthFrames` | Depth feed silent. Likely 200-level pool on wrong path (`/` vs `/twohundreddepth`) — the banned-pattern scanner prevents this in code. |
| 🔴 `DeepDepthDataLoss` | Dropped depth frames. Check backpressure → QuestDB / channel lag. |
| 🟠 `DepthRebalancerDown` | Rebalancer task crashed. Restart app. |
| 🔴 `Depth200ResetLoop` | 200-level server resetting our TCP. **ATM strike too far OTM** — rebalancer should fix within 60s. If persistent, see below. |
| 🔴 `DATA-805` | Too many connections (>5 per pool). App auto-stops all, waits 60s. Operator: verify no duplicate processes running. |
| 🔴 `DATA-804` | Too many instruments per connection (>5000 main, >50 depth-20, >1 depth-200). Investigate subscription builder. |
| 🟠 `DATA-806` | Data API not subscribed. Check Dhan dashboard "Data plan" status. |

## Root-cause checklist

### 1. Is Dhan itself up?

```bash
curl -sS -m 5 https://api.dhan.co/v2/ > /dev/null && echo "Dhan REST OK"
# Check status.dhan.co in browser for scheduled maintenance
```

If Dhan is down: there's nothing we can do locally. Wait.

### 2. Are connections actually established?

```bash
# Main feed
curl -sS http://localhost:9091/metrics | grep tv_websocket_connections_active
# Expected during market hours: 5

# Depth pools
curl -sS http://localhost:9091/metrics | grep -E "tv_depth_20_connections_active|tv_depth_200_connections_active"
# Expected: 4 and 2 respectively
```

### 3. Disconnect code classification

WS disconnects carry a code (see `.claude/rules/dhan/annexure-enums.md`
rule 11 for the full table):

| Code | Meaning | Auto-handled? |
|---|---|---|
| 800 | Internal server error | Reconnectable |
| 804 | Too many instruments | Non-reconnectable — operator must reduce |
| 805 | Too many connections | STOP ALL 60s, then retry |
| 806 | Data plan not subscribed | Non-reconnectable — fix Dhan account |
| 807 | Access token expired | Triggers token refresh, then reconnect |
| 808–810 | Auth codes | Non-reconnectable — see auth.md runbook |
| 811–814 | Request invalid | Non-reconnectable — bug in our subscribe JSON |

```bash
mcp__tickvault-logs__tail_errors --limit 20 --code WS-GAP-01
mcp__tickvault-logs__tail_errors --limit 20 --code DATA-805
```

### 4. Depth-specific checks

**Strike selection sanity:**

```bash
# The ATM strike must be close to spot. Far OTM triggers Depth200ResetLoop.
curl -sS http://localhost:9091/metrics | grep tv_depth_atm_strike
# Compare to the underlying's spot LTP from the main feed
curl -sS http://localhost:9091/metrics | grep tv_index_ltp
```

**Rebalancer health:**

```bash
curl -sS http://localhost:9091/metrics | grep tv_depth_rebalancer_ticks_total
# Should increment ~once per 60s during market hours
```

### 5. Backpressure

```bash
curl -sS http://localhost:9091/metrics | grep -E "tv_deep_depth_dropped_total|tv_depth_spilled_total"
# Both should be 0 during normal operation.
# If rising: the WAL / ILP writer is slow → see zero-tick-loss.md runbook.
```

## Recovery

### Main feed stuck at 0-3 connections during market hours

```bash
# The boot sequence builds the main feed connection pool. If it's
# degraded, the usual fix is a restart — reconnect logic is per-pool.
make stop && make run

# Verify:
curl -sS http://localhost:9091/metrics | grep tv_websocket_connections_active
# Expected: 5 within ~30s of boot
```

### Depth 200-level in a reset loop

Most common cause: ATM strike drifted too far OTM and Dhan's server
resets the TCP connection. The rebalancer should fix within 60s.

```bash
# Force an immediate rebalance (if app exposes an endpoint)
curl -X POST http://localhost:3001/api/depth/rebalance

# If not exposed:
scripts/auto-fix-restart-depth.sh
```

If it persists after rebalance: the ATM selector logic might be
using stale spot prices. Check `tv_index_ltp` age:

```bash
# How old is the last NIFTY tick?
now=$(date +%s); ltp_epoch=$(curl -sS http://localhost:9091/metrics | awk '/tv_ws_canary_last_tick_epoch_secs/{print $2}')
echo "NIFTY last tick age: $(( now - ${ltp_epoch%.*} ))s"
# > 60s = stale spot → rebalancer is using wrong ATM → reset loop persists
```

### DATA-805 "Too many connections"

The app's own logic is to STOP ALL connections for 60s then retry.
If the alert persists:

1. Check for duplicate processes — `ps aux | grep tickvault` should show one.
2. Check for stale browser tabs on the Dhan webapp (each counts).
3. If both clear, the issue is Dhan-side — contact support.

## Never do these

- **Never recycle-upgrade to the 200-level root path `/`** — banned
  by pattern scanner. Must be `/twohundreddepth`. Dhan actively
  resets TCP on root path.
- **Never set 200-level `InstrumentCount` > 1** per connection.
  Dhan's docs are clear: 200-level is 1 instrument per socket.
- **Never use main-feed parser on depth packets.** 8-byte vs 12-byte
  header will produce garbled f32 bid/ask prices and corrupt candles.
- **Never unsubscribe with RequestCode 24.** The correct code is 25;
  Dhan's own SDK has this bug. Our code uses 25 correctly.

## Preventive measures

1. **Daily Dhan status check** (morning) — `https://status.dhan.co`
2. **Weekly connection cap audit** — run `make docker-status` and
   ensure no zombie containers.
3. **Monthly rebalancer rehearsal** — force a manual rebalance and
   verify ATM drift detection works end-to-end.

## Related files

- `.claude/rules/dhan/live-market-feed.md` — main feed protocol
- `.claude/rules/dhan/full-market-depth.md` — depth protocol (Ticket #5519522 path pin)
- `.claude/rules/project/depth-subscription.md` — ATM selection rules
- `.claude/rules/project/websocket-enforcement.md` — banned patterns
- `crates/core/src/websocket/*.rs` — connection + parser code
- `crates/core/src/instrument/depth_rebalancer.rs` — rebalance loop
- `scripts/auto-fix-restart-depth.sh` — restart helper
- `deploy/docker/grafana/dashboards/depth-flow.json` — live dashboard
