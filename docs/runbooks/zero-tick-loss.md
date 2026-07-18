# Runbook — Zero-tick-loss breach

> **⚠ RETIRED 2026-07-18 (stage-4 dead-producer sweep):** the tick ring→spill→DLQ chain this runbook pages on (`tick_persistence.rs`) was DELETED in the stage-2 dead-WS sweep (2026-07-17) — the runtime is REST-only and nothing writes the `ticks` table anymore. The `tv_spill_dropped_total` / `tv_dlq_ticks_total` / `tv_ticks_dropped_total` metrics have ZERO emit sites and their CloudWatch alarms were deleted from `app-alarms.tf` (2026-07-18). The candle-side seal chain keeps its own pagers (seal-drop-alarm.tf + AGGREGATOR-DROP-01). Content below retained as historical audit.


**When it fires:** `TicksDropped`, `TickBufferActive`,
`TickDiskSpillActive`, `TickDataLoss`, `STORAGE-GAP-01`,
`BroadcastLagTickLoss`, `WebSocketBackpressure`.

**Consequence if not resolved:** SEBI audit gap, strategy decisions
made on stale data, candle aggregation corruption. The three-tier
buffer (ring → disk spill → recovery) is designed to make this
impossible during normal operation.

## The three tiers

```
Live tick
  │
  ▼
┌──────────────────────────┐
│ Ring buffer (600K cap)   │  ← Tier 1: RAM, SPSC, 65 ns lookup
└──────────┬───────────────┘
           │ overflow when QuestDB slow/down
           ▼
┌──────────────────────────┐
│ Disk spill (data/spill/) │  ← Tier 2: Local disk, append-only
└──────────┬───────────────┘
           │ on QuestDB recovery
           ▼
┌──────────────────────────┐
│ QuestDB ILP writer       │  ← Tier 3: Permanent storage
└──────────────────────────┘
```

Only if ALL THREE fail does a tick actually drop. The metric
`tv_ticks_dropped_total` increments then. **Must remain 0 during
normal operation.**

## Telegram alert → action (60-second version)

| Alert | First action |
|---|---|
| 🟡 `TickBufferActive` | Normal — QuestDB is slow but catching up. Watch for escalation. |
| 🟠 `TickDiskSpillActive` | QuestDB was down OR slow for seconds. Buffer overflowed, spilling to disk. Expected to auto-drain on recovery. |
| 🔴 `TicksDropped` (`tv_ticks_dropped_total > 0`) | **Both buffers full.** Operator action required — see below. |
| 🔴 `TickDataLoss` (catch-all) | Check `errors.jsonl` for root cause, then this runbook. |
| 🟠 `WebSocketBackpressure` | SPSC channel lagging. Check consumer health (tick_processor). |
| 🟠 `BroadcastLagTickLoss` | Telegram/SNS notification fan-out lagging. Non-critical — live data still persisted. |

## Root-cause checklist

### 1. Is QuestDB up?

```bash
curl -sS http://localhost:9000/status | jq .
# Expected: {"status":"Ok"}

curl -sS http://localhost:9091/metrics | grep tv_questdb_connected
# Expected: tv_questdb_connected 1
```

If QuestDB is down: start it (`make docker-up`). Tier 2 will drain
automatically once it's back.

### 2. Disk pressure?

```bash
df -h /var/lib/tickvault/data  # QuestDB volume
df -h data/spill/              # Spill volume (usually same)
```

If either > 85%: expand EBS volume OR run the partition manager
manually (`make partition-manager-run` if Makefile target exists, or
invoke the maintenance binary directly).

### 3. Is the pipeline consuming ticks?

```bash
curl -sS http://localhost:9091/metrics | grep -E "tv_ticks_processed_total|tv_pipeline_active"
```

`tv_pipeline_active == 0` AND outside market hours = normal (pipeline
sleeps).

`tv_pipeline_active == 0` AND inside market hours = **abnormal** →
the tick_processor task crashed. Check `errors.log` for a panic trace.

### 4. Ring buffer saturation history

```bash
# Check the buffer size history via the metrics exporter / CloudWatch.
# Buffer at > 100K = backpressure. Buffer at 600K+ = spilling.
# Query the tv_tick_buffer_size metric in the CloudWatch console (prod)
# or curl the app's /metrics endpoint (dev). The prometheus_query MCP
# tool was retired in #O5 (2026-05-30) — Prometheus container removed in #O3.
```

## Recovery

### Spill files present but not draining

Symptom: `data/spill/ticks-YYYYMMDD.bin` exists, QuestDB is up,
ticks not flowing back.

```bash
# Trigger the drain helper (Phase 8.1)
scripts/auto-fix-clear-spill.sh --dry-run    # see what it would do
scripts/auto-fix-clear-spill.sh              # execute
```

If the helper returns `exit 2` with "drain endpoint pending": the
drain endpoint on the app isn't shipped yet. Fallback: restart the
app — on boot it auto-drains stale spill files via
`recover_stale_spill_files()`.

```bash
make stop && make run
```

### Ticks actually dropped (`tv_ticks_dropped_total > 0`)

This is an SEBI audit gap. Required actions:

1. **Capture evidence** — dump the failing time window's logs:
   ```bash
   make tail-errors | grep -E "drop|spill" > /tmp/tick-loss-$(date +%s).txt
   ```
2. **Open a GitHub Issue** with the log dump attached. Title:
   `Tick loss at <IST timestamp>`.
3. **Do NOT wipe `data/spill/`** — those files are the evidence that
   tier-2 worked.
4. **Estimate loss** — compare `tv_ticks_processed_total` against
   `tv_ticks_dropped_total` over the window.
5. **Reply to SEBI audit trail** if material.

### WebSocketBackpressure

The SPSC channel between the WebSocket reader and the tick_processor
is full. Causes:

- tick_processor stuck on a slow ILP write — fix QuestDB pressure
- tick_processor panicked — restart the app
- Channel capacity misconfigured — _(historical: `TICK_BUFFER_CAPACITY` +
  its `zero_tick_loss_alert_guard` were deleted 2026-07-18 with the tick
  rescue ring; the live bound is `SEAL_BUFFER_CAPACITY` = 200_000, ratcheted
  in `crates/trading/src/candles/seal_ring.rs`)_

## Never do these

- **Never delete `data/spill/*.bin` without draining them first.** The
  files represent ticks not yet in QuestDB. Deleting = SEBI violation.
- **Never lower `SEAL_BUFFER_CAPACITY` below 200K.** The seal_ring.rs
  lib ratchet (`test_seal_buffer_capacity_constant_is_locked_value`) blocks
  this at the unit-test level _(2026-07-18: re-pointed from the deleted
  tick-ring constant + guard)_.
- **Never disable the loss pagers.** Today these are the AGGREGATOR-DROP-01
  routes — the `tv-<env>-errcode-aggregator-drop-01` log-filter alarm + the
  `tv-<env>-seal-writer-dropped` counter alarm — pinned by
  `seal_drop_paging_wiring_guard.rs` _(2026-07-18: the tick-side alarms +
  the `zero_tick_loss_alert_guard` emission pins were deleted with the dead
  tick chain, stage-4 sweep)_.

## Preventive measures

1. **Weekly disk audit** — EBS volume utilization trend.
2. **Monthly spill-drain dry-run** — verify
   `auto-fix-clear-spill.sh --dry-run` exits clean.
3. **Chaos rehearsal** — `docker pause tv-questdb` for 60s, verify
   spill-to-disk triggers and drain-on-resume works. Run quarterly.

## Related files

- `tick_persistence.rs` — the 3-tier (ring → spill → DLQ) tick buffer
  logic. DELETED 2026-07-17 (stage-2 dead-WS sweep): the tick writer had
  zero production callers after the live-WS retirements (Dhan 2026-07-13,
  Groww 2026-07-15); nothing writes the `ticks` table anymore. The
  candle-side absorption chain (seal ring → spill → DLQ) lives on in the
  seal/shadow writers and is what this runbook's tiers map to today.
- `crates/trading/src/candles/seal_ring.rs` — `SEAL_BUFFER_CAPACITY` +
  its lib ratchets _(2026-07-18: `TICK_BUFFER_CAPACITY` was deleted from
  constants.rs with the tick rescue ring)_
- `zero_tick_loss_alert_guard` — DELETED 2026-07-18 (stage-4 dead-producer
  sweep) with the tick rescue ring; its role is covered by the seal_ring.rs
  ratchets + the AGGREGATOR-DROP-01 pagers
- `scripts/auto-fix-clear-spill.sh` — drain helper
- CloudWatch operator-health dashboard — the buffer/spill/DLQ tiers (the
  local Grafana `operator-health.json` panels 8/9/10 were retired in #O1,
  2026-05-19)
