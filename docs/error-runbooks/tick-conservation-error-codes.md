# Daily Tick Conservation Check — Error Code (TICK-CONSERVE-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this file.
> **Plan item:** K4 in `.claude/plans/active-plan-audit-2026-10-20.md` (PR K, APPROVED).
> **Companion code:** `crates/app/src/tick_conservation.rs` (the check and the
> boot crash-gap row), `crates/storage/src/tick_conservation_ledger.rs` (the
> process-side counts), `crates/storage/src/tick_conservation_audit_persistence.rs`
> (the `tick_conservation_audit` writer), `crates/storage/src/wal_last_receipt.rs`
> (the previous process's last WAL receipt), and
> `crates/common/src/error_code.rs::TickConserve01DailyMismatch`.
> **History:** the 2026-06-10 audit and its code were retired on 2026-07-18
> because nothing wrote `ticks` then; the retired runbook is
> `docs/rules-archive/tick-conservation-audit-error-codes.md`. The Dhan live lane
> writes `ticks` again, so the check is back in a smaller shape.

## What the check does

Once per trading day at **15:45 IST** (five minutes after the `ticks` persist
window closes at 15:40), on its own task, it compares:

| Input | Source |
|---|---|
| accepted | `ticks` rows the Dhan writer appended for today (one relaxed add per row, in process) |
| database | `select count() from ticks where ts in '<today>' and feed = 'dhan'` (30 s timeout) |
| not landed | rows counted as spilled, failed or handed to the WAL replay (the sites that increment `tv_ticks_dropped_total`) |

and writes one row to `tick_conservation_audit` (DEDUP `ts, trading_date_ist, feed`).

| verdict | reason | meaning |
|---|---|---|
| `match` | `exact` | every accepted row is in the table |
| `match` | `within_counted_losses` | the shortfall is no larger than the rows already counted as not landed |
| `mismatch` | `unexplained_shortfall` | more rows are missing than every counted loss explains |
| `mismatch` | `extra_db_rows` | the table holds more of today's Dhan rows than this process accepted |
| `unverified` | `count_unreadable` | the count query failed, timed out or returned a bad body; never a mismatch |
| `unverified` | `no_rows` | nothing accepted and nothing stored; a zero baseline proves nothing |
| `partial` | `process_started_mid_day` | this process started at or after 09:00 IST (or on another day); it knows only its own rows, so no mismatch is claimed |

A `mismatch` or `unverified` read is retried up to 3 times, 120 s apart,
because a row QuestDB has acknowledged is countable only once its WAL is
applied.

## TICK-CONSERVE-01 — the day did not reconcile

**Severity:** High. **Auto-triage:** No (a data verdict the operator judges).
**Delivery:** log only. No CloudWatch filter, no EMF name, no alarm, no
Telegram (`dhan-rest-only-noise-lock-2026-07-14.md`).

**Triage:**
1. `select * from tick_conservation_audit order by ts desc limit 5` — the
   numbers behind the verdict (`accepted_rows`, `db_tick_rows`,
   `not_landed_rows`, `shortfall`, `count_reads`).
2. `unexplained_shortfall`: check QuestDB's apply backlog
   (`tv_questdb_wal_apply_lag_max`, `wal_tables()`) — rows still being applied
   read as missing. Re-run the count by hand after the backlog drains. If it is
   still short, look for loss counters that do not feed the ledger
   (`tv_ticks_lost_total`, `tv_wal_shed_frames_lost_total`) and the
   `HOT-PATH-02` lines for the day.
3. `extra_db_rows`: another writer stamped `feed='dhan'` on today's rows (a
   second process, or a WAL replay of a frame this process also wrote). Check
   for `RESILIENCE-01` and the boot's replay log line.

## The boot crash-gap row (`feed_gap_audit`, `reason = process_down`)

At boot inside the session (09:15 to 15:40 IST) on a trading day, the downtime
since the previous process's last WAL receipt is written as one
`feed_gap_audit` row: `ws_type = main_feed`, `connection_index = -1` (the
whole process, never a socket slot), `dhan_code = -1`. A previous frame from
before today's 09:15 open starts the gap at the open. Nothing is written
outside the window, with no previous frame on disk (every segment pruned, or
only pre-v3 records), or for a gap of 5 s or less. A failed write is a coded
`STORAGE-GAP-03` line.

## Honest limits

- The not-landed total is per process and can include `feed_aux_packets`
  rows that shared a rescued batch: a tolerance, not an exact term. A real
  loss smaller than the counted spills reads as `match`.
- Frames lost before the drain folded them (`tv_ticks_lost_total`) were never
  accepted, so this check cannot see them; the WAL and its own counters do.
- The previous process's last receipt is the receipt of the record with the
  highest sequence, which can differ from the latest receipt by the
  cross-socket reorder slack (under a second).
- `process_down` measures the gap between processes only; a socket gap inside
  a running process is its own `feed_gap_audit` row from the lifecycle stream.
