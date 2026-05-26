//! Ratchets for the 2026-05-25 option-chain warmup + current-expiry
//! cache + minute-aligned sequential scheduler design.
//!
//! Operator lock 2026-05-25:
//!   * Pre-market 09:00:30 IST fetches expiry list per underlying;
//!     `data[0]` populates `CurrentExpiryCache`.
//!   * Minute scheduler reads cached expiry (skips per-slot expiry-list
//!     Dhan call).
//!   * Sequence: SENSEX → BANKNIFTY → NIFTY.
//!   * Crash recovery via REHYDRATE from QuestDB.
//!
//! 2026-05-26 amendment: slot spacing widened from 3s (:50/:53/:56) to
//! 5s (:49/:54/:59) after live logs showed ~1 DH-904 429 every 5 minutes
//! at the 3s spacing — Dhan's "1 unique request per 3 seconds" boundary
//! is too tight under network jitter. The S→B→N sequence is preserved;
//! only the per-call gap changed.
//!
//! These ratchets fail the build if any of those contracts regresses.

use std::fs;
use std::path::{Path, PathBuf};

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

#[test]
fn test_option_chain_fetch_sequence_is_sensex_banknifty_nifty() {
    // Operator-locked 2026-05-25: at every :50 minute boundary the
    // scheduler fires SENSEX first, then BANKNIFTY, then NIFTY.
    // Re-ordering requires a rule-file edit + this test update.
    use tickvault_common::locked_universe::OPTION_CHAIN_FETCH_SEQUENCE;

    assert_eq!(
        OPTION_CHAIN_FETCH_SEQUENCE.len(),
        3,
        "exactly 3 underlyings per operator lock — NIFTY, BANKNIFTY, SENSEX"
    );
    assert_eq!(OPTION_CHAIN_FETCH_SEQUENCE[0], (51, "SENSEX"));
    assert_eq!(OPTION_CHAIN_FETCH_SEQUENCE[1], (25, "BANKNIFTY"));
    assert_eq!(OPTION_CHAIN_FETCH_SEQUENCE[2], (13, "NIFTY"));
}

#[test]
fn test_config_slot_secs_are_49_54_59() {
    // 2026-05-26: SENSEX :49, BANKNIFTY :54, NIFTY :59. 5-second spacing
    // eliminates Dhan rate-limit 429s observed at the previous 3s
    // spacing (:50/:53/:56). Sequence S→B→N is preserved; all three
    // fire within the same wall-clock minute.
    let body = read("config/base.toml");
    // Find the option_chain section and pull the slot_sec assignments.
    let section_start = body
        .find("[[option_chain_minute_snapshot.underlyings]]")
        .expect("section must exist");
    let section = &body[section_start..];

    // Look for the SENSEX/BANKNIFTY/NIFTY blocks in order.
    let sensex_idx = section
        .find("symbol      = \"SENSEX\"")
        .expect("SENSEX block missing");
    let banknifty_idx = section
        .find("symbol      = \"BANKNIFTY\"")
        .expect("BANKNIFTY block missing");
    let nifty_idx = section
        .find("symbol      = \"NIFTY\"")
        .expect("NIFTY block missing");

    // Pull the slot_sec assignment line within each block (next 250 chars).
    fn extract_slot_sec(block: &str) -> u32 {
        let s = block.find("slot_sec").expect("slot_sec missing in block");
        let line_end = block[s..]
            .find('\n')
            .expect("slot_sec line must have newline");
        let line = &block[s..s + line_end];
        // Pattern: `slot_sec    = 50           # comment` — pull the int.
        line.split('=')
            .nth(1)
            .expect("slot_sec = N")
            .split_whitespace()
            .next()
            .expect("slot_sec value")
            .parse::<u32>()
            .expect("slot_sec is u32")
    }

    assert_eq!(
        extract_slot_sec(&section[sensex_idx..sensex_idx + 600]),
        49,
        "SENSEX MUST fire at :49 — 5s spacing avoids Dhan DH-904 429s"
    );
    assert_eq!(
        extract_slot_sec(&section[banknifty_idx..banknifty_idx + 600]),
        54,
        "BANKNIFTY MUST fire at :54 — 5s after SENSEX"
    );
    assert_eq!(
        extract_slot_sec(&section[nifty_idx..nifty_idx + 600]),
        59,
        "NIFTY MUST fire at :59 — 5s after BANKNIFTY, within same minute"
    );
}

#[test]
fn test_current_expiry_cache_module_exists() {
    // The cache lives in core; renaming/deleting it would break the
    // scheduler + warmup + rehydrate trio.
    let path = workspace_root().join("crates/core/src/option_chain/current_expiry_cache.rs");
    assert!(
        path.exists(),
        "current_expiry_cache.rs is the canonical cache module"
    );
    let body = fs::read_to_string(&path).unwrap_or_default(); // APPROVED: test
    assert!(
        body.contains("pub struct CurrentExpiryCache"),
        "must export CurrentExpiryCache as pub struct"
    );
    assert!(
        body.contains("pub fn insert") && body.contains("pub fn get"),
        "must export insert + get methods"
    );
}

#[test]
fn test_expiry_warmup_module_exists_and_fires_at_09_00_30_ist() {
    let path = workspace_root().join("crates/core/src/option_chain/expiry_warmup.rs");
    assert!(path.exists(), "expiry_warmup.rs must exist");
    let body = fs::read_to_string(&path).unwrap_or_default(); // APPROVED: test
    assert!(
        body.contains("pub const WARMUP_FIRE_SECS_OF_DAY_IST: u32 = 9 * 3_600 + 30"),
        "warmup MUST fire at 09:00:30 IST per operator lock"
    );
    assert!(
        body.contains("pub fn pick_current_expiry_from_list"),
        "must export pick_current_expiry_from_list (pure helper)"
    );
    assert!(
        body.contains("OPTION_CHAIN_FETCH_SEQUENCE"),
        "warmup MUST iterate OPTION_CHAIN_FETCH_SEQUENCE (SENSEX→BANKNIFTY→NIFTY)"
    );
}

#[test]
fn test_option_chain_cache_loader_module_exists() {
    // Crash-recovery REHYDRATE — must survive PR refactors.
    let path = workspace_root().join("crates/app/src/option_chain_cache_loader.rs");
    assert!(path.exists(), "option_chain_cache_loader.rs must exist");
    let body = fs::read_to_string(&path).unwrap_or_default(); // APPROVED: test
    assert!(
        body.contains("pub async fn rehydrate_current_expiry_cache_at_boot"),
        "must export the boot rehydrate entry point"
    );
    assert!(
        body.contains("LATEST ON ts PARTITION BY underlying_security_id, exchange_segment"),
        "SELECT must use LATEST ON + composite PARTITION BY per I-P1-11"
    );
    assert!(
        body.contains("dateadd('h', -26"),
        "SELECT must filter to last 26h (one trading day + headroom)"
    );
}

#[test]
fn test_scheduler_consumes_current_expiry_cache() {
    // The scheduler MUST take a CurrentExpiryCache parameter — that's
    // what cuts Dhan REST traffic in half. Source-scan ratchet so a
    // future refactor can't silently drop it.
    let body = read("crates/core/src/option_chain/snapshot_scheduler.rs");
    assert!(
        body.contains(
            "current_expiry_cache: crate::option_chain::current_expiry_cache::CurrentExpiryCache"
        ),
        "spawn_snapshot_scheduler MUST accept a CurrentExpiryCache handle"
    );
    assert!(
        body.contains("current_expiry_cache.get("),
        "run_one_slot_fetch MUST read from the cache before falling back to Dhan"
    );
}

#[test]
fn test_main_rs_wires_warmup_and_rehydrate() {
    // The boot orchestrator MUST spawn the warmup task AND call
    // rehydrate_and_log. Without these the cache never populates and
    // the scheduler falls back to per-slot Dhan calls forever.
    let body = read("crates/app/src/main.rs");
    assert!(
        body.contains("rehydrate_and_log"),
        "main.rs MUST call option_chain_cache_loader::rehydrate_and_log at boot"
    );
    assert!(
        body.contains("spawn_expiry_warmup_task"),
        "main.rs MUST spawn the 09:00:30 IST warmup task"
    );
    assert!(
        body.contains("CurrentExpiryCache::new()"),
        "main.rs MUST construct exactly one CurrentExpiryCache and \
         share it between rehydrate, warmup, and scheduler"
    );
}

#[test]
fn test_dhan_data_zero_index_contract_documented() {
    // Operator-confirmed 2026-05-25: Dhan's `/optionchain/expirylist`
    // returns only ACTIVE expiries; `data[0]` is ALWAYS the
    // nearest/current expiry. Pin this contract in expiry_warmup docs
    // so it doesn't drift.
    let body = read("crates/core/src/option_chain/expiry_warmup.rs");
    assert!(
        body.contains("data[0]"),
        "expiry_warmup docs MUST mention the data[0]-is-current contract"
    );
    assert!(
        body.contains("ACTIVE"),
        "expiry_warmup docs MUST mention ACTIVE-only filtering"
    );
}
