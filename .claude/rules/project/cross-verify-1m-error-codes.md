# Post-Market 1-Minute Cross-Verification — Error Codes

> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Companion code:** `crates/app/src/cross_verify_1m_boot.rs`,
> `crates/storage/src/cross_verify_1m_audit_persistence.rs`.
> **Companion rule:** `live-feed-purity.md` rule 11 (the narrowed re-allow).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `CrossVerify1m*` variant.

---

> **⚠ RETIREMENT AUTHORIZED 2026-07-13 — deletion LANDED in PR-C3 (2026-07-14):**
> `cross_verify_1m_boot.rs` is DELETED (the parser-relocation obligation below was
> satisfied by C1's `dhan_intraday_parse.rs` move — Verified: `spot_1m_rest_boot` +
> `groww_spot_1m_boot` import from the relocated module); the two paging entries the
> same-day automation-gaps PR added were retired with it (§2b banner). Original
> authorization text follows:** the
> 15:31 IST Dhan live-vs-historical 1m cross-verify — and with it `CROSS-VERIFY-1M-01`
> (`CrossVerify1m01MismatchFound`) and `CROSS-VERIFY-1M-02`
> (`CrossVerify1m02FetchDegraded`) — retires with the Dhan live WS: with no Dhan live
> candles there is no live side to compare (operator Q1/Q2, 2026-07-13;
> `websocket-connection-scope-lock.md` "2026-07-13 Amendment"). For the record: this
> subsystem was BLIND SINCE BIRTH until PR #1474 (2026-07-11) fixed the micros-vs-nanos
> SQL literal bug (`compared=0` on every run, honestly reported as BLIND), and its FIRST
> working sessions surfaced the live-vs-historical mismatches behind the operator's Q2
> retirement rationale — the amendment's §E evidence table carries the numbers. Phase C
> obligation: `spot_1m_rest_boot` imports `parse_intraday_1m_candles` + `MinuteCandle`
> from `cross_verify_1m_boot.rs` — the parser MUST be relocated before the file is
> deleted (Phase B map §4, Verified). The `cross_verify_1m_audit` table + CSVs are
> retained (forensic). Ongoing OHLCV parity signals: the §37 BruteX comparison + the §38
> Groww/Dhan per-minute REST tables. Content below retained for historical audit.

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

## §2b. 2026-07-14 Update — CROSS-VERIFY-1M-01/-02 briefly PAGED (superseded HOURS later by the PR-C3 deletion)

> **⚠ SUPERSEDED 2026-07-14 (PR-C3 — the header banner's authorized deletion
> LANDED):** `cross_verify_1m_boot.rs` was deleted with the Dhan instrument
> chain, and the two `error_code_alerts` entries this section describes were
> RETIRED in the same PR (dated notes in `error-code-alarms.tf` +
> `observability-architecture.md` "Retired paging entries") — a filter with
> no possible emit site is a dead filter per the drift guard. Content below
> retained as the historical record of the few-hour paging window.

The 2026-07-10 automation audit found both codes emitted `error!` but were
**log-sink-only** — neither had an `error_code_alerts` entry, so a 15:31
IST mismatch or a degraded/blind run paged NOBODY. Both now route via the
canonical errcode chain: `error!` → errors.jsonl → CloudWatch Logs
`/tickvault/<env>/app` → filters `tv-<env>-errcode-cross-verify-1m-01` /
`-02` (`deploy/aws/terraform/error-code-alarms.tf`) → alarm (≤5 min) →
SNS → Telegram. `ok_recovery = false` on both: these are daily ONE-SHOT
audit findings — the auto-OK ~15 min after the single datapoint ages out
can never mean the mismatch/coverage gap was fixed (Rule-11
false-recovery; the aggregator-drop-01 precedent); the real recovery
signal is the NEXT trading day's clean run. Lockstep surfaces: the
"Which codes page" list in `observability-architecture.md` + the
bidirectional drift ratchet
`crates/common/tests/error_code_paging_filter_drift_guard.rs` (whose
production-region scanner was upgraded the same day to EXCISE mid-file
`#[cfg(test)]` modules — `cross_verify_1m_boot.rs`'s
`mod start_decision_tests` at ~line 134 previously truncated the scan and
would have reported these emits as dead filters).

---

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `CrossVerify1m*` variant)
- `crates/app/src/cross_verify_1m_boot.rs`
- `crates/storage/src/cross_verify_1m_audit_persistence.rs`
- Any file containing `CROSS-VERIFY-1M-` or `CrossVerify1m`
