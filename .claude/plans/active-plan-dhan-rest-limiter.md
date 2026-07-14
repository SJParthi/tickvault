# Implementation Plan: shared self-tuning Dhan Data-API rate limiter (3→2 rps) + spot retry-shaping + fetch-mode flag

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — 2026-07-14 pacing directive relayed via the coordinator session (verbatim below)

> **Operator directive (2026-07-14, verbatim as relayed):** pace Dhan to 3
> requests/sec (tunable DOWN to 2), spread overflow into the next second(s),
> route the option-chain API through the SAME limiter, with
> incremental/decremental self-tuning — *"if it accepts max 3 or 2, stick to
> that and split it up"*. Dhan-ONLY; Groww untouched.

> **Per-item guarantee matrix:** this plan cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row + 7-row
> matrices apply to every item below; each item names its ratchet tests).

## THE HONESTY CONSTRAINT (bake into every claim)

Today (2026-07-14) the Dhan spot-1m leg was **0/980 (0%)** with **~244 wasted
429s** — the ladder re-fires ~20 req/min in the all-empty regime; the first
two ladder steps are 0.7s apart → ~8 requests land in a rolling second; the
sid%3-era jitter collapses to 2 offsets for SIDs {13,25,51,21}. The option
chain was **735/735 (100%)**. **THE LIMITER FIX ELIMINATES THE 429 WASTE AND
MAKES US A GOOD API CITIZEN — IT DOES NOT MAKE DHAN SERVE SAME-DAY CANDLES.**
The 429s are a symptom of hammering empties, not the root cause of the 0%.
A separate sweep-discriminator verdict (~15:40 IST) decides per-minute vs
batch-catch-up architecture — this PR makes that a CONFIG MODE, not a
rewrite. Never imply "3 rps fixes spot-1m".

## Design

One process-wide async token-bucket limiter (`crates/app/src/dhan_data_api_limiter.rs`)
that EVERY Dhan Data-API REST fire in the per-minute pipelines passes
through: spot-1m per-minute fires, ladder re-polls, the 15:33:30 post-session
sweep, the #1524 serving-delay diagnostic probes, AND the option-chain
per-minute fires + expirylist warmup/probe. The routing choke points are
`spot_1m_rest_boot::spot_1m_fetch_once` and
`option_chain_1m_boot::chain_fetch_once` — every enumerated caller funnels
through those two fns, so ONE `acquire().await` per fn covers the whole
scope. The chain's 1-unique-per-3s per-underlying `min_gap_wait_ms` stays
LAYERED ON TOP, unchanged.

- **Limiter core:** pre-built `governor::DefaultDirectRateLimiter` cells,
  one per legal rps level (2..=4), selected through `arc_swap::ArcSwap`
  (governor quotas are fixed at construction — switching = swapping the
  active pre-built cell). `acquire().await` = `until_ready()` on the active
  cell, so overflow naturally spills into later seconds (GCRA). Process-wide
  via `OnceLock` (`shared_dhan_data_api_limiter()`); configured at both
  Dhan spawn seams (`main.rs::spawn_post_market_tasks` +
  `dhan_rest_stack.rs`) from `[dhan_data_api] target_rps` (default 3, legal
  range 2..=4, validated in `ApplicationConfig::validate`).
- **Self-tuning (pure `RpsTuner` FSM):** observed HTTP-429s feed it
  (`record_429` at the two fetch fns' real-StatusCode 429 arms). ≥3 429s
  within a rolling 2-minute window at the current rate → step DOWN to the
  2 rps floor (the operator's literal "step DOWN to 2 rps"; at the default
  target 3 this is also exactly one level). A clean streak of 10 minutes at
  a reduced rate → step back UP one level toward the config target (cap).
  Every transition logged ONCE (edge — `error!` down / `info!` up, never
  per-request) + `tv_dhan_data_api_rps` gauge +
  `tv_dhan_data_api_tuner_transitions_total{direction}` counter.
- **Retry-shaping (spot leg):** (a) STALE-WATERMARK CUTOFF — if ladder
  attempt N returns a parsed day payload whose last-candle watermark equals
  the previous attempt's (including two consecutive zero-row payloads), STOP
  the ladder for that minute (re-polling cannot outrun a serving delay; the
  #1524 serving-lag data justifies it). (b) ADAPTIVE DEGRADE — after 5
  consecutive no-data minutes (zero SIDs served their own candle), drop to
  single-attempt-per-minute (no ladder) until ANY success re-arms the full
  ladder; loud one-time transition logs + `tv_spot1m_ladder_degraded` gauge.
  The old fixed slot-jitter stagger is KEPT but demoted to a harmless
  schedule de-sync — the shared limiter is now the actual pacing authority
  (removing the jitter would churn const-asserts for zero behaviour gain;
  noted in the PR body).
- **Mode flag:** `[spot_1m_rest] fetch_mode = "per_minute" | "batch_catchup"`
  (serde default `per_minute`). Batch mode replaces per-minute fires with a
  sweep-style catch-up every `batch_interval_minutes` (default 5): one
  day-window fetch per SID through the limiter, persist everything new above
  the per-SID persisted watermark — a thin mode wrapper over the EXISTING
  sweep machinery (`sweep_missing_minutes` + the extracted
  `sweep_sids_above_watermark` helper shared with `run_post_session_sweep`),
  not new fetch logic. Default stays `per_minute` pending the operator's
  sweep-discriminator ruling (~15:40 IST today).
- **Scope boundary (honest):** the boot-time prev-day fetch
  (`prev_day_ohlcv_boot`, its own 4-rps governor cell) and the 15:31 bulk
  cross-verify (`cross_verify_1m_boot`, its own `OrderRateLimiter`) are NOT
  routed through this limiter — they were not in the operator's enumerated
  scope and run in disjoint time windows; unifying them is a flagged
  follow-up. Groww is UNTOUCHED (its own vendor, its own budgets).

## Edge Cases

- **429 burst inside one minute:** window sum trips the step-down once;
  window cleared on transition so one burst can never cascade; floor 2 is
  hard (never below).
- **`target_rps` misconfigured (0, 1, 7):** `ApplicationConfig::validate()`
  REJECTS at boot (range 2..=4); the limiter's `configure` additionally
  clamps (belt + braces, loud warn) so a bypassed validate can never build a
  0-rps cell (`NonZeroU32`).
- **Multi-minute idle gaps (tuner):** advancing across N idle minutes counts
  bounded clean minutes (cap: one step-up per advance call) — deterministic,
  no unbounded loop on an overnight gap.
- **Reconfigure with a lower cap while stepped up / higher cap while
  stepped down:** `set_cap` clamps current to the new cap downward
  immediately; upward movement still requires the clean-streak (never a
  free jump past observed 429 pressure).
- **In-flight `acquire` across a level swap:** the in-flight waiter finishes
  on the OLD cell (bounded — one request); every subsequent acquire uses the
  new level. Documented, not fought.
- **Watermark cutoff on a healthy-serving day:** the cutoff trades the
  1.5–6s marginal-appearance window (attempt 3+) for quota whenever the
  watermark provably did not move between two polls ~0.7s apart; the
  per-minute backfill + 15:33:30 sweep remain the repair paths. Stated
  plainly in module docs + PR body.
- **Degrade re-arm:** ANY SID serving its own candle re-arms the full
  ladder instantly (single success is a regime change signal).
- **Batch mode late wake / suspend:** a late batch fire recomputes its
  catch-up ceiling from the wall clock (that is the point of catch-up);
  boundaries stay grid-aligned so a slow cycle never double-fires.
- **Batch mode + chain sequencing:** the chain leg's 2.5s fallback timer
  already fires it per minute when the spot signal is absent — batch mode
  publishes the signal per cycle; the chain leg is unaffected either way.
- **Batch interval out of range (0, 999):** validate REJECTS outside 1..=60.
- **SID budget vs limiter wait:** worst-case queue depth at a minute
  boundary is ~7 concurrent requests (4 spot + 3 chain) → ≤ ~4s wait at the
  2 rps floor — inside both 20s budgets; documented, and the per-request
  timeout still bounds each leg.

## Failure Modes

- **Limiter cell exhausted / queueing:** `until_ready` waits — never an
  error, never a drop; the SID/underlying budgets still hard-bound each fire
  (a pathological queue surfaces as the EXISTING budget-exceeded failure,
  counted + coalesced-logged).
- **Tuner stuck at floor (Dhan keeps 429ing at 2 rps):** window keeps
  tripping; transitions are edge-logged once per episode (no spam); the
  existing `tv_spot1m_rate_limited_total` + SPOT1M-01/CHAIN-02 edges page.
- **Step-up flapping (clean 10 min → up → burst → down):** each direction
  logs once; the gauge + transitions counter make a flap visible; the
  10-minute clean-streak requirement bounds flap frequency to ≤1 cycle per
  ~12 min.
- **Degrade mode never exits (all-day empty regime):** it fires 4 requests
  per minute (one per SID) — the honest minimum probe cadence; the sweep +
  SPOT1M-01 escalation keep paging. Exit requires a real success, never a
  timer (a timer would re-open the waste).
- **Batch cycle fetch fails for every SID:** counted via the existing
  FailureEdge (3 consecutive fully-failed cycles → SPOT1M-01 escalation
  page, same event surface as per-minute mode).
- **Batch cycle persist flush fails:** poisoned-buffer discard (existing
  writer semantics), cycle counted fully-failed, watermarks NOT advanced —
  next cycle re-fetches idempotently (DEDUP UPSERT keys).
- **Process restart mid-session:** limiter + tuner + degrade state are
  process-scoped (fresh at target rps, full ladder) — the SAME envelope as
  the existing FailureEdge/PersistTracker session-scoped state; worst case
  is one window of re-learning (bounded, loud).

## Test Plan

Unit (in-module + config tests, all pure/deterministic):
- `test_dhan_data_api_config_default_is_3_and_round_trips`,
  `test_dhan_data_api_config_validate_rejects_out_of_range` (0/1/5 reject;
  2/3/4 pass) — `crates/common/src/config.rs`
- `test_spot1m_fetch_mode_defaults_per_minute_and_parses_batch`,
  `test_spot1m_batch_interval_validate_bounds` — `crates/common/src/config.rs`
- Tuner FSM: `test_tuner_steps_down_to_floor_on_429_burst`,
  `test_tuner_holds_below_threshold`,
  `test_tuner_steps_up_one_level_after_clean_streak`,
  `test_tuner_never_below_floor_never_above_cap`,
  `test_tuner_window_expires_old_429s`,
  `test_tuner_set_cap_clamps_current_downward`,
  `test_limiter_levels_prebuilt_and_quota_matches_rps`,
  `test_configure_clamps_out_of_range_target` —
  `crates/app/src/dhan_data_api_limiter.rs`
- Stale-watermark cutoff: `test_ladder_watermark_repeated_stops_on_equal`,
  `test_ladder_watermark_zero_rows_twice_repeats`,
  `test_ladder_watermark_advancing_does_not_stop`,
  `test_ladder_watermark_first_observation_never_stops` —
  `crates/app/src/spot_1m_rest_boot.rs`
- Adaptive degrade: `test_degrade_enters_after_five_no_data_minutes`,
  `test_degrade_any_success_rearms`, `test_degrade_attempt_count_single`,
  `test_degrade_does_not_reenter_page_spam` (edge semantics) —
  `crates/app/src/spot_1m_rest_boot.rs`
- Mode dispatch + batch scheduling: `test_next_batch_fire_grid_alignment`,
  `test_next_batch_fire_past_window_none`,
  `test_batch_cycle_fully_failed_semantics` —
  `crates/app/src/spot_1m_rest_boot.rs`
- Wiring ratchet: `crates/app/tests/dhan_data_api_limiter_wiring_guard.rs`
  — spot + chain fetch fns route through `shared_dhan_data_api_limiter()`
  (acquire + record_429 needles), both spawn seams configure the limiter
  from `config.dhan_data_api.target_rps`, the mode dispatch exists, and the
  chain min-gap layer survives.

Verify gates (paste verbatim in the PR): `cargo fmt --check`,
`cargo clippy --workspace -- -D warnings -W clippy::perf`,
`cargo test -p tickvault-common --lib`, `cargo test -p tickvault-app --lib`
(+ spot/chain filters), the wiring guards, `bash .claude/hooks/plan-gate.sh`.

## Rollback

- `[dhan_data_api] target_rps` has serde default 3 — deleting the section
  changes nothing; the limiter itself cannot be config-disabled (it IS the
  good-citizen contract), but reverting the PR restores the exact pre-PR
  call graph (the two fetch fns lose one `acquire` line each).
- `[spot_1m_rest] fetch_mode` serde-defaults to `per_minute` — absent key =
  today's behaviour; `batch_catchup` is opt-in only.
- Adaptive degrade + watermark cutoff ship as constants (no config); a
  revert of `spot_1m_rest_boot.rs` restores the pre-PR ladder byte-for-byte.
- No schema change, no table change, no hot-path change — single-PR revert
  is clean.

## Observability

- `tv_dhan_data_api_rps` gauge (current level), set on every transition +
  at configure.
- `tv_dhan_data_api_tuner_transitions_total{direction="down"|"up"}` counter.
- `tv_dhan_data_api_acquires_total` counter (per permitted request).
- `tv_spot1m_ladder_watermark_cutoff_total` counter (ladder stops saved).
- `tv_spot1m_ladder_degraded` gauge (0/1) + one `warn!` on enter / `info!`
  on exit (edge-triggered, audit Rule 4).
- Batch mode: `tv_spot1m_batch_cycles_total{outcome}` + the existing sweep
  counters (`tv_spot1m_sweep_backfilled_total` reused via the shared
  helper's stage label logs).
- Existing SPOT1M-01/02 + CHAIN-01..04 edges, events and counters are
  UNCHANGED; the runbook (`rest-1m-pipeline-error-codes.md`) gains a dated
  2026-07-14 section (limiter design, self-tuning levels, honesty
  paragraph, mode flag); `no-rest-except-live-feed-2026-06-27.md` §8 gains
  a one-line dated pointer (no §8 rewrite).
- All new metrics are log-sink/`/metrics`-local (no CloudWatch EMF
  allowlist change, no new alarms — zero cost delta).

## Plan Items

- [x] Item 1 — `[dhan_data_api]` config + `SpotFetchMode`/batch-interval config + validation
  - Files: crates/common/src/config.rs, crates/common/src/constants.rs, config/base.toml
  - Tests: test_dhan_data_api_config_default_is_3_and_round_trips, test_dhan_data_api_config_validate_rejects_out_of_range, test_spot1m_fetch_mode_defaults_per_minute_and_parses_batch, test_spot1m_batch_interval_validate_bounds

- [x] Item 2 — shared limiter module (pre-built governor levels + ArcSwap + pure RpsTuner FSM + OnceLock global)
  - Files: crates/app/src/dhan_data_api_limiter.rs, crates/app/src/lib.rs
  - Tests: test_tuner_steps_down_to_floor_on_429_burst, test_tuner_holds_below_threshold, test_tuner_steps_up_one_level_after_clean_streak, test_tuner_never_below_floor_never_above_cap, test_tuner_window_expires_old_429s, test_tuner_set_cap_clamps_current_downward, test_limiter_levels_prebuilt_and_quota_matches_rps, test_configure_clamps_out_of_range_target

- [x] Item 3 — route spot + chain fetch fns through the limiter; feed 429s to the tuner; configure at both spawn seams
  - Files: crates/app/src/spot_1m_rest_boot.rs, crates/app/src/option_chain_1m_boot.rs, crates/app/src/main.rs, crates/app/src/dhan_rest_stack.rs
  - Tests: crates/app/tests/dhan_data_api_limiter_wiring_guard.rs (all needles)

- [x] Item 4 — spot retry-shaping: stale-watermark cutoff + adaptive degrade
  - Files: crates/app/src/spot_1m_rest_boot.rs, crates/common/src/constants.rs
  - Tests: test_ladder_watermark_repeated_stops_on_equal, test_ladder_watermark_zero_rows_twice_repeats, test_ladder_watermark_advancing_does_not_stop, test_ladder_watermark_first_observation_never_stops, test_degrade_enters_after_five_no_data_minutes, test_degrade_any_success_rearms, test_degrade_attempt_count_single

- [x] Item 5 — fetch-mode flag: batch_catchup thin wrapper over the shared sweep helper
  - Files: crates/app/src/spot_1m_rest_boot.rs, crates/app/src/main.rs, crates/app/src/dhan_rest_stack.rs
  - Tests: test_next_batch_fire_grid_alignment, test_next_batch_fire_past_window_none, test_batch_cycle_fully_failed_semantics

- [x] Item 6 — runbook dated section + §8 one-line pointer + plan hygiene (archive 2 merged plans)
  - Files: .claude/rules/project/rest-1m-pipeline-error-codes.md, .claude/rules/project/no-rest-except-live-feed-2026-06-27.md, .claude/plans/archive/*
  - Tests: n/a (docs; plan-gate green)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Minute boundary: 4 spot + 3 chain fires at 3 rps | requests spread over ~2.3s, zero client-side drops, budgets hold |
| 2 | Dhan 429s 3× inside 2 min | ONE step-down to 2 rps, one error! + gauge 2, window cleared |
| 3 | 10 clean minutes at 2 rps | ONE step-up to 3 rps, one info! + gauge 3 |
| 4 | All-empty regime (today's) | after 5 no-data minutes: 4 req/min (degraded), watermark cutoff stops rung 2+, ~244 wasted 429s → ~0 |
| 5 | Any SID serves again | full ladder re-armed next minute, degrade exit logged once |
| 6 | fetch_mode=batch_catchup, interval 5 | one day-window fetch per SID every 5 min, everything above watermark persisted, per-minute fires absent |
| 7 | target_rps=1 in TOML | boot REJECTED by validate (range 2..=4) |
