# Groww Shared-Master Persist — Error Codes (GROWW-MASTER-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` (SAME shared tables, `feed='groww'`) >
> `data-integrity.md` "feed-in-key EVERYWHERE" (operator override 2026-06-28) > this file.
> **Companion code:** `crates/core/src/feed/groww/shared_master_writer.rs`,
> `crates/common/src/error_code.rs::ErrorCode::GrowwMaster01PersistFailed`,
> wired at the `Ok(set)` arm of `crates/app/src/groww_activation.rs`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this file to
> mention every `GrowwMaster01*` variant verbatim — `GROWW-MASTER-01` appears below.

---

## §0. Why this exists

PR #1229 put `feed` into the DEDUP key + DDL + row struct of the two SHARED master tables —
`instrument_lifecycle` (DEDUP `ts, security_id, exchange_segment, feed`) and
`index_constituency` (DEDUP `ts, index_name, security_id, exchange_segment, feed`) — and the
Dhan reconciler/boot stamp `feed='dhan'`. Per the operator's 2026-06-19 "SAME tables + feed
column" decision (no `groww_*` master table), PR-A adds the Groww-side writer that fills the
`feed='groww'` rows from the daily `GrowwWatchSet`, reusing the existing bulk ILP append fns.

The write is COLD-PATH, fire-and-forget, and best-effort: it is a forensic master/metadata
record, NEVER on the data-correctness or recovery path. A Dhan row and a Groww row for the
same `(security_id, exchange_segment)` are DISTINCT rows because `feed` is in the DEDUP key —
neither overwrites the other.

### 2026-06-29 — audit-coverage close-out: Groww now emits `instrument_lifecycle_audit` (feed='groww')

> **Operator directive 2026-06-29 (100%-audit-coverage charter + his direct question about the
> empty `instrument_lifecycle_audit WHERE feed='groww'` table):** the Groww shared-master writer
> persisted `instrument_lifecycle` DATA rows (feed='groww', ~767) + `index_constituency` (~742) but
> emitted ZERO `instrument_lifecycle_audit` TRANSITION rows — leaving no forensic "what changed in
> the Groww universe day-over-day" chain (only the Dhan reconciler wrote it).

This is now CLOSED. `persist_groww_instruments` first calls `emit_groww_lifecycle_audit`
(`crates/core/src/feed/groww/shared_master_writer.rs`) — **audit-first (§24)**, BEFORE the lifecycle
DATA UPSERT — which:

1. reads yesterday's `feed='groww'` snapshot (`build_groww_lifecycle_select_sql` →
   `parse_groww_lifecycle_dataset`, scoped `WHERE feed='groww' AND dry_run=false`),
2. diffs it against today's master set (`groww_today_attrs`) via the SAME pure
   `tickvault_storage::lifecycle_reconciler::classify_transition` the Dhan reconciler uses
   (appeared / updated / split / reactivated / expired / segment_moved / no-op; Day-1 empty prior ⇒
   all `appeared`; composite `(security_id, exchange_segment)` per I-P1-11),
3. appends one `instrument_lifecycle_audit` row per transition tagged `feed='groww'` via the existing
   `append_instrument_lifecycle_audit_rows`.

It is best-effort + degrade-safe: a prior-read OR append failure logs `error!(code=GROWW-MASTER-01)`,
bumps `tv_groww_master_persist_errors_total{stage="lifecycle_audit"}`, and RETURNS — NEVER aborting
Groww activation, the live feed, the data UPSERT, or any order/tick path; the next idempotent boot
re-emits (DEDUP UPSERT KEYS). `dry_run` skips the emission (§27 isolation). The pure
diff/parser/builders are unit-tested; the live diff-vs-yesterday needs a real QuestDB carrying
prior-day rows (boot-exercised).

---

## §1. GROWW-MASTER-01 — shared-master persist failed

**Severity:** Medium. **Auto-triage safe:** Yes (best-effort cold-path write; never gates
Groww activation, the live feed, or any order/tick path).

**Trigger:** `persist_groww_instruments` (spawned fire-and-forget from the Groww watch-set
`Ok(set)` arm) could not persist the Groww instrument set into `instrument_lifecycle`,
`index_constituency`, OR the `instrument_lifecycle_audit` transition chain via QuestDB
(ILP/HTTP down, QuestDB unreachable, flush error, prior-snapshot read error). The `stage` label
on `tv_groww_master_persist_errors_total` distinguishes `lifecycle` / `constituency` /
`lifecycle_audit` (the 2026-06-29 audit-chain emit, which can fail on EITHER the prior-snapshot
read OR the audit append).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — look for `GROWW-MASTER-01`; the payload carries the
   `stage` (`lifecycle` / `constituency` / `lifecycle_audit`) and the error context.
2. `tv_groww_master_persist_errors_total` rate non-zero → QuestDB ILP degraded; run
   `make doctor` (cross-check BOOT-01/BOOT-02 if it coincides with boot).
3. The Groww live feed, the `ticks` table, and trading are UNAFFECTED — only the queryable
   `feed='groww'` master/constituency rows are missing until QuestDB recovers. The append is
   idempotent (DEDUP UPSERT), so the next boot / next activation re-runs cleanly.

**Honest envelope:** the master/constituency write is best-effort. A sustained ILP outage
loses the `feed='groww'` forensic rows for the outage window; it never affects tick capture,
order placement, the Dhan master rows, or Groww feed activation.

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::GrowwMaster01PersistFailed`
- `crates/core/src/feed/groww/shared_master_writer.rs::persist_groww_instruments`
- Reused bulk append fns: `instrument_lifecycle_persistence::append_instrument_lifecycle_rows`,
  `index_constituency_persistence::append_index_constituency_rows`,
  `instrument_lifecycle_persistence::append_instrument_lifecycle_audit_rows` (feature-gated
  `daily_universe_fetcher`).
- Audit chain (2026-06-29): `shared_master_writer::emit_groww_lifecycle_audit` (audit-first §24) +
  the pure `build_groww_lifecycle_select_sql` / `parse_groww_lifecycle_dataset` / `groww_today_attrs`
  / `build_groww_audit_rows`, reusing `tickvault_storage::lifecycle_reconciler::classify_transition`.
- Wiring: `crates/app/src/groww_activation.rs` (the watch-set `Ok(set)` arm).

---

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwMaster01*` variant)
- `crates/core/src/feed/groww/shared_master_writer.rs`
- Any file containing `GROWW-MASTER-01`, `GrowwMaster01`, or `persist_groww_instruments`
