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
   completeness is HORIZON-aware — a target day past the ~48h retention
   is treated as partial evidence even when every retained file opened
   cleanly (I/O success alone previously bypassed both `run_partial` and
   the guard). If the guard's own read fails
   (`stage="keep_better_read"`), it is OFF for that run — re-run stale
   days only with a healthy QuestDB read side. The DAILY row has its own
   keep-better (round 4): a rerun can never erase an existing
   `outcome='degraded'` verdict (`stage="outcome_regression"` names a
   suppressed erase; `stage="outcome_keep_better_read"` = that guard's
   read failed and it is OFF for the run). A post-trigger same-day boot
   whose day already carries complete rows for BOTH feeds SKIPS the
   redundant rerun + duplicate card (a forced `TICKVAULT_SCOREBOARD_NOW`
   run still overrides). What a backfill CANNOT
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
5. `outcome='feed_off'` days (round 4, 2026-07-10): the feed was switched
   off for the day (zero up-kind connection rows + measured-zero ticks;
   same-day runs additionally consult the runtime enabled flag so an
   ENABLED-but-dead-broker day is never softened into feed_off). The card
   says "no contest" and the runbook month SQL EXCLUDES these one-horse
   days from the win/coverage sums.

**Honest envelope:** the scoreboard is evidence, not proof. Lone mid-stream
resets are honestly `indeterminate` (the `disconnect_cause.rs` envelope);
blame upgrades to broker only on corroboration (Dhan codes, WS-GAP-09
overlap ±120s, stall-watchdog semantics). Lag columns are −1 sentinels until
PR-3 lands the day histograms (Dhan additionally carries a ≥1s whole-second
quantization floor — `lag_floor_ms` column). Per-instrument unique-wins land
in PR-4; stall episode rows land in PR-2 (pre-ship days read 0 stalls — the
CloudWatch `tv_feed_sidecar_stall_restart_total` counter holds the past).
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
| Stall (PR-2), silent-socket / never-streamed | broker | `silent_socket` / `never_streamed` |
| Stall, token/auth-stale class | ours | `token_minter_stale` |
| Stall, Authorization/Permissions/entitlement class | broker | `entitlement_reject` |
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
- Any file containing `SCOREBOARD-01`, `Scoreboard01`, `feed_scoreboard_daily`,
  `feed_episode_audit`, `feed_coverage_daily`, `classify_episode`, or
  `BlameClass`
