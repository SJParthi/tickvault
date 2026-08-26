//! The deaf socket: a connection that keeps ponging but stops delivering.
//!
//! # Why the obvious fixes do not work
//!
//! This failure defeats three existing mechanisms by construction, and that is
//! the whole reason a fourth signal exists:
//!
//! * **The idle watchdog** governs SILENCE on the wire. A socket sending pongs
//!   is not silent, so the 27-second timer never fires.
//! * **The reconnect family** (`tv_dhan_ws_reconnect_total`,
//!   `_dial_failed_total`, `_subscribe_failed_total`) stays flat. Alarming
//!   those was the recommended fix and it cannot catch this case: the defining
//!   property of a deaf socket is that nothing about it is retrying.
//! * **The lane-level tick-age gauge** cannot see it either. With fifteen of
//!   sixteen sockets delivering normally, the lane's last tick is always about
//!   a second old however dead the sixteenth is.
//!
//! So the signal has to be per-connection. It is published as ONE gauge — the
//! worst age across connections — rather than sixteen, because sixteen series
//! is ~$4.80/mo to answer a yes/no question that one series answers with
//! identical detection power. Per-connection attribution stays on `/metrics`,
//! which is where a human triaging actually looks.

use tickvault_app::dhan_feed_stack::{record_connection_tick, worst_connection_tick_age_secs};

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A fixed "now" so nothing here depends on the wall clock.
const NOW_MS: i64 = 1_800_000_000_000;

/// Clears every slot by stamping them all at `NOW_MS`, so a test starts from a
/// known state. There is deliberately no reset API in production: the slots
/// are process-lifetime by design, and adding a reset only tests would call is
/// how a production path acquires a footgun.
fn all_fresh() {
    for i in 0..64u8 {
        record_connection_tick(i, NOW_MS);
    }
}

#[test]
fn one_deaf_socket_among_many_healthy_ones_is_visible() {
    let _s = serial();
    all_fresh();

    // Socket 7 last delivered ten minutes ago; everything else is current.
    record_connection_tick(7, NOW_MS - 600_000);

    let worst = worst_connection_tick_age_secs(NOW_MS).expect("connections have ticked");
    assert_eq!(
        worst, 600,
        "the worst socket must surface at 600 s. This is the entire point: the \
         LANE gauge reads ~0 in this exact situation, because fifteen sockets \
         are delivering — so a signal that averages, or that reports the most \
         recent tick anywhere, reports perfect health while a socket is dead"
    );
}

#[test]
fn all_healthy_reads_near_zero_not_a_stale_worst() {
    let _s = serial();
    all_fresh();
    let worst = worst_connection_tick_age_secs(NOW_MS).expect("connections have ticked");
    assert_eq!(
        worst, 0,
        "with every socket current the gauge must fall back to 0 — a maximum \
         that never recovers is a permanently-red chart, which is a chart \
         nobody reads"
    );
}

#[test]
fn a_recovered_socket_lets_the_gauge_fall() {
    let _s = serial();
    all_fresh();
    record_connection_tick(3, NOW_MS - 900_000);
    assert_eq!(worst_connection_tick_age_secs(NOW_MS), Some(900));

    // Socket 3 starts delivering again.
    record_connection_tick(3, NOW_MS);
    assert_eq!(
        worst_connection_tick_age_secs(NOW_MS),
        Some(0),
        "recovery must be visible. A gauge that only climbs cannot tell an \
         operator whether the fix worked"
    );
}

#[test]
fn a_clock_that_steps_backwards_reads_zero_not_a_giant_age() {
    let _s = serial();
    all_fresh();
    // NTP correction: the stamp is now in the "future" relative to `now`.
    record_connection_tick(2, NOW_MS + 60_000);
    let worst = worst_connection_tick_age_secs(NOW_MS).expect("connections have ticked");
    assert_eq!(
        worst, 0,
        "a backwards clock step must clamp to 0, not wrap into a huge age. A \
         spurious multi-thousand-second reading would page at the one moment \
         the logs are hardest to interpret"
    );
}

#[test]
fn an_out_of_range_index_is_dropped_and_never_corrupts_another_slot() {
    let _s = serial();
    all_fresh();
    // 200 is far past the connection bound. It must not panic, must not wrap
    // into a valid slot, and must not move the gauge — writing the WRONG slot
    // would report the wrong socket healthy, which is worse than not writing.
    record_connection_tick(200, NOW_MS - 3_600_000);
    assert_eq!(
        worst_connection_tick_age_secs(NOW_MS),
        Some(0),
        "an out-of-range index must be dropped, not folded into a real slot"
    );
}

/// The gauge must be WIRED, not merely defined.
///
/// Two stamp sites are required, and the depth one is easy to forget: without
/// it, ten of the sixteen sockets read "never ticked", are excluded from the
/// maximum, and the gauge looks healthy for precisely that reason — a
/// false-OK produced by the exclusion rule that exists to prevent false-OKs.
#[test]
fn both_stamp_sites_and_the_publish_are_wired() {
    let src = include_str!("../src/dhan_feed_stack.rs");
    let production = src.split("#[cfg(test)]").next().unwrap_or(src);
    assert_eq!(
        production.matches("record_connection_tick(").count(),
        3,
        "expected exactly 3 occurrences in production: the fn definition, the \
         main-feed stamp and the DEPTH stamp. Fewer means a socket class has \
         no deaf-socket coverage while the gauge reads healthy"
    );
    assert!(
        production.contains("metrics::gauge!(WORST_CONN_TICK_AGE_GAUGE)"),
        "the gauge must actually be published — a maximum that is tracked and \
         never written is the same defect one layer up"
    );
    // Gated on productive frames, not on arrival: a socket returning frames
    // that all fail to parse is not delivering market data.
    assert!(
        production.contains("if outcome.folded > 0 {"),
        "the main-feed stamp must be gated on folded ticks"
    );
    assert!(
        production.contains("if outcome.rows > 0 {"),
        "the depth stamp must be gated on rows produced"
    );
}
