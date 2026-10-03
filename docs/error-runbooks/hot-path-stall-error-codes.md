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
3. `main_runtime` / `reader_runtime`: a runtime could not run any task.
   CPU starvation or a synchronous step on a worker; check host CPU and
   steal time.
4. A `frame_drain` heartbeat that never recovers after the lane stopped is
   the lane-down case; `tv-<env>-dhan-live-lane-down` pages it too.

**Not covered:** a stall shorter than 2 s; a stall outside 09:00–15:40 IST;
the whole process frozen (this thread stops too — the liveness alarms that
treat missing data as breaching cover that).

**Source:** `crates/storage/src/hot_path_telemetry.rs` (`StallAlarm`,
`report_stall`).
