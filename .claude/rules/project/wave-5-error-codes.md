# Wave 5 Error Codes

> **Authority:** This file is the runbook target for the four ErrorCode
> variants added in the Wave 5 hardening implementation
> (`.claude/plans/active-plan-wave-5-indices-only.md` Item 9). Cross-ref
> test `crates/common/tests/error_code_rule_file_crossref.rs` requires
> every variant in `crates/common/src/error_code.rs::ErrorCode` to be
> mentioned in at least one rule file under `.claude/rules/`.

## CORE-PIN-01 — `core_affinity` pinning failed at boot

**Trigger:** during boot, the Wave 5 core-pin step iterates the four Tokio
worker threads (WS read loop, parser, ILP writer, "other") and calls
`core_affinity::set_for_current(core_id)` on each. If any call returns
`false` — typically because the host has fewer than 4 logical cores or
the kernel rejects the affinity request — this code fires. Severity::High.

**App behaviour:** the app continues to start without pinning. Latency
budgets in `quality/benchmark-budgets.toml` may not be met because the
kernel can preempt the WS read loop with unrelated work. The gauge
`tv_core_pinning_workers_pinned_total` reports the actual count of
successfully-pinned workers (target = 4).

**Triage:**
1. `nproc` — does the host have ≥ 4 logical cores? Wave 5 mandates
   AWS c7i.xlarge (4 vCPUs) and dev Mac mirroring 4 P-cores.
2. `cat /proc/<pid>/status | grep Cpus_allowed_list` — confirm the cgroup
   policy lets the process pin individual CPUs.
3. On Linux, check that the container is run with `--cpuset-cpus=0-3`
   (or equivalent) so the kernel will accept the affinity request.
4. Restart the app; CORE-PIN-01 either clears or repeats with the same
   root cause.

**Auto-triage safe:** NO (Severity::High requires operator inspection).

**Source (planned):** Wave 5 Item 6 lands the runtime+pin code in
`crates/app/src/main.rs` and a `core_pinning::pin_workers` helper.

## CORE-PIN-02 — pinned worker drifted off its assigned core

**Trigger:** the 60s drift watchdog observed a Tokio worker running on a
core other than its recorded pin. Severity::Medium. Counter
`tv_core_pinning_drift_total` increments per detected drift, labelled
by `worker_kind` (`ws_read`, `parser`, `ilp_writer`, `other`).

**Why this happens:** Linux re-balances threads when a CPU goes idle, the
kernel scheduler may move a thread off its pinned core if the affinity
request was advisory rather than strict, or the cgroup limits changed
mid-session.

**Triage:**
1. `tv_core_pinning_drift_total` rate — single drifts are recoverable;
   sustained drift > 1/min for 5 min indicates a systemic issue.
2. Verify cgroup `--cpuset-cpus` still pins all 4 cores; if a sidecar
   container started consuming a core, the operator must give it a
   different cpuset.
3. The drift watchdog re-applies the pin; if drift recurs, the kernel
   is rejecting the pin — escalate to CORE-PIN-01 root cause.

**Auto-triage safe:** YES (the drift watchdog re-pins automatically).

**Source (planned):** Wave 5 Item 6 — drift watchdog spawned alongside
the pin step.

## DEPTH-20-DYN-03 — depth-20 dynamic conn 5 top-50 selector returned empty / sub-50

**Trigger:** every 60s the depth-20 dynamic selector queries
`option_movers` filtered to `category = 'TOP_VOLUME'`, sorted by
`change_pct DESC`, with SENSEX (BSE_FNO) rows skipped, and reads up to
50 contracts. If the returned set has fewer than 50 contracts,
`Depth20Dyn03TopGainersEmpty` fires with the diagnostic
`{ returned_count, reason: "empty_after_sensex_skip" | "bucket_below_capacity" }`.
Severity::High.

**Why this fires:** universe-wide bear day where < 50 NSE_FNO contracts
have positive `change_pct`, OR the upstream `OptionMoversWriter` is
unhealthy (follow MOVERS-02 runbook in
`.claude/rules/project/wave-1-error-codes.md`). Outside market hours
this is expected — the runner uses
`is_within_market_hours_ist()` to suppress emission.

**App behaviour:** edge-triggered on rising edge only. Conn 5 keeps the
last-good top-50 set until the next successful query. The 4 single-side
index conns (NIFTY/BANKNIFTY CE/PE) are unaffected.

**Triage:**
1. `mcp__tickvault-logs__questdb_sql "select count(*) from option_movers where category = 'TOP_VOLUME' and ts > now() - 5m"`
   — empty / very low count means the writer is failing.
2. `tv_movers_writer_dropped_total{stage="append"}` — non-zero indicates
   schema drift or QuestDB ILP failure.
3. If movers data is healthy, this is a market-condition signal, not a
   bug. Do NOT relax the `change_pct` filter; Wave 5 Option B is locked.

**Auto-triage safe:** YES (the next 60s cycle either recovers or the
operator follows MOVERS-02).

**Source (planned):** Wave 5 Item 4 — `crates/core/src/instrument/depth_20_top_gainers_selector.rs`.

## DEPTH-200-DYN-01 — depth-200 dynamic top-5 selector returned fewer than 5

**Trigger:** same 60s scheduler runs the same SENSEX-skipped TOP_VOLUME
+ `change_pct DESC` query but reads only the top 5 contracts. If the
returned set has fewer than 5 contracts, `Depth200Dyn01TopGainersEmpty`
fires. Severity::High. Edge-triggered.

**App behaviour:** the 5 depth-200 conns each subscribe to one contract;
they keep their last-good gainer set until the next successful query.
No `Swap200` command is issued.

**Triage:** identical to DEPTH-20-DYN-03 above (same upstream table,
same selector, smaller K).

**Auto-triage safe:** YES.

**Source (planned):** Wave 5 Item 5 — `crates/core/src/instrument/depth_200_top_gainers_selector.rs`.

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

1. `mcp__tickvault-logs__prometheus_query "tv_websocket_connections_active"`
   — if depth-200 count is 0, this is auth/handshake; follow Cause 1.
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
