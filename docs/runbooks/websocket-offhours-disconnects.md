# Off-Hours WebSocket Disconnect Runbook

> **Trigger:** Telegram alert `[LOW] WebSocket N/5 disconnected [off-hours,
> auto-reconnecting]`.
> **Shipped:** 2026-04-22, commit `996b0cc`.

## What the alert means

Dhan's market-feed / depth / order-update servers sometimes TCP-RST idle
connections outside market hours (09:00-15:30 IST). Our code detects this
via the read loop's `Err(..)` branch and fires the `LOW` variant
`WebSocketDisconnectedOffHours`. The reconnect loop immediately retries
with exponential backoff and almost always succeeds within 4 seconds.

**No operator action required.** This runbook exists purely so the LOW
Telegram doesn't look scary.

## When to escalate

Escalate ONLY if:

1. The alert fires DURING market hours (09:00-15:30 IST). In-market
   disconnects route to `WebSocketDisconnected` (HIGH) — if you see LOW
   during market hours, there's a classification bug.

2. Consecutive-failure count exceeds 20 (reconnect loop gives up and
   fires `ReconnectionExhausted` CRITICAL).

3. All 5 connections disconnect simultaneously and don't reconnect
   within 30 seconds — likely Dhan-wide outage, check
   `https://status.dhan.co/` (if exists) or Dhan support channels.

## Known false-positive pattern

Pre-market 08:00-09:00 IST: Dhan routinely resets 1-3 WebSockets on idle
cleanup. All 5 reconnect within 4 seconds. This produced Telegram spam
before commit `996b0cc` (was HIGH severity). Now LOW — audit trail
preserved, operator not paged.

## Order-update WS specifically

Order-update WebSocket has a DIFFERENT pattern: it stayed silent for
30+ minutes on idle dry-run accounts because Dhan's order-update server
doesn't ping on the same 10s cadence as market-feed. This caused the
1800s watchdog to fire every 30 minutes (9:10, 9:40, 10:10, 10:40 IST
on 2026-04-22).

**Fix (commit 55452c2):** watchdog bumped to 14400s (4 hours). Silence
for < 4h is now considered normal. TCP-RST on truly dead sockets is
still caught by the read loop's `Err(..)` branch.

## Related

- `.claude/rules/project/websocket-enforcement.md` — WebSocket rules
- `crates/core/src/websocket/activity_watchdog.rs` — watchdog constants
- `crates/core/src/websocket/connection.rs:456-510` — severity split
- `crates/core/src/notification/events.rs` — `WebSocketDisconnectedOffHours` variant
