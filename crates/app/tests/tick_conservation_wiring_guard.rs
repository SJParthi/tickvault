//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the daily
//! 15:40 IST tick-conservation audit is wired PROCESS-GLOBALLY for BOTH feeds.
//!
//! Background (2026-07-02 adversarial sweep): the conservation task originally
//! lived inside `spawn_post_market_tasks`, whose two call sites are BOTH
//! Dhan-gated — so a Groww-only session (`dhan_enabled=false`) ran ZERO
//! conservation audits all day (silent audit-coverage hole), and every runtime
//! Dhan disable→enable cycle spawned a DUPLICATE post-market task family. The
//! fix hoisted the conservation spawn into `main()`'s process-global prefix
//! (`spawn_daily_tick_conservation_task`, feed-gated per lane at 15:40) and
//! once-guarded `spawn_post_market_tasks`. This guard fails the build if any
//! of that topology regresses.
//!
//! Mirrors the codebase's `*_is_wired` guard pattern
//! (`orphan_position_watchdog_wiring_guard.rs`,
//! `daily_universe_boot_wiring_guard.rs`). Reads `main.rs` SOURCE text, so it
//! runs on the default build independent of any feature flag.

use std::fs;
use std::path::PathBuf;

fn main_rs_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_dhan_tick_conservation_audit_is_wired_into_main() {
    let src = main_rs_source();
    assert!(
        src.contains("run_tick_conservation_audit("),
        "main.rs must call run_tick_conservation_audit(...) at 15:40 IST — \
         otherwise the daily Dhan WAL-vs-DB zero-tick-loss audit never fires."
    );
}

#[test]
fn test_groww_tick_conservation_audit_is_wired_into_main() {
    let src = main_rs_source();
    assert!(
        src.contains("run_groww_tick_conservation_audit("),
        "main.rs must call run_groww_tick_conservation_audit(...) at 15:40 IST — \
         otherwise the Groww feed writes ticks but no daily conservation row."
    );
}

#[test]
fn test_conservation_spawn_lives_in_common_path_not_post_market() {
    // The conservation task MUST be process-global: spawned from main()'s
    // common prefix via spawn_daily_tick_conservation_task, NOT from inside
    // the Dhan-gated spawn_post_market_tasks (the 2026-07-02 CRITICAL: a
    // Groww-only boot never reached it there).
    let src = main_rs_source();
    let spawn_def_idx = src
        .find("fn spawn_daily_tick_conservation_task(")
        .expect("spawn_daily_tick_conservation_task must exist in main.rs");
    let call_count = src.matches("spawn_daily_tick_conservation_task(").count();
    assert!(
        call_count >= 2,
        "spawn_daily_tick_conservation_task must be defined once and CALLED at \
         least once from main()'s process-global prefix (>= 2 textual mentions); \
         found {call_count}."
    );
    // No conservation run may appear inside spawn_post_market_tasks: every
    // run_*_tick_conservation_audit mention must live inside the dedicated
    // spawn fn (which is defined AFTER spawn_post_market_tasks in the file) —
    // i.e. after the post-market fn's definition point AND after the dedicated
    // fn's definition point.
    let post_market_def_idx = src
        .find("fn spawn_post_market_tasks(")
        .expect("spawn_post_market_tasks must exist in main.rs");
    for pat in [
        "run_tick_conservation_audit(",
        "run_groww_tick_conservation_audit(",
    ] {
        let mut search_from = 0;
        while let Some(rel) = src[search_from..].find(pat) {
            let abs = search_from + rel;
            assert!(
                abs > spawn_def_idx && abs > post_market_def_idx,
                "conservation run `{pat}` found at byte {abs}, before the \
                 dedicated spawn fn (byte {spawn_def_idx}) — a conservation run \
                 must not be re-nested into a Dhan-gated path."
            );
            search_from = abs + pat.len();
        }
    }
}

#[test]
fn test_both_feed_gates_present_and_groww_after_dhan() {
    // Each lane's 15:40 run is gated on the truthful runtime feed flag so a
    // disabled lane writes no misleading zero-balanced row; the Groww run is
    // sequenced after the Dhan run in the same task (same IST day/window).
    let src = main_rs_source();
    assert!(
        src.contains("is_enabled(tickvault_common::feed::Feed::Dhan)"),
        "the Dhan conservation run must be gated on \
         feed_runtime.is_enabled(Feed::Dhan)."
    );
    assert!(
        src.contains("is_enabled(tickvault_common::feed::Feed::Groww)"),
        "the Groww conservation run must be gated on \
         feed_runtime.is_enabled(Feed::Groww)."
    );
    let dhan_idx = src
        .find("run_tick_conservation_audit(")
        .expect("Dhan conservation run must exist in main.rs");
    let groww_idx = src
        .find("run_groww_tick_conservation_audit(")
        .expect("Groww conservation run must exist in main.rs");
    assert!(
        groww_idx > dhan_idx,
        "the Groww conservation run must be sequenced AFTER the Dhan run inside \
         the same 15:40 IST task so both reconcile the same IST day."
    );
}

#[test]
fn test_post_market_tasks_has_process_global_once_guard() {
    // Runtime Dhan cold-start re-invokes spawn_post_market_tasks on every
    // disable→enable cycle; without the once-guard, N cycles = N duplicate
    // EOD digests / cross-verifies / orphan watchdogs (2026-07-02 HIGH).
    //
    // 2026-07-13 (Phase A FIX 4): the guard is now SHARED with the Dhan
    // REST-only stack — the static lives in the lib
    // (dhan_rest_stack::POST_MARKET_TASK_FAMILY_CLAIMED) so BOTH spawn
    // paths (the lane's spawn_post_market_tasks AND the stack's Phase 5
    // canary/spot/chain family) claim the SAME once-guard. This guard now
    // pins both halves of that invariant.
    let src = main_rs_source();
    assert!(
        src.contains("!tickvault_app::dhan_rest_stack::claim_post_market_task_family_once()"),
        "spawn_post_market_tasks must be protected by the SHARED \
         claim_post_market_task_family_once once-guard (lib static shared \
         with the Dhan REST-only stack)."
    );
    let stack_src = {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_rest_stack.rs");
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    };
    assert!(
        stack_src.contains("POST_MARKET_TASK_FAMILY_CLAIMED.swap(true"),
        "the shared once-guard must use an atomic swap(true) so exactly the \
         FIRST caller proceeds and later re-entries no-op."
    );
    assert!(
        stack_src.contains("claim_post_market_task_family_once()"),
        "the Dhan REST-only stack must claim the shared task-family \
         once-guard before spawning canary/spot/chain (FIX 4 invariant)."
    );
}
