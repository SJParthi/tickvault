# Daily Timeframe-Consistency Verifier — Error Codes (TF-VERIFY-01 / TF-VERIFY-02)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `live-feed-purity.md` (read-only over `candles_*`; writes ONLY its own
> audit table) > this file.
> **Operator directive (2026-07-13, verbatim):** *"how will you guarantee
> that all our defined timeframes internally are correct — how do you
> identify whether any miscalculation or data issues"*.
> **Companion code:** `crates/app/src/tf_consistency_boot.rs`
> (grid + recompute + compare + scheduler + supervised dual-spawn),
> `crates/storage/src/tf_consistency_audit_persistence.rs`
> (the `tf_consistency_audit` table DDL + ILP-over-HTTP writer),
> `crates/common/src/error_code.rs::ErrorCode::{TfVerify01MismatchFound,
> TfVerify02RunDegraded}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `TfVerify0*` variant verbatim —
> `TF-VERIFY-01`, `TF-VERIFY-02`, `TfVerify01MismatchFound` and
> `TfVerify02RunDegraded` appear below.

---

## §0. Why these codes exist (the internal-correctness answer)

The 21-TF aggregator seals every higher-timeframe candle from live ticks and
persists it to the per-TF `candles_<tf>` tables. Nothing previously proved,
after the fact, that a stored `candles_5m` row actually equals the aggregate
of its five stored `candles_1m` rows — an aggregator bug, a seal-boundary
regression, a late-tick refold miss, or a write-side corruption would be
invisible until a strategy consumed the wrong bar. The verifier closes that:
at **15:40 IST** each trading day it recomputes every higher-TF candle
(**19 TFs: 2m..4h** — M1 is the baseline itself; D1 is historical-only per
`live-feed-purity.md` rule 10) from the stored `candles_1m` rows and
compares field-by-field against the stored TF tables, per feed:

- **`feed='dhan'` verifies TODAY** — stable after the 15:30:05 IST
  close-time force-seal.
- **`feed='groww'` verifies the PREVIOUS trading day** — Groww has no
  close-time force-seal (session tails seal at the IST-midnight force-seal),
  so only D-1 has full coverage. There is deliberately NO `tail_unsealed`
  category: a missing Groww D-1 tail row IS a `missing_tf_row` finding.

The bucket grid is REIMPLEMENTED independently in the verifier (windows
`[09:15:00 + k·S, min(+S, 15:30:00))`, final partial buckets truncated at
close) and tripwire-tested against `TfIndex::bucket_start` + hand-typed
literals — so a shared-bug in the aggregator's own grid cannot vacuously
self-verify. Comparison is exact: OHLC via integer paise, volume exact i64
(checked-add overflow → degraded), `tick_count` divergence is SOFT
(counter only). D1 and always-on/GIFT-Nifty cells are excluded and counted
(`tv_tf_verify_excluded_total{reason="d1"|"always_on"}`).

Everything is COLD-PATH (one scheduled run/day, read-only SELECTs +
audit-table writes) — the tick hot path, the aggregator, the seal writer,
and trading are never touched.

## §1. TF-VERIFY-01 — stored TF candle disagrees with its 1m recompute

**Severity:** High. **Auto-triage safe:** **No** — severity-independent
override in `is_auto_triage_safe()` (the FUTIDX-02 precedent): a
data-integrity divergence between two of our own stores is never
auto-actioned; the operator decides whether the aggregator, the writer, or
the 1m baseline is wrong.

**Trigger:** a (feed, date) pass found ≥1 PAGING finding. ONE coalesced
`error!(code = ErrorCode::TfVerify01MismatchFound.code_str(), feed, date,
mismatches, missing_tf_rows, no_coverage, off_grid, duplicates, samples)`
fires per (feed, date) pass — never per finding (audit Rule 4 discipline);
`samples` carries ≤10 named finding lines. The paging categories (each also
a `tf_consistency_audit` row + `tv_tf_verify_findings_total{category}`):

| Category | Meaning |
|---|---|
| `mismatch` | a stored TF field (open/high/low/close paise, or volume i64) differs from the 1m recompute — one row PER differing field |
| `missing_tf_row` | 1m rows cover a bucket window but the TF table has NO row for it (a lost seal — incl. the Groww D-1 tail) |
| `no_1m_coverage` | a stored TF row exists but ZERO 1m rows cover its window (baseline hole — the recompute cannot vouch) |
| `off_grid_ts` | a stored TF row's `ts` is not on the session bucket grid (bucket-boundary corruption) |
| `duplicate_key` | two stored TF rows share `(ts, security_id, segment, tf)` for one feed — DEDUP failed; compared on the FIRST row |

SOFT (never pages, no audit row beyond counters): `tick_count` divergence
(`tv_tf_verify_soft_divergence_total{field="tick_count"}`) and 1m-baseline
gaps between buckets (`tv_tf_verify_bucket_gap_total`).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `TF-VERIFY-01`; the payload
   names the feed, the verified date, per-category counts, and ≤10 sample
   finding lines (symbol-free: security_id + segment + tf + bucket + field
   + stored vs recomputed).
2. `mcp__tickvault-logs__questdb_sql "select category, tf, count(*) from
   tf_consistency_audit where trading_date_ist = '<date>' and feed =
   '<feed>' group by category, tf order by 3 desc"` — which TF/category
   dominates. All-TF `mismatch` on one SID = bad 1m baseline for that SID;
   one-TF `mismatch` across SIDs = that TF's seal path; `missing_tf_row`
   clusters = cross-check AGGREGATOR-DROP-01 / AGGREGATOR-SEAL-01 /
   BOUNDARY-01 for the same day.
3. `high`/`low`-only mismatches on the Dhan side can be legitimate
   late-tick refold artifacts (Option B amends the 1m row; the TF row is
   only amended while it is the most-recently-sealed bucket) — a small
   stable count is a known envelope; a spike or open/close drift is a real
   aggregator/seal bug.
4. The audit table is the trend store — compare today's counts against
   prior days before escalating (the CROSS-VERIFY-1M-01 discipline: track
   the trend, not the absolute).

**Honest envelope:** the verifier proves INTERNAL consistency (stored TF ==
aggregate of stored 1m). It does NOT prove either store matches the
exchange — that is the 15:31 cross-verify's job (Dhan REST vs `candles_1m`)
— and a bug that corrupts the 1m rows AND their TF rows identically is
invisible to it by construction. `oi` and the `*_pct_from_prev_day` columns
are not compared (OI arrives on a separate packet; pct columns are derived).

## §2. TF-VERIFY-02 — verifier run degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already
happened; the audit table is DEDUP-idempotent, so a backfill re-run —
`TICKVAULT_TF_VERIFY_NOW=1` + `TICKVAULT_TF_VERIFY_DATE=YYYY-MM-DD` —
repairs the day once the cause is fixed; the operator inspects first).

**Trigger:** one of the run's legs failed
(`ErrorCode::TfVerify02RunDegraded`, distinguished by the `stage` field):

| stage | Meaning |
|---|---|
| `client_build` | the reqwest client could not be built (HTTP-CLIENT-01 class — host fd/TLS/resolver pressure); the run emits a BLIND summary |
| `ensure_client_build` / `ensure_ddl` | boot-time table-ensure failed — NOTE the duplicate-row window: the first ILP write may auto-create `tf_consistency_audit` WITHOUT DEDUP UPSERT KEYS until a later ensure succeeds |
| `discovery` | the per-(feed,date) instrument-discovery query failed |
| `query_failed` | a per-SID 1m / TF-union query or parse failed (that SID skipped; also folds a volume checked-add overflow) |
| `questdb_unreachable` | the /exec transport leg failed outright |
| `truncated` | a query hit its explicit LIMIT (500 1m / 2,000 TF-union / 3,000 discovery rows) — a partial compare is NEVER trusted; the SID/pass reads degraded |
| `flush_failed` | the audit ILP-over-HTTP flush was refused by the per-request server ACK — pending rows DISCARDED (poisoned-buffer defense, `tv_tf_verify_audit_rows_discarded_total`) |
| `budget_exceeded` | the 900s run budget elapsed between SIDs — remaining instruments skipped, ONE coalesced error |

**RunStatus honesty (audit Rule 11):** `compared == 0` while candle rows
EXISTED classifies **Blind** — the Telegram says BLIND, never PASS.
`compared == 0` with NO rows on either side is **NoData** (Info — an
off-day, not a failure). Any paging finding → MismatchFound; any degrade →
Degraded; else Pass. `tv_tf_verify_runs_total{status}` records the verdict.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `TF-VERIFY-02`; the `stage`
   names the failing leg. `tv_tf_verify_query_failures_total{stage}` rates
   name the dominant class.
2. `make doctor` / `mcp__tickvault-logs__run_doctor` — a failed /exec or
   ILP leg at 15:40 almost always means QuestDB was down (cross-check
   BOOT-01/BOOT-02, WAL-SUSPEND-01, and the sibling 15:31/15:40/15:45
   post-market tasks, which share the target).
3. Backfill once healthy: restart with `TICKVAULT_TF_VERIFY_NOW=1` (+
   `TICKVAULT_TF_VERIFY_DATE` for a specific trading day; NOW without DATE
   on a non-trading day is REFUSED with an info log). Re-runs UPSERT in
   place (DEDUP key includes the deterministic run `ts` = target day
   15:40:00 IST).
4. Sustained `truncated` on real instruments means the day's row counts
   outgrew the LIMIT envelope — raise the named cap constants in a
   reviewed PR, never silently.

**Honest envelope:** the verifier is best-effort forensic — a degraded or
lost run loses ONE day's verification signal, never any market data. The
audit-row cap (`TF_VERIFY_MAX_AUDIT_ROWS_PER_RUN` = 10,000) bounds a
pathological day: beyond it findings are counted-only and the summary says
truncated. Release-build panics abort the process (`panic = "abort"`); the
supervised outer task's Aborted page covers the unwind-build/cancel arms.

## §3. Delivery boundary (honest — no false-OK)

Both codes are **log-sink-only today**: NO `error_code_alerts` map entry in
`deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s paging list. The operator page is the
typed **`TfConsistencySummary`** Telegram (ONE per run — Info only when
clean WITH coverage or pure no-data; High on any paging finding, degrade,
or blind run) plus the **`TfConsistencyAborted`** High page if the run task
dies. The coded `error!` lines are the forensic WHY. Adding a CloudWatch
log-filter alarm is a **flagged follow-up** (one map entry + the doc
paragraph + a cost note — the SCOREBOARD-01 / FEED-REJECT-01 precedent).

## §4. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `TfVerify0*` variant)
- `crates/app/src/tf_consistency_boot.rs`
- `crates/storage/src/tf_consistency_audit_persistence.rs`
- `crates/common/src/config.rs` (`TfConsistencyConfig`)
- Any file containing `TF-VERIFY-01`, `TF-VERIFY-02`,
  `TfVerify01MismatchFound`, `TfVerify02RunDegraded`,
  `tf_consistency_audit`, `spawn_tf_consistency_tasks`, or
  `tv_tf_verify_`
