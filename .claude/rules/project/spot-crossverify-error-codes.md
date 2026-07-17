# Post-Close Dhan↔Groww Spot Cross-Broker Comparator — Error Codes (SPOT-XVERIFY-01 / SPOT-XVERIFY-02)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `live-feed-purity.md` (read-only over `spot_1m_rest`; writes ONLY its own
> audit tables) > `no-rest-except-live-feed-2026-06-27.md` §8 (the `spot_1m_rest`
> source legs) > this file.
> **Operator context (2026-07-17):** revive the cross-broker OHLC parity signal
> lost when `cross_verify_1m_boot.rs` was deleted with the Dhan live WS — a
> daily post-close comparator of OUR stored `spot_1m_rest` rows `feed='dhan'`
> vs `feed='groww'`.
> **Companion code:** `crates/app/src/spot_crossverify_boot.rs`
> (15:47 IST supervised dual-spawn + `/exec` readers + pure `compare_day`),
> `crates/storage/src/spot_crossverify_persistence.rs`
> (the `spot_crossverify_cell_audit` + `spot_crossverify_daily` tables),
> `crates/common/src/error_code.rs::ErrorCode::{SpotXverify01MismatchFound,
> SpotXverify02RunDegraded}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `SpotXverify0*` variant verbatim —
> `SPOT-XVERIFY-01`, `SPOT-XVERIFY-02`, `SpotXverify01MismatchFound` and
> `SpotXverify02RunDegraded` appear below.

---

## §0. Why these codes exist

Both broker legs' per-minute REST captures write the SAME `spot_1m_rest` table
with `feed` in the DEDUP key — a Dhan row and a Groww row for the same
(minute, index) are DISTINCT rows by construction. At **15:47 IST** each
trading day (after the ~15:33:30 spot post-session sweep has settled the
source table, and in the 15:45↔15:50 gap between the scoreboard and the
brutex crossverify), a supervised comparator reads the day's `IDX_I` rows per
feed, joins them by CANONICAL INDEX NAME
(`canonicalize_index_symbol` — never security_id, since Dhan pins 13/25/51/21
and Groww stamps its own id space), and compares Open/High/Low/Close
minute-by-minute as INTEGER PAISE with a configurable tolerance
(`tolerance_paise`, default 0 = exact). Volume is stored both sides but NEVER
classified (IDX_I volume is 0). Results land in:

1. QuestDB `spot_crossverify_cell_audit` — one forensic row per divergent /
   missing cell, DEDUP `(ts, trading_date_ist, index_name, exchange_segment,
   minute_ts_ist, kind, field)`.
2. QuestDB `spot_crossverify_daily` — one per-day summary row with a
   keep-better guard (a measured `clean|diverged|partial` outcome is never
   overwritten by a later `no_data|blind|degraded` rerun).
3. One typed Telegram summary + the two audit tables (the divergence TREND
   store).

The subsystem is COLD-PATH, config-gated, best-effort — never on the tick hot
path, never on any order / recovery / REST-capture path.

## §1. SPOT-XVERIFY-01 — cross-broker OHLC divergence found

**Severity:** High. **Auto-triage safe:** **No** — severity-independent
override in `is_auto_triage_safe()` (the FUTIDX-02 precedent): a
data-comparability VERDICT is never auto-actioned; the operator judges which
capture (Dhan vs Groww) is wrong — neither is ground truth.

**Trigger:** the daily run found ≥1 divergent or missing cell —
`ErrorCode::SpotXverify01MismatchFound`, emitted ONE coalesced `error!` per
run (never per row). Cell kinds:

| kind | Meaning |
|---|---|
| `diverged` | both feeds have the minute; an OHLC field differs beyond `tolerance_paise` |
| `missing_dhan` | Groww has the minute, Dhan's `spot_1m_rest` does not |
| `missing_groww` | Dhan has the minute, Groww's `spot_1m_rest` does not |
| `out_of_session` | a row outside [09:15, 15:30) IST — recorded, not classified |

**Why non-zero is not necessarily a bug:** both feeds sample the SAME vendor
LTP stream at different capture points; per-minute high/low can legitimately
differ by delivery/capture skew. Track the TREND (the daily table is the trend
store) — a stable small baseline is skew; a spike or sustained open/close
drift is a real capture problem on one side. The run's noise stats (p50/p95/max
paise) QUANTIFY the normal fluctuation so the operator SEES the baseline.

**Volume honest note:** IDX_I volume is 0, so volume is stored for forensics
but HARD-refused as a divergence field.

**Triage:**
1. `mcp__tickvault-logs__questdb_sql "select field, kind, count(*) from
   spot_crossverify_cell_audit where trading_date_ist = today() group by
   field, kind"` — which field/kind dominates.
2. Compare the day's counts against prior days (the audit table is the trend
   store) — track the trend, not the absolute.
3. Cross-check the day's per-feed REST health (SPOT1M-01 for both feeds) — a
   missing-in-one-feed cluster usually correlates with that feed's
   escalation edges.
4. Do NOT hand-patch either table — both re-produce tomorrow; the audit rows
   are the durable record. Missing-in-one-feed is NAMED, never cross-copied.

## §2. SPOT-XVERIFY-02 — run degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already happened;
the run is DEDUP-idempotent and the keep-better guard protects a previously
measured day — the operator inspects at leisure).

**Trigger:** a leg of the daily run failed — `ErrorCode::SpotXverify02RunDegraded`,
`stage` on the payload:

| stage | Meaning |
|---|---|
| `client_build` | the reqwest client could not be built (HTTP-CLIENT-01 class) |
| `questdb_unreachable` | the `/exec` transport leg failed outright |
| `query_failed` | a per-feed day query or dataset parse failed |
| `oversize` | an `/exec` response exceeded the 8 MiB body cap |
| `truncated` | a query hit its explicit LIMIT — a partial compare is never trusted |
| `ensure_ddl` | boot-time table-ensure failed (duplicate-row window until a later ensure succeeds) |
| `flush` | the audit ILP-over-HTTP flush was refused by the per-request server ACK — pending rows DISCARDED (poisoned-buffer defense) |
| `budget_exceeded` | the ~600s run budget elapsed |
| `outcome_regression` | a rerun tried to downgrade a measured daily outcome — suppressed, never applied |

BLIND-on-empty: if `minutes_compared == 0` while EITHER feed had rows → the
day records `blind` (High), never a silent clean (audit Rule 11). BOTH feeds
empty → `no_data` (Info, log-only — a feed-off / non-trading day is not a
failure, never a page).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `SPOT-XVERIFY-02`; the `stage`
   names the failing leg.
2. `questdb_*` / `client_build` stages: `make doctor` (cross-check
   BOOT-01/BOOT-02, WAL-SUSPEND-01 — the sibling post-market readers share the
   QuestDB target).
3. Backfill once healthy: `TICKVAULT_SPOT_XVERIFY_NOW=1` +
   `TICKVAULT_SPOT_XVERIFY_DATE=YYYY-MM-DD` re-runs a past trading day; the
   run is DEDUP-idempotent and the keep-better guard prevents a blind rerun
   from erasing a measured day.

## §3. Delivery boundary (honest — no false-OK)

Both codes are **log-sink-only today**: NO `error_code_alerts.tf` entry and NO
mention in `observability-architecture.md`'s paging list (the paging drift
guard sees no drift). The operator page is the typed `SpotCrossverifySummary`
Telegram (Info when clean-with-coverage or pure no-data; High on any mismatch,
degrade, or blind run) plus the `SpotCrossverifyAborted` High page if the run
task dies. The coded `error!` lines are the forensic WHY. Adding a CloudWatch
log-filter alarm is a flagged follow-up (one map entry + doc paragraph + cost
note — the SCOREBOARD-01 / FEED-REJECT-01 precedent).

## Honest envelope

Neither Dhan nor Groww is ground truth — both are per-minute REST captures of
the same sampled vendor stream; per-minute high/low can legitimately differ by
capture skew. Track the divergence TREND (the daily table is the store), not
the absolute count. A spike or sustained open/close drift is a real capture
problem on one side. No fabricated rows; missing-in-one-feed is NAMED, never
cross-copied. A degraded / no-data day is honestly stamped
(`partial` / `no_data` / `degraded` / `blind`), never reported clean on an
empty compare set (audit Rule 11).

## Trigger

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `SpotXverify0*` variant)
- `crates/common/src/config.rs` (`SpotCrossverifyConfig`)
- `crates/app/src/spot_crossverify_boot.rs`
- `crates/storage/src/spot_crossverify_persistence.rs`
- Any file containing `SPOT-XVERIFY-`, `SpotXverify0`,
  `spot_crossverify_cell_audit`, `spot_crossverify_daily`, or
  `SpotCrossverifyConfig`
