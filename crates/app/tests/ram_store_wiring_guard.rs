//! Build-failing wiring ratchet for the RAM residency stores (operator
//! directive 2026-07-16, PR-2 — RAMSTORE-01 runbook:
//! `.claude/rules/project/ram-store-error-codes.md`).
//!
//! Pins, source-scan style (the `rest_candle_fold_wiring_guard.rs` house
//! pattern — call-site anchoring, production-region split):
//! 1. main.rs installs the stores + spawns the rehydrate/stats tasks in a
//!    `config.market_ram_store.enabled` gated block that PRECEDES the fold
//!    spawn gate — the fold's boot catch-up is what fills the spot rings,
//!    so a store installed after the catch-up starts is silently empty.
//! 2. The fold's emit paths call the RAM hooks: the live `emit_seals`
//!    upsert hook + BOTH `refold_day` day-block record sites (today +
//!    past-day force-seal).
//! 3. The shared chain publish helper records into the day store AFTER
//!    the latest-minute registry publish, and BOTH chain legs still route
//!    through that helper.
//! 4. The boot module keeps its load-bearing pieces: `record_rehydrated`
//!    consumption, the depth gauges, the heartbeat, and the RAMSTORE-01
//!    coded degrades.
//!
//! A refactor that drops any of these compiles green without this guard
//! and silently leaves the operator's "is the month in RAM?" answer empty.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_source(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

/// Production region = everything above the FIRST column-0 `#[cfg(test)]`
/// line (the house source-scan convention — assertion literals in a test
/// module must never satisfy a production needle).
fn production_region(source: &str) -> &str {
    match source.find("\n#[cfg(test)]") {
        Some(idx) => &source[..idx],
        None => source,
    }
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

/// Occurrence indices of `needle` that are CALL SITES (not `fn `-preceded
/// definitions) — the M5 call-site-anchoring lesson from the fold guard.
fn call_site_indices(haystack: &str, needle: &str) -> Vec<usize> {
    haystack
        .match_indices(needle)
        .map(|(i, _)| i)
        .filter(|&idx| !haystack[..idx].ends_with("fn "))
        .collect()
}

#[test]
fn main_rs_installs_ram_stores_before_the_fold_spawn_gate() {
    let main_rs = read_source("crates/app/src/main.rs");

    let gate_idx = main_rs
        .find("config.market_ram_store.enabled")
        .expect("main.rs must gate the RAM store install on [market_ram_store] enabled");
    let install_idx = main_rs
        .find("market_ram_store_boot::install_market_ram_stores(")
        .expect("main.rs must install the RAM stores");
    let rehydrate_idx = main_rs
        .find("market_ram_store_boot::spawn_chain_day_rehydrate(")
        .expect("main.rs must spawn the chain-day rehydrate");
    let stats_idx = main_rs
        .find("market_ram_store_boot::spawn_ram_store_stats_task(")
        .expect("main.rs must spawn the stats/heartbeat task");
    let fold_gate_idx = main_rs
        .find("config.rest_candle_fold.enabled")
        .expect("main.rs must still gate the fold spawn");

    assert!(
        gate_idx < install_idx && install_idx < rehydrate_idx && rehydrate_idx < stats_idx,
        "install → rehydrate → stats must sit inside the gated block in that \
         order (gate {gate_idx}, install {install_idx}, rehydrate \
         {rehydrate_idx}, stats {stats_idx})"
    );
    assert!(
        stats_idx.saturating_sub(gate_idx) < 4096,
        "the gate and the spawns must be the SAME block — a gate that \
         drifted away no longer guards them"
    );
    // The load-bearing ORDER pin: the stores must exist BEFORE the fold
    // spawn gate, so PR-1's boot catch-up populates the spot rings
    // (pre-market spot rehydration IS the catch-up).
    assert!(
        install_idx < fold_gate_idx,
        "install_market_ram_stores must PRECEDE the rest_candle_fold spawn \
         gate — a store installed after the catch-up starts is silently \
         empty (install {install_idx}, fold gate {fold_gate_idx})"
    );
    // LOW accepted residual (documented, house convention): textual source
    // scan — helper-fn refactors track definition positions, not runtime
    // call order (the fold guard's LOW-3 class).
}

#[test]
fn fold_emit_paths_call_the_ram_hooks() {
    let src = read_source("crates/app/src/rest_candle_fold.rs");
    let prod = production_region(&src);

    // Live path: emit_seals mirrors into the spot rings BEFORE the channel
    // send (RAM residency independent of the seal-channel outcome).
    let upsert_calls = call_site_indices(prod, "ram_store_upsert_seals(");
    assert_eq!(
        upsert_calls.len(),
        1,
        "emit_seals must call the live RAM upsert hook at EXACTLY one site"
    );
    let emit_seals_def = prod.find("fn emit_seals(").expect("emit_seals must exist");
    let sender_read = prod[emit_seals_def..]
        .find("global_seal_sender()")
        .map(|i| emit_seals_def + i)
        .expect("emit_seals must read the global seal sender");
    assert!(
        emit_seals_def < upsert_calls[0] && upsert_calls[0] < sender_read,
        "the live RAM hook must run inside emit_seals BEFORE the seal-channel \
         send (def {emit_seals_def}, hook {}, sender {sender_read})",
        upsert_calls[0]
    );

    // Catch-up path: refold_day records the whole day as per-TF blocks at
    // BOTH exits (today + past-day incl. the force-sealed tails).
    let record_calls = call_site_indices(prod, "ram_store_record_day(");
    assert_eq!(
        record_calls.len(),
        2,
        "refold_day must record the day into RAM at EXACTLY two sites \
         (the today exit + the past-day force-seal exit)"
    );

    // Both hooks resolve the process-global store (no-op when disabled).
    assert!(
        count_occurrences(prod, "spot_bar_store()") >= 2,
        "both RAM hooks must resolve the process-global SpotBarStore"
    );
}

#[test]
fn chain_publish_helper_records_into_the_day_store_after_the_registry_publish() {
    let src = read_source("crates/app/src/option_chain_1m_boot.rs");
    let prod = production_region(&src);

    let helper_def = prod
        .find("pub fn publish_chain_moneyness_snapshot(")
        .expect("the shared chain publish helper must exist");
    let publish_idx = prod[helper_def..]
        .find("publish_chain_snapshot(")
        .map(|i| helper_def + i)
        .expect("the helper must still publish the latest-minute registry snapshot");
    let day_store_idx = prod[helper_def..]
        .find("chain_day_store::chain_day_store()")
        .map(|i| helper_def + i)
        .expect("the helper must resolve the chain day store");
    let record_idx = prod[helper_def..]
        .find(".record_live(")
        .map(|i| helper_def + i)
        .expect("the helper must record the minute into the day store");
    assert!(
        publish_idx < record_idx,
        "the latest-minute registry publish must PRECEDE the day-store \
         record (publish {publish_idx}, record {record_idx}) — the registry \
         stays the moneyness decision source of truth"
    );
    assert!(
        day_store_idx > helper_def && record_idx > helper_def,
        "day-store resolution + record must live inside the helper"
    );

    // Both chain legs still route through the ONE shared helper — the
    // single hook site covers Dhan AND Groww by construction.
    let dhan_calls = call_site_indices(prod, "publish_chain_moneyness_snapshot(");
    assert!(
        !dhan_calls.is_empty(),
        "the Dhan chain leg must call publish_chain_moneyness_snapshot"
    );
    let groww = read_source("crates/app/src/groww_option_chain_1m_boot.rs");
    let groww_prod = production_region(&groww);
    assert!(
        !call_site_indices(groww_prod, "publish_chain_moneyness_snapshot(").is_empty(),
        "the Groww chain leg must call publish_chain_moneyness_snapshot"
    );
}

#[test]
fn ram_store_boot_module_keeps_load_bearing_pieces() {
    let src = read_source("crates/app/src/market_ram_store_boot.rs");
    let prod = production_region(&src);

    // Rehydrated minutes go through the never-overwrites-live API.
    assert!(
        prod.contains("record_rehydrated(snap)"),
        "the rehydrate must record via record_rehydrated (live wins)"
    );
    // The bounded-read hardening: explicit LIMIT tripwire + streamed cap.
    assert!(
        prod.contains("rehydrate_truncated"),
        "a truncated rehydrate window must degrade loudly, never fold partial"
    );
    assert!(
        prod.contains("accumulate_capped("),
        "the rehydrate reads must stay under the streamed response cap"
    );
    // The operator's depth gauges + the dense heartbeat.
    for gauge in [
        "tv_ram_store_spot_bars_resident",
        "tv_ram_store_spot_days_depth",
        "tv_ram_store_chain_minutes_resident",
        "tv_ram_store_estimated_bytes",
        "tv_ram_store_heartbeat_total",
    ] {
        assert!(
            prod.contains(gauge),
            "the stats task must publish {gauge} — the honest fill-level surface"
        );
    }
    // Degrades are CODED (tag-guard discipline), never silent.
    assert!(
        prod.contains("ErrorCode::RamStore01Degraded.code_str()"),
        "RAM store degrades must carry the RAMSTORE-01 code field"
    );
}
