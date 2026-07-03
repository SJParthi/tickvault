<!--
=============================================================================
GROWW SUPPORT DOSSIER — 2026-07-03 latency floor + NATS silent-EOF closes
Share the GitHub link in the email — do NOT copy-paste plain text.

BEFORE SENDING — operator-fill checklist (every intentional placeholder):
  1. <GROWW_API_SUPPORT_EMAIL — operator fill before sending>
       — no Groww support email exists in this repo; do NOT guess one.
  2. <GROWW_CLIENT_ID — operator fill before sending>  (appears 3x)
       — Groww account identifier; NEVER substitute the Dhan Client ID.
  3. <PENDING — run SQL §1, paste>  (Section A result tables)
       — run docs/groww-support/sql/external-floor.sql Q1/Q2/Q3 against
         prod QuestDB and paste the numbers.
  4. <PENDING — paste verbatim CloudWatch line>  (Section B timeline +
     verbatim-log section) — fetch from CloudWatch group /tickvault/prod/app,
     window 2026-07-03 12:00–12:10 IST, filter patterns "groww" and "NATS".
  5. <PENDING — exact offset from CloudWatch> / <PENDING — timestamp from
     CloudWatch>  (Section B timeline timestamp cells) — fill each row's
     exact second/sub-second from the pasted CloudWatch lines.
  6. REDACT any token/JWT/credential material from pasted CloudWatch lines
     before sending — the `last_error=<detail>` field carries raw exception
     text.
  7. `<N>` tokens in the verbatim log-format block and the Section B
     timeline "format:" cells are FORMAT placeholders (part of the
     sidecar's log-string templates), NOT operator-fill cells — the pasted
     verbatim CloudWatch lines replace the whole format line.
     (`<PENDING>` in SUMMARY-2026-07-03.md is a meta-reference to this
     convention, also not a fill cell.)
  Find them all:  grep -n '<[A-Z_<]' docs/groww-support/2026-07-03-*.md
=============================================================================
-->

# Live-feed snapshot latency floor (~50–150 ms server→client) + silent NATS socket closes without ERR frame — New request

**To:** <GROWW_API_SUPPORT_EMAIL — operator fill before sending>
**From:** sjparthi93@gmail.com
**Subject:** Live NATS feed — measured server→client latency floor + server-side socket closes with no ERR frame / no WS close handshake (2026-07-03 12:02:43 IST evidence)
**Date:** 2026-07-03

---

Hi Groww API team,

We consume your live market feed via the official `growwapi` Python SDK
and are reporting two independent, data-backed observations: (A) a
consistent latency floor between your server-side tick timestamp and
client receipt, and (B) server-side socket closes that arrive with no
NATS ERR frame and no WebSocket close handshake.

| Field | Value |
|---|---|
| Groww Client ID | `<GROWW_CLIENT_ID — operator fill before sending>` |
| Name | Parthiban Subramanian |
| Contact email | sjparthi93@gmail.com |
| SDK | official `growwapi` 1.5.0 (nats-py 2.15.0), Python |
| Feed endpoint | `wss://socket-api.groww.in` (NATS-over-WebSocket + Protobuf) |
| Date of incident (Section B) | 2026-07-03 IST |
| Time of incident (Section B) | 12:02:43 IST |

*(TickVault = our internal capture system; feed = Groww live NATS feed
via official growwapi SDK 1.5.0.)*

---

## Section A — Snapshot-pipeline latency (the external floor)

### How we measure

Every tick your feed delivers carries your server-stamped `tsInMillis`
(millisecond precision — we preserve it end-to-end). Our capture system
stores, per tick row:

- `ts` — your server stamp `tsInMillis`, converted UTC-ms → IST
  wall-clock, full millisecond precision preserved;
- `received_at` — our machine's receipt stamp (IST wall-clock).

Both stamps are on the SAME wall-clock basis, so
`external floor = received_at − ts` is a clean like-for-like delta
(after subtracting our own internal share — see the honesty note below).
Our host clock is chrony-disciplined and a boot-time skew probe halts
the system if wall-clock skew exceeds 2 s, so the delta is not a
clock-drift artifact; a negative-delta sanity query is part of the
evidence pack.

**Honesty note on attribution.** Today `received_at` is stamped in our
consumer process (one clock read per drain wake), so the measured delta
= your server+network share PLUS our own sidecar/file/bridge share. A
per-callback capture stamp (taken at the SDK callback instant,
`capture_ns`) is being deployed on our side; once live, the split
becomes exact: OUR share = `received_at − capture_ns` (target ≤ 1 ms),
GROWW share = `capture_ns − ts`. Our prior operational observation of
the Groww-attributable share is roughly **50–150 ms**; the tables below
carry the measured numbers. The **minimum** delta per hour is the
strongest signal — it is a hard lower bound on the server+network floor
regardless of our internal share.

### Results — per hour × per segment (2026-07-03, IST)

*(Produced by Q1 in `docs/groww-support/sql/external-floor.sql`.)*

| hour (IST) | segment | ticks | min ms (best-case floor) | p50 ms | p95 ms | p99 ms |
|---|---|---|---|---|---|---|
| <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> |

### Results — per segment, all day (2026-07-03)

*(Produced by Q2 in the same SQL file.)*

| segment | ticks | min ms | p50 ms | p95 ms | p99 ms |
|---|---|---|---|---|---|
| <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> | <PENDING — run SQL §1, paste> |

**Best-case floor observed (global minimum across all ticks, all day):**
`<PENDING — run SQL §1, paste>` ms, achieved by security_id
`<PENDING — run SQL §1, paste>` at `<PENDING — run SQL §1, paste>` IST
(Q3 lists the 5 lowest deltas with instrument + timestamp). When pasting
Q3 results, resolve each security_id to its Groww trading symbol +
exchange_token per the README instrument-label convention — the `ticks`
table carries no symbol column; use the resolve-helper query in the Q3
comment of `docs/groww-support/sql/external-floor.sql`
(`SELECT symbol_name, display_name FROM instrument_lifecycle WHERE
security_id = <id> AND feed = 'groww' LIMIT 1;`).

### Why this pattern suggests a periodic snapshot pipeline

The distribution shape we observe is consistent with a server-side
snapshot/publish cadence (ticks batched and published on a fixed period)
rather than per-trade push: deltas cluster in a band rather than
tailing down toward ~network-RTT. We would like your confirmation of the
publish cadence — see the numbered questions below.

---

## Section B — NATS socket silently closed by server, 12:02:43 IST 2026-07-03

### What we observed

Our feed consumer is the official SDK's `GrowwFeed` (NATS-over-WebSocket).
At 12:02:43 IST today the server side closed the socket with **no NATS
`-ERR` frame delivered to the client and no WebSocket close handshake**
— the SDK's blocking `feed.consume()` simply never returned another
message, and nats-py's stored `last_error` was populated only after the
fact (the classic swallowed "Authorization Violation" / bare EOF
signature we have seen on prior occurrences). Our stall watchdog
detected decoded-record silence during market hours, force-closed the
socket, and the reconnect ladder (50 ms initial, doubling to a 5 s cap)
restored the feed with a fresh subscribe.

Note on timing: 12:02:43 IST is **not** near the daily 06:00 IST token
reset, so a scheduled token-expiry close does not fit this event — see
questions 5 and 7.

### Timeline

We attempted to export the verbatim CloudWatch lines while assembling
this dossier from the build environment; the AWS CLI was unavailable
there, so the evidence-source cells below carry the EXACT log-string
formats our sidecar emits (stderr → CloudWatch group
`/tickvault/prod/app`) with paste markers. The operator fills the
verbatim lines before sending — we state this plainly rather than
fabricate log content.

| Timestamp (IST) | Event | Evidence source |
|---|---|---|
| `2026-07-03 12:02:43.<PENDING — paste verbatim CloudWatch line>` | last decoded tick before silence (server-side close begins) | tick capture record + `<PENDING — paste verbatim CloudWatch line>` |
| `12:02:43 + <PENDING — exact offset from CloudWatch>` | nats-py stores swallowed error; no `-ERR` frame reached the application | format: `groww sidecar: NATS last_error -> <detail>` — `<PENDING — paste verbatim CloudWatch line>` |
| `12:02:43 + <PENDING — exact offset from CloudWatch>` | SDK connection-closed callback fires (or is bypassed entirely) | format: `groww sidecar: NATS connection closed -> last_error=<detail>` — `<PENDING — paste verbatim CloudWatch line>` |
| `12:02:43 + <PENDING — exact offset from CloudWatch>` | watchdog detects decoded-record stall during market hours; force-closes NATS socket | format: `groww sidecar: FEED STALLED — <N>s with NO advancing exchange timestamp ...` (or the watermark-lag form `groww sidecar: FEED STALLED — watermark lag <N>ms behind wall-clock ...`) — `<PENDING — paste verbatim CloudWatch line>` |
| `12:02:43 + <PENDING — exact offset from CloudWatch>` | nats-py disconnected callback fires after the force-close | format: `groww sidecar: NATS disconnected -> last_error=<detail>` — `<PENDING — paste verbatim CloudWatch line>` |
| `12:02:43 + <PENDING — exact offset from CloudWatch>` | reconnect ladder fires (50 ms base, ×2 per attempt, 5 s cap); re-subscribes with the CACHED daily token (pure feed-side failures keep the token cached; SSM is NOT re-read on this path — only an AUTH-class failure drops the cache and re-reads SSM) | reconnect log — `<PENDING — paste verbatim CloudWatch line>` |
| `<PENDING — timestamp from CloudWatch>` | ticks flowing again on fresh socket | tick capture record — `<PENDING — paste verbatim CloudWatch line>` |

### Mechanics (why this is painful client-side)

1. The server closes the NATS-over-WS socket without sending a NATS
   `-ERR` frame and without a WebSocket close handshake — the client
   sees a bare EOF.
2. nats-py records the condition into its internal `last_error` but the
   error callback path does not raise into the consuming coroutine.
3. The SDK's blocking `feed.consume()` therefore hangs indefinitely —
   there is no exception for an `except → reconnect` handler to catch.
4. Recovery only happens because OUR watchdog notices decoded-record
   silence during market hours and force-closes the socket, making
   `consume()` return so the reconnect loop can re-subscribe.

This means every silent close costs us the watchdog detection window on
top of reconnect time, purely because no protocol-level close signal is
sent.

---

## What works on the same account / same minutes

Rules out account / token / host / network issues on our side.

| Check | Status | Evidence |
|---|---|---|
| Groww token read (shared SSM-distributed daily token) | Working — token valid, minted ~06:05 IST, no auth error at mint or read | token telemetry; no `GROWW LIVE FEED REJECTED:` stale-token alert fired |
| NATS subscribe ACK after connect | Working — subscribe accepted on every (re)connect, including the 12:02 reconnect | sidecar logs |
| Tick delivery before 12:02:43 IST | Working — ticks flowed continuously up to the close instant | tick capture records |
| Tick delivery after reconnect | Working — resumed on the fresh socket | tick capture records |
| Independent market-data feed (different broker) on the same host | Streaming — unaffected at 12:02:43 IST | rules out our host, NIC, and network path |
| The ONLY failure | the NATS-over-WS socket silently EOFs server-side | Section B |
| Latency (Section A) | Delivery itself works — but arrival lags your server stamp by the measured floor (`<PENDING — run SQL §1, paste>` ms p50) | Section A tables |

---

## Verbatim log lines (for grep)

The exact stderr formats our consumer emits (these strings are what we
grep in CloudWatch; verbatim captured lines to be pasted by the operator
before sending — labeled as formats until then):

```
groww sidecar: NATS error_cb -> <detail>
groww sidecar: NATS connection closed -> last_error=<detail>
groww sidecar: NATS disconnected -> last_error=<detail>          (nats-py disconnected callback — post-close timeline row, NOT the detection row)
groww sidecar: NATS last_error -> <detail>
groww sidecar: force-close of NATS socket failed (<type>)
groww sidecar: FEED STALLED — <N>s with NO advancing exchange timestamp across the universe during market hours (...); a frozen snapshot re-dump counts as stalled. Force-closing the NATS socket to trigger reconnect + re-subscribe (self-heal).
groww sidecar: FEED STALLED — watermark lag <N>ms behind wall-clock (> <N>ms for <N> consecutive checks) during market hours (...); micro-advancing timestamps that never catch up count as stalled. Force-closing the NATS socket to trigger reconnect + re-subscribe (self-heal).
groww sidecar: SILENT FEED — subscribed <N> stocks + <N> indices but received NO live records in <N>s. (...)
GROWW LIVE FEED REJECTED: <reason>
```

The watchdog's own stall-detection lines are the `FEED STALLED` /
`SILENT FEED` strings above (the two `FEED STALLED` forms and the
`SILENT FEED` form are the detection signals); the
`groww sidecar: NATS disconnected -> last_error=<detail>` line is the
nats-py disconnected callback that fires AFTER the force-close.

```json
<PENDING — paste verbatim CloudWatch line>
```

---

## Requests to Groww engineering

1. **What is the internal snapshot/publish cadence and the expected
   server→client latency budget for the NATS live feed?** Our measured
   floor (Section A) sits at `<PENDING — run SQL §1, paste>` ms minimum
   / `<PENDING — run SQL §1, paste>` ms p50 — is that within your design
   budget, or does it indicate a problem specific to our account or
   subjects?
2. **Is the observed floor consistent with a periodic snapshot pipeline
   (and at what period), rather than per-trade push?** The delta
   distribution clusters in a band rather than tailing toward network
   RTT, which is the signature of a fixed publish period.
3. **Can a lower-latency tier or subject be enabled for this account?**
   If a faster (per-trade or shorter-period) stream exists, we would
   like to evaluate it.
4. **Why does the server close NATS-over-WS sockets without a `-ERR`
   frame or a WebSocket close handshake?** The client sees a bare EOF;
   this violates normal NATS protocol expectations and defeats
   exception-based reconnect handling in the official SDK (nats-py
   swallows it into `last_error` and the blocking `consume()` never
   returns).
5. **What server-side event correlates with 12:02:43 IST 2026-07-03 on
   our connection?** Connection identifiers / client IP available on
   request. This was mid-session, ~6 hours after the daily token reset,
   so scheduled expiry does not fit.
6. **Is there a keepalive/ping cadence we should honor to avoid idle
   reaping?** If a load balancer reaps connections it deems idle, please
   document the expected client-side PING interval so we can comply.
7. **Please confirm the daily 06:00 IST token reset does NOT terminate
   in-flight NATS sessions mid-day.** If it can, please document the
   expected close signal so clients can distinguish it from a fault.

---

We are happy to run any diagnostic tests you need — tcpdump captures on
our side, reproduction with specific instruments/subjects, a specific
reproduction window during market hours, our connection id / client IP,
or millisecond-precision IST timestamps for every event on request.
These two issues (arrival latency floor and silent socket closes)
directly constrain our live capture quality and we need to move forward.

Thank you,
**Parthiban Subramanian**
Groww Client ID: `<GROWW_CLIENT_ID — operator fill before sending>`

---

## Appendix — internal notes (not part of the email body)

- **Measurement basis:** the delta is `received_at − ts` in our tick
  store; both are IST wall-clock TIMESTAMPs, so the subtraction is
  same-basis. We deliberately do NOT subtract the raw
  `exchange_timestamp` column — it is SECOND-granular on both feeds
  (Groww's mirrors Dhan's IST-epoch-seconds convention, ms truncated —
  `crates/storage/src/groww_persistence.rs:268-270`); `ts` is the only
  ms-true stamp. In QuestDB, TIMESTAMP−TIMESTAMP arithmetic yields LONG
  **microseconds**; the SQL converts to ms via /1000.0 OUTSIDE any
  aggregate (integer /1000 inside a percentile would truncate sub-ms
  deltas to 0).
- **capture_ns dependency:** `capture_ns` (per-callback capture stamp in
  the Python sidecar) is NOT yet merged anywhere in this repo (0 hits).
  Until it lands, `received_at` (stamped in the Rust bridge,
  `crates/app/src/groww_bridge.rs::row_received_at`, one clock read per
  drain wake) is the receipt proxy and the measured delta includes our
  sidecar+NDJSON+bridge share. Post-capture_ns the attribution split is
  exact (OUR share target ≤ 1 ms).
- **Portal metric row — skipped in this change:** no per-feed
  receipt−exchange_ts source exists today (verified: `/api/feeds/health`
  exposes only `last_tick_age_secs` — a staleness measure, not a
  receipt−server-stamp delta — and `groww_bridge.rs` emits no latency
  histogram/gauge). Cheapest follow-up: stamp one gauge
  `tv_feed_external_floor_ms{feed}` at parse time in `groww_bridge` +
  one dict key in `_feed_latency_rows` in
  `deploy/aws/lambda/operator-control/handler.py` (its own PR — Rust
  change is plan-gated by design-first-wall).
- **SQL:** `docs/groww-support/sql/external-floor.sql` — see its header
  for validation status.
