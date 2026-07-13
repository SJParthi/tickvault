# Implementation Plan: Dhan Live WS Feed OFF — Phase A (flip + REST-only bootstrap)

**Status:** APPROVED
**Date:** 2026-07-13
**Approved by:** Parthiban (operator) — 2026-07-13 directive relayed verbatim via the coordinator session

> **Operator directive (2026-07-13, verbatim):**
> "now remove this entire Dhan live websocket feed instruments subscription even
> entire live websocket feed itself... As of now only Groww and Dhan historical
> api pull as we discussed last night along with option chain."
>
> **Rationale (verbatim):** "when we checked the live websocket feed candles and
> historical data api candles for Dhan has a massive major mismatches... that's
> why I want to remove this. For Groww let us have live websocket feed api as of
> now."

Phase A goal: Dhan live WS OFF by default at next boot; Groww stays the live
feed; the Dhan REST retained surface (token/auth stack, per-minute
`spot_1m_rest`, per-minute `option_chain_1m` + entitlement probe, REST canary)
KEEPS RUNNING without the WS lane. Full deletion is a LATER phase — Phase A is
reversible (config) + additive (bootstrap); NO lane code is deleted.

Crates touched: **tickvault-app**, **tickvault-api**, **tickvault-common**
(plus `config/*.toml` + `deploy/aws/terraform/market-hours-liveness-alarm.tf`).

Per-item guarantee matrix: cross-reference
`.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row matrices
apply; this item's specifics are in the sections above).

## Design

1. **Config flips** — `config/base.toml` + `config/production.toml` `[feeds]`
   set `dhan_enabled = false` with a dated 2026-07-13 comment. Groww stays
   `true` in production.toml (the live feed), untouched in base.toml.
2. **Ratchet update** — `crates/common/tests/production_config_wiring.rs`:
   `test_production_runs_both_feeds` (2026-06-30 both-feeds lock) is replaced
   by `test_production_groww_live_dhan_rest_only` pinning the NEW locked
   state: production.toml `dhan_enabled = false` AND `groww_enabled = true`;
   base.toml `dhan_enabled = false`. Carries the verbatim quote and the
   supersession note.
3. **Overlay hardening** — `crates/api/src/feed_state_persist.rs`
   `overlay_feeds` becomes narrow-only for Dhan: effective dhan =
   `config.feeds.dhan_enabled && persisted.dhan_enabled` (a stale
   `data/feed-state.json` with `dhan_enabled: true` can no longer resurrect
   the retired lane). Groww overlay semantics UNCHANGED (persisted wins both
   ways). New pure predicate `dhan_overlay_suppressed` lets the main.rs
   application site log ONE `warn!` naming the 2026-07-13 directive when a
   widening overlay is suppressed.
4. **Runtime cold-start gate** — `main.rs::run_dhan_lane_runtime_supervisor`
   refuses to spawn `run_dhan_lane_cold_start` when the RAW boot TOML
   retires the lane (`feed_runtime.is_dhan_config_enabled() == false` —
   round-2 FIX A; a POST /api/feeds/dhan enable would otherwise cold-start
   the full lane and collide with the REST-only auth stack: dual-instance
   SSM lock AlreadyHeld → DHAN-LANE-03 retry loop + pages). Refusal is
   edge-latched once per attempt-episode (re-arms when the desired-ON flag
   drops). REVISED (review round 1, FIX 3): the /feeds POST handler ALSO
   refuses the Dhan ENABLE direction with 409 CONFLICT on the same
   condition (both trading modes; runtime flag not flipped, nothing
   persisted — the pre-fix accept was a Rule-11 false-OK: /feeds showed ON
   while nothing could start). REVISED (review round 2, FIX A): BOTH gates
   key off the RAW pre-overlay TOML value (`dhan_config_enabled_raw`
   captured in main.rs BEFORE `overlay_feeds`, threaded via
   `FeedRuntimeState::from_config_with_dhan_config`) — seeding from the
   post-overlay effective value would let a persisted runtime-OFF overlay
   permanently 409-lock a config-ON boot with a misleading "config change +
   restart" message (breaking the PR-E disable→restart→re-enable round
   trip). A config-ON + overlay-OFF boot is dormant-not-retired: the toggle
   re-enable stays ALLOWED and cold-starts the full lane (pre-Phase-A
   behavior); the REST-only stack does NOT spawn on that boot (same raw
   gate — no lock collision possible). Boot-ON semantics and the PR-E
   disable gate + bearer auth are unchanged.
5. **THE CORE — Dhan REST-only auth bootstrap** — new module
   `crates/app/src/dhan_rest_stack.rs` (`spawn_dhan_rest_stack`), spawned from
   main.rs's Dhan-OFF branch (the `else` of `if config.feeds.dhan_enabled`,
   i.e. the "DHAN LANE SKIPPED" arm — process-global scope, after
   `build_shared_infra` + the metrics recorder install), and ONLY when the
   RAW boot TOML retires the lane (`is_dhan_config_enabled() == false` —
   round-2 FIX A; a config-ON + overlay-OFF boot must not hold the SSM
   lock, since its runtime re-enable cold-starts the full lane). One
   background task:
   a. dual-instance SSM lock BEFORE any mint (`try_acquire_instance_lock` +
      `spawn_instance_lock_heartbeat`, lock-before-mint per
      `dual-instance-lock-2026-07-04.md` §2, addendum §3.5). REVISED
      (review round 1, FIX 1 — lurk-and-steal): AlreadyHeld gets a BOUNDED
      patience window (300s cumulative backoff ≥ 3× the 90s TTL — a
      same-machine previous process's stale entry always clears inside it),
      then ONE TTL-deferred DualInstanceDetected page + a PERMANENT PARK
      (no further SSM polling — a live peer's lock can never be seized via
      the stale-takeover; restart is the only retry). SSM transport errors
      (`stage="ssm_transport"`) retry with bounded backoff forever in the
      background. NEVER halts the process; Groww/shared infra never blocked.
      Holder machine identity is not reliably comparable (all host_ids are
      `local:`-prefixed), so the bounded window applies to ALL AlreadyHeld.
   b. `TokenManager::initialize` (SSM → TOTP → JWT) with the SAME
      `instance_lock_held` wiring (RESILIENCE-03 tripwire), then
      `set_global_token_manager` + `feed_runtime.set_live_token_manager` so
      the token gauges read this manager. Auth failure → log + backoff retry
      in background.
   c. token renewal loop (`spawn_renewal_task`) + mid-session profile
      watchdog (`spawn_mid_session_profile_watchdog` — needs only
      TokenManager + notifier + a fresh profile-valid AtomicBool → spawned).
   d. REST canary (`rest_canary_boot::run_rest_canary`), spot_1m_rest
      scheduler (`spot_1m_rest_boot::spawn_supervised_spot_1m_rest`),
      option_chain_1m scheduler + entitlement probe
      (`option_chain_1m_boot`) — each with a `TokenHandle` from THIS
      TokenManager, mirroring `spawn_post_market_tasks` exactly (incl. the
      spot→chain watch-channel sequencing + the existing `[spot_1m_rest]` /
      `[option_chain_1m]` config gates). NOT spawned (stay lane-only): WS
      pool, universe build/CSV download, prev-day OHLCV, SLO publisher,
      cross-verify, EOD digest, orphan watchdog, order-update WS.
   e. Mutual exclusion by construction: spawned ONLY from the Dhan-OFF
      branch AND only when the raw TOML retires the lane; the runtime
      cold-start is refused on the same raw gate (item 4); a
      process-global once-guard rejects double-spawn.
   f. Observability: `tv_dhan_rest_stack_up` gauge (0 at bring-up start, 1 on
      stack-up), one `info!` naming what was spawned; failures use `error!`
      with existing ErrorCodes where they fit (NO new ErrorCode variants).
6. **Alarm fix** — `deploy/aws/terraform/market-hours-liveness-alarm.tf`:
   `tv-<env>-market-hours-liveness-missing` re-points from
   `tv_realtime_guarantee_score` (published ONLY by the lane-owned SLO
   publisher → missing every Dhan-off market day → deterministic false page
   ~09:25 IST) to `tv_groww_exchange_lag_p99_seconds` (published
   process-global in-session by the Groww lag publisher; already
   CloudWatch-shipped per silent-feed-alarms.tf S4; same
   `local.app_dimensions` shape as the groww-exchange-lag-p99-high alarm).
   Missing Groww lag inside the gated window now means process-dead OR
   Groww-feed-dead — both page-worthy.
7. **New ratchets** — `crates/app/tests/dhan_live_off_phase_a_guard.rs`:
   (a) both TOMLs carry `dhan_enabled = false`; (b) main.rs spawns the REST
   stack in the Dhan-OFF branch; (c) the overlay AND-gate exists in
   feed_state_persist.rs; (d) the runtime cold-start refusal gate reads the
   RAW config snapshot and the spawn lives inside its else arm
   (brace-matched — round-2 FIX C; an inverted gate, an emptied gate body,
   an overlaid-config regression, or an unconditional/hoisted spawn all
   fail the build).

## Edge Cases

- **Stale `data/feed-state.json` with `dhan_enabled: true`** (last webpage
  toggle before the directive): the AND-gate suppresses it → effective false
  + ONE warn naming the directive. Groww's persisted choice still honored.
- **Corrupt/missing overlay**: unchanged fail-safe (config default).
- **Operator toggles Dhan ON via POST /api/feeds/dhan at runtime**
  (REVISED, FIX 3): with the RAW TOML carrying `dhan_enabled = false`, the
  API REFUSES with 409 (directive + restart path named in the body); the
  runtime flag never flips, nothing is persisted, /feeds keeps showing OFF.
  The supervisor's edge-latched refusal remains as defence-in-depth behind
  it. No lane, no double auth stack.
- **Config-ON boot + persisted runtime-OFF overlay** (round-2 FIX A): the
  lane is dormant, NOT retired — the boot leaves Dhan runtime-off, the
  REST-only stack does NOT spawn (raw gate), and the toggle re-enable is
  ALLOWED: the D2b cold-start brings up the full lane (which owns the Dhan
  REST surface via `spawn_post_market_tasks`). Pre-Phase-A behavior
  restored; honest residual: the Dhan REST surface is absent on such a
  boot until the operator re-enables (exactly as pre-Phase-A). Latent
  today (both shipped TOMLs carry false).
- **Quick restart (<90s) after a stop**: the previous process's REST-stack
  SSM lock entry is not yet stale → AlreadyHeld → the stack retries with
  backoff inside the bounded patience window and acquires once the 90s TTL
  clears it (well inside the 300s park threshold). The Telegram
  DualInstanceDetected page fires only from attempt 5 (~150s cumulative
  backoff > TTL) so the self-stale window never pages; `error!` lines are
  coalesced (first + every 10th attempt).
- **TV_ENVIRONMENT=dev/local boots** (base.toml only): dhan_enabled=false +
  groww_enabled=false = a no-feed config — boots with the existing WARN
  (main.rs `any_enabled()` arm), REST stack still runs (it is gated only on
  the RAW dhan_enabled=false), matching "Dhan REST retained surface keeps
  running".
- **Non-trading day / late boot**: canary/spot/chain tasks self-skip exactly
  as they do today (audit Rule 3 gates live inside those modules).
- **spot/chain config gates**: `[spot_1m_rest].enabled` /
  `[option_chain_1m].enabled|probe_and_report` respected byte-identically
  (the spawn arms are copied from `spawn_post_market_tasks`).
- **Fast crash-recovery boot arm**: gated on `config.feeds.dhan_enabled`
  (main.rs ~1815) so with Dhan off it NEVER runs — the slow path (and hence
  the Dhan-OFF branch spawn site) is reached on every Dhan-off boot; the
  spawn site is therefore effectively on ALL Dhan-off boot arms.

## Failure Modes

- **SSM unreachable at stack start** → lock acquire retries forever (cap
  300s), coalesced `error!`; no token, so canary/spot/chain are not yet
  spawned; Groww capture unaffected.
- **AlreadyHeld persists (genuine peer)** (REVISED, FIX 1) → RESILIENCE-01
  `error!` coalesced + ONE DualInstanceDetected Telegram (attempt ≥ 5,
  ~150s), then at 300s+ of cumulative patience the stack PARKS permanently
  (terminal `error!` + parked `info!`; restart is the only retry) — it
  never lurks, so it can never seize the peer's lock via the 90s
  stale-takeover when the peer restarts. Fail-safe both ways: the peer's
  token is never invalidated (lock-before-mint + park).
- **Auth permanently failing (rotated TOTP — AUTH-GAP-04 class)** →
  TokenManager's own internal retries + AUTH-GAP-04 emission fire as today;
  the stack's outer loop retries at the 300s cap; canary/spot/chain absent
  until auth succeeds (honest: tables get no rows; the SPOT1M/CHAIN failure
  edges cannot fire because the tasks are not yet spawned — the stack-up
  gauge stays 0 and the coalesced error! lines are the signal).
- **Stack task dies (unwind builds)** → a monitor task logs `error!`;
  release builds abort the process on panic (`panic = "abort"` — the
  TICK-FLUSH-01 honesty note; no in-process respawn is claimed).
- **Graceful shutdown** → the REST stack's SSM lock heartbeat has no
  shutdown-notify chain (lane-only Item 19f wiring); the lock entry is
  cleared by the 90s TTL on the next boot; the next stack's retry loop
  absorbs it (see Edge Cases). Documented honest envelope, not silent.
- **Alarm re-point**: if the Groww lag gauge is never set in a session
  (Groww dead/thin from boot), the liveness alarm pages — by design
  (Groww-feed-dead is page-worthy now that Groww is THE live feed).

## Test Plan

- `crates/api` unit tests (feed_state_persist.rs): stale overlay dhan=true +
  config false → false; config true + overlay false → false; groww unchanged
  both ways; `dhan_overlay_suppressed` predicate truth table.
  Tests: `test_overlay_feeds_dhan_and_gate_suppresses_stale_widen`,
  `test_overlay_feeds_dhan_narrow_still_wins`,
  `test_overlay_feeds_groww_semantics_unchanged`,
  `test_dhan_overlay_suppressed_predicate`.
- `crates/common` — `test_production_groww_live_dhan_rest_only` (replaces
  `test_production_runs_both_feeds`).
- `crates/app` — new guard `dhan_live_off_phase_a_guard.rs` (5 tests, source
  scans per §7 of Design; Pin 4 is region-scoped + brace-matched into the
  gate's else arm per review FIX 6 + round-2 FIX C); `dhan_rest_stack.rs`
  unit tests for the pure helpers
  (`dhan_rest_stack_backoff_secs`, `dhan_rest_retry_should_log`, bounded
  AlreadyHeld patience + page-then-park math, ssm-transport retry,
  `dhan_rest_token_backoff_secs` ≥130s mint-cooldown floor, the shared
  `claim_post_market_task_family_once` once-guard).
- `crates/api` (FIX 3) — handler tests: Dhan enable refused 409 in BOTH
  trading modes when config-off (flag not flipped), Dhan disable still a
  no-op success, Groww toggles unaffected, config snapshot immutable.
- `crates/api` (round-2 FIX A) — raw-seeding truth table:
  `test_dhan_config_enabled_seeded_from_raw_toml_not_overlay`
  (feed_state.rs — runtime seeds effective, gate seeds raw, both
  directions) + `test_set_feed_dhan_enable_gate_keys_off_raw_config_not_overlay`
  (feeds.rs — config-ON + overlay-OFF enable ALLOWED; raw-false stays 409
  regardless of the effective value).
- Full suites: `cargo test -p tickvault-common -p tickvault-api -p
  tickvault-app` + fmt + clippy + banned-pattern scanner + plan-gate.
- Files: `config/base.toml`, `config/production.toml`,
  `crates/common/tests/production_config_wiring.rs`,
  `crates/api/src/feed_state_persist.rs`, `crates/app/src/main.rs`,
  `crates/app/src/lib.rs`, `crates/app/src/dhan_rest_stack.rs`,
  `crates/app/tests/dhan_live_off_phase_a_guard.rs`,
  `deploy/aws/terraform/market-hours-liveness-alarm.tf`.

## Rollback

- **Config-only revert**: set `dhan_enabled = true` in production.toml (and
  update `production_config_wiring.rs` + the phase-A guard with a fresh dated
  operator quote) → the lane boots exactly as before; the REST stack does not
  spawn (its gate is the RAW `!dhan_enabled`); `spawn_post_market_tasks` resumes
  owning canary/spot/chain. The overlay AND-gate then has no effect
  (config=true && persisted wins = old behaviour for persisted=false → false,
  persisted=true → true — byte-identical to the pre-Phase-A overlay).
- **Alarm revert**: restore `metric_name = "tv_realtime_guarantee_score"` in
  market-hours-liveness-alarm.tf (one line + comments).
- No data migration, no schema change, no deleted code — Phase A is
  flip + bootstrap only, reversible by construction.

## Observability

- `tv_dhan_rest_stack_up` gauge (0/1) + one stack-up `info!` naming spawned
  subsystems.
- Existing codes reused at failure sites: RESILIENCE-01 (lock), DH-901
  (auth init exhausted/timeout arms, log-only + retry). NO new ErrorCode
  variants in this PR (rule: error_code_tag_guard — any error! naming a
  known code carries the `code =` field).
- Token gauges (`tv_token_remaining_seconds` / headroom reads) work via
  `set_live_token_manager` + the renewal loop; the mid-session profile
  watchdog pages CRITICAL on real /v2/profile invalidation as today.
- Canary/spot/chain keep their existing 7-layer observability
  (REST-CANARY-01, SPOT1M-01/02, CHAIN-01..04) unchanged.
- The market-hours liveness alarm now watches the Groww lag gauge — dated
  comment block in the tf explains the signal move + honest envelope.
- Runtime-enable refusal: 409 at the API layer (FIX 3) + the edge-latched
  supervisor `warn!` naming the 2026-07-13 directive; overlay suppression:
  one boot-time `warn!` naming the directive.
- Honest pager gap (flagged follow-up for Phase C): a REST stack wedged in
  its lock/auth retry loops — or PARKED after the bounded AlreadyHeld
  patience window (the FIX 1 park state) — is LOG-visible only (coalesced
  `error!`, plus the one DualInstanceDetected Telegram on the peer case)
  but has NO CloudWatch pager: `tv_dhan_rest_stack_up` is not CW-shipped
  (not in the EMF allowlist) and RESILIENCE-01 has no errcode alarm entry —
  the entire retained Dhan REST surface (incl. the canary, which lives
  inside the stack) can be absent all session without a page.
