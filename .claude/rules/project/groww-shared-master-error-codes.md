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

---

## §1. GROWW-MASTER-01 — shared-master persist failed

**Severity:** Medium. **Auto-triage safe:** Yes (best-effort cold-path write; never gates
Groww activation, the live feed, or any order/tick path).

**Trigger:** `persist_groww_instruments` (spawned fire-and-forget from the Groww watch-set
`Ok(set)` arm) could not persist the Groww instrument set into `instrument_lifecycle` and/or
`index_constituency` via QuestDB ILP (ILP TCP down, QuestDB unreachable, flush error). The
`stage` label on `tv_groww_master_persist_errors_total` distinguishes `lifecycle` from
`constituency`.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — look for `GROWW-MASTER-01`; the payload carries the
   `stage` (`lifecycle` / `constituency`) and the ILP error context.
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
  `index_constituency_persistence::append_index_constituency_rows` (feature-gated
  `daily_universe_fetcher`).
- Wiring: `crates/app/src/groww_activation.rs` (the watch-set `Ok(set)` arm).

---

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwMaster01*` variant)
- `crates/core/src/feed/groww/shared_master_writer.rs`
- Any file containing `GROWW-MASTER-01`, `GrowwMaster01`, or `persist_groww_instruments`
