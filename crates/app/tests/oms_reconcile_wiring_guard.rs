//! Build-failing ratchets for the scheduled OMS reconcile loop (R2–R4).
//!
//! Pins the design contract of the config-gated DEFAULT-OFF reconcile
//! scheduler (2026-07-14): the `tokio::select!` timer arm lives in
//! `trading_pipeline.rs`, deliberately bypasses BOTH the GCRA order rate
//! limiter and the circuit breaker, and every config surface keeps the
//! loop OFF by default. Source-scan pattern per
//! `spot_1m_rest_wiring_guard.rs` (read + `match_indices` on needle
//! constants; scanned files are READ-ONLY here).

use std::fs;
use std::path::PathBuf;

/// Reads a file relative to the app crate root (CARGO_MANIFEST_DIR).
fn read_repo_file(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Bounds a source string to its production region (everything before the
/// first `#[cfg(test)]` marker), so test code can never satisfy a pin.
fn production_region(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(pos) => &src[..pos],
        None => src,
    }
}

/// Returns the body of a TOML section (from its header to the next `[`
/// header or EOF), or `None` when the section is absent.
fn toml_section_body<'a>(toml: &'a str, header: &str) -> Option<&'a str> {
    let start = toml.find(header)? + header.len();
    let rest = &toml[start..];
    let end = rest.find("\n[").unwrap_or(rest.len());
    Some(&rest[..end])
}

// R2 — the scheduled reconcile arm is wired into the trading pipeline
// production region with the locked timer semantics.
#[test]
fn ratchet_reconcile_arm_wired_in_pipeline() {
    let src = read_repo_file("src/trading_pipeline.rs");
    let prod = production_region(&src);

    let reconcile_calls: Vec<usize> = prod
        .match_indices("oms.reconcile()")
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        reconcile_calls.len(),
        1,
        "expected exactly 1 `oms.reconcile()` call site in the trading \
         pipeline production region (the scheduled select! arm)"
    );
    assert!(
        prod.contains("MissedTickBehavior::Skip"),
        "the reconcile timer must keep MissedTickBehavior::Skip \
         (no catch-up burst after a suspend/stall)"
    );
    assert!(
        prod.contains("tokio::time::interval_at("),
        "the reconcile timer must use interval_at (skips the immediate \
         t=0 tick — no boot-instant reconcile)"
    );
    assert!(
        prod.contains("fn should_run_scheduled_reconcile("),
        "the pure per-tick gate fn must exist in trading_pipeline.rs"
    );
    assert!(
        prod.contains("std::future::pending::<()>()"),
        "the disabled arm must pend forever (the make_reset_future \
         house pattern), never poll a timer"
    );
}

// R4 — the reconcile body deliberately bypasses BOTH the GCRA rate
// limiter and the circuit breaker (no starvation of live cancels, no
// breaker poisoning). `oms/engine.rs` is READ-ONLY for this guard.
#[test]
fn ratchet_reconcile_body_touches_neither_rate_limiter_nor_circuit_breaker() {
    let engine = read_repo_file("../trading/src/oms/engine.rs");
    let start = engine
        .find("pub async fn reconcile(")
        .expect("`pub async fn reconcile(` must exist in oms/engine.rs");
    let rest = &engine[start..];
    // Bound the body at the next `pub` item at the same 4-space indent.
    let end = rest[1..].find("\n    pub ").map_or(rest.len(), |p| p + 1);
    let body = &rest[..end];

    assert!(
        !body.contains("rate_limiter"),
        "reconcile() must NOT participate in the GCRA order rate limiter"
    );
    assert!(
        !body.contains("circuit_breaker"),
        "reconcile() must NOT participate in the circuit breaker"
    );
    assert!(
        body.contains("dry_run"),
        "reconcile() must keep the dry-run pre-HTTP short-circuit"
    );
}

/// Returns true when `region` maps a timeout elapse to the failed-outcome
/// counter: an `Err(_elapsed)` arm followed (inside the arm) by the
/// failed-run counter increment.
fn elapsed_maps_to_failed(region: &str) -> bool {
    match region.find("Err(_elapsed)") {
        Some(pos) => region[pos..]
            .find("m_reconcile_failed.increment(1)")
            .is_some_and(|off| off < 1200),
        None => false,
    }
}

// B4a — the production Elapsed→failed mapping: the reconcile call stays
// wrapped in a bounded timeout, and a timeout elapse counts as a FAILED
// run (outcome="failed" — streak/edge semantics identical to a typed
// OMS error, never a silent drop).
#[test]
fn ratchet_reconcile_timeout_elapse_counts_as_failed() {
    let src = read_repo_file("src/trading_pipeline.rs");
    let prod = production_region(&src);

    let reconcile_pos = prod
        .find("oms.reconcile()")
        .expect("reconcile call site must exist in the production region");
    let timeout_pos = prod[..reconcile_pos]
        .rfind("tokio::time::timeout(")
        .expect("the reconcile call must be wrapped in tokio::time::timeout");
    assert!(
        reconcile_pos - timeout_pos < 200,
        "oms.reconcile() must be the timeout's bounded future (the wrap \
         must sit immediately around the call)"
    );
    assert!(
        elapsed_maps_to_failed(&prod[reconcile_pos..]),
        "the reconcile arm's Err(_elapsed) branch must increment the \
         failed-run counter (a timeout elapse is a FAILED run)"
    );
    assert!(
        prod.contains(r#"metrics::counter!("tv_oms_reconcile_runs_total", "outcome" => "failed")"#),
        "the m_reconcile_failed handle must stay bound to the \
         outcome=failed series"
    );
}

// Self-test: the Elapsed→failed scanner cannot regress to a vacuous pass.
#[test]
fn elapsed_scanner_detects_planted_regression() {
    let good = "match tokio::time::timeout(d, oms.reconcile()).await {\n\
                Err(_elapsed) => { m_reconcile_failed.increment(1); } }";
    assert!(elapsed_maps_to_failed(good));
    let renamed_arm = "match tokio::time::timeout(d, oms.reconcile()).await {\n\
                       Err(e) => { m_reconcile_failed.increment(1); } }";
    assert!(
        !elapsed_maps_to_failed(renamed_arm),
        "a renamed elapsed arm must not satisfy the pin"
    );
    let arm_without_count = "Err(_elapsed) => { /* silent */ }";
    assert!(
        !elapsed_maps_to_failed(arm_without_count),
        "an elapsed arm that drops the failed count must trip the pin"
    );
}

// R3 — every tracked config TOML keeps [oms_reconcile] OFF; base.toml
// must carry the explicit disabled section.
#[test]
fn ratchet_reconcile_default_off_in_all_config_toml() {
    let config_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let entries = fs::read_dir(&config_dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", config_dir.display()));

    let mut base_section_disabled = false;
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let body =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if let Some(section) = toml_section_body(&body, "[oms_reconcile]") {
            assert!(
                !section.contains("enabled = true"),
                "{} must keep [oms_reconcile] enabled = false (DEFAULT-OFF \
                 lock; flipping it on needs a dated operator quote)",
                path.display()
            );
            if path.file_name().and_then(|n| n.to_str()) == Some("base.toml") {
                base_section_disabled = section.contains("enabled = false");
            }
        }
    }
    assert!(
        base_section_disabled,
        "config/base.toml must carry an explicit `[oms_reconcile]` section \
         with `enabled = false`"
    );
}

// R3 companion — the serde/Default shape in crates/common stays OFF, so an
// absent section can never enable the loop.
#[test]
fn ratchet_reconcile_config_default_is_off_in_common() {
    let cfg = read_repo_file("../common/src/config.rs");
    let start = cfg
        .find("impl Default for OmsReconcileConfig")
        .expect("OmsReconcileConfig must keep a manual Default impl");
    let window_end = (start + 600).min(cfg.len());
    let window = &cfg[start..window_end];
    assert!(
        window.contains("enabled: false"),
        "OmsReconcileConfig::default() must keep `enabled: false`"
    );
}

// Self-test: the section scanner cannot regress to a vacuous pass.
#[test]
fn toml_section_scanner_detects_flipped_flag() {
    let flipped = "[other]\nx = 1\n\n[oms_reconcile]\nenabled = true\n\n[next]\ny = 2\n";
    let section = toml_section_body(flipped, "[oms_reconcile]").expect("section found");
    assert!(section.contains("enabled = true"));
    assert!(
        !section.contains("y = 2"),
        "scanner must stop at the next section"
    );
    assert!(toml_section_body("[a]\nb = 1\n", "[oms_reconcile]").is_none());
}
