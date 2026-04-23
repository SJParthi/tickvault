# Depth Rebalance Telegram Runbook

> **Trigger:** Telegram alert `[HIGH] Depth rebalance: <UNDERLYING>`.
> **Shipped (label fix):** 2026-04-22, commit `6f6edc5`.

## What the alert means

The depth rebalancer detected that the live NIFTY / BANKNIFTY / FINNIFTY
/ MIDCPNIFTY spot price drifted ≥ 3 strikes from the currently-subscribed
depth-20 (and depth-200 for NIFTY + BANKNIFTY) ATM. The rebalancer fired
a `DepthCommand::Swap20` (and `Swap200` if applicable) command via the
mpsc channel — the existing WebSocket sends RequestCode 25 (unsub old)
then 23 (sub new) on the SAME open socket.

**Zero disconnect. Zero reconnect. Zero tick gap.**

## Reading the message

```
[HIGH] Depth rebalance: NIFTY
Spot: 26850.75 → 26724.00
Old CE: NIFTY 28 APR 26850 CALL (SID 77022)
Old PE: NIFTY 28 APR 26850 PUT (SID 77023)
New CE: NIFTY 28 APR 26700 CALL (SID 70054)
New PE: NIFTY 28 APR 26700 PUT (SID 70055)
Action: zero-disconnect swap — 20-level + 200-level unsub old / sub new on same socket
```

- **Spot**: old spot → new spot that triggered the drift.
- **Old CE/PE**: contracts previously subscribed.
- **New CE/PE**: contracts now subscribed.
- **Action**: always "zero-disconnect swap". Before commit `6f6edc5` this
  text was "aborting old 200-level → spawning new ATM" which falsely
  implied a socket teardown — it was a label bug, not a behaviour bug.

## Underlying-specific messages

- **NIFTY / BANKNIFTY**: both 20-level AND 200-level swap. Message says
  `"zero-disconnect swap — 20-level + 200-level unsub old / sub new on same socket"`.
- **FINNIFTY / MIDCPNIFTY**: 20-level only (no 200-level connection).
  Message says `"zero-disconnect swap — 20-level unsub old / sub new on
  same socket (no 200-level for this underlying)"`.

## When to escalate

**Never** on a single rebalance — this is the designed behaviour.

Escalate if:

1. Same underlying rebalances ≥ 10 times in 10 minutes — suggests spot
   oscillating around a strike boundary. Investigate whether the 3-strike
   drift threshold needs widening. See
   `DEPTH_REBALANCE_STRIKE_THRESHOLD` in `depth_strike_selector.rs`.

2. Telegram contains the word `"aborting"` — regression of commit
   `6f6edc5`. The `session_2026_04_22_regression_guard.rs` test should
   have caught this at CI; if it slipped, file a bug.

3. FINNIFTY or MIDCPNIFTY Telegram mentions "200-level" — regression of
   the same commit. These underlyings have no 200-level connection.

## Related

- `.claude/rules/project/depth-subscription.md` — depth rules
- `crates/core/src/instrument/depth_rebalancer.rs` — source
- `crates/app/src/main.rs:3193-3330` — Telegram message construction
- `crates/core/tests/session_2026_04_22_regression_guard.rs` — ratchet
