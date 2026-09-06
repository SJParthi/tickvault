//! `ws_lag_ms` must keep measuring EXCHANGE-vs-RECEIPT, and never the fold clock.
//!
//! # Why this is the most dangerous single edit in the receipt-clock work
//!
//! W2 moved five sites onto `fold_clock_ist_secs` so bucketing and ordering
//! agree. `ws_lag_ms` is on the plan's MUST-NOT-MOVE list, and the reason is
//! arithmetic:
//!
//! ```text
//! lag_ms = received_ms - (exchange_timestamp - IST_OFFSET) * 1000
//! ```
//!
//! The fold clock IS the receipt, converted to IST, whenever the receipt is
//! plausible -- which is the ordinary case. Feed it in as `exchange_timestamp`
//! and the expression becomes `received - received`:
//!
//! **every lag measurement collapses to ~0.**
//!
//! That is not a crash and not an error value. `WsLag::Measured(0.0)` is a
//! perfectly legal reading, so:
//!
//! * every lag histogram flattens to zero,
//! * the per-connection p99 reads perfect,
//! * `tv_dhan_ws_worst_conn_tick_age_secs` stops being able to rise,
//! * and `tv-<env>-dhan-worst-socket-deaf` can never fire again.
//!
//! No test in this workspace would catch it, because no test asserts a lag is
//! non-zero -- and on a healthy synthetic fixture zero is the RIGHT answer.
//! The measured production values this would erase are p50 1.38 s, p95 14.93 s,
//! p99 46.37 s, max 198.69 s (2026-07-06).
//!
//! A "consistency" refactor that moved every clock read onto `fold_secs` would
//! look tidy, pass CI, and silently blind the one metric that reports the
//! vendor delivering late.

use std::path::Path;
use tickvault_common::source_scan::strip_rust_comments;

fn stack_src() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/dhan_feed_stack.rs");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    strip_rust_comments(&raw)
}

#[test]
fn ws_lag_is_measured_against_the_raw_exchange_stamp() {
    let src = stack_src();

    assert!(
        src.contains("ws_lag_ms(tick.exchange_timestamp, received_at_nanos)"),
        "`record_ws_lag` must pass the RAW exchange stamp. Passing a fold-clock \
         value makes the subtraction `received - received`, so every lag reads \
         ~0 and the deaf-socket alarm can never fire again."
    );

    // And the fold clock must not have leaked into the lag path at all -- the
    // call above could be joined by a second, fold-clocked one.
    let lag_fn = src
        .split("pub fn record_ws_lag(")
        .nth(1)
        .expect("record_ws_lag must exist");
    let lag_fn = &lag_fn[..lag_fn.find("\npub fn ").unwrap_or(lag_fn.len())];
    assert!(
        !lag_fn.contains("fold_clock_ist_secs") && !lag_fn.contains("fold_secs"),
        "the lag path must never see the fold clock; found it inside \
         `record_ws_lag`"
    );
}

/// The arithmetic itself, demonstrated rather than argued.
///
/// Proves BOTH halves: a real delivery lag is measured, and substituting the
/// receipt-derived clock for the exchange stamp collapses it to zero. Without
/// the second assertion the first proves only that subtraction works.
#[test]
fn substituting_the_fold_clock_would_collapse_every_lag_to_zero() {
    use tickvault_app::dhan_feed_stack::{WsLag, ws_lag_ms};

    const IST_OFFSET: i64 = tickvault_common::constants::IST_UTC_OFFSET_SECONDS as i64;
    // A trade printed at an IST epoch second, delivered 46 seconds later --
    // this feed's MEASURED p99.
    const TRADE_IST_SECS: u32 = 1_800_000_000;
    const LAG_SECS: i64 = 46;
    let received_utc_nanos = (i64::from(TRADE_IST_SECS) - IST_OFFSET + LAG_SECS) * 1_000_000_000;

    match ws_lag_ms(TRADE_IST_SECS, received_utc_nanos) {
        Some(WsLag::Measured(ms)) => assert!(
            (ms - 46_000.0).abs() < 1.0,
            "the real p99 lag must measure ~46,000 ms, got {ms}"
        ),
        other => panic!("expected a measured lag, got {other:?}"),
    }

    // Now the defect: the fold clock for that same tick IS the receipt in IST
    // seconds, because 46 s is inside the plausible band.
    let fold_secs = tickvault_trading::candles::tf_index::fold_clock_ist_secs(
        TRADE_IST_SECS,
        received_utc_nanos,
    );
    assert_ne!(
        fold_secs, TRADE_IST_SECS,
        "the fixture must actually exercise the receipt-preferred branch, else \
         the assertion below is vacuous"
    );
    match ws_lag_ms(fold_secs, received_utc_nanos) {
        Some(WsLag::Measured(ms)) => assert!(
            ms < 1_000.0,
            "feeding the fold clock must collapse the 46-second lag toward zero \
             -- got {ms}. This test exists to make that collapse VISIBLE, so if \
             this assertion fails the arithmetic changed and the guard above \
             needs re-deriving, not deleting."
        ),
        other => panic!("expected a measured lag, got {other:?}"),
    }
}
