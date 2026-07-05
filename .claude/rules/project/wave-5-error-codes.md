# Wave 5 Error Codes

> **Authority:** This file is the runbook target for the four ErrorCode
> variants added in the Wave 5 hardening implementation
> (`.claude/plans/active-plan-wave-5-indices-only.md` Item 9). Cross-ref
> test `crates/common/tests/error_code_rule_file_crossref.rs` requires
> every variant in `crates/common/src/error_code.rs::ErrorCode` to be
> mentioned in at least one rule file under `.claude/rules/`.

## DEPTH-20-DYN-03 — depth-20 dynamic top-volume selector returned a sub-capacity set

> **⚠ RETIRED 2026-07-06 (audit finding).** The movers runtime feeding this
> code (`movers_1m` view + `depth_dynamic_pipeline_v2`) was deleted
> 2026-05-19 (AWS-lifecycle PRs #2-#4) and the `Depth20Dyn03TopGainersEmpty`
> ErrorCode variant was deleted with it. Content below retained for
> historical audit.

**Trigger:** every 60s the unified depth-dynamic pipeline (`depth_dynamic_pipeline_v2`,
introduced by the depth-dynamic redesign — supersedes the legacy "conn 5
single-dynamic" wording) queries the `movers_1m` materialised view through
the shared `depth_dynamic_top_volume_selector` module. The query is
config-driven: `[depth_20.dynamic.universe]` in `config/base.toml` selects
the cohort by `exchange_segments` (default `["NSE_FNO"]`), `cohort_size`,
`rerank_metric`, and `window_secs`. Stage 2 ranks the cohort by the
configured metric and takes the top `conns * sids_per_conn` (= 5 × 50 =
250 today). If the resulting set has fewer SIDs than capacity,
`Depth20Dyn03TopGainersEmpty` fires with diagnostic
`{ returned_count, reason: "cohort_below_capacity" | "empty_after_segment_filter" }`.
Severity::High. Edge-triggered (rising edge only).

**Why this fires:** universe-wide bear day where the configured exchange
segments produce fewer than 250 contracts in the cohort window, OR the
upstream `movers_unified_pipeline` writer is unhealthy. Check
`tv_movers_writer_dropped_total` + `movers_pipeline` task logs. Phase
4b (2026-05-05) retired the legacy MOVERS-02 runbook reference here.
Outside market hours this is expected — the unified pipeline uses
`is_within_market_hours_ist()` to suppress emission.

**App behaviour:** the 5 dynamic depth-20 conns retain their previous
`current_set` (each holding up to 50 SIDs). The diff algorithm in
`DynamicSubscriptionState` issues no Add/Remove frames when the next
set is empty — last-good subscriptions stay live. The new
`tv_depth_dynamic_set_size{feed="depth-20"}` Prometheus gauge reflects
the current size; the alert `tv-depth-dynamic-set-size-low` fires
when it drops below 5 for 5+ minutes.

**Triage:**
1. `mcp__tickvault-logs__questdb_sql "select count(*) from movers_1m where exchange_segment = 'NSE_FNO' and ts > now() - 5m"`
   — empty / very low count means the upstream writer is failing.
2. `tv_movers_writer_dropped_total{stage="append"}` — non-zero indicates
   schema drift or QuestDB ILP failure.
3. If movers data is healthy, this is a market-condition signal, not a
   bug. To widen the cohort, edit `[depth_20.dynamic.universe].cohort_size`
   or add segments (e.g. `exchange_segments = ["NSE_FNO", "BSE_FNO"]`).
   These are runtime-overridable per environment.

**Auto-triage safe:** YES (the next 60s cycle either recovers or the
operator inspects the unified `movers_pipeline` task and `MoversWriter`
metrics; the legacy MOVERS-02 code retired in Phase 4b).

**Source:** `crates/core/src/instrument/depth_dynamic_top_volume_selector.rs`,
`crates/app/src/depth_dynamic_pipeline_v2.rs`.

## DEPTH-200-DYN-01 — depth-200 dynamic top-volume selector returned a sub-capacity set

> **⚠ RETIRED 2026-07-06 (audit finding).** The movers runtime feeding this
> code was deleted 2026-05-19 (AWS-lifecycle PRs #2-#4) and the
> `Depth200Dyn01TopGainersEmpty` ErrorCode variant was deleted with it.
> Content below retained for historical audit.

**Trigger:** the same unified pipeline runs depth-200 with
`PoolShape { conns: 5, sids_per_conn: 1 }` and `[depth_200.dynamic.universe]`
config. Stage 2 takes the top 5 from the `movers_1m` cohort. If the result
has fewer than 5 SIDs, `Depth200Dyn01TopGainersEmpty` fires.
Severity::High. Edge-triggered.

**App behaviour:** the 5 depth-200 conns each subscribe to one contract.
The diff algorithm preserves the previous `current_set` when the next
set is empty — no `RemoveSubscriptions200` is issued. The
`tv_depth_dynamic_set_size{feed="depth-200"}` gauge reflects the current
size.

**Triage:** identical to DEPTH-20-DYN-03 above (same upstream
`movers_1m` table, same selector module, same config-driven filter,
smaller K).

**Auto-triage safe:** YES.

**Source:** `crates/core/src/instrument/depth_dynamic_top_volume_selector.rs`,
`crates/app/src/depth_dynamic_pipeline_v2.rs`.

## PREVCLOSE-03 — boot-time prev-close routing assertion failed (Item 13)

**Trigger:** at boot, the subscription plan is scanned against the per-segment
prev-close routing matrix from `live-market-feed.md`:

| Segment | Required feed mode | Where prev_close lives |
|---|---|---|
| IDX_I | Ticker | standalone code 6 packet |
| NSE_EQ | Quote OR Full | bytes 38-41 (Quote) / 50-53 (Full) of the packet |
| NSE_FNO | Full | bytes 50-53 of the Full packet |
| BSE_FNO | Full | bytes 50-53 of the Full packet |

If any subscribed instrument has a `(segment, feed_mode)` pair outside the
allowed cell of this matrix, PREVCLOSE-03 fires Severity::Critical and the
boot HALTS — refusing to start a pipeline that loses prev_close for half
the universe is cheaper than recovering from a corrupted day's snapshot.

**Triage:**

1. The Telegram event names the offending instrument (`security_id`,
   `display_label`) + actual feed_mode + expected feed_mode for its segment.
2. Most common cause: operator set `[subscription] feed_mode = "Quote"` in
   `config/base.toml`, which silently downgrades NSE_FNO + BSE_FNO from
   Full and loses the bytes-50-53 prev_close. Set back to `"Full"`.
3. Less common: a code-side regression where `make_derivative_instrument`
   stamps the wrong mode. The 3 ratchet tests in
   `subscription_planner::tests::test_idx_i_subscriptions_use_ticker_mode`,
   `test_nse_eq_subscriptions_use_quote_or_full`, and
   `test_nse_fno_bse_fno_subscriptions_use_full_mode` fail the build first
   in this case.

**Auto-triage safe:** NO (Severity::Critical halts boot — operator action
required).

**Source (Item 13):** the 3 unit-level ratchet tests in
`crates/core/src/instrument/subscription_planner.rs::tests`. The runtime
boot emission in `crates/app/src/main.rs` is a follow-up (the tests pin
the contract today; runtime emission is purely defence-in-depth for the
case where someone bypasses the unit tests).

## VOLUME-MONO-01 — cumulative volume monotonicity breach (Item 26 L1)

**Trigger:** within a single IST trading day, a tick arrives for some
`(security_id, exchange_segment)` pair where `volume[i] < volume[i-1]`.
The Wave 5 in-memory L1 guard
(`crates/core/src/pipeline/volume_monotonicity_guard.rs`) detects this on
the hot path (O(1) per tick) and emits Severity::High with the violating
delta.

**Why this matters:** Dhan Ticket #5525125 + Cowork-verified evidence
(Tracks 1, 3, 4) strongly suggest the `volume` field at bytes 22-25 of
the Quote/Full packet is **cumulative-day total** since session open, not
per-tick incremental. If the cumulative invariant breaks mid-session,
either:
1. Dhan changed the semantic without notice (escalate via Item 26 L3
   Dhan support email — already drafted, awaiting operator Gmail send).
2. Our parser regressed on the byte offset (run the parser unit-tests +
   the live Mon May 4 09:45 IST monotonicity SELECT in
   `docs/operator/track-2-monotonicity-select.md`).

**App behaviour:** the guard does NOT stop the pipeline — ticks continue
flowing, the breach is logged + counted (`tv_volume_monotonicity_breaches_total`),
and the `last_seen` map preserves the previous high-water mark so a single
decrease does not silently reset the baseline. Operator decides whether
to halt.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — read the violating
   `(security_id, segment, previous, current, delta)` tuples.
2. Cross-check against the parser unit-tests
   (`crates/core/src/parser/quote.rs` bytes 22-25 + `full.rs` bytes 22-25).
   If the byte offsets are correct, the breach is Dhan-side.
3. Run `docs/operator/track-2-monotonicity-select.md` SELECT against
   QuestDB for the affected `(security_id, segment)` tuple over the
   recent 30 minutes. Confirm whether the breach reproduces in QuestDB
   (which would indicate persistent semantic change).
4. If reproducible: trigger Item 26 L3 escalation. If not: parser bug —
   open follow-up issue.

**Auto-triage safe:** YES (Severity::High; the guard log + counter +
audit are sufficient for the operator to act).

**Source (Item 26 L1):**
- `crates/core/src/pipeline/volume_monotonicity_guard.rs`
- `crates/common/src/error_code.rs::Volume01MonotonicityBreach`

**Item 26 L2 (NSE bhavcopy daily cross-check):** runbook captured in
`docs/operator/track-2-monotonicity-select.md` and Item 29 fold-in;
implementation deferred to a follow-up sub-PR.

**Item 26 L3 (Dhan support email):** drafted via PR #414. Operator must
Gmail-send the GitHub link to apihelp@dhan.co. The L3 escalation runs
on Track 2 verdict from the Mon May 4 SELECT.

## DEPTH200-SMOKE-01 — boot-time depth-200 smoke test failed (PR-B, 2026-05-02)

**Trigger:** at boot, the smoke-test task in
`crates/app/src/boot_smoke_test.rs::run_smoke_test_loop` polls a shared
`Arc<AtomicU64>` that all 5 dynamic depth-200 receivers increment on
every frame. After `SMOKE_DEADLINE_SECS` (= 60s) of in-market polling
the counter is still 0. Severity::Critical. Edge-trigger semantics:
fires exactly once per process lifetime.

**Why it exists:** post-PR #427 depth-200 reuses the shared TOTP/APP
token (Dhan Ticket #5610706 removed the SELF-only gate). DEPTH200-SMOKE-01
is the boot-time confirmation that the gate is still removed. If Dhan
ever silently regresses, the operator gets a Critical signal at boot
instead of waiting until the 09:16:30 IST market-open self-test.

**Three causes, in descending likelihood:**

| Cause | Diagnosis | Fix |
|---|---|---|
| Dhan re-enabled the SELF-only gate | `WebSocketDisconnected` events also firing for kind=depth-200 with reset reasons; `tv_websocket_connections_active{kind="depth-200"}` stays at 0 | Re-open Ticket #5610706 with Dhan support; in the meantime `git revert <sha>` of PR #427 to restore the SELF-token workaround |
| All 5 SecurityIds far OTM (server-side filtering) | depth-200 conns show as Connected but Dhan never streams; depth-rebalancer not yet computed ATM | Wait for the 60s rebalancer cycle; smoke runs ONCE per boot, will not re-fire |
| WS pool subscribe failed to dispatch | conns stuck in DEFERRED mode (`security_id: None` never resolved); `DEPTH-200-DYN-01` also firing | Follow DEPTH-200-DYN-01 runbook above |

**Triage:**

1. Inspect `tv_websocket_connections_active` in CloudWatch metrics (from
   the app `/metrics` exporter) — if depth-200 count is 0, this is
   auth/handshake; follow Cause 1.
2. `mcp__tickvault-logs__tail_errors` — search for `WebSocketDisconnected`
   with kind=depth-200 in the boot window. If present, it's a server-side
   reset; escalate to operator.
3. `mcp__tickvault-logs__questdb_sql "select count(*) from option_movers
   where category = 'TOP_VOLUME' and ts > now() - 5m"` — empty/very-low
   means the upstream `OptionMoversWriter` isn't producing the gainer
   set the depth-200 selector reads; this is Cause 3.
4. If Causes 1-3 all rule out → operator inspects the depth-200 selector
   for a logic bug that picks far-OTM SecurityIds.

**Auto-triage safe:** NO (Severity::Critical). Mitigation requires
operator decision (`git revert` vs Dhan support escalation vs config
change).

**Source:** `crates/app/src/boot_smoke_test.rs`,
`crates/common/src/error_code.rs::Depth200Smoke01NoFramesAtBoot`,
`.claude/triage/error-rules.yaml::depth200-smoke-01-no-frames-at-boot-escalate`.

## PHASE2-READY-01 — Phase 2 readiness pre-flight failed (PR-G, 2026-05-02)

**Trigger:** the Phase 2 readiness pre-flight task in
`crates/core/src/instrument/phase2_readiness_check.rs::classify_readiness`
returned `ReadinessOutcome::Failed` at 09:13:01 IST. One or more of
the 11 forward-looking pre-conditions for the upcoming market-open
milestones (09:15 / 09:15:30 / 09:16:30 IST) are not met.
Severity::Critical. Single-fire per process — fires exactly once at
09:13:01 IST (1 second after `Phase2Complete`).

**Why it exists:** before this check, the operator only learned at
09:16:30 IST (after the market-open self-test) that something was
wrong — by which point the market had already opened with
broken state and ticks had been missing for ~90 seconds. This
pre-flight buys ~120 seconds of operator-actionable warning BEFORE
NSE opens.

**The 11 checks:**

| # | Check | Failure cause |
|---|---|---|
| 1 | `token_expiry_headroom` | Token < 4h validity → mid-session DH-901 cascade |
| 2 | `main_feed_pool` | < 5 main feed conns → ticks won't flow at 09:15 |
| 3 | `depth_20_pool` | < 4 depth-20 conns → depth-20 ATM±24 missing |
| 4 | `depth_200_pool` | < 5 depth-200 conns → depth-200 frames missing |
| 5 | `order_update_ws` | Order WS disconnected or stale heartbeat |
| 6 | `questdb_ilp` | QuestDB unreachable or last write > 60s ago |
| 7 | `preopen_buffer_coverage` | < 95% of F&O stocks have a 09:00–09:12 price |
| 8 | `phase2_plan_quality` | Phase 2 added 0 stocks OR > 5% skipped |
| 9 | `subscribe_ack_rate` | Dhan rejected one or more subscribe batches |
| 10 | `rescue_ring_health` | Rescue ring > 10% used at 09:13 (overflow risk) |
| 11 | `composite_slo_score` | Composite score < 0.95 (hidden upstream issue) |

**Triage:**

1. Read the Telegram payload — `failed: [name1, name2, ...]` plus
   `details:` with `expected/observed` for each failing check.
2. Each failing check name maps to a specific runbook section:
   - `token_expiry_headroom` → `AUTH-GAP-03` runbook
   - `main_feed_pool` / `depth_20_pool` / `depth_200_pool` →
     `disaster-recovery.md` Scenarios 5/6
   - `order_update_ws` → `WS-GAP-01` runbook
   - `questdb_ilp` → `BOOT-01` / `BOOT-02` runbooks
   - `preopen_buffer_coverage` → `depth-subscription.md` 2026-04-22
     Updates §5 (pre-open buffer + REST fallback)
   - `phase2_plan_quality` → `PHASE2-01` runbook
   - `subscribe_ack_rate` → check Dhan rate-limit (DH-904) +
     Dhan account status
   - `rescue_ring_health` → `STORAGE-GAP-03` runbook
   - `composite_slo_score` → `SLO-02` runbook (named the weakest
     dimension already)
3. Operator has ~120 seconds (until 09:15:00 IST) to act before
   NSE opens. If the issue is not fixable in that window, the
   operator at least KNOWS the next 4 minutes will partially fail.

**Auto-triage safe:** NO (Severity::Critical). Mitigation requires
operator decision per the failing-check-specific runbook.

**Source:**
- `crates/core/src/instrument/phase2_readiness_check.rs`
- `crates/common/src/error_code.rs::Phase2Ready01PreflightFailed`
- `crates/core/src/notification/events.rs::Phase2ReadinessFailed` /
  `Phase2ReadinessPassed`
- `.claude/triage/error-rules.yaml::phase2-ready-01-preflight-failed-escalate`

## Cross-references

- `.claude/plans/active-plan-wave-5-indices-only.md` Items 4, 5, 6, 9
- `.claude/rules/project/wave-4-shared-preamble.md` (charter)
- `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory matrix)
- `crates/common/src/error_code.rs::ErrorCode` (`CorePin01PinningFailedAtBoot`,
  `CorePin02WorkerDrifted`, `Depth20Dyn03TopGainersEmpty`,
  `Depth200Dyn01TopGainersEmpty`, `PrevClose03BootRoutingAssertion` variants)
