<!--
Dhan support draft per docs/dhan-support/TEMPLATE.md
Topic: Live Market Feed WebSocket — does Quote/Full subscription work for IDX_I segment?
Created: 2026-05-18 (Mon) PR #2.5 follow-up
Reference: PR #2.5 of AWS-lifecycle 14-PR sequence
            (claude/aws-lifecycle-pr-2-5-explicit-day-ohlc, GitHub PR #702)
            + .claude/rules/project/live-market-feed-subscription.md "IDX_I Special Case"
-->

# Live Market Feed WebSocket — Does Quote/Full Subscription Work for IDX_I Segment?

**To:** apihelp@dhan.co
**From:** \<OPERATOR_EMAIL\>
**Subject:** New Ticket — IDX_I (Index segment): does `Quote` (code 17) or `Full` (code 21) WebSocket subscription mode deliver 50/162-byte packets, or are IDX_I subscriptions silently downgraded to `Ticker` (16-byte) packets?
**Date:** 2026-05-18

---

Hi Dhan API Support team,

We are building an indices-only trading system. Our universe is locked to 4 IDX_I `security_id` values:

| Security ID | Symbol | Segment |
|---|---|---|
| 13 | NIFTY | IDX_I |
| 25 | BANKNIFTY | IDX_I |
| 51 | SENSEX | IDX_I |
| 21 | INDIA VIX | IDX_I |

We need to know **definitively** which Live Market Feed WebSocket subscription modes are supported for `exchange_segment = IDX_I`.

## Background

Per your **`docs/dhan-ref/03-live-market-feed-websocket.md`** documentation (and equivalently the published API docs at https://dhanhq.co/docs/v2/live-market-feed/):

| Request Code | Mode | Packet Size | Documented Fields |
|---|---|---|---|
| 15 | Subscribe Ticker | 16 bytes | LTP (f32), LTT (u32) only |
| 17 | Subscribe Quote | 50 bytes | LTP, LTQ, LTT, ATP, **day_open**, **previous_close**, **day_high**, **day_low**, day_volume, total_buy_qty, total_sell_qty |
| 21 | Subscribe Full | 162 bytes | All of Quote + 5-level market depth |

The **`day_open` / `day_high` / `day_low` / `day_volume`** fields in Quote (bytes 34-41, 42-49) and Full (bytes 46-49, 54-61) modes are critical for us — they would carry the **NSE/BSE official equilibrium open price** as well as the cumulative day high/low/volume directly from the exchange, removing the need for us to derive these from a stream of LTPs.

Our internal documentation contains the following claim (from our project rules file, source-uncited):

> "Indices (segment code 0) are forced to Ticker mode regardless of configured feed mode. Dhan silently ignores Quote/Full subscriptions for IDX_I."

**We have not been able to find this restriction in your official documentation**, and we cannot empirically test it on the production account during market hours without risking subscription disruption. Please confirm the actual behaviour.

## What works (empirically verified by us today, 2026-05-18 Monday pre-open + post-open window)

For NSE indices (NIFTY = 13, BANKNIFTY = 25) subscribed in `Ticker` mode (code 15) we DO receive the NSE pre-open auction's equilibrium price as a tick value in the 09:08–09:14 IST window. Our pre-open buffer captures it correctly. The 09:15:00 IST first-trade tick LTP differs from the equilibrium, but the pre-open buffer's last captured value (09:14) is a faithful proxy for the equilibrium open.

For SENSEX (BSE, security_id = 51) we have a separate ticket open (2026-05-18-bse-preopen-equilibrium-for-sensex-idx-i.md) about the BSE pre-open mechanism — that is unrelated to this Quote/Full question.

## Specific questions (please answer for `exchange_segment = IDX_I`)

| # | Question | Our assumption | Please confirm or correct |
|---|---|---|---|
| 1 | If we send subscribe-request RequestCode = **17** (Quote) for an IDX_I instrument (e.g. NIFTY security_id = 13), will your feed deliver **50-byte Quote packets** to us, OR will it deliver 16-byte Ticker packets regardless (silent downgrade)? | UNKNOWN — our internal docs say it silently downgrades, but the claim has no Dhan-citation. | Please confirm exact behaviour for IDX_I. |
| 2 | If we send subscribe-request RequestCode = **21** (Full) for an IDX_I instrument, will your feed deliver **162-byte Full packets** with the 5-level market depth section, OR is depth meaningless for indices and the request is downgraded to Quote (50 bytes) or further to Ticker (16 bytes)? | UNKNOWN | Please confirm. |
| 3 | If Quote mode IS supported for IDX_I: the Quote packet's `day_open` field (bytes 34-37 per your docs) — is this the **NSE/BSE official equilibrium open price**, populated at 09:15:00 IST and held constant for the rest of the trading day? Or does it have different semantics for IDX_I vs equity/derivative segments? | We assume it would be the equilibrium open (consistent with Quote packet's documented `day_open` field for other segments). | Please confirm. |
| 4 | If Full mode IS supported for IDX_I: the 5-level market depth section (bytes 62-161) — what does it carry for an index value that doesn't have a real order book? Empty? Synthetic? Or is this mode genuinely unsupported for IDX_I? | UNKNOWN | Please clarify. |
| 5 | If Quote/Full mode is NOT supported for IDX_I (i.e. our internal docs are correct that there is silent downgrade): is there a programmatic way for us to detect this — e.g. an `errorCode` in the subscribe-response, or a packet-size we can check on receipt? | UNKNOWN | Please advise. |
| 6 | Is there any official Dhan documentation page or release note that states the mode restriction for IDX_I subscriptions? | Could not find one. | Please link if it exists. |

## Why this matters

Our trading pipeline requires the 09:15:00 IST official open price for the 4 IDX_I SIDs. We have two paths:

- **Path A (current):** subscribe in `Ticker` mode and derive the open from the pre-open buffer's last captured close at 09:14 IST. Works empirically for NIFTY/BANKNIFTY; SENSEX/VIX equilibrium mechanism is the subject of the parallel BSE-equilibrium ticket.
- **Path B (preferred if supported):** subscribe in `Quote` or `Full` mode and read the `day_open` field directly from the packet — that field would carry the exchange-authoritative equilibrium price. No buffer derivation needed.

We need a definitive answer on whether Path B is supported for IDX_I segment before we choose the production architecture.

## What we offer for diagnostics

- We can subscribe a single IDX_I instrument (e.g. INDIA VIX = 21) in Quote/Full mode at a time of your choosing, capture the raw byte stream via tcpdump, and share the binary frames with you for forensic review. This isolates the test from our main production subscriptions.
- We can run the test on a secondary IP (the `secondaryIP` registered via `/v2/ip/setIP`) so it does not affect our primary subscription pool.
- We can provide our binary packet parser source code for review if a packet-size mismatch is suspected.

## Reference details

| Field | Value |
|---|---|
| Client ID | 1106656882 |
| Client Name | \<OPERATOR_NAME\> |
| UCC | NWXF17021Q |
| Data API subscription status | Active (per `GET /v2/profile`) |
| Active segments | Equity, Derivative, Currency, Commodity |
| Primary IP | (registered per `/v2/ip/getIP`) |

## Open tickets (cross-reference)

| Ticket | Topic |
|---|---|
| 2026-05-18-bse-preopen-equilibrium-for-sensex-idx-i.md | BSE pre-open equilibrium price for SENSEX IDX_I (parallel concern) |
| (this ticket) | IDX_I Quote/Full mode support |

---

Thank you for your time. A definitive answer on questions 1, 2, 3 above unblocks our production architecture decision.

Best regards,
\<OPERATOR_NAME\>
Client ID: 1106656882
UCC: NWXF17021Q
