# Proposal: Dhan live-feed revival — 16-connection architecture

**Status:** APPROVED — 2026-08-11, operator (Parthiban), direct in-session
**Date:** 2026-08-09
**Approved by:** Parthiban (operator)
**Authority satisfied:** BOTH conditions below are met.

(a) and (b) — the dated operator quote required in
`websocket-connection-scope-lock.md` LANDED 2026-08-09 (see that file's section
"2026-08-09 (SAME DAY, SECOND QUOTE) — 16 CONNECTIONS + depth-20/depth-200
AUTHORIZED"), which explicitly lifts the depth-20/depth-200 ban and raises the
main-feed pool from 1 to 5. So the "Authority needed" this header used to
carry was already discharged two days before this flip.

**The approval quote for THIS flip (2026-08-11, typed directly in-session —
preserve EXACTLY, typos and expletives included):**

> "Yes bro our plan is to put back the entire 16 websocket connections to check
> and connect wirh all these massively right dude by upgrading our instance to
> the super instance right dude then why the fuck again and again confused dude
> I have provided the newer architecture newer design everything entilrey
> related to dhan right dude then why confusion dude just go ahead dude okay?
> Entirely go through dude okay?"

Reaffirmed twice in the same message with the zero-tick-loss and
no-reconnect-issue requirements, and the explicit instruction to use the
r8g.xlarge upgrade's kernel/cores/memory/core-pinning.

**Recorded honestly:** the executing session had been treating the DRAFT status
as a blocker and asking for a flip. That was over-cautious — the substantive
scope authorization already existed in the scope lock. The `Status:` line is a
workflow artifact owned by the operator, and the operator has now flipped it.
Implementation may start.

---

## Design

Rebuild the Dhan live market-data lane at 16 WebSocket connections, mirroring the layered
shape Dhan describe publicly, with the capture chain arranged so that the socket reader can
never be blocked by anything downstream.

The target set is 5 main-feed connections (5,000 instruments each), 5 depth-20 connections
(50 instruments each), 5 depth-200 connections (1 instrument each), and 1 order-update
connection. Dhan confirmed 2026-04-06 that the 5-connection limit applies to each endpoint
type independently, so 16 is within their limits — it is our own scope-lock that blocks it.

### The one rule the design rests on

The socket read task does exactly two things: append the raw frame to the write-ahead log,
and push it to the bounded ring. Nothing else, ever. No parsing, no aggregation, no database
write, no lock shared with a writer, no await on network or disk.

Time budget on the busiest socket (5,000 instruments at the measured ~3 updates/sec):

| Quantity | Value |
|---|---|
| Packets per second | 15,000 |
| Time available per packet | 66.7 microseconds |
| Work performed per packet | ~1.1 microseconds (log append + ring push) |
| Headroom | ~60x |

This is what keeps us in Dhan's fast-consumer lane, and it is arithmetic rather than
aspiration. Anything that enters that loop consumes the margin: a database write is ~1,000
microseconds, which is 15x over budget on its own.

### Why this also prevents disconnects

The automatic pong is only emitted while the read loop polls the socket. Dhan's server pings
every 10 seconds and closes the connection after 4 unanswered pings. A reader blocked doing
work therefore stops ponging and is disconnected — meaning a slow consumer does not merely
get its queue trimmed, it gets dropped. The drain-only rule fixes tick loss and disconnects
with a single constraint.

### Layers

| # | Layer | State | Notes |
|---|---|---|---|
| 1 | Connection pools, watchdog, reconnect ladder, subscribe guard | rebuild | deleted 2026-07-17 |
| 2 | Capture — raw frame to WAL before parse | exists | tune socket buffers |
| 3 | Bounded ring, spill, dead-letter | exists | one panic to fix |
| 4 | Binary parser, fixed-offset, zero-allocation | exists | add depth parsers |
| 5 | Multi-timeframe aggregator | rebuild | 13 timeframes, sparse |
| 6 | Persistence split | change | raw ticks to object storage, candles to database |
| 7 | Automation | build | four patterns, below |
| 8 | Proof metrics | build | eight measurements |

### Data path

Dhan sockets feed a reader that writes to the WAL on durable storage before any parse, then
pushes into a bounded ring. Everything downstream of the ring — spill, dead-letter, parser,
candle state, raw-segment upload, database — can stall, crash or fall behind without the
reader noticing.

### Thresholds

| Setting | Value | Rationale |
|---|---|---|
| Idle reconnect | 27 seconds | Server pings every 10s and closes after 4 missed; 27s means 2-3 missed, so we reconnect on our terms |
| Reconnect ladder | 0ms, 1s, 2s, 5s, 15s, 30s cap | First attempt instant; bounded thereafter |
| Ring capacity | 200,000 seals | Existing constant |
| Socket receive buffer | 16 MB | Each feed socket carries 2.43 MB/s; default ~208 KB absorbs only ~85ms |
| Adaptive universe start | 1,000 instruments | Below the measured ingest ceiling |
| Adaptive step | plus/minus 250 every 5 minutes | Grow above 30% headroom, shed below 10% |

---

## Protocol specification

Extracted from the operator-supplied Dhan API v2 documentation pack. Every fact below carries
its source; anything absent from the pack is marked NOT DOCUMENTED rather than inferred.

### Two findings that change the design

**Snapshot-on-subscribe does not exist on this feed.** Dhan's public architecture material
describes a live snapshot cache, and the documentation confirms it for the US global-stocks
feed: "After subscribing to Trade data, the server immediately sends the latest snapshot
values" (25-global-stocks.md:718, feed code 29, a different socket). No equivalent statement
exists anywhere for the India feed. What does arrive automatically on subscribe is the
prev-close packet (code 6), carrying previous close and previous-day open interest — useful,
but not the live price. Consequently a reconnect returns current price only when the
instrument next trades. For a liquid index that is milliseconds; for a far-month option it
may be minutes. The design must not claim a universal sub-2-second recovery.

**There is no sequence number and no gap detection.** An exhaustive search of all 29 files
returns no sequence field, no ordering guarantee, no conflation statement, no replay
mechanism. The only sequence-like field, on depth-20, is documented as "Message Sequence (to
be ignored)" (16-full-market-depth.md:117). Depth-200's equivalent bytes carry a row count,
not a counter (16-full-market-depth.md:149). Missed packets are therefore undetectable at the
protocol level. Loss detection must be synthesised from last-traded-time monotonicity,
volume and open-interest monotonic deltas, per-instrument silence timers, and the daily REST
cross-verification — which becomes the only ground truth available.

### Byte-numbering hazard

The header table is 1-based while every packet table describes the same header as "0-8" and
sizes it 8 bytes. Nine numbers, eight bytes. The header is 8 bytes at 0-based offsets 0 to 7,
and payload tables are 1-based, so a documented "9-12" means 0-based offsets 8 to 11. Ticker
totals 16 bytes only under this reading. A naive 0-based interpretation shifts every field by
one byte and produces plausible but wrong prices across the entire feed. Golden-vector tests
per packet type are mandatory for this reason.

### Wire facts

Endianness is little-endian, stated explicitly. Requests are JSON, responses binary. Security
identifiers are strings in the subscribe payload.

| Packet | Code | Size | Notes |
|---|---|---|---|
| Ticker | 2 | 16 bytes | float32 price, int32 last-trade-time |
| Quote | 4 | 50 bytes | day close only sent post market close |
| Open interest | 5 | 12 bytes | delivered alongside quote |
| Prev close | 6 | 16 bytes | second field is int32 open interest, NOT a price |
| Full | 8 | 162 bytes | includes 5 depth levels at bytes 63-162 |
| Disconnect | 50 | 10 bytes | int16 reason code |
| Index | 1 | NOT DOCUMENTED | code exists, no layout published |
| Market status | 7 | NOT DOCUMENTED | code exists, no layout published |

Request codes: 15/16 ticker subscribe and unsubscribe, 17/18 quote, 21/22 full, 23/25 depth,
11 connect, 12 disconnect. Codes 19, 20 and 24 are unassigned.

Exchange segments: 0 index, 1 NSE equity, 2 NSE derivatives, 4 BSE equity, 5 MCX commodity,
8 BSE derivatives. Values 3, 6 and 7 are unassigned — the enum must tolerate the gap without
panicking.

Depth feeds use a 12-byte header. Each level is 16 bytes: float64 price, uint32 quantity,
uint32 order count. Bid and ask arrive as separate packets, codes 41 and 51 respectively.
Note the prose at 16-full-market-depth.md:120 contradicts the code table at :125-126 on which
is buy and which is sell; the code table is authoritative. Critically, a single WebSocket
frame may contain several packets stacked together, so the reader must split by the header's
length field rather than assuming one packet per frame.

Limits: 5 connections per endpoint type, 5,000 instruments per main-feed connection, 100
instruments per subscribe message, 50 instruments per depth-20 connection, 1 instrument per
depth-200 connection. Exceeding the connection limit does not reject the new socket — it
disconnects the oldest with code 805.

---

## Edge cases

The connection layer must handle all sixteen sockets dropping simultaneously without a
thundering herd, a reader task panicking (supervised respawn, WAL replays), a clock step from
time synchronisation (the watchdog must use a monotonic clock or every socket reconnects at
once), and the account-level rate limit that was observed on three fresh addresses in June.

The capture layer must survive a process kill mid-write with idempotent replay, and must
preserve multiple ticks arriving within the same whole second — the feed's timestamps are
second-granular, so the capture sequence is what distinguishes them.

The parser must be total across arbitrary bytes, must tolerate the two documented-but-unspecified
message codes by counting and discarding them, and must never assume a fixed depth-200 packet
size, since the real length is derived from the row count.

The aggregator must append new timeframe ordinals rather than inserting them, because the
ordinal is written into spilled records on disk; inserting would silently re-map historical
data.

Persistence must keep raw ticks out of the database. At the target scale the measured ceiling
is approximately 1,127 instruments before database ingest saturates, while disk and memory
allow roughly 9,465 and 3,371 respectively. Routing raw ticks to compressed segments and
object storage moves the binding constraint from ingest to disk.

## Failure modes

| Failure | Blast radius | Automated response |
|---|---|---|
| Socket reset | One pool blind | Reconnect at zero delay, resubscribe, verify first frame |
| Reader blocked | Missed pongs, server disconnects | Prevented by the drain-only rule |
| Ring saturated | Backpressure | Spill to disk; reader never blocks |
| Spill full | Overflow | Dead-letter queue, loud |
| Database unavailable | Ingest halted | Three-tier absorb, replay on recovery |
| Object storage unreachable | Segment backlog | Local buffer; reader unaffected |
| Disk full | Capture at risk | Auto-archive partitions |
| Ingest ceiling exceeded | Rows dropped | Adaptive sizer sheds instruments |
| Account rate limit | Connections refused | Backoff plus per-pool budget |
| Sixth socket opened | Oldest killed silently | Pool budget refuses to open it |

## Test plan

Unit tests for the reconnect ladder sequence, the 27-second watchdog, monotonic clock usage,
and pool budget enforcement. Property tests for ladder bounds and sparse sealing. Loom tests
for the subscribe guard under concurrent reconnection. Chaos tests for killing a socket
mid-subscribe, killing all sixteen at once, a time-synchronisation step, a process kill
mid-WAL-write, a sixty-second database outage, and a stalled writer. Fuzz tests asserting the
parser never panics on arbitrary bytes. Golden-vector tests per packet type, which are the
defence against the byte-numbering hazard. Backward-compatibility tests proving pre-change
spilled records still decode after new timeframe ordinals are appended.

Runtime self-tests run in production every fifteen minutes: kill one socket and assert
reconnect, resubscribe and first frame within the target. Boot-time assertions verify every
kernel and container setting and refuse to serve if any fails.

## Rollback

Every layer is additive behind a configuration flag defaulting to off. The feed can be
disabled without redeploying. The persistence split writes to a new location and leaves
existing tables untouched. Timeframe ordinals are appended, so a revert leaves older records
readable; records written with the new ordinals are rejected loudly by an older build rather
than mis-decoded.

## Observability

Eight measurements convert the guarantee into evidence: zero-window events at zero, receive
queue depth near zero, reader loop 99th percentile under 10 microseconds, ring high-water
under half, spill and dead-letter counters at zero in normal operation, no gaps in the capture
sequence, reconnect gap seconds, and the daily divergence against Dhan's own candle tape.

Alarms trigger actions rather than messages. A message is sent only when the automated action
itself fails.

---

## Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: every message Dhan
delivers is captured durably before it is parsed, absorbed through a bounded ring with disk
spill and a dead-letter tier, and replayed idempotently; the parse path is fixed-offset and
allocation-free, verified by heap-profiling tests that gate every change.

NOT claimed: that we observe every trade. The feed is conflated — Dhan's own charts have
carried a five-second close that was never delivered on the stream — and no redistributed
Indian retail feed can do better, since the exchange provides true tick-by-tick only inside
its own co-location facility. NOT claimed: zero disconnects. The protocol closes a socket
after four unanswered pings and the access token expires daily. NOT claimed: sub-two-second
recovery for illiquid instruments, because this feed has no snapshot-on-subscribe. NOT
claimed: protocol-level detection of missed packets, because no sequence number exists.

What can honestly be promised is this: every message that reaches us is captured and never
lost, we remain in the fast-consumer lane with roughly sixty times the required headroom, and
the daily divergence against Dhan's own tape is measured and reported rather than assumed.
