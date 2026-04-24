# Clarification — Stock F&O tradability on expiry day and day before — Ticket #TBD

**To:** apihelp@dhan.co
**From:** parthiban@example.com
**Subject:** Clarification request — stock F&O tradability on T and T-1 (expiry)
**Date:** 2026-04-24

---

Hi Dhan Support,

Writing to formally confirm our internal understanding of stock F&O
liquidity on expiry day and the day before. Our subscription planner
implements a rollover rule that depends on Dhan's / NSE's behaviour
for these two sessions, and we want a written reference we can cite
in our runbook.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban |
| UCC | `NWXF17021Q` |
| Context | Planning / subscription-capacity decision, not a bug report |

---

## What we currently implement (our understanding)

For **stock F&O contracts only** — NIFTY / BANKNIFTY / FINNIFTY /
MIDCPNIFTY (indices) are **excluded** from this rule — our planner
rolls the subscribed expiry forward when today is within 1 trading
day of the nearest expiry:

| Today | Trading days to nearest expiry | Planner picks |
|---|---|---|
| T-2 (e.g. Tue with Thu expiry) | 2 | **Current** expiry |
| T-1 (e.g. Wed with Thu expiry) | 1 | **Next** expiry |
| T (expiry day, e.g. Thu) | 0 | **Next** expiry |

Our reasoning, which we want Dhan to confirm or correct:

1. **Stock F&O contracts are illiquid / near-untradeable on expiry day
   (T).** Volume collapses after the early session; the contract's
   utility ends at the NSE settlement cutoff (~3:25 PM IST).
2. **Stock F&O contracts are noticeably illiquid on T-1** as the
   rollover crowd has already moved to the next contract.
3. **Indices (NIFTY / BANKNIFTY / FINNIFTY / MIDCPNIFTY) remain liquid
   right up to expiry seconds** — spreads widen, but volume stays
   tradeable until market close.

Treating stock F&O on T and T-1 as "don't subscribe" frees 25 strikes ×
~200 stocks = ~10,000 slots of our 25,000-slot main-feed capacity for
the next expiry cycle instead.

---

## Specific questions

1. **Do you concur with the stock-vs-index distinction above?** i.e.
   that stock options/futures are effectively untradeable on T and T-1,
   whereas index options are tradeable right up to expiry.
2. **Is there an NSE circular or Dhan guideline** we should reference
   instead of writing our own policy? We'd rather cite a primary source.
3. **Does Dhan's own web/app order-book reject or warn on stock F&O
   orders placed on T-1 / T?** We'd like to align our pre-trade check
   with Dhan's behaviour.
4. **Does Dhan's market-quote REST endpoint
   (`POST /v2/marketfeed/ltp`) still return valid LTPs for stock F&O
   on T-1 / T?** If the endpoint goes silent early we want to know so
   we don't alarm operators.

No blocker on our side — we've implemented Fix #6 with the strict
≤1 trading-day rollover based on our best understanding. Your
confirmation (or correction) will be added as the citation in our
runbook at `docs/runbooks/expiry-day.md`.

---

## Code references (public URLs will be available on merge)

- Helper: `crates/core/src/instrument/subscription_planner.rs` →
  `select_stock_expiry_with_rollover`
- Threshold: `STOCK_EXPIRY_ROLLOVER_TRADING_DAYS = 1`
- Runbook: `docs/runbooks/expiry-day.md` → section "Stock F&O Expiry
  Rollover"

Happy to hop on a brief call if a conversation is faster than email.

Thanks,
Parthiban

---

<!--
=============================================================================
WORKFLOW NOTES (per docs/dhan-support/README.md)
=============================================================================

1. After this file is merged to main, the public URL becomes:
   https://github.com/SJParthi/tickvault/blob/main/docs/dhan-support/2026-04-24-expiry-day-non-tradeable-clarification.md
2. Send Gmail reply to apihelp@dhan.co with the URL in one short line.
3. Do NOT paste the markdown body into Gmail — Arial destroys table
   alignment.
4. On reply:
   - Update the "Hi <SUPPORT_AGENT_NAME>" line with their name.
   - Update the ticket number in the title.
   - Append Dhan's response (verbatim) in a new section at the bottom.
5. Link the Dhan response in docs/runbooks/expiry-day.md as the
   citation for the rollover rule.
=============================================================================
-->
