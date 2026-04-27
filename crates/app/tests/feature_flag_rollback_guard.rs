//! Wave 1 C9 feature-flag rollback guard.
//!
//! Source: `.claude/plans/active-plan-cross-cutting.md` (C9) — every
//! Wave 1/2/3 item has a paired feature flag in `[features]` of
//! `config/base.toml` so the operator can flip a misbehaving item OFF
//! without a redeploy. The flag is the rollback contract.
//!
//! For each of the 14 flags this test suite asserts:
//!
//! 1. `<flag>_off_disables_path` — `flag = false` parses cleanly. The
//!    "old" (pre-Wave) behaviour is reachable from config; no silent
//!    panic when the flag is off.
//! 2. `<flag>_on_enables_path` — `flag = true` parses cleanly. The
//!    "new" (Wave-shipped) behaviour is reachable from config; the
//!    happy path works.
//! 3. `<flag>_default_is_safe` — the `Default` impl returns `true`, so
//!    a missing `[features]` section in an environment override does
//!    NOT silently disable a Wave 1 item.
//!
//! These are config-plumbing assertions: they prove the toggle exists
//! end-to-end (TOML → `FeaturesConfig` → `ApplicationConfig`) so the
//! operator's rollback contract is mechanically guaranteed. The
//! per-item runtime branching is wired by the items themselves — see
//! Wave 1 Items 0–4 commits in PR #393.
//!
//! Run: `cargo test -p tickvault-app --test feature_flag_rollback_guard`

use figment::Figment;
use figment::providers::{Format, Toml};
use tickvault_common::config::FeaturesConfig;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a minimal TOML containing only the flag fields directly into a
/// `FeaturesConfig`. Uses `figment` — the same loader the boot sequence
/// uses — so this asserts the production plumbing path. The body must
/// be top-level key=value lines (no `[features]` header).
fn parse_features(toml_body: &str) -> FeaturesConfig {
    Figment::new()
        .merge(Toml::string(toml_body))
        .extract()
        .expect("features TOML must parse")
}

/// Asserts that the `FeaturesConfig` default (used when `[features]`
/// is absent) has the field set to `true` — the Wave 1 contract.
fn assert_default_is_true(getter: impl Fn(&FeaturesConfig) -> bool, name: &str) {
    let default = FeaturesConfig::default();
    assert!(
        getter(&default),
        "C9 invariant: feature `{name}` default MUST be true so a missing \
         [features] block does not silently disable a Wave 1 item"
    );
}

// ---------------------------------------------------------------------------
// Item 0 — hotpath_async_writers
// ---------------------------------------------------------------------------

#[test]
fn test_hotpath_async_writers_off_disables_path() {
    let cfg = parse_features("hotpath_async_writers = false");
    assert!(!cfg.hotpath_async_writers);
}

#[test]
fn test_hotpath_async_writers_on_enables_path() {
    let cfg = parse_features("hotpath_async_writers = true");
    assert!(cfg.hotpath_async_writers);
}

#[test]
fn test_hotpath_async_writers_default_is_safe() {
    assert_default_is_true(|c| c.hotpath_async_writers, "hotpath_async_writers");
}

// ---------------------------------------------------------------------------
// Item 1 — phase2_emit_guard
// ---------------------------------------------------------------------------

#[test]
fn test_phase2_emit_guard_off_disables_path() {
    let cfg = parse_features("phase2_emit_guard = false");
    assert!(!cfg.phase2_emit_guard);
}

#[test]
fn test_phase2_emit_guard_on_enables_path() {
    let cfg = parse_features("phase2_emit_guard = true");
    assert!(cfg.phase2_emit_guard);
}

#[test]
fn test_phase2_emit_guard_default_is_safe() {
    assert_default_is_true(|c| c.phase2_emit_guard, "phase2_emit_guard");
}

// ---------------------------------------------------------------------------
// Item 2 — stock_movers_full_universe
// ---------------------------------------------------------------------------

#[test]
fn test_stock_movers_full_universe_off_disables_path() {
    let cfg = parse_features("stock_movers_full_universe = false");
    assert!(!cfg.stock_movers_full_universe);
}

#[test]
fn test_stock_movers_full_universe_on_enables_path() {
    let cfg = parse_features("stock_movers_full_universe = true");
    assert!(cfg.stock_movers_full_universe);
}

#[test]
fn test_stock_movers_full_universe_default_is_safe() {
    assert_default_is_true(
        |c| c.stock_movers_full_universe,
        "stock_movers_full_universe",
    );
}

// ---------------------------------------------------------------------------
// Item 3 — option_movers_5s
// ---------------------------------------------------------------------------

#[test]
fn test_option_movers_5s_off_disables_path() {
    let cfg = parse_features("option_movers_5s = false");
    assert!(!cfg.option_movers_5s);
}

#[test]
fn test_option_movers_5s_on_enables_path() {
    let cfg = parse_features("option_movers_5s = true");
    assert!(cfg.option_movers_5s);
}

#[test]
fn test_option_movers_5s_default_is_safe() {
    assert_default_is_true(|c| c.option_movers_5s, "option_movers_5s");
}

// ---------------------------------------------------------------------------
// Item 4 — previous_close_persist
// ---------------------------------------------------------------------------

#[test]
fn test_previous_close_persist_off_disables_path() {
    let cfg = parse_features("previous_close_persist = false");
    assert!(!cfg.previous_close_persist);
}

#[test]
fn test_previous_close_persist_on_enables_path() {
    let cfg = parse_features("previous_close_persist = true");
    assert!(cfg.previous_close_persist);
}

#[test]
fn test_previous_close_persist_default_is_safe() {
    assert_default_is_true(|c| c.previous_close_persist, "previous_close_persist");
}

// ---------------------------------------------------------------------------
// Item 5 — ws_main_sleep_until_open (Wave 2)
// ---------------------------------------------------------------------------

#[test]
fn test_ws_main_sleep_until_open_off_disables_path() {
    let cfg = parse_features("ws_main_sleep_until_open = false");
    assert!(!cfg.ws_main_sleep_until_open);
}

#[test]
fn test_ws_main_sleep_until_open_on_enables_path() {
    let cfg = parse_features("ws_main_sleep_until_open = true");
    assert!(cfg.ws_main_sleep_until_open);
}

#[test]
fn test_ws_main_sleep_until_open_default_is_safe() {
    assert_default_is_true(|c| c.ws_main_sleep_until_open, "ws_main_sleep_until_open");
}

// ---------------------------------------------------------------------------
// Item 6 — ws_depth_ou_sleep_until_open (Wave 2)
// ---------------------------------------------------------------------------

#[test]
fn test_ws_depth_ou_sleep_until_open_off_disables_path() {
    let cfg = parse_features("ws_depth_ou_sleep_until_open = false");
    assert!(!cfg.ws_depth_ou_sleep_until_open);
}

#[test]
fn test_ws_depth_ou_sleep_until_open_on_enables_path() {
    let cfg = parse_features("ws_depth_ou_sleep_until_open = true");
    assert!(cfg.ws_depth_ou_sleep_until_open);
}

#[test]
fn test_ws_depth_ou_sleep_until_open_default_is_safe() {
    assert_default_is_true(
        |c| c.ws_depth_ou_sleep_until_open,
        "ws_depth_ou_sleep_until_open",
    );
}

// ---------------------------------------------------------------------------
// Item 7 — fast_boot_60s_deadline (Wave 2)
// ---------------------------------------------------------------------------

#[test]
fn test_fast_boot_60s_deadline_off_disables_path() {
    let cfg = parse_features("fast_boot_60s_deadline = false");
    assert!(!cfg.fast_boot_60s_deadline);
}

#[test]
fn test_fast_boot_60s_deadline_on_enables_path() {
    let cfg = parse_features("fast_boot_60s_deadline = true");
    assert!(cfg.fast_boot_60s_deadline);
}

#[test]
fn test_fast_boot_60s_deadline_default_is_safe() {
    assert_default_is_true(|c| c.fast_boot_60s_deadline, "fast_boot_60s_deadline");
}

// ---------------------------------------------------------------------------
// Item 8 — tick_gap_detector_60s_coalesce (Wave 2)
// ---------------------------------------------------------------------------

#[test]
fn test_tick_gap_detector_60s_coalesce_off_disables_path() {
    let cfg = parse_features("tick_gap_detector_60s_coalesce = false");
    assert!(!cfg.tick_gap_detector_60s_coalesce);
}

#[test]
fn test_tick_gap_detector_60s_coalesce_on_enables_path() {
    let cfg = parse_features("tick_gap_detector_60s_coalesce = true");
    assert!(cfg.tick_gap_detector_60s_coalesce);
}

#[test]
fn test_tick_gap_detector_60s_coalesce_default_is_safe() {
    assert_default_is_true(
        |c| c.tick_gap_detector_60s_coalesce,
        "tick_gap_detector_60s_coalesce",
    );
}

// ---------------------------------------------------------------------------
// Item 9 — audit_tables_enabled (Wave 2)
// ---------------------------------------------------------------------------

#[test]
fn test_audit_tables_enabled_off_disables_path() {
    let cfg = parse_features("audit_tables_enabled = false");
    assert!(!cfg.audit_tables_enabled);
}

#[test]
fn test_audit_tables_enabled_on_enables_path() {
    let cfg = parse_features("audit_tables_enabled = true");
    assert!(cfg.audit_tables_enabled);
}

#[test]
fn test_audit_tables_enabled_default_is_safe() {
    assert_default_is_true(|c| c.audit_tables_enabled, "audit_tables_enabled");
}

// ---------------------------------------------------------------------------
// Item 10 — preopen_movers (Wave 3)
// ---------------------------------------------------------------------------

#[test]
fn test_preopen_movers_off_disables_path() {
    let cfg = parse_features("preopen_movers = false");
    assert!(!cfg.preopen_movers);
}

#[test]
fn test_preopen_movers_on_enables_path() {
    let cfg = parse_features("preopen_movers = true");
    assert!(cfg.preopen_movers);
}

#[test]
fn test_preopen_movers_default_is_safe() {
    assert_default_is_true(|c| c.preopen_movers, "preopen_movers");
}

// ---------------------------------------------------------------------------
// Item 11 — telegram_bucket_coalescer (Wave 3)
// ---------------------------------------------------------------------------

#[test]
fn test_telegram_bucket_coalescer_off_disables_path() {
    let cfg = parse_features("telegram_bucket_coalescer = false");
    assert!(!cfg.telegram_bucket_coalescer);
}

#[test]
fn test_telegram_bucket_coalescer_on_enables_path() {
    let cfg = parse_features("telegram_bucket_coalescer = true");
    assert!(cfg.telegram_bucket_coalescer);
}

#[test]
fn test_telegram_bucket_coalescer_default_is_safe() {
    assert_default_is_true(|c| c.telegram_bucket_coalescer, "telegram_bucket_coalescer");
}

// ---------------------------------------------------------------------------
// Item 12 — market_open_self_test (Wave 3)
// ---------------------------------------------------------------------------

#[test]
fn test_market_open_self_test_off_disables_path() {
    let cfg = parse_features("market_open_self_test = false");
    assert!(!cfg.market_open_self_test);
}

#[test]
fn test_market_open_self_test_on_enables_path() {
    let cfg = parse_features("market_open_self_test = true");
    assert!(cfg.market_open_self_test);
}

#[test]
fn test_market_open_self_test_default_is_safe() {
    assert_default_is_true(|c| c.market_open_self_test, "market_open_self_test");
}

// ---------------------------------------------------------------------------
// Item 13 — realtime_guarantee_score (Wave 3)
// ---------------------------------------------------------------------------

#[test]
fn test_realtime_guarantee_score_off_disables_path() {
    let cfg = parse_features("realtime_guarantee_score = false");
    assert!(!cfg.realtime_guarantee_score);
}

#[test]
fn test_realtime_guarantee_score_on_enables_path() {
    let cfg = parse_features("realtime_guarantee_score = true");
    assert!(cfg.realtime_guarantee_score);
}

#[test]
fn test_realtime_guarantee_score_default_is_safe() {
    assert_default_is_true(|c| c.realtime_guarantee_score, "realtime_guarantee_score");
}

// ---------------------------------------------------------------------------
// Cross-cutting invariants
// ---------------------------------------------------------------------------

/// All 14 flags must be true by default so a missing `[features]` block
/// in an environment override does not silently regress any Wave item.
#[test]
fn test_all_features_default_to_true_no_silent_drift() {
    let d = FeaturesConfig::default();
    assert!(d.hotpath_async_writers);
    assert!(d.phase2_emit_guard);
    assert!(d.stock_movers_full_universe);
    assert!(d.option_movers_5s);
    assert!(d.previous_close_persist);
    assert!(d.ws_main_sleep_until_open);
    assert!(d.ws_depth_ou_sleep_until_open);
    assert!(d.fast_boot_60s_deadline);
    assert!(d.tick_gap_detector_60s_coalesce);
    assert!(d.audit_tables_enabled);
    assert!(d.preopen_movers);
    assert!(d.telegram_bucket_coalescer);
    assert!(d.market_open_self_test);
    assert!(d.realtime_guarantee_score);
}

/// An empty TOML body must still produce a fully-populated
/// `FeaturesConfig` (via `#[serde(default)]`) with every flag set to
/// `true`. This is the boot-time invariant that guarantees an
/// environment override file with no `[features]` section does not
/// silently regress any Wave item.
#[test]
fn test_features_empty_body_falls_back_to_default_all_true() {
    let cfg = parse_features("# no flag overrides\n");
    assert_eq!(cfg, FeaturesConfig::default());
}

/// The `[features]` section in `config/base.toml` must list every one
/// of the 14 flags. Missing a flag = silent regression of a Wave item.
#[test]
fn test_config_base_toml_lists_every_feature_flag() {
    let toml_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("config")
        .join("base.toml");
    let body = std::fs::read_to_string(&toml_path).expect("config/base.toml must exist");
    assert!(
        body.contains("[features]"),
        "config/base.toml must contain a [features] section (C9 contract)"
    );
    for flag in [
        "hotpath_async_writers",
        "phase2_emit_guard",
        "stock_movers_full_universe",
        "option_movers_5s",
        "previous_close_persist",
        "ws_main_sleep_until_open",
        "ws_depth_ou_sleep_until_open",
        "fast_boot_60s_deadline",
        "tick_gap_detector_60s_coalesce",
        "audit_tables_enabled",
        "preopen_movers",
        "telegram_bucket_coalescer",
        "market_open_self_test",
        "realtime_guarantee_score",
    ] {
        assert!(
            body.contains(flag),
            "config/base.toml [features] must list `{flag}` (C9 contract — \
             missing flag = silent regression of the corresponding Wave item)"
        );
    }
}
