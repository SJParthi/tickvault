//! DORMANT-alarm ratchet for `deploy/aws/terraform/order-side-alarms.tf`
//! (observability-blind-spot audit, 2026-08-09).
//!
//! ## The blind spot this pins
//!
//! Two of the three alarms in `order-side-alarms.tf` watch metrics that have
//! **ZERO production emit sites** anywhere under `crates/*/src`:
//!
//! | alarm                          | metric                      | state |
//! |--------------------------------|-----------------------------|-------|
//! | `tv-<env>-daily-loss-breach`   | `tv_daily_pnl`              | **ARMED 2026-08-10** — emit site landed, row removed from the ledger below |
//! | `tv-<env>-order-fill-lag-high` | `tv_order_fill_lag_seconds` | still dormant (Phase-1 owns the emitter) |
//!
//! That is INTENTIONAL pre-wiring ("arm-on-arrival" — the emit sites ship
//! with cluster A / Phase-1), so the alarms must NOT be deleted. But both
//! carry `treat_missing_data = "notBreaching"`, which makes a metric that
//! never publishes evaluate as **OK** — visually identical to a healthy
//! alarm, with nothing in the console saying otherwise.
//!
//! Precision the original text got wrong, corrected 2026-08-10: the phrase
//! "the daily-loss KILL SIGNAL does not exist" was too strong. The in-process
//! kill switch always existed and worked — `evaluate_daily_loss_halt` reads
//! the risk engine's fields directly. What did not exist was this INDEPENDENT
//! route, which matters precisely because its job is to page when the
//! app-side Telegram leg is degraded.
//!
//! ## What this guard enforces (both directions — the anti-drift contract)
//!
//! 1. Both alarm resources still EXIST (deleting the pre-wiring is a
//!    silent loss of the arm-on-arrival contract).
//! 2. While a metric has no emitter, its `alarm_description` must carry the
//!    `DORMANT` + `NO PRODUCTION EMIT SITE` markers, so the operator-facing
//!    surface (CloudWatch console + SNS payload) states the truth.
//! 3. **The moment a metric GAINS a production emit site, this guard FAILS**
//!    — forcing the arming PR to strip the now-false DORMANT marker and
//!    re-evaluate `treat_missing_data` instead of inheriting a stale ledger.
//!
//! Point 3 is the whole reason this is a test and not a comment: a comment
//! claiming dormancy silently becomes a LIE the day someone wires the gauge.
//!
//! Scope note: the remaining dormant metric appears today only inside
//! `crates/*/tests/` guard files, so scanning `crates/*/src` alone is
//! sufficient and needs no `#[cfg(test)]` heuristics.

#![cfg(test)]

use std::fs;
use std::path::{Path, PathBuf};

/// The two (alarm resource name, metric name, human label) triples this
/// guard governs. Adding a row here is how a future dormant alarm opts in.
const DORMANT_ALARMS: &[(&str, &str, &str)] = &[
    // 2026-08-10 — `daily_loss_breach` / `tv_daily_pnl` REMOVED from this
    // ledger because the metric was ARMED: `RiskEngine::daily_loss_state` now
    // publishes the gauge. This is the transition the three tests below were
    // written to force, and it worked exactly as designed — the arming PR
    // landed the emit site, `dormancy_claim_is_true_today` went red, and the
    // failure message named the four things to do (strip the DORMANT markers,
    // re-decide treat_missing_data, update the tf ledger, delete this row).
    // All four were done in that PR. The alarm's `treat_missing_data` was
    // consciously KEPT at "notBreaching" with the reason recorded in
    // `order-side-alarms.tf`: the box is off outside 08:30-17:30 IST weekdays,
    // so no-data is the normal state and "breaching" would page nightly — with
    // the honest limit that this alarm therefore cannot detect an in-session
    // data outage, which is the liveness alarm's job.
    (
        "order_fill_lag_high",
        "tv_order_fill_lag_seconds",
        "order fill-lag pager",
    ),
];

const ORDER_SIDE_TF: &str = "deploy/aws/terraform/order-side-alarms.tf";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Strip `//` line comments before needle matching.
///
/// Mirrors `cloudwatch_app_alarms_wiring.rs::strip_line_comments` (the
/// 2026-07-06 anti-vacuity fix): a COMMENT mentioning `counter!("tv_x")`
/// can never be an emit site, and letting it match is exactly the false-OK
/// class this repo forbids. `://` is treated as code (URL scheme inside a
/// string literal), per the `http_client_fallback_guard.rs` precedent.
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

/// Every `.rs` file under `crates/<crate>/src` (production code only —
/// `crates/*/tests` is deliberately excluded: a guard test naming a metric
/// is not an emit site).
fn production_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let crates_dir = repo_root().join("crates");
    let Ok(entries) = fs::read_dir(&crates_dir) else {
        panic!("cannot read {}", crates_dir.display());
    };
    for entry in entries.flatten() {
        let src = entry.path().join("src");
        if src.is_dir() {
            collect_rs(&src, &mut out);
        }
    }
    assert!(
        !out.is_empty(),
        "anti-vacuum: production_sources() found no .rs files under crates/*/src"
    );
    out
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| {
        // 2026-08-10: was a silent `else { return; }` — an unreadable or
        // MISSING directory became "nothing to check, pass", so the guard
        // could report green while scanning zero files.
        panic!("guard corpus unreadable {:?}: {}", dir, e)
    });
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// True iff `name` appears inside a `counter!`/`gauge!`/`histogram!` call in
/// production source. Whitespace is stripped so a rustfmt-wrapped multi-line
/// emit still matches as one contiguous needle.
fn has_production_emit_site(name: &str) -> bool {
    let needles = [
        format!("counter!(\"{name}\""),
        format!("gauge!(\"{name}\""),
        format!("histogram!(\"{name}\""),
    ];
    for path in production_sources() {
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        let code_only = strip_line_comments(&body);
        let compact: String = code_only.chars().filter(|c| !c.is_whitespace()).collect();
        if needles.iter().any(|n| compact.contains(n)) {
            return true;
        }
    }
    false
}

/// Extract ONE terraform resource block, bounded at the next top-level
/// `resource ` declaration — never a fixed byte window (the 2026-07-06
/// mutation-verified overrun hole in `cw_agent_selector_lockstep_guard.rs`).
fn alarm_block<'a>(tf: &'a str, resource_name: &str) -> &'a str {
    let marker = format!("resource \"aws_cloudwatch_metric_alarm\" \"{resource_name}\"");
    let start = tf
        .find(&marker)
        .unwrap_or_else(|| panic!("{ORDER_SIDE_TF}: resource {resource_name} not found"));
    let rest = &tf[start..];
    let end = rest[1..].find("\nresource ").map_or(rest.len(), |i| i + 1);
    &rest[..end]
}

#[test]
fn dormant_alarms_still_exist_and_are_marked_dormant() {
    let tf = read(ORDER_SIDE_TF);
    for (resource_name, metric, label) in DORMANT_ALARMS {
        let block = alarm_block(&tf, resource_name);

        // The alarm must still watch the metric it claims to watch.
        assert!(
            block.contains(&format!("metric_name         = \"{metric}\""))
                || block.contains(&format!("metric_name = \"{metric}\"")),
            "{resource_name} ({label}) no longer reads {metric} — the dormant \
             ledger in {ORDER_SIDE_TF} is stale; update it in the same PR"
        );

        if has_production_emit_site(metric) {
            // Handled by the dedicated inverse test below — skip here so the
            // failure message names the arming case precisely.
            continue;
        }

        assert!(
            block.contains("DORMANT 2026-08-09"),
            "{resource_name} ({label}) watches {metric}, which has ZERO \
             production emit sites, but its alarm_description no longer \
             carries the `DORMANT 2026-08-09` marker. With \
             treat_missing_data=notBreaching this alarm reads OK forever — an \
             operator MUST be able to see from the description alone that the \
             OK means NO DATA, not healthy."
        );
        assert!(
            block.contains("NO PRODUCTION EMIT SITE"),
            "{resource_name} ({label}): the alarm_description must state \
             `NO PRODUCTION EMIT SITE` verbatim — the console/SNS payload is \
             the only operator-facing surface that can carry this fact."
        );
    }
}

#[test]
fn arming_a_dormant_metric_forces_the_ledger_to_be_revisited() {
    // THE INVERSE RATCHET. The day cluster A / Phase-1 wires one of these
    // gauges, this test fails and the arming PR is forced to (a) strip the
    // now-false DORMANT marker from the description and (b) consciously
    // re-decide treat_missing_data. Without it, a stale "DORMANT" claim
    // would quietly become a lie the moment the metric went live.
    let tf = read(ORDER_SIDE_TF);
    let mut newly_armed = Vec::new();
    for (resource_name, metric, label) in DORMANT_ALARMS {
        if !has_production_emit_site(metric) {
            continue;
        }
        let block = alarm_block(&tf, resource_name);
        if block.contains("DORMANT 2026-08-09") || block.contains("NO PRODUCTION EMIT SITE") {
            newly_armed.push(format!("{resource_name} ({label}, metric {metric})"));
        }
    }
    assert!(
        newly_armed.is_empty(),
        "These alarms' metrics NOW HAVE production emit sites, but their \
         alarm_descriptions still claim DORMANT / NO PRODUCTION EMIT SITE: \
         {newly_armed:?}. Remove the stale dormancy markers AND re-evaluate \
         treat_missing_data (\"notBreaching\" was chosen only because the \
         metric never published; with real data it silently converts a \
         data-outage into a green OK). Update the DORMANT-ALARM LEDGER header \
         in {ORDER_SIDE_TF} and the crates/app order_side_paging_wiring_guard \
         in the same PR."
    );
}

#[test]
fn dormancy_claim_is_true_today() {
    // Records the audited state as of 2026-08-09 so a REVIEWER can see the
    // claim was verified, not assumed. If this starts failing, the two tests
    // above have already told the arming PR what to do — this one just makes
    // the transition explicit and dated.
    for (resource_name, metric, _label) in DORMANT_ALARMS {
        assert!(
            !has_production_emit_site(metric),
            "{metric} (alarm {resource_name}) gained a production emit site. \
             This is GOOD NEWS — the alarm can finally evaluate real data. \
             Update the DORMANT-ALARM LEDGER in {ORDER_SIDE_TF}, strip the \
             DORMANT markers from the alarm_description, re-decide \
             treat_missing_data, and delete this row from DORMANT_ALARMS."
        );
    }
}

#[test]
fn emit_site_scanner_is_not_vacuous() {
    // Anti-vacuity self-test: a scanner that always returned `false` would
    // make `dormancy_claim_is_true_today` pass forever, exactly the false-OK
    // class audit-findings Rule 11 forbids. Pin it against a metric with a
    // KNOWN live production emit site.
    assert!(
        has_production_emit_site("tv_questdb_disconnected_seconds"),
        "scanner is broken: tv_questdb_disconnected_seconds has a real \
         gauge! emit site in crates/storage/src/questdb_health.rs"
    );
    assert!(
        !has_production_emit_site("tv_definitely_not_a_real_metric_xyz"),
        "scanner is broken: it reports an emit site for a made-up metric"
    );
}

#[test]
fn emit_site_scanner_ignores_comment_only_mentions() {
    // A commented-out emit must never count (the 2026-07-06 mutation-proven
    // hole). Verified through the same strip_line_comments path the scanner
    // uses, on a synthetic body.
    let body = "// counter!(\"tv_fake_commented_metric\").increment(1);\nlet a = 1;\n";
    let stripped = strip_line_comments(body);
    let compact: String = stripped.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        !compact.contains("counter!(\"tv_fake_commented_metric\""),
        "comment stripping failed — a commented emit would satisfy the guard"
    );
    // ...and real code on the same line shape survives.
    let real = "counter!(\"tv_fake_real_metric\").increment(1); // note\n";
    let kept: String = strip_line_comments(real)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        kept.contains("counter!(\"tv_fake_real_metric\""),
        "comment stripping removed real code"
    );
}
