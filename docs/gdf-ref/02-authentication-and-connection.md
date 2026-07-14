# GDF 02 — Authentication & Connection (WebSockets API)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/how-to-connect-using-websockets-api/ · …/websockets-api-documentation/authenticate/ · …/faqs/faqs/ · …/documentation-support/diagnostic-api-responses/
> **Fetched:** 2026-07-13 · **Evidence tier:** SEARCH (doc-page prose) + CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: gfdlws/ws.py; ws-gfdl@1.1.0) + GITHUB-SAMPLE(js-2020) — verbatim JSON is from the official artifacts; nothing is LIVE-DOC.

---

## 1. Endpoint model — per-account host:port, NOTHING public

| Rule | Detail | Confidence |
|---|---|---|
| Connection URL form | `ws://endpoint:port` — "End point URL and Port number will be provided by Global Datafeeds" on purchase. **No production host or port is hardcoded in ANY official artifact** | Verified |
| Historic sample endpoint | `// var wsUri = "ws://globaldatafeeds.in:8080";` (commented-out line in the official JS sample — NOT a guaranteed live endpoint) | Verified-as-sample |
| TLS | **Plaintext `ws://` only** — zero `wss://` occurrences across all 4 official SDKs, the JS sample, the 2018 client, and all captured doc snippets. The API key therefore travels in a CLEARTEXT frame. Whether a wss endpoint can be issued on request: **Unknown** (U-2 — sales question). Plan for plaintext | Verified (absence) / Unknown (availability) |
| One socket, all functions | Every function (auth, realtime, snapshot, history, instruments, greeks, limitation) multiplexes over the SAME socket, discriminated by `MessageType`. (Exception: the official SDK opens its OWN connection for `GetHistoryAfterMarket` — hints at a separate endpoint/key class, Assumed; see 08 §5) | Verified |
| Streaming API endpoint | same-vs-separate endpoint/port for StreamAllSymbols: **Unknown** (U-1) | Unknown |
| REST | parallel per-account `http://endpoint:port/` base (one SDK README also mentions `https://endpoint:port` for REST) — see 09 | Verified |

## 2. Authenticate handshake (first frame after connect)

No headers, no query params — auth is one JSON frame. **The API key travels in the `Password` field.**

Request (verbatim — CLIENT-LIB-SOURCE ws.py + GITHUB-SAMPLE):

```json
{"MessageType":"Authenticate","Password":"<API_KEY>"}
```

Success response (verbatim):

```json
{"Complete":true,"Message":"Welcome!","MessageType":"AuthenticateResult"}
```

- Failure = same `AuthenticateResult` with `"Complete":false`. The failure `Message` text is NOT captured in any artifact (**Unknown**, U-10); the official SDK's handling is simply to re-send the Authenticate frame. The server may ALSO deliver plain-text diagnostics for an expired/invalid key (§5).
- Sequencing: send data requests only AFTER `Complete:true`. The official JS sample waits 500 ms after the auth result before the first data request (`setTimeout(doTest, 500)`).

## 3. Full connection lifecycle

```
open ws://endpoint:port
  → send Authenticate            (1 frame, key as Password)
  → recv AuthenticateResult      (Complete:true → proceed; false → retry/diagnose)
  → send Subscribe*/Get* frames  (1 subscribe frame PER SYMBOL — no batch subscribe)
  ← server pushes data frames    (~1/sec/symbol for SubscribeRealtime)
  ← server pushes {"MessageType":"Echo"} every few seconds (liveness signal)
on ANY disconnect:
  reconnect → re-Authenticate → RE-SEND EVERY SUBSCRIPTION (no server-side persistence,
  no resume/sequence protocol). Re-subscribes COUNT against the request quota
  (100 symbols re-subscribed = 100 requests).
```

## 4. Session exclusivity — ONE session per key, NEW CONNECTION WINS

Docs statement (SEARCH, how-to-connect page — keep the exact direction): "WebSockets API allows only 1 active session with the server with 1 API Key. If you create another session with the same key, **the previous session will be invalidated** with the response:"

```
Access Denied. Key already in use by other session.
```

**Operational hazard (verified direction nuance):** the NEW connection wins — a rogue duplicate connect (debug shell, second host, a monitoring probe) SILENTLY KILLS the production session. There is no "second connect refused" safety. Multi-connection = multiple API keys (i.e. multiple subscriptions) — Assumed, no source mentions any multi-connection allowance. REST is sessionless (GET + accessKey) and is the parallel-pull path.

The exact wire envelope of the "Access Denied…" message (JSON vs plain text, which MessageType): **Unknown** (U-11).

## 5. Plain-text diagnostic frames (parser requirement)

"Data is sent only in JSON format; however the server sometimes sends **diagnostic messages in plain text** instead of the requested data" — e.g. expired/invalid API key, and "Reached instrument limitation" (per-key symbol cap). Confirmed message list is in 11 §3; the COMPLETE verbatim list was not capturable (**Unknown**, U-10).

**Client rule:** the frame decoder MUST tolerate non-JSON text frames without panicking — route them to an error/diagnostic channel, never the tick path.

## 6. Heartbeat — server-push `Echo`

- Server sends `{"MessageType":"Echo"}` "every few seconds to confirm connection is live" (verbatim GFDL comment, GITHUB-SAMPLE(js-2020)). No client reply is sent in any official sample — passive observation only.
- Exact cadence and full payload shape: **Unknown** (U-12). Whether the server also uses WS-protocol ping frames: **Unknown** (the Python SDK relies on `websockets` lib defaults, which auto-answer protocol pings) (U-12).
- **Client rule:** treat Echo (or any tick) as the liveness signal for a feed-stall watchdog; absence of both for N seconds during market hours = dead socket.

## 7. Frame sizes & transport constants

| Constant | Value | Source |
|---|---|---|
| SDK frame-size cap | `websockets.connect(endpoint, max_size=2**50)` — the official SDK lifts the frame cap to ~1 PiB because the server sends HUGE single text frames (a full NFO `GetInstruments` master arrives as ONE frame). Any client MUST lift default (1 MiB-class) caps | CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: ws.py) |
| Connect timeout | 60 s in the official SDK | CLIENT-LIB-SOURCE |
| Post-auth settle | 500 ms in the official JS sample before first request | GITHUB-SAMPLE(js-2020) |
| Reconnect/backoff | NO reconnect or backoff logic exists in ANY official SDK — entirely the client's job | Verified (absence) |
| Disconnect codes | No app-level disconnect/close codes documented anywhere; JS sample logs standard WS close codes only | Verified (absence) |

## 8. Maintenance windows — tickvault boot-window COLLISION

Scheduled maintenance **02:00–02:30 IST and 08:00–08:45 IST** (SEARCH, FAQ page via a 2026-07-03 third-party eval — single-source chain, Assumed; confirm with sales, U-18).

**⚠ Operational flag:** the 08:00–08:45 window overlaps tickvault's 08:30 IST instance start + 08:45 CSV-fetch/boot sequence (`daily-universe-scope-expansion-2026-05-27.md` §10). If GDF becomes a live feed, boot-time GDF connect/instrument pulls must tolerate this window (retry-not-fail until 08:45+).

## What this prevents

- Killing prod with a second connect (new-connection-wins).
- Waiting for a subscribe-ACK that never comes (first data frame IS the confirmation — no documented ACK; U-14 Unsubscribe ack likewise unknown).
- Frame-cap panics on the instrument master; JSON-parse panics on text diagnostics.
- Assuming TLS: the key is cleartext on the wire until U-2 is answered.
