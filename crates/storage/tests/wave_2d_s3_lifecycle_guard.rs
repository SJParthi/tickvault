//! Wave-2-D Fix 5 — S3 lifecycle policy guard.
//!
//! The Wave-2-D security audit flagged that `order_audit` had an
//! `Expiration: { Days: 1825 }` rule (= exactly 5 years) — the SEBI
//! floor with zero margin. A late SEBI inspection request the day
//! after the rule fires would find the data gone. `aws-budget.md`
//! states "all orders forever"; Wave-2-D Fix 5 removes the Expiration
//! on the `order_audit` prefix and renames the rule ID from
//! `order-audit-sebi-5y` to `order-audit-forever`. Future regressions
//! that re-add an Expiration to this rule fail the build.

use std::fs;
use std::path::{Path, PathBuf};

const S3_LIFECYCLE_JSON: &str = "deploy/aws/s3-lifecycle-audit-tables.json";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_lifecycle() -> serde_json::Value {
    let path = workspace_root().join(S3_LIFECYCLE_JSON);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|err| panic!("S3 lifecycle JSON must parse: {err}"))
}

fn rules(doc: &serde_json::Value) -> &Vec<serde_json::Value> {
    doc["Rules"]
        .as_array()
        .expect("S3 lifecycle JSON must have a `Rules` array")
}

#[test]
fn order_audit_rule_has_no_expiration() {
    let doc = read_lifecycle();
    let rules = rules(&doc);
    let order_rule = rules
        .iter()
        .find(|r| {
            r["Filter"]["Prefix"]
                .as_str()
                .is_some_and(|p| p == "order_audit/")
        })
        .expect("order_audit/ rule MUST exist");
    assert!(
        order_rule.get("Expiration").is_none(),
        "Wave-2-D Fix 5 regression: order_audit MUST NOT have an \
         Expiration rule. SEBI 5y is a floor, not a ceiling. \
         aws-budget.md states 'all orders forever'. A future PR that \
         re-adds Expiration risks silently deleting order audit data \
         the day after the rule fires."
    );
}

#[test]
fn order_audit_rule_id_is_forever_not_sebi_5y() {
    let doc = read_lifecycle();
    let rules = rules(&doc);
    let order_rule = rules
        .iter()
        .find(|r| {
            r["Filter"]["Prefix"]
                .as_str()
                .is_some_and(|p| p == "order_audit/")
        })
        .expect("order_audit/ rule MUST exist");
    let id = order_rule["ID"].as_str().expect("rule must have ID");
    assert_eq!(
        id, "order-audit-forever",
        "Wave-2-D Fix 5 regression: order_audit rule ID must be \
         `order-audit-forever` to make the no-Expiration intent \
         self-documenting. Found `{id}`."
    );
}

#[test]
fn five_other_audit_rules_keep_their_5y_expiration() {
    // The 5 non-order audit tables CAN expire at 1825 days because
    // they are operational/diagnostic — not SEBI-mandated. Pin this
    // so a future "let's apply forever-keep to everything" PR
    // doesn't silently bloat the storage budget.
    let doc = read_lifecycle();
    let rules = rules(&doc);
    let mut count_with_5y_expiration = 0;
    for r in rules {
        let prefix = r["Filter"]["Prefix"].as_str().unwrap_or("");
        if prefix == "order_audit/" {
            continue;
        }
        if let Some(exp) = r.get("Expiration") {
            if exp["Days"].as_i64() == Some(1825) {
                count_with_5y_expiration += 1;
            }
        }
    }
    assert_eq!(
        count_with_5y_expiration, 5,
        "Wave-2-D Fix 5: expected 5 non-order audit rules with \
         `Expiration: 1825d` (phase2_audit, depth_rebalance_audit, \
         ws_reconnect_audit, boot_audit, selftest_audit). Found {count_with_5y_expiration}."
    );
}

#[test]
fn all_rules_transition_to_glacier_ir_at_90d_then_deep_archive_at_365d() {
    // Glacier Deep Archive has a 180-day minimum storage duration.
    // Day 90 → GLACIER_IR (90d minimum) → Day 365 → DEEP_ARCHIVE.
    // Net storage at 365d position: 90d in IT + 275d in GLACIER_IR
    // → satisfies GLACIER_IR's 90d minimum before transition. Pin
    // the schedule so a future "let's go straight to Deep Archive
    // at 90d" PR doesn't break Glacier minimums.
    let doc = read_lifecycle();
    let rules = rules(&doc);
    for r in rules {
        let id = r["ID"].as_str().unwrap_or("?");
        let transitions = r["Transitions"]
            .as_array()
            .unwrap_or_else(|| panic!("rule `{id}` must have Transitions array"));
        assert_eq!(
            transitions.len(),
            3,
            "rule `{id}` must have exactly 3 transitions \
             (IT @0, GLACIER_IR @90, DEEP_ARCHIVE @365)"
        );
        assert_eq!(
            transitions[0]["Days"].as_i64(),
            Some(0),
            "rule `{id}` transition 0 must be at Day 0"
        );
        assert_eq!(
            transitions[1]["StorageClass"].as_str(),
            Some("GLACIER_IR"),
            "rule `{id}` transition 1 must be GLACIER_IR (NOT \
             DEEP_ARCHIVE — DA has 180d min storage; jumping there \
             at 90d would prematurely block retrieval)"
        );
        assert_eq!(
            transitions[1]["Days"].as_i64(),
            Some(90),
            "rule `{id}` transition 1 must be at Day 90"
        );
        assert_eq!(
            transitions[2]["StorageClass"].as_str(),
            Some("DEEP_ARCHIVE"),
            "rule `{id}` transition 2 must be DEEP_ARCHIVE"
        );
        assert_eq!(
            transitions[2]["Days"].as_i64(),
            Some(365),
            "rule `{id}` transition 2 must be at Day 365"
        );
    }
}
