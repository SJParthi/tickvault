---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/storage/src/shadow_persistence.rs"
  - "crates/storage/src/seal_writer_loop.rs"
  - "crates/app/tests/seal_drop_paging_wiring_guard.rs"
  - "crates/storage/tests/wave_6_aggregator_alert_guard.rs"
  - "crates/trading/src/aggregator/multi_tf.rs"
  - "crates/app/src/groww_bridge.rs"
  - "crates/app/src/main.rs"
  - "crates/trading/src/candles/heartbeat.rs"
  - "crates/trading/src/aggregator/heartbeat.rs"
  - "crates/trading/src/aggregator/boundary_timer.rs"
  - "crates/trading/src/candles/multi_tf_aggregator.rs"
  - "crates/trading/src/candles/aggregator_cell.rs"
  - "crates/app/tests/aggregation_task_wiring_guard.rs"
  - "crates/storage/src/ws_frame_spill.rs"
---

# Wave 6 Error Codes

> **Authority:** This file is the runbook target for the Wave 6 ErrorCode
> variants added across Sub-PRs #1–#4 of
> `.claude/plans/active-plan-aggregator-direct-flush-rehydrate.md`.
> **Cross-ref test** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires every variant in `crates/common/src/error_code.rs::ErrorCode` to
> be mentioned in at least one rule file under `.claude/rules/`. This file
> serves that contract for the codes Sub-PR #1 introduces. Sub-PR #2/#3/#4
> will append further sections as they land (REHYDRATE-*, BUFFER-GATE-*,
> POSTMARKET-*, VERIFY-*).

## AGGREGATOR-DROP-01 — sealed candle dropped after ring+spill+DLQ all failed

**Trigger:** the multi-TF aggregator sealed a candle, attempted to flush
it via the ring buffer (`SEAL_BUFFER_CAPACITY`), the disk spill
(`data/spill/seals-YYYYMMDD.bin`) and the NDJSON DLQ
(`data/dlq/seals-*.ndjson`); ALL three absorbing tiers refused the row.
This is the only code path that constitutes silent data loss for a
sealed candle. Severity::Critical.

**Why this is Critical and not Medium:** the mirror of `tick_persistence`
ring→spill→DLQ explicitly absorbs the IST-midnight burst (~99K seals
across 11K instruments × 9 TFs). If all three tiers fail, the host is
out of memory AND out of disk AND the `data/dlq/` directory is
unwritable — by definition catastrophic.

**Triage:**
1. `mcp__tickvault-logs__docker_status` — is the host OOM-killed?
2. `df -h /data` — is the volume full?
3. `ls -la data/spill/ data/dlq/` — are the directories writable?
4. If host is healthy and dirs are writable, restart the app — the
   bounded ring resets and resumes from healthy state.

**Source:** `crates/storage/src/shadow_persistence.rs::ShadowCandleWriter::handle_drop`.

### 2026-07-09 Update — AGGREGATOR-DROP-01 now PAGES (dual route)

The 2026-07-09 audit confirmed this Severity::Critical code paged NOBODY
post-CloudWatch-migration: no `error_code_alerts` entry existed and the
companion counter never reached CloudWatch. Both routes are now live:

1. **errcode log-filter alarm** (`tv-<env>-errcode-aggregator-drop-01`,
   `deploy/aws/terraform/error-code-alarms.tf`): the emit site —
   `crates/storage/src/seal_writer_loop.rs::record_cycle_observability`,
   `error!(code = ErrorCode::AggregatorDrop01.code_str(), dropped = N)` per
   drain cycle with a non-zero truly-dropped count — flows
   errors.jsonl → CW Logs `/tickvault/<env>/app` → filter
   `{ $.code = "AGGREGATOR-DROP-01" && $.level = "ERROR" }` →
   `tv_errcode_aggregator_drop_01` → alarm (≤5 min) → SNS → Telegram.
   `ok_recovery = false`: the loss is PERMANENT — an auto-OK ~15 min after
   the episode ages out can never mean the candles came back (Rule-11).
2. **counter-side pager** (`tv-<env>-seal-writer-dropped`,
   `deploy/aws/terraform/seal-drop-alarm.tf`, the
   `feed-stall-restart-alarm.tf` house pattern): a log metric filter on
   `/tickvault/<env>/metrics` extracts the per-scrape deltas of
   `tv_seal_writer_drain_total{kind="dropped"}` into the DERIVED
   `tv_seal_writer_drain_dropped_total` [host] metric (kind-sliced so the
   busy submitted/flushed series never inflate the Sum; distinct name so a
   future unfiltered extraction can never double-count), alarmed at
   Sum ≥ 1 per aligned 300s window, always armed (a drop at the
   IST-midnight force-seal burst is equally permanent), no ok_actions.
   Redundancy rationale: a drop still pages if the errors.jsonl shipping
   leg is degraded (the 2026-07-06 collect_list incident class).
   **First-sample baseline (the feed-stall round-5 lesson):** the CW
   agent's delta pipeline drops each counter series' first sample as its
   baseline; since `kind="dropped"` increments only on a real drop, a
   lazily-born series would lose the session's FIRST drop entirely — so
   main.rs pre-registers the dropped series at 0 immediately after the
   metrics recorder installs (boot Step 2, next to the stall-restart
   registration), making it dense from boot.

Lockstep ratchet: `crates/app/tests/seal_drop_paging_wiring_guard.rs`
(5 tests — post-install registration order, emit-site stub-guard, both
terraform shapes built from the real `code_str()` / metric literals, plus
a string-aware HCL-comment-stripper self-test so tf comments can never
satisfy the shape needles).
Honest envelope: paging latency ≤ ~5 min on either route; a drop inside the
pre-install boot window increments a no-op counter handle (route 2 blind;
physically implausible — ring+spill+DLQ must all fail on live seal traffic
inside the boot prefix) but route 1 still sees its ERROR line; the delta
counter shape is not live-verified from the sandbox — if it ever proved
cumulative, Sum over-pages (fail-loud, never a silent miss).

### 2026-05-11 Update — 4-alert drop-class family is now live

Per Wave 6 Sub-PR #1 items 1.4j/l/n/o (merged #584/#587/#589/#590) the
operator has Telegram coverage for EVERY drop class shown on the 1.4k
dashboard panel:

| Prometheus alert uid | Counter | Threshold | for | PR |
|---|---|---|---|---|
| `tv-aggregator-no-seals-during-market` | `tv_aggregator_seals_emitted_total == 0` | seals == 0 / 5m | 5m | #584 |
| `tv-aggregator-mpsc-drop-storm` | `tv_seal_mpsc_dropped_total` | > 100 / 1m | 1m | #587 |
| `tv-aggregator-broadcast-lag-storm` | `tv_aggregator_tick_lag_total` | > 100 / 1m | 1m | #589 |
| `tv-aggregator-late-tick-sustained` | `tv_aggregator_late_ticks_discarded_total` | > 300 / 5m sustained | 5m | #590 |

All 4 alerts:
- Gated by `tv_market_hours_active == 1` (audit-findings Rule 3)
- Severity::High (pages Telegram)
- Wrapped in `increase()` per Rule 12

Family completeness is meta-ratcheted by 3 tests in
`crates/storage/tests/wave_6_aggregator_alert_guard.rs` (item 1.4p,
merged #591). Future deletion of any single alert OR severity
downgrade OR market-hours-gate removal fails the build.

## AGGREGATOR-LATE-01 — tick arrived after its bucket sealed (discarded)

> **⚠ EMIT SITES DELETED 2026-07-17 (stage-3 dead-WS sweep):** the 21-TF TICK
> aggregator (`multi_tf_aggregator.rs` / `aggregator_cell.rs` — the late-arm
> emit sites) was DELETED — it had ZERO tick publishers after the live-WS
> retirements (Dhan 2026-07-13, Groww 2026-07-15); the REST-only runtime
> derives candles via the bar-fold (FOLD-01). The `AggregatorLate01...`
> variant is RETAINED; the code can never fire again. Content below retained
> as historical audit.
> **[ARCHIVED 2026-07-20]** AGGREGATOR-LATE-01 body (emit sites deleted 2026-07-17; variant retained; historical audit) — moved verbatim to `docs/rules-archive/wave-6-error-codes-archive.md` (context-size incident; content unchanged).
## AGGREGATOR-SEAL-01 — seal-time ILP write to a shadow table failed (ring caught it)

**Trigger:** at seal time the aggregator attempted to write the sealed
candle to one of the 9 `candles_*_shadow` tables via QuestDB ILP; the
ILP buffer/flush returned an error; the row was caught by the ring
buffer (`SEAL_BUFFER_CAPACITY`). Severity::Medium — the data is NOT
lost, just buffered; Telegram alert ensures the operator knows.

**Note (PR1 H11 fix):** the legacy `LiveCandleWriter` at
`crates/storage/src/candle_persistence.rs:636` previously logged this
condition at `warn!`. The PR1 precursor commit upgrades that site to
`error!(code = ErrorCode::AggregatorSeal01IlpFailed.code_str(), ...)`
so it routes through Telegram per `error_level_meta_guard.rs` Rule 5.

**Triage:**
1. Counter `tv_shadow_writer_buffered_total{table}` rate — if it
   sustains, QuestDB ILP is degraded; check `BOOT-01`/`BOOT-02`.
2. Inspect `data/spill/seals-*.bin` size — growing means the ring is
   filling and disk-spill is engaging.

**Source:** `crates/storage/src/shadow_persistence.rs::ShadowCandleWriter`
+ existing fix at `candle_persistence.rs::flush_buffer` (legacy path).

### 2026-07-06 Update — silent candle-persist exam bug closed (HTTP ACK + loud loop)

The 2026-07-06 groww-only live exam exposed the worst-case silent variant:
~71K candles sealed in RAM, ZERO rows in the `candles_*` tables, ZERO log
lines from the seal-writer leg — no AGGREGATOR-SEAL-01, no heartbeat. Two
verified holes, both fixed on branch `claude/fix-candle-writer-recovery`:

1. **Fire-and-forget ILP TCP** — `ShadowCandleWriter` used
   `tcp::addr=host:ilp_port`. A server-side reject NEVER returned `Err`
   (the same class as the 2026-07-05 `ws_event_audit` empty-table
   incident), so every drain cycle "flushed Ok" (or quietly recovered via
   reconnect+replay at `debug!`) while QuestDB discarded the rows. The
   writer now uses **ILP-over-HTTP**
   (`http::addr=host:http_port;protocol_version=1;`) — every flush gets a
   per-request server ACK, so a reject surfaces as `Err`, `drain_once`
   fires `error!(code = AGGREGATOR-SEAL-01)` and the ring→spill→DLQ rescue
   engages. IN-CYCLE reconnect + same-buffer replay is KEPT (recovery now
   logs at `info!`, matching the tick writer's visible line). Ratchet:
   `shadow_candle_writer::tests::test_ilp_conf_targets_http_port_not_tcp`.
   **2026-07-06 hostile-review hardening (same branch):** (a) after a
   failed flush, `drain_once` now rescues the popped seals to spill/DLQ
   AND calls `ShadowCandleWriter::discard_pending()` — cross-cycle buffer
   retention would replay a server-REJECTED row forever (one poisoned row
   = dead candle leg for the session) and grow the buffer without bound
   during an outage toward the questdb-rs 100 MiB `max_buf_size` wedge;
   the spill/DLQ tier is the durable floor for the rescued rows. (b) The
   HTTP conf pins `retry_timeout=0` (the questdb-rs INTERNAL 10s
   sleep-and-resend loop is disabled — the writer's own bounded
   reconnect ladder owns retry) + `request_timeout=5000` ms, and the loop
   runs each drain cycle under `block_in_place` on the multi-thread
   runtime — a failed cycle blocks ≤ ~30s worst-case (was ~80-120s of
   synchronous blocking pinning a tokio worker with the library
   defaults). (c) The observability ratchet below now scans the
   PRODUCTION region only (split at `#[cfg(test)]`) — the previous
   whole-file `include_str!` scan matched its own assertion literals
   (vacuous; false-OK class); mutation-verified.
   Additional ratchets:
   `seal_writer_task::tests::test_drain_once_discards_writer_buffer_after_flush_failure_rescue`,
   `shadow_candle_writer::tests::test_discard_pending_clears_buffer_and_pending_for_clean_next_cycle`.
2. **The loop dropped every `CycleOutcome`** — the Wave-6 item-1.4 counter
   fan-out was never wired, and truly-dropped seals never fired
   AGGREGATOR-DROP-01. `run_seal_writer_loop` now fans every cycle into
   `tv_seal_writer_drain_total{kind=submitted|flushed_rows|flush_failed|
   rescued_spill|rescued_dlq|dropped}`, fires
   `error!(code = ErrorCode::AggregatorDrop01.code_str())` on any
   truly-dropped seal, and emits an UNCONDITIONAL once-per-60s `info!`
   progress report (`SEAL_WRITER_PROGRESS_REPORT_SECS`) — silence can never
   again mean unknown. Ratchet:
   `seal_writer_loop::tests::test_ratchet_loop_wires_observability_not_silence`.

## AGGREGATOR-HB-01 — per-minute aggregator seal-burst heartbeat (positive signal)

> **⚠ EMIT SITES DELETED 2026-07-17 (stage-3 dead-WS sweep):**
> `crates/trading/src/candles/heartbeat.rs` (the sole emit surface) was
> DELETED with the publisher-less tick aggregator. The
> `AggregatorHb01Heartbeat` variant is RETAINED; the positive-signal role
> for the surviving REST-era chain is `tv_rest_candle_fold_heartbeat_total`
> + the seal-writer progress report. Content below retained as historical
> audit.

**Trigger:** every minute boundary, after the seal burst completes, the
aggregator emits a coalesced 60s heartbeat carrying
`(seals_emitted, seals_dropped, late_ticks_discarded)`. Severity::Info.

**Why it exists:** per `audit-findings-2026-04-17.md` Rule 11 the system
MUST have a positive false-OK avoidance signal. Without this heartbeat,
the operator only learns aggregator is dead when Sub-PR #3's post-market
cross-verify fails — a long latency for a hot-path subsystem.

**Triage:** none. Absence of this code for > 90 s during market hours
should be detected by SLO-02 (the composite score weakest dimension
becomes `aggregator_health`).

**Source:** `crates/trading/src/aggregator/heartbeat.rs::emit_seal_burst_heartbeat`.

## BOUNDARY-01 — missed-boundary catch-up seal fired

> **⚠ EMIT SITES DELETED 2026-07-17 (stage-3 dead-WS sweep):** the watermark
> catch-up sealer (`multi_tf_aggregator.rs::catch_up_seal_all` + the main.rs
> Task 4 driver) and the IST-midnight/close-time force-seal tasks were
> DELETED with the publisher-less tick aggregator. The
> `Boundary01CatchupSeal` variant is RETAINED; the
> `tv-<env>-boundary-catchup-storm-dhan` alarm, its window-gate entry, the
> dashboard widget and the [host,feed] EMF declaration were retired in the
> SAME PR (dated notes in silent-feed-alarms.tf S2 /
> market-hours-liveness-alarm.tf / dashboard.tf / app-alarms.tf). Content
> below retained as historical audit.

> **[ARCHIVED 2026-07-20]** BOUNDARY-01 body (emit sites deleted 2026-07-17; variant retained; historical audit) — moved verbatim to `docs/rules-archive/wave-6-error-codes-archive.md` (context-size incident; content unchanged).
## AGGREGATOR-LAG-01 — candle aggregator tick-broadcast lagged (zero-tick-loss PR-8b, H2-lite)

> **⚠ EMIT SITES DELETED 2026-07-17 (stage-3 dead-WS sweep):** the emit site
> (the aggregator subscriber's `RecvError::Lagged` arm in main.rs) was
> DELETED with the tick-aggregator driver — the broadcast had no publisher.
> The `AggregatorLag01TickLagDropped` variant is RETAINED; the storage-side
> `aggregator_lag_loud_guard.rs` ratchet was deleted with its subject.
> Content below retained as historical audit.

> **[ARCHIVED 2026-07-20]** AGGREGATOR-LAG-01 body (emit sites deleted 2026-07-17; variant retained; historical audit) — moved verbatim to `docs/rules-archive/wave-6-error-codes-archive.md` (context-size incident; content unchanged).
