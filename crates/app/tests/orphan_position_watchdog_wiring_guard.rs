//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the Phase 0
//! Item 20 orphan-position 15:25 IST watchdog is actually wired into boot.
//!
//! Background: the pure evaluator + clock helpers existed in
//! `tickvault_trading::orphan_position_watchdog` for months, and
//! `phase-0-item-20-error-codes.md` CLAIMED a guard pinned the spawn — but
//! no such guard (and no spawn) existed. The watchdog was dead code: the
//! daily open-position safety gate never fired. This guard closes that gap.
//!
//! Topology (PR-C2 × main #1559-era merge resolution, 2026-07-14): the
//! SINGLE spawn lives in the PROCESS-GLOBAL prefix of `main()` using
//! `WatchdogAuth::GlobalAtFireTime` — fire-time session resolution (live
//! lane-owned manager preferred, global OnceLock fallback; both registered
//! by the Dhan REST stack's token phase post-PR-C2). Rationale over the
//! briefly-considered Static-in-`dhan_rest_stack` re-home: a stack that
//! PARKS on AlreadyHeld (dual-instance-lock §3.5) before its token phase
//! would silently never spawn a Static watchdog, while the process-global
//! spawn always arms and degrades LOUDLY (Critical page) when no session
//! exists at 15:25 — never a silent skip (audit Rule 11). The old
//! `spawn_post_market_tasks` family site + the fast crash-recovery arm died
//! with the PR-C2 lane deletion, so the boot-symmetry pin is subsumed by
//! the single-boot-path spawn. The boot module resolves the session at
//! fire time (review round 1, 2026-07-14: the D2c staleness class) and
//! carries the no-manager degraded wording (stub-guard — the fallback
//! cannot be hollowed out into a silent skip). Pinned via CODE-SHAPED
//! needles (real call expressions), so a comment can never satisfy the
//! assert.
//!
//! Mirrors the codebase's `*_is_wired` guard pattern
//! (`instrument_build_failed_wiring_guard.rs`, `daily_universe_boot_wiring_guard.rs`).
//! Reads SOURCE text, so it runs on the default build independent of any
//! feature flag.

use std::fs;
use std::path::PathBuf;

fn main_rs_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn stack_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_rest_stack.rs");
    let full = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    // PRODUCTION region only — cut at the module's in-file test module so a
    // test literal can never satisfy the pins.
    match full.find("#[cfg(test)]") {
        Some(cut) => full[..cut].to_string(),
        None => full,
    }
}

fn watchdog_boot_source() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/orphan_position_watchdog_boot.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn watchdog_boot_source() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/orphan_position_watchdog_boot.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_orphan_position_watchdog_is_wired_into_main() {
    let src = main_rs_source();
    assert!(
        src.contains("spawn_supervised_orphan_position_watchdog("),
        "main.rs must spawn the orphan-position watchdog via \
         spawn_supervised_orphan_position_watchdog(...) — otherwise the daily \
         15:25 IST open-position safety gate never fires (the dead-code gap \
         this guard exists to close)."
    );
    // Fire-time session resolution — the merge-resolved topology. A Static
    // spawn here would freeze a boot-time handle; GlobalAtFireTime prefers
    // the live manager and falls back to the global OnceLock, so a runtime
    // token-manager replacement (or a late-minting REST stack) is picked up
    // at each 15:25 fire.
    assert!(
        src.contains("WatchdogAuth::GlobalAtFireTime {"),
        "the main.rs watchdog spawn must use WatchdogAuth::GlobalAtFireTime \
         (fire-time session resolution) — a boot-frozen Static handle would \
         pin a dead manager across the REST stack's async mint / a runtime \
         manager replacement."
    );
    // The dry_run wording flag must come from config, not a hardcoded bool.
    // Scope the pin to the spawn call's argument block so an unrelated
    // config.strategy.dry_run mention elsewhere can never satisfy it.
    let spawn_idx = src
        .find("spawn_supervised_orphan_position_watchdog(")
        .expect("watchdog spawn must exist in main.rs");
    let call_region = &src[spawn_idx..src.len().min(spawn_idx + 800)];
    assert!(
        call_region.contains("config.strategy.dry_run"),
        "the watchdog spawn must pass config.strategy.dry_run (the Telegram \
         wording gate) — never a hardcoded literal."
    );
}

#[test]
fn test_orphan_position_watchdog_exactly_one_spawn_site_in_main() {
    // Exactly 1 spawn-call occurrence in main.rs: the process-global prefix
    // (GlobalAtFireTime). The Dhan-gated `spawn_post_market_tasks` family
    // site + the fast crash-recovery arm died with the PR-C2 lane deletion,
    // so a second occurrence means a topology regression (the module's
    // once-guard makes the runtime safe either way, but the topology must
    // stay a deliberate decision — never a silent double-spawn race).
    let src = main_rs_source();
    let count = src
        .matches("spawn_supervised_orphan_position_watchdog(")
        .count();
    assert_eq!(
        count, 1,
        "main.rs must contain EXACTLY 1 orphan-watchdog spawn call \
         (the process-global prefix, GlobalAtFireTime); found {count}."
    );
}

#[test]
fn test_orphan_position_watchdog_not_spawned_from_rest_stack() {
    // The merge resolution deliberately REMOVED the Static-in-stack spawn:
    // a stack that PARKS on AlreadyHeld (dual-instance-lock §3.5) before
    // its token phase would silently never spawn the watchdog — the exact
    // silent-skip class the GlobalAtFireTime degraded page exists to
    // prevent. A spawn reappearing here is a topology regression.
    let src = stack_src();
    assert!(
        !src.contains("spawn_supervised_orphan_position_watchdog("),
        "dhan_rest_stack.rs must NOT spawn the orphan-position watchdog — \
         the single spawn lives in main.rs's process-global prefix \
         (GlobalAtFireTime); a parked stack would make a Static-in-stack \
         spawn a silent skip."
    );
}

#[test]
fn test_watchdog_boot_module_resolves_global_token_manager_at_fire_time() {
    // Stub-guard: the boot module must (a) resolve the session at fire time
    // for the GlobalAtFireTime arm — live lane-owned manager PREFERRED, the
    // global OnceLock as fallback (review round 1, 2026-07-14; the reads
    // deliberately live in the module, NOT main.rs — main.rs ratchets
    // exactly one `global_token_manager()` read in its production region),
    // and (b) carry the no-manager degraded arm's wording + failure counter
    // so the fallback cannot be hollowed out into a silent skip (audit Rule
    // 11 — never a clean signal on a failed check). The needles below are
    // CODE-SHAPED (real match-arm / call expressions from the resolver), so
    // a comment mentioning the fn names can never satisfy this assert.
    let src = watchdog_boot_source();
    let resolver_start = src
        .find("fn resolve_watchdog_session(")
        .expect("orphan_position_watchdog_boot.rs must define fn resolve_watchdog_session(");
    let resolver = &src[resolver_start..];
    assert!(
        resolver
            .contains("WatchdogAuth::GlobalAtFireTime { feed_runtime } => prefer_live_session("),
        "the GlobalAtFireTime match arm must route through prefer_live_session \
         (the live-first resolution — code-shaped needle)."
    );
    let live_pos = resolver
        .find("feed_runtime.live_token_manager(),")
        .expect("the resolver must read the live lane-owned manager (code-shaped needle)");
    let global_pos = resolver
        .find("global_token_manager().cloned(),")
        .expect("the resolver must keep the global OnceLock fallback (code-shaped needle)");
    assert!(
        live_pos < global_pos,
        "the resolver must PREFER the live lane-owned manager (its read must \
         come before the global fallback in the prefer_live_session args) — \
         otherwise a runtime lane stop→re-start pins the dead boot manager \
         (the D2c staleness class)."
    );
    assert!(
        src.contains("no broker session at 15:25 IST"),
        "the no-manager degraded arm's error! wording must survive — the \
         fallback must stay LOUD (degraded Critical), never a silent skip."
    );
    assert!(
        src.contains("could NOT run — no"),
        "the no-manager degraded Telegram wording must survive — the operator \
         must be told to check Dhan manually before the 15:30 close."
    );
    assert!(
        src.contains("tv_orphan_position_watchdog_fetch_failures_total"),
        "the degraded arms must keep incrementing the fetch-failures counter."
    );

    // Refuter round 1 (2026-07-14, LOW): the wording needles above pin the
    // pure message fns, NOT the Telegram DELIVERY — deleting the None arm's
    // `notifier.notify` (or cross-wiring it to degraded_no_token_message())
    // previously failed no test. Pin the DELIVERY with code-shaped needles
    // scoped to the None-arm REGION of the run loop: from the resolver
    // CALL SITE's `None => {` arm up to the non-trading-day else branch —
    // so neither of the other two degraded arms (the no-token arm inside
    // run_orphan_check_once, nor the fetch-failure arm) can satisfy it.
    let call_site = src
        .find("match resolve_watchdog_session(")
        .expect("the run loop must resolve the session via match resolve_watchdog_session(...)");
    let after_call = &src[call_site..];
    let none_arm_start = after_call
        .find("None => {")
        .expect("the resolver match must carry a None (no-session) degraded arm");
    let none_arm_end = after_call
        .find("non-trading day")
        .expect("the non-trading-day else branch must follow the resolver match");
    assert!(
        none_arm_start < none_arm_end,
        "the None arm must come before the non-trading-day else branch"
    );
    let none_arm = &after_call[none_arm_start..none_arm_end];
    assert!(
        none_arm.contains("notifier.notify(NotificationEvent::Custom {"),
        "the no-session None arm must DELIVER a Telegram event via \
         notifier.notify(NotificationEvent::Custom {{ .. }}) — the degraded \
         page must actually be sent, not just have its wording survive as a \
         dead pure fn (audit Rule 11)."
    );
    assert!(
        none_arm.contains("message: degraded_no_session_message(),"),
        "the no-session None arm's notify must pass degraded_no_session_message() \
         as the event message (code-shaped needle — deleting the notify or \
         swapping in a different degraded message fails this)."
    );
    assert!(
        !none_arm.contains("degraded_no_token_message"),
        "the no-session None arm must NOT be cross-wired to \
         degraded_no_token_message() — that wording tells the operator the \
         wrong failure class (a token problem instead of a missing session)."
    );
}
