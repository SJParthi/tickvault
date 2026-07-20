//! Ensure-DDL boot wiring guard (Track A, 2026-07-18).
//!
//! The PR-C2 (#1522) Dhan-lane deletion and the #1581 Groww live-feed
//! deletion removed the ONLY call sites of the candle-table DDL chain
//! (`drop_legacy_candle_objects` → `ensure_shadow_candle_tables` →
//! `ensure_named_views`) while the REST-era bar-fold KEPT writing the
//! `candles_*` tables — so a fresh QuestDB volume auto-created them via
//! ILP WITHOUT `DEDUP ENABLE UPSERT KEYS` (silent duplicate-row window).
//!
//! This build-failing source-scan pins the re-wire so a future refactor
//! cannot silently re-open the class:
//!
//! 1. `build_shared_infra` in main.rs awaits
//!    `candle_ddl_boot::run_candle_ddl_at_boot` BEFORE the
//!    `spawn_seal_writer_loop` call (DDL-before-first-ILP ordering).
//! 2. `candle_ddl_boot.rs` actually calls the three storage DDL fns in
//!    the load-bearing drop → ensure → views order (stub-guard).
//! 3. Every LIVE-writer table's ensure fn keeps ≥1 production call site
//!    (the writer → table → ensure → boot-call-site coverage table).

use std::path::Path;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read_src(rel: &str) -> String {
    let path = manifest_dir().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Production region = everything above the first column-0 `#[cfg(test)]`
/// line (the house source-scan convention), so test-module mentions can
/// never satisfy a production pin.
fn production_region(src: &str) -> String {
    match src.find("\n#[cfg(test)]") {
        Some(idx) => src[..idx].to_string(),
        None => src.to_string(),
    }
}

#[test]
fn test_candle_ddl_boot_awaited_before_seal_writer_spawn_in_main() {
    let main_src = production_region(&read_src("src/main.rs"));

    let ddl_call = "candle_ddl_boot::run_candle_ddl_at_boot(&config.questdb).await";
    let ddl_pos = main_src.find(ddl_call).unwrap_or_else(|| {
        panic!(
            "main.rs production region must await `{ddl_call}` inside \
             build_shared_infra — the fresh-volume no-DEDUP fix (Track A 2026-07-18)"
        )
    });
    assert_eq!(
        main_src.matches(ddl_call).count(),
        1,
        "exactly ONE candle-DDL boot await site (double execution would \
         double the 60s down-QuestDB probe stall)"
    );

    // The CALL site (not the fn definition) — the argument form is unique
    // to the build_shared_infra call.
    let seal_call = "spawn_seal_writer_loop(&config.questdb);";
    let seal_pos = main_src.find(seal_call).unwrap_or_else(|| {
        panic!("main.rs production region must call `{seal_call}` in build_shared_infra")
    });

    assert!(
        ddl_pos < seal_pos,
        "run_candle_ddl_at_boot must be awaited BEFORE spawn_seal_writer_loop \
         installs the global seal sender — the candle DDL (with DEDUP) must \
         land before the first fold seal can reach ILP"
    );
}

#[test]
fn test_candle_ddl_boot_calls_drop_ensure_views_in_order() {
    let boot_src = production_region(&read_src("src/candle_ddl_boot.rs"));

    let drop_pos = boot_src
        .find("shadow_persistence::drop_legacy_candle_objects(questdb).await")
        .expect("candle_ddl_boot must run the retired-object drop sweep");
    let ensure_pos = boot_src
        .find("shadow_persistence::ensure_shadow_candle_tables(questdb).await")
        .expect("candle_ddl_boot must ensure the 21 candle tables (DEDUP)");
    let views_pos = boot_src
        .find("console_views::ensure_named_views(questdb).await")
        .expect("candle_ddl_boot must ensure the analyst console views");

    assert!(
        drop_pos < ensure_pos && ensure_pos < views_pos,
        "load-bearing order: drop legacy objects (free squatted names) -> \
         ensure candle tables -> named views (validate column refs)"
    );
}

/// The writer → table → ensure → boot-call-site coverage table: every
/// LIVE-writer table's ensure fn must keep at least the named production
/// call site. Losing one silently re-opens the fresh-volume no-DEDUP
/// window for that table.
#[test]
fn test_every_live_table_ensure_fn_keeps_its_boot_call_site() {
    // (ensure fn call needle, caller file relative to crates/app)
    let coverage: &[(&str, &str)] = &[
        // candles_* (21 tables) — seal chain writer (rest_candle_fold)
        ("ensure_shadow_candle_tables", "src/candle_ddl_boot.rs"),
        // analyst console views (read-only projections)
        ("ensure_named_views", "src/candle_ddl_boot.rs"),
        // spot_1m_rest — Dhan/Groww/cadence spot legs
        ("ensure_spot_1m_rest_table", "src/spot_1m_rest_boot.rs"),
        ("ensure_spot_1m_rest_table", "src/cadence_boot.rs"),
        ("ensure_spot_1m_rest_table", "src/groww_spot_1m_boot.rs"),
        // option_chain_1m — chain legs
        (
            "ensure_option_chain_1m_table",
            "src/option_chain_1m_boot.rs",
        ),
        (
            "ensure_option_chain_1m_table",
            "src/groww_option_chain_1m_boot.rs",
        ),
        ("ensure_option_chain_1m_table", "src/cadence_boot.rs"),
        // option_contract_1m_rest — contract leg
        (
            "ensure_option_contract_1m_rest_table",
            "src/groww_contract_1m_boot.rs",
        ),
        // rest_fetch_audit — every REST leg's forensics
        ("ensure_rest_fetch_audit_table", "src/spot_1m_rest_boot.rs"),
        // tf_consistency_audit — 15:40 IST verifier
        (
            "ensure_tf_consistency_audit_table",
            "src/tf_consistency_boot.rs",
        ),
        // spot_crossverify_* — 15:47 IST comparator
        (
            "ensure_spot_crossverify_tables",
            "src/spot_crossverify_boot.rs",
        ),
        // brutex_crossverify_* — 15:50 IST comparator
        (
            "ensure_brutex_crossverify_tables",
            "src/brutex_crossverify_boot.rs",
        ),
        // order_audit / pnl_audit — order-side observability consumers
        ("ensure_order_audit_table", "src/order_observability.rs"),
        ("ensure_pnl_audit_table", "src/order_observability.rs"),
        // ws_event_audit — SEBI-retentioned lifecycle forensics
        ("ensure_ws_event_audit_table", "src/ws_audit_consumer.rs"),
        // scoreboard tables — 15:45 IST aggregation
        (
            "ensure_feed_scoreboard_tables",
            "src/feed_scoreboard_boot.rs",
        ),
        (
            "ensure_feed_episode_audit_table",
            "src/feed_scoreboard_boot.rs",
        ),
        // (tick_conservation_audit's ensure row retired 2026-07-18 with the
        // tick-conservation audit modules — dead-WS sweep follow-up; the
        // TABLE itself is retained per SEBI 5y, no DDL drop anywhere.)
        // index_constituency — ts-pin boot migration (SEBI, never dropped)
        (
            "ensure_index_constituency_table",
            "src/index_constituency_boot.rs",
        ),
    ];

    for (needle, rel) in coverage {
        let src = production_region(&read_src(rel));
        assert!(
            src.contains(needle),
            "{rel} lost its production `{needle}` call — the fresh-volume \
             no-DEDUP duplicate-row window re-opens for that table \
             (Track A coverage table, 2026-07-18)"
        );
    }
}

/// Scanner self-test: the production-region split must actually excise a
/// test module so a `#[cfg(test)]`-only mention can never satisfy a pin.
#[test]
fn test_production_region_split_excises_test_modules() {
    let fixture = "fn prod() {}\n#[cfg(test)]\nmod tests { fn t() { needle(); } }\n";
    let region = production_region(fixture);
    assert!(region.contains("fn prod"));
    assert!(!region.contains("needle()"));
}
