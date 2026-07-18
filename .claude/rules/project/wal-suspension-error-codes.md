---
paths:
  - "crates/storage/src/wal_suspension_watcher.rs"
  - "crates/common/src/error_code.rs"
  - "deploy/aws/terraform/error-code-alarms.tf"
---

# QuestDB Per-Table WAL-Suspension Probe — Error Codes (WAL-SUSPEND-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F > this file.
> **Operator authorization:** 2026-07-09 audit follow-up row 10,
> coordinator-approved WAVE-2 PR #6 (2026-07-10).
> **Companion code:** `crates/storage/src/wal_suspension_watcher.rs`
> (`parse_wal_tables_response` pure parser, `WalSuspensionTracker` edge
> latch, `spawn_supervised_wal_suspension_watcher` supervised 60s poll),
> `crates/common/src/error_code.rs::ErrorCode::WalSuspend01TableSuspended`.
> **Companion rules:** `http-client-error-codes.md` (the shared probe
> client this reuses — site `wal_suspension_probe`), `wave-2-error-codes.md`
> (BOOT-01/02 — the DOWN-server pages this deliberately does NOT duplicate),
> `observability-architecture.md` "Which codes page" (the paging list this
> entry lives on, drift-ratcheted by
> `crates/common/tests/error_code_paging_filter_drift_guard.rs`).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `WalSuspend01*` variant verbatim —
> `WAL-SUSPEND-01` and `WalSuspend01TableSuspended` appear below.

---

## §0. Why this code exists (the gap, 2026-07-09 audit row 10)

QuestDB tables can enter **WAL-suspended** state — typically after a
disk-full episode or a WAL apply error. In that state ILP ingestion keeps
ACKing rows into the table's WAL, but the table's WAL APPLY is suspended:
rows silently stop becoming visible/durable-applied, and the table's WAL
backlog grows on disk. Before this probe NOTHING in tickvault saw it — the
boot probe and `make doctor` check reachability (`SELECT 1`), and
`questdb_health.rs` tracks the ILP writer's CONNECTION state; none of them
see per-table apply health. A suspended `ticks` or `candles_1m` table was
silent data-visibility loss until someone manually noticed.

The probe: a supervised process-global task (the disk_health_watcher /
oom_monitor / resource_monitor house family) issues
`select * from wal_tables()` against the QuestDB HTTP `/exec` endpoint every
60s via the SHARED probe client, parses the response DEFENSIVELY BY COLUMN
NAME (never position — the column set `name`/`suspended`/`writerTxn`/
`bufferedTxnSize`/`sequencerTxn`/`errorTag`/`errorMessage`/`memoryPressure`
was verified against upstream QuestDB source, `WalTableListFunctionFactory`;
the live 9.3.5 image's exact shape is UNVERIFIED-LIVE and any drift fails
soft), and edge-latches per-table suspension episodes.

## §1. WAL-SUSPEND-01 — a table's WAL apply is SUSPENDED

**Severity:** High. **Auto-triage safe:** **No** — severity-independent
override in `is_auto_triage_safe()` (the FUTIDX-02 precedent): the recovery
action `ALTER TABLE <t> RESUME WAL` is an OPERATOR decision. Auto-resume can
replay the apply into a still-broken disk and re-suspend (or worse); the
auto-triage daemon must never execute it.

**Why High and not Critical:** the rows are durably IN the table's WAL —
apply resumes them once the operator runs RESUME WAL after fixing the cause.
This is unbounded STALENESS + disk growth, not permanent loss (contrast
AGGREGATOR-DROP-01, where the data is gone). Operator action is still
required — hence a paged High, not a log-only Medium.

**Trigger:** the 60s probe received a SUCCESSFUL 2xx `wal_tables()` response
in which a table's `suspended` column reads `true` and that table was not
already latched as suspended (rising edge). ONE
`error!(code = ErrorCode::WalSuspend01TableSuspended.code_str(), table, error_tag,
error_message, writer_txn, sequencer_txn, txn_lag)` fires per (table,
suspension episode) — audit-findings Rule 4, edge-triggered only. The
`error_tag`/`error_message` fields carry QuestDB's own reason for the
suspension (e.g. a disk-full tag), sanitized + truncated (≤300 chars,
credential/JWT-redacted via `capture_rest_error_body`).

**Paging route:** `error!` → errors.jsonl → CloudWatch Logs
`/tickvault/<env>/app` → filter `tv-<env>-errcode-wal-suspend-01`
(`deploy/aws/terraform/error-code-alarms.tf`) → alarm (≤5 min) → SNS →
Telegram. `ok_recovery = false`: the code fires once per episode, so the
auto-OK ~15 min after the datapoint ages out would be a Rule-11 false
recovery while the table may still be suspended — the real recovery signals
are the falling-edge recovery `info!` and the
`tv_questdb_wal_suspended_tables` gauge returning to 0.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `WAL-SUSPEND-01`; the payload
   names the `table`, QuestDB's `error_tag`/`error_message` (WHY it
   suspended), and the `writer_txn`/`sequencer_txn`/`txn_lag` (how far
   behind the apply is).
2. Fix the underlying cause FIRST: `df -h /data` (disk-full is the dominant
   cause — cross-check DISK-WATCHER-01 / RESOURCE-03 /
   `tv_spill_dir_free_bytes`), then the QuestDB server logs
   (`mcp__tickvault-logs__docker_status` + `docker logs tv-questdb`) for the
   apply error.
3. Confirm current state:
   `mcp__tickvault-logs__questdb_sql "select * from wal_tables()"` — the
   suspended table shows `suspended=true` with a growing
   `sequencerTxn − writerTxn` lag.
4. ONLY after the cause is fixed, run in the QuestDB console:
   `ALTER TABLE <table> RESUME WAL;` — the apply replays the backlog and the
   lag drains. **NEVER auto-executed** (operator decision; resuming into a
   still-broken disk replays the failure).
5. Watch recovery: the next probe poll (≤60s) logs the falling-edge recovery
   `info!` and `tv_questdb_wal_suspended_tables` returns to 0. If the table
   re-suspends (a NEW episode re-fires the page), the underlying cause is
   not fixed.

**Observability:**
- Gauge `tv_questdb_wal_suspended_tables` — count of currently-suspended
  tables; set on every successful probe (including 0).
- Counter `tv_wal_suspension_probe_failed_total{reason}` —
  `http` (server unreachable — BOOT-01/02 territory, debug-level here) /
  `status` (non-2xx) / `parse` / `missing_column` (schema drift; the
  parse-class failures also emit an edge-latched `warn!` once per
  contiguous failure run).
- Counter `tv_wal_suspension_watcher_respawn_total{reason}` — the
  supervisor respawned a dead watcher (5s backoff; a sustained rate = a
  flapping probe, inspect the panic backtrace in errors.jsonl).
- Counter `tv_http_client_build_failed_total{site="wal_suspension_probe"}`
  — the shared probe client could not be built (HTTP-CLIENT-01 degrade;
  the tick is skipped, never a panic).

**Honest envelope:** suspension is asserted ONLY from a successful response
— a DOWN QuestDB is never paged as "suspended" (no double-page; BOOT-01/02 +
`tv_questdb_connected` own the down-server page), and consequently a
suspension that begins DURING a QuestDB outage window is detected on the
first successful poll after recovery (≤60s later), not during the outage.
One page per (table, episode); a watcher respawn or process restart re-fires
one bounded page per still-suspended table (strictly better than a silent
gap — and it re-sets the gauge, covering gauge staleness across a task
death). The live 9.3.5 `wal_tables()` response shape is not live-verified
from the dev sandbox: the by-name parser tolerates column reordering and
missing OPTIONAL diagnostics; a rename of `name`/`suspended` degrades to the
loud `missing_column` probe-failed signal — monitoring degradation is always
visible, never a silent false-OK. A 2xx response with an EMPTY dataset while
tables are latched suspended is treated as a suspicious transient (server
mid-start), never as a mass recovery (2026-07-10 hostile-review fix).
**Fast-boot-arm bound (2026-07-10 review MEDIUM, documented + flagged):**
the main.rs spawn site sits in the process-global monitor block AFTER the
fast crash-recovery boot arm's early return, so a market-hours crash-restart
session runs WITHOUT this watcher — the identical pre-existing gap as its
siblings (disk / OOM / resource monitors). Disk-full → crash → fast-restart
is a suspension-producing sequence; moving the whole monitor block ahead of
the fast arm is a flagged sibling-wide follow-up (an operator-visible boot-
semantics change for four existing monitors, deliberately not smuggled into
this PR).

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/storage/src/wal_suspension_watcher.rs`
- `crates/common/src/error_code.rs` (any `WalSuspend01*` variant)
- `deploy/aws/terraform/error-code-alarms.tf` (the `wal-suspend-01` entry)
- Any file containing `WAL-SUSPEND-01`, `WalSuspend01`, `wal_tables`,
  `tv_questdb_wal_suspended_tables`, or
  `tv_wal_suspension_probe_failed_total`
