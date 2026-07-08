<!--
=============================================================================
DHAN SUPPORT EMAIL — 2026-07-08 order-update RST-after-login, Jul-6 token
invalidation, main-feed delivery lag, recurring main-feed TCP resets
Share the GitHub link in the Gmail reply — do NOT copy-paste plain text.
=============================================================================
-->

# Order-update WS TCP-reset ~10ms after accepted login, server-side token invalidation, and main-feed delivery lag — New Ticket (please assign)

**To:** apihelp@dhan.co
**From:** sjparthi93@gmail.com
**Subject:** New Ticket — Jul-6 server-side token invalidation + order-update WS RST-after-login (no Close frame) + main-feed delivery lag up to 198s + recurring api-feed TCP RSTs (Jul 8, continuing Jul-2 ticket)
**Date:** 2026-07-08

---

Hi Dhan API Support team,

We are reporting four related production incidents from 2026-07-06 and 2026-07-08 with full client-side evidence: a server-side token invalidation with no second mint from our side, order-update WebSocket TCP resets ~10ms after an accepted MsgCode-42 login, main-feed delivery latency reaching 46s+ at p99, and a recurrence of the bare-TCP-reset pattern we reported on 2026-07-02.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban Subramanian |
| UCC | `NWXF17021Q` |
| Date of incident | 2026-07-06 IST and 2026-07-08 IST |
| Time of incident | 10:20–10:35, 14:05:49 onwards, all-day (Jul 6); 13:50–14:10 (Jul 8) — all IST |

---

## What we tested / observed

This is a connection-level and account-level report — no specific option/future contracts are involved, so no contract labels apply. Our setup on both days: ONE Live Market Feed connection to `wss://api-feed.dhan.co` carrying a 776-instrument Quote-mode subscription (batches of at most 100 instruments per subscribe message, well under the 5,000-per-connection cap), plus ONE Order Update connection to `wss://api-order-update.dhan.co` authenticated via the MsgCode-42 JSON login. Four distinct incidents:

### Incident 1 — access token invalidated server-side with NO second mint from our side (2026-07-06)

Our access token was minted at `2026-07-06 10:05:44 IST` via `generateAccessToken` after boot. It was working at `10:20:49 IST` and invalid by `10:35:49 IST`. **Our box performed NO second mint in that window** — verified from our complete boot + auth logs (every mint is logged; there is exactly one for the day). From 10:35 IST, `GET /v2/profile` returned the following every 15 minutes for 4+ hours:

```json
{"errorType":"Order_Error","errorCode":"DH-906","errorMessage":"Invalid Token"}
```

We understand Dhan enforces one active token at a time (a new generation invalidates the old). But we generated no new token — so something invalidated it server-side between 10:20:49 and 10:35:49 IST.

### Incident 2 — order-update WS TCP-reset ~10ms after accepted login (2026-07-06, from 14:05:49 IST)

Every reconnect of the Order Update WebSocket completed the WebSocket upgrade AND the MsgCode-42 login send, and was then TCP-reset by your side approximately 10ms after login. Verbatim client error:

```
WebSocket protocol error: Connection reset without closing handshake
```

39+ consecutive attempts at ~1/minute, all with the identical signature, while the token was the invalid one from Incident 1. An auth rejection is expected with an invalid token — but a WebSocket Close frame with a reason code would let clients distinguish auth-reject from network failure. A bare RST without a closing handshake is indistinguishable from a transport fault, so clients cannot know they should re-mint instead of retrying.

### Incident 3 — main-feed delivery latency up to 198s (2026-07-06, all trading day)

Delivery latency measured as exchange timestamp (LTT) to our receive time, over stored ticks in 10-minute windows, across the 776-instrument Quote-mode subscription on ONE connection:

| Percentile | Latency |
|---|---|
| p50 | 1.38 s |
| p90 | 8.50 s |
| p95 | 14.93 s |
| p99 | 46.37 s |
| max | 198.69 s |

Simultaneously, 29–67 instruments per minute had tick gaps of 300–978 seconds (590 gap events logged). Our comparison feed from another vendor on the same box, same minutes, showed p99 = 562 ms. The Dhan LTT is whole seconds, so quantization explains at most ~1s of the measurement — not 46s.

### Incident 4 — recurring main-feed bare TCP RSTs (2026-07-08, 13:50–14:10 IST)

Seven disconnect/reconnect cycles on `wss://api-feed.dhan.co` between 13:55 and 14:06 IST, plus one full outage 14:03–14:04 IST (~2 minutes, all connections down). Each reconnect succeeded and held. Same "Connection reset without closing handshake" signature — this continues the pattern in our 2026-07-02 ticket ("Main-feed WebSocket killed by server-side TCP reset without a code-50 disconnect packet", `docs/dhan-support/2026-07-02-mainfeed-tcp-resets.md` — please link it to the same investigation).

**URL used:**

```
wss://api-order-update.dhan.co
wss://api-feed.dhan.co?version=2&token=REDACTED_JWT&clientId=1106656882&authType=2
```

**Request JSON (order-update login, MsgCode 42):**

```json
{"LoginReq": {"MsgCode": 42, "ClientId": "1106656882", "Token": "REDACTED_JWT"}, "UserType": "SELF"}
```

**Error / response captured:**

```
WebSocket protocol error: Connection reset without closing handshake
```

```json
{"errorType":"Order_Error","errorCode":"DH-906","errorMessage":"Invalid Token"}
```

---

## Key observation

Three of the four incidents point at the server side, not our client: (1) the token died between 10:20:49 and 10:35:49 IST on Jul 6 with **zero** token-generation calls from our box in that window — under the one-active-token rule, something on your side (or a second generation you can see in your logs that we cannot) invalidated it; (2) the order-update endpoint ACCEPTS the WebSocket upgrade and the login message, then resets the TCP connection ~10ms later with no Close frame — an application-level rejection delivered as a transport-level failure; (3) the same 776-instrument subscription that lagged 8–46s+ on Dhan was delivered at p99 562ms by an independent vendor feed on the same host, same minutes, ruling out our host, network path, and processing pipeline.

| Timestamp (IST) | Event | Value |
|---|---|---|
| `2026-07-06 10:05:44` | token minted via generateAccessToken (the ONLY mint of the day) | token valid |
| `2026-07-06 10:20:49` | last confirmed-good token use | token still valid |
| `2026-07-06 10:35:49` | `GET /v2/profile` → DH-906 "Invalid Token" | token invalidated server-side in the 10:20–10:35 window |
| `2026-07-06 10:35 → 14:35+` | DH-906 on every 15-min profile check | 4+ hours, identical response |
| `2026-07-06 14:05:49` | first order-update RST ~10ms after accepted MsgCode-42 login | "Connection reset without closing handshake" |
| `2026-07-06 14:05 → 14:44+` | 39+ consecutive identical order-update RST-after-login cycles | ~1 attempt/minute, all identical |
| `2026-07-06 all day` | main-feed delivery lag (LTT → receive), 10-min windows | p50 1.38s / p90 8.50s / p95 14.93s / p99 46.37s / max 198.69s |
| `2026-07-06 all day` | per-minute tick gaps of 300–978s | 29–67 instruments/minute affected; 590 gap events logged |
| `2026-07-08 13:55 → 14:06` | 7 main-feed disconnect/reconnect cycles, bare RST, no code-50 | each reconnect succeeded and held |
| `2026-07-08 14:03 → 14:04` | full main-feed outage, all connections | ~2 minutes |

---

## What works on the same account / same minute

Rules out network / IP / firewall / account issues on our side.

| WebSocket / check | Endpoint | Status | Evidence |
|---|---|---|---|
| Auth token mint | `auth.dhan.co generateAccessToken` | Works — Jul-6 10:05:44 IST mint succeeded | boot auth logs |
| Live Market Feed | `api-feed.dhan.co` | Connects and streams (but with the Incident-3 delivery lag; Incident-4 RSTs on Jul 8) | 776-instrument Quote subscription streaming |
| Order Update | `api-order-update.dhan.co` | Login ACCEPTED, then TCP-reset ~10ms later while the token was invalid | 39+ identical cycles from 14:05:49 IST Jul 6 |
| REST | `GET /v2/profile` | Reachable — returns the DH-906 body promptly every 15 min | rules out network/IP/firewall on our side |
| Static IP | `GET /v2/ip/getIP` | Registered, `ordersAllowed=true` | static-IP gate check |
| Comparison feed (different vendor) | independent provider socket, same box | Streaming at p99 562ms same minutes as Incident 3 | rules out our host, NIC, and network path |

---

## Verbatim log lines (for grep)

```json
{"timestamp":"2026-07-06T10:35:49+05:30","level":"ERROR","message":"profile check failed: {\"errorType\":\"Order_Error\",\"errorCode\":\"DH-906\",\"errorMessage\":\"Invalid Token\"}","code":"DH-906","endpoint":"/v2/profile"}
{"timestamp":"2026-07-06T14:05:49+05:30","level":"ERROR","message":"WebSocket protocol error: Connection reset without closing handshake","ws_type":"order_update","phase":"~10ms after MsgCode-42 login accepted","consecutive_failures":1}
{"timestamp":"2026-07-06T14:44:00+05:30","level":"ERROR","message":"WebSocket protocol error: Connection reset without closing handshake","ws_type":"order_update","phase":"~10ms after MsgCode-42 login accepted","consecutive_failures":39}
{"timestamp":"2026-07-08T14:03:00+05:30","level":"ERROR","message":"WebSocket read error: Connection reset without closing handshake","ws_type":"live_feed","disconnect_code":null,"note":"full outage 14:03-14:04 IST, all connections; 7 disconnect/reconnect cycles 13:55-14:06 IST"}
```

(Source: CloudWatch log group /tickvault/prod/app. The Incident-3 latency percentiles are computed from our stored tick database — exchange LTT vs receive timestamp — over 10-minute windows for the full Jul-6 trading day; we can export the full per-window table on request.)

---

## Requests to Dhan engineering

1. **What invalidated our access token server-side between 10:20:49 and 10:35:49 IST on 2026-07-06?** Our box performed NO second `generateAccessToken` call (verified from our logs — exactly one mint at 10:05:44 IST). Under the one-active-token rule, was there a second token generation on client ID `1106656882` in that window? Please check your token-generation logs for 10:20–10:35 IST on Jul 6.
2. **Why does `api-order-update.dhan.co` TCP-reset ~10ms after an accepted MsgCode-42 login instead of sending a WebSocket Close frame with a reason code on auth rejection?** RST-without-close is indistinguishable from a network failure, so clients cannot tell "re-mint the token" from "retry the connection".
3. **Is the 8–46s+ (max 198.69s) main-feed delivery lag on 2026-07-06 visible in your side's metrics — was there a known degradation that day?** Our independent comparison feed on the same host delivered the same minutes at p99 562ms, so the lag is upstream of our box.
4. **Are the recurring bare TCP RSTs on `api-feed.dhan.co` (our 2026-07-02 ticket; again on 2026-07-08 13:55–14:06 IST including a ~2-minute full outage 14:03–14:04) server-side connection recycling, and what cadence should clients expect?** Please correlate with your load balancer / feed-server logs for clientId `1106656882` at those windows.
5. **What is the recommended client behavior to distinguish an auth-reject from a transient reset on both WebSocket endpoints?** If a Close frame with a reason code cannot be added, is there any other signal (HTTP status on the upgrade, a documented pre-reset packet) we should key on?

---

We are happy to run any diagnostic tests you need — tcpdump captures on our side with exact microsecond timestamps, a reproduction from a secondary IP, alternate connection counts, or a specific reproduction window during market hours. The Jul-6 token invalidation blinded our order-update stream for the rest of the session and the delivery lag degrades our live market data capture, so we need to move forward.

Thank you,
**Parthiban Subramanian**
Client ID: `1106656882`
