//! Order-runtime SPAWN-SITE ratchet (dry-run PR, 2026-07-14; H1 fix-round
//! rewrite same day).
//!
//! The dry-run order runtime owns an OMS + RiskEngine pair. The dhan-ON
//! trading_pipeline owns its OWN OMS + RiskEngine. A second runtime spawned
//! anywhere else (main.rs, the fast crash-recovery arm, the Groww bridge)
//! would be a DUAL-OMS split-brain: two books, two halts, divergent P&L
//! (design F8). This guard pins the single legal spawn site — the dhan-OFF
//! REST stack's Phase 5a — so the mutual exclusion stays structural:
//!   - rest_stack spawns ONLY from main.rs's dhan-OFF branch (raw-TOML
//!     double gate), where trading_pipeline can never run;
//!   - trading_pipeline runs ONLY on dhan-ON boots, where the rest stack
//!     never spawns.
//!
//! H1 (fix-round 2026-07-14): the original guard used a naive
//! `split_once("#[cfg(test)]")` production-region split — groww_bridge.rs's
//! FIRST `#[cfg(test)]` is a mid-impl attribute at ~15% of the file, and
//! main.rs carries production fns AFTER its test module
//! (`#[allow(items_after_test_module)]`), so ~85% of groww_bridge and ~18%
//! of main.rs prod code were UNSCANNED. This rewrite uses the canonical
//! `tickvault_common::source_scan::production_region` helper (built
//! 2026-07-08 for exactly this failure class), which blanks ONLY the
//! `#[cfg(test)] mod tests { … }` block and keeps everything else —
//! including post-test-module production code — in the scanned region.
//! `scanner_self_tests` below prove the coverage is real, not asserted.

#![cfg(test)]

use std::path::PathBuf;

use tickvault_common::source_scan::production_region;

fn src(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Production region via the canonical helper — loud on refactor (a file
/// losing its test module must update this guard, never hollow it).
fn prod_region(file: &str) -> String {
    let body = src(file);
    production_region(&body).unwrap_or_else(|| {
        panic!("{file}: production_region found no #[cfg(test)] mod tests block")
    })
}

#[test]
fn ratchet_order_runtime_spawned_only_from_rest_stack() {
    // Exactly ONE spawn call site, in dhan_rest_stack.rs's production region.
    let stack_prod = prod_region("dhan_rest_stack.rs");
    let stack_spawns = stack_prod
        .matches("crate::order_runtime::spawn_order_runtime(")
        .count();
    assert_eq!(
        stack_spawns, 1,
        "dhan_rest_stack.rs production region must contain EXACTLY ONE \
         spawn_order_runtime call (Phase 5a); found {stack_spawns}"
    );

    // ZERO spawns anywhere else that could run alongside trading_pipeline's
    // OMS (main.rs incl. the fast crash-recovery arm AND the post-test-module
    // trailing fns, the pipeline itself, the mark-source REST legs —
    // groww_bridge.rs retired with the Groww live feed, 2026-07-15/#1581).
    for file in [
        "main.rs",
        "trading_pipeline.rs",
        "groww_spot_1m_boot.rs",
        "groww_contract_1m_boot.rs",
    ] {
        let prod = prod_region(file);
        let spawns = prod.matches("spawn_order_runtime(").count();
        assert_eq!(
            spawns, 0,
            "{file} must NOT spawn the order runtime (found {spawns} call(s)) — \
             a second runtime is a dual-OMS split-brain (two paper books, two \
             halts, divergent P&L); the ONLY legal spawn site is \
             dhan_rest_stack.rs Phase 5a on the dhan-OFF arm"
        );
    }

    // The definition itself lives in order_runtime.rs (one supervisor entry).
    let runtime_prod = prod_region("order_runtime.rs");
    assert!(
        runtime_prod.contains("pub fn spawn_order_runtime("),
        "order_runtime.rs lost its spawn_order_runtime entry point"
    );

    // H3 residual pin (refuter round-2): the LOOP must actually call the
    // MTM halt evaluator — the e2e test drives the arm bodies directly, so
    // deleting the loop's two production calls (post-mark-batch + the
    // housekeeping backstop) would silently kill the halt in the spawned
    // task while every test stays green.
    let halt_calls = runtime_prod
        .matches("risk.evaluate_daily_loss_halt()")
        .count();
    assert!(
        halt_calls >= 2,
        "order_runtime.rs production region must call \
         risk.evaluate_daily_loss_halt() at least twice (post-mark-batch + \
         housekeeping backstop); found {halt_calls} — the MTM halt would be \
         dead in the spawned task"
    );
}

/// M2 (fix-round 2026-07-14; re-homed 2026-07-16 after the Groww live
/// bridge retired with #1581): the Groww per-minute REST legs' OWN-FIRE
/// persist-confirm taps are the ONLY thing feeding the runtime's marks —
/// deleting a `mark_forward(` call (or a `staged_marks.push`) would
/// silently kill paper fills + the mark-to-market halt in prod while every
/// unit test (which hand-feeds `mark_tx`) stays green. Pin EXACTLY ONE tap
/// per leg (1-tap-per-leg): one own-fire staging site + one post-flush-ACK
/// forward site per file's production region — a second staging site would
/// mean the backfill/sweep arms started producing marks (the stale-price
/// class the C11 replay-window gate existed for on the old bridge).
#[test]
fn ratchet_groww_rest_leg_mark_taps_pinned() {
    for file in ["groww_spot_1m_boot.rs", "groww_contract_1m_boot.rs"] {
        let prod = prod_region(file);
        let stages = prod.matches("staged_marks.push(").count();
        assert_eq!(
            stages, 1,
            "{file} production region must stage marks at EXACTLY ONE site \
             (the OWN-FIRE just-closed-minute close) — found {stages}; a \
             second site means backfill/sweep/warm-up started producing \
             marks (stale prices must never fill paper orders), zero means \
             the runtime receives no marks from this leg"
        );
        let forwards = prod.matches("forwarder.mark_forward(").count();
        assert_eq!(
            forwards, 1,
            "{file} production region must forward marks at EXACTLY ONE \
             site (after the persist flush ACK) — found {forwards}"
        );
        // The forward site stays gated on the flush-ACK success arm: the
        // staging vec is only drained where the fold-writer/commit handoff
        // confirms persistence. Pin the None-gate shape so a refactor can't
        // start forwarding unconditionally.
        assert!(
            prod.contains("if let Some(forwarder) = params.mark_forwarder.as_ref()"),
            "{file} lost the Option-gate on the mark forward — the tap must \
             be a no-op when [order_runtime] is disabled"
        );
    }
    // The sweep/backfill arms must stay mark-free: the spot leg's
    // post-session sweep fn exists in the scanned region (so the
    // 1-site count above genuinely covers it).
    let spot_prod = prod_region("groww_spot_1m_boot.rs");
    assert!(
        spot_prod.contains("async fn run_post_session_sweep"),
        "groww_spot_1m_boot.rs scanned region lost run_post_session_sweep — \
         the 1-tap pin no longer proves the sweep is mark-free"
    );
}

/// The runtime's dry-run posture is hardcoded — no config path can flip it
/// to live order placement (operator boundary: no live orders before the
/// pre-live follow-ups in the rule file land with a fresh dated quote).
/// `OrderManagementSystem::new` constructs `dry_run: true` and the ONLY
/// false-flip (`enable_live_mode`) is `#[cfg(test)] pub(crate)` — this
/// ratchet pins both sides so neither the runtime nor the engine can grow
/// a production live-mode path silently.
///
/// L3 (fix-round 2026-07-14): the engine scan is scoped to its PRODUCTION
/// region (a future test writing `dry_run: true` can no longer vacuate the
/// pin) and a `set_dry_run` setter is banned outright.
#[test]
fn ratchet_order_runtime_is_dry_run_hardcoded() {
    // Runtime side: no dry-run mutation of any shape in production code.
    let prod = prod_region("order_runtime.rs");
    for banned in [
        "enable_live_mode",
        "dry_run: false",
        "dry_run = false",
        "set_dry_run",
    ] {
        assert!(
            !prod.contains(banned),
            "order_runtime.rs production region contains `{banned}` — the \
             dry-run-only lock is broken (live orders need the pre-live \
             follow-ups + a fresh dated operator quote)"
        );
    }

    // Engine side: the constructor default stays dry-run TRUE, and the only
    // live-mode flip stays test-gated (cfg(test) + pub(crate)).
    let engine_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../trading/src/oms/engine.rs");
    let engine = std::fs::read_to_string(&engine_path)
        .unwrap_or_else(|e| panic!("read {} failed: {e}", engine_path.display()));
    let engine_prod =
        production_region(&engine).expect("engine.rs must keep its #[cfg(test)] mod tests block"); // APPROVED: test
    assert!(
        engine_prod.contains("dry_run: true"),
        "OrderManagementSystem::new lost its hardcoded `dry_run: true` default"
    );
    assert!(
        !engine_prod.contains("fn set_dry_run"),
        "engine.rs grew a `set_dry_run` setter — a production live-mode flip \
         is forbidden without a fresh dated operator quote"
    );
    assert!(
        engine.contains("#[cfg(test)]\n    pub(crate) fn enable_live_mode"),
        "enable_live_mode must stay #[cfg(test)] pub(crate) — a production \
         live-mode flip is forbidden without a fresh dated operator quote"
    );
}

/// H1 self-tests: prove the scanner's coverage is REAL — (a) a synthetic
/// source planting a spawn AFTER a mid-impl `#[cfg(test)]` attr (the naive
/// split_once blind spot) is caught; (b) the real scanned regions contain
/// known LATE-FILE production symbols (post-test-module in main.rs; deep in
/// groww_spot_1m_boot.rs past its first cfg(test) marker — the retired
/// groww_bridge.rs check re-anchored 2026-07-16).
#[test]
fn scanner_self_tests_prove_full_production_coverage() {
    // (a) Synthetic: the exact H1 evasion shape.
    let synthetic = "\
fn early_prod() {}
#[cfg(test)]
fn test_only_helper() {}
fn late_prod() { spawn_order_runtime(params); }
#[cfg(test)]
mod tests {
    #[test]
    fn t() { let _ = \"spawn_order_runtime( in a test literal\"; }
}
";
    let region = production_region(synthetic).expect("synthetic has a test module"); // APPROVED: test
    assert!(
        region.contains("spawn_order_runtime("),
        "a spawn planted AFTER a mid-impl #[cfg(test)] attr must be INSIDE \
         the scanned region (the naive split_once missed it — H1)"
    );
    assert!(
        !region.contains("in a test literal"),
        "test-module literals must be blanked (no self-satisfying needles)"
    );

    // (b) Real files: late-file production symbols must be in-region.
    // Sentinel updated at the PR-C2 merge reconcile (2026-07-15):
    // `spawn_post_market_tasks` was deleted with the Dhan live-WS lane
    // (#1522); `spawn_feed_scoreboard_tasks` is the surviving
    // post-test-module production fn that proves the same H1 coverage.
    let main_prod = prod_region("main.rs");
    assert!(
        main_prod.contains("fn spawn_feed_scoreboard_tasks"),
        "main.rs scanned region lost `fn spawn_feed_scoreboard_tasks` — the \
         post-test-module production code is no longer covered (H1 regression)"
    );
    // (groww_bridge.rs retired with the Groww live feed 2026-07-15/#1581;
    // groww_spot_1m_boot.rs has the same H1 shape — mid-impl #[cfg(test)]
    // attrs long before its late production sweep fn.)
    let spot = src("groww_spot_1m_boot.rs");
    let first_cfg_test = spot
        .find("#[cfg(test)]")
        .expect("groww_spot_1m_boot.rs has cfg(test) markers"); // APPROVED: test
    let spot_prod = prod_region("groww_spot_1m_boot.rs");
    assert!(
        spot_prod[first_cfg_test..].contains("async fn run_post_session_sweep"),
        "groww_spot_1m_boot.rs scanned region lost `run_post_session_sweep` \
         AFTER its first #[cfg(test)] marker — the guard is back to scanning \
         only the file prefix (H1 regression)"
    );
}
