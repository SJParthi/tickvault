# =============================================================================
# Error-code log-filter alarms — the error! -> page route, RESTORED 2026-07-06
# =============================================================================
# THE GAP THIS CLOSES (zero-page incident, 2026-07-06):
#   The CloudWatch-only migration (#O1/#O2/#O3) retired the
#   Loki -> Alertmanager -> Telegram route with NO replacement, so an `error!`
#   reached only the log sinks. On 2026-07-06 the 12:00 IST REST-CANARY-01
#   probe failure produced ZERO pages. These 12 log metric filters + alarms
#   (8 on 2026-07-06; +AGGREGATOR-DROP-01 on 2026-07-09; +WAL-SUSPEND-01 on
#   2026-07-10; -REST-CANARY-01 retired + CROSS-VERIFY-1M-01/-02 +
#   TICK-CONSERVE-01 on 2026-07-14) on
#   the /tickvault/<env>/app log group (the errors.jsonl stream) restore the
#   route: error! -> errors.jsonl -> CloudWatch Logs -> filter -> tv_errcode_*
#   metric -> alarm (<=5 min) -> SNS tv-alerts -> Telegram webhook Lambda.
#
# HONEST ALARM COUNT: this file takes the REAL total from 33 -> 41 alarms
# (45 with the reconnect-storm + feed-stall-restarts + readiness-lambda-errors
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
# 2026-07-14 UPDATE (REST-pipeline adversarial audit, GAP-01 + GAP-03 —
# docs/audits/2026-07-14-rest-pipeline-adversarial-audit.md): +5 entries ->
# 17 filters + 17 alarms (~+$0.50/mo; on top of the same-day REST-CANARY-01
# retirement and the automation-gaps +3 above). The audit's single biggest systemic
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
  #   - ws-reinject-01: emitted exactly ONCE per boot (wal_reinject.rs abort
  #     arm); the condition -- frames staged in WAL replaying/ with a
  #     dead/wedged consumer -- persists until the NEXT boot. OK ~15 min
  #     later cannot mean recovered.
  #   - proc-01: a discrete kernel OOM-kill event; the memory pressure that
  #     caused it is not fixed by the episode aging out.
  #   - dh-906: a discrete per-order reject; OK = aged out, never "orders
  #     working again".
  #   - cross-verify-1m-01 / cross-verify-1m-02 / tick-conserve-01
  #     (2026-07-14): daily ONE-SHOT audit findings — the 15:31 IST
  #     cross-verify fires its mismatch/degraded lines once per run and
  #     the 15:40 IST conservation audit fires its residual once per day;
  #     the auto-OK ~15 min later can never mean the mismatch/residual was
  #     fixed. Recovery signal = the NEXT trading day's clean run.
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
    # FEED-STALL-01 (round-3 review fix, 2026-07-06): the ONLY ERROR-level
    # FEED-STALL-01 emission is the sidecar's own STORM escalation — the 6th+
    # rapid restart inside a 300s ANCHORED-RESET window
    # (>STALL_RESTART_STORM_MAX=5, groww_sidecar_supervisor.rs — the window
    # start resets when it elapses; round-13 correction: NOT a sliding
    # window, so a burst straddling the anchor can defer the escalation by
    # up to ~one extra 300s window). Per-restart emissions are warn!-level
    # and NEVER reach the ERROR-only errors.jsonl sink, so this filter counts
    # storm-escalation LINES, not restarts. The earlier "Sum >= 3 restarts per
    # 15 min" tuning could therefore never see 3-5 restarts/15 min (zero ERROR
    # lines) — a Rule-11 false-OK envelope. Retuned: ONE storm line pages
    # (threshold 1 per 300s; the Rust detector already debounces at >5
    # restarts/5 min, so a single self-heal restart still never pages).
    # Tripwire floor (span math, round-13 — the earlier "~50s" used 300/6
    # average-rate math): 6 restarts span 5 gaps <= the 300s window, so
    # cycles <= ~60s can escalate. The
    # ">=3 restarts per 15 min" pager — counting EVERY restart, warn! + error!
    # alike — is the separate tv-<env>-feed-stall-restarts counter alarm
    # (feed-stall-restart-alarm.tf).
    "feed-stall-01" = {
      pattern     = "{ $.code = \"FEED-STALL-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = true
      desc        = "FEED-STALL-01 STORM escalation: the Groww sidecar's own storm detector fired (>5 stall-restarts within a 5-min ANCHORED-reset window - the 6th+ rapid restart emits the only ERROR-level FEED-STALL-01 line; per-restart emissions are warn!-level and invisible to this filter). The provider keeps closing the socket at <=~60s/cycle (span math: 6 restarts span 5 gaps <= 300s; anchored-reset, not sliding - a burst straddling the anchor can defer the escalation by up to ~one extra 300s window). A single self-heal restart never pages. The >=3-restarts-per-15-min pager (all restart cadences) is tv-<env>-feed-stall-restarts (feed-stall-restart-alarm.tf). Check credential/entitlement. Runbook: .claude/rules/project/feed-stall-watchdog-error-codes.md"
    }
    "ws-reinject-01" = {
      pattern     = "{ $.code = \"WS-REINJECT-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # round-4: emitted exactly ONCE per boot; the staged-WAL condition persists until the NEXT boot - auto-OK would be a Rule-11 false recovery
      desc        = "WS-REINJECT-01: boot WAL re-injection ABORTED - consumer dead/wedged; frames stay staged in WAL replaying/ and re-replay next boot. NO recovered/OK page: the code fires once per boot and the condition persists until the next boot, so the auto-OK ~15 min later only means the single datapoint aged out - recovery is the NEXT boot's clean replay. Runbook: .claude/rules/project/ws-reinject-error-codes.md"
    }
    "proc-01" = {
      pattern     = "{ $.code = \"PROC-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # round-4: discrete OOM-kill event - auto-OK means the episode aged out, not that the memory pressure is resolved (Rule-11 false-recovery)
      desc        = "PROC-01: kernel OOM kill detected in this cgroup (Severity Critical). NO recovered/OK page: an OOM kill is a discrete event, so the auto-OK ~15 min later only means the episode aged out - the leak/pressure behind it is not thereby fixed; watch tv_process_rss_bytes + host memory alarms for the real recovery. Runbook: .claude/rules/project/wave-4-error-codes.md"
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
    "aggregator-drop-01" = {
      pattern     = "{ $.code = \"AGGREGATOR-DROP-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-09: discrete permanent data loss - the dropped sealed candles do not come back when the episode ages out (Rule-11 false-recovery; PROC-01 precedent)
      desc        = "AGGREGATOR-DROP-01: sealed candle(s) DROPPED after ring + spill + DLQ ALL failed (Severity Critical - the only silent-data-loss path for sealed candles; by definition the host is out of memory AND out of disk AND data/dlq/ is unwritable). NO recovered/OK page: the loss is permanent - the auto-OK ~15 min later only means the episode aged out. Triage: docker/host state, df -h /data, ls -la data/spill/ data/dlq/; if the host is healthy and dirs writable, restart the app. Counter-side pager: tv-<env>-seal-writer-dropped (seal-drop-alarm.tf). Runbook: .claude/rules/project/wave-6-error-codes.md"
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
    # CROSS-VERIFY-1M-01/-02 + TICK-CONSERVE-01 (added 2026-07-14 —
    # automation-gaps PR-3): the 2026-07-10 automation audit found all
    # three High post-market audit codes were LOG-SINK-ONLY. Emit sites:
    # crates/app/src/cross_verify_1m_boot.rs (the 15:31 IST run's
    # mismatch/degraded arms + the audit-append/final-flush failure arms)
    # and crates/app/src/tick_conservation_boot.rs (the 15:40 IST
    # reconciler's Leak arm). All three are daily one-shot audit findings
    # -> ok_recovery = false (the aggregator-drop-01 / ws-reinject-01
    # precedent): the auto-OK ~15 min after the single datapoint ages out
    # can never mean the mismatch/residual was fixed (Rule-11
    # false-recovery); the real recovery signal is the NEXT trading day's
    # clean run.
    "cross-verify-1m-01" = {
      pattern     = "{ $.code = \"CROSS-VERIFY-1M-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-14: daily one-shot audit finding - the mismatched candles do not fix themselves when the episode ages out (Rule-11 false-recovery)
      desc        = "CROSS-VERIFY-1M-01: post-market 1m cross-verify found OHLCV mismatches between our live candles_1m and Dhan's intraday history (15:31 IST daily run; also fires when a mismatch audit row/flush could not persist). Track the trend, not the absolute count - a stable baseline is sampling noise, a spike or sustained Open/Close drift is real. Open data/cross-verify/cross-verify-1m-<date>.csv + the cross_verify_1m_audit table. NO recovered/OK page: a daily one-shot finding - the auto-OK ~15 min later only means the episode aged out; recovery = the next trading day's clean run. Runbook: .claude/rules/project/cross-verify-1m-error-codes.md"
    }
    "cross-verify-1m-02" = {
      pattern     = "{ $.code = \"CROSS-VERIFY-1M-02\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-14: daily one-shot audit finding - the day's coverage gap persists after the episode ages out (Rule-11 false-recovery)
      desc        = "CROSS-VERIFY-1M-02: post-market 1m cross-verify fetch DEGRADED - Dhan intraday REST errored/rate-limited/empty (or no JWT at run time) for a material fraction of the spot SIDs, so the day's OHLCV parity signal is partial or blind. Check Dhan Data-API health + the captured sample_failure field in the errors-jsonl stream. NO recovered/OK page: the run fires once per day - the auto-OK ~15 min later only means the episode aged out; recovery = the next trading day's clean run. Runbook: .claude/rules/project/cross-verify-1m-error-codes.md"
    }
    "tick-conserve-01" = {
      pattern     = "{ $.code = \"TICK-CONSERVE-01\" && $.level = \"ERROR\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-14: daily one-shot data-accounting finding - the residual is a discrete event; aging out never means the accounting balanced (Rule-11 false-recovery)
      desc        = "TICK-CONSERVE-01: the 15:40 IST daily tick-conservation audit found a positive residual - frames Dhan delivered (in the WAL) never reached the processor (delivery_residual: recovered by next-boot WAL replay) and/or ticks entered the pipeline but reached no known outcome (outcome_residual: a true in-process leak - investigate). Read the per-stage numbers in tick_conservation_audit + the 60s conservation ledger logs. NO recovered/OK page: a daily one-shot finding - the auto-OK ~15 min later only means the episode aged out; recovery = the next trading day's balanced row. Runbook: .claude/rules/project/tick-conservation-audit-error-codes.md"
    }
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
    # the per-minute spot-1m REST legs (Dhan spot + Groww spot + Groww
    # contract — all emit SPOT1M-01) page HIGH via app Telegram at the
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
      desc        = "SPOT1M-01 escalation: a per-minute REST 1m candle leg (Dhan spot, Groww spot, or Groww contract - read the feed/leg fields in the errors-jsonl stream) fully failed 3+ consecutive minutes (persist-gated: fetch-ok-but-lost rows count as failed). Fires once per episode (edge-latched). Triage: cross-check DH-901 (REST surface/token; the REST canary was retired 2026-07-14 with the Dhan noise lock), tv_spot1m_fetch_total outcome rates, QuestDB health for persist-gated episodes. NO recovered/OK page: recovery = the leg's typed recovery Telegram + rows landing again. Runbook: .claude/rules/project/rest-1m-pipeline-error-codes.md"
    }
    # CHAIN-02 escalation edge (added 2026-07-14 — REST-audit GAP-03):
    # same contract as spot1m-01-escalation for the option-chain legs
    # (Dhan + Groww). stage="escalation" only — per-minute sub-edge lines
    # deliberately unmatched.
    "chain-02-escalation" = {
      pattern     = "{ $.code = \"CHAIN-02\" && $.level = \"ERROR\" && $.stage = \"escalation\" }"
      period      = 300
      threshold   = 1
      eval        = 3
      dta         = 1
      ok_recovery = false # 2026-07-14: once-per-episode edge - same rationale as spot1m-01-escalation
      desc        = "CHAIN-02 escalation: a per-minute option-chain REST leg (Dhan or Groww - read the feed field) fully failed 3+ consecutive minutes (persist-gated). Fires once per episode (edge-latched). Triage: spot leg healthy + chain failing = chain-API-surface problem (entitlement wobble short of CHAIN-01, gateway); both failing = REST/token (AUTH-GAP runbooks). NO recovered/OK page: recovery = the typed ChainFetchRecovered Telegram + rows landing again. Runbook: .claude/rules/project/rest-1m-pipeline-error-codes.md"
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
  # cross-verify-1m-01/-02 + tick-conserve-01 [2026-07-14];
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
  # codes here (dh-901, auth-gap-04, feed-stall-01; ws-gap-07 retired PR-C2 2026-07-13) +
  # feed-stall-restarts. Exempt: the reconnect-storm alarm via
  # actions_enabled=false, and BOTH AWS/Lambda Errors watchman alarms
  # (readiness-errors + market-hours-gate-errors) via ok_actions=[]
  # (round-14 — their auto-OK is aged-out, never a fix).
  # Creation settling, NOT recoveries. Flagged
  # follow-up (not this PR): an OldStateValue == INSUFFICIENT_DATA
  # suppression branch in the telegram-webhook Lambda - benefits every
  # future alarm PR.
  ok_actions = each.value.ok_recovery ? local.app_alarm_ok : []
}
