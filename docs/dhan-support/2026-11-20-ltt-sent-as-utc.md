<!--
=============================================================================
DHAN SUPPORT EMAIL — 2026-11-20 Live Market Feed Last Trade Time arrived as
UTC epoch seconds instead of the documented IST epoch seconds
Drafted by Claude Code on the owner's request (decision card 2026-11-20,
"Yes, draft it (Recommended)"). The owner sends it; no session contacts Dhan.
Share the GitHub link in the Gmail reply — do NOT copy-paste plain text.
=============================================================================
-->

# Live Market Feed: Last Trade Time arrived 5 h 30 m early (UTC instead of IST) on 2026-11-20 — New Ticket (please assign)

**To:** apihelp@dhan.co
**From:** sjparthi93@gmail.com
**Subject:** New Ticket — Live Market Feed v2 sent Last Trade Time as UTC epoch (5 h 30 m behind) for every segment on 2026-11-20; was this a change or a fault, and will it continue?
**Date:** 2026-11-20

---

Hi Dhan API Support team,

On 20 November 2026 every packet on our Live Market Feed carried a Last Trade Time that was exactly 5 hours 30 minutes earlier than on every previous day. Your documentation says this field is in IST epoch seconds. On 20 November it matched UTC epoch seconds instead. We need to know whether this was a deliberate change or a fault, and which format to expect from now on.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban Subramanian |
| UCC | `NWXF17021Q` |
| Date of incident | 2026-11-20 (Friday) |
| Time of incident | From our first connection at 08:31:22 IST until at least 16:24 IST (the whole session) |

---

## What we tested / observed

We connect to the Live Market Feed at `wss://api-feed.dhan.co` (version 2), Quote and Full packets, about 20,000 instruments across `IDX_I`, `NSE_EQ` and `NSE_FNO`. Our server clock is kept in sync with the AWS time service; its offset stayed under 1 millisecond all day.

The Last Trade Time field (bytes 13–16 of a Ticker packet, bytes 15–18 of Quote and Full packets, `uint32` little-endian) is documented as **IST epoch seconds** (seconds counted as if the epoch started at 1970-01-01 00:00 IST). Until 19 November 2026 the value always matched that. On 20 November it matched **UTC epoch seconds**, so read as IST every trade appeared 19,800 seconds (5 h 30 m) too early.

Exact packets (receive time is our server clock; "raw LTT" is the `uint32` exactly as received):

| Contract | SecurityId | Segment | Received (IST) | Raw LTT | Read as IST epoch (documented) | Read as UTC epoch | LTP |
|---|---|---|---|---|---|---|---|
| `NIFTY 50` | `13` | `IDX_I` | `2026-11-20 11:00:00.035322` | `1795152600` | 05:30:00 IST | **11:00:00 IST** | 25549.10 |
| `NIFTY BANK` | `25` | `IDX_I` | `2026-11-20 11:00:00.097987` | `1795152600` | 05:30:00 IST | **11:00:00 IST** | 63371.25 |
| `SOLARINDS` | `13332` | `NSE_EQ` | `2026-11-20 11:00:00.001699` | `1795152595` | 05:29:55 IST | **10:59:55 IST** | 18691.00 |
| `TATAPOWER` | `3426` | `NSE_EQ` | `2026-11-20 11:00:00.006870` | `1795152597` | 05:29:57 IST | **10:59:57 IST** | 339.65 |
| `NIFTY-23Nov2026-25550-CE` | `64475` | `NSE_FNO` | `2026-11-20 09:15:00.054209` | `1795146300` | 03:45:00 IST | **09:15:00 IST** | 56.00 |
| `HDFCBANK-23Nov2026-820-CE` | `97544` | `NSE_FNO` | `2026-11-20 09:15:00.053726` | `1795146300` | 03:45:00 IST | **09:15:00 IST** | 5.40 |

The same instruments on 19 November 2026 had a Last Trade Time within about a second of our receive time when read as IST, as documented. For example, NIFTY 50 (`13`) at 11:00 IST on 19 November closed its minute at 25302.20 with an IST-read trade time inside that minute.

---

## Key observation

- Every segment was affected (`IDX_I`, `NSE_EQ`, `NSE_FNO`), on every one of our connections, from the very first packet of the day (08:31:22 IST).
- The offset was exactly 19,800 seconds every time, which is the IST offset. The value looks like a correct trade time in plain UTC, not random corruption.
- The prices, volumes and order book looked normal. Only the time field changed.

| Time (IST) | Event | Value |
|---|---|---|
| `08:31:22` | First packet after connecting | Raw LTT read as IST = 03:01:22 IST (5 h 30 m early) |
| `11:00:00` | NIFTY 50 (`13`) | Raw LTT `1795152600` = 05:30:00 if IST, 11:00:00 if UTC |
| `16:24` | Last packet we checked | Still 5 h 30 m early |

---

## Questions

1. Was the Last Trade Time on the Live Market Feed changed from IST epoch seconds to UTC epoch seconds on 20 November 2026, or was this a fault?
2. From Monday 23 November 2026, which format will this field use: IST epoch (as documented) or UTC epoch?
3. If the format has changed, will the documentation be updated, and does the same change apply to any other time field (for example Full Market Depth, Order Update or Historical data)?
4. Was any other field changed on 20 November 2026 (prices, volume, open interest, packet layout)?

---

## What we can provide

- The raw binary frames for 20 November 2026 (every packet, with our receive time in microseconds), for any contract and time you name.
- The same for 19 November 2026, for comparison.

Thank you,
Parthiban Subramanian
Client ID `1106656882`

---

## Dhan's answer (received 2026-11-21)

**Ticket:** #6684672. **Received:** 21 Nov 2026, 10:35 UTC (16:05 IST). Verbatim as relayed to us (the `...` marks text left out of the relay, not ours):

> "Yes, the Live LTT timestamp format has been changed from IST-based epoch to UTC-based epoch effective 20th November 2026. ... The change was implemented on 20th November. However, an exact implementation timestamp is not available. ... Yes, this change is part of an API restructuring exercise. ... The restructuring was carried out to standardise the timestamp format across all API endpoints. Please note that UTC-based epoch is now the permanent format for all Live Market Feed timestamps. ... As of now, there are no further changes planned regarding the timestamp format. Please note that these changes were implemented before the market opened on 20th November."

### What it settles

| Question | Status | Dhan's words |
|---|---|---|
| Was it a deliberate change or a fault? (our Q1) | **Verified: deliberate change** | "changed from IST-based epoch to UTC-based epoch effective 20th November 2026" |
| When did it start? | **Verified: before the market opened on 20 Nov; no exact time** | "implemented before the market opened on 20th November" / "an exact implementation timestamp is not available" |
| Which format from Mon 23 Nov? (our Q2) | **Verified: UTC epoch, permanent** | "UTC-based epoch is now the permanent format for all Live Market Feed timestamps" |
| Was it part of a wider change? | **Verified: yes, an API restructuring** | "part of an API restructuring exercise ... to standardise the timestamp format across all API endpoints" |
| Will it change again? | **Verified: none planned** | "As of now, there are no further changes planned regarding the timestamp format" |
| Will the documentation be updated? (part of our Q3) | **Unknown** | not stated in the relayed text |
| Do other time fields change (depth, order update, historical)? (rest of our Q3) | **Unknown** | "across all API endpoints" suggests other endpoints were also moved to UTC; historical data was already UTC epoch. Not checked yet for order update or depth (depth sockets are switched off) |
| Was any non-time field changed? (our Q4) | **Unknown** | not stated in the relayed text; on 20 Nov prices, volumes and the book looked normal (our own check) |

### What we did with it

- Owner decisions (cards, 2026-11-21): **"Yes, always UTC (Recommended)"** and **"Before Monday if checks pass (Recommended)"**.
- Rule: `.claude/rules/project/websocket-connection-scope-lock.md` §"2026-11-21" (full text in `docs/claude-rules-full/project/`). From a WAL receipt of 2026-11-20 00:00 IST every Live Market Feed trade time gets exactly +19,800 s; earlier receipts are unchanged; a page fires if Dhan ever goes back to IST epoch (noise lock §2.24-a).
- Dated notes added to `docs/dhan-ref/03-live-market-feed-websocket.md` (Section 10 note 1) and `docs/dhan-ref/08-annexure-enums.md` §12b. The original documentation text is kept.
- No reply needed to close the ticket. A follow-up question on the documentation and on the order update time field can be sent if the owner wants.
