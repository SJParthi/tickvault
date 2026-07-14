//! Source-scan ratchet — cluster-C order-side observability wiring
//! (2026-07-14). Pins the load-bearing wiring so a refactor can never
//! silently sever it:
//!
//! 1. `run_trading_pipeline` wires BOTH alert sinks — the first-ever
//!    `set_alert_sink` call sites (both sinks were `None` since birth;
//!    every `fire_alert` / `fire_risk_halt` was a no-op).
//! 2. main.rs constructs `OrderSideWiring` at EVERY trading-pipeline
//!    spawn site. (Reshaped 2026-07-14 through the PR-C2 merge: the
//!    original "BOTH boot arms" wording pinned the fast crash-recovery
//!    arm + slow Step 10.5, BOTH deleted with the Dhan live-WS lane —
//!    main.rs carries ZERO pipeline spawn sites until the live-trading
//!    re-wire. The pin is now conditional: constructions == spawn sites,
//!    so 0/0 passes honestly today and the FIRST re-wired pipeline spawn
//!    without order-side wiring fails the build.)
//! 3. `crates/trading/src/risk/engine.rs::trigger_halt` carries the
//!    RISK-GAP-01 coded `error!` field and the stale "Loki → Grafana"
//!    routing comment stays deleted (the error!-alone-pages premise died
//!    with the CloudWatch-only migration).
//!
//! House precedents: `fast_boot_token_validation_wiring_guard.rs`,
//! `seal_drop_paging_wiring_guard.rs` (source-order scans from an app
//! integration test).

use std::fs;
use std::path::PathBuf;

use tickvault_common::error_code::ErrorCode;

fn read_repo_file(rel_from_app: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_from_app);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())) // APPROVED: test
}

/// Strip `//`-line-comments (treating `://` URL scheme separators as code)
/// so a comment mentioning a needle can never satisfy the scan.
fn strip_line_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut cut = line.len();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Everything strictly before the first top-level `#[cfg(test)]` — the
/// production region (robust to code after `mod tests`).
fn production_region(body: &str) -> &str {
    body.split("#[cfg(test)]").next().unwrap_or(body)
}

/// Whitespace-compacted form so rustfmt re-wrapping can never hide a real
/// site from a contiguous needle.
fn compact(body: &str) -> String {
    body.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn test_trading_pipeline_sets_both_alert_sinks() {
    let src = read_repo_file("src/trading_pipeline.rs");
    let prod = compact(&strip_line_comments(production_region(&src)));
    assert!(
        prod.contains("oms.set_alert_sink("),
        "trading_pipeline.rs must wire the OMS alert sink — without it every \
         OmsAlert (rejections, circuit transitions, rate-limit exhaustion) is \
         a silent no-op again (the pre-cluster-C state)"
    );
    assert!(
        prod.contains("risk_engine.set_alert_sink("),
        "trading_pipeline.rs must wire the risk alert sink — without it the \
         Critical RiskHalt Telegram page never fires"
    );
    assert!(
        prod.contains("run_order_side_consumer("),
        "trading_pipeline.rs must spawn the order-side consumer that drains \
         the bounded channel the two bridges feed"
    );
}

#[test]
fn test_main_wires_order_side_wiring_at_every_pipeline_spawn_site() {
    // Reshaped 2026-07-14 (PR-C2 merge reconcile): the original assertion
    // required >= 2 `OrderSideWiring{` constructions ("BOTH boot arms" —
    // the fast crash-recovery arm + slow Step 10.5). BOTH arms were
    // deleted with the Dhan live-WS lane in PR-C2 (the reviewed C2 head
    // already carried zero `spawn_trading_pipeline` call sites in
    // main.rs), so the trading pipeline — and with it cluster-C's
    // boot-time order-side wiring — is DORMANT until the live-trading
    // re-wire (the same class as the order-update WS dormancy,
    // scope-lock §A.1). The guard's INTENT is preserved conditionally:
    // every pipeline spawn site must construct OrderSideWiring, so the
    // first re-wired spawn without it fails the build — while 0/0 passes
    // honestly today instead of demanding wiring on arms that no longer
    // exist.
    let src = read_repo_file("src/main.rs");
    let prod = compact(&strip_line_comments(production_region(&src)));
    let spawn_sites = prod.matches("spawn_trading_pipeline").count();
    let constructions = prod.matches("OrderSideWiring{").count();
    assert_eq!(
        constructions, spawn_sites,
        "main.rs must construct OrderSideWiring at EVERY trading-pipeline \
         spawn site (found {constructions} construction(s) for \
         {spawn_sites} spawn site(s)) — a spawn without the wiring makes \
         order-side observability silently unwired on that boot path; a \
         construction without a spawn is dead wiring"
    );
}

#[test]
fn test_risk_halt_error_is_coded_risk_gap_01() {
    // The code_str is built from the REAL enum so a rename breaks THIS
    // test instead of silently orphaning the coded emit.
    assert_eq!(ErrorCode::RiskGapPreTrade.code_str(), "RISK-GAP-01");
    let src = read_repo_file("../trading/src/risk/engine.rs");
    let prod_raw = production_region(&src);
    let prod = compact(&strip_line_comments(prod_raw));
    let halt_at = prod
        .find("fntrigger_halt(")
        .expect("risk/engine.rs must still define trigger_halt"); // APPROVED: test
    let after_halt = &prod[halt_at..];
    assert!(
        after_halt.contains("code=")
            && after_halt.contains("ErrorCode::RiskGapPreTrade.code_str()"),
        "trigger_halt's error! must carry code = ErrorCode::RiskGapPreTrade.code_str() \
         (charter always-on rule 5 — the first production RISK-GAP-01 emit)"
    );
    assert!(
        !prod_raw.contains("Loki → Grafana") && !prod_raw.contains("Loki -> Grafana"),
        "the stale 'ERROR level triggers Telegram via Loki → Grafana' comment \
         must stay deleted — that route was retired by the CloudWatch-only \
         migration; the Telegram page is the sink's Critical RiskHalt event"
    );
}
