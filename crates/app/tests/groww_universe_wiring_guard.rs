//! Source-scan ratchet — the `[groww_universe]` daily rider is wired into
//! main.rs (Groww live-feed retirement fix-round-1b, 2026-07-15).
//!
//! The rider is the SOLE surviving producer of the daily Groww watch file
//! (the spot leg's VIX resolver input) and the SOLE caller of
//! `persist_groww_instruments` (SEBI `feed='groww'` master continuity —
//! GROWW-MASTER-01 contract). A refactor that drops the spawn, the config
//! gate, or either call would compile green and silently starve both
//! consumers — this guard fails the build instead.
//!
//! Pins (house pattern: `groww_chain_1m_wiring_guard.rs`):
//! 1. main.rs has exactly ONE `spawn_groww_universe_rider(` call site,
//!    gated on `config.groww_universe.enabled` (serde default OFF).
//! 2. Stub-guard: the rider module really does the work — the SUPERVISED
//!    respawn wrapper (`classify_join_exit` +
//!    `tv_groww_universe_respawn_total{reason}`), the daily
//!    `build_and_write_groww_watch` pull-until-success loop, and the
//!    fire-and-forget `persist_groww_instruments` spawn.
//!
//! Runbook: `.claude/rules/project/groww-shared-master-error-codes.md`.

use std::fs;
use std::path::PathBuf;

fn read_app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Production region only: everything above the first column-0 `#[cfg(test)]`.
fn production_region(src: &str) -> String {
    match src.find("\n#[cfg(test)]") {
        Some(idx) => src[..idx].to_string(),
        None => src.to_string(),
    }
}

#[test]
fn test_main_spawns_rider_gated_on_config() {
    let main_src = production_region(&read_app_src("src/main.rs"));
    assert_eq!(
        main_src.matches("spawn_groww_universe_rider(").count(),
        1,
        "main.rs must carry exactly ONE groww_universe rider spawn site"
    );
    let gate = main_src
        .find("if config.groww_universe.enabled")
        .expect("the rider spawn must be gated on [groww_universe] enabled");
    let spawn = main_src
        .find("spawn_groww_universe_rider(")
        .expect("spawn site present");
    assert!(
        gate < spawn && spawn - gate < 800,
        "the config gate must immediately guard the spawn site (gate at {gate}, spawn at {spawn})"
    );
}

#[test]
fn test_rider_module_is_supervised_and_does_the_work() {
    let rider = production_region(&read_app_src("src/groww_universe.rs"));
    for needle in [
        // supervised-respawn pattern (fix-round-1b hardening)
        "classify_join_exit",
        "tv_groww_universe_respawn_total",
        "fn run_groww_universe_rider",
        // the daily build loop (watch file for the REST legs)
        "build_and_write_groww_watch(",
        // SEBI master continuity (sole caller repo-wide)
        "persist_groww_instruments(",
        // per-attempt IST date re-derivation (hostile-round-3 rule, re-homed)
        "today_ist_date()",
        // day-roll sleep (IST midnight + grace)
        "secs_until_day_roll(",
    ] {
        assert!(
            rider.contains(needle),
            "groww_universe.rs production region must contain `{needle}`"
        );
    }
    for banned in ["unwrap(", "expect(", "println!"] {
        assert!(
            !rider.contains(banned),
            "groww_universe.rs production region must not contain `{banned}`"
        );
    }
}
