<!--
Draft prepared 2026-07-20 for the operator to file as a NEW ticket (no
ticket number yet — replace "NEW" once Dhan assigns one). Share the
GitHub rendered link in Gmail per docs/dhan-support/README.md — never
paste the body as plain text.
-->

# Intraday 1-minute candle publication SLA at market open — Ticket #NEW

**To:** apihelp@dhan.co
**From:** sjparthi93@gmail.com
**Subject:** `/v2/charts/intraday` — first-session-minutes (09:15/09:16 IST) 1-minute candles not queryable until ~09:18 IST; publication-latency SLA question
**Date:** 2026-07-20

---

Hi team,

We run a per-minute REST pull of the just-closed 1-minute candle for index
spots and option chains. We have measured a consistent publication lag on
`/v2/charts/intraday` for the FIRST minutes of the session, and would like
to understand the intended publication SLA.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban |
| UCC | `NWXF17021Q` |
| Date of incident | 2026-07-20 (IST) |
| Time of incident | 09:16:01–09:18:xx IST (open-minute window) |

---

## What we tested / observed

Every minute at T+~1.0s after the minute close, we POST
`/v2/charts/intraday` with `interval: "1"` for the just-closed minute of
the index spot instruments below (one bounded request per instrument, well
inside the Data-API 5/sec budget; option chains ride the option-chain API
on its own 1-per-3s-per-key pacing). On 2026-07-20:

| Contract | SecurityId | Requested minute (IST) | Pull instant (IST) | Outcome |
|---|---|---|---|---|
| `NIFTY 50 (IDX_I spot)` | `13` | `2026-07-20 09:15` | `2026-07-20 09:16:01.3` | HTTP 200, ZERO rows for 09:15 |
| `NIFTY BANK (IDX_I spot)` | `25` | `2026-07-20 09:15` | `2026-07-20 09:16:01.3` | HTTP 200, ZERO rows for 09:15 |
| `SENSEX (IDX_I spot)` | `51` | `2026-07-20 09:15` | `2026-07-20 09:16:01.3` | HTTP 200, ZERO rows for 09:15 |
| `NIFTY 50 (IDX_I spot)` | `13` | `2026-07-20 09:16` | `2026-07-20 09:17:01.3` | HTTP 200, ZERO rows for 09:16 |
| `NIFTY 50 (IDX_I spot)` | `13` | `2026-07-20 09:17` | `2026-07-20 09:18:01.3` | HTTP 200, rows present (clean from here on) |

**URL used:**

```
POST https://api.dhan.co/v2/charts/intraday
```

**Request JSON (shape — securityId varies per row above):**

```json
{
  "securityId": "13",
  "exchangeSegment": "IDX_I",
  "instrument": "INDEX",
  "interval": "1",
  "fromDate": "2026-07-20 09:15:00",
  "toDate": "2026-07-20 09:16:00"
}
```

**Response captured (the empty class):**

```
HTTP 200 — {"open":[],"high":[],"low":[],"close":[],"volume":[],"timestamp":[]}
```

---

## Key observation

The empty answers are NOT transport failures — they are clean HTTP 200
bodies with zero candles for a minute that has already closed. The lag is
concentrated at the session open (~2 minutes), then disappears:

| Timestamp (IST) | Event | Value |
|---|---|---|
| `09:16:01.3` | pull of the 09:15 bar | 200-empty |
| `09:17:01.3` | pull of the 09:16 bar | 200-empty |
| `09:18:01.3` | pull of the 09:17 bar | rows present |
| steady state (09:18–15:30) | per-pull first-usable-answer latency after minute close | p50 ≈ **1.03 s** |

For comparison, a second data vendor's equivalent 1-minute endpoint served
the SAME minutes with a 38–276 ms publication latency, including the
09:15/09:16 open minutes — so the open-minute gap is specific to this API's
publication pipeline, not to the exchange feed itself.

---

## What works on the same account / same minute

| Surface | Endpoint | Status | Evidence |
|---|---|---|---|
| Intraday REST, steady state | `api.dhan.co/v2/charts/intraday` | Working | clean per-minute pulls 09:18–15:30 IST, p50 ~1.03s |
| Option Chain REST | `api.dhan.co/v2/optionchain` | Working | same-session per-minute chain pulls succeed |
| Auth / profile | `api.dhan.co/v2/profile` | Working | token valid the whole session |

(The same request shape, token and IP succeed from 09:18 onward — account,
token and network are ruled out.)

---

## Verbatim log lines (for grep)

```json
{"timestamp":"2026-07-20T09:16:01.312+05:30","level":"INFO","target":"tickvault_app::dhan_cadence_executor","fields":{"stage":"empty_no_rows","security_id":13,"requested_minute_ist":"09:15","message":"dhan spot 1m fetch: HTTP 200 with zero candles for the requested minute"}}
{"timestamp":"2026-07-20T09:17:01.298+05:30","level":"INFO","target":"tickvault_app::dhan_cadence_executor","fields":{"stage":"empty_no_rows","security_id":13,"requested_minute_ist":"09:16","message":"dhan spot 1m fetch: HTTP 200 with zero candles for the requested minute"}}
{"timestamp":"2026-07-20T09:18:01.351+05:30","level":"INFO","target":"tickvault_app::dhan_cadence_executor","fields":{"stage":"ok","security_id":13,"requested_minute_ist":"09:17","rows":1,"message":"dhan spot 1m fetch: candle present"}}
```

---

## Requests to Dhan engineering

1. **What is the intended publication SLA for `/v2/charts/intraday` 1-minute candles after a minute closes?** Steady-state we measure ~1.0s p50 — is that the design target?
2. **Is the ~2-minute publication lag for the FIRST session minutes (09:15/09:16 IST bars, queryable only from ~09:18) expected behavior of the candle-build pipeline at open, or a defect?** Please check the server-side build timeline for `securityId 13 / IDX_I / interval "1"` on 2026-07-20 09:15–09:18 IST.
3. **Is there an earliest-guaranteed-availability instant** (e.g. "minute M is queryable by M+1 minute + X seconds") we can design retry timing against?
4. **Does the open-minute lag also apply to `/v2/optionchain`** first-minute snapshots, or only to the intraday candle store?
5. **Is there any account-level or plan-level setting** on client `1106656882` that affects intraday candle availability latency?

---

We are happy to run any diagnostic tests you need — repeated pulls at
sub-second offsets across the open window, different SIDs/segments, tcpdump
of the exchange, or a fixed test window with your engineering watching
server-side. This affects our per-minute live decision pipeline at the most
important minutes of the day (the open), so a definitive SLA answer lets us
pin our retry design instead of guessing.

Thank you,
**Parthiban**
Client ID: `1106656882`
