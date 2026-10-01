//! Hostile review of audit PR58 (the day's first trade lands in its first
//! bar): SESSIONS, SEGMENTS AND DAY EDGES, re-run against the review
//! 2026-10-01 fixes (LTQ == cumulative, no proof during replay, named
//! segments only for the open extension). Every packet carries its own trade
//! quantity in `last_trade_quantity`. Each assertion states the bar the fold
//! must write: the true volume where the first trade is provably the day's
//! whole volume, else 0 (seeded: may under-report, never over-report).

use std::collections::HashMap;

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::{
    AGGREGATOR_MAX_SLOTS, MultiTfAggregator, untraded_proof_holds,
};
use tickvault_trading::candles::tf_index::TfIndex;

const IDX: u8 = 0;
const EQ: u8 = 1;
const FNO: u8 = 2;
const CUR: u8 = 3;
const BSE_EQ: u8 = 4;
const MCX: u8 = 5;
const BSE_CUR: u8 = 7;
const BSE_FNO: u8 = 8;

/// IST midnight of a test day (IST epoch seconds).
const D0: u32 = 1_756_000_000 - (1_756_000_000 % 86_400);
const fn at(day: u32, h: u32, m: u32, s: u32) -> u32 {
    D0 + day * 86_400 + h * 3_600 + m * 60 + s
}

fn tk(sid: u64, seg: u8, ts: u32, price: f32, cum: u32, ltq: u16, receipt_ist: u32) -> ParsedTick {
    ParsedTick {
        security_id: sid,
        exchange_segment_code: seg,
        last_traded_price: price,
        last_trade_quantity: ltq,
        exchange_timestamp: ts,
        volume: cum,
        received_at_nanos: (i64::from(receipt_ist) - 19_800) * 1_000_000_000,
        ..ParsedTick::default()
    }
}

/// Stale connect/book snapshot: yesterday's 15:29 last trade (qty 25), with
/// yesterday's day cumulative.
fn stale(sid: u64, seg: u8, day: u32, receipt_ist: u32) -> ParsedTick {
    tk(
        sid,
        seg,
        at(day - 1, 15, 29, 0),
        100.0,
        90_000,
        25,
        receipt_ist,
    )
}

/// A trade of quantity `ltq` received at its own exchange second.
fn trade(sid: u64, seg: u8, ts: u32, cum: u32, ltq: u16) -> ParsedTick {
    tk(sid, seg, ts, 101.0, cum, ltq, ts)
}

type Key = (u64, u8, usize, u32);
#[derive(Default)]
struct Bars(HashMap<Key, u64>);
impl Bars {
    fn push(&mut self, agg: &mut MultiTfAggregator, t: &ParsedTick) {
        let m = &mut self.0;
        let _ = agg.consume_tick(Feed::Dhan, t, None, |_, sid, seg, tf, st| {
            m.insert(
                (sid, seg, tf.as_ordinal(), st.bucket_start_ist_secs),
                st.volume,
            );
        });
    }
    fn close(&mut self, agg: &mut MultiTfAggregator) -> usize {
        let m = &mut self.0;
        agg.force_seal_all(|_, sid, seg, tf, st| {
            m.insert(
                (sid, seg, tf.as_ordinal(), st.bucket_start_ist_secs),
                st.volume,
            );
        })
    }
    fn vol(&self, sid: u64, seg: u8, tf: TfIndex, ts: u32) -> Option<u64> {
        let start = tf.bucket_start(ts);
        self.0.get(&(sid, seg, tf.as_ordinal(), start)).copied()
    }
}

fn agg() -> MultiTfAggregator {
    MultiTfAggregator::new(FeedStrategy::DEFAULT)
}

// ---------------------------------------------------------------- options

/// Row 1. 09:15:59 is inside the open-extended window (59 s): true 650.
/// 09:16:01 is 61 s: seeds (documented limit, missing not wrong).
#[test]
fn option_proved_0855_first_trade_091559_holds_091601_seeds() {
    for (first, expect, m1) in [(at(1, 9, 15, 59), 650u64, 650u64), (at(1, 9, 16, 1), 0, 50)] {
        let mut a = agg();
        let mut b = Bars::default();
        b.push(&mut a, &stale(1, FNO, 1, at(1, 8, 55, 0)));
        b.push(&mut a, &trade(1, FNO, first, 650, 650));
        b.push(&mut a, &trade(1, FNO, first + 1, 700, 50));
        b.close(&mut a);
        assert_eq!(
            b.vol(1, FNO, TfIndex::S1, first),
            Some(expect),
            "first={first}"
        );
        assert_eq!(
            b.vol(1, FNO, TfIndex::M1, first),
            Some(m1),
            "09:16:00 is the next minute; 09:16:02 is the same one"
        );
        assert_eq!(
            b.vol(1, FNO, TfIndex::S1, first + 1),
            Some(50),
            "next trade: own qty"
        );
    }
}

/// Row 2 (was a wrong bar). Lost first trade at 09:15:50 (qty 100); next
/// accepted 09:16:00 carries cumulative 150 and its own qty 50, so it seeds:
/// the 09:16 minute reads 0 (true 50), never 150. Mid-day variant too.
#[test]
fn lost_first_trade_across_a_minute_boundary_never_over_counts() {
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(2, FNO, 1, at(1, 8, 55, 0)));
    b.push(&mut a, &trade(2, FNO, at(1, 9, 16, 0), 150, 50));
    b.close(&mut a);
    assert_eq!(b.vol(2, FNO, TfIndex::M1, at(1, 9, 16, 0)), Some(0));
    assert_eq!(b.vol(2, FNO, TfIndex::S1, at(1, 9, 16, 0)), Some(0));

    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(3, FNO, 1, at(1, 10, 0, 30)));
    b.push(&mut a, &trade(3, FNO, at(1, 10, 1, 10), 150, 50));
    b.close(&mut a);
    assert_eq!(b.vol(3, FNO, TfIndex::M1, at(1, 10, 1, 10)), Some(0));

    // And when 09:16:00 genuinely IS the first trade, it lands (true 50).
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(4, FNO, 1, at(1, 8, 55, 0)));
    b.push(&mut a, &trade(4, FNO, at(1, 9, 16, 0), 50, 50));
    b.close(&mut a);
    assert_eq!(b.vol(4, FNO, TfIndex::M1, at(1, 9, 16, 0)), Some(50));
}

/// Two fills between two packets (Dhan conflation): LTQ is the last fill,
/// cumulative both. Seeds (documented limit (2)).
#[test]
fn conflated_first_two_fills_seed() {
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(5, FNO, 1, at(1, 9, 15, 0)));
    b.push(&mut a, &trade(5, FNO, at(1, 9, 15, 3), 75, 50));
    b.close(&mut a);
    assert_eq!(b.vol(5, FNO, TfIndex::M1, at(1, 9, 15, 3)), Some(0));
}

/// LTQ is u16 on the wire: a first trade above 65,535 can never prove itself
/// and seeds (missing, never wrong). Exactly 65,535 still lands.
#[test]
fn first_trade_above_u16_ltq_seeds() {
    for (cum, ltq, expect) in [(65_535u32, u16::MAX, 65_535u64), (70_000, 4_464, 0)] {
        let mut a = agg();
        let mut b = Bars::default();
        b.push(&mut a, &stale(6, FNO, 1, at(1, 9, 15, 0)));
        // 70,000 truncated mod 65,536 = 4,464: must NOT be read as equal.
        b.push(&mut a, &trade(6, FNO, at(1, 9, 15, 4), cum, ltq));
        b.close(&mut a);
        assert_eq!(
            b.vol(6, FNO, TfIndex::M1, at(1, 9, 15, 4)),
            Some(expect),
            "cum={cum}"
        );
    }
}

// ---------------------------------------------------------------- equities

/// Row 3. The auction match packet carries LTQ = matched quantity.
#[test]
fn equity_pre_open_match_proof_age_decides() {
    // Proof 09:05, match printed at 09:07:50 (170 s later): seeds.
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(10, EQ, 1, at(1, 9, 5, 0)));
    b.push(&mut a, &trade(10, EQ, at(1, 9, 7, 50), 5_000, 5_000));
    b.push(&mut a, &trade(10, EQ, at(1, 9, 15, 2), 5_100, 100));
    b.close(&mut a);
    assert_eq!(b.vol(10, EQ, TfIndex::M1, at(1, 9, 7, 50)), Some(0));
    assert_eq!(b.vol(10, EQ, TfIndex::M1, at(1, 9, 15, 2)), Some(100));

    // Proof 09:07:30 (book update), match 09:07:50: true 5,000 in 09:07.
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(11, EQ, 1, at(1, 9, 7, 30)));
    b.push(&mut a, &trade(11, EQ, at(1, 9, 7, 50), 5_000, 5_000));
    b.push(&mut a, &trade(11, EQ, at(1, 9, 15, 2), 5_100, 100));
    b.close(&mut a);
    assert_eq!(b.vol(11, EQ, TfIndex::M1, at(1, 9, 7, 50)), Some(5_000));
    assert_eq!(b.vol(11, EQ, TfIndex::M1, at(1, 9, 15, 2)), Some(100));
    assert_eq!(b.vol(11, EQ, TfIndex::M60, at(1, 9, 15, 2)), Some(5_100));

    // An auction bigger than u16 cannot prove itself: seeds.
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(14, EQ, 1, at(1, 9, 7, 30)));
    b.push(&mut a, &trade(14, EQ, at(1, 9, 7, 50), 250_000, u16::MAX));
    b.close(&mut a);
    assert_eq!(b.vol(14, EQ, TfIndex::M1, at(1, 9, 7, 50)), Some(0));
}

/// Row 4.
#[test]
fn equity_proof_at_091204_vs_091205() {
    for (proof, expect) in [(at(1, 9, 12, 4), 0u64), (at(1, 9, 12, 5), 300)] {
        let mut a = agg();
        let mut b = Bars::default();
        b.push(&mut a, &stale(12, EQ, 1, proof));
        b.push(&mut a, &trade(12, EQ, at(1, 9, 15, 30), 300, 300));
        b.close(&mut a);
        assert_eq!(b.vol(12, EQ, TfIndex::M1, at(1, 9, 15, 30)), Some(expect));
    }
}

/// Row 5 (was a wrong bar). IF post-auction packets kept yesterday's LTT, a
/// 09:13 packet is a proof; the first continuous trade carries the auction
/// in its cumulative (5,100) against its own 100, so it seeds: 09:15 reads 0
/// (true 100), never 5,100.
#[test]
fn equity_if_post_match_packets_kept_prior_day_ltt_auction_is_not_poured_in() {
    let mut a = agg();
    let mut b = Bars::default();
    b.push(
        &mut a,
        &tk(
            13,
            EQ,
            at(0, 15, 29, 0),
            100.0,
            5_000,
            5_000,
            at(1, 9, 13, 0),
        ),
    );
    b.push(&mut a, &trade(13, EQ, at(1, 9, 15, 2), 5_100, 100));
    b.close(&mut a);
    assert_eq!(b.vol(13, EQ, TfIndex::M1, at(1, 9, 15, 2)), Some(0));
    assert_eq!(b.vol(13, EQ, TfIndex::S1, at(1, 9, 15, 2)), Some(0));
}

// ------------------------------------------------------ indices, BSE, others

#[test]
fn index_volume_zero_unchanged_by_proof() {
    let mut a = agg();
    let mut b = Bars::default();
    b.push(
        &mut a,
        &tk(13, IDX, at(0, 15, 30, 0), 24_000.0, 0, 0, at(1, 8, 59, 0)),
    );
    b.push(
        &mut a,
        &tk(13, IDX, at(1, 9, 15, 1), 24_010.0, 0, 0, at(1, 9, 15, 1)),
    );
    b.close(&mut a);
    assert_eq!(b.vol(13, IDX, TfIndex::M1, at(1, 9, 15, 1)), Some(0));
}

#[test]
fn bse_segments_follow_their_nse_twins() {
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(20, BSE_FNO, 1, at(1, 8, 55, 0)));
    b.push(&mut a, &trade(20, BSE_FNO, at(1, 9, 15, 30), 40, 40));
    b.push(&mut a, &stale(21, BSE_EQ, 1, at(1, 9, 5, 0)));
    b.push(&mut a, &trade(21, BSE_EQ, at(1, 9, 15, 30), 40, 40));
    b.push(&mut a, &stale(22, BSE_EQ, 1, at(1, 9, 13, 0)));
    b.push(&mut a, &trade(22, BSE_EQ, at(1, 9, 15, 30), 40, 40));
    b.close(&mut a);
    assert_eq!(b.vol(20, BSE_FNO, TfIndex::M1, at(1, 9, 15, 30)), Some(40));
    assert_eq!(b.vol(21, BSE_EQ, TfIndex::M1, at(1, 9, 15, 30)), Some(0));
    assert_eq!(b.vol(22, BSE_EQ, TfIndex::M1, at(1, 9, 15, 30)), Some(40));
}

/// Row 8 (was latent). Currency/MCX/other segments are never extended to the
/// open; a fresh proof (within 60 s) still holds for them.
#[test]
fn currency_and_mcx_proofs_are_not_extended() {
    for seg in [IDX, CUR, MCX, BSE_CUR, 6, u8::MAX] {
        assert!(
            !untraded_proof_holds(at(1, 9, 13, 0), at(1, 9, 16, 0), seg),
            "seg={seg}"
        );
        assert!(
            !untraded_proof_holds(at(1, 8, 55, 0), at(1, 9, 15, 2), seg),
            "seg={seg}"
        );
        assert!(
            untraded_proof_holds(at(1, 10, 0, 0), at(1, 10, 0, 40), seg),
            "seg={seg}"
        );
    }
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(23, MCX, 1, at(1, 9, 13, 0)));
    b.push(&mut a, &trade(23, MCX, at(1, 9, 16, 0), 9, 9));
    b.close(&mut a);
    assert_eq!(b.vol(23, MCX, TfIndex::M1, at(1, 9, 16, 0)), Some(0));
}

// ---------------------------------------------------------------- day edges

#[test]
fn proof_from_previous_day_never_holds_without_day_close() {
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(30, FNO, 1, at(1, 15, 35, 0)));
    b.push(&mut a, &trade(30, FNO, at(2, 9, 15, 2), 70, 70));
    b.close(&mut a);
    assert_eq!(b.vol(30, FNO, TfIndex::M1, at(2, 9, 15, 2)), Some(0));
    assert!(!untraded_proof_holds(
        at(1, 23, 59, 59),
        at(2, 0, 0, 1),
        FNO
    ));
    assert!(!untraded_proof_holds(at(1, 23, 59, 59), at(2, 0, 0, 1), EQ));
}

#[test]
fn after_day_close_a_post_midnight_proof_is_extended_for_options_only() {
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &trade(31, FNO, at(1, 10, 0, 0), 10, 10));
    b.push(&mut a, &trade(32, EQ, at(1, 10, 0, 0), 10, 10));
    b.close(&mut a);
    a.reset_watermark();
    b.push(
        &mut a,
        &tk(31, FNO, at(1, 10, 0, 0), 101.0, 10, 10, at(2, 0, 30, 0)),
    );
    b.push(
        &mut a,
        &tk(32, EQ, at(1, 10, 0, 0), 101.0, 10, 10, at(2, 0, 30, 0)),
    );
    b.push(&mut a, &trade(31, FNO, at(2, 9, 15, 20), 25, 25));
    b.push(&mut a, &trade(32, EQ, at(2, 9, 15, 20), 25, 25));
    b.close(&mut a);
    assert_eq!(b.vol(31, FNO, TfIndex::M1, at(2, 9, 15, 20)), Some(25));
    assert_eq!(b.vol(32, EQ, TfIndex::M1, at(2, 9, 15, 20)), Some(0));
}

#[test]
fn muhurat_style_sessions() {
    assert!(untraded_proof_holds(
        at(1, 13, 44, 30),
        at(1, 13, 45, 5),
        FNO
    ));
    assert!(!untraded_proof_holds(at(1, 9, 0, 0), at(1, 13, 45, 5), FNO));
    assert!(!untraded_proof_holds(
        at(1, 13, 30, 0),
        at(1, 13, 38, 0),
        EQ
    ));
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(33, FNO, 1, at(1, 17, 59, 30)));
    let st = a.consume_tick(
        Feed::Dhan,
        &trade(33, FNO, at(1, 18, 0, 5), 9, 9),
        None,
        |_, _, _, _, _| {},
    );
    assert!(st.out_of_session);
    assert_eq!(b.close(&mut a), 0, "no phantom bar from a proof-only slot");
}

#[test]
fn first_trade_at_1529_and_day_close_seal() {
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(40, FNO, 1, at(1, 15, 28, 50)));
    b.push(&mut a, &trade(40, FNO, at(1, 15, 29, 30), 40, 40));
    let n = b.close(&mut a);
    assert_eq!(b.vol(40, FNO, TfIndex::M1, at(1, 15, 29, 30)), Some(40));
    assert_eq!(n, 10, "one bar per timeframe, no extra");
}

#[test]
fn u32_and_receipt_extremes() {
    let mut a = agg();
    let mut t = stale(50, FNO, 1, at(1, 9, 0, 0));
    t.received_at_nanos = i64::MAX;
    let _ = a.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});
    assert!(a.lookup(Feed::Dhan, 50, FNO).is_none());
    let t = tk(51, FNO, 0, 0.0, 0, 0, u32::MAX - 100);
    let _ = a.consume_tick(Feed::Dhan, &t, None, |_, _, _, _, _| {});
    let mut b = Bars::default();
    b.push(&mut a, &stale(51, FNO, 1, at(1, 9, 0, 0)));
    b.push(&mut a, &trade(51, FNO, at(1, 9, 15, 3), 20, 20));
    b.close(&mut a);
    assert_eq!(
        b.vol(51, FNO, TfIndex::M1, at(1, 9, 15, 3)),
        Some(0),
        "pinned future proof: seeds"
    );
    for (p, tr) in [
        (u32::MAX, u32::MAX),
        (1, 0),
        (u32::MAX, 0),
        (86_399, 86_400),
    ] {
        for seg in [0, 1, 2, 3, 4, 5, 7, 8, u8::MAX] {
            let _ = untraded_proof_holds(p, tr, seg);
        }
    }
}

#[test]
fn proof_only_slots_emit_no_bar() {
    let mut a = agg();
    let mut b = Bars::default();
    for sid in 0..100u64 {
        b.push(&mut a, &stale(1_000 + sid, FNO, 1, at(1, 9, 0, 0)));
        b.push(&mut a, &tk(2_000 + sid, FNO, 0, 0.0, 0, 0, at(1, 9, 0, 0)));
    }
    assert_eq!(a.len(), 200, "proofs take slots");
    assert_eq!(a.catch_up_seal_all(at(1, 15, 40, 0), |_, _, _, _, _| {}), 0);
    assert_eq!(b.close(&mut a), 0);
    assert!(b.0.is_empty());
}

/// Fix (2): a replayed frame records no proof and takes no slot; the live
/// connect snapshot proves the key again.
#[test]
fn replay_records_no_proof() {
    let mut a = agg();
    a.set_replay_mode(true);
    let _ = a.consume_tick(
        Feed::Dhan,
        &stale(70, FNO, 1, at(1, 9, 14, 50)),
        None,
        |_, _, _, _, _| {},
    );
    let _ = a.consume_tick(
        Feed::Dhan,
        &tk(71, FNO, 0, 0.0, 0, 0, at(1, 9, 14, 50)),
        None,
        |_, _, _, _, _| {},
    );
    assert!(a.lookup(Feed::Dhan, 70, FNO).is_none());
    assert!(a.lookup(Feed::Dhan, 71, FNO).is_none());
    let _ = a.finish_replay(false, |_, _, _, _, _| {});
    let mut b = Bars::default();
    // 70: no live proof -> seeds. 71: live proof at 09:15:00 -> lands.
    b.push(&mut a, &tk(71, FNO, 0, 0.0, 0, 0, at(1, 9, 15, 0)));
    b.push(&mut a, &trade(70, FNO, at(1, 9, 15, 5), 30, 30));
    b.push(&mut a, &trade(71, FNO, at(1, 9, 15, 5), 30, 30));
    b.close(&mut a);
    assert_eq!(b.vol(70, FNO, TfIndex::M1, at(1, 9, 15, 5)), Some(0));
    assert_eq!(b.vol(71, FNO, TfIndex::M1, at(1, 9, 15, 5)), Some(30));
}

/// Row 15: UNCHANGED by design (limit (3) in the field doc).
#[test]
fn proof_slots_can_push_a_trading_key_into_exhaustion() {
    let ceiling = AGGREGATOR_MAX_SLOTS - AGGREGATOR_MAX_SLOTS / 20;
    assert_eq!(ceiling, 23_750);
    let mut a = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, AGGREGATOR_MAX_SLOTS);
    let r = at(1, 9, 0, 0);
    for sid in 0..ceiling as u64 {
        let _ = a.consume_tick(
            Feed::Dhan,
            &stale(100_000 + sid, FNO, 1, r),
            None,
            |_, _, _, _, _| {},
        );
    }
    assert_eq!(a.len(), 23_750);
    let _ = a.consume_tick(Feed::Dhan, &stale(7, FNO, 1, r), None, |_, _, _, _, _| {});
    assert_eq!(a.len(), 23_750);
    let mut b = Bars::default();
    b.push(&mut a, &trade(7, FNO, at(1, 9, 15, 5), 44, 44));
    for sid in 0..1_249u64 {
        b.push(&mut a, &trade(500_000 + sid, FNO, at(1, 9, 15, 5), 1, 1));
    }
    assert_eq!(a.len(), AGGREGATOR_MAX_SLOTS);
    assert_eq!(a.slots_exhausted_total(), 0);
    let st = a.consume_tick(
        Feed::Dhan,
        &trade(9_999_999, FNO, at(1, 9, 15, 6), 5, 5),
        None,
        |_, _, _, _, _| {},
    );
    assert!(st.slot_exhausted, "trading key refused: no candles all day");
    assert_eq!(a.slots_exhausted_total(), 1);
    b.close(&mut a);
    assert_eq!(
        b.vol(7, FNO, TfIndex::M1, at(1, 9, 15, 5)),
        Some(0),
        "late proof: seeds"
    );
}

/// Row 16 (was a wrong bar). A refused zero-price packet stamped today with
/// volume 500; the next trade (cum 600, own qty 100) seeds: 09:16 reads 0
/// (true 100), never 600.
#[test]
fn refused_zero_price_today_with_volume_inside_window_seeds() {
    let mut a = agg();
    let mut b = Bars::default();
    b.push(&mut a, &stale(60, FNO, 1, at(1, 9, 15, 0)));
    b.push(
        &mut a,
        &tk(60, FNO, at(1, 9, 15, 50), 0.0, 500, 500, at(1, 9, 15, 50)),
    );
    b.push(&mut a, &trade(60, FNO, at(1, 9, 16, 0), 600, 100));
    b.close(&mut a);
    assert_eq!(b.vol(60, FNO, TfIndex::M1, at(1, 9, 16, 0)), Some(0));
}
