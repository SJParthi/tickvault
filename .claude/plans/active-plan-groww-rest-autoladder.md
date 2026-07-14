# Implementation Plan: Groww per-minute REST auto-ladder (rate-safe burst tiers, sub-5s decision data)

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — verbatim quote, 2026-07-14, relayed via the
coordinator session: *"approved and go ahead with the recommendation"* (with the
stated operator preference for the 7-concurrent tier, probe-gated behind the
off-hours rate probe; the shipped default is the rate-safe two-wave tier).

**Crates:** `crates/app` (the two Groww REST boot modules + new burst/probe
modules + main.rs wiring + wiring-guard tests), `crates/common` (constants +
config), plus `config/base.toml` and three `.claude/rules/project/` files.

---

## Design

Rebuild the per-minute Groww REST legs (`crates/app/src/groww_spot_1m_boot.rs`
spot leg + `crates/app/src/groww_option_chain_1m_boot.rs` chain leg) as a
rate-safe AUTO-LADDER with two burst tiers so worst-case decision data lands
< 5 s after minute close. The ~4.5 s of SELF-IMPOSED pacing (2.5 s
spot→chain fallback serialization + 2 × 1,000 ms chain inter-underlying
min-gap + the sequential spot target loop) is DELETED; actual HTTP is
~0.2 s/request (2026-07-13 live probe).

**Tier `two_wave` (shipped DEFAULT):** 3 chain requests CONCURRENTLY at minute
close + 300 ms (`GROWW_CHAIN_1M_FIRE_DELAY_MS`); 4 spot requests CONCURRENTLY
at close + 1,350 ms (`GROWW_SPOT_1M_TWO_WAVE_FIRE_DELAY_MS`). Wave separation
Δ = 1,050 ms > 1,000 ms ⇒ no rolling 1-second window contains both waves ⇒
initial-burst max 4 req/s.

**Tier `seven_concurrent` (operator preference — probe-gated, NOT default):**
all 7 requests (4 spot + 3 chain) concurrently at close + 300 ms. 7/10 of the
broker ceiling solo; promoted later ONLY by a config flip + dated rule note
after the off-hours rate probe passes.

**Shared changes (both tiers):**
- Chain leg fires on its OWN minute-boundary timer — the spot→chain watch
  channel, `wait_for_signal_or_fallback` usage, `GROWW_CHAIN_1M_FALLBACK_DELAY_MS`
  and `GROWW_CHAIN_1M_MIN_GAP_MS` (+ `groww_min_gap_wait_ms`) are deleted from
  the Groww chain path. The chain→contract sequencing signal is KEPT untouched.
- Spot targets fetch CONCURRENTLY (tokio `JoinSet`, results processed in
  target order for the persist/audit/verdict pipeline). Each spot target's
  bounded re-poll ladder (`GROWW_SPOT_1M_RETRY_OFFSETS_MS = [700, 1500, 3000,
  6000]` ms offsets) is UNCHANGED; backfill + post-session sweep UNCHANGED —
  never-skip capture is a hard constraint; rungs landing after the decision
  window are record-only.
- Tightened per-request timeouts: chain 10 s → 4.5 s
  (`GROWW_CHAIN_1M_REQUEST_TIMEOUT_MS = 4_500`), spot 5 s → 3.5 s
  (`GROWW_SPOT_1M_REQUEST_TIMEOUT_MS = 3_500`). Budgets re-derived: spot
  symbol budget 14 s → 11 s (ladder 6.0 s + one 3.5 s timeout = 9.5 s fits);
  chain underlying budget 15 s → 6 s (stagger ≤ 0.7 s + one 4.5 s timeout).
- AUTO-DEMOTE: any HTTP 429 on any Groww REST leg (spot, chain, contract) →
  session-scoped `AtomicBool` demotion in the new shared
  `crates/app/src/groww_rest_burst.rs` state → subsequent minutes use
  `two_wave`; if already `two_wave`, a 350 ms intra-wave stagger
  (`GROWW_REST_BURST_INTRA_WAVE_STAGGER_MS`) is added (slot × 350 ms before
  each request in a wave). One coded warn on the demotion edge (REUSE
  SPOT1M-01 / CHAIN-02 with the NEW stage value `burst_demoted` — no new
  ErrorCode variants) + counter `tv_groww_rest_burst_demoted_total`. Boot
  resets to the configured tier (process-scoped state).
- Tier selection: new config section `[groww_rest_burst]` →
  `GrowwRestBurstConfig { tier: GrowwRestBurstTier, warm_up: bool }` in
  `crates/common/src/config.rs`; serde default tier = `two_wave`, warm_up
  default OFF; `config/base.toml` ships `tier = "two_wave"`, `warm_up = true`.
- WARM-UP (config-gated, base.toml ON): at minute boundary − 4 s
  (`GROWW_REST_WARMUP_LEAD_SECS`), each leg sends ONE lightweight
  unauthenticated GET on its own long-lived client (spot: the NIFTY 1-day
  candles query; chain: the NIFTY chain URL), response discarded — the
  401/400 reply still re-establishes an idle-closed TLS/H2 connection before
  the critical window. Bounded by its own 3 s timeout
  (`GROWW_REST_WARMUP_TIMEOUT_SECS < GROWW_REST_WARMUP_LEAD_SECS`) so it can
  never push a fire late. ≤ 2 extra requests in a second that never overlaps
  the waves (4.3 s separation), counted in the budget arithmetic.
- OFF-HOURS RATE PROBE (`crates/app/src/groww_rate_probe.rs`; env-gated
  `TICKVAULT_GROWW_RATE_PROBE=1`, default OFF): escalating bursts 4 → 6 → 8 →
  11 req/s (one second each, ~10 s pause between steps, max 2 rounds, ~58
  requests total) against the granted candles endpoint; REFUSED during the
  in-session blackout window on trading days; results (per-step 429 count +
  latency min/avg/max) as structured log lines + counters
  (`tv_groww_rate_probe_requests_total{outcome}`); writes NO data tables.
  This probe's verdict is what can promote `seven_concurrent`.
- The `constants.rs` ≤ 6 req/s const-assert block (~L2187-2205) is REWRITTEN
  for the new shapes: `seven_concurrent` initial wave 7 ≤ 10; `two_wave`
  initial burst 4/s (wave separation asserted > 1,000 ms); the honest
  vendor-lag worst case (two adjacent spot rung waves inside one rolling
  second = 8 solo, + the config-OFF contract leg's 2/s = 10 = AT the broker
  ceiling) documented + asserted ≤ 10; assumed BruteX ~3/s co-tenancy
  documented in comments; per-minute totals (30 contract + 20 spot + 3 chain
  + 2 warm-up = 55) asserted ≤ 60 (20 % of the 300/min budget).
- Metrics: ALL existing metric names kept; new `tv_groww_rest_burst_tier_total`
  (static label `tier`), `tv_groww_rest_burst_demoted_total`,
  `tv_groww_rest_warmup_total{leg, outcome}`, probe counters. The
  `close_to_data_ms` histograms are untouched (they are the success measure).
- Dhan legs completely untouched. No decision consumer (§28 boundary).
  Fail-closed semantics (edge pages, forensics rows, named gaps) unchanged.

**Files:**
- `crates/common/src/constants.rs` — timeout/budget/wave/warm-up/probe
  constants + const-assert rewrite
- `crates/common/src/config.rs` — `GrowwRestBurstConfig` + `GrowwRestBurstTier`
- `config/base.toml` — `[groww_rest_burst]` section
- `crates/app/src/groww_rest_burst.rs` (NEW) — shared tier/demotion state +
  warm-up sender + pure schedule fns
- `crates/app/src/groww_rate_probe.rs` (NEW) — off-hours rate probe
- `crates/app/src/groww_spot_1m_boot.rs` — concurrent wave + tier delay +
  warm-up + demote wiring
- `crates/app/src/groww_option_chain_1m_boot.rs` — own-timer fire + concurrent
  wave + warm-up + demote wiring; min-gap/fallback machinery deleted
- `crates/app/src/groww_contract_1m_boot.rs` — 429 → demote wiring only
- `crates/app/src/main.rs` — burst state creation, spot→chain channel removal,
  probe spawn
- `crates/app/src/lib.rs` — new module declarations
- `crates/app/tests/groww_spot_1m_wiring_guard.rs`,
  `crates/app/tests/groww_chain_1m_wiring_guard.rs` — ratchets re-pinned to
  the auto-ladder contract
- Rule files: `no-rest-except-live-feed-2026-06-27.md` (§9),
  `groww-second-feed-scope-2026-06-19.md` (§38),
  `rest-1m-pipeline-error-codes.md` (§0/§2c)

## Edge Cases

- **Vendor-lag minute (all spot targets empty):** every target rides its full
  ladder; adjacent rung waves (700 → 1500 ms offsets, 800 ms apart) put
  2 × 4 = 8 requests in one rolling second solo — documented + asserted ≤ 10;
  a resulting 429 auto-demotes, and the demoted stagger (350 ms/slot, which
  the rungs inherit since offsets are measured from each target's own first
  attempt) drops the rung worst case to ≤ 5/s.
- **`seven_concurrent` on a vendor-lag minute:** initial 7 + first rung wave 4
  = 11 in one rolling second — ABOVE the 10/s ceiling; that is exactly the
  shape the probe's 11 req/s top step tests, and a live 429 demotes to
  `two_wave` within the same session. Documented honestly in the const-assert
  comment; never claimed safe.
- **429 mid-session:** demotion is edge-latched (one warn, one counter
  increment); subsequent minutes use the demoted shape; the fire that saw the
  429 completes normally (ladder never retries past its rungs).
- **Auth-class reject (401/403) with concurrent targets:** all in-flight
  targets fail their ladders (each ladder short-circuits its own remaining
  rungs on auth reject — unchanged); after collection the token cache is
  dropped once; worst case 4 doomed requests instead of the old 1 + skipped
  (the `build_auth_short_circuit_rows` skip path is removed — every target
  now has a REAL forensics row).
- **Warm-up on a slow/black-holed peer:** bounded by its own 3 s timeout
  (< the 4 s lead) — a fire can never start late because of warm-up.
- **Warm-up with no token:** deliberately sent WITHOUT Authorization — a 401
  still warms TLS/H2; the token cache is never touched (an SSM read failure
  4 s before the fire would consume the ≥ 60 s pacing floor and turn the fire
  into a no_token miss — avoided by design).
- **Boot mid-minute (< 4 s to the boundary):** warm-up is skipped for that
  boundary (lead window already passed), fire proceeds normally.
- **Rate probe invoked in-session on a trading day:** refused with one warn;
  no requests sent. Non-trading days and off-hours run normally.
- **Suspend/clock-step wakes:** the existing `fire_is_fresh` +
  `count_missed_boundaries` staleness gates are preserved verbatim on both
  legs (boundary-skip forensics + edge accounting unchanged).
- **Chain leg with spot leg disabled:** unchanged behaviour by construction —
  the chain no longer depends on the spot signal at all.
- **Contract leg (config-OFF today):** still sequenced on the chain
  minute-done signal (untouched); its 429s now ALSO demote the burst tier.

## Failure Modes

- **Groww 429-storms the burst:** auto-demote to `two_wave` (or staggered
  two_wave) for the rest of the session; 429s stay counted
  (`tv_groww_*_rate_limited_total`) and shape-captured; the demotion edge is
  one coded warn (SPOT1M-01/CHAIN-02 `stage="burst_demoted"`).
- **Both waves fail a minute:** the existing per-leg 3-consecutive-minutes
  escalation edges (persist-gated, core-keyed on spot) page exactly as
  before — no edge semantics change.
- **QuestDB down:** persist failures keep feeding the failure edges
  (SPOT1M-02 / CHAIN-03 unchanged); fetch concurrency does not change the
  discard-pending poisoned-buffer defense.
- **JoinSet task failure (unwind builds):** a joined task error synthesizes a
  `Failed` outcome with a bounded reason — never a silent missing target;
  release builds abort on panic (TICK-FLUSH-01 honesty note applies).
- **Warm-up transport failure:** counted (`outcome="error"`), debug-logged,
  never coded/paged — warm-up is best-effort by definition.
- **Rate probe token read failure:** probe aborts with one warn; no partial
  bursts.
- **Demotion state lost on restart:** by design — boot resets to the
  configured tier; a persistent 429 condition re-demotes within one minute.

## Test Plan

- `crates/common`: config round-trip tests for `[groww_rest_burst]` (absent
  section → two_wave + warm_up off; explicit values honored; unknown tier
  string rejected by serde); const-asserts compile (the budget math is
  compile-time).
- `crates/app` unit tests (`groww_rest_burst.rs`): `effective_tier` mapping
  (Seven+demoted→TwoWave, TwoWave+demoted→Staggered), `spot_fire_delay_ms`
  per tier, `intra_wave_stagger_ms` slots, demote edge returns true exactly
  once, tier `as_str` labels are the 3 static values.
- `crates/app` unit tests (`groww_rate_probe.rs`): blackout-window pure fn
  (trading-day in-session refused; off-hours/weekend allowed), step plan
  shape (4/6/8/11 × 2 rounds ≤ 60 requests), request spacing math.
- Existing spot/chain module tests updated where signatures changed (min-gap
  tests deleted with the fn; mock-server fire tests re-run against the
  concurrent path).
- Wiring guards re-pinned: chain guard asserts NO spot→chain channel, the
  own-timer fire delay const, burst wiring needles, and keeps the
  chain→contract publish pin; spot guard stub needles keep the ladder/sweep/
  forensics pins and gain the burst needles.
- Gates: `cargo fmt --check`, `cargo clippy -p tickvault-app -p
  tickvault-common --no-deps -- -D warnings`, `cargo test -p
  tickvault-common -p tickvault-app`, `bash .claude/hooks/plan-verify.sh`,
  `bash .claude/hooks/banned-pattern-scanner.sh`.

## Rollback

- Config-level: `[groww_rest_burst] tier = "two_wave"` is already the safe
  default; `warm_up = false` disables warm-up; the legs' own
  `[groww_spot_1m]/[groww_option_chain_1m] enabled = false` switches are
  unchanged and disable everything.
- Demotion is the built-in runtime rollback: any 429 self-demotes the burst
  shape without a deploy.
- Code-level: revert the code commits on this branch; the plan + rule edits
  are additive dated amendments (house style — old text retained as
  historical), so a revert leaves no dangling authority.
- The probe is env-gated OFF by default — no rollback surface.

## Observability

- Kept: `tv_groww_spot1m_*` / `tv_groww_chain1m_*` counters + histograms
  (fetch_total, close_to_data_ms, fetch_duration_ms, rate_limited_total,
  boundary_skipped_total, persist_errors_total, task_respawn_total, …) —
  names unchanged; `close_to_data_ms` is the success measure for the < 5 s
  goal.
- New: `tv_groww_rest_burst_tier_total{tier}` (one increment per leg fire —
  static labels `seven_concurrent` / `two_wave` / `two_wave_staggered`),
  `tv_groww_rest_burst_demoted_total` (one per demotion edge),
  `tv_groww_rest_warmup_total{leg, outcome}`,
  `tv_groww_rate_probe_requests_total{outcome}` +
  `tv_groww_rate_probe_rate_limited_total`.
- New coded stages (documented in `rest-1m-pipeline-error-codes.md`):
  SPOT1M-01 `stage="burst_demoted"` (spot/contract 429 demotion edge),
  CHAIN-02 `stage="burst_demoted"` (chain 429 demotion edge). No new
  ErrorCode variants; no new pager routes (both codes stay log-sink-only per
  the existing §3 delivery boundary; the typed Telegram edges are unchanged).
- Probe results: structured `info!` lines per step (rps, sent, ok, 429s,
  latency min/avg/max) — log-sink only, no tables.

---

## Plan Items

- [x] Item 1 — Plan file (this document)
  - Files: .claude/plans/active-plan-groww-rest-autoladder.md
  - Tests: n/a (docs)
- [x] Item 2 — Rule-file dated amendments (auto-ladder contract)
  - Files: .claude/rules/project/no-rest-except-live-feed-2026-06-27.md,
    .claude/rules/project/groww-second-feed-scope-2026-06-19.md,
    .claude/rules/project/rest-1m-pipeline-error-codes.md
  - Tests: n/a (docs)
- [x] Item 3 — Constants + config + base.toml
  - Files: crates/common/src/constants.rs, crates/common/src/config.rs,
    config/base.toml
  - Tests: test_groww_rest_burst_config_defaults_and_round_trips,
    const-asserts (compile-time)
- [x] Item 4 — Shared burst state + warm-up module
  - Files: crates/app/src/groww_rest_burst.rs, crates/app/src/lib.rs
  - Tests: test_effective_tier_mapping, test_spot_fire_delay_per_tier,
    test_intra_wave_stagger_slots, test_demote_edge_fires_once,
    test_tier_labels_are_static
- [x] Item 5 — Spot leg auto-ladder rebuild
  - Files: crates/app/src/groww_spot_1m_boot.rs
  - Tests: existing module tests updated; test_fire_token_path_found_and_empty_via_mock
    (concurrent path), wiring guard
- [x] Item 6 — Chain leg auto-ladder rebuild
  - Files: crates/app/src/groww_option_chain_1m_boot.rs
  - Tests: existing module tests updated (min-gap tests removed with the fn);
    test_fetch_once_classifies_auth_rate_limit_5xx_and_transport
- [x] Item 7 — Contract leg 429→demote wiring
  - Files: crates/app/src/groww_contract_1m_boot.rs
  - Tests: existing contract tests (429 classification) unchanged
- [x] Item 8 — main.rs wiring + rate probe
  - Files: crates/app/src/main.rs, crates/app/src/groww_rate_probe.rs
  - Tests: test_rate_probe_blackout_window, test_probe_step_plan_bounds
- [x] Item 9 — Wiring-guard ratchets re-pinned
  - Files: crates/app/tests/groww_spot_1m_wiring_guard.rs,
    crates/app/tests/groww_chain_1m_wiring_guard.rs
  - Tests: the ratchets themselves

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Healthy minute, two_wave | chain data ~0.5 s, spot data ~1.6 s after close; burst ≤ 4/s |
| 2 | Healthy minute, seven_concurrent | all 7 responses ~0.5 s after close; burst 7/s |
| 3 | Vendor-lag minute (spot empty) | ladder rungs re-poll; late rungs record-only; empty counted; ≤ 10/s solo |
| 4 | 429 on any leg | one coded warn + demote counter; next minute two_wave/staggered |
| 5 | Token dead at fire | all targets no_token/auth-fail with forensics rows; cache dropped once |
| 6 | Warm-up peer black-holed | 3 s timeout; fire unaffected |
| 7 | Probe run at 11:00 IST on a trading day | refused with one warn, zero requests |
| 8 | Probe run Saturday | 2 rounds × 4 steps, ~58 requests, structured verdict lines |
| 9 | Chain leg alone enabled (spot off) | chain fires on own timer at +300 ms — no signal dependency |
| 10 | Restart after demotion | configured tier restored; re-demotes on the next 429 |

## Per-Item Guarantee Matrix

Per `.claude/rules/project/per-wave-guarantee-matrix.md` — the 15-row and
7-row matrices apply to every item above; this section instantiates them for
this plan.

### 15-row 100% Guarantee Matrix

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | new pure fns unit-tested; `quality/crate-coverage-thresholds.toml` ratcheted floors | post-merge llvm-cov | coverage delta ≥ 0 |
| 100% audit coverage | `rest_fetch_audit` rows unchanged (success AND failure per target) | `mcp__tickvault-logs__questdb_sql` | no forensics row removed |
| 100% testing coverage | unit + serde round-trip + mock-server tokio tests + source-scan ratchets (app: 2 required categories; common: 12) | `cargo test -p tickvault-common -p tickvault-app` | green before commit 3 |
| 100% code checks | banned-pattern + plan-verify + pre-commit gates | pre-push mandatory | all gates green |
| 100% code performance | cold-path only — no hot-path involvement (no DHAT/bench needed; N/A — scheduled REST tasks) | n/a | no per-tick code touched |
| 100% monitoring | tier/demote/warm-up/probe counters + kept histograms | /metrics | new counters static-labeled |
| 100% logging | every error!/warn! naming a code carries `code = ErrorCode::X.code_str()` | errors.jsonl | tag-guard test |
| 100% alerting | existing typed Telegram edges unchanged (persist-gated 3-minute escalation) | edge tests | no edge semantics change |
| 100% security | token only in Authorization header; warm-up sends NO credentials; probe redacts bodies via `capture_rest_error_body` | secret-scan | no new secret surface |
| 100% security hardening | body caps + redirect Policy::none preserved on all clients | existing tests | unchanged |
| 100% bugs fixing | adversarial review by the parent session post-task | PR review | deviations declared |
| 100% scenarios covering | the 10-scenario table above; each maps to a test or documented bound | tests | scenario column |
| 100% functionalities covering | every new pub fn has a test or TEST-EXEMPT + a call site | pre-push gates 6+11 | wiring guards |
| 100% code review | parent-session review before PR | per-PR | this task does not push |
| 100% extreme check | const-asserts fail the BUILD on budget-math regression; wiring guards fail on contract drift | every commit | ratchets in commit 3 |

### 7-row Resilience Demand Matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | untouched — cold-path REST legs only; WAL/ring/spill/DLQ not involved | no tick-path file changed |
| WS never disconnects | untouched — no WS code changed | diff scope |
| Never slow/locked/hanged | cold scheduled tasks; every request timeout-bounded (3.5/4.5 s), every fire budget-bounded (11/6 s), warm-up 3 s-bounded | const-asserts |
| QuestDB never fails | ABSORB unchanged — discard-pending flush defense + DEDUP-idempotent re-appends preserved | persist paths unchanged |
| O(1) latency | n/a hot path; the burst decision is one atomic load per fire (honestly O(1)); the fires themselves are bounded O(targets) cold-path HTTP — flagged, never claimed O(1) | code comments |
| Uniqueness + dedup | `spot_1m_rest` / `option_chain_1m` / `rest_fetch_audit` DEDUP keys untouched | no schema change |
| Real-time proof | `close_to_data_ms` histograms + per-row stamps measure the < 5 s claim live; tier counter names the shape in force | metrics |

### Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the
< 5 s decision-data claim is BY CONSTRUCTION for the request schedule (chain
wave at +300 ms + ~0.2 s measured round-trip; spot wave at +1,350 ms +
~0.2 s; both bounded by 4.5 s/3.5 s timeouts ⇒ worst in-schedule response
< 5 s after close) and MEASURED live by the untouched `close_to_data_ms`
histograms — vendor serving lag (a minute Groww has not sealed yet) remains
UNVERIFIED-LIVE and is absorbed by the unchanged ladder/backfill/sweep as
record-only recovery, never claimed as decision data. Groww's true burst
tolerance is UNKNOWN until the off-hours probe runs; the shipped default
never exceeds 4 req/s at the boundary and auto-demotes on the first 429.
