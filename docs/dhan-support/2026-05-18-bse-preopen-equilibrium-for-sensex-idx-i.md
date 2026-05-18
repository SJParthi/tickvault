<!--
Dhan support draft per docs/dhan-support/TEMPLATE.md
Topic: BSE pre-open equilibrium price for SENSEX IDX_I — wire-format question
Created: 2026-05-18 (Mon, Sunday post-market)
Reference: Phase 0 Items 13+14 wiring PR (`topic-PHASE-0-LEAN-LOCKED.md` §9)
-->

# BSE Pre-Open Equilibrium Price for SENSEX IDX_I — Wire-Format Question

**To:** apihelp@dhan.co
**From:** \<OPERATOR_EMAIL\>
**Subject:** New Ticket — IDX_I (BSE indices): how is the 09:15 official open price published in your feeds?
**Date:** 2026-05-18

---

Hi Dhan API Support team,

We're building a candle-aggregation pipeline and need to populate the **`open` field of the 09:15 IST 1-minute candle** for SENSEX (`security_id = 51`, `exchange_segment = IDX_I`) with **BSE's officially-published pre-open auction equilibrium price**, NOT the first post-09:15 trade tick price.

For NSE indices (NIFTY = 13, BANKNIFTY = 25) our pre-open buffer captures the equilibrium correctly from the live feed during 09:00–09:14 IST. For SENSEX (BSE-derived) we have not been able to identify the analogous wire-format path. Today's QuestDB query (2026-05-18, Monday — first trading day after Sunday) shows:

| `ts` (IST) | `open` | `high` | `low` | `close` | `tick_count` |
|---|---|---|---|---|---|
| 09:10 | 74807.97 | 74807.97 | 74807.97 | 74807.97 | (heartbeat) |
| 09:11 | 74807.97 | 74807.97 | 74807.97 | 74807.97 | 53 |
| 09:12 | 74807.97 | 74807.97 | 74807.97 | 74807.97 | 47 |
| 09:13 | 74807.97 | 74807.97 | 74807.97 | 74807.97 | 43 |
| 09:14 | 74807.97 | 74807.97 | 74807.97 | 74807.97 | 51 |
| **09:15** | **74670.64** | **74670.64** | **74429.77** | **74429.77** | 58 |
| 09:16 | 74425.86 | 74446.68 | 74347.68 | 74396.61 | 54 |

Observations:

1. Your IDX_I feed for SecurityId 51 emits ~50 ticks per minute during 09:10–09:14 IST, all at the same price (74,807.97). We interpret this as the **previous-day-close held value** during the BSE pre-open window — please confirm.
2. The first tick after 09:15:00 IST is at **74,670.64**. Our aggregator stamps this as the 09:15 candle's `open` — but it does not match TradingView (also Dhan-sourced) which shows the 09:15 candle's `open = 74,807.97` (the pre-open held value).
3. We could not find any wire-format mechanism that emits BSE's **pre-open call-auction equilibrium price** for SENSEX IDX_I. NSE has the pre-open buffer mechanism (09:00–09:08 auction → equilibrium frozen at 09:08 → fed as ticks 09:08-09:15); we do not see the equivalent for BSE in your IDX_I feed.

---

## Specific questions (please answer for SENSEX `security_id = 51`, segment `IDX_I`)

| # | Question | Our current guess (please confirm or correct) |
|---|---|---|
| 1 | During the BSE pre-open window (09:00–09:15 IST), is the IDX_I tick we receive at 74,807.97 the **previous trading day's closing index value**, frozen until market open? | YES (previous-day close held) |
| 2 | Does BSE run an opening call-auction for SENSEX that produces a **distinct equilibrium price** (i.e. not equal to previous close) at 09:08 IST? If yes, do you publish that price anywhere in your Live Market Feed for IDX_I? | UNKNOWN |
| 3 | Does your REST endpoint `POST /v2/marketfeed/quote` populate `day_open` for IDX_I segment requests? Or is IDX_I unsupported in that endpoint (the documented examples only show `NSE_EQ` and `NSE_FNO`)? | UNKNOWN — please clarify |
| 4 | Is the first post-09:15:00 IST tick for SENSEX (in our query: 74,670.64) the BSE official opening price published by BSE, or the first matched trade in the BSE order book post-open? | UNKNOWN |
| 5 | If TradingView's Dhan-fed chart shows the 09:15 candle's `open = 74,807.97` (matching the pre-open held value, NOT the first post-09:15 tick), what wire-format mechanism do they use to source that value? | UNKNOWN |
| 6 | For NSE indices (NIFTY = 13, BANKNIFTY = 25): do you publish the NSE pre-open auction's equilibrium price (frozen at 09:08 IST) as a tick value in the 09:08–09:15 window? | YES (we receive it correctly via the IDX_I `Ticker` mode tick stream) |

---

## Why this matters for us

Our trading algorithm uses the **09:15 candle's `open` field** as the reference price for gap-up / gap-down classification, opening-range breakout detection, and pivot-level computation. If the 09:15 candle's `open` is the first POST-09:15 trade (74,670.64) instead of the pre-open finalized equilibrium / previous-day held value (74,807.97), every gap-detection rule is wrong by ~130 index points (~0.17%). Our internal cross-check against your TradingView chart proves the discrepancy.

We want to ship the right wire-format path for SENSEX before going live; please confirm the official source.

---

## Reference: previous tickets that anchored similar wire-format clarifications

| Ticket | Topic | Outcome |
|---|---|---|
| #5525125 | Previous-day close routing in Quote bytes 38-41 / Full bytes 50-53 | Confirmed — used in `previous_close_persistence.rs` |
| #5519522 | Depth-200 endpoint URL pattern | Resolved |
| Pending (2026-05-01) | Volume field cumulative semantic | (awaiting reply) |
| (this ticket) | SENSEX IDX_I pre-open equilibrium price source | (pending) |

---

## What we'll do once we have your answer

| Your answer | Our wiring |
|---|---|
| The pre-open held value at 09:14:59 (74,807.97) is the "official open" | Wire SENSEX into our pre-open buffer (`crates/core/src/instrument/preopen_price_buffer.rs`); read at 09:14:59 IST snapshot |
| `/v2/marketfeed/quote` `day_open` field IS populated for IDX_I | Add SENSEX to our REST fallback at 09:14:55 IST; capture `day_open` value |
| BSE publishes a distinct equilibrium price via some other mechanism (e.g. specific tick code at 09:08) | Add a parser branch for that mechanism |
| First post-09:15 trade (74,670.64) IS the BSE official open by design | Keep current behaviour; document explicitly that BSE has no pre-open auction-equilibrium price separate from first trade |

We will block the wiring PR until we have your confirmation. Thank you for your continued support.

---

Best regards,

\<OPERATOR_NAME\>
Client ID: 1106656882
UCC: NWXF17021Q
