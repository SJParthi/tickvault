//! The ring-dwell gauge: the one live-lane signal that rises BEFORE the loss.
//!
//! # What this pins and why it is not a source scan
//!
//! `run_frame_drain` has always derived how long a frame sat in the ring —
//! `queued_nanos`, computed on every frame at roughly 5,000 frames per second
//! — used it once to back-date `received_at_nanos`, and thrown it away. Every
//! other ring signal the lane publishes is an after-the-fact count:
//! `tv_dhan_ws_ring_full_total` and `tv_dhan_ws_frame_refused_total` move only
//! once frames have already been turned away. Dwell climbs while there is
//! still headroom, which is the only window in which anyone can act.
//!
//! Two properties carry the whole value of the signal and neither is visible
//! in the shape of the code:
//!
//! * it must keep the MAXIMUM, not the last value or a mean — one long dwell
//!   inside a window of short ones is exactly the sample that matters, and any
//!   averaging erases it;
//! * it must RESET on publish — a sticky maximum reads alarming forever after
//!   a single stall, and a permanently-red signal is one nobody reads.
//!
//! A source scan cannot check either. It can check that `fetch_max` appears;
//! it cannot check that the value actually survives a smaller sample or that
//! the second read comes back empty. So these are behavioural.

use tickvault_app::dhan_feed_stack::{record_ring_dwell, take_ring_dwell_max_ms};

/// Serialises the tests: they share one process-global maximum.
///
/// Without this they race — test A's `take` drains test B's recorded value and
/// B asserts against zero. Written as an explicit mutex rather than relying on
/// `--test-threads=1`, which is a property of how the suite is INVOKED and
/// therefore not a property anyone can rely on.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn the_gauge_keeps_the_worst_sample_not_the_last() {
    let _s = serial();
    take_ring_dwell_max_ms(); // clear whatever a previous test left

    // A long stall FOLLOWED by short frames — the ordering that a
    // last-value-wins implementation gets wrong and that a max gets right.
    record_ring_dwell(8_000_000_000); // 8 s: the drain stopped draining
    record_ring_dwell(1_000_000); //     1 ms
    record_ring_dwell(2_000_000); //     2 ms

    let ms = take_ring_dwell_max_ms();
    assert!(
        (ms - 8_000.0).abs() < 0.001,
        "expected the 8-second stall to survive the three short frames that \
         followed it, got {ms} ms. A signal that reports the LAST dwell says \
         'fine' at the exact moment the drain has stopped — which is the \
         failure this metric exists to catch"
    );
}

#[test]
fn the_gauge_resets_on_read_so_one_stall_is_not_permanent() {
    let _s = serial();
    take_ring_dwell_max_ms();

    record_ring_dwell(5_000_000_000);
    let first = take_ring_dwell_max_ms();
    assert!(
        (first - 5_000.0).abs() < 0.001,
        "the recorded stall must be reported once, got {first} ms"
    );

    let second = take_ring_dwell_max_ms();
    assert!(
        second.abs() < f64::EPSILON,
        "the second read must be 0, got {second} ms. A sticky maximum keeps \
         reporting a stall that ended hours ago, and a chart that is \
         permanently red is a chart nobody looks at — which costs more than \
         not having published it at all"
    );
}

#[test]
fn a_quiet_window_publishes_zero_not_a_stale_value() {
    let _s = serial();
    take_ring_dwell_max_ms();
    // No frames at all this window.
    let ms = take_ring_dwell_max_ms();
    assert!(
        ms.abs() < f64::EPSILON,
        "a window with no frames must publish 0, got {ms}. Carrying the \
         previous window forward would make an idle lane indistinguishable \
         from a stalled one"
    );
}

#[test]
fn nanoseconds_convert_to_milliseconds_not_the_other_way_round() {
    let _s = serial();
    take_ring_dwell_max_ms();

    // 1,500,000 ns = 1.5 ms. Getting the direction wrong yields 1.5e9, which
    // would look like a 17-day stall and is exactly the kind of unit error
    // that survives review because "the number is big and the situation is
    // bad" reads as consistent.
    record_ring_dwell(1_500_000);
    let ms = take_ring_dwell_max_ms();
    assert!(
        (ms - 1.5).abs() < 0.0001,
        "1,500,000 ns must publish as 1.5 ms, got {ms}"
    );
}

#[test]
fn a_zero_dwell_is_recordable_and_does_not_poison_the_maximum() {
    let _s = serial();
    take_ring_dwell_max_ms();

    // The healthy steady state: the drain reaches every frame immediately.
    record_ring_dwell(0);
    record_ring_dwell(0);
    record_ring_dwell(3_000_000);
    record_ring_dwell(0);

    let ms = take_ring_dwell_max_ms();
    assert!(
        (ms - 3.0).abs() < 0.0001,
        "zeros must not displace a real sample, got {ms} ms"
    );
}

/// The metric must actually be PUBLISHED, not merely computed.
///
/// This is the one source-level check here, and it earns its place: the defect
/// being fixed is precisely "the number was computed and never published", so
/// a test suite that verified only the arithmetic would pass on the broken
/// code it exists to replace.
#[test]
fn the_gauge_is_published_and_the_recorder_is_wired_into_the_drain() {
    let src = include_str!("../src/dhan_feed_stack.rs");
    let production = src.split("#[cfg(test)]").next().unwrap_or(src);
    assert!(
        production.contains("record_ring_dwell(queued_nanos)"),
        "the drain must record the dwell it already computes — without this \
         call the whole signal is dead code with a gauge in front of it"
    );
    assert!(
        production
            .contains("metrics::gauge!(RING_DWELL_MAX_MS_GAUGE).set(take_ring_dwell_max_ms())"),
        "the gauge must be SET on the periodic publish; a maximum that is \
         collected and never written is the same defect one layer up"
    );
    // It rides the existing periodic publish. A dedicated timer would be a new
    // `select!` arm on the drain loop — a new way to starve the very loop this
    // metric measures the starvation of.
    let publish_fn = production
        .find("fn publish_fold_depth")
        .expect("the periodic publish must exist");
    let body = &production[publish_fn..(publish_fn + 1_400).min(production.len())];
    assert!(
        body.contains("RING_DWELL_MAX_MS_GAUGE"),
        "the dwell publish must sit inside `publish_fold_depth`, not on a \
         timer of its own"
    );
}

/// The microsecond conversion SATURATES rather than wrapping.
///
/// `take_ring_dwell_max_ms` goes through `u32` micros to stay lossless (the
/// house `u32::try_from` + `f64::from` pattern, chosen over an
/// `#[allow(clippy::cast_precision_loss)]` on a bare `as`). That buys a
/// ceiling: `u32::MAX` micros is roughly 71 minutes.
///
/// The ceiling is fine; WRAPPING would not be. A dwell past the ceiling
/// reporting as a small number would say "the drain is fine" at the single
/// worst moment it could say it — the inverse of the signal — so the boundary
/// gets its own test rather than resting on `try_from`'s reputation.
#[test]
fn a_dwell_past_the_ceiling_saturates_and_never_wraps_to_a_small_number() {
    let _s = serial();
    take_ring_dwell_max_ms();

    // Two hours of dwell: well past the ~71-minute u32-micros ceiling.
    record_ring_dwell(7_200_000_000_000);
    let ms = take_ring_dwell_max_ms();
    assert!(
        ms > 4_000_000.0,
        "an over-ceiling dwell must saturate HIGH, got {ms} ms. Wrapping to a \
         small value would report a healthy drain at the exact moment it has \
         been broken for over an hour"
    );

    // And the absolute worst case must behave the same way, not overflow the
    // intermediate division.
    take_ring_dwell_max_ms();
    record_ring_dwell(i64::MAX);
    let ms = take_ring_dwell_max_ms();
    assert!(
        ms > 4_000_000.0,
        "i64::MAX must saturate high too, got {ms} ms"
    );
}
