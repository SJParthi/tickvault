# Dual-Feed Scoreboard — Error Codes (SCOREBOARD-01)

> **⚠ 2026-07-15 dormancy note (Groww live feed retired — operator Q1, received directly in this session:
> *"remove the whole Groww live feed; keep only spot 1m and option chain for both brokers; go."*):** the
> Groww LIVE inputs retire with the feed — live lifecycle episode rows, the presence registry, and the live
> lag histograms stop being produced; the FEED-GAP dangling-close sweep step is REMOVED (its subsystem is
> deleted — `feed-gap-error-codes.md` retirement banner). The REST-leg digest (§2b) and the episode
> aggregation over HISTORICAL `ws_event_audit` rows SURVIVE; from 2026-07-15 forward the live
> episode/presence/lag columns honestly read `feed_off`/absent per the existing round-4/5/6 semantics.

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

> **⚠ 2026-07-13 supersession (Dhan live WS retired):** the month-long dual-feed
> comparison purpose above is CONCLUDED — the Dhan live WS is retired by the 2026-07-13
> operator directive (verbatim + quantified evidence: `websocket-connection-scope-lock.md`
> "2026-07-13 Amendment" §E). The scoreboard machinery is KEPT as the Groww-monitoring
> scorecard + the audit surface for the Dhan REST cross-checks. Contract points:
> **(a)** Dhan `feed_scoreboard_daily` rows legitimately read `feed_off`/absent —
> "Dhan live feed disabled by operator 2026-07-13", never an outage; the round-6
> disable-at-session-open detection already classifies a config-OFF Dhan day as
> `feed_off` (no up rows + no toggle = the config-off arm). **(b)** The blame classifier
> and SCOREBOARD-01 must NOT page for the missing Dhan feed: no Dhan episodes exist to
> classify on a retired-feed day (no `ws_event_audit` Dhan rows are written — true TODAY,
> post-Phase-A, where the order-update WS is not spawned; see the obligation below for
> the post-Phase-C state), and a
> `feed_off` day is excluded from the month sums per the existing round-4/5/6 semantics —
> any future "Dhan absent" page would be a Rule-11 false alarm, REJECT. **(c)** The Dhan
> rows of the blame taxonomy table (805/807/streak arms) are retained for the
> already-captured dual-feed days + historical backfills; the order-update WS (kept
> functional-dormant in `dhan_rest_stack`) still writes its lifecycle rows, which the
> scoreboard may aggregate as before. **(d)** Cross-feed presence pairing degrades
> honestly to Groww-singleton slots (every slot one-sided); `unique_win_minutes` /
> `both_minutes` on the Groww row follow the documented one-horse semantics.
>
> **Honest obligation — Phase C acceptance criterion, CLOSED in PR-C1 (2026-07-13):**
> (b) and (c) were in tension AFTER the Phase C rewire — the rewired order-update WS
> still writes Dhan-feed `ws_event_audit` lifecycle rows, and an IN-SESSION order-update
> reconnect row is an up-kind row inside the session window that could defeat the
> round-6 `feed_off` classification (which requires ZERO session up-rows) for the
> permanently-off Dhan feed. **CLOSED:** PR-C1 (the same PR that ships the Q4-i
> order-update rewire) gates the feed_off up-signals on the existing
> `is_market_data_ws_type()` allowlist — `is_session_up_row`, `is_pre_session_up_row`
> AND the `any_up` fold all exclude `ws_type='order_update'` rows, pinned by
> `test_is_session_up_row_ignores_order_update_ws` +
> `test_is_pre_session_up_row_ignores_order_update_ws`. Episode ROWS for order_update
> are still persisted for forensics; only the up-signal classification filters.

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
overlap ±120s, stall-watchdog semantics). **Lag columns are LIVE since PR-C (2026-07-11):** the per-feed
in-memory DAY histograms (fed by the same `record_*_tick` calls that drive
the live lag gauges — `tv_dhan_exchange_lag_p99_seconds` +
`tv_groww_exchange_lag_p99_seconds` *(2026-07-17: the Dhan lag gauge was
deleted with the dead Dhan-lag publisher chain — the day histograms' Dhan
side reads −1 sentinels; the Groww gauge died 2026-07-15 with the Groww
live feed)*; replay/re-tail excluded at record time,
never a SQL approximation over the replay-contaminated `received_at` column)
drain into `lag_p50_ms`/`lag_p99_ms`/`lag_max_ms`/`lag_samples` on same-day
runs; a rerun/backfill that measured nothing folds the day's EXISTING
measured row forward (the step-6c lag keep-better, review round 1
2026-07-11 — `stage="lag_regression"` logs the suppression; a fresh
post-close catch-up process's empty histograms can no longer erase the
15:45 run's measured distribution with −1), so −1 survives only on days
NOTHING ever measured: pre-ship days, never-measured backfilled days, and
thin <50-sample days. The card's verdict delay rung declares a winner only
when the cross-feed p99 delta exceeds the 1000 ms Dhan clock floor
("faster prices beyond the clock floor" — sub-floor deltas are clock
asymmetry, not speed, and fall through). Resolution
asymmetry stays stated on every surface: Dhan carries a ≥1s whole-second
quantization floor while Groww is millisecond-precise but measured at the
sidecar capture instant one hop downstream of the socket (`lag_floor_ms`
column: 1000 dhan / 1 groww). **Per-instrument coverage is LIVE since
PR-D (2026-07-11)** *(RETIRED 2026-07-18, stage-4 dead-producer sweep: the
`FeedPresenceRegistry` (`feed_presence.rs` + `presence_registration.rs`) was
DELETED — its record/register producers died with the live feeds (Dhan
2026-07-13, Groww 2026-07-15), so the registry was structurally unfeedable
and every drain read `None`; `coverage_source` degrades honestly to the
documented `sql_backfill` fallback, the `feed_coverage_daily` table +
historical `in_memory`/`mixed` rows + the keep-better guard stay, and the
day-lag −1 sentinels follow the same REST-only-runtime dormancy class)*:** the in-memory `FeedPresenceRegistry`
(`crates/core/src/pipeline/feed_presence.rs`) folds one relaxed
`fetch_or` per tick into per-slot 375-minute bitsets at the SAME
DHAT-proven persist sites as the lag rings (both Dhan arms +
the Groww drain), over a CANONICAL cross-feed slot space built at the
daily-universe / Groww watch builds (stocks paired by ISIN, indices by
`canonicalize_index_symbol`, the §36 futures by `(underlying, expiry)`
contract identity — never native ids). On same-day runs the 15:45 drain
(flagged O(slots × 12 words), cold) flips `unique_win_minutes` /
`both_minutes` to registry truth ONLY when the registry covered the
full session (`in_memory`) — on `mixed` days (mid-day restart — the
pre-restart window is invisible to the process-local registry) the SQL
minute sets stand for those two columns (PR-D fix round 1). It fills
`mapped_instruments` / `unmapped_instruments` /
`covered_instrument_minutes` (the registry's PARTIAL measurement on
mixed days), writes the config-gated `feed_coverage_daily`
per-instrument rows, and stamps `coverage_source` = `in_memory` (full
session) / `mixed`. Fallback stays the SQL minute sets (`sql_backfill`), and the
coverage keep-better (`stage="coverage_regression"` — mirror of the lag
guard) stops a registry-less rerun/backfill from erasing a measured
day's registry columns. Unmapped singletons are counted + named in the
day's logs (bounded ≤20 sample); ticks folding for unregistered keys
and slot-cap overflow are counted, never silent (Rule 11). Honest
bounds: presence = ticks WE captured (not proof the exchange traded);
the registry costs a fixed 192 KiB (2,048 slots × 2 feeds × 48 B) and
one papaya read + one `fetch_or` per tick (DHAT
`dhat_feed_presence.rs`, Criterion budget `feed_presence_record`).
**Stall episode rows are LIVE since PR-B (2026-07-10):** the Groww
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
- `crates/core/src/pipeline/feed_presence.rs` (the presence registry) +
  `crates/core/src/instrument/presence_registration.rs` (Dhan slot build)
  + the Groww slot build in `crates/app/src/groww_activation.rs` (PR-D)

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
| Stall row, source `stall_entitlement` (a HARD Authorization/Permissions/entitlement line — fix round 1 2026-07-10: the sidecar's benign SILENT-FEED watchdog lines no longer produce this slug; off-hours they never latch, in-market they latch a weak value that keeps the kill arm's own slug) | broker | `entitlement_reject` |
| Stall row, UNKNOWN/drifted source slug (fix round 1: exact-match on the 4 lockstep slugs; a drifted slug is never silently broker-blamed) | indeterminate | `unclassified` (rule-9 floor; the `never_streamed_restart` KIND still attributes broker/`never_streamed`) |
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

## §2b. 2026-07-13 update — REST 1m pull digest rides the daily card (Groww REST plan PR-5)

The 15:45 aggregation gained an ADDITIVE step 6e (operator Quote 2,
2026-07-13: *"always clearly note within a second — or within how many
seconds precisely — we are fetching this live real OHLCV, along with the
option chain API"*): it reads the day's `rest_fetch_audit` per-fetch
forensics rows (+ a latency-only fallback from the `spot_1m_rest`
`close_to_data_ms` per-row column for feeds whose forensics emits have not
landed — the Dhan spot leg today) and renders one plain-English
"Official minute candles — how fast after each minute closed" line per
(feed, leg) on the SAME Telegram card (`RestLegScoreLine` on
`DualFeedDailyScorecard`). Semantics: prompt pulls (< 60s after close,
`REST_LEG_LATE_RECOVERY_MS`) feed the nearest-rank p50/p99/max
distribution; ≥ 60s repairs count "recovered late" (never skewing the
prompt numbers — the #1499 histogram split applied to the digest);
`named_gap` rows render "never recovered ⚠️"; 429s sum to "rate-limit
hits". The four canonical feed/leg pairs ALWAYS render — an absent source
reads "not measured yet", never a fabricated zero (Rule 11) — and the
PR-4 contract leg lights up automatically once its forensics rows land
(the aggregation is generic over the `leg` column). Nothing is persisted:
the aggregates are recomputed from the durable audit table on every run,
so backfills/reruns are idempotent by construction and no keep-better
guard is needed (the PR-C −1-sentinel convention applies at render time).
New SCOREBOARD-01 degrade stages (log-sink-only, additive — a failure
flags the card's honest footnote and NEVER touches the episode/coverage/
lag sections or the daily rows): `rest_leg_read` / `rest_leg_parse` /
`rest_leg_fallback_read` / `rest_leg_fallback_parse`. Dashboard gauge:
`tv_rest_leg_close_to_data_p99_ms{feed, leg}` — /metrics-local (NOT
CloudWatch-shipped; zero alarm/cost impact), same-day runs only, static
label values via a bounded allowlist. Honest bounds: a box whose
`rest_fetch_audit` / `spot_1m_rest` table does not exist yet reads the
read-failure footnote (the ensure-DDL belongs to the legs, not the
scorecard); the Dhan option-chain line stays "not measured yet" until the
Dhan forensics follow-up lands (its per-fire histogram is not
day-drainable). Ratchets:
`crates/app/tests/rest_leg_digest_wiring_guard.rs` (3 tests) + the
aggregation/render unit suites in `feed_scoreboard_boot.rs` / `events.rs`.

**2026-07-14 update — Dhan forensics rows are LIVE (GAP-11):** the Dhan
spot AND chain legs now emit `rest_fetch_audit` rows (one per (minute,
SID/underlying) — every verdict, backfill, sweep repair, named gap,
no-token and skipped boundary; `rest-1m-pipeline-error-codes.md` §2/§2d),
so the digest's PRIMARY source covers all four feed/leg pairs and the
Dhan option-chain line flips from "not measured yet" to measured with
zero scoreboard code change (the aggregation was always generic over
`feed`/`leg`). The `spot_1m_rest` latency-fallback column read is KEPT
as historical/defensive coverage (pre-2026-07-14 days + a
forensics-writer outage window); removal is a later cleanup. The rows
additionally carry the NEW `close_to_persist_ms` column (minute close →
data-flush-ACK, stamped post-ACK via the hold-then-stamp pattern; -1 =
not persisted/not measured) — not yet rendered on the card, available
for a future digest rung.

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `Scoreboard01*` variant)
- `crates/common/src/feed_blame.rs`
- `crates/storage/src/feed_scoreboard_persistence.rs`
- `crates/storage/src/feed_episode_audit_persistence.rs`
- `crates/app/src/feed_scoreboard_boot.rs`
- `crates/app/src/groww_sidecar_supervisor.rs` (the `stall_restarted` emit)
- `crates/core/src/pipeline/feed_presence.rs` (the presence registry)
- `crates/core/src/instrument/presence_registration.rs` (Dhan slot build)
- Any file containing `SCOREBOARD-01`, `Scoreboard01`, `feed_scoreboard_daily`,
  `feed_episode_audit`, `feed_coverage_daily`, `classify_episode`,
  `BlameClass`, `StallRestarted`, `STALL_SOURCE_`, `FeedPresenceRegistry`,
  `record_presence`, `PresenceRegistration`, `RestLegScoreLine`,
  `aggregate_rest_leg_day`, or `tv_rest_leg_close_to_data_p99_ms`
