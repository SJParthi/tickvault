# Reply to Trishul — Ticket #5519522

**To:** apihelp@dhan.co
**From:** sjparthi93@gmail.com
**Subject:** Re: Ticket #5519522 — 200-level Full Depth still resetting after applying both fixes
**Date:** 2026-04-15

---

Hi Trishul,

Thank you for the quick response on Ticket #5519522.

We applied **both** fixes you suggested on April 10:

1. Switched URL path from `/` to `/twohundreddepth`
2. Switched `SecurityId` from far-OTM to live ATM of current-week expiry

The 200-level depth is **still** resetting within seconds of subscription. Details below for your server log correlation.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban Subramanian |
| UCC | `NWXF17021Q` |
| Date | Tuesday 2026-04-15 (regular trading day) |
| Time of failures | 15:25:48 IST |

---

## What we tested today

Only ATM contracts, current-week expiry, NIFTY + BANKNIFTY only — **4 connections total**, well within your 5-connection limit.

| Contract | SecurityId | Reset timestamp (IST) |
|---|---|---|
| `NIFTY-Jun2026-28500-CE` | `55190` | `2026-04-15 15:25:48.782155` |
| `NIFTY-Jun2026-28500-PE` | `55191` | `2026-04-15 15:25:48.781519` |
| `BANKNIFTY-May2026-74900-CE` | `68775` | `2026-04-15 15:25:48.781526` |
| `BANKNIFTY-May2026-74900-PE` | `68776` | `2026-04-15 15:25:48.782032` |

**All 4 reset within 1 millisecond of each other** — same pattern as before.

**URL used** (exactly as you instructed on April 10):

```
wss://full-depth-api.dhan.co/twohundreddepth?token=***&clientId=1106656882&authType=2
```

**Subscription JSON sent:**

```json
{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"55190"}
```

**Error captured:**

```
ConnectionFailed {
    url: "wss://full-depth-api.dhan.co/twohundreddepth",
    source: Protocol(ResetWithoutClosingHandshake)
}
```

---

## Key observation — data DID flow before the reset

In the same millisecond window as the resets, our QuestDB writer was flushing 200-level depth frames to disk:

| Timestamp (IST) | Event | Rows flushed |
|---|---|---|
| `15:25:48.860` | deep depth batch flushed to QuestDB | **500** |
| `15:25:48.862` | deep depth batch flushed to QuestDB | **513** |

So the subscription **was** accepted, 200-level frames **were** streaming, and then your server sent a TCP `RST` without any WebSocket Close frame. This is not a clean disconnect — it is a raw TCP reset.

---

## What works perfectly on the same account, same token, same minute

| WebSocket | Endpoint | Status | Evidence |
|---|---|---|---|
| 20-level depth | `depth-api-feed.dhan.co/twentydepth` | Streaming | 200 snapshots flushed |
| Live Market Feed | `api-feed.dhan.co` | Streaming | 704 tick rows flushed |
| Live candle aggregator | (internal) | Working | 65, 60, 47, 43, 128 rows flushed across threads |
| Order Update WebSocket | `api-order-update.dhan.co` | Connected | auth accepted, listening |

**Only the 200-level endpoint on `full-depth-api.dhan.co` is resetting.** Everything else is fine.

---

## Verbatim log lines (for your grep)

```json
{"timestamp":"2026-04-15T15:25:48.781519+05:30","level":"WARN",
 "fields":{"security_id":55191,"label":"NIFTY-Jun2026-28500-PE","attempt":1,
           "err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\",
                  source: Protocol(ResetWithoutClosingHandshake) }"}}

{"timestamp":"2026-04-15T15:25:48.781526+05:30","level":"WARN",
 "fields":{"security_id":68775,"label":"BANKNIFTY-May2026-74900-CE","attempt":1,
           "err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\",
                  source: Protocol(ResetWithoutClosingHandshake) }"}}

{"timestamp":"2026-04-15T15:25:48.782032+05:30","level":"WARN",
 "fields":{"security_id":68776,"label":"BANKNIFTY-May2026-74900-PE","attempt":1,
           "err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\",
                  source: Protocol(ResetWithoutClosingHandshake) }"}}

{"timestamp":"2026-04-15T15:25:48.782155+05:30","level":"WARN",
 "fields":{"security_id":55190,"label":"NIFTY-Jun2026-28500-CE","attempt":1,
           "err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\",
                  source: Protocol(ResetWithoutClosingHandshake) }"}}
```

---

## Requests to your support / engineering team

1. **Server log correlation.** Please check your server logs for these exact SecurityIds at `2026-04-15 15:25:48.78 IST` — what does your server log as the reason for resetting the TCP connection?

   - `55190` → NIFTY Jun2026 28500 CE
   - `55191` → NIFTY Jun2026 28500 PE
   - `68775` → BANKNIFTY May2026 74900 CE
   - `68776` → BANKNIFTY May2026 74900 PE

2. **Feature flag.** Is there a separate feature flag on account `1106656882` for the 200-level Full Market Depth that is different from the 20-level flag? 20-level works, 200-level does not — on the same Data API plan.

3. **Connection cap.** Is 4 concurrent connections on `full-depth-api.dhan.co` hitting any internal cap lower than the documented 5?

4. **Working example.** Can you share a working Python or Node example that successfully receives **continuous** 200-level frames (not just one packet) for any SecurityId? Even one working example would help us compare byte-by-byte.

5. **Verbose close-frame logging.** The TCP `RST` suggests your server closed us intentionally. Can engineering enable verbose close-frame logging on our `clientId` for the next 24 hours so we can see the reason code?

---

We are happy to run any diagnostic tests you need — different SecurityIds, different times of day, `tcpdump` capture, different IP (we have a secondary IP registered). This is blocking our live order book for NIFTY and BANKNIFTY and we need to move forward.

Thank you,
**Parthiban Subramanian**
Client ID: `1106656882`
