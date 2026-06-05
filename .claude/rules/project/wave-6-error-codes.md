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

**Trigger:** at a minute boundary the aggregator sealed a 1m bucket; a
tick whose `exchange_timestamp` falls inside that bucket arrives ≥1 ms
after the seal completes. The aggregator MUST NOT silently merge the
late tick into the next bucket (would shift data across timestamps);
MUST NOT silently discard it; the only correct action is `error!` log
+ counter increment + discard. Severity::High.

**Triage:**
1. Counter `tv_aggregator_late_tick_total{action="discard"}` rate — a
   sustained > 1/sec rate indicates clock drift between Dhan and our
   host (BOOT-03 territory) OR a slow consumer keeping ticks in the
   pipeline channel longer than the boundary period.
2. Cross-check `tv_websocket_connections_active` and
   `tv_aggregator_seal_in_progress_duration_ns` — if seal duration
   spikes near the boundary, the lock-free seal is contending; raise
   the per-cell shard count or shorten the seal critical section.

**Source:** `crates/trading/src/aggregator/multi_tf.rs::AggregatorEngine::on_tick`
+ the `seal_in_progress` epoch fence per cell.

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

## AGGREGATOR-HB-01 — per-minute aggregator seal-burst heartbeat (positive signal)

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

**Trigger:** the boundary timer detected `last_seen_minute < expected_minute - 1`
(one or more minute boundaries skipped, typically due to OS scheduler
preemption, clock slew, or a slow consumer). The catch-up seal walks
forward from the last-seen minute to the current expected minute,
sealing each missed bucket with the in-cell state. Severity::Medium.

**Why this is not Critical:** the in-memory cell still holds the
correct OHLCV state for the missed bucket; the seal is correct, just
late. The Critical condition would be `last_seen_minute < expected_minute - K`
for some K (current value: 5 minutes); above K the run was preempted
for so long that the boundary timer can no longer trust its own
state — that case escalates to `AGGREGATOR-DROP-01`.

**Triage:**
1. Counter `tv_boundary_catchup_total` — repeated firing within a
   single minute window indicates wall-clock instability (re-check
   BOOT-03).
2. Inspect the `data/logs/errors.jsonl.*` event payload's
   `missed_minutes` field; if > 1 minute, escalate.

**Source:** `crates/trading/src/aggregator/boundary_timer.rs::tick_boundary_loop`.

## AGGREGATOR-LAG-01 — candle aggregator tick-broadcast lagged (zero-tick-loss PR-8b, H2-lite)

**Trigger:** the candle aggregator's `tokio::broadcast` receiver
(`spawn_seal_writer_loop` in `crates/app/src/main.rs`) returned
`RecvError::Lagged(n)` — the aggregator fell so far behind that the
broadcast dropped `n` ticks from ITS view. `TICK_BROADCAST_CAPACITY`
is 262,144 (~52 seconds of buffer at ~5K ticks/sec across the 243-SID
universe), so a `Lagged` means the aggregator task stalled for tens of
seconds — a serious incident (OOM-pressure, CPU starvation, a blocked
seal-writer). Severity::High.

**CRITICAL ASSURANCE — ticks are NOT lost and NOT reordered.** The
dropped ticks are dropped only from the *aggregator's* broadcast view.
The lossless + ORDERED durable record is the **WAL frame spill**
(`crates/storage/src/ws_frame_spill.rs`): raw frames are captured by the
WS read loop *before* any broadcast fan-out, into single-producer FIFO
segments (ring → disk spill → DLQ), replayed in exact append order on
boot. Because this broadcast `Lagged` is strictly *downstream* of that
WAL, it can affect ONLY the derived candles (`candles_*_shadow`) for the
lagged window — never the durable tick record, never tick order. (The
`ticks` table's own persistence consumer is also a broadcast subscriber
that can lag — but it is backfilled from the WAL on recovery and alarmed
via `tv_ticks_permanently_lost`, so the WAL, not the ticks-table
consumer, is the lossless guarantor.) The 15:31 IST post-market 1m
cross-verify pinpoints the affected minutes for rebuild from the
WAL-backed, ts-ordered `ticks` table. Tick routing + ordering on the
live WS read loop are untouched by this code path.

**Why it was upgraded from a silent counter (audit Rule 5):** before
PR-8b the `Lagged` arm only did `counter!("tv_aggregator_tick_lag_total")`
— a candle-data-loss-class event with zero operator signal. It now emits
`error!(code = AGGREGATOR-LAG-01)` so it routes to Telegram + the
`errors.jsonl` forensic sink.

**Triage:**
1. The `error!` payload carries `skipped` (tick count). Inspect
   `data/logs/errors.jsonl.*` for the timestamp → that is the lagged
   window.
2. Root-cause the stall: `mcp__tickvault-logs__run_doctor` + check host
   memory/CPU (a >52s stall implies OOM pressure or a blocked
   seal-writer — cross-check `AGGREGATOR-SEAL-01` / `AGGREGATOR-DROP-01`
   and `tv_seal_mpsc_dropped_total`).
3. **Rebuild the affected candles (no tick is lost):** the 15:31 IST
   post-market 1-minute cross-verify (`CROSS-VERIFY-1M-01`) compares
   `candles_1m` vs Dhan's authoritative 1m candles, exact-match, and
   names every mismatched minute. Those minutes are rebuildable from the
   lossless, ordered `ticks` table.

**Auto-triage safe:** NO (Severity::High; a >52s aggregator stall needs
operator root-cause — the candle under-count is recoverable but the
underlying stall is not self-healing).

**Source:** `crates/app/src/main.rs` (the aggregator subscriber
`RecvError::Lagged` arm in `spawn_seal_writer_loop`),
`crates/common/src/error_code.rs::AggregatorLag01TickLagDropped`.
