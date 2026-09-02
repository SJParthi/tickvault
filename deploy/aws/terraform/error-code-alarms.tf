# =============================================================================
# Error-code log-filter alarms — the error! -> page route, RESTORED 2026-07-06
# =============================================================================
# THE GAP THIS CLOSES (zero-page incident, 2026-07-06):
#   The CloudWatch-only migration (#O1/#O2/#O3) retired the
#   Loki -> Alertmanager -> Telegram route with NO replacement, so an `error!`
#   reached only the log sinks. On 2026-07-06 the 12:00 IST REST-CANARY-01
#   probe failure produced ZERO pages. These 11 log metric filters + alarms
#   (8 on 2026-07-06; +AGGREGATOR-DROP-01 on 2026-07-09; +WAL-SUSPEND-01 on
#   2026-07-10; -REST-CANARY-01 retired + CROSS-VERIFY-1M-01/-02 +
#   TICK-CONSERVE-01 added, then the two CROSS-VERIFY-1M entries RETIRED
#   the same day by PR-C3 with their emit module, + the 5 REST-audit
#   entries, all on 2026-07-14; -WS-GAP-07 on 2026-07-13 (PR-C2, #1522);
#   -FEED-STALL-01 on 2026-07-15 (#1581); -WS-REINJECT-01 on 2026-07-17;
#   -TICK-CONSERVE-01 on 2026-07-18) on
#   the /tickvault/<env>/app log group (the errors.jsonl stream) restore the
#   route: error! -> errors.jsonl -> CloudWatch Logs -> filter -> tv_errcode_*
#   metric -> alarm (<=5 min) -> SNS tv-alerts -> Telegram webhook Lambda.
#
# HONEST ALARM COUNT: this file takes the REAL total from 33 -> 41 alarms
# (45 with the reconnect-storm + feed-stall-restarts [pager retired
# 2026-07-15 with the Groww live feed] + readiness-lambda-errors
# + market-hours-gate-errors alarms landing in the same PR). Overage above the
# 10 free-tier alarms moves $2.30 -> $3.50/mo. The rule-file "10 alarms free
# tier" claims were already stale pre-PR. (2026-07-09: +2 more alarms —
# errcode-aggregator-drop-01 here + seal-writer-dropped in seal-drop-alarm.tf,
# ~+$0.20/mo.) (+3 (order-side-alarms.tf, 2026-07-14): orders-placed-storm +
# daily-loss-breach + order-fill-lag-high (disarmed), ~+$0.30/mo — see
# aws-budget.md COST NOTE 2026-07-14.)
#
# DIMENSIONLESS BY DESIGN: errors.jsonl events carry NO `host` field (the host
# label is added by the Prometheus scrape, not the tracing layer), and metric
# filter transformations can only EXTRACT dimensions from event JSON — they
# cannot emit constant dimensions. So the tv_errcode_* metrics and their
# alarms are dimensionless; the filter and alarm match each other exactly.
#
# NO default_value ANYWHERE: a default_value emits a datapoint for every
# NON-matching event in the group, making every metric always-billed. Sparse
# metrics (billed only in hours a code fires) + treat_missing_data=notBreaching
# is correct and near-free.
#
# ADDING A FUTURE PAGED CODE = ONE map entry below (filter + alarm generated).
#
# 2026-07-09 UPDATE: +1 entry (AGGREGATOR-DROP-01) -> 9 filters + 9 alarms.
# The 2026-07-09 audit confirmed the Severity::Critical sealed-candle-drop
# code (the ONLY silent-data-loss path for sealed candles) paged nobody.
# Companion counter-side pager: seal-drop-alarm.tf.
#
# 2026-07-10 UPDATE (W2 PR#6): +1 entry (WAL-SUSPEND-01) -> 10 filters +
# 10 alarms (~+$0.10/mo). Audit follow-up row 10: a WAL-suspended QuestDB
# table silently stopped applying ILP-ACKed writes with zero signal — the
# new 60s wal_tables() probe pages it here.
#
# 2026-07-14 UPDATE (operator Dhan noise lock): -1 entry (REST-CANARY-01
# retired with the canary module) -> 9 filters + 9 alarms (~-$0.10/mo).
#
# 2026-07-14 UPDATE (automation-gaps PR-3): +3 entries (CROSS-VERIFY-1M-01,
# CROSS-VERIFY-1M-02, TICK-CONSERVE-01) -> 12 filters + 12 alarms
# (~+$0.30/mo). The 2026-07-10 automation audit found all three High
# post-market audit codes emitted error! but were log-sink-only — a 15:31
# IST OHLCV mismatch / degraded cross-verify run and a 15:40 IST
# tick-conservation residual paged NOBODY.
#
# 2026-07-14 UPDATE (PR-C3 — Dhan instrument-chain deletion, operator
# retirement directive 2026-07-13): -2 entries (CROSS-VERIFY-1M-01 +
# CROSS-VERIFY-1M-02, ~-$0.20/mo) -> the two same-day automation-gaps
# entries retired WITH their emit module `cross_verify_1m_boot.rs` (the
# 15:31 IST Dhan live-vs-historical cross-verify has no live side to
# compare — cross-verify-1m-error-codes.md retirement banner). Final
# same-day total: 15 filters + 15 alarms. TICK-CONSERVE-01 stays (the
# 15:40 conservation audit survives).
#
# 2026-07-17 UPDATE (dead live-WS sweep stage 1): -1 entry (WS-REINJECT-01,
# ~-$0.10/mo) -> its ONLY emit site (crates/app/src/wal_reinject.rs,
# retained un-consumed since PR-C2 "pending the Phase C module cleanup")
# was deleted in that cleanup — a filter with no possible emit site is a
# dead filter per the paging drift guard. New total: 14 filters + 14
# alarms. FEED-STALL-01's earlier retirement pattern followed.
#
# 2026-07-18 UPDATE (tick-conservation retirement — dead-WS sweep
# follow-up): -1 entry (TICK-CONSERVE-01, ~-$0.10/mo) -> its ONLY emit
# site (crates/app/src/tick_conservation_boot.rs, the 15:40 IST
# reconciler's Leak arm) was deleted with the audit modules — every audit
# input died with the dead tick chain in the stage-2 sweep #1631 (no live
# WAL frame writer, no processor outcome counters, nothing writes
# `ticks`), so every run could only record `partial` and the filter could
# never match again (a filter with no possible emit site is a dead filter
# per the paging drift guard — the ws-reinject-01 / feed-stall-01
# precedent). The `tick_conservation_audit` QuestDB TABLE is retained
# (SEBI 5y, never dropped). New total: 11 filters + 11 alarms —
# mechanically RECOUNTED against the live error_code_alerts map at the
# 2026-07-18 merge of origin/main: the prior running-total chain had
# drifted +2 because the -WS-GAP-07 (PR-C2 #1522, 2026-07-13) and
# -FEED-STALL-01 (#1581, 2026-07-15) retirements decremented the map
# without updating this chain (PR-C3's "15" and ws-reinject's "14"
# inherited the drift; actual counts were 14 and 12).
#
# 2026-07-14 UPDATE (REST-pipeline adversarial audit, GAP-01 + GAP-03 —
# docs/audits/2026-07-14-rest-pipeline-adversarial-audit.md): +5 entries ->
# 17 filters + 17 alarms (~+$0.50/mo; on top of the same-day REST-CANARY-01
# retirement and the automation-gaps +3 above; 15 + 15 after the same-day
# PR-C3 cross-verify retirement). The audit's single biggest systemic
# weakness: REST-leg paging was app-emitted Telegram ONLY — a dead app
# notifier (or Telegram bot) silenced AUTH-GAP-05 + SPOT1M/CHAIN entirely.
# SCOPED sub-filters (a 2026-07-14 extension of the pinned coded shape —
# error_code_paging_filter_drift_guard.rs accepts one extra $.field clause):
#   - auth-gap-05-remint-failed matches ONLY the mint-FAILURE arm (the
#     $.cooldown_skip bool field exists only on that emission; IS FALSE
#     additionally excludes the same-day noise-lock H3 mint-cooldown-skip
#     lines, which are non-terminal) — the trigger arm fires on every
#     forced re-mint INCLUDING successful self-heals, and the operator
#     ruled those pages noise ("silent-when-healing,
#     loud-only-when-unobtainable").
#   - spot1m-01-escalation / chain-02-escalation match ONLY the
#     once-per-episode stage="escalation" edge lines — the per-minute
#     stage="minute_failed" lines are sub-edge by design (the 3-minute
#     escalation edge is the page; a plain code filter would over-page
#     every failed minute).
#   - chain-04-warmup matches ONLY the down-for-the-day stage="warmup"
#     arm — the probe_* / warmup_no_token stages are log-only-by-design
#     respawn-retry arms (rest-1m-pipeline-error-codes.md §2e).
#   - chain-01 is a plain coded filter (both its stages — warmup +
#     mid_session — are once-per-episode page-worthy).
#
# 2026-09-02 UPDATE (second-sweep finding 5 — dhan-rest-only-noise-lock
# section 2.3p): +1 entry (RESOURCE-02, ~+$0.10/mo). The process's own memory
# early warning was log-sink-only, and until the same change it could not
# even reach its threshold: the systemd unit set no memory directive, the
# cgroup reported `max`, and the resolver fell back to MemTotal (~31 GiB), so
# its 80% line sat above anything the kernel would tolerate. The unit now
# sets MemoryHigh=15G (a throttle, never a kill) and OOMScoreAdjust=-900,
# so the page and the throttle share one real ceiling. Coded filter, eval
# 3 / dta 1, ok_recovery = true: a repeat-emitter whose OK genuinely tracks
# the RSS falling back under the line.
# =============================================================================

locals {
  # eval/dta 3/1: identical first-page latency to 1/1, but holds ALARM across
  # <=15-min repeat gaps (2026-07-06 DH-901 shape: 2 messages per episode --
  # one ALARM + one OK -- instead of ~32 flapping pairs).
  #
  # ok_recovery (round-1 review fix, 2026-07-06; widened round-4): for
  # repeat-emitters (DH-901 every 15 min, WS-GAP-07 storms, the FEED-STALL-01
  # storm detector) the eval-3/dta-1 OK transition genuinely tracks recovery
  # (the code stopped firing) -> ok_actions ON. For ONE-SHOT / DISCRETE
  # emitters the same sparse-metric + notBreaching mechanics AUTO-transition
  # to OK ~15 min after the single datapoint ages out of the lookback -- and
  # the telegram-webhook Lambda forwards OK states as a green "recovered"
  # page -- while the underlying condition still persists (a Rule-11
  # false-recovery). ok_recovery = false suppresses that misleading OK for:
  #   (rest-canary-01 was in this list until its 2026-07-14 retirement
  #   with the canary module - operator Dhan noise lock.)
  #   (ws-reinject-01 was in this list until its 2026-07-17 retirement
  #   with the wal_reinject module - dead live-WS sweep stage 1.)
  #   - proc-01: a discrete kernel OOM-kill event; the memory pressure that
  #     caused it is not fixed by the episode aging out.
  #   - dh-906: a discrete per-order reject; OK = aged out, never "orders
  #     working again".
  #   (tick-conserve-01 was in this list from 2026-07-14 until its
  #   2026-07-18 retirement with the tick-conservation audit modules —
  #   dead-WS sweep follow-up; see the header note. Its two same-day
  #   siblings cross-verify-1m-01/-02 had already retired in PR-C3.)
  # auth-gap-04 stays ok_recovery = true with a stated ambiguity (round-4):
  # its emit site returns Err from the boot mint path, systemd Restart=always
  # re-boots and re-emits roughly every failing boot cycle (each cycle spans
  # TOTP_MAX_RETRIES x 30s windows) -- a repeat-emitter whose OK ~= "stopped
  # firing" (secret reconciled, or the unit stopped). Caveat: if systemd's
  # StartLimitBurst (8/600s) ever halts the restart loop while the secret is
  # still wrong, emissions stop and the OK would be an aged-out false
  # recovery -- borderline, kept ON with this stated residual.
  # rest-canary-01 entry RETIRED 2026-07-14 with the REST canary module
  # (operator Dhan noise lock - dhan-rest-only-noise-lock-2026-07-14.md):
  # the retained spot-1m + option-chain legs self-detect a dead Dhan REST
  # surface within ~3-4 min via their own escalation edges.
  error_code_alerts = {
    "dh-901" = {
      pattern     = "{ $.code = \"DH-901\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = true
      desc        = "DH-901: Dhan auth failing - token invalid/expired or profile checks failing. Check tv_token_remaining_seconds + SSM TOTP secret. Runbook: .claude/rules/dhan/annexure-enums.md rule 11 + wave-4-error-codes.md"
    }
    # DH-906 is a plain TERM filter, not a coded JSON filter: zero coded emit
    # sites exist in the codebase (verified 2026-07-06 - tests, one doc
    # comment, one cfg(test) counter only). At runtime the literal arrives
    # only inside Dhan's response text via OmsError free text, in an unknown
    # field at an unknown level - the delimiter-based term filter matches both
    # streams at all levels. Honest boundary: an UNLOGGED reject is invisible;
    # dormant while dry_run=true. Flagged follow-up (NOT this PR): a 3-line
    # error!(code = ErrorCode::Dh906OrderError.code_str(), ...) at the
    # OmsError classification site converts this to a coded filter.
    "dh-906" = {
      pattern     = "\"DH-906\""
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # round-4: discrete per-order reject - auto-OK ~15 min later means the episode aged out, never "orders working again" (Rule-11 false-recovery)
      desc        = "DH-906: Dhan order error - NEVER auto-retry; fix the order. NO recovered/OK page: a reject is a discrete event, so the auto-OK ~15 min later only means the episode aged out of the lookback. NOTE: pre-armed tripwire - no coded emit site exists and dry_run=true means no live orders today; the literal arrives inside Dhan's response text via OmsError. Runbook: .claude/rules/dhan/annexure-enums.md rule 11"
    }
    "auth-gap-04" = {
      pattern     = "{ $.code = \"AUTH-GAP-04\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = true # round-4 documented ambiguity: repeat-emits per failing boot cycle under systemd Restart=always, so OK ~= stopped firing; if StartLimitBurst (8/600s) halts the loop, the OK would be aged-out - stated residual (see locals header)
      desc        = "AUTH-GAP-04: TOTP secret likely rotated externally - auth is DEAD until the SSM totp-secret is reconciled with dhan.co. CAVEAT on the recovered/OK page: it is trustworthy while the systemd restart loop keeps re-emitting; if systemd's StartLimitBurst halted the unit, the OK only means emissions stopped - verify the app is actually up before treating it as recovery. Runbook: .claude/rules/project/wave-4-error-codes.md"
    }
    # RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion): the
    # "ws-gap-07" entry — its ONLY error!-level emit site (the main-feed
    # frame-channel Closed arm in crates/core/src/websocket/connection.rs)
    # was deleted with the lane, so the filter could never match again
    # (dead paging filter). The WsGap07 variant retirement is Phase C
    # variant cleanup.
    # RETIRED (2026-07-15 — Groww live-feed retirement): the "feed-stall-01"
    # entry — its ONLY ERROR-level emit site (the sidecar stall watchdog's
    # storm escalation in the deleted groww_sidecar_supervisor.rs) died with
    # the Groww live feed, so the filter could never match again (dead
    # paging filter; the ws-gap-07 precedent above). The companion
    # >=3-restarts-per-15-min counter pager was deleted whole in the same PR
    # (feed-stall-restart-alarm.tf). Variant retirement is the post-C4 sweep.
    # RETIRED (2026-07-17 — dead live-WS sweep stage 1): the "ws-reinject-01"
    # entry — its ONLY emit site (crates/app/src/wal_reinject.rs, retained
    # un-consumed since PR-C2 "pending the Phase C module cleanup") was
    # deleted in that cleanup, so the filter could never match again (dead
    # paging filter; the ws-gap-07 / feed-stall-01 precedent above). The
    # WsReinject01Aborted variant retirement is the post-sibling-merge
    # variant sweep.
    "proc-01" = {
      pattern     = "{ $.code = \"PROC-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # round-4: discrete OOM-kill event - auto-OK means the episode aged out, not that the memory pressure is resolved (Rule-11 false-recovery)
      desc        = "PROC-01: kernel OOM kill detected in this cgroup (Severity Critical). NO recovered/OK page: an OOM kill is a discrete event, so the auto-OK ~15 min later only means the episode aged out - the leak/pressure behind it is not thereby fixed; watch tv_process_rss_bytes + host memory alarms for the real recovery. Runbook: .claude/rules/project/wave-4-error-codes.md"
    }
    # ADDED 2026-09-02 (second-sweep finding 5; noise-lock section 2.3p).
    # RESOURCE-02 is the process's OWN memory early warning — the one signal
    # meant to fire BEFORE the OOM killer — and it was log-sink-only. Worse,
    # it was unreachable: the systemd unit set no memory directive, so the
    # cgroup reported `max`, the resolver fell back to MemTotal (~31 GiB), and
    # 80% of the whole machine is a line the kernel acts before. The unit now
    # carries MemoryHigh=15G (a THROTTLE — deliberately no MemoryMax, which
    # would turn a spike into a kill of the only tick-capture process) and
    # OOMScoreAdjust=-900 (killed AFTER QuestDB, whose loss the spill tier
    # absorbs). This alarm is the page that pairs with that throttle.
    #
    # Two emit sites share the code: resource_monitor.rs (the RSS-vs-ceiling
    # arm, the intended one) and subsystem_memory.rs (the sampler-died
    # respawn arm). Both are genuine memory-observability failures and both
    # warrant the same triage, so a plain coded filter is correct here.
    #
    # ok_recovery = true: the monitor re-evaluates every cycle and re-emits
    # while RSS stays over the line, so an OK genuinely means the RSS fell
    # back under it (or the operator restarted the app) — the DH-901 shape,
    # not the discrete-event proc-01 shape.
    "resource-02" = {
      pattern     = "{ $.code = \"RESOURCE-02\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = true
      desc        = "RESOURCE-02: the trading app's resident memory is at or above 80% of its ceiling (MemoryHigh=15G on the systemd unit). Past the ceiling the kernel THROTTLES this process - it is not killed (no MemoryMax by design) - but a throttled tick decoder falls behind the socket and the vendor drops ticks upstream. Triage NOW: tv_process_rss_bytes + the tv_subsystem_memory_bytes components (which one is growing?); tv_spill_dir_free_bytes + tv_questdb_wal_suspended_tables (a stalled database backs up every writer queue). If a queue grows without bound, restart the app in the next quiet window; under host exhaustion QuestDB (-500) is killed before this process (-900) and the spill tier absorbs that. OK = RSS fell back under the line. Runbook: docs/error-runbooks/wave-4-error-codes.md + dhan-rest-only-noise-lock-2026-07-14.md section 2.3p"
    }
    # AGGREGATOR-DROP-01 (added 2026-07-09 — audit finding): the ONLY
    # silent-data-loss path for a sealed candle (ring + spill + DLQ all
    # failed), Severity::Critical, previously paged NOBODY. Emit site:
    # crates/storage/src/seal_writer_loop.rs::record_cycle_observability —
    # error!(code = ErrorCode::AggregatorDrop01.code_str(), dropped = N)
    # fires once per drain cycle with a non-zero truly-dropped count, so a
    # persistent catastrophic host state repeat-emits per cycle and
    # eval-3/dta-1 holds ALARM across <=15-min gaps. ok_recovery = false:
    # a drop is a discrete PERMANENT data-loss event (the dropped seals are
    # gone from the durable chain) — the auto-OK ~15 min after the episode
    # ages out can never mean "the candles came back" (Rule-11
    # false-recovery; the PROC-01 precedent). The counter-side pager on
    # tv_seal_writer_drain_total{kind="dropped"} lives in seal-drop-alarm.tf.
    #
    # ⚠ DESCRIPTION CORRECTED 2026-08-25 — the code has THREE emit sites, not
    # one, and the old description named only the first. Two of them are the
    # sealed-candle family this entry was written for
    # (seal_writer_loop.rs::record_cycle_observability, the consumer side; and
    # seal_loss_alarm.rs::record_lost_seal, the producer side added 2026-08-19
    # for the case where the writer never spawned and the drain counter reads a
    # flat healthy zero). The THIRD is a different failure entirely:
    # dhan_feed_stack.rs's 30-second silence-timer arm reports the per-window
    # DELTAS of aggregator TICK REFUSALS under the same code. An operator paged
    # by that arm was being handed a runbook about ring/spill/DLQ and disk
    # space, for an incident about packet sanity checks and instrument-slot
    # exhaustion. The threshold, period and eval are deliberately UNCHANGED —
    # only the text an operator reads at 2am.
    #
    # That arm also EXCLUDES the by-design `out_of_session` refusal reason at
    # the emit site, which is why the raw counter
    # tv_dhan_feed_ingest_refused_total is deliberately NOT alarmed on the
    # metric side: EMF folds its `reason` label by summing, so a metric alarm
    # would page on normal pre-open traffic. Recorded in full in
    # loss-and-retention-alarms.tf.
    "aggregator-drop-01" = {
      pattern     = "{ $.code = \"AGGREGATOR-DROP-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-09: discrete permanent data loss - the dropped sealed candles do not come back when the episode ages out (Rule-11 false-recovery; PROC-01 precedent)
      desc        = "AGGREGATOR-DROP-01 fires from TWO places - read the fields to tell them apart. (a) SEALED CANDLES DROPPED (fields security_id/timeframe/cause): ring + spill + DLQ all failed, or no durable tier was installed at all; the log is throttled to powers of two but the FIRST loss always logs. Triage: host state, df -h /data, ls -la data/spill/ data/dlq/. (b) TICKS REFUSED BY THE AGGREGATOR (fields refused_price/refused_timestamp/refused_slot_exhausted), reported every 30s by the live drain: never folded into a candle, never written. Price/timestamp = the packet failed a sanity check; slot_exhausted = instrument capacity is full and NEW instruments are turned away. Both are permanent loss: NO recovered/OK page. Counter-side pager: tv-<env>-seal-writer-dropped. Runbook: .claude/rules/project/wave-6-error-codes.md"
    }
    # WAL-SUSPEND-01 (added 2026-07-10, W2 PR#6 — audit follow-up row 10):
    # a QuestDB table's WAL apply is SUSPENDED (post disk-full / apply
    # error) — ILP keeps ACKing rows into the table's WAL while they
    # silently stop becoming visible/applied. Emit site:
    # crates/storage/src/wal_suspension_watcher.rs::emit_wal_delta —
    # error!(code = ErrorCode::WalSuspend01TableSuspended.code_str(),
    # table = ...) fires ONCE per (table, suspension episode) on the
    # rising edge of the 60s wal_tables() probe (Rule-4 edge latch; a
    # merely-DOWN QuestDB never fires it — BOOT-01/02 own that page).
    # ok_recovery = false: once-per-episode emitter (the ws-reinject-01
    # precedent) — the auto-OK ~15 min after the single datapoint ages
    # out would be a Rule-11 false recovery while the table is still
    # suspended; the real recovery signals are the falling-edge info!
    # line + tv_questdb_wal_suspended_tables returning to 0.
    "wal-suspend-01" = {
      pattern     = "{ $.code = \"WAL-SUSPEND-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-10: once-per-episode emitter - the auto-OK ~15 min later only means the datapoint aged out while the table may still be suspended (Rule-11 false-recovery; ws-reinject-01 precedent)
      desc        = "WAL-SUSPEND-01: a QuestDB table's WAL apply is SUSPENDED - ingestion keeps ACKing rows while they silently stop becoming visible/applied (silent data-visibility loss; typical cause = a disk-full episode or a WAL apply error). Operator action: read the table/error_tag/error_message fields in the errors-jsonl stream, fix the underlying cause (df -h /data, QuestDB logs), then run ALTER TABLE <table> RESUME WAL in the QuestDB console - NEVER auto-executed (resuming into a still-broken disk replays the failure). NO recovered/OK page: the code fires once per suspension episode; recovery signal = the falling-edge recovery log + tv_questdb_wal_suspended_tables returning to 0. Runbook: .claude/rules/project/wal-suspension-error-codes.md"
    }
    # TICK-CONSERVE-01 (added 2026-07-14 — automation-gaps PR-3; RETIRED
    # 2026-07-18 — tick-conservation retirement, dead-WS sweep follow-up):
    # its ONLY emit site (crates/app/src/tick_conservation_boot.rs, the
    # 15:40 IST reconciler's Leak arm) was deleted with the audit modules
    # — every audit input died with the dead tick chain (stage-2 sweep
    # #1631), so the filter could never match again (a filter with no
    # possible emit site is a dead filter per
    # error_code_paging_filter_drift_guard.rs — the ws-reinject-01 /
    # cross-verify-1m-01/-02 precedent). The `tick_conservation_audit`
    # QuestDB TABLE is retained (SEBI 5y, never dropped). Runbook
    # retirement banner:
    # .claude/rules/project/tick-conservation-audit-error-codes.md.
    # AUTH-GAP-05 (added 2026-07-14 — REST-audit GAP-01): the mid-session
    # forced token re-mint previously paged via app Telegram ONLY (no CW
    # backstop — a dead notifier silenced the token-death page entirely).
    # SCOPED to the mint-FAILURE arm via $.cooldown_skip IS FALSE: the
    # cooldown_skip boolean field exists ONLY on the "forced re-mint
    # failed" emission (crates/core/src/auth/mid_session_watchdog.rs —
    # alongside `permanent`: permanent=true is the RESILIENCE-03 in-flight
    # lock refusal, permanent=false every other mint failure; both are the
    # session-dead state per the audit's GAP-02/GAP-04: the retry-once
    # latch holds and the token stays dead for the rest of the session).
    # cooldown_skip=true lines are EXCLUDED (the 2026-07-14 Dhan-noise-lock
    # H3 arm: a TokenManager mint-cooldown skip is NOT terminal — the next
    # re-arm window retries, and the app Telegram is equally gated
    # !permanent && !cooldown_skip; matching it here would page a
    # self-retrying non-failure). The TRIGGER arm ("forcing re-mint")
    # fires on every episode INCLUDING successful ~30-min self-heals and
    # carries NO cooldown_skip/permanent fields — operator-ruled noise
    # ("silent-when-healing, loud-only-when-unobtainable"), deliberately
    # NOT matched. ok_recovery = false: once-per-episode emitter (the
    # ws-reinject-01 precedent) — the token does not come back when the
    # datapoint ages out; real recovery = tv_token_valid returning to 1 /
    # the next clean watchdog cycle.
    "auth-gap-05-remint-failed" = {
      pattern     = "{ $.code = \"AUTH-GAP-05\" && $.level = \"ERROR\" && $.cooldown_skip IS FALSE }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-14: once-per-episode mint failure - the retry-once latch holds, so the token stays dead for the session; auto-OK ~15 min later would be a Rule-11 false recovery
      desc        = "AUTH-GAP-05 forced re-mint FAILED: the mid-session watchdog detected a sustained dead Dhan token, issued its ONE forced re-mint for the episode, and the mint FAILED (permanent=true = a peer holds the dual-instance lock in-flight; permanent=false = mint HTTP/TOTP failure) - the token stays DEAD for the rest of the session (the retry-once latch holds; the 4h sweep backstop is lane-only per audit GAP-02). Successful self-heal re-mints deliberately do NOT page (trigger arm unmatched - silent-when-healing), and cooldown_skip=true mint-cooldown skips are excluded (non-terminal; the next re-arm window retries). NO recovered/OK page: recovery signal = tv_token_valid back to 1 / the next clean profile cycle. Runbook: .claude/rules/project/wave-4-error-codes.md (AUTH-GAP-05)"
    }
    # SPOT1M-01 escalation edge (added 2026-07-14 — REST-audit GAP-03):
    # the per-minute Dhan spot-1m REST leg (until 2026-08-21 the Groww spot +
    # Groww contract legs emitted SPOT1M-01 too; they left with the Groww
    # feed) pages HIGH via app Telegram at the
    # 3-consecutive-fully-failed-minutes edge; this filter is the CW
    # backstop for exactly that edge. Stage-scoped: stage="escalation" is
    # the ONCE-per-episode edge line (edge-latched, re-armed only after a
    # fetch+persist-clean minute); the per-minute stage="minute_failed" /
    # "boundary_skipped" / etc. lines fire every failed minute and are
    # sub-edge by design — a plain code filter would over-page vs the
    # designed 3-minute escalation (rest-1m-pipeline-error-codes.md §1).
    "spot1m-01-escalation" = {
      pattern     = "{ $.code = \"SPOT1M-01\" && $.level = \"ERROR\" && $.stage = \"escalation\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-14: once-per-episode edge - the recovery signal is the leg's own typed Info recovery Telegram / rows landing again, not the datapoint aging out
      desc        = "SPOT1M-01 escalation: the per-minute Dhan REST 1m spot candle leg (the Groww spot + contract legs were removed 2026-08-21 with the Groww feed; the feed/leg fields in the errors-jsonl stream still name the leg) fully failed 3+ consecutive minutes (persist-gated: fetch-ok-but-lost rows count as failed). Fires once per episode (edge-latched). Triage: cross-check DH-901 (REST surface/token; the REST canary was retired 2026-07-14 with the Dhan noise lock), tv_spot1m_fetch_total outcome rates, QuestDB health for persist-gated episodes. NO recovered/OK page: recovery = the leg's typed recovery Telegram + rows landing again. Runbook: .claude/rules/project/rest-1m-pipeline-error-codes.md"
    }
    # CHAIN-02 escalation edge (added 2026-07-14 — REST-audit GAP-03):
    # same contract as spot1m-01-escalation for the option-chain legs
    # (Dhan only since 2026-08-21). stage="escalation" only — per-minute
    # sub-edge lines deliberately unmatched.
    "chain-02-escalation" = {
      pattern     = "{ $.code = \"CHAIN-02\" && $.level = \"ERROR\" && $.stage = \"escalation\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-14: once-per-episode edge - same rationale as spot1m-01-escalation
      desc        = "CHAIN-02 escalation: the per-minute Dhan option-chain REST leg (the Groww chain leg was removed 2026-08-21 with the Groww feed) fully failed 3+ consecutive minutes (persist-gated). Fires once per episode (edge-latched). Triage: spot leg healthy + chain failing = chain-API-surface problem (entitlement wobble short of CHAIN-01, gateway); both failing = REST/token (AUTH-GAP runbooks). NO recovered/OK page: recovery = the typed ChainFetchRecovered Telegram + rows landing again. Runbook: .claude/rules/project/rest-1m-pipeline-error-codes.md"
    }
    # CHAIN-01 (added 2026-07-14 — REST-audit GAP-03): entitlement absent.
    # Plain coded filter is safe: BOTH stages (warmup = day-down at boot,
    # mid_session = revoked intra-day) fire ONCE per day/episode and are
    # page-worthy; the probe-only path never emits CHAIN-01 (info!-level
    # verdict only — verified 2026-07-14, option_chain_1m_boot.rs).
    "chain-01" = {
      pattern     = "{ $.code = \"CHAIN-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-14: once-per-day emitter - the entitlement stays absent when the datapoint ages out (Rule-11 false-recovery)
      desc        = "CHAIN-01: Dhan Option Chain Data-API entitlement ABSENT (DH-902/806 class) - the chain pipeline is DOWN for the day (warmup stage) or was revoked mid-session (mid_session stage). Operator action: verify the account's Data-API plan on the Dhan portal; restoring the entitlement auto-resumes at the next trading-day boot. NO recovered/OK page: the entitlement does not return when the episode ages out. Runbook: .claude/rules/project/rest-1m-pipeline-error-codes.md"
    }
    # CHAIN-04 warmup arm (added 2026-07-14 — REST-audit GAP-03): the
    # day-start expirylist warmup exhausted its bounded retries — the
    # chain pipeline is DOWN FOR THE DAY (expiries are never guessed).
    # Stage-scoped to "warmup" ONLY: the probe_client_build /
    # probe_no_token / probe_inconclusive / probe_task_exit /
    # warmup_no_token stages are log-only-by-design transient/respawn
    # arms (warmup_no_token REPEATS every ~30s supervisor respawn until a
    # token exists — the AUTH-GAP runbooks own the token page); a plain
    # code filter would page on all of them.
    "chain-04-warmup" = {
      pattern     = "{ $.code = \"CHAIN-04\" && $.level = \"ERROR\" && $.stage = \"warmup\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-14: once-per-day emitter - the day stays chain-less when the datapoint ages out; recovery = the next trading-day boot's clean warmup
      desc        = "CHAIN-04 warmup FAILED: the day-start option-chain expirylist warmup exhausted its bounded retries - the chain pipeline is DOWN FOR THE DAY (expiry dates are never guessed; no mid-day retry by design). Triage: cross-check DH-901 + the WS feed (the REST canary was retired 2026-07-14); a healthy REST surface with only the expirylist failing points at the option-chain API specifically. Restart the app once the REST surface is healthy to re-run the warmup, else tomorrow's boot re-warms. NO recovered/OK page: the day stays down when the datapoint ages out. Runbook: .claude/rules/project/rest-1m-pipeline-error-codes.md"
    }
    # =====================================================================
    # 2026-08-11 — CRITICAL-SEVERITY PAGING GAP (+4 entries -> 15 filters +
    # 15 alarms, ~+$0.40/mo; see aws-budget.md COST NOTE 2026-08-11).
    #
    # THE GAP: a Severity::Critical code that reaches NO human surface is a
    # silent failure by definition. A mechanical sweep of all 167 ErrorCode
    # variants found 29 Critical; only 4 of them had an entry in this map
    # (dh-901, auth-gap-04, proc-01, aggregator-drop-01). Of the remaining
    # 25, fourteen have NO error!-level emit site at all (dead enum entries
    # — listed for retirement, NEVER alarmed here: a filter with no possible
    # emit site is a dead filter per error_code_paging_filter_drift_guard.rs
    # — the ws-reinject-01 / tick-conserve-01 precedent), and eleven have a
    # real emit. Of those eleven, four already reach a human through a typed
    # NotificationEvent dispatch at the emit site (ORPHAN-POSITION-01 ->
    # OrphanPositionDetected, RESILIENCE-01 -> DualInstanceDetected,
    # RESILIENCE-03 -> AuthenticationFailed, OMS-GAP-03 ->
    # CircuitBreakerOpened per dhan-rest-only-noise-lock §2a) and are NOT
    # duplicated here on cost discipline; two are BLOCKED pending a dated
    # operator quote (AUTH-GAP-01 / DATA-805 are Dhan-scoped, and
    # dhan-rest-only-noise-lock-2026-07-14.md §3 REJECTs any new Dhan-scoped
    # page outside its 4-item family; GROWW-OCO-02 was DELETED 2026-08-21 with the
    # Groww order-side (its emit site is gone, so its ErrorCode variant is
    # retired) — an alarm for it would have had no producer. The FOUR below
    # are the genuinely silent, genuinely reachable, genuinely unblocked
    # remainder. Ratchet: crates/storage/tests/
    # critical_errcode_alarm_coverage_guard.rs re-derives this whole
    # classification on every build and FAILS if a Critical code with a real
    # emit site is neither alarmed nor allowlisted.
    # =====================================================================
    # BOOT-02: the QuestDB boot-probe deadline (Severity Critical — boot
    # BLOCKS). This entry also repairs a documented FALSE-OK: the
    # wal-suspend-01 desc above tells the operator "a merely-DOWN QuestDB
    # never fires it — BOOT-01/02 own that page", but BOOT-02 owned no page
    # at all until this entry existed. ok_recovery = false: boot-blocking is
    # terminal for that boot — the process does not proceed, so the auto-OK
    # ~15 min after the datapoint ages out can only mean the app stopped
    # emitting (it exited), never "QuestDB came back" (Rule-11
    # false-recovery; the proc-01 precedent).
    "boot-02" = {
      pattern     = "{ $.code = \"BOOT-02\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-08-11: boot-blocking terminal event - the auto-OK ~15 min later means the app stopped emitting (it exited), never that QuestDB recovered (Rule-11 false-recovery; proc-01 precedent)
      desc        = "BOOT-02: the QuestDB boot probe exceeded its deadline - BOOT IS BLOCKED and the app is NOT running (no REST legs, no persistence, no cadence). Triage: docker ps / QuestDB container health, df -h /data, QuestDB logs; the app self-restarts under systemd once QuestDB answers. NO recovered/OK page: the auto-OK only means the process stopped emitting - confirm the app is actually UP (health endpoint + tv_boot_completed) before treating it as recovery. Runbook: docs/error-runbooks/wave-2-error-codes.md"
    }
    # BOOT-03: clock skew beyond the boot tolerance (Severity Critical). A
    # skewed clock silently corrupts EVERY IST timestamp the system writes -
    # candle bucket boundaries, dedup keys, the SEBI audit trail - so it is
    # a data-integrity event, not a nuisance. ok_recovery = false: same
    # boot-blocking terminal shape as boot-02.
    "boot-03" = {
      pattern     = "{ $.code = \"BOOT-03\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-08-11: boot-blocking terminal event - same shape as boot-02; the auto-OK means emissions stopped, not that the clock was corrected
      desc        = "BOOT-03: host CLOCK SKEW exceeded the boot tolerance - every IST timestamp this host writes would be wrong (candle bucket boundaries, dedup keys, the SEBI audit trail), so boot is BLOCKED rather than writing corrupt history. Triage: timedatectl / chrony-ntp sync status on the box, then restart the app. NO recovered/OK page: confirm the clock is actually synced before treating the auto-OK as recovery. Runbook: docs/error-runbooks/wave-2-c-error-codes.md"
    }
    # OMS-GAP-06: the dry-run/paper order runtime task DIED and was
    # respawned with a FRESH paper book (Severity Critical). Per the emit
    # site's own honesty note, PAPER positions + day P&L are in-RAM only and
    # are SILENTLY ZEROED by the respawn - nothing replays in the
    # socket-free shape. Order-execution family, NOT a Dhan REST page:
    # dhan-rest-only-noise-lock-2026-07-14.md §2a states the order-execution
    # family is a separate landed family outside the §2 4-item Dhan set, so
    # this entry does not touch that lock. ok_recovery = false: a discrete
    # book-loss event - the zeroed paper positions do not come back when the
    # datapoint ages out (the aggregator-drop-01 precedent).
    "oms-gap-06" = {
      pattern     = "{ $.code = \"OMS-GAP-06\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-08-11: discrete book-loss event - the silently-zeroed paper positions + day P&L do not come back when the episode ages out (Rule-11 false-recovery; aggregator-drop-01 precedent)
      desc        = "OMS-GAP-06: the dry-run order runtime task DIED and respawned with a FRESH paper book - paper positions and day P&L are in-RAM only and were SILENTLY ZEROED (nothing replays in the socket-free shape). Any P&L or position number read after this point is measured from zero, not from the real session. Triage: read the reason + consecutive_abnormal_exits fields in the errors-jsonl stream; repeated respawns mean a real defect in the runtime, not a blip. NO recovered/OK page: the lost book does not return when the episode ages out. Runbook: .claude/rules/project/order-runtime-dryrun.md"
    }
    # WS-SPILL-02: a raw WS frame was DROPPED at the capture-at-receipt WAL
    # because the writer was dead at the append instant (Severity Critical).
    # This is the durable floor's own failure - the frame is gone BEFORE
    # parse/broadcast, so no downstream tier can recover it. Exactly the
    # silent-data-loss class that earned aggregator-drop-01 its alarm on
    # 2026-07-09 (that one is the sealed-candle side; this is the raw-frame
    # side). ok_recovery = false: permanent loss.
    # WS-SPILL-01: the WAL spill WRITER hit an I/O error - it could not open
    # a segment, or a write_record failed (Severity High). 2026-08-12: added
    # after noticing WS-SPILL-02 was alarmed and its SIBLING was not, which
    # left the more common disk failure mode unpaged.
    #
    # Why this matters as much as -02: `WsFrameSpill::append` returns
    # `Spilled` the instant a record enters the crossbeam channel, and the
    # disk write happens LATER on the writer thread. So when the disk is
    # full or the WAL dir is unwritable, every caller keeps being told the
    # frame was durably captured while `persist_record_resilient` quietly
    # returns 0 and moves on (deliberately - it must not kill the writer
    # thread). The capture-at-receipt durable floor is GONE and nothing
    # upstream can tell. This alarm is the only thing that says so.
    #
    # Distinct from -02 by cause, not by severity of consequence: -02 is
    # "the channel was full at the append instant" (frame never entered the
    # WAL); -01 is "the frame entered the WAL and the disk refused it".
    # Both end with the raw frame absent from the durable chain.
    #
    # eval = 3 x 300s deliberately matches -02: a single transient I/O blip
    # self-heals on the next record (the writer reopens the segment), and
    # this file's own convention is that a real failure sustains. A flapping
    # writer is the documented "disk dying" signal.
    #
    # ok_recovery = false: while the writer is down, the frames that arrived
    # are not written anywhere retroactively. The disk recovering does not
    # bring them back, so an OK page would read as "we got the data back".
    "ws-spill-01" = {
      pattern     = "{ $.code = \"WS-SPILL-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-08-12: the frames that arrived while the writer was down were never written - a recovering disk does not restore them (Rule-11 false-recovery; ws-spill-02 precedent)
      desc        = "WS-SPILL-01: the capture-at-receipt WAL writer hit a disk I/O error and could not persist frames (could not open a segment, or a record write failed). The writer thread deliberately stays alive and retries, so the app looks healthy and append() still reports frames as spilled - but the durable floor is NOT holding while this fires. Triage: df -h on the data volume FIRST (disk full is the usual cause), then ls -la data/spill/ for permissions/mount, then dmesg for device errors. Read the stage field in errors-jsonl to tell open_segment / no_segment / write_record apart. NO recovered/OK page: frames that arrived during the outage were never written and do not come back. Runbook: docs/error-runbooks/ws-frame-spill-error-codes.md"
    }
    "ws-spill-02" = {
      pattern     = "{ $.code = \"WS-SPILL-02\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-08-11: discrete PERMANENT data loss - the dropped raw frames are gone from the durable chain and do not come back when the episode ages out (Rule-11 false-recovery; aggregator-drop-01 precedent)
      desc        = "WS-SPILL-02: a raw WebSocket frame was DROPPED at the capture-at-receipt WAL (the spill writer was dead at the append instant) - the frame is lost BEFORE parse and broadcast, so no downstream tier can recover it. This is the raw-frame twin of AGGREGATOR-DROP-01 (sealed candles) and is the durable floor's own failure mode. Triage: df -h /data, ls -la data/spill/, host + container health; if the host is healthy and the dirs writable, restart the app. NO recovered/OK page: the loss is permanent - the auto-OK only means the episode aged out. Runbook: docs/error-runbooks/ws-frame-spill-error-codes.md"
    }
    # STORAGE-GAP-05: pressure-triggered archival ran and could NOT relieve
    # the volume (feed-hardening Item 5, 2026-08-19). Severity Critical.
    #
    # Why this pages rather than sitting in the log: the next state is a FULL
    # volume, and a full volume does not merely stop retention - every QuestDB
    # write blocks, so the ILP flush backs up, so the frame drain backs up, so
    # the socket receive buffer overflows, and Dhan's published architecture
    # skips a slow consumer forward to "the latest available state". A disk
    # problem becomes a TICK-LOSS problem, upstream, at the vendor, silently.
    #
    # Why the automation stops here rather than deleting more: the only
    # partitions left are younger than the hard MIN_HOT_DAYS=2 floor (still
    # being written, so a drop can swallow an arriving tick) or have no
    # verified S3 copy. Both are data-loss trades and belong to the operator.
    #
    # eval = 3 like its BOOT/WS-SPILL siblings: the emit is already
    # edge-latched to ONCE per episode in the app, so this is about ride-out,
    # not de-duplication. ok_recovery = TRUE, unlike ws-spill: nothing was
    # lost here - the volume genuinely can recover (an operator grows it, or a
    # later pass frees space once partitions age past the floor), and "the
    # disk is healthy again" is a real state worth telling.
    "storage-gap-05" = {
      pattern     = "{ $.code = \"STORAGE-GAP-05\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = true # 2026-08-19: no data was lost by this code - the volume can genuinely recover (grown, or partitions age past the floor), so an OK is honest here where it would be false for ws-spill
      desc        = "STORAGE-GAP-05: the data volume is above high water and pressure archival has nothing left it is ALLOWED to reclaim. NOTHING further will be auto-deleted. A full volume does not just stop retention - it blocks every QuestDB write, backs up the drain, and Dhan then skips us forward as a slow consumer, dropping ticks at their side. Triage: df -h on the data volume; read partitions_dropped in the log payload. If 0 dropped, check S3: aws s3 ls s3://tv-<env>-cold/questdb-partitions/ and partition_archive_audit - a verify failure keeps partitions BY DESIGN. Remedy is yours: grow the gp3 volume (online, one command, NEVER shrinkable) or cut ingest scope (depth is the heaviest writer). Do NOT lower MIN_HOT_DAYS or hand-drop an unverified partition. Runbook: docs/error-runbooks/wave-2-error-codes.md"
    }
    # 2026-08-15 (authority: dhan-rest-only-noise-lock-2026-07-14.md §2.3a,
    # operator quote same day) - the CONNECTED-BUT-SILENT page.
    #
    # This condition has NO other evidence anywhere in the system. A subscribe
    # that silently did not take produces no payload to count, no parse to
    # fail, and no error of its own; the socket is open, the lane gauge reads
    # 1, and every loss counter sits at a healthy flat zero. Absence measured
    # against a seeded key is the only thing that can ever report it, which is
    # why scan_silence exists - and why leaving its verdict log-sink-only (as
    # it was from 2026-08-12 to 2026-08-15) meant the one detector for an
    # invisible failure was itself invisible.
    #
    # A LOG FILTER rather than a threshold on the gauges, deliberately: the
    # app already gates the emit to the CONTINUOUS session (never the
    # legitimately-silent pre-open) and edge-latches it to one per episode, so
    # the coded error carries the market-hours gating and the de-duplication
    # that a raw gauge alarm would need a window Lambda and an unknown
    # baseline to reproduce. The derived metric is sparse and dimensionless -
    # billed only in hours the code actually fires.
    #
    # eval = 1, unlike the ws-spill pair above: the emit is ALREADY latched to
    # two consecutive 30s scans by the app, so the sustain requirement has been
    # met before the line is ever written. Requiring three CloudWatch windows
    # on top would delay the page by 10 minutes for a condition the detector
    # has already confirmed.
    #
    # ok_recovery = true, ALSO unlike the ws-spill pair: silence is not a
    # permanent loss of a specific frame. Instruments genuinely start ticking
    # again - after a resubscribe, or when a thin contract simply trades - and
    # "the feed is being heard again" is a real, self-explanatory recovery an
    # operator wants told.
    # 2026-08-15, SAME DAY as this entry was added: period 300 -> 3600 and
    # ok_recovery true -> false, after checking what the emit ACTUALLY does in
    # production rather than what its edge-latch was designed to do.
    #
    # Friday 2026-08-14 produced 25 distinct RISK-GAP-03 emits in one session.
    # The silent count oscillated the whole day -- 4, 9, 1, 2, 1, 3, 208, 10 --
    # clearing between episodes and re-arming the per-episode latch each time,
    # entirely legitimately: sparse-cadence instruments go quiet and come back.
    # The latch worked as designed and still produced 25 pages, because the
    # world produced 25 episodes. At period 300 with an OK page that is ~50
    # operator messages in a day, which is how a pager gets ignored.
    #
    # (An earlier draft of this comment said the condition "never changed --
    # never_ticked=4 of 4, all day". That was wrong: never_ticked was 4 on the
    # 09:15 emit and 0 on every one after it, so the feed WAS delivering. The
    # correction matters because it changes which fix works -- gating on
    # never-ticked alone would have suppressed almost none of these.)
    #
    # The emit side gained a 30-minute cooldown between pages in the same
    # change. This is the second half: an hour-long window collapses whatever
    # still gets through into one alarm state, and the alarm returns to OK
    # silently via notBreaching once the emits stop.
    #
    # ok_recovery = false, reversing the note this entry shipped with. Silence
    # ending is a real recovery for the INSTRUMENT, but the alarm cannot tell
    # "the feed is healthy again" from "one sparse contract happened to trade".
    # An OK page that means the second while reading as the first is worse than
    # no page at all.
    "risk-gap-03" = {
      pattern     = "{ $.code = \"RISK-GAP-03\" && $.level = \"ERROR\" }"
      period      = 3600
      threshold   = 1
      eval        = 1
      dta         = 1
      ok_recovery = false # 2026-08-15: an OK here cannot distinguish a recovered feed from one sparse instrument trading once
      # Kept under the 1024-char alarm_description ceiling INCLUDING the
      # suffix the resource appends (~162 chars) - see the length guard in
      # crates/common/tests/error_code_paging_filter_drift_guard.rs.
      desc = "RISK-GAP-03: the live feed is CONNECTED BUT HEARING NOTHING from instruments it subscribed. The 30s silence scan found instruments quiet beyond their own learned cadence, or that never ticked at all - the second is the serious one, because a subscribe that silently did not take leaves NO other trace: no payload, no parse failure, no error, and every loss counter reads a healthy zero. Once per episode, session-gated so the legitimately-silent pre-open never pages. Triage on the dashboard live-lane row: never_ticked climbing means subscriptions are not taking (check tv_dhan_ws_subscribe_failed_total and the subscribe batches in the app log); silent climbing while never_ticked stays 0 is usually a thin universe on a quiet day, not a fault. Runbook: .claude/rules/project/gap-enforcement.md"
    }

    # 2026-08-22 (operator: "Fix and resolve wvrytni fdude okay", given in direct
    # response to a message naming this fix, its cost and that it needed his go —
    # the §2.3d dated authorization).
    #
    # The pattern carries THREE conditions, not the usual two, and that is the
    # whole design. WS-GAP-03 is the WebSocket connection-state code with ~50
    # emit sites — every dial failure, reconnect and pool event uses it — so a
    # bare `$.code = "WS-GAP-03"` filter would page on ordinary connection
    # churn. That is the RISK-GAP-03 noise trap (25 pages in one session) with
    # 50x the surface. `$.source = "fell_back_to_indices"` appears on exactly
    # one ERROR emit: the universe-collapse arm in dhan_live_universe.rs. The
    # sibling emits on that path are `info!`, so `$.level = "ERROR"` already
    # excludes them; the source condition excludes the other 49 sites.
    #
    # ok_recovery = false, matching the discrete-event precedent above: the
    # universe is chosen ONCE per boot, so an auto-OK an hour later means the
    # datapoint aged out, never that the next session widened correctly.
    "ws-gap-03-universe-collapse" = {
      pattern     = "{ $.code = \"WS-GAP-03\" && $.level = \"ERROR\" && $.source = \"fell_back_to_indices\" }"
      period      = 3600
      threshold   = 1
      eval        = 1
      dta         = 1
      ok_recovery = false # chosen once per boot - an auto-OK means the datapoint aged out, not that the next session widened
      desc        = "WS-GAP-03 universe collapse: the DHAN live feed fell back to the 4-instrument index universe. Either today's master exceeded the authorized capacity envelope, or it produced no usable widening (artifact unreadable, absent or empty). The session is running 4 instruments instead of the authorized ~24,600 - a 99.98% loss of market data - and nothing else reports it: the 4 indices still tick, so the no-ticks alarm stays green and every loss counter reads a healthy zero. Triage from the same log line: capacity vs master_entries at/over the cap means the universe outgrew 25,000 (a vendor option-chain expansion is the usual cause); master_entries 0 means the artifact did not load. Runbook: .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md"
    }

    # 2026-08-25 (operator: "Fix wbrytjonf dude oaku", given in direct response
    # to a message whose open-items list named this alarm and said it needed his
    # go — the §2.3f dated authorization, written before this terraform).
    #
    # The 15:41 live-vs-official cross-verification is the ONLY ground truth the
    # revived Dhan feed has, and until now neither of its failure verdicts
    # reached anything: `tv_dhan_feed_xverify_runs_total` is in NEITHER EMF
    # selector copy, and the error line carries WS-GAP-03, which has ~50 emit
    # sites in dhan_feed_stack.rs.
    #
    # FOUR conditions, not the usual two. `$.level = "ERROR"` excludes the info
    # arms; the two `$.source` values were added by PR #1808 SPECIFICALLY so
    # this filter could exist, and they appear on exactly these two emits. A
    # bare `$.code = "WS-GAP-03"` filter would page on every dial failure and
    # reconnect — the RISK-GAP-03 noise trap (25 pages in one session) with 50x
    # the surface, and the same mistake §2.3d-i records being approved and then
    # caught before it shipped.
    #
    # Why a log filter and not a metric: the counter name is 31 bytes and needs
    # 32 with its separating pipe, against 31 free in the user-data budget
    # (§2.3d-ii). The EMF route misses by ONE byte; this lane costs none.
    #
    # ok_recovery = false: the comparison runs ONCE per session, so an auto-OK
    # an hour later means the datapoint aged out, never that anything compared.
    # SPLIT INTO TWO ENTRIES, deliberately (2026-08-25, same change).
    #
    # The first draft was ONE entry matching both verdicts with
    # `($.source = "a" || $.source = "b")`. `terraform plan` accepted it — and
    # that acceptance means nothing here: the provider treats `pattern` as an
    # opaque string, so filter-pattern SYNTAX is parsed only by the real
    # PutMetricFilter call at APPLY time. A malformed pattern would therefore
    # sail through every PR check and break the post-merge apply lane.
    #
    # Two single-condition entries use only the shape already proven live by
    # ws-gap-03-universe-collapse above. It costs one extra alarm (~$0.10/mo)
    # and buys better triage anyway: the two verdicts have DIFFERENT causes and
    # different next steps, so naming them separately tells the operator which
    # one fired without opening the log.
    "ws-gap-03-xverify-vacuous" = {
      pattern     = "{ $.code = \"WS-GAP-03\" && $.level = \"ERROR\" && $.source = \"xverify_vacuous\" }"
      period      = 3600
      threshold   = 1
      eval        = 1
      dta         = 1
      ok_recovery = false # runs once per session - an auto-OK means the datapoint aged out, not that the next run compared
      desc        = "WS-GAP-03 cross-verify VACUOUS: the 15:41 live-vs-official comparison RAN and compared ZERO minutes. This is not a pass with no findings - it is no measurement at all, and a vacuous run rendering as 'no mismatches' is the false-OK class this repo has already retired twice. The comparison is the only ground truth the DHAN live feed has: the one check separating a lane that captures real ticks from one that merely dials. Triage from the same log line: missing_live high means the live lane produced no candles for the window (check tv_dhan_feed_last_tick_age_secs and the no-ticks alarm); missing_rest high means the official REST leg did not serve it (check the spot-1m leg). Runbook: .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md"
    }

    "ws-gap-03-xverify-failed" = {
      pattern     = "{ $.code = \"WS-GAP-03\" && $.level = \"ERROR\" && $.source = \"xverify_failed\" }"
      period      = 3600
      threshold   = 1
      eval        = 1
      dta         = 1
      ok_recovery = false # runs once per session - an auto-OK means the datapoint aged out, not that the next run ran
      desc        = "WS-GAP-03 cross-verify FAILED TO RUN: the 15:41 live-vs-official comparison errored out, so the day's captured candles are UNVERIFIED - never assume they are clean. Distinct from the vacuous alarm: that one ran and found nothing to compare; this one did not complete. The comparison is the only ground truth the DHAN live feed has. Triage: the same log line carries the underlying error verbatim in its err field; a token or QuestDB failure is the usual cause. Runbook: .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md"
    }

    # ADDED 2026-08-28 (noise-lock section 2.3k). The two entries above both
    # mean "we could not tell you". This one is the only verdict that is an
    # actual FINDING about the feed - it ran, it measured, and the two records
    # disagree - and it was the one with no source field and no alarm, logging
    # at info! among forty fields. The check that exists to say whether the
    # revived feed is trustworthy could answer NO in a form nothing watched.
    #
    # Threshold is HALF the compared price fields, and that bar is deliberate:
    # a non-zero divergence count is EXPECTED (a sampled live stream and the
    # vendor's full tape legitimately differ - cross-verify-1m-error-codes.md
    # section 1 says track the trend, not the count), so paging on any
    # divergence pages every trading day. No baseline exists for a NORMAL rate,
    # so 1% or 5% would be a number invented and called a measurement. More
    # than half is the one claim that holds at any baseline: the two records
    # are not describing the same market. The app-side gate carries the
    # arithmetic; this filter only matches the line it emits.
    "ws-gap-03-xverify-diverged" = {
      pattern     = "{ $.code = \"WS-GAP-03\" && $.level = \"ERROR\" && $.source = \"xverify_diverged\" }"
      period      = 3600
      threshold   = 1
      eval        = 1
      dta         = 1
      ok_recovery = false # runs once per session - an auto-OK means the datapoint aged out, not that the next run agreed
      desc        = "WS-GAP-03 cross-verify MASS DIVERGENCE: the 15:41 live-vs-official comparison ran and found MORE THAN HALF of the compared price fields disagreeing with Dhan's own record beyond tolerance. That is not sampling noise at any baseline - the captured candles and the vendor tape are not describing the same market, so treat the day's candles as untrustworthy until explained. The other two xverify alarms mean the check could not tell you; this one means it did and the answer is bad. Triage from the same log line: minutes_compared and price_fields_compared are the denominator, cells_diverged the numerator, noise_p95_paise / noise_max_paise say whether it is a small systematic offset (tolerance or rounding) or wholesale (wrong instruments, segment mismatch, clock fault). Runbook: .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md"
    }

    # ADDED 2026-08-28 (noise-lock section 2.3m). A depth-200 socket that
    # UNSUBSCRIBED its old contract and then failed to subscribe the new one is
    # carrying NOTHING. It stays transport-healthy - it keeps ponging, the
    # connection gauge counts it alive, the lane-up gauge reads 1 - and it
    # delivers no data for the rest of the session. Every existing alarm reads
    # green through it, including the no-ticks one, because fifteen other
    # sockets are still flowing and the lane's last tick is always ~1s old.
    #
    # Scoped by `$.source`, never by the bare code: WS-GAP-02 is emitted by the
    # top-up path too, which STOPS at its wire budget without emptying anything
    # and is an ordinary, non-paging outcome. A bare-code filter would page on
    # routine ATM top-up budget exhaustion - the RISK-GAP-03 noise trap, on a
    # path that runs every minute.
    #
    # ok_recovery = true: unlike the xverify entries above, this condition
    # genuinely recovers - the redial the same arm schedules brings the socket
    # back holding the CURRENT contract (the retained set already names it), so
    # a return to OK means the remediation worked and is worth telling.
    "ws-gap-02-swap-emptied-socket" = {
      pattern     = "{ $.code = \"WS-GAP-02\" && $.level = \"ERROR\" && $.source = \"swap_emptied_socket\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = true
      desc        = "WS-GAP-02 DEPTH SOCKET EMPTIED: an at-the-money swap unsubscribed the old contract and then failed to subscribe the new one, so this socket is now carrying NO instruments. It stays transport-healthy and keeps ponging, which is why no other alarm sees it: the connection gauge counts it alive and the no-ticks alarm reads the whole lane, where fifteen other sockets are still flowing. A redial is scheduled automatically by the same code path and the retained set already names the NEW contract, so the socket should come back holding the right strike within one backoff - this alarm returning to OK is that remediation working. If it does NOT clear, the socket is failing to re-dial: check tv_dhan_ws_park_total and the endpoint field on this log line. Runbook: .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md"
    }
    # SCOREBOARD-01 — 2026-09-02. Authorized by the operator's reaffirmation
    # that day; the dated quote is in
    # dhan-rest-only-noise-lock-2026-07-14.md section 2.3q, recorded BEFORE
    # this entry per the rule-file-first law.
    #
    # WHY IT WAS INVISIBLE. A 2026-09-02 sweep of every write path found
    # ws_connection_rollup.rs is the ONE file whose flush-failure arm has no
    # counter at all — and SCOREBOARD-01 is Severity::Medium, so
    # error_code_alarm_coverage_guard never required it to be alarmed either.
    # Between the two it was log-sink-only by accident, not by decision: a
    # failed flush loses the WHOLE per-connection day summary and nothing
    # anywhere reports it.
    #
    # WHY THIS IS A SAFE PAGE, NOT NOISE. The rollup runs ONCE per day, after
    # close. A failure is a single discrete event on a cold path, so this can
    # fire at most once a day and cannot flap — unlike risk-gap-03, which
    # produced 25 pages in one session and is the noise case this file
    # records. eval 1 / dta 1 because a once-daily emitter has no second
    # datapoint to wait for.
    #
    # ok_recovery = false: the rollup does not retry within the day, so an OK
    # an hour later is the datapoint ageing out, never the summary arriving.
    # The underlying ws_event_audit and feed_episode_audit rows are unaffected
    # and the day can be re-rolled by hand.
    "scoreboard-01" = {
      pattern     = "{ $.code = \"SCOREBOARD-01\" && $.level = \"ERROR\" }"
      period      = 86400
      threshold   = 1
      eval        = 1
      dta         = 1
      ok_recovery = false
      desc        = "SCOREBOARD-01: a daily rollup failed to write. The per-connection or per-feed summary for that day is MISSING - this is the table you read to answer 'which connection misbehaved yesterday'. The raw ws_event_audit and feed_episode_audit rows it is folded FROM are unaffected, so nothing is lost and the day can be re-rolled by hand; what you lose until then is the summary view. Check QuestDB reachability at the 15:45 IST rollup window, then the stage field on the log line - it names which rollup and whether it failed on append or on flush. Runbook: .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md"
    }
  }
}

resource "aws_cloudwatch_log_metric_filter" "error_code" {
  for_each       = local.error_code_alerts
  name           = "tv-${var.environment}-errcode-${each.key}"
  log_group_name = aws_cloudwatch_log_group.tv_app.name # terraform-managed group (main.tf)
  pattern        = each.value.pattern
  metric_transformation {
    name      = "tv_errcode_${replace(each.key, "-", "_")}"
    namespace = "Tickvault/Prod"
    value     = "1"
    unit      = "Count"
    # NO dimensions: errors.jsonl events carry no host field and metric filters
    # cannot emit constant dimensions. Dimensionless by design; alarm matches.
    # NO default_value: sparse metric = billed only in hours with datapoints;
    # treat_missing_data=notBreaching makes sparseness correct.
  }
}

resource "aws_cloudwatch_metric_alarm" "error_code" {
  for_each            = local.error_code_alerts
  alarm_name          = "tv-${var.environment}-errcode-${each.key}"
  alarm_description   = "${each.value.desc} (log-derived from /tickvault/${var.environment}/app; added 2026-07-06 after the zero-page incident - the error! -> Telegram route was severed by the CloudWatch-only migration)"
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = each.value.eval
  datapoints_to_alarm = each.value.dta
  metric_name         = "tv_errcode_${replace(each.key, "-", "_")}"
  namespace           = local.app_namespace
  period              = each.value.period
  statistic           = "Sum"
  threshold           = each.value.threshold
  treat_missing_data  = "notBreaching"
  # deliberately NO dimensions (see filter comment)
  alarm_actions = local.app_alarm_actions
  # ok_recovery = false (ws-reinject-01, proc-01, dh-906,
  # aggregator-drop-01 [2026-07-09], wal-suspend-01 [2026-07-10],
  # cross-verify-1m-01/-02 + tick-conserve-01 [2026-07-14;
  # tick-conserve-01 retired 2026-07-18 with the audit modules];
  # rest-canary-01 retired 2026-07-14 -
  # the one-shot/discrete emitters) suppresses the OK page: their auto-OK
  # ~15 min after the datapoint ages out would be a Rule-11 false
  # "recovered" message while the condition persists (see the locals
  # comment above for the per-code rationale).
  #
  # ONE-TIME apply-evening noise (round-8, accepted + pre-briefed in the PR
  # body): every NEW alarm is created in INSUFFICIENT_DATA and - with
  # treat_missing_data=notBreaching on sparse/absent metrics - transitions
  # INSUFFICIENT_DATA -> OK on its first evaluation. CloudWatch invokes
  # ok_actions on ANY transition into OK, and the telegram-webhook Lambda
  # formats every OK as a green message (it reads only NewStateValue - no
  # OldStateValue filter). Expect up to ~5 one-time green "recovered" pages
  # the apply evening (canonical count, round-14): the 4 ok_recovery=true
  # codes here (dh-901, auth-gap-04; ws-gap-07 retired PR-C2 2026-07-13;
  # feed-stall-01 + the feed-stall-restarts counter pager retired
  # 2026-07-15 with the Groww live feed). Exempt: the reconnect-storm alarm via
  # actions_enabled=false, and BOTH AWS/Lambda Errors watchman alarms
  # (readiness-errors + market-hours-gate-errors) via ok_actions=[]
  # (round-14 — their auto-OK is aged-out, never a fix).
  # Creation settling, NOT recoveries. Flagged
  # follow-up (not this PR): an OldStateValue == INSUFFICIENT_DATA
  # suppression branch in the telegram-webhook Lambda - benefits every
  # future alarm PR.
  ok_actions = each.value.ok_recovery ? local.app_alarm_ok : []
}

# ---------------------------------------------------------------------------
# Pre-open readiness — did the attach finish by 09:12 IST?
# (operator 2026-08-22, recorded in dhan-rest-only-noise-lock-2026-07-14.md
#  §2.3e BEFORE this terraform, per the rule-file-first law)
#
# A SEPARATE pair rather than another `local.error_code_alerts` entry, because
# that map's shared metric_transformation hardcodes `value = "1"` — a COUNT.
# This one needs the NUMBER: `value = "$.ready_at_ist_secs"` extracts the field
# itself, so the metric IS the readiness second.
#
# Why that matters beyond tidiness: it costs ZERO user-data bytes, and the
# log-filter lane is already in place and already carries the value.
#
# CORRECTED TWICE, 2026-08-25 — both the original claim and the first
# correction were wrong, in opposite directions, and the measured numbers are
# recorded here so there is no third time.
#
#   * This comment used to say the gauge "cannot ship — the user-data
#     template renders at exactly its 15,872-byte budget with nothing free".
#     STALE: the Groww removal and a metric rename freed space.
#     `user_data_size_guard` measures the render at 15,841 bytes = 31 FREE.
#   * The first correction then claimed the name was 31 bytes and so still
#     did not fit. ALSO WRONG — that is the length of a DIFFERENT metric.
#     `tv_dhan_preopen_ready_secs` is 26 bytes; with its separating pipe, 27.
#     It WOULD fit in the 31 free bytes today.
#
# So the honest position is: the EMF route is no longer BLOCKED for this
# gauge, and the log-filter route is kept anyway — because it already works,
# costs nothing, and does not spend the last of a budget whose exhaustion is
# what forced this design in the first place. That is a choice with a reason,
# not a constraint. (For scale: `tv_dhan_feed_xverify_runs_total` is 31 bytes,
# so with its pipe it needs 32 and genuinely does NOT fit — one byte over.)
#
# Filtered on the INFO completion line, NOT the ERROR line that fires on a
# miss. The ERROR line would only ever produce a datapoint on a BAD morning —
# alarmable, but it could never answer "how early were we today?", which is the
# question that re-tunes STOCK_OPTION_PRICING_QUORUM_PERCENT from evidence
# instead of defending it.
resource "aws_cloudwatch_log_metric_filter" "preopen_ready" {
  name           = "tv-${var.environment}-preopen-ready-secs"
  log_group_name = aws_cloudwatch_log_group.tv_app.name
  # Anchored on the FIELD, not on the message text: the message is prose and
  # will be reworded, the field name is a contract the alarm depends on.
  pattern = "{ $.fields.ready_at_ist_secs = * }"
  metric_transformation {
    name      = "tv_dhan_preopen_ready_secs"
    namespace = "Tickvault/Prod"
    value     = "$.fields.ready_at_ist_secs"
    # NOT "Seconds": this is an absolute second-of-day, not an elapsed
    # duration, and labelling it a duration would make every chart read as a
    # nine-hour latency.
    unit = "None"
    # Sparse by design — one datapoint per session. No default_value, so the
    # metric is billed only in hours where an attach actually completed.
  }
}

resource "aws_cloudwatch_metric_alarm" "preopen_ready_late" {
  alarm_name        = "tv-${var.environment}-preopen-ready-late"
  alarm_description = "The DHAN live lane finished its contract and depth attach AFTER 09:12:00 IST (32120 = 9*3600+12*60 seconds of day). The session opened without its full ~23,000-instrument set on the wire: everything dialed, just late, so the first minutes after 09:15 carry spot only. Usual cause is the 60% stock-option pricing quorum arriving late on a thin pre-open - read underlyings_without_spot on the same 'late-attach complete' line, and re-tune STOCK_OPTION_PRICING_QUORUM_PERCENT from that rather than defending it. Runbook: .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md"
  # GreaterThan, not GreaterThanOrEqual: 09:12:00 exactly is MET, matching the
  # code's `ready_at <= PREOPEN_READY_DEADLINE_IST_SECS`. An off-by-one here
  # would page on the one morning that hit the deadline precisely.
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  datapoints_to_alarm = 1
  metric_name         = "tv_dhan_preopen_ready_secs"
  namespace           = local.app_namespace
  period              = 3600
  # Maximum, not Average: one datapoint per session, and on a restart day the
  # LATEST attach is the one that matters. An average would let a good 09:08
  # hide a 10:30 re-attach.
  statistic = "Maximum"
  threshold = 33120
  # notBreaching: the box is stopped overnight and does not attach at weekends,
  # so absent data is the normal off-hours state. A lane that never attaches at
  # all is a DIFFERENT alarm (dhan-no-ticks-flowing, breaching + gated).
  treat_missing_data = "notBreaching"
  # `local.app_alarm_actions`, the same indirection every alarm in this repo
  # uses — NOT a direct topic reference. The first draft wrote
  # `aws_sns_topic.alerts.arn`, a resource that does not exist (the topic is
  # `tv_alerts`, and nothing outside app-alarms.tf references it directly).
  # Terraform plan caught it; `terraform validate` could not run locally
  # because the provider registry is 403-blocked from the dev sandbox.
  alarm_actions = local.app_alarm_actions
  # NO ok_actions, deliberately — the discrete-emitter precedent set by the
  # ok_recovery = false codes above. This metric is SPARSE: one datapoint per
  # session. With treat_missing_data = notBreaching the alarm would flip back
  # to OK about an hour after that datapoint ages out, which is not a
  # recovery — nothing has been re-measured, and the next attach is a day
  # away. A "recovered" page on that transition is the Rule-11 false-OK the
  # locals comment above describes.
}

# ---------------------------------------------------------------------------
# HOT-PATH-02 — the persistence layer's own loss code (2026-08-25)
#
# THE GAP: HOT-PATH-02 has TEN `error!` emit sites — five in
# `tick_persistence.rs`, five in `depth_persistence.rs` — and had no filter in
# this file. It is the code every persistence-side loss line carries: a tick
# flush that failed and was rescued to a spill file, a flush whose rescue ALSO
# failed (permanently lost), a depth flush with no QuestDB connection, a depth
# flush that failed and discarded its buffer, and the table-ensure failures
# that can leave `ticks` auto-created WITHOUT its 5-key DEDUP — which silently
# collapses intra-second ticks. For the DEPTH path it is the only remaining
# loss signal at all: depth has no spill tier, so a discarded buffer is gone.
#
# WHY A STANDALONE PAIR, NOT A `local.error_code_alerts` ENTRY. That map is
# lockstepped to the documented paging list in
# `.claude/rules/project/observability-architecture.md` by
# `error_code_paging_filter_drift_guard.rs::tf_map_and_doc_paging_list_agree_bidirectionally`,
# so a map entry must land in the same change as its doc line. This change owns
# terraform only. The standalone form is the same shape the `preopen_ready`
# pair above uses and behaves identically at runtime. FLAGGED FOLLOW-UP for
# whoever owns the rules tree next: move this into the map and add the doc
# line, so "which codes page?" stays answerable from one place.
#
# SEVERITY NOTE: HOT-PATH-02 is `Severity::Low` in the enum, so
# `error_code_alarm_coverage_guard.rs` never required a decision here — which
# is precisely how ten emit sites on the tick and depth write paths ended up
# with no pager. The severity is arguably wrong; changing it is a Rust-side
# call and is deliberately not made from terraform.
#
# ALWAYS ARMED, no market-hours gate: the table-ensure arms fire at BOOT
# (08:30 IST, before the gate opens at 09:20), and a discarded row is a
# permanently missing row at any hour.
#
# eval 3 / dta 1 mirrors the coded entries above: a persistent condition
# repeat-emits and holds ALARM across <=15-minute gaps, while a single
# discarded buffer still pages on its own datapoint.
resource "aws_cloudwatch_log_metric_filter" "hot_path_02" {
  name           = "tv-${var.environment}-errcode-hot-path-02"
  log_group_name = aws_cloudwatch_log_group.tv_app.name
  pattern        = "{ $.code = \"HOT-PATH-02\" && $.level = \"ERROR\" }"
  metric_transformation {
    name      = "tv_errcode_hot_path_02"
    namespace = "Tickvault/Prod"
    value     = "1"
    unit      = "Count"
    # NO dimensions (errors.jsonl carries no host field and filters cannot emit
    # constant ones) and NO default_value (that emits a datapoint for every
    # NON-matching event, making the metric always-billed). Sparse metric +
    # treat_missing_data = notBreaching is the correct, near-free pairing.
  }
}

resource "aws_cloudwatch_metric_alarm" "hot_path_02" {
  alarm_name          = "tv-${var.environment}-errcode-hot-path-02"
  alarm_description   = "HOT-PATH-02: the persistence layer lost or could not write rows. Read the fields. `rescued` = a tick flush failed but the rows went to the named spill file; they are NOT in QuestDB and re-ingest is one safe, repeatable command (the ticks dedup key carries capture_seq). `dropped` with a spill_error = the rescue failed too and those ticks are permanently gone. On the DEPTH path `dropped` is always permanent - depth has no spill tier, so the writer discards its buffer to stop one rejected row wedging the session. stage=ensure_client_build or ensure_ddl is the quiet one: the ticks table may have been auto-created WITHOUT its 5-key DEDUP, which silently collapses intra-second ticks until a later ensure succeeds - verify with SHOW COLUMNS / the table's DEDUP keys. Raw frames remain in the write-ahead log. Runbook: docs/error-runbooks/wave-1-error-codes.md"
  comparison_operator = "GreaterThanOrEqualToThreshold"
  evaluation_periods  = 3
  datapoints_to_alarm = 1
  metric_name         = "tv_errcode_hot_path_02"
  namespace           = local.app_namespace
  period              = 300
  statistic           = "Sum"
  threshold           = 1
  treat_missing_data  = "notBreaching"
  # Dimensionless, matching its filter (see the filter comment).
  alarm_actions = local.app_alarm_actions
  # NO ok_actions. Rows that were discarded do not come back, and rows that
  # were rescued to a spill file are still not in QuestDB until someone
  # re-ingests them - so an auto-OK when the datapoint ages out would report a
  # recovery that nobody performed (Rule 11, the ok_recovery = false precedent
  # above).
  ok_actions = []
}

# ---------------------------------------------------------------------------
# THE WATCHER OF THE WATCHERS (2026-09-02)
#
# Every one of the coded-error filters above reads `$.code` -- the FLAT schema.
# The agent ships TWO files into this one log group, and only one of them
# carries that shape:
#
#   data/logs/machine/errors.jsonl.2*  ->  {"level":"ERROR","code":"X",...}
#   data/logs/machine/app.2*           ->  {"level":"ERROR","fields":{"code":"X"}}
#
# So all 27 coded-error alarms depend on ONE file reaching CloudWatch. If its
# glob stops matching -- exactly the 2026-07-06 incident signature, a
# data/logs/machine/ reorg that left the agent tailing paths that no longer
# existed -- every one of them goes permanently silent.
#
# And the guard that exists for that class cannot see it:
# `app_log_ingestion_silent` (log-retention.tf) watches AWS/Logs
# IncomingLogEvents dimensioned on the LOG GROUP, so it fires only if the
# WHOLE group goes quiet. With app.log still flowing the group looks perfectly
# healthy while the alarms it feeds are dead. That is a false OK on the
# alerting path itself: the thing that tells you your alarms died is blind to
# the only way they realistically die.
#
# The fix is a DETECTOR for the exact signature rather than a restructure.
# Splitting the two files into separate log groups would give per-group
# IncomingLogEvents for each, but it invalidates every runbook and saved query
# that names /tickvault/<env>/app. Making the 27 filters match both schemas
# would double-count every error and silently break all 27 thresholds. Both
# "fixes" cost more than the gap. Counting each schema separately costs two
# metric filters and answers the question directly.
# ---------------------------------------------------------------------------

# Errors visible in the FLAT schema -- the population the 27 filters can see.
resource "aws_cloudwatch_log_metric_filter" "log_schema_flat_errors" {
  name           = "tv-${var.environment}-log-schema-flat-errors"
  log_group_name = aws_cloudwatch_log_group.tv_app.name
  # `= *` is CloudWatch's EXISTENCE test (unquoted), the same form the
  # preopen_ready filter above uses. Anchored on the FIELD rather than on any
  # message text: prose gets reworded, a field name is the contract.
  pattern = "{ $.level = \"ERROR\" && $.code = * }"
  metric_transformation {
    name      = "tv_log_errors_flat_total"
    namespace = "Tickvault/Prod"
    value     = "1"
    unit      = "Count"
    # Sparse by design, like every filter above: billed only in hours that
    # actually produce datapoints.
  }
}

# The same errors as seen in the NESTED schema -- app.log's shape. This is the
# control. It is what proves the app is still producing errors when the flat
# count is zero, which is the difference between "a quiet session" and "the
# stream that feeds 27 alarms has stopped arriving".
resource "aws_cloudwatch_log_metric_filter" "log_schema_nested_errors" {
  name           = "tv-${var.environment}-log-schema-nested-errors"
  log_group_name = aws_cloudwatch_log_group.tv_app.name
  pattern        = "{ $.level = \"ERROR\" && $.fields.code = * }"
  metric_transformation {
    name      = "tv_log_errors_nested_total"
    namespace = "Tickvault/Prod"
    value     = "1"
    unit      = "Count"
  }
}

# Fires ONLY on the divergence, never on either count alone.
#
# A quiet session with zero errors of either shape is HEALTHY and must not
# page -- which is why this cannot be an alarm on the flat count being zero.
# The signature of the real failure is asymmetric: the app is demonstrably
# still emitting errors (nested > 0) while the file the alarms read produces
# none (flat == 0). Only that combination is a defect, so only that
# combination alarms.
#
# The nested threshold is 5 rather than 1 deliberately. At the boundary of a
# 5-minute period the two files can legitimately land in different windows for
# one or two events; five errors present in one schema and none at all in the
# other is not a boundary artifact.
resource "aws_cloudwatch_metric_alarm" "errcode_stream_silent" {
  alarm_name        = "tv-${var.environment}-errcode-stream-silent"
  alarm_description = <<-EOT
    The log stream that every coded-error alarm depends on has STOPPED arriving,
    while the app is still producing errors.

    All 27 coded-error metric filters read $.code, which only exists in
    data/logs/machine/errors.jsonl. This alarm fires when the app.log stream
    shows errors ($.fields.code) and errors.jsonl shows none -- meaning those 27
    alarms are now silent and CANNOT fire, no matter what breaks next.

    The log group still looks healthy, so app-log-ingestion-silent will NOT
    catch this. That is why this alarm exists.

    Triage on the box via SSM:
      sudo /opt/aws/amazon-cloudwatch-agent/bin/amazon-cloudwatch-agent-ctl -a status
      sudo tail -n 40 /opt/aws/amazon-cloudwatch-agent/logs/amazon-cloudwatch-agent.log
      ls -la /opt/tickvault/data/logs/machine/
    The agent's collect_list globs must match deploy/aws/cloudwatch-agent.json
    (machine/errors.jsonl.2* + machine/app.2*). The 2026-07-06 signature is a
    glob that no longer matches after a log-directory reorg.
  EOT

  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  evaluation_periods  = 2
  datapoints_to_alarm = 2

  # No data at all = a session with no errors in EITHER schema = healthy.
  treat_missing_data = "notBreaching"

  metric_query {
    id          = "verdict"
    # FILL(flat, 0) is load-bearing, not decoration. A metric filter emits a
    # datapoint ONLY when a matching line arrives, so in the exact failure this
    # alarm exists for -- errors.jsonl not arriving -- `flat` has NO datapoints
    # at all. CloudWatch metric math drops any period where an input is
    # missing, so a bare `flat < 1` would silently evaluate to nothing and the
    # alarm would be blind in its own target case. FILL is AWS's documented
    # tool for "a metric that only reports when the value is non-zero", which
    # is precisely what a log metric filter is.
    #
    # `nested` is deliberately NOT filled: it supplies the timestamp grid, and
    # in the failure case it is the series that HAS data. A period where
    # neither file reports anything therefore yields no datapoint at all and
    # falls through to treat_missing_data below -- a quiet session stays quiet.
    expression  = "IF(nested >= 5 AND FILL(flat, 0) < 1, 1, 0)"
    label       = "errors.jsonl stream silent while app.log still has errors"
    return_data = true
  }

  metric_query {
    id = "flat"
    metric {
      metric_name = "tv_log_errors_flat_total"
      namespace   = "Tickvault/Prod"
      period      = 300
      stat        = "Sum"
    }
  }

  metric_query {
    id = "nested"
    metric {
      metric_name = "tv_log_errors_nested_total"
      namespace   = "Tickvault/Prod"
      period      = 300
      stat        = "Sum"
    }
  }

  alarm_actions = [aws_sns_topic.tv_alerts.arn]

  # No ok_actions. Recovery here means an operator repaired the agent or the
  # glob; a datapoint ageing out of the window is not that, and a green
  # "recovered" page for an aged-out datapoint is the false-OK class this file
  # records repeatedly.
  ok_actions = []

  tags = {
    Environment = var.environment
    ManagedBy   = "terraform"
    Purpose     = "watch-the-watchers"
  }
}
