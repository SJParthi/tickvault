# FINAL SPEC 805-probe (ship=true)

## Summary
**The defect is real, exactly as reported.** Checked in origin/main, crates/core/src/websocket/pool_supervisor.rs:
- The Disconnected arm (line 2903) calls overflow_episode_note_disconnect(code, now) and passes no slot.
- Once any 805 has happened (OVERFLOW_ENGAGED), every close with no code goes to OverflowEpisodes::on_bare_reset (lines 1823-1828). That feeds both episodes, and OverflowEpisode::on_bare_reset (lines 1603-1610) fails Probing/Resuming unconditionally with FailedBareReset.
- probes_started goes up at the grant (line 1655) and is never refunded. fail() (lines 1747-1765) moves to DownForSession once 3 main-feed probes (6 depth) are spent. After that, wait_for_overflow_grant returns None, so the parked sockets stay down for the session.
- The scope lock records this rule on purpose (full file, line 8544: "an 805 on ANY socket, or any socket closing with no code, inside a window"), and test_probe_fails_on_sibling_bare_reset_in_window (line 17496) pins it.
- Net effect: one unrelated blip burns one of only 3 probes.

**Fix.** A probe now fails only on one of these:
- an 805 anywhere;
- the probed socket closing with no code, or its own idle watchdog firing;
- no first frame in time;
- another socket's no-code close that is CORROBORATED as an eviction. That means either some other socket (not the closing one, not one that dropped in the same burst) BEGAN a dial in the last 20 s, or the dropped socket does not reconnect within 120 s.

So a lone sibling blip that reconnects cleanly no longer burns a probe. An eviction caused by the probe's own dial still fails at once, as today.

**Review points folded in:**
- Dials are attributed by time, using a per-slot BeginDial instant. This replaces the design's window-wide DIAL_GENERATION snapshot, which wrongly failed on two separate blips and on one flapping sibling. Reading BeginDial rather than DialSucceeded also closes the race where the evicted socket's close is processed before the evicting dial's bump.
- A sibling counts as healed at its own DialSucceeded, not at its first frame. A depth-200 contract can legitimately stay blank for minutes, and after 15:30 nothing ticks.
- Each suspect gets its own heal deadline.
- A suspect that parks for a reason other than 805 is dropped.
- The watched socket's IdleElapsed now fails the window.
- Non-failure outcomes log at warn!.
- The jitter figure is corrected to 625 ms (RECONNECT_DELAY_WITH_JITTER_MAX_MS).
- The thread race test is replaced by deterministic orderings.
- The false claim that DHAT already covers the supervisor is dropped (no dhat_*.rs references it) and replaced by a source pin.

**Review points dropped:**
- "release_next skips start_watch": wrong. Its grant path calls start_watch (line 1718). A debug_assert is added anyway.
- Adding a separate BeginDial attempt counter: superseded by the per-slot instants.

**Honest scope.** The fix helps when ONE socket (or one burst) drops while the probed socket stays up. A blip on the box's own network also drops the probed socket, and that still fails the probe, as the ask allows.

No page, alarm or Telegram change, so no noise-lock row. The scope lock needs a dated 2026-10-06 amendment, quoting the operator's 2026-10-06 approval.

All edits are in crates/core/src/websocket/pool_supervisor.rs unless another file is named. Line numbers are origin/main.

### 0. Rule files first (same PR, before the code)

- **(a) docs/claude-rules-full/project/websocket-connection-scope-lock.md.** Add a new section: `### 2026-10-06 — OVERFLOW PROBE ATTRIBUTION: a close with no code fails a probe only on the probed socket, or when another socket's drop is corroborated as an eviction`.
  - Quote the operator verbatim (2026-10-06): "Go ahead with whatever you want dude" and "See do everything whatever is recommended dude okay?".
  - It amends the 2026-10-02 depth table 'Failure' row (line 8544) AND states the main-feed rule (D7 / plan R3-7), so both are recorded in the rule file.
  - Keep, verbatim: one probe window process-wide; main feed first; ROTATION_HALTED is never cleared; the 3 / 6 probe caps with no refund; no new socket; caps 5,000 / 50 / 1.
  - Record these limits: a foreign process's dials cannot be seen; a blip that also drops the probed socket still fails.
  - REJECT list:
    - ignoring sibling drops with no corroboration check;
    - removing 805-anywhere;
    - passing while a suspect is unhealed or inside its settle time;
    - refunding a probe or raising a cap;
    - opening a socket to heal a sibling;
    - a second window while one is deferred;
    - counting a dial by the closing slot itself, or by a member of the same burst, as corroboration.
- **(b) .claude/rules/project/websocket-connection-scope-lock.md (summary stub).** Add one 2026-10-06 bullet.
- **(c) .claude/plans/active-plan-audit-fixes-2026-09-26.md.** Add item "805-probe attribution", with files, test names, the 6 design-first sections, and the 15-row and 7-row matrices. Five active plans exist, so a new plan file would trip the V7 cap; add the item to this existing plan.
- **(d) CLAUDE.md, the OVERFLOW_EPISODES row.** Add a 2026-10-06 note:
  - per-socket attribution;
  - the OVERFLOW_DIAL_BEGIN_MS table: one store per dial at BeginDial, plus an O(32) scan only on a no-code close while engaged;
  - one atomic load at DialSucceeded and at a non-805 park;
  - a pass is deferred by at most OVERFLOW_PROBE_SIBLING_HEAL_DEADLINE_SECS;
  - "O(1) per step" becomes "O(1), O(32) on a sibling close".
- **No edits to:**
  - dhan-rest-only-noise-lock (no CloudWatch, EMF or Telegram change; WS-GAP-01 has no paging filter);
  - live-lane-alarms.tf. Its line 160 description, which says only a restart restores a parked socket, is stale since D7; that is a separate follow-up.

### 1. Constants (beside OVERFLOW_PROBE_WATCH_SECS, ~line 1291)

- `pub const OVERFLOW_PROBE_EVICTION_ATTRIBUTION_SECS: u64 = 20;`
  - Covers BeginDial to accept, bounded by connection.rs DIAL_TIMEOUT (15 s), plus 5 s for the evicted socket's close to be processed.
  - `const _: () = assert!(OVERFLOW_PROBE_EVICTION_ATTRIBUTION_SECS > crate::websocket::connection::DIAL_TIMEOUT.as_secs());`
- `pub const OVERFLOW_PROBE_SIBLING_BURST_MS: u64 = 2_000;`
  - No-code closes within 2 s of a burst's first close are one event. Their own redials do not corroborate each other. (Assumed: covers one server-side multi-close.)
- `pub const OVERFLOW_PROBE_SIBLING_HEAL_DEADLINE_SECS: u64 = 120;`
  - `const _: () = assert!(OVERFLOW_PROBE_SIBLING_HEAL_DEADLINE_SECS * 1000 > 2 * (RECONNECT_DELAY_WITH_JITTER_MAX_MS + DIAL_TIMEOUT.as_secs() * 1000));`
  - That is 120,000 > 91,250, i.e. room for two full ladder-plus-dial attempts.

### 2. OverflowProbeOutcome (line 1403)

Add five outcomes:
- `SiblingResetNoted` ("sibling_reset_noted")
- `SiblingHealed` ("sibling_healed")
- `FailedEvictionCorroborated` ("failed_eviction_corroborated")
- `FailedSiblingUnhealed` ("failed_sibling_unhealed")
- `FailedWatchedSilent` ("failed_watched_silent")

Then:
- `ALL: [Self; 9]` becomes `[Self; 14]`.
- Add `const fn is_note(self) -> bool`, true only for SiblingResetNoted and SiblingHealed.

### 3. OverflowEpisodeEffect

- Add `sibling: Option<u8>`. It keeps `Default` and `Copy`.

### 4. OverflowEpisode new fields (still Copy, no heap)

- `suspect_mask: u32` — unhealed sibling slots.
- `suspect_since: [Option<Instant>; GHOST_REDIAL_SLOTS]` — each suspect's own close time.
- `burst_start: Option<Instant>`, `burst_mask: u32`.
- `settle_until: Option<Instant>` — when the last heal's dial can no longer evict anyone.

Initialise empty in of_kind. Add `fn clear_attribution(&mut self)`, which empties all five fields. Call it in start_watch, in fail(), and in the pass branch of poll. Add `debug_assert!(self.suspect_mask == 0)` in release_next.

### 5. Replace on_bare_reset(now) with `fn on_bare_reset(&mut self, slot: u8, recent_dials: u32, now: Instant) -> OverflowEpisodeEffect`

- Not Probing/Resuming: return default.
- `slot == watched_slot`: return `fail(FailedBareReset)`.
- `1u32.checked_shl(u32::from(slot))` is None: return `fail(FailedBareReset)`, failing closed.
- Burst: if `burst_start` is Some(b) and `now - b <= BURST`, join the burst; otherwise set `burst_start = Some(now)` and `burst_mask = 0`. Then `burst_mask |= bit`.
- `let evictors = recent_dials & !bit & !burst_mask;` If it is non-zero, return `fail(FailedEvictionCorroborated)` with `sibling: Some(slot)`.
  - The watched slot is never a burst member (its own close fails earlier), so a sibling dropped by the probe's own dial fails at once.
- Otherwise:
  - set `suspect_mask |= bit`;
  - if `suspect_since[slot]` is None, set it to Some(now);
  - return outcomes `[Some(SiblingResetNoted), None]` with `sibling: Some(slot)`. No grant, no repark.

### 6. `fn on_sibling_dialled(&mut self, slot: u8, now: Instant) -> OverflowEpisodeEffect`

- Only in a window and only when the bit is in suspect_mask:
  - clear the bit and `suspect_since[slot]`;
  - set `settle_until = max(settle_until, now + ATTRIBUTION)`;
  - return `[Some(SiblingHealed), None]` with `sibling: Some(slot)`.

`fn on_sibling_left(&mut self, slot: u8)` clears the bit and `suspect_since[slot]`, and sets no settle: a parked socket adds no connection.

### 7. `fn on_watched_self_redial(&mut self, slot: u8, now: Instant) -> OverflowEpisodeEffect`

- In a window, when `slot == watched_slot`: return `fail(FailedWatchedSilent)`. Otherwise return default.

### 8. poll, Probing|Resuming arm, before the existing first-frame and pass logic

- **Unhealed check.** For each set bit of suspect_mask (`trailing_zeros` loop, O(popcount)): if `now - suspect_since[s] >= HEAL_DEADLINE`, return `fail(FailedSiblingUnhealed)` with `sibling: Some(s)`.
- **Pass deferral.** Where the watch window has elapsed and the code would pass: if `suspect_mask != 0` or `settle_until > now`, return default (stay in the window).
- **Deferral cap.** If `now >= first_frame_at + WATCH + HEAL_DEADLINE` and the window is still unsettled, return `fail(FailedSiblingUnhealed)`. This bounds the deferral against a sibling that keeps flapping.

### 9. OverflowEpisodes

Change on_bare_reset to take `(slot, recent_dials, now)`, and add on_sibling_dialled, on_sibling_left and on_watched_self_redial. Each one feeds both episodes; only the episode with a window acts. poll, main_may_grant and depth_may_grant are unchanged.

### 10. New statics and helpers

- `static OVERFLOW_DIAL_BEGIN_MS: [AtomicU64; GHOST_REDIAL_SLOTS]`
  - 0 means never; otherwise ms since OVERFLOW_DIAL_EPOCH, plus 1.
- `static OVERFLOW_DIAL_EPOCH: std::sync::OnceLock<Instant>`
- `static OVERFLOW_SUSPECT_MASK: AtomicU32`
- `fn dial_begin_ms(epoch: Instant, now: Instant) -> u64`
- `fn record_dial_begin(table: &[AtomicU64; GHOST_REDIAL_SLOTS], slot: u8, ms: u64)` — Release store; out-of-range slots ignored.
- `fn recent_dial_mask(table: &[AtomicU64; GHOST_REDIAL_SLOTS], now_ms: u64, within_ms: u64) -> u32`
  - O(32) Acquire loads; bit set where `0 < v` and `now_ms + 1 - v <= within_ms`, with saturating math.
  - It takes `&` arrays so tests use local tables.

### 11. Publishing the suspect mask

In overflow_episode_step, inside the lock, store `eps.main.suspect_mask | eps.depth.suspect_mask` to OVERFLOW_SUSPECT_MASK with Release, beside OVERFLOW_WATCHED_SLOT.

### 12. Shell entry points

- **`overflow_episode_note_disconnect(code, global_index, now)`.** The 805 branch is unchanged. The bare branch becomes `overflow_episode_step(false, |eps| eps.on_bare_reset(global_index, recent_dial_mask(&OVERFLOW_DIAL_BEGIN_MS, dial_begin_ms(epoch, now), ATTRIBUTION_SECS * 1000), now))`. The mask is computed inside the closure, under the lock.
- **Call sites in ConnectionSupervisor::on_event:**
  - Disconnected (line 2903): pass `self.slot.global_index`.
  - BeginDial arm (line 2766): `overflow_note_dial_begin(self.slot.global_index, now)`. One store, always (not only when engaged), so a dial that began just before the 805 is still known.
  - DialSucceeded arm (line 2785): `overflow_episode_note_dial_succeeded(idx, now)`. One Acquire load of OVERFLOW_SUSPECT_MASK; only if this slot's bit is set does it step on_sibling_dialled. This is a cold arm, once per dial.
  - IdleElapsed arm (line 2996), after the eligibility check: `overflow_episode_note_self_redial(idx, now)`. It steps only if OVERFLOW_ENGAGED is set and OVERFLOW_WATCHED_SLOT == idx.
  - park() (line 3282), after the respawn early-return, when `reason != ParkReason::PoolOverflow` and the bit is set: step on_sibling_left.
- FrameSilenceElapsed is deliberately NOT wired: a quiet depth contract is legitimate. Record this as a limit.

### 13. Reporting

- `apply_overflow_effect` passes `effect.sibling` on.
- `report_overflow_probe_outcome(kind, outcome, slot, watched, sibling)` adds a field `sibling_connection = sibling.map_or(-1, i32::from)`.
- `outcome.is_note()` uses warn!; every other outcome keeps error!. Both keep `code = ErrorCode::WsGapDisconnectClassification.code_str()`.
- Rewrite both message texts and the doc comments on OVERFLOW_PROBE_WATCH_SECS, on_bare_reset and OverflowEpisodes. Old wording: "only if no socket is closed with 805 or with no code". New wording: "only if no 805 arrives anywhere, the probed socket stays up, and any other socket that drops reconnects without knocking another off".

### 14. Untouched

- ROTATION_HALTED is not read or written.
- Probe caps, delays, the first-frame deadline and the watch length are unchanged.
- No new dial or socket.
- The socket read loop, the ring and the drain are not touched.

## Tests
["**Rewrite** test_probe_fails_on_sibling_bare_reset_in_window as test_probe_survives_one_sibling_blip_that_heals. Probe slot 2. Slot 0 drops at granted+90 s with recent_dials=0: expect SiblingResetNoted, no repark, phase Probing. on_sibling_dialled(0) then gives SiblingHealed. Poll at first_frame+WATCH while inside the settle time: no pass. Poll at the settle end: [ProbePassed, Recovered].","test_probed_socket_bare_reset_still_fails: on_bare_reset(watched, 0, t) gives FailedBareReset with repark of the watched slot, in Probing AND in Resuming.","test_sibling_drop_right_after_probe_dial_fails_at_once: recent_dials includes only the watched slot's bit, sibling 0 closes, expect FailedEvictionCorroborated with repark of the watched slot. This keeps today's guard for the probe-caused eviction.","test_two_separate_blips_in_one_window_do_not_fail: slot 0 drops, heals 30 s later; slot 1 drops 60 s after that with recent_dials holding slot 0's bit but more than 20 s old (so 0). Both heal and the window passes. This covers the reviewer's case that the design wrongly failed.","test_flapping_sibling_is_not_cascade_evidence: slot 0 drops, redials (its own bit in recent_dials), drops again within 20 s. The closing slot's own dial is excluded, so it is not a fail. It becomes a suspect again.","test_cascade_after_burst_fails: slot 0 drops (burst A), slot 0's dial begins, slot 1 drops 3 s later (a new burst) with slot 0 in recent_dials: FailedEvictionCorroborated with sibling=Some(1).","test_simultaneous_burst_is_one_event: slots 0 and 1 drop 500 ms apart, slot 0's dial begun between them (bit 0 recent). Slot 0 is a burst member, so no fail. Both heal and the window passes.","test_evictee_close_processed_before_evictor_dial_completes: slot 3 has a BeginDial instant but no DialSucceeded yet, and sibling 1 closes: FailedEvictionCorroborated. This pins the race reviewer 1 found with DIAL_GENERATION.","test_unhealed_sibling_fails_at_its_own_deadline: suspect at t0, poll at t0+119 s gives nothing, t0+120 s gives FailedSiblingUnhealed with repark. On the third main probe the phase goes to DownForSession. Variant: a second suspect added at t0+100 s still gets its full 120 s if the first has healed (per-slot stamps).","test_pass_deferral_is_capped: a sibling flaps forever (drop and heal repeatedly, no recent foreign dials). At first_frame+WATCH+HEAL_DEADLINE: FailedSiblingUnhealed. holds_turn() stays true throughout and no depth grant happens while deferred.","test_suspect_that_parks_for_non_805_is_dropped: on_sibling_left(0) clears the bit and the window passes on time.","test_watched_idle_redial_fails_window: on_watched_self_redial(watched) gives FailedWatchedSilent; on_watched_self_redial(other) does nothing.","ORDERING test_blip_then_805_fails_once_as_805: sibling suspect, then on_overflow gives exactly [FailedOverflow, None], one repark, probes_started==1, attribution fields cleared. A late on_sibling_dialled does nothing. The next window starts with suspect_mask==0 and burst_start==None.","ORDERING test_805_then_blip_no_double_fail: after FailedOverflow the phase is Waiting, so on_bare_reset(sibling) and on_bare_reset(watched) both return default and probes_started is unchanged.","ORDERING test_blip_on_watched_and_805_single_failure: both orders give exactly one Failed* and one repark.","test_depth_window_attribution: depth twin of the heal, corroborated and unhealed cases; also updates the existing depth bare-reset block near line 18198 to the new signature.","test_depth_window_deferred_keeps_main_waiting: main is Waiting with parked sockets while a depth window is deferred by a suspect. main_may_grant stays false, so main-first ordering is never inverted and no two windows run at once.","test_out_of_range_slot_fails_closed: slot 40 gives FailedBareReset.","test_recent_dial_mask_on_local_table: never (0), inside the window, exactly at the window edge, past the window, saturating math at now_ms<v.","test_record_dial_begin_ignores_out_of_range_slot.","Extend test_never_more_than_one_probe_window_in_flight (line 18273) and add proptest_attribution_rules: random sequences of 805, bare resets on random slots with random recent_dials masks, sibling dialled, sibling left, watched self-redial and polls. Assert:\n- at most one window;\n- at most one grant per step;\n- caps respected;\n- a Failed* outcome only when its rule matches (805, watched close, corroborated mask excluding the closer and the burst, deadline, deferral cap, watched idle);\n- no ProbePassed or ReleasePassed while suspect_mask!=0 or now<settle_until.","Source pins:\n- the Disconnected arm contains overflow_episode_note_disconnect(code, self.slot.global_index, now);\n- the BeginDial arm calls overflow_note_dial_begin;\n- the DialSucceeded arm calls overflow_episode_note_dial_succeeded;\n- park() calls the non-805 hook after the respawn return;\n- OVERFLOW_SUSPECT_MASK is stored inside the overflow_episode_step lock block;\n- the OverflowEpisode/OverflowEpisodes region contains no Vec, Box, String, format! or to_string. This replaces the false claim that DHAT covers the supervisor.","test_overflow_probe_outcome_labels_unique (line ~17824): 14 unique labels; is_note() is true only for the two notes.","Self-redial pin: IdleElapsed and FrameSilenceElapsed never call on_bare_reset (siblings' own redials never mark a suspect), but the BeginDial they lead to is recorded.","Scope-lock phrase pin: the full file contains the 2026-10-06 heading and the phrase 'the probed socket closing with no code still fails a probe'. No existing pinned phrase is removed.","Run cargo test -p tickvault-core (lib and tests), then the banned-pattern, pub-fn-test, pub-fn-wiring, plan-verify and plan-gate hooks. Paste the counts."]

## Guards to update
["crates/core/src/websocket/pool_supervisor.rs tests: test_probe_fails_on_sibling_bare_reset_in_window (line 17496, rewritten), depth bare-reset block (~line 18198), test_never_more_than_one_probe_window_in_flight (line 18273, new signature), OverflowProbeOutcome::ALL label-uniqueness test (~line 17824), any assertion on outcome arrays at 17465 / 17496 / 18201 / 18289","Source pin at line ~17260 that finds 'fn park(&mut self, reason: ParkReason': keep the signature unchanged","Scope-lock phrase guards reading the full websocket-connection-scope-lock.md (tick_gap_reset_wiring_guard.rs, top_volume_stock_only_guard.rs, instance_type_lock_guard.rs): add only, remove nothing","crates/common/tests/claude_md_codebase_map_guard.rs when editing the CLAUDE.md OVERFLOW_EPISODES row","plan-gate.sh / per-item-guarantee-check.sh: the new item in active-plan-audit-fixes-2026-09-26.md needs the 6 sections and the 15-row and 7-row matrices; there are 5 active plans, so do not add a sixth plan file","pub-fn-wiring / pub-fn-test guards: the three new pub consts need a test reference"]

## Residual risks
["Assumed model: Dhan evicts the oldest socket for each extra connection, and only at accept time (within DIAL_TIMEOUT of BeginDial). If Dhan evicts with no code without any new connection, or more than 20 s after the dial, the first eviction is treated as a blip. That eviction is caught only if the cycle continues: the next close corroborated by the evicted socket's own redial, or the heal deadline.","A foreign process's dials (the usual cause of an 805) are invisible to the dial-begin table. A single healed eviction caused by a foreign dial now passes where today it fails, until a later 805 or a corroborated close. This must be stated in the rule-file amendment.","A blip that also drops the probed socket (the box's own network) still burns a probe under the probed-socket rule. The fix covers only drops that leave the probed socket up.","False fail: any socket (ours) beginning a dial within 20 s before an unrelated sibling blip, outside that blip's burst, fails the probe. This errs toward fail, never toward pass. BeginDial may also be recorded on a dial the permit gate then refuses (Assumed), which errs the same way.","A cascade faster than the 2 s burst is grouped as one event and is caught only after the burst, at the cost of one or two more sibling reconnects.","A sibling stuck failing dials for more than 120 s, or flapping past the deferral cap, fails the probe. This is deliberate and fail-closed; a refund would break the 3/6 caps.","A deferred pass delays later releases and the depth turn by up to HEAL_DEADLINE plus the settle time. It is bounded, and no second window opens.","FrameSilenceElapsed on the probed socket still does not fail a window (kept on purpose for quiet depth contracts). Only IdleElapsed does.","Memory ordering: the BeginDial store and the evicted socket's later close have no in-memory happens-before; they are ordered only by the network round trip and the connect syscall (Assumed safe). A missed read errs toward suspect, then the heal deadline.","live-lane-alarms.tf line 160 description stays stale (park restored only by restart); left for a separate change."]

