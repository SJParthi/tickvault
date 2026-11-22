//! No-rest rule §12.16 (owner card 2026-11-13, "Yes, connect-only
//! (Recommended)"): the order host pre-warm runs ONLY when
//! `[order_runtime] prewarm_order_connection` is true (default false).
//!
//! Its own test binary because the pre-warm's "spawned once per process"
//! flag is process-wide: no other test here may set it.
//!
//! 1. Behaviour: the order runtime's client builder with the switch OFF
//!    spawns no daily pre-warm task, proven by the next spawn attempt still
//!    being the process's first.
//! 2. Wiring: the order runtime builds its client only through that builder,
//!    passing the config switch, and calls neither pre-warm entry directly.
//!
//! Local only: the host is 127.0.0.1, so no test reaches a Dhan host.

use std::sync::Arc;

use figment::Figment;
use figment::providers::{Format, Toml};
use tickvault_app::order_prewarm::{
    order_runtime_http_client, order_tls_from, spawn_daily_order_prewarm,
};
use tickvault_common::config::ApplicationConfig;
use tickvault_common::trading_calendar::TradingCalendar;

fn calendar() -> Arc<TradingCalendar> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    let config: ApplicationConfig = Figment::new()
        .merge(Toml::file(path))
        .extract()
        .expect("config/base.toml must parse"); // APPROVED: test
    Arc::new(TradingCalendar::from_config(&config.trading).expect("calendar builds")) // APPROVED: test
}

#[tokio::test]
async fn test_switch_off_builds_the_plain_client_and_spawns_no_prewarm() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cal = calendar();
    let client = order_runtime_http_client(false, "https://127.0.0.1/v2", Arc::clone(&cal));
    assert!(client.is_ok(), "the plain order client builds");
    let base = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    assert!(
        spawn_daily_order_prewarm("127.0.0.1".to_string(), order_tls_from(&base), cal),
        "with the switch off, no pre-warm task was spawned before this one"
    );
}

#[test]
fn test_order_runtime_builds_its_client_through_the_switch() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/order_runtime.rs");
    let src = std::fs::read_to_string(path).expect("order_runtime.rs reads"); // APPROVED: test
    let flat: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(
            "order_prewarm::order_runtime_http_client( config.order_runtime.prewarm_order_connection,"
        ),
        "the order runtime must build its client through the switch"
    );
    for direct in [
        "prewarmed_oms_http_client",
        "spawn_daily_order_prewarm",
        "build_oms_http_client",
    ] {
        let calls = flat
            .match_indices(direct)
            .filter(|(i, _)| flat[i + direct.len()..].starts_with('('))
            .count();
        assert_eq!(
            calls, 0,
            "order_runtime.rs must not call `{direct}` directly"
        );
    }
}

/// The once-a-day bound is per PROCESS (the spawn flag is a process static;
/// nothing is persisted), on purpose: a process restarted the same day has
/// an empty TLS session store. The auto-loaded rule summary and the full
/// rule text must both say so, or a reviewer reading the summary would
/// REJECT the restart's second handshake the code is built to make.
#[test]
fn test_rule_summary_and_full_text_both_bound_the_prewarm_per_process() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let squeeze = |rel: &str| -> String {
        std::fs::read_to_string(root.join(rel))
            .unwrap_or_else(|e| panic!("read {rel}: {e}")) // APPROVED: test
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let stub = squeeze(".claude/rules/project/no-rest-except-live-feed-2026-06-27.md");
    let full = squeeze("docs/claude-rules-full/project/no-rest-except-live-feed-2026-06-27.md");
    let stub_1216 = stub
        .split("**§12.16 (2026-11-13")
        .nth(1)
        .unwrap_or_else(|| panic!("the summary stub lost its §12.16 line")); // APPROVED: test
    let stub_1216 = stub_1216
        .split("**REJECT (headlines")
        .next()
        .unwrap_or(stub_1216);
    assert!(
        stub_1216.contains("more than once a day per process"),
        "the summary stub's §12.16 REJECT must bound the pre-warm per process, as the code does"
    );
    assert!(
        full.contains("more than once a day per process"),
        "the full §12.16.3 REJECT must bound the pre-warm per process"
    );
    assert!(
        full.contains("once per IST day per process"),
        "the full §12.16.1 'When' row must say once per IST day per process"
    );
    let code = squeeze("crates/app/src/order_prewarm.rs");
    assert!(
        code.contains("static PREWARM_SPAWNED: AtomicBool"),
        "the once-per-process flag is a process static; if a per-day marker replaces it, \
         update the rule text and this test together"
    );
}
