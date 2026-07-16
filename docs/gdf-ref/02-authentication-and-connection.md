# GDF 02 — Authentication & Connection (WebSockets API)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/how-to-connect-using-websockets-api/ · …/websockets-api-documentation/authenticate/ · …/faqs/faqs/ · …/documentation-support/diagnostic-api-responses/
> **Fetched:** 2026-07-13 · **Evidence tier:** SEARCH (doc-page prose) + CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: gfdlws/ws.py; ws-gfdl@1.1.0) + GITHUB-SAMPLE(js-2020) — verbatim JSON is from the official artifacts; ~~nothing is LIVE-DOC~~ **2026-07-14: the four source pages above (plus the official code-sample pages) are now machine-fetched verbatim — RUNNER-DOC tier throughout; see the reconciliation section below.**

---

## 2026-07-14 RUNNER-DOC reconciliation

The GDF doc corpus was machine-fetched verbatim on 2026-07-14 (GitHub Actions `docs-fetch.yml`, runs 29316509037 + 29317477759, branch `docs-fetch/gdf-2026-07-14` @ 7d354a8d; 213/215 pages HTTP 200). Every claim in this file was compared against the live pages. Verdict summary:

| Verdict | Claims |
|---|---|
| **CONFIRMED (tier upgraded to RUNNER-DOC)** | `ws://endpoint:port` per-account model (§1); the Authenticate request/response JSON (§2); the reconnect flow "follow same steps from 1 to 3" (§3); the one-session/new-connection-wins rule + exact "Access Denied. Key already in use by other session." literal (§4); JSON-only data + plain-text diagnostics (§5); Echo payload `{"MessageType":"Echo"}` (§6); plan-tier hourly quotas 1800/3600/7200 (§3 note) |
| **CORRECTED (dated notes in place)** | §2: re-sending the Authenticate frame on the SAME session is server-REJECTED ("Duplicated Request Not Allowed." diagnostic) — the SDK's re-send pattern is only safe on a fresh connection; §8: maintenance windows are 02:00–02:30 + **08:00–08:30** IST (BOD sometimes until 08:45), server answers "Connection refused" during them — the pack's single "08:00–08:45" window was the loose reading |
| **NEW facts added** | doc-cited test endpoint `ws://test.lisuns.com:4575` + "some test endpoints are closed over weekends and market holidays" (§1); unsolicited post-auth `AllowVMRunningResult` / `AllowServerOSRunningResult` frames + Echo-before-AuthenticateResult ordering (§3); unsubscribe ALSO consumes request quota (§3); the complete text/plain diagnostic list incl. "Key Expired." / "IP address not allowed." (§5 → 11 §3); official transcripts show Echo interleaved every ~2–3 s (§6) |
| **U-row impacts** | U-10 diagnostic list CLOSED · U-11 envelope PARTIALLY CLOSED (text/plain diagnostic; old-vs-new session delivery still probe) · U-12 payload CONFIRMED, cadence observed ~2–3 s in official transcripts (formal number still probe) · U-13 CLOSED ("Function not enabled." text/plain) · U-18 CLOSED at doc level · U-2 still open (zero `wss://` in the 215-page corpus; the FD docs-portal test server IS `https://test.lisuns.com:4532`, so TLS exists on the vendor's FD REST surface) |

Citations use `RUNNER-DOC (2026-07-14, <live URL>)` inline below.

## 1. Endpoint model — per-account host:port, NOTHING public

| Rule | Detail | Confidence |
|---|---|---|
| Connection URL form | `ws://endpoint:port` — the doc lists the three prerequisites verbatim: "'endpoint' to connect to / 'port number' to connect to / 'API Key' received from our team to access data", then "1. Make a connection to the ws://endpoint:port" | **RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/how-to-connect-using-websockets-api/)** |
| Historic sample endpoint | `// var wsUri = "ws://globaldatafeeds.in:8080";` (commented-out line in the official JS sample — NOT a guaranteed live endpoint) | Verified-as-sample |
| Doc-cited TEST endpoint (2026-07-14) | `ws://test.lisuns.com` port `4575` — the FAQ's own telnet-troubleshooting example ("telnet test.lisuns.com 4575"); also: "some test endpoints are closed over weekends and market holidays". The endpoint family is `lisuns.com`, plaintext, non-443 port — firewall/egress planning input. NOT a guaranteed prod endpoint | **RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/faqs/faqs/)** |
| TLS | **Plaintext `ws://` only** — zero `wss://` occurrences across all 4 official SDKs, the JS sample, the 2018 client, AND (2026-07-14) the full 215-page machine-fetched doc corpus. The API key therefore travels in a CLEARTEXT frame. Whether a wss endpoint can be issued on request: **Unknown** (U-2 — sales question). Nuance 2026-07-14: the vendor's Fundamental-Data docs-portal test server IS HTTPS (`https://test.lisuns.com:4532`, RUNNER-DOC, https://docs.globaldatafeeds.in/getlimitation-15575606e0) — TLS exists in their infra for the FD REST surface; no evidence for the market-data WS. Plan for plaintext | Verified (absence, corpus-wide) / Unknown (availability) |
| One socket, all functions | Every function (auth, realtime, snapshot, history, instruments, greeks, limitation) multiplexes over the SAME socket, discriminated by `MessageType`. (Exception: the official SDK opens its OWN connection for `GetHistoryAfterMarket` — hints at a separate endpoint/key class, Assumed; see 08 §5) | Verified |
| Streaming API endpoint | same-vs-separate endpoint/port for StreamAllSymbols: **Unknown** (U-1) | Unknown |
| REST | parallel per-account `http://endpoint:port/` base (one SDK README also mentions `https://endpoint:port` for REST) — see 09 | Verified |

## 2. Authenticate handshake (first frame after connect)

No headers, no query params — auth is one JSON frame. **The API key travels in the `Password` field.**

Request (verbatim — CLIENT-LIB-SOURCE ws.py + GITHUB-SAMPLE; **CONFIRMED 2026-07-14 by the official Authenticate page**, whose JS sample sends exactly `{ MessageType: "Authenticate", Password: accessKey }` — RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/authenticate/)):

```json
{"MessageType":"Authenticate","Password":"<API_KEY>"}
```

Success response (verbatim; **CONFIRMED 2026-07-14** — the Authenticate page prints the identical line, with a stray trailing `"` doc-site typo: `{"Complete":true,"Message":"Welcome!","MessageType":"AuthenticateResult"}"` — RUNNER-DOC, same URL. The page states "What is returned? Nothing. This function authenticates the user and sends response accordingly"):

```json
{"Complete":true,"Message":"Welcome!","MessageType":"AuthenticateResult"}
```

- Failure = same `AuthenticateResult` with `"Complete":false`. ~~The failure `Message` text is NOT captured in any artifact (**Unknown**, U-10)~~ **2026-07-14 update:** the failure WORDINGS are now captured — key problems surface as text/plain diagnostics: `Access Denied. Key not found.` / `Access Denied. Key blocked.` / `Access Denied. Key unknown or empty.` / `Key Expired.` / `Access Denied. Key Invalid.` (full table: 11 §3; RUNNER-DOC, 2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/documentation-support/diagnostic-api-responses/). Whether a JSON `AuthenticateResult{Complete:false}` ALSO carries one of these strings in `Message` remains unproven (probe residual of U-10).
- **⚠ CORRECTED 2026-07-14 — do NOT re-send Authenticate on the same session.** The diagnostic list contains `Duplicated Request Not Allowed.` = "The same authentication request is sent more than once on the same session, which the server doesn't allow" (RUNNER-DOC, diagnostic-api-responses URL above). The official SDK's re-send-Authenticate retry pattern (noted 2026-07-13) is therefore only safe on a FRESH connection; a Rust client must reconnect-then-authenticate, never re-authenticate in place.
- Sequencing: send data requests only AFTER `Complete:true`. The official JS sample waits 500 ms after the auth result before the first data request (`setTimeout(doTest, 500)`). The doc flow is explicit: "2. Send Authentication Request using API Key / 3. Once authentication is successful, send all other data requests" (RUNNER-DOC, how-to-connect URL).

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

**2026-07-14 RUNNER-DOC confirmations + additions:**

- Reconnect flow CONFIRMED verbatim: "4. If connection is lost for any reasons, follow same steps from 1 to 3 as above" (RUNNER-DOC, 2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/how-to-connect-using-websockets-api/). The vendor's FD-portal WS page spells the same pattern out even more explicitly — "Step 5: Handle Disconnections … Establish a new WebSocket connection. Send the authentication request. Wait for successful authentication. **Resubscribe or resend the required data requests**" (RUNNER-DOC, 2026-07-14, https://docs.globaldatafeeds.in/how-to-connect-using-websocket-api-2181778m0 — the Fundamental-Data WS, same connection model).
- **Quota consumption refined (FAQ, verbatim example):** "if your Symbol limit is 100 and your Request per hour limit is 1800, subscribing to 100 symbols … will consume 100 requests. If you then unsubscribe from these 100 symbols and subscribe to a different set of 100 symbols, total 300 requests i.e. 100 (original) + 100 (unsubscription) + 100 (new subscription)" — **UNSUBSCRIBE frames also consume quota**, not just subscribes (RUNNER-DOC, 2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/faqs/faqs/).
- **NEW unsolicited post-auth frames (decoder requirement):** the official NodeJS/Python sample transcripts show the server pushing `{"AllowVMRunning":false,"MessageType":"AllowVMRunningResult"}` and `{"AllowServerOSRunning":false,"MessageType":"AllowServerOSRunningResult"}` right after connect/auth, unrequested (RUNNER-DOC, 2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/api-code-samples/websockets-nodejs/ + …/api-code-samples/websockets-python/). Meaning is undocumented (VM/server-OS licensing flags — Assumed from the vendor's VM product line); the client must tolerate-and-route unknown `MessageType`s, never panic.
- **Ordering quirk:** the official NodeJS transcript shows an `Echo` arriving BEFORE the `AuthenticateResult` — Echo is not gated on auth completion (RUNNER-DOC, same NodeJS URL).

## 4. Session exclusivity — ONE session per key, NEW CONNECTION WINS

Docs statement (~~SEARCH~~ **RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/how-to-connect-using-websockets-api/)** — verbatim from the live page's Important Notes): "WebSockets API allows only 1 active session with the server with 1 API Key. If you create another session with same key, previous session will be invalidated with response 'Access Denied. Key already in use by other session.'"

```
Access Denied. Key already in use by other session.
```

**Operational hazard (verified direction nuance):** the NEW connection wins — a rogue duplicate connect (debug shell, second host, a monitoring probe) SILENTLY KILLS the production session. There is no "second connect refused" safety. Multi-connection = multiple API keys (i.e. multiple subscriptions) — Assumed, no source mentions any multi-connection allowance. REST is sessionless (GET + accessKey) and is the parallel-pull path.

The exact wire envelope of the "Access Denied…" message: **PARTIALLY CLOSED 2026-07-14** — the literal appears in the official text/plain diagnostic table ("Key is used in more than 1 session. Please ensure that key creates only 1 session. If any new sessions are created, previous sessions are invalidated with this message" — RUNNER-DOC, 2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/documentation-support/diagnostic-api-responses/), i.e. it is a **plain-text diagnostic frame, not JSON**. Which socket receives it (the invalidated OLD session per the table wording, the refused NEW one, or both): still a probe (U-11 residual — two connects with one trial key).

## 5. Plain-text diagnostic frames (parser requirement)

"Data is sent only in JSON format; however the server sometimes sends **diagnostic messages in plain text** instead of the requested data" — CONFIRMED verbatim 2026-07-14: "WebSockets API Sends data only in JSON format … Sometimes, API sends diagnostic messages in plain/text – instead of actual data requested. For example, when API Key is expired / invalid, etc.. Your application should be able to handle these messages in non-JSON format" (RUNNER-DOC, 2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/how-to-connect-using-websockets-api/). ~~The COMPLETE verbatim list was not capturable (**Unknown**, U-10)~~ **U-10 CLOSED 2026-07-14:** the complete official diagnostic table (22 messages) is now captured verbatim in 11 §3 (RUNNER-DOC, …/documentation-support/diagnostic-api-responses/).

**Client rule:** the frame decoder MUST tolerate non-JSON text frames without panicking — route them to an error/diagnostic channel, never the tick path.

## 6. Heartbeat — server-push `Echo`

- Server sends `{"MessageType":"Echo"}` "every few seconds to confirm connection is live" (verbatim GFDL comment, GITHUB-SAMPLE(js-2020)). No client reply is sent in any official sample — passive observation only.
- **Payload shape CONFIRMED 2026-07-14:** exactly `{"MessageType":"Echo"}` — no other fields — appears repeatedly in the official Python/NodeJS/Java sample transcripts on the live site (RUNNER-DOC, 2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/api-code-samples/websockets-python/ + …/websockets-nodejs/ + …/websockets-java/). U-12 payload half CLOSED.
- **Cadence observed (not formally documented):** in the two timestamped official transcripts (MCX `ServerTime` 1594040013→1594040018; NFO 1594112004→1594112011) an Echo lands roughly every 2 data frames at the 1/sec push rate ⇒ **~every 2–3 seconds** in those captures. The formal number + whether WS-protocol ping frames are also used: still probe (U-12 residual).
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

~~Scheduled maintenance **02:00–02:30 IST and 08:00–08:45 IST** (SEARCH, FAQ page via a 2026-07-03 third-party eval — single-source chain, Assumed; confirm with sales, U-18).~~

**U-18 CLOSED at doc level 2026-07-14 (RUNNER-DOC, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/faqs/faqs/) — the FAQ states it verbatim:**

> "Due to server Maintenance activity the server throw you the message as 'Connection refused' / 02:00:00 To 02:30:00 AM (MidNight) / 08:00:00 To 08:30:00 AM (Morning) / Some times morning activity (BOD Process) till 08:45:00 AM. / if you are connecting and setting up any tasks/cron jobs, etc., please connect after 8:45AM"

| Window (IST) | Nature | Symptom |
|---|---|---|
| 02:00–02:30 | midnight maintenance | connection attempts get "Connection refused" |
| 08:00–08:30 | morning maintenance | same |
| …sometimes to 08:45 | BOD (begin-of-day) process overrun | same; **vendor's own advice: schedule connects/cron jobs AFTER 08:45 IST** |

(The pack's earlier single "08:00–08:45" window was the loose reading of the same FAQ — the doc splits it into a hard 08:00–08:30 window + an occasional BOD tail to 08:45. Whether the WS FEED of an already-connected session degrades during the windows, vs only new connects being refused: still a probe residual.)

**⚠ Operational flag (unchanged):** the 08:00–08:45 worst case overlaps tickvault's 08:30 IST instance start + 08:45 boot sequence (`daily-universe-scope-expansion-2026-05-27.md` §10). If GDF becomes a live feed, boot-time GDF connect/instrument pulls must tolerate this window (retry-not-fail until 08:45+) — the vendor's own connect-after-08:45 advice makes this mandatory, not defensive.

## What this prevents

- Killing prod with a second connect (new-connection-wins).
- Waiting for a subscribe-ACK that never comes (first data frame IS the confirmation — no documented ACK; U-14 Unsubscribe ack likewise unknown).
- Frame-cap panics on the instrument master; JSON-parse panics on text diagnostics.
- Assuming TLS: the key is cleartext on the wire until U-2 is answered.
