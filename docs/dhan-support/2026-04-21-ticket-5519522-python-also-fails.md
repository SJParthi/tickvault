<!--
=============================================================================
DHAN SUPPORT EMAIL — Ticket #5519522 follow-up, 2026-04-21
=============================================================================
Share this GitHub link in the Gmail reply — do NOT copy-paste plain text
(proportional font destroys the ASCII tables below).
-->

# 200-level depth still failing on ATM contracts — Python SDK also reproduces — Ticket #5519522

**To:** apihelp@dhan.co
**From:** Parthiban (`1106656882`)
**Subject:** Re: Ticket #5519522 — 200-level depth TCP reset reproduces identically in Python SDK
**Date:** 2026-04-21

---

Hi team,

Thank you for the 2026-04-10 confirmation that `/twohundreddepth` is the
correct path on `full-depth-api.dhan.co`. We redeployed against that
path and are still seeing the same TCP reset today in production AND
in your own Python SDK pattern. This email isolates the fault to the
**server side** and asks for a targeted check.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban |
| UCC | `NWXF17021Q` |
| Date of incident | 2026-04-21 |
| Time of incident | 13:06:26 – 14:10:30 IST |
| Prior ticket | #5519522 |
| Data plan | Active (verified via `GET /v2/profile`) |
| Active segment | Derivative (verified) |

---

## What we tested

We subscribed to the NEAREST expiry, ATM strike for NIFTY via two
independent clients on the same account:

1. **Our production Rust client** — `tokio-tungstenite 0.29.0` + `rustls` (aws-lc-rs)
2. **Minimal Python repro** — `websockets` 15.0.1 + `requests` 2.32.5 (pasted in full at the bottom)

Both connect to:

```
wss://full-depth-api.dhan.co/twohundreddepth?token=<REDACTED>&clientId=1106656882&authType=2
```

With the subscribe JSON:

```json
{"RequestCode": 23, "ExchangeSegment": "NSE_FNO", "SecurityId": "63458"}
```

---

## Observations

### REST APIs are 100% healthy

| API | Result |
|---|---|
| `GET /v2/profile` | ✅ 200 OK — `dataPlan = "Active"`, `activeSegment` contains `"Derivative"` |
| `POST /v2/optionchain/expirylist` | ✅ nearest expiry: `2026-04-21` |
| `POST /v2/optionchain` | ✅ ATM strike resolved: `24550.0` (spot LTP `24536.40`, delta 13.60 < strike gap 50 → confirmed ATM) |

### 20-level depth WORKS on the same account + same SecurityId

`wss://depth-api-feed.dhan.co/twentydepth`, same token, same client ID,
same `SecurityId = 63458`:

| Test | Result |
|---|---|
| Subscribe `{ "RequestCode": 23, "InstrumentCount": 1, "InstrumentList": [{ "ExchangeSegment": "NSE_FNO", "SecurityId": "63458" }] }` | ✅ Connected + subscribed |
| Wait for frames | ✅ 4 frames received in < 2 s (BID code 41, 1328 B / 1992 B) |

**Conclusion:** auth, IP, dataPlan, ATM-strike selection, and the
underlying instrument record are ALL correct.

### 200-level depth FAILS on the SAME account + SAME SecurityId

`wss://full-depth-api.dhan.co/twohundreddepth`, same token, same client
ID, same SecurityId = 63458 (which the 20-level endpoint happily
streams for):

| Test | Result |
|---|---|
| TLS handshake + HTTP upgrade to WebSocket | ✅ OK |
| Server accepted WebSocket handshake | ✅ OK |
| Sent subscribe JSON above | ✅ (write returns OK) |
| Wait up to 60 s for any frame | ❌ **`ConnectionClosedError: no close frame received or sent`** — TCP reset |

Our Rust client sees exactly the same thing:

```
err: ConnectionFailed {
    url: "wss://full-depth-api.dhan.co/twohundreddepth",
    source: Protocol(ResetWithoutClosingHandshake),
}
```

Reproduces on all 4 contracts we tried:

| Contract | SecurityId | Timestamp IST | Outcome |
|---|---|---|---|
| `NIFTY-2026-04-21-24550-CE` | `63458` | 14:10:30 | TCP reset |
| `NIFTY-2026-04-21-24550-PE` | `63463` | 13:16 | TCP reset |
| `BANKNIFTY-2026-04-28-57300-CE` | `67554` | 13:16 | TCP reset |
| `BANKNIFTY-2026-04-28-57300-PE` | `67555` | 13:16 | TCP reset |

All 4 are nearest-expiry ATM or near-ATM (≤ 1 strike from spot).

---

## Python-SDK-pattern repro (paste-and-run)

The attached `scripts/depth200-quick-test.py` in our repo runs the
full probe end-to-end: REST profile → expiry list → ATM lookup →
20-level control test (proves account+SID work) → 200-level test. It
uses **only `websockets` + `requests`** (the same primitives your
`dhanhq` Python SDK wraps). Full last 50 lines of its output at
14:10:30 IST:

```
Client ID:  1106656882
Token:      eyJ0eXAi...REDACTED
[OK] Token from cache file (data/cache/tv-token-cache)

[1/3] Fetching NIFTY spot LTP (proves token + REST works)...
  NIFTY Spot LTP: 24536.4
[2/3] Fetching nearest expiry...
  Nearest expiry: 2026-04-21
[3/3] Fetching option chain for ATM strike...
  ATM strike: 24550.0 (diff from spot: 13.60)
  ATM CE SecurityId: 63458
  Contract: NIFTY-2026-04-21-24550-CE

========================================================================
  REST API Status (all working)
========================================================================
  NIFTY Spot LTP:    24536.4
  ATM SecurityId:    63458
  Contract:          NIFTY-2026-04-21-24550-CE
  Token valid:       YES (REST APIs responded)

========================================================================
  Control Test: 20-Level Depth
========================================================================

--- Control test: 20-level depth (should WORK) ---
SecurityId: 63458 (NIFTY-2026-04-21-24550-CE)
  [20-lvl FRAME 1] BID | sid=63458 | 1328B
  [20-lvl FRAME 2] BID | sid=63458 | 1328B
  [20-lvl FRAME 3] BID | sid=63458 | 1328B
  [20-lvl FRAME 4] BID | sid=63458 | 1992B
  [OK] 20-level depth WORKS (4 frames)

========================================================================
  200-Level Depth Tests
========================================================================

--- Test: /twohundreddepth ---
URL:         wss://full-depth-api.dhan.co/twohundreddepth?token=REDACTED&clientId=1106656882&authType=2
SecurityId:  63458 (NIFTY-2026-04-21-24550-CE)
Segment:     NSE_FNO
Subscribe:   {"RequestCode": 23, "ExchangeSegment": "NSE_FNO", "SecurityId": "63458"}
[...] Connecting...
[OK]  Connected!
[OK]  Subscription sent
[...] Waiting for depth frames (up to 60s)...
websockets.exceptions.ConnectionClosedError: no close frame received or sent
```

---

## What works vs what fails (table for clarity)

| Layer | 20-level `/twentydepth` | 200-level `/twohundreddepth` |
|---|---|---|
| TLS handshake | ✅ | ✅ |
| WS upgrade | ✅ | ✅ |
| Subscribe request accepted | ✅ | ✅ |
| Server responds within 60 s | ✅ (4 frames in < 2 s) | ❌ (TCP reset) |
| `dataPlan`, `activeSegment`, token, IP allowlist | ✅ | — (same account) |
| ATM strike, SecurityId 63458 | ✅ | — (same SID) |

The ONLY variable is the endpoint. Same account, same token, same SecurityId on the same minute.

---

## Questions / asks

1. On the 200-level server for client `1106656882` on 2026-04-21
   13:06–14:10 IST, did you observe the 4 subscription attempts for
   security_ids `63458`, `63463`, `67554`, `67555`? If so, what was
   the server-side disposition — accepted-and-silent, accepted-and-reset,
   or rejected before stream?
2. Is there a known issue or feature-flag on the 200-level service for
   this account that would cause the server to accept the subscribe
   and then immediately TCP-reset?
3. Our `GET /v2/profile` shows `dataPlan = "Active"` with Derivative
   segment. Is 200-level depth billed / gated separately from 20-level?
4. Can you point us at a known-working `(account, SecurityId, time)`
   tuple against `/twohundreddepth` so we can rule out whether our
   implementation is silently different? Ideally on a dev account so
   we can diff the handshake byte-for-byte.

---

## What we have already ruled out on our side

* URL path (`/twohundreddepth` confirmed by your 2026-04-10 reply)
* Auth / token validity (same token streams 20-level fine and REST APIs OK)
* IP allowlist (REST APIs accept our IP)
* Strike / expiry (verified ATM via the option chain API)
* Rust-specific TLS quirks (reproduced with Python `websockets` + `ssl`)

We are now blocked at the server boundary. Any insight on the
server-side disposition of our requests would unblock us.

---

## Offered diagnostics (on request)

* `tcpdump` on the failing TLS session (ready to capture whenever)
* Additional SecurityIds at any strike/expiry you specify
* Test from a secondary registered IP
* Change the subscribe JSON to any shape you want us to try
* Paste-and-run Python script is already in our repo at
  [`scripts/depth200-quick-test.py`](https://github.com/SJParthi/tickvault/blob/main/scripts/depth200-quick-test.py)
  — you can run it on your side against a test account if useful.

Thanks very much for the continued attention on this one.

Best,
Parthiban

---

## Operator workflow note

Per `docs/dhan-support/README.md`, share the GitHub link in a short Gmail reply:

> Hi team,
> Full reproduction details + Python output attached:
> https://github.com/SJParthi/tickvault/blob/main/docs/dhan-support/2026-04-21-ticket-5519522-python-also-fails.md
>
> Parthiban

Do **not** paste the markdown body directly into Gmail — proportional fonts destroy the ASCII tables.
