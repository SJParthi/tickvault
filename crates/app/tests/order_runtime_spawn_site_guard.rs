//! Order-runtime SPAWN-SITE ratchet (dry-run PR, 2026-07-14).
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

#![cfg(test)]

use std::path::PathBuf;

fn src(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Production region of a source file (before its test-module marker, when
/// one exists) so assertion literals inside tests can never self-satisfy.
fn prod_region(body: &str) -> &str {
    body.split_once("#[cfg(test)]")
        .map_or(body, |(prod, _)| prod)
}

#[test]
fn ratchet_order_runtime_spawned_only_from_rest_stack() {
    // Exactly ONE spawn call site, in dhan_rest_stack.rs's production region.
    let stack = src("dhan_rest_stack.rs");
    let stack_prod = prod_region(&stack);
    let stack_spawns = stack_prod
        .matches("crate::order_runtime::spawn_order_runtime(")
        .count();
    assert_eq!(
        stack_spawns, 1,
        "dhan_rest_stack.rs production region must contain EXACTLY ONE \
         spawn_order_runtime call (Phase 5a); found {stack_spawns}"
    );

    // ZERO spawns anywhere else that could run alongside trading_pipeline's
    // OMS (main.rs incl. the fast crash-recovery arm, the pipeline itself,
    // the Groww bridge).
    for file in ["main.rs", "trading_pipeline.rs", "groww_bridge.rs"] {
        let body = src(file);
        let spawns = prod_region(&body).matches("spawn_order_runtime(").count();
        assert_eq!(
            spawns, 0,
            "{file} must NOT spawn the order runtime (found {spawns} call(s)) — \
             a second runtime is a dual-OMS split-brain (two paper books, two \
             halts, divergent P&L); the ONLY legal spawn site is \
             dhan_rest_stack.rs Phase 5a on the dhan-OFF arm"
        );
    }

    // The definition itself lives in order_runtime.rs (one supervisor entry).
    let runtime = src("order_runtime.rs");
    assert!(
        prod_region(&runtime).contains("pub fn spawn_order_runtime("),
        "order_runtime.rs lost its spawn_order_runtime entry point"
    );
}

/// The runtime's dry-run posture is hardcoded — no config path can flip it
/// to live order placement (operator boundary: no live orders before the
/// pre-live follow-ups in the rule file land with a fresh dated quote).
/// `OrderManagementSystem::new` constructs `dry_run: true` and the ONLY
/// false-flip (`enable_live_mode`) is `#[cfg(test)] pub(crate)` — this
/// ratchet pins both sides so neither the runtime nor the engine can grow
/// a production live-mode path silently.
#[test]
fn ratchet_order_runtime_is_dry_run_hardcoded() {
    // Runtime side: no dry-run mutation of any shape in production code.
    let runtime = src("order_runtime.rs");
    let prod = prod_region(&runtime);
    for banned in ["enable_live_mode", "dry_run: false", "dry_run = false"] {
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
    assert!(
        engine.contains("dry_run: true"),
        "OrderManagementSystem::new lost its hardcoded `dry_run: true` default"
    );
    assert!(
        engine.contains("#[cfg(test)]\n    pub(crate) fn enable_live_mode"),
        "enable_live_mode must stay #[cfg(test)] pub(crate) — a production \
         live-mode flip is forbidden without a fresh dated operator quote"
    );
}
