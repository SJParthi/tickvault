//! Build-failing ratchet pinning the **effective budget kill-ceiling lockstep**.
//!
//! The ceiling is not a display value — it is a KILL line. At 100% of
//! `budget.tf limit_amount`, the one native action stops the prod box; the
//! hard-stop-guard Lambda additionally disables the morning start cron.
//! Since 1 November 2026, native $75 matches the EFFECTIVE guard/digest line.
//! Their configured $185 values preserve the October-only allowance and are
//! clamped by UTC billing month. Four places encode these two roles:
//!
//! | # | Site | Role |
//! |---|---|---|
//! | 1 | `deploy/aws/terraform/budget.tf` `limit_amount` | the REAL AWS ceiling |
//! | 2 | `deploy/aws/terraform/budget-guards.tf` `BUDGET_KILL_USD` | injected into the guard Lambda |
//! | 3 | `crates/aws-lambdas/src/budget_digest.rs` `BUDGET_USD` | the "% of stop-budget" the operator reads |
//! | 4 | `crates/aws-lambdas/src/hard_stop_guard.rs` `DEFAULT_BUDGET_KILL_USD` | env-missing FALLBACK |
//!
//! **Why this guard exists (a real defect, not a hypothetical).** Until
//! 2026-08-08 these were held together by `KEEP IN SYNC` COMMENTS ONLY, and
//! site 4 had already drifted: it sat at 55.0 across the $55 → $25 → $35
//! ceiling steps. That was tolerated as a "flagged follow-up" because 55 was
//! ABOVE the live ceiling, so a missing env var killed LATER than the real
//! line — the harmless direction, and the rule files said so explicitly.
//!
//! Raising the ceiling to $100 (operator Quote 13) SILENTLY INVERTED that
//! property. 55 became BELOW both the new ceiling and the expected ~$73.60
//! monthly bill, so a missing or fat-fingered `BUDGET_KILL_USD` would have
//! stopped the prod box mid-session at $55 — every month, with the documented
//! safety argument still reading "kills later, never earlier". A comment
//! cannot catch that. This test can.
//!
//! Authority: `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md`
//! §0 Quotes 13, 37 and 45 + §7; `.claude/rules/project/aws-budget.md`.

#![cfg(test)]

use std::path::{Path, PathBuf};

use tickvault_aws_lambdas::budget_digest::BUDGET_USD;
use tickvault_aws_lambdas::hard_stop_guard::{
    DEFAULT_BUDGET_KILL_USD, OCTOBER_2026_ALLOWANCE, OCTOBER_2026_KILL_USD,
    SEPTEMBER_2026_ALLOWANCE, STANDING_BUDGET_KILL_USD, effective_budget_kill_usd,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/aws-lambdas parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Pull the value out of an assignment like `limit_amount      = "100"`,
/// ignoring commented-out lines so a dated audit comment can never satisfy it.
fn hcl_quoted_number(body: &str, key: &str) -> f64 {
    let mut found: Option<f64> = None;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix(key) else {
            continue;
        };
        let Some(eq) = rest.find('=') else { continue };
        let after = &rest[eq + 1..];
        let Some(open) = after.find('"') else {
            continue;
        };
        let tail = &after[open + 1..];
        let Some(close) = tail.find('"') else {
            continue;
        };
        let raw = &tail[..close];
        let parsed = raw
            .parse::<f64>()
            .unwrap_or_else(|e| panic!("{key} value {raw:?} is not a number: {e}"));
        assert!(
            found.is_none() || found == Some(parsed),
            "{key} appears more than once with DIFFERENT values ({found:?} vs {parsed}) — \
             the lockstep is ambiguous; collapse it to one assignment"
        );
        found = Some(parsed);
    }
    found.unwrap_or_else(|| panic!("no uncommented `{key} = \"<number>\"` assignment found"))
}

/// Native $75 must match all effective November-onward lines, while the
/// three configured values retain the explicitly historical October allowance.
#[test]
fn budget_ceiling_agrees_across_all_four_sites() {
    let root = repo_root();
    let tf_limit = hcl_quoted_number(
        &read(&root.join("deploy/aws/terraform/budget.tf")),
        "limit_amount",
    );
    let tf_kill = hcl_quoted_number(
        &read(&root.join("deploy/aws/terraform/budget-guards.tf")),
        "BUDGET_KILL_USD",
    );
    assert_eq!(
        tf_limit, STANDING_BUDGET_KILL_USD,
        "the native limit must be the authorized standing line, not October's allowance"
    );
    for (site, configured) in [
        ("BUDGET_KILL_USD", tf_kill),
        ("budget_digest::BUDGET_USD", BUDGET_USD),
        ("DEFAULT_BUDGET_KILL_USD", DEFAULT_BUDGET_KILL_USD),
    ] {
        assert_eq!(
            configured, OCTOBER_2026_KILL_USD,
            "{site} must preserve the historical October configuration"
        );
        for year in 2026..=2028 {
            for month in 1..=12 {
                if (year, month) < (2026, 11) {
                    continue;
                }
                assert_eq!(
                    effective_budget_kill_usd(configured, year, month),
                    tf_limit,
                    "{site} disagrees with native standing limit in {year}-{month:02}"
                );
            }
        }
    }
}

/// The effective November fallback may never sit below the native line.
/// Compare after the month clamp: the historical raw $185 default is not
/// the amount that the running November guard actually uses.
#[test]
fn fallback_kill_line_is_never_below_the_real_ceiling() {
    let root = repo_root();
    let tf_limit = hcl_quoted_number(
        &read(&root.join("deploy/aws/terraform/budget.tf")),
        "limit_amount",
    );
    let effective = effective_budget_kill_usd(DEFAULT_BUDGET_KILL_USD, 2026, 11);
    assert!(
        effective >= tf_limit,
        "effective November fallback ({effective}) is below the native ceiling ({tf_limit})"
    );
}

/// The recorded October bill estimate belongs to October's allowance,
/// never to the November standing native line authorized by Quotes 37/45.
#[test]
fn october_allowance_brackets_the_recorded_bill_high_estimate() {
    let october_line = effective_budget_kill_usd(DEFAULT_BUDGET_KILL_USD, 2026, 10);
    // 2026-10-13 (Quote 30): the IOPS reduction made the October projection
    // about $165 (Assumed). Retain that historical check without imposing
    // October's bill on the separately authorized November $75 limit.
    const BILL_HIGH_USD: f64 = 165.0;
    assert!(
        october_line > BILL_HIGH_USD,
        "October's stop line (${october_line:.2}) must clear its recorded estimate \
         (${BILL_HIGH_USD:.2}); this is not the November native limit"
    );
    assert!(
        october_line <= 3.0 * BILL_HIGH_USD,
        "October's line (${october_line}) is more than 3x its recorded estimate \
         (${BILL_HIGH_USD:.2})"
    );
}

/// Quote 30 (2026-10-13): exactly ONE native budget stop, at 100%. A second
/// action below 100% would stop the box under the owner's ₹15,000 line
/// (at $170, a 90% action fires at $153 ≈ ₹13,500).
#[test]
fn native_stop_is_one_action_at_100_percent() {
    let body = read(&repo_root().join("deploy/aws/terraform/budget.tf"));
    let live: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        live.matches("resource \"aws_budgets_budget_action\"")
            .count(),
        1,
        "budget.tf must declare exactly one aws_budgets_budget_action (Quote 30)"
    );
    assert!(
        live.contains("action_threshold_value = 100"),
        "the one native stop must fire at 100% of limit_amount"
    );
    assert!(
        !live.contains("stop_at_90"),
        "the 90% early stop was removed by Quote 30; re-adding it needs a dated quote"
    );
}

/// The rule files must keep RECORDING the ceiling, so the authority chain and
/// the code cannot silently disagree.
#[test]
fn rule_files_record_the_current_ceiling() {
    let root = repo_root();
    let tf_limit = hcl_quoted_number(
        &read(&root.join("deploy/aws/terraform/budget.tf")),
        "limit_amount",
    );
    let dollars = format!("${}", tf_limit as i64);

    let daily = read(
        &root.join("docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md"),
    );
    assert!(
        daily.contains(&dollars),
        "daily-universe §7 must record the current kill ceiling ({dollars})"
    );
    let budget_rules = read(&root.join(".claude/rules/project/aws-budget.md"));
    assert!(
        budget_rules.contains(&dollars),
        "aws-budget.md must record the current kill ceiling ({dollars})"
    );
}

/// Quote 23 (2026-09-23): the September 2026 allowance is ONE month, and the
/// standing ceiling after it was $150; operator Quote 30 (2026-10-13) set it
/// to $170 (≈₹15,000); operator Quote 33 (2026-10-21) set it to $100 from
/// November 2026, with October 2026 capped at $170; operator Quote 36 (the
/// same evening) lowered November onward to $85, and operator Quote 37 to $75
/// (GST included). Pinned so an allowance can
/// never quietly become the new normal, and so the clamp can never sit ABOVE
/// the configured ceiling (which would make it a raise, not a clamp).
#[test]
fn standing_cap_is_75_from_november_and_each_allowance_is_one_month() {
    assert!(
        (STANDING_BUDGET_KILL_USD - 75.0).abs() < f64::EPSILON,
        "the standing ceiling moved off $75 without a dated operator quote"
    );
    // "The clamp never sits above the configured ceiling" is a compile-time
    // assert in hard_stop_guard.rs, next to the two constants.
    assert_eq!(SEPTEMBER_2026_ALLOWANCE, (2026, 9));
    assert_eq!(OCTOBER_2026_ALLOWANCE, (2026, 10));
    // Operator Quote 45 (2026-10-28): October 2026 only, $170 -> $185.
    assert!((OCTOBER_2026_KILL_USD - 185.0).abs() < f64::EPSILON);

    // Every month of the next three years EXCEPT 2026-09 and 2026-10 resolves
    // to $75.
    for year in 2026..=2028 {
        for month in 1..=12 {
            let effective = effective_budget_kill_usd(DEFAULT_BUDGET_KILL_USD, year, month);
            if (year, month) == SEPTEMBER_2026_ALLOWANCE {
                assert!((effective - DEFAULT_BUDGET_KILL_USD).abs() < f64::EPSILON);
            } else if (year, month) == OCTOBER_2026_ALLOWANCE {
                assert!((effective - OCTOBER_2026_KILL_USD).abs() < f64::EPSILON);
            } else {
                assert!(
                    (effective - STANDING_BUDGET_KILL_USD).abs() < f64::EPSILON,
                    "{year}-{month:02} resolved to ${effective}, not the standing $75"
                );
            }
        }
    }

    // Both rule files record BOTH numbers.
    let root = repo_root();
    for rel in [
        "docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md",
        ".claude/rules/project/aws-budget.md",
    ] {
        let body = read(&root.join(rel));
        assert!(
            body.contains("$150")
                && body.contains("$225")
                && body.contains("$170")
                && body.contains("$185")
                && body.contains("$75 from 1 November"),
            "{rel} must record $150, $225, $170, $185 and $75 from 1 November"
        );
    }
}
