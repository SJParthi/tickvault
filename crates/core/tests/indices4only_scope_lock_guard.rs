// PR #7b (AWS-lifecycle) — source-scan ratchet pinning the
// `SubscriptionScope::Indices4Only` LOCK per
// `.claude/rules/project/websocket-connection-scope-lock.md` +
// operator-charter §I (2026-05-15 lock).
//
// This test fails the build if any of the retired identifiers reappear
// in the `crates/` tree (excluding doc comments, archived plan files,
// and this guard file itself).
//
// Slice 6 deliverable per `.claude/plans/active-plan-pr-7-subscription-scope-lock.md`.

use std::fs;
use std::path::{Path, PathBuf};

const RETIRED_VARIANTS: &[&str] = &[
    "SubscriptionScope::FullUniverse",
    "SubscriptionScope::IndicesOnlyAllExpiries",
    "SubscriptionScope::IndicesUnderlyingsOnly",
];

const RETIRED_FLAGS: &[&str] = &[
    "subscribe_index_derivatives",
    "subscribe_stock_derivatives",
    "subscribe_display_indices",
];

/// Files (relative to repo root) where retired identifiers are
/// permitted because they appear inside a doc comment, retired-test
/// cfg(any()) body, or archived test allowlist. Keep this list as
/// small as possible.
const ALLOWLIST_PATHS: &[&str] = &[
    // The planner has #[cfg(any())]-retired test fns that still
    // mention retired identifiers in their disabled bodies. They
    // don't compile, but the source-scan still sees the strings.
    "crates/core/src/instrument/subscription_planner.rs",
    // The SubscriptionScope enum + retired-test comments live in
    // config.rs and reference the legacy variant names in doc
    // comments (no live code uses them).
    "crates/common/src/config.rs",
    // This guard file itself contains the retired identifiers as
    // string literals.
    "crates/core/tests/indices4only_scope_lock_guard.rs",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn walk_rust_files(root: &Path) -> Vec<PathBuf> {
    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            // Skip build/target dirs + hidden + node_modules-like
            if name == "target" || name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                collect(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    collect(&root.join("crates"), &mut out);
    out
}

#[test]
fn indices4only_lock_no_retired_variant_in_crates() {
    let root = repo_root();
    let mut hits: Vec<String> = Vec::new();
    for path in walk_rust_files(&root) {
        let rel = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        if ALLOWLIST_PATHS.iter().any(|p| rel == *p) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for needle in RETIRED_VARIANTS {
            if content.contains(needle) {
                hits.push(format!("{rel}: contains '{needle}'"));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "AWS-lifecycle PR #7 LOCK violation — retired SubscriptionScope variants found:\n  {}\n\nSee .claude/rules/project/websocket-connection-scope-lock.md",
        hits.join("\n  ")
    );
}

#[test]
fn indices4only_lock_no_retired_flag_in_crates() {
    let root = repo_root();
    let mut hits: Vec<String> = Vec::new();
    for path in walk_rust_files(&root) {
        let rel = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        if ALLOWLIST_PATHS.iter().any(|p| rel == *p) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for needle in RETIRED_FLAGS {
            if content.contains(needle) {
                hits.push(format!("{rel}: contains '{needle}'"));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "AWS-lifecycle PR #7 LOCK violation — retired SubscriptionConfig flags found:\n  {}",
        hits.join("\n  ")
    );
}

/// Hotfix regression test (2026-05-19): `FnoUniverse::locked_4_idx_i()`
/// fed through the planner MUST emit exactly the 4 LOCKED_UNIVERSE SIDs
/// (NIFTY=13 + BANKNIFTY=25 + SENSEX=51 as MajorIndexValue, INDIA VIX=21
/// as DisplayIndex). Before the hotfix the planner emitted only INDIA VIX
/// (total=1) because the 3 majors live in `subscribed_indices` with
/// `category=FnoUnderlying`, not in the empty `underlyings` HashMap.
#[test]
fn hotfix_locked_universe_planner_emits_all_4_idx_i_sids() {
    use chrono::NaiveDate;
    use std::collections::HashMap;
    use tickvault_common::config::SubscriptionConfig;
    use tickvault_common::instrument_types::FnoUniverse;
    use tickvault_core::instrument::subscription_planner::build_subscription_plan;

    let universe = FnoUniverse::locked_4_idx_i();
    let cfg = SubscriptionConfig::default();
    let today = NaiveDate::from_ymd_opt(2026, 5, 19).unwrap();
    let plan = build_subscription_plan(&universe, &cfg, today, &HashMap::new(), None);

    let emitted: std::collections::HashSet<u64> =
        plan.registry.iter().map(|i| i.security_id).collect();

    for sid in [13_u64, 25, 51, 21] {
        assert!(
            emitted.contains(&sid),
            "LOCKED_UNIVERSE SID {sid} missing from plan — got {emitted:?}; \
             total={}, major_index_values={}, display_indices={}",
            plan.summary.total,
            plan.summary.major_index_values,
            plan.summary.display_indices,
        );
    }
    assert_eq!(
        plan.summary.total, 4,
        "LOCK: planner must emit exactly 4 SIDs from LOCKED_UNIVERSE"
    );
    assert_eq!(
        plan.summary.major_index_values, 3,
        "LOCK: NIFTY + BANKNIFTY + SENSEX must be MajorIndexValue"
    );
    assert_eq!(
        plan.summary.display_indices, 1,
        "LOCK: INDIA VIX must be the only DisplayIndex"
    );
}

#[test]
fn indices4only_lock_planner_emits_4_idx_i_only() {
    // Verify the planner's helper functions return the LOCKED contract
    // for the only legal scope.
    use tickvault_common::config::SubscriptionConfig;
    use tickvault_core::instrument::subscription_planner::{
        is_display_index_allowed_under_scope, should_subscribe_index_derivatives,
        should_subscribe_stock_derivatives,
    };
    let cfg = SubscriptionConfig::default();
    assert!(
        !should_subscribe_stock_derivatives(&cfg),
        "LOCK: stock derivatives must never be subscribed"
    );
    assert!(
        !should_subscribe_index_derivatives(&cfg),
        "LOCK: index derivatives must never be subscribed"
    );
    // INDIA VIX (SID 21) is the only allowed display index.
    assert!(is_display_index_allowed_under_scope(
        &cfg,
        tickvault_common::constants::INDIA_VIX_SECURITY_ID
    ));
    for stray_sid in [14_u64, 17, 19, 23] {
        assert!(
            !is_display_index_allowed_under_scope(&cfg, stray_sid),
            "LOCK: display index SID {stray_sid} must be PARKED"
        );
    }
}
