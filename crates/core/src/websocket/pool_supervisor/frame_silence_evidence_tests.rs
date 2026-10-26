//! Frame-silence evidence tests for `pool_supervisor` (scope lock
//! 2026-10-10, per-kind frame-silence thresholds, plus the K3 300 s fallback
//! spacing). Moved out of `pool_supervisor.rs` on 2026-10-26 so that file stays
//! under its size pin; the tests are unchanged.

use super::*;
use proptest::prelude::*;
use std::cell::Cell;
use std::sync::atomic::AtomicI64;

const WALL0: i64 = 1_800_000_000_000;

fn t0() -> Instant {
    Instant::now()
}

fn slot_at(endpoint: DhanEndpointType, global_index: u8) -> ConnectionSlot {
    ConnectionSlot {
        account: crate::websocket::pool_budget::DhanAccount::Primary,
        endpoint,
        pool_index: 0,
        global_index,
    }
}

/// A live supervisor on `global_index`, opted into evidence, with `held`
/// instruments and fast path `mode`.
fn live_sup(
    endpoint: DhanEndpointType,
    global_index: u8,
    held: u32,
    mode: FrameSilenceFastPath,
    t: Instant,
) -> ConnectionSupervisor {
    let mut s = ConnectionSupervisor::new(slot_at(endpoint, global_index), t);
    s.set_silence_evidence(SiblingEvidence::Pool);
    s.set_fast_path(mode);
    s.set_held_instruments(held);
    let _ = s.on_event(ConnEvent::BeginDial, t);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    let _ = s.on_event(ConnEvent::SubscribeAcked, t);
    assert_eq!(s.phase(), ConnPhase::Live);
    s
}

/// Stamps with the given slots live at `wall`.
fn stamps_live(slots: &[u8], wall: i64) -> [i64; GHOST_REDIAL_SLOTS] {
    let mut out = [0_i64; GHOST_REDIAL_SLOTS];
    for s in slots {
        if let Some(v) = out.get_mut(usize::from(*s)) {
            *v = wall;
        }
    }
    out
}

/// Polls at `t + secs` after a pong (so the IDLE watchdog stays quiet),
/// with the given siblings live at that instant.
fn poll_at(
    s: &mut ConnectionSupervisor,
    t: Instant,
    secs: u64,
    siblings: &[u8],
    overflow_or_halted: bool,
    spacing: &[AtomicI64; CONFIRMED_SPACING_KINDS],
) -> SupervisorAction {
    let at = t + Duration::from_secs(secs);
    let _ = s.on_event(ConnEvent::KeepAliveReceived, at);
    let wall = WALL0 + i64::try_from(secs).unwrap_or(0) * 1_000;
    s.poll_with(at, || SilenceInputs {
        wall_ms: wall,
        overflow_or_halted,
        stamps: stamps_live(siblings, wall),
        spacing,
    })
}

fn fresh_spacing() -> [AtomicI64; CONFIRMED_SPACING_KINDS] {
    [const { AtomicI64::new(0) }; CONFIRMED_SPACING_KINDS]
}

fn is_redial(a: SupervisorAction) -> bool {
    matches!(a, SupervisorAction::SleepThenDial { .. })
}

#[test]
fn silence_redial_basis_truth_table() {
    use DhanEndpointType::{Depth20, Depth200, MainFeed, OrderUpdate};
    let all = |e, silent, held| silence_redial_basis(e, silent, 60, true, false, 2, held, true);
    // Main feed: confirmed at its threshold with every condition.
    assert_eq!(all(MainFeed, 59, 1_000), None);
    assert_eq!(
        all(MainFeed, 60, 1_000),
        Some(SilenceBasis::SiblingConfirmed)
    );
    assert_eq!(
        all(MainFeed, 299, 1_000),
        Some(SilenceBasis::SiblingConfirmed)
    );
    assert_eq!(all(MainFeed, 300, 1_000), Some(SilenceBasis::Unconfirmed));
    // Each condition alone blocks the confirmed path but never the fallback.
    for (window, overflow, siblings, held, spacing) in [
        (false, false, 2, 1_000, true),
        (true, true, 2, 1_000, true),
        (true, false, 1, 1_000, true),
        (true, false, 2, 999, true),
        (true, false, 2, 1_000, false),
    ] {
        assert_eq!(
            silence_redial_basis(MainFeed, 120, 60, window, overflow, siblings, held, spacing),
            None
        );
        assert_eq!(
            silence_redial_basis(MainFeed, 300, 60, window, overflow, siblings, held, spacing),
            Some(SilenceBasis::Unconfirmed)
        );
    }
    // Depth-20: its own floor.
    assert_eq!(
        silence_redial_basis(Depth20, 90, 90, true, false, 2, 20, true),
        Some(SilenceBasis::SiblingConfirmed)
    );
    assert_eq!(
        silence_redial_basis(Depth20, 90, 90, true, false, 2, 19, true),
        None
    );
    // Depth-200: time alone, only at the backstop.
    assert_eq!(all(Depth200, 300, 1), None);
    assert_eq!(all(Depth200, 899, 1), None);
    assert_eq!(all(Depth200, 900, 1), Some(SilenceBasis::Backstop));
    // Order update: never.
    assert_eq!(all(OrderUpdate, 100_000, 1), None);
    // Labels are distinct and stable.
    let labels: Vec<&str> = SilenceBasis::ALL.iter().map(|b| b.as_str()).collect();
    assert_eq!(
        labels,
        ["unconfirmed", "sibling_confirmed", "cross_feed", "backstop"]
    );
}

#[test]
fn live_siblings_counts_same_kind_other_slots_within_10s_only() {
    let now = WALL0;
    let mut stamps = [0_i64; GHOST_REDIAL_SLOTS];
    // Main-feed slots 0..5. Self is 0.
    stamps[0] = now; // self: never counted
    stamps[1] = now - 10_000; // exactly 10 s old: counted
    stamps[2] = now - 10_001; // just past: not counted
    stamps[3] = now + 5_000; // 5 s ahead: counted (skew allowance)
    stamps[4] = now + 5_001; // further ahead: not counted
    stamps[5] = now; // depth-20 slot: other kind
    stamps[10] = now; // depth-200 slot: other kind
    assert_eq!(
        live_siblings(0, DhanEndpointType::MainFeed, now, &stamps),
        2
    );
    // Depth-20 seen from slot 6: slot 5 is live, the depth account's
    // depth-20 slots (16..21) count as the same kind.
    stamps[16] = now - 1;
    assert_eq!(live_siblings(6, DhanEndpointType::Depth20, now, &stamps), 2);
    // Never-delivered (0) and negative stamps never count.
    let empty = [0_i64; GHOST_REDIAL_SLOTS];
    assert_eq!(live_siblings(0, DhanEndpointType::MainFeed, now, &empty), 0);
    // A short slice is handled.
    assert_eq!(
        live_siblings(0, DhanEndpointType::MainFeed, now, &[now, now]),
        1
    );
}

proptest! {
    /// Random inter-frame gaps, every one below the kind's confirmed
    /// threshold, with two siblings live and a poll every second inside
    /// each gap: no fast-path mode ever produces a redial.
    #[test]
    fn proptest_gaps_below_the_confirmed_threshold_never_redial(
        depth20 in any::<bool>(),
        mode_pick in 0_u8..3,
        gaps in proptest::collection::vec(1_u64..1_000, 1..40),
    ) {
        let endpoint = if depth20 {
            DhanEndpointType::Depth20
        } else {
            DhanEndpointType::MainFeed
        };
        let mode = match mode_pick {
            0 => FrameSilenceFastPath::Off,
            1 => FrameSilenceFastPath::Shadow,
            _ => FrameSilenceFastPath::Act,
        };
        let t = t0();
        let spacing = fresh_spacing();
        let mut s = live_sup(endpoint, 3, 5_000, mode, t);
        let threshold = s.confirmed_threshold_secs();
        prop_assert!(threshold >= MAIN_FEED_CONFIRMED_SILENCE_REDIAL_SECS);
        let mut now_secs = 0_u64;
        let _ = s.on_event(ConnEvent::FrameReceived, t);
        for raw in gaps {
            // Map onto [1, threshold - 1]: always below the threshold.
            let gap = 1 + raw % (threshold - 1);
            for step in 1..=gap {
                let action = poll_at(&mut s, t, now_secs + step, &[0, 1], false, &spacing);
                prop_assert!(
                    !is_redial(action),
                    "redial after {step} s of a {gap} s gap (threshold {threshold} s, {mode:?})"
                );
            }
            now_secs += gap;
            let _ = s.on_event(
                ConnEvent::FrameReceived,
                t + Duration::from_secs(now_secs),
            );
        }
        prop_assert_eq!(s.silence_strikes(), 0);
    }
}

#[test]
fn main_feed_confirmed_redial_at_60s_with_two_live_siblings_in_act_mode() {
    let t = t0();
    let spacing = fresh_spacing();
    let mut s = live_sup(
        DhanEndpointType::MainFeed,
        0,
        5_000,
        FrameSilenceFastPath::Act,
        t,
    );
    assert_eq!(s.confirmed_threshold_secs(), 60);
    assert_eq!(
        poll_at(&mut s, t, 59, &[1, 2], false, &spacing),
        SupervisorAction::Continue
    );
    assert!(is_redial(poll_at(&mut s, t, 60, &[1, 2], false, &spacing)));
    assert_eq!(s.last_silence_basis(), SilenceBasis::SiblingConfirmed);
    assert_eq!(s.last_redial_reason(), ReconnectReason::IdleSilence);
    assert_eq!(s.silence_strikes(), 1);
    assert_eq!(
        spacing[0].load(std::sync::atomic::Ordering::Relaxed),
        WALL0 + 60_000,
        "the act path stamps the pool-wide spacing"
    );
    assert_eq!(spacing[1].load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn shadow_mode_counts_once_per_episode_and_never_redials() {
    let t = t0();
    let spacing = fresh_spacing();
    let mut s = live_sup(
        DhanEndpointType::MainFeed,
        1,
        5_000,
        FrameSilenceFastPath::Shadow,
        t,
    );
    for secs in [60, 61, 120, 299] {
        assert_eq!(
            poll_at(&mut s, t, secs, &[0, 2], false, &spacing),
            SupervisorAction::Continue
        );
    }
    assert_eq!(s.shadow_would_redials(), 1, "once per episode");
    assert_eq!(
        spacing[0].load(std::sync::atomic::Ordering::Relaxed),
        0,
        "shadow never stamps the spacing"
    );
    // A frame ends the episode; a new silence counts again.
    let _ = s.on_event(ConnEvent::FrameReceived, t + Duration::from_secs(299));
    assert_eq!(
        poll_at(&mut s, t, 299 + 60, &[0, 2], false, &spacing),
        SupervisorAction::Continue
    );
    assert_eq!(s.shadow_would_redials(), 2);
    // The fallback still governs.
    assert!(is_redial(poll_at(
        &mut s,
        t,
        299 + 300,
        &[0, 2],
        false,
        &spacing
    )));
    assert_eq!(s.last_silence_basis(), SilenceBasis::Unconfirmed);
    // Off never even reads the evidence.
    let reads = Cell::new(0_u32);
    let mut off = live_sup(
        DhanEndpointType::MainFeed,
        2,
        5_000,
        FrameSilenceFastPath::Off,
        t,
    );
    let at = t + Duration::from_secs(120);
    let _ = off.on_event(ConnEvent::KeepAliveReceived, at);
    let action = off.poll_with(at, || {
        reads.set(reads.get() + 1);
        SilenceInputs {
            wall_ms: WALL0,
            overflow_or_halted: false,
            stamps: [WALL0; GHOST_REDIAL_SLOTS],
            spacing: &spacing,
        }
    });
    assert_eq!(action, SupervisorAction::Continue);
    assert_eq!(reads.get(), 0);
    assert_eq!(off.shadow_would_redials(), 0);
}

/// A socket that is deaf on every connection: confirmed redials escalate
/// 60 → 120 → 240 → 300 and then stay at the fallback, so confirmed
/// redials alone can never outpace the flap ceiling.
#[test]
fn escalation_bounds_snapshot_then_deaf_loop() {
    let t = t0();
    let mut s = live_sup(
        DhanEndpointType::MainFeed,
        3,
        5_000,
        FrameSilenceFastPath::Act,
        t,
    );
    let mut elapsed: u64 = 0;
    let mut gaps = Vec::new();
    for _ in 0..6 {
        let start = elapsed;
        let spacing = fresh_spacing();
        loop {
            elapsed += 1;
            if is_redial(poll_at(&mut s, t, elapsed, &[0, 1], false, &spacing)) {
                break;
            }
            assert!(elapsed - start <= 300, "never past the fallback");
        }
        gaps.push(elapsed - start);
        // Redial and come back up, still deaf.
        let at = t + Duration::from_secs(elapsed);
        let _ = s.on_event(ConnEvent::BeginDial, at);
        let _ = s.on_event(ConnEvent::DialSucceeded, at);
        let _ = s.on_event(ConnEvent::SubscribeAcked, at);
    }
    assert_eq!(gaps, [60, 120, 240, 300, 300, 300]);
    assert_eq!(s.silence_strikes(), 6);
    // Strikes reset after SILENCE_STRIKE_RESET with no silence redial.
    let mut quiet = elapsed;
    while quiet < elapsed + SILENCE_STRIKE_RESET_SECS {
        quiet += 30;
        let _ = s.on_event(ConnEvent::FrameReceived, t + Duration::from_secs(quiet));
        let _ = poll_at(&mut s, t, quiet, &[0, 1], false, &fresh_spacing());
    }
    assert_eq!(s.silence_strikes(), 0);
    assert_eq!(s.confirmed_threshold_secs(), 60);
}

#[test]
fn confirmed_redial_spacing_is_pool_wide_per_kind() {
    let t = t0();
    let spacing = fresh_spacing();
    let mut a = live_sup(
        DhanEndpointType::MainFeed,
        0,
        5_000,
        FrameSilenceFastPath::Act,
        t,
    );
    let mut b = live_sup(
        DhanEndpointType::MainFeed,
        1,
        5_000,
        FrameSilenceFastPath::Act,
        t,
    );
    let mut d = live_sup(
        DhanEndpointType::Depth20,
        5,
        50,
        FrameSilenceFastPath::Act,
        t,
    );
    assert!(is_redial(poll_at(&mut a, t, 60, &[2, 3], false, &spacing)));
    // The second main-feed socket waits out the spacing.
    for secs in 60..75 {
        assert_eq!(
            poll_at(&mut b, t, secs, &[2, 3], false, &spacing),
            SupervisorAction::Continue,
            "+{secs}s"
        );
    }
    assert!(is_redial(poll_at(&mut b, t, 75, &[2, 3], false, &spacing)));
    // Depth-20 has its own spacing slot.
    assert!(is_redial(poll_at(&mut d, t, 90, &[6, 7], false, &spacing)));
}

/// Plan item K3: a market-wide stall silences all five main-feed sockets
/// at once. With no live sibling there is no confirmed path, so only the
/// 300 s fallback can fire, and before K3 it fired on all five in the
/// same second, every five minutes. Now no two redials of the kind land
/// inside one spacing, and every socket still redials.
#[test]
fn five_silent_main_feed_sockets_never_redial_two_inside_one_spacing() {
    for mode in [
        FrameSilenceFastPath::Off,
        FrameSilenceFastPath::Shadow,
        FrameSilenceFastPath::Act,
    ] {
        let t = t0();
        let spacing = fresh_spacing();
        let mut sockets: Vec<ConnectionSupervisor> = (0..5_u8)
            .map(|i| live_sup(DhanEndpointType::MainFeed, i, 5_000, mode, t))
            .collect();
        let mut redial_secs: Vec<u64> = Vec::new();
        let mut per_socket = [0_u32; 5];
        for secs in 1..=1_800_u64 {
            for (i, s) in sockets.iter_mut().enumerate() {
                // No sibling is live: every socket is silent.
                if is_redial(poll_at(s, t, secs, &[], false, &spacing)) {
                    assert_eq!(s.last_silence_basis(), SilenceBasis::Unconfirmed);
                    redial_secs.push(secs);
                    per_socket[i] += 1;
                    // Comes back up, still deaf: the silence clock restarts.
                    let at = t + Duration::from_secs(secs);
                    let _ = s.on_event(ConnEvent::BeginDial, at);
                    let _ = s.on_event(ConnEvent::DialSucceeded, at);
                    let _ = s.on_event(ConnEvent::SubscribeAcked, at);
                }
            }
        }
        let spacing_secs = u64::try_from(CONFIRMED_REDIAL_SPACING_SECS).unwrap_or(u64::MAX);
        for pair in redial_secs.windows(2) {
            assert!(
                pair[1] - pair[0] >= spacing_secs,
                "{mode:?}: redials at {} s and {} s are inside one {spacing_secs} s spacing",
                pair[0],
                pair[1]
            );
        }
        assert!(
            per_socket.iter().all(|n| *n >= 1),
            "{mode:?}: every socket still redials: {per_socket:?}"
        );
        // 300, 315, 330, 345, 360 for the first round, not 300 x5.
        assert_eq!(&redial_secs[..5], &[300, 315, 330, 345, 360], "{mode:?}");
    }
}

/// The fallback shares the confirmed path's slot: a confirmed redial
/// holds the fallback of another socket of the kind for one spacing.
#[test]
fn a_confirmed_redial_holds_another_sockets_fallback_for_one_spacing() {
    let t = t0();
    let spacing = fresh_spacing();
    let mut a = live_sup(
        DhanEndpointType::MainFeed,
        0,
        5_000,
        FrameSilenceFastPath::Act,
        t,
    );
    // b holds too few instruments for the confirmed path: fallback only.
    let mut b = live_sup(
        DhanEndpointType::MainFeed,
        1,
        MAIN_FEED_CONFIRMED_MIN_HELD - 1,
        FrameSilenceFastPath::Act,
        t,
    );
    // a delivers every second until +240, so its 60 s confirmed threshold falls at
    // +300, the same second b's 300 s fallback falls due. a polls first.
    for secs in 1..300 {
        if secs <= 240 {
            let _ = a.on_event(ConnEvent::FrameReceived, t + Duration::from_secs(secs));
        }
        assert!(!is_redial(poll_at(
            &mut a,
            t,
            secs,
            &[2, 3],
            false,
            &spacing
        )));
        assert!(!is_redial(poll_at(
            &mut b,
            t,
            secs,
            &[2, 3],
            false,
            &spacing
        )));
    }
    assert!(is_redial(poll_at(&mut a, t, 300, &[2, 3], false, &spacing)));
    assert_eq!(a.last_silence_basis(), SilenceBasis::SiblingConfirmed);
    for secs in 300..315 {
        assert_eq!(
            poll_at(&mut b, t, secs, &[2, 3], false, &spacing),
            SupervisorAction::Continue,
            "+{secs}s: the fallback waits out the confirmed redial's spacing"
        );
    }
    assert!(is_redial(poll_at(&mut b, t, 315, &[2, 3], false, &spacing)));
    assert_eq!(b.last_silence_basis(), SilenceBasis::Unconfirmed);
    assert_eq!(
        spacing[0].load(std::sync::atomic::Ordering::Relaxed),
        WALL0 + 315_000
    );
}

#[test]
fn claim_fallback_spacing_free_held_raced_and_clock_stepped_back() {
    let slot = AtomicI64::new(0);
    // Never taken: free.
    assert!(claim_fallback_spacing(&slot, 0, WALL0));
    assert_eq!(slot.load(std::sync::atomic::Ordering::Relaxed), WALL0);
    // Held inside the spacing.
    assert!(!claim_fallback_spacing(&slot, WALL0, WALL0 + 14_999));
    // Free exactly at the spacing.
    assert!(claim_fallback_spacing(&slot, WALL0, WALL0 + 15_000));
    // A lost race: `last` is stale, the CAS fails, nothing is written.
    assert!(!claim_fallback_spacing(&slot, WALL0, WALL0 + 60_000));
    assert_eq!(
        slot.load(std::sync::atomic::Ordering::Relaxed),
        WALL0 + 15_000
    );
    // A stamp slightly ahead (two wall reads microseconds apart): held.
    let ahead = WALL0 + 15_000;
    assert!(!claim_fallback_spacing(&slot, ahead, ahead - 5_000));
    // A stamp far ahead (the wall clock stepped back an hour): free, so
    // the fallback is never held off for the size of a clock step.
    assert!(claim_fallback_spacing(&slot, ahead, ahead - 3_600_000));
    assert_eq!(
        slot.load(std::sync::atomic::Ordering::Relaxed),
        ahead - 3_600_000
    );
}

/// A supervisor not opted into the pool register (every test default,
/// and order update / depth-200 in production) never reads it for the
/// fallback either.
#[test]
fn the_fallback_reads_the_register_only_when_opted_in_on_a_spaced_kind() {
    let t = t0();
    let spacing = fresh_spacing();
    let reads = Cell::new(0_u32);
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::MainFeed, 0), t);
    let _ = s.on_event(ConnEvent::BeginDial, t);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    let _ = s.on_event(ConnEvent::SubscribeAcked, t);
    let at = t + Duration::from_secs(300);
    let _ = s.on_event(ConnEvent::KeepAliveReceived, at);
    let action = s.poll_with(at, || {
        reads.set(reads.get() + 1);
        SilenceInputs {
            wall_ms: WALL0,
            overflow_or_halted: false,
            stamps: [0; GHOST_REDIAL_SLOTS],
            spacing: &spacing,
        }
    });
    assert!(
        is_redial(action),
        "the unspaced default still redials at 300 s"
    );
    assert_eq!(reads.get(), 0);
    // Depth-200 opted in: its 900 s backstop has no spacing slot.
    let mut d = live_sup(
        DhanEndpointType::Depth200,
        10,
        1,
        FrameSilenceFastPath::Act,
        t,
    );
    let at = t + Duration::from_secs(900);
    let _ = d.on_event(ConnEvent::KeepAliveReceived, at);
    let action = d.poll_with(at, || {
        reads.set(reads.get() + 1);
        SilenceInputs {
            wall_ms: WALL0,
            overflow_or_halted: false,
            stamps: [0; GHOST_REDIAL_SLOTS],
            spacing: &spacing,
        }
    });
    assert!(is_redial(action));
    assert_eq!(reads.get(), 0);
}

#[test]
fn below_the_instrument_floor_falls_back_to_300s() {
    let t = t0();
    let spacing = fresh_spacing();
    let mut s = live_sup(
        DhanEndpointType::MainFeed,
        0,
        MAIN_FEED_CONFIRMED_MIN_HELD - 1,
        FrameSilenceFastPath::Act,
        t,
    );
    for secs in [60, 120, 299] {
        assert_eq!(
            poll_at(&mut s, t, secs, &[1, 2, 3], false, &spacing),
            SupervisorAction::Continue
        );
    }
    assert!(is_redial(poll_at(
        &mut s,
        t,
        300,
        &[1, 2, 3],
        false,
        &spacing
    )));
    assert_eq!(s.last_silence_basis(), SilenceBasis::Unconfirmed);
}

#[test]
fn an_engaged_overflow_episode_or_a_halted_rotation_suppresses_every_fast_path() {
    let t = t0();
    let spacing = fresh_spacing();
    let mut s = live_sup(
        DhanEndpointType::Depth20,
        5,
        50,
        FrameSilenceFastPath::Act,
        t,
    );
    for secs in [90, 200, 299] {
        assert_eq!(
            poll_at(&mut s, t, secs, &[6, 7, 8], true, &spacing),
            SupervisorAction::Continue
        );
    }
    assert!(is_redial(poll_at(
        &mut s,
        t,
        300,
        &[6, 7, 8],
        true,
        &spacing
    )));
    assert_eq!(s.last_silence_basis(), SilenceBasis::Unconfirmed);
    // No fast path fired: the only stamp is the 300 s fallback's own
    // spacing claim (plan item K3), taken at the fallback instant.
    assert_eq!(
        spacing[1].load(std::sync::atomic::Ordering::Relaxed),
        WALL0 + 300_000
    );
}

#[test]
fn confirmed_window_open_at_boundaries() {
    let gate = FrameSilenceGate::ContinuousSessionIst;
    let open = CONTINUOUS_SESSION_OPEN_SECS_OF_DAY_IST;
    let close = CONFIRMED_SILENCE_CLOSE_SECS_OF_DAY_IST;
    assert_eq!(close, 15 * 3600 + 15 * 60, "15:15 IST");
    assert!(!gate.confirmed_window_open_at(open - 1));
    assert!(gate.confirmed_window_open_at(open));
    assert!(gate.confirmed_window_open_at(close - 1));
    assert!(!gate.confirmed_window_open_at(close));
    // The closing auction is inside the session gate but outside the
    // confirmed window: the 300 s fallback governs there.
    assert!(gate.is_open_at(close));
    assert!(FrameSilenceGate::AlwaysOn.confirmed_window_open_at(0));
    assert!(!FrameSilenceGate::Off.confirmed_window_open_at(open));
}

#[test]
fn depth200_is_not_redialled_by_time_before_the_900s_backstop() {
    let t = t0();
    let spacing = fresh_spacing();
    let reads = Cell::new(0_u32);
    let mut s = live_sup(
        DhanEndpointType::Depth200,
        10,
        1,
        FrameSilenceFastPath::Act,
        t,
    );
    let poll = |s: &mut ConnectionSupervisor, secs: u64| {
        let at = t + Duration::from_secs(secs);
        let _ = s.on_event(ConnEvent::KeepAliveReceived, at);
        s.poll_with(at, || {
            reads.set(reads.get() + 1);
            SilenceInputs {
                wall_ms: WALL0,
                overflow_or_halted: false,
                stamps: [WALL0; GHOST_REDIAL_SLOTS],
                spacing: &spacing,
            }
        })
    };
    for secs in [60, 300, 600, 899] {
        assert_eq!(poll(&mut s, secs), SupervisorAction::Continue, "+{secs}s");
    }
    assert!(is_redial(poll(&mut s, 900)));
    assert_eq!(s.last_silence_basis(), SilenceBasis::Backstop);
    assert_eq!(reads.get(), 0, "depth-200 never reads sibling evidence");

    // The order-update socket is never redialled for frame silence.
    let mut ou = live_sup(
        DhanEndpointType::OrderUpdate,
        15,
        0,
        FrameSilenceFastPath::Act,
        t,
    );
    assert_eq!(poll(&mut ou, 3_600), SupervisorAction::Continue);
    assert_eq!(ou.phase(), ConnPhase::Live);

    // A cross-feed request redials a live depth-200 socket on its basis.
    let mut x = live_sup(
        DhanEndpointType::Depth200,
        11,
        1,
        FrameSilenceFastPath::Act,
        t,
    );
    x.note_silence_basis(SilenceBasis::CrossFeed);
    assert!(is_redial(x.on_event(
        ConnEvent::FrameSilenceElapsed,
        t + Duration::from_secs(95)
    )));
    assert_eq!(x.last_silence_basis(), SilenceBasis::CrossFeed);
    assert_eq!(x.silence_strikes(), 1);
    // A basis noted while not live is consumed, never carried to a later
    // redial.
    let mut y = ConnectionSupervisor::new(slot_at(DhanEndpointType::Depth200, 12), t);
    y.note_silence_basis(SilenceBasis::CrossFeed);
    assert_eq!(
        y.on_event(ConnEvent::FrameSilenceElapsed, t),
        SupervisorAction::Continue
    );
    let _ = y.on_event(ConnEvent::BeginDial, t);
    let _ = y.on_event(ConnEvent::DialSucceeded, t);
    let _ = y.on_event(ConnEvent::SubscribeAcked, t);
    assert!(is_redial(y.on_event(
        ConnEvent::FrameSilenceElapsed,
        t + Duration::from_secs(1)
    )));
    assert_eq!(y.last_silence_basis(), SilenceBasis::Unconfirmed);
}

#[test]
fn data_silence_request_is_dropped_when_the_dial_generation_moved() {
    let pending = [const { std::sync::atomic::AtomicU64::new(0) }; GHOST_REDIAL_SLOTS];
    let last = [const { AtomicI64::new(0) }; GHOST_REDIAL_SLOTS];
    let cooldown_ms = DATA_SILENCE_REQUEST_COOLDOWN_SECS * 1_000;
    assert_eq!(
        request_data_silence_redial_in(&pending, &last, 3, WALL0, false, 7),
        Ok(())
    );
    assert_eq!(
        request_data_silence_redial_in(&pending, &last, 3, WALL0, false, 7),
        Err(DataSilenceRefusal::StillPending)
    );
    assert_eq!(
        take_data_silence_redial_in(&pending, 3, 7),
        DataSilenceTake::Take
    );
    assert_eq!(
        take_data_silence_redial_in(&pending, 3, 7),
        DataSilenceTake::Nothing
    );
    assert_eq!(
        request_data_silence_redial_in(&pending, &last, 3, WALL0 + cooldown_ms - 1, false, 7),
        Err(DataSilenceRefusal::CoolingDown)
    );
    assert_eq!(
        request_data_silence_redial_in(&pending, &last, 3, WALL0 + cooldown_ms, false, 7),
        Ok(())
    );
    // The socket redialled since the request: the request names an
    // earlier connection and is dropped.
    assert_eq!(
        take_data_silence_redial_in(&pending, 3, 8),
        DataSilenceTake::Stale
    );
    assert_eq!(
        take_data_silence_redial_in(&pending, 3, 8),
        DataSilenceTake::Nothing
    );
    // After an 805 nothing is recorded.
    assert_eq!(
        request_data_silence_redial_in(&pending, &last, 4, WALL0, true, 1),
        Err(DataSilenceRefusal::Halted)
    );
    assert_eq!(
        take_data_silence_redial_in(&pending, 4, 1),
        DataSilenceTake::Nothing
    );
    // Out of range never panics.
    assert_eq!(
        request_data_silence_redial_in(&pending, &last, 200, WALL0, false, 1),
        Err(DataSilenceRefusal::OutOfRange)
    );
    assert_eq!(
        take_data_silence_redial_in(&pending, 200, 1),
        DataSilenceTake::Nothing
    );
    assert_eq!(DataSilenceRefusal::Halted.as_str(), "halted");
}

#[test]
fn a_taken_data_silence_request_after_805_is_refused_with_path_data_silence() {
    let src = include_str!("../pool_supervisor.rs");
    let prod = src.split("#[cfg(test)]").next().expect("production half");
    let take = prod
        .find("&& take_data_silence_redial(supervisor.slot().global_index)")
        .expect("the connection task takes cross-feed requests");
    let rest = &prod[take..];
    let halted = rest.find("if rotation_halted() {").expect("805 check");
    let refused = rest
        .find("refuse_voluntary_redial_after_805(supervisor.slot(), \"data_silence\")")
        .expect("refused with its own path label");
    let basis = rest
        .find("supervisor.note_silence_basis(SilenceBasis::CrossFeed)")
        .expect("labelled cross-feed");
    let redial = rest
        .find("supervisor.on_event(ConnEvent::FrameSilenceElapsed")
        .expect("redials as a fault");
    assert!(halted < refused && refused < basis && basis < redial);
    // The request side refuses after an 805 too.
    assert!(
        prod.contains("        rotation_halted(),\n        dial_generation(connection_index),")
    );
}

#[test]
fn depth200_held_round_trips_and_never_returns_a_torn_id() {
    let idx = 31;
    let a = SubscribeInstrument {
        security_id: 72_271,
        segment: ExchangeSegment::NseFno,
    };
    let b = SubscribeInstrument {
        security_id: 72_272,
        segment: ExchangeSegment::NseFno,
    };
    publish_depth200_held(idx, None, 0);
    assert_eq!(depth200_held(idx), None);
    publish_depth200_held(idx, Some(a), 1_000);
    assert_eq!(
        depth200_held(idx),
        Some((72_271, ExchangeSegment::NseFno, 1_000))
    );
    // The same contract again keeps its original start.
    publish_depth200_held(idx, Some(a), 5_000);
    assert_eq!(
        depth200_held(idx),
        Some((72_271, ExchangeSegment::NseFno, 1_000))
    );
    publish_depth200_held(idx, Some(b), 6_000);
    assert_eq!(
        depth200_held(idx),
        Some((72_272, ExchangeSegment::NseFno, 6_000))
    );
    // An id that cannot be packed clears the slot rather than lying.
    publish_depth200_held(
        idx,
        Some(SubscribeInstrument {
            security_id: u64::MAX,
            segment: ExchangeSegment::NseFno,
        }),
        7_000,
    );
    assert_eq!(depth200_held(idx), None);
    publish_depth200_held(idx, None, 0);
    assert_eq!(depth200_held(idx), None);
    // Out of range never panics.
    publish_depth200_held(200, Some(a), 1);
    assert_eq!(depth200_held(200), None);
}

#[test]
fn publish_depth200_held_is_called_at_every_held_set_change() {
    let src = include_str!("../pool_supervisor.rs");
    let prod = src.split("#[cfg(test)]").next().expect("production half");
    let lines: Vec<&str> = prod.lines().map(str::trim_start).collect();
    let mut sized = 0;
    let mut zeroed = 0;
    for (i, line) in lines.iter().enumerate() {
        let next = lines.get(i + 1).copied().unwrap_or("");
        if line.starts_with("publish_connection_instruments(&supervisor.slot(), guard.len())") {
            sized += 1;
            assert!(next.starts_with("note_held_set("), "line {i}: {next}");
        }
        if line.starts_with("publish_connection_instruments(&supervisor.slot(), 0)") {
            zeroed += 1;
            assert!(next.starts_with("note_held_cleared("), "line {i}: {next}");
        }
    }
    assert_eq!((sized, zeroed), (5, 2));
    // The swap arm (which can change the depth-200 contract) and its
    // emptied path are covered too.
    assert!(prod.contains("                    note_held_set(supervisor, guard);\n                }\n                Ok(LiveSubscriptionCommand::ProbeUnsubscribe {"));
    let emptied = prod
        .find("metrics::counter!(SWAP_EMPTIED_SOCKET_METRIC).increment(1);")
        .expect("emptied path");
    let cleared = prod[emptied..]
        .find("note_held_cleared(supervisor);")
        .expect("emptied socket clears the register");
    let failed = prod[emptied..]
        .find(".on_event(ConnEvent::SubscribeFailed, Instant::now())")
        .expect("emptied socket redials");
    assert!(cleared < failed);
    // Both backoff arms clear the register before they sleep.
    for arm in [
        "SupervisorAction::SleepThenDial { delay_ms } => {",
        "SupervisorAction::RefreshTokenThenDial { delay_ms } => {",
    ] {
        let start = prod.find(arm).expect("backoff arm");
        let body = &prod[start..];
        let cleared = body
            .find("note_held_cleared(&mut supervisor);")
            .expect("backoff clears the register");
        let slept = body.find("sleep_unless_stopped(").expect("backoff sleeps");
        assert!(cleared < slept, "{arm}");
    }
    let set_fn = prod
        .find("fn note_held_set(")
        .expect("note_held_set exists");
    let set_body = &prod[set_fn..];
    let set_body = &set_body[..set_body.find("\n}\n").expect("end of note_held_set")];
    let squashed: String = set_body.split_whitespace().collect();
    assert!(
        squashed.contains(
            "publish_depth200_held(supervisor.slot().global_index,guard.first(),wall_clock_ms(),);"
        ),
        "note_held_set publishes the depth-200 contract: {set_body}"
    );
}

#[test]
fn frame_activity_published_at_most_once_per_second() {
    let idx = 30;
    let t = t0();
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::MainFeed, idx), t);
    let _ = s.on_event(ConnEvent::BeginDial, t);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    let _ = s.on_event(ConnEvent::SubscribeAcked, t);
    let _ = s.on_event(ConnEvent::FrameReceived, t);
    let first = last_frame_wall_ms(idx).expect("published on the first frame");
    assert!(first > 0);
    // Overwrite with a sentinel: a frame inside the second must not
    // republish.
    SLOT_LAST_FRAME_WALL_MS[usize::from(idx)]
        .0
        .store(1, std::sync::atomic::Ordering::Relaxed);
    let _ = s.on_event(ConnEvent::FrameReceived, t + Duration::from_millis(500));
    let _ = s.on_event(ConnEvent::FrameReceived, t + Duration::from_millis(999));
    assert_eq!(last_frame_wall_ms(idx), Some(1));
    let _ = s.on_event(ConnEvent::FrameReceived, t + Duration::from_millis(1_000));
    assert!(last_frame_wall_ms(idx).unwrap_or(0) >= first);
    assert_eq!(last_frame_wall_ms(200), None);
    assert_eq!(snapshot_frame_activity().len(), GHOST_REDIAL_SLOTS);
}

#[test]
fn test_subscribed_wall_ms_is_stamped_at_the_subscribe_ack() {
    // PR54: the deaf-socket gauge ages a never-delivering socket from its
    // subscribe. The stamp is taken at the ack, before any frame.
    let idx = 28;
    let t = t0();
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::Depth20, idx), t);
    let _ = s.on_event(ConnEvent::BeginDial, t);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    let before = wall_clock_ms();
    let _ = s.on_event(ConnEvent::SubscribeAcked, t);
    let stamp = subscribed_wall_ms(idx).expect("stamped at the subscribe ack");
    assert!(stamp >= before, "the stamp is the ack's wall time");
    assert_eq!(subscribed_wall_ms(200), None, "out of range reads None");
}

#[test]
fn frame_gap_peak_is_the_largest_gap_inside_the_window() {
    let idx = 29;
    let t = t0();
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::MainFeed, idx), t);
    let _ = s.on_event(ConnEvent::BeginDial, t);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    let _ = s.on_event(ConnEvent::SubscribeAcked, t);
    // The first poll opens the window (AlwaysOn) and resets the peak.
    assert_eq!(s.poll(t), SupervisorAction::Continue);
    // The first frame's gap is the dial, not a gap between frames.
    let _ = s.on_event(ConnEvent::FrameReceived, t + Duration::from_secs(20));
    assert_eq!(frame_gap_peak_secs(idx), Some(0));
    let _ = s.on_event(ConnEvent::FrameReceived, t + Duration::from_secs(25));
    let _ = s.on_event(ConnEvent::FrameReceived, t + Duration::from_secs(32));
    let _ = s.on_event(ConnEvent::FrameReceived, t + Duration::from_secs(33));
    assert_eq!(frame_gap_peak_secs(idx), Some(7));
    assert_eq!(frame_gap_peak_secs(200), None);
    publish_frame_gap_peaks();
    seed_frame_silence_counters();
}

/// Slot 27 is written by no other test in this module.
const REGISTER_TEST_SLOT: u8 = 27;

#[test]
fn test_last_frame_wall_ms_and_snapshot_frame_activity_read_the_register() {
    let idx = usize::from(REGISTER_TEST_SLOT);
    SLOT_LAST_FRAME_WALL_MS[idx]
        .0
        .store(WALL0, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(last_frame_wall_ms(REGISTER_TEST_SLOT), Some(WALL0));
    assert_eq!(snapshot_frame_activity()[idx], WALL0);
    // 0 means never delivered: no stamp.
    SLOT_LAST_FRAME_WALL_MS[idx]
        .0
        .store(0, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(last_frame_wall_ms(REGISTER_TEST_SLOT), None);
    assert_eq!(snapshot_frame_activity()[idx], 0);
    assert_eq!(last_frame_wall_ms(u8::MAX), None);
}

#[test]
fn test_frame_gap_peak_secs_rounds_down_and_publish_frame_gap_peaks_reads_it() {
    let idx = usize::from(REGISTER_TEST_SLOT);
    SLOT_FRAME_GAP_PEAK_MS[idx].store(7_999, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(frame_gap_peak_secs(REGISTER_TEST_SLOT), Some(7));
    SLOT_FRAME_GAP_PEAK_MS[idx].store(999, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(frame_gap_peak_secs(REGISTER_TEST_SLOT), Some(0));
    assert_eq!(frame_gap_peak_secs(u8::MAX), None);
    // Publishing reads the register and never panics without a recorder.
    publish_frame_gap_peaks();
    SLOT_FRAME_GAP_PEAK_MS[idx].store(0, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn test_seed_frame_silence_counters_is_idempotent() {
    // Seeding adds zero, so running it twice (two lane starts) is safe.
    seed_frame_silence_counters();
    seed_frame_silence_counters();
    assert_eq!(
        DATA_SILENCE_STALE_METRIC,
        "tv_dhan_ws_data_silence_request_stale_total"
    );
}

#[test]
fn test_overflow_episode_engaged_reads_the_never_cleared_flag() {
    assert_eq!(
        overflow_episode_engaged(),
        OVERFLOW_ENGAGED.load(std::sync::atomic::Ordering::Acquire)
    );
}

#[test]
fn test_confirmed_threshold_secs_doubles_per_strike_and_caps_at_300() {
    let t = t0();
    for (endpoint, expected) in [
        (DhanEndpointType::MainFeed, [60, 120, 240, 300, 300]),
        (DhanEndpointType::Depth20, [90, 180, 300, 300, 300]),
        (DhanEndpointType::Depth200, [300, 300, 300, 300, 300]),
    ] {
        let mut s = ConnectionSupervisor::new(slot_at(endpoint, REGISTER_TEST_SLOT), t);
        for (strikes, want) in expected.into_iter().enumerate() {
            s.silence_strikes = u8::try_from(strikes).expect("small");
            assert_eq!(s.confirmed_threshold_secs(), want, "{endpoint:?} {strikes}");
        }
        s.silence_strikes = u8::MAX;
        assert_eq!(s.confirmed_threshold_secs(), FRAME_SILENCE_REDIAL_SECS);
    }
}

proptest! {
    /// Frames that never leave a gap as long as the shortest confirmed
    /// threshold never earn a silence redial, whatever the evidence says.
    #[test]
    fn gaps_below_the_threshold_never_redial(
        gaps in proptest::collection::vec(1_u64..60, 1..20),
        endpoint_pick in 0_u8..2,
    ) {
        let endpoint = if endpoint_pick == 0 {
            DhanEndpointType::MainFeed
        } else {
            DhanEndpointType::Depth20
        };
        let base = endpoint.jitter_base();
        let t = t0();
        let spacing = fresh_spacing();
        let mut s = live_sup(endpoint, base, 5_000, FrameSilenceFastPath::Act, t);
        let siblings = [base + 1, base + 2, base + 3];
        let mut at: u64 = 0;
        for gap in gaps {
            for _ in 0..gap {
                at += 1;
                let action = poll_at(&mut s, t, at, &siblings, false, &spacing);
                prop_assert_eq!(action, SupervisorAction::Continue);
            }
            let _ = s.on_event(ConnEvent::FrameReceived, t + Duration::from_secs(at));
        }
        prop_assert_eq!(s.silence_strikes(), 0);
        prop_assert_eq!(s.reconnects(), 0);
    }
}
