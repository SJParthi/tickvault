# Wave 1 Error Codes

> **Authority:** This file is the runbook target for the eight new
> ErrorCode variants added in the Wave 1 hardening implementation
> (PR #393). The cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires that
> every variant in `crates/common/src/error_code.rs::ErrorCode` is
> mentioned in at least one rule file under `.claude/rules/`.

## HOT-PATH-01 — sync filesystem I/O failed inside an async writer task

> **⚠ EMIT SITES DELETED 2026-07-17 (stage-2 dead-WS sweep):**
> `crates/core/src/pipeline/prev_close_writer.rs` — the sole emit surface for
> HOT-PATH-01 and HOT-PATH-02 — was DELETED with the dead Dhan tick chain
> (zero production callers after the 2026-07-13 Dhan live-WS retirement).
> The `HotPath01SyncFsFailed` / `HotPath02WriterQueueDrop` ErrorCode
> variants are RETAINED (no ErrorCode deletions in the sweep); neither code
> can fire again. Content below retained as historical audit.

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

> **⚠ EMIT SITES DELETED 2026-07-17 (stage-2 dead-WS sweep):** the prev-close
> persist drain lived inside the deleted dead tick chain (the cited source
> path `crates/core/src/pipeline/prev_close_persist.rs` never existed as a
> file on `main` — the drain task was wired through the deleted
> `tick_processor.rs` chain). The `PrevClose01IlpFailed` variant is
> RETAINED; the code can no longer fire. Historical audit below.

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

> **2026-07-17 note (dead live-WS sweep stage 1):** the source module
> `first_seen_set.rs` is DELETED — it had ZERO production callers (only
> comments referenced it since the PR-C2 Dhan live-WS lane deletion,
> 2026-07-13). The `PrevClose02` variant was already emitter-less
> ("reserved") and is RETAINED pending the post-sibling-merge variant
> sweep; this file keeps satisfying the cross-ref test.

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

## PREVCLOSE-04 — F2 PrevDayCache boot loader degraded

**Trigger:** F2 (Wave-5 §K-L13 / #504e follow-up, 2026-05-08) wires
`crates/app/src/prev_day_cache_loader.rs::populate_prev_day_cache_at_boot`
into the boot path so the cascade seal-time pct-stamping path
(PR #520 / F1) sees non-zero `prev_day_close` values from QuestDB's
`previous_close` table on cold boot. Two failure modes both surface
as `PREVCLOSE-04`:

1. **QuestDB unreachable** — the boot SELECT against `previous_close`
   failed (network timeout, table corruption, /exec returns non-2xx).
   Emitted at `error!` level so Loki routes to Telegram.
2. **`previous_close` empty** — the table has zero rows for the last
   7 trading days (lookback window pinned by
   `QUESTDB_LOOKBACK_DAYS = 7`). Fresh deployment, or an outage
   during the previous trading session that prevented
   `PreviousCloseWriter::append_prev_close` writes. Emitted at
   `warn!` level — degraded but expected on a brand-new deployment.

**App behaviour:** the cascade seal-time pct-stamping path falls
back to `0.0` for the 3 % fields (`close_pct_from_prev_day`,
`oi_pct_from_prev_day`, `volume_pct_from_prev_day`) per the
`compute_*_pct` div-by-zero policy. No tick is dropped, no seal is
lost — operator simply sees zero-valued % columns until the next
boot succeeds in reading `previous_close`.

**Severity:** Medium. Visibility WARN/ERROR for the operator;
NOT a boot-halt. The downstream read path is safe by construction.

**Triage:**

1. `mcp__tickvault-logs__questdb_sql "select count(*) from previous_close where ts > dateadd('d', -7, now())"`
   — 0 rows means the table is empty; either the previous session's
   `PreviousCloseWriter` never wrote (check for `PREVCLOSE-01`
   ILP failures) or the QuestDB partition was dropped.
2. `mcp__tickvault-logs__run_doctor` to confirm QuestDB health.
3. If the table has rows but the loader still fails: inspect the
   `error!` payload — the truncated body field carries the QuestDB
   `/exec` HTTP body (200 char cap).
4. After remediation, restart the app — the loader runs once at
   boot, no in-process retry. The next boot will pick up any rows
   that have since been written.

**Auto-triage safe:** YES (visibility-only WARN/ERROR; the cascade
fallback prevents data corruption).

**Source:**
- `crates/app/src/prev_day_cache_loader.rs::populate_prev_day_cache_at_boot`
- `crates/common/src/error_code.rs::PrevClose04CacheEmptyAtBoot`
- Boot wiring: `crates/app/src/main.rs` (after the
  `prev_oi cache populated` log).
