# Live Market Feed returns HTTP 429 from cloud (AWS) IPs while same token works from a residential IP

**To:** apihelp@dhan.co
**From:** Parthiban
**Subject:** Live Market Feed (`api-feed.dhan.co`) — HTTP 429 from AWS IP, same token streams fine from home IP — please confirm connection rate-limit + cooldown
**Date:** 2026-06-01

---

Hi DhanHQ team,

We run a single-account market-data capture system. Today the **Live Market Feed** WebSocket repeatedly returns **HTTP 429 (Too Many Requests)** when we connect from our **AWS server**, while the **exact same access token** streams perfectly from a **home/residential IP** minutes earlier, and our **Order Update** WebSocket connects fine from the *same* AWS server at the same time. We'd like to understand the feed's connection rate-limit so we can configure our reconnect correctly.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban |
| UCC | `NWXF17021Q` |
| Date of incident | 2026-06-01 (IST) |
| Time of incident | ~11:38–12:20 IST |

---

## What we tested / observed

Single Live Market Feed connection subscribing **331 instruments in Quote mode** (NSE indices + NSE_EQ F&O underlyings). The app boots, authenticates (JWT acquired OK), opens **one** connection to `wss://api-feed.dhan.co`, and sends the subscribe batches. From the **AWS server** the connection is refused with **HTTP 429** (and at times `Connection reset without closing handshake`). From a **home/residential IP**, the identical build + identical token connects and streams all 331 instruments immediately.

| Source IP (AWS, ap-south-1) | Timestamp (IST) | Outcome |
|---|---|---|
| `13.205.212.203` | 2026-06-01 ~12:11 | `429 Too Many Requests` / reset |
| `13.234.39.4` | 2026-06-01 ~12:13 | `429 Too Many Requests` / reset |
| `65.1.58.124` | 2026-06-01 ~12:18 | `429 Too Many Requests` / reset |
| Home/residential IP | 2026-06-01 ~11:38 | **Streaming OK — 331 instruments, ticks flowing** |

**URL used:**

```
wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=1106656882&authType=2
```

**Error captured (verbatim):**

```
WebSocket connection failed to wss://api-feed.dhan.co: HTTP error: 429 Too Many Requests
WebSocket protocol error: Connection reset without closing handshake
```

---

## Key observation

We rotated the AWS server's public IP **three times** (three different fresh Elastic IPs, listed above) — **all three got 429**. This tells us the limit is **not per-IP**. Combined with the home IP working on the same token, and the Order Update socket working from the same AWS server, we believe we tripped an **account-level (per `dhanClientId`) connection rate-limit** on the feed — likely because our reconnect logic retried too aggressively after the first failure. We have since changed our client to back off 60s→5m after a 429 and to stop auto-restarting during cooldown.

---

## What works on the same account / same minute

Rules out token / account / network issues:

| WebSocket | Endpoint | Status from AWS server | Evidence |
|---|---|---|---|
| Live Market Feed | `api-feed.dhan.co` | **Failing — HTTP 429** | logs below |
| Order Update | `api-order-update.dhan.co` | **Connecting OK** | "order update WebSocket connected", same token, same IP, same minute |
| Live Market Feed (home IP) | `api-feed.dhan.co` | **Streaming OK** | 331 instruments, ticks flowing, ~11:38 IST |

Auth itself is healthy: "✅ Auth OK — Dhan JWT acquired" fires on every boot, and `/v2/profile` returns `dataPlan: Active`, `activeSegment` includes Derivative.

---

## Verbatim log lines (for grep)

```json
{"level":"INFO","fields":{"message":"WebSocket connected and subscribed","connection_id":0,"instruments":331},"target":"tickvault_core::websocket::connection"}
{"level":"HIGH","message":"WebSocket 1/1 disconnected — connect failed: WebSocket connection failed to wss://api-feed.dhan.co: HTTP error: 429 Too Many Requests"}
{"level":"LOW","message":"OrderUpdateReconnected — Order Update WS reconnected (same token, same AWS IP)"}
```

---

## Requests to Dhan engineering

1. **What is the Live Market Feed connection rate-limit?** Is there a cap on **new connection attempts per minute** per `dhanClientId` (separate from the 5-connections / 5000-instruments caps), and what is the exact number?
2. **How long does the feed `429` cooldown last** once we stop attempting? (So we can size our backoff — we currently use 60s→5m.)
3. **Is the `429` limit per `dhanClientId` or per source IP?** Our 3-fresh-IP test suggests per-account — please confirm.
4. **Does the feed treat cloud/datacenter source IPs (AWS ap-south-1) differently** from residential IPs in any way (reputation, throttling)? We want to rule this out.
5. **Can engineering check account `1106656882` for any feed connection block/penalty currently in effect**, and confirm when it clears?

---

We are happy to run any diagnostic tests you need — different SecurityIds, different times, a tcpdump from the AWS server, or a controlled single-connection test at a time you specify. We have already hardened our client to back off on `429`; we just need to confirm the limit + cooldown so we don't trip it again.

Thank you,
**Parthiban**
Client ID: `1106656882`
