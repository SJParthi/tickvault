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
- `socket_to_wal` and `ring_dwell` aggregate endpoints; `drain_main_frame` and `receipt_to_fold` cover main-feed frames. All use `feed='dhan'`, which does not separate endpoints. Counts are not comparable as one population. Decode/fold/append are combined in the frame timer; no individual depth/flush/maintenance p99 is established here. (From 2026-11-13 the frame arm's size-trigger hand-offs and its 1,024-frame upkeep are measured together in `drain_frame_post`, below; the drain's other arms are not, see the list there.)
- `ring_dwell` starts at the pre-WAL receipt instant and overlaps socket work; `receipt_to_fold` overlaps both. Do not sum these values or divide a frame percentile into a per-row percentile.
- `i64::MAX` means the value was above every histogram bound; read `max_ns` for the real value.
- `main_runtime_lag`, `reader_runtime_lag` and `metrics_upkeep` are not tick steps. They measure the lateness of a 100 ms sleep and the exporter upkeep.
- **`ts` is IST written as Z (2026-11-06, PR DP-6).** The minute stamp is the IST wall clock stored in the designated timestamp, so QuestDB shows it with a `Z` suffix although it is IST, not UTC. Compare it with `dateadd('m', 330, now())`, never with `now()` (which is UTC): `WHERE ts > dateadd('m', 330 - 10, now())` is the last ten minutes.
- **`samples_over_1024ns` is NULL on `main_runtime_lag`, `reader_runtime_lag`, `ring_dwell` and `receipt_to_fold` (from 2026-11-06, PR DP-6).** Every sample of those stages is above 1,024 ns by construction (a 100 ms sleep, a queue wait, three steps end to end), so the count read 100% and looked like a failure. Rows written before the change still carry the old count.
- **Per-tick cost: the `drain_main_tick` stage (from 2026-11-06, PR DP-6).** `drain_main_frame` times whole frames, and a frame stacks many tick packets, so one frame sample is many ticks' work. Each main-feed frame now also records its decode/fold/append time divided by its tick packets, as that many samples (`record_stage_nanos_weighted`). So `samples` on the `drain_main_tick` row is the minute's tick count, and its percentiles are over ticks. A multi-packet frame gives each of its ticks the frame's per-tick AVERAGE, not each tick's own time, so a slow tick inside a fast frame is averaged down. 1,024 ns is a bucket edge (the first log2 bucket holds every sample up to 1,024 ns), so `p99_ns <= 1024` means the minute's p99 per-tick cost sat in the bucket ending at 1,024 ns: a green tile on the speed page is that bucket, NOT a measured ≤1,000 ns. `samples_over_1024ns` counts the ticks above that edge. The frame time divided out is the decode + fold + row append for EVERY packet in the frame, its non-tick packets (OI, market status, previous close) included, divided by its tick packets only, so a frame heavy in non-tick packets reads higher per tick. A frame with no tick packet (only OI, status or disconnect packets) records nothing here.

```sql
SELECT ts, samples AS ticks, p50_ns, p99_ns, p999_ns, max_ns, samples_over_1024ns
FROM hot_path_latency_minute
WHERE ts IN today() AND stage = 'drain_main_tick'
ORDER BY ts;
```

- **Work after the fold: the `drain_frame_post` stage (from 2026-11-13, B3).** `drain_main_frame`, `drain_main_tick` and `receipt_to_fold` all stop their clock when a frame's fold returns. The frame arm then still runs, before it can take the next frame: the socket delivery stamp, the tick size-trigger hand-off (every 1,000 rows; with no recycled spare it reserves and touches a fresh staged batch of about 4.5 MiB on the drain), the depth size-trigger or unbacked hand-off, and the gauges every 1,024 frames. Until 2026-11-13 no stage timed it directly; it showed only as later frames' `ring_dwell`, so a minute could read `drain_main_tick p99_ns <= 1024` while hand-offs held the drain. `drain_frame_post` records it once per frame (every endpoint); for a main-feed frame it starts where `drain_main_frame` ends, so the two add up to the frame's time on the drain from the frame timer's start (the ring-dwell bookkeeping just before that start is untimed: a few atomics and one clock read, Assumed well under a microsecond). A depth or order-update frame has no stage of its own, so its `drain_frame_post` sample starts at the frame timer's start and also holds that frame's own handling (depth decode and staging, up to 200 levels a packet); depth and main-feed samples share the one row and cannot be told apart. Do not add `drain_frame_post` to `ring_dwell` or `receipt_to_fold`: the time a post step holds the drain is also inside the `ring_dwell` (and `receipt_to_fold`) of every frame queued behind it. Most frames trip no hand-off, so read its `p999_ns`, `max_ns` and `samples_over_1024ns`, not its p50. Still in no stage, all on the same drain task (B3 review, 2026-11-13: the first version of this line named only the first): the 500 ms flush timer arm (the tick and depth hand-offs and the gauge publish); the 30 s silence arm (the silence scan over every tracked instrument, O(tracked), about 70 µs at 25,000, the depth-200 cross-feed check and the gap gauges); the idle arm (one catch-up seal step of 256 slots times every timeframe plus the seals it emits, or one step of 512 seed inserts); the 5 s catch-up timer arm and the seed receive arm (both O(1)); the shutdown arm; and, inside the frame arm itself, the frame's drop after the record (a buffer reference released, which frees the socket buffer when the WAL writer has already released its copy). Each shows only as later frames' `ring_dwell`, so a large `ring_dwell` with small `drain_main_frame` and `drain_frame_post` points at one of these, not only the timer arm.

```sql
SELECT ts, samples AS frames, p50_ns, p99_ns, p999_ns, max_ns, samples_over_1024ns
FROM hot_path_latency_minute
WHERE ts IN today() AND stage = 'drain_frame_post'
ORDER BY ts;
```

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

## NIC coalescing trial (one measured day, from 2026-11-13)

Owner card (2), Fri 13 Nov 2026: "Yes, measure 1 day (Recommended)" — "About 0–20 µs per packet faster, slightly more CPU. Applied outside market hours; easy to undo." While the trial is on, `tickvault host-tuning apply` sets the default-route NIC to `rx-usecs 0`, `adaptive-rx off` (receive interrupt coalescing off). The switch and the last day are two lines of `deploy/systemd/tickvault-host-tuning.service` (`TV_NIC_RX_COALESCE=off`, `TV_NIC_RX_COALESCE_UNTIL=2026-11-16`); the first run after that day puts the saved values back.

Check it applied, from the boot journal (`journalctl -u tickvault-host-tuning -b`), one line:

- `host-tuning: NIC ens34 rx coalescing rx-usecs=20 adaptive-rx=on -> rx-usecs=0 adaptive-rx=off (verified; trial until 2026-11-16 IST)`: on, and read back.
- `... skipped — not readable here ...` or a `WARNING` line: the trial did NOT run; the day is not a trial day.
- `... not touched — HH:MM IST is inside 09:00-15:45 IST ...`: the unit ran mid-session (a late boot, or an emergency deploy that overrode the deploy block), so the trial was not started; it starts at the next run outside that window. A day with this line and no earlier `verified` line is not a trial day.
- The values found before the trial stay in `/opt/tickvault/nic-coalesce.before` until the restore removes it.

What can and cannot show the gain:

- **No stage measures the gain itself.** Every stage clock starts after the kernel has handed the packet to our socket read, so the up to 20 µs the NIC used to hold a packet is before `socket_to_wal`, `ring_dwell` and `receipt_to_fold` begin. Those stages can only show side effects: more interrupts and softirq work on core 0 (QuestDB's core), and the reader thread's run delay. Do not claim a latency win from them. The gain is also small next to the vendor's own delay (about 0.75 s p50).
- Compare the trial day with the previous trading day, same 09:15–15:30 window, with the section 2 and 3 queries:
  - `hot_path_latency_minute`: `p99_ns`, `p999_ns` and `max_ns` for `socket_to_wal`, `ring_dwell` and `receipt_to_fold`. Unchanged or lower is the expected result; higher means the extra interrupts hurt.
  - `host_capacity_minute`: `host_cpu_busy_pct`, `process_cpu_pct`, `reader_cpu_wait_max_ns_per_s`, `drain_cpu_wait_max_ns_per_s`. "Slightly more CPU" is the expected cost; a clear rise in reader or drain CPU wait is a reason to undo.
  - Local `/metrics`: growth of `tv_hot_thread_run_delay_ns_total{thread="ws_reader_0"}` (and `ws_reader_1` to `ws_reader_3`) over the session, against the previous day.
- The app records no interrupt counts. If wanted, read the NIC's lines in `/proc/interrupts` by hand at the open and at the close of both days.

Undo early, outside market hours: put `TV_NIC_RX_COALESCE=keep` in `/etc/tickvault/host-tuning.env` (it wins over the unit and is read at each start, so no reload is needed), then `systemctl restart tickvault-host-tuning`. Only the env-file route survives a deploy: every deploy copies the repo unit over `/etc/systemd/system/tickvault-host-tuning.service`, reloads systemd and restarts the unit, which puts `TV_NIC_RX_COALESCE=off` back and applies the trial again; deleting the two `Environment=` lines does not undo it either, because the code's own defaults are also `off` and the same date. A quicker undo that lasts only until the next deploy: change `Environment=TV_NIC_RX_COALESCE=off` to `keep` in the unit, then `systemctl daemon-reload && systemctl restart tickvault-host-tuning` (without the daemon-reload systemd keeps the cached unit, only warns that the file changed, and the restart applies the trial again). An EC2 stop/start is NOT an undo while the trial is on: it resets the card to the driver's defaults, but `tickvault-host-tuning` runs at every boot and, with the switch still `off` and the date not past, sets `rx-usecs 0` / `adaptive-rx off` again before the app starts (journal line `... -> rx-usecs=0 adaptive-rx=off (verified; trial until ...)`). A stop/start leaves the card at its defaults only together with the `keep` switch, or after `TV_NIC_RX_COALESCE_UNTIL` has passed. Pinned by `test_stop_start_during_the_trial_reapplies_it_so_it_is_not_an_undo`.

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
