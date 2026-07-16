# Per-Minute REST Pipeline — Adversarial Audit (2026-07-14)

> **Date:** 2026-07-14
> **Scope:** the per-minute REST 1m pipeline (Dhan spot/chain + Groww spot/chain/contract),
> its token/self-heal chain, infra failure modes, and the alarm delivery chain —
> 60 scenario cells (18 Dhan + 17 Groww + 15 infra + 10 self-heal).
> **Method:** 4 parallel adversarial auditors (dhan-rest, groww-rest, infra, self-heal)
> + 1 refuter (15/15 claims confirmed, 1 weakened) + 1 O(1)-honesty agent; consolidated
> by a synthesizer. Refutation report wins on conflicts.
> **Committed for durability** from the session scratchpad by the follow-up alarm-gaps PR
> (GAP-01/GAP-05/GAP-03 fixes reference this register).


Consolidated from: audit-dhan-rest.md, audit-groww-rest.md, audit-infra.md, audit-selfheal.md, audit-refutation.md, audit-complexity.md. Refutation report wins on conflicts (D2 downgraded).

## 1. Executive summary

- 60 scenario cells audited: 18 Dhan (D) + 17 Groww (G) + 15 infra (I) + 10 self-heal (S).
- Verdicts: **51 HANDLED / 7 PARTIAL / 2 GAP** (PARTIAL: D2, D3, D8, D16, G3, S1, S9; GAP: I9 clock-skew, S8 Telegram-degraded).
- Refuter: **15/15 claims CONFIRMED (12 assigned + 3 extra), 0 refuted, 1 weakened** — D2 "zero-human recovery YES" downgraded to PARTIAL (holds on happy mint path only).
- VIX (operator question): a VIX-only failure can NEVER page or starve the 3 core indices — skipped, counted, warned once daily, in the digest — but it never reaches Telegram (log-sink only, GAP-12).
- Token auto re-login: YES — worst-case dead-token window ≈ 30 min on the happy path (watchdog detect + ONE forced re-mint); if that single mint FAILS, the token stays dead for the REST OF THE SESSION (sweep backstop is lane-only, renewal loop halts after 5 CB cycles) until the 16:30 stop → next-boot retry-forever mint (GAP-01/02/04).
- O(1) honesty: code surface is clean (all cold-path O(N) bounded + flagged, one exemplary `O(1)-EXEMPT` flag); exactly **1 overstated doc claim** — `docs/architecture/o1-per-feed-uniqueness-dedup-mapping.md:38` "QuestDB DEDUP = amortized O(1) hash upsert" (vendor-internal, Assumed-as-Verified) — GAP-27.
- Single biggest systemic weakness: REST-leg paging is app-emitted Telegram ONLY (no CloudWatch backstop for SPOT1M/CHAIN/AUTH-GAP-05) compounded by no alarm on Telegram drops (GAP-01/03/05).
- GAP register: 31 deduplicated gaps — 2 HIGH, 11 MEDIUM, 16 LOW, 2 INFO. Zero CRITICAL.

## 2. Unified verdict table

| ID | Scenario (≤8 words) | Handled? | Evidence (one file:line) | Test (fn name or —) | Alert + broker-tagged? | O-complexity (honest) | Zero-touch recovery? | Gap ref |
|---|---|---|---|---|---|---|---|---|
| D1 | Token expired/invalid at boot | HANDLED | dhan_rest_stack.rs:519-570 | ratchet_spot1m_spawn_is_config_gated_inside_post_market_seam | DH-901 CW alarm→Telegram, broker/account-tagged | n/a (cold path) | YES — retry-forever mint + sweep backfill | — |
| D2 | Token expires mid-session between fires | PARTIAL | mid_session_watchdog.rs:87 | test_failure_edge_pages_once_at_threshold_then_stays_silent | HIGH Telegram + DH-901 alarm; broker-tagged | O(1) arc-swap load (C12) | Happy path ~30 min; mint-failure = session-dead | GAP-02, GAP-04 |
| D3 | Token invalid during 15:33:30 sweep | PARTIAL | spot_1m_rest_boot.rs:1923-1932 | test_sweep_missing_minutes_full_session_and_tail_gap | log-only (sweep_failed/incomplete), no page | n/a (cold path) | NO — day's rows stay absent | GAP-08, GAP-09 |
| D4 | HTTP 429 storm, spot leg | HANDLED | constants.rs:1641 | test_ladder_worst_case_429_schedule_stays_inside_sid_budget | counted + escalation HIGH; http-429 broker-tagged | O(1) rungs, 19.45s<20s const-assert (C3) | YES — bounded backoff, next minute | — |
| D5 | 429 on both legs same minute | HANDLED | option_chain_1m_boot.rs:1230-1262 | test_wait_until_chain_fire_signal_wakes_early_and_fallback_bounds | CHAIN-02 edge; 429 in log msg only | O(1) rungs + 2.5s fallback | YES — legs independent, retry next minute | GAP-15 |
| D6 | 2xx empty body / zero candles | HANDLED | spot_1m_rest_boot.rs:395-409 | test_classify_empty_body_no_rows_vs_stale | minute_failed log + lag histogram; vendor-lag tagged | O(rows), 2 MiB cap (C2) | YES — backfill + sweep when served | — |
| D7 | 5xx / transport timeout | HANDLED | spot_1m_rest_boot.rs:1318-1343 | test_declared_len_within_cap_rejects_oversize_content_length | error counted; escalation HIGH page | O(1) bounded timeouts/budget | YES — next minute retries | — |
| D8 | Malformed JSON / silent schema drift | PARTIAL | dhan_intraday_parse.rs:106-114 | parse_intraday_1m_candles_rejects_length_mismatch_and_malformed | spot misattributed vendor-empty; chain typed parse error | O(rows) reject-all (C2) | YES transient; no drift diagnosis (spot) | GAP-10 |
| D9 | Partial columnar arrays (length mismatch) | HANDLED | dhan_intraday_parse.rs:94-103 | parse_intraday_1m_candles_rejects_length_mismatch_and_malformed | via empty classification (D6 caveat) | O(rows) all-or-nothing | YES — empty minute, repaired later | — |
| D10 | One index failing persistently | HANDLED | spot_1m_rest_boot.rs:738-785 | test_sid_not_served_global_outage_neither_counts_nor_resets | Spot1mSidNotServed HIGH Telegram, vendor-tagged | O(1) streak (C9) | Detection yes; serving vendor-side | GAP-23 |
| D11 | All four fail 3 consecutive minutes | HANDLED | spot_1m_rest_boot.rs:1862-1864 | test_minute_fully_failed_requires_fetch_and_persist_ok | Spot1mFetchDegraded HIGH (persist-gated); status/body attached | O(1) edge (C9) | Page yes; repair via backfill/sweep | GAP-03 |
| D12 | INDIA VIX Dhan-side specifics | HANDLED | constants.rs:1588 | test_partial_minute_three_ok_one_empty_is_not_fully_failed_or_edge_counted | sid_not_served HIGH names the symbol | O(1); chain const-asserted VIX-free | Detection automated; serving vendor-side | — |
| D13 | Chain entitlement revoked mid-session | HANDLED | option_chain_1m_boot.rs:1446-1461 | test_is_entitlement_reject_classification | ChainEntitlementAbsent HIGH; broker/account-tagged (DH-902/806) | n/a (cold path) | Next-boot re-warm; day down by design | — |
| D14 | Expirylist warmup fails at boot | HANDLED | option_chain_1m_boot.rs:579-620 | test_expirylist_warmup_fail_closed_keeps_leg_dormant | CHAIN-04 ERROR + HIGH Telegram | O(1) — 3 bounded attempts | Down-for-day; next boot auto re-warms | — |
| D15 | Boundary skipped (overrun/suspend/clock step) | HANDLED | spot_1m_rest_boot.rs:1499-1536 | test_count_missed_boundaries_counts_each_overrun_minute | boundary_skipped ERROR; feeds escalation edge | O(1) | YES spot (backfill+sweep); chain absent by design | — |
| D16 | Dhan serving same-day candles DELAYED | PARTIAL | spot_1m_rest_boot.rs:941-1046 | test_sweep_recovers_vendor_late_1529_minute | empty_stale logs name lag (broker-tagged) | O(4×375×rows) once/day sweep (C5) | Bounded — only if served by 15:33:30 | GAP-08 |
| D17 | Scheduler task panics/dies | HANDLED | spot_1m_rest_boot.rs:2194-2226 | classify_join_exit unit tests | task_respawn ERROR + counter (log-only) | n/a (cold path) | Unwind builds yes; release = systemd restart | GAP-24 |
| D18 | HTTP client build fails (fd/TLS) | HANDLED | spot_1m_rest_boot.rs:1367-1384 | wiring guards (non-stub, spawn-gated) | client_build log-only; RESOURCE alarms host-side | n/a (cold path) | YES — supervisor retry 30s | — |
| G1 | SSM token read fails at fire | HANDLED | groww_spot_1m_boot.rs:885-935 | test_should_reread_token_respects_60s_floor | token_read log (ours-tagged); 3-min HIGH page | O(1) cached read, 60s pace (C12) | YES — next fire re-reads | GAP-03 |
| G2 | Token stale (minter Lambda dead) | HANDLED | groww_spot_1m_boot.rs:1026-1034 | test_fire_token_path_auth_reject_short_circuits_via_mock | HIGH Telegram minter-attributed + sidecar 10-min alert | O(1) (C12) | NO by design — external minter recovers | GAP-31 |
| G3 | 429 / shared BruteX bucket | PARTIAL | constants.rs:2202-2225 | test_groww_min_gap_wait_ms_engages_only_inside_gap | per-429 warn "shared bucket"; tenant unattributable | O(1) pacing, ≤6 req/s const-asserted | YES — next boundary; no out-polling | GAP-15, GAP-16 |
| G4 | VIX-only failure (operator question) | HANDLED | groww_spot_1m_boot.rs:359-390 | test_minute_edge_tally_vix_failure_never_pages_core_failure_does | log-sink only — never reaches Telegram | O(≤1000 watch entries)/min (C8) | YES leg-side; vendor-not-serving named honestly | GAP-12 |
| G5 | Chain anchor stale >5 min | HANDLED | groww_contract_1m_boot.rs:581-598 | test_partition_fresh_anchors_names_stale_and_keeps_fresh | edge-latched warn + audit rows (log-only) | O(strikes≤400) (C7) | YES — next good chain minute heals | GAP-26 |
| G6 | Chain expiry unresolved from master | HANDLED | groww_option_chain_1m_boot.rs:1428-1522 | test_resolve_groww_chain_targets_full_and_partial_degrade | GrowwChain1mExpiryUnresolved HIGH, broker-tagged | n/a (cold path warmup) | YES — tomorrow's boot re-warms | — |
| G7 | Truncation / token collision / budget | HANDLED | groww_contract_1m_boot.rs:1087-1102 | test_select_contracts_for_minute_cap_truncates_furthest_first_and_counts | counted + audited; book-unresolved HIGH once/day | O(strikes) linear, cap 30, no per-minute sort (C7) | YES — next minute re-selects | — |
| G8 | Thin-strike 2xx-empty minute | HANDLED | groww_contract_1m_boot.rs:1388-1413 | test_fetch_bounded_found_empty_and_parse_failure_via_mock | counted + target_absent audit row; never fabricated | O(1) empty arm | YES — lookback backfill; no contract sweep | GAP-25, GAP-30 |
| G9 | Groww live WS DOWN, REST running | HANDLED | main.rs:3102+3302 | ratchet_groww_spot1m_spawn_is_config_gated_and_dual_site | blame split: WS=FEED-STALL, REST own edges | n/a (structural) | YES — REST independent of sidecar | — |
| G10 | Connection-cap penalty vs REST | HANDLED | groww_spot_1m_boot.rs:1513-1531 | wiring guards (broker-side unverifiable) | 429 counters catch account-level coupling | n/a (structural) | YES — HTTP client unaffected by WS penalty | — |
| G11 | Unknown candle-ts wire format | HANDLED | groww_spot_1m_boot.rs:473-511 | test_parse_groww_candle_ts_rejects_garbage_and_implausible | malformed counted; ts_form counters answer probe | O(rows), plausibility-gated (C2) | YES — never mis-bucketed | — |
| G12 | Persist failure contract/chain tables | HANDLED | option_contract_1m_rest_persistence.rs:404-461 | test_contract1m_flush_when_disconnected_errors_and_discards_pending | SPOT1M-02/CHAIN-03 + persist-gated edge HIGH | O(1) append; discard-pending defense | YES — DEDUP-idempotent re-fetch | GAP-03 |
| G13 | rest_fetch_audit write failure | HANDLED | groww_spot_1m_boot.rs:1461-1490 | test_rest_fetch_audit_dedup_key_ts_first_segment_feed_leg_and_outcome_in_key | log-only; verified never feeds paging edge | O(rows≤cap) per fire (C10) | YES — forensics-only loss window | GAP-28 |
| G14 | Groww sweep fails/incomplete | HANDLED | groww_spot_1m_boot.rs:2190-2489 | test_sweep_missing_minutes_noop_full_and_tail | sweep_failed/incomplete log-only; named gap rows | O(SIDs×minutes×rows) once/day (C5) | PARTIAL — manual re-run is the floor | GAP-08 |
| G15 | Box restart mid-minute (Groww) | HANDLED | groww_spot_1m_boot.rs:2036-2041 | test_persist_tracker_commit_max_merge_and_double_persist_idempotent | respawn counter; pre_boot rows named | O(1) tracker commit (C4) | YES spot; contract minutes = named absences | GAP-30 |
| G16 | 3-minute edge, typed pages, re-arm | HANDLED | constants.rs:1655 | test_chain_failure_edge_pages_at_three_and_recovers_once | HIGH Telegram/episode; NO cause attribution in body | O(1) edge (C9) | YES — edge self re-arms, continuous retry | GAP-03, GAP-17 |
| G17 | Decision-freshness gate | HANDLED | groww_spot_1m_boot.rs:1958-1965 | grep-verified: zero crates/trading consumers | n/a (structural gate) | O(1) per-row stamp | n/a — nothing to enforce yet | — |
| I1 | QuestDB DOWN during ILP flush | HANDLED | spot_1m_rest_persistence.rs:386-432 | test_spot1m_discard_pending_clears_buffer_and_count | SPOT1M-02 + edge HIGH; no ring/spill claim | O(1) discard-pending | Partial — next minute + sweep; sub-watermark holes manual | GAP-07 |
| I2 | WAL-suspended table (ACKs, rows invisible) | HANDLED | wal_suspension_watcher.rs:389 | watcher module unit tests | WAL-SUSPEND-01 CW alarm → Telegram | n/a (60s probe) | Detect yes; RESUME WAL manual by design | GAP-20 |
| I3 | Ensure-DDL fails at boot | HANDLED | spot_1m_rest_persistence.rs:142-196 | DDL-string unit tests | SPOT1M-02 log-only; dup window named verbatim | n/a (boot-only) | Self-heal next boot; dup rows undetected | GAP-14 |
| I4 | Disk full on the box | HANDLED | partition_manager.rs:108-132 | retention/dedup storage guards | disk_used_high 75% CW alarm + watchers | n/a | Partial — EBS grow operator-manual | — |
| I5 | Box restart mid-minute (infra view) | HANDLED | spot_1m_rest_boot.rs:486-493 | test_sweep_missing_minutes_full_session_and_tail_gap | boundary_skipped + respawn counters | O(1) watermark (C4) | Backfill+sweep; Dhan blind window unnamed | GAP-07, GAP-11 |
| I6 | Deploy landing mid-session | HANDLED | deploy-aws.yml:220-248 | workflow guard steps + re-gate at swap | deploy skip/abort in summary; watchdog Lambda | n/a | YES — queues to post-close crons | — |
| I7 | systemd watchdog interplay | HANDLED | tickvault.service:86-99 | infra.rs:2481-2576 payload tests | crash-loop → liveness alarm pages | n/a | YES one-off; >8/10min crash-loop manual | — |
| I8 | IST-midnight boundary | HANDLED | spot_1m_rest_boot.rs:1404-1410 | test_fire_is_fresh_rejects_stale_and_midnight_wrap | boundary_skipped coded log | O(1) per-fire date derive | YES — no aggregator coupling | — |
| I9 | Clock skew MID-SESSION | GAP | main.rs:6969-7009 (boot-only) | boot-probe tests only (BOOT-03) | none — unattributed empty_stale symptoms only | n/a | NO — undetected until symptoms | GAP-13 |
| I10 | Holiday / weekend / Muhurat | HANDLED | trading_calendar.rs:58-133 | calendar unit tests + spot_1m_day_is_over | correct silence; Muhurat dormancy unsignalled | O(1) gate per iteration | YES — self-skip | GAP-21 |
| I11 | Duplicate / UPSERT collisions | HANDLED | dedup_segment_meta_guard.rs:322 | every_persisted_table_dedup_key_must_include_feed | ensure-failure window loud (I3) | O(1) key build; DB-side vendor-internal (C1) | Self-heal next ensure; dup sweep manual | GAP-14, GAP-27 |
| I12 | BOTH brokers down simultaneously | HANDLED | market-hours-liveness-alarm.tf:140-160 | aws_alarm_semantics_guard (rules-cited) | app-independent CW alarms; per-feed attribution split | n/a | Detect zero-human; upstream fix manual | GAP-22 |
| I13 | Sweep vs cross-verify 429 coordination | HANDLED | spot_1m_rest_boot.rs:512-520 | test_sweep_fire_instant_clears_cross_verify_burst_window | 429 counters + DEGRADED/BLIND verdict | O(1) const-assert (zero-slack equality) | YES — one bounded pass each | — |
| I14 | Metrics/alarm delivery-chain honesty | HANDLED | error-code-alarms.tf:80-224 | error_code_paging_filter_drift_guard | REST codes Telegram-only; no CW backstop | n/a | Only while app + notifier alive | GAP-03, GAP-05 |
| I15 | 16:30 auto-stop vs post-session tasks | HANDLED | spot_1m_rest_boot.rs:517-520 | const-assert + scoreboard trigger sanitizer tests | scorecard-abort HIGH page; auto-stop WARN | O(1) const-asserted schedule | YES — ≥45 min margin | — |
| S1 | AUTH-GAP-05 mid-session forced re-mint | PARTIAL | dhan_rest_stack.rs:593-599 | decide_remint_* (5 tests) | app Telegram only (HIGH + CRITICAL); no CW alarm | O(1) FSM (threshold→latch→lock→cooldown) | Happy ≤~30 min; mint-failure = session-dead | GAP-01, GAP-02, GAP-04 |
| S2 | AUTH-GAP-04 TOTP rotated externally | HANDLED | token_manager.rs:330-353 | error_code_paging_filter_drift_guard.rs:682 | AUTH-GAP-04 CW alarm; ours/operator-tagged | n/a (boot retry loop) | NO honest — SSM fix; auto-resumes after | — |
| S3 | Peer holds dual-instance lock | HANDLED | dhan_rest_stack.rs:361-486 | test_dhan_rest_peer_page_due_genuine_peer_pages_once_then_parks | DualInstanceDetected Telegram; RESILIENCE-01 log-only | O(1) patience ladder | Self-stale auto (TTL); live peer = park (design) | GAP-18 |
| S4 | SSM outage during token ops | HANDLED | dhan_rest_stack.rs:455-483 | backoff/coalesce tests :887-1009 | DH-901 CW alarm; lock-stall log-only | O(1) bounded backoff | YES transient — all loops resume | GAP-18 |
| S5 | Groww feed-stall self-heal | HANDLED | groww_sidecar_supervisor.rs:766 | test_should_restart_on_stall_* | 2 CW alarms; cause slugs ours/broker | O(1) feed-level checks | YES — kill+relaunch; reject bounded-loud | — |
| S6 | Supervisor task / process death | HANDLED | main.rs:3221-3265 | supervisor unit tests + classify_join_exit | respawn counters; liveness alarm backstop | n/a | YES tasks/one-off; crash-loop >8 manual | GAP-22, GAP-24 |
| S7 | Whole app dies 10:30 IST | HANDLED | market-hours-liveness-alarm.tf:140-160 | aws_alarm_semantics_guard | CW missing-data page ~5-6 min, app-independent | n/a | YES — systemd restarts in 3s | — |
| S8 | Telegram delivery degraded | GAP | notification/service.rs:358 | coalescer/service unit tests | NO alarm on drops; typed-only pages silenced | n/a | Partial — only alarmed codes survive | GAP-05 |
| S9 | Never-give-up vs terminal states | PARTIAL | token_manager.rs:1134-1153 | — | mixed — 4 terminal states (park, CB-halt, latch, day-down) | n/a | 4 terminal states need human/restart/next-boot | GAP-02, GAP-04 |
| S10 | Spot task dead → chain fallback | HANDLED | option_chain_1m_boot.rs:1230 | test_wait_until_chain_fire_signal_wakes_early_and_fallback_bounds (Groww local 2-arm; shared core) | dead spot pages separately (SPOT1M-01) | O(1) 2.5s fallback timer | YES — one broken leg never silences other | — |

## 3. Consolidated GAP register

| GAP id | Severity | Description (≤20 words) | Affected scenarios | Proposed fix (one line) | Owner/next step |
|---|---|---|---|---|---|
| GAP-01 | HIGH | AUTH-GAP-05 has no CloudWatch alarm — forced-re-mint page depends entirely on app Telegram | S1, D2 | Add `auth-gap-05` entry to `error_code_alerts` in error-code-alarms.tf | terraform PR (not crates-gated) |
| GAP-02 | HIGH | 4h token sweep (`force_renewal_if_stale`) is lane-only — absent from dhan_rest_stack; no re-mint backstop | S1, S9, D2 | Spawn the sweep loop in `run_dhan_rest_stack` Phase 3 | fix PR after 15:46 IST (needs plan approval) |
| GAP-03 | MEDIUM | SPOT1M-01/02 + CHAIN-01..04 log-sink-only — no CW alarm anywhere (refuter-verified, all reports) | D11, G1, G12, G16, I14 | One `error_code_alerts` map entry per escalation ERROR line | terraform PR (not crates-gated) |
| GAP-04 | MEDIUM | Watchdog re-mint latch is one-shot per episode; a failed mint never retries while token dead | S1, S9, D2 | Re-arm latch after N cycles with 125s cooldown | fix PR after 15:46 IST (needs plan approval) |
| GAP-05 | MEDIUM | No alarm on `tv_telegram_dropped_total` — broken bot silently kills every typed-event page | S8, I14 | One metrics-log delta-extraction alarm (auth-failed-alarm.tf pattern) | terraform PR (not crates-gated) |
| GAP-06 | MEDIUM | `tv_token_valid` gauge poller lane-only — no token-validity metric on today's dhan-off boots | S1 | Spawn `spawn_token_health_gauge_poller` in REST stack Phase 3 | fix PR after 15:46 IST (needs plan approval) |
| GAP-07 | MEDIUM | Mid-session persist holes BELOW the watermark never auto-repaired (sweep covers only above watermark) | I1, I5, D3 | Derive sweep missing-set from a QuestDB per-SID SELECT | fix PR after 15:46 IST (needs plan approval) |
| GAP-08 | MEDIUM | One-shot 15:33:30 sweep; no second/late/next-day repair — late vendor serving loses the day | D3, D16, G14 | Bounded second sweep ~16:00 + typed HIGH on sweep_incomplete | fix PR after 15:46 IST (needs plan approval) |
| GAP-09 | MEDIUM | Post-close token death unhealable same-day (watchdog market-hours-gated); compounds GAP-08 | D3 | One post-close re-mint window covering the sweep instant | fix PR after 15:46 IST (needs plan approval) |
| GAP-10 | MEDIUM | Spot columnar schema drift indistinguishable from vendor-empty; skipped rows uncounted | D8 | Add `parse_shape_failed` classification + skipped-row counter | fix PR after 15:46 IST (needs plan approval) |
| GAP-11 | MEDIUM | Dhan spot/chain legs emit ZERO rest_fetch_audit rows — Dhan blind windows silent in forensics | I5, D16 | Port Groww `audit_append_best_effort` sites into the Dhan legs | fix PR after 15:46 IST (needs plan approval) |
| GAP-12 | MEDIUM | VIX failure never reaches Telegram (warn!-only, no typed event, no alarm) | G4, D12 | Typed `GrowwSpot1mVixNotServed` event at the sweep latch, if page-worthy | operator decision |
| GAP-13 | MEDIUM | Clock-skew enforcement boot-only; mid-session chrony drift undetected, symptoms unattributed | I9 | Re-run `probe_clock_skew` on 15-min cadence, coded warn >2s | fix PR after 15:46 IST (needs plan approval) |
| GAP-14 | LOW | No automated duplicate detection for REST tables after an ensure-DDL-failure window | I3, I11 | Extend tf_consistency `duplicate_key` scan to the three REST tables | fix PR after 15:46 IST (needs plan approval) |
| GAP-15 | LOW | 429 asymmetries: Dhan chain no 429 counter/backoff; Groww spot lacks +2s extra backoff | D5, G3 | Classify chain 429 + counter; mirror the +2s backoff | fix PR after 15:46 IST (needs plan approval) |
| GAP-16 | LOW | No 429-rate alert — partial-429 regime (BruteX co-tenancy) that never fully fails is counters-only | G3, D4 | Alarm on `tv_*_rate_limited_total` rate, or digest verdict rung | terraform PR (not crates-gated) |
| GAP-17 | LOW | Groww escalation Telegram carries no cause attribution (SSM-ours vs minter vs 429-bucket) | G16 | Thread dominant episode `error_class` into the Degraded payload | fix PR after 15:46 IST (needs plan approval) |
| GAP-18 | LOW | RESILIENCE-01/03 + `tv_dhan_rest_stack_up` log/gauge-only — parked/never-up stack invisible to CloudWatch | S3, S4 | Alarm on `tv_dhan_rest_stack_up < 1` in boot window | terraform PR (not crates-gated) |
| GAP-19 | LOW | Off-hours token invalidation invisible until 09:00; first ~15 spot minutes can fail | S1 | Accept (documented), or one watchdog probe at stack-up | operator decision |
| GAP-20 | LOW | WAL/disk/OOM/resource monitors skipped on FAST crash-recovery boot arm (moot Dhan-OFF) | I2 | Move monitor block above the fast-arm gate (flagged follow-up) | fix PR after 15:46 IST (needs plan approval) |
| GAP-21 | LOW | Muhurat evening sessions: REST legs dormant, zero rows, zero runtime signal | I10 | One info "REST legs dormant during Muhurat" log, or scope-lock | operator decision |
| GAP-22 | LOW | SLO composite publisher `start_dhan_lane`-gated — dormant on Dhan-OFF boots | I12, S6 | Hoist to process-global prefix or dhan_rest_stack | fix PR after 15:46 IST (needs plan approval) |
| GAP-23 | LOW | Session-scoped edge/streak state resets on task respawn (flapping task may never page) | D10, D11 | Accept (respawn counter makes flap loud), or persist streaks | operator decision |
| GAP-24 | LOW | Release `panic = "abort"`: in-process respawn never fires for prod panics (honestly documented) | D17, S6 | Accept — recovery is systemd restart + loop/sweep re-entry | operator decision |
| GAP-25 | LOW | Contract-leg 2xx-empty fire arm has no dedicated mock test | G8 | One mock body without target minute asserting Empty + target_absent | fix PR after 15:46 IST (needs plan approval) |
| GAP-26 | LOW | Anchor-stale fire-arm emission (audit rows + latch) is TEST-EXEMPT wiring | G5 | Fire test with aged anchor asserting Skipped row + warn | fix PR after 15:46 IST (needs plan approval) |
| GAP-27 | LOW | Overstated doc claim: "QuestDB DEDUP = amortized O(1) hash upsert" — vendor-internal, Assumed-as-Verified | I11, C1 | Reword to "vendor-side commit-time dedup; idempotent by key" | docs-only PR |
| GAP-28 | LOW | Stale doc bound: rest_fetch_audit "≤3 rows/minute/leg" — spot is 4 targets since VIX | G13, C10 | Update the doc comment bound (4 + sweep/gap rows) | docs-only PR |
| GAP-29 | LOW | Scoreboard rest-leg digest query has no SQL LIMIT / body cap (structural bound only) | C11 | Add LIMIT + byte cap (tf_consistency envelope pattern) | fix PR after 15:46 IST (needs plan approval) |
| GAP-30 | INFO | Contract leg has no post-session sweep by design — pre-restart minutes are permanent named absences | G8, G15 | Accept (selection is minute-scoped; named honestly) | operator decision |
| GAP-31 | INFO | Groww token recovery structurally external (never-mint lock) — bruteX minter Lambda is the healer | G2 | Accept — by-design division of ownership, honestly attributed | operator decision |

## 4. Compact chat table

| ID | Scenario | Handled? | Evidence | Alert | O | Zero-touch | Gap |
|---|---|---|---|---|---|---|---|
| D1 | Token dead at boot | HANDLED | dhan_rest_stack.rs:519 | DH-901 CW, broker-tagged | n/a | YES | — |
| D2 | Token dies mid-session | PARTIAL | mid_session_watchdog.rs:87 | HIGH TG + DH-901 | O(1) | Happy path only | 02,04 |
| D3 | Token dead at sweep | PARTIAL | spot_1m_rest_boot.rs:1923 | log-only | n/a | NO | 08,09 |
| D4 | 429 storm spot | HANDLED | constants.rs:1641 | counted + HIGH edge | O(1) | YES | — |
| D5 | 429 both legs | HANDLED | option_chain_1m_boot.rs:1230 | CHAIN-02 edge | O(1) | YES | 15 |
| D6 | 2xx empty body | HANDLED | spot_1m_rest_boot.rs:395 | lag-tagged logs + edge | O(rows) | YES | — |
| D7 | 5xx / timeout | HANDLED | spot_1m_rest_boot.rs:1318 | counted + edge | O(1) | YES | — |
| D8 | Schema drift | PARTIAL | dhan_intraday_parse.rs:106 | misattributed vendor-empty | O(rows) | Partial | 10 |
| D9 | Array length mismatch | HANDLED | dhan_intraday_parse.rs:94 | via empty class | O(rows) | YES | — |
| D10 | One index dead | HANDLED | spot_1m_rest_boot.rs:738 | HIGH TG vendor-tagged | O(1) | Detect yes | 23 |
| D11 | All 4 fail 3min | HANDLED | spot_1m_rest_boot.rs:1862 | HIGH TG persist-gated | O(1) | YES | 03 |
| D12 | VIX Dhan-side | HANDLED | constants.rs:1588 | sid_not_served HIGH | O(1) | Detect yes | — |
| D13 | Entitlement revoked | HANDLED | option_chain_1m_boot.rs:1446 | HIGH TG broker-tagged | n/a | Next boot | — |
| D14 | Expirylist warmup fail | HANDLED | option_chain_1m_boot.rs:579 | CHAIN-04 HIGH TG | O(1) | Next boot | — |
| D15 | Boundary skipped | HANDLED | spot_1m_rest_boot.rs:1499 | coded ERROR, feeds edge | O(1) | YES | — |
| D16 | Vendor serving delayed | PARTIAL | spot_1m_rest_boot.rs:941 | lag-named logs + edge | O(N)/day | Only ≤15:33:30 | 08 |
| D17 | Task panics | HANDLED | spot_1m_rest_boot.rs:2194 | respawn counter, log | n/a | Unwind/systemd | 24 |
| D18 | Client build fails | HANDLED | spot_1m_rest_boot.rs:1367 | log-only | n/a | YES | — |
| G1 | SSM read fails | HANDLED | groww_spot_1m_boot.rs:885 | ours-tagged log + edge | O(1) | YES | 03 |
| G2 | Minter token stale | HANDLED | groww_spot_1m_boot.rs:1026 | HIGH TG minter-tagged | O(1) | External minter | 31 |
| G3 | 429 shared bucket | PARTIAL | constants.rs:2202 | warn, tenant-unattributable | O(1) | YES | 15,16 |
| G4 | VIX-only failure | HANDLED | groww_spot_1m_boot.rs:359 | log-sink only | O(N)/min | YES leg-side | 12 |
| G5 | Anchor stale >5min | HANDLED | groww_contract_1m_boot.rs:581 | latched warn + audit | O(N≤400) | YES | 26 |
| G6 | Expiry unresolved | HANDLED | groww_option_chain_1m_boot.rs:1428 | HIGH TG broker-tagged | n/a | Next boot | — |
| G7 | Truncation/collision/budget | HANDLED | groww_contract_1m_boot.rs:1087 | counted + HIGH once/day | O(N) cap 30 | YES | — |
| G8 | Thin-strike empty | HANDLED | groww_contract_1m_boot.rs:1388 | counted + audit row | O(1) | YES | 25,30 |
| G9 | WS down, REST alive | HANDLED | main.rs:3102+3302 | blame split clean | n/a | YES | — |
| G10 | WS penalty vs REST | HANDLED | groww_spot_1m_boot.rs:1513 | via 429 counters | n/a | YES | — |
| G11 | Unknown ts format | HANDLED | groww_spot_1m_boot.rs:473 | counted, never mis-bucketed | O(rows) | YES | — |
| G12 | Persist failure | HANDLED | option_contract_1m_rest_persistence.rs:404 | coded + edge page | O(1) | YES | 03 |
| G13 | Audit write fails | HANDLED | groww_spot_1m_boot.rs:1461 | log-only, never edges | O(rows) | YES | 28 |
| G14 | Groww sweep fails | HANDLED | groww_spot_1m_boot.rs:2190 | log-only, named gaps | O(N)/day | Manual floor | 08 |
| G15 | Restart mid-minute | HANDLED | groww_spot_1m_boot.rs:2036 | respawn + pre_boot rows | O(1) | YES spot | 30 |
| G16 | Edge + typed pages | HANDLED | constants.rs:1655 | HIGH TG, no cause | O(1) | YES | 03,17 |
| G17 | Freshness gate | HANDLED | groww_spot_1m_boot.rs:1958 | n/a structural | O(1) | n/a | — |
| I1 | QuestDB down flush | HANDLED | spot_1m_rest_persistence.rs:386 | SPOT1M-02 + edge | O(1) | Partial | 07 |
| I2 | WAL-suspended table | HANDLED | wal_suspension_watcher.rs:389 | CW alarm | n/a | Detect only | 20 |
| I3 | Ensure-DDL fails | HANDLED | spot_1m_rest_persistence.rs:142 | log-only, window named | n/a | Next boot | 14 |
| I4 | Disk full | HANDLED | partition_manager.rs:108 | disk_used_high CW | n/a | Partial | — |
| I5 | Restart mid-minute | HANDLED | spot_1m_rest_boot.rs:486 | counters | O(1) | Backfill+sweep | 07,11 |
| I6 | Mid-session deploy | HANDLED | deploy-aws.yml:220 | workflow summary | n/a | YES | — |
| I7 | systemd watchdog | HANDLED | tickvault.service:86 | liveness alarm | n/a | YES | — |
| I8 | IST midnight | HANDLED | spot_1m_rest_boot.rs:1404 | coded log | O(1) | YES | — |
| I9 | Clock skew mid-session | GAP | main.rs:6969 boot-only | none | n/a | NO | 13 |
| I10 | Holiday/Muhurat | HANDLED | trading_calendar.rs:58 | correct silence | O(1) | YES | 21 |
| I11 | UPSERT collisions | HANDLED | dedup_segment_meta_guard.rs:322 | loud ensure-fail | O(1)+vendor | Next ensure | 14,27 |
| I12 | Both brokers down | HANDLED | market-hours-liveness-alarm.tf:140 | app-independent CW | n/a | Detect yes | 22 |
| I13 | Sweep vs cross-verify 429 | HANDLED | spot_1m_rest_boot.rs:512 | counters | O(1) | YES | — |
| I14 | Alarm-chain honesty | HANDLED | error-code-alarms.tf:80 | Telegram-only REST codes | n/a | Conditional | 03,05 |
| I15 | 16:30 stop vs tasks | HANDLED | spot_1m_rest_boot.rs:517 | abort HIGH page | O(1) | YES | — |
| S1 | Forced re-mint | PARTIAL | dhan_rest_stack.rs:593 | TG only, no CW | O(1) | Happy path only | 01,02,04 |
| S2 | TOTP rotated | HANDLED | token_manager.rs:330 | AUTH-GAP-04 CW alarm | n/a | NO (honest) | — |
| S3 | Peer holds lock | HANDLED | dhan_rest_stack.rs:361 | TG; RESILIENCE-01 log-only | O(1) | Stale auto; peer park | 18 |
| S4 | SSM outage | HANDLED | dhan_rest_stack.rs:455 | DH-901 CW | O(1) | YES transient | 18 |
| S5 | Feed-stall self-heal | HANDLED | groww_sidecar_supervisor.rs:766 | 2 CW alarms, blame slugs | O(1) | YES | — |
| S6 | Supervisor/process death | HANDLED | main.rs:3221 | counters + liveness alarm | n/a | YES mostly | 22,24 |
| S7 | App dies 10:30 | HANDLED | market-hours-liveness-alarm.tf:140 | CW ~5-6 min | n/a | YES | — |
| S8 | Telegram degraded | GAP | notification/service.rs:358 | no drop alarm | n/a | Partial | 05 |
| S9 | Terminal states | PARTIAL | token_manager.rs:1134 | mixed | n/a | 4 terminal states | 02,04 |
| S10 | Spot dead, chain fallback | HANDLED | option_chain_1m_boot.rs:1230 | spot pages separately | O(1) | YES | — |
