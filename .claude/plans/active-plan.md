# Implementation Plan: Gate the boot "alive" signal on a live feed

**Status:** VERIFIED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — standing approval for the permutation-coverage
audit fix queue (`.claude/plans/permutation-coverage-audit-2026-07-01.md`), cross-cutting
"alerting hole" hardening surfaced by the recovery re-check, this session 2026-07-01.

Crate referenced: **tickvault-app** (change lives in `crates/app/src/main.rs` +
its `crates/app/tests/` guard). Terraform (`deploy/aws/terraform/*liveness*.tf`)
edited only if a residual market-hours-liveness gap is found — see Design §2.

> Guarantee matrices: carried by cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory per
> `per-item-guarantee-check.sh`). Dominant guarantee: **strictly-no-worse than
> today for the boot-heartbeat page** — the emit still fires on a healthy boot;
> it now ADDITIONALLY withholds "alive" when every enabled feed failed to come
> up, so the heartbeat alarm (`treat_missing_data="breaching"`) pages exactly in
> the previously-silent case. No hot-path change (boot is cold path). Does NOT
> touch `dry_run`, budget, instance lock, WAL spill, `MAX_DAILY_UNIVERSE_SIZE`,
> `SEAL_BUFFER_CAPACITY`, the 2-WebSocket lock, or feed enablement semantics.

## Design

### Ground truth established (STEP 0, verified against the live tree)

`emit_boot_completed()` (`crates/app/src/main.rs`) sets the `tv_boot_completed`
gauge to `1.0`. It is called at TWO boot-completion points, and the CURRENT emit
condition at each is:

- **Slow-boot / normal** (main.rs:7458): fires unconditionally after the
  boot-complete `if/else` + `notify_systemd_ready()`. For `dhan_enabled=true`,
  the Dhan lane's `start_dhan_lane` must have returned `Ok` earlier to REACH this
  line (a Dhan-start failure `return`s/exits before it) — so the Dhan-enabled
  slow path is *already* effectively gated on the Dhan lane succeeding. BUT the
  `dhan_enabled=false` / Groww-only branch takes the `None` arm and proceeds to
  emit even though Groww's activation is an ASYNC spawned watcher
  (`run_groww_activation_watcher`) that may never come up.
- **Fast-boot / crash-recovery** (main.rs:2191): fires UNCONDITIONALLY after
  `notify_systemd_ready()`. This arm is Dhan-ON-only (guarded by
  `config.feeds.dhan_enabled` at main.rs:1320), but the warm-snapshot re-injects
  the WS pool asynchronously — a crash-recovery where the pool never reconnects
  still emits "alive."

**Net current behaviour:** a boot where every enabled feed failed to reach a live
state can still publish `tv_boot_completed=1` → the boot-heartbeat alarm
(`treat_missing_data="breaching"`, boot-heartbeat-alarm.tf) does NOT page. **Gap
is real and OPEN** for the Groww-only and fast-boot-pool-fail permutations.

The async reality: both emit points are SYNCHRONOUS instants, but feed lanes come
up asynchronously (`mark_dhan_lane_running` inside the `dhan_enabled` block +
`start_dhan_lane` → Running; `mark_groww_lane_running` only after the async
watch-list build). A point-in-time `lane_running` snapshot at the emit instant
would FALSE-NEGATIVE a feed that is still legitimately coming up → a false page.

### The fix

1. **Pure guard helper** `boot_completed_should_emit(dhan_enabled, groww_enabled,
   dhan_running, groww_running) -> bool`:
   - No feed enabled (`!dhan_enabled && !groww_enabled`) → `true` (preserve
     today's behaviour; a deliberately feed-less run must not false-page). The
     caller logs this explicitly.
   - At least one feed enabled → `true` iff at least ONE ENABLED feed is running
     (`(dhan_enabled && dhan_running) || (groww_enabled && groww_running)`).
     Feed-agnostic: Groww-only-live satisfies it, Dhan-only-live satisfies it,
     both-enabled needs either.
   - Both-enabled-none-live / only-enabled-feed-not-live → `false` (withhold
     "alive" → heartbeat alarm pages).

2. **Bounded liveness wait at each emit site.** Before calling
   `emit_boot_completed()`, poll `feed_runtime.is_dhan_lane_running()` /
   `is_groww_lane_running()` against `boot_completed_should_emit(...)` for a
   bounded window (poll ~1s cadence, capped by a bounded boot-scale timeout). The
   moment the guard returns `true`, emit and stop waiting. If the window elapses
   with the guard still `false` (every enabled feed failed to come up), do NOT
   emit and log an `error!` naming the dead feed(s) — the heartbeat alarm then
   pages on the MISSING metric, exactly the intended signal. The no-feed-enabled
   case returns `true` on the first poll (instant emit, unchanged timing).

   Rationale for a bounded WAIT (not an instant snapshot): it removes the async
   false-negative while keeping the honest end-state — a feed that comes up
   within the window still yields a prompt "alive"; a feed that never comes up
   correctly withholds it.

3. **Source-scan wiring guard** in `crates/app/tests/boot_completed_metric_guard.rs`:
   assert `emit_boot_completed()` is gated by `boot_completed_should_emit(` at
   BOTH call sites (the emit is no longer a bare unconditional call), and that
   the helper is defined. Rule-13 "defined-and-wired" ratchet.

### §2 Market-hours liveness alarm

PR #1284 already shipped `deploy/aws/terraform/market-hours-liveness-alarm.tf`:
an ALWAYS-ON (window-gated 09:20–15:35 IST) alarm that pages when
`tv_realtime_guarantee_score` is MISSING for ~5 min — i.e. the app is
wedged/crash-looped/dead during market hours REGARDLESS of the narrow
08:50–09:10 boot-heartbeat window. It covers "no enabled feed is streaming during
market hours ⇒ SLO loop stops publishing ⇒ page": a dead feed makes the SLO
composite loop stop emitting the score. **No duplicate alarm is needed.** This
plan does NOT add a second market-hours alarm; §2 is a verify, not an extend.

## Edge Cases

- **No feed enabled (both off):** helper → `true`, emit fires instantly, logged
  explicitly (`info!`). Never a false page for a deliberately feed-less run.
- **Groww-only, Groww comes up in 2s:** bounded wait polls, sees
  `is_groww_lane_running()` flip true, emits. No false-negative.
- **Groww-only, Groww never comes up:** window elapses, guard stays false, NO
  emit, `error!` names Groww dead → heartbeat alarm pages.
- **Dhan-only slow boot, Dhan start fails:** process `return`s/exits before the
  emit (unchanged) → metric missing → pages. Guard is belt-and-suspenders here.
- **Both enabled, one live:** helper → `true` (either satisfies). Correct.
- **Both enabled, none live:** guard false → no emit → pages.
- **Fast-boot pool re-inject fails:** bounded wait sees `is_dhan_lane_running()`
  false for the window → no emit → pages (was silently "alive" before).
- **Fast-boot pool re-inject succeeds:** `is_dhan_lane_running()` already true
  (marked at main.rs:330 inside the `dhan_enabled` block) → instant emit.

## Failure Modes

- **Helper returns wrong verdict:** covered by the unit truth-table (both-off,
  dhan-only-live/dead, groww-only-live/dead, both-live variants,
  both-enabled-none-live, running-but-disabled-does-not-count).
- **Bounded wait hangs boot:** capped by a bounded timeout, polls a cheap Relaxed
  atomic; on timeout it proceeds (no emit + error log), so boot never blocks
  indefinitely on this check.
- **Guard regressed to a bare emit:** the source-scan wiring guard fails the
  build (both call sites must show `boot_completed_should_emit`).
- **False page on a healthy feed-less run:** prevented by the explicit
  no-feed-enabled → `true` arm + its dedicated unit test + `info!` log.

## Test Plan

Unit (in `crates/app/src/main.rs` `#[cfg(test)]`), naming
`test_boot_completed_should_emit_*`:
- both-off → true
- dhan-only enabled + dhan running → true
- dhan-only enabled + dhan NOT running → false
- groww-only enabled + groww running → true
- groww-only enabled + groww NOT running → false
- both enabled + only dhan running → true
- both enabled + only groww running → true
- both enabled + neither running → false
- dhan disabled + dhan_running true (running-but-disabled) + groww enabled+dead
  → false (a running-but-disabled feed does not count)

Source-scan guard (`crates/app/tests/boot_completed_metric_guard.rs`):
- `boot_completed_should_emit(` appears at BOTH emit sites and the helper is
  defined.
- existing 4 guard tests still pass (metric name, both-paths, CW scrape filter,
  alarm repoint) — the emit is still present on both paths, just gated.

Commands: `cargo check -p tickvault-app` then `cargo test -p tickvault-app --lib`
+ `cargo test -p tickvault-app --test boot_completed_metric_guard`. Paste the
`test result: ok.` line. Known-unrelated env-only failures (ip_verifier /
secret_manager "fails without real SSM") confirmed with empty `git diff` on those
files.

## Rollback

Single-file logic change in `crates/app/src/main.rs` + its guard test. Revert the
commit to restore the unconditional emit. No schema, no migration, no terraform
state change (the alarm is untouched — it already pages on MISSING; this PR only
makes MISSING happen in the dead-feed case). `git revert <sha>` is complete.

## Observability

- `tv_boot_completed` gauge — unchanged name/semantics; now published ONLY when a
  live feed exists (or no feed is enabled). MISSING ⇒ boot-heartbeat alarm pages
  (existing, `treat_missing_data="breaching"`).
- New `error!` at each emit site when the bounded wait elapses with no live feed:
  names the dead enabled feed(s) so the operator's Telegram/`errors.jsonl` shows
  WHY the box will page (no new ErrorCode — boot-path diagnostic routed through
  the existing 5-sink error pipeline; complements, not replaces, the
  MISSING-metric page).
- `info!` on the no-feed-enabled emit path (explicit, per task requirement).
- Market-hours liveness: covered by the existing PR #1284 alarm (§2 verify).

## Plan Items

- [x] Add pure `boot_completed_should_emit(dhan_enabled, groww_enabled,
      dhan_running, groww_running) -> bool` helper + unit truth-table tests.
  - Files: crates/app/src/main.rs
  - Tests: test_boot_completed_should_emit_both_off_emits,
    test_boot_completed_should_emit_dhan_only_live,
    test_boot_completed_should_emit_dhan_only_dead,
    test_boot_completed_should_emit_groww_only_live,
    test_boot_completed_should_emit_groww_only_dead,
    test_boot_completed_should_emit_both_enabled_only_dhan_live,
    test_boot_completed_should_emit_both_enabled_only_groww_live,
    test_boot_completed_should_emit_both_enabled_none_live,
    test_boot_completed_should_emit_running_but_disabled_feed_does_not_count

- [x] Gate BOTH `emit_boot_completed()` call sites behind a bounded liveness wait
      driven by the helper; error-log + withhold on timeout; info-log the
      no-feed-enabled path.
  - Files: crates/app/src/main.rs
  - Tests: (source-scan) boot_completed_emit_sites_are_gated_on_live_feed

- [x] Extend the source-scan wiring guard so a regression to a bare
      unconditional emit fails the build.
  - Files: crates/app/tests/boot_completed_metric_guard.rs
  - Tests: boot_completed_emit_sites_are_gated_on_live_feed

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Both feeds off | emit fires, info-logged, no page |
| 2 | Groww-only, comes up | emit fires promptly, no page |
| 3 | Groww-only, never comes up | no emit, error-logged, heartbeat pages |
| 4 | Both enabled, only Dhan live | emit fires, no page |
| 5 | Both enabled, neither live | no emit, error-logged, heartbeat pages |
| 6 | Fast-boot pool re-inject fails | no emit, heartbeat pages (was silent) |
| 7 | Healthy Dhan slow boot | emit fires (unchanged), no page |
