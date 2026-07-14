//! Source-scan ratchet — DHAN order-side alert bridge wired end-to-end
//! (2026-07-14, noise-lock §2.1 grant; umbrella Cluster B).
//!
//! Pins the chain in lockstep so a rename / deletion on either side fails
//! the build:
//!
//! 1. `crates/app/src/trading_pipeline.rs` wires BOTH engines' alert
//!    sinks (`oms.set_alert_sink` + `risk_engine.set_alert_sink`) inside
//!    the production region, gated on `config.notifier` — the first-ever
//!    production `set_alert_sink` callers. Losing either call silently
//!    re-orphans that engine's alerts (the pre-2026-07-14 state: fire
//!    sites existed, sink was always `None`).
//! 2. `crates/app/src/oms_alert_bridge.rs` maps ALL FOUR `OmsAlert::`
//!    arms + `fire_risk_halt`, carries BOTH coded emits
//!    (`ErrorCode::OmsGapRateLimit` / `ErrorCode::RiskGapPreTrade` — the
//!    paging drift guard's real ERROR emit sites for OMS-GAP-04 /
//!    RISK-GAP-01), the `Handle::try_current` panic guard (release
//!    `panic = "abort"` — notify's internal spawn would abort outside a
//!    runtime), and the counted drop arm.
//!
//! House pattern: `seal_drop_paging_wiring_guard.rs` (comment-stripped
//! source-order scan). Runbook: `.claude/rules/project/gap-enforcement.md`.

use std::fs;
use std::path::PathBuf;

fn read_repo_file(rel_from_app: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_from_app);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())) // APPROVED: test
}

/// Strip `//`-line-comments (treating `://` URL scheme separators as code,
/// the `http_client_fallback_guard.rs` precedent) so a comment mentioning a
/// needle can never satisfy the scan (anti-vacuity, the
/// cloudwatch_app_alarms_wiring.rs:80-89 precedent).
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
/// production region.
fn production_region(body: &str) -> &str {
    body.split("#[cfg(test)]").next().unwrap_or(body)
}

#[test]
fn test_strip_line_comments_drops_comment_mentions() {
    // Anti-vacuity self-test: a comment mentioning a needle must be
    // removed; code and URLs survive.
    let src = "let a = 1; // fake: oms.set_alert_sink(\n\
               let url = \"https://example.com\";\n";
    let stripped = strip_line_comments(src);
    assert!(
        !stripped.contains("set_alert_sink"),
        "comment text survived"
    );
    assert!(
        stripped.contains("let a = 1;") && stripped.contains("https://example.com"),
        "code or URL text was wrongly removed"
    );
}

#[test]
fn test_trading_pipeline_sets_both_alert_sinks() {
    let body = read_repo_file("src/trading_pipeline.rs");
    let prod = strip_line_comments(production_region(&body));
    let compact: String = prod.chars().filter(|c| !c.is_whitespace()).collect();

    // Exactly TWO set_alert_sink calls in the production region — one per
    // engine. More would be a duplicate wiring; fewer re-orphans an engine.
    let count = prod.matches("set_alert_sink(").count();
    assert_eq!(
        count, 2,
        "trading_pipeline.rs production region must contain exactly two \
         set_alert_sink( calls (oms + risk_engine); found {count}"
    );
    assert!(
        compact.contains("oms.set_alert_sink(Box::new(OmsAlertBridge::new("),
        "the OMS alert sink must be wired to OmsAlertBridge in run_trading_pipeline"
    );
    assert!(
        compact.contains("risk_engine.set_alert_sink(Box::new(OmsAlertBridge::new("),
        "the Risk alert sink must be wired to OmsAlertBridge in run_trading_pipeline"
    );
    // The wiring is gated on the optional notifier (None in tests keeps
    // the engines sink-less — no NoOp Telegram counters from unit tests).
    assert!(
        compact.contains("ifletSome(refnotifier)=config.notifier"),
        "the sink wiring must be gated on `if let Some(ref notifier) = config.notifier`"
    );
}

#[test]
fn test_bridge_covers_all_oms_alert_arms() {
    let body = read_repo_file("src/oms_alert_bridge.rs");
    let prod = strip_line_comments(production_region(&body));
    let compact: String = prod.chars().filter(|c| !c.is_whitespace()).collect();

    // All four OmsAlert arms + the risk-halt trait method.
    for needle in [
        "OmsAlert::OrderRejected",
        "OmsAlert::CircuitBreakerOpened",
        "OmsAlert::CircuitBreakerClosed",
        "OmsAlert::RateLimitExhausted",
        "fnfire_risk_halt(&self,reason:&'staticstr)",
    ] {
        assert!(
            compact.contains(
                &needle
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>()
            ),
            "oms_alert_bridge.rs production region must match `{needle}` — \
             an unmapped alert arm is a silently-dropped Telegram class"
        );
    }
    // Both coded emits (the paging drift guard's OMS-GAP-04 / RISK-GAP-01
    // ERROR-level emit sites) — tag-guard enum convention, never raw strings.
    for needle in [
        "ErrorCode::OmsGapRateLimit.code_str()",
        "ErrorCode::RiskGapPreTrade.code_str()",
    ] {
        assert!(
            compact.contains(needle),
            "oms_alert_bridge.rs must carry the coded emit `{needle}` — the \
             error-code-alarms.tf filter is dead without it"
        );
    }
    // The panic guard + the counted drop arm (release panic=abort: notify's
    // internal tokio::spawn would abort the process outside a runtime).
    assert!(
        compact.contains("tokio::runtime::Handle::try_current().is_ok()"),
        "the bridge must guard dispatch with Handle::try_current() — \
         removing it makes a no-runtime fire an instant process abort"
    );
    assert!(
        compact.contains("tv_oms_alert_bridge_dropped_total"),
        "the no-runtime drop arm must increment tv_oms_alert_bridge_dropped_total"
    );
}
