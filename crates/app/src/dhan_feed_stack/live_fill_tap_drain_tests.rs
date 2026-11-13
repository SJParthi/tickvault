//! LF-6b: the live paper fill tap on the frame drain. The drain hands the
//! chain only ticks the fold accepted, from a live socket, outside a WAL
//! re-fold.

use super::live_fill_tap::{LiveFillTap, TapTick, live_fill_tap_ring};
use super::*;

/// An in-session second (IST epoch seconds) at or before now, so the receipt
/// never lies in the future whatever time the suite runs: now while the
/// session is open, else 10:00:00 IST of today (after the close) or of the
/// day before (before the open).
fn in_session_ltt() -> u32 {
    let ist_now = chrono::Utc::now().timestamp().saturating_add(i64::from(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ));
    let sod = ist_now.rem_euclid(86_400);
    let midnight = ist_now - sod;
    let at = if (33_301..55_800).contains(&sod) {
        ist_now - 1
    } else if sod >= 55_800 {
        midnight + 36_000
    } else {
        midnight - 86_400 + 36_000
    };
    u32::try_from(at).unwrap_or(0)
}

fn recv_nanos_for(ltt: u32) -> i64 {
    (i64::from(ltt) - i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS))
        .saturating_mul(1_000_000_000)
}

fn ticker(security_id: u32, ltp: f32, ltt: u32) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = 2; // ticker
    p[1] = 16;
    p[3] = ExchangeSegment::NseEquity.binary_code();
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[8..12].copy_from_slice(&ltp.to_le_bytes());
    p[12..16].copy_from_slice(&ltt.to_le_bytes());
    p
}

fn frame(bytes: &[u8], connection_index: u8, recv: i64) -> CapturedFrame {
    CapturedFrame {
        seq: 1 << 22,
        endpoint: DhanEndpointType::MainFeed,
        connection_index,
        received_at: std::time::Instant::now(),
        received_at_nanos: recv,
        wal_backed: true,
        bytes: bytes::Bytes::copy_from_slice(bytes),
    }
}

fn tapped_ingest() -> (
    LiveIngest,
    tickvault_core::websocket::frame_ring::FrameRingReceiver<TapTick>,
) {
    let (tx, rx) = live_fill_tap_ring(8);
    let ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_live_fill_tap(LiveFillTap::with_shed_source(tx, || 0));
    (ingest, rx)
}

#[test]
fn test_with_live_fill_tap_hands_a_folded_live_tick_to_the_chain() {
    let (mut ingest, mut rx) = tapped_ingest();
    assert_eq!(ingest.live_fill_tap_dropped(), Some(0));
    let ltt = in_session_ltt();
    let recv = recv_nanos_for(ltt);
    let out = drain_main_feed_frame(
        &mut ingest,
        &frame(&ticker(2885, 101.5, ltt), 0, recv),
        recv,
        1_000,
        counters(),
    );
    assert_eq!(out.folded, 1);
    let t = rx.try_recv().expect("the folded tick reached the tap");
    assert_eq!(t.security_id, 2885);
    assert_eq!(t.segment, ExchangeSegment::NseEquity.binary_code());
    assert_eq!(t.exchange_secs, ltt);
    assert!((t.ltp - 101.5).abs() < 1e-6);
    assert_eq!(
        t.receipt_ist_ns,
        recv + tickvault_trading::brutex_host::live_candles::IST_OFFSET_NS
    );
    assert_eq!(t.best_bid, 0.0, "a ticker packet has no depth");
    assert!(rx.try_recv().is_none(), "exactly one tick");
}

#[test]
fn test_tap_skips_replayed_frames_wal_refold_and_unfolded_ticks_live_fill_tap_dropped() {
    let ltt = in_session_ltt();
    let recv = recv_nanos_for(ltt);
    // A replayed WAL frame (connection u8::MAX): folded, never tapped.
    let (mut ingest, mut rx) = tapped_ingest();
    let out = drain_main_feed_frame(
        &mut ingest,
        &frame(&ticker(1, 100.0, ltt), u8::MAX, recv),
        recv,
        1_000,
        counters(),
    );
    assert_eq!(out.folded, 1);
    assert!(rx.try_recv().is_none(), "a replayed frame never taps");
    // While the WAL backlog is re-folded: never tapped.
    let (mut ingest, mut rx) = tapped_ingest();
    ingest.replaying_wal = true;
    drain_main_feed_frame(
        &mut ingest,
        &frame(&ticker(2, 100.0, ltt), 0, recv),
        recv,
        1_000,
        counters(),
    );
    assert!(rx.try_recv().is_none(), "nothing taps during a WAL re-fold");
    // A pre-open tick (08:30 IST) is written out of session, not folded:
    // never tapped.
    let (mut ingest, mut rx) = tapped_ingest();
    let pre = ltt - ltt % 86_400 + 30_600;
    let pre_recv = recv_nanos_for(pre);
    drain_main_feed_frame(
        &mut ingest,
        &frame(&ticker(3, 100.0, pre), 0, pre_recv),
        pre_recv,
        1_000,
        counters(),
    );
    assert!(rx.try_recv().is_none(), "only Folded ticks reach the chain");
    // A full tap ring drops and counts, the drain carries on.
    let (tx, _rx_full) = live_fill_tap_ring(1);
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_live_fill_tap(LiveFillTap::new(tx));
    for i in 0..3u32 {
        let out = drain_main_feed_frame(
            &mut ingest,
            &frame(&ticker(10 + i, 100.0, ltt), 0, recv),
            recv,
            1_000,
            counters(),
        );
        assert_eq!(out.folded, 1);
    }
    assert_eq!(ingest.live_fill_tap_dropped(), Some(2));
    // No tap armed: nothing to count.
    let plain = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    assert_eq!(plain.live_fill_tap_dropped(), None);
}

/// The tap call sits inside the `Folded` arm and behind both gates.
#[test]
fn test_live_fill_tap_call_sits_inside_the_folded_arm_behind_both_gates() {
    let src = include_str!("../dhan_feed_stack.rs");
    let prod = src.split("\n#[cfg(test)]").next().unwrap_or(src);
    assert_eq!(
        prod.matches("tap.offer(tick, best, new_trade, older_trade, volume)")
            .count(),
        1
    );
    let call = prod
        .find("tap.offer(tick, best, new_trade, older_trade, volume)")
        .expect("tap call");
    let arm = prod[..call]
        .rfind("IngestOutcome::Folded { .. } => {")
        .expect("inside a Folded arm");
    let gate = prod[arm..call]
        .find("live_fill_tap::tap_gate(frame.connection_index, ingest.replaying_wal)")
        .expect("both gates before the call");
    assert!(arm + gate < call);
    assert!(
        !prod[arm..call].contains("IngestOutcome::WrittenOutOfSession"),
        "the call is in the Folded arm, not a later one"
    );
}

/// Finding 2 (LF-6b review): a repeat quote or a first-sight packet carries an
/// old trade time and price; the tap marks it `new_trade = false` so the chain
/// never prices a candle with it. (No capture start is recorded here, so the
/// first packet cannot be proven fresh; see the next test for one that is.)
#[test]
fn test_tap_marks_only_new_trades_as_new_trade() {
    let (mut ingest, mut rx) = tapped_ingest();
    let ltt = in_session_ltt();
    let recv = recv_nanos_for(ltt + 1);
    for (i, t) in [ltt, ltt + 1, ltt + 1].into_iter().enumerate() {
        let out = drain_main_feed_frame(
            &mut ingest,
            &frame(&ticker(2885, 101.5, t), 0, recv),
            recv,
            1_000 + i as u64,
            counters(),
        );
        assert_eq!(out.folded, 1);
    }
    let flags: Vec<bool> = std::iter::from_fn(|| rx.try_recv())
        .map(|t| t.new_trade)
        .collect();
    assert_eq!(
        flags,
        vec![false, true, false],
        "first sight and the repeat are not new trades"
    );
}

/// Finding 1 (LF-6b review): a live frame whose tail the drain abandons moves
/// the next tapped tick's `drop_seq`; a replayed frame's abandon does not.
#[test]
fn test_abandoned_live_frame_tail_moves_drop_seq_note_live_fill_abandoned() {
    let (mut ingest, mut rx) = tapped_ingest();
    let ltt = in_session_ltt();
    let recv = recv_nanos_for(ltt);
    // One good ticker, then a packet whose code is unknown and whose length
    // stamp cannot be trusted: the walk abandons the rest.
    let mut bytes = ticker(2885, 100.0, ltt).to_vec();
    bytes.extend_from_slice(&[250, 0, 0, 0, 0, 0, 0]);
    let out = drain_main_feed_frame(
        &mut ingest,
        &frame(&bytes, 0, recv),
        recv,
        1_000,
        counters(),
    );
    assert!(out.abandoned_bytes > 0, "the tail was abandoned");
    assert_eq!(rx.try_recv().map(|t| t.drop_seq), Some(0));
    drain_main_feed_frame(
        &mut ingest,
        &frame(&ticker(2886, 100.0, ltt), 0, recv),
        recv,
        1_001,
        counters(),
    );
    assert_eq!(
        rx.try_recv().map(|t| t.drop_seq),
        Some(1),
        "the next tick names the lost tail"
    );
    // A replayed frame (u8::MAX) abandons without moving the count.
    drain_main_feed_frame(
        &mut ingest,
        &frame(&bytes, u8::MAX, recv),
        recv,
        1_002,
        counters(),
    );
    drain_main_feed_frame(
        &mut ingest,
        &frame(&ticker(2887, 100.0, ltt), 0, recv),
        recv,
        1_003,
        counters(),
    );
    assert_eq!(rx.try_recv().map(|t| t.drop_seq), Some(1));
}

/// LF-6b review finding 2: after a pre-market boot, a pre-open packet is
/// refused by the fold (never tapped) and the 09:15:00 opening trade, the
/// first packet the fold accepts, reaches the chain as a NEW trade. A connect
/// snapshot of a trade from before the capture start stays `false`.
#[test]
fn test_opening_trade_after_a_pre_open_packet_reaches_the_chain_as_new_trade() {
    let ltt = in_session_ltt();
    let day = ltt - ltt % 86_400;
    let open = day + 33_300;
    let (mut ingest, mut rx) = tapped_ingest();
    ingest.aggregator.set_live_capture_start(day + 30_600); // an 08:30 boot
    let pre = day + 32_700; // 09:05, the pre-open
    drain_main_feed_frame(
        &mut ingest,
        &frame(&ticker(2885, 100.0, pre), 0, recv_nanos_for(pre)),
        recv_nanos_for(pre),
        1_000,
        counters(),
    );
    assert!(rx.try_recv().is_none(), "the pre-open packet is not folded");
    let out = drain_main_feed_frame(
        &mut ingest,
        &frame(&ticker(2885, 101.0, open), 0, recv_nanos_for(open)),
        recv_nanos_for(open),
        1_001,
        counters(),
    );
    assert_eq!(out.folded, 1);
    let t = rx.try_recv().expect("the opening trade reached the tap");
    assert_eq!(t.exchange_secs, open);
    assert!(t.new_trade, "the 09:15:00 opening trade is a new trade");
    assert!(!t.older_trade);
    // A mid-session boot: the connect snapshot's trade precedes the capture
    // start.
    let (mut ingest, mut rx) = tapped_ingest();
    ingest.aggregator.set_live_capture_start(ltt);
    drain_main_feed_frame(
        &mut ingest,
        &frame(&ticker(2886, 101.0, open), 0, recv_nanos_for(ltt)),
        recv_nanos_for(ltt),
        1_002,
        counters(),
    );
    let snap = rx.try_recv().expect("the snapshot is folded and tapped");
    assert!(
        !snap.new_trade && !snap.older_trade,
        "a snapshot is quote-only"
    );
}

/// LF-6b review finding 5: a live tick the fold folded but whose row did not
/// stage (`WriteFailed`) moves the next tapped tick's `drop_seq`; a replayed
/// frame's does not. The arm itself is pinned by a source scan (the staging
/// append cannot be made to fail from here).
#[test]
fn test_write_failed_tick_moves_drop_seq_note_live_fill_loss() {
    let (mut ingest, mut rx) = tapped_ingest();
    let ltt = in_session_ltt();
    let recv = recv_nanos_for(ltt);
    ingest.note_live_fill_loss(u8::MAX);
    ingest.note_live_fill_loss(0);
    drain_main_feed_frame(
        &mut ingest,
        &frame(&ticker(2885, 100.0, ltt), 0, recv),
        recv,
        1_000,
        counters(),
    );
    assert_eq!(
        rx.try_recv().map(|t| t.drop_seq),
        Some(1),
        "only the live loss counts"
    );
    let src = include_str!("../dhan_feed_stack.rs");
    let prod = src.split("\n#[cfg(test)]").next().unwrap_or(src);
    let arm = prod
        .find("IngestOutcome::WriteFailed => {")
        .expect("the drain's WriteFailed arm");
    let end = prod[arm..]
        .find("\n                    }")
        .expect("arm end");
    assert!(
        prod[arm..arm + end].contains("ingest.note_live_fill_loss(frame.connection_index);"),
        "a folded tick whose row failed must count as a live fill loss"
    );
}

fn quote(security_id: u32, ltp: f32, ltt: u32, volume: u32) -> [u8; 50] {
    let mut p = [0u8; 50];
    p[0] = 4; // quote
    p[1..3].copy_from_slice(&50u16.to_le_bytes());
    p[3] = ExchangeSegment::NseFno.binary_code();
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[8..12].copy_from_slice(&ltp.to_le_bytes());
    p[14..18].copy_from_slice(&ltt.to_le_bytes());
    p[22..26].copy_from_slice(&volume.to_le_bytes());
    p
}

/// LF-9 review, finding 8: the volume the frame drain hands the chain is the
/// fold's own, field for field, through the drain's real plumbing (the
/// `IngestOutcome::Folded` construction and the tap's `offer`), not a test's
/// copy of it. A pre-market boot's opening trade, a stale regression, a
/// `u32` wrap and a counter drop go through `drain_main_feed_frame`; a twin
/// fold built the same way consumes the same packets, and every tapped
/// tick's four volume fields must equal the twin's `ConsumeStats`.
#[test]
fn test_regression_drain_hands_the_chain_the_folds_own_volume_tap_volume_from_stats() {
    let ltt = in_session_ltt();
    let day = ltt - ltt % 86_400;
    let open = day + 33_300;
    let t0 = open; // the opening trade: the fold credits the 09:15 bucket
    let (tx, mut rx) = live_fill_tap_ring(64);
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_live_fill_tap(LiveFillTap::with_shed_source(tx, || 0));
    ingest.aggregator.set_live_capture_start(day + 30_600); // an 08:30 boot
    let mut twin = live_fold(8);
    twin.set_live_capture_start(day + 30_600);
    let script: [(u32, u32); 7] = [
        (t0, 1_000),
        (t0 + 1, 1_500),
        (t0 + 2, 1_200), // a stale regression
        (t0 + 3, 2_000),
        (t0 + 4, u32::MAX - 10),
        (t0 + 5, 40), // a u32 wrap
        (t0 + 6, 5),  // a drop again
    ];
    let (mut tapped, mut stale, mut moved, mut seeds) = (0, 0, 0, 0);
    let mut first_differs = false;
    for (k, &(secs, cum)) in script.iter().enumerate() {
        let recv = recv_nanos_for(secs);
        let bytes = quote(2885, 100.0 + k as f32, secs, cum);
        drain_main_feed_frame(
            &mut ingest,
            &frame(&bytes, 0, recv),
            recv,
            1_000 + k as u64,
            counters(),
        );
        // The packet as the parser reads it (every field the quote does not
        // set is 0, as here).
        let parsed = tickvault_common::tick_types::ParsedTick {
            security_id: 2885,
            exchange_segment_code: ExchangeSegment::NseFno.binary_code(),
            last_traded_price: 100.0 + k as f32,
            exchange_timestamp: secs,
            received_at_nanos: recv,
            volume: cum,
            ..tickvault_common::tick_types::ParsedTick::default()
        };
        let stats = twin.consume_tick(Feed::Dhan, &parsed, None, |_, _, _, _, _| {});
        if let Some(t) = rx.try_recv() {
            tapped += 1;
            let sat = |v: u64| u64::from(u32::try_from(v).unwrap_or(u32::MAX));
            assert_eq!(
                u64::from(t.volume_delta),
                sat(stats.volume_delta),
                "tick {k}"
            );
            assert_eq!(
                u64::from(t.first_bucket_volume_delta),
                sat(stats.first_bucket_volume_delta),
                "tick {k}"
            );
            assert_eq!(
                t.cumulative_restarted, stats.cumulative_restarted,
                "tick {k}"
            );
            assert_eq!(t.stale_packet, stats.stale_packet, "tick {k}");
            assert_eq!(t.fold_seed, stats.seeded, "tick {k}");
            seeds += usize::from(t.fold_seed);
            stale += usize::from(t.stale_packet);
            first_differs |= t.first_bucket_volume_delta != t.volume_delta;
            moved += usize::from(t.volume_delta > 0);
        }
    }
    assert!(tapped >= 5, "the script reached the tap: {tapped}");
    assert!(moved >= 3, "volume moved on the tapped ticks: {moved}");
    assert!(stale >= 1, "the regression reached the tap as stale");
    assert_eq!(
        seeds, 1,
        "the seeding tick reached the tap marked as the seed"
    );
    assert!(
        first_differs,
        "the opening trade credits the 09:15 bucket apart"
    );
}
