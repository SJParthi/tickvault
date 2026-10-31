# Hot-path latency and capacity proof — how to read it

Added 2026-10-30. Three QuestDB tables answer two questions with no manual digging:

1. Does every in-memory step a tick passes through stay under 1 µs at p99?
2. Is the 4-core server (r8g.xlarge) needed, or does the 2-core box keep up?

All three are written by the app itself. The `tv-telemetry` thread builds the rows once a minute, off the frame path, and the `tv-latency-minute` thread writes them. Nothing here pages, alarms or sends a Telegram message.

| Table | One row per | Key |
|---|---|---|
| `hot_path_latency_minute` | stage per minute | `ts, stage, feed` |
| `host_capacity_minute` | minute | `ts, feed` |
| `capacity_verdict_daily` | trading day (written at the first minute at or after 15:40 IST) | `ts, trading_date_ist, feed` |

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
ORDER BY ts DESC
LIMIT 5;
```

How to read `verdict`:

| Verdict | Meaning |
|---|---|
| `UPGRADE_NEEDED` | A hot thread waited at least 1 ms for a CPU inside one second, or QuestDB's backlog was still high at 15:40 while the host was at least 90% busy. More cores fix these. |
| `NOT_NEEDED` | Neither happened. A missed 1 µs target, ticks over 1 ms or a busy disk can still appear in `reasons`. When the hot threads were not waiting for a CPU, more cores would not fix them. |
| `UNKNOWN` | Fewer than 300 of the 375 session minutes were seen (late boot, or a restart), or a signal had no data. It is never green on missing data. |

A process that saw no session minute writes no verdict, so a restart after the close never overwrites the day's row.

## 2. Is every tick step under 1 µs? (per minute, today)

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
- The percentiles are bucket upper bounds and never read low. `p99_ns = 1024` proves the minute's p99 was at most 1,024 ns.
- "Over 1 µs" means over 1,024 ns, the histograms' first bucket bound. Counting exactly 1,000 would need one more compare on the recording path.
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
- `*_cpu_wait_max_ns_per_s` is the worst single second in the minute, in nanoseconds the group sat runnable but not running. `1000000` means the threads waited 1 ms within one second.
- The `drain` group is every main-runtime worker thread (the frame drain, the seal writer and every other main-runtime task).
- Disk figures are minute averages for the device behind the `data/` folder. The gp3 cap is 125 MiB/s, reads and writes together.
- `ring_refused_frames` counts main-feed frames that skipped every live stage and waited in the WAL. A minute with refusals is not fully covered by section 2.
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

- The verdict covers only the minutes one process saw. After a mid-session restart it is likely `UNKNOWN`.
- Minute alignment is ±1 s.
- The process's last, partial minute is not written at exit.
- A backward clock step folds the following seconds into the current minute (`publishes` > 60).
- Log line: one `info` line per day with `source = "capacity_verdict"`.
