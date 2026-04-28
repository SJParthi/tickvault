# Depth-200 WebSocket — APP token rejected, SELF token accepted (same account, same minute, same script)

**To:** apihelp@dhan.co
**Subject:** Depth-200 WebSocket: TOTP-generated APP token fails, web.dhan.co SELF token works — same account, same SecurityId, same script
**Date:** 2026-04-28

---

Hi Dhan Support team,

We have isolated the root cause of our 2+ week depth-200 disconnect issue (originally Ticket #5519522). The endpoint `wss://full-depth-api.dhan.co/` **rejects access tokens with `tokenConsumerType: "APP"` but accepts tokens with `tokenConsumerType: "SELF"`** — verified today on our account using your official Python SDK `dhanhq==2.2.0rc1`.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban |
| UCC | `NWXF17021Q` |
| Date of test | 2026-04-28 (Tuesday) |
| Time of test | ~15:20–15:28 IST |
| SDK | `dhanhq==2.2.0rc1` (your official Python package) |
| Test script | `scripts/dhan-200-depth-repro/check_depth_200.py` (in our repo) |

---

## The smoking gun — same script, same SID, only the token source changed

| # | Token source | `tokenConsumerType` claim | Outcome |
|---|---|---|---|
| 1 | `POST /app/generateAccessToken` (TOTP-driven, our app's automated flow) | `APP` | ❌ subscribe accepted, then immediate `Exception: no close frame received or sent` — zero frames received |
| 2 | Manual generation from `web.dhan.co/index/profile` "Generate Access Token" button | `SELF` | ✅ streams continuously, thousands of bid/ask depth-200 frames received in 30s |

**Test SecurityId:** `74147` (NIFTY ATM contract, NSE_FNO segment, current expiry).

Both tokens were:
- Generated within the same hour
- Used against the exact same Python script (no code change between runs)
- Used to call the same URL: `wss://full-depth-api.dhan.co/?token=...&clientId=1106656882&authType=2`
- Sent the same subscribe message: `{"RequestCode": 23, "InstrumentCount": 1, "InstrumentList": [{"ExchangeSegment": "NSE_FNO", "SecurityId": "74147"}]}`

The ONLY difference between Test 1 and Test 2 is the token source.

---

## JWT claim comparison

Decoding both JWTs (we can share the full payloads privately if needed):

| Claim | APP token (failing) | SELF token (working) |
|---|---|---|
| `tokenConsumerType` | `"APP"` | `"SELF"` |
| `partnerId` | present | absent |
| `webhookUrl` | absent | present |
| `dhanClientId` | `1106656882` | `1106656882` |
| `exp` | 24h validity | 24h validity |
| Length | ~280 chars | ~303 chars |

Same client, same expiry semantics, same JWT signing — only the consumer-type discriminator differs.

---

## What works on the same account at the same minute (rules out account / token / network issues)

| WebSocket | Endpoint | APP token | SELF token |
|---|---|---|---|
| Live Market Feed | `api-feed.dhan.co` | ✅ Streams | ✅ Streams |
| 20-level depth | `depth-api-feed.dhan.co/twentydepth` | ✅ Streams | ✅ Streams |
| **200-level depth** | **`full-depth-api.dhan.co/`** | ❌ **Reset** | ✅ **Streams** |
| Order Update | `api-order-update.dhan.co` | ✅ Streams | ✅ Streams |
| REST `/v2/marketfeed/ltp` etc. | `api.dhan.co/v2` | ✅ 200 OK | ✅ 200 OK |

**Only the 200-level depth endpoint discriminates by token consumer type.** This is a server-side gate, not a client bug.

---

## Verbatim error from Test 1 (APP token)

```
Testing SecurityId=74147 for 30s ...
[connect] wss://full-depth-api.dhan.co/?token=eyJ...&clientId=1106656882&authType=2
[subscribe sent] {"RequestCode": 23, "InstrumentCount": 1, "InstrumentList": [{"ExchangeSegment": "NSE_FNO", "SecurityId": "74147"}]}
Exception: no close frame received or sent

  Frames received : 0
  Test duration   : 30s
  Frames per sec  : 0.0

FAIL — zero frames in 30s — connection dropped or far-OTM filtered
```

## Verbatim success from Test 2 (SELF token, same script)

```
Testing SecurityId=74147 for 30s ...
[connect] wss://full-depth-api.dhan.co/?token=eyJ...&clientId=1106656882&authType=2
[subscribe sent] {"RequestCode": 23, "InstrumentCount": 1, "InstrumentList": [{"ExchangeSegment": "NSE_FNO", "SecurityId": "74147"}]}
  ... frame 50 at +1.2s
  ... frame 100 at +2.5s
  ... frame 500 at +12.8s
  ... frame 1000 at +25.4s

  Frames received : 1187
  Test duration   : 30s
  Frames per sec  : 39.6

PASS — sustained streaming (1187 frames in 30s)
```

---

## Why this is a problem for us

Our trading system uses your `POST /app/generateAccessToken` endpoint daily — it is the **only documented programmatic token-mint API**, and it produces APP-type tokens by design. We rely on this for all 4 WebSockets and all REST APIs.

Today we have proof that depth-200 alone refuses APP tokens. This means:

- We cannot use depth-200 with our automated TOTP flow.
- The only working alternative we have found is for the operator to manually generate a SELF token from `web.dhan.co` every 24 hours and paste it into the app — which defeats the purpose of automation.

---

## Specific questions for Dhan engineering

1. **Is `tokenConsumerType: "SELF"` an intentional hard requirement on `wss://full-depth-api.dhan.co/`?** If yes, please confirm so we can plan around it. If no, this is a server bug — please raise with the depth-200 service team.

2. **Is there any documented or undocumented programmatic API that mints SELF-type tokens?** We have searched `/v2/` docs and only see APP-type via `/app/generateAccessToken` and `/app/consumeApp-consent`. If a SELF-mint API exists, please share the endpoint.

3. **Will `GET /v2/RenewToken` preserve `tokenConsumerType: "SELF"` if we feed it a SELF token?** If yes, we can do the manual paste once and renew programmatically every 23h. Please confirm.

4. **Can our account hold both an active APP token AND an active SELF token simultaneously?** Per docs "one active token per application" — does this allow two tokens if we register a second `Application` entry on `web.dhan.co`, or is it one token per `dhanClientId` regardless of application count?

5. **Was depth-200 always SELF-only, or did this gate get added recently?** We had it working on 2026-04-23 with the same SDK on the same account — at that moment we believe a SELF token was in use. If the gate was added in a recent server deploy, please confirm the date so we can correlate with our incident timeline.

6. **If this is policy, please update `docs/v2/full-market-depth-websocket` to state `tokenConsumerType` requirement explicitly.** Other developers will hit the same wall.

---

## What we can run for you on request

- The exact failing/passing JWT payloads (privately — they are still valid until tomorrow morning)
- `tcpdump` of both connections side-by-side
- Re-run with any SecurityId you specify
- Re-run from our secondary registered IP
- Try `RenewToken` on the SELF token and report whether claims are preserved

---

This has blocked our depth-200 production rollout for 2+ weeks (Ticket #5519522 history). The repro is now mechanical and reproducible on demand. We just need a clear answer on questions 1–4 above and we can either work around it (per-endpoint token cache) or remove the workaround (if you fix the gate).

Thank you for the help so far.

**Parthiban**
Client ID: `1106656882`
UCC: `NWXF17021Q`
