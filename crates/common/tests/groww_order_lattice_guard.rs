//! Ratchet: the Groww order-side 4-gate live-fire lattice (operator
//! authorization 2026-07-14 —
//! `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §39,
//! `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md` §10).
//!
//! Zero Groww order-side request may fire until the operator's explicit,
//! future, DATED live-orders enable aligns all four gates:
//!   Gate 1 — `[groww_orders]` config, every key default-OFF.
//!   Gate 2 — the NON-DEFAULT `groww_orders` cargo feature (BOTH manifests).
//!   Gate 3 — the hardcoded `GROWW_ORDER_LIVE_FIRE = false` source const.
//!   Gate 4 — the rule lock (§39 / §10 present + naming this guard).
//! Plus a workspace scan (Gate 5) proving NO ungated Groww order-side call
//! site exists outside the sanctioned `oms/groww/` subtree.
//!
//! Any PR that weakens a gate is a BUILD FAILURE here, not a review comment.

use std::fs;
use std::path::{Path, PathBuf};

use tickvault_common::config::GrowwOrdersConfig;
use tickvault_common::constants::GROWW_ORDER_LIVE_FIRE;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

/// Recursively collect every production `.rs` file under `dir` (skips test
/// trees + target — THIS guard names the banned strings in its own asserts).
fn rust_sources_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or(""); // APPROVED: test
            if name == "tests" || name == "target" {
                continue;
            }
            rust_sources_under(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// GATE 1 — config default-OFF
// ---------------------------------------------------------------------------

#[test]
fn test_gate1_groww_orders_config_defaults_all_off() {
    let cfg = GrowwOrdersConfig::default();
    assert!(!cfg.orders_read, "orders_read must default off (Gate 1)");
    assert!(
        !cfg.portfolio_read,
        "portfolio_read must default off (Gate 1)"
    );
    assert!(!cfg.margin_read, "margin_read must default off (Gate 1)");
    assert!(!cfg.user_read, "user_read must default off (Gate 1)");
    assert!(
        !cfg.live_fire_requested,
        "live_fire_requested must default off — and is inert without Gate 3"
    );
}

#[test]
fn test_gate1_base_toml_groww_orders_keys_all_false() {
    let toml = read("config/base.toml");
    assert!(
        toml.contains("[groww_orders]"),
        "config/base.toml must ship the [groww_orders] section (Gate 1)."
    );
    // Every documented key must be explicitly present and false. A `= true`
    // on any groww_orders key would flip a gate open in the shipped config.
    for key in [
        "orders_read",
        "portfolio_read",
        "margin_read",
        "user_read",
        "live_fire_requested",
    ] {
        let false_line = format!("{key} = false");
        let true_line = format!("{key} = true");
        assert!(
            toml.contains(&false_line),
            "config/base.toml [groww_orders] must set `{false_line}` (Gate 1)."
        );
        assert!(
            !toml.contains(&true_line),
            "config/base.toml must NOT set `{true_line}` — Gate 1 ships dark."
        );
    }
}

// ---------------------------------------------------------------------------
// GATE 2 — the non-default `groww_orders` cargo feature (BOTH manifests)
// ---------------------------------------------------------------------------

#[test]
fn test_gate2_groww_orders_feature_present_and_never_default() {
    for manifest in ["crates/common/Cargo.toml", "crates/trading/Cargo.toml"] {
        let body = read(manifest);
        assert!(
            body.contains("[features]"),
            "{manifest} must declare a [features] section (Gate 2)."
        );
        assert!(
            body.contains("groww_orders = []"),
            "{manifest} must declare the `groww_orders = []` feature (Gate 2)."
        );
        // The feature must NEVER be enabled by default: no `default = [...]`
        // line may name it (a default build must exclude all Groww order code).
        for line in body.lines() {
            let t = line.trim_start();
            if t.starts_with("default") && t.contains('=') {
                assert!(
                    !t.contains("groww_orders"),
                    "{manifest}: `groww_orders` must not be in a default feature \
                     set — a default build must exclude the Groww order-side."
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GATE 3 — the hardcoded live-fire const
// ---------------------------------------------------------------------------

#[test]
fn test_gate3_live_fire_const_is_false() {
    assert!(
        !GROWW_ORDER_LIVE_FIRE,
        "GROWW_ORDER_LIVE_FIRE must stay false until a dated operator \
         live-orders enable (Gate 3, §39.2)."
    );
    // Source-literal scan: the declaration must read `= false`, so a value
    // computed at runtime cannot slip past the compile-time constant.
    let constants = read("crates/common/src/constants.rs");
    assert!(
        constants.contains("pub const GROWW_ORDER_LIVE_FIRE: bool = false;"),
        "constants.rs must declare `pub const GROWW_ORDER_LIVE_FIRE: bool = false;` \
         verbatim (Gate 3)."
    );
}

// ---------------------------------------------------------------------------
// GATE 4 — the rule lock
// ---------------------------------------------------------------------------

#[test]
fn test_gate4_rule_files_carry_the_lattice_markers() {
    let groww_scope = read(".claude/rules/project/groww-second-feed-scope-2026-06-19.md");
    let no_rest = read(".claude/rules/project/no-rest-except-live-feed-2026-06-27.md");

    assert!(
        groww_scope.contains("§39"),
        "groww-second-feed-scope must carry the §39 order-side grant (Gate 4)."
    );
    assert!(
        no_rest.contains("§10"),
        "no-rest-except-live-feed must carry the §10 order-side grant (Gate 4)."
    );
    // The lattice must be named + this guard cross-referenced, so the ratchet
    // cannot be silently deleted (the cross-ref discipline).
    assert!(
        groww_scope.contains("GROWW_ORDER_LIVE_FIRE"),
        "§39 must name the GROWW_ORDER_LIVE_FIRE const (Gate 4)."
    );
    assert!(
        groww_scope.contains("groww_order_lattice_guard.rs"),
        "§39 must reference this guard file (Gate 4 anti-deletion)."
    );
}

// ---------------------------------------------------------------------------
// GATE 5 — no ungated Groww order-side call site anywhere in crates/*/src
// ---------------------------------------------------------------------------

#[test]
fn test_gate5_no_ungated_groww_order_side_call_site() {
    // Groww order-side REST paths + order/position update WS subjects. These
    // may appear ONLY inside the sanctioned `oms/groww/` subtree (Gate 2
    // feature-gated). Anywhere else = an ungated order surface = FAIL.
    // (Groww spot/chain use /v1/historical/candles + /v1/option-chain — NOT
    // matched here; Dhan orders use /v2/* — NOT matched here.)
    let banned = [
        "/v1/order/create",
        "/v1/order/modify",
        "/v1/order/cancel",
        "/v1/order/list",
        "/v1/order/detail",
        "/v1/order/status",
        "/v1/order/trades",
        "/v1/order-advance",
        "/v1/margins/detail",
        "/v1/holdings/user",
        "/v1/positions/user",
        "/v1/positions/trading-symbol",
        "/v1/user/detail",
        "subscribe_equity_order_updates",
        "subscribe_fno_order_updates",
        "subscribe_fno_position_updates",
    ];
    let mut sources = Vec::new();
    rust_sources_under(&workspace_root().join("crates"), &mut sources);
    assert!(
        sources.len() > 100,
        "sanity: expected the workspace's production sources, got {}",
        sources.len()
    );
    for path in &sources {
        // Sanctioned area: the feature-gated Groww OMS subtree is where these
        // paths are ALLOWED to appear (behind Gates 1-4).
        let p = path.to_string_lossy().replace('\\', "/");
        if p.contains("/oms/groww/") {
            continue;
        }
        let body = fs::read_to_string(path).unwrap_or_default();
        for needle in &banned {
            assert!(
                !body.contains(needle),
                "Groww order-side lattice VIOLATED: ungated call-site string \
                 `{needle}` found in {} — Groww order-side paths may live ONLY \
                 in crates/trading/src/oms/groww/ (feature `groww_orders`), \
                 behind the 4-gate live-fire lattice (§39.2).",
                path.display()
            );
        }
    }
}
