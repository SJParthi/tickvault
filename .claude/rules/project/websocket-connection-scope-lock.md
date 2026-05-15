# WebSocket Connection Scope Lock — Operator Lock 2026-05-15

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > defaults.
> **Scope:** PERMANENT. Every Phase. Every PR. Every future Claude/Cowork session.
> **Operator-locked:** 2026-05-15 (verbatim quote below).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## The verbatim operator demand (preserve exactly)

> "except [for] this 1 connection main feed websocket and order update websocket we will never ever use anything else"

And the immediate follow-up:

> "only these indices and equities will be subscribed and connected right dude?"

(Answered YES — 4 IDX_I + 218 NSE_EQ F&O underlying stocks = 222 SIDs on the 1 main-feed connection.)

---

## The rule (one line)

**This product opens exactly TWO WebSocket connections to Dhan, FOREVER: one main-feed + one order-update. No depth. No second main-feed. No new WS type. Period.**

---

## The complete allowed set

| WebSocket | Count | Endpoint | Allowed instruments | Mode |
|---|---|---|---|---|
| **Main feed** | **1** | `wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2` | **4 IDX_I** (NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21) + **218 NSE_EQ** F&O underlying stocks = **222 SIDs total** | Ticker for IDX_I (16-byte packets) + Quote for NSE_EQ (50-byte packets) |
| **Order update** | **1** | `wss://api-order-update.dhan.co` | Receives order events for orders WE place; filter `Source=P` | JSON, MsgCode 42 auth |

**Total live WebSocket connections to Dhan ever: 2.**

---

## What this rule FORBIDS

| ❌ Forbidden FOREVER | Why |
|---|---|
| Depth-20 connections (`wss://depth-api-feed.dhan.co/twentydepth`) | Operator lock 2026-05-15; depth not in product scope for any phase |
| Depth-200 connections (`wss://full-depth-api.dhan.co/?token=...`) | Same |
| Any 2nd/3rd/4th/5th main-feed conn | 222 SIDs fit comfortably on 1 conn (Dhan cap = 5,000/conn); more conns waste token+IP budget |
| Any new WebSocket endpoint Dhan introduces in future | Not in scope without operator explicit re-approval |
| BSE F&O / commodity / currency feeds | Same |
| Stock F&O derivative subscriptions on the main-feed conn | Phase 0 lock — `should_spawn_phase2_scheduler` returns false for `IndicesUnderlyingsOnly` scope |
| Index F&O full-chain derivative subscriptions | Same |
| Display-only indices beyond INDIA VIX (sectoral, INDIA VOL, etc.) | Phase 0 filter — only VIX kept |

---

## Reconnect parity (both allowed WS types)

The 2026-05-13 disconnect storm (09:16-09:29 IST) showed that 500ms first-reconnect was pure unnecessary downtime. After Phase 0 Item 4 fix (2026-05-15):

| WS type | First retry | Subsequent retries |
|---|---|---|
| **Main feed** | **0 ms instant** via `compute_reconnect_base_delay_ms(0, _, _) → 0` (`crates/core/src/websocket/connection.rs:1666`) | `initial * 2^(attempt-1)` capped at `reconnect_max_delay_ms` |
| **Order update** | **0 ms instant** via `compute_reconnect_backoff_ms(1) → 0` (`crates/core/src/websocket/order_update_connection.rs:639`, Phase 0 Item 4 fix 2026-05-15) | `initial * 2^(failures-2)` capped at `ORDER_UPDATE_RECONNECT_MAX_DELAY_MS` |

**Subscription/state preservation across reconnects:**

- Main feed: `SubscribeRxGuard` (`connection.rs:145`) — Drop reinstalls `Receiver` so post-reconnect subscribe commands reach the new socket.
- Order update: stateless protocol — auth message (MsgCode 42) is re-sent on every connect.

---

## Mechanical guards (existing — verify each PR they remain)

| Guard | What it enforces |
|---|---|
| `phase2_recovery::should_spawn_depth_dynamic_pipeline(IndicesUnderlyingsOnly, _) → false` (`crates/app/src/phase2_recovery.rs:197`) | Depth-20 + depth-200 pipelines NEVER spawn under Phase 0 scope |
| `phase2_recovery::should_spawn_greeks_pipeline(IndicesUnderlyingsOnly, _) → false` (`phase2_recovery.rs:218`) | Greeks pipeline never spawns |
| `phase2_recovery::should_spawn_phase2_scheduler(IndicesUnderlyingsOnly, _) → false` (`phase2_recovery.rs:174`) | Stock F&O Phase 2 dispatcher never spawns |
| `effective_main_feed_pool_size(IndicesUnderlyingsOnly, _) → 1` (`crates/common/src/config.rs:1192`) | Main-feed pool always has exactly 1 conn under Phase 0 scope |
| Test: `test_should_spawn_*_skipped_under_phase_0_regardless_of_flag` | All three spawn helpers return false regardless of TOML flag value |
| Test: `test_effective_main_feed_pool_size_under_phase_0_returns_one` | Pool size is clamped to 1, no exceptions |
| Test: `test_first_reconnect_attempt_is_zero_ms_instant` (order_update_connection.rs) | Order-update first retry is 0ms (Phase 0 Item 4 fix, this commit) |

---

## What a PR that violates this lock looks like (REJECT)

- Adds a `spawn_twenty_depth_connection` or `spawn_two_hundred_depth_connection` call site in `main.rs`.
- Flips `should_spawn_depth_dynamic_pipeline` to return `true` for `IndicesUnderlyingsOnly`.
- Adds a new WebSocket type / endpoint without operator explicit re-approval recorded in this file or charter §I.
- Subscribes to derivative contracts on the main-feed conn (stock F&O, index F&O full chain).
- Subscribes to BSE_EQ / NSE_FNO / BSE_FNO / NSE_CURRENCY / MCX_COMM SIDs.
- Increases `effective_main_feed_pool_size(IndicesUnderlyingsOnly, _)` to anything > 1.

**Any such PR MUST be rejected in review even if operator explicitly approves verbally** — the operator must update this rule file FIRST, with a dated quote, and only then can the PR land. This prevents accidental scope creep through casual approvals.

---

## Operator re-approval protocol (if scope ever expands)

To add a new WS type or instrument class in a future phase:

1. Operator provides explicit verbatim quote authorizing the expansion.
2. Edit this rule file: add the new WS type to the "complete allowed set" table.
3. Edit `operator-charter-forever.md` §I to reflect the new scope.
4. Add ratchet test(s) pinning the new scope.
5. Open the actual scope-expansion PR citing the rule-file edit commit as its authority.

**No "I think the operator probably meant…" expansions.** This rule file is the single source of truth.

---

## Auto-driver / Insta-reel explanation

> "Sir, imagine your juice shop has only TWO phone lines. One phone is for customers to place juice orders (this is the order-update line). The other phone is for the daily price list from the fruit market — 4 fruit categories and 218 fruit types = 222 prices. That's it. We will NEVER install a third phone. No phone for vegetable prices. No phone for grocery prices. No second phone for fruits. Two phones forever. If anyone says 'sir, let's add one more phone for spices,' you tell them: NO, this rule file forbids it. Two phones is enough. Two phones forever."

---

## Trigger (auto-loaded paths)

Always loaded. Activates on any session that:
- Edits `crates/app/src/main.rs` (boot sequence)
- Edits any file under `crates/core/src/websocket/`
- Edits `crates/common/src/config.rs` `SubscriptionScope` or related enums
- Edits `crates/app/src/phase2_recovery.rs`
- Edits `config/base.toml` `[subscription]` or `[websocket]` sections
- Adds any new `wss://` URL constant
- Calls any `spawn_*_connection` or `spawn_*_pipeline` function
