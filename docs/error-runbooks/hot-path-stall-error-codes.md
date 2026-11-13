---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/storage/src/hot_path_telemetry.rs"
---

# Hot-Path Stall Error Codes

> **Authority:** `docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md`
> §2.8 (owner-approved 2026-10-02). `ErrorCode::HotPathStall01.runbook_path()`
> points here.

## HOT-PATH-STALL-01 — a hot step on the Dhan live path stalled for 2 s or more

**Severity:** High. **Pages:** `tv-<env>-errcode-hot-path-stall-01`
(Telegram: "🔷 DHAN: the live market data path stalled…"). **No OK page.**

**What fired it.** The `tv-telemetry` thread checks, once a second and only
in session (09:00–15:40 IST), seven signals:

| Signal | `kind` | Stalled when |
|---|---|---|
| `main_runtime` | heartbeat | the main tokio runtime's 100 ms probe has not woken for 2 s |
| `reader_runtime` | heartbeat | the socket-reader runtime's probe has not woken for 2 s |
| `frame_drain` | heartbeat | the frame drain has not taken a frame or a 500 ms flush tick for 2 s |
| `socket_to_wal` | stage | one socket read's WAL + ring hand-off took 2 s |
| `ring_dwell` | stage | one frame waited 2 s in the ring before the drain took it |
| `main_runtime_lag` / `reader_runtime_lag` | stage | a probe woke 2 s late |
| `drain_main_frame` | stage | the drain took 2 s to decode, fold and append ONE main-feed frame (added 2026-10-26, plan item K6; its 1 ms stall budget is dashboard-only) |

Not signals (plan item DT0, 2026-10-29): the stages `receipt_to_fold` (a
main-feed frame's socket receipt → folded) and `metrics_upkeep` (one run of
the metrics exporter's upkeep) and the per-thread scheduling-wait series
`tv_hot_thread_run_delay_ns_total` / `tv_hot_thread_involuntary_switches_total`
are measurement only and never page. Read them beside a page to tell whether
the stall was a thread waiting for a CPU. Two more stages are measurement
only and never page, each with a 1 ms budget used only by the over-budget
count and the p99 warn below: `drain_main_tick` (PR DP-6, 2026-11-06: one
main-feed frame's decode, fold and append time divided by its tick packets)
and `drain_frame_post` (B3, 2026-11-13: the frame arm's synchronous work
AFTER the fold, for every endpoint: the socket stamping, the tick
size-trigger and depth hand-offs, a spare-less hand-off's reserve and page
touch, a full-queue rescue that writes a spill file inline, and the
1,024-frame publish; for a depth or order-update frame it also holds that
frame's own handling, depth decode and staging included, from the frame
timer's start). The time a post step holds the drain is also in the
`ring_dwell` of every frame queued behind it, so do not add the two. `drain_frame_post` is the only stage that shows a
stall after the fold, and only once that step has returned: a stage
records only when the step ends, so a hand-off that is still blocked (or
wedged for good) has written no sample yet. While it is blocked, the
per-second `tv_hot_path_stage_max_ns` reads 0 for every frame stage (no
step of that stage finished that second: the publisher resets the maximum
every second), and `tv_hot_path_stage_max_ns_75s` keeps the largest
finished step for 75 s and then also reads 0. A 0 there means "nothing
finished", never "the step was instant".

**Log-only line, same code.** When a stage's p99 over the last second passes
its budget, the first second of that episode writes one `warn!` with
`code = HOT-PATH-STALL-01`, `source = "stage_p99_over_budget"` and fields
`stage`, `p99_at_most_ns`, `budget_ns`, `samples`, `over_budget`. It is a
`warn!`, so the page's metric filter (`level = ERROR`) never matches it; it
can name any stage, the never-paged ones included.

A signal that becomes stalled starts an episode; the first line names the
worst newly stalled signal. The signal is silent until it clears, then
re-arms. At most one line a minute: episodes that started and ended while the
line was held back are counted in the next line's `missed_episodes`.

**Fields:** `signal`, `kind`, `stalled_secs`, `threshold_secs` (2), `signals`
(how many newly stalled signals the line covers), `missed_episodes`.

**What it means.** While the step was stalled the socket reads may have
waited. Dhan skips a slow reader forward with no sequence number, so ticks in
that window may be missing. Nothing in this line proves a loss; it proves the
wait.

**Triage:**
1. `tv_task_heartbeat_age_seconds{task}`, `tv_hot_path_stage_max_ns{stage}`
   and `tv_task_busy_seconds{task}` on the box's metrics endpoint show which
   step and for how long.
2. `frame_drain` stalled with `ring_dwell` rising: the drain is busy or
   blocked. Look for `AGGREGATOR-STALL-01` (a disk wait), a QuestDB stall,
   or CPU saturation (`top -H`, thread names `tv-worker`, `tv-ws-reader`).
   The drain's other arms are timed by no stage and show only as
   `ring_dwell`: the 500 ms flush timer arm, the 30 s silence scan, the
   idle arm's catch-up seal and seed steps, the catch-up timer and seed
   arms, the shutdown arm, and each frame's drop after its record (the
   full list is in `docs/runbooks/hot-path-capacity-proof.md`).
3. `main_runtime` / `reader_runtime`: a runtime could not run any task.
   CPU starvation or a synchronous step on a worker; check host CPU and
   steal time.
4. `frame_drain` stalled while `tv_hot_path_stage_max_ns{stage="drain_main_frame"}`
   is small: the fold is not the cause. A stage records only when the step
   ends, so while a tick or depth hand-off is still blocked BOTH frame
   stages read 0 (or small, from frames that finished earlier in the
   second) and only the stale `frame_drain` heartbeat shows it.
   Do not read a small `drain_frame_post` as "not the hand-off". Check the
   writer side directly: `tv_tick_rescue_parked_bytes` and
   `tv_depth_rescue_parked_bytes` (the park filling), the rescue and spill
   counters `tv_tick_rescue_inline_fallback_total` and
   `tv_depth_rescue_inline_fallback_total` (a spill file written inline on
   the drain), and disk health (`tv_disk_hours_left_at_fill_rate`,
   `df -h`, `iostat`). Once the hand-off returns, its time shows in
   `tv_hot_path_stage_max_ns{stage="drain_frame_post"}` and, for 75 s, in
   `tv_hot_path_stage_max_ns_75s{stage="drain_frame_post"}`; a volume that
   stays wedged never produces that sample. With depth sockets on, a large
   `drain_frame_post` can also be a depth frame's own decode and staging
   (depth and main-feed samples share the stage).
5. A `frame_drain` heartbeat that never recovers after the lane stopped is
   the lane-down case; `tv-<env>-dhan-live-lane-down` pages it too.

**Not covered:** a stall shorter than 2 s; a stall outside 09:00–15:40 IST;
the whole process frozen (this thread stops too — the liveness alarms that
treat missing data as breaching cover that).

**Source:** `crates/storage/src/hot_path_telemetry.rs` (`StallAlarm`,
`report_stall`).
