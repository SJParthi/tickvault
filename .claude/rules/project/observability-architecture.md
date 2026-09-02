---
paths:
  - "crates/app/src/observability.rs"
  - "crates/app/src/main.rs"
  - "crates/common/src/error_code.rs"
  - "crates/common/tests/error_code_*.rs"
  - "crates/storage/tests/error_level_meta_guard.rs"
  - "crates/storage/src/instrument_persistence.rs"
  - ".claude/triage/**"
  - "data/logs/**"
---

# Zero-Touch Observability Architecture — SINGLE SOURCE OF TRUTH

> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** Every `error!`, `warn!`, `info!`, every file/hook/metric/alert
> in the tickvault observability chain. Future Claude sessions MUST read
> this file before touching logging, alerting, monitoring, or error-handling.
> **Directive:** Parthiban, 2026-04-18 — *"in the future we should never
> ever recreate the same scenario or go through the same condition ...
> nowhere repeating the same process again and again from scratch ...
> I need 100 percent guarantee"*.

## The one-line architecture

```
error! → tracing → [4 local sinks + CloudWatch Logs] → (alarmed codes) metric-filter alarm → SNS → Telegram
                                ↓
                         Claude auto-triage
                                ↓
                     known→auto-fix | novel→escalate
```

## The 5 sinks every `error!` hits (in order)

| # | Sink | Where | Purpose | Retention |
|---|------|-------|---------|-----------|
| 1 | stdout / journald | host | `docker logs`, `journalctl -u tickvault` | systemd default |
| 2 | `data/logs/app.YYYY-MM-DD.log` | disk | full app log, daily rotation | `LOG_MAX_FILES` |
| 3 | `data/logs/errors.log` | disk | WARN+ only, single file, grep-friendly | single file |
| 4 | `data/logs/errors.jsonl.YYYY-MM-DD-HH` | disk | **ERROR-only, JSONL, hourly rotation** | 48h (auto-swept) |
| 5 | CloudWatch Logs → metric-filter alarms → SNS → Telegram | AWS | operator paging for filtered High/Critical codes | 14d logs; one page per ALARM episode |

Sink 4 is the one future Claude Code sessions and any log-ingestion MCP tail.
`cat data/logs/errors.jsonl.$(date -u +%Y-%m-%d-%H) | jq` = one pipe, every
structured ERROR event in the last hour.

### Which codes page (2026-07-06)

**The canonical routing:** `error!` → errors.jsonl (`data/logs/machine/`) →
CloudWatch Logs `/tickvault/prod/app` (CW agent) → log metric filter →
`tv_errcode_*` metric → CloudWatch alarm (≤5 min) → SNS `tv-prod-alerts` →
Telegram webhook Lambda. An `error!` ALONE does not reach Telegram; only codes
with a filter+alarm (or paths that also call `NotificationService::notify`)
page. The Loki→Alertmanager→Telegram path was retired in the CloudWatch-only
migration (#O1/#O2/#O3) — the 2026-07-06 zero-page incident is why this list
now exists (`deploy/aws/terraform/error-code-alarms.tf`).

Filtered+alarmed codes (each = one `error_code_alerts` map entry):
**WS-GAP-03/universe-collapse (added 2026-08-22** — the universe-collapse arm ONLY, matched by a three-condition pattern including `$.source = "fell_back_to_indices"`; the bare code has ~50 connection-state emit sites and filtering on it alone would page on ordinary reconnect churn. Closes the gap where a fall back from ~24,600 instruments to 4 reported nothing: the 4 indices keep ticking, so the no-ticks alarm stays green. See `dhan-rest-only-noise-lock-2026-07-14.md` §2.3d**), DH-901, DH-906 (term-match tripwire — no coded emit site
exists yet), AUTH-GAP-04 (the Groww stall-storm entry left this list
2026-07-15 — its only ERROR-level emit site, the deleted sidecar stall
watchdog, died with the Groww live feed; see "Retired paging entries" below),
(the WAL re-injection-abort entry left this list 2026-07-17 — its only
emit site, the un-consumed `wal_reinject.rs` helper, was deleted in the
dead live-WS sweep stage 1; see "Retired paging entries" below), PROC-01, **RESOURCE-02 (added 2026-09-02** — the process's own resident-memory early warning, the one signal meant to fire BEFORE the OOM killer; it was log-sink-only and, with no memory directive on the systemd unit, its 80% line was measured against MemTotal (~31 GiB) and was unreachable. Paired with the same-day `MemoryHigh=15G` throttle + `OOMScoreAdjust=-900` on the unit; plain coded filter, `ok_recovery = true` because the monitor re-emits while RSS stays over the line. Dated row: `dhan-rest-only-noise-lock-2026-07-14.md` §2.3p**),
**AGGREGATOR-DROP-01 (added 2026-07-09** — the
audit found the Severity::Critical sealed-candle-drop code, the ONLY
silent-data-loss path for sealed candles, paged nobody; it also gains a
redundant counter-side pager on `tv_seal_writer_drain_total{kind="dropped"}`
— `tv-<env>-seal-writer-dropped`, `seal-drop-alarm.tf`, with the dropped
series pre-registered at 0 post-recorder-install in main.rs per the
feed-stall round-5 first-sample-baseline lesson; lockstep ratchet
`crates/app/tests/seal_drop_paging_wiring_guard.rs`**)**, **WAL-SUSPEND-01
(added 2026-07-10, W2 PR#6** — audit follow-up row 10: a WAL-suspended
QuestDB table (post disk-full / apply error) keeps ACKing ILP rows while
they silently stop becoming visible/applied, previously with ZERO signal;
the new 60s `wal_tables()` probe (`crates/storage/src/wal_suspension_watcher.rs`)
fires one edge-latched ERROR per (table, suspension episode) — a merely-DOWN
QuestDB never fires it, the boot-probe escalation codes own the down-server
page; recovery = the operator's
`ALTER TABLE <t> RESUME WAL`, never auto-executed; runbook
`.claude/rules/project/wal-suspension-error-codes.md`**)** (the
TICK-CONSERVE-01 entry added here 2026-07-14 by automation-gaps PR-3 left
this list 2026-07-18 — its emit site was deleted with the retired
tick-conservation audit; see "Retired paging entries" below), **AUTH-GAP-05
(added 2026-07-14, REST-audit gap 01** — SCOPED to the mint-FAILURE arm only
via `$.cooldown_skip IS FALSE` (the boolean field exists only on that
emission; IS FALSE additionally excludes the same-day noise-lock H3
non-terminal mint-cooldown-skip lines, which self-retry at the next re-arm
window and must never page); the
trigger arm fires on every forced re-mint INCLUDING successful ~30-min
self-heals and is operator-ruled noise — silent-when-healing,
loud-only-when-unobtainable**)**, **SPOT1M-01 and CHAIN-02 (added
2026-07-14, REST-audit gap 03** — SCOPED to the once-per-episode
`stage="escalation"` edge lines only, covering the Dhan spot + Groww spot +
Groww contract legs (SPOT1M-01) and both feeds' chain legs (CHAIN-02); the
per-minute sub-edge lines are deliberately unmatched — a plain code filter
would over-page every failed minute vs the designed 3-minute escalation;
the persist-failure codes feed these edges persist-gated, so a persist
outage still reaches the page**)**, **CHAIN-01 (added 2026-07-14** — plain
coded filter; both stages are once-per-episode page-worthy and the
probe-only path never emits it at ERROR**)**, and **CHAIN-04 (added
2026-07-14** — SCOPED to the down-for-the-day `stage="warmup"` arm only;
the probe_* / warmup_no_token stages are log-only-by-design
transient/respawn arms**)**, and **BOOT-02, BOOT-03, OMS-GAP-06,
WS-SPILL-02 (added 2026-08-11, Critical-severity paging-gap sweep** — see
the dated subsection below this paragraph for the full classification**)**,
and **WS-SPILL-01 (added 2026-08-12** — the sibling that sweep left behind.
The sweep was scoped to `Severity::Critical`; WS-SPILL-01 is `High`, so the
WAL writer's own disk failure — the MORE common of the two — stayed unpaged
while WS-SPILL-02 got an alarm. It matters because `WsFrameSpill::append`
returns `Spilled` the instant a record enters the crossbeam channel and the
disk write happens LATER on the writer thread: on a full or unwritable disk
`persist_record_resilient` counts the failure and returns 0 while every
caller is still told the frame was durably captured, so the
capture-at-receipt floor is gone and nothing upstream can tell. Same 3×300s
shape and `ok_recovery = false` as its sibling — frames that arrived during
the outage were never written, so a recovering disk does not restore
them**)**, and **STORAGE-GAP-05 (added 2026-08-19** — the pressure-archival give-up page, feed-hardening Item 5. The Critical-severity coverage guard refused the new code without it, correctly: a filling volume that the automation has stopped acting on is precisely a silent failure. It pages beyond storage because a full volume blocks every QuestDB write, which backs up the ILP flush, the frame drain and finally the socket — and Dhan skips a slow consumer forward to "the latest available state", so a disk problem becomes upstream TICK LOSS. `ok_recovery = true`, unlike the ws-spill pair: nothing was lost by this code, so a genuine recovery is a real state worth telling**), and **RISK-GAP-03 (added 2026-08-15** — the CONNECTED-BUT-SILENT
page, authority `dhan-rest-only-noise-lock-2026-07-14.md` §2.3a. The 30s
silence scan was wired on 2026-08-12 and its verdict left log-sink-only,
which made the one detector for an otherwise-invisible failure itself
invisible: a subscribe that silently does not take produces no payload to
count, no parse to fail, and no error of its own, so the socket stays open,
the lane gauge reads 1, and every loss counter sits at a healthy flat zero.
A LOG FILTER rather than a gauge threshold, deliberately — the app already
gates the emit to the CONTINUOUS session (never the legitimately-silent
pre-open) and edge-latches it to one per episode, so the coded line carries
the market-hours gating and de-duplication a raw gauge alarm would need a
window Lambda and an unknown baseline to reproduce. `eval = 1`, unlike the
ws-spill pair, because the app has ALREADY required two consecutive 30s
scans before writing the line — three more CloudWatch windows would delay
the page ten minutes for a condition the detector has confirmed.
`ok_recovery = true`, also unlike that pair: silence is not the permanent
loss of a specific frame — instruments genuinely start ticking again, and
"the feed is being heard again" is a real recovery worth telling**)**.
and **WS-GAP-02 / swap-emptied-socket (added 2026-08-28** — authority
`dhan-rest-only-noise-lock-2026-07-14.md` §2.3m. An at-the-money depth swap
that UNSUBSCRIBED its old contract and then failed to subscribe the new one
leaves that socket carrying NOTHING while staying transport-healthy: it keeps
ponging, the connection gauge counts it alive, and the lane's no-ticks alarm
reads the WHOLE lane, where fifteen other sockets are still flowing and the
last tick is always ~1 s old. So the one failure that empties a socket was
invisible to every alarm in family (5). SCOPED by `$.source =
"swap_emptied_socket"`, never the bare code: WS-GAP-02 is also emitted by the
per-minute ATM top-up path, which stops at its wire budget without emptying
anything, so a bare-code filter would page on routine top-up exhaustion every
minute — the RISK-GAP-03 noise trap on a hotter path. `ok_recovery = true`,
unlike the xverify entries: the same arm schedules a redial and the retained
set already names the NEW contract, so a return to OK is the remediation
working and is worth telling**)**.
and **SCOREBOARD-01 (added 2026-09-02** — authority
`dhan-rest-only-noise-lock-2026-07-14.md` §2.3q, operator reaffirmation that
day. A daily rollup that fails to write loses the WHOLE per-connection day
summary — the table you read to answer "which connection misbehaved
yesterday". It was log-sink-only by ACCIDENT rather than by decision: a sweep
of every storage write path found `ws_connection_rollup.rs` is the one file
whose flush-failure arm pairs `error!` with no counter at all, and
`Scoreboard01AggregationDegraded` is `Severity::Medium`, so the coverage guard
never required it to be alarmed and it is not on the exempt list either. It
fell between two rules. SAFE TO PAGE because the rollup runs ONCE per day
after close: this can fire at most once a day and structurally cannot flap,
unlike RISK-GAP-03's 25 pages in one session. `period = 86400` and `eval = 1`
because a once-daily emitter has no second datapoint to wait for.
`ok_recovery = false`, unlike RISK-GAP-03: the rollup does not retry within
the day, so an OK an hour later is the datapoint ageing out, never the summary
arriving. Nothing is LOST when it fires — the raw `ws_event_audit` and
`feed_episode_audit` rows it folds from are unaffected and the day is
re-rollable by hand; what is lost until then is the view**)**.
**Everything else
is log-sink-only** unless it has its own metric alarm (app-alarms.tf) or a
typed `NotificationEvent`. Counter-side (non-errcode) pager added
2026-07-14 (REST-audit gap 05): `tv-<env>-telegram-drops`
(`telegram-drop-alarm.tf` — Sum ≥ 3 drops of `tv_telegram_dropped_total`
per aligned 900s window via the metrics-log delta-extraction house
pattern; a broken bot silently killed every typed-event page; honest
residual: the counter is NOT yet pre-registered at 0 post-recorder-install,
so the session's first drop per reason-series is eaten as the CW delta
baseline — flagged crates follow-up).

**Critical-severity paging-gap sweep (2026-08-11).** A mechanical sweep of
all 167 `ErrorCode` variants found **29** at `Severity::Critical`, of which
only **4** carried an `error_code_alerts` entry (`DH-901`, `AUTH-GAP-04`,
`PROC-01`, `AGGREGATOR-DROP-01`). A Critical that reaches no human surface
is a silent failure by definition — the 2026-07-06 zero-page class,
inverted. Classification of the other 25:

- **14 have NO `error!`-level emit site at all** — `AUTH-GAP-02`,
  `DH-902`, `DH-903`, `DATA-808`, `DATA-809`, `DATA-810`, `SELFTEST-02`,
  `PREVCLOSE-03`, `BAR-MISMATCH-01/-02/-03`, `GROWW-SCALE-03`,
  `GROWW-SCALE-05`, `GROWW-ORD-03`. These are deliberately **NOT** alarmed:
  a filter with no possible emit site is a dead filter that reads as a
  permanently-green alarm forever (the `ws-reinject-01` /
  `tick-conserve-01` precedent). They are **enum-retirement candidates** —
  each carries a `runbook_path()` and advertises coverage that does not
  exist. Pinned in count by the ratchet so the set can only shrink.
- **4 already reach a human** via a typed `NotificationEvent` dispatched at
  the emit site (`ORPHAN-POSITION-01` → `OrphanPositionDetected`,
  `RESILIENCE-01` → `DualInstanceDetected`, `RESILIENCE-03` →
  `AuthenticationFailed`, `OMS-GAP-03` → `CircuitBreakerOpened` per
  `dhan-rest-only-noise-lock-2026-07-14.md` §2a). Not duplicated as
  CloudWatch alarms, on cost discipline (~$0.10/mo each).
- **3 are BLOCKED pending a dated operator quote.** `AUTH-GAP-01` and
  `DATA-805` are Dhan-scoped, and an alarm → SNS → Telegram IS a new
  Dhan-scoped page, which `dhan-rest-only-noise-lock-2026-07-14.md` §3
  REJECTs without a fresh dated operator quote in THAT file first (its §1
  fixes the Dhan alert set at 4 items). `GROWW-OCO-02` is compiled out by
  the non-default `groww_orders` cargo feature (Gate 2 of the
  `groww-second-feed-scope-2026-06-19.md` §39 lattice), so an alarm for it
  would be dormant-by-construction.
- **4 were genuinely silent and are now alarmed** — the entries added to
  the list above. `BOOT-02` additionally repairs a documented false-OK: the
  WAL-suspension entry's description told the operator that the boot-probe
  escalation codes "own that page" while one of them owned no page at all.

Ratchet: `crates/storage/tests/critical_errcode_alarm_coverage_guard.rs`
re-derives this whole classification on every build and fails if a Critical
code with a real emit site is neither alarmed nor allowlisted, if an
allowlist row goes stale, or if a no-emit Critical code gains an alarm
(dead monitor). Its allowlist is a shrinking ratchet. Cost: +4 alarms
≈ +$0.40/mo — see `aws-budget.md` COST NOTE 2026-08-11.

**Retired paging entries:** the `ws-gap-07` filter+alarm was RETIRED
PR-C2 2026-07-13 — its only ERROR-level emit site (the main-feed
frame-channel Closed arm in the deleted `connection.rs`) died with the Dhan
live-WS lane, so the filter could never match again; the tf map entry was
removed the same day (dated note in `error-code-alarms.tf`). The
`cross-verify-1m-01` + `cross-verify-1m-02` filters+alarms (added earlier
the SAME day by automation-gaps PR-3) were RETIRED PR-C3 2026-07-14 — their
emit module `cross_verify_1m_boot.rs` was deleted with the Dhan instrument
chain (the 15:31 IST Dhan live-vs-historical cross-verify has no live side
to compare — `cross-verify-1m-error-codes.md` retirement banner), so both
filters could never match again; the tf map entries were removed in the
same PR (dated note in `error-code-alarms.tf`). The `feed-stall-01`
filter+alarm AND the `tv-<env>-feed-stall-restarts` counter alarm
(`feed-stall-restart-alarm.tf`) were RETIRED 2026-07-15 with the Groww live
feed — their emit sites (the stall watchdog + sidecar supervisor) were
deleted, so neither could ever fire again (dated notes in both tf files).
The `ws-reinject-01` filter+alarm was RETIRED 2026-07-17 (dead live-WS
sweep stage 1) — its only emit site, `crates/app/src/wal_reinject.rs`
(retained un-consumed since PR-C2 "pending the Phase C module cleanup"),
was deleted in that cleanup, so the filter could never match again; the tf
map entry was removed in the same PR (dated note in
`error-code-alarms.tf`; the `WsReinject01Aborted` variant retirement is
the post-sibling-merge variant sweep). The `tick-conserve-01` filter+alarm
was RETIRED 2026-07-18 (tick-conservation retirement — dead-WS sweep
follow-up) — its only emit site, `crates/app/src/tick_conservation_boot.rs`
(the 15:40 IST reconciler's Leak arm), was deleted with the audit modules:
every audit input died with the dead tick chain in the stage-2 sweep #1631
(no live WAL frame writer, no processor outcome counters, nothing writes
`ticks`), so every run could only record `partial` and the filter could
never match again; the tf map entry was removed in the same PR (dated note
in `error-code-alarms.tf`; the `TickConserve01DailyResidual` variant
retired in the same PR; the `tick_conservation_audit` QuestDB TABLE is
retained — SEBI 5y, never dropped).

> Removed from the filtered+alarmed set: the Dhan REST canary code
> (RETIRED 2026-07-14 with its module + both spawn sites + the
> `rest-canary-01` map entry, per the operator Dhan noise lock —
> `dhan-rest-only-noise-lock-2026-07-14.md`; the retained spot-1m +
> option-chain legs self-detect a dead Dhan REST surface within ~3-4 min
> via their own escalation edges).

## The ErrorCode taxonomy (53 variants, 100% rule-synced)

Every tracked error/invariant lives in `crates/common/src/error_code.rs`:

- **I-P0-03** — Instrument priority-0 (expiry check at OMS gate 4)
- **I-P1-05/06/08/11** — Instrument priority-1
- **I-P2-02** — Instrument priority-2 (trading-day guard)
- **GAP-NET-01** — IP monitor ; **GAP-SEC-01** — API auth
- **OMS-GAP-01..06** — Order Management System
- **WS-GAP-01..03** — WebSocket
- **RISK-GAP-01..03** — Risk engine
- **AUTH-GAP-01..02** — Authentication
- **STORAGE-GAP-01..02** — Storage layer
- **DH-901..910** — Dhan Trading API error codes
- **DATA-800/804..814** — Dhan Data API error codes (rules reference
  these as bare backticked numbers — the cross-ref test handles both)

Every variant carries:
- `code_str()` — the stable wire-format string ("I-P1-11", "DH-904", ...)
- `severity()` — `Info < Low < Medium < High < Critical`
- `runbook_path()` — `.claude/rules/*.md` file documenting triage
- `is_auto_triage_safe()` — never true for Critical

## The 21 mechanical ratchets (every one blocks a regression)

| # | Test file | What it guarantees |
|---|-----------|--------------------|
| 1 | `crates/common/src/error_code.rs::test_all_variants_have_unique_code_str` | No code-string collisions |
| 2 | same::`test_code_str_roundtrip_via_from_str` | `ErrorCode -> str -> ErrorCode` identity |
| 3 | same::`test_from_str_rejects_unknown_code` | Unknown input returns typed error, never panics |
| 4 | same::`test_every_variant_has_non_empty_runbook_path` | Every variant points at `.claude/` |
| 5 | same::`test_severity_ordering` | Info < Low < Medium < High < Critical |
| 6 | same::`test_severity_as_str_is_stable` | Wire-format labels are stable |
| 7 | same::`test_critical_codes_never_auto_triage` | Safety invariant: Critical is always operator-action |
| 8 | same::`test_every_severity_is_assigned_to_at_least_one_code` | No dead severity tiers |
| 9 | same::`test_display_matches_code_str` | `Display` produces the wire format |
| 10 | same::`test_all_list_length_matches_catalogue_size` | `all()` vector cannot drift from enum |
| 11 | same::`test_code_str_follows_expected_prefix_pattern` | No rogue prefixes |
| 12 | `crates/common/tests/error_code_rule_file_crossref.rs::every_error_code_variant_appears_in_a_rule_file` | Every variant has rule documentation |
| 13 | same::`every_rule_file_code_has_an_enum_variant` | Every rule code has an enum variant (2-entry allowlist for historical typos) |
| 14 | same::`every_runbook_path_exists_on_disk` | `runbook_path()` always resolves to a real file |
| 15 | `crates/common/tests/error_code_tag_guard.rs::every_error_macro_tagged_with_a_known_code_carries_code_field` | Every `error!` that mentions a code in its message MUST also have `code = ErrorCode::X.code_str()` |
| 16 | same::`tagged_prefix_set_is_non_empty` | Guard setup sanity |
| 17 | `crates/storage/tests/error_level_meta_guard.rs::flush_persist_broadcast_failures_must_use_error_level` | No flush/persist/drain failure may be logged at `warn!` |
| 18 | same::`phrases_list_is_non_empty_and_lowercase` | Guard setup sanity |
| 19 | `crates/app/src/observability.rs::test_histogram_buckets_are_non_empty_and_monotonic` | Prometheus `_duration_ns` buckets stay monotonic |
| 20 | same::`init_errors_jsonl_appender_creates_directory` | JSONL sink does the side-effect it promises |
| 21 | same::`sweep_errors_jsonl_retention_*` (4 tests) | 48h retention sweeper preserves fresh, deletes old, ignores unrelated, handles missing dir |

Running `cargo test --workspace` executes every one; CI blocks merge on
any failure.

## The `#![deny(unused_must_use)]` blanket

Every prod `lib.rs` carries:

```rust
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
```

Any dropped `Result`, `let _ = result`, `.unwrap()`, `.expect()` in prod
code fails the build. The `(not(test))` gate keeps test boilerplate clean.

## Schema self-heal at boot

`ensure_instrument_tables` runs idempotent `ALTER TABLE ADD COLUMN IF
NOT EXISTS` between `CREATE TABLE` and `DEDUP ENABLE` so that tables
created by earlier builds (before the 2026-04-17 lifecycle columns
arrived) auto-migrate without a one-shot SQL script. See
`crates/storage/src/instrument_persistence.rs::ensure_instrument_tables`.

Future schema changes MUST follow this pattern: `CREATE TABLE IF NOT
EXISTS` with the new full schema, then an `ALTER TABLE ADD COLUMN IF
NOT EXISTS` for every column that didn't exist in a previous release.
QuestDB ignores ADDs that already exist, so running every boot is free.

## The auto-triage flow (Phase 6+, upcoming)

```
errors.jsonl ──→ signature_hash (sha256 of code+module+truncated_message)
                 │
                 ↓
     .claude/triage/error-rules.yaml
                 │
    ┌────────────┼────────────┐
    ↓            ↓            ↓
Known + safe   Known but    Novel
(severity<     Critical     signature
Critical)
    │            │            │
Auto-fix    Escalate:    Open draft
(runbook    Telegram +   GitHub Issue
script)     SMS +        with full
            GitHub       context,
            Issue        ping operator
```

Claude Code `/loop 5m .claude/triage/claude-loop-prompt.md` polls the
summary file and drives the above flow.

## Canonical on-disk paths (DO NOT CHANGE without updating Loki/Alloy)

> **2026-07-05 UPDATE (operator directive: "one human log file; robot files
> into machine/ subfolder"):** every MACHINE sink the app writes moved from
> the `data/logs/` top level to `data/logs/machine/`. The top level is the
> HUMAN surface only: the launcher-owned `data/logs/tickvault.log` symlink +
> `data/logs/app.<IST-date>.log` daily rolling file. Consumers (MCP server,
> Makefile, doctor scripts, triage hooks/configs, Alloy config) were updated
> in the same change; the retention sweepers sweep BOTH dirs during a grace
> window so legacy files at the old paths age out naturally (no boot-time
> file moves). The app-log sweeper skips every `*.log` name so it can never
> delete the human daily log. Machine-dir ratchet:
> `crates/app/src/observability.rs::test_all_machine_sink_dirs_live_under_machine_subdir`.
>
> **2026-07-06 correction — the AWS shipping consumer was MISSED:** the
> 2026-07-05 consumer sweep did NOT update the CloudWatch agent's collect_list
> (`deploy/aws/cloudwatch-agent.json` + `user-data.sh.tftpl`) — its old
> top-level globs do not descend into `machine/`, so BOTH `/tickvault/prod/app`
> log streams went dead and every log metric filter on that group was DOA.
> Fixed 2026-07-06: the agent now tails `data/logs/machine/errors.jsonl.2*` +
> `data/logs/machine/app.2*` (date-stamped rotations ONLY — excludes the bare
> `errors.jsonl` compat symlink + the 0-byte `app.log` placeholder; the exact
> collect_list is pinned by `crates/app/tests/cloudwatch_agent_glob_guard.rs`
> from #1438, so no legacy top-level globs are allowed), ratcheted by
> `crates/common/tests/cloudwatch_app_alarms_wiring.rs::test_cw_agent_collects_machine_log_paths`.

| Path | Purpose | Writer |
|------|---------|--------|
| `data/logs/tickvault.log` → `app.<IST-date>.log` | the ONE human log surface (symlink + daily rolling) | launcher (local-runtime) |
| `data/logs/machine/app.YYYY-MM-DD-HH` | full app log, hourly rotated (robot) | `init_app_log_appender` |
| `data/logs/machine/app.log` | 0-byte Alloy file-watch placeholder | `infra.rs` |
| `data/logs/machine/errors.log` | WARN+ single file | existing |
| `data/logs/machine/errors.jsonl.YYYY-MM-DD-HH` | ERROR JSONL hourly | Phase 2 — `tracing-appender 0.2.3` |
| `data/logs/machine/errors.summary.md` | human/Claude-readable snapshot | Phase 5 — refreshed every 60s |
| `data/logs/machine/candles/`, `data/logs/machine/live_ticks/` | per-category hourly appenders | category layers |
| `data/logs/auto-fix.log` | audit trail of auto-triage actions (script-owned; machine/ move is a flagged follow-up) | Phase 6 |
| `.claude/triage/error-rules.yaml` | triage classifier | Phase 6 |
| `.claude/triage/claude-loop-prompt.md` | Claude-watches-logs runbook | Phase 7 |
| `.claude/state/triage-seen.jsonl` | edge-trigger dedup | Phase 6 |
| `data/orders/` (+ `groww-intents-YYYYMMDD.ndjson`) | Groww order intent write-ahead ledger (fsync-per-append; IST-midnight rotation; retained, no sweeper) | Groww orders (2026-07-15) |

## What future sessions MUST NOT do

1. **Do not re-audit WARN→ERROR for flush/persist/drain sites.** The 28
   phrases in `crates/storage/tests/error_level_meta_guard.rs` are
   ratcheted. Adding a new flush handler? The meta-guard tells you the
   pattern by example. Don't scan the codebase from scratch.
2. **Do not duplicate the ErrorCode enum.** If a new code is needed,
   add a variant + a rule-file mention in the SAME PR. The cross-ref
   test enforces both directions; running it shows the gap.
3. **Do not add a new dropped-Result site.** `unused_must_use` denies
   at compile time. If the build fails with that error, handle the
   Result — don't `#[allow(...)]` it.
4. **Do not change the canonical paths in the table above.** Downstream
   Alloy/Loki scrapers, the summary writer, and the triage hook all
   hard-code these.
5. **Do not introduce a new `warn!` on a flush/persist/drain failure.**
   The meta-guard regexes these phrases; violations fail the build.
   Use `error!` with a `code =` field.
6. **Do not log at ERROR without a `code =` field** if the message
   mentions a known code prefix (I-P*, OMS-*, WS-*, STORAGE-*, etc.).
   The tag-guard fails the build.
7. **RETIRED 2026-06-10 (Phase B batch 2 of the deletion audit).**
   Originally: do not upgrade `DepthRebalanced` back to `Severity::High`.
   The `DepthRebalanced` / `DepthRebalanceFailed` variants and their
   severity ratchets were DELETED — the depth rebalancer that emitted
   them was removed in AWS-lifecycle PR #4 (2026-05-19) and the variants
   had zero production constructors since. Retained for historical
   audit: 2026-04-24 PR #337 downgraded routine zero-disconnect drift
   swaps from `[HIGH]` to `Severity::Low` to stop pager fatigue.
8. **RETIRED 2026-06-10 (Phase B batch 2).** Originally: do not drop
   the swap-level from the depth-rebalance Telegram title
   (`Depth-20 rebalance: <UL>` / `Depth-20+200 rebalance: <UL>`).
   `DepthRebalanceLevels::title_fragment()` and its title ratchets were
   DELETED with the variants. Historical context: the 2026-04-22
   BANKNIFTY incident proved swap-scope-at-a-glance wording was
   safety-critical while depth feeds existed.
9. **Do not regress the `websocket_connections` counter write.**
   `spawn_pool_watchdog_task` MUST call
   `health.set_websocket_connections(active_count)` every 5s.
   Removing this silently restores the "0/5 forever" bug the 09:15:30
   heartbeat revealed on 2026-04-24. Ratchet:
   `test_pool_watchdog_task_accepts_health_status` (source-scan guard
   in `crates/api/tests/health_counter_fix7_guard.rs`).
10. **RETIRED 2026-06-10 (Phase B batch 2).** Originally: do not
    re-route `DepthRebalanced` (Severity::Low) to Telegram. The
    suppression block in `NotificationService::notify()`, the
    `tv_depth_rebalance_telegram_suppressed_total` counter, and the
    suppression-guard ratchets were DELETED along with the
    `DepthRebalanced` variant itself (zero production emitters since
    the depth feeds were removed in AWS-lifecycle PR #4). Historical
    context: the 2026-05-11 suppression existed because operators
    reported 10-30 non-actionable Telegram messages/day; the principle
    it encoded — Telegram is reserved for eyes-on-now events — remains
    in force for all surviving events.

## Completion status of the Zero-Touch plan

- [x] **Phase 0** — 25 WARN→ERROR escalations, `unused_must_use` lint,
      schema self-heal, error-level meta-guard
- [x] **Phase 1** — ErrorCode enum (53 variants), 3 cross-ref tests,
      tag-guard meta-test, first 3 production sites migrated
- [x] **Phase 2** — errors.jsonl hourly-rotated appender, 48h retention
      sweeper, 6 new observability tests, fully wired in main.rs
- [ ] **Phase 3** — Re-add Loki + Alloy (blocked on QuestDB mem trim
      confirmation)
- [ ] **Phase 4** — AWS CloudWatch parity (deferred until instance
      provisioned; Terraform stays ready)
- [x] **Phase 5** — `errors.summary.md` writer (60s refresh,
      signature-hash grouping, lookback filter, 18 unit tests, wired
      as a background tokio task from main.rs) + `make errors-summary`
- [x] **Phase 6** — `.claude/triage/error-rules.yaml` (7 seed rules
      incl. clear-spill), `.claude/triage/claude-loop-prompt.md`,
      `error-triage.sh` shell hook, 3 auto-fix scripts, triage_rules_guard
      meta-test, `make triage-dry-run/triage-execute`
- [x] **Phase 7.1** — `/loop` runbook prompt
      (`.claude/triage/claude-loop-prompt.md`).
- [x] **Phase 7.2** — Triage MCP server shipped:
      `crates/tickvault-logs-mcp/src/tools.rs` (2026-07-18 Rust cutover —
      the Python server was deleted; launched via
      `scripts/mcp-servers/tickvault-logs-launch.sh`) exposes the full
      triage flow over stdio MCP — `_signature_hash` (FNV-1a of
      code+target+first-160), `tool_triage_log_tail`,
      `tool_find_runbook_for_code`, `tool_list_novel_signatures`,
      `tool_tail_errors`, `tool_summary_snapshot`, `tool_signature_history`.
      Auto-loaded via `.mcp.json` `tickvault-logs` entry; tools surface
      as `mcp__tickvault-logs__*` in any Claude Code session.
- [~] **Phase 8.1** — Common auto-fix scripts (restart-depth, refresh-
      instruments, clear-spill — all three scaffolded, refresh-instruments
      fully functional today); Phase 8.2 Lambda bridge deferred until
      AWS instance provisioned
- [~] **Phase 9.1** — Grafana Operator Health single-page dashboard
      RETIRED with the CloudWatch-only migration (#O1 — Grafana removal).
      The `deploy/docker/grafana/dashboards/` tree and its
      `operator_health_dashboard_guard.rs` ratchet were deleted along
      with the `tv-grafana` container. CloudWatch Dashboards replace
      operator visualization in prod (free tier: 3 dashboards).
- [x] **Phase 9.2** — `scripts/validate-automation.sh` + `make
      validate-automation` runs 20 end-to-end checks.
- [x] **Phase 10.1** — Zero-tick-loss alert guard (4 Prometheus alerts
      pinned, 7 source-invariant tests).
- [~] **Phase 10.2** — Sequence-hole detector was shipped, then RETIRED
      when the depth-20/200 feeds were removed (AWS-lifecycle PR #4).
      Depth WebSockets are forbidden forever per
      `.claude/rules/project/websocket-connection-scope-lock.md`; the
      detector module and its DHAT + loom ratchets were deleted too.
- [~] **Phase 10.3** — Tick-loss chaos test shipped 2026-05, then RETIRED
      2026-07-17 (stage-2 dead-WS sweep): `chaos_zero_tick_loss.rs` was
      deleted with its subject — the dead tick writer
      (`tick_persistence.rs`) had zero production callers after the
      live-WS retirements (Dhan 2026-07-13, Groww 2026-07-15). The
      candle-side chaos coverage (seal ring → spill → DLQ) remains in the
      surviving `chaos_*` suites.
- [~] **Phase 11** — WS + QuestDB + Valkey resilience SLA ALERT GUARD
      partially retired: the Prometheus-side `resilience_sla_alert_guard.rs`
      was deleted with the Alertmanager + Prometheus removals (#O2 / #O3).
      The chaos integration tests remain: `chaos_questdb_docker_pause.rs`
      (Phase 11.1 nightly chaos), `chaos_rescue_ring_overflow.rs` +
      `chaos_ws_frame_spill_saturation.rs` (Phase 11.2 backpressure sim),
      `chaos_valkey_kill.rs` (Phase 11.3 Valkey kill-test). The CloudWatch
      alarm equivalents land alongside the prod CloudWatch migration.
- [x] **Phase 12.1** — ratcheted per-crate line-coverage floors (68.3–99.4, target 100%; floors only move up) set in
      `quality/crate-coverage-thresholds.toml`, enforced by
      `scripts/coverage-gate.sh` in CI
- [x] **Phase 12.5** — O(1) hot-path ratchet: DHAT zero-alloc tests +
      `quality/benchmark-budgets.toml` (Criterion ≤10ns/50ns/100ns
      budgets, 5% regression gate, enforced by
      `scripts/bench-gate.sh`)
- [x] **Phase 12.6** — Boot-time/process-time self-check via the e2e
      chain test (`crates/app/tests/observability_chain_e2e.rs`)
      asserting error! → JSONL → summary.md in-process
- [x] **Phase 12.2** — Mutation zero-survivor gate already active in
      `.github/workflows/mutation.yml:103-113` — any `SURVIVED` line
      in results fails the PR.
- [~] **Phase 12.3** — Fuzz duration. Current default config in
      `.github/workflows/fuzz.yml` is 1 hour per target per run
      (`FUZZ_SECS: 3600`, overridable via workflow_dispatch). "24h
      clean" aspiration would exceed GitHub Actions free-tier budget —
      deferred until either (a) operator confirms paid-tier OK
      or (b) we self-host the fuzz runner.
- [x] ~~**Phase 12.4** `#![deny(warnings)]` workspace-wide~~ SKIPPED —
      future toolchain deprecation warnings would silently break
      prod builds; the targeted lints already in place
      (`unused_must_use`, `unwrap_used`, `expect_used`, `print_*`,
      `dbg_macro`, `let_underscore_must_use`) are strictly better.

## Commit pointers (so sessions can jump to a specific change)

Branch: `claude/debug-expired-update-error-p5jSL` (PR #276)

| Commit | What |
|--------|------|
| `ae5c855` | Schema self-heal `ALTER TABLE ADD COLUMN IF NOT EXISTS` |
| `b53bca6` | Tick flush WARN→ERROR (5 sites) |
| `0ca2cb6` | Candle + depth flush WARN→ERROR (9 sites) |
| `f37484d` | App/core/historical WARN→ERROR (8 sites) |
| `eea39f7` | `unused_must_use` lint + error-level meta-guard |
| `e4fdf95` | ErrorCode enum + cross-ref tests |
| `8d34421` | Tag-field migration + tag-guard meta-test |
| `cce7188` | `tracing-appender` dep + errors.jsonl foundation |
| `d942695` | Wire JSONL layer + retention sweeper into main.rs |
| `7b87ab2` | Phase 5 summary_writer + this architecture doc |
| `a3145e9` | Phase 6 triage YAML + auto-fix scripts + shell hook |
| `9e807ca` | Phase 12.6 observability_chain_e2e (+ flatten_event fix) |
| `275157a` | Phase 10.1 zero-tick-loss alert guard (7 pins) |
| `a81206f` | Phase 2.2 / 5.2 / 8.1 / 9.2 operator commands + 20-check validation |
| `1cdd78a` | Phase 9.1 operator-health single-page Grafana dashboard |
| `897f7b6` | Phase 11 resilience SLA alert guard (WS/QuestDB/Valkey pins) |

## Trigger (auto-loaded paths)

This rule activates when editing:
- `crates/app/src/observability.rs`
- `crates/app/src/main.rs` (tracing subscriber section)
- `crates/common/src/error_code.rs`
- `crates/common/tests/error_code_*.rs`
- `crates/storage/tests/error_level_meta_guard.rs`
- `crates/storage/src/instrument_persistence.rs` (schema)
- Any `*.rs` file containing `error!(`, `warn!(`, `tracing::error!(`
- Any file under `data/logs/` or `.claude/triage/`
