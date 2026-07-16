//! Build-failing wiring ratchet for the REST-era bar-fold candle
//! derivation (operator directive 2026-07-16 — FOLD-01 runbook:
//! `.claude/rules/project/rest-candle-fold-error-codes.md`).
//!
//! Pins, source-scan style (the `spot_1m_rest_wiring_guard.rs` house
//! pattern):
//! 1. main.rs spawns the fold in the SHARED-INFRA prefix — config-gated
//!    on `config.rest_candle_fold.enabled`, AFTER the seal-writer install
//!    (`spawn_seal_writer_loop`) so `global_seal_sender` exists before the
//!    first seal emission.
//! 2. BOTH spot legs hand off persist-confirmed bars — exactly TWO
//!    `send_confirmed_bars` call sites per leg (fire + sweep), each fed by
//!    `ConfirmedBar::from_minute_candle` staging at the append-ok arms.
//! 3. The fold module keeps its load-bearing pieces: the boot catch-up
//!    call, the global-sender install/read pair, and the FOLD-01 coded
//!    degrade emissions.
//!
//! A refactor that drops any of these compiles green without this guard
//! and silently starves the candles_* tables the operator demanded.

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

#[test]
fn main_rs_spawns_fold_gated_after_seal_writer_install() {
    let main_rs = read_source("crates/app/src/main.rs");

    let seal_writer_idx = main_rs
        .find("spawn_seal_writer_loop(")
        .expect("main.rs must install the global seal sender (spawn_seal_writer_loop)");
    let gate_idx = main_rs
        .find("config.rest_candle_fold.enabled")
        .expect("main.rs must gate the fold spawn on [rest_candle_fold] enabled");
    let sender_install_idx = main_rs
        .find("rest_candle_fold::set_global_fold_bar_sender(")
        .expect("main.rs must install the global fold-bar sender");
    let spawn_idx = main_rs
        .find("rest_candle_fold::spawn_supervised_rest_candle_fold(")
        .expect("main.rs must spawn the supervised fold task");

    assert!(
        seal_writer_idx < spawn_idx,
        "the seal-writer MUST be installed before the fold task spawns — \
         otherwise the first fold emission hits the no_seal_sender drop arm \
         (FOLD-01 stage=seal_send) on every boot"
    );
    assert!(
        gate_idx < sender_install_idx && sender_install_idx < spawn_idx,
        "the config gate must precede the sender install, which must precede \
         the spawn (gate at {gate_idx}, install at {sender_install_idx}, \
         spawn at {spawn_idx})"
    );
    assert!(
        spawn_idx.saturating_sub(gate_idx) < 4096,
        "the gate and the spawn must be the SAME block — a gate that drifted \
         away from the spawn no longer guards it"
    );
}

#[test]
fn dhan_spot_leg_hands_off_confirmed_bars_at_both_flush_ok_arms() {
    let src = read_source("crates/app/src/spot_1m_rest_boot.rs");
    let prod = production_region(&src);

    assert_eq!(
        count_occurrences(
            prod,
            "rest_candle_fold::send_confirmed_bars(&confirmed_bars)"
        ),
        2,
        "the Dhan spot leg must hand off confirmed bars at EXACTLY two sites \
         (the fire flush-ok arm + the sweep flush-ok arm)"
    );
    // Fire: own-minute + backfill staging; sweep: swept-minute staging.
    assert_eq!(
        count_occurrences(prod, "ConfirmedBar::from_minute_candle("),
        3,
        "the Dhan spot leg must stage bars at the 3 append-ok arms \
         (fire own-minute, fire backfill, sweep)"
    );
}

#[test]
fn groww_spot_leg_hands_off_confirmed_bars_at_both_flush_ok_arms() {
    let src = read_source("crates/app/src/groww_spot_1m_boot.rs");
    let prod = production_region(&src);

    assert_eq!(
        count_occurrences(
            prod,
            "rest_candle_fold::send_confirmed_bars(&confirmed_bars)"
        ),
        2,
        "the Groww spot leg must hand off confirmed bars at EXACTLY two sites \
         (the fire flush-ok arm + the sweep flush-ok arm)"
    );
    assert_eq!(
        count_occurrences(prod, "ConfirmedBar::from_minute_candle("),
        3,
        "the Groww spot leg must stage bars at the 3 append-ok arms \
         (fire own-minute, fire backfill, sweep)"
    );
}

#[test]
fn fold_module_keeps_load_bearing_pieces() {
    let src = read_source("crates/app/src/rest_candle_fold.rs");
    let prod = production_region(&src);

    // The boot catch-up must actually be invoked by the run loop.
    assert!(
        prod.contains("boot_catchup(&mut runtime, config.catchup_days, today).await"),
        "run_rest_candle_fold must invoke the boot catch-up with the \
         configured window"
    );
    // Seal emission goes through the EXISTING shared seal-writer channel.
    assert!(
        prod.contains("seal_writer_runner::global_seal_sender()"),
        "the fold must emit seals via the shared global_seal_sender — never \
         a parallel writer"
    );
    // The handoff sender is the OnceLock first-wins global.
    assert!(
        prod.contains("pub fn set_global_fold_bar_sender"),
        "the global fold-bar sender install must exist"
    );
    // Degrades are CODED (tag-guard discipline), never silent.
    assert!(
        prod.contains("ErrorCode::RestCandleFold01Degraded.code_str()"),
        "fold degrades must carry the FOLD-01 code field"
    );
    // Persist-confirmed contract: bars from an unflushed batch must never
    // fold — the hook sites call send_confirmed_bars only after the ACK,
    // and the module documents that contract verbatim.
    assert!(
        prod.contains("persist-CONFIRMED"),
        "the persist-confirmed handoff contract wording must stay in the \
         module (the hook-site reviews key off it)"
    );
}

#[test]
fn fold_module_never_writes_ticks() {
    // live-feed-purity rules 1-6: the fold derives candles from bars — it
    // must never reference the ticks writer surface.
    let src = read_source("crates/app/src/rest_candle_fold.rs");
    for banned in [
        "TickPersistenceWriter",
        "append_tick(",
        "synthesize_ticks",
        "ParsedTick",
    ] {
        assert!(
            !src.contains(banned),
            "rest_candle_fold must never touch the ticks surface: found {banned}"
        );
    }
}
