# Dual-Feed Scoreboard — Error Codes (SCOREBOARD-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` (the two-feed contract) > this file.
> **Operator directive (2026-07-10, dual-feed scoreboard):** *"run these two
> websockets live for a month... all tracked, captured, visualized, logged,
> monitored, 100% automated"* + the blame-attribution demand *"ensure and
> CAPTURE that the issue really arose from the broker side"*.
> **Companion code:** `crates/app/src/feed_scoreboard_boot.rs` (the 15:45 IST
> daily aggregation + the boot-time process-death reconciler),
> `crates/common/src/feed_blame.rs` (the pure TOTAL blame classifier),
> `crates/storage/src/feed_scoreboard_persistence.rs` +
> `crates/storage/src/feed_episode_audit_persistence.rs` (the forensic tables),
> `crates/common/src/error_code.rs::ErrorCode::Scoreboard01AggregationDegraded`.
> **Companion runbook:** `docs/runbooks/dual-feed-scoreboard.md` (month-end
> cumulative verdict SQL + the indeterminate-review procedure).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `Scoreboard01*` variant verbatim —
> `Scoreboard01AggregationDegraded` and `SCOREBOARD-01` appear below.

---

## §0. Why this code exists (the month-long dual-feed verdict)

The operator runs Dhan (feed #1) + Groww (feed #2) live in parallel for a
month and needs a DAILY, durable, blame-attributed scorecard: per-feed ticks,
coverage minutes, disconnects with **who caused them** (broker / ours /
unclear), stalls, uptime — one Telegram at 3:45 PM IST + two
`feed_scoreboard_daily` rows + one `feed_episode_audit` row per episode. The
whole subsystem is a best-effort forensic AGGREGATE over the existing
system-of-record tables (`ws_event_audit`, `ticks`) — it is NEVER on the tick
hot path, the order path, or any feed's recovery path. SCOREBOARD-01 is the
typed record of every degraded leg of that aggregation.

## §1. SCOREBOARD-01 — daily scoreboard aggregation degraded

**Severity:** Medium. **Auto-triage safe:** Yes (the degrade already
happened; the tables are DEDUP-idempotent, so a re-run backfills the DAILY
AGGREGATION — the operator inspects at leisure). EXCEPTION (round-2 hostile
review 2026-07-10): `detector='boot_reconciled'` process-death rows are NOT
re-creatable by any re-run — only the boot that detected the death can pair
prior-up → first-connect. Their flush therefore retries in place (3 × 60s);
if `stage="reconcile_flush"` exhausted every attempt (or
`stage="reconcile_panic"` fired), those episodes are permanently lost and
the month restart count under-counts — annotate the month summary.

**Trigger:** one of the scoreboard legs failed
(`ErrorCode::Scoreboard01AggregationDegraded`):

1. A QuestDB `/exec` read (today's `ws_event_audit` rows, the
   `feed_episode_audit` blame aggregate, the per-feed `ticks`
   counts / distinct-minute sets) failed or returned an unparsable body —
   the affected columns are recorded as **−1 sentinels** and the daily row
   is stamped `outcome='partial'`. Never fabricated zeros (Rule 11).
2. The `feed_scoreboard_daily` / `feed_episode_audit` ILP-over-HTTP write
   was rejected (the per-flush server ACK surfaces schema drift / DEDUP
   violations as `Err` — the 2026-07-05 fire-and-forget lesson).
3. The boot-time process-death reconciler could not read today's
   `ws_event_audit` or write its synthesized `process_death` episodes.
4. The same-day `tv_ws_event_audit_dropped_total` cross-check is non-zero —
   the episode source itself under-counted (AUDIT-WS-01 drops), so the day
   is stamped `outcome='degraded'`.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `SCOREBOARD-01`; the payload
   names the failed leg (`stage` / `reason` fields).
2. `make doctor` / `mcp__tickvault-logs__run_doctor` — a failed `/exec` or
   ILP leg almost always means QuestDB was down at 15:45 (cross-check
   BOOT-01/BOOT-02).
3. Backfill once QuestDB is healthy: restart with
   `TICKVAULT_SCOREBOARD_NOW=1` (the run is DEDUP-idempotent — same
   deterministic 15:45 IST `ts`, so the day's rows UPSERT in place; on a
   NON-trading day the no-DATE forced run is REFUSED — pass
   `TICKVAULT_SCOREBOARD_DATE` for the trading day you meant). ⚠ Re-runs
   are KEY-idempotent, NOT VALUE-idempotent (round 3, 2026-07-10): a
   re-run re-classifies with the evidence of the RE-RUN instant, and past
   the 48h errors.jsonl horizon that evidence is gone — the keep-better
   guard therefore SUPPRESSES any overwrite that would downgrade an
   existing `run_partial=false` (evidence-backed) episode row with a
   `run_partial=true` re-classification, keeping the original verdict and
   logging `stage="blame_regression"`. Round 4 (2026-07-10): evidence
   completeness is HORIZON-aware — a target day past the retention
   horizon is treated as partial evidence even when every retained file
   opened cleanly (I/O success alone previously bypassed both
   `run_partial` and the guard). Round 5 (2026-07-10) tightened the
   horizon to YESTERDAY (`ERRORS_JSONL_EVIDENCE_RETENTION_DAYS = 1`):
   the sweep is 48h by file MTIME, so only yesterday's 24 hourly files
   are guaranteed alive at any instant of today — a 2-day-old target's
   session-hour files are already swept for any run after ~10:00, and
   the old `2` let exactly that backfill claim complete evidence and
   destructively re-stamp evidence-backed blame. If the guard's own read fails
   (`stage="keep_better_read"`), it is OFF for that run — re-run stale
   days only with a healthy QuestDB read side. The DAILY row has its own
   keep-better (round 4; extended round 5): a rerun can never erase an
   existing `outcome='degraded'` verdict, and a rerun that re-measured
   ZERO ticks can never upgrade an existing `outcome='feed_off'` row to
   complete-with-zeros (the evening re-enable-for-tomorrow rerun shape;
   real measured ticks may upgrade) — `stage="outcome_regression"` names
   a suppressed erase; `stage="outcome_keep_better_read"` = the guards'
   shared read failed and both are OFF for the run. A post-trigger
   same-day boot whose day already carries a TERMINAL row for BOTH feeds
   — `complete` OR `feed_off` (round 5: complete-on-both left the latch
   permanently dead on single-feed profiles) — SKIPS the redundant rerun
   + duplicate card (a forced `TICKVAULT_SCOREBOARD_NOW` run still
   overrides). What a backfill CANNOT
   recover is in `docs/runbooks/dual-feed-scoreboard.md`
   (same-day errors.jsonl correlation ages out after 48h → 805 episodes
   default broker, RSTs default indeterminate; boot-reconciled
   process-death rows are boot-only — see the §1 exception above; a
   `stage="reconcile_flush_exhausted"` line means the boot's synthesized
   death rows never reached QuestDB at all — counted on that boot's own
   card in-memory, permanently absent from the month table).
4. `outcome='degraded'` days (audit under-count): treat the day's episode
   counts as a floor, not a truth — the runbook's month-end checklist
   excludes/annotates them.
5. `outcome='feed_off'` days (round 4, 2026-07-10; detection REDESIGNED
   round 5, HARDENED round 6): the feed was switched off for the day —
   measured-zero ticks + zero up-kind rows INSIDE the session window
   ([09:00, 15:30) IST — the ~08:33 boot Connected row no longer defeats
   the /api/feeds runtime-disable day, and a WS-GAP-04 wake's ~09:00:00
   SleepResumed row immediately re-parked by the dormant gate with a
   `feed_disabled` marker is excluded as machinery, round 6) + either no
   up rows at all (config-off) or a pre-session
   `source='feed_disabled'` toggle row that is the feed's STATE AT
   SESSION OPEN — the LAST pre-session toggle (round 6: a
   disable→re-enable flap does NOT qualify; both feeds stamp the same
   slug: the Groww bridge disable falling edge; the Dhan dormant-entry
   SleepEntered row). A boot up row WITHOUT a disable-at-open state is an
   ENABLED-but-dead-broker day and is never softened into feed_off (the
   runtime enabled flag additionally blocks the NO-marker arm on same-day
   runs; the durable marker itself OUTRANKS the run-instant flag, so a
   15:30–15:45 re-enable-for-tomorrow no longer stamps the day
   complete-with-zeros). The PARTNER feed's
   `unique_win_minutes`/`both_minutes` stamp the `-1` sentinel on such a
   day (exclusive-vs-nothing is not a measurement). The card says "no
   contest" and the runbook month SQL excludes the WHOLE no-contest day
   (day-level LEFT JOIN anti-join since round 6 — QuestDB 9.3.5 rejects
   `NOT IN (SELECT …)` on TIMESTAMP with "cannot compare TIMESTAMP with
   type CURSOR"; a row-level filter left the surviving feed's one-horse
   row summing into the verdict).

**Honest envelope:** the scoreboard is evidence, not proof. Lone mid-stream
resets are honestly `indeterminate` (the `disconnect_cause.rs` envelope);
blame upgrades to broker only on corroboration (Dhan codes, WS-GAP-09
overlap ±120s, stall-watchdog semantics). Lag columns are −1 sentinels until
PR-3 lands the day histograms (Dhan additionally carries a ≥1s whole-second
quantization floor — `lag_floor_ms` column). Per-instrument unique-wins land
in PR-4. **Stall episode rows are LIVE since PR-B (2026-07-10):** the Groww
sidecar stall watchdog stamps ONE `WsEventKind::StallRestarted`
(`stall_restarted`) ws_event_audit row per kill+relaunch (both the classic
FEED-STALL-01 arm and the §1b never-streamed arm), carrying a FIXED machine
cause slug in `source` (`stall_silent_socket` / `stall_never_streamed` /
`stall_auth_stale` / `stall_entitlement` — `feed_blame::STALL_SOURCE_*`;
never raw child text) through the SAME best-effort try_send pipeline
(failure = AUDIT-WS-01); the 15:45 aggregation maps them to the
`stall_restart` / `never_streamed_restart` episode kinds (detector
`stall_row`) and the card's Stalls column is a measurement from the PR-B
deploy forward (0 = measured 0; PRE-ship days honestly read 0 with the
CloudWatch `tv_feed_sidecar_stall_restart_total` counter holding the past —
runbook caveat). Two honest bounds: (a) the **stalls column is Groww-only
by construction** — the Dhan main-feed has no sidecar; its silent-socket
detection is the WS-GAP-06/activity-watchdog reconnect machinery, already
counted as disconnect/reconnect rows; (b) in-sidecar reconnects that
recover FASTER than the 30s stall threshold write no row on either column.
A failure here NEVER affects tick capture, candles, orders, or feed
recovery. Delivery boundary: SCOREBOARD-01 is log-sink-only today (no
`error_code_alerts` map entry — the alarm budget is exhausted); the daily
Telegram scorecard (or its `DualFeedScorecardAborted` High page) is the
operator signal.

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::Scoreboard01AggregationDegraded`
- `crates/app/src/feed_scoreboard_boot.rs` + the `spawn_feed_scoreboard_tasks`
  arms in `crates/app/src/main.rs` (spawned on BOTH boot paths — the fast
  crash-recovery arm AND the slow process-global prefix)
- `crates/storage/src/feed_scoreboard_persistence.rs` /
  `crates/storage/src/feed_episode_audit_persistence.rs` (the tables; their
  ensure-DDL failure arms carry the literal `code = "SCOREBOARD-01"` +
  `stage` per the tag-guard convention)
- `crates/common/src/feed_blame.rs` (the classifier — no emit sites; pure)

## §2. Blame taxonomy (persisted on every `feed_episode_audit` row)

| Evidence | Blame | reason slug |
|---|---|---|
| `process_death` (boot-reconciled), build sha changed vs deployed sha | ours | `deploy_restart` |
| `process_death`, sha unchanged/unknown (fail-soft) | ours | `process_restart` |
| `process_death`, reconnect landed AT/AFTER the 15:30 close AND the feed streamed through ~15:28 (round 4, 2026-07-10: the carve-out now requires streamed-through-close — a last streamed minute BEFORE 15:28 is a hole a clean scheduled stop cannot leave, so that shape re-classifies as a REAL in-market death, `process_restart`/`deploy_restart`, counted + partial floor. A failed minute read keeps the carve-out, loudly: `stage="post_close_disambiguation"`) | ours (row only — `market_hours=false`, EXCLUDED from headline restarts/blame + the partial floor) | `post_close_restart` |
| Dhan 805 + same-day RESILIENCE-01/03 line | ours | `dual_instance` |
| Dhan 805, no peer evidence (incl. >48h backfill) | broker | `rate_limit_805` |
| Dhan 807 | broker | `auth_token_expired` |
| Dhan 806/808–814 | broker | `auth_entitlement` |
| Groww `feed_disabled` | ours | `feed_toggle` |
| Groww `bridge_died` (our bridge task panicked + respawned — FEED-SUPERVISOR-01 class) | ours | `bridge_task_died` |
| Order-update `clean close` (server Close frame / stream end — idle-day close vs auth-reject delivery, not attributable per-row) | indeterminate | `clean_close` |
| Stall row (LIVE since PR-B 2026-07-10), source `stall_silent_socket` / `stall_never_streamed` | broker | `silent_socket` / `never_streamed` |
| Stall row, source `stall_auth_stale` (the child's last confirmed reject was auth-class — the shared token minter is OUR duty) | ours | `token_minter_stale` |
| Stall row, source `stall_entitlement` (Authorization/Permissions/SILENT-FEED class) | broker | `entitlement_reject` |
| PROC-01 / RESOURCE-01..03 within ±300s | ours | `resource_pressure` |
| 'Dhan or network' RST + WS-GAP-09 overlap ±120s | broker | `bare_rst` / `rate_limit_429` |
| 'Dhan or network' RST alone | indeterminate | `transport_ambiguous` |
| 'Network / connection' | indeterminate | `network_path` |
| 'Unknown' | indeterminate | `unknown_cause` |
| Dhan off-hours disconnect | indeterminate | `off_hours_idle` (excluded from the headline market-hours count) |
| anything else (fail-closed floor) | indeterminate | `unclassified` |

Blame can NEVER be blank: `BlameClass` is a 3-variant enum (no `Option`),
the episode writer takes it by value (compile error without one), the
classifier is total (`_ => Indeterminate`, proptest-pinned never-panic).

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `Scoreboard01*` variant)
- `crates/common/src/feed_blame.rs`
- `crates/storage/src/feed_scoreboard_persistence.rs`
- `crates/storage/src/feed_episode_audit_persistence.rs`
- `crates/app/src/feed_scoreboard_boot.rs`
- `crates/app/src/groww_sidecar_supervisor.rs` (the `stall_restarted` emit)
- Any file containing `SCOREBOARD-01`, `Scoreboard01`, `feed_scoreboard_daily`,
  `feed_episode_audit`, `feed_coverage_daily`, `classify_episode`,
  `BlameClass`, `StallRestarted`, or `STALL_SOURCE_`
