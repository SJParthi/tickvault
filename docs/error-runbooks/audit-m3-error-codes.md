# Audit M3 — the last uncoded error lines (WS-SPILL-03, WS-SPILL-04, PROC-03, API-SERVER-01, TICK-GAP-01, PIPELINE-LAG-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this file.
> **Added:** 2026-10-06 (workspace audit M3, third pass; `docs/audit-2026-10-04.md` row M3).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires every
> code string below to appear here verbatim; `runbook_path()` for all six variants
> points at this file. `crates/common/tests/error_code_tag_guard.rs` holds the
> remaining uncoded `error!` count at 3 (all in the frozen indicator/strategy area).

---

## Why these six codes exist

Every CloudWatch metric filter that pages reads `$.code`. An `error!` with no code
reaches the log file and nothing else: it cannot be counted, alarmed or found by code
in triage. After the 2026-10-05 pass, ten such lines were left. Three sit in the
frozen indicator/strategy area and stay as they are. The other seven get the six
codes below.

**None of them pages.** All six are `Severity::Medium` and no metric filter matches
any of them (the filters in `deploy/aws/terraform/error-code-alarms.tf` match exact
code strings, and none names these). That is deliberate: the Dhan noise lock forbids
a new page without a dated operator quote, and a High or Critical code would make
`error_code_alarm_coverage_guard` demand an alarm or an entry on a shrink-only
exemption list. Where the underlying condition already pages, it pages through an
EXISTING alarm, named in each section.

| Code | Where | Pages? |
|---|---|---|
| WS-SPILL-03 | `crates/app/src/main.rs`, STAGE-C WAL init | No new page. The halted boot is paged by the existing boot-heartbeat and market-hours liveness alarms. |
| WS-SPILL-04 | `crates/storage/src/ws_frame_spill.rs`, boot replay | No new page. The event already pages through the existing `durable-floor-breach` alarm (counter `tv_wal_replay_corrupted_segments_total`). |
| PROC-03 | `crates/app/src/main.rs`, panic hook | No new page. The process death is paged by the existing liveness alarms. *(⚠ Corrected 2026-10-11: only a restart loop or an outage of 5+ minutes between 09:20 and 15:35 IST pages; a single panic that systemd restarts is not paged. See the PROC-03 section.)* |
| API-SERVER-01 | `crates/app/src/main.rs`, API server task | No. |
| TICK-GAP-01 | `crates/trading/src/risk/tick_gap_tracker.rs` (three lines since 2026-10-20) | No. Dormant today. |
| PIPELINE-LAG-01 | `crates/app/src/trading_pipeline.rs` | No. Dormant today. |

---

## WS-SPILL-03 — the frame WAL could not be opened at boot

**What happened:** STAGE-C could not create or open the write-ahead log directory. The
WAL is the durable floor for every received frame, so the boot refuses to continue
and exits with status 1. systemd restarts the service; the same fault repeats until
it is fixed.

**What pages:** not this line. A halted boot never publishes `tv_boot_completed`, so
the boot-heartbeat alarm (`deploy/aws/terraform/boot-heartbeat-alarm.tf`) and, in
session, the market-hours liveness alarm
(`deploy/aws/terraform/market-hours-liveness-alarm.tf`) fire. This line tells you WHY.

**Do:**
1. Read the `err` and `dir` fields on the line.
2. `df -h /data` and `ls -la` on the WAL directory: a full or read-only volume, or a
   permission change, is the usual cause.
3. Fix the volume, then let systemd restart the service (or restart it by hand).
4. Never work around it by disabling the WAL: running without it allows silent frame
   loss under backpressure.

## WS-SPILL-04 — a WAL segment could not be read at boot replay

**What happened:** boot replay could not read one WAL segment at all (for example it
was cut short by a crash or the file is damaged). Replay skips it, counts it on
`tv_wal_replay_corrupted_segments_total`, and marks the next segment as following a
gap, so the candles across that gap are rebuilt as partial rather than as complete.
Replay does not delete the segment; it is staged with the other segments it read.

**What pages:** the existing `durable-floor-breach` alarm
(`deploy/aws/terraform/loss-and-retention-alarms.tf`) sums that counter. This code was
chosen instead of WS-SPILL-02 because WS-SPILL-02 pages on any line and means "the
writer dropped a frame"; using it here would page the same event twice and blur
what it means.

**Do:**
1. Note the `segment` and `error` fields and the time window the segment covers.
2. Those frames were not re-folded in this process. If the segment's raw copy was
   uploaded to the cold bucket, it can be inspected there.
3. Check for a crash or power loss just before the boot (journal, `PROC-03` lines).

## PROC-03 — the process panicked

**What happened:** a thread panicked. The panic hook first writes one line
synchronously to `data/logs/machine/errors.log` (it carries `"code": "PROC-03"` too),
then, under the release profile's `panic = "abort"`, gives the WAL writer a bounded
moment to empty its queue (`wal_drain` field), logs this line and lets the process
abort. systemd restarts it.

**What pages:** not this line. The process death is paged by the existing liveness
alarms. (PROC-02 is reserved for a container restart loop and is not used here.)

**⚠ CORRECTED 2026-10-11 (audit OBS-1, CR-3)** — the synchronous line above went
only to `errors.log`, which the CloudWatch agent does not ship, and the coded
tracing line rides the non-blocking errors.jsonl worker, which dies with the
process under `panic = "abort"`. So in production no CloudWatch row carried the
panic. The hook now also writes the same crash, synchronously and before the WAL
drain, to the current hourly `data/logs/machine/errors.jsonl.<UTC hour>` file, so
it reaches CloudWatch Logs (`/tickvault/<env>/app`, stream `errors-jsonl`). That
line is flattened like the rest of the file: `code`, `message`, `panic_location`
and `panic_payload` at the top level, with a UTC `timestamp`, and carries no
`wal_drain` field (the drain runs after it). Still **no page**: no metric filter or
alarm reads it. The hook is also installed earlier, right after the one-shot CLI
commands and before the config load, so a panic anywhere in boot leaves these
lines; a panic in `main()` before `async_main` starts (reader-runtime setup) is
still covered only by the default stderr message. In test and debug builds
(unwinding) the tracing line can also arrive, giving two rows for one panic.

**⚠ CORRECTED AGAIN 2026-10-11 (review of OBS-1)** — four statements above were
wrong or too strong:

- **"The process death is paged by the existing liveness alarms" is false for an
  ordinary single panic.** systemd restarts the unit within seconds
  (`Restart=always`, `RestartSec=3`), and `tv-<env>-market-hours-liveness-missing`
  needs five missing 60 s periods, only between 09:20 and 15:35 IST. So a panic
  that restarts and boots in under about five minutes, or any panic outside that
  window, pages nobody. Only a restart loop (past `StartLimitBurst` = 5 in an hour
  the unit stays failed) or an outage of five minutes or more inside the window
  pages. The table row above and the "What pages" line carry the same error. A
  PROC-03 page is an **open owner decision** under the Dhan noise lock; no alarm
  is added here.
- **"No metric filter or alarm reads it" is false.** The existing generic filter
  `tv-<env>-log-schema-flat-errors` (`{ $.level = "ERROR" && $.code = * }`)
  counts the flat PROC-03 line into `tv_log_errors_flat_total`. That can only hold
  `errcode-stream-silent` off, never trip it, so no alarm pages on it.
- **"A panic anywhere in boot" was too strong.** Before the hook are also the TLS
  provider install at Step 0 and the `holiday-gate` and `ensure-questdb`
  one-shots; a panic there leaves only the default stderr message, like the
  reader-runtime setup in `main()`.
- **Step 2 below pointed at a field no surviving record carried** in the release
  build: the line with `wal_drain` was the non-blocking tracing line, lost with
  the abort. The hook now writes a second synchronous `errors.jsonl` line after
  the drain returns and before the abort, `"message": "PANIC: WAL drain before
  abort"`, with `wal_drain` at the top level. It is written only in abort builds
  (unwinding builds do not drain).

The `timestamp` on both panic lines is UTC with a `Z`, the same as every other
line in `errors.jsonl` (that layer keeps tracing-subscriber's default UTC timer;
only `app.log`, `errors.log` and the category files use IST), which matches the
agent's `"timezone": "UTC"`.

**Do:**
1. Read `panic_location` and `panic_payload`: they name the source line and message.
   In CloudWatch, search the `errors-jsonl` stream for `PROC-03`.
2. Read `wal_drain`: it says whether the WAL queue was emptied before the abort.
   *(2026-10-11: in CloudWatch it is on the second PROC-03 line, message
   `PANIC: WAL drain before abort`; see the correction above.)*
3. Treat every panic as a defect: open a fix for the named location. A panic that
   repeats on every boot is a restart loop.

## API-SERVER-01 — the HTTP API server stopped

**What happened:** `axum::serve` returned an error, so the HTTP API stopped serving.
The trading lanes, capture and persistence keep running; `/health`, `/api/*` and the
log-reading tools that call the API are unreachable until a restart.

**What pages:** nothing. The live-lane alarms are unaffected because they do not read
the API.

**Do:**
1. Read the `err` field (usually an accept-loop I/O error).
2. Restart the service after market hours, or during the session only if the API is
   needed: a restart is a reconnect for every socket.

## TICK-GAP-01 — the per-instrument tick-gap tracker saw a gap

**What happened:** one of three lines, told apart by `source`:

- `source = "instrument_gap"`: an instrument went silent past the error threshold.
  The line fires once per instrument per gap (edge-latched) and clears when ticks
  resume.
- `source = "reconnect_backfill_window"`: a WebSocket reconnect opened a backfill
  window for the instruments that were recently active.
- `source = "stale_ltp"` (coded 2026-10-20): the 30-second scan found an
  instrument with no tick for 10 minutes or more (`frozen_secs`). Once per
  instrument per stale episode; the next tick resets it. Before this date the line
  had no code: the tag guard read the field `exchange_segment_code =` as a `code`
  field.

**Why not RISK-GAP-03:** that code's filter pages on every line, and this tracker can
fire once per silent instrument, which for illiquid options is normal. The live
lane's own silence check, which does page under RISK-GAP-03, is a different, rate
limited path.

**Dormant today:** the tracker has no production tick producer, so these lines do not
fire in the running system.

**Do:** if it ever fires, compare `security_id`, `gap_secs` and the timestamps with
the live lane's silence gauges before acting; a single illiquid instrument is not a
feed fault.

## PIPELINE-LAG-01 — the trading pipeline skipped ticks

**What happened:** the trading pipeline's receiver on the tick broadcast fell so far
behind that the broadcast skipped at least the error threshold of ticks in one
receive (`skipped` field). Signals for those ticks were not evaluated. The ticks
themselves are not lost: the WAL and the candle fold are separate consumers.

**Dormant today:** the trading pipeline has no production spawn site.

**Do:** if it fires, look for a stall on the pipeline task (CPU, a blocking call)
around the time of the line.

---

## Trigger

Read when triaging any of the six codes above, or when editing the lines named in the
table.
