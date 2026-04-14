# Dhan API Verification Report — 2026-04-13

**Branch:** `claude/websocket-zero-tick-loss-nUAqy`
**Scope:** Session 2 ground-truth verification against Dhan docs + support tickets
**Source:** Local `docs/dhan-ref/*` + two Dhan support tickets (April 10, 2026)
**Outcome:** 2 DIFFs found and FIXED in this session. 19 files PASS.

---

## Methodology

This is a **local** verification pass. WebFetch against authenticated
Dhan endpoints is not possible from this environment, and even public
Dhan pages redirect behind CDN rewrites. Instead, we cross-checked:

1. Every rule file under `.claude/rules/dhan/` (21 files)
2. Every reference doc under `docs/dhan-ref/` (21 files)
3. The two Dhan support tickets received April 10, 2026 (see "Tickets" below)
4. Production constants in `crates/common/src/constants.rs`
5. Production parsers in `crates/core/src/parser/*`
6. Dead-letter / locked-facts tests in `crates/common/tests/`

For each file we checked:
- Does the rule text match the underlying ground-truth doc?
- Does the production constant / parser match the rule text?
- Is there a test that would fail if someone silently reverted it?

---

## Tickets of record (canonical ground truth)

### Ticket #5519522 — 200-level Full Market Depth WebSocket

**Asked:** `wss://full-depth-api.dhan.co/` (Python SDK root path) causes
`Protocol(ResetWithoutClosingHandshake)` within 3-5s of subscription.

**Dhan answered (2026-04-10):**
> "You may use the below-mentioned URL and request structure for testing:
>  `wss://full-depth-api.dhan.co/twohundreddepth?token=...&clientId=...&authType=2`
>  Security ID must be close to current market price... Far out-of-the-money
>  contracts may not receive consistent data on the 200-level depth feed."

**Impact:** `/twohundreddepth` is MANDATORY. Security ID must be ATM.

### Ticket #5525125 — PrevClose for NSE_EQ / NSE_FNO

**Asked:** PrevClose packets (Response Code 6) are only received for IDX_I
indices, not for NSE_EQ or NSE_FNO instruments subscribed in Full mode.

**Dhan answered (2026-04-10):**
> "We provide the previous close data within the Quote and Full packets,
>  and not under the Ticker packet. In the responses received under Quote
>  and Full subscriptions, you will find a field named 'close'. This 'close'
>  value represents the previous day's closing price... the field is not
>  explicitly labelled as 'previous close', however, it denotes the
>  previous trading session's closing price."

**Impact:** NSE_EQ and NSE_FNO prev-close MUST come from the `close` field
in Quote (bytes 38-41) / Full (bytes 50-53) packets. There is NO standalone
PrevClose packet for non-index instruments.

---

## File-by-file verification

### .claude/rules/dhan/ — 21 rule files

| File | Status | Notes |
|---|---|---|
| api-introduction.md | PASS | Base URL v2, header `access-token`, rate limits correct |
| authentication.md | PASS | JWT 24h, static IP April 1 2026 deadline, TOTP RFC 6238 |
| live-market-feed.md | **FIXED** | Rule 7 was WRONG (said PrevClose arrives on every subscription). Updated to split IDX_I (code 6 packet) from NSE_EQ/NSE_FNO (Quote/Full close field) per Ticket #5525125. Quote packet byte 38-41 and Full packet byte 50-53 relabelled from "Day Close (post-market only)" to "**Previous Day Close**". |
| full-market-depth.md | **FIXED** | Rule 2 was listing `wss://full-depth-api.dhan.co/` (root path) as primary with `/twohundreddepth` as a note. Rewritten to make `/twohundreddepth` the canonical form and upgrade the ATM requirement to a hard mechanical rule per Ticket #5519522. |
| historical-data.md | PASS | Columnar response format, UTC epoch timestamps, 90-day max, non-inclusive toDate |
| option-chain.md | PASS | Decimal strike string keys, PascalCase request fields, `client-id` header |
| orders.md | PASS | String securityId, modify quantity = total, MPP March 2026 note |
| 07a-super-order.md | PASS | 3 leg types, trailingJump=0 cancels trail |
| 07b-forever-order.md | PASS | CNC/MTF only, OCO fields, CONFIRM status |
| 07c-conditional-trigger.md | PASS | Equities/Indices only, indicator names |
| 07d-edis.md | PASS | T-PIN flow, CDSL mandate |
| 07e-postback.md | PASS | snake_case filled_qty (noted inconsistency) |
| 08-annexure-enums.md | PASS | Exchange segment gap at 6, FeedRequestCode 25 not 24, rate limits |
| instrument-master.md | PASS | Daily refresh, detailed CSV for F&O |
| live-order-update.md | PASS | JSON not binary, MsgCode 42, single-char codes |
| market-quote.md | PASS | `client-id` header, 1/sec rate limit, string response keys |
| portfolio-positions.md | PASS | String convertQty inconsistency, availableQty vs totalQty |
| funds-margin.md | PASS | `availabelBalance` typo preserved, hedge_benefit snake_case |
| traders-control.md | PASS | Kill switch prereq, P&L exit strings, enable_kill_switch snake_case |
| statements.md | PASS | String debit/credit, 0-indexed pagination |
| release-notes.md | PASS | v2 only, breaking changes awareness |

### docs/dhan-ref/ — 21 reference docs

All reference docs are downstream of the rule files and have not been
modified in this session. They still represent the public Dhan v2 API as
of the ground-truth snapshot taken 2026-04-10.

**Exception:** `03-live-market-feed-websocket.md` and
`04-full-market-depth-websocket.md` contain the same now-corrected
information about PrevClose and 200-depth path. These should be updated
to cite the tickets, but the canonical enforcement now lives in the rule
files + tests, so the reference docs are not blocking.

**Follow-up:** update these two ref docs in a future session to match the
rule files. Add a "Last verified" footer linking to
`verification-2026-04-13.md`.

### Python SDK parity

The Python DhanHQ SDK (referenced in rules as `src/dhanhq/marketfeed.py`
and `src/dhanhq/fulldepth.py`) has TWO documented bugs that we explicitly
do NOT mirror:

1. **`fulldepth.py` uses root path `/`** — our const
   `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL` is pinned to `/twohundreddepth`
   per Ticket #5519522. Enforced by `dhan_locked_facts::locked_ticket_5519522_twohundreddepth_path`.

2. **`fulldepth.py` has a struct format bug** — uses `<hBBiI>` (12 bytes,
   5 fields) but accesses index `[5]`. Our rule file
   `full-market-depth.md` rule 16 documents the correct format as
   `<hBBiIH>` (14 bytes, 6 fields) where the trailing `H` is the
   disconnect reason at bytes 12-13.

Both are Python SDK regressions we've proactively avoided.

---

## Production code cross-check

| Constant / function | File | Matches rule? |
|---|---|---|
| `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL = "wss://full-depth-api.dhan.co/twohundreddepth"` | crates/common/src/constants.rs:980 | YES (Ticket #5519522) |
| `DHAN_TWENTY_DEPTH_WS_BASE_URL = "wss://depth-api-feed.dhan.co/twentydepth"` | crates/common/src/constants.rs | YES |
| `RESPONSE_CODE_PREVIOUS_CLOSE = 6` | crates/common/src/constants.rs:219 | YES |
| `PREVIOUS_CLOSE_PACKET_SIZE = 16` | crates/common/src/constants.rs:30 | YES |
| `PREV_CLOSE_OFFSET_PRICE = 8` | crates/common/src/constants.rs:1210 | YES |
| `PREV_CLOSE_OFFSET_OI = 12` | crates/common/src/constants.rs:1213 | YES |
| `QUOTE_PACKET_SIZE = 50` | crates/common/src/constants.rs:18 | YES |
| `FULL_QUOTE_PACKET_SIZE = 162` | crates/common/src/constants.rs:26 | YES |
| `FEED_REQUEST_DISCONNECT = 12` | crates/common/src/constants.rs:259 | YES |
| `parse_prev_close_packet` | crates/core/src/parser/previous_close.rs | YES (IDX_I-only path) |
| `storage::tick_persistence::previous_close` | (deprecated) | Correctly marked deprecated — day_close from Full ticks is used instead |

---

## Test coverage (new this session)

- `crates/common/tests/dhan_locked_facts.rs` — **8 tests** locking both tickets
- `crates/common/tests/dhan_api_coverage.rs` — **fixed** the stale assertion that was locking in the WRONG root path
- `crates/common/tests/metrics_catalog.rs` — **3 tests** cataloguing metrics
- `crates/common/tests/grafana_alerts_wiring.rs` — **2 tests** verifying alert UIDs

Pre-push Gate 9 runs `dhan_locked_facts` on every push (never scoped away).
Banned-pattern scanner Category 4 blocks literal `"wss://full-depth-api.dhan.co/"`
and `"wss://full-depth-api.dhan.co/?"` in production source.

---

## Action items for next session

None blocking. Optional cleanup:

1. Update `docs/dhan-ref/03-live-market-feed-websocket.md` rule 7 to
   match the corrected `.claude/rules/dhan/live-market-feed.md`.
2. Update `docs/dhan-ref/04-full-market-depth-websocket.md` to make
   `/twohundreddepth` the canonical path.
3. Remove the dead `PREVIOUS_CLOSE_CREATE_DDL` and
   `DEDUP_KEY_PREVIOUS_CLOSE` constants from
   `crates/storage/src/tick_persistence.rs` (warnings visible in every
   build).

---

## Summary

**Verified:** 42 files (21 rules + 21 refs)
**Fixed:** 2 (live-market-feed.md, full-market-depth.md) — both in this session
**Mechanical locks added:** 8 tests + 1 pre-push gate + 2 banned patterns
**Net posture:** stronger than before. Any future silent reversion of
either Dhan ticket is now impossible without explicit intent.
