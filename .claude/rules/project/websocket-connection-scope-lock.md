# WebSocket Connection Scope Lock — Operator Lock 2026-05-15

> **⚠ ALLOWED-INSTRUMENTS SUPERSEDED 2026-05-27 by [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md):** main-feed subscription expanded from 4 IDX_I SIDs (`LOCKED_UNIVERSE`) to ~250 daily-fetched SIDs (all NSE indices + 1 BSE SENSEX + unique F&O underlyings); all in Quote mode (was Ticker for IDX_I). `SubscriptionScope::Indices4Only` retires; replaced by `SubscriptionScope::DailyUniverse`. The 2-WebSocket lock itself (1 main-feed + 1 order-update) is UNCHANGED. Contents below retained as 2026-05-15 historical audit.
>
> **⚠ SECOND-FEED EXTENSION 2026-06-19 by [`groww-second-feed-scope-2026-06-19.md`](./groww-second-feed-scope-2026-06-19.md):** the 2-**Dhan**-WebSocket lock below is UNCHANGED. The operator authorized adding **GROWW** as an independent, **default-OFF** second market-data feed (feed #2) under a per-feed enable/disable contract. Groww is **native tickvault Rust** (brutex is reference only — no code pulled) reusing the same WAL/ring/spill/DLQ/aggregator chain; it adds NO Dhan connection and touches NO Dhan code. See that file for the verbatim authorization + full contract.
>
> **⚠ DHAN RUNTIME-TOGGLE AUTHORIZED 2026-06-21 (PR-E):** the **count + scope** of Dhan connections is UNCHANGED (still exactly 1 main-feed + 1 order-update, same endpoints, same locked universe). What changed: Dhan is no longer *config+restart only* — it is now **runtime enable/disable-able** from the feed-control webpage, exactly like Groww. Operator verbatim 2026-06-21: *"if I want to switch off or on dhan also it should be accepted right dude"* + (AskUserQuestion) **"Fully disconnect Dhan"** = OFF closes the Dhan WS(es) + stops storing; ON reconnects + re-subscribes (via the existing `SubscribeRxGuard` + dormant-reconnect machinery). Implementation: an `Arc<AtomicBool> dhan_enabled` flag (sourced from `FeedRuntimeState`) is read by the Dhan connection read/reconnect loop — OFF → close + dormant-idle polling the flag; ON → reconnect. **Safety guard (operator-approved 2026-06-21):** Dhan runtime-disable is allowed ONLY while no real orders are live (`dry_run = true` / no open orders+positions); once live trading is on, the toggle REFUSES to disable Dhan so the system can never be blinded mid-trade. This preserves the original "primary trading feed" safety intent that made Dhan config+restart-only. This authorization changes ONLY the lifecycle (start/stop), NOT the 2-connection lock, the endpoints, or the universe.
>
> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > defaults.
> **Scope:** PERMANENT. Every Phase. Every PR. Every future Claude/Cowork session.
> **Operator-locked:** 2026-05-15 (verbatim quote below).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## The verbatim operator demand (preserve exactly)

> "except [for] this 1 connection main feed websocket and order update websocket we will never ever use anything else"

And the immediate follow-up:

> "only these indices and equities will be subscribed and connected right dude?"

(Originally answered YES with 4 IDX_I + 218 NSE_EQ F&O underlying stocks = 222 SIDs. **AWS-lifecycle PR #6/#7 (2026-05-19) narrowed further:** the 218 NSE_EQ stocks dropped, leaving **only the 4 IDX_I SIDs** on the 1 main-feed connection. `SubscriptionScope` is now a single-variant enum — compile-time prevention of accidental expansion.)

---

## The rule (one line)

**This product opens exactly TWO WebSocket connections to Dhan, FOREVER: one main-feed + one order-update. No depth. No second main-feed. No new WS type. Period.**

---

## The complete allowed set

| WebSocket | Count | Endpoint | Allowed instruments | Mode |
|---|---|---|---|---|
| **Main feed** | **1** | `wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2` | **4 IDX_I SIDs ONLY**: NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21 (per `LOCKED_UNIVERSE` in `crates/common/src/locked_universe.rs`) | Ticker for IDX_I (16-byte packets) |
| **Order update** | **1** | `wss://api-order-update.dhan.co` | Receives order events for orders WE place; filter `Source=P` | JSON, MsgCode 42 auth |

**Total live WebSocket connections to Dhan ever: 2.**

**AWS-lifecycle PR #7 (2026-05-19) update:** the original lock allowed 4 IDX_I + 218 NSE_EQ F&O underlying stocks (= 222 SIDs). The 218 NSE_EQ stocks were dropped from the live universe in PRs #6a/#6b/#7a/#7b as the trading strategy narrowed to indices-only. `SubscriptionScope` is now a **single-variant enum** (`Indices4Only`); a new scope variant cannot be added without a rule-file edit + ratchet update.

---

## What this rule FORBIDS

| ❌ Forbidden FOREVER | Why |
|---|---|
| Depth-20 connections (`wss://depth-api-feed.dhan.co/twentydepth`) | Operator lock 2026-05-15; depth not in product scope for any phase. Modules deleted in PR #4 (#707). |
| Depth-200 connections (`wss://full-depth-api.dhan.co/?token=...`) | Same |
| Any 2nd/3rd/4th/5th main-feed conn | 4 SIDs fit comfortably on 1 conn (Dhan cap = 5,000/conn); more conns waste token+IP budget. `effective_main_feed_pool_size` returns constant 1. |
| Any new WebSocket endpoint Dhan introduces in future | Not in scope without operator explicit re-approval |
| NSE_EQ / BSE_EQ subscriptions (including F&O underlying cash equities) | AWS-lifecycle PR #7 dropped the 218 NSE_EQ universe; `SubscriptionScope::Indices4Only` has no path to emit them. |
| BSE F&O / commodity / currency feeds | Same |
| Stock F&O derivative subscriptions on the main-feed conn | Phase 2 dispatcher chain deleted in PR #5 (#708). Planner returns `false` from `should_subscribe_stock_derivatives` unconditionally. |
| Index F&O full-chain derivative subscriptions | Same — planner returns `false` from `should_subscribe_index_derivatives` unconditionally. |
| Display-only indices beyond INDIA VIX (sectoral, INDIA VOL, etc.) | `is_display_index_allowed_under_scope` returns `true` only for SID 21 (INDIA VIX). |

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

## Mechanical guards (post-PR #7b — verify each PR they remain)

| Guard | What it enforces |
|---|---|
| `SubscriptionScope` is a single-variant enum (`Indices4Only`) in `crates/common/src/config.rs` | Compile-time prevention — no new scope can be added without a rule-file edit |
| `effective_main_feed_pool_size(_, _) → PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1` constant in `crates/common/src/config.rs` | Main-feed pool always has exactly 1 conn |
| `should_subscribe_stock_derivatives(_) → false`, `should_subscribe_index_derivatives(_) → false`, `is_display_index_allowed_under_scope(_, sid)` returns true only for `INDIA_VIX_SECURITY_ID = 21` (`crates/core/src/instrument/subscription_planner.rs`) | Compile-time impossibility of derivative or sectoral subscriptions |
| Source-scan ratchet `crates/core/tests/indices4only_scope_lock_guard.rs` (3 tests) | Blocks reappearance of retired `SubscriptionScope` variants OR retired `subscribe_*` flags anywhere in `crates/` |
| Test: `test_subscription_scope_has_exactly_one_variant` (`config.rs`) | Match expression in the test must remain exhaustive over the single variant — adding a variant fails the build |
| Test: `test_effective_main_feed_pool_size_is_always_one_under_indices4only` | Pool size is constant 1, no exceptions |
| Test: `test_first_reconnect_attempt_is_zero_ms_instant` (order_update_connection.rs) | Order-update first retry is 0ms |
| Depth / Phase 2 / movers / greeks modules: deleted in PRs #2-#6b | No code path exists to spawn them; module-level deletion is the strongest guard |

---

## What a PR that violates this lock looks like (REJECT)

- Adds a new variant to the `SubscriptionScope` enum without a rule-file edit + dated operator quote.
- Re-introduces any of the 3 deleted `subscribe_*` config flags (`subscribe_index_derivatives`, `subscribe_stock_derivatives`, `subscribe_display_indices`).
- Adds a `spawn_twenty_depth_connection` or `spawn_two_hundred_depth_connection` call site (the modules are deleted; re-creating them would require restoring whole subtrees).
- Adds a new WebSocket type / endpoint without operator explicit re-approval recorded in this file or charter §I.
- Subscribes to derivative contracts on the main-feed conn (stock F&O, index F&O full chain).
- Subscribes to NSE_EQ / BSE_EQ / NSE_FNO / BSE_FNO / NSE_CURRENCY / MCX_COMM SIDs.
- Changes `effective_main_feed_pool_size(_, _)` to return anything other than `PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1`.

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

> "Sir, imagine your juice shop has only TWO phone lines. One phone is for customers to place juice orders (this is the order-update line). The other phone is for the daily price list from the fruit market — just 4 fruits: NIFTY, BANKNIFTY, SENSEX, INDIA VIX. That's it. We will NEVER install a third phone. No phone for vegetable prices. No phone for grocery prices. No phone for the 200 individual fruit stalls. No second phone for fruits. Two phones forever. If anyone says 'sir, let's add one more phone for spices,' you tell them: NO, this rule file AND the compiler both forbid it — the enum has only 1 slot, there is nowhere to add a 3rd phone. Two phones is enough. Two phones forever."

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
