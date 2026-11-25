<!--
=============================================================================
DHAN SUPPORT EMAIL — 2026-11-13 live market feed delivery delay and the AWS
region / availability zone of the feed and order servers
Drafted by Claude Code on the owner's request (decision card 2026-11-13,
"Yes, draft it (Recommended)"). The owner sends it; no session contacts Dhan.
Share the GitHub link in the Gmail reply — do NOT copy-paste plain text.
=============================================================================
-->

# Live Market Feed delivery delay (exchange to our socket) and the AWS region / zone of the feed and order servers — New Ticket (please assign)

**To:** apihelp@dhan.co
**From:** sjparthi93@gmail.com
**Subject:** New Ticket — Live Market Feed delivery delay (options median 1.18 s, stocks 1.13 s, indices 0.50 s, exchange to socket) + an apparent per-contract update cap (about 2 per second for options, 1 per second for stocks) + which AWS zone serves api-feed.dhan.co
**Date:** 2026-11-18 (drafted 2026-11-13; all figures re-measured on 2026-11-18, 09:15 to 09:38 IST)

---

Hi Dhan API Support team,

We measure the delivery delay of the Live Market Feed on every trading day and almost all of it happens before the data reaches our server. We would like to understand where that time goes, where your feed and order servers run, and whether a faster option exists.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban Subramanian |
| UCC | `NWXF17021Q` |
| Date of incident | Every trading session (ongoing; figures below measured 2026-11-18 IST) |
| Time of incident | Measured continuously, 09:15–15:30 IST |

---

## What we tested / observed

This is a connection-level question, not a fault with any one contract; the packet-level samples further down name exact contracts, SecurityIds and microsecond times so you can trace them. Our setup:

- Our server runs in AWS **ap-south-1 (Mumbai)**, on one r8g.large instance in availability zone ID **`aps1-az3` (zone ap-south-1b in our account)** (we quote the zone ID, for example `aps1-az1`, because zone letters such as `ap-south-1b` map to different physical zones in each AWS account).
- The server clock is kept in sync with the AWS time service.
- We connect to the Live Market Feed at `wss://api-feed.dhan.co` (version 2) and subscribe in batches of at most 100 instruments per message, within the 5,000-per-connection limit.
- For every packet we record the moment it arrives on our socket, before any parsing.

What we measure, split by where the time is spent (2026-11-18, 09:15 to 09:38 IST, 1,576,349 packets that carried a new trade time; the network figures are from 15 connections made at 09:25 IST):

| Stage | Measured | Share of total |
|---|---|---|
| Exchange trade time to arrival on our socket, stock and index options (`NSE_FNO`) | median **1.18 s**, p99 **2.94 s** | almost all of it |
| Same, stocks (`NSE_EQ`) | median **1.13 s**, p99 **2.93 s** | almost all of it |
| Same, indices (`IDX_I`) | median **0.50 s**, p99 **0.99 s** | almost all of it |
| TCP connect to `api-feed.dhan.co` (one network round trip) | **0.68–1.54 ms** | under 0.2% |
| TCP connect to `api.dhan.co` | **0.25–0.28 ms** | — |
| TLS handshake to `api-feed.dhan.co`, once per connection | **1.7–2.7 ms** | once per connection, not per packet |
| Our socket read to the hand-off into our in-memory write queue | about **1 microsecond** typical, about 2 microseconds at p99 | negligible |
| Our processing per tick | about **2 microseconds** on average | negligible |

**Measured summary:**

```
exchange -> our socket   options median 1.18 s  p99 2.94 s
                         stocks  median 1.13 s  p99 2.93 s
                         indices median 0.50 s  p99 0.99 s
network round trip       api-feed.dhan.co 0.68-1.54 ms, api.dhan.co 0.25-0.28 ms
TLS handshake            1.7-2.7 ms (once per connection)
our socket -> queue      about 1 microsecond (in memory; disk write later)
our processing per tick  about 2 microseconds (average)
server                   AWS ap-south-1 (Mumbai), zone ID aps1-az3
```

### How we measure (please read: it affects the numbers)

Delay = the time the packet arrives on our socket minus the packet's Last Trade Time (LTT). The LTT is sent in whole seconds, so every sample reads **0 to 1 second high, about 0.5 seconds on average**.

For indices the whole distribution fits that rounding alone (25th / 50th / 75th / 95th percentiles 0.24 / 0.50 / 0.74 / 0.93 s, the shape of a uniform 0–1 s offset), so index packets reach us with very little delay on your side. For options and stocks the same percentiles are 0.75 / 1.18 / 1.87 / 2.73 s (options) and 0.73 / 1.13 / 1.76 / 2.67 s (stocks): after removing the rounding, roughly **0.5–1 s** more is added before the packet reaches us. (Packets whose trade time is more than 2 seconds old are counted separately on our side, so the slow tail is, if anything, understated here.)

### An apparent per-contract update cap

Across every contract we subscribe, the most packets any single contract sent us in one minute (09:30–09:31 IST) were:

| Segment | Contracts | Most packets from one contract in one minute | So at most |
|---|---|---|---|
| `NSE_FNO` | 7,740 | 122 | about **2 per second** |
| `NSE_EQ` | 743 | 67 | about **1 per second** |
| `IDX_I` | 118 | 598 | about 10 per second |

The busiest contracts sit exactly at the cap (for example `MARUTI-23Nov2026-12800-CE`, `MCX-23Nov2026-3800-CE` and `BSE-23Nov2026-4000-CE` each sent 120 packets that minute), and the typical gap between two packets of one busy option is **476 ms**. One busy option arrived at these fractions of a second, at a steady rhythm, whatever its trade time:

```
MARUTI-23Nov2026-12800-CE  SecurityId 117314  NSE_FNO
LTT 09:30:09  received 09:30:10.154548
LTT 09:30:10  received 09:30:10.575252
LTT 09:30:11  received 09:30:11.148617
LTT 09:30:11  received 09:30:11.568987
LTT 09:30:11  received 09:30:12.160490
LTT 09:30:11  received 09:30:12.668150
```

This looks like each contract is sent at most once per about 500 ms (options) or 1 s (stocks), and a trade waits for the next slot. That alone would add about 0.25–0.5 s on average, which matches the extra delay above.

### Packet-level samples (for tracing on your side)

Five packets from 2026-11-18, one per window, each a new trade (its cumulative volume is higher than the previous packet of the same contract). Delay = receipt minus Last Trade Time, both IST.

| Contract | SecurityId | Segment | Last Trade Time in packet (IST) | Arrived on our socket (IST) | Delay |
|---|---|---|---|---|---|
| `CLEAN` | `5049` | `NSE_EQ` | `2026-11-18 09:20:05` | `2026-11-18 09:20:05.600014` | `0.600014 s` |
| `NIFTY-23Nov2026-22500-PE` | `64279` | `NSE_FNO` | `2026-11-18 09:22:11` | `2026-11-18 09:22:11.600129` | `0.600129 s` |
| `NIFTY-23Nov2026-27300-CE` | `64563` | `NSE_FNO` | `2026-11-18 09:25:31` | `2026-11-18 09:25:33.974560` | `2.974560 s` |
| `NIFTY-23Nov2026-24350-PE` | `64412` | `NSE_FNO` | `2026-11-18 09:30:36` | `2026-11-18 09:30:36.400003` | `0.400003 s` |
| `TARIL` | `15174` | `NSE_EQ` | `2026-11-18 09:35:21` | `2026-11-18 09:35:22.000110` | `1.000110 s` |

**The same rows, verbatim from our database** (`ts` = Last Trade Time, `received_at` = arrival, both IST; `prev_vol` = the previous packet's cumulative volume; `delay_us` in microseconds):

```json
["CLEAN",5049,"NSE_EQ","2026-11-18T09:20:05.000000Z","2026-11-18T09:20:05.600014Z",3469,3269,600014]
["NIFTY-23Nov2026-22500-PE",64279,"NSE_FNO","2026-11-18T09:22:11.000000Z","2026-11-18T09:22:11.600129Z",885690,885040,600129]
["NIFTY-23Nov2026-27300-CE",64563,"NSE_FNO","2026-11-18T09:25:31.000000Z","2026-11-18T09:25:33.974560Z",1740830,1733290,2974560]
["NIFTY-23Nov2026-24350-PE",64412,"NSE_FNO","2026-11-18T09:30:36.000000Z","2026-11-18T09:30:36.400003Z",1224860,1224665,400003]
["TARIL",15174,"NSE_EQ","2026-11-18T09:35:21.000000Z","2026-11-18T09:35:22.000110Z",115286,115285,1000110]
```

---

## Key observation

The network between our server and yours takes about 1 millisecond, and our own handling takes a few microseconds. Index packets reach us with little delay beyond the whole-second rounding. Options and stocks arrive about 0.5–1 s later than that, and each contract arrives at most about twice per second (options) or once per second (stocks). We cannot reduce this from our side, so we need your help to understand it.

| Measurement | Value | What it tells us |
|---|---|---|
| Options, exchange to our socket, median | 1.18 s (reads 0–1 s high) | the delay we are asking about |
| Indices, exchange to our socket, median | 0.50 s (reads 0–1 s high) | indices are not delayed the same way |
| Most packets from one option in a minute | 122 (about 2 per second) | an apparent per-contract cap |
| Round trip to `api-feed.dhan.co` | 0.68–1.54 ms | the network is not the cause |
| Our handling | a few microseconds | our server is not the cause |

---

## What works on the same account / same minute

This rules out the account, the token, the network and our server.

| Check | Endpoint | Status | Evidence |
|---|---|---|---|
| Live Market Feed | `api-feed.dhan.co` | Works — connects and streams every session | 1,576,349 new-trade packets from 09:15 to 09:38 IST on 2026-11-18 |
| Secure connection setup | `api-feed.dhan.co` | Works — normal | TLS handshake 1.7–2.7 ms |
| Network path | `api-feed.dhan.co` | Works — fast | TCP connect 0.68–1.54 ms |
| Index packets | `api-feed.dhan.co` | Works — prompt | median 0.50 s, the same as the whole-second rounding |

**What does not work as we would like:** only the delivery of option and stock packets: about 0.5–1 s after removing the rounding, and at most about 2 (options) or 1 (stocks) packets per contract per second.

---

## Requests to Dhan engineering

1. **Which AWS region and availability zone (or data centre) serve `api-feed.dhan.co` (Live Market Feed) and `api.dhan.co` (order API)?** Please give the **zone ID** (for example `aps1-az1`, `aps1-az2`, `aps1-az3`), not the zone letter: a letter such as `ap-south-1a` names a different physical zone in your AWS account than in ours, so only the ID can be compared. We run in ap-south-1 (Mumbai), zone ID `aps1-az3`, and want to know whether we are in the same zone as your servers.
2. **Is there a lower-latency or co-located feed product (for example a tick-by-tick feed or a co-location option), and what are its terms and eligibility for client ID `1106656882`?**
3. **Is an exchange-to-socket delay of about 1.2 s median and 2.9 s p99 for options and stocks (0.5 s for indices) expected for the Live Market Feed, and which stage takes most of that time** — the exchange broadcast, your own aggregation or update interval, or your fan-out to clients?
4. **Would running our client from one specific availability zone in ap-south-1 measurably reduce the delivery delay or the connection setup time?** If so, which zone (by zone ID)?
5. **Does any Live Market Feed packet carry an exchange timestamp finer than whole seconds?** That would let us measure the delay without the 1-second rounding described above.
6. **GIFT Nifty sends no Live Market Feed packets.** *(Added 2026-11-16.)* We subscribe `GIFTNIFTY`, SecurityId `5024`, segment `IDX_I` (from your instrument master), on the main Live Market Feed every trading day. On every trading day from 2026-11-02 to 2026-11-16 we received **zero** packets for it, while every other index on the same socket (117 of them) produced a live bar in every minute. On the same days, `POST /v2/charts/intraday` with interval `"1"` returns a full day of 1-minute bars for SecurityId `5024`. Is GIFT Nifty available on the Live Market Feed? If it is, which segment and SecurityId should we subscribe? If not, please confirm so we stop subscribing it.
7. **Is each contract limited to a fixed number of packets per second on the Live Market Feed?** *(Added 2026-11-18.)* We never receive more than about 2 packets per second for any option contract or about 1 per second for any stock, even for the busiest contracts (see "An apparent per-contract update cap" above), while indices reach about 10 per second. If there is such a limit, what is it per segment, and is there a mode, a different request code, or a different product that sends every trade or a faster update rate for client ID `1106656882`?

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
5. Figures were re-measured on 2026-11-18 from 09:15 to 09:38 IST, read-only on
   the box (`ticks` table, packets whose trade time is at most 2 s old; the
   older ones are in `tick_quotes` and are not in these percentiles). The
   network figures are the 15 `network_rtt_session` rows written at 09:25 IST.
6. DONE 2026-11-18: the "Packet-level samples" section is filled from five
   real rows (one per window: 09:20, 09:22, 09:25, 09:30, 09:35 IST; each a
   new trade, its volume above the previous packet's; one slow row, 2.97 s).
   Original instruction, kept for the record: NOT READY TO SEND until the "Packet-level samples" section is filled from
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
7. DONE 2026-11-13, re-checked 2026-11-18 (instance metadata: zone ID
   `aps1-az3`): the zone ID is `aps1-az3`, read with DescribeInstances
   (the instance runs in ap-south-1b) and DescribeAvailabilityZones (ap-south-1b
   is `aps1-az3` in this account). The box is multi-AZ, so re-check both reads
   on the day you send this and correct the two places if it moved.
8. "Socket to write queue" is the `socket_to_wal` stage: it times the
   in-memory hand-off to the write-ahead-log writer and the ring, not a disk
   write (the write and its sync happen later on other threads).
=============================================================================
-->
