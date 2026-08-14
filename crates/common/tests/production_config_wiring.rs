//! Single-prod-env lockdown test for config/production.toml (operator 2026-06-30).
//!
//! Replaces the retired `staging_config_wiring.rs` (config/staging.toml was
//! deleted when dev/staging were collapsed into the single `prod` env).
//!
//! Ensures the production profile exists, parses cleanly, and — the HARD safety
//! constraint the operator has insisted on dozens of times — locks
//! `dry_run = true` (NO real orders) with a far-future `sandbox_only_until`
//! belt-and-suspenders so it can never accidentally promote to live trading.
//! This is still the no-real-orders data-pull phase.

use std::path::Path;

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

#[test]
fn test_production_config_exists_and_parses() {
    let path = workspace_root().join("config").join("production.toml");
    assert!(
        path.exists(),
        "config/production.toml missing — the single real env config is gone"
    );
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("production.toml not readable: {e}")); // APPROVED: test
    assert!(
        content.len() > 100,
        "production.toml suspiciously short ({} bytes)",
        content.len()
    );
}

#[test]
fn test_production_locks_dry_run_no_real_orders() {
    let content = std::fs::read_to_string(workspace_root().join("config").join("production.toml"))
        .expect("production.toml must be readable"); // APPROVED: test

    // HARD SAFETY: dry_run MUST be true — this is the no-real-orders data-pull
    // phase. A build-failing assertion so dry_run can never silently flip back
    // to false (which would arm real order placement).
    assert!(
        content.contains("dry_run = true"),
        "PROD LOCKDOWN: production.toml MUST set dry_run = true (NO real orders)"
    );
    assert!(
        !content.contains("dry_run = false"),
        "PROD LOCKDOWN: production.toml MUST NOT set dry_run = false — that arms REAL orders"
    );
    // mode MUST never be live in this phase.
    assert!(
        !content.contains("mode = \"live\""),
        "PROD LOCKDOWN: mode = live is forbidden in production.toml (data-pull phase)"
    );
    // Belt-and-suspenders: a far-future sandbox_only_until makes accidental
    // live promotion mechanically impossible at boot even if mode is misedited.
    assert!(
        content.contains("2099-12-31"),
        "PROD LOCKDOWN: sandbox_only_until must be 2099-12-31 to prevent accidental \
         Live promotion (belt-and-suspenders defense-in-depth)"
    );
}

#[test]
fn test_production_rest_only_no_live_feeds() {
    // Operator directive 2026-07-15 (relayed via the coordinator session):
    // "remove the whole Groww live feed; keep only spot 1m and option chain
    // for both brokers" — the runtime is REST-ONLY for BOTH brokers.
    //
    // SUPERSEDES the 2026-07-13 Groww-is-the-live-feed pin (the renamed
    // `test_production_groww_live_dhan_rest_only`), which itself SUPERSEDED
    // the 2026-06-30 both-feeds lock (the retired
    // `test_production_runs_both_feeds`). The 2026-07-13 Dhan half stands:
    // the Dhan live WS lane is OFF; the Dhan REST retained surface
    // (token/auth stack, spot_1m_rest, option_chain_1m + entitlement probe,
    // REST canary) runs WITHOUT the WS lane via
    // crates/app/src/dhan_rest_stack.rs. Phase A is a config flip +
    // additive bootstrap — flipping dhan_enabled back to true requires a
    // fresh dated operator quote HERE first.
    // 2026-08-11: comment lines are stripped before every needle below. These
    // assertions were raw `contains` on the whole file, so the dated rationale
    // comments this flip added — which necessarily quote BOTH `dhan_enabled =
    // true` and `dhan_enabled = false` while explaining the change — would have
    // satisfied and violated the pins at the same time. A guard that reads
    // prose is a guard that can be talked out of its own assertion.
    let strip_toml_comments = |src: &str| {
        src.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let prod = strip_toml_comments(
        &std::fs::read_to_string(workspace_root().join("config").join("production.toml"))
            .expect("production.toml must be readable"), // APPROVED: test
    );
    // INVERTED 2026-08-11 — see the same-dated inversion note in
    // crates/app/tests/dhan_live_off_phase_a_guard.rs. The operator quote in
    // websocket-connection-scope-lock.md ("2026-08-11 — THE DEFAULT IS FLIPPED
    // ON") is the "fresh dated operator quote HERE first" the paragraph above
    // demands. The pin is kept and reversed, not removed: OFF is now the state
    // that must never land unnoticed, because it silently stops live capture
    // while looking identical to a healthy REST-only boot.
    assert!(
        prod.contains("dhan_enabled = true"),
        "production.toml must ENABLE the Dhan live WS feed (operator quote \
         2026-08-11, recorded in websocket-connection-scope-lock.md)"
    );
    assert!(
        !prod.contains("dhan_enabled = false"),
        "production.toml must NOT disable the Dhan live WS feed — reverting \
         needs a fresh dated operator quote + this guard updated in the same PR"
    );
    assert!(
        prod.contains("groww_enabled = false"),
        "production.toml must keep the Groww live feed disabled — the runtime \
         is REST-only (operator directive 2026-07-15: remove the whole Groww \
         live feed; keep only spot 1m and option chain for both brokers)"
    );
    assert!(
        !prod.contains("groww_enabled = true"),
        "production.toml must not re-enable the retired Groww live feed \
         (operator directive 2026-07-15)"
    );

    // base.toml carries the same Dhan setting so a TV_ENVIRONMENT=dev/local
    // boot (base-only merge) agrees with prod rather than diverging silently.
    //
    // INVERTED 2026-08-11 alongside the prod pin above. Note the second gate:
    // `spawn_dhan_feed_stack` ALSO requires TICKVAULT_DHAN_LIVE_FEED=1, which
    // is set only in deploy/systemd/tickvault.service — so base.toml saying
    // `true` does NOT put sockets into a developer's `cargo run`.
    let base = strip_toml_comments(
        &std::fs::read_to_string(workspace_root().join("config").join("base.toml"))
            .expect("base.toml must be readable"), // APPROVED: test
    );
    assert!(
        base.contains("dhan_enabled = true"),
        "base.toml must ENABLE the Dhan live WS feed (operator quote 2026-08-11)"
    );
    assert!(
        !base.contains("dhan_enabled = false"),
        "base.toml must NOT disable the Dhan live WS feed — reverting needs a \
         fresh dated operator quote + this guard updated in the same PR"
    );

    // The second gate, pinned so the flip cannot be half-applied. A config
    // saying `true` with no env opt-in on the box is a lane that reports
    // enabled and opens nothing — the false-OK shape this repo's rule 11
    // forbids, and the exact failure a config-only flip would produce.
    let unit = std::fs::read_to_string(
        workspace_root()
            .join("deploy")
            .join("systemd")
            .join("tickvault.service"),
    )
    .expect("tickvault.service must be readable"); // APPROVED: test
    assert!(
        unit.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .any(|l| l.trim() == "Environment=TICKVAULT_DHAN_LIVE_FEED=1"),
        "deploy/systemd/tickvault.service must set \
         Environment=TICKVAULT_DHAN_LIVE_FEED=1 — it is the SECOND of the two \
         gates on the live lane, and without it `dhan_enabled = true` opens no \
         socket at all while every config surface reports the feed as enabled"
    );
}

#[test]
fn test_base_config_sandbox_only_until_is_2099_sentinel() {
    // Refuter round 1 (2026-07-14, LOW): production.toml's 2099-12-31 pin
    // above left config/base.toml UN-pinned — the historical incident was
    // exactly base.toml's `sandbox_only_until = "2026-06-30"` EXPIRING
    // silently on 2026-07-01. A silent revert to a past date must fail the
    // BUILD, not just fire the runtime gate-4 boot warn. Comment-filtered
    // (base.toml carries prose comments mentioning the sentinel date, so a
    // bare `contains("2099-12-31")` would pass even after a revert).
    let base = std::fs::read_to_string(workspace_root().join("config").join("base.toml"))
        .expect("base.toml must be readable"); // APPROVED: test
    let assignments: Vec<&str> = base
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && l.contains("sandbox_only_until"))
        .collect();
    assert_eq!(
        assignments.len(),
        1,
        "base.toml must carry exactly ONE sandbox_only_until assignment; \
         found {assignments:?}"
    );
    assert!(
        assignments[0].starts_with("sandbox_only_until = \"2099-12-31\""),
        "BASE LOCKDOWN: base.toml must set sandbox_only_until = \"2099-12-31\" \
         (the far-future sentinel matching production.toml) — a past date \
         silently disarms the sandbox window on every base-merged boot \
         (the 2026-07-01 silent-expiry incident class); found: {}",
        assignments[0]
    );
}

#[test]
fn test_staging_and_dev_configs_are_retired() {
    // Single-prod-env consolidation (operator 2026-06-30): base.toml +
    // production.toml are the ONLY env configs. config/staging.toml and
    // config/dev.toml must not exist.
    assert!(
        !workspace_root()
            .join("config")
            .join("staging.toml")
            .exists(),
        "config/staging.toml must be retired (single prod env)"
    );
    assert!(
        !workspace_root().join("config").join("dev.toml").exists(),
        "there must be no config/dev.toml (single prod env)"
    );
}
