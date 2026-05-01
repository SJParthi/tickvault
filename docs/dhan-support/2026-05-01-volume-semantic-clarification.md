<!--
Dhan support draft per docs/dhan-support/TEMPLATE.md
Topic: official wire-format spec request for Volume field semantic in Quote/Full packets
Created: 2026-05-01 (NSE Labour Day — drafted offline)
Reference: Wave 5 Item 26 L3 (active-plan-wave-5-indices-only.md)
-->

# Volume Field Semantic in Quote/Full Packets — Wire Format Specification Request

**To:** apihelp@dhan.co
**From:** <OPERATOR_EMAIL>
**Subject:** New Ticket — Live Market Feed: official wire-format spec for Volume field semantic
**Date:** 2026-05-01

---

Hi Dhan API Support team,

We're requesting an official wire-format specification for the `Volume` field returned in your Live Market Feed Quote and Full packets. Our system has internally treated this field as cumulative-since-session-open based on industry convention, but we have not yet seen explicit confirmation in your v2 documentation. We want to lock this assumption with an authoritative answer before pinning multiple downstream calculations to it.

| Field | Value |
|---|---|
| Client ID | `1106656882` |
| Name | Parthiban |
| UCC | `NWXF17021Q` |
| Topic | Live Market Feed binary protocol — Volume field semantic |
| Reference docs | https://dhanhq.co/docs/v2/live-market-feed/ |
| Reference SDK | https://github.com/dhan-oss/DhanHQ-py/blob/main/src/dhanhq/marketfeed.py (sha 17e64b7bae58e3a0f3b5ef4ee41969ad25524e89) |

---

## What we observed

Our Rust parser (`crates/core/src/parser/quote.rs`, `crates/core/src/parser/full_packet.rs`) reads the Volume field at the byte offsets documented in your v2 Live Market Feed reference:

| Packet | Response Code | Byte offset | Type | Field |
|---|---|---|---|---|
| Quote | 4 | 22-25 | uint32 LE | Volume |
| Full | 8 | 22-25 | uint32 LE | Volume |
| OI (separate, on F&O Quote/Full subscriptions) | 5 | 8-11 | uint32 LE | Open Interest |

Our parser is bit-identical to your official Python SDK's `process_quote()` `<BHBIfHIfIIIffq>` struct format, confirmed via cross-reference of the same byte offset.

The v2 documentation labels the field as **"Volume"** with description **"Total traded volume"**. We want to pin the precise semantic of "Total" so we can correctly compute downstream aggregations (1-second / 5-minute / 1-day candles, top-volume rankings, percentage change versus previous session, NSE bhavcopy cross-check).

---

## Specific questions (please answer Yes/No to each)

| # | Question | Our assumption (please confirm or correct) |
|---|---|---|
| 1 | Is the `Volume` value at Quote bytes 22-25 / Full bytes 22-25 **cumulative** total traded volume since the start of the current trading session? | YES |
| 2 | If yes — does the cumulative window start at **09:15:00 IST** (NSE F&O regular session open) for NSE_FNO contracts? | YES |
| 3 | Is the `Volume` value **monotonically non-decreasing** within a single trading day for any given `(security_id, exchange_segment)`? | YES |
| 4 | Does the cumulative reset to **0 at the next session open** (09:15:00 IST next trading day, after the 15:30 IST close)? | YES |
| 5 | At 15:30:00 IST close, does the final `Volume` value equal NSE's officially published daily total volume (matchable against the bhavcopy `TtlTradgVol` column)? | YES |
| 6 | Same five questions for the `Open Interest` field at Full bytes 34-37 — is OI a **point-in-time current value** (NOT cumulative; can decrease as positions unwind)? | YES (point-in-time) |
| 7 | For pre-open auction window (09:00:00 - 09:14:59 IST), do you publish `Volume = 0` consistently, or is there auction-window volume reported? | We assume `Volume = 0` until 09:15:00 IST first regular trade |

---

## Why this matters for our system

We're building a candle aggregation pipeline (1s base table, materialized views at 5s/15s/30s/1m/5m/15m/30m/1h/1d) and a top-movers ranking service. If `Volume` is cumulative-since-09:15 IST as we assume:

- 1-second candle volume = `last_cumulative_in_second - first_cumulative_in_second` (correct via monotonicity)
- 5-minute candle volume = `last_cumulative_in_5min - first_cumulative_in_5min` (correct via monotonicity)
- Daily total = final `Volume` value at 15:30 IST = NSE bhavcopy `TtlTradgVol` (cross-checkable)

If the field is per-tick incremental (NOT cumulative), every one of those formulas is wrong. We want to ship the right design first.

---

## Reference: previous tickets that anchored similar wire-format clarifications

| Ticket | Topic | Outcome |
|---|---|---|
| #5525125 | Previous-day close routing in Quote bytes 38-41 / Full bytes 50-53 | Confirmed — used in `previous_close_persistence.rs` |
| #5519522 | Depth-200 endpoint URL pattern | Resolved (endpoint changed to root path) |
| (this ticket) | Volume field semantic | (pending) |

---

## What we'd like back

A single Yes/No answer to each of the 7 questions above, ideally with a one-line citation to your internal spec / NSE convention reference.

If any answer is "No", please describe the actual semantic so we can re-design before deployment.

If a written confirmation is not feasible from your end, a verbal confirmation in our next support call works too — we just want it recorded.

---

## Acknowledgement format

Once we have your reply, we will pin it as an authoritative source in `.claude/rules/dhan/live-market-feed.md` rule 8 / 10 of our internal codebase, and add a runtime monotonicity guard (per Wave 5 Item 26 of our active plan) that fires CRITICAL alerts if any tick ever violates the confirmed semantic.

This protects both sides: our system catches any unintended Dhan-side wire format change immediately; your side gets a partner who flags wire-format bugs within seconds rather than weeks.

Thank you for your time.

Best regards,
Parthiban Subramanian
Tickvault — O(1) latency live F&O trading system for NSE
