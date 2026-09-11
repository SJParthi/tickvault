---
paths:
  - "crates/core/src/websocket/**/*.rs"
  - "crates/core/src/parser/**/*.rs"
  - "crates/core/src/pipeline/tick_processor.rs"
  - "crates/storage/src/tick_persistence.rs"
  - "crates/storage/src/deep_depth_persistence.rs"
---

# WebSocket Enforcement Rules

> **Ground truth:** `docs/architecture/websocket-complete-reference.md`
> **Dhan refs:** `docs/dhan-ref/03-live-market-feed-websocket.md`, `docs/dhan-ref/04-full-market-depth-websocket.md`, `docs/dhan-ref/10-live-order-update-websocket.md`
> **Scope:** Any file touching WebSocket connections, binary parsing, subscription building, depth processing, or tick persistence.

## Mechanical Rules

1. **Read the reference first.** Before modifying ANY WebSocket-related code: `Read docs/architecture/websocket-complete-reference.md`. This is mandatory, not optional.

2. **Also read the relevant Dhan ground truth:**
   - Main feed changes → `Read docs/dhan-ref/03-live-market-feed-websocket.md`
   - Depth changes → `Read docs/dhan-ref/04-full-market-depth-websocket.md`
   - Order update changes → `Read docs/dhan-ref/10-live-order-update-websocket.md`
   - Enum/code changes → `Read docs/dhan-ref/08-annexure-enums.md`

3. **Four WebSocket types — NEVER confuse their protocols.**
   - Main feed: 8-byte header, byte 0 = response code, prices are f32
   - Depth (20 + 200): 12-byte header, bytes 0-1 = message length, prices are f64
   - Order update: JSON (not binary at all)
   - Getting header byte order wrong = garbage data for every field

4. **Depth has NO timestamp.** Only `received_at_nanos` (local clock). Never reference `exchange_timestamp` or `LTT` in depth code.

5. **200-level bytes 8-11 = row count, NOT sequence.** 20-level bytes 8-11 = sequence (ignore). Mixing these up = parsing garbage levels.

6. **Depth bid/ask arrive SEPARATELY.** Code 41 = Bid, 51 = Ask. Two packets per update. Never assume combined.

7. **200-level: 1 instrument per connection.** 20-level: up to 50. Main feed: up to 5,000.

8. **Connection pools are independent.** 5 main + 5 depth-20 + 5 depth-200 = 15 total available. Using main feed slots does NOT reduce depth slots.

9. **All binary reads are Little Endian.** `from_le_bytes()` always. Never big endian.

10. **SecurityId: u32 in binary, STRING in JSON.** Binary header bytes 4-7 = u32. JSON subscribe = `"1333"` (quoted string).

11. **Order update field names differ from REST.** Single-char codes (`C`=CNC, `B`=Buy, etc.), PascalCase keys, `MsgCode: 42`.

12. **Unsubscribe depth = code 25 — the vendor's own documented value, restored 2026-09-11.** This rule said "25, NOT 24 — the SDK has a bug", then flipped to 24 on 2026-09-10 when a live session proved Dhan IGNORES code 25 (measured 2026-09-11 by re-query: **80** `unsubscribe_ignored` lines, 40 depth-20 + 40 depth-200, across **all ten** depth sockets, redials exhausted at the session ceiling of 8 — the "20 ignored / 10 redials" figure recorded on 2026-09-10 matches no window and is withdrawn). **Code 24 was then ALSO ignored on 2026-09-11, identically** — 80 lines, 40/40, all ten sockets, ceiling reached. ~~Two codes producing a byte-identical failure signature is evidence the RequestCode is NOT the variable.~~ **⚠ WITHDRAWN 2026-09-11 (same day) — that argument is CIRCULAR.** 80 = `GHOST_REDIAL_SESSION_CEILING` (8) × 10 depth sockets, and 40/40 is 5+5 sockets × 8; the line only fires inside the `Ok(())` arm of `request_ghost_redial`, which refuses past the ceiling. **A metric that can only report one value is not evidence** — two saturated ceilings are identical whatever the vendor did. The conclusion may still be right; the stated evidence cannot support it, and a vendor ticket built on it invites "we cannot reproduce". The discriminating numbers are the UNBOUNDED ones (`ghost`, `unsubscribed_grace`). **✅ MEASURED for BOTH days 2026-09-11 (evening) — they are NOT alike, and the byte-identical claim is now refuted by data as well as by arithmetic.** The 09-10 split was still recoverable from the CloudWatch EMF group (the method reproduces five already-known 09-11 figures to the unit, so it is trusted): on comparable swap volume (7,780 vs 9,461 unsubscribes, 1.22x) code 25 produced **110,114** ghost packets against code 24's **5,345,436** — 48.5x — while the only two numbers that MATCHED were `ghost_redial` 80 and `ghost_exhausted` 10, the two that a ceiling forbids from differing. Traffic-independent `ghost/grace`: **1.996 (25) vs 3.170 (24)**, against **4.667** if never honoured — so **neither code is fully honoured, and neither is fully ignored either.** ⚠ CAUSE IS NOT ESTABLISHED: four other PRs merged between the two builds and depth traffic differed 3.50x for unexplained reasons, so this is "the sessions differed and 25 ghosted far less on every normalisation", never "25 is better because it is 25". Full table + the bounded under-count caveat: `websocket-connection-scope-lock.md` "✅ STEP 1 DONE 2026-09-11". **The cheap settling experiment, not yet run:** a depth-200 socket holds exactly ONE instrument, so unsubscribe it, do NOT re-subscribe, and watch that socket for frame silence — ~4 minutes, one socket, decisive either way. And `RequestCode 12` (Feed Disconnect), the ONLY stop mechanism the vendor's depth guide documents, has **zero production callers** and has never been tried.
    **✅ STEP 2, 2026-09-11 (evening) — the "maybe our own unsubscribe never reached the wire" alternative is now EXCLUDED by measurement.** Every arm of the unsubscribe chain was read for both sessions: all 24 `tv_dhan_ws_subscribe_failed_total{endpoint,reason}` series read **0** with 537+ samples each (the raw EMF records carry `endpoint` and `reason` even though the CloudWatch metric folds them to `host`, so the split is readable from `filter-log-events` but NOT from `get-metric-statistics`), and the coded `WS-GAP-02` `source="swap_wire_failed"` / `"swap_emptied_socket"` log lines — present on `origin/main` during both sessions — also read **0** in both log schemas. So across **7,780** (code 25) and **9,461** (code 24) unsubscribes, not one failed on our side. ⚠ Two caveats that matter: (a) `reason="unsubscribe_timeout"` is **VACUOUS** — the swap wraps `send_unsubscribe` in `SWAP_WIRE_BUDGET` (1 s) while the write inside is bounded by `SUBSCRIBE_SEND_TIMEOUT` (10 s), so the outer always wins and that reason can never increment; citing four zeros is citing three measurements and a tautology. (b) `send_unsubscribe` is fire-and-forget with no vendor ack, so this proves our side up to the socket boundary and no further. Also found and fixed free the same evening: the whole `tv_dhan_ws_swap_*` family (six counters) was neither EMF-selected nor seeded, so the arm that can manufacture a FALSE ghost had never reached CloudWatch at all. Full record: `websocket-connection-scope-lock.md` "✅ STEP 2 DONE 2026-09-11".

    **⚠ 2026-09-11 — the operator's fresh full pull of the Dhan v2 documentation settles which value to SHIP:** the Feed Request Code table reads `23 Subscribe — Full Market Depth` then `25 Unsubscribe — Full Market Depth`, **skipping 24**, and the literal 24 appears as a request code **nowhere in all 10,263 lines**. The Full Market Depth guide itself documents only `23` (subscribe) and `12` (disconnect the socket) and shows **no unsubscribe at all**. So 24 was an undocumented guess. Depth is the ONE family that breaks the `subscribe_code + 1` rule; that asymmetry is the vendor's and must not be "corrected". Do NOT ship a third guess — the next step is a vendor ticket citing their own annexure. Dated record: `websocket-connection-scope-lock.md` "2026-09-11 (SECOND) — THE VENDOR DOCUMENTATION SETTLES THE UNSUBSCRIBE CODE".

## Depth Rebalancing Rules (added 2026-04-16)

13. **Depth rebalancing uses command channel — NEVER disconnect+reconnect for ATM swap.**
    - `DepthCommand::Swap20` for 20-level, `DepthCommand::Swap200` for 200-level
    - Sends RequestCode 25 (unsub old — the vendor-documented value; 25 -> 24 -> 25, see rule 12) then 23 (sub new) on same WebSocket
    - Zero disconnect, zero reconnect, zero tick gap, O(1) latency

14. **Depth connections cap at 60 retry attempts.** No infinite retry loops.
    - After 60 consecutive failures (~30 min at 30 s max backoff), gives up
      via `ReconnectionExhausted` and fires CRITICAL Telegram.
    - Raised from 20 on 2026-04-21 after all 4 depth-200 connections
      exhausted the 20-attempt budget during market hours.
    - **Root cause FIXED 2026-04-23**: the `Protocol(ResetWithoutClosingHandshake)`
      storm was caused by our Rust client using `/twohundreddepth` as the
      200-depth URL path (per Dhan ticket #5519522 advice). Python SDK
      verification on the same account showed root path `/` is the
      actually-working URL. URL constant flipped to
      `wss://full-depth-api.dhan.co` (no path — URL builder emits
      `/?token=...`). Queue item I14 closed. See
      `.claude/rules/dhan/full-market-depth.md` rule 2 for the full
      reversal notes and test references.

15. **Depth ATM selection uses real index LTP, not median or arbitrary.** See `depth-subscription.md`.
    - Boot: waits up to 30s for first index LTP from main WebSocket
    - Runtime: rebalancer checks every 60s, swaps on ≥3 strike drift

16. **Tick persistence requires BOTH exchange_timestamp AND wall-clock within [09:00, 15:30) IST.**
    - `is_within_persist_window(exchange_timestamp)` — checks LTT
    - `is_wall_clock_within_persist_window(received_at_nanos)` — checks current time
    - Prevents post-market stale WebSocket snapshots from polluting `ticks` table

17. **Backfill worker is DISABLED.** Historical candle data must NOT be injected into the `ticks` table.
    - `ticks` table = live WebSocket data only
    - `historical_candles` table = REST API historical data only

## Gaps to Track

Any change to WebSocket code must check the open gaps in `docs/architecture/websocket-complete-reference.md` Section 10.2 and not introduce regressions.

Key open gaps (high severity — see ref §10.2 for design sketches):
- **O1-B** — Phase 2 `SubscribeCommand` channel on main-feed `WebSocketConnection` (scheduler + events shipped in O1-A, 2026-04-17; actual subscribe dispatch is the remaining piece)

Resolved gaps:
- Telegram now fires on first data frame (not just subscription) — fixed 2026-04-09
- Depth connections stay connected 24/7 (no market-hours sleep) — fixed 2026-04-09
- Depth metrics labeled per underlying (not shared gauge) — fixed 2026-04-09
- Telegram for depth rebalancer ATM changes — fixed 2026-04-16
- Depth connections infinite retry after market close — fixed 2026-04-16 (capped at 20)
- Depth disconnect+reconnect for ATM swap — fixed 2026-04-16 (command channel, zero disconnect)
- Depth strike selector picking wrong strikes — fixed 2026-04-16 (real ATM from index LTP)
- Backfill worker injecting synthetic ticks — fixed 2026-04-16 (disabled)
- Post-market stale ticks — fixed 2026-04-16 (wall-clock guard)
- FAST BOOT emitted zero `WebSocketConnected` alerts — fixed 2026-04-17 (shared helper + guard test)
- Pool watchdog `Degraded`/`Recovered` verdicts silently discarded — fixed 2026-04-17 (typed events wired)
- Depth index-LTP timeout warned silently — fixed 2026-04-17 (ERROR + `DepthIndexLtpTimeout` event)

## What This Prevents

- f32 parser on f64 depth prices (or vice versa)
- 8-byte header parser on 12-byte depth header
- 20-level header semantics on 200-level (sequence vs row count)
- Missing Little Endian reads
- Numeric SecurityId in JSON subscribe
- Binary parser on JSON order update
- REST field names on WS order data
- Depth code confused with main feed code

## Trigger

This rule activates when editing files matching the paths above, or any file containing:
`WebSocketConnection`, `WebSocketConnectionPool`, `DepthConnection`, `OrderUpdateConnection`, `parse_ticker_packet`, `parse_quote_packet`, `parse_full_packet`, `parse_oi_packet`, `parse_prev_close_packet`, `parse_disconnect_packet`, `parse_twenty_depth_packet`, `parse_two_hundred_depth_packet`, `parse_deep_depth_header`, `dispatch_deep_depth_frame`, `run_twenty_depth_connection`, `run_two_hundred_depth_connection`, `run_order_update_connection`, `build_subscription_messages`, `SubscribeRequest`, `InstrumentSubscription`, `api-feed.dhan.co`, `depth-api-feed.dhan.co`, `full-depth-api.dhan.co`, `api-order-update.dhan.co`, `DepthCommand`, `Swap200`, `Swap20`, `SharedSpotPrices`, `depth_cmd_senders`, `is_wall_clock_within_persist_window`, `select_depth_instruments`, `DEPTH_ATM_STRIKES_EACH_SIDE`
