<!--
=============================================================================
DHAN SUPPORT EMAIL — 2026-07-02 main-feed bare TCP resets (no code-50)
Share the GitHub link in the Gmail reply — do NOT copy-paste plain text.
=============================================================================
-->

# Main-feed WebSocket killed by server-side TCP reset without a code-50 disconnect packet — New Ticket (please assign)

**To:** apihelp@dhan.co
**From:** sjparthi93@gmail.com
**Subject:** New Ticket — api-feed.dhan.co TCP-resets a healthy mid-market main-feed socket without any code-50 disconnect packet (Jul 2 + Jun 30 evidence)
**Date:** 2026-07-02

---

Hi Dhan API Support team,

We are reporting server-side TCP resets on the Live Market Feed WebSocket that arrive without any documented disconnect code, observed today (2026-07-02) and previously on 2026-06-30.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban Subramanian |
| UCC | `NWXF17021Q` |
| Date of incident | 2026-07-02 IST (history: 2026-06-30 IST) |
| Time of incident | 13:31:28 IST and 13:36:35 IST (2026-07-02) |

---

## What we tested / observed

One Live Market Feed connection to `wss://api-feed.dhan.co` carrying 775 subscribed instruments (well under the 5,000-per-connection cap), subscribed in Quote mode in batches of at most 100 instruments per subscribe message. The socket had been healthy and streaming continuously for 16,288 seconds (~4.5 hours) mid-market when the server killed it with a bare TCP reset — "Connection reset without closing handshake". No Disconnect packet (Feed Response Code 50) was ever received on either event, so none of the 12 documented disconnect reason codes applies.

| Connection / scope | Instruments on connection | Timestamp (IST) | Outcome |
|---|---|---|---|
| Main feed conn #0 (775 instruments, Quote mode) | 775 | `2026-07-02 13:31:28.000000` | server TCP reset after 16,288s healthy uptime; NO code-50 packet; reconnected in 7s (2 attempts; first retry failed with "Handshake not finished") |
| Main feed conn #0 (same 775 instruments, fresh socket) | 775 | `2026-07-02 13:36:35.000000` | the FRESH socket TCP-reset again at exactly 300s uptime; NO code-50 packet; reconnected in 11s (3 attempts) |
| Main feed conn #0 (history) | 775 | `2026-06-30 10:10:00.000000` to `2026-06-30 11:10:00.000000` | same bare-RST signature repeating at ~315 resets/hour |

**URL used:**

```
wss://api-feed.dhan.co?version=2&token=REDACTED_JWT&clientId=1106656882&authType=2
```

**Request JSON:**

```json
{"RequestCode": 17, "InstrumentCount": 100, "InstrumentList": [{"ExchangeSegment": "IDX_I", "SecurityId": "13"}, {"ExchangeSegment": "IDX_I", "SecurityId": "25"}, {"ExchangeSegment": "IDX_I", "SecurityId": "51"}, "... 775 instruments total, sent in batches of <=100 per message ..."]}
```

**Error / response captured:**

```
Connection reset without closing handshake
```

No Feed Response Code 50 (Disconnect packet, 10 bytes) preceded either reset — the TCP connection was reset at the transport layer with zero application-level notice.

---

## Key observation

The socket was NOT idle, NOT stale, and NOT over any documented limit: it had streamed continuously for ~4.5 hours mid-market before the 13:31:28 IST reset. The replacement socket then reset again at exactly 300 seconds of uptime (13:36:35 IST), which suggests a server-side per-connection timer or cycling policy of ~5 minutes rather than a fault on our side. On 2026-06-30 between roughly 10:10 and 11:10 IST the identical bare-RST signature repeated at ~315 resets per hour.

| Timestamp (IST) | Event | Value |
|---|---|---|
| `13:31:28.000` | server TCP reset (no code-50) | socket uptime 16,288s (~4.5h) |
| `13:31:35.000` | reconnected + re-subscribed | 7s downtime, 2 attempts (first retry: "Handshake not finished") |
| `13:36:35.000` | second server TCP reset (no code-50) | fresh socket uptime exactly 300s |
| `13:36:46.000` | reconnected + re-subscribed | 11s downtime, 3 attempts |
| `2026-06-30 10:10-11:10` | same bare-RST signature (history) | ~315 resets/hour |

Our client side is verified compliant with the documented protocol: no manual ping frames (server ping/pong handled by the WebSocket library), code-50 Disconnect handling implemented for all 12 documented reason codes, subscribe batching at most 100 instruments per message, and 775 instruments on 1 connection (under the 5,000 cap). Token was healthy with ~23h validity remaining and zero token re-mints all day; our rate-limit tracking shows `rate_limit_streak=0` (this was NOT an HTTP 429 / DATA-805 event).

---

## What works on the same account / same minute

Rules out account / token / host / network issues — everything else on the same box was healthy at both timestamps.

| WebSocket / check | Endpoint | Status | Evidence |
|---|---|---|---|
| Live Market Feed | `api-feed.dhan.co` | Failing (bare TCP resets, no code-50) | 13:31:28 + 13:36:35 IST resets per verbatim logs below |
| Order Update | `api-order-update.dhan.co` | Streaming — 0 disconnects all day | same account, same box, same token, same egress IP |
| Second market-data feed (different broker) on the same host | independent provider socket | Streaming — unaffected at both timestamps | rules out our host, NIC, and network path |
| Auth token | 24h JWT | Healthy — ~23h validity, zero re-mints | token telemetry on the same box |
| EC2 host (ap-south-1) | instance + system status | ok/ok at both timestamps | AWS status checks |
| Rate limit | HTTP 429 / DATA-805 | Not triggered — `rate_limit_streak=0` | connection health telemetry |

---

## Verbatim log lines (for grep)

```json
{"timestamp":"2026-07-02T13:31:28+05:30","level":"ERROR","message":"WebSocket read error: Connection reset without closing handshake","connection_id":0,"ws_type":"live_feed","uptime_secs":16288,"disconnect_code":null}
{"timestamp":"2026-07-02T13:31:35+05:30","level":"INFO","message":"WebSocket reconnected and re-subscribed","connection_id":0,"attempts":2,"first_retry_error":"Handshake not finished","downtime_secs":7}
{"timestamp":"2026-07-02T13:36:35+05:30","level":"ERROR","message":"WebSocket read error: Connection reset without closing handshake","connection_id":0,"ws_type":"live_feed","uptime_secs":300,"disconnect_code":null}
{"timestamp":"2026-07-02T13:36:46+05:30","level":"INFO","message":"WebSocket reconnected and re-subscribed","connection_id":0,"attempts":3,"downtime_secs":11}
```

(Source: CloudWatch log group /tickvault/prod/app; 2026-06-30 forensics show the same "Connection reset without closing handshake" signature at ~315 occurrences/hour between 10:10 and 11:10 IST.)

---

## Requests to Dhan engineering

1. **Why does `api-feed.dhan.co` TCP-reset a healthy mid-market socket without sending a code-50 Disconnect packet?** Please check the server-side logs for clientId `1106656882` at 2026-07-02 13:31:28 IST and 13:36:35 IST.
2. **Is there a server-side per-connection lifetime or idle policy we should know about?** The socket had 16,288s of healthy uptime when it was reset; no such limit is documented.
3. **Is a ~5-minute server-side connection cycle expected?** The fresh replacement socket was reset at exactly 300s uptime — please confirm whether a 300s policy exists on your load balancers / feed servers.
4. **What diagnostics can we provide to help you correlate?** We can run tcpdump on our side, reproduce with different SecurityIds, and supply microsecond-precision IST timestamps for every event on request.

---

We are happy to run any diagnostic tests you need — tcpdump captures, different SecurityIds, alternate connection counts, a secondary IP, or a specific reproduction window during market hours. These resets interrupt our live market data capture mid-session and we need to move forward.

Thank you,
**Parthiban Subramanian**
Client ID: `1106656882`
