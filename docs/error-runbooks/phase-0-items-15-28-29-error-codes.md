---
paths:
  - "crates/storage/src/bar_correction_audit_persistence.rs"
  - "crates/core/src/historical/post_open_cross_check.rs"
  - "crates/common/src/error_code.rs"
  - "crates/app/src/main.rs"
---

# Phase 0 Items 15+28+29 Error Codes

> **Authority:** This file is the runbook target for the three
> `BarMismatch*` ErrorCode variants added in Phase 0 Items 15+28+29
> (post-open cross-check + bar correction). Cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires every
> variant in `ErrorCode` to be mentioned in at least one rule file
> under `.claude/rules/`. This file serves that contract.
>
> **Cross-references:**
> - Plan: `.claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md` §785
> - Audit table: `crates/storage/src/bar_correction_audit_persistence.rs`
> - Comparator: `crates/core/src/historical/post_open_cross_check.rs`
> - Schema: `candles_source` SYMBOL column on candles shadow tables +
>   historical_candles (Item 29 ALTER ADD COLUMN IF NOT EXISTS)

## BAR-MISMATCH-01 — 09:15 bar(s) corrected from Dhan REST

**Trigger:** at 09:16:05 IST every trading day, the post-open
cross-check scheduler in `post_open_cross_check::compare_bar` found
one or more 09:15 1m bars whose OHLCV disagreed with Dhan REST
`/v2/charts/intraday` outside `PRICE_TOLERANCE_RUPEES` (0.01 INR) for
OHLC or `VOLUME_TOLERANCE_UNITS` (0) for volume. The mismatched bars
were corrected — Dhan's authoritative values written to:

1. `candles_1m_shadow` (writable Wave 6 table — RAM bar cache source)
2. `historical_candles` (mirror for cross_verify parity)
3. `bar_correction_audit` (forensic chain — outcome=`Corrected`)

Strategy gate flips to `true` after corrections land (CrossCheckOutcome::Corrected).

**Severity:** Critical. Operator must see the corrections summary
before any strategy decisions fire.

**Triage:**

1. Telegram payload names every mismatched symbol + field +
   (local_value, dhan_value). Operator confirms top 10 manually
   against Dhan's TradingView chart.
2. `mcp__tickvault-logs__questdb_sql "select * from bar_correction_audit
   where trading_date_ist = today() and outcome = 'corrected'"` —
   full forensic snapshot.
3. If MORE THAN ~10 SIDs were corrected, investigate root cause:
   - WS pre-open feed degraded (check `tv_tick_gap_detector_stale_total`)
   - Items 13+14 wiring missing (still BLOCKED on Dhan support reply)
   - Clock skew at 09:15:00.000 boundary
4. Strategy gate is OPEN after corrections land — trading proceeds.

**Auto-triage safe:** YES (Severity::Critical for visibility; the
correction itself is already applied).

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::BarMismatch01CorrectedFromHistorical`
- `crates/core/src/historical/post_open_cross_check.rs` (sub-deliverable 3 — primitives)
- Scheduler boot wiring: `crates/app/src/main.rs` (sub-deliverable 4 — pending)

## BAR-MISMATCH-02 — cross-check INCONCLUSIVE (low coverage)

**Trigger:** at 09:16:05 IST the cross-check completed but Dhan REST
returned valid data for fewer than `MIN_COMPARED_COUNT_FOR_PASS` (200
of 222) SIDs. The cross-check cannot vouch for universe correctness;
strategy gate stays **CLOSED** for the trading day.

**Why threshold = 200/222:** allows up to 22 illiquid stragglers
(some NSE_EQ may genuinely lack 09:15 trade data on low-volume days)
without flapping. Below 200, Dhan REST is presumed degraded.

**Severity:** Critical. Operator must intervene OR accept no-trade
day.

**Triage:**

1. Inspect `tv_cross_check_rest_errors_total` in CloudWatch metrics (from
   the app `/metrics` exporter) — if non-zero, Dhan REST is failing across
   many SIDs.
2. Operator can manually flip strategy gate via admin endpoint
   IF they confirm Dhan REST health via direct curl test.
3. 15:31 IST post-market cross-verify will run AGAIN with the full
   day's data; if THAT also fails, file Dhan support ticket.

**Auto-triage safe:** NO (Severity::Critical; operator decision
required — fail-closed by design).

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::BarMismatch02CrossCheckInconclusive`
- Threshold: `post_open_cross_check::MIN_COMPARED_COUNT_FOR_PASS = 200`

## BAR-MISMATCH-03 — cross-check HARD-FAILED

**Trigger:** at 09:16:05 IST the cross-check encountered a hard
failure (all 222 Dhan REST fetches errored — auth failure, network
outage, DH-904 ladder exhausted, etc.). Strategy gate stays **CLOSED**.

**Severity:** Critical. Operator must intervene.

**Triage:**

1. `mcp__tickvault-logs__tail_errors` — search for DH-* error codes.
   If DH-901 (auth), token may have died; check `tv_token_remaining_seconds`.
   If DH-904 (rate limit), the 5-wide JoinSet is being too aggressive;
   check `compute_dh904_backoff` ladder.
2. `make doctor` for full health snapshot.
3. 15:31 post-market cross-verify runs AGAIN; if that succeeds, the
   trading day can proceed in dry-run-only mode.

**Auto-triage safe:** NO (Severity::Critical; operator MUST diagnose
the underlying REST failure).

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::BarMismatch03CrossCheckFailed`
- Failure path: `post_open_cross_check::classify_outcome` returns
  `CrossCheckOutcome::Failed { reason }` when `rest_error_count == expected_count`.

## Cross-references

- `.claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md` §28+§29+line 785
- `.claude/rules/project/audit-findings-2026-04-17.md` Rule 11 (no false-OK)
- `.claude/rules/project/security-id-uniqueness.md` I-P1-11 composite key
- `.claude/rules/project/live-feed-purity.md` rule 4 (historical_candles is the allowed destination)
- `crates/common/src/error_code.rs::ErrorCode` (`BarMismatch01CorrectedFromHistorical`, `BarMismatch02CrossCheckInconclusive`, `BarMismatch03CrossCheckFailed`)
