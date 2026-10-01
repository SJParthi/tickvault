//! Audit PR58 (untraded-proof zero baseline) against the plan-47 restart /
//! WAL-replay / hand-over layer (review 2026-10-01). A restarted run must never
//! write a bar that differs from, or exceeds, an uninterrupted run, with
//! untraded-proof packets in the stream; and a pre-market restart that replays
//! the morning's connect snapshots must withhold no first bar. Case count:
//! `FIRST_TRADE_DIFFERENTIAL_CASES` (default 512).

use std::collections::BTreeMap;

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::candles::aggregator_cell::FeedStrategy;
use tickvault_trading::candles::multi_tf_aggregator::MultiTfAggregator;
use tickvault_trading::candles::{LiveCandleState, TfIndex};

const DAY: u32 = 1_779_321_600;
const OPEN: u32 = DAY + 33_300; // 09:15:00
const MARGIN: u32 = 240;
const EARLY_CAPTURE: u32 = DAY + 32_000; // 08:53:20
const FNO: u8 = 2;
const EQ: u8 = 1;
const SID: u64 = 58_780;

fn hms(h: u32, m: u32, s: u32) -> u32 {
    DAY + h * 3600 + m * 60 + s
}

fn trade(seg: u8, ts: u32, price: f32, cum: u32, recv: u32) -> ParsedTick {
    ParsedTick {
        security_id: SID,
        exchange_segment_code: seg,
        last_traded_price: price,
        exchange_timestamp: ts,
        volume: cum,
        day_open: 13.0,
        day_close: 12.5,
        day_high: 14.0,
        day_low: 12.0,
        received_at_nanos: (i64::from(recv) - 19_800) * 1_000_000_000,
        ..Default::default()
    }
}

/// Prior-day last-trade-time snapshot (the connect / book-change packet of a
/// key that has not traded today): kind 0 stale LTT with price, 1 zero price
/// beside a stale LTT, 2 zero LTT and zero price.
fn proof(seg: u8, kind: u8, recv: u32) -> ParsedTick {
    let mut t = trade(seg, DAY - 86_400 + 55_000, 12.5, 90_000, recv);
    match kind {
        1 => {
            t.last_traded_price = 0.0;
            t.volume = 0;
        }
        2 => {
            t.exchange_timestamp = 0;
            t.last_traded_price = 0.0;
            t.volume = 0;
        }
        _ => {}
    }
    t.day_open = 0.0;
    t.day_high = 0.0;
    t.day_low = 0.0;
    t
}

/// The same packet as `proof` but refused WITHOUT recording a proof (a zero
/// LTT beside a real price): the pre-PR58 behaviour of a proof packet, with
/// the same top-of-function side effects (capture-start confirmation, last
/// replayed frame).
fn no_proof(seg: u8, recv: u32) -> ParsedTick {
    let mut t = proof(seg, 0, recv);
    t.exchange_timestamp = 0;
    t
}

type Key = (u64, usize, u32);

#[derive(Default, Debug)]
struct Written {
    last: BTreeMap<Key, LiveCandleState>,
    max_volume: BTreeMap<Key, u64>,
}

impl Written {
    fn put(&mut self, sid: u64, tf: TfIndex, bar: LiveCandleState) {
        let key = (sid, tf.as_ordinal(), bar.bucket_start_ist_secs);
        let m = self.max_volume.entry(key).or_insert(0);
        *m = (*m).max(bar.volume);
        self.last.insert(key, bar);
    }
    fn vol(&self, tf: TfIndex, start: u32) -> Option<u64> {
        self.last
            .get(&(SID, tf.as_ordinal(), start))
            .map(|b| b.volume)
    }
}

fn recv_secs(t: &ParsedTick) -> u32 {
    (t.received_at_nanos / 1_000_000_000 + 19_800) as u32
}

fn agg() -> MultiTfAggregator {
    let mut a = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 16);
    a.set_catch_up_margin_secs(MARGIN);
    a.seed_watermark_at_least(DAY);
    a
}

fn feed_live(a: &mut MultiTfAggregator, ticks: &[ParsedTick], out: &mut Written) {
    for t in ticks {
        a.consume_tick(Feed::Dhan, t, None, |_, sid, _, tf, bar| {
            out.put(sid, tf, bar)
        });
        let wall = recv_secs(t);
        let cutoff = a.watermark_secs().min(wall).saturating_sub(MARGIN);
        a.catch_up_seal_all(cutoff, |_, sid, _, tf, bar| out.put(sid, tf, bar));
    }
}

fn truth(ticks: &[ParsedTick]) -> Written {
    let mut out = Written::default();
    let mut a = agg();
    a.set_live_capture_start(EARLY_CAPTURE);
    feed_live(&mut a, ticks, &mut out);
    a.force_seal_all(|_, sid, _, tf, bar| out.put(sid, tf, bar));
    out
}

/// Replays `replay` (frames whose index is in `unreadable` are skipped and
/// mark a gap), hands over at `capture_start`, then folds `live`.
fn restart(
    replay: &[ParsedTick],
    unreadable: &[usize],
    capture_start: u32,
    live: &[ParsedTick],
) -> Written {
    let mut out = Written::default();
    let mut a = agg();
    a.set_replay_mode(true);
    let mut gap = true;
    for (k, t) in replay.iter().enumerate() {
        if unreadable.contains(&k) {
            gap = true;
            continue;
        }
        if std::mem::replace(&mut gap, false) {
            a.mark_replay_gap();
        }
        a.consume_tick(Feed::Dhan, t, None, |_, sid, _, tf, bar| {
            out.put(sid, tf, bar)
        });
    }
    a.set_live_capture_start(capture_start);
    a.finish_replay(gap, |_, sid, _, tf, bar| out.put(sid, tf, bar));
    let cutoff = a.watermark_secs().min(capture_start).saturating_sub(MARGIN);
    a.catch_up_seal_all(cutoff, |_, sid, _, tf, bar| out.put(sid, tf, bar));
    feed_live(&mut a, live, &mut out);
    a.force_seal_all(|_, sid, _, tf, bar| out.put(sid, tf, bar));
    out
}

fn dump(label: &str, w: &Written) {
    let mut lines = Vec::new();
    for ((_, tf, start), bar) in &w.last {
        let tf = TfIndex::ALL.iter().find(|t| t.as_ordinal() == *tf).unwrap();
        if tf.seconds_per_bucket() >= 60 {
            lines.push(format!(
                "{tf:?}@{:+} v{}",
                *start as i64 - OPEN as i64,
                bar.volume
            ));
        }
    }
    eprintln!("{label}: {}", lines.join(" "));
}

/// Compare every bar the restart wrote with the truth: over-count / wrong.
fn diff(truth: &Written, r: &Written) -> (Vec<String>, Vec<String>, usize) {
    let mut over = Vec::new();
    let mut wrong = Vec::new();
    let mut withheld = 0;
    for (k, &max) in &r.max_volume {
        let t = truth.last.get(k).map_or(0, |b| b.volume);
        if max > t {
            over.push(format!("{k:?} restart max {max} > truth {t}"));
        }
    }
    for (k, b) in &r.last {
        match truth.last.get(k) {
            Some(t)
                if t.volume == b.volume
                    && t.open == b.open
                    && t.high == b.high
                    && t.low == b.low
                    && t.close == b.close => {}
            t => wrong.push(format!(
                "{k:?} restart v{} o{} c{} | truth {:?}",
                b.volume,
                b.open,
                b.close,
                t.map(|t| (t.volume, t.open, t.close))
            )),
        }
    }
    for k in truth.last.keys() {
        if !r.last.contains_key(k) {
            withheld += 1;
        }
    }
    (over, wrong, withheld)
}

/// The day's trades for SID (an option), first at 09:15:02.
fn ltq(mut t: ParsedTick, q: u16) -> ParsedTick {
    t.last_trade_quantity = q;
    t
}

fn day_trades(seg: u8) -> Vec<ParsedTick> {
    vec![
        ltq(trade(seg, OPEN + 2, 13.0, 650, OPEN + 2), 650),
        ltq(trade(seg, OPEN + 3, 13.05, 700, OPEN + 3), 50),
        ltq(trade(seg, OPEN + 70, 13.1, 900, OPEN + 70), 200),
        ltq(trade(seg, OPEN + 400, 13.2, 1_200, OPEN + 400), 300),
        ltq(trade(seg, OPEN + 4_000, 13.3, 2_000, OPEN + 4_000), 800),
    ]
}

// ---------------------------------------------------------------------------
// S1: pre-market restart (08:50) whose WAL holds the 08:40 connect snapshot.
// ---------------------------------------------------------------------------

fn premarket_case(with_proof: bool, seg: u8) -> (Written, Written) {
    let p = |recv| {
        if with_proof {
            proof(seg, 0, recv)
        } else {
            no_proof(seg, recv)
        }
    };
    let mut all = vec![p(hms(8, 40, 0)), p(hms(8, 50, 5))];
    all.extend(day_trades(seg));
    let t = truth(&all);
    // Previous process crashed at 08:45; WAL = [08:40 snapshot].
    let mut live = vec![p(hms(8, 50, 5))];
    live.extend(day_trades(seg));
    let r = restart(&all[..1], &[], hms(8, 50, 0), &live);
    (t, r)
}

#[test]
fn s1_premarket_restart_with_replayed_proof() {
    for seg in [FNO, EQ] {
        for with_proof in [false, true] {
            let (t, r) = premarket_case(with_proof, seg);
            eprintln!("--- seg {seg} with_proof {with_proof}");
            dump("truth  ", &t);
            dump("restart", &r);
            let (over, wrong, withheld) = diff(&t, &r);
            eprintln!(
                "over {} wrong {} withheld {}: {over:?} {wrong:?}",
                over.len(),
                wrong.len(),
                withheld
            );
            assert!(
                over.is_empty() && wrong.is_empty() && withheld == 0,
                "seg {seg} with_proof {with_proof}"
            );
        }
    }
}

/// Minimal reproduction: an 08:50 restart that replayed one pre-market
/// connect snapshot withholds the instrument's first bar of EVERY timeframe
/// (09:15 1m, 3m, 5m, 15m, 30m, 60m). Without the replayed snapshot (pre-PR58
/// the snapshot created no slot) the same restart writes them all.
#[test]
fn test_regression_premarket_restart_with_replayed_proof_writes_every_first_bar() {
    let (t_before, r_before) = premarket_case(false, FNO);
    let (t_after, r_after) = premarket_case(true, FNO);
    for tf in TfIndex::ALL {
        let first = tf.bucket_start(OPEN + 2);
        // Before PR58 the restart wrote the bar, identical to the truth.
        assert_eq!(
            r_before.vol(tf, first),
            t_before.vol(tf, first),
            "{tf:?} before"
        );
        assert!(r_before.vol(tf, first).is_some(), "{tf:?} written before");
        // After the fix the restart writes the bar, equal to the truth, which
        // holds the first trade (no first bar withheld after a replayed proof).
        assert!(t_after.vol(tf, first).is_some());
        assert_eq!(
            r_after.vol(tf, first),
            t_after.vol(tf, first),
            "{tf:?} after"
        );
        let (over, wrong, withheld) = diff(&t_after, &r_after);
        assert!(
            over.is_empty() && wrong.is_empty() && withheld == 0,
            "{over:?} {wrong:?} {withheld}"
        );
    }
}

// ---------------------------------------------------------------------------
// S2: mid-session restart, WAL holds proofs, crash before the first trade,
// first trade arrives live within the proof window.
// ---------------------------------------------------------------------------

#[test]
fn s2_midsession_restart_proof_replayed_trade_live() {
    for seg in [FNO, EQ] {
        for (crash, cap) in [
            (hms(9, 14, 30), hms(9, 14, 40)),
            (OPEN + 1, OPEN + 1),
            (hms(9, 14, 50), OPEN + 10),
        ] {
            let mut all = vec![
                proof(seg, 0, hms(9, 14, 0)),
                proof(seg, 1, hms(9, 14, 20)),
                proof(seg, 2, crash),
            ];
            all.extend(day_trades(seg));
            let t = truth(&all);
            let replay: Vec<_> = all
                .iter()
                .filter(|x| recv_secs(x) <= crash)
                .cloned()
                .collect();
            let mut live = vec![proof(seg, 0, cap)];
            live.extend(
                all.iter()
                    .filter(|x| recv_secs(x) >= cap && recv_secs(x) > crash)
                    .cloned(),
            );
            let r = restart(&replay, &[], cap, &live);
            let (over, wrong, withheld) = diff(&t, &r);
            eprintln!(
                "S2 seg {seg} crash {} cap {}: over {over:?} wrong {wrong:?} withheld {withheld}",
                crash as i64 - OPEN as i64,
                cap as i64 - OPEN as i64
            );
            assert!(over.is_empty() && wrong.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// S3: replay contains proof, then T1 (unreadable), then T2 -- a skipped trade
// must never be credited to T2's bar.
// ---------------------------------------------------------------------------

#[test]
fn s3_replay_gap_between_proof_and_first_trade() {
    for seg in [FNO, EQ] {
        let mut all = vec![proof(seg, 0, hms(9, 14, 30)), proof(seg, 0, OPEN + 1)];
        all.extend(day_trades(seg));
        let t = truth(&all);
        // Crash after the 4th trade; T1 (index 2) unreadable.
        let replay: Vec<_> = all[..6].to_vec();
        for unreadable in [vec![2usize], vec![1, 2], vec![0, 2], vec![2, 3]] {
            let live: Vec<_> = all[6..].to_vec();
            let r = restart(&replay, &unreadable, OPEN + 4_000, &live);
            let (over, wrong, withheld) = diff(&t, &r);
            eprintln!(
                "S3 seg {seg} unreadable {unreadable:?}: over {over:?} wrong {wrong:?} withheld {withheld}"
            );
            assert!(over.is_empty() && wrong.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// S4: boot (no replay at all) at 08:50 / 09:14 / 09:16 with a live proof.
// ---------------------------------------------------------------------------

#[test]
fn s4_fresh_boot_times() {
    for seg in [FNO, EQ] {
        for boot in [hms(8, 50, 0), hms(8, 58, 0), hms(9, 14, 0), OPEN + 1] {
            let mut all = vec![proof(seg, 0, boot + 5)];
            all.extend(day_trades(seg).into_iter().filter(|x| recv_secs(x) >= boot));
            let t = truth(&all);
            let mut out = Written::default();
            let mut a = agg();
            a.set_live_capture_start(boot);
            feed_live(&mut a, &all, &mut out);
            a.force_seal_all(|_, sid, _, tf, bar| out.put(sid, tf, bar));
            let (over, wrong, withheld) = diff(&t, &out);
            eprintln!(
                "S4 seg {seg} boot {:+}: M1@open truth {:?} boot {:?}; over {over:?} wrong {wrong:?} withheld {withheld}",
                boot as i64 - OPEN as i64,
                t.vol(TfIndex::M1, OPEN),
                out.vol(TfIndex::M1, OPEN)
            );
            assert!(over.is_empty() && wrong.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// S5: the slot created early by a replayed proof; gap marked between proof
// and the first (replayed) trade; trade's own frame read. The epoch captured
// at proof time is older, so the gap applies to the first trade.
// ---------------------------------------------------------------------------

#[test]
fn s5_gap_between_proof_and_readable_first_trade() {
    for seg in [FNO, EQ] {
        // An unrelated frame (index 1, another key) is unreadable between
        // the proof and T1: a gap with nothing of this key skipped.
        let mut other = ltq(trade(seg, OPEN + 1, 50.0, 10, OPEN + 1), 10);
        other.security_id = SID + 1;
        let mut all = vec![proof(seg, 0, hms(9, 14, 50)), other];
        all.extend(day_trades(seg));
        let t = truth(&all);
        let replay: Vec<_> = all[..5].to_vec();
        let live: Vec<_> = all[5..].to_vec();
        let r = restart(&replay, &[1], OPEN + 4_000, &live);
        let (over, wrong, withheld) = diff(&t, &r);
        eprintln!(
            "S5 seg {seg}: M1@open truth {:?} restart {:?}; over {over:?} wrong {wrong:?} withheld {withheld}",
            t.vol(TfIndex::M1, OPEN),
            r.vol(TfIndex::M1, OPEN)
        );
        assert!(over.is_empty() && wrong.is_empty());
    }
}

// ---------------------------------------------------------------------------
// S6: reconnect at 09:15 with a snapshot, no process restart: proof 09:14:50,
// socket down 09:15:01..09:15:40, T1 at 09:15:05 lost, reconnect snapshot
// carries T2 at 09:15:30 (cumulative includes T1).
// ---------------------------------------------------------------------------

#[test]
fn s6_feed_reconnect_snapshot_without_restart() {
    for seg in [FNO, EQ] {
        let p = proof(seg, 0, hms(9, 14, 50));
        let t1 = ltq(trade(seg, OPEN + 5, 13.0, 400, OPEN + 5), 400);
        let t2 = ltq(trade(seg, OPEN + 30, 13.1, 650, OPEN + 30), 250);
        let t2_snapshot = ltq(trade(seg, OPEN + 30, 13.1, 650, OPEN + 40), 250);
        let t3 = ltq(trade(seg, OPEN + 45, 13.2, 700, OPEN + 45), 50);
        let truth_w = truth(&[p.clone(), t1, t2, t3.clone()]);
        let mut out = Written::default();
        let mut a = agg();
        a.set_live_capture_start(EARLY_CAPTURE);
        feed_live(&mut a, &[p, t2_snapshot, t3], &mut out);
        a.force_seal_all(|_, sid, _, tf, bar| out.put(sid, tf, bar));
        let (over, wrong, _) = diff(&truth_w, &out);
        eprintln!("S6 seg {seg}: over {over:?} wrong {wrong:?}");
        assert!(over.is_empty(), "seg {seg}: no bar above the truth");
    }
}

fn mk(with_proof: bool, seg: u8, kind: u8, recv: u32) -> ParsedTick {
    if with_proof {
        proof(seg, kind, recv)
    } else {
        no_proof(seg, recv)
    }
}

/// Withheld / over / wrong counts before (proof packets record nothing) and
/// after PR58, for the restart scenarios S2, S3, S5, S7.
#[test]
fn compare_before_after_restart_scenarios() {
    for seg in [FNO, EQ] {
        for with_proof in [false, true] {
            // S2: proofs replayed, crash 09:14:30, listen 09:14:40.
            let (crash, cap) = (hms(9, 14, 30), hms(9, 14, 40));
            let mut all = vec![
                mk(with_proof, seg, 0, hms(9, 14, 0)),
                mk(with_proof, seg, 1, hms(9, 14, 20)),
            ];
            all.extend(day_trades(seg));
            let t = truth(&all);
            let replay: Vec<_> = all
                .iter()
                .filter(|x| recv_secs(x) <= crash)
                .cloned()
                .collect();
            let mut live = vec![mk(with_proof, seg, 0, cap)];
            live.extend(all.iter().filter(|x| recv_secs(x) > crash).cloned());
            let r = restart(&replay, &[], cap, &live);
            let (o, w, h) = diff(&t, &r);
            eprintln!(
                "S2 seg {seg} proof {with_proof}: truth M1@open {:?} restart {:?} over {} wrong {} withheld {h}",
                t.vol(TfIndex::M1, OPEN),
                r.vol(TfIndex::M1, OPEN),
                o.len(),
                w.len()
            );
            assert!(o.is_empty() && w.is_empty());

            // S3: proof, T1 unreadable, crash after the 4th trade.
            let mut all = vec![
                mk(with_proof, seg, 0, hms(9, 14, 30)),
                mk(with_proof, seg, 0, OPEN + 1),
            ];
            all.extend(day_trades(seg));
            let t = truth(&all);
            let r = restart(&all[..6], &[2], OPEN + 4_000, &all[6..]);
            let (o, w, h) = diff(&t, &r);
            eprintln!(
                "S3 seg {seg} proof {with_proof}: over {} wrong {} withheld {h}",
                o.len(),
                w.len()
            );
            assert!(o.is_empty() && w.is_empty());

            // S5: proof, gap on another key, T1 readable.
            let mut other = ltq(trade(seg, OPEN + 1, 50.0, 10, OPEN + 1), 10);
            other.security_id = SID + 1;
            let mut all = vec![mk(with_proof, seg, 0, hms(9, 14, 50)), other];
            all.extend(day_trades(seg));
            let t = truth(&all);
            let r = restart(&all[..5], &[1], OPEN + 4_000, &all[5..]);
            let (o, w, h) = diff(&t, &r);
            let h_sid = t
                .last
                .keys()
                .filter(|k| k.0 == SID && !r.last.contains_key(k))
                .count();
            eprintln!(
                "S5 seg {seg} proof {with_proof}: over {} wrong {} withheld {h} (sid {h_sid})",
                o.len(),
                w.len()
            );
            assert!(o.is_empty() && w.is_empty());

            // S7: proof received during replay, the WHOLE day's trades live
            // (crash 09:14:55, listen 09:15:00, every trade received live).
            let mut all = vec![mk(with_proof, seg, 0, hms(9, 14, 50))];
            all.extend(day_trades(seg));
            let t = truth(&all);
            let r = restart(&all[..1], &[], OPEN, &all[1..]);
            let (o, w, h) = diff(&t, &r);
            eprintln!(
                "S7 seg {seg} proof {with_proof}: truth M1@open {:?} restart {:?} over {} wrong {} withheld {h}",
                t.vol(TfIndex::M1, OPEN),
                r.vol(TfIndex::M1, OPEN),
                o.len(),
                w.len()
            );
            assert!(o.is_empty() && w.is_empty());
        }
    }
}

// ===========================================================================
// The restart differential (copied from restart_differential.rs) with
// untraded-today proof packets before each instrument's first trade.
// ===========================================================================
#[allow(clippy::all)]
mod differential {
    use std::collections::{HashMap, HashSet};

    use proptest::prelude::*;
    use tickvault_common::feed::Feed;
    use tickvault_common::tick_types::ParsedTick;
    use tickvault_trading::candles::aggregator_cell::FeedStrategy;
    use tickvault_trading::candles::multi_tf_aggregator::MultiTfAggregator;
    use tickvault_trading::candles::{LiveCandleState, TfIndex};

    /// An IST trading day's midnight, in IST epoch seconds.
    const DAY: u32 = 1_779_321_600;
    /// 09:16:00 IST: the first trade second.
    const START: u32 = DAY + 33_360;
    /// The live catch-up margin production runs with.
    const MARGIN: u32 = 240;
    /// A capture start well before the session, for the uninterrupted runs.
    const EARLY_CAPTURE: u32 = DAY + 32_000;
    /// IST minus UTC, in milliseconds.
    const IST_OFFSET_MS: i64 = 19_800_000;
    /// The fold's clock-skew allowance (`REPLAY_FRONTIER_SKEW_SECS`).
    const SKEW_SECS: u64 = 5;
    /// Out of 100: a replayed WAL frame below this is unreadable.
    const UNREADABLE_PERCENT: u8 = 4;

    #[derive(Clone, Debug)]
    struct Event {
        dt: u8,
        size: u16,
        price_steps: i8,
        lag_ms: u32,
        resend_ms: Option<u16>,
        delivered: bool,
        /// Delivered on its own lag, possibly after a later trade of the same
        /// instrument (review round 23: two downtime trades that arrive late and
        /// out of order after the socket listens).
        out_of_order: bool,
    }

    #[derive(Clone, Debug)]
    struct Case {
        instruments: Vec<Vec<Event>>,
        crash_per_mille: u16,
        start_after_crash: u32,
        listen_after_start: Vec<u32>,
        unreadable: Vec<u8>,
        /// Per instrument, what its subscribe re-sends at the listen instant:
        /// 0 nothing, 1 its latest state (the last packet received before it
        /// listened), 2 a STALE copy of the last trade the previous process
        /// received (review round 20), which looks exactly like a true re-send.
        resend_on_subscribe: Vec<u8>,
        /// Both runs end with the day-close seal (`force_seal_all`) instead of a
        /// final catch-up sweep (review round 24: the day-close seal judged
        /// bars after clearing each instrument's capture start, and no run here
        /// ever reached it).
        day_close: bool,
        /// Every stock's cumulative starts just below `u32::MAX` and wraps
        /// (review round 25: no run here ever came near the top of the range).
        near_wrap: bool,
        /// PR58 attack: per instrument, the "untraded today" packets it sends
        /// before its first trade.
        proofs: Vec<ProofSpec>,
    }

    #[derive(Clone, Debug)]
    struct ProofSpec {
        /// 0 none, 1 prior-day last-trade time with a price, 2 zero price beside
        /// a prior-day last-trade time, 3 zero last-trade time and zero price.
        kind: u8,
        /// Connect snapshot, this many seconds before the first trade's stamp.
        connect_before: u32,
        /// Book-change re-sends, at the first trade's stamp plus these seconds
        /// (negative = before it; up to +7 = inside or just past the skew).
        resends: Vec<i32>,
        /// A copy delivered out of order AFTER the first trade's receipt, this
        /// many ms later (generated before the trade, delivered late).
        late_ms: Option<u32>,
    }

    fn proof_spec() -> impl Strategy<Value = ProofSpec> {
        (
            prop_oneof![1 => Just(0u8), 3 => 1u8..=3],
            prop_oneof![1 => 0u32..=90, 1 => 0u32..=1_500],
            prop::collection::vec(-120i32..=7, 0..5),
            prop_oneof![3 => Just(None), 1 => (0u32..=60_000).prop_map(Some)],
        )
            .prop_map(|(kind, connect_before, resends, late_ms)| ProofSpec {
                kind,
                connect_before,
                resends,
                late_ms,
            })
    }

    fn event() -> impl Strategy<Value = Event> {
        (
            1u8..=20,
            0u16..=40,
            -3i8..=3,
            prop_oneof![500 => 0u32..=10_000, 16 => 10_001u32..=200_000, 1 => 240_001u32..=400_000],
            prop_oneof![9 => Just(None), 1 => (0u16..=3_000).prop_map(Some)],
            prop_oneof![9 => Just(true), 1 => Just(false)],
            prop_oneof![14 => Just(false), 1 => Just(true)],
        )
            .prop_map(
                |(dt, size, price_steps, lag_ms, resend_ms, delivered, out_of_order)| Event {
                    dt,
                    size,
                    price_steps,
                    lag_ms,
                    resend_ms,
                    delivered,
                    out_of_order,
                },
            )
    }

    fn case() -> impl Strategy<Value = Case> {
        (
            prop::collection::vec(prop::collection::vec(event(), 1..150), 2..=4),
            50u16..=950,
            0u32..=30,
            prop::collection::vec(0u32..=20, 4),
            prop::collection::vec(0u8..100, 64..600),
            prop::collection::vec(0u8..3, 4),
            prop::bool::weighted(0.35),
            prop::bool::weighted(0.2),
            prop::collection::vec(proof_spec(), 4),
        )
            .prop_map(
                |(
                    instruments,
                    crash_per_mille,
                    start_after_crash,
                    listen_after_start,
                    unreadable,
                    resend_on_subscribe,
                    day_close,
                    near_wrap,
                    proofs,
                )| Case {
                    instruments,
                    crash_per_mille,
                    start_after_crash,
                    listen_after_start,
                    unreadable,
                    resend_on_subscribe,
                    day_close,
                    near_wrap,
                    proofs,
                },
            )
    }

    #[derive(Clone, Copy, Debug)]
    struct Packet {
        instrument: usize,
        trade: u32,
        /// The true day-cumulative on a monotonic 64-bit axis. The packet the
        /// fold sees carries its low 32 bits, as the vendor's `u32` field does,
        /// so a run that starts near the top of the range wraps (review round 25).
        cumulative: u64,
        price: f32,
        high: f32,
        low: f32,
        open: f32,
        received_ms: u64,
        /// 0 a trade; 1..=3 an untraded-today proof of that `ProofSpec::kind`.
        kind: u8,
        /// This trade own quantity (Dhan last-trade quantity); re-sends repeat it.
        ltq: u16,
    }

    /// Instrument 0 is an index (NSE index segment, volume always 0); the rest
    /// are stocks.
    fn security_id(instrument: usize) -> u64 {
        if instrument == 0 {
            13
        } else {
            100 + instrument as u64
        }
    }

    fn segment(instrument: usize) -> u8 {
        match instrument {
            0 => 0,
            2 => 2,
            _ => 1,
        }
    }

    fn tick(p: &Packet, received_ms: u64) -> ParsedTick {
        if p.kind != 0 {
            let (trade, price, volume) = match p.kind {
                1 => (DAY - 86_400 + 55_000, 990.0, 777_000),
                2 => (DAY - 86_400 + 55_000, 0.0, 0),
                // 4: a refused packet that proves nothing (zero LTT, real price).
                4 => (0, 990.0, 0),
                _ => (0, 0.0, 0),
            };
            return ParsedTick {
                security_id: security_id(p.instrument),
                exchange_segment_code: segment(p.instrument),
                last_traded_price: price,
                exchange_timestamp: trade,
                volume,
                day_close: 1_000.0,
                received_at_nanos: (received_ms as i64 - IST_OFFSET_MS) * 1_000_000,
                ..Default::default()
            };
        }
        ParsedTick {
            security_id: security_id(p.instrument),
            exchange_segment_code: segment(p.instrument),
            last_traded_price: p.price,
            exchange_timestamp: p.trade,
            volume: u32::try_from(p.cumulative % (1_u64 << 32)).unwrap_or(0),
            last_trade_quantity: p.ltq,
            day_open: p.open,
            day_close: 1_000.0,
            day_high: p.high,
            day_low: p.low,
            received_at_nanos: (received_ms as i64 - IST_OFFSET_MS) * 1_000_000,
            ..Default::default()
        }
    }

    /// Every delivered packet, in receipt order.
    fn packets(c: &Case) -> Vec<Packet> {
        let mut out = Vec::new();
        for (instrument, events) in c.instruments.iter().enumerate() {
            // Stocks of a near-wrap case start 300 units below `u32::MAX`, so their
            // counter wraps early in the day (review round 25).
            let spec = &c.proofs[instrument];
            let base = if c.near_wrap && instrument != 0 && spec.kind == 0 {
                u64::from(u32::MAX) - 300
            } else {
                0
            };
            let (mut trade, mut cumulative, mut price) = (START, base, 1_000.0f32);
            let (mut high, mut low, mut open, mut last_received) = (0f32, f32::MAX, 0f32, 0u64);
            if spec.kind != 0
                && let Some(first) = events.first()
            {
                let first_trade = START + u32::from(first.dt);
                let mut at = vec![i64::from(first_trade) - i64::from(spec.connect_before)];
                at.extend(
                    spec.resends
                        .iter()
                        .map(|&d| i64::from(first_trade) + i64::from(d)),
                );
                for secs in at {
                    out.push(Packet {
                        instrument,
                        trade: 0,
                        cumulative: 0,
                        price: 0.0,
                        high: 0.0,
                        low: 0.0,
                        open: 0.0,
                        received_ms: u64::try_from(secs).unwrap_or(0) * 1000 + 37,
                        kind: spec.kind,
                        ltq: 0,
                    });
                }
            }
            for (k, e) in events.iter().enumerate() {
                trade += u32::from(e.dt);
                if instrument != 0 {
                    cumulative += u64::from(e.size);
                }
                price = (price + f32::from(e.price_steps) * 0.05).max(1.0);
                if open == 0.0 {
                    open = price;
                }
                high = high.max(price);
                low = low.min(price);
                if !e.delivered {
                    continue;
                }
                let own = u64::from(trade) * 1000 + u64::from(e.lag_ms);
                // Stocks only: a stock packet older than one already accepted is
                // refused by its cumulative, so either process drops it the same
                // way. An index (volume always 0) cannot be refused like that, and
                // whether a late one amends a bar or is discarded depends on packets
                // the restart lost in the downtime (a recorded limit). Only a trade
                // the next one adds volume after: a real stock packet with an equal
                // cumulative is the same last trade, at the same price, while this
                // generator moves a price with no volume (pre-open quotes), which
                // would make an older packet indistinguishable from a newer one.
                let out_of_order = e.out_of_order
                    && instrument != 0
                    && events.get(k + 1).is_some_and(|next| next.size > 0);
                let received_ms = if out_of_order {
                    own
                } else {
                    last_received = last_received.max(own);
                    last_received
                };
                let p = Packet {
                    instrument,
                    trade,
                    cumulative,
                    price,
                    high,
                    low,
                    open,
                    received_ms,
                    kind: 0,
                    ltq: if instrument == 0 { 0 } else { e.size },
                };
                out.push(p);
                if k == 0 && spec.kind != 0 {
                    let proof_at = |ms: u64| Packet {
                        instrument,
                        trade: 0,
                        cumulative: 0,
                        price: 0.0,
                        high: 0.0,
                        low: 0.0,
                        open: 0.0,
                        received_ms: ms,
                        kind: spec.kind,
                        ltq: 0,
                    };
                    if let Some(late) = spec.late_ms {
                        out.push(proof_at(received_ms + u64::from(late)));
                    }
                }
                if let Some(resend) = e.resend_ms {
                    let again = if out_of_order {
                        received_ms + u64::from(resend)
                    } else {
                        last_received += u64::from(resend);
                        last_received
                    };
                    out.push(Packet {
                        received_ms: again,
                        ..p
                    });
                }
            }
        }
        out.sort_by_key(|p| (p.received_ms, p.instrument, p.cumulative));
        out
    }

    /// `(security_id, timeframe ordinal, bucket start)`.
    type Key = (u64, usize, u32);

    #[derive(Default)]
    struct Written {
        last: HashMap<Key, LiveCandleState>,
        max_volume: HashMap<Key, u64>,
    }

    impl Written {
        fn put(&mut self, sid: u64, tf: TfIndex, bar: LiveCandleState) {
            let key = (sid, tf.as_ordinal(), bar.bucket_start_ist_secs);
            let max = self.max_volume.entry(key).or_insert(0);
            *max = (*max).max(bar.volume);
            self.last.insert(key, bar);
        }
    }

    fn aggregator() -> MultiTfAggregator {
        let mut agg = MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, 16);
        agg.set_catch_up_margin_secs(MARGIN);
        agg.seed_watermark_at_least(DAY);
        agg
    }

    /// The live catch-up sweep the app runs: `min(watermark, wall) - margin`.
    fn sweep(agg: &mut MultiTfAggregator, wall_secs: u32, out: &mut Written) {
        let cutoff = agg.watermark_secs().min(wall_secs).saturating_sub(MARGIN);
        agg.catch_up_seal_all(cutoff, |_, sid, _, tf, bar| out.put(sid, tf, bar));
    }

    /// Ends a run: the day-close seal, or a final catch-up sweep to `end_cutoff`.
    fn finish(agg: &mut MultiTfAggregator, day_close: bool, end_cutoff: u32, out: &mut Written) {
        if day_close {
            agg.force_seal_all(|_, sid, _, tf, bar| out.put(sid, tf, bar));
        } else {
            agg.catch_up_seal_all(end_cutoff, |_, sid, _, tf, bar| out.put(sid, tf, bar));
        }
    }

    /// Folds `packets` live from a capture start before the session.
    fn fold_live<'a>(
        packets: impl Iterator<Item = &'a Packet>,
        out: &mut Written,
    ) -> MultiTfAggregator {
        let mut agg = aggregator();
        agg.set_live_capture_start(EARLY_CAPTURE);
        for p in packets {
            agg.consume_tick(
                Feed::Dhan,
                &tick(p, p.received_ms),
                None,
                |_, sid, _, tf, bar| out.put(sid, tf, bar),
            );
            sweep(&mut agg, (p.received_ms / 1000) as u32, out);
        }
        agg
    }

    fn same(a: &LiveCandleState, b: &LiveCandleState, with_net: bool) -> bool {
        a.volume == b.volume
            && a.open == b.open
            && a.high == b.high
            && a.low == b.low
            && a.close == b.close
            && (!with_net
                || (a.net_volume_signed == b.net_volume_signed
                    && a.net_volume_classified == b.net_volume_classified))
    }

    fn describe(bar: Option<&LiveCandleState>) -> String {
        bar.map_or_else(
            || "absent".to_string(),
            |b| {
                format!(
                    "v{} o{} h{} l{} c{} net {} {}",
                    b.volume,
                    b.open,
                    b.high,
                    b.low,
                    b.close,
                    b.net_volume_signed,
                    b.net_volume_classified
                )
            },
        )
    }

    fn check(c: &Case) -> Result<(), TestCaseError> {
        let all = packets(c);
        let (Some(first), Some(last)) = (all.first(), all.last()) else {
            return Ok(());
        };
        let end_cutoff = all.iter().map(|p| p.trade).max().unwrap_or(START) + 10_000;

        let mut truth = Written::default();
        let mut truth_agg = fold_live(all.iter(), &mut truth);
        finish(&mut truth_agg, c.day_close, end_cutoff, &mut truth);

        let crash_ms = first.received_ms
            + (last.received_ms - first.received_ms) * u64::from(c.crash_per_mille) / 1000;
        let crash_secs = (crash_ms / 1000) as u32;
        let capture_start = crash_secs + 1 + c.start_after_crash;
        let listen: Vec<u32> = (0..c.instruments.len())
            .map(|i| capture_start + c.listen_after_start[i])
            .collect();

        // The previous process: live up to the crash. What it sealed is what
        // the database holds for the restart to overwrite.
        let mut stored = Written::default();
        fold_live(
            all.iter().filter(|p| p.received_ms <= crash_ms),
            &mut stored,
        );

        // The restarted process: replay the WAL (some frames unreadable), hand
        // over, then fold what each socket receives once it listens.
        let mut restart = Written::default();
        let mut agg = aggregator();
        agg.set_replay_mode(true);
        // Per instrument, the last replayed packet the fold accepted (a stale one,
        // whose cumulative went backwards, is refused).
        let mut last_replayed: HashMap<usize, Packet> = HashMap::new();
        let mut gap = true;
        for (k, p) in all.iter().filter(|p| p.received_ms <= crash_ms).enumerate() {
            if c.unreadable[k % c.unreadable.len()] < UNREADABLE_PERCENT {
                gap = true;
                continue;
            }
            if p.kind == 0
                && last_replayed
                    .get(&p.instrument)
                    .is_none_or(|l| p.cumulative >= l.cumulative)
            {
                last_replayed.insert(p.instrument, *p);
            }
            if std::mem::replace(&mut gap, false) {
                agg.mark_replay_gap();
            }
            agg.consume_tick(
                Feed::Dhan,
                &tick(p, p.received_ms),
                None,
                |_, sid, _, tf, bar| restart.put(sid, tf, bar),
            );
        }
        agg.set_live_capture_start(capture_start);
        agg.finish_replay(gap, |_, sid, _, tf, bar| restart.put(sid, tf, bar));
        sweep(&mut agg, capture_start, &mut restart);

        // `(receipt ms, order, packet)`: a subscribe re-send (order 0) arrives
        // at the listen instant, before anything else received then.
        let mut live: Vec<(u64, u8, Packet)> = all
            .iter()
            .filter(|p| {
                p.received_ms > crash_ms && p.received_ms >= u64::from(listen[p.instrument]) * 1000
            })
            .map(|p| (p.received_ms, 1, *p))
            .collect();
        for (i, &resend) in c
            .resend_on_subscribe
            .iter()
            .enumerate()
            .take(c.instruments.len())
        {
            let listen_ms = u64::from(listen[i]) * 1000;
            let sent_by = match resend {
                1 => listen_ms,
                2 => crash_ms + 1,
                _ => 0,
            };
            if let Some(p) = all.iter().rfind(|p| {
                p.instrument == i && p.received_ms < sent_by && (resend == 1 || p.kind == 0)
            }) {
                live.push((listen_ms, 0, *p));
            }
        }
        live.sort_by_key(|(received, order, p)| (*received, *order, p.instrument, p.cumulative));
        // Per instrument: the first live packet's `(trade second, receipt ms)`.
        // Per instrument, the receipt of the first live packet that ends its
        // hand-over gap: not a repeat of the last replayed trade (a stale copy
        // cannot be told from one, review round 20) and not stale itself.
        let mut gap_ended: HashMap<usize, u64> = HashMap::new();
        // The cumulative of the packet that ended each instrument's gap (or
        // seeded it: every boot here is mid-session), and the trade second of the first later packet that
        // added volume, whose bars are withheld because the gap packet may have
        // been a stale copy (review round 20).
        // It is the running baseline from then on.
        let mut gap_cum: HashMap<usize, u64> = HashMap::new();
        // Every packet that added volume while the gap packet was untrusted,
        // `(trade second, receipt ms)`: each one that traded no later than the
        // instrument's listen time plus the skew, and the first that traded later
        // (review round 23). Their bars are withheld.
        let mut tainted: HashMap<usize, Vec<(u32, u64)>> = HashMap::new();
        let mut taint_done: HashSet<usize> = HashSet::new();
        // The instrument's later packets after the last of those: a late one's
        // volume is carried into the bucket its timeframe has open, or opens next,
        // when that bucket seals (review round 22), so that bucket is withheld too.
        let mut after_add: HashMap<usize, Vec<(u32, u64)>> = HashMap::new();
        let mut first_live: HashMap<usize, (u32, u64)> = HashMap::new();
        for (received, _, p) in &live {
            if p.kind == 0 {
                let repeats_or_stale = last_replayed.get(&p.instrument).is_some_and(|l| {
                    p.cumulative < l.cumulative
                        || (p.trade == l.trade
                            && p.cumulative == l.cumulative
                            && p.price.to_bits() == l.price.to_bits())
                });
                if let Some(cum) = gap_cum.get_mut(&p.instrument) {
                    if taint_done.contains(&p.instrument) {
                        let last = tainted
                            .get(&p.instrument)
                            .and_then(|t| t.last())
                            .map_or(0, |t| t.0);
                        // A later trade, not a re-delivered copy of that one.
                        if p.trade > last {
                            after_add
                                .entry(p.instrument)
                                .or_default()
                                .push((p.trade, *received));
                        }
                    } else if p.cumulative > *cum {
                        *cum = p.cumulative;
                        tainted
                            .entry(p.instrument)
                            .or_default()
                            .push((p.trade, *received));
                        let listen_secs = gap_ended.get(&p.instrument).map_or(0, |r| r / 1000);
                        if u64::from(p.trade) > listen_secs + SKEW_SECS {
                            taint_done.insert(p.instrument);
                        }
                    }
                } else if !repeats_or_stale {
                    gap_ended.entry(p.instrument).or_insert(*received);
                    // A gap packet that traded after the instrument's first live
                    // receipt cannot be a stale copy (review round 21); a seeding
                    // packet of an instrument the replay never saw always may be.
                    let first_receipt_secs =
                        first_live.get(&p.instrument).map_or(*received, |f| f.1) / 1000;
                    let trusted = last_replayed.contains_key(&p.instrument)
                        && u64::from(p.trade) > first_receipt_secs + SKEW_SECS;
                    if !trusted {
                        gap_cum.insert(p.instrument, p.cumulative);
                    }
                }
                first_live
                    .entry(p.instrument)
                    .or_insert((p.trade, *received));
            }
            agg.consume_tick(
                Feed::Dhan,
                &tick(p, *received),
                None,
                |_, sid, _, tf, bar| restart.put(sid, tf, bar),
            );
            sweep(&mut agg, (*received / 1000) as u32, &mut restart);
        }
        finish(&mut agg, c.day_close, end_cutoff, &mut restart);
        // The first live receipt of ANY instrument confirms the capture start.
        let confirmed_start = live.first().map_or(capture_start, |(received, _, _)| {
            capture_start.max((*received / 1000) as u32)
        });

        for (i, &listen_at) in listen.iter().enumerate() {
            let sid = security_id(i);
            for tf in TfIndex::ALL {
                let mut starts: Vec<u32> = truth
                    .last
                    .keys()
                    .chain(restart.last.keys())
                    .filter(|k| k.0 == sid && k.1 == tf.as_ordinal())
                    .map(|k| k.2)
                    .collect();
                starts.sort_unstable();
                starts.dedup();
                for start in starts {
                    let key = (sid, tf.as_ordinal(), start);
                    let end = start + tf.seconds_per_bucket();
                    let (t, r) = (truth.last.get(&key), restart.last.get(&key));
                    // The latest instant the fold can treat as "before this
                    // instrument was captured": the confirmed capture start, or
                    // the receipt of the packet that ended the instrument's gap. An
                    // instrument whose gap never ended never confirmed at all.
                    let captured_from = gap_ended
                        .get(&i)
                        .map(|&received| confirmed_start.max((received / 1000) as u32));
                    let context = format!(
                        "sid {sid} {tf:?} bucket +{}..+{}, crash +{}, capture start +{}, \
                     listen +{}, first live {:?}: restart {} | truth {}",
                        start as i64 - START as i64,
                        end as i64 - START as i64,
                        crash_secs as i64 - START as i64,
                        capture_start as i64 - START as i64,
                        listen_at as i64 - START as i64,
                        first_live.get(&i),
                        describe(r),
                        describe(t)
                    );
                    // 1. Never more volume than the truth, at any emission.
                    if let Some(&max) = restart.max_volume.get(&key) {
                        prop_assert!(
                            max <= t.map_or(0, |t| t.volume),
                            "over-count ({max}): {context}"
                        );
                    }
                    // Beyond the envelope: a trade of this bucket received after the
                    // crash and after the catch-up margin past the bucket's end, so
                    // the catch-up may already have sealed it. Even the uninterrupted
                    // process takes such a trade as a late amendment, and a restart
                    // may have lost it in the downtime or refuse to amend a bar that
                    // began before the capture start; only 1 is checked for such a
                    // bucket (production's measured maximum lag is 199 s).
                    let beyond_margin = all.iter().any(|p| {
                        p.instrument == i
                            && p.kind == 0
                            && p.trade >= start
                            && p.trade < end
                            && p.received_ms > crash_ms
                            && p.received_ms >= (u64::from(end) + u64::from(MARGIN)) * 1000
                    });
                    if beyond_margin {
                        continue;
                    }
                    let Some(r) = r else {
                        // 4. Missing after the crash: only a bucket that started
                        // before the instrument was known to be captured.
                        if t.is_some() && u64::from(end) * 1000 > crash_ms {
                            // The bucket that holds the first volume trade, or
                            // that its carried volume settles into: ending after
                            // it, and starting by the first later packet whose own
                            // bucket of this timeframe the catch-up cannot yet
                            // have sealed when it arrived (a bucket open then, or
                            // opened by it, takes the carry).
                            let secs = tf.seconds_per_bucket();
                            let settles_by = after_add.get(&i).and_then(|later| {
                                later.iter().find_map(|&(trade, received)| {
                                    let open_until =
                                        u64::from(tf.bucket_start(trade) + secs + MARGIN);
                                    (open_until * 1000 > received).then_some(trade)
                                })
                            });
                            // The range runs from the first tainted trade's bucket
                            // to the last one's, and past it only when that trade
                            // was late (its own bucket sealable when it arrived).
                            let holds_first_add = tainted.get(&i).is_some_and(|adds| {
                                let (Some(&(first, _)), Some(&(last, last_rx))) =
                                    (adds.first(), adds.last())
                                else {
                                    return false;
                                };
                                let last_late = u64::from(tf.bucket_start(last) + secs + MARGIN)
                                    * 1000
                                    <= last_rx;
                                let upto = if last_late { settles_by } else { Some(last) };
                                first < end && upto.is_none_or(|next| start <= next)
                            });
                            prop_assert!(
                                holds_first_add || captured_from.is_none_or(|from| start <= from),
                                "withheld without cause: {context}"
                            );
                        }
                        continue;
                    };
                    if let Some(s) = stored.last.get(&key) {
                        // 2. Stored by the previous process: identical to the truth,
                        // or to the row already stored (rewriting a row with its own
                        // content changes nothing: a late trade lost in the downtime
                        // is missing from both), or absent.
                        prop_assert!(
                            t.is_some_and(|t| same(r, t, true)) || same(r, s, true),
                            "a stored bucket was rewritten differently: {context}"
                        );
                    } else {
                        // 3. Stored nowhere else: exact, and a classified net
                        // volume is the truth's (an unclassified one says the
                        // direction is unknown, which is honest, not wrong).
                        prop_assert!(
                            t.is_some_and(|t| same(r, t, false)
                                && (!r.net_volume_classified || same(r, t, true))),
                            "a wrong bar was written: {context}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    static EFFECTIVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static CRASH_BETWEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Whether the proofs changed the truth (some first trade landed in its bar),
    /// and whether the crash fell between a proof and its first trade.
    fn stats(c: &Case) {
        use std::sync::atomic::Ordering::Relaxed;
        let all = packets(c);
        let mut with = Written::default();
        let _ = fold_live(all.iter(), &mut with);
        let stripped: Vec<Packet> = all.iter().filter(|p| p.kind == 0).copied().collect();
        let mut without = Written::default();
        let _ = fold_live(stripped.iter(), &mut without);
        TOTAL.fetch_add(1, Relaxed);
        if with.max_volume != without.max_volume {
            EFFECTIVE.fetch_add(1, Relaxed);
        }
        let (Some(first), Some(last)) = (all.first(), all.last()) else {
            return;
        };
        let crash_ms = first.received_ms
            + (last.received_ms - first.received_ms) * u64::from(c.crash_per_mille) / 1000;
        if (0..c.instruments.len()).any(|i| {
            all.iter()
                .any(|p| p.instrument == i && p.kind != 0 && p.received_ms <= crash_ms)
                && all
                    .iter()
                    .any(|p| p.instrument == i && p.kind == 0 && p.received_ms > crash_ms)
                && !all
                    .iter()
                    .any(|p| p.instrument == i && p.kind == 0 && p.received_ms <= crash_ms)
        }) {
            CRASH_BETWEEN.fetch_add(1, Relaxed);
        }
    }

    #[test]
    fn pr58_differential_coverage_stats() {
        use proptest::test_runner::{Config, TestRunner};
        use std::sync::atomic::Ordering::Relaxed;
        let mut runner = TestRunner::new(Config {
            cases: cases(),
            failure_persistence: None,
            ..Config::default()
        });
        runner
            .run(&case(), |c| {
                stats(&c);
                Ok(())
            })
            .unwrap();
        eprintln!(
            "stats: total {} proofs-changed-truth {} crash-between-proof-and-first-trade {}",
            TOTAL.load(Relaxed),
            EFFECTIVE.load(Relaxed),
            CRASH_BETWEEN.load(Relaxed)
        );
    }

    /// Pre-market restart: every instrument with a proof kind also sends an
    /// 08:40 connect snapshot; the previous process crashed at 08:45 (its WAL
    /// holds only those snapshots); the restart listens from 08:50 and folds
    /// every later packet live. Returns (over, wrong, withheld) bar counts
    /// against the truth, with the WAL replayed or not.
    fn premarket(c: &Case, replay_wal: bool) -> (usize, usize, usize) {
        let crash_ms = u64::from(DAY + 8 * 3600 + 45 * 60) * 1000;
        let capture = DAY + 8 * 3600 + 50 * 60;
        let mut all = packets(c);
        for (i, spec) in c.proofs.iter().enumerate().take(c.instruments.len()) {
            all.push(Packet {
                instrument: i,
                trade: 0,
                cumulative: 0,
                price: 0.0,
                high: 0.0,
                low: 0.0,
                open: 0.0,
                received_ms: (u64::from(capture) + 1) * 1000,
                kind: if spec.kind == 0 { 4 } else { spec.kind },
                ltq: 0,
            });
            if spec.kind != 0 {
                all.push(Packet {
                    instrument: i,
                    trade: 0,
                    cumulative: 0,
                    price: 0.0,
                    high: 0.0,
                    low: 0.0,
                    open: 0.0,
                    received_ms: u64::from(DAY + 8 * 3600 + 40 * 60) * 1000,
                    kind: spec.kind,
                    ltq: 0,
                });
            }
        }
        all.sort_by_key(|p| (p.received_ms, p.instrument, p.cumulative));
        let end_cutoff = all.iter().map(|p| p.trade).max().unwrap_or(START) + 10_000;
        let mut truth = Written::default();
        let mut t_agg = fold_live(all.iter(), &mut truth);
        finish(&mut t_agg, c.day_close, end_cutoff, &mut truth);
        let mut out = Written::default();
        let mut agg = aggregator();
        agg.set_replay_mode(true);
        agg.mark_replay_gap();
        if replay_wal {
            for p in all.iter().filter(|p| p.received_ms <= crash_ms) {
                agg.consume_tick(
                    Feed::Dhan,
                    &tick(p, p.received_ms),
                    None,
                    |_, sid, _, tf, bar| out.put(sid, tf, bar),
                );
            }
        }
        agg.set_live_capture_start(capture);
        agg.finish_replay(false, |_, sid, _, tf, bar| out.put(sid, tf, bar));
        for p in all
            .iter()
            .filter(|p| p.received_ms > crash_ms && p.received_ms >= u64::from(capture) * 1000)
        {
            agg.consume_tick(
                Feed::Dhan,
                &tick(p, p.received_ms),
                None,
                |_, sid, _, tf, bar| out.put(sid, tf, bar),
            );
            sweep(&mut agg, (p.received_ms / 1000) as u32, &mut out);
        }
        finish(&mut agg, c.day_close, end_cutoff, &mut out);
        let over = out
            .max_volume
            .iter()
            .filter(|(k, m)| **m > truth.last.get(*k).map_or(0, |t| t.volume))
            .count();
        let wrong = out
            .last
            .iter()
            .filter(|(k, r)| !truth.last.get(*k).is_some_and(|t| same(r, t, false)))
            .count();
        let withheld = truth
            .last
            .keys()
            .filter(|k| !out.last.contains_key(*k))
            .count();
        (over, wrong, withheld)
    }

    #[test]
    fn pr58_premarket_restart_differential() {
        use proptest::test_runner::{Config, TestRunner};
        let mut runner = TestRunner::new(Config {
            cases: cases(),
            failure_persistence: None,
            ..Config::default()
        });
        use std::cell::Cell;
        let (n, bars_no_wal, bars_wal, cases_hit, bad) = (
            Cell::new(0u64),
            Cell::new(0usize),
            Cell::new(0usize),
            Cell::new(0u64),
            Cell::new(0usize),
        );
        runner
            .run(&case(), |c| {
                let (o1, w1, h1) = premarket(&c, false);
                let (o2, w2, h2) = premarket(&c, true);
                n.set(n.get() + 1);
                bars_no_wal.set(bars_no_wal.get() + h1);
                bars_wal.set(bars_wal.get() + h2);
                bad.set(bad.get() + o1 + w1 + o2 + w2);
                if h2 > h1 {
                    cases_hit.set(cases_hit.get() + 1);
                }
                Ok(())
            })
            .unwrap();
        let (n, bars_no_wal, bars_wal, cases_hit, bad) = (
            n.get(),
            bars_no_wal.get(),
            bars_wal.get(),
            cases_hit.get(),
            bad.get(),
        );
        eprintln!(
            "premarket: cases {n}, over+wrong {bad}, withheld bars without WAL replay {bars_no_wal}, with WAL replay {bars_wal}, cases where replay withheld more {cases_hit}"
        );
        assert_eq!(bad, 0);
    }

    fn cases() -> u32 {
        std::env::var("FIRST_TRADE_DIFFERENTIAL_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(512)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: cases(), failure_persistence: None, ..ProptestConfig::default() })]

        #[test]
        fn pr58_restart_with_untraded_proofs_never_writes_a_wrong_bar(c in case()) {
            check(&c)?;
        }
    }
}

#[test]
fn s4_fresh_boot_times_before_after() {
    for seg in [FNO, EQ] {
        for boot in [hms(8, 50, 0), hms(8, 58, 0), hms(9, 14, 0), OPEN + 1] {
            for with_proof in [false, true] {
                let mut all = vec![mk(with_proof, seg, 0, boot + 5)];
                all.extend(day_trades(seg).into_iter().filter(|x| recv_secs(x) >= boot));
                let t = truth(&all);
                let mut out = Written::default();
                let mut a = agg();
                a.set_live_capture_start(boot);
                feed_live(&mut a, &all, &mut out);
                a.force_seal_all(|_, sid, _, tf, bar| out.put(sid, tf, bar));
                let (o, w, h) = diff(&t, &out);
                eprintln!(
                    "S4 seg {seg} boot {:+} proof {with_proof}: M1@open truth {:?} boot {:?} over {} wrong {} withheld {h}",
                    boot as i64 - OPEN as i64,
                    t.vol(TfIndex::M1, OPEN),
                    out.vol(TfIndex::M1, OPEN),
                    o.len(),
                    w.len()
                );
                assert!(o.is_empty() && w.is_empty());
            }
        }
    }
}
