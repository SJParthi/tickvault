# Dhan API v2.5 Coverage Map — What Our Locked Design Uses + What's Deferred

> **Status:** REFERENCE (no code shipped).
> **Authority:** CLAUDE.md > operator-charter-forever.md > Dhan API v2.5 docs > this file.
> **Created:** 2026-05-18 in response to operator pasting full Dhan API docs ("what about these docs dude?").
> **Companion:** `option-chain-z-plus-heart-piece.md`, `THE-FINAL-PLAN.md`.

---

## §0. Auto-driver one-liner

> "Sir, Dhan offers ~50 APIs. For the locked indices-only scope (4 SIDs, no live trading yet), we USE 6 today. When you flip the switch to live trading later, we add 8 more. The rest (forever orders, super orders, EDIS, statements, partner login) we may never need. This map shows which is which."

---

## §1. The 4 categories — how each Dhan API maps to us

| Category | Status today |
|---|---|
| 🟢 USED in locked scope | We hit these APIs daily from day 1 |
| 🟡 USED only when live trading enabled (Phase 1.5+) | Designed but wiring deferred |
| 🟠 OPTIONAL future use | Available if strategy evolves |
| 🔴 NOT needed | Out of scope for retail indices-only |

---

## §2. AUTHENTICATION (must work day 1)

| Dhan API | Status | Our usage | Where in our design |
|---|---|---|---|
| `POST /app/generateAccessToken` (TOTP-based, individual trader) | 🟢 USED | Daily at boot Step 3 | `aws-indices-only-locked-architecture.md` §16 |
| `GET /v2/RenewToken` (extend 24h to another 24h) | 🟢 USED | Token manager 23h pre-emptive renewal | Existing `auth/token_manager.rs` |
| API key + secret + 3-step OAuth (12-month validity, v2.4) | 🟠 OPTIONAL | Alternative to TOTP flow — defer unless TOTP-secret-rotation becomes painful | — |
| Partner login (3-step) | 🔴 NOT needed | We are NOT a partner platform | — |
| `POST /v2/ip/setIP` (Static IP whitelist) | 🟢 USED | Bootstrap one-time during `aws-bootstrap.sh` (operator registers EIP with Dhan) | operator-end-to-end-automation.md §2 stage 6 |
| `PUT /v2/ip/modifyIP` (7-day cooldown after change) | 🟡 USED rarely | Only if EIP changes (extremely rare) | runbook only |
| `GET /v2/ip/getIP` (pre-market check) | 🟢 USED | Boot Step 5.5 — verify our IP is whitelisted with `ordersAllowed=true` | Existing `network/ip_verifier.rs` |
| `GET /v2/profile` (User Profile API) | 🟢 USED | Boot validation: dataPlan=Active, activeSegment includes Derivative, tokenValidity >4h | Existing |

**Authentication is FULLY covered for locked scope.** TOTP automated forever (per §16).

---

## §3. DATA APIs (live market feed + option chain + historical)

| Dhan API | Status | Our usage |
|---|---|---|
| `wss://api-feed.dhan.co` (Live Market Feed WebSocket, Ticker/Quote/Full modes) | 🟢 USED | The 1 main-feed WS, Ticker mode for 4 IDX_I SIDs |
| `POST /v2/optionchain` (1 req per 3s per unique underlying) | 🟢 USED | Heart-piece: every 50s for NIFTY+BANKNIFTY+SENSEX concurrent |
| `POST /v2/optionchain/expirylist` | 🟢 USED | Boot + 15:31 IST daily refresh to detect Thursday expiry rollover |
| `POST /v2/charts/intraday` (interval 1/5/15/25/60) | 🟢 USED | 15:31 IST cross-verify against our derived candles (1m, 5m, 15m only) |
| `POST /v2/charts/historical` (daily, supports `oi` parameter) | 🟢 USED | 08:05 IST morning 1d cross-check; also boot rehydration of 30-day RAM history |
| `POST /v2/marketfeed/ltp` (LTP only) | 🟠 OPTIONAL | NOT needed at 4-SID scope (WS gives us LTP). Was used in pre-open backfill for 218 stocks — now obsolete since stocks deleted |
| `POST /v2/marketfeed/ohlc` | 🔴 NOT needed | WS Quote/Full mode provides OHLC |
| `POST /v2/marketfeed/quote` (full quote + 5-level depth) | 🔴 NOT needed | WS Full mode covers it |
| `wss://depth-api-feed.dhan.co/twentydepth` (20-level depth) | 🔴 NOT needed | DROPPED per LOCK-I (depth disabled entirely) |
| `wss://full-depth-api.dhan.co/?token=...` (200-level depth) | 🔴 NOT needed | DROPPED per LOCK-I |
| Historical Expired Options Data (v2.3) | 🟠 OPTIONAL | Useful for BRUTEX backtesting later; deferred |

**Data APIs FULLY covered.** Option chain + historical + WS = the 3 critical paths.

---

## §4. ORDER APIs (when live trading enabled — Phase 1.5 wiring)

| Dhan API | Status | Our usage |
|---|---|---|
| `POST /v2/orders` (Place Order) | 🟡 USED Phase 1.5+ | Currently `OmsEngine::place_order` has ZERO production call sites — Phase 1.5 wiring blocker per `gap-inventory-2026-05-18.md` §1 |
| `PUT /v2/orders/{order-id}` (Modify) | 🟡 USED Phase 1.5+ | Same |
| `DELETE /v2/orders/{order-id}` (Cancel) | 🟡 USED Phase 1.5+ | Same |
| `POST /v2/orders/slicing` (auto-split for F&O freeze qty) | 🟠 OPTIONAL | Not needed at low option qty; defer |
| `GET /v2/orders` (Order book) | 🟡 USED Phase 1.5+ | Position reconciliation |
| `GET /v2/orders/{order-id}` (Single order) | 🟡 USED Phase 1.5+ | Per-order status fetch |
| `GET /v2/orders/external/{correlation-id}` | 🟡 USED Phase 1.5+ | Idempotency check |
| `GET /v2/trades` (Trade book) | 🟡 USED Phase 1.5+ | EOD P&L computation |
| `wss://api-order-update.dhan.co` (Live Order Update WS) | 🟢 USED | The 2nd of our 2 WS connections; auth via MsgCode 42 |
| `POST /v2/super/orders` (Super Order — entry + SL + target) | 🟠 OPTIONAL Phase 1.5+ | Recommended for SL+target combo; defer until strategy demands |
| `PUT /v2/super/orders/{order-id}` (Modify Super) | 🟠 OPTIONAL | Same |
| `POST /v2/forever/orders` (Forever / GTT) | 🔴 NOT needed | We don't park orders overnight |
| Postback URL (HTTP webhook) | 🟠 OPTIONAL | Order update WS is our primary — postback is backup |
| `POST /v2/alerts/orders` (Conditional Trigger Orders, v2.5) | 🟠 OPTIONAL | Useful for "place order when RSI > X" — defer until strategy design |

**Order APIs are designed but NOT WIRED today** (this is the Phase 1.5 blocker per gap inventory §1).

---

## §5. RISK & PORTFOLIO APIs

| Dhan API | Status | Our usage |
|---|---|---|
| `GET /v2/holdings` (Demat holdings) | 🔴 NOT needed | We do options trading, not equity holdings |
| `GET /v2/positions` (Open positions) | 🟡 USED Phase 1.5+ | Live position tracking — gap #11 in gap inventory |
| `POST /v2/positions/convert` (Intraday ↔ CNC) | 🔴 NOT needed | We don't convert intraday→delivery |
| `DELETE /v2/positions` (Exit All — v2.5) | 🟡 USED Phase 1.5+ | EOD square-off + emergency square-off |
| `POST /v2/margincalculator` (single order margin) | 🟡 USED Phase 1.5+ | Pre-trade margin check per risk engine — gap #3 in gap inventory |
| `POST /v2/margincalculator/multi` | 🟠 OPTIONAL | Useful when placing multi-leg combinations |
| `GET /v2/fundlimit` (funds available) | 🟡 USED Phase 1.5+ | Pre-trade balance check |
| `POST /v2/killswitch` (kill switch activate/deactivate) | 🟡 USED Phase 1.5+ | Emergency halt — gap #3 in gap inventory |
| `GET /v2/killswitch` (status) | 🟡 USED Phase 1.5+ | Boot validation; periodic state check |
| `POST /v2/pnlExit` (P&L based auto-exit, v2.5) | 🟠 OPTIONAL | Server-side automatic position exit at threshold — alternative to client-side risk engine |

**All risk + portfolio APIs are USED when live trading enabled.** Today they're library-only, not wired.

---

## §6. NOT-NEEDED APIs (out of scope for retail indices-only)

| Dhan API | Why not needed |
|---|---|
| `POST /v2/super/orders` for stock options | We don't trade stock options anymore |
| `POST /v2/forever/orders` (GTT) | Retail-trader strategy doesn't need overnight GTT |
| `GET /v2/edis/tpin` + `POST /v2/edis/form` (EDIS for selling holdings) | We don't hold demat stocks |
| `GET /v2/ledger` (ledger statements) | Operator gets these from Dhan web/email if needed |
| `GET /v2/trades/{from}/{to}/{page}` (trade history beyond today) | Today's trade_book is enough for P&L |
| Partner authentication flow (3-step) | We are not a partner platform |
| BSE F&O subscriptions | Out of indices-only scope |
| Currency F&O / Commodity F&O | Out of scope |
| Conditional Trigger Orders for non-options | We only trade index options |
| Margin Conversion APIs | We don't convert margin product types |

---

## §7. Dhan v2 RELEASE CHANGES — what affects us

### v2.4 (Sep 2025) — applies to us

| Change | Impact on tickvault |
|---|---|
| 24-hour Access Token mandate | Already locked in `auth/token_manager.rs` (23h pre-emptive refresh) |
| Static IP mandatory for Order APIs (effective April 1, 2026) | Boot Step 5.5 verifies via `/v2/ip/getIP`; `ordersAllowed=true` gate |
| API Key + Secret 12-month validity | Available as alternative to TOTP flow; deferred |

### v2.3 (Sep 2025) — partial relevance

| Change | Impact |
|---|---|
| Order API rate limit 10/sec | Locked in OMS rate limiter (when live trading wired) |
| 200-level depth | We DROPPED depth entirely per LOCK-I — not relevant |
| Historical expired options data | Useful for BRUTEX backtesting (deferred) |

### v2.5 (Feb 2026) — new features available

| Feature | Used? |
|---|---|
| Conditional Trigger Orders | 🟠 OPTIONAL — useful if strategy uses indicator-based entries |
| P&L based exit (`/v2/pnlExit`) | 🟠 OPTIONAL — server-side exit; alternative to our client-side risk engine |
| Exit All API (`DELETE /v2/positions`) | 🟡 USED Phase 1.5+ — EOD + emergency square-off |
| Programmatic token generation (with TOTP) | 🟢 USED — exactly what we do |
| Option Chain enhancements (`average_price`, `security_id` per strike) | 🟢 USED — already in our option chain RAM schema |

### v2.2 (Mar 2025)

| Feature | Used? |
|---|---|
| Super Orders | 🟠 OPTIONAL Phase 1.5+ |
| User Profile API | 🟢 USED — boot validation |
| Intraday Historical 5-year + OI | 🟢 USED — cross-verify + BRUTEX future |

### v2.1 (Jan 2025)

| Feature | Used? |
|---|---|
| 20-level depth | 🔴 NOT needed (depth dropped) |
| Option Chain API | 🟢 USED — heart-piece |

### v2.0 (Sep 2024)

| Feature | Used? |
|---|---|
| Market Quote REST | 🔴 NOT needed at our scope |
| Forever Orders | 🔴 NOT needed |
| Live Order Update WS | 🟢 USED |
| Margin Calculator | 🟡 USED Phase 1.5+ |

---

## §8. The 6 Dhan APIs we hit daily in LOCKED scope

For 100% clarity, these are the ONLY Dhan APIs tickvault calls today (no live trading enabled):

| # | API | Frequency | Purpose |
|---|---|---|---|
| 1 | `POST /app/generateAccessToken` | Once at boot (08:03 IST) | TOTP-based 24h JWT |
| 2 | `GET /v2/profile` | Once at boot | Validate dataPlan + segment + token validity |
| 3 | `GET /v2/ip/getIP` | Once at boot Step 5.5 | Verify static IP whitelist |
| 4 | `wss://api-feed.dhan.co` | Persistent connection | 4 IDX_I ticker subscription |
| 5 | `POST /v2/optionchain` | Every 50s × 3 underlyings | Option chain RAM update |
| 6 | `POST /v2/charts/intraday` | At 15:31 IST | Cross-verify for 1m/5m/15m × 4 SIDs |
| 7 | `POST /v2/charts/historical` | At 08:05 IST | Morning 1d cross-check + 30-day rehydration on boot |
| 8 | `POST /v2/optionchain/expirylist` | At 15:31 IST | Detect expiry rollover |
| 9 | `GET /v2/RenewToken` | Every 23h | Pre-emptive JWT renewal |
| 10 | `wss://api-order-update.dhan.co` | Persistent connection | Order update WS (currently no orders placed in dry-run, but connection stays alive for when live trading is enabled) |

**Total daily API call count at locked scope (estimated):**

| API | Calls/day |
|---|---|
| TOTP generate + RenewToken | 2 |
| /v2/profile | 1 |
| /v2/ip/getIP | 1 |
| Option chain | 450 cycles × 3 underlyings = 1,350 |
| /v2/charts/intraday | 16 (15:31 cross-verify: 4 SIDs × 4 TFs — wait, locked to 3 TFs = 12) |
| /v2/charts/historical | 4 (morning 1d) + 84 (boot rehydration, once at boot) = 88 |
| /v2/optionchain/expirylist | 1 |
| **Daily total** | **~1,455 REST calls** |

Well within Dhan's Data API rate limits (5/sec, 100K/day).

---

## §9. The 8+ Dhan APIs we add when LIVE TRADING enables (Phase 1.5)

Currently designed (libraries exist) but NOT WIRED:

| API | Why we add |
|---|---|
| `POST /v2/orders` | Place orders from strategy signals |
| `PUT /v2/orders/{id}` | Modify on partial fill / re-pricing |
| `DELETE /v2/orders/{id}` | Cancel on strategy reverse |
| `GET /v2/orders` | Reconcile order book |
| `GET /v2/positions` | Live position tracking |
| `POST /v2/margincalculator` | Pre-trade margin check |
| `GET /v2/fundlimit` | Pre-trade balance check |
| `POST /v2/killswitch` | Emergency halt + risk engine |
| `GET /v2/killswitch` | Boot validation |
| `DELETE /v2/positions` | EOD + emergency square-off |
| `GET /v2/trades` | EOD P&L computation |

Plus the order update WS we already have connected — events flow as orders are placed.

**Adding these is the Phase 1.5 wiring milestone (~6-8 weeks).**

---

## §10. Static IP — the operational reality you must own

| Step | Action | Who | When |
|---|---|---|---|
| 1 | AWS EIP allocated via Terraform | Cowork (bootstrap) | At AWS setup |
| 2 | EIP shown to operator | Cowork | After `terraform apply` |
| 3 | Operator logs into web.dhan.co → Profile → Setup IP | **OPERATOR (manual)** | After step 2 |
| 4 | Operator enters EIP as PRIMARY → submits | OPERATOR | Same |
| 5 | 7-day cooldown begins after submission | Dhan-side | Automatic |
| 6 | Tickvault boots → `/v2/ip/getIP` → confirms `ordersAllowed=true` | Tickvault | Daily at boot |
| 7 | If EIP ever changes (e.g., accidental release) | OPERATOR repeats step 3-4 | Rare |

**Cannot be automated by Claude/Cowork — Dhan requires operator's logged-in web session.** This is the ONE remaining manual step.

---

## §11. Honest envelope (operator-charter §F)

> "We use 10 Dhan APIs daily in the locked indices-only scope (no live trading). We add 11 more when Phase 1.5 wiring enables live trading. We do NOT use ~30 other Dhan APIs (Super Order, Forever Order, EDIS, holdings, partner login, BSE F&O, currency F&O, etc) — they're not in scope for retail indices-only.
>
> All used APIs map to existing or designed Rust code paths. v2.4 mandates (24h token, Static IP) are honored at boot. v2.5 features (Conditional Trigger, P&L Exit, Exit All) are available if operator wants them — currently deferred.
>
> The only manual step Claude cannot automate: operator must register the EIP with Dhan via web.dhan.co. 5-minute task, one-time per IP change (7-day cooldown for re-registration)."

---

## §12. Trigger / auto-load

This rule activates when editing:
- `crates/core/src/auth/`
- `crates/trading/src/oms/api_client.rs`
- `crates/core/src/websocket/`
- `crates/core/src/option_chain/`
- Any file calling `api.dhan.co` or `auth.dhan.co`
