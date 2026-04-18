# Runbook — Zero-tick-loss breach

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
# Check the buffer size history via Prometheus.
# Buffer at > 100K = backpressure. Buffer at 600K+ = spilling.
curl -sS 'http://localhost:9090/api/v1/query?query=tv_tick_buffer_size' | jq .
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
- Channel capacity misconfigured — check `TICK_BUFFER_CAPACITY` (must
  be ≥ 100_000 per `zero_tick_loss_alert_guard`)

## Never do these

- **Never delete `data/spill/*.bin` without draining them first.** The
  files represent ticks not yet in QuestDB. Deleting = SEBI violation.
- **Never lower `TICK_BUFFER_CAPACITY` below 100K.** The
  `zero_tick_loss_alert_guard` blocks this at the unit-test level.
- **Never disable the three Prometheus tick-loss alerts.** Pinned by
  `zero_tick_loss_alert_guard` — build fails if removed.

## Preventive measures

1. **Weekly disk audit** — EBS volume utilization trend.
2. **Monthly spill-drain dry-run** — verify
   `auto-fix-clear-spill.sh --dry-run` exits clean.
3. **Chaos rehearsal** — `docker pause tv-questdb` for 60s, verify
   spill-to-disk triggers and drain-on-resume works. Run quarterly.

## Related files

- `crates/storage/src/tick_persistence.rs` — 3-tier buffer logic
- `crates/common/src/constants.rs` — `TICK_BUFFER_CAPACITY`
- `crates/storage/tests/zero_tick_loss_alert_guard.rs` — 7 pinned invariants
- `deploy/docker/prometheus/rules/tickvault-alerts.yml` — 4 zero-tick-loss alerts
- `scripts/auto-fix-clear-spill.sh` — drain helper
- `deploy/docker/grafana/dashboards/operator-health.json` — panels 8/9/10 show each tier
