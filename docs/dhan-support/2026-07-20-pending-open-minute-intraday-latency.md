# POST /v2/charts/intraday — open-minute (09:15/09:16 IST) candles served EMPTY (HTTP 2xx, zero candles) + elevated response latency vs. a peer data vendor — Ticket #PENDING

- **To:** apihelp@dhan.co
- **From:** Parthiban (sjparthi93@gmail.com)
- **Date:** 2026-07-20

| Field | Value |
|---|---|
| Client ID | 1106656882 |
| Name | Parthiban |
| UCC | NWXF17021Q |
| API surface | Data API — `POST /v2/charts/intraday` |
| Incident window | Monday 2026-07-20, 09:15–09:18 IST (empties) + whole session (latency) |

## What we tested / observed

We pull the just-closed 1-minute candle for three index spots once per minute, ~1.3 s after each minute close, at <=3 requests/second:

| Contract | SecurityId | exchangeSegment | interval | Window shape |
|---|---|---|---|---|
| NIFTY 50 (spot index) | 13 | IDX_I | "1" | day-granular `fromDate = D 00:00:00`, `toDate = D+1 00:00:00`, client-side minute filter |
| NIFTY BANK (spot index) | 25 | IDX_I | "1" | same |
| SENSEX (spot index) | 51 | IDX_I | "1" | same |

**URL used:** `https://api.dhan.co/v2/charts/intraday`

**Request JSON (shape):**

```json
{
  "securityId": "13",
  "exchangeSegment": "IDX_I",
  "instrument": "INDEX",
  "interval": "1",
  "fromDate": "2026-07-20 00:00:00",
  "toDate": "2026-07-21 00:00:00"
}
```

**Error / response captured — two distinct problems:**

1. **Open-minute candles served EMPTY.** For the session's first two minutes (09:15 and 09:16 IST bars), the endpoint returned **HTTP 2xx with ZERO candles** for all three SecurityIds. The bars only appeared in the response ~2 minutes later. A peer data vendor queried from the same host served the same two bars ~19.4 s after each minute close.
2. **Elevated response latency.** Whole-session per-cycle response times: Dhan p50 1077 ms / p90 1152 ms / p99–max 4019 ms vs peer p50 308 ms / p90 505 ms / max 1798 ms (n=88 cycles per vendor, ~3.5x median gap; two Dhan cycles took ~4.0 s).

## Key observation

The empty responses were clean HTTP 2xx with zero candles — nothing in the response distinguishes "bar not yet published" from "no such data", so a client has nothing to key retries on. The latency gap is persistent across the whole session, not a one-off spike.

| Time (IST) | Event |
|---|---|
| 09:16:xx | intraday request for the 09:15 bar -> 2xx, zero candles (all 3 SIDs) |
| 09:17:xx | intraday request for the 09:16 bar -> 2xx, zero candles (all 3 SIDs) |
| ~09:17:35 | peer vendor (same host, same minutes) serving both bars (~+19.4 s after close) |
| ~09:18 | Dhan intraday response finally includes the 09:15 + 09:16 bars |
| whole session | Dhan p50 1077 ms vs peer p50 308 ms per cycle |

Microsecond request/response timestamps for the two empty minutes: `<TS-0915-REQ>` / `<TS-0915-RESP>`, `<TS-0916-REQ>` / `<TS-0916-RESP>` (available on request from our logs).

## What works on the same account / same minute

| Check | Result |
|---|---|
| `POST /v2/optionchain` (same account, same session) | Working — 735/735 per-minute chain pulls served |
| The same intraday request shape from ~09:18 onward | Works — returns the bars incl. 09:15/09:16 |
| Auth / static IP / entitlement | Valid — every affected call returned 2xx, so auth + IP + data plan are ruled out |
| Peer data vendor, same host, same minutes | Served the 09:15/09:16 bars ~19.4 s after close |

## Verbatim log lines (for grep)

```
09:16 IST fire: SPOT1M ok=0 empty=3 errors=0   (09:15 bar missing on all 3 SIDs)
09:17 IST fire: SPOT1M ok=0 empty=3 errors=0   (09:16 bar missing on all 3 SIDs)
dhan_cycle p50=1077ms p90=1152ms max=4019ms n=88 | peer_cycle p50=308ms p90=505ms max=1798ms n=88
```

## Requests to Dhan engineering

1. What is the publication SLA for a just-closed 1-minute candle on `POST /v2/charts/intraday`, especially for the first minutes of the session (09:15/09:16 IST)? Is a ~2-minute delay expected behaviour or a fault?
2. Is a ~1.0–1.2 s median response time expected for this endpoint with a day-granular window? Is there a lower-latency request shape for "the latest closed minute" you recommend?
3. Were there known server-side delays on 2026-07-20 between 09:15–09:17 IST for SecurityIds 13 / 25 / 51 (IDX_I)?
4. Is there any endpoint or parameter that exposes just-closed bars faster without a live market-feed WebSocket?

## Diagnostic offer

We can run tcpdump on our side, repeat the requests at any cadence you specify, test alternate SecurityIds or request shapes, or provide microsecond-stamped request/response logs for any window you name.

— Parthiban (Client ID 1106656882, UCC NWXF17021Q)
