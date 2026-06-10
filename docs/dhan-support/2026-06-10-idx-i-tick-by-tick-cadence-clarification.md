# Live Market Feed: "tick-by-tick" claim vs measured sampling on IDX_I — cadence clarification

**To:** apihelp@dhan.co
**From:** sjparthi93@gmail.com
**Subject:** Live Market Feed — is the WebSocket truly tick-by-tick? Your own chart shows prints your WS never delivered (NIFTY 50, 2026-06-10 09:49:59 IST, microsecond evidence inside)
**Date:** 2026-06-10

---

Hi Team,

We need an engineering-level clarification on the **push cadence contract** of the Live Market Feed WebSocket. Your documentation states the feed is *"Real-time **tick-by-tick event-based** market data via WebSocket"*, but our microsecond-stamped capture shows the feed delivering **~2–4 packets per second** for an index, and — the diagnostic part — **your own tv.dhan.co chart displays a price print that your WebSocket never transmitted to us**. Full evidence below.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban |
| UCC | `NWXF17021Q` |
| Date of incident | 2026-06-10 (Wednesday, normal trading day) |
| Time of incident | 09:49:57 – 09:50:02 IST |
| Instrument | **NIFTY 50 index** (`IDX_I`, SecurityId `13`) |
| Subscription mode | Quote (Feed Request Code 17) |

---

## What we tested / observed

One Live Market Feed connection, subscribed in Quote mode to NIFTY 50 (SecurityId 13, `IDX_I`). Our capture pipeline stamps every received WebSocket frame with a **microsecond receive timestamp before any processing** (write-ahead log → database), so the list below is the complete, gap-free record of every packet your server sent us for SID 13 in this window.

**URL used:**

```
wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=1106656882&authType=2
```

### Every packet received for NIFTY 50 (SID 13), 09:49:57 – 09:50:02 IST

| # | received_at (IST, microsecond) | LTT in packet (IST epoch) | LTP | Gap since previous |
|---|---|---|---|---|
| 1 | `09:49:57.817220` | 1781084997 | 23,378.35 | — |
| 2 | `09:49:58.041574` | 1781084998 | 23,377.80 | 224 ms |
| 3 | `09:49:58.538758` | 1781084998 | 23,378.15 | 497 ms |
| 4 | `09:49:58.791020` | 1781084998 | 23,378.10 | 252 ms |
| 5 | `09:49:59.041260` | 1781084999 | 23,378.55 | 250 ms |
| 6 | `09:49:59.291430` | 1781084999 | 23,378.60 | 250 ms |
| 7 | `09:49:59.543030` | 1781084999 | **23,379.25** | 252 ms |
| 8 | `09:50:00.035531` | 1781085000 | 23,380.00 | 492 ms |
| 9 | `09:50:01.054520` | 1781085001 | 23,379.95 | 1,019 ms |
| 10 | `09:50:01.310805` | 1781085001 | 23,379.75 | 256 ms |

That is **~2–4 packets/second**, steady, with no receive-side anomaly (our socket was healthy, ping/pong normal, zero disconnects, the same connection kept streaming before and after).

---

## Key observation — your own chart contradicts the stream

Open **tv.dhan.co**, NIFTY 50, **5-second** chart, candle starting **09:49:55 on 2026-06-10**. Your chart shows that candle closing at:

```
O 23,376.15   H 23,379.25   L 23,376.15   C 23,379.15
```

A close of **23,379.15** means Dhan's internal tape has a print of 23,379.15 occurring AFTER our packet #7 (23,379.25 at 09:49:59.543) and BEFORE 09:50:00.000 — i.e. **inside the 09:49:59.543 → 09:50:00.035 window in which your WebSocket sent us NOTHING**.

| Timestamp (IST) | Event | Value |
|---|---|---|
| `09:49:59.543` | Last WS packet of the 09:49 minute delivered to us | LTP 23,379.25 |
| `09:49:59.5 – 09:50:00.0` | Print visible on tv.dhan.co 5s candle close, **never delivered on WS** | 23,379.15 |
| `09:50:00.035` | Next WS packet delivered to us | LTP 23,380.00 |

So the same data your charting backend has was **not forwarded on the "tick-by-tick" WebSocket**. This is not a complaint about one paise — it tells us the WS is a **conflated/sampled snapshot stream**, which directly contradicts the documentation wording and matters for anyone building candles from the stream.

---

## What works on the same account / same minute (rules out account/token/network)

| WebSocket | Endpoint | Status | Evidence |
|---|---|---|---|
| Live Market Feed | `api-feed.dhan.co` | Streaming normally | the 10 packets above; uninterrupted before/after |
| Order Update | `api-order-update.dhan.co` | Connected | login ack at 08:46 IST same session, same token |

Token valid (generated 08:45 IST), single connection, no 805/disconnect events, no rate-limit responses. The only question is **what the server chooses to emit**.

---

## Requests to Dhan engineering (numbered — please answer each)

1. **Is the Live Market Feed WebSocket tick-by-tick or conflated?** The docs say *"tick-by-tick event-based"*. Precisely: is **every exchange print** for a subscribed instrument forwarded as a packet, or does the server **sample/conflate** to a cadence? Yes/no per mode (Ticker/Quote/Full) please.
2. **If conflated: what is the exact cadence rule?** Per segment please — `IDX_I` vs `NSE_EQ` vs `NSE_FNO` (e.g. "latest snapshot every N ms per instrument"). We measure ~2–4 packets/sec on IDX_I; please confirm the designed number.
3. **Check the specific window from your server-side logs** for clientId `1106656882`: between `2026-06-10 09:49:59.543` and `09:50:00.035` IST, was ANY packet for SecurityId 13 (`IDX_I`) emitted to our connection? If none was emitted (rather than dropped), that confirms conflation is server-side by design.
4. **Is there any Dhan product or mode that delivers the full unconflated tick stream for indices**, or is `/v2/charts/intraday` (your full internal tape) the only access to it?
5. **Is there any sequence number or counter in the binary protocol** that would let a client detect a skipped/withheld packet? The 8-byte response header (code, length, segment, SecurityId) carries none — if conflation is by design, clients have no protocol-level way to know.
6. **If the answer to (1) is "conflated":** please correct or qualify the *"tick-by-tick event-based"* wording in the v2 documentation for the affected modes/segments, so integrators can build to the real contract.

---

## Why this matters to us (context, not a complaint)

We build 1-minute candles from the live stream and cross-verify nightly against `/v2/charts/intraday`. Persistent paise-level differences on minute closes (e.g. our 09:49 close = 23,379.25 from your last delivered packet vs your chart's 23,379.15) are fully explained **if and only if** the WS is conflated. We simply need the real contract in writing to set our verification tolerances correctly.

We are happy to run any diagnostic you need — tcpdump on our side for any window you nominate, different SIDs, parallel Ticker-vs-Quote-vs-Full subscriptions, or a screen-share. Please involve someone from engineering who can read server-side emit logs for `clientId=1106656882`.

Thank you,
**Parthiban**
Client ID: `1106656882`
