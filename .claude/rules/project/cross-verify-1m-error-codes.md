# Post-Market 1-Minute Cross-Verification — Error Codes

> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Companion code:** `crates/app/src/cross_verify_1m_boot.rs`,
> `crates/storage/src/cross_verify_1m_audit_persistence.rs`.
> **Companion rule:** `live-feed-purity.md` rule 11 (the narrowed re-allow).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `CrossVerify1m*` variant.

---

## §0. Why this exists (operator directive 2026-06-02)

Operator quote: *"put back the historical cross verification at precise 3.31 pm
onwards … cross verification between dhan historical intraday of entire one
minute candle OHLCV for every timestamp … among our candles_1m … csv … exact
match … trackable to find how many times there is a mismatch … only for one min
alone … for all those subscribed spot instruments."* — and the follow-up: *"csv
and exact match cross verification is needed."*

At **15:31 IST** each trading day, for every subscribed **spot** instrument (the
243 daily-universe SIDs — indices + F&O underlyings), we fetch Dhan's
authoritative **intraday 1-minute** candles (`POST /v2/charts/intraday`, interval
`"1"`) and compare OHLCV **timestamp-by-timestamp**, **EXACT match**, against our
live `candles_1m`. Mismatches go to:

1. QuestDB `cross_verify_1m_audit` table (DEDUP `(trading_date_ist, security_id, segment, minute_ts_ist, field)` — I-P1-11 compliant)
2. CSV file `data/cross-verify/cross-verify-1m-YYYY-MM-DD.csv` (opens in Excel)
3. Telegram per-day summary (compared / mismatches / missing)

The per-day **mismatch count** is the quality signal. This is a **narrowed**
replacement for the deleted `cross_verify.rs` chain — 1-minute only, spot only,
today only, post-market only. No 90-day range, no other timeframe, no
synthesized ticks into `ticks`.

---

## §1. CROSS-VERIFY-1M-01 — mismatch found

**Severity:** High. **Auto-triage:** No (data-integrity signal).

**Trigger:** one or more 1-minute OHLCV cells disagreed (exact compare) between
our `candles_1m` and Dhan intraday for the trading day. Each differing
`(security_id, segment, minute, field)` cell is a row in `cross_verify_1m_audit`
and a line in the day's CSV.

**Why this is expected to be non-zero (and is NOT necessarily a bug):** our live
feed is a **sampled** stream (Dhan WebSocket pushes ~2–4 ticks/sec), while Dhan's
intraday candle API is built from their **full internal tape**. So per-minute
High/Low especially can legitimately differ. **Track the trend, not the absolute
count** — a stable baseline is sampling noise; a sudden spike (or a sustained
Open/Close drift, which should be tiny) is a real problem.

**Triage:**
1. Open `data/cross-verify/cross-verify-1m-<date>.csv` in Excel; sort by `field`
   and `security_id`. Open/Close drift > a few paise on many SIDs = investigate
   the aggregator boundary; High/Low-only drift = expected sampling noise.
2. `mcp__tickvault-logs__questdb_sql "select field, count(*) from cross_verify_1m_audit where trading_date_ist = today() group by field"`
   — which field dominates.
3. Compare the count vs prior days (the audit table is the trend store).

**Source:** `crates/app/src/cross_verify_1m_boot.rs::run_cross_verify_1m`,
`crates/common/src/error_code.rs::CrossVerify1m01MismatchFound`.

---

## §2. CROSS-VERIFY-1M-02 — fetch degraded

**Severity:** High. **Auto-triage:** No.

**Trigger:** Dhan intraday REST errored / rate-limited / returned empty for a
material fraction of the spot SIDs, so the verification could not vouch for the
full universe that day. The run still records whatever it could compare, but the
operator is told coverage was partial (false-OK avoidance — audit Rule 11: never
report a clean "all match" on an empty/partial compare set).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — look for `CROSS-VERIFY-1M-02` and the
   per-symbol fetch failure reasons.
2. Check Dhan Data-API health + our rate budget (5/sec, 100k/day). 243 calls is
   well inside budget, so sustained failure points at Dhan-side or network.
3. The 15:31 run is best-effort and never blocks; next trading day re-runs.

**2026-07-13 update — bounded 429 second pass now exists:** the live
2026-07-13 run lost 91/776 fetches to HTTP 429 at 15:31–15:33 (compared=0,
a BLIND day). Fetch failures are now TYPED on the real `StatusCode`: 429s
are deferred out of the first pass into a cohort, and after a 45 s
cool-down the run retries that cohort ONCE, paced at ≤3 requests/second
(strictly below the Data-API 5/sec budget), folding successes into the
comparison BEFORE the report. Anything still failing lands in
`fetch_failures` and rides the unchanged honest BLIND/DEGRADED
classification — one pass, linearly bounded, never a loop. Counters:
`tv_cross_verify_1m_retry_429_total{outcome="recovered"|"still_failed"}`.
The spot-1m post-session sweep simultaneously moved to ~15:33:30 IST so
its requests clear this run's burst window (see
`rest-1m-pipeline-error-codes.md`).

**Source:** `crates/app/src/cross_verify_1m_boot.rs::run_cross_verify_1m`,
`crates/common/src/error_code.rs::CrossVerify1m02FetchDegraded`.

---

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `CrossVerify1m*` variant)
- `crates/app/src/cross_verify_1m_boot.rs`
- `crates/storage/src/cross_verify_1m_audit_persistence.rs`
- Any file containing `CROSS-VERIFY-1M-` or `CrossVerify1m`
