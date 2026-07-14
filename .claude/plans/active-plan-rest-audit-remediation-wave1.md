# Implementation Plan: REST-audit remediation wave 1 — Dhan forensics, VIX page, sweep hardening, mid-session clock skew

**Status:** IN_PROGRESS
**Date:** 2026-07-14
**Approved by:** coordinator (relayed operator directive, 2026-07-14)

> **Source:** the 2026-07-14 per-minute-REST adversarial audit (60 scenario cells; final
> verdict + GAP register in the audit session's scratchpad `audit-final-verdict.md` §3).
> This ONE plan consolidates what was originally scoped as two plans (Plan A =
> GAP-11 + GAP-12; Plan B = GAP-07 + GAP-08 + GAP-13) because the repo already has
> **5 active plans** — the plan-gate V7 cap (`PLAN_GATE_MAX_ACTIVE = 5`,
> `design-first-wall.md` "2026-07-10 hardening") blocks implementation pushes when
> MORE than 5 active-plan files exist. **Even this single file makes 6 — before any
> implementation push, the coordinator must archive at least one completed active plan**
> (candidates: `active-plan-spot-1m-diagnostics.md` if its PR merged;
> `active-plan-questdb-partition-s3-archive.md` if the 2026-07-13 disk-retention PRs
> completed it). This plan file itself is docs-only and does not trip the gate.
>
> **Explicitly OUT of scope (tracked separately in the GAP register):** GAP-01/03/05/16/18
> (terraform-only alarm entries — not crates-gated, no plan needed per the gate's
> `crates/*/src` trigger), GAP-02/04/06/09 (token-lifecycle fixes — separate plan after
> operator ruling on re-mint semantics), GAP-27/28 (docs-only PRs).

## Crates touched

- **tickvault-app** (`crates/app`) — `spot_1m_rest_boot.rs`, `option_chain_1m_boot.rs`,
  `groww_spot_1m_boot.rs`, `infra.rs`, `main.rs`, `dhan_rest_stack.rs` (spawn seams)
- **tickvault-storage** (`crates/storage`) — `rest_fetch_audit_persistence.rs` (consts
  reused, no schema change), `spot_1m_rest_persistence.rs` (read-only reference)
- **tickvault-core** (`crates/core`) — `notification/events.rs` (2 new typed
  `NotificationEvent` variants)
- **tickvault-common** (`crates/common`) — `error_code.rs` (1 new `ErrorCode` variant for
  mid-session skew), `constants.rs` (second-sweep + skew-cadence constants)

## Plan Items

- [x] **Item 1 (GAP-11, priority 1) — Dhan spot + chain legs emit `rest_fetch_audit` rows.**
  DONE 2026-07-14 on branch `claude/rest-audit-gap11-dhan-forensics` (spot emits ee31b861;
  chain port + the close_to_persist_ms hold-then-stamp extension in the follow-on commits —
  see the delivered-tests note at the end of this item).
  Verified today: `grep rest_fetch_audit crates/app/src/spot_1m_rest_boot.rs
  option_chain_1m_boot.rs` = ZERO hits — the Dhan legs are forensics-silent while the
  Groww legs write one row per (minute, symbol, feed, leg). Port the Groww pattern
  (`groww_spot_1m_boot.rs`: `build_fetch_audit_row` :1396, `build_auth_short_circuit_rows`
  :1428, `audit_append_best_effort` :1461, `audit_flush_best_effort` :1477) into BOTH Dhan
  legs with `feed = SPOT_1M_REST_FEED_DHAN` (exists —
  `crates/storage/src/rest_fetch_audit_persistence.rs` uses the same
  `RestFetchAuditRow`/`RestFetchAuditWriter`; feed const at
  `crates/storage/src/spot_1m_rest_persistence.rs:49`), `leg = REST_FETCH_LEG_SPOT_1M`
  (:61) / `REST_FETCH_LEG_CHAIN_1M` (:66). This IS the flagged follow-up in
  `rest-1m-pipeline-error-codes.md` §2 ("the Dhan leg's emit sites are a fast FOLLOW-UP
  after #1499 merges").
  - Files: `crates/app/src/spot_1m_rest_boot.rs` (per-SID row at the `SidFetchOutcome`
    fold in the fire loop ~:1680-1740; sweep rows in `run_post_session_sweep` :1882;
    no-token short-circuit rows; writer threaded via `Spot1mRestTaskParams` :175 or
    constructed in `run_spot_1m_rest` :1353), `crates/app/src/option_chain_1m_boot.rs`
    (per-underlying row in `run_option_chain_1m` :1270 fire path, `attempts=1` — no
    re-poll ladder, live-snapshot semantics), `crates/storage/src/rest_fetch_audit_persistence.rs`
    (no schema change — DEDUP key already feed-keyed :58),
    `.claude/rules/project/rest-1m-pipeline-error-codes.md` (flip the follow-up note to
    done; audit-failure stages reuse SPOT1M-02/CHAIN-03 `audit_append`/`audit_flush` per
    the Groww precedent), `.claude/rules/project/dual-feed-scoreboard-error-codes.md`
    (§2b note only: the Dhan-spot `spot_1m_rest.close_to_data_ms` latency FALLBACK in
    `feed_scoreboard_boot.rs` becomes redundant once Dhan forensics rows exist — fallback
    code is KEPT this wave, removal is a later cleanup)
  - Tests (AS DELIVERED 2026-07-14 — names updated at tick time; the emit-site coverage
    is source-scan ratcheted, the pure hold/stamp/discard decision is unit-tested):
    `test_chain_audit_row_attempts_is_one_and_leg_chain_1m` (option_chain_1m_boot.rs),
    `test_close_to_persist_ms_for_math_and_clamp`,
    `test_stamp_held_ok_rows_flush_ok_stamps_each_row`,
    `test_stamp_held_ok_rows_flush_err_discards_all` (spot_1m_rest_boot.rs — the GAP-11
    persist-stamp extension: ok rows HELD until the data flush ACK, stamped with the real
    close_to_persist_ms, discarded on flush Err), ratchets in
    `crates/app/tests/spot_1m_rest_wiring_guard.rs`
    (`ratchet_dhan_spot_and_chain_legs_emit_rest_fetch_audit` — production-region
    source-scan of both Dhan legs' emit needles;
    `ratchet_ok_audit_rows_stamp_after_data_flush_ack` — per fire/sweep fn region, the
    stamp call must follow the data `writer.flush()` in source order + no hardcoded
    `close_to_persist_ms: -1` at the ok emit sites). Isolation is structural
    (`audit_append_best_effort` has no access to `persist_failed`/the tally/the edge —
    the behavioral pin `test_dhan_audit_failure_never_sets_persist_failed_or_feeds_edge`
    needs a fault-injectable writer and is deferred with the plan's remaining items)

- [ ] **Item 2 (GAP-12) — VIX failure reaches Telegram (Groww side ONLY).**
  Precise scope, verified: the DHAN side ALREADY pages — a persistently-unserved VIX
  trips the `sid_not_served` detector (`spot_1m_rest_boot.rs:768` threshold,
  `:2082 .notify(NotificationEvent::Spot1mSidNotServed …)`, Severity::High per
  `events.rs:4012`), so GAP-12 needs NO Dhan change. The GROWW side is `warn!`-only:
  the once-per-session `vix_not_served` sweep latch (`groww_spot_1m_boot.rs`
  `vix_not_served_verdict` :398-406, emit :2450-2467) and the `vix_unresolved`
  edge-latched warn have no typed event. Add ONE typed
  `NotificationEvent::GrowwSpot1mVixNotServed { … }`, Severity::High, emitted at the
  :2450-2467 sweep latch only (once per session by construction — the sweep runs once);
  the per-minute `vix_unresolved` arm stays warn!-only (a never-activated-lane day would
  otherwise page daily noise — named in Edge Cases). Body per the 10 Telegram
  commandments: plain English, e.g. "India VIX minute candles are not being served by
  Groww today — the other 3 indices are unaffected", no file paths, no library names.
  - Files: `crates/core/src/notification/events.rs` (variant + topic + severity + body;
    mirror the `Spot1mSidNotServed` wording style at :587/:2536),
    `crates/app/src/groww_spot_1m_boot.rs` (notify call at the :2450-2467 latch —
    a `NotificationService` handle must be threaded into `GrowwSpot1mTaskParams` /
    `run_groww_spot_1m` :1500 the same way the Dhan spot leg carries one),
    `.claude/rules/project/rest-1m-pipeline-error-codes.md` (§1 `feed="groww"`
    `stage="vix_not_served"` paragraph gains the typed-event note — delivery boundary
    §3 wording updated: VIX now pages via typed Telegram, still no CW alarm)
  - Tests: `test_vix_not_served_emits_typed_event_once_per_session`,
    `test_vix_unresolved_arm_never_notifies` (warn-only pin),
    events.rs `test_groww_vix_not_served_topic_severity_and_body_plain_english`
    (auto-driver litmus: no paths, no jargon)

- [ ] **Item 3 (GAP-07) — sweep repairs sub-watermark holes via a QuestDB-derived missing set.**
  Today `sweep_missing_minutes(last_persisted, first, last)`
  (`spot_1m_rest_boot.rs:527`; Groww mirror `groww_spot_1m_boot.rs:685`) selects ONLY
  `(watermark, session_last]` — a mid-session QuestDB outage or restart hole BELOW the
  in-memory watermark is never auto-repaired. Design: at sweep time, the missing set
  becomes the UNION of (a) the existing watermark-derived set (kept — covers the
  fresh-tracker/no-watermark arm) and (b) a per-SID bounded QuestDB `/exec` SELECT of
  the day's DISTINCT persisted minutes (`select ts from spot_1m_rest where
  trading_date_ist = … and security_id = … and feed = '<feed>' limit 400` — 375-minute
  session bound + margin), diffed against the session minute grid (pure fn
  `grid_diff_missing_minutes(persisted: &[i64], first, last) -> Vec<i64>`). SELECT
  failure/oversize/truncation degrades FAIL-SOFT to the watermark-only set + one coded
  `warn!` (SPOT1M-01 `stage="sweep_select_failed"`) — the sweep NEVER regresses below
  today's behaviour. Bounded: ≤4 SIDs (Dhan) / ≤4 targets (Groww) × 1 SELECT, response
  body capped (reuse the leg's 2 MiB cap helpers `declared_len_within_cap`/
  `accumulation_within_cap`). O(375) per SID, once per day — flagged O(N) honestly.
  - Files: `crates/app/src/spot_1m_rest_boot.rs` (`sweep_missing_minutes` union arm +
    `grid_diff_missing_minutes` + the SELECT helper in `run_post_session_sweep` :1882),
    `crates/app/src/groww_spot_1m_boot.rs` (mirror in `run_post_session_sweep` :2190),
    `.claude/rules/project/rest-1m-pipeline-error-codes.md` (§1 sweep paragraph +
    new `sweep_select_failed` stage)
  - Tests: `test_grid_diff_missing_minutes_holes_below_watermark`,
    `test_grid_diff_full_day_when_no_rows`, `test_grid_diff_noop_when_all_persisted`,
    `test_sweep_union_dedups_watermark_and_select_sets`,
    `test_sweep_select_failure_degrades_to_watermark_only` (fail-soft pin),
    proptest `prop_grid_diff_never_selects_persisted_minute`

- [ ] **Item 4 (GAP-08) — bounded SECOND sweep ~16:00 IST + typed HIGH `sweep_incomplete` page (both feeds).**
  Today both sweeps are one-shot with log-only failure (Dhan fires 15:33:30 —
  `SPOT_1M_REST_SWEEP_FIRE_SECS_OF_DAY_IST` :507 with const-asserts :513-518 pinning
  ≥ cross-verify trigger + 150s and < 16:30 stop; Groww fires 15:31 —
  `GROWW_SPOT_1M_SWEEP_FIRE_SECS_OF_DAY_IST` groww :158). A vendor serving LATE (the
  live 2026-07-14 Dhan 21/21-empty day) or a sweep-time token/transport failure loses
  the day. Design: (a) ONE bounded second pass at
  `SPOT_1M_REST_SECOND_SWEEP_FIRE_SECS_OF_DAY_IST = 16:00:00 IST` (shared instant, both
  feeds; const-asserts: > first sweep instants, < 16:30:00 box stop — the §7 schedule
  gives ≥30 min margin), running the SAME sweep body (DEDUP-idempotent re-appends;
  with Item 3, the second pass also sees any minutes the first pass persisted); it runs
  ONLY when the first pass ended `sweep_failed`/`sweep_incomplete` OR skipped (never a
  redundant clean re-run). (b) A typed `NotificationEvent::Spot1mSweepIncomplete
  { feed, swept, still_missing }`, Severity::High, emitted ONCE per day per feed when
  the FINAL pass still has missing minutes (edge — never per-pass double-page: the
  first pass's incomplete outcome is a coded ERROR only when a second pass is pending).
  Honest envelope: this does NOT heal a post-close dead token (GAP-09 — the mid-session
  watchdog is market-hours-gated; out of scope here, named) and does NOT add a next-day
  prior-day repair (manual re-run stays the floor for vendor lag > 16:00).
  - Files: `crates/common/src/constants.rs` (second-sweep const + const-asserts),
    `crates/app/src/spot_1m_rest_boot.rs` (scheduler arm after the first sweep in the
    `run_spot_1m_rest` :1353 day loop; outcome plumbing from `run_post_session_sweep`),
    `crates/app/src/groww_spot_1m_boot.rs` (mirror in `run_groww_spot_1m` :1500),
    `crates/core/src/notification/events.rs` (variant),
    `.claude/rules/project/rest-1m-pipeline-error-codes.md` (§1 SPOT1M-01 sweep
    stages + the typed-page delivery note; §3 delivery boundary updated)
  - Tests: `test_second_sweep_const_after_first_and_before_box_stop` (const pin),
    `test_second_sweep_runs_only_after_incomplete_or_failed_first`,
    `test_sweep_incomplete_event_fires_once_after_final_pass_only`,
    `test_second_sweep_clean_first_pass_is_noop`,
    events.rs `test_spot1m_sweep_incomplete_severity_high_and_body`

- [ ] **Item 5 (GAP-13) — mid-session clock-skew re-probe (visibility only, never HALT).**
  Today skew enforcement is boot-only: `enforce_clock_skew_at_boot`
  (`crates/app/src/infra.rs:549`, called `main.rs:6969`) around `probe_clock_skew`
  (`infra.rs:515`; chronyc primary, QuestDB `now()` fallback;
  `CLOCK_SKEW_HALT_THRESHOLD_SECS = 2.0` `constants.rs:3358`). Mid-session chrony
  death/drift is undetected — a 2-60s drift silently mis-targets minute fires and
  surfaces only as unattributed `empty_stale` storms. Design: a supervised process-global
  task (the disk_health_watcher/oom_monitor house family — supervised respawn +
  `classify_join_exit`) re-runs `probe_clock_skew` every
  `CLOCK_SKEW_RECHECK_INTERVAL_SECS = 900` (15 min), in-session gated
  (`[09:00, 16:00)` IST trading days — covers pre-open + the sweep window). Emissions:
  edge-latched per drift episode — `warn!` at >1.0s, `error!(code =
  ErrorCode::Skew01MidSessionDrift.code_str())` ("SKEW-01") at
  ≥ `CLOCK_SKEW_HALT_THRESHOLD_SECS` (2.0s) — NEVER a mid-session HALT (a HALT would
  drop the live Groww feed; visibility only, per the honest envelope). Probe failure
  (chronyc absent + QuestDB down) is a counted soft-skip, never a false skew signal.
  Counter `tv_clock_skew_recheck_total{outcome="ok"|"warn"|"exceeded"|"probe_failed"}`
  + gauge `tv_clock_skew_last_probe_secs`. New `ErrorCode::Skew01MidSessionDrift`
  (Severity::High, auto-triage-safe YES — inspection only) with its rule-file mention
  added to `wave-2-c-error-codes.md` (BOOT-03's home) to satisfy
  `error_code_rule_file_crossref.rs`. Delivery boundary stated honestly: SKEW-01 is
  log-sink-only at ship (no CW `error_code_alerts` entry — flagged follow-up, the
  GAP-03 terraform lane).
  - Files: `crates/app/src/infra.rs` (reuse `probe_clock_skew` :515; new
    `spawn_supervised_clock_skew_recheck` + pure `classify_skew_sample(skew, warn, err)
    -> SkewVerdict`), `crates/app/src/main.rs` (spawn in the process-global monitor
    block next to the disk/OOM/resource monitors — NOTE the documented sibling residual:
    that block sits after the fast crash-recovery arm's early return, GAP-20 class,
    accepted this wave), `crates/common/src/error_code.rs` (variant),
    `crates/common/src/constants.rs` (interval + warn-threshold consts + const-asserts:
    warn < halt threshold; interval ≥ 60),
    `.claude/rules/project/wave-2-c-error-codes.md` (SKEW-01 runbook section)
  - Tests: `test_classify_skew_sample_ok_warn_exceeded_boundaries` (±1.0/±2.0 edges,
    negative skew symmetric), `test_skew_recheck_edge_latch_once_per_episode`,
    `test_skew_recheck_probe_failure_is_soft_skip_not_signal`,
    `test_skew_recheck_gate_in_session_only`, error_code.rs catalogue tests auto-cover
    the new variant (unique code_str, runbook path exists, severity),
    wiring pin `test_clock_skew_recheck_is_wired_into_main` (secret_manager.rs
    source-scan family)

## Design

**Item 1 (Dhan forensics).** The `rest_fetch_audit` table + writer are already
feed-generic (`feed` in the DEDUP key `ts, trading_date_ist, feed, leg, security_id,
exchange_segment, outcome` — `rest_fetch_audit_persistence.rs:58`; leg consts for
spot/chain/contract :61-73; `RestFetchOutcome` wire strings stable). The Dhan legs gain
a per-leg `RestFetchAuditWriter` (lazy-ensure, ILP-over-HTTP — the writer's existing
`audit_ensure_*` stages) constructed inside the leg task (cold path). Emission points
mirror Groww exactly: one row per (target minute, SID) at the fire's outcome fold
(`ok`/`empty`/`error` from `SidFetchOutcome`, `no_token` short-circuit), plus sweep rows
(`ok` for swept repairs with the REAL ≥60s `close_to_data_ms`, `named_gap` class rows for
still-missing minutes, `no_token` when the sweep had no token). The chain leg writes one
row per (minute, underlying) with `attempts=1` and the underlying's SID/plain symbol
(the Groww chain precedent). Audit writes are BEST-EFFORT choke points
(`audit_append_best_effort` returns after `error!` + counter) — verified isolation
requirement: no audit failure path may touch `persist_failed`, the tally, or the
`FailureEdge` (the Groww G13 isolation property, re-pinned by a Dhan test). Scoreboard
step 6e consumes the new rows automatically (the aggregation is generic over
`feed`/`leg` — `dual-feed-scoreboard-error-codes.md` §2b: "the PR-4 contract leg …
feeds the SAME digest line automatically"); the Dhan chain line flips from "not measured
yet" to measured with zero scoreboard changes.

**Item 2 (VIX page).** One new typed event at the Groww sweep's once-per-session
`vix_not_served` latch. The latch condition (`vix_not_served_verdict` — zero persisted
VIX minutes while ≥1 core index persisted) already encodes "vendor not serving THIS
index", so the event is vendor-attributed and structurally once-per-session (no new
dedup state needed). Severity High per the coordinator's approval; recovery ping
deliberately NOT added (the next session's absence-of-page is the recovery signal —
the sweep verdict has no falling edge to hang an Info on; stated honestly).

**Item 3 (sub-watermark repair).** The union design keeps the watermark set as the
authoritative TAIL selector (it knows about staged-but-unflushed state) and adds the
DB-derived set as the HOLE detector. The SELECT uses the leg's existing long-lived
reqwest client against the QuestDB `/exec` HTTP endpoint (the tf_consistency/
cross_verify house pattern) with an explicit LIMIT and the leg's body-cap helpers; the
parse is a bounded ts-column extraction (defensive by column name). The pure
`grid_diff_missing_minutes` walks the 375-minute session grid against a sorted persisted
set — O(375 + rows), once per day per SID, flagged O(N).

**Item 4 (second sweep + page).** The sweep body is already idempotent
(DEDUP UPSERT; `outcome` in the audit key so first-pass rows survive). The second pass
re-invokes it with the SAME tracker + (Item 3) a fresh DB-derived missing set — so a
vendor that started serving between 15:33:30 and 16:00 (the D16 hypothesis class) is
repaired with zero manual action. Outcome plumbing: `run_post_session_sweep` returns a
`SweepOutcome { swept, still_missing, failed }` instead of logging terminally; the day
loop decides second-pass scheduling + the final typed page. The typed page replaces
nothing — the existing `sweep_failed`/`sweep_incomplete` coded ERRORs stay (forensic
WHY), the event is the operator signal (GAP-03's missing-CW-alarm caveat stated in the
rule file).

**Item 5 (skew re-probe).** Reuses the existing probe verbatim (chronyc `Last offset`
primary; QuestDB `now()` fallback — both already implemented + unit-tested for boot).
The recheck task is deliberately NOT wired into any fetch/fire/HALT decision — the
minute schedulers keep their existing staleness gates (`fire_is_fresh` 30s grace catches
big steps as `boundary_skipped` already); this task closes the 2-60s slow-drift
attribution hole ONLY. Edge-latch: one WARN/ERROR per drift episode (latch clears when a
probe reads back under the warn threshold).

## Edge Cases

| # | Case | Handling |
|---|---|---|
| 1 | Dhan audit writer ensure-DDL fails at leg start | best-effort: `audit_ensure_*` stages (SPOT1M-02/CHAIN-03 class), leg fires normally, forensics absent until next ensure — never blocks the fetch loop |
| 2 | Audit flush rejected mid-session (schema drift) | discard-pending on the audit writer; counted; fire verdict computed independently (isolation pin test) |
| 3 | VIX unresolved ALL day (Groww lane never activated) | Item 2 deliberately does NOT page this arm — `vix_unresolved` stays warn-only + counter; only the not-SERVED sweep verdict pages (avoids a daily false page on groww-lane-off boots) |
| 4 | VIX not served AND all 3 cores also failed | `vix_not_served_verdict` requires ≥1 core persisted — a global outage day does NOT fire the VIX page (the core escalation edge owns that page); pinned by the existing `test_vix_not_served_verdict_arms` |
| 5 | QuestDB down at sweep SELECT time | fail-soft to watermark-only set + `sweep_select_failed` warn — today's behaviour is the floor, never regressed |
| 6 | SELECT returns >LIMIT rows (impossible ≤375 by session math) | truncation detected (rows == LIMIT) → treat as SELECT failure → fail-soft (never trust a partial persisted set — would re-fetch already-persisted minutes; harmless via DEDUP but wasteful, and worse, could MISS holes) |
| 7 | Second sweep at 16:00 with token dead post-close | sweep's no-token arm fires `sweep_failed` + the final typed page carries the full missing count — GAP-09 residual NAMED, not fixed here |
| 8 | First sweep clean, vendor un-serves nothing later | second pass skipped (clean-first-pass no-op test) — no redundant 16:00 requests |
| 9 | Restart between the two sweeps | the day loop re-derives state; with Item 3 the DB-derived set makes the second pass correct even with a fresh in-memory tracker (`pre_boot`/blind-window semantics unchanged — §38 bulk-backfill ban honored: same-day only, never a past day) |
| 10 | Muhurat/holiday | both sweeps + the skew recheck sit behind the existing trading-day gates (`spot_1m_day_is_over` :269 / calendar) — correct silence |
| 11 | chronyc absent on the box (container/dev Mac) | QuestDB fallback probes; both failing = `probe_failed` soft-skip counter, no false signal |
| 12 | Clock STEP (>30s) mid-session | already caught loudly as `boundary_skipped` by the schedulers; the recheck adds attribution on the NEXT 15-min tick |
| 13 | Skew flapping around 2.0s | edge-latch per episode: one ERROR on crossing, re-arm only after a sub-warn-threshold probe — no 15-min page storm |
| 14 | 429 during the 16:00 second sweep | inside the Data-API budget (≤4 requests + the existing sweep pacing); the 15:31-15:33 cross-verify burst window is long past (const-asserted ordering) |

## Failure Modes

| Failure | Blast radius | Detection | Recovery |
|---|---|---|---|
| Audit writer wedged (ILP-HTTP down all day) | forensics rows absent for the window; fetch/persist/paging UNAFFECTED (isolation pin) | `tv_rest_fetch_audit_persist_errors_total{stage}` + coded SPOT1M-02/CHAIN-03 logs | next append/flush retries; rows for past minutes are lost (forensics-only loss, honest) |
| VIX event notify fails (Telegram down) | one page lost; logs + counters intact | TELEGRAM-01 drop counter (existing) | none same-day (GAP-05 residual, out of scope, named) |
| Sweep SELECT parses garbage | fail-soft to watermark set | `sweep_select_failed` warn + counter | Item 3 degrades to today's exact behaviour |
| Second sweep task dies | day's second pass lost | supervisor respawn counter + `task_respawn` stage (existing family) | next trading day; manual re-run floor unchanged |
| Skew recheck task dies | drift visibility lost until respawn | `tv_clock_skew_recheck_total` stops moving + supervisor respawn counter | supervised respawn (5s backoff); release-panic aborts process (documented family envelope — systemd restarts) |
| False skew from a QuestDB-fallback probe against a skewed DB host | mis-attribution risk | chronyc primary wins when present; fallback sample source named in the log line | operator triage per the SKEW-01 runbook section |

## Test Plan

Scoped per `testing-scope.md`: changes touch `crates/app`, `crates/core`,
`crates/storage`, `crates/common` — `crates/common` (error_code + constants) escalates to
`cargo test --workspace`. Categories declared: unit (all pure fns above), proptest
(`prop_grid_diff_never_selects_persisted_minute`), source-scan ratchets (audit-emit
wiring guard, main.rs skew wiring pin), const-asserts (second-sweep ordering, skew
thresholds), events catalogue tests (topic/severity/body), error_code catalogue tests
(auto-cover the new variant: unique code_str, runbook path on disk via
`error_code_rule_file_crossref`), isolation pins (audit-failure-never-edges;
vix-unresolved-never-notifies). No hot-path change → no DHAT/Criterion delta claimed
(N/A, flagged honestly). Full per-item test names are listed under each Plan Item.
Pre-PR: banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + 3-agent adversarial
review before AND after implementation.

## Rollback

- Items 1/2/4 are additive emit sites + typed events: revert = `git revert` of the PR;
  no schema change anywhere (the audit table is untouched; DEDUP keys unchanged), so
  rollback leaves no data debt. Rows already written remain valid forensics.
- Item 3: the union arm is a superset selector — rollback restores watermark-only
  sweeps; rows repaired while live remain (DEDUP-idempotent, harmless).
- Item 4: deleting the second-pass arm restores one-shot semantics; the typed event
  variant can remain unused (events are not load-bearing) or be reverted with the PR.
- Item 5: the recheck is a standalone supervised task with zero consumers — abort-safe
  revert; the ErrorCode variant reverts with its rule-file section in the same PR
  (cross-ref test keeps both directions consistent).
- No config-default changes ship in this plan (no `enabled` flips), so no config
  rollback path is needed.

## Observability

| Surface | Addition |
|---|---|
| QuestDB | `rest_fetch_audit` gains `feed='dhan'` rows for `spot_1m`/`chain_1m` legs (existing table, existing DEDUP) |
| Telegram | `GrowwSpot1mVixNotServed` (High, once/session), `Spot1mSweepIncomplete { feed }` (High, once/day/feed, final-pass edge) |
| Counters | `tv_clock_skew_recheck_total{outcome}`, `tv_spot1m_sweep_select_failed_total{feed}` (or stage-labelled reuse), second-sweep swept/still-missing reuse the existing `*_sweep_backfilled_total`/`sweep_still_missing_total` families |
| Gauges | `tv_clock_skew_last_probe_secs` |
| Coded logs | SPOT1M-01 new stages `sweep_select_failed`, `second_sweep`; SKEW-01 (`Skew01MidSessionDrift`) WARN/ERROR edge-latched |
| Scoreboard | Dhan spot/chain rest-leg digest lines flip from fallback/"not measured yet" to measured (zero scoreboard code change) |
| Delivery boundary (honest) | SPOT1M/CHAIN/SKEW-01 remain log-sink-only on CloudWatch (no `error_code_alerts` entries — GAP-03 terraform lane, separate); the typed Telegram events are the operator pages |

## Per-Item Guarantee Matrix

The canonical 15-row + 7-row matrices from `per-wave-guarantee-matrix.md` apply to EVERY
item; project-specific fills:

### 15-row 100% Guarantee Matrix

| Demand | This plan's proof artefact |
|---|---|
| 100% code coverage | ratcheted per-crate floors (`quality/crate-coverage-thresholds.toml`); every item ships tests with its module |
| 100% audit coverage | Item 1 CLOSES an audit-coverage hole (Dhan forensics rows into the DEDUP-keyed `rest_fetch_audit`); Items 3/4 add named-gap/swept rows for repaired minutes |
| 100% testing coverage | unit + proptest + ratchet + const-assert + events/error-code catalogue tests (declared per item) |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan gates on every push |
| 100% code performance | cold-path only — NO hot-path change; no DHAT/Criterion delta claimed (N/A — flagged honestly) |
| 100% monitoring | counters + gauge per the Observability table |
| 100% logging | every new failure path `error!`/`warn!` with `code = ErrorCode::X.code_str()` + stage field (tag-guard enforced) |
| 100% alerting | typed HIGH Telegram at the VIX latch + final-sweep edge; SKEW-01 coded ERROR; CW-alarm absence stated honestly (GAP-03 lane) |
| 100% security | no new secret surface; SELECT bodies capped + column-name parsed; audit rows carry no token/URL (existing redaction contract) |
| 100% security hardening | body caps + LIMIT on the new SELECT; no new endpoints, no new REST hosts (§8/§9 scope untouched) |
| 100% bugs fixing | adversarial 3-agent review before + after each code PR |
| 100% scenarios covering | Edge Cases (14) + Failure Modes (6) enumerate the classes; Scenarios table below |
| 100% functionalities covering | every new pub fn has a call site + test (gates 6+11) |
| 100% code review | 3-agent pass on each PR diff, before AND after impl |
| 100% extreme check | ratchets: audit-emit wiring guard, isolation pins, const-asserts, skew wiring pin — all build-failing |

### 7-row Resilience Demand Matrix

| Demand | This plan's honest envelope |
|---|---|
| Zero ticks lost | UNTOUCHED — no `ticks`/`candles_*` write path modified; WAL/ring/spill/DLQ unchanged |
| WS never disconnects | UNTOUCHED — no WS code; SKEW-01 explicitly never HALTs (a HALT would drop the live feed) |
| Never slow/locked/hanged | cold-path only; new SELECT bounded (LIMIT + body cap + timeout); second sweep once/day |
| QuestDB never fails | sweep SELECT fail-soft to today's behaviour; audit writes best-effort with discard-pending; DEDUP-idempotent re-appends |
| O(1) latency | N/A hot path; new work is O(375)/SID once daily — flagged O(N) honestly, never claimed O(1) |
| Uniqueness + dedup | reuses the feed-in-key `rest_fetch_audit` DEDUP (`outcome` in-key so transition rows survive); no key change |
| Real-time proof | Dhan forensics rows carry measured `close_to_data_ms`; skew gauge + recheck counter are live measured signals — never asserted |

Honest claim wording: any "100% guarantee" in these PRs is qualified "100% inside the
tested envelope, with ratcheted regression coverage" per operator-charter §F.

## Scenarios

| # | Scenario | Expected |
|---|---|---|
| 1 | Normal Dhan minute fire, candle served | one `rest_fetch_audit` row `feed='dhan' leg='spot_1m' outcome='ok'` with measured latency; scoreboard digest line measured |
| 2 | Dhan chain minute, 200 OK | one row per underlying, `leg='chain_1m'`, `attempts=1` |
| 3 | Dhan no-token minute | `no_token` rows for all 4 SIDs; edge counts as today; forensics name the auth class |
| 4 | Groww VIX never served, cores healthy | ONE High Telegram "India VIX … not served — other indices unaffected" at the 15:31 sweep; cores never affected |
| 5 | Groww lane never activated (VIX unresolved) | warn + counter only — NO page (Edge Case 3) |
| 6 | QuestDB down 11:00-11:20, minutes lost below watermark | 15:33:30 sweep's DB-derived set finds + repairs the 20-minute hole (Item 3) |
| 7 | Dhan serves the whole day late (2026-07-14 class), starts serving 15:45 | first sweep incomplete (coded ERROR, no page yet); 16:00 second sweep repairs the day; NO incomplete page |
| 8 | Vendor never serves before 16:00 | second sweep still incomplete → ONE `Spot1mSweepIncomplete` HIGH page naming the missing count; manual re-run floor documented |
| 9 | chrony dies 12:00, drift reaches 3s by 13:00 | SKEW-01 ERROR (edge-latched) within ≤15 min of crossing 2.0s; `empty_stale` storms now attributed |
| 10 | Skew probe both-source failure | `probe_failed` counter, no signal, no page — monitoring degradation visible, never false-OK |
