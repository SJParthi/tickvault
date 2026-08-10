//! Ratchet: the daily-P&L alarm chain is connected end to end (2026-08-10).
//!
//! # What was broken
//!
//! `tv_daily_pnl` had an alarm (`order-side-alarms.tf`, armed, `Minimum <
//! threshold`) and a place in the CloudWatch EMF allowlist
//! (`cloudwatch-agent.json`) — and **no code anywhere wrote it**. The alarm's
//! own description said as much: *"NO PRODUCTION EMIT SITE: tv_daily_pnl is
//! written by ZERO non-test code paths... that OK means NO DATA, not healthy."*
//!
//! That is worse than an absent alarm. An absent alarm is visibly absent; a
//! permanently-green alarm on a metric that never arrives looks like a
//! monitored, healthy system.
//!
//! To be precise about what was and was not at risk: the in-process kill switch
//! was **never** broken. `RiskEngine::evaluate_daily_loss_halt` reads the
//! fields directly and halts correctly. What did not exist was the independent
//! external route — and that route's entire purpose (stated in the alarm
//! description) is to page when the app-side Telegram leg is itself degraded.
//! A backup that only works while the primary works is not a backup.
//!
//! # What this guard pins
//!
//! Three links, because breaking any one silently restores the original state:
//!
//!   1. production code emits `tv_daily_pnl` (not just tests);
//!   2. it is emitted from the shared `daily_loss_state` helper, so the two
//!      callers cannot publish different numbers than the one the halt decides
//!      on;
//!   3. all three P&L gauges survive the EMF allowlist — a metric that is
//!      emitted but not allowlisted is dropped on the box and never reaches
//!      CloudWatch, which is the same invisible failure wearing the other shoe.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/trading") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

/// Strip `#[cfg(test)]` modules so a gauge that only exists in a test cannot
/// satisfy a production-emit assertion. This is the exact false-OK that made
/// the original gap survive review: `tv_daily_pnl` DID appear in the crate —
/// in tests, and in a doc comment describing the metric.
fn production_region(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut lines = src.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("#[cfg(test)]") {
            // Skip the attribute, then the item it guards, by brace depth.
            let mut depth: i32 = 0;
            let mut started = false;
            for inner in lines.by_ref() {
                depth += inner.matches('{').count() as i32;
                depth -= inner.matches('}').count() as i32;
                if inner.contains('{') {
                    started = true;
                }
                if started && depth <= 0 {
                    break;
                }
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

const ENGINE: &str = "crates/trading/src/risk/engine.rs";
const AGENT_JSON: &str = "deploy/aws/cloudwatch-agent.json";
const ALARM_TF: &str = "deploy/aws/terraform/order-side-alarms.tf";

#[test]
fn daily_pnl_gauge_has_a_production_emit_site() {
    let prod = production_region(&read(ENGINE));
    assert!(
        prod.contains(r#"metrics::gauge!("tv_daily_pnl")"#),
        "tv_daily_pnl has NO production emit site in {ENGINE}. The alarm in \
{ALARM_TF} is armed and reads Minimum < threshold with notBreaching, so with \
no datapoints it sits permanently OK — and that OK means NO DATA, not healthy. \
Restore the gauge emit rather than removing the alarm."
    );
}

#[test]
fn daily_pnl_is_emitted_from_the_shared_helper_the_halt_uses() {
    // Emitting from `check_order` and `evaluate_daily_loss_halt` separately
    // would let the published number drift from the number the halt decided
    // on — the exact drift `daily_loss_state` was extracted to prevent.
    let src = read(ENGINE);
    let helper_start = src
        .find("fn daily_loss_state(&self)")
        .expect("daily_loss_state helper must exist — it is the single source of the halt input"); // APPROVED: test
    let helper_end = src[helper_start..]
        .find("\n    }")
        .map(|off| helper_start + off)
        .expect("daily_loss_state body must terminate"); // APPROVED: test
    let body = &src[helper_start..helper_end];

    assert!(
        body.contains(r#"metrics::gauge!("tv_daily_pnl")"#),
        "tv_daily_pnl must be published from inside daily_loss_state, so the \
value CloudWatch sees is by construction the value the daily-loss halt \
evaluated. Emitting at the call sites instead reintroduces the drift this \
helper exists to prevent."
    );
    assert!(
        body.contains("is_finite()"),
        "the publish must be guarded on is_finite — a non-finite total is \
already handled one frame up by failing CLOSED into a halt plus a Critical \
Telegram, and shipping a NaN datapoint would add noise to a moment that is \
already as loud as it gets"
    );
}

#[test]
fn all_three_pnl_gauges_survive_the_emf_allowlist() {
    // A gauge that is emitted but not allowlisted is discarded by the agent on
    // the box. From CloudWatch it is indistinguishable from a gauge that was
    // never emitted — which is how this whole class of gap stays invisible.
    let agent = read(AGENT_JSON);
    for metric in ["tv_daily_pnl", "tv_realized_pnl", "tv_unrealized_pnl"] {
        assert!(
            agent.contains(metric),
            "{metric} is emitted by the risk engine but missing from the EMF \
metric_selectors allowlist in {AGENT_JSON} — the agent drops it on the box and \
it never reaches CloudWatch"
        );
    }
}

#[test]
fn the_daily_loss_alarm_still_exists_to_receive_it() {
    // The other half of the pair. Deleting the alarm would make the emit
    // pointless, and nothing else in the repo would notice.
    let tf = read(ALARM_TF);
    assert!(
        tf.contains("tv_daily_pnl"),
        "the daily-loss alarm no longer references tv_daily_pnl in {ALARM_TF} — \
if paging coverage for a daily-loss breach was deliberately removed, that needs \
a dated rule-file note, not a silent terraform edit"
    );
}

#[test]
fn cfg_test_stripper_actually_strips() {
    // Bite test for the one piece of parsing above. Without this, a stripper
    // that silently returned its input would make the production-emit
    // assertion pass on a test-only gauge — the precise false-OK being guarded
    // against.
    let src = r#"
fn real() { metrics::gauge!("tv_prod_only").set(1.0); }

#[cfg(test)]
mod tests {
    fn fake() { metrics::gauge!("tv_test_only").set(1.0); }
}
"#;
    let prod = production_region(src);
    assert!(
        prod.contains("tv_prod_only"),
        "stripper removed production code: {prod}"
    );
    assert!(
        !prod.contains("tv_test_only"),
        "stripper left a #[cfg(test)] module in place, so a test-only gauge \
could satisfy a production-emit assertion: {prod}"
    );
}
