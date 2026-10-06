# FINAL SPEC data-silent (ship=true)

## Summary
The defect is real, and there are two parts (Verified on origin/main).

(1) `FRAME_SILENCE_REDIAL_SECS = 300` is one threshold for every socket kind. `ConnectionSupervisor::poll` never reads `slot.endpoint`. So a deaf main-feed or depth-20 socket stays blind for 5 minutes, and the deaf alarm pages at 600 s.

(2) The bigger problem: the same 300 s rule closes and redials HEALTHY but quiet depth-200 sockets. Each one carries a single contract, and the repo itself measured 56 minutes of benign silence on one (`worst_connection_tick_age_secs` excludes depth-200 for that reason). The watchdog test pins this redial: `a_ponging_socket_that_delivers_no_frame_is_redialled_after_the_frame_silence_window` uses Depth200 and expects a redial at 300 s.

The fix uses evidence instead of time alone.
- **Main feed and depth-20:** redial at 60 s / 90 s, but only when at least 2 OTHER sockets of the same kind delivered in the last 10 s. This applies only within 09:15–15:15 IST, only when the socket holds enough instruments, only with no 805 halt or overflow episode, and only with a per-kind spacing between such redials. The threshold escalates per socket on every redial that does not cure it. Otherwise the 300 s rule applies as today.
- **Depth-200:** a 900 s time backstop, plus a fast path. If the main feed shows its contract trading 30 s or more after the depth socket's last frame (taken from the read task, so it is immune to the depth shed), the socket is redialled.

The fast paths ship in SHADOW mode by default: they count "would redial" for a week before a one-line config flip to act. That makes the unmeasured gap figures measured before anything acts on them. The depth-200 change (fewer redials) is active from merge. The alarm stays at 600 s.

Review points folded in:
- Read-task wall-ms baseline. Both reviewers were right: the drain's stamp freezes during a depth shed.
- Escalation. R1 was right: the first frame after every dial resets `attempt`, so a deaf socket that bursts a snapshot after each dial loops every ~61 s and never reaches the flap ceiling.
- A dial-generation tag on the cross-feed request.
- `rotation_halted()` gating, with a `data_silence` refusal label.
- Publishing `depth200_held` at every place the guard changes.
- `slot_owner` instead of a new `SLOT_KIND`.
- Instrument-count floor.
- At least 2 siblings, to cover a stall of the F&O segment only.
- Spacing between fast redials pool-wide.
- Explicit evidence opt-in for test isolation.
- A noise-lock row recording how this interacts with the reconnect-slow and deaf alarms.

Dropped or changed:
- The design's 1,800 s depth-200 backstop: changed to 900 s. The 56-minute evidence is weaker than the design presented (it ran partly pre-open, under the old re-fit), and a deaf contract the main feed does not track needs recovery in under 30 minutes.
- R2's claim that every false redial pages reconnect-slow: kept only as a recorded interaction. A healthy main socket gets a frame pushed on subscribe (the code comment says the prev-close packet arrives before the ack), so its recovery sample is about 2 s. The alarm definition is unchanged.
- Separate per-segment sibling classes: replaced by the at-least-2-siblings rule. It covers the same failure without new state.

Scope: crates core and app, one config field (common, or wherever the live-lane config struct lives, so workspace tests run), rule files, plan. Socket close/redial behaviour stays a genuine-fault redial under scope-lock 2026-10-01 (reason label `IdleSilence`, unchanged). `ROTATION_HALTED` is only ever READ.

0. PLAN AND RULE FILES FIRST (design-first wall)
- Add `.claude/plans/active-plan-frame-silence-per-kind.md`: Status APPROVED; six sections (Design / Edge Cases / Failure Modes / Test Plan / Rollback / Observability); names `tickvault-core` and `tickvault-app`; 15+7 matrix by cross-reference. Keep the active-plan count at or below 5.
- `docs/claude-rules-full/project/websocket-connection-scope-lock.md`: new section "2026-10-06 — PER-KIND FRAME-SILENCE THRESHOLDS (fault redials only)". Quote verbatim: "Go ahead with whatever you want dude" and "See do everything whatever is recommended dude okay?". Record:
  - main 60 s / depth-20 90 s, sibling-confirmed (at least 2 live same-kind siblings within 10 s), only 09:15:00–15:15:00 IST, instrument floors, escalation, per-kind spacing;
  - 300 s fallback otherwise;
  - depth-200: cross-feed 90 s silence with a 30 s trade lead, plus a 900 s backstop;
  - all fast paths off while `OVERFLOW_ENGAGED` or `rotation_halted()`;
  - the cross-feed request is refused after an 805 with path `data_silence`;
  - shadow mode by default.
  Amend the 2026-10-02 inventory row "idle and silence watchdogs | redial | kept" to add "cross-feed (drain-inferred) silence: kept as a fault redial, refused after 805".
  REJECT rows:
  - a pure-time depth-200 threshold below 900 s;
  - any fast path without sibling or cross-feed evidence;
  - cross-kind or single-sibling evidence;
  - firing while an overflow episode is engaged or after an 805;
  - removing escalation;
  - a confirmed threshold where threshold × `FLAP_REDIAL_CEILING` <= `FLAP_WINDOW_MS`/1000;
  - a new `ReconnectReason` variant without a dated quote.
- `.claude/rules/project/websocket-connection-scope-lock.md` (summary stub): one bullet pointing at the new section.
- `docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md`: new dated row §2.11 (2026-10-06) with the same two verbatim quotes. It records:
  - no alarm definition changes: `dhan-worst-socket-deaf` stays at 600 s / 300 s / Maximum / eval 1, and `dhan-main-reconnect-slow` stays at 15,000 ms;
  - faster frame-silence redials can now feed main reconnect-recovery samples earlier. A healthy socket's sample is about 2 s (a frame is pushed on subscribe). A sample of 15 s or more means the redial did not cure the socket, which is the alarm's intended meaning;
  - pre-existing: during `ShedLevel::AllDepth`, depth-20 `PER_CONN_LAST_TICK_MILLIS` stamps freeze. A fix would be a page change and is not made here;
  - the new counters are local-only (no EMF, no alarm).
  REJECT: lowering 600 or editing either `alarm_description` without a further dated row.

1. CONSTANTS (`crates/core/src/websocket/pool_supervisor.rs`, next to `FRAME_SILENCE_REDIAL_SECS`)
- Keep `FRAME_SILENCE_REDIAL_SECS = 300` and `FRAME_SILENCE_REDIAL`. Rewrite its doc: it is the UNCONFIRMED fallback for main feed and depth-20. Delete the false sentence "a depth socket carries an at-the-money contract, so five minutes of silence there is a dead subscription" and cite the 56-minute measurement.
- `pub const MAIN_FEED_CONFIRMED_SILENCE_REDIAL_SECS: u64 = 60;`
- `pub const DEPTH20_CONFIRMED_SILENCE_REDIAL_SECS: u64 = 90;`
- `pub const DEPTH200_SILENCE_BACKSTOP_SECS: u64 = 900;`
- `pub const SIBLING_LIVE_EVIDENCE_SECS: u64 = 10;`
- `pub const MIN_LIVE_SIBLINGS: u8 = 2;`
- `pub const MAIN_FEED_CONFIRMED_MIN_HELD: u32 = 1_000;`
- `pub const DEPTH20_CONFIRMED_MIN_HELD: u32 = 20;`
- `pub const CONFIRMED_REDIAL_SPACING_SECS: i64 = 15;` (per kind, pool-wide)
- `pub const SILENCE_STRIKE_RESET_SECS: u64 = 900;`
- `pub const CONFIRMED_SILENCE_CLOSE_SECS_OF_DAY_IST: u32 = tickvault_common::constants::CAS_WINDOW_OPEN_SECS_OF_DAY_IST;` (15:15)
- `const FRAME_ACTIVITY_PUBLISH_INTERVAL: Duration = Duration::from_secs(1);`
- `pub const FRAME_SILENCE_REDIAL_METRIC: &str = "tv_dhan_ws_frame_silence_redial_total";` (labels `endpoint`, `basis`)
- `pub const FRAME_SILENCE_WOULD_REDIAL_METRIC: &str = "tv_dhan_ws_frame_silence_would_redial_total";` (same labels)
- `pub const FRAME_GAP_MAX_SECS_GAUGE: &str = "tv_dhan_ws_conn_frame_gap_max_secs";` (label `connection`; local-only; peak inter-frame gap inside 09:15–15:15, reset at 09:15)
- Const-asserts, each confirmed value:
  - > `IDLE_RECONNECT_TIMEOUT_SECS`;
  - > 2 × `CLIENT_KEEPALIVE_PING_INTERVAL` secs;
  - < `FRAME_SILENCE_REDIAL_SECS` < `DEPTH200_SILENCE_BACKSTOP_SECS`;
  - `SIBLING_LIVE_EVIDENCE_SECS*3 <= MAIN_FEED_CONFIRMED_SILENCE_REDIAL_SECS`;
  - confirmed × `FLAP_REDIAL_CEILING as u64 > FLAP_WINDOW_MS/1000`;
  - `SILENCE_STRIKE_RESET_SECS > FRAME_SILENCE_REDIAL_SECS`;
  - `CONTINUOUS_SESSION_OPEN < CONFIRMED_SILENCE_CLOSE <= CONTINUOUS_SESSION_CLOSE`.

2. POLICY TYPES (pool_supervisor.rs)
- `#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, serde::Deserialize)] pub enum FrameSilenceFastPath { Off, #[default] Shadow, Act }`
- `pub enum SilenceBasis { Unconfirmed, SiblingConfirmed, CrossFeed, Backstop }` with `as_str` and `ALL`.
- `pub enum SiblingEvidence { Off /* default */, Pool }`.
- Supervisor setters, cold, called from the app spawn loop beside `set_frame_silence_gate`: `set_silence_evidence(SiblingEvidence)`, `set_fast_path(FrameSilenceFastPath)`, `set_held_instruments(u32)`.
- `FrameSilenceGate::confirmed_window_open_at(secs: u32) -> bool`: `ContinuousSessionIst` → [09:15:00, 15:15:00); `AlwaysOn` → true; `Off` → false. `is_open` and `overflow_probe_window_open` are UNCHANGED.

3. ACTIVITY REGISTER (pool_supervisor.rs)
- `#[repr(align(64))] struct PaddedI64(AtomicI64);` and `static SLOT_LAST_FRAME_WALL_MS: [PaddedI64; GHOST_REDIAL_SLOTS]` (0 = never). Values are WALL ms, the same clock as `TickObservation.recv_monotonic_millis`, which is `received_at_nanos/1e6` despite its name (Verified).
- New supervisor field `next_activity_publish_at: Instant`. In the `ConnEvent::FrameReceived` arm (or at its dispatch in `handle_socket_event`, which has the frame's receipt nanos used for the WAL; Assumed available, otherwise one coarse wall read inside the gate):
  `if now >= next_activity_publish_at { SLOT_LAST_FRAME_WALL_MS[idx].store(recv_wall_ms, Relaxed); next_activity_publish_at = now + FRAME_ACTIVITY_PUBLISH_INTERVAL; }`
  Also track the local gap peak: `gap = now - last_frame_at` before overwrite, `max_gap = max`. One compare per frame, published by the drain's publish tick (O(32)).
- `pub fn last_frame_wall_ms(idx: u8) -> Option<i64>`.
- Pure `pub fn live_siblings(self_idx: u8, endpoint: DhanEndpointType, now_wall_ms: i64, stamps: &[i64]) -> u8`:
  - counts slots j != self_idx where `pool_budget::slot_owner(j).map(|(_, e)| e) == Some(endpoint)` and `0 <= now - stamps[j] <= SIBLING_LIVE_EVIDENCE_SECS*1000`;
  - a stamp in the future by more than 5 s does NOT count (a clock step fails toward the fallback);
  - O(32). The production caller passes a snapshot read from the static.

4. PURE DECISION
`pub const fn silence_redial_basis(endpoint, silent_secs, confirmed_threshold_secs, confirmed_window_open, overflow_or_halted, live_siblings, held, spacing_ok) -> Option<SilenceBasis>`:
- Depth200: `Backstop` if `silent >= 900`, else None (cross-feed arrives through its own event, §6).
- MainFeed / Depth20:
  - `Unconfirmed` if `silent >= 300`;
  - else `SiblingConfirmed` if all of: `silent >= confirmed_threshold_secs`, `confirmed_window_open`, `!overflow_or_halted`, `live_siblings >= MIN_LIVE_SIBLINGS`, `held >= min_held(endpoint)`, `spacing_ok`;
  - else None.
- OrderUpdate: None.

5. ESCALATION AND POLL CHANGE
- Supervisor fields: `silence_strikes: u8`, `last_silence_redial_at: Option<Instant>`, `last_silence_basis: SilenceBasis`, `would_redial_latched: bool`.
- `fn confirmed_threshold_secs(&self) -> u64 = min(base(endpoint) << silence_strikes.min(3), FRAME_SILENCE_REDIAL_SECS)`. Main: 60 → 120 → 240 → 300.
- At the top of the Live branch in poll: if `strikes > 0` and `now - last_silence_redial_at >= SILENCE_STRIKE_RESET_SECS`, set `strikes = 0`.
- Poll, Live branch:
  - keep the hold-at-now when the gate is closed;
  - else compute `silent`;
  - only if `evidence == Pool && fast_path != Off && silent >= confirmed_threshold_secs()` (rare): read wall clock once, read `OVERFLOW_ENGAGED` (new `pub fn overflow_episode_engaged() -> bool`, one Acquire load) and `rotation_halted()`, snapshot stamps, compute `live_siblings`. `spacing_ok` = a non-mutating read of `LAST_CONFIRMED_REDIAL_WALL_MS[kind]`;
  - call `silence_redial_basis`.
- Outcomes:
  - `Some(Unconfirmed | Backstop)`: fire as today.
  - `Some(SiblingConfirmed)` with `fast_path == Act`: CAS `LAST_CONFIRMED_REDIAL_WALL_MS[kind]` (`static [AtomicI64; 2]`, main/depth20) from the read value to now. If the CAS loses, Continue (another socket just took the slot). If it wins, fire.
  - `Some(SiblingConfirmed)` with `Shadow`: if `!would_redial_latched`, increment `FRAME_SILENCE_WOULD_REDIAL_METRIC{endpoint, basis}`, log one `info!` (`source = "frame_silence_shadow"`, `silent_secs`, `live_siblings`), set the latch. The `FrameReceived` arm clears it. Continue.
- Firing: set `last_silence_basis`, `strikes = strikes.saturating_add(1)`, `last_silence_redial_at = Some(now)`, then `return self.on_event(ConnEvent::FrameSilenceElapsed, now)`.
- `FrameSilenceElapsed` arm: unchanged gate and `warn!`, plus fields `basis = self.last_silence_basis.as_str()`, `threshold_secs`, `strikes`; increment `FRAME_SILENCE_REDIAL_METRIC{endpoint, basis}`; `schedule_redial(ReconnectReason::IdleSilence, now)`. The `warn!` already carries `code = ErrorCode::WsGapConnectionState.code_str()`.
- Seed both metrics at 0 at boot for every endpoint × basis combination (the existing local pre-registration path, NOT any alarmed list).

6. DEPTH-200 CROSS-FEED
core pool_supervisor.rs:
- `static DEPTH200_HELD: [AtomicU64; GHOST_REDIAL_SLOTS]` (`sid << 8 | segment code`, 0 = none) and `static DEPTH200_HELD_SINCE_MS: [AtomicI64; ..]`.
- `pub fn publish_depth200_held(idx, Option<SubscribeInstrument>, now_wall_ms)`: writes `since`, then `id` with Release; None clears both.
- Call it after EVERY place a Depth200 guard changes: initial subscribe, reconnect replay, `try_swap` success, the emptied path (unsubscribe ok, subscribe failed → None), `undo_swap` (revert to old), and `apply_resubscribe` (cap 1).
- `pub fn depth200_held(idx) -> Option<(u64, ExchangeSegment, i64)>`: reads id, since, id; None if the id changed. Doc: the re-read guards against a mismatched id only; a stale `since` with the correct id only pushes the baseline later, which is safe.
- `static DATA_SILENCE_PENDING_GEN: [AtomicU64; ..]` (0 = none, else `dial_generation + 1`) and `static DATA_SILENCE_LAST_REQUEST_MS: [AtomicI64; ..]`.
- `pub const DATA_SILENCE_REQUEST_COOLDOWN_SECS: i64 = 300;`
- `pub fn request_data_silence_redial(idx, now_ms) -> Result<(), DataSilenceRefusal { OutOfRange, CoolingDown, StillPending, Halted }>`: refuses when `rotation_halted()`; stores `dial_generation(idx) + 1`.
- `pub fn take_data_silence_redial(idx) -> bool`: swap to 0; true only if the stored value equals `dial_generation(idx) + 1` at take time. A request from an earlier connection is dropped and counted as `stale`.
- Connection task 1 s idle arm, after the probe-close block:
  `if action == Continue && take_data_silence_redial(idx) { if rotation_halted() { refuse_voluntary_redial_after_805(slot, "data_silence") } else { supervisor.note_silence_basis(CrossFeed); action = supervisor.on_event(FrameSilenceElapsed, Instant::now()) } }`
  Extend the `DIAL_REFUSED_AFTER_805_METRIC` doc label list with `data_silence`.

core `crates/core/src/pipeline/tick_gap_detector.rs`:
- `InstrumentState.last_trade_millis: u64` (0 = never), set to `obs.recv_monotonic_millis` only when `obs.volume > last_volume`. One store, zero allocation.
- `pub fn last_trade_millis(&self, key: InstrumentKey) -> Option<u64>`: one probe on the composite key.

app `crates/app/src/dhan_feed_stack.rs`:
- `pub fn check_depth200_cross_feed(&self, now_millis: u64, fast_path)` on `LiveIngest`, called from the existing 30 s `scan_silence` timer arm in `run_frame_drain`.
- Constants `DEPTH200_CROSS_FEED_SILENCE_MS: u64 = 90_000` and `DEPTH200_CROSS_FEED_TRADE_LEAD_MS: u64 = 30_000`.
- Returns at once unless all of: inside `ContinuousSessionIst.is_open_at`, `!overflow_episode_engaged()`, `!rotation_halted()`, `fast_path != Off`.
- For each idx with `endpoint_for_slot(idx) == Some(Depth200)` (at most 12):
  - `(sid, seg, since) = depth200_held(idx)?`;
  - `baseline = max(last_frame_wall_ms(idx).unwrap_or(0), since)` (read-task stamp, immune to shed and ring refusal; NOT `PER_CONN_LAST_TICK_MILLIS`);
  - if `now - baseline >= 90_000` and `last_trade_millis((sid, seg)) >= baseline + 30_000`:
    - Act: `request_data_silence_redial`, plus `warn!` with `code = ErrorCode::WsGapConnectionState.code_str()`, `source = "depth200_cross_feed"`, the contract label and the refusal outcome;
    - Shadow: count `FRAME_SILENCE_WOULD_REDIAL_METRIC{endpoint="depth_200", basis="cross_feed"}` once per baseline.
- An untracked contract gives no evidence; only the 900 s backstop applies.

7. CONFIG
- One field `frame_silence_fast_path: FrameSilenceFastPath`, `#[serde(default)]` = Shadow, on the config struct that owns the live Dhan lane settings (the one read where `set_frame_silence_gate(ContinuousSessionIst)` is applied). Add an explicit `frame_silence_fast_path = "shadow"` line to `config/base.toml`.
- App spawn loop for main / depth-20 / depth-200: `set_silence_evidence(SiblingEvidence::Pool)` and `set_fast_path(cfg)`. The connection task calls `set_held_instruments(guard.len())` after subscribe, replay, extend, resubscribe and swap.
- Flip to "act" only by a later one-line PR, after the shadow week shows:
  - p99.9 of `FRAME_GAP_MAX` < 20 s on main and < 30 s on depth-20 inside 09:15–15:15;
  - fewer than 1 would-redial per socket per day without a matching real fault.
- Rollback: set "off" (exactly today's behaviour for main/depth-20), or revert. The depth-200 900 s backstop is in code; reverting the PR restores 300.

8. ALARM: no change to `deploy/aws/terraform/live-lane-alarms.tf` thresholds or descriptions. Optionally fix only the `#` comment "15:30 close" → "15:40". No new EMF metric; `cloudwatch_app_alarms_wiring.rs` must stay green unchanged.

9. CLAUDE.md O(1) TABLE: add rows for:
- `pool_supervisor` silence evidence (per frame: one compare plus a store at most 1/s/socket; O(32) scan only when already silent past the confirmed threshold);
- `TickGapDetector::last_trade_millis` (one store per volume-raising tick);
- `check_depth200_cross_feed` (O(12) probes per 30 s, cold);
- the register statics.

## Tests
["core pool_supervisor: silence_redial_basis_truth_table — exhaustive over endpoint x silent in {thr-1, thr, 299, 300, 899, 900} x window x overflow_or_halted x siblings {0,1,2} x held {below, at floor} x spacing_ok. Depth200 gives only Backstop at 900; OrderUpdate always None; 300 gives Unconfirmed whatever the evidence; 1 sibling never confirms.","core: live_siblings_counts_same_kind_other_slots_within_10s_only — own slot excluded, cross-kind excluded (uses pool_budget::slot_owner), 11 s stale excluded, a future stamp more than 5 s ahead excluded. Pure over an injected stamp array, no globals.","core: main_feed_confirmed_redial_at_60s_with_two_live_siblings_in_act_mode — the supervisor uses an injected evidence snapshot; Continue at 59 s, SleepThenDial at 60 s, last_redial_reason IdleSilence, basis SiblingConfirmed, strikes 1.","core: shadow_mode_counts_once_per_episode_and_never_redials — counter +1 at 60 s, Continue through 299 s, Unconfirmed redial at 300 s; the latch clears on FrameReceived.","core: escalation_bounds_snapshot_then_deaf_loop — each cycle: dial, subscribe ack, ONE frame, then silence, with siblings live and Act. Assert thresholds 60/120/240/300/300, and at most 15 silence redials in the first simulated hour (today: about 12). Strikes reset only after 900 s with no silence redial.","core: confirmed_redial_spacing_is_pool_wide_per_kind — two main sockets both eligible within 15 s: the second's CAS loses and it waits; depth-20 is unaffected by a main-feed redial.","core: below_the_instrument_floor_falls_back_to_300s (held 999 main, 19 depth-20).","core: an_engaged_overflow_episode_or_a_halted_rotation_suppresses_every_fast_path (falls back to 300 s; ROTATION_HALTED never written).","core: confirmed_window_open_at_boundaries — 09:14:59 false, 09:15:00 true, 15:14:59 true, 15:15:00 false; AlwaysOn true; Off false; is_open and overflow_probe_window_open unchanged.","core: rewrite a_ponging_socket_that_delivers_no_frame_is_redialled_after_the_frame_silence_window to MainFeed with SiblingEvidence::Off (300 s); new depth200_is_not_redialled_by_time_before_the_900s_backstop (Continue at 300/899, SleepThenDial at 900).","core: existing 48 MainFeed / 6 Depth20 supervisor tests unchanged and green: the evidence default is Off, so no global sibling leakage.","core: data_silence_request_is_dropped_when_the_dial_generation_moved — request, then BeginDial plus DialSucceeded: take returns false and is counted stale; a same-generation take returns true once; cooldown 300 s; out of range refused; Halted refused when rotation_halted().","core: a_taken_data_silence_request_after_805_is_refused_with_path_data_silence (mirrors an_805_refuses_a_queued_swap_and_a_pending_ghost_at_the_connection; source-scan that the idle arm checks rotation_halted before on_event).","core: depth200_held_round_trips_and_never_returns_a_torn_id — two segments with the same sid stay distinct (I-P1-11); None clears.","core source-scan: publish_depth200_held is called on the subscribe, replay, try_swap-success, emptied, undo_swap and apply_resubscribe paths.","core: frame_activity_published_at_most_once_per_second (1,000 FrameReceived in 1 s give one store; advances after the interval).","core proptest: random inter-frame gaps below the confirmed threshold with siblings live never produce a redial action, in any fast-path mode.","core const-asserts compile (per-kind ordering, flap ceiling, strike reset > fallback).","core tick_gap_detector: last_trade_millis_moves_only_on_a_volume_increase — not on quote-only, equal or decreasing volume, or a counter restart; None when untracked; composite key separates segments.","app: depth200_cross_feed_requests_when_trade_leads_by_30s_after_90s_silence; no_request_when_the_trade_precedes_the_lead; no_request_under_90s; no_request_for_an_untracked_contract; swap_resets_baseline_via_held_since; no_request_outside_the_session_gate, during an overflow episode, after rotation_halted, or in Off; shadow_counts_without_requesting; only_depth200_slots_checked.","app regression: a_depth_shed_never_triggers_a_cross_feed_redial — read-task stamps keep advancing while PER_CONN_LAST_TICK_MILLIS is frozen; no request.","app: spawn loop sets SiblingEvidence::Pool and the configured fast path on main, depth-20 and depth-200 (source-scan beside set_frame_silence_gate).","common/app config: frame_silence_fast_path defaults to Shadow when absent; base.toml carries \"shadow\"; rollback test that Off reproduces today's 300 s timing for main/depth-20.","dhat (crates/core/tests/dhat_frame_silence_activity.rs): 10,000 FrameReceived plus 1,000 polls, with the publish register and gap tracking, allocate 0 blocks.","guard: no_deliberate_redial_remains_for_a_subscription_change still green (ReconnectReason::ALL unchanged); FRAME_SILENCE_REDIAL_METRIC, FRAME_SILENCE_WOULD_REDIAL_METRIC and FRAME_GAP_MAX_SECS_GAUGE absent from deploy/; cloudwatch_app_alarms_wiring unchanged."]

## Guards to update
["crates/core/src/websocket/pool_supervisor.rs test a_ponging_socket_that_delivers_no_frame_is_redialled_after_the_frame_silence_window (Depth200@300 s becomes MainFeed@300 s, plus a new Depth200 900 s backstop test)","pool_supervisor.rs tests the_frame_watchdog_does_not_fire_before_the_socket_is_live and the_frame_silence_gate_defaults_to_always_on_and_opens_only_in_session_when_asked (extend with confirmed_window_open_at)","pool_supervisor.rs FRAME_SILENCE_REDIAL_SECS doc and const-assert block","pool_supervisor.rs DIAL_REFUSED_AFTER_805_METRIC doc label list (+ data_silence) and test_refuse_voluntary_redial_after_805_does_not_panic_and_names_the_path (add the path)","pool_supervisor.rs no_deliberate_redial_remains_for_a_subscription_change (must stay green, no new ReconnectReason)","crates/common/tests/cloudwatch_app_alarms_wiring.rs (unchanged; asserts no new EMF/alarm)","local metric pre-registration path (seed the new counters at 0; NOT alarmed_counters_are_seeded_guard alarmed lists)","pub-fn-test-guard / pub-fn-wiring-guard for live_siblings, silence_redial_basis, last_frame_wall_ms, overflow_episode_engaged, publish_depth200_held, depth200_held, request_data_silence_redial, take_data_silence_redial, set_silence_evidence, set_fast_path, set_held_instruments, TickGapDetector::last_trade_millis, LiveIngest::check_depth200_cross_feed","crates/core/tests dhat: new dhat_frame_silence_activity.rs","CLAUDE.md O(1) exception table rows (pool_supervisor silence evidence, tick_gap_detector last_trade_millis, check_depth200_cross_feed)","docs/claude-rules-full/project/websocket-connection-scope-lock.md 2026-10-02 inventory row (idle and silence watchdogs)","config/base.toml (frame_silence_fast_path = \"shadow\")"]

## Residual risks
["Assumed: normal per-socket inter-frame gaps on main feed and depth-20 in 09:15–15:15 are far below 60/90 s. No per-socket measurement exists. Shadow mode plus FRAME_GAP_MAX_SECS_GAUGE measures this before anything acts on it.","Assumed: Dhan emits a depth-200 frame on the book change a trade causes. If it conflates or throttles, the cross-feed path could redial a healthy socket; this is bounded by 90 s silence, a 30 s lead, a 300 s per-slot cooldown and shadow mode first.","Assumed: the read task has the frame's receipt wall nanos at FrameReceived dispatch (the WAL is written on the read task). If not, the builder adds one coarse wall read inside the 1 s-gated branch.","Every redial is a genuine reconnect with no snapshot-on-subscribe, so prints inside the roughly 2 s blind window are lost upstream (scope lock 2026-10-01). A false confirmed redial costs that on up to 5,000 instruments.","Every dial into a full account risks an 805; this is mitigated (fast paths off during an overflow episode and after any 805, per-kind spacing, escalation), not eliminated.","Detection gets worse for one case: a deaf depth-200 socket whose contract the main feed does not track (seeded boot contract, detector cap) now waits 900 s instead of 300 s, and depth-200 is not on the deaf gauge, so nothing pages.","Pools with 2 or fewer same-kind sockets (fallback 4-index universe, depth-20 count 1–2) never get sibling confirmation and stay at 300 s; a same-segment vendor stall that leaves 2 or more siblings live can still confirm.","15:15–15:40 stays on the 300 s fallback; behaviour of stock-only sockets during the CAS auction is unmeasured.","During ShedLevel::AllDepth, depth-20 PER_CONN stamps freeze, so dhan-worst-socket-deaf can page on healthy depth-20 sockets. This is pre-existing and recorded in the noise-lock row, not fixed.","A market-wide halt still redials every socket about every 5 minutes through the unchanged fallback (pre-existing).","The supervisor counts any binary frame as delivery while the gauge counts folded data only; a socket returning only unparseable frames is still never redialled (pre-existing).","Adding a config field to common triggers workspace-wide tests; the exact owning struct name is Assumed and must be found by grepping for where FrameSilenceGate::ContinuousSessionIst is applied."]

