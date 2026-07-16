# Implementation Plan: Broker-Agnostic Fetch-Cadence + Decision-Timing Scheduler (dry-run skeleton)

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — cadence directive 2026-07-14 relayed via coordinator (judge-locked design rev-8 FINAL; multi-agent read/design/judge panel 2026-07-14)

> **Per-item guarantee matrix:** this plan cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row + 7-row
> matrices apply per that file; item-specific proof artefacts are the
> ratchet tests listed below).

> **Honest 100% claim (envelope-qualified per operator-charter §F):**
> 100% inside the tested envelope, with ratcheted regression coverage:
> zero-429 is a STRUCTURAL property of the monotonic CAS gates
> (per-(underlying, expiry) chain gate 3000ms, dhan spot
> ROLLING-1000ms-WINDOW gate ≤ spot_window_cap, the combined cap-5
> rolling-second ring — the 2026-07-16 binding cadence-lane pacing per
> coordinator ruling A) proven by the
> deterministic replay proptest across 64-cycle permutations of skew,
> jitter, GC pauses, latencies, failures, ladder walks (Dhan shape rung +
> concurrency step + Groww shape) and restarts;
> the decide-path read is DHAT-pinned zero-alloc/O(1); default-OFF
> config gate means byte-identical behavior until the operator flips
> `[cadence] enabled`. NOT claimed: broker-side serving behavior
> (/v2/charts/intraday same-day 1m is a live-probe Unknown — the
> 14-day 200-empty saga), Groww 7-parallel burst tolerance (Unknown,
> live-probe), and the per-cycle scheduler loop which is honestly
> O(requests-per-cycle)=11 with N fixed — flagged O(N), not claimed O(1).
> Beyond the envelope, every failure is a typed coded error + counter,
> never a silent skip.

## Design

Per the judge-locked design (rev-8 FINAL, 2026-07-14): a new
`crates/core/src/cadence/` module (schedule.rs pure IST minute-boundary +
slot table parameterized by broker anchor + ladder rung; gate.rs pure CAS
`MinSpacingGate` in the MONOTONIC time domain with injected clock;
ladder.rs Dhan failure-ladder FSM rungs 0..=5 with step-back-one recovery;
executor.rs the LOCKED `CadenceExecutor` seam (native RPITIT async,
single-target requests, typed `CadenceFetchError`, `DryRunLoggingExecutor`
that logs the fire and returns Err(Empty) — never synthesizes prices);
assembly.rs per-cycle per-broker-lane store with inline moneyness via
`tickvault_common::moneyness` (zero re-implemented math) + chain rows from
`tickvault_core::pipeline::chain_snapshot::load_chain_snapshot` +
cross-source fill both directions; decision.rs event-driven per-lane
DecisionSnapshot with honest-skip past cutoffs; runner.rs ONE supervised
tokio task driving slots via the gates). `CadenceConfig` (`[cadence]`,
DEFAULT-OFF, all operator numbers as serde defaults with validate()
bounds) lands in `tickvault-common` (`crates/common/src/config.rs`), the
three new ErrorCode variants (CADENCE-01/02/03) in
`crates/common/src/error_code.rs` with the rule file
`.claude/rules/project/cadence-error-codes.md`. `tickvault-app` gains
`crates/app/src/cadence_boot.rs` (config-gated dual-spawn from both
main.rs boot paths, DryRunLoggingExecutor for both brokers) and
`config/base.toml` gains the `[cadence]` section with `enabled = false`.
This PR ships NO REST caller — dry-run log sink only; merges AFTER #1540
(hard dependency: `crates/common/src/moneyness.rs` +
`crates/core/src/pipeline/chain_snapshot.rs`).

**2026-07-15 operator spec ADDITION (adaptive ladders + window gate +
resolver seam):** doc verification FIRST — NEITHER broker's
historical-candle endpoint supports a multi-symbol batch in one HTTP
request (Dhan `docs/dhan-ref/05-historical-data.md`: `securityId` is ONE
string per request; Groww `docs/groww-ref/11-historical-candles.md` +
the live `groww_spot_1m_boot.rs` caller: one `groww_symbol` per GET), so
ladder step 0 = 4 SIMULTANEOUS single-symbol calls, never one batched
request. (a) Dhan spot ADAPTIVE CONCURRENCY LADDER
(`ladder.rs::StreakLadder` + `spot_group_index`): steps [[4]] → [[3],[1]]
→ [[2],[2]] → [[1],[1],[1],[1]], groups at consecutive 1000ms-spaced
anchors from the spot anchor slot; rung shift + post-close clamp apply to
the group BASE. Degrade one step after 2 CONSECUTIVE spot-dirty
(≥1 RateLimited spot outcome) cycles; recover one step after 3
consecutive fully-clean-spot cycles — both config keys
(`concurrency_degrade_after_dirty_cycles` = 2,
`concurrency_recover_after_clean_cycles` = 3, BOTH Assumed pending
operator confirm). (b) GATE CHANGE: the Dhan spot min-spacing gate is
REPLACED by `gate.rs::RollingWindowGate` — max `spot_window_cap`
(default 4, validate 1..=5 — Dhan hard cap 5/sec) authorizations in ANY
sliding 1000ms window, same injected-clock monotonic-domain design,
fixed 5-slot ring of the last authorization instants (O(1)); chain gates
UNCHANGED. Honest composition note: the shared 3 rps
`dhan_data_api_limiter` smooths a 4-simultaneous burst to ~3/sec — the
window gate is the STRUCTURAL ceiling, the limiter defense-in-depth
(documented, not fought). Failed-spot retries stay ≤1/instrument,
APPENDED one full window after the last group anchor. (c) Groww
THREE-CHOICE fallback-shape ladder (operator verbatim 2026-07-15,
supersedes the same-day 3-wave note): choice 1 (default) = :00
all-7-parallel; choice 2 = :01 all 3 chains / :02 ALL 4 spots (VIX
included); choice 3 (last resort) = :01 chains / :02 core spots / :03
VIX alone — its own `StreakLadder` (the SAME primitive as the spot
ladder), same 2-dirty (any RateLimited Groww leg) / 3-clean rules. VIX
absence NEVER blocks the Groww data-complete predicate
(coordinator-confirmed 2026-07-15 — VIX stays advisory, surfaced via
`vix_missing`). (d) ExpiryResolver seam (CONFIRMED minimal):
`executor.rs::ExpiryResolver` trait (`resolved_expiry(broker,
underlying) -> Option<u32>` yyyymmdd; None = unresolved — the scheduler
NEVER guesses; the lane carries the coalesced CADENCE-01
`expiry_unresolved` stage and the executor impl may fall back to its
warmup expiry) + `ChainFetchRequest.expiry_yyyymmdd` stamped by the
runner at request-build time + the day-1 `StubExpiryResolver` (always
None) wired in `cadence_boot.rs`; the FULL resolution boot phase is a
SEPARATE follow-up increment by a different worker.

**2026-07-15 increment 2 — PRE-MARKET EXPIRY RESOLUTION (Workstream A,
operator spec 2026-07-15):** the executor seam gains
`fetch_expiry_list(ExpiryListRequest) -> Result<Vec<u32>, CadenceFetchError>`
(yyyymmdd, vendor-raw, unsorted tolerated; DryRunLoggingExecutor logs +
returns Err(Empty)). PURE POLICY (`cadence/expiry.rs::resolve_policy_expiry`):
Nifty/Sensex = NearestActiveDate (min date >= today); BankNifty =
LastExpiryOfNearestActiveMonth (group by (year,month), nearest group with
ANY date >= today, that group's LAST date — NEVER the flat minimum;
premise "no BANKNIFTY weeklies post-Nov-2024 NSE rationalisation" is
Assumed, flagged in the rule file). DAY-LOCKED STORE
(`DayLockedExpiryStore`, process-global `global_expiry_store()` OnceLock +
poison-recovering Mutex): keyed by the IST trading day via the house
`trading_calendar::ist_offset()` FixedOffset (NEVER UTC); re-resolution
ONLY at day flip; respawn-proof (a runner respawn re-reads the same
day-locked verdicts). Read API: winning date per underlying + per-broker
raw provenance + disagreement flag; `ExpiryDate` = yyyymmdd u32 +
`as_iso_string()` + `NaiveDate`. The store IS the production
`ExpiryResolver` read facade — cadence_boot wires it as BOTH
`expiry_resolver` and `expiry_store`. BOOT PHASE
(`runner.rs::run_expiry_resolution_loop`, spawned per runner life,
abort-on-drop): per (broker, underlying) fetch with bounded retry
(`expiry_retry_interval_ms`, default 60000) until the IST deadline
(`expiry_deadline_secs_of_day_ist`, default 32100 = 08:55) → past
deadline ONE edge-latched CADENCE-01 `expiry_unresolved` per pair
episode + background retry continues to session end; lanes degrade
(chains fire with `expiry_yyyymmdd = None`). The deadline gates the
PAGE, never the attempts. DISAGREEMENT ARM: both brokers resolve and
differ → Dhan WINS keying BOTH lanes; edge-latched CADENCE-01
`expiry_disagreement`; the store records both raws + the verdict.

**2026-07-15 increment 2 — VERIFIER FIXES F1–F10 (Workstream B):**
F1 per-(underlying,expiry) chain gate stamp alongside the always-on
per-underlying gate (expiry-less fire strictly more conservative —
subsumption-tested) + process-global `global_dhan_gates()` +
COMPOSITION CONTRACT (2026-07-15) section in the rule file binding
every future Dhan-firing executor (route through dhan_data_api_limiter
+ the global gate; limiter-queue delays are the NEW NON-ARMING
`CadenceFetchError::QueueDelay`) + the F1(iv) source-scan ratchet
`cadence_composition_contract_guard.rs`; F2 shutdown Notify parked in
`cadence_boot::CADENCE_SHUTDOWN` + `notify_cadence_shutdown()` fired
from main.rs teardown; F3 dispatch order extracted into pure
`build_cycle_events` driven directly by proptest; F4 the Groww verdict
never refetches an in-flight leg (`groww_leg_inflight` tracking); F5/F6/
F8 honest doc corrections (trend-only counter, brief-queue-at-floor,
level-triggered enable flags); F7 any-failure arming kept as contract
(amplitude-1 oscillation test) with a strengthened Assumed flag; F9
retry admission tests the ACTUAL insertion instant
(`retry_at.max(now_wall)`); F10 `dry_run: bool` threaded through the
runner — dry-run-shaped skips/degrades demote to `info!(dry_run=true)`,
real failures keep the coded `error!`, and the decision double-latch
debug_assert is replaced by a coded CADENCE-03 `double_latch` error +
counter. Unnumbered: `dhan_spot_start_offset_ms` +
`groww_anchor_offset_ms` range-validated in `validate()`. ONE-SOURCE-OF-
TRUTH DELEGATION (pending) recorded in the rule file naming the 3
duplicate expiry-selection sites to be delegated to the policy module.

**2026-07-16 POST-CLOSE BURST RESHAPE (operator directive 2026-07-16,
relayed via the coordinator; verbatim quote + full contract in
`cadence-error-codes.md` §0b):** the Dhan lane moves ENTIRELY POST-CLOSE
and the pre-close machinery retires. The new cadence per minute close T:

| Broker | Rung | Second 1 (T+1s Dhan / T+0 Groww) | Second 2 | Second 3 |
|---|---|---|---|---|
| Dhan | 0 (primary) | ALL 7 CONCURRENT — 3 chains + all 4 spots (the same-day all-7 correction; two-bucket cap-legal) | — | — |
| Dhan | 1 (fallback) | 3 chains concurrent | ALL 4 spots | — |
| Groww | 0 (primary) | all 7 parallel at T+0 | — | — |
| Groww | 1 (fallback) | chains :01 | all 4 spots :02 | — |
| Groww | 2 (retained last resort — coordinator addendum item 1) | chains :01 | core spots :02 | VIX alone :03 |

RETIRED: the :55/:58/:02 pre-close chain instants
(`dhan_chain_offsets_ms`), the T+3s spot anchor
(`dhan_spot_start_offset_ms`), the anchor-shift failure ladder
(`LadderState`/`CycleVerdict`/`next_rung`/`dhan_ladder_step_ms`/
`dhan_ladder_max_rungs`/`recovery_mode`), the GLOBAL 3s chain gate (the
directive: the 3s rule binds the SAME chain expiry only — different
underlyings are explicitly concurrent), and the lender-aware cross-fill
floor widening (CADENCE-XFILL-RUNG-1 — plain base T−5000 now; addendum
item 3). REPLACED BY: `dhan_burst_offset_ms` (default 1000) + a Dhan
SHAPE `StreakLadder` (rung 0⇄1, same 2-dirty/3-clean streaks as the
concurrency ladders; stage `dhan_shape_shift`, gauge
`tv_cadence_dhan_shape_step`, counter
`tv_cadence_dhan_shape_shifts_total`; the `ladder_exhausted` CADENCE-01
edge keeps firing on a dirty cycle AT the fallback rung). The spot tiers
compose with the shape via the pure `spot_second_buckets(shape, tier)`
per-second-group bucket math (addendum item 4), proptested per
(shape × tier × failure) permutation in the replay proof. PACING
(coordinator ruling A): cadence fires are governed by the combined cap-5
ring — NOT routed through the shared 3 rps `dhan_data_api_limiter`
(which stays the authority for the legacy per-minute paths; mirrored
dated note in `rest-1m-pipeline-error-codes.md` §2f). COEXISTENCE
(coordinator ruling B): `AppConfig::validate()` fail-closes cadence
enabled + a broker lane active + that broker's per-minute capture legs
enabled (mutual exclusion, tested both directions); full subsumption is
the flagged follow-up. Decisions unchanged (event-driven, Groww 6000ms /
Dhan 15000ms cutoffs, exactly-once latch, VIX advisory).

**2026-07-16 SAME-DAY RULING CORRECTIONS (coordinator-relayed operator
verbatim; full quotes in `cadence-error-codes.md` §0b):**
(1) *"i clearly told you for dhan also as the primary all 7 parallel at
first second one and only when it fails or rate limited alone only then
this option chain first second and spot second."* — Dhan rung 0 = ALL 7
concurrent at the burst second (the interim 5+2 packing is RETIRED as
primary; `spot_second_buckets` shape-0 base = `[0,0,0,0]`). Cap-legal
via the TWO-BUCKET model: the combined rolling-1000ms cap-5 ring is
RE-SCOPED to SPOT + EXPIRY-LIST fires only (Data-API bucket, 4 ≤ 5);
CHAIN fires are governed SOLELY by the per-(underlying, expiry) ≥3s CAS
gate (the option-chain API's own budget — different underlyings
explicitly concurrent).
(2) *"see that too instantly dont commit — one and only when you tried
that multiple times and gets rate limited alone alone fallback."* —
`RateLimited` is the SOLE ladder-arming class (Timeout / Transport /
Empty / QueueDelay never reshape); "multiple times" = the existing
2-consecutive-dirty-cycles trigger; and a rate-limited leg KEEPS its ONE
bounded in-cycle retry through the gates (reversing the first pass's
"429 never blind-retried in-cycle" rule) — one retry per leg per cycle,
never more.
ALSO landed with the corrections: R5 — chain-row moneyness anchors the
chain's OWN embedded underlying spot FIRST (assembly
`chain_moneyness_anchor`: chain-embedded → OwnFetch fallback → Unknown
last; wired into the decide-time fold); R6 — the expiry cross-broker
disagreement is a REAL typed `CadenceExpiryDisagreement` HIGH Telegram
page (edge-latched once per underlying per day; sink threaded
boot → `CadenceRunnerDeps.notifier`; dated authority row in
`dhan-rest-only-noise-lock-2026-07-14.md` §2.2).

## Edge Cases

Minute-boundary races (wake at T±ε — no double fire, latch on
(lane, cycle_minute)); process restart mid-cycle (no-mid-cycle-join +
conservative gate re-seed `next_allowed = now + spacing`); wall-clock
regression/step (gates live in the monotonic domain — immune); late wakes
past a slot (re-read clock, count missed boundaries loud, fire gate-checked
or skip); rung clamp at `spot_min_post_close_ms` (spots never fire
pre-close); day edges (first cycle T=09:16:00 with Dhan pre-fire 09:15:55,
rung 5 pre-fire 09:15:50 legal; last cycle T=15:30:00 stamped
post_close=true); VIX advisory (never blocks a decision); spot exactly on
a strike / exact midpoint tie (rounds UP per atm_strike_paise); zero /
negative / NaN spot → every row Unknown, surfaced, all-3-Unknown ⇒
Skipped; empty chain sentinel ⇒ not data-complete; 200-empty spot does
NOT arm the ladder (Assumed, flagged); Groww wave partial failures
(fallback re-fetches ONLY failures, chains-then-spots); concurrency
ladders clamp to [structural floor for spot_window_cap, max step] and
reset at day start; a spot_window_cap below 4 FLOORS the spot ladder's
starting step (a cap of 2 cannot admit the 4-simultaneous group); at
choice 2 the CoreSpots + VixSpot waves share the :02 anchor (all 4 spots
together); Groww wave/fallback tails can NEVER overlap the next minute's
:00 burst (validate() bounds the worst shape's verdict + 7 sequential
request timeouts inside the 60s cycle, asserted again in the replay
proof and the paused-time runner test).

2026-07-15 increment 2: unsorted/duplicate vendor expiry lists (policy
sorts + dedups internally); expiry lists entirely in the past →
unresolved (never a stale pick); year-0/absurd yyyymmdd values rejected
by `ExpiryDate::from_yyyymmdd` (year clamped to 2000..=2100); BankNifty
month-boundary (a group whose only remaining date is today still wins;
an exhausted month falls to the next month's LAST date); one broker
resolved + one unresolved → the resolved broker's date keys BOTH lanes
WITHOUT a disagreement page (disagreement requires both resolved AND
differing); day flip mid-process → store re-resolves, edge latches
reset; runner respawn mid-day → day-locked store keeps the verdicts
(no re-page, no re-fetch churn beyond the loop's own bounded retry);
expiry-less chain fire consults ONLY the per-underlying gate (stamp map
untouched — subsumption holds); a same-(underlying,expiry) fire inside
the spacing window is deferred by the stamp even when the per-underlying
gate would admit it; QueueDelay outcomes never arm the failure ladder
but do surface in the degrade flags; Groww verdict with a leg still
in-flight at verdict time → awaited/skipped, never double-fetched.

## Failure Modes

Dhan transport/timeout/5xx/RateLimited arms the failure ladder (next-cycle
anchor shifts 1s earlier, floor rung 5); RateLimited is NEVER blind-retried
in-cycle; in-cycle retries ≤1 per failed request, only through the gates
and only if landing before the lane cutoff; ladder floor exhausted ⇒
cross-source steady state (Groww same-cycle data drives the Dhan lane,
decisions stamped DecidedDegraded); both brokers dead ⇒ HONEST-SKIP +
CADENCE-02 loud alert; lane incomplete at cutoff (groww 6000ms / dhan
15000ms) ⇒ HONEST-SKIP, never a late decision, never a decision on
missing/stale data; runner task death ⇒ supervised respawn (unwind builds;
release panic=abort honesty) + CADENCE-03 stage=respawn; a 429 arriving
DESPITE the gates is typed RateLimited, arms the ladder and fires a
gate-bug error. Every degrade is a typed coded error!, coalesced per-cycle
never per-request (Rule 11: a skip is never rendered OK).

2026-07-15 increment 2: expiry-list fetch failures retry bounded
(`expiry_retry_interval_ms`) until the 08:55 IST deadline, then ONE
edge-latched CADENCE-01 `expiry_unresolved` per (broker, underlying)
episode while background retry continues to session end — the deadline
gates the PAGE, never the attempts; unresolved past deadline = lanes
degrade honestly (chains fire expiry-less, the executor may fall back
to its own warmup expiry — never a scheduler guess); cross-broker
disagreement = Dhan wins + ONE edge-latched CADENCE-01
`expiry_disagreement` (both raws + verdict recorded for forensics);
dry-run executors return Empty forever → the store stays honestly
unresolved and the unresolved page demotes to `info!(dry_run=true)`
(F10 — the real coded error ships with the real-executor flip);
decision double-latch (structurally unreachable) is a coded CADENCE-03
`double_latch` error + counter, never a debug-only assert.

## Test Plan

Full judge-locked matrix: unit tests inline per module
(test_cadence_schedule_rung0_slots_match_operator_table,
test_cadence_schedule_rung1_split_fallback_slots,
test_cadence_schedule_burst_packing_never_exceeds_broker_cap,
test_cadence_schedule_spot_clamp_never_pre_close,
test_cadence_schedule_boundary_vectors_mirror_spot_1m_rest,
test_cadence_schedule_window_09_16_to_15_30_inclusive,
test_cadence_gate_min_spacing_acquire_and_defer,
test_cadence_gate_monotonic_immune_to_wall_regression,
test_cadence_gate_boot_reseed_conservative,
test_dhan_shape_ladder_rung0_rung1_transitions_under_streak_rules,
test_ladder_rate_limited_sole_arming_class_with_bounded_retry,
test_ladder_spot_200_empty_does_not_arm,
test_may_retry_in_cycle_respects_gate_and_cutoff,
test_cadence_assembly_predicate_3_chains_3_spots_vix_advisory,
test_cadence_decision_latch_one_per_lane_per_cycle,
test_decision_fires_instant_predicate_completes,
test_honest_skip_at_cutoff_emits_alert_once,
test_cross_source_freshness_window_spans_the_base_floor,
test_spot_provenance_order_own_crossfill_chain_embedded,
test_groww_burst_fallback_refetches_only_failures,
test_cadence_config_default_off,
test_cadence_config_validate_rejects_bad_spot_window_cap,
test_cadence_config_validate_groww_shape_no_overlap_bounds,
test_cadence_config_validate_rejects_sub_3s_chain_spacing); the 2026-07-15
ladder/gate/seam tests
(test_spot_concurrency_ladder_degrades_after_2_dirty_recovers_after_3_clean,
test_groww_shape_ladder_all_choice_transitions,
test_spot_second_buckets_encodes_rung_and_tier_groupings,
test_groww_wave_indices_encode_three_choice_shapes,
test_cadence_gate_rolling_window_cap_and_boundary,
test_cadence_gate_rolling_window_monotonic_immune_and_reseed,
test_cadence_schedule_spot_concurrency_groupings_per_step,
test_cadence_schedule_groww_three_choice_wave_instants_no_overlap,
test_cadence_expiry_resolver_stub_returns_none_scheduler_never_guesses,
test_cadence_expiry_resolver_stamps_requests_when_resolved,
test_dhan_spot_ladder_rate_limit_mid_ladder_degrades_then_recovers,
test_groww_three_choice_ladder_all_transitions_and_vix_waves — runner
end-to-end tier transitions 1→2, 2→3, 3→2, 2→1 + VIX wave placement +
partial wave failures + the no-overlap-into-next-:00 assertion); the
deterministic zero-429 replay proptest
`crates/core/tests/cadence_zero_429_replay.rs::proptest_cadence_replay_zero_rate_violations`
(SimClock, 64 consecutive cycles, skew/jitter/GC/latency/outcome/restart
permutations FOLDING the real concurrency + shape ladders — per-UL +
GLOBAL chain deltas ≥3000ms, NEVER more than spot_window_cap spot
authorizations in ANY rolling 1000ms window,
zero gate denials on nominal slots, ≤1 decision/lane/cycle, no decision
past cutoff, every skip carries a reason); deterministic named tests
(test_minute_boundary_race_no_double_fire,
test_restart_mid_cycle_cannot_violate_spacing,
test_retry_through_gate_never_compresses_chain_spacing); #1540 boundary
tests (test_cadence_atm_exact_strike_is_atm,
test_cadence_atm_midpoint_tie_rounds_up,
test_cadence_invalid_spot_all_unknown_surfaced,
test_cadence_empty_chain_sentinel_skips,
test_cadence_200_empty_spot_fallback_chain_end_to_end); DHAT
`crates/core/tests/dhat_cadence_decide.rs`; integration
`test_cadence_runner_dry_run_full_cycle_emits_decisions_or_skips` (paused
tokio time) + the app spawn wiring guard
`crates/app/tests/cadence_boot_wiring_guard.rs`.

2026-07-15 increment 2 additions: expiry-policy unit + PROPERTY tests
(nearest-active-date is provably the min date >= today; BankNifty
month-last invariants — winner is in the nearest active month, is that
month's max, and is NEVER the flat min when the group has >1 future
date), day-locked store tests (day flip re-resolution, respawn-proof
reads, Dhan-wins disagreement), runner integration tests (boot-phase
resolution stamps ChainFetchRequest.expiry_yyyymmdd; disagreement keys
BOTH lanes from the Dhan date; in-flight Groww leg never double-fetched
— SlowLegExecutor), gate tests (per-(underlying,expiry) stamp recorded/
consulted; expiry-less subsumption; global handle first-write-wins),
ladder tests (QueueDelay non-arming; amplitude-1 oscillation contract),
the dispatch-order parity proptest over `build_cycle_events`, the
cross-ladder no-oscillation replay test, and the F1(iv) composition-
contract source-scan ratchet
`crates/core/tests/cadence_composition_contract_guard.rs`.

- [x] Plan-gate precursor: archive `active-plan-telegram-groww-episode-fold.md` (work merged as #1560 / 82db448)
  - Files: .claude/plans/archive/2026-07-14-telegram-groww-episode-fold.md
- [x] CadenceConfig + validate + ErrorCode variants + rule file
  - Files: crates/common/src/config.rs, crates/common/src/error_code.rs, .claude/rules/project/cadence-error-codes.md
  - Tests: test_cadence_config_default_off, test_cadence_config_validate_rejects_sub_334_spacing, test_cadence_config_validate_rejects_sub_3s_chain_spacing
- [x] crates/core/src/cadence/ module (schedule/gate/ladder/executor/assembly/decision/runner)
  - Files: crates/core/src/cadence/mod.rs, crates/core/src/cadence/schedule.rs, crates/core/src/cadence/gate.rs, crates/core/src/cadence/ladder.rs, crates/core/src/cadence/executor.rs, crates/core/src/cadence/assembly.rs, crates/core/src/cadence/decision.rs, crates/core/src/cadence/runner.rs, crates/core/src/lib.rs
  - Tests: the full unit matrix above
- [x] Zero-429 replay proptest + DHAT + boundary + runner integration tests
  - Files: crates/core/tests/cadence_zero_429_replay.rs, crates/core/tests/dhat_cadence_decide.rs
  - Tests: proptest_cadence_replay_zero_rate_violations, dhat_cadence_decide_zero_alloc, test_cadence_runner_dry_run_full_cycle_emits_decisions_or_skips
- [x] App boot wiring (dual-spawn, DEFAULT-OFF) + base.toml section
  - Files: crates/app/src/cadence_boot.rs, crates/app/src/main.rs, crates/app/src/lib.rs, config/base.toml
  - Tests: crates/app/tests/cadence_boot_wiring_guard.rs
- [x] Increment 2 Workstream A: pre-market expiry resolution (policy + day-locked store + boot phase + disagreement arm)
  - Files: crates/core/src/cadence/expiry.rs, crates/core/src/cadence/executor.rs, crates/core/src/cadence/runner.rs, crates/core/src/cadence/mod.rs, crates/common/src/config.rs, crates/app/src/cadence_boot.rs, config/base.toml, .claude/rules/project/cadence-error-codes.md
  - Tests: test_cadence_expiry_policy_for_underlyings_locked_mapping, test_cadence_expiry_nearest_active_date_unsorted_vendor_list, test_cadence_expiry_banknifty_month_last_never_flat_min, test_cadence_expiry_empty_and_garbage_lists_fail_closed, test_cadence_expiry_store_day_lock_first_write_wins_and_day_flip, test_cadence_expiry_store_disagreement_dhan_wins_edge_latched, test_cadence_expiry_store_resolver_facade_reads_winner_for_both_brokers, test_cadence_expiry_page_due_edge_latch_deadline_gates_page_not_attempts, test_cadence_expiry_date_yyyymmdd_iso_naive_helpers, proptest_cadence_expiry_nearest_active_is_min_geq_today, proptest_cadence_expiry_banknifty_group_last_is_active_and_group_max, proptest_cadence_expiry_all_past_or_garbage_never_selected, proptest_cadence_expiry_day_holds_then_rolls_next_group, test_cadence_runner_expiry_boot_phase_resolves_and_stamps, test_cadence_runner_expiry_disagreement_dhan_wins_both_lanes
- [x] Increment 2 Workstream B: verifier fixes F1–F10 + unnumbered config validation
  - Files: crates/core/src/cadence/gate.rs, crates/core/src/cadence/ladder.rs, crates/core/src/cadence/decision.rs, crates/core/src/cadence/runner.rs, crates/app/src/cadence_boot.rs, crates/app/src/main.rs, crates/common/src/config.rs, crates/core/tests/cadence_composition_contract_guard.rs, crates/core/tests/cadence_zero_429_replay.rs, crates/core/tests/cadence_runner_dry_run.rs, .claude/rules/project/cadence-error-codes.md
  - Tests: test_cadence_gate_expiry_stamp_recorded_and_consulted, test_cadence_gate_expiryless_fire_subsumes_expiry_stamp, test_cadence_gate_global_handle_first_write_wins_and_shared, test_ladder_queue_delay_is_non_arming_but_retryable, test_cadence_ladder_failure_arms_ladder_is_total, proptest_cadence_build_cycle_events_dispatch_order_parity, test_cadence_shape_and_concurrency_ladders_never_oscillate_against_each_other, test_groww_verdict_skips_inflight_leg_never_duplicates, test_cadence_rule_file_pins_composition_contract_heading, test_cadence_gate_module_keeps_global_handle_fns, test_composition_contract_guard_needles_are_non_vacuous

## Rollback

Config-gated DEFAULT-OFF: `[cadence] enabled = false` (the shipped
default and the serde default) means the runner never spawns — behavior is
byte-identical to pre-PR. Rollback = leave/flip the flag off; no schema
changes, no persistence, no strategy wiring, no REST calls, no new
WebSocket, no changes to the existing record-capture legs (the cadence
runner never touches the `minute_done_tx` watch channels). A full revert
of the PR is also clean: all new files, plus additive-only edits to
config.rs / error_code.rs / lib.rs / main.rs / base.toml.

## Observability

Counters (static labels only): tv_cadence_fetch_total{lane,leg,outcome},
tv_cadence_gate_denials_total, tv_cadence_gate_deferred_total{key},
tv_cadence_ladder_rung (gauge), tv_cadence_ladder_shifts_total{direction},
tv_cadence_ladder_exhausted_total, tv_cadence_groww_fallback_total{leg},
tv_cadence_spot_fallback_total{source}, tv_cadence_cross_fill_total{direction},
tv_cadence_decision_total{lane,outcome}, tv_cadence_decision_latency_ms
(histogram), tv_cadence_late_response_total{lane},
tv_cadence_boundary_skipped_total, tv_cadence_late_wake_ms (histogram),
tv_cadence_moneyness_unknown_total{lane,underlying},
tv_cadence_runner_respawn_total{reason},
tv_cadence_spot_concurrency_step (gauge) +
tv_cadence_spot_concurrency_shifts_total{direction},
tv_cadence_groww_shape_step (gauge) +
tv_cadence_groww_shape_shifts_total{direction} (the 2026-07-15 adaptive
ladders — CADENCE-03 stages spot_concurrency_shift / groww_shape_shift;
CADENCE-01 gains the expiry_unresolved stage). ErrorCodes CADENCE-01
(lane degraded, High), CADENCE-02 (decision skipped, High — the skip IS
the fail-closed action), CADENCE-03 (scheduler degraded, Medium), all
with stage fields, coalesced per-cycle, runbook
`.claude/rules/project/cadence-error-codes.md`. Delivery boundary
(honest): all three log-sink-only day 1; the typed edge-latched Telegram
for 3 consecutive skips/lane ships with the enable flip (flagged
follow-up in the rule file — the 2026-07-14 Telegram noise lock forbids
dry-run page noise).
