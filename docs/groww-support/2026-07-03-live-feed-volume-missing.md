<!--
=============================================================================
GROWW SUPPORT DOSSIER — 2026-07-03 live feed sends zero volume / OI / OHLC
SEND FROM subramaniaparthiban@gmail.com (the Groww-registered account)
Share the GitHub link in the email — do NOT copy-paste plain text
(Gmail's proportional font destroys the tables; GitHub renders them).

GMAIL BODY TO PASTE (one line + the link — nothing else):

    Hi Groww API team, we are seeing zero volume/openInterest/OHLC on every
    live-feed tick (only ltp + tsInMillis populate) — full evidence, verbatim
    tick JSON, and 4 specific questions here:
    https://github.com/SJParthi/tickvault/blob/main/docs/groww-support/2026-07-03-live-feed-volume-missing.md
    Thank you, Parthiban

BEFORE SENDING — operator-fill checklist (every intentional placeholder):
  1. <GROWW_API_SUPPORT_EMAIL — operator fill before sending>
       — no Groww support email exists in this repo; do NOT guess one.
  2. <GROWW_CLIENT_ID — operator fill before sending>  (appears 2x)
       — Groww account identifier; NEVER substitute the Dhan Client ID / UCC
         (those are Dhan credentials — see docs/dhan-support/).
  Find them all:  grep -n '<[A-Z_]' docs/groww-support/2026-07-03-live-feed-volume-missing.md
=============================================================================
-->

# Live feed delivers zero volume / openInterest / OHLC on every tick — only `ltp` + `tsInMillis` populate — New request

**To:** <GROWW_API_SUPPORT_EMAIL — operator fill before sending>
**From:** subramaniaparthiban@gmail.com (the Groww-registered account)
**Subject:** Live feed (growwapi SDK, subscribe_ltp) — volume / openInterest / OHLC are 0.0 on 444,657 consecutive market-hours ticks; only ltp + tsInMillis ever populate (2026-07-03 evidence)
**Date:** 2026-07-03

---

Hi Groww API team,

We consume your live market feed via the official `growwapi` Python SDK.
Prices stream correctly (LTP with millisecond timestamps is excellent),
but **every other field on every tick — volume, openInterest, and the
OHLC family — arrives as `0.0`**, across the entire subscribed universe,
throughout market hours. We would like to know whether this is expected
for our subscription mode, a different mode, or an account entitlement
issue. All evidence below was captured live on 2026-07-03 (labelled
**Verified** = observed directly in our capture pipeline).

| Field | Value |
|---|---|
| Groww Client ID | `<GROWW_CLIENT_ID — operator fill before sending>` |
| Name | Parthiban Subramanian |
| Contact email | subramaniaparthiban@gmail.com (Groww-registered account) |
| SDK | official `growwapi` Python SDK, `GrowwFeed` |
| Feed endpoint | `wss://socket-api.groww.in` (NATS-over-WebSocket + Protobuf) |
| Subscription mode | `subscribe_ltp` |
| Universe | 768 instruments — NSE_EQ (segment `CASH`) + indices |
| Date of evidence | 2026-07-03 IST |
| Evidence window | 14:36–15:30 IST (in-market) + probe tick at 14:35:35 IST |

---

## What we subscribed (Verified)

- Official `growwapi` Python SDK, `GrowwFeed`.
- Subscription call: `subscribe_ltp` for **768 instruments** — NSE_EQ
  equities (segment `CASH`) plus indices.
- Ticks stream continuously during market hours; connection, decode, and
  LTP capture all work (see the what-works table below).

## What every tick looks like (Verified — verbatim raw tick)

One raw tick captured by a probe on 2026-07-03 at **09:05:35 UTC
(14:35:35 IST)**, for `exchange_token` **7929** (NSE_EQ / segment CASH),
pasted verbatim and unreformatted:

```json
{"avgPrice":0.0,"bidQty":0.0,"close":0.0,"high":0.0,"highPriceRange":0.0,"highTradeRange":0.0,"low":0.0,"lowPriceRange":0.0,"lowTradeRange":0.0,"ltp":1134.9,"offerQty":0.0,"open":0.0,"openInterest":0.0,"tsInMillis":1783069535444.0,"value":0.0,"volume":0.0}
```

Note: `ltp` (1134.9) and `tsInMillis` (millisecond precision) are
populated and correct. **Every other numeric field is exactly `0.0`.**

## This is not a single bad tick — the full-session count (Verified)

We instrumented a counter over the live stream during market hours,
**14:36–15:30 IST on 2026-07-03**:

| Measure | Count |
|---|---|
| Consecutive live ticks inspected | **444,657** |
| Ticks with nonzero `volume` | **0** |
| Ticks with nonzero `openInterest` | **0** |
| Ticks with nonzero OHLC (`open`/`high`/`low`/`close`/`avgPrice`/`value`/`bidQty`/`offerQty`) | **0** |
| Fields ever populated | only `ltp` + `tsInMillis` |

444,657 out of 444,657 in-market ticks across 768 instruments carried
zeros in every field except `ltp` and `tsInMillis`. This rules out an
illiquid-instrument explanation — the universe includes the most liquid
NSE equities and indices.

---

## What works vs what fails — same account, same session (Verified)

| Aspect | Status | Evidence |
|---|---|---|
| Feed connect + auth (`GrowwFeed`, daily token) | ✅ Works | 444,657 ticks received 14:36–15:30 IST |
| `ltp` values | ✅ Works | e.g. `ltp: 1134.9` for exchange_token 7929; prices track the market |
| `tsInMillis` millisecond timestamps | ✅ Works — excellent | preserved end-to-end in our capture |
| `volume` | ❌ Always `0.0` | 0 of 444,657 ticks nonzero |
| `openInterest` | ❌ Always `0.0` | 0 of 444,657 ticks nonzero |
| `open` / `high` / `low` / `close` / `avgPrice` / `value` / `bidQty` / `offerQty` | ❌ Always `0.0` | 0 of 444,657 ticks nonzero |

Because connect, auth, decode, and LTP all work on the same connection
in the same minute, account/token/network problems are ruled out as the
cause of the zero fields.

---

## Questions

1. **Is volume / openInterest / OHLC expected to be populated on the
   `subscribe_ltp` subscription mode?** Or is `subscribe_ltp`
   LTP-plus-timestamp only by design, with the remaining protobuf fields
   always zero?
2. **If another subscription mode carries volume/OI/OHLC** (e.g. a
   quote/market-depth/full mode), can you share for that mode: the exact
   SDK call or NATS subject pattern, the protobuf message type, and any
   rate limits / instrument caps that apply to it?
3. **Is this account- or entitlement-related?** Does the account
   `<GROWW_CLIENT_ID — operator fill before sending>` need a different
   data-plan/entitlement for volume/OI on the live feed, and if so which
   one?
4. **What is your recommended way to obtain cumulative day volume per
   instrument on the live feed** (768 NSE_EQ/index instruments), given
   the constraint that we consume the live stream only (no per-instrument
   REST polling)?

---

We are happy to run any diagnostics you need — different subscription
modes, a reduced instrument set, specific exchange_tokens, packet
captures (tcpdump) of the WebSocket session, or a reproduction window at
a time of your choosing. The missing volume/OI/OHLC currently blocks our
candle (OHLCV) construction from your feed, so a definitive answer on
questions 1–4 unblocks us immediately.

Thank you,
**Parthiban Subramanian**
subramaniaparthiban@gmail.com
