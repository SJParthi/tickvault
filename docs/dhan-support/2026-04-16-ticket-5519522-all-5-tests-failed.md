# Re: 200-Level Full Market Depth — All 5 Tests Failed Including Your SDK & SID

**Ticket:** #5519522
**Client ID:** 1106656882
**Date:** 2026-04-16, 13:23 IST (during market hours)

---

Hi Shubham,

Thank you for the Python script and SDK suggestion. We tested everything you recommended — **all tests fail on our account.**

---

## Test Results

| # | Method | SecurityId | URL Path | Segment Format | Result |
|---|--------|-----------|----------|----------------|--------|
| 1 | Raw WebSocket | `63422` (ATM NIFTY 24100 CE, spot 24124) | Root `/` | `"NSE_FNO"` (string) | **FAILED** — TCP reset, zero frames |
| 2 | Raw WebSocket | `63422` (ATM) | `/twohundreddepth` | `"NSE_FNO"` (string) | **FAILED** — zero frames, then reset |
| 3 | Raw WebSocket | `63422` (ATM) | `/twohundreddepth` | `2` (numeric) | **FAILED** — zero frames, then reset |
| 4 | Raw WebSocket | `63424` **(your suggested SID)** | Root `/` | `"NSE_FNO"` (string) | **FAILED** — zero frames, then reset |
| 5 | Your SDK `dhanhq==2.2.0rc1` | `63422` | SDK handles URL | SDK handles segment | **SDK crashes** — Python 3.9 incompatible |

> **Note on Test 5:** Your SDK v2.2.0rc1 uses Python 3.10+ `match` statement (`_super_order.py` line 54). Our Mac has Python 3.9. This is a bug in the SDK.

---

## What WORKS on the Same Account, Same Token, Same Moment

| WebSocket Type | Status | Evidence |
|---------------|--------|----------|
| **20-Level Depth** | **Streaming** | 4 connections, all green, flushing to QuestDB |
| **Live Market Feed** | **Streaming** | ~3K ticks/sec across 5 connections |
| **Order Update WS** | **Connected** | Auth OK, receiving order events |
| **All REST APIs** | **Healthy** | Option chain, LTP, historical — all responding |

---

## Conclusion

200-level Full Market Depth appears to be **not enabled at the account level** for Client ID `1106656882`.

We have exhausted every variable:
- Both URL paths (`/` and `/twohundreddepth`)
- Both segment formats (string `"NSE_FNO"` and numeric `2`)
- Both SecurityIds (live ATM `63422` and your suggested `63424`)
- Your own Python SDK `dhanhq==2.2.0rc1`

**Every other Dhan WebSocket and API works perfectly on this account.**

---

## Request

Please check on your server whether 200-level Full Market Depth is **enabled** for Client ID `1106656882` and enable it if not.

Happy to connect on **Google Meet** — available today 2-4 PM IST or tomorrow 9:30-11 AM IST. I'll run the diagnostic live on screen share. Please have someone from engineering who can check server-side logs for `clientId=1106656882` on `full-depth-api.dhan.co`.

Thank you,
**Parthiban Subramanian**
Client ID: `1106656882`
UCC: `NWXF17021Q`
