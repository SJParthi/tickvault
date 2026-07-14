//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the Phase 0
//! Item 20 orphan-position 15:25 IST watchdog is actually wired into boot.
//!
//! Background: the pure evaluator + clock helpers existed in
//! `tickvault_trading::orphan_position_watchdog` for months, and
//! `phase-0-item-20-error-codes.md` CLAIMED a guard pinned the spawn — but
//! no such guard (and no spawn) existed. The watchdog was dead code: the
//! daily open-position safety gate never fired. This guard closes that gap.
//!
//! 2026-07-14 re-home update: the ONLY spawn used to live inside the
//! Dhan-gated `spawn_post_market_tasks` family — dead on `dhan_enabled =
//! false` boots (the production default since the 2026-07-13 Dhan live-WS
//! retirement). The topology this guard now pins:
//!
//!  1. PRIMARY spawn in the PROCESS-GLOBAL prefix of `main()` (before the
//!     `fn spawn_post_market_tasks(` definition in source order), using
//!     `WatchdogAuth::GlobalAtFireTime` — dhan-off boots covered.
//!  2. The family-site spawn INSIDE `spawn_post_market_tasks` (fast
//!     crash-recovery coverage, `WatchdogAuth::Static`) — exactly 2 spawn
//!     occurrences total; a third/moved spawn trips the count.
//!  3. The boot module resolves the session at fire time — live lane-owned
//!     manager PREFERRED, global OnceLock as the fallback (review round 1,
//!     2026-07-14: the D2c staleness class) — and carries the no-manager
//!     degraded wording (stub-guard — the fallback cannot be hollowed out
//!     into a silent skip). Pinned via CODE-SHAPED needles (real call
//!     expressions), so a comment can never satisfy the assert.
//!
//! Mirrors the codebase's `*_is_wired` guard pattern
//! (`instrument_build_failed_wiring_guard.rs`, `daily_universe_boot_wiring_guard.rs`).
//! Reads SOURCE text, so it runs on the default build independent of any
//! feature flag.

use std::fs;
use std::path::PathBuf;

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

#[test]
fn test_orphan_position_watchdog_is_wired_into_rest_stack() {
    let src = stack_src();
    assert!(
        src.contains("spawn_supervised_orphan_position_watchdog("),
        "dhan_rest_stack.rs must spawn the orphan-position watchdog via \
         spawn_supervised_orphan_position_watchdog(...) — otherwise the daily \
         15:25 IST open-position safety gate never fires (the dead-code gap \
         this guard exists to close; re-homed here in PR-C2, 2026-07-13)."
    );
    // The dry_run wording flag must come from config, not a hardcoded bool.
    assert!(
        src.contains("config.strategy.dry_run"),
        "the watchdog spawn must pass config.strategy.dry_run (the Telegram \
         wording gate) — never a hardcoded literal."
    );
}

#[test]
fn test_orphan_position_watchdog_spawned_from_process_global_prefix() {
    // 2026-07-14 re-home (INVERTS the pre-re-home assertion): the FIRST
    // occurrence of the spawn must come BEFORE the `fn spawn_post_market_tasks(`
    // definition — i.e. the primary spawn lives in the process-global prefix
    // that runs on every slow boot regardless of `dhan_enabled`. Before this,
    // the only spawn was inside the Dhan-gated family, so dhan-off boots
    // (the production default since 2026-07-13) never ran the 15:25 check.
    let src = main_rs_source();
    let fn_idx = src
        .find("fn spawn_post_market_tasks(")
        .expect("spawn_post_market_tasks must exist in main.rs");
    let spawn_idx = src
        .find("spawn_supervised_orphan_position_watchdog(")
        .expect("watchdog spawn must exist in dhan_rest_stack.rs");
    assert!(
        spawn_idx < fn_idx,
        "the FIRST watchdog spawn must live in the process-global prefix \
         (BEFORE the `fn spawn_post_market_tasks(` definition in source order) \
         so dhan-off boots still run the daily 15:25 IST open-position check; \
         found the first spawn at byte {spawn_idx}, after the fn at {fn_idx}."
    );
}

#[test]
fn test_orphan_position_watchdog_exactly_two_spawn_sites() {
    // Exactly 2 spawn-call occurrences in main.rs: the process-global prefix
    // (GlobalAtFireTime) + the `spawn_post_market_tasks` family site (Static,
    // fast crash-recovery coverage). A third — or a moved — spawn trips this
    // count and forces a conscious topology decision (e.g. the C2/integration
    // branch re-homes the watchdog into dhan_rest_stack; a merge of both
    // approaches must resolve to ONE topology, never a silent double page).
    let src = main_rs_source();
    let count = src
        .matches("spawn_supervised_orphan_position_watchdog(")
        .count();
    assert_eq!(
        count, 2,
        "main.rs must contain EXACTLY 2 orphan-watchdog spawn calls \
         (process-global prefix + the spawn_post_market_tasks family site); \
         found {count}. The module's once-guard makes the runtime safe either \
         way, but the topology must stay a deliberate decision."
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
}

#[test]
fn test_post_market_tasks_called_from_both_boot_paths() {
    // Boot-symmetry: spawn_post_market_tasks must be invoked at >= 2 call
    // sites (fast-boot + standard-boot). Definition + calls = >= 3 mentions.
    let src = main_rs_source();
    let mentions = src.matches("spawn_post_market_tasks(").count();
    assert!(
        mentions >= 3,
        "spawn_post_market_tasks must be defined once and called from both \
         boot paths (>= 3 textual mentions); found {mentions}."
    );
}
