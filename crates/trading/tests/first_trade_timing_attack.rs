//! Hostile timing / ordering attack on the audit-PR58 "untraded today" proof,
//! re-run against the review-2026-10-01 fix (first trade must also carry a day
//! cumulative EQUAL to its own last-trade quantity; no proof during a WAL
//! replay; the 09:15 extension only for F&O and post-auction equity).
//!
//! Every scenario folds a hand-built packet stream through the REAL
//! `MultiTfAggregator`, collects every bar it emits (seals, then a final
//! `force_seal_all`), and compares S1 / S5 / M1 / M5 against bars computed
//! independently from a trade list. Every packet carries its OWN trade
//! quantity as `last_trade_quantity` (a re-send repeats it).
//!
//! Two expectations are used per scenario:
//! - `counted`: the bars the fold SHOULD write (the true trades it can know
//!   about; a first trade it cannot prove is the day's first is "seeded",
//!   i.e. missing, which under-reports and is the documented policy);
//! - `truth`: what actually traded; NO bar may ever exceed it (`no_over`).
//!
//! 2026-10-09 (ADANIENT): when the first folded packet seeds, the buckets
//! that hold 09:15 still open on a zero baseline, so they also take the seed
//! (`with_open_seed`): everything in that cumulative traded inside them. For
//! equities this includes the pre-open auction under the default "Match Dhan"
//! choice, so an equity's `truth` here places its auction at 09:15:00.

use std::collections::{BTreeMap, BTreeSet};

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::TfIndex;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::{
    ConsumeStats, MultiTfAggregator, UNTRADED_PROOF_MAX_AGE_SECS, UNTRADED_PROOF_MAX_SKEW_SECS,
    untraded_proof_holds,
};

/// An IST trading day's midnight, in IST epoch seconds (same as restart_differential).
const DAY: u32 = 1_779_321_600;
const OPEN: u32 = DAY + 33_300; // 09:15:00
const SID: u64 = 58_999;
const FNO: u8 = 2;
const EQ: u8 = 1;
const CUR: u8 = 3;
/// A prior-day last trade time (15:16 yesterday).
const PREV_LTT: u32 = DAY - 86_400 + 55_000;

const TFS: [(TfIndex, u32); 4] = [
    (TfIndex::S1, 1),
    (TfIndex::S5, 5),
    (TfIndex::M1, 60),
    (TfIndex::M5, 300),
];

fn tk(seg: u8, ltt: u32, price: f32, cum: u32, ltq: u16, receipt_ist: u32) -> ParsedTick {
    ParsedTick {
        security_id: SID,
        exchange_segment_code: seg,
        last_traded_price: price,
        last_trade_quantity: ltq,
        exchange_timestamp: ltt,
        volume: cum,
        received_at_nanos: (i64::from(receipt_ist) - 19_800) * 1_000_000_000,
        ..ParsedTick::default()
    }
}

/// A prior-day snapshot (the proof shape) received at `receipt`. Yesterday's
/// last trade quantity rides along, as Dhan sends it.
fn stale(seg: u8, receipt: u32) -> ParsedTick {
    tk(seg, PREV_LTT, 12.5, 90_000, 75, receipt)
}

struct H {
    agg: MultiTfAggregator,
    bars: BTreeMap<(usize, u32), u64>,
}

impl H {
    fn new() -> Self {
        Self {
            agg: MultiTfAggregator::new(FeedStrategy::DEFAULT),
            bars: BTreeMap::new(),
        }
    }
    fn push(&mut self, t: &ParsedTick) -> ConsumeStats {
        let bars = &mut self.bars;
        self.agg
            .consume_tick(Feed::Dhan, t, None, |_, sid, _, tf, st| {
                if sid == SID {
                    bars.insert((tf.as_ordinal(), st.bucket_start_ist_secs), st.volume);
                }
            })
    }
    fn finish_replay(&mut self, capture_start: u32) {
        self.agg.set_live_capture_start(capture_start);
        let bars = &mut self.bars;
        let _ = self.agg.finish_replay(false, |_, sid, _, tf, st| {
            if sid == SID {
                bars.insert((tf.as_ordinal(), st.bucket_start_ist_secs), st.volume);
            }
        });
    }
    fn finish(mut self) -> BTreeMap<(usize, u32), u64> {
        let bars = &mut self.bars;
        let _ = self.agg.force_seal_all(|_, sid, _, tf, st| {
            if sid == SID {
                bars.insert((tf.as_ordinal(), st.bucket_start_ist_secs), st.volume);
            }
        });
        self.bars
    }
}

/// Independent bars from trades `(trade_secs, qty)`.
fn bars_of(trades: &[(u32, u64)]) -> BTreeMap<(usize, u32), u64> {
    let mut m = BTreeMap::new();
    for &(ts, q) in trades {
        for (tf, n) in TFS {
            *m.entry((tf.as_ordinal(), ts - ts % n)).or_insert(0) += q;
        }
    }
    m
}

type Diff = Vec<(&'static str, u32, u64, u64)>;

/// (tf, bucket secs-of-day, got, want) for every checked bucket that differs.
fn diff(got: &BTreeMap<(usize, u32), u64>, want: &BTreeMap<(usize, u32), u64>) -> Diff {
    let mut out = Vec::new();
    for (tf, name) in [
        (TfIndex::S1, "S1"),
        (TfIndex::S5, "S5"),
        (TfIndex::M1, "M1"),
        (TfIndex::M5, "M5"),
    ] {
        let o = tf.as_ordinal();
        let keys: BTreeSet<u32> = got
            .keys()
            .chain(want.keys())
            .filter(|k| k.0 == o)
            .map(|k| k.1)
            .collect();
        for b in keys {
            let g = got.get(&(o, b)).copied().unwrap_or(0);
            let w = want.get(&(o, b)).copied().unwrap_or(0);
            if g != w {
                out.push((name, b - DAY, g, w));
            }
        }
    }
    out
}

fn over_counts(d: &Diff) -> Diff {
    d.iter().copied().filter(|x| x.2 > x.3).collect()
}

fn day_total(got: &BTreeMap<(usize, u32), u64>) -> u64 {
    let o = TfIndex::M1.as_ordinal();
    got.iter().filter(|k| k.0.0 == o).map(|(_, v)| *v).sum()
}

fn run(stream: &[ParsedTick]) -> BTreeMap<(usize, u32), u64> {
    let mut h = H::new();
    for t in stream {
        let _ = h.push(t);
    }
    h.finish()
}

/// Asserts the fold wrote exactly `counted` and never exceeded `truth`.
#[track_caller]
fn assert_bars(got: &BTreeMap<(usize, u32), u64>, counted: &[(u32, u64)], truth: &[(u32, u64)]) {
    assert_expected(got, &bars_of(counted), truth);
}

/// Asserts the fold wrote exactly `want` and never exceeded `truth`.
#[track_caller]
fn assert_expected(
    got: &BTreeMap<(usize, u32), u64>,
    want: &BTreeMap<(usize, u32), u64>,
    truth: &[(u32, u64)],
) {
    let over = over_counts(&diff(got, &bars_of(truth)));
    assert!(over.is_empty(), "OVER-COUNT vs truth: {over:?}");
    let d = diff(got, want);
    assert!(d.is_empty(), "differs from the expected bars: {d:?}");
}

/// `bars` plus `seed` in every checked bucket that holds both 09:15 and
/// `first_ts` (2026-10-09): a seeding first packet at `first_ts` with day
/// cumulative `seed` still fills the buckets that hold 09:15.
fn with_open_seed(
    mut bars: BTreeMap<(usize, u32), u64>,
    first_ts: u32,
    seed: u64,
) -> BTreeMap<(usize, u32), u64> {
    for (tf, n) in TFS {
        if first_ts - first_ts % n == OPEN {
            *bars.entry((tf.as_ordinal(), OPEN)).or_insert(0) += seed;
        }
    }
    bars
}

/// The first folded packet at `first_ts` seeded with day cumulative `seed`;
/// the rest wrote exactly `counted`.
#[track_caller]
fn assert_seeded(
    got: &BTreeMap<(usize, u32), u64>,
    counted: &[(u32, u64)],
    first_ts: u32,
    seed: u64,
    truth: &[(u32, u64)],
) {
    assert_expected(
        got,
        &with_open_seed(bars_of(counted), first_ts, seed),
        truth,
    );
}

// ---------------------------------------------------------------------------
// 1. Happy path: F&O proof before the open, true first trade 2 s after.
#[test]
fn s01_happy_path_first_trade_in_first_bar() {
    let got = run(&[
        stale(FNO, OPEN - 30),
        tk(FNO, OPEN + 2, 13.0, 650, 650, OPEN + 2),
        tk(FNO, OPEN + 3, 13.1, 700, 50, OPEN + 3),
    ]);
    let t = [(OPEN + 2, 650), (OPEN + 3, 50)];
    assert_bars(&got, &t, &t);
}

// 2. First trade 61 s after a mid-session proof -> seeds (first trade missing).
#[test]
fn s02_first_trade_61s_after_proof_seeds() {
    let p = OPEN + 600;
    let got = run(&[stale(FNO, p), tk(FNO, p + 61, 13.0, 650, 650, p + 61)]);
    assert_bars(&got, &[], &[(p + 61, 650)]);
}

// 3. First trade minutes after a 09:00:30 connect snapshot -> seeds; the
//    09:15 five-minute bar still holds it (2026-10-09).
#[test]
fn s03_first_trade_minutes_after_open_seeds() {
    let got = run(&[
        stale(FNO, DAY + 32_430),
        tk(FNO, OPEN + 61, 13.0, 650, 650, OPEN + 61),
    ]);
    assert_seeded(&got, &[], OPEN + 61, 650, &[(OPEN + 61, 650)]);
}

// 4. Early: an F&O packet stamped 09:10, 30 s after a proof. Candles start
// at 09:15 (2026-10-05), so the fold refuses it: no bar in any timeframe.
#[test]
fn s04_pre_open_stamp_is_refused_by_the_fold() {
    let p = DAY + 33_000;
    let got = run(&[stale(FNO, p - 30), tk(FNO, p, 13.0, 100, 100, p)]);
    assert_bars(&got, &[], &[(p, 100)]);
}

// 5. Duplicated first-trade packet (x3, same LTQ) -> no double count.
#[test]
fn s05_duplicate_first_trade_is_not_double_counted() {
    let a = tk(FNO, OPEN + 2, 13.0, 650, 650, OPEN + 2);
    let mut b = a.clone();
    b.received_at_nanos += 300_000_000;
    let got = run(&[stale(FNO, OPEN - 10), a.clone(), b, a]);
    let t = [(OPEN + 2, 650)];
    assert_bars(&got, &t, &t);
}

// 6. Out of order: A(:10, 100) and B(:40, +200); B arrives first. B's cum
//    (300) != its LTQ (200) -> seeds; A is then stale. Both missing from
//    the short bars; the 09:15 minute and five minutes hold both (2026-10-09).
#[test]
fn s06_out_of_order_first_two_trades_same_minute() {
    let got = run(&[
        stale(FNO, OPEN - 10),
        tk(FNO, OPEN + 40, 13.2, 300, 200, OPEN + 41),
        tk(FNO, OPEN + 10, 13.0, 100, 100, OPEN + 42),
    ]);
    assert_seeded(
        &got,
        &[],
        OPEN + 40,
        300,
        &[(OPEN + 10, 100), (OPEN + 40, 200)],
    );
}

// 6b. Same trades, in order -> exact.
#[test]
fn s06b_first_two_trades_in_order_exact() {
    let got = run(&[
        stale(FNO, OPEN - 10),
        tk(FNO, OPEN + 10, 13.0, 100, 100, OPEN + 11),
        tk(FNO, OPEN + 40, 13.2, 300, 200, OPEN + 41),
    ]);
    let t = [(OPEN + 10, 100), (OPEN + 40, 200)];
    assert_bars(&got, &t, &t);
}

// 7. Prior-day snapshots arriving AFTER real trades today -> ignored.
#[test]
fn s07_stale_snapshot_after_real_trade_is_ignored() {
    let got = run(&[
        stale(FNO, OPEN - 10),
        tk(FNO, OPEN + 5, 13.0, 100, 100, OPEN + 5),
        stale(FNO, OPEN + 20),
        tk(FNO, OPEN + 30, 13.0, 160, 60, OPEN + 30),
        stale(FNO, OPEN + 31),
        tk(FNO, OPEN + 50, 13.0, 200, 40, OPEN + 50),
    ]);
    let t = [(OPEN + 5, 100), (OPEN + 30, 60), (OPEN + 50, 40)];
    assert_bars(&got, &t, &t);
}

// 7b. Slot seeded by a mid-day trade (no proof), then a stale snapshot ->
//     never re-zeroes.
#[test]
fn s07b_stale_snapshot_after_seeded_trade_never_rezeroes() {
    let got = run(&[
        tk(FNO, OPEN + 600, 13.0, 5_000, 100, OPEN + 600),
        stale(FNO, OPEN + 610),
        tk(FNO, OPEN + 615, 13.0, 5_100, 100, OPEN + 615),
    ]);
    assert_bars(
        &got,
        &[(OPEN + 615, 100)],
        &[(OPEN + 300, 4_900), (OPEN + 600, 100), (OPEN + 615, 100)],
    );
}

// 8. First-trade packet LOST, next trade 30 s later, same minute -> seeds.
#[test]
fn s08_lost_first_packet_same_minute_seeds() {
    let p = OPEN + 600;
    let got = run(&[stale(FNO, p), tk(FNO, p + 40, 13.0, 300, 200, p + 40)]);
    assert_bars(&got, &[], &[(p + 10, 100), (p + 40, 200)]);
}

// 8b. Former BUG: lost first packet, next trade in the NEXT minute. Was 09:26
//     M1 = 150 vs true 50. Now seeds: 0 / 0, never over.
#[test]
fn s08b_lost_first_packet_across_a_minute_boundary_no_longer_overcounts() {
    let p = OPEN + 630;
    let got = run(&[stale(FNO, p), tk(FNO, p + 50, 13.0, 150, 50, p + 50)]);
    assert_bars(&got, &[], &[(p + 28, 100), (p + 50, 50)]);
}

// 8b-bite. The SAME stream with a (false) LTQ equal to cum reproduces the
//     old over-count, proving `check` would catch it.
#[test]
fn s08b_bite_a_packet_claiming_ltq_equal_cum_still_overcounts() {
    let p = OPEN + 630;
    let got = run(&[stale(FNO, p), tk(FNO, p + 50, 13.0, 150, 150, p + 50)]);
    let over = over_counts(&diff(&got, &bars_of(&[(p + 28, 100), (p + 50, 50)])));
    assert!(over.contains(&("M1", 33_960, 150, 50)), "{over:?}");
}

// 8c. Former BUG across a 5-minute edge (was M5 09:30 = 1,000 vs 100).
#[test]
fn s08c_lost_first_packet_across_a_5m_boundary_no_longer_overcounts() {
    let p = OPEN + 880;
    let got = run(&[stale(FNO, p), tk(FNO, p + 45, 13.0, 1_000, 100, p + 45)]);
    assert_bars(&got, &[], &[(p + 15, 900), (p + 45, 100)]);
}

// 9. Reconnect snapshot at exactly 09:15:00; first trade at +60 / +61 s.
#[test]
fn s09_proof_at_open_exact_boundaries() {
    let got = run(&[
        stale(FNO, OPEN),
        tk(FNO, OPEN + 60, 13.0, 650, 650, OPEN + 60),
    ]);
    let t = [(OPEN + 60, 650)];
    assert_bars(&got, &t, &t);
    let got = run(&[
        stale(FNO, OPEN),
        tk(FNO, OPEN + 61, 13.0, 650, 650, OPEN + 61),
    ]);
    assert_seeded(&got, &[], OPEN + 61, 650, &[(OPEN + 61, 650)]);
    assert!(untraded_proof_holds(DAY + 32_400, OPEN + 60, FNO));
    assert!(!untraded_proof_holds(DAY + 32_400, OPEN + 61, FNO));
    assert_eq!(UNTRADED_PROOF_MAX_AGE_SECS, 60);
}

// 10. Skew: trade stamped exactly 5 s before the proof counts, 6 s seeds.
#[test]
fn s10_skew_boundaries() {
    let p = OPEN + 1_000;
    let s = UNTRADED_PROOF_MAX_SKEW_SECS;
    let got = run(&[stale(FNO, p), tk(FNO, p - s, 13.0, 650, 650, p + 1)]);
    let t = [(p - s, 650)];
    assert_bars(&got, &t, &t);
    let got = run(&[stale(FNO, p), tk(FNO, p - s - 1, 13.0, 650, 650, p + 1)]);
    assert_bars(&got, &[], &[(p - s - 1, 650)]);
}

// 11. Box clock ahead by 10 s (seeds) / by 4 s (exact).
#[test]
fn s11_clock_ahead() {
    let e = OPEN + 600;
    let got = run(&[stale(FNO, e + 10), tk(FNO, e + 3, 13.0, 650, 650, e + 13)]);
    assert_bars(&got, &[], &[(e + 3, 650)]);
    let got = run(&[stale(FNO, e + 4), tk(FNO, e + 3, 13.0, 650, 650, e + 7)]);
    let t = [(e + 3, 650)];
    assert_bars(&got, &t, &t);
}

// 12. Box clock behind by 10 s: 65 s from the proof seeds, 55 s counts.
#[test]
fn s12_clock_behind() {
    let e = OPEN + 600;
    let got = run(&[stale(FNO, e - 10), tk(FNO, e + 55, 13.0, 650, 650, e + 45)]);
    assert_bars(&got, &[], &[(e + 55, 650)]);
    let got = run(&[stale(FNO, e - 10), tk(FNO, e + 45, 13.0, 650, 650, e + 35)]);
    let t = [(e + 45, 650)];
    assert_bars(&got, &t, &t);
}

// 13. Future receipts (tomorrow / +6 h / ~2100 / i64::MAX): never hold; a
//     future proof disables the benefit for the key for that day.
#[test]
fn s13_future_receipt_proofs() {
    let t = [(OPEN + 12, 650)];
    let got = run(&[
        stale(FNO, DAY + 86_400 + 33_000),
        stale(FNO, OPEN + 10),
        tk(FNO, OPEN + 12, 13.0, 650, 650, OPEN + 12),
    ]);
    assert_seeded(&got, &[], OPEN + 12, 650, &t);
    let got = run(&[
        stale(FNO, OPEN + 21_600),
        stale(FNO, OPEN + 10),
        tk(FNO, OPEN + 12, 13.0, 650, 650, OPEN + 12),
    ]);
    assert_seeded(&got, &[], OPEN + 12, 650, &t);
    let mut far = stale(FNO, OPEN);
    far.received_at_nanos = 4_100_000_000_i64 * 1_000_000_000;
    let mut huge = stale(FNO, OPEN);
    huge.received_at_nanos = i64::MAX;
    let got = run(&[far, huge, tk(FNO, OPEN + 12, 13.0, 650, 650, OPEN + 12)]);
    assert_seeded(&got, &[], OPEN + 12, 650, &t);
}

// 14. 540 refreshing proofs then a first trade 5 s after the last -> exact.
#[test]
fn s14_many_refreshing_proofs() {
    let mut s = Vec::new();
    let mut p = DAY + 32_400;
    while p <= OPEN + 4_000 {
        s.push(stale(FNO, p));
        p += 10;
    }
    let last = p - 10;
    s.push(tk(FNO, last + 5, 13.0, 75, 75, last + 5));
    s.push(tk(FNO, last + 9, 13.0, 80, 5, last + 9));
    let got = run(&s);
    let t = [(last + 5, 75), (last + 9, 5)];
    assert_bars(&got, &t, &t);
}

// 15. Zero-volume first packet (LTQ 0 = cum 0), then a real trade.
#[test]
fn s15_zero_volume_first_packet() {
    let got = run(&[
        stale(FNO, OPEN - 5),
        tk(FNO, OPEN + 2, 13.0, 0, 0, OPEN + 2),
        tk(FNO, OPEN + 4, 13.0, 500, 500, OPEN + 4),
    ]);
    let t = [(OPEN + 4, 500)];
    assert_bars(&got, &t, &t);
}

// 16. Day cumulative going DOWN after the zero-baseline first trade.
#[test]
fn s16_cumulative_goes_down() {
    let got = run(&[
        stale(FNO, OPEN - 5),
        tk(FNO, OPEN + 2, 13.0, 650, 650, OPEN + 2),
        tk(FNO, OPEN + 3, 13.0, 600, 10, OPEN + 3),
        tk(FNO, OPEN + 4, 13.0, 700, 50, OPEN + 4),
    ]);
    let t = [(OPEN + 2, 650), (OPEN + 4, 50)];
    assert_bars(&got, &t, &t);
}

// 17. Former over-count: equity, stalled proof read 09:12:30 after the 09:08
//     auction (50,000) whose packet Dhan skipped; first trade 09:15:02 carries
//     cum 50,400 with LTQ 400 -> seeds (09:15 bars 0, not 50,400).
//     2026-10-09: under "Match Dhan" the auction belongs to the 09:15 bars,
//     so they read 50,400 and the truth places the auction at 09:15:00.
#[test]
fn s17_equity_stalled_proof_no_longer_pulls_the_auction() {
    let got = run(&[
        stale(EQ, DAY + 33_150),
        tk(EQ, OPEN + 2, 812.0, 50_400, 400, OPEN + 2),
    ]);
    assert_seeded(
        &got,
        &[],
        OPEN + 2,
        50_400,
        &[(OPEN, 50_000), (OPEN + 2, 400)],
    );
}

// 17b. Equity proof after the auction, auction never traded, first trade at
//      09:15:02 -> exact (the 09:12:05 extension).
#[test]
fn s17b_equity_post_auction_proof_extends_to_open() {
    let got = run(&[
        stale(EQ, DAY + 33_130),
        tk(EQ, OPEN + 2, 812.0, 400, 400, OPEN + 2),
    ]);
    let t = [(OPEN + 2, 400)];
    assert_bars(&got, &t, &t);
    // 09:12:04 is too early to extend: seeds.
    let got = run(&[
        stale(EQ, DAY + 33_124),
        tk(EQ, OPEN + 2, 812.0, 400, 400, OPEN + 2),
    ]);
    assert_seeded(&got, &[], OPEN + 2, 400, &t);
}

// 18. Proofs out of order: only moves forward.
#[test]
fn s18_out_of_order_proofs_forward_only() {
    let got = run(&[
        stale(FNO, OPEN + 600),
        stale(FNO, OPEN + 500),
        tk(FNO, OPEN + 655, 13.0, 650, 650, OPEN + 655),
    ]);
    let t = [(OPEN + 655, 650)];
    assert_bars(&got, &t, &t);
}

// 19. Zero price with TODAY's time refreshes nothing; first trade 65 s after
//     the only real proof seeds.
#[test]
fn s19_zero_price_today_does_not_refresh() {
    let p = OPEN + 600;
    let got = run(&[
        tk(FNO, PREV_LTT, 0.0, 0, 0, p),
        tk(FNO, p + 50, 0.0, 0, 0, p + 50),
        tk(FNO, p + 65, 13.0, 650, 650, p + 65),
    ]);
    assert_bars(&got, &[], &[(p + 65, 650)]);
}

// 20. Replay: proof replayed (no longer recorded), WAL gap, trades; then live.
#[test]
fn s20_replay_gap_no_overcount() {
    let mut h = H::new();
    h.agg.set_replay_mode(true);
    let _ = h.push(&stale(FNO, OPEN - 10));
    assert!(
        h.agg.lookup(Feed::Dhan, SID, FNO).is_none(),
        "no proof slot during replay"
    );
    h.agg.mark_replay_gap();
    let _ = h.push(&tk(FNO, OPEN + 20, 13.0, 300, 200, OPEN + 20));
    let _ = h.push(&tk(FNO, OPEN + 30, 13.1, 350, 50, OPEN + 30));
    h.finish_replay(OPEN + 40);
    let _ = h.push(&tk(FNO, OPEN + 70, 13.2, 400, 50, OPEN + 70));
    let got = h.finish();
    let truth = [
        (OPEN + 5, 100),
        (OPEN + 20, 200),
        (OPEN + 30, 50),
        (OPEN + 70, 50),
    ];
    let d = diff(&got, &bars_of(&truth));
    eprintln!("s20 diff vs truth: {d:?}");
    assert!(over_counts(&d).is_empty(), "{d:?}");
}

// 21. Hand-over: proof replayed (ignored), downtime trade, first live packet
//     carries both (cum 300, LTQ 200) -> seeds; no over-count.
#[test]
fn s21_handover_downtime_trade() {
    let mut h = H::new();
    h.agg.set_replay_mode(true);
    let _ = h.push(&stale(FNO, OPEN + 600));
    h.finish_replay(OPEN + 640);
    let _ = h.push(&tk(FNO, OPEN + 645, 13.0, 300, 200, OPEN + 646));
    let _ = h.push(&tk(FNO, OPEN + 700, 13.1, 350, 50, OPEN + 700));
    let got = h.finish();
    let truth = [(OPEN + 630, 100), (OPEN + 645, 200), (OPEN + 700, 50)];
    let d = diff(&got, &bars_of(&truth));
    eprintln!("s21 diff vs truth: {d:?}");
    assert!(over_counts(&d).is_empty(), "{d:?}");
}

// 21b. Hand-over after replay where the LIVE connect snapshot re-proves and
//      the true first trade follows -> must not over-count; record what lands.
#[test]
fn s21b_handover_live_reproof_then_true_first_trade() {
    let mut h = H::new();
    h.agg.set_replay_mode(true);
    let _ = h.push(&stale(FNO, OPEN + 600));
    h.finish_replay(OPEN + 640);
    let _ = h.push(&stale(FNO, OPEN + 641));
    let _ = h.push(&tk(FNO, OPEN + 650, 13.0, 100, 100, OPEN + 650));
    let _ = h.push(&tk(FNO, OPEN + 700, 13.1, 150, 50, OPEN + 700));
    let got = h.finish();
    let truth = [(OPEN + 650, 100), (OPEN + 700, 50)];
    let d = diff(&got, &bars_of(&truth));
    eprintln!("s21b diff vs truth (under-counts are hand-over withholding): {d:?}");
    assert!(over_counts(&d).is_empty(), "{d:?}");
}

// 22. Mid-session boot (no replay): live proof, then the true first trade.
#[test]
fn s22_mid_session_boot_proof_then_trade() {
    let mut h = H::new();
    h.agg.set_live_capture_start(OPEN + 2_700);
    let _ = h.push(&stale(FNO, OPEN + 2_701));
    let _ = h.push(&tk(FNO, OPEN + 2_720, 13.0, 50, 50, OPEN + 2_720));
    let _ = h.push(&tk(FNO, OPEN + 2_790, 13.1, 90, 40, OPEN + 2_790));
    let got = h.finish();
    let d = diff(&got, &bars_of(&[(OPEN + 2_720, 50), (OPEN + 2_790, 40)]));
    eprintln!("s22 diff vs truth: {d:?}");
    assert!(over_counts(&d).is_empty(), "{d:?}");
}

// 23. Proof on (SID, EQ) never applies to (SID, F&O).
#[test]
fn s23_proof_is_per_composite_key() {
    let got = run(&[
        stale(EQ, OPEN + 600),
        tk(FNO, OPEN + 610, 13.0, 650, 650, OPEN + 610),
    ]);
    assert_bars(&got, &[], &[(OPEN + 610, 650)]);
}

// 24. Trade packet overtaken by a newer stale snapshot (3 s) -> exact.
#[test]
fn s24_trade_packet_overtaken_by_stale_snapshot() {
    let got = run(&[
        stale(FNO, OPEN + 600),
        stale(FNO, OPEN + 633),
        tk(FNO, OPEN + 630, 13.0, 650, 650, OPEN + 634),
    ]);
    let t = [(OPEN + 630, 650)];
    assert_bars(&got, &t, &t);
}

// 25. Equity auction traded before 09:15 (refused by the fold), then a stale
// snapshot. The first continuous trade carries the auction in its cumulative
// (50,400 against its own 400), so it seeds: missing, never 50,400.
// 2026-10-09: under "Match Dhan" the 09:15 bars hold the auction (50,400).
#[test]
fn s25_equity_auction_trade_then_stale_snapshot() {
    let got = run(&[
        tk(EQ, DAY + 32_880, 810.0, 50_000, 50_000, DAY + 32_881),
        stale(EQ, OPEN - 1),
        tk(EQ, OPEN + 2, 812.0, 50_400, 400, OPEN + 2),
    ]);
    assert_seeded(
        &got,
        &[],
        OPEN + 2,
        50_400,
        &[(OPEN, 50_000), (OPEN + 2, 400)],
    );
}

// 26. Former over-count: reconnect at 10:00 receives a prior-day copy for a
//     contract that traded 5,000 at 09:20; the next packet carries cum 5,100
//     with LTQ 100 -> seeds (was 5,100 in the 10:00 bars).
#[test]
fn s26_stale_prior_day_copy_after_a_morning_of_trades() {
    let got = run(&[
        stale(FNO, OPEN + 2_700),
        tk(FNO, OPEN + 2_730, 13.0, 5_100, 100, OPEN + 2_730),
    ]);
    assert_bars(&got, &[], &[(OPEN + 300, 5_000), (OPEN + 2_730, 100)]);
}

// 26b. Worst residual of 26: the morning was ONE trade of 5,000 at 09:20, the
//      stale copy arrives within 5 s of a re-send of that trade -> the re-send
//      has cum == LTQ, is the true first trade, and lands at 09:20 (correct).
#[test]
fn s26b_stale_copy_beside_a_resend_of_the_only_trade() {
    let got = run(&[
        stale(FNO, OPEN + 304),
        tk(FNO, OPEN + 300, 13.0, 5_000, 5_000, OPEN + 305),
    ]);
    let t = [(OPEN + 300, 5_000)];
    assert_bars(&got, &t, &t);
}

// 27. LIMIT (2): a true first trade of two fills in one packet (100 + 200 at
//     the same second, LTQ = last fill) -> seeds, both missing; no over.
#[test]
fn s27_two_fills_first_packet_seeds() {
    let got = run(&[
        stale(FNO, OPEN - 5),
        tk(FNO, OPEN + 2, 13.0, 300, 200, OPEN + 2),
    ]);
    assert_seeded(&got, &[], OPEN + 2, 300, &[(OPEN + 2, 300)]);
}

// 28. LIMIT: a first trade above u16 (LTQ cannot represent it). Dhan's LTQ
//     wire field is 2 bytes, so a cum of 70,000 never equals the LTQ -> seeds.
#[test]
fn s28_first_trade_larger_than_u16_seeds() {
    let got = run(&[
        stale(EQ, DAY + 33_130),
        tk(EQ, OPEN + 2, 812.0, 70_000, 4_464, OPEN + 2), // 70,000 mod 65,536
    ]);
    assert_seeded(&got, &[], OPEN + 2, 70_000, &[(OPEN + 2, 70_000)]);
}

// 29. Currency / commodity / other segments: never extended to 09:15.
#[test]
fn s29_currency_proof_not_extended_to_open() {
    let got = run(&[
        stale(CUR, OPEN - 120),
        tk(CUR, OPEN + 30, 83.1, 10, 10, OPEN + 30),
    ]);
    assert_bars(&got, &[], &[(OPEN + 30, 10)]);
    let got = run(&[
        stale(CUR, OPEN - 20),
        tk(CUR, OPEN + 30, 83.1, 10, 10, OPEN + 30),
    ]);
    let t = [(OPEN + 30, 10)];
    assert_bars(&got, &t, &t);
    for seg in [0_u8, 3, 5, 7, 255] {
        assert!(
            !untraded_proof_holds(OPEN - 120, OPEN + 30, seg),
            "seg {seg}"
        );
    }
}

// 30. Out of order with the TRUE first packet later in the stream but
//     carrying a fresh receipt: A(:10, 100) delayed to after C, B lost.
#[test]
fn s30_true_first_packet_delayed_behind_later_trade() {
    let got = run(&[
        stale(FNO, OPEN),
        tk(FNO, OPEN + 50, 13.2, 350, 50, OPEN + 50), // C (B lost)
        tk(FNO, OPEN + 10, 13.0, 100, 100, OPEN + 51), // A, late
    ]);
    assert_seeded(
        &got,
        &[],
        OPEN + 50,
        350,
        &[(OPEN + 10, 100), (OPEN + 30, 200), (OPEN + 50, 50)],
    );
}

mod randomized {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Debug)]
    struct Trade {
        dt: u16,
        qty: u16,
        lost: bool,
        delay: u8,
        dup: bool,
    }

    fn trade() -> impl Strategy<Value = Trade> {
        (1u16..40, 1u16..500, 0u8..10, 0u8..30, 0u8..10).prop_map(|(dt, qty, l, delay, d)| Trade {
            dt,
            qty,
            lost: l < 2,
            delay: if delay < 20 { 0 } else { delay - 19 },
            dup: d == 0,
        })
    }

    /// Builds the packet stream. `prefix = Some(head)`: the first `head` trades are
    /// lost and everything after is clean and in order.
    #[allow(clippy::too_many_arguments)]
    fn build(
        seg: u8,
        t0: u32,
        proof_lead: u32,
        skew: i32,
        n_proofs: usize,
        trades: &[Trade],
        prefix: Option<usize>,
    ) -> (Vec<ParsedTick>, Vec<(u32, u64)>) {
        let mut packets: Vec<(i64, ParsedTick)> = Vec::new();
        for i in 0..n_proofs {
            let emit = t0
                .saturating_sub(proof_lead + 7 * i as u32)
                .max(DAY + 30_000);
            let rec = (i64::from(emit) + i64::from(skew)) as u32;
            packets.push((i64::from(rec) * 1_000, stale(seg, rec)));
        }
        let mut ts = t0;
        let mut cum: u32 = 0;
        let mut truth = Vec::new();
        for (k, tr) in trades.iter().enumerate() {
            if k > 0 {
                ts += u32::from(tr.dt);
            }
            if ts >= DAY + 55_800 {
                break;
            }
            cum += u32::from(tr.qty);
            truth.push((ts, u64::from(tr.qty)));
            let (lost, delay) = match prefix {
                None => (tr.lost, tr.delay),
                Some(head) => (k < head, 0),
            };
            if lost {
                continue;
            }
            let rec = (i64::from(ts) + i64::from(skew) + i64::from(delay)) as u32;
            let price = 13.0 + (k as f32) * 0.05;
            let t = tk(seg, ts, price, cum, tr.qty, rec);
            packets.push((i64::from(rec) * 1_000 + k as i64, t.clone()));
            if tr.dup {
                packets.push((i64::from(rec) * 1_000 + 500 + k as i64, t));
            }
        }
        packets.sort_by_key(|p| p.0);
        (packets.into_iter().map(|p| p.1).collect(), truth)
    }

    fn seg_of(pick: u8) -> u8 {
        [EQ, FNO, CUR, 4, 5][usize::from(pick % 5)]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 6_000, .. ProptestConfig::default() })]

        /// Loss, reordering, duplicates, skew ANYWHERE: the day total never
        /// exceeds what traded (no double count), and a clean stream is exact
        /// or misses only the first trade.
        #[test]
        fn no_double_count_under_loss_reorder_dup_skew(
            t0_off in 0u32..20_000,
            proof_lead in 0u32..200,
            skew in -10i32..=10,
            n_proofs in 1usize..5,
            seg_pick in any::<u8>(),
            trades in prop::collection::vec(trade(), 1..12),
        ) {
            let seg = seg_of(seg_pick);
            let t0 = OPEN + t0_off;
            let (stream, truth) = build(seg, t0, proof_lead, skew, n_proofs, &trades, None);
            let got = run(&stream);
            let total: u64 = truth.iter().map(|t| t.1).sum();
            prop_assert!(day_total(&got) <= total, "double count: {} > {}", day_total(&got), total);
            let clean = trades.iter().all(|t| !t.lost && t.delay == 0);
            if clean {
                let d = diff(&got, &bars_of(&truth));
                let first = truth[0];
                for (name, b, g, w) in &d {
                    let n = match *name { "S1" => 1, "S5" => 5, "M1" => 60, _ => 300 };
                    let fb = (first.0 - first.0 % n) - DAY;
                    prop_assert!(*b == fb && *g + first.1 == *w,
                        "clean stream but {} {} got {} want {} (first {:?}) all={:?}",
                        name, b, g, w, first, d);
                }
            }
        }

        /// The day's first `head` trades are ALL lost (their volume reaches us
        /// only inside a later packet's cumulative), everything after is
        /// clean. NO BAR on any checked timeframe may exceed the truth. The
        /// pre-fix code poured the lost volume into the first delivered
        /// trade's bars here (s08b / s17 / s26 shapes; see s08b-bite).
        #[test]
        fn no_bar_over_truth_when_the_first_trades_are_lost(
            t0_off in 0u32..20_000,
            proof_lead in 0u32..90,
            skew in -10i32..=10,
            n_proofs in 1usize..5,
            seg_pick in any::<u8>(),
            head in 1usize..4,
            trades in prop::collection::vec(trade(), 2..12),
        ) {
            let seg = seg_of(seg_pick);
            let t0 = OPEN + t0_off;
            let (stream, truth) = build(seg, t0, proof_lead, skew, n_proofs, &trades, Some(head));
            let got = run(&stream);
            let over = over_counts(&diff(&got, &bars_of(&truth)));
            prop_assert!(over.is_empty(), "over-count {:?}", over);
        }

        /// Arbitrary loss / reorder / dups / skew, checked against a reference
        /// fold built from the packets alone: a packet whose day cumulative
        /// went down is ignored, any other adds its increase to the bar of
        /// its own trade time. The fold must equal that reference either
        /// SEEDED (baseline = the first folded packet's cumulative) or ZERO
        /// (baseline 0), and ZERO only when the first folded packet really is
        /// the day's first trade. So the zero baseline can only ever add the
        /// first trade's own quantity, into the first trade's own bars.
        /// 2026-10-09: SEEDED for an F&O or equity key also gives the buckets
        /// holding 09:15 the seed (`with_open_seed`); currency and commodity
        /// keep the plain seed.
        #[test]
        fn zero_baseline_only_ever_adds_the_true_first_trade(
            t0_off in 0u32..20_000,
            proof_lead in 0u32..120,
            skew in -10i32..=10,
            n_proofs in 1usize..5,
            seg_pick in any::<u8>(),
            trades in prop::collection::vec(trade(), 1..12),
        ) {
            let seg = seg_of(seg_pick);
            let t0 = OPEN + t0_off;
            let (stream, truth) = build(seg, t0, proof_lead, skew, n_proofs, &trades, None);
            let got = run(&stream);
            let folded: Vec<&ParsedTick> = stream.iter().filter(|t| t.exchange_timestamp > DAY).collect();
            let Some(first) = folded.first() else { return Ok(()); };
            let model = |base: u64| {
                let mut last = base;
                let mut out = Vec::new();
                for (i, t) in folded.iter().enumerate() {
                    let c = u64::from(t.volume);
                    if i > 0 && c < last { continue; }
                    if c > last { out.push((t.exchange_timestamp, c - last)); }
                    last = c;
                }
                bars_of(&out)
            };
            let seed = u64::from(first.volume);
            let seeded_model = if matches!(seg, EQ | FNO | 4) {
                with_open_seed(model(seed), first.exchange_timestamp, seed)
            } else {
                model(seed)
            };
            let seeded = diff(&got, &seeded_model);
            let zero = diff(&got, &model(0));
            prop_assert!(seeded.is_empty() || zero.is_empty(),
                "fold matches neither reference: seeded {:?} zero {:?}", seeded, zero);
            if zero.is_empty() && !seeded.is_empty() {
                prop_assert_eq!(first.exchange_timestamp, truth[0].0, "zero baseline on a packet that is not the first trade");
                prop_assert_eq!(u64::from(first.volume), truth[0].1, "zero baseline carried earlier volume");
            }
        }
    }
}
