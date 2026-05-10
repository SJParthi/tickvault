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

## AGGREGATOR-AUDIT-01 — `aggregator_seal_audit` table write failed

**Trigger:** the per-seal audit-table writer returned an error when
appending to `aggregator_seal_audit`. Audit tables are SEBI-relevant
forensic records; write failures must surface immediately. Severity::Medium.

**Triage:** identical to `AUDIT-01` (`Phase2AuditWriter::append`):
check QuestDB ILP TCP port + disk-full state.

**Source:** `crates/storage/src/aggregator_seal_audit_persistence.rs::AggregatorSealAuditWriter::append`.
