//! Audit Finding #9 (2026-05-03): the portal browser auto-open at boot
//! must be gated on a config flag so AWS / headless deployments can
//! disable the spurious shell-out.
//!
//! This test pins the default value (`true`, preserving local-dev
//! experience) and the gate behaviour (when `false`, no shell-out).

use tickvault_common::config::{ApiConfig, default_auto_open_portal};

#[test]
fn auto_open_portal_default_is_true_on_dev() {
    // The default helper used by `#[serde(default = ...)]` returns true
    // so a fresh `make run` on a developer Mac still pops the browser.
    // Audit Finding #9.
    assert!(
        default_auto_open_portal(),
        "default auto_open_portal must be `true` so a fresh `make run` \
         on a developer Mac still pops the browser. Audit Finding #9."
    );
}

#[test]
fn auto_open_portal_can_be_disabled() {
    // Sanity: the flag is a plain bool — flipping to false works.
    let cfg = ApiConfig {
        host: "0.0.0.0".to_string(),
        port: 3001,
        allowed_origins: vec![],
        auto_open_portal: false,
    };
    assert!(
        !cfg.auto_open_portal,
        "auto_open_portal must be settable to false for headless / AWS \
         deployments. Audit Finding #9."
    );
}

#[test]
fn portal_auto_open_grep_guard_in_main() {
    // Pin the gate site in main.rs. If a future refactor removes the
    // `if cfg.api.auto_open_portal {` guard, this test fails — catching
    // the regression before it ships to AWS.
    let main = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../app/src/main.rs"
    ))
    .expect("crates/app/src/main.rs must exist");
    assert!(
        main.contains("cfg.api.auto_open_portal"),
        "main.rs must gate the portal browser-open on \
         `cfg.api.auto_open_portal`. Audit Finding #9 — without this \
         gate, AWS boots will spam logs trying to xdg-open a missing \
         browser."
    );
    assert!(
        main.contains("auto_open_portal = false"),
        "main.rs must reference the disabled state in its log message \
         so operators searching for `auto_open_portal = false` find \
         the gate site immediately."
    );
}

#[test]
fn config_base_toml_documents_the_flag() {
    let toml_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../config/base.toml"
    );
    let toml = std::fs::read_to_string(toml_path).expect("config/base.toml must exist");
    assert!(
        toml.contains("auto_open_portal"),
        "config/base.toml must document the auto_open_portal flag \
         under [api]. Audit Finding #9."
    );
    assert!(
        toml.contains("AWS / headless"),
        "config/base.toml must explain WHY the flag exists (AWS / \
         headless deployments) so operators know when to flip it."
    );
}
