<!--
=============================================================================
DHAN SUPPORT EMAIL — 2026-11-13 live market feed delivery delay and the AWS
region / availability zone of the feed and order servers
Drafted by Claude Code on the owner's request (decision card 2026-11-13,
"Yes, draft it (Recommended)"). The owner sends it; no session contacts Dhan.
Share the GitHub link in the Gmail reply — do NOT copy-paste plain text.
=============================================================================
-->

> **NOT READY TO SEND — DRAFT.** The "Packet-level samples" section and our zone ID below still hold template placeholders. Fill it from real database rows (operator instructions at the end of this file, visible in the raw file), confirm `grep -n '<[A-Z_]'` finds nothing, then delete this note before sharing the link.

# Live Market Feed delivery delay (exchange to our socket) and the AWS region / zone of the feed and order servers — New Ticket (please assign)

**To:** apihelp@dhan.co
**From:** sjparthi93@gmail.com
**Subject:** New Ticket — Live Market Feed delivery delay (median 0.75 s, p99 1.7 s, exchange to socket) + which AWS region / zone serves api-feed.dhan.co and api.dhan.co + any lower-latency feed option
**Date:** 2026-11-13

---

Hi Dhan API Support team,

We measure the delivery delay of the Live Market Feed on every trading day and almost all of it happens before the data reaches our server. We would like to understand where that time goes, where your feed and order servers run, and whether a faster option exists.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban Subramanian |
| UCC | `NWXF17021Q` |
| Date of incident | Every trading session (ongoing; latest figures as of 2026-11-13 IST) |
| Time of incident | Measured continuously, 09:15–15:30 IST |

---

## What we tested / observed

This is a connection-level question, not a fault with any one contract; the packet-level samples further down name exact contracts, SecurityIds and microsecond times so you can trace them. Our setup:

- Our server runs in AWS **ap-south-1 (Mumbai)**, on one r8g.large instance in availability zone ID **`aps1-az3` (zone ap-south-1b in our account)** (we quote the zone ID, for example `aps1-az1`, because zone letters such as `ap-south-1b` map to different physical zones in each AWS account).
- The server clock is kept in sync with the AWS time service.
- We connect to the Live Market Feed at `wss://api-feed.dhan.co` (version 2) and subscribe in batches of at most 100 instruments per message, within the 5,000-per-connection limit.
- For every packet we record the moment it arrives on our socket, before any parsing.

What we measure, split by where the time is spent:

| Stage | Measured delay | Share of total |
|---|---|---|
| Exchange trade time to arrival on our socket (your side plus the network) | median **0.75 s**, p99 **1.7 s** | about 99.5% |
| Network round trip from our server to `api-feed.dhan.co` | about **3.5 ms** | small part of the line above |
| Secure connection setup (TLS handshake), once per connection | **13–22 ms** | once per connection, not per packet |
| Our socket read to the hand-off into our in-memory write queue (p99; the disk write happens later on another thread) | about **8–16 microseconds** | negligible |
| Our processing per tick (p99) | about **2–4 microseconds** | negligible |

**Measured summary:**

```
exchange -> our socket   median 0.75 s    p99 1.7 s     (about 99.5% of total)
network round trip       about 3.5 ms
TLS handshake            13-22 ms (once per connection)
socket -> write queue    p99 about 8-16 microseconds (in memory; disk write later)
our processing per tick  p99 about 2-4 microseconds
server                   AWS ap-south-1 (Mumbai), one r8g.large in zone ID aps1-az3
```

### How we measure (please read: it affects the numbers)

Delay = the time the packet arrives on our socket minus the packet's Last Trade Time (LTT). The LTT is sent in whole seconds on the IST clock, so we convert it to UTC (subtract 5 h 30 min) before subtracting.

Because the LTT has no fraction of a second, every sample reads **up to 1 second high, about 0.5 seconds on average**. So the true median delay may be well under 0.75 s, and the 99.5% share is an upper bound on your side's part, not an exact figure. This is why question 5 below asks for a finer timestamp.

### Packet-level samples (for tracing on your side)

Five packets from one session, one per row: the contract, its SecurityId and segment, the Last Trade Time the packet carried, and the moment it arrived on our socket (IST, microseconds).

| Contract | SecurityId | Segment | Last Trade Time in packet (IST) | Arrived on our socket (IST) | Delay |
|---|---|---|---|---|---|
| `<PRECISE-CONTRACT-LABEL>` | `<SID>` | `<SEGMENT>` | `<YYYY-MM-DD HH:MM:SS>` | `<YYYY-MM-DD HH:MM:SS.ffffff>` | `<S.ffffff> s` |
| `<PRECISE-CONTRACT-LABEL>` | `<SID>` | `<SEGMENT>` | `<YYYY-MM-DD HH:MM:SS>` | `<YYYY-MM-DD HH:MM:SS.ffffff>` | `<S.ffffff> s` |
| `<PRECISE-CONTRACT-LABEL>` | `<SID>` | `<SEGMENT>` | `<YYYY-MM-DD HH:MM:SS>` | `<YYYY-MM-DD HH:MM:SS.ffffff>` | `<S.ffffff> s` |
| `<PRECISE-CONTRACT-LABEL>` | `<SID>` | `<SEGMENT>` | `<YYYY-MM-DD HH:MM:SS>` | `<YYYY-MM-DD HH:MM:SS.ffffff>` | `<S.ffffff> s` |
| `<PRECISE-CONTRACT-LABEL>` | `<SID>` | `<SEGMENT>` | `<YYYY-MM-DD HH:MM:SS>` | `<YYYY-MM-DD HH:MM:SS.ffffff>` | `<S.ffffff> s` |

**The same rows, verbatim from our database:**

```json
<VERBATIM JSON ROWS FROM THE QUERY IN THE OPERATOR INSTRUCTIONS>
```

---

## Key observation

Our own part of the path is measured in microseconds, and the network round trip to your server is about 3.5 ms. The remaining delay, between half a second and two seconds, happens before the packet reaches our socket. We cannot reduce it from our side, so we need your help to understand it.

| Measurement | Value | What it tells us |
|---|---|---|
| Exchange to our socket, median | 0.75 s (reads up to 1 s high) | the delay we are asking about |
| Exchange to our socket, p99 | 1.7 s (reads up to 1 s high) | the slow tail |
| Round trip to `api-feed.dhan.co` | about 3.5 ms | the network is not the cause |
| Our socket read to our in-memory write queue, p99 | about 8–16 microseconds | our server is not the cause |
| Our processing per tick, p99 | about 2–4 microseconds | our server is not the cause |

---

## What works on the same account / same minute

This rules out the account, the token, the network and our server.

| Check | Endpoint | Status | Evidence |
|---|---|---|---|
| Live Market Feed | `api-feed.dhan.co` | Works — connects and streams every session | packets arrive all day; only the delivery delay is in question |
| Secure connection setup | `api-feed.dhan.co` | Works — normal | TLS handshake 13–22 ms |
| Network path | `api-feed.dhan.co` | Works — fast | round trip about 3.5 ms |
| Our own processing | our server | Works — fast | socket read to in-memory write queue p99 about 8–16 µs, per tick p99 about 2–4 µs |

**What does not work as we would like:** only the delivery delay between the exchange and our socket (median 0.75 s, p99 1.7 s, both reading up to 1 s high).

---

## Requests to Dhan engineering

1. **Which AWS region and availability zone (or data centre) serve `api-feed.dhan.co` (Live Market Feed) and `api.dhan.co` (order API)?** Please give the **zone ID** (for example `aps1-az1`, `aps1-az2`, `aps1-az3`), not the zone letter: a letter such as `ap-south-1a` names a different physical zone in your AWS account than in ours, so only the ID can be compared. We run in ap-south-1 (Mumbai), zone ID `<OUR_ZONE_ID>`, and want to know whether we are in the same zone as your servers.
2. **Is there a lower-latency or co-located feed product (for example a tick-by-tick feed or a co-location option), and what are its terms and eligibility for client ID `1106656882`?**
3. **Is an exchange-to-socket delay of 0.75 s median and 1.7 s p99 expected for the Live Market Feed, and which stage takes most of that time** — the exchange broadcast, your own aggregation or update interval, or your fan-out to clients?
4. **Would running our client from one specific availability zone in ap-south-1 measurably reduce the delivery delay or the connection setup time?** If so, which zone (by zone ID)?
5. **Does any Live Market Feed packet carry an exchange timestamp finer than whole seconds?** That would let us measure the delay without the 1-second rounding described above.

---

We are happy to run any diagnostic tests you need — a tcpdump capture of a session window with exact microsecond timestamps, a measurement from a different availability zone, measurements for specific security IDs you name, or an export of our per-packet receive times for a time window you choose. The answer decides whether we move our server, so it directly affects how we build our trading setup.

Thank you,
**Parthiban Subramanian**
Client ID: `1106656882`

<!--
=============================================================================
OPERATOR INSTRUCTIONS (DO NOT INCLUDE IN THE EMAIL)
=============================================================================
1. The coordinator commits and pushes this file.
2. Copy the GitHub URL of this file on the pushed branch.
3. In Gmail, send a SHORT message to apihelp@dhan.co with the link, for example:
     "Hi Dhan API Support team, full details of our feed delay question are
      here: (link). Thank you, Parthiban"
4. Never paste the markdown body into Gmail directly — proportional font breaks tables.
5. Figures are as measured by the orchestrator on 2026-11-13; they were not
   re-measured while drafting. The 99.5% share is an upper bound (whole-second LTT).
6. NOT READY TO SEND until the "Packet-level samples" section is filled from
   real rows (the support-email checklist in CLAUDE.md requires precise
   contract labels, SecurityIds, microsecond IST times and verbatim log
   lines; none were available while drafting (no database reachable from the
   drafting session), and none may be invented). The query below was NOT run
   while drafting; run it read-only in the QuestDB console on the box.
   Two tables hold the packets: `ticks` keeps a packet whose trade time is at
   most 2 whole seconds before its receipt (`ts` = trade time, `received_at`
   = receipt); `tick_quotes` keeps every later one (`ts` = receipt,
   `exchange_timestamp` = trade time). Reading `ticks` alone can never show
   a packet more than about 2-3 s late, so it hides the slow tail the email
   asks about. A `ticks` row with `ts = received_at` had no usable trade time
   (the writer fell back to the receipt time); its delay is a fake 0, so it
   is excluded. Run it once per window, changing only the window, for five
   windows across the session (for example 09:30, 11:00, 12:30, 14:00 and
   15:15), and take ONE row from each:
     SELECT * FROM (
       SELECT 'ticks' AS tbl, contract, security_id, segment,
              ts AS trade_time, received_at, volume,
              cast(received_at AS LONG) - cast(ts AS LONG) AS delay_us
       FROM ticks
       WHERE ts IN '2026-11-13T09:30;1m'
         AND contract IS NOT NULL AND received_at != ts
       UNION ALL
       SELECT 'tick_quotes' AS tbl, contract, security_id, segment,
              exchange_timestamp AS trade_time, ts AS received_at, volume,
              cast(ts AS LONG) - cast(exchange_timestamp AS LONG) AS delay_us
       FROM tick_quotes
       WHERE ts IN '2026-11-13T09:30;1m' AND contract IS NOT NULL
     ) ORDER BY delay_us DESC LIMIT 20;
   (Timestamps are microseconds, so `delay_us` is receipt minus trade time
   in microseconds; it must be positive.) Pick rows of busy contracts (index
   options near the money trade every second): a `tick_quotes` row is either
   a late trade or a quote update whose last trade is simply old, and only
   the first is a delivery delay. Before quoting a row, check that its
   `volume` is higher than the previous packet of the same contract (a new
   trade). Include at least one slow row (delay above 1.5 s) so the samples
   match the p99 in the email. Then confirm nothing is left with:
   grep -n '<[A-Z_]' <this file>, and delete the NOT READY note at the top.
7. DONE 2026-11-13: the zone ID is `aps1-az3`, read with DescribeInstances
   (the instance runs in ap-south-1b) and DescribeAvailabilityZones (ap-south-1b
   is `aps1-az3` in this account). The box is multi-AZ, so re-check both reads
   on the day you send this and correct the two places if it moved.
8. "Socket to write queue" is the `socket_to_wal` stage: it times the
   in-memory hand-off to the write-ahead-log writer and the ring, not a disk
   write (the write and its sync happen later on other threads).
=============================================================================
-->
