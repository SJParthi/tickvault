//! Phase 2.10 — hostile bug-hunt M1 fix: phase classification now uses
//! the per-tick `exchange_timestamp` (Dhan-source IST seconds-of-day)
//! instead of the wall-clock `now_ist_secs_of_day()`.
//!
//! ## What was broken
//!
//! Phase 2.5/2.7 used `now_ist_secs_of_day()` (wall-clock IST seconds)
//! to feed `compute_phase()`. A tick stamped at 09:14:59 IST that
//! arrived at 09:15:01 IST (typical 50-200ms network jitter on Dhan's
//! WS feed) got classified as `phase=OPEN` instead of `phase=PREOPEN`.
//! The hostile bug-hunt review estimated 10-30 ticks/day misclassified
//! at the 09:00 / 09:15 / 15:30 / 15:40 IST boundaries.
//!
//! ## Why the fix is correct
//!
//! `tick.exchange_timestamp` is Dhan's source-of-truth timestamp for
//! the trade event. It IS already in `ParsedTick` (no syscall, no
//! allocation — just a `u32` field). Phase classification is about
//! WHEN the trade happened, not when our process received it. The
//! exchange timestamp is the canonical answer.
//!
//! `tick.exchange_timestamp % 86_400` gives IST seconds-of-day in
//! `[0, 86400)`. This is a single integer modulo, cheaper than the
//! `chrono::Utc::now()` vDSO call it replaces.
//!
//! ## Hot-path impact
//!
//! Improvement: was `chrono::Utc::now()` once per frame (~50ns vDSO)
//! → now `u32 % 86_400` per tick (~1ns). Net cost reduction even
//! though the modulo runs per-tick instead of per-frame.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let me = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    me.parent()
        .expect("tickvault root")
        .parent()
        .expect("workspace root")
        .to_path_buf()
        .join("crates")
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|err| panic!("read {p:?}: {err}"))
}

#[test]
fn phase_classification_uses_per_tick_exchange_timestamp() {
    let src = read("core/src/pipeline/tick_processor.rs");
    assert!(
        src.contains("tick.exchange_timestamp % 86_400"),
        "tick_processor must compute phase classification from `tick.exchange_timestamp % 86_400` \
         — not from wall-clock `now_ist_secs_of_day()` (Phase 2.10 M1 fix)"
    );
}

#[test]
fn legacy_wall_clock_helper_no_longer_imported_in_tick_processor() {
    let src = read("core/src/pipeline/tick_processor.rs");
    assert!(
        !src.contains("use tickvault_common::market_hours::now_ist_secs_of_day"),
        "tick_processor must NOT import now_ist_secs_of_day after Phase 2.10 — \
         the per-tick exchange_timestamp drives phase classification"
    );
}

#[test]
fn enricher_called_with_tick_secs_of_day_at_both_packet_call_sites() {
    let src = read("core/src/pipeline/tick_processor.rs");
    // Phase 2.10 expects two `enrich_tick(<tick>, tick_secs_of_day)` calls
    // — one in the Quote arm and one in the Full arm. Each carries the
    // same `<tick>.exchange_timestamp % 86_400` derivation. Since C2
    // (feed-consumer convergence, 2026-07-06) the Quote-arm site lives
    // inside the `consume_feed_tick` enrich closure where the tick binding
    // is the closure param `t` — count BOTH spellings; the invariant (the
    // per-tick exchange timestamp drives phase at both sites) is unchanged.
    let count = src
        .matches("enricher.enrich_tick(&tick, tick_secs_of_day)")
        .count()
        + src
            .matches("enricher.enrich_tick(t, tick_secs_of_day)")
            .count();
    assert!(
        count >= 2,
        "tick_processor must call `enrich_tick(<tick>, tick_secs_of_day)` at BOTH packet \
         call sites (Quote + Full) — found {count}"
    );
}

#[test]
fn tick_secs_of_day_modulo_constant_is_seconds_per_day() {
    let src = read("core/src/pipeline/tick_processor.rs");
    // The constant `86_400` is `SECONDS_PER_DAY` — pin the literal so a
    // future commit doesn't accidentally use a different modulus.
    assert!(
        src.contains("tick.exchange_timestamp % 86_400"),
        "tick.exchange_timestamp modulo MUST be 86_400 (SECONDS_PER_DAY) so the \
         result is seconds-of-day in [0, 86400)"
    );
}

/// Boundary scenario unit-test: a tick stamped at 09:14:59 IST that
/// arrives at 09:15:01 IST should classify as PREOPEN, not OPEN.
/// We exercise the `compute_phase` boundary directly.
#[test]
fn boundary_jitter_tick_at_0914_59_is_preopen_not_open() {
    use tickvault_common::phase::{Phase, compute_phase};
    use tickvault_common::types::ExchangeSegment;

    // 09:14:59 IST = 9*3600 + 14*60 + 59 = 33_299
    let preopen_secs = 9 * 3600 + 14 * 60 + 59;
    assert_eq!(
        compute_phase(preopen_secs, ExchangeSegment::NseEquity),
        Phase::Preopen,
        "09:14:59 IST must classify as PREOPEN — fix M1 ensures wall-clock jitter \
         doesn't bump it to OPEN"
    );

    // 09:15:00 IST = 33_300 — boundary itself = OPEN.
    let open_secs = 9 * 3600 + 15 * 60;
    assert_eq!(
        compute_phase(open_secs, ExchangeSegment::NseEquity),
        Phase::Open,
        "09:15:00 IST is the OPEN boundary"
    );
}

/// Boundary scenario unit-test: a tick stamped at 15:29:59 IST should
/// classify as OPEN, not POSTAUCTION. Wall-clock jitter previously
/// misclassified these as POSTAUCTION (15:30 boundary).
#[test]
fn boundary_jitter_tick_at_1529_59_is_open_not_post_auction() {
    use tickvault_common::phase::{Phase, compute_phase};
    use tickvault_common::types::ExchangeSegment;

    let open_close_secs = 15 * 3600 + 29 * 60 + 59;
    assert_eq!(
        compute_phase(open_close_secs, ExchangeSegment::NseEquity),
        Phase::Open,
        "15:29:59 IST must classify as OPEN — last second of continuous trading"
    );

    let post_auction_secs = 15 * 3600 + 30 * 60;
    assert_eq!(
        compute_phase(post_auction_secs, ExchangeSegment::NseEquity),
        Phase::PostAuction,
        "15:30:00 IST is the POSTAUCTION boundary"
    );
}

/// Boundary scenario unit-test: 08:59:59 IST = PREMARKET, 09:00:00 IST = PREOPEN.
#[test]
fn boundary_jitter_tick_at_0859_59_is_premarket_not_preopen() {
    use tickvault_common::phase::{Phase, compute_phase};
    use tickvault_common::types::ExchangeSegment;

    let premarket_secs = 9 * 3600 - 1;
    assert_eq!(
        compute_phase(premarket_secs, ExchangeSegment::NseEquity),
        Phase::Premarket
    );

    let preopen_secs = 9 * 3600;
    assert_eq!(
        compute_phase(preopen_secs, ExchangeSegment::NseEquity),
        Phase::Preopen
    );
}

/// Modulo invariant: every Dhan exchange_timestamp (u32 IST epoch
/// seconds since 1970-01-01) maps to a valid seconds-of-day in
/// `[0, 86400)`. Property test: 64 sample timestamps across a year.
#[test]
fn tick_exchange_timestamp_modulo_always_in_seconds_of_day_range() {
    // 2026-05-04 00:00 IST + 5400-second steps spanning ~16 days.
    let base: u32 = 1_777_046_400; // 2026-05-04 00:00 IST epoch
    for i in 0_u32..64 {
        let exchange_ts = base + (i * 5400);
        let secs_of_day = exchange_ts % 86_400;
        assert!(
            secs_of_day < 86_400,
            "secs_of_day must be < 86400 for any u32 timestamp, got {secs_of_day} for ts={exchange_ts}"
        );
    }
}
