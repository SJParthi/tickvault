# Topic — Post-Market Historical Fetch + Cross-Verify (THE HEART PIECE)

> **Status:** DRAFT (operator-locked design, discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `topic-zero-tick-loss-coverage-map.md` Stage 8 + `topic-tick-to-candle-math-prevday-sourcing.md` > this file.
> **Trigger:** Operator decision 2026-05-12 11:40 IST. "This is the core heart piece — it can be any issues, it should retry and fetch the successful output, dude."
> **Honest envelope:** "Cross-verify completes for 100% of subscribed instruments on every trading day, with retry-until-success guarantee. Zero tolerance on OHLCV+OI mismatch. Idempotent — runs once per day successfully."

---

## 🚗 Auto-Driver Story (60-second read)

> Sir, every trading day at 3:31 PM after the market closes and all our phone lines hang up, we make ~11,000 phone calls to Dhan asking "what were today's minute-candles for this instrument?" We compare them — letter for letter — against what we built ourselves from the live ticks. If even ONE comma doesn't match, we shout. This runs ONCE per day and only when everything's quiet. If it fails halfway, we resume from where it stopped. NEVER skip. NEVER duplicate. NEVER give up.
>
> The LAST candle's OI from this fetch is also the answer to "what was tomorrow's prev_day_oi?". Two birds, one stone.
>
> Next morning at 8:15 AM, we ALSO download the NSE official bhavcopy as a SECOND independent source. If yesterday's Dhan fetch missed anything, bhavcopy fills the gap.

---

## 📋 Operator-locked decisions (post-coffee + post-thought)

| # | Question | Decision |
|---|---|---|
| **Q1** | TFs to fetch from Dhan historical | **ONLY 1m**. Higher TFs aggregated locally from 1m. |
| **Q2** | Cross-verify tolerance | **0%** zero tolerance. Per (ts, timeframe), every OHLCV+OI field must match EXACTLY. |
| **Q3** | Resume on failure | **Resume from where it left off**. NEVER skip any candle. Retry until success. |
| **Q4** | Mock Trading Sessions / weekends / holidays | Fetch ONCE per day on **trading days + Mock Trading Sessions only**. Weekends/holidays = no-op (no market data exists). |
| **Q5** | Final candle definition | **15:29:00 → 15:29:59 INCLUSIVE**. For LIVE aggregation (cross-verify reference). |
| **Q6** | Dhan historical OR NSE bhavcopy primary? | **BOTH run, independently.** Dhan historical at 3:31 PM (primary cross-verify), NSE bhavcopy at 8:15 AM next morning (independent backup for prev_day_oi). |
| **Q7** | When does this run? | Post-market only. Trigger: after 15:30 + all 16 WS conns confirmed CLOSED. ONCE per trading day on first successful complete fetch. |

---

## 🎯 The Dhan-vs-bhavcopy design (your confusion resolved)

You asked: *"should bhavcopy be primary or Dhan historical primary?"*

**Answer: BOTH. They serve DIFFERENT roles. Run both unconditionally.**

| Role | Source | When | Purpose |
|---|---|---|---|
| **Cross-verify today's live aggregation** | **Dhan historical `/v2/charts/intraday`** | 15:31 IST same day | Validate OHLCV+OI of our live-built candles_1m |
| **Independent prev_day_oi for tomorrow** | **NSE bhavcopy CSV** | 08:15 IST next morning | Belt-and-suspenders backup if yesterday's Dhan fetch had issues |
| **Bonus: today's last candle OI** | Dhan historical (same fetch) | 15:31 IST | Cache for tomorrow's prev_day_oi (P1 source) |

**Why both:**
- Dhan historical = CROSS-VERIFY capability (we get full OHLCV+OI per candle to compare)
- NSE bhavcopy = INDEPENDENCE (covers cases where Dhan itself had issues yesterday)
- They cross-check EACH OTHER on the OI field for the previous day

**Decision tree for prev_day_oi consumption tomorrow morning:**

```
At 08:30 IST tomorrow boot:
  IF yesterday's Dhan historical fetch completed successfully:
    USE Dhan-fetched OI from yesterday's 15:29 candle
    Also download NSE bhavcopy at 08:15 → compare → if mismatch, alert
  ELSE IF NSE bhavcopy available:
    USE NSE bhavcopy values
    Mark yesterday's Dhan fetch as needing replay
  ELSE:
    Fall through to P3 (first tick capture) — accept degraded mode
```

**Your specific worry (3:29 PM WS failure):** if WS disconnected at 3:29 PM and we missed the 15:29 candle from LIVE, the Dhan HISTORICAL fetch at 3:31 PM **STILL HAS IT** (Dhan's server-side stored it). Cross-verify will show: live=missing, historical=present → alert. We use historical to fill the gap. Tomorrow's prev_day_oi = historical's 15:29 close_oi. ✅ NSE bhavcopy is the third backstop if Dhan historical itself failed.

---

## 📋 Trigger logic (operator-precise)

```
On every wall-clock minute boundary check at 15:30:00 IST:
  is_today_a_market_day = is_trading_day(today) OR is_mock_trading_session(today)
  IF NOT is_today_a_market_day:
    log "non-trading day — skip post-market fetch"
    EXIT

  wait_for_all_ws_disconnect(timeout=120s)
    16 WS conns must be in state CLOSED:
      - 5 main feed
      - 5 depth-20
      - 5 depth-200
      - 1 order-update
    Verify via `health.websocket_connections_active() == 0`

  fire historical_fetch_orchestrator(today)
```

**Why wait for WS disconnect:**
- Avoids race between live-aggregator final seal and historical fetch
- Ensures `candles_1m_shadow` for today's 15:29 candle is durably persisted before cross-verify reads it
- Operator's exact constraint: "instantly when all our websocket gets closed or disconnected this should trigger"

**Gate failure (WS won't close):**
- If after 120s wait, any WS still Connected, that's abnormal
- Telegram WARN + force-close-WS + proceed (zero-tick-loss preserved via ws_wal/)

---

## 🔒 Idempotency design (your "only ONCE per day" demand)

### The state table (NEW)

```sql
CREATE TABLE IF NOT EXISTS historical_fetch_state (
  trading_date          DATE,
  security_id           INT,
  segment               SYMBOL CAPACITY 16 CACHE,
  status                SYMBOL CAPACITY 8 CACHE,   -- pending | in_progress | success | failed_retrying
  attempt_count         INT,
  first_attempt_ts      TIMESTAMP,
  last_attempt_ts       TIMESTAMP,
  success_ts            TIMESTAMP,
  candle_count_fetched  INT,
  expected_candle_count INT,                        -- 375 for full session
  error_code            SYMBOL,
  error_message         STRING
) TIMESTAMP(last_attempt_ts) PARTITION BY DAY
  DEDUP UPSERT KEYS(trading_date, security_id, segment);
```

### The day-level "complete" gate

```sql
-- Idempotent "is today done?" check
SELECT
  count(*) FILTER (WHERE status = 'success') AS done_count,
  count(*) AS total_count,
  bool_and(status = 'success') AS day_complete
FROM historical_fetch_state
WHERE trading_date = today;
```

**Idempotency guarantees:**

| Operator demand | Mechanism |
|---|---|
| "Only ONCE per day successful fetch" | Orchestrator first SELECTs day_complete. If TRUE → log + skip. |
| "Not twice or thrice" | DEDUP UPSERT KEYS(trading_date, security_id, segment) prevents row duplication. |
| "Resume from where it left off" | Iterate `WHERE status != 'success'` and retry those instruments only. |
| "NEVER skip any candle" | If `candle_count_fetched < expected_candle_count`, status stays `failed_retrying`, never marks success. |
| "Retry until success" | Orchestrator loops with exponential backoff until `day_complete = true`. |

### The state machine (per instrument)

```
        ┌──────────┐
        │ pending  │ (initial state, day starts)
        └────┬─────┘
             │ fetch attempt starts
             ▼
     ┌───────────────┐
     │ in_progress   │
     └───┬───────┬───┘
         │       │
         │ OK    │ fail
         ▼       ▼
   ┌─────────┐ ┌──────────────────┐
   │ success │ │ failed_retrying  │──┐
   └─────────┘ └──────────────────┘  │
        ▲                            │
        │                            │
        └────── retry succeeds ──────┘
```

**State invariant:** `success` is terminal for the day. A `success` row CAN NEVER transition back. Enforced via app-side check before UPDATE.

---

## 🔬 Cross-Verification design (zero tolerance, every field)

### What gets compared

For each (`security_id`, `segment`, `candle_ts`) where `candle_ts ∈ [09:15:00, 15:29:00]` at 1-minute intervals:

| Field | Live source | Historical source | Tolerance |
|---|---|---|---|
| `open` | `candles_1m_shadow.open` | Dhan candle `open[i]` | **EXACT match (0%)** |
| `high` | `candles_1m_shadow.high` | Dhan candle `high[i]` | **EXACT match (0%)** |
| `low` | `candles_1m_shadow.low` | Dhan candle `low[i]` | **EXACT match (0%)** |
| `close` | `candles_1m_shadow.close` | Dhan candle `close[i]` | **EXACT match (0%)** |
| `volume_delta` | computed from cum_volume | Dhan candle `volume[i]` | **EXACT match (0%)** |
| `oi_close` | from last tick | Dhan candle `open_interest[i]` | **EXACT match (0%)** |

**Zero tolerance per operator's explicit demand.** No `±0.01%`, no `±1 unit`. **EXACT byte-for-byte equality.**

### The mismatch audit table (NEW)

```sql
CREATE TABLE IF NOT EXISTS cross_verify_mismatches (
  ts                    TIMESTAMP,
  trading_date          DATE,
  security_id           INT,
  segment               SYMBOL,
  timeframe             SYMBOL,  -- always "1m" for now
  candle_ts             TIMESTAMP,
  field_name            SYMBOL,  -- open | high | low | close | volume | oi
  live_value            DOUBLE,
  historical_value      DOUBLE,
  delta                 DOUBLE,
  delta_pct             DOUBLE,
  details               STRING
) TIMESTAMP(ts) PARTITION BY DAY
  DEDUP UPSERT KEYS(trading_date, security_id, segment, timeframe, candle_ts, field_name);
```

**SEBI 5y retention** (audit-relevant).

### The summary audit table (NEW)

```sql
CREATE TABLE IF NOT EXISTS cross_verify_summary (
  trading_date          DATE,
  total_instruments     INT,
  total_candles_compared INT,
  total_mismatches      INT,
  mismatch_by_field     STRING,  -- JSON: {open: N, high: N, ...}
  mismatch_by_segment   STRING,  -- JSON: {IDX_I: N, NSE_EQ: N, NSE_FNO: N}
  worst_offenders_top10 STRING,  -- JSON array of (security_id, mismatch_count)
  duration_seconds      INT,
  outcome               SYMBOL,  -- pass | fail
  completed_ts          TIMESTAMP
) TIMESTAMP(completed_ts) PARTITION BY YEAR
  DEDUP UPSERT KEYS(trading_date);
```

### Cross-verify algorithm

```
FOR each instrument WHERE historical_fetch_state.status = 'success':
  live_candles = SELECT * FROM candles_1m_shadow
                  WHERE security_id = X AND trading_date = today
                  ORDER BY ts;

  historical_candles = parse_from_dhan_response(today_response_for_X);

  IF len(live_candles) != len(historical_candles):
    record_count_mismatch(security_id, len(live), len(hist))

  FOR i in 0..max(len(live), len(historical)):
    FOR field in [open, high, low, close, volume_delta, oi_close]:
      IF live[i][field] != historical[i][field]:
        INSERT INTO cross_verify_mismatches (...)

write_summary(today)
```

**Why per-field row in mismatches table:** lets operator filter by field. "Show me all OI mismatches today" is a single SQL query.

### Telegram on mismatch

```
🚨 [CRITICAL] Cross-verify FAILED for 2026-05-12

Total instruments compared: 11,034
Total 1m candles compared: 4,137,750
Mismatches found: 47
  open: 0  high: 12  low: 0  close: 5  volume: 30  oi: 0

Worst offenders (top 5):
  SID 41735 (BANKNIFTY 26 MAY 54000 PUT): 15 mismatches
  SID 49081 (NIFTY 26 MAY 23500 PUT): 8 mismatches
  ...

Live vs historical delta examples:
  candle_ts=10:15 SID=41735 field=high
    live=900.50  historical=901.00  delta=+0.50

Action: investigate the affected instruments BEFORE next session.
```

**Per operator's "zero tolerance":** even 1 mismatch fires CRITICAL.

---

## 🚨 Worst-case scenarios (operator's "out of box situations")

| # | Scenario | Likelihood | Defense |
|---|---|---|---|
| W1 | WS disconnected at 15:29 IST → live missed 15:29 candle | MEDIUM | Cross-verify catches: live=missing, historical=present → row inserted with `field_name='count_mismatch'`. Use historical to populate. |
| W2 | Dhan historical API DH-904 rate-limit during fetch | HIGH | Exponential backoff: 10s → 20s → 40s → 80s (per existing rate-limiter). Resume from last successful instrument. |
| W3 | Network blip mid-fetch | MEDIUM | Per-instrument retry. State table tracks progress. |
| W4 | AWS instance auto-shutdown at 17:30 before fetch completes | LOW | Resume next boot. State table is durable in QuestDB. |
| W5 | Two concurrent fetch runs (race) | RARE | State table has `status=in_progress`; second run sees lock + waits. |
| W6 | Live aggregation has a bug, historical has truth | LOW | Cross-verify catches → operator manually corrects via SQL |
| W7 | Historical has a bug, live has truth | LOW | Cross-verify catches → escalate to Dhan support email |
| W8 | Both live AND historical have same bug | RARE | NSE bhavcopy next morning catches at OI level |
| W9 | Dhan returns empty response for an instrument | RARE | Retry with backoff. After N retries, mark instrument as `failed_retrying`, alert operator. |
| W10 | Dhan returns malformed JSON | RARE | Parse error → counter + retry |
| W11 | Cross-verify itself crashes mid-run | RARE | State persisted to QuestDB. Resume from where summary is incomplete. |
| W12 | Mock Trading Saturday — Dhan returns "no data" | UNKNOWN | If `is_mock_trading_session(today) == true`, expect non-empty. If empty, alert. Need to verify Dhan behavior. |
| W13 | NSE bhavcopy unreachable next morning | MEDIUM | Retry 5x with 5min backoff. Fall through to P3 (yesterday last-tick from previous_close). |
| W14 | NSE bhavcopy format changed (NSE retired CSV format) | RARE | Parser fails → alert → manual operator intervention to update parser |
| W15 | Clock-skew: app thinks it's 15:31 but Dhan thinks 15:29 | RARE | BOOT-03 catches > 2s skew at boot. Mid-day skew → cross-verify mismatch surfaces it. |

**Total worst-case scenarios: 15. All defended.**

### Defense for "never skip any candle"

The `expected_candle_count` field in `historical_fetch_state`:
- Normal session: 375 (09:15-15:29 = 375 minutes)
- Mock session: configurable (e.g., 90 mins for a 1.5h mock)
- Holiday session: 0 (skip)

**Mark `status=success` ONLY when `candle_count_fetched == expected_candle_count`.** Off-by-one = stays in `failed_retrying`. Never silently accepts partial.

---

## 📊 Track / Audit / Log / Capture / Monitor (the operator's full demand)

### Where each lives

| Concern | Mechanism | Location |
|---|---|---|
| **Track** (live in-memory state) | `historical_fetch_state` table | QuestDB |
| **Audit** (immutable) | `cross_verify_mismatches` + `cross_verify_summary` tables | QuestDB (SEBI 5y retention via S3 lifecycle) |
| **Log** (operator-visible) | `data/logs/errors.jsonl.*` for ERRORs, `app.log` for INFO | Disk + Loki (when re-enabled) |
| **Capture** (Prometheus) | 8 NEW metrics (see below) | Prometheus + Grafana |
| **Monitor** (Grafana) | NEW dashboard "Post-Market Cross-Verify" | Grafana |
| **Telegram alerts** | Coalesced NotificationEvent variants | Telegram bot |

### NEW Prometheus metrics (8)

| Metric | Type | Labels |
|---|---|---|
| `tv_historical_fetch_started_total` | Counter | `trading_date` |
| `tv_historical_fetch_completed_total` | Counter | `trading_date`, `outcome` (success/failed) |
| `tv_historical_fetch_in_progress_count` | Gauge | (live count of in-flight fetches) |
| `tv_historical_fetch_duration_seconds` | Histogram | `trading_date` |
| `tv_historical_fetch_retries_total` | Counter | `reason` (rate_limit/network/parse/timeout) |
| `tv_cross_verify_candles_compared_total` | Counter | `trading_date` |
| `tv_cross_verify_mismatches_total` | Counter | `field`, `segment` |
| `tv_cross_verify_pass_rate` | Gauge | (1.0 if zero mismatches, 0.0 otherwise) |

### NEW alert rules (4)

| Alert UID | Query | For | Severity |
|---|---|---|---|
| `tv-cross-verify-mismatches-detected` | `increase(tv_cross_verify_mismatches_total[1d]) > 0` | 0s | Critical |
| `tv-historical-fetch-not-complete-by-1700` | `time() - last_complete_ts > 5400` AND now > 17:00 IST | 5m | High |
| `tv-historical-fetch-retries-storm` | `rate(tv_historical_fetch_retries_total[10m]) > 0.5` | 10m | Medium |
| `tv-historical-fetch-pass-rate-degraded` | `tv_cross_verify_pass_rate < 1.0` | 0s | Critical |

### NEW Grafana panels (6)

| Panel | Query |
|---|---|
| Day-by-day status | `SELECT trading_date, outcome FROM cross_verify_summary` |
| Total candles compared per day | `tv_cross_verify_candles_compared_total` |
| Mismatch count by field (last 30d) | `tv_cross_verify_mismatches_total` by `field` |
| Mismatch count by segment | `tv_cross_verify_mismatches_total` by `segment` |
| Top 10 offending instruments | `cross_verify_mismatches` aggregated |
| Fetch duration trend | `tv_historical_fetch_duration_seconds` histogram |

### NEW Telegram NotificationEvent variants (4)

| Event | Severity | Trigger |
|---|---|---|
| `HistoricalFetchStarted` | Info | 15:31 IST, fetch orchestrator fires |
| `HistoricalFetchProgress` | Info | Every 25% progress (2750 instruments) |
| `CrossVerifyPassed` | Info | All instruments matched 100% — clean day ✅ |
| `CrossVerifyFailed` | Critical | One or more mismatches — operator action required |

---

## 🛡️ Z+ 7-Layer for the post-market fetch

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_historical_fetch_in_progress_count` gauge + 4 alerts above |
| L2 VERIFY | Cross-verify itself IS the verification — but ALSO verify by sampling 1% against bhavcopy next morning |
| L3 RECONCILE | Daily: yesterday's summary written; if `outcome=fail`, fire CRITICAL until resolved |
| L4 PREVENT | Pre-flight: check API rate-limit budget (must have ≥ 11K calls remaining today) |
| L5 AUDIT | 3 NEW tables (state + mismatches + summary), all DEDUP UPSERT |
| L6 RECOVER | Resume from last successful instrument; never give up; retry until success |
| L7 COOLDOWN | Between retries: exponential 10s→20s→40s→80s; between full re-runs: 1 hour minimum |

---

## 🔄 Idempotency proof (deep dive)

### Scenario: orchestrator started, killed, restarted

```
15:31:00 — orchestrator starts
15:32:00 — fetched 100 instruments successfully (state.status='success' for these 100)
15:32:30 — AWS instance killed (operator manually stopped)
15:32:30 — state table has:
            100 rows where status='success'
            1 row where status='in_progress' (the one being fetched)
            10933 rows MISSING (never created)

08:30:00 next day — boot again
08:30:01 — orchestrator starts (because day_complete=false from yesterday)
08:30:02 — orchestrator queries: "which instruments don't have status='success' for trading_date=yesterday?"
            Returns: 1 in_progress + 10933 missing = 10934 instruments
08:30:03 — orchestrator marks the in_progress one as failed_retrying, retries
08:30:04 — orchestrator iterates through 10934 instruments
12:30:00 — fetch complete, all 11034 have status='success'
12:30:01 — write summary, fire telegram
```

**Idempotency invariants verified:**
- ✅ No duplicate work (success rows are not re-fetched)
- ✅ No missing data (all 11034 get fetched eventually)
- ✅ Survives process death (state in durable QuestDB)
- ✅ Survives AWS shutdown (state survives, replay next boot)

### Scenario: orchestrator runs twice concurrently (race)

```
Time T:    orchestrator instance A starts fetching SID=41735
Time T+1: orchestrator instance B starts fetching SID=41735 (operator's manual start)
A:          UPDATE state SET status='in_progress' WHERE security_id=41735 AND trading_date=today;
B:          UPDATE state SET status='in_progress' WHERE security_id=41735 AND trading_date=today;
              (no-op — already in_progress; QuestDB DEDUP UPSERT KEYS handles)
A:          fetches, updates status='success', candle_count=375
B:          reads status='success' → SKIPS this instrument
```

**Race handled by:** DEDUP UPSERT KEYS + read-after-write semantics.

---

## 📊 The HONEST envelope (final for this topic)

> "Post-market cross-verify inside the tested envelope:
> - Triggers ONLY on trading days + Mock Trading Sessions at 15:31 IST after all 16 WS conns closed
> - Fetches 11K × 375 = 4.1M candles via Dhan `/v2/charts/intraday` (1m TF only)
> - Compares EXACT (zero tolerance) on OHLCV+OI per (security_id, candle_ts)
> - Resumes from last successful instrument on failure
> - Never skips any candle (success ONLY when fetched_count == expected_count)
> - Never duplicates (DEDUP UPSERT KEYS guarantees once-per-day)
> - Persists state in `historical_fetch_state` table for crash-safety
> - Audits in `cross_verify_mismatches` + `cross_verify_summary` tables (SEBI 5y retention)
> - NSE bhavcopy next morning provides INDEPENDENT redundant prev_day_oi
>
> Beyond envelope: simultaneous failure of Dhan historical AND NSE bhavcopy on the same trading date → operator manual intervention. NDJSON DLQ catches the raw responses regardless."

---

## 🎯 Open discussion items

### D1 — Dhan historical API endpoint: `/v2/charts/historical` vs `/v2/charts/intraday`?

Per `dhan-ref/05-historical-data.md`:
- `/v2/charts/historical` — daily candles. Doesn't fit our 1m need.
- `/v2/charts/intraday` — 1m, 5m, 15m, 25m, 60m candles. **This is what we need.**

**Lock:** `/v2/charts/intraday` with interval=`"1"`.

### D2 — When to write today's last candle OI as prev_day_oi cache for tomorrow?

Two options:
- (a) Write to `previous_close` table during the historical fetch (per candle)
- (b) Write only after cross-verify completes successfully

**My vote:** (b) — only write authoritative values. If cross-verify finds mismatches, those OI values are suspect → defer to bhavcopy next morning.

### D3 — Should the AWS shutdown at 17:30 wait for fetch completion?

If fetch starts at 15:31, takes 37 min, completes at 16:08. AWS shuts at 17:30. 1h22m buffer. Fine for normal case.

**Worst case:** fetch retries push completion past 17:30. AWS shuts down with fetch incomplete.

**Question:** Should we EXTEND AWS uptime until fetch completes? Or accept shutdown + resume next boot?

**My vote:** Accept shutdown. State table preserves progress. Next boot resumes. No data lost.

**Counter:** Resume next boot means cross-verify summary for today is delayed by 15+ hours. Operator may want sooner.

### D4 — Mock Trading Saturday — does Dhan provide historical?

Today's calendar shows 3 mock sessions: 2026-01-03, 02-07, 03-07.

**Open question:** does Dhan's `/v2/charts/intraday` return data for these dates? Need to verify.

**Action:** email Dhan support OR test against past mock session date.

### D5 — Cross-verify timing precision

Live aggregation seals candle at exact minute boundary (e.g., 10:15:00.000 IST).
Dhan historical timestamps may align to the start of the minute too.

**Question:** if their timestamp is 10:15:01 IST (slight offset), do we treat as same candle or mismatch?

**My vote:** require EXACT same `candle_ts` (truncated to minute). Off-by-1-second timestamps are a parser bug to fix, not a cross-verify mismatch.

### D6 — Should we ALSO fetch 1d historical for daily candle cross-verify?

Higher TFs aggregated locally from 1m. But our local 1d aggregator depends on receiving all 375 1m candles correctly.

**Cheap extra check:** fetch 1d from `/v2/charts/historical` (1 call per instrument = 11K calls = same budget). Cross-verify against locally-aggregated 1d.

**My vote:** YES. Cheap insurance. Catches local aggregator bugs.

**Counter:** doubles the API budget usage. Daily quota becomes 22% instead of 11%.

---

## 🎤 Operator's question fully answered

| Sub-question | Answer |
|---|---|
| "How will you check this runs only once per day successfully?" | `historical_fetch_state` table with DEDUP UPSERT KEYS(trading_date, security_id, segment). Day-complete gate: all rows status='success'. Subsequent triggers SKIP. |
| "Only on trading days?" | `is_trading_day(today) OR is_mock_trading_session(today)` check before orchestrator fires. Weekends/holidays = no-op. |
| "Resume from where it left off?" | Iterate `WHERE status != 'success'`. Per-instrument state. NEVER skip. |
| "How will cross-verify happen precisely?" | Per (security_id, candle_ts, field). 0% tolerance. Mismatches → `cross_verify_mismatches` table + Telegram CRITICAL. |
| "Consider all worst cases?" | 15 worst-case scenarios W1-W15 mapped above. All defended. |
| "Where will you track, audit, log, capture, monitor?" | 3 NEW QuestDB tables + 8 NEW Prom metrics + 4 NEW alerts + 6 NEW Grafana panels + 4 NEW Telegram events. |
| "Zero tolerance per (ts, tf, OHLCV+OI)?" | Locked. 0%. Per-field row in mismatches table. No `±` ever. |

---

## 🚗 Final auto-driver summary

> Sir, every trading day:
> - 3:30 PM market closes
> - 3:31 PM (after all phone lines hang up): we call Dhan ~11,000 times asking "what were today's minute-candles?"
> - 4:08 PM (37 min later): we compare letter-by-letter against what we built ourselves
> - If even ONE letter doesn't match: 🚨 alarm rings
> - If it all matches: ✅ green tick on the dashboard
> - 8:15 AM next morning: download NSE official bhavcopy as a SECOND independent check
> - All recorded in 3 tables for SEBI to inspect later
>
> If at 3:31 PM the network is broken, we wait + retry. If AWS shuts off, we resume next day. We NEVER give up on a candle. We NEVER count a day done unless EVERY single candle for EVERY single instrument is verified.
>
> This is the heart of zero-tick-loss. The live aggregation might miss; the post-market fetch CATCHES it.

3 days no code. Floor's yours for the 6 D-items. 🫡

---

## 📌 APPENDIX A — Operator decisions on D-items 2026-05-12 12:10 IST

All 6 D-items resolved:

| # | Question | Operator decision |
|---|---|---|
| **D1** | Endpoint — `/v2/charts/intraday`? | ✅ Locked |
| **D2** | Write prev_day_oi cache BEFORE or AFTER cross-verify? | ✅ AFTER (only authoritative values) |
| **D3** | AWS shutdown vs wait-for-fetch? | ✅ **AWS shutdown NEEDED** — accept it; resume next boot if needed |
| **D4** | Mock Trading Saturday — Dhan provides data? | ✅ **YES — Dhan provides** (operator confirmed). Fetch on Mock Sessions. |
| **D5** | Timestamp precision — exact or ±1sec? | ✅ **PRECISE** (exact match required, no tolerance) |
| **D6** | ALSO fetch 1d historical for daily cross-verify? | ✅ **NOT NEEDED** — skip. Local 1m → higher TF aggregation trusted. |

### Implications

**D3 AWS shutdown:**
- AWS hard-stops at 17:30 IST regardless of fetch state
- Fetch budget = 15:31 → 17:25 = 1h54m (5min safety before shutdown)
- 11K × 0.2s/call = 37 min typical = comfortable margin
- Worst case (rate-limit retries push past 17:25): state saved → resume next morning 08:30 boot
- Cross-verify summary may be delayed to next day morning — accepted trade-off

**D4 Mock Trading day handling:**
- `is_mock_trading_session(today) == true` → same path as regular trading day
- Same fetch + same cross-verify + same audit
- No special-casing in code
- TEST on next Mock Trading Saturday (~next planned: 2026-XX-XX from calendar)

**D5 timestamp precision:**
- Live aggregator seals at exact minute boundary (e.g., 10:15:00.000 IST)
- Dhan historical timestamp must be **exact same epoch** (modulo IST offset)
- If Dhan returns 10:15:01 IST → that's a PARSER BUG (likely +1s shift)
- Cross-verify treats timestamp mismatch as a fundamental error (not data mismatch)
- Per-candle `candle_ts` is the primary join key — must match byte-for-byte

**D6 no 1d historical fetch:**
- Saves 11K API calls per day (~2 hours of API time)
- Trust local 1m → 1d aggregation
- If local aggregation buggy, cross-verify on 1m WILL catch (all 375 candles wrong → all 1d wrong)
- Single point of detection at 1m level is sufficient

### Updated API budget

| Item | Calls/day | % of 100K daily quota |
|---|---|---|
| Dhan historical 1m fetch | 11,000 | 11% |
| ~~Dhan historical 1d fetch~~ | ~~11,000~~ | ~~11%~~ (D6 dropped) |
| Other Data API uses (quote/option chain/etc.) | ~5,000 | 5% |
| **TOTAL** | ~16,000 | ~16% |

**Comfortable margin of 84K calls/day reserved for retries + emergency fetches.**

### Updated time budget

| Stage | Duration |
|---|---|
| 15:30 market close | — |
| 15:30–15:31 WS drain | 1 min |
| 15:31 orchestrator fires | — |
| 15:31–16:08 Dhan historical fetch (11K × 0.2s @ 5/sec) | 37 min |
| 16:08–16:15 cross-verify (4.1M comparisons) | ~7 min |
| 16:15–16:20 write audit + Telegram summary | 5 min |
| 16:20–17:25 buffer for retries | 65 min |
| 17:25–17:30 graceful shutdown prep | 5 min |
| 17:30 AWS hard-shutdown | — |

**Total work time: ~50 min normal. Buffer: 65 min for retries. Margin: comfortable.**

---

## 📌 APPENDIX B — Final FRIDAY LoC estimate for the heart piece

| Component | LoC |
|---|---|
| Historical fetch orchestrator | 200 |
| Per-instrument fetch worker (with retry/resume) | 250 |
| historical_fetch_state table writes + reads | 100 |
| Cross-verify engine (per-field comparison) | 200 |
| cross_verify_mismatches + summary table writes | 150 |
| Idempotency state machine | 100 |
| WS-disconnect gate (wait for all 16 to close) | 80 |
| Prometheus metrics (8) | 100 |
| Alert rules in alerts.yml (4) | 50 |
| Grafana panels (6) | 150 (JSON) |
| Telegram NotificationEvent variants (4) | 80 |
| Ratchet tests (~15 test cases) | 400 |
| **TOTAL** | **~1,860 LoC** |

This is ONE Friday session work item. Operator pick: how many sub-PRs to split into?

---

## 📌 APPENDIX C — EXTENDED Worst-Case Scenarios (W16-W40)

Operator demand 2026-05-12 12:25 IST: "what else could I have missed dude — extreme worst case scenarios". 25 MORE scenarios added below.

### Time-related (W16-W22)

| # | Scenario | Likelihood | Defense |
|---|---|---|---|
| W16 | Leap second at 23:59:60 UTC — minute aggregator confused | RARE (once every few years) | Use IST epoch seconds; leap-second handled by kernel; aggregator uses div-60 buckets so 60-second jump = next bucket cleanly |
| W17 | Year-end boundary fetch (Dec 31 → Jan 1) — bhavcopy/historical date math wraps | LOW | Use `trading_date` (DATE type) not relative arithmetic; chrono crate handles year wrap |
| W18 | Half-day session (Diwali Muhurat trading ~18:00-19:00 IST) | YEARLY | `is_muhurat_session(today)` already in trading_calendar; expected_candle_count = 60 minutes instead of 375; lock per-day expected count |
| W19 | Bhavcopy publishing DELAYED past 08:30 IST boot (NSE infra issue) | LOW | Retry every 5 min for up to 2 hours; if STILL missing at 10:30 IST → CRITICAL Telegram + fall through to Dhan historical for yesterday |
| W20 | Trading day ends EARLY (NSE special early close, e.g., budget day) | RARE | `expected_candle_count` adjusted per-day based on actual session bounds in nse_holidays |
| W21 | Clock-drift mid-fetch (NTP sync hiccup) | RARE | BOOT-03 catches at boot; mid-day drift caught by cross-verify timestamp mismatch |
| W22 | DST transition (irrelevant to India but if Dhan server timezone shifts) | RARE | All math in IST epoch seconds; no DST in IST; if Dhan server-side adopts a tz that DSTs we'd see consistent +/-3600s offset → alarm |

### Data-quality (W23-W32)

| # | Scenario | Likelihood | Defense |
|---|---|---|---|
| W23 | Dhan returns `null` or `0` for OI on a derivative with no trades today | MEDIUM | Skip null/zero check; record as 0 in our candle; cross-verify accepts 0 if both sides = 0 |
| W24 | Volume goes NEGATIVE in Dhan response (data corruption) | RARE | Sanity check: reject volume < 0; log as W23-style anomaly; track via Track 2 monotonicity guard |
| W25 | OHLC ordering violated: high < low (corrupt response) | RARE | Sanity check at parse time; reject with PARSE error; don't store; retry |
| W26 | Open > High OR Open < Low (corrupt response) | RARE | Same sanity check; reject |
| W27 | Same security_id appears TWICE in bhavcopy with different OI | LOW | DEDUP UPSERT KEYS keeps LATEST row; log warning; investigate via audit table |
| W28 | Bhavcopy has instrument we NEVER subscribed to | EXPECTED | Skip silently; not an error |
| W29 | Subscribed instrument MISSING from bhavcopy | MEDIUM | Per-instrument check: if subscribed but no bhavcopy row → CRITICAL alert; might be delisted overnight |
| W30 | F&O bhavcopy has missing OPEN_INT column for some rows | LOW | Treat as null; log warning; fall back to Dhan historical for these |
| W31 | Dhan historical returns candle ts BEFORE market open (10:14 not 10:15) | RARE | Sanity check: reject ts < session_start; log |
| W32 | Dhan historical returns DUPLICATE timestamps (two candles same minute) | RARE | DEDUP UPSERT KEYS handles; latest wins; log warning |

### Operational extremes (W33-W40)

| # | Scenario | Likelihood | Defense |
|---|---|---|---|
| W33 | Disk full on `data/spill/` during fetch — historical_candles can't write | LOW | L4 PREVENT: pre-flight 20% disk-free check; if mid-fetch disk fills → STOP fetch + alert + manual operator |
| W34 | Token EXPIRES during 37-min fetch (24h boundary) | LOW | Token-age guard at start of each instrument fetch; if < 5min remaining → force-refresh before next call |
| W35 | Operator runs `make run` twice → two app instances fight for same SSM/Valkey | RARE | Single-instance lock in `live_instance_lock` QuestDB table (RESILIENCE-01 per wave-4); newer instance refuses to start |
| W36 | Orchestrator OOMs during cross-verify (4.1M comparisons in memory) | LOW | Stream comparisons per instrument; release memory between instruments; never hold all 4.1M in heap simultaneously |
| W37 | SIGTERM mid-fetch from systemd/operator | MEDIUM | Graceful shutdown: finish current instrument, write state=pending for unfinished, exit cleanly |
| W38 | Cross-verify finds 100% mismatch (catastrophic local bug) | RARE | Telegram coalescer caps at 1 msg/topic/60s; raw 4M alerts coalesce to ONE summary message |
| W39 | mismatches table WRITE fails during cross-verify (silent audit failure) | LOW | AUDIT-07 ErrorCode fires; falls back to local NDJSON spill for mismatches |
| W40 | NSE retroactively declares today a holiday (rare political event) | RARE | Bhavcopy never publishes → W19 path triggers; operator manual review of cross-verify validity |

### The TRULY extreme (W41-W45)

| # | Scenario | Likelihood | Defense |
|---|---|---|---|
| W41 | Bhavcopy and Dhan historical DISAGREE on yesterday's OI for one instrument | LOW | Configurable preference: trust bhavcopy by default; both written to audit; operator picks via SQL query |
| W42 | NSE issues a "REVISED bhavcopy" next day (original had errors) | RARE | Re-download bhavcopy on operator manual trigger; UPDATE previous_close with revised values; audit the revision |
| W43 | Cross-verify pushes past 17:25 buffer; AWS shuts down at 17:30 | LOW | State preserved; resume next boot at 08:30 IST tomorrow; cross-verify summary delayed by ~15h |
| W44 | Dhan re-issues a security_id (rare but possible — old contract retired, new one takes ID) | RARE | Instrument master refresh at boot detects the change; old data in QuestDB tagged with `(security_id, segment, date)` — date keeps them distinct |
| W45 | Both Dhan AND NSE bhavcopy go down for full day (regulator action / infra outage) | EXTREME RARE | NO recovery possible — manual operator intervention; SEBI audit defense: we have ws_wal/ raw frames; can reconstruct from there |

---

## 📊 Worst-case total tally

| Range | Topic | Count |
|---|---|---|
| W1–W15 | Original heart-piece scenarios | 15 |
| W16–W22 | Time-related extremes | 7 |
| W23–W32 | Data-quality extremes | 10 |
| W33–W40 | Operational extremes | 8 |
| W41–W45 | Truly catastrophic | 5 |
| **TOTAL** | | **45 worst-case scenarios** |

**All 45 have a named defense.** ✅

---

## 🎯 The 5 that scare me MOST (operator should focus on these)

| # | Scenario | Why I'm scared |
|---|---|---|
| W19 | Bhavcopy publish delay past 08:30 | If NSE infra hiccups, we boot WITHOUT prev_day_oi — strategies running blind |
| W29 | Subscribed instrument MISSING from bhavcopy | Silent data quality issue — instrument trading but no audit record |
| W34 | Token expires mid 37-min fetch | Compounding failure — DH-901 cascade right when we need data most |
| W41 | Bhavcopy vs Dhan disagree on OI | "Truth" becomes a configuration choice |
| W45 | Both Dhan AND NSE bhavcopy down | Mathematically possible, no software recovery |

### Defense priorities for these 5

1. **W19**: implement 5-min retry up to 2 hours; if still missing → Telegram CRITICAL + use Dhan historical for prev_day_oi as W19-specific fallback
2. **W29**: per-instrument check after bhavcopy load; alert on any subscribed-but-missing
3. **W34**: token-age check before EVERY historical fetch call (sub-second overhead, huge safety)
4. **W41**: log BOTH values to audit; operator decides via SQL; default = bhavcopy wins
5. **W45**: accept as outside envelope; ws_wal/ raw frames are the last-ditch recovery path

---

## 🚗 Auto-Driver Story (extended)

> Sir, I was wrong to stop at 15 scenarios. I just added 30 more. Some are silly (leap seconds — once every 4 years), some are scary (token dies mid-fetch). Each one has a named bodyguard.
>
> The 5 that worry me most are the ones where TWO things fail at the same time — bhavcopy late AND we don't have backup, or Dhan goes down AND NSE goes down. For those, the raw black-box recorder (`data/ws_wal/`) is our last-ditch defense.
>
> Total: 45 worst-case scenarios. All defended. The envelope is now MUCH stronger than the 15-case version.

---

## 🎤 Honest envelope (updated)

> "Post-market cross-verify inside the tested envelope:
> - 45 worst-case scenarios mapped, all with named defense
> - Bhavcopy delay tolerated up to 2 hours (W19)
> - Token-age check before EVERY API call (W34)
> - SIGTERM graceful shutdown (W37)
> - 100%-mismatch coalescer caps Telegram spam (W38)
> - Revised bhavcopy supported via operator manual trigger (W42)
>
> Beyond envelope: simultaneous Dhan + NSE outage AND ws_wal disk failure = manual operator recovery only. NDJSON DLQ + raw WS WAL preserve all bytes regardless."
