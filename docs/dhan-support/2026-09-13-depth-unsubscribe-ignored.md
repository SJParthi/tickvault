<!--
=============================================================================
DHAN SUPPORT EMAIL — 2026-09-13 Full Market Depth per-instrument unsubscribe
Share the GitHub link in the Gmail reply — do NOT copy-paste plain text.
=============================================================================
-->

# What is the supported way to stop a Full Market Depth stream for ONE instrument? — New Ticket (please assign)

**To:** apihelp@dhan.co
**From:** sjparthi93@gmail.com
**Subject:** New Ticket — Full Market Depth: is RequestCode 25 implemented on the Indian feed, or is RequestCode 12 (Feed Disconnect) the only way to change a depth socket's instrument set? (three sessions of measurements inside)
**Date:** 2026-09-13 (drafted) · 2026-09-25 (sent, with the 2026-09-24 session and our current design added)

---

Hi Dhan API Support team,

This is a **clarification request, not a defect report**. We need to know the supported way to stop a Full Market Depth stream for a **single instrument** on a connection that is carrying others. Your Annexure documents a per-instrument depth unsubscribe RequestCode; your Full Market Depth guide documents none. We have measured two full trading sessions, one on each candidate code, and in both sessions a meaningful share of unsubscribed instruments kept streaming. Full numbers below.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban Subramanian |
| UCC | `NWXF17021Q` |
| Date of incident | 2026-09-10 and 2026-09-11 (both full trading sessions, IST) |
| Time of incident | Whole session on both days; the remedy described below was exhausted on all ten depth sockets within roughly the first 90 minutes of each session |

---

## What we tested / observed

We run ten Full Market Depth connections: **five 20-level sockets** (`depth-api-feed.dhan.co/twentydepth`, 50 instruments each) and **five 200-level sockets** (`full-depth-api.dhan.co`, 1 instrument each). Our instrument selection is re-ranked once a minute, so a socket routinely needs to swap one instrument out and another in. A swap is exactly two frames: one unsubscribe for the outgoing instrument, then one subscribe for the incoming one.

To measure whether the unsubscribe took effect, we time-box every dropped instrument. After we send the unsubscribe we allow a **90-second grace window**; depth packets that arrive for that instrument **during** the grace are counted as `unsubscribed_grace` (expected — in-flight data), and packets that are **still** arriving **after** the grace has elapsed are counted as `ghost`. A ghost therefore means your servers kept streaming an instrument we had asked you to stop.

We ran one full session on each of the two candidate codes:

| Session (IST) | RequestCode sent | Unsubscribes sent | Ghost packets | `unsubscribed_grace` | ghost / grace ratio |
|---|---:|---:|---:|---:|---:|
| 2026-09-10 | `25` | 7,780 | 110,114 | 55,160 | 1.996 |
| 2026-09-11 | `24` | 9,461 | 5,345,436 | 1,686,468 | 3.170 |

**URL used:**

```
wss://depth-api-feed.dhan.co/twentydepth?token=REDACTED_JWT&clientId=1106656882&authType=2
wss://full-depth-api.dhan.co/?token=REDACTED_JWT&clientId=1106656882&authType=2
```

**Request JSON — the exact unsubscribe frames we emit.**

The instrument shown below is **`NSE_EQ` / SecurityId `1333`, taken from your own Full Market Depth documentation's subscribe example**. It is reproduced here only to fix the frame *shape* beyond doubt — **it is not one of the instruments that ghosted** (see the honesty note directly beneath this section). Only the `RequestCode` differs between our subscribe and unsubscribe frames; everything else, including the string-typed `SecurityId`, is identical to the subscribe frames your servers accept without complaint.

20-level (list form, one instrument shown; we batch at most the per-message cap):

```json
{"RequestCode":25,"InstrumentCount":1,"InstrumentList":[{"ExchangeSegment":"NSE_EQ","SecurityId":"1333"}]}
```

200-level (flat form — no `InstrumentList`, no `InstrumentCount`, per your guide):

```json
{"RequestCode":25,"ExchangeSegment":"NSE_EQ","SecurityId":"1333"}
```

On 2026-09-11 the identical frames were sent with `"RequestCode":24`.

**Error / response captured:**

```
(no response of any kind)
```

This is itself part of our question. The socket does not close, no error frame arrives, no disconnect packet (feed code 50) arrives, and no acknowledgement arrives. From our side a delivered-and-ignored unsubscribe and a never-received unsubscribe are indistinguishable.

### Honesty note — why this report names no contracts

Your support process, and our own, normally requires a precise contract label and a SecurityId for every instrument cited. **We are not citing any, and we will not guess.**

- On 2026-09-10 and 2026-09-11 the log line that records a ghost did not yet carry an instrument identifier.
- We added one, and a third session on **2026-09-24** (`RequestCode 25`, 1,724 unsubscribes: 1,517 on 200-level sockets and 207 on 20-level sockets) produced **41 ghost verdicts**, each naming a SecurityId.
- When we traced those 41 back to our own send log, **most could not be matched to an unsubscribe of that instrument on the same socket.** Our selection moves an instrument between sockets within a minute, and our detector cannot attribute a late packet cleanly when that happens. Some of those verdicts may be our own misattribution, not your servers.

So this report gives you **counts, codes and frame shapes**, which are exact, and makes no per-contract claim. The clean way to settle it is the single-socket test offered at the end: one named contract, one socket, one unsubscribe, and nothing else that could move.

---

## Key observation

**Your two documentation surfaces disagree with each other, and the depth guide documents no per-instrument unsubscribe at all.**

**1. The Annexure — Feed Request Codes.** In the complete v2 documentation pulled from `docs.dhanhq.co` on 2026-09-11, the Feed Request Code table reads, for depth:

```
23 | Subscribe - Full Market Depth
25 | Unsubscribe - Full Market Depth
```

The table **skips 24**, and a search for the literal `24` as a request code across all 10,263 lines of that pull returns **zero**. Note also that depth is the one packet family that breaks the `subscribe + 1` pattern every other family follows:

| Packet family | Subscribe | Unsubscribe |
|---|---:|---:|
| Ticker | `15` | `16` |
| Quote | `17` | `18` |
| Full | `21` | `22` |
| **Full Market Depth** | **`23`** | **`25`** — not 24 |

**2. An earlier rendering of the same Annexure page carried `24`.** Our reference archive holds a 2026-06-02 snapshot and a 2026-07-14 capture of the classic Annexure page, both of which render the depth rows as `23 | Subscribe - Full Market Depth` followed by `24 | Unsubscribe - Full Market Depth`. That is why our 2026-09-11 session tested `24`. Please tell us which rendering is authoritative today.

**3. The Full Market Depth guide documents neither.** It shows only two request codes: `RequestCode 23` for subscribe, in the two payload shapes quoted above, and

```json
{ "RequestCode": 12 }
```

for **Feed Disconnect**, which closes the whole socket. It contains **no per-instrument unsubscribe example anywhere**, and its response-field tables list `Values: 23` and nothing else. If 12 is genuinely the only stop mechanism the depth feed implements, that would explain everything we measured — and it is the answer we most need confirmed or denied.

**What our numbers suggest.** With a 90-second grace, the implied mean streaming tail after an unsubscribe is `90 × (1 + ratio)` seconds. An unsubscribe that were **never** honoured would stream for the full 510-second tail we track, producing a ratio of **4.667**. Both sessions sit **below** that ceiling — 1.996 on code 25 and 3.170 on code 24 — so on the face of it **some** unsubscribes appear to take effect and some do not. We are not asserting a cause; we are asking which code, if either, your servers actually act on.

---

## What works on the same account / same minute

Rules out account, token, entitlement, host and network issues — everything else was healthy throughout both sessions.

| WebSocket / check | Endpoint | Status | Evidence |
|---|---|---|---|
| Live Market Feed | `api-feed.dhan.co` | Streaming — healthy both days | same account, same box, same token, same egress IP |
| 20-level depth | `depth-api-feed.dhan.co/twentydepth` | **Streaming — subscribes accepted; unsubscribes not observably honoured** | 5 sockets, both sessions |
| 200-level depth | `full-depth-api.dhan.co` | **Streaming — subscribes accepted; unsubscribes not observably honoured** | 5 sockets, both sessions |
| Order Update | `api-order-update.dhan.co` | Streaming — unaffected | same account, same box |
| Depth **subscribes** | both depth endpoints | Accepted — data flows for every newly subscribed instrument | this is the control: the same socket, the same frame shape, the same string-typed `SecurityId`, one byte of `RequestCode` different |
| Error 804 (instrument cap) | both depth endpoints | Not triggered in either session | no socket was parked on a subscription-limit rejection |
| Error 805 (too many connections) | both depth endpoints | Not triggered in either session | connection count unchanged throughout |

**Our side of the socket is clean, and we can prove it.** Across all **17,241** unsubscribes in the two sessions, every counter that records a failure on our side reads **zero**:

| Our-side failure mode | Metric / log field | Both sessions |
|---|---|---:|
| Payload could not be built | `tv_dhan_ws_subscribe_failed_total{reason="unsubscribe_payload"}` | 0 |
| Socket not connected at send time | `tv_dhan_ws_subscribe_failed_total{reason="unsubscribe_not_connected"}` | 0 |
| Socket write returned an error | `tv_dhan_ws_subscribe_failed_total{reason="unsubscribe_send"}` | 0 |
| Send exceeded its timeout | `tv_dhan_ws_subscribe_failed_total{reason="unsubscribe_timeout"}` | 0 |
| Swap abandoned on a wire failure | coded log `source="swap_wire_failed"` | 0 events |
| Swap left a socket empty | coded log `source="swap_emptied_socket"` | 0 events |

In every one of those 17,241 cases the payload was built, the socket was connected, the write succeeded, and it completed inside a **one-second** budget.

**What we explicitly do NOT claim:** that you received the frames. Because the API sends no acknowledgement for an unsubscribe, our evidence stops at the socket boundary — bytes written to a healthy, connected socket. We cannot distinguish "delivered and ignored" from "never received", and closing that gap is question 4 below.

---

## Verbatim log lines (for grep)

The detector emits one coded line per ghosting socket per cooldown. We are reproducing the **field set** rather than a sample line, because — as stated in the honesty note above — the field that matters most to you is the one these lines did not carry on those two dates.

```
CloudWatch log group : /tickvault/prod/app
Filter pattern       : { $.fields.source = "unsubscribe_ignored" }
Fields present       : code="WS-GAP-02", source="unsubscribe_ignored",
                       connection_index, endpoint, ghost_packets, redials_taken
Field ABSENT on 2026-09-10 and 2026-09-11 : security_id  (added 2026-09-11, ships next session)
```

**Operational cost on our side.** Because we cannot make your servers stop an instrument, our only remedy is to **re-dial the whole socket** so that your view is rebuilt from the instrument set we replay on reconnect. That is deliberately bounded at **8 redials per socket per session** so we never approach your connection limits. On both days **all ten depth sockets exhausted that ceiling within roughly the first 90 minutes**, after which approximately **78% of each session's ghost traffic arrived with no remedy left**. We estimate roughly **13% of the depth rows we stored were for contracts we had already unsubscribed** — bandwidth, on both sides, spent on data we had asked you not to send.

---

## Requests to Dhan engineering

1. **Is RequestCode 25 implemented for Full Market Depth on the Indian feed?** Your Annexure lists it; your Full Market Depth guide does not mention it at all.
2. **If not 25, what is the correct per-instrument unsubscribe RequestCode for 20-level and 200-level depth?** We have tested 25 and 24 and measured ghosts on both; please tell us the right value rather than letting us guess a third. Related: an earlier rendering of your Annexure page showed `24` where the current pull shows `25` — which is authoritative?
3. **If no per-instrument unsubscribe exists for depth, is `RequestCode 12` (Feed Disconnect) plus a fresh subscribe the only supported way to change a depth socket's instrument set?** A plain yes is a complete answer and we will redesign around it.
4. **Does the API acknowledge an unsubscribe in any form** — a response frame, a status packet, anything? We currently have no way to distinguish "delivered and ignored" from "never received", which is why this report can offer counts but not a root cause.
5. **Does a duplicate subscribe for an instrument a connection already holds count again against the per-connection instrument cap (error 804)?** This decides whether a re-subscribe is a safe fallback when an unsubscribe does not take effect.

---

**What we changed in the meantime.** From 2026-09-25 we stopped relying on the per-instrument unsubscribe. Our 20-level sockets now hold one fixed instrument set for the whole day. Our 200-level sockets change instrument by closing the socket and dialling a fresh one, at most once per socket per minute, and never more than five times a minute in total. That works, but it costs a reconnect every time, and your answer to question 3 decides whether it is the right design.

We are happy to run any diagnostic tests you need: a packet capture of the exact unsubscribe frames we send; a **single-socket controlled test** on one named contract at a time of your choosing, so you can watch one instrument on your side while we watch it on ours; and testing any alternative RequestCode you nominate, on any endpoint and in any session window you prefer. If a per-instrument unsubscribe does work, we would rather use it than keep re-dialling.

Thank you,
**Parthiban Subramanian**
Client ID: `1106656882`

---

## 2026-09-30 — Dhan's reply (received 12:56 IST, after a phone call)

Transcribed verbatim from the email (sender "Dhan Cares", signed Shubham, DhanHQ Support Team). The JSON is reproduced as sent, including the mismatch between "Request Code **11**" in the text and `12` in the example that follows it.

> Dear Parthiban,
>
> Greetings from the team at Dhan.
>
> As discussed over the call, we would like to clarify the Full Depth WebSocket feed disconnection process. If you are using Request Code 25 to unsubscribe from a feed, the request must include the same instrument details that were used during subscription. For example :
>
> ```json
> {
>     "RequestCode": 25,
>     "InstrumentCount": 1,
>     "InstrumentList": [
>         {
>             "ExchangeSegment": "NSE_EQ",
>             "SecurityId": "11536"
>         }
>     ]
> }
> ```
>
> Sending only the Request Code, as shown below, will not unsubscribe or disconnect the feed :
>
> ```json
> {
>     "RequestCode": 25
> }
> ```
>
> If your intention is to disconnect the entire Full Depth WebSocket feed directly, you can use Request Code 11 as shown below:
>
> ```json
> {
>     "RequestCode": 12
> }
> ```
>
> Request Code 12 is used to terminate the complete WebSocket connection. We will also be updating the documentation to include this information for better clarity.
>
> Please let us know if you have any further queries.
>
> Best Regards,
> Shubham
> DhanHQ Support Team

### What it answers

| Our question | Answer in the reply | Status |
|---|---|---|
| 1. Is RequestCode 25 implemented for Full Market Depth? | Yes: 25 is the per-instrument unsubscribe. | Answered |
| 2. Which code, 24 or 25? | 25. | Answered |
| 3. Is RequestCode 12 plus a fresh subscribe the only way? | No: 25 with the instrument list unsubscribes one instrument; 12 closes the whole connection. | Answered |
| 4. Is an unsubscribe acknowledged in any form? | Not addressed. | Open |
| 5. Does a duplicate subscribe count again against the 804 cap? | Not addressed. | Open |

### What it does not explain

- **Our frames already match the example.** Our 20-level unsubscribe with code 25 (the frame quoted under "Request JSON" above) has exactly the reply's shape: `RequestCode` 25, `InstrumentCount`, and an `InstrumentList` carrying the same segment and string `SecurityId` as the subscribe. Both code-25 sessions in this ticket still measured instruments arriving after it: 110,114 ghost packets on 2026-09-10, and 41 ghost verdicts on 2026-09-24, when 207 of the 1,724 unsubscribes were on 20-level sockets.
- **The 200-level shape is not shown.** Our 200-level subscribe is the flat form from the guide (`{"RequestCode":23,"ExchangeSegment":…,"SecurityId":…}`), so our unsubscribe is the flat form with 25. The reply's example is the list form. "The same instrument details that were used during subscription" reads as the flat form for 200-level, but that is our reading, not their statement.
- **"11" or "12".** The text says Request Code 11; the example and the next sentence say 12. The Annexure lists 12 as Feed Disconnect, and 12 is what we would send.

### What changes on our side

Nothing today. Since 2026-09-25 the 20-level sockets hold one fixed set for the day and the 200-level sockets change instrument by reconnecting, so neither sends a per-instrument unsubscribe. If per-instrument steering comes back, the reply supports sending 25 in the subscribe shape, which is what `build_unsubscribe_payload` already emits. The ghost counter stays in place to measure it.

### Suggested follow-up (not sent)

1. Our 20-level unsubscribe already has exactly your example's shape, and instruments still arrived after it on 2026-09-10 and 2026-09-24. Could you check the SecurityIds from our 2026-09-24 session on your side?
2. For 200-level sockets, should the unsubscribe be the flat form (as their subscribe is) or the list form in your example?
3. Is the whole-connection disconnect Request Code 12 or 11? The email says both.
4. Is an unsubscribe acknowledged in any form (our question 4), and does a duplicate subscribe count again against the 804 cap (question 5)?
