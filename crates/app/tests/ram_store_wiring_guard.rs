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
    // ---- The REHYDRATE term is RETIRED 2026-09-16 ----
    //
    // This chain read `install → rehydrate → stats`. The middle term was
    // `market_ram_store_boot::spawn_chain_day_rehydrate(`, the one-shot boot
    // step that read today's per-minute option-chain rows back out of
    // QuestDB so a mid-session restart did not open with an empty options
    // view.
    //
    // Its source table's WRITER was removed by the operator's SOCKETS-ONLY
    // narrowing (`no-rest-except-live-feed-2026-06-27.md` §12.10), so the
    // reader went with it — deliberately, and as that section's own §12.6
    // REJECT row demands in as many words: "Removes a WRITER and leaves a
    // READER pointed at the now-frozen table." A rehydrate left standing
    // would have read a table nothing writes, found nothing, recorded zero
    // minutes and reported success — a boot step whose silence is
    // indistinguishable from a quiet market.
    //
    // The two assertions BELOW are the reason this test was narrowed rather
    // than deleted, and neither depended on the rehydrate: the same-block
    // pin (a gate that drifted away no longer guards anything) and the
    // load-bearing `install < fold gate` ORDER pin.
    let stats_idx = main_rs
        .find("market_ram_store_boot::spawn_ram_store_stats_task(")
        .expect("main.rs must spawn the stats/heartbeat task");
    // Anchored on the `if` — the GATE — not on a bare mention of the flag.
    //
    // 2026-08-11: the loose needle broke when the live-feed lane started
    // reading the same flag (to refuse running while the fold also writes
    // Dhan candles). That read sits earlier in main.rs, so `find` returned
    // it instead of the spawn gate and this assertion compared the wrong two
    // positions. The flag now legitimately appears more than once; the gate
    // does not.
    let fold_gate_idx = main_rs
        .find("if config.rest_candle_fold.enabled {")
        .expect("main.rs must still gate the fold spawn");

    assert!(
        gate_idx < install_idx && install_idx < stats_idx,
        "install → stats must sit inside the gated block in that order \
         (gate {gate_idx}, install {install_idx}, stats {stats_idx})"
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
    // PR-2 round-1 LOW: the day_seals ACCUMULATION sites feed those record
    // hooks — deleting either compiles green and silently empties the
    // catch-up's RAM day blocks (in-loop folds + past-day force-seal tail).
    assert_eq!(
        count_occurrences(prod, "day_seals.extend_from_slice(&sealed)"),
        2,
        "refold_day must accumulate seals into day_seals at EXACTLY two \
         sites (the in-loop fold + the past-day force-seal tail)"
    );

    // Both hooks resolve the process-global store (no-op when disabled).
    assert!(
        count_occurrences(prod, "spot_bar_store()") >= 2,
        "both RAM hooks must resolve the process-global SpotBarStore"
    );
}

// ---- `chain_publish_helper_records_into_the_day_store_after_the_registry_publish`
// ---- is RETIRED 2026-09-16 ----
//
// It pinned the ORDER inside `option_chain_1m_boot::publish_chain_moneyness_snapshot`:
// the latest-minute registry publish had to PRECEDE the day-store
// `.record_live(` call, so the registry stayed the moneyness decision source
// of truth and the day store stayed the history behind it.
//
// `crates/app/src/option_chain_1m_boot.rs` no longer exists. The per-minute
// option-chain REST pull is one of the two classes the operator's
// SOCKETS-ONLY narrowing removed (`no-rest-except-live-feed-2026-06-27.md`
// §12.10 — "Bro just remove per minute price falls and 3.41 pm accuracy
// check alone dude okay"), so `read_source` on that path panics with a
// bare `No such file or directory (os error 2)`.
//
// RETIRED rather than re-pointed, and the distinction matters: the sibling
// `tf_consistency_boot` verifier in this same removal was RE-POINTED (to
// `ticks`) because its SUBJECT survived the writer — candles are still
// folded, just from a different source. This test's subject is the helper
// itself, and there is no surviving publisher of a chain moneyness snapshot
// to re-point at. Pinning an order between two calls that no longer happen
// would be a guard that can only ever pass.
//
// WHAT THIS LEAVES UNWATCHED, stated rather than implied: nothing re-checks
// that a FUTURE chain publisher puts the registry before the day store. If
// one is ever built, this test is the shape to restore — §12.10.7(c) already
// records that `chain_day_store` is installed, never written and read only
// for the `tv_ram_store_chain_minutes_resident` gauge, which is the open
// decision that would come with it.

#[test]
fn ram_store_boot_module_keeps_load_bearing_pieces() {
    let src = read_source("crates/app/src/market_ram_store_boot.rs");
    let prod = production_region(&src);

    // ---- The three REHYDRATE pins are RETIRED 2026-09-16 ----
    //
    // `record_rehydrated(snap)`, `rehydrate_truncated` and
    // `accumulate_capped(` pinned the one-shot chain-day rehydrate: it read
    // today's per-minute option-chain rows back out of QuestDB so a
    // mid-session restart did not start with an empty options view.
    //
    // The WRITER of the table it read was removed by the operator's
    // SOCKETS-ONLY narrowing (`no-rest-except-live-feed-2026-06-27.md`
    // §12.10), so the reader went with it — deliberately, and as that
    // section's own §12.6 REJECT row requires in as many words: "Removes a
    // WRITER and leaves a READER pointed at the now-frozen table." A
    // rehydrate left behind would have read a table nothing writes, found
    // nothing, recorded zero minutes, and reported success — a boot step
    // whose silence is indistinguishable from a quiet market.
    //
    // The REST of this test is UNCHANGED and still binds: the seven operator
    // gauges below and the coded-degrade pin have nothing to do with the
    // rehydrate, and narrowing to them is why this test was not deleted
    // wholesale. `tv_ram_store_chain_minutes_resident` in particular is KEPT
    // on purpose — the chain store is now installed and never written
    // (§12.10.7(c) records the open decision), so that gauge reading a flat
    // zero is the honest surface for exactly that state, and it must keep
    // being published rather than quietly dropped along with its writer.
    // The operator's depth gauges + the dense heartbeat.
    for gauge in [
        "tv_ram_store_spot_bars_resident",
        "tv_ram_store_spot_days_depth",
        "tv_ram_store_chain_minutes_resident",
        "tv_ram_store_estimated_bytes",
        "tv_ram_store_heartbeat_total",
        // PR-2 round-1 HIGH: the spot store's over-window drop total must
        // stay PUBLISHED (counter-style gauge from the 60s stats task) —
        // dropping it regresses spot drops to an unpublished stat.
        "tv_ram_store_spot_dropped_over_window",
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
