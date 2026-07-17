# Implementation Plan: Post-Close Dhan↔Groww spot_1m_rest Cross-Broker Comparator (SPOT-XVERIFY-01/02)

**Status:** APPROVED
**Date:** 2026-07-17
**Approved by:** Parthiban (operator) — standing authority, this session

Revives the cross-broker OHLC parity signal lost when `cross_verify_1m_boot.rs`
was deleted with the Dhan live WS. A daily post-close (15:47 IST) comparator
reads OUR stored `spot_1m_rest` rows `feed='dhan'` vs `feed='groww'` for the
same (trading day, minute, canonical index) and NAMES every OHLC divergence.
Neither feed is ground truth — a divergence-TREND signal only.

Guarantee matrix: this item carries the 15-row + 7-row guarantee matrix by
cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md` (the
canonical matrix source); every row is satisfied by the artefacts below.

## Design

- **NEW** `crates/storage/src/spot_crossverify_persistence.rs` — ILP-over-HTTP
  audit writer + two QuestDB tables (`spot_crossverify_cell_audit` one row per
  divergent/missing cell; `spot_crossverify_daily` one row per run) with
  `CREATE TABLE IF NOT EXISTS` + `ALTER TABLE ADD COLUMN IF NOT EXISTS`
  self-heal and DEDUP UPSERT KEYS. Mirrors `tf_consistency_audit_persistence.rs`.
- **NEW** `crates/app/src/spot_crossverify_boot.rs` — supervised dual-spawn +
  15:47 IST schedule gate + env-backfill override + `/exec` readers of
  `spot_1m_rest` (per feed, `exchange_segment='IDX_I'`) + PURE `compare_day`
  core (paise-exact OHLC, join by `canonicalize_index_symbol`, BLIND-on-empty
  Rule 11, keep-better rerun guard). Mirrors `tf_consistency_boot.rs` +
  `brutex_crossverify_compare.rs`.
- **NEW** `.claude/rules/project/spot-crossverify-error-codes.md` — the runbook
  (crossref-test target for the two new ErrorCodes) + honest-envelope +
  log-sink-only delivery-boundary note.
- **EDIT** `crates/common/src/error_code.rs` — 2 new variants
  `SpotXverify01MismatchFound` / `SpotXverify02RunDegraded` (both High;
  01 severity-independent NOT-auto-triage-safe via the FUTIDX-02 override arm).
- **EDIT** `crates/common/src/config.rs` — `SpotCrossverifyConfig` (serde
  default false) wired into `ApplicationConfig`.
- **EDIT** `config/base.toml` — `[spot_crossverify] enabled = true`.
- **EDIT** `crates/core/src/notification/events.rs` — `SpotCrossverifySummary`
  + `SpotCrossverifyAborted` NotificationEvent variants (10 Telegram
  commandments; name the broker 🔷 DHAN vs 🟢 GROWW).
- **EDIT** `crates/app/src/main.rs` — process-global spawn next to the
  tf_consistency + scoreboard spawns.

Join key is the CANONICAL INDEX NAME (never security_id — Dhan pins 13/25/51/21,
Groww stamps its own id space). Scope = NIFTY/BANKNIFTY/SENSEX + INDIA VIX
(a VIX-only gap must not fail the run).

## Edge Cases

- ONE feed empty while the other has rows ⇒ `blind` (High), never a silent
  clean (audit Rule 11). BOTH feeds empty ⇒ `no_data` (Info, log-only — a
  feed-off / non-trading day is not a failure).
- A minute present in only one feed ⇒ `missing_dhan` / `missing_groww` NAMED,
  never cross-copied or fabricated.
- Minute outside [09:15, 15:30) IST ⇒ `out_of_session` (window-filter both
  sides).
- 1-paise OHLC difference ⇒ exactly one `diverged` cell finding for that field
  with `diff_paise`; volume STORED both sides but NEVER classified (IDX_I
  volume is 0).
- Non-finite / ≤0 / > MAX price cells rejected on read.
- `/exec` query returning `returned == LIMIT` ⇒ truncation tripwire → degraded,
  never a silent partial.
- INDIA VIX present in Groww spot but absent in Dhan chain ⇒ counted missing,
  never a special carve-out; must not fail the run.
- Rerun / env-backfill: reruns UPSERT in place (deterministic 15:47 IST run
  `ts`); a measured (`clean`/`diverged`/`partial`) daily outcome is NEVER
  erased by a later blind/no_data/degraded rerun (keep-better guard,
  `stage="outcome_regression"`).

## Failure Modes

`SpotXverify02RunDegraded` fires with a `stage` naming the failing leg:
`client_build` / `questdb_unreachable` / `query_failed` / `oversize` /
`truncated` / `ensure_ddl` / `flush` / `budget_exceeded`. Each degrade leg
logs `error!(code = ErrorCode::SpotXverify02RunDegraded.code_str(), stage=…)`.
`SpotXverify01MismatchFound` fires ONE coalesced `error!` per run when
findings exist (never per row). A flush failure discards the pending ILP
buffer (poisoned-buffer defense) and the run is stamped degraded. The whole
subsystem is COLD-PATH, best-effort — a degraded run loses ONE day's parity
signal, never any market data; the tick hot path, the aggregator, the REST
capture legs and trading are never touched. Release-build panics abort the
process; the supervised outer task's `SpotCrossverifyAborted` High page
covers the unwind-build/cancel arms (never fired on graceful shutdown).

## Test Plan

Inline `#[cfg(test)]` ratchets in both new src files + a main.rs wiring guard:
- `create_ddl()` contains every column + literal `DEDUP UPSERT KEYS(…)` incl.
  `exchange_segment` + `field` + `kind`; daily key ends with `outcome`.
- `append_finding` buffer shape; `discard_pending` clears buffer + count;
  writer `new` is lazy on unreachable QuestDB.
- Pure `compare_day`: paise-exact match; 1-paise diff ⇒ 1 diverged finding;
  `missing_dhan` / `missing_groww`; `out_of_session` filter; BLIND when one
  side has rows + zero overlap; NoData when both empty; volume
  stored-not-classified; noise p50/p95/max.
- `decide_start` / `parse_*_date` boundary (NOW-without-DATE-before-trigger
  refusal; future / non-trading date refusal).
- `canonicalize_index_symbol` join maps a Groww index symbol + a Dhan index
  symbol to the same canonical (a real `INDEX_SYMBOL_ALIASES` alias).
- Source-scan wiring guard: `spawn_spot_crossverify_tasks(` appears in main.rs.

CI runs the full 22-test battery + the error-code crossref/tag guards + the
dedup-segment meta-guard on every PR (per `.claude/rules/project/testing.md`).

## Rollback

`[spot_crossverify] enabled` is serde-default `false` — an absent section
boots byte-identical to today. Flipping `config/base.toml` back to
`enabled = false` (or removing the section) disarms the whole subsystem: the
spawn guard returns early and no `/exec` read, table ensure, or Telegram
fires. No data migration is needed (the two audit tables are additive,
read-only for every other consumer). Reverting the branch removes all code
with zero effect on the retained REST capture legs or any other table.

## Observability

- Counters (static labels): `tv_spot_xverify_runs_total{status}`,
  `tv_spot_xverify_findings_total{kind}`,
  `tv_spot_xverify_query_failures_total{stage}`.
- Typed Telegram: `SpotCrossverifySummary` (page on mismatch / degraded /
  blind; `no_data` = log-only, never a page) + `SpotCrossverifyAborted`.
- Coded structured logs: `error!(code = SPOT-XVERIFY-01)` (coalesced findings)
  / `error!(code = SPOT-XVERIFY-02, stage=…)` (each degrade leg).
- Durable forensics: `spot_crossverify_cell_audit` (per divergent cell) +
  `spot_crossverify_daily` (per run — the divergence-TREND store).
- Delivery boundary (honest, no false-OK): both codes are LOG-SINK-ONLY today
  — no `error_code_alerts.tf` entry; the typed Telegram summary is the
  operator signal (the SCOREBOARD-01 precedent). Adding a CloudWatch
  log-filter alarm is a flagged follow-up.
