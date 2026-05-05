# Wave 1 Error Codes

> **Authority:** This file is the runbook target for the eight new
> ErrorCode variants added in the Wave 1 hardening implementation
> (PR #393). The cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires that
> every variant in `crates/common/src/error_code.rs::ErrorCode` is
> mentioned in at least one rule file under `.claude/rules/`.

## HOT-PATH-01 — sync filesystem I/O failed inside an async writer task

**Trigger:** the dedicated tokio task that drains the prev_close cache
mpsc channel (Wave 1 Item 0.a) failed an underlying
`tokio::fs::write` or `tokio::fs::rename` call. The hot path is NOT
blocked by this — the writer task logs ERROR and continues.

**Triage:**
1. `df -h data/instrument-cache/` — disk full?
2. `ls -la data/instrument-cache/` — permission / mount issue?
3. If both look healthy, restart the app (the writer task self-recovers
   on the next cache update).

**Source:** `crates/core/src/pipeline/prev_close_writer.rs::PrevCloseWriter::spawn`

## HOT-PATH-02 — prev_close writer queue full / closed / uninitialized

**Trigger:** `try_enqueue_global` could not push a `Bytes` payload to
the writer task's mpsc(64) channel. The cache update is dropped; the
next successful enqueue re-snapshots the full HashMap so the cache
becomes fresh again.

**Triage:**
1. Check `tv_prev_close_writer_dropped_total{reason}` — `full` means
   slow disk; `closed` means the writer task exited (look for
   HOT-PATH-01 errors); `uninit` means boot wiring missing.
2. If `closed`, restart the app.

**Source:** `crates/core/src/pipeline/prev_close_writer.rs::try_enqueue_global`

## PHASE2-01 — Phase 2 dispatch failed (LTPs absent or empty plan)

**Trigger:** `run_phase2_scheduler` emitted `Phase2Failed` after one
of: empty plan at trigger, dispatch_subscribe returned None (pool
saturated), retries exhausted (LTPs never arrived).

**Triage:** see `.claude/rules/project/depth-subscription.md` 2026-04-22
Updates §5 — full diagnostic fields are in the Telegram message.

## PHASE2-02 — Phase2EmitGuard dropped without firing

**Trigger:** the `Phase2EmitGuard` (Wave 1 Item 1) was dropped without
calling any of `emit_complete` / `emit_failed` / `emit_skipped`. In
debug builds this panics; in release builds it logs ERROR
`PHASE2-02` AND increments the Prometheus counter
`tv_phase2_emit_guard_dropped_total` (so the
`tv-phase2-02-emit-guard-dropped` alert fires). Indicates a regression
where the Phase 2 dispatcher returned without firing an outcome
notification.

**Triage:** find the recent edit to `phase2_scheduler.rs` that added a
new return path and ensure it consumes `guard.emit_*(...)` before
returning.

**Alert acknowledgement caveat:** the alert query is
`increase(tv_phase2_emit_guard_dropped_total[1d]) > 0` with a 24h
sliding window. A single regression keeps the alert firing for 24
hours; "acknowledge" in Alertmanager only suppresses pages for the
acknowledged window, not the underlying value. The alert returns to
OK on its own once the sample ages out, or immediately after a
Prometheus restart.

**Source:** `crates/core/src/instrument/phase2_emit_guard.rs::Drop`

## PREVCLOSE-01 — previous_close ILP write or flush failed

**Trigger:** the `prev_close_persist` async drain task hit an error
calling `PreviousCloseWriter::append_prev_close` or `force_flush`.

**Triage:**
1. `tv_prev_close_persist_errors_total{stage="append"}` increments —
   the QuestDB ILP append rejected the row (schema drift?).
2. `tv_prev_close_persist_errors_total{stage="flush"}` increments —
   ILP TCP connection broken or QuestDB down.
3. Run `make doctor` to confirm QuestDB health.

**Source:** `crates/core/src/pipeline/prev_close_persist.rs`

## PREVCLOSE-02 — first_seen_set inconsistency (reserved)

**Reserved** for a future signal where the first-seen set's IST-
midnight reset task fails to fire. Not currently emitted by any code
path; included so the enum variant exists for future use without
needing another rule-file edit.

**Source:** `crates/core/src/pipeline/first_seen_set.rs`

## PREVOI-01 — prev_oi cache empty at boot

**Trigger:** PR #450 commit 6 (2026-05-03) wires the boot-time
`spawn_movers_pipeline` call with an empty `Arc<HashMap<(u32, u8), i64>>`
because the actual bhavcopy + Option Chain prev_oi loader is deferred
to PR #452 (8:15 AM ready boot orchestrator). Until that orchestrator
ships, the new unified `/api/movers` endpoint's `OI Change` and
`OI Change %` columns compute as `current_OI - 0 = current_OI` —
a misleading value.

**Severity:** Medium (`error_code.rs` — fires a single boot-time WARN
gated to once-per-process per Wave-4 charter Rule 11 "no false-OK
signals").

**Triage:**
1. Verify the WARN appears in `data/logs/errors.jsonl.*` ONCE per
   process boot — not per-row, not per-tick.
2. Operators MUST NOT trust the OI Change / OI Change % columns on
   `/api/movers` until PR #452 ships.
3. Once PR #452 lands the bhavcopy + Option Chain loader, this
   warning disappears (cache is populated before
   `spawn_movers_pipeline` is invoked).

**Auto-triage safe:** YES (visibility-only WARN, no operator action
required during the PR #452 transition window).

**Source:** `crates/app/src/main.rs::spawn_movers_pipeline` boot
wiring + `crates/core/src/option_chain/prev_oi.rs::extract_prev_oi_from_option_chain`
+ `crates/core/src/instrument/bhavcopy_cross_check.rs::build_prev_oi_cache_from_bhavcopy`.
