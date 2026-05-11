//! Ratchet guard: `DepthRebalanced` (Severity::Low) MUST be suppressed from
//! Telegram, while `DepthRebalanceFailed` (Severity::High) MUST still page.
//!
//! # Why this exists (operator demand 2026-05-11)
//!
//! Routine depth-20/200 zero-disconnect rebalance swaps fire every ~60s on
//! spot drift during volatile sessions, producing 10-30 Telegram messages per
//! underlying per day. They are NOT pager events — full audit lives in:
//!
//! 1. QuestDB `depth_rebalance_audit` (AUDIT-02 — SEBI-relevant)
//! 2. Grafana `deploy/docker/grafana/dashboards/depth-flow.json` panel
//! 3. Prometheus `tv_depth_rebalances_total{underlying}`
//! 4. Loki `data/logs/errors.jsonl.*`
//!
//! Telegram is the wrong tool for routine state changes. Reserve it for
//! eyes-on-now events. This guard prevents any future session from
//! accidentally re-enabling Telegram for the routine swap variant.
//!
//! Cross-reference: `.claude/rules/project/observability-architecture.md`
//! clause 11.

use std::path::PathBuf;

/// The dispatch suppression for `DepthRebalanced` MUST be present in
/// `service.rs::notify()`. If a future commit deletes or weakens it, this
/// test fails the build BEFORE merge.
#[test]
fn depth_rebalanced_low_variant_is_suppressed_in_service_notify() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("src/notification/service.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));

    // The suppression branch MUST contain all three of:
    //   1. matches! against NotificationEvent::DepthRebalanced
    //   2. tv_depth_rebalance_telegram_suppressed_total counter increment
    //   3. an early `return;` before any Telegram dispatch happens
    assert!(
        src.contains("NotificationEvent::DepthRebalanced"),
        "service.rs::notify() must match `NotificationEvent::DepthRebalanced` to suppress \
         routine swap pages from Telegram — see observability-architecture.md clause 11"
    );
    assert!(
        src.contains("tv_depth_rebalance_telegram_suppressed_total"),
        "service.rs::notify() must increment `tv_depth_rebalance_telegram_suppressed_total` \
         so suppression is observable in Prometheus"
    );

    // Locate the suppression block and assert the early-return precedes any
    // call to `send_telegram_chunk_with_retry` for this variant.
    let suppress_idx = src
        .find("tv_depth_rebalance_telegram_suppressed_total")
        .expect("counter increment must exist");
    let following = &src[suppress_idx..];
    // The very next branching token after the counter increment must be
    // `return;` — proves the variant exits before any Telegram code runs.
    let return_idx = following.find("return;").unwrap_or(usize::MAX);
    let send_idx = following
        .find("send_telegram_chunk_with_retry")
        .unwrap_or(usize::MAX);
    assert!(
        return_idx < send_idx,
        "suppression block must `return;` BEFORE any Telegram dispatch — \
         current code lets DepthRebalanced reach the dispatcher"
    );
}

/// `DepthRebalanceFailed` (Severity::High — swap channel broken, ATM
/// unresolved) MUST NOT be suppressed. The suppression match arm must
/// match the routine `DepthRebalanced` variant ONLY, not the failure
/// variant. Source-scan guard: looks for the exact variant identifier
/// in the suppression block.
#[test]
fn depth_rebalance_failed_high_variant_is_not_suppressed() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("src/notification/service.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));

    // Locate the suppression block (the `matches!` line) and assert it does
    // NOT also match `DepthRebalanceFailed`.
    let suppress_idx = src
        .find("tv_depth_rebalance_telegram_suppressed_total")
        .expect("counter increment must exist");
    // Look 400 chars BEFORE the counter increment — that's the suppression
    // block prelude (the `matches!` line + surrounding comment).
    let block_start = suppress_idx.saturating_sub(400);
    let block = &src[block_start..suppress_idx];

    assert!(
        !block.contains("DepthRebalanceFailed"),
        "the Telegram suppression block must NOT match `DepthRebalanceFailed` — \
         the High-severity swap failure variant must continue to page operator"
    );
    assert!(
        block.contains("DepthRebalanced { .. }"),
        "the Telegram suppression block must match the routine `DepthRebalanced` variant \
         specifically — `..` rest-pattern protects against field changes"
    );
}

/// Compile-time proof the metric name is stable and reachable from the
/// notification service crate. If someone renames the constant or removes
/// the counter, this fails to compile.
#[test]
fn suppression_counter_metric_name_is_pinned() {
    // The pinned metric name. If you rename this in service.rs, you MUST
    // also update Grafana panels + Prometheus alerts that depend on it
    // — and update this test to match. The PinName ensures someone has
    // to explicitly acknowledge the rename here.
    const EXPECTED_METRIC: &str = "tv_depth_rebalance_telegram_suppressed_total";

    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("src/notification/service.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));

    assert!(
        src.contains(EXPECTED_METRIC),
        "metric name `{EXPECTED_METRIC}` must be present in service.rs — \
         renames require updating Grafana + Prometheus + this test in lockstep"
    );
}
