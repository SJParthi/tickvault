# Movers 22-TF Error Codes (Phase 11 of v3 plan, 2026-04-28)

> **Authority:** This file is the runbook target for the new ErrorCode
> variants added in the Phase 11 observability layer of the v3 movers
> 22-timeframe redesign. The cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires that
> every variant in `crates/common/src/error_code.rs::ErrorCode` is
> mentioned in at least one rule file under `.claude/rules/`.
>
> **Background:** `.claude/plans/v2-architecture.md` Section A covers
> the movers 22-tf architecture; `.claude/plans/v2-risks.md` covers the
> 8 risks each ErrorCode partially addresses.

## MOVERS-22TF-01 — Movers22TfWriter ILP append/flush failed

**Trigger:** one of the 22 ILP writers (one per timeframe) hit an
error calling `append_row` or `flush()` against QuestDB. Severity::Medium.

**Triage:**
1. Check `tv_movers_writer_errors_total{stage,timeframe}` to identify
   which timeframe + stage (`append` vs `flush`) is failing.
2. `append` failures usually indicate schema drift — re-run
   `ensure_movers_22tf_tables` (idempotent) and verify the DDL on
   the affected table.
3. `flush` failures usually indicate the QuestDB ILP TCP connection
   broke. The rescue ring buffers rows; recovery is automatic on the
   next snapshot cycle.
4. If sustained for > 5 minutes, run `make doctor` to confirm QuestDB
   health. The 0.1% drop-rate SLA (`tv_movers_writer_dropped_total`)
   alert fires if losses exceed budget.

**Auto-triage safe:** YES (Medium severity).

**Source:**
- `crates/storage/src/movers_22tf_persistence.rs` (Phase 8 DDL)
- `crates/core/src/pipeline/movers_22tf_writer_state.rs` (Phase 10 writers)

## MOVERS-22TF-02 — Snapshot scheduler task panicked

**Trigger:** one of the 22 `tokio::spawn` tasks (one per timeframe)
panicked. The `MoversSupervisor` polls `JoinHandle::is_finished()`
every 5s and respawns the task. Severity::High.

**Triage:**
1. `data/logs/errors.jsonl.*` carries the panic backtrace immediately
   preceding the `MOVERS-22TF-02` event.
2. Check `tv_movers_supervisor_respawn_total{timeframe}` — single
   respawn is recoverable. Sustained respawn rate > 1/min for
   5 minutes indicates a deterministic panic the supervisor cannot
   recover from.
3. Per the audit-findings Rule 4 (edge-triggered alerts), the typed
   event fires only on the rising edge of a panic burst; the falling
   edge logs INFO without re-paging.

**Auto-triage safe:** NO (High; operator should inspect backtraces).

**Source:**
- `crates/core/src/pipeline/movers_22tf_supervisor.rs` (Phase 10)

## MOVERS-22TF-03 — Universe size drift outside ±5% band

**Trigger:** `tv_movers_universe_size` gauge is outside `24,324 ± 5%`
during market hours (~23,107..25,540). This indicates the upstream
universe build dropped instruments unexpectedly OR the snapshot
scheduler is iterating a stale snapshot. Severity::Medium.

**Triage:**
1. Compare `tv_movers_universe_size` vs `tv_instrument_registry_total_entries`
   — they should match within tolerance. Discrepancy means the snapshot
   scheduler holds a stale `Arc<MoversTracker>`.
2. If both gauges agree but both are below 23,107: investigate the
   universe build (check `tv_fno_universe_*` metrics + the latest
   `InstrumentBuildSuccess` Telegram event).
3. Outside market hours this is expected (registry shrinks as expired
   contracts flip to `expired` status). The runner only emits
   MOVERS-22TF-03 inside `[09:00, 15:30] IST`.

**Auto-triage safe:** YES (Medium severity).

**Source:**
- `crates/core/src/pipeline/movers_22tf_scheduler.rs` (Phase 10)

## Cross-references

- `.claude/plans/v2-architecture.md` Section A (movers core)
- `.claude/plans/v2-risks.md` Risks #2 (drop SLA), #3 (drift), #4 (panic)
- `.claude/rules/project/audit-findings-2026-04-17.md` Rule 4 (edge-triggered)
- `crates/common/src/error_code.rs::Movers22Tf01WriterIlpFailed`,
  `Movers22Tf02SchedulerPanic`, `Movers22Tf03UniverseDrift`
