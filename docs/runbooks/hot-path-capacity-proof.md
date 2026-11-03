# Hot-path latency and capacity proof — how to read it

## Strict 1,000 ns sample evidence (2026-10-31)

New latency rows can carry three nullable LONG columns: `strict_version`,
`strict_samples`, and `strict_samples_at_or_above_1000ns`. Version 1 uses
disjoint raw bins [0,999] and [1000,1024], with later bins unchanged. The
numerator and denominator come from the same raw snapshot difference and
minute accumulation. The existing p99 bounds, `histogram` labels and
`samples_over_1024ns` keep their historical meaning.

| Input | Recorded-sample result |
| --- | --- |
| Version absent/unsupported, either count absent/negative, violations greater than samples | UNKNOWN |
| Zero strict samples | UNKNOWN |
| Valid version 1 and violations ≤ integer `samples / 100` | Sample population's nearest-rank p99 is strictly below 1,000 ns |
| More violations | Observed strict FAIL |

Equality at 1,000 ns is a violating sample. The percentile permits its
specified tail; it does not claim every call is below the target. Use exact
integer arithmetic, not a rounded floating ratio. A 1,024 ns legacy p99
alone remains UNKNOWN. Receipt-to-fold's fine `samples` count is an
independent atomic population; never combine it with the strict numerator.

These fields establish the threshold classification of recorded samples
only. Missing freshness, elapsed observation intervals, excluded work or
persisted minutes still prevent a session PASS. The daily evaluator can
retain an observed strict FAIL but introduces no positive session path:
`strict_session_intervals_unproven` identifies positive exact counts that
still lack interval/freshness proof. `NOT_NEEDED` remains a capacity result.

Read the exact requested eligible date/window and stage, including the three
fields and `publishes`. Historical rows have NULL strict evidence. A schema
without these columns needs the additive ensure before a query can use them;
a failed query is missing evidence, not a fast result. Readiness now checks
all three columns are LONG before allowing proof writes. No table key,
CloudWatch metric/label, stall threshold or alarm changes.

## Existing operational and capacity evidence

Added 2026-10-30; coverage correction 2026-10-31. Three QuestDB tables provide evidence for two questions:

1. What latency was observed for the instrumented frame stages? The existing 1,024 ns first bucket cannot prove the strict p99 <1,000 ns target.
2. Is the 4-core server (r8g.xlarge) needed, or does the 2-core box keep up?

All three are written by the app itself. The `tv-telemetry` thread builds the rows once a minute, off the frame path, and the `tv-latency-minute` thread writes them. Nothing here pages, alarms or sends a Telegram message.

| Table | One row per | Key |
|---|---|---|
| `hot_path_latency_minute` | stage per minute | `ts, stage, feed` |
| `host_capacity_minute` | minute | `ts, feed` |
| `capacity_verdict_daily` | eligible calendar date, after the selected session's close check | `ts, trading_date_ist, feed` |

`ts` is IST. A NULL means "not measured", never zero.

## 1. Today's verdict

```sql
SELECT trading_date_ist, verdict, reasons, session_minutes,
       tick_step_p99_result, worst_tick_step, worst_tick_step_p99_ns,
       ticks_over_1ms_result, minutes_with_tick_over_1ms,
       cpu_wait_result, worst_reader_cpu_wait_ns_per_s, worst_drain_cpu_wait_ns_per_s,
       disk_result, disk_hot_minutes,
       backlog_result, backlog_txns_at_close, backlog_growing_tables_at_close,
       worst_host_cpu_busy_pct
FROM capacity_verdict_daily
WHERE ts IN today();
```

How to read `verdict`:

| Verdict | Meaning |
|---|---|
| `UPGRADE_NEEDED` | With complete required evidence, observed CPU wait failed its existing threshold, or close backlog failed while the host was at least 90% busy. This is a capacity signal, not proof that more cores alone fix the cause. |
| `NOT_NEEDED` | Neither happened. A missed 1 µs target, ticks over 1 ms or a busy disk can still appear in `reasons`. When the hot threads were not waiting for a CPU, more cores would not fix them. |
| `UNKNOWN` | A distinct expected minute, required signal or sample was missing; observed frames/rows were excluded; the calendar was unavailable; or the strict latency target is unresolved. The historical first bucket is unresolved even with otherwise complete coverage. |

The normal proof interval is [09:15,15:30) IST: all 375 distinct minutes are required, narrower than the 09:00–15:40 capture window. Muhurat uses the configured date and start/end, with a close check ten minutes after its end. Holidays, weekends and mock-only dates have no ordinary real-market verdict. Missing/unpublished/stale calendar information never falls back to an assumed weekday session. Unknown calendars can emit a named UNKNOWN at rollover; consumers must treat an absent current-date row as insufficient before then.

A process observed before session end emits UNKNOWN after the close check even when it saw no session rows. A process first observed entirely after close writes no row, preserving the daily dedup key and earlier evidence. A consumer must select the requested eligible date and left-join its data: no row is UNKNOWN, never yesterday's status. A late backlog reading cannot stand in for the scheduled close reading. `NOT_NEEDED` is a core-capacity decision and can coexist with a failed latency signal; never display it as latency PASS.

## 2. Observed frame-stage latency (per minute, today)

```sql
SELECT ts, stage, samples, p50_ns, p99_ns, p999_ns, max_ns,
       samples_over_1024ns, histogram, publishes
FROM hot_path_latency_minute
WHERE ts IN today()
  AND stage IN ('socket_to_wal', 'ring_dwell', 'drain_main_frame', 'receipt_to_fold')
ORDER BY ts, stage;
```

Worst minute per stage today:

```sql
SELECT stage, max(p99_ns) AS worst_p99_ns, max(p999_ns) AS worst_p999_ns,
       max(max_ns) AS worst_ns, sum(samples_over_1024ns) AS over_1024ns, sum(samples) AS samples
FROM hot_path_latency_minute
WHERE ts IN today()
GROUP BY stage
ORDER BY stage;
```

How to read it:

- `p99_ns = 1024` bounds the recorded population at 1,024 ns; it cannot distinguish 999, 1000 or 1001 ns. The strict result is UNKNOWN, not PASS. Historical later buckets demonstrate a miss. This correction closes false assurance; it does not achieve the latency target or alter existing histograms.
- `samples_over_1024ns` retains its exact historical meaning: above 1,024 ns. It is not a strict 1,000 ns classifier. Zero samples provide no percentile, and zero appended tick rows do not independently prove zero eligible frames.
- `socket_to_wal` and `ring_dwell` aggregate endpoints; `drain_main_frame` and `receipt_to_fold` cover main-feed frames. All use `feed='dhan'`, which does not separate endpoints. Counts are not comparable as one population. Decode/fold/append are combined in the frame timer; no individual depth/flush/maintenance p99 is established here.
- `ring_dwell` starts at the pre-WAL receipt instant and overlaps socket work; `receipt_to_fold` overlaps both. Do not sum these values or divide a frame percentile into a per-row percentile.
- `i64::MAX` means the value was above every histogram bound; read `max_ns` for the real value.
- `main_runtime_lag`, `reader_runtime_lag` and `metrics_upkeep` are not tick steps. They measure the lateness of a 100 ms sleep and the exporter upkeep.

## 3. Host evidence per minute (CPU wait, CPU, disk, QuestDB, ticks)

```sql
SELECT ts,
       reader_cpu_wait_max_ns_per_s, drain_cpu_wait_max_ns_per_s, wal_writer_cpu_wait_max_ns_per_s,
       process_cpu_pct, host_cpu_busy_pct, host_cpu_steal_pct,
       disk_read_bytes_per_s / 1048576.0 AS disk_read_mib_s,
       disk_write_bytes_per_s / 1048576.0 AS disk_write_mib_s, disk_write_await_us,
       questdb_backlog_txns, questdb_backlog_growing_tables,
       ticks_accepted, ticks_not_landed, ticks_lost, ring_refused_frames
FROM host_capacity_minute
WHERE ts IN today()
ORDER BY ts;
```

How to read it:

- The existing `*_cpu_wait_max_ns_per_s` columns contain maximum run-delay growth between sampling attempts, nominally one second apart. A delayed sampler spans a longer interval; it cannot reconstruct the maximum of hidden individual seconds. A missing aggregate observation now leaves that minute's wait NULL instead of retaining an earlier successful one. Existing group aggregation can still return a value when one registered member failed and another succeeded; per-member completeness needs additional scheduler provenance and is not certified by this patch.
- With `TICKVAULT_DRAIN_THREAD=on` and its registration present, `drain` is the dedicated frame-drain thread. Without a registered own drain it is the main-runtime worker sum, including the seal writer and other tasks, so shared mode supplies a proxy. An unreadable registered dedicated thread produces NULL, never a substitute background-worker reading. The existing `thread="main_workers"` metrics continue to describe the shared runtime.
- Disk figures are minute averages for the device behind the `data/` folder. The gp3 cap is 125 MiB/s, reads and writes together.
- `ring_refused_frames` counts main-feed frames omitted from downstream live-path timing. Their socket handoff is still timed. These refusals, observed not-landed/lost ticks, or missing loss-counter evidence make full live-path coverage incomplete even with a low measured-subset p99. Missing host utilization cannot justify NOT_NEEDED when backlog fails; an observed high utilization or independent CPU-wait failure remains affirmative capacity evidence.
- The first row after a restart has NULL rates (no previous reading).

## 4. Joined view: slow minutes next to their CPU wait

```sql
SELECT l.ts, l.p99_ns, l.max_ns, h.drain_cpu_wait_max_ns_per_s, h.reader_cpu_wait_max_ns_per_s,
       h.host_cpu_busy_pct, h.ring_refused_frames
FROM hot_path_latency_minute l
JOIN host_capacity_minute h ON l.ts = h.ts
WHERE l.ts IN today() AND l.stage = 'receipt_to_fold' AND l.max_ns > 1000000
ORDER BY l.ts;
```

## Known limits

- The verdict requires every selected distinct minute from one process. It does not merge separate process lifetimes into a whole-day proof.
- Matching positive `publishes`/scheduler counts check observation consistency, not elapsed-time completeness. 59 reads can be normal because the loop sleeps after its work. No exact-60 gate is used. Actual interval metadata is a separate follow-up; sampling delay/clock steps mean the current rows cannot prove exact wall-minute attribution or uninterrupted one-second CPU coverage.
- The process's last, partial minute is not written at exit.
- A backward clock step folds the following seconds into the current minute (`publishes` > 60).
- Log line: one `info` line per day with `source = "capacity_verdict"`.

The scheduled close check still reads un-timestamped cached WAL-watcher values. Matching the due minute prevents substituting a later observation; it does not prove the watcher completed a fresh successful poll. Read actual `wal_tables()` and fresh persisted rows for live verification.

## Writer recovery and schema readiness

The proof writer starts with its own closed readiness gate. Existing create/alter
statements must succeed, and metadata must confirm all three tables have WAL,
deduplication, the exact keys listed above, their identity-column types and `ts`
as the sole designated timestamp. HTTP success alone cannot open the gate. An
unreadable or oversized response (above 64 KiB) keeps it closed. The ensure task
retries with bounded backoff while the process runs; a rejected second writer
start does not leave an extra ensure task behind.

Before readiness, and during sender initialization or flush failure, rows stay
in the bounded writer buffer. Initialization is retried rather than permanently
disabling the writer. Retained rows are retried on the writer's timeout even if
no later producer message arrives. At the existing pending-row cap a failed
flush discards buffered rows and increments the existing discard metric; a full
handoff queue rejects the incoming message and increments its existing drop
counter. These counters are local `/metrics` evidence, not new alarms.

| Evidence | What it establishes | What it does not establish |
| --- | --- | --- |
| Ready gate | Successful ensure and verified metadata for this writer start | Detection of a later external schema change |
| Successful flush | The configured endpoint acknowledged the write | Power-loss durability or a complete persisted minute grid |
| Daily log line | The process computed a daily decision | That its queued row was written successfully |
| Expected-date row readback | Rows visible for the selected day and keys | Freshness of every cached input or exact sampling intervals |

Proof rows have no durable spool and no owned shutdown drain. Queue contents,
pending rows and the last partial minute can be lost on termination. A dead
writer after its successful start is not restarted by this correction. Keep
these limits visible when using the daily row: check the exact eligible date,
expected minute/stage grid, sample counts, drop/discard counters and current
database metadata. Missing or stale evidence remains UNKNOWN.
