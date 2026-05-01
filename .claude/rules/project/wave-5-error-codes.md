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

## Cross-references

- `.claude/plans/active-plan-wave-5-indices-only.md` Items 4, 5, 6, 9
- `.claude/rules/project/wave-4-shared-preamble.md` (charter)
- `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory matrix)
- `crates/common/src/error_code.rs::ErrorCode` (`CorePin01PinningFailedAtBoot`,
  `CorePin02WorkerDrifted`, `Depth20Dyn03TopGainersEmpty`,
  `Depth200Dyn01TopGainersEmpty`, `PrevClose03BootRoutingAssertion` variants)
