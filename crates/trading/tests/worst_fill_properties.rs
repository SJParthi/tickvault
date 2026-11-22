//! Property tests for the brutex host's order-side helpers:
//!
//! * `worst_fill` against a share-by-share brute-force reference, plus
//!   properties taken from the spec rather than from the model's formulas:
//!   a slice never beats the decision bar, the book's best or its own second
//!   (by one tick), never takes more than the participation share of a
//!   second, filled + remainder == qty, and a bar more than one second after
//!   a slice cannot change that slice. Participation (1..=10,000 bps) and the
//!   first fill second vary;
//! * `marketable_limit`: on the tick grid, never beyond band / slot cap /
//!   circuit, always marketable, and refused only when no grid price fits;
//! * `SendQueue` against a sorted `Vec` reference.
//!
//! The three live in one file so the crate builds one more test binary, not
//! three.

use proptest::prelude::*;
use tickvault_trading::brutex_host::engine::Side;
use tickvault_trading::brutex_host::order_price::{LimitBound, PriceRefusal, marketable_limit};
use tickvault_trading::brutex_host::send_queue::{QueueFull, SendQueue};
use tickvault_trading::brutex_host::worst_fill::{
    BookAtDecision, FillParams, FillSchedule, SecondBar, worst_fill,
};

const DECISION: i64 = 50_000;

fn worse(side: Side, a: i64, b: i64) -> i64 {
    match side {
        Side::Buy => a.max(b),
        Side::Sell => a.min(b),
    }
}

fn ext(side: Side, b: &SecondBar) -> i64 {
    match side {
        Side::Buy => b.high_paise,
        Side::Sell => b.low_paise,
    }
}

fn against(side: Side, p: i64, amount: i64) -> i64 {
    match side {
        Side::Buy => p + amount,
        Side::Sell => p - amount,
    }
}

/// Share-by-share reference: the model written as plainly as possible.
/// Returns one `(sec, price)` per filled share.
fn reference(
    p: &FillParams,
    db: &SecondBar,
    secs: &[SecondBar],
    book: Option<&BookAtDecision<'_>>,
) -> Vec<(i64, i64)> {
    let side = p.side;
    let mut base = ext(side, db);
    if let Some(b) = book {
        base = worse(side, base, b.best_paise);
    }
    let horizon = p.first_fill_sec + i64::from(p.max_seconds);
    let mut remaining = p.qty;
    let mut shares = Vec::new();
    for i in 0..secs.len() {
        if remaining == 0 || secs[i].sec >= horizon {
            break;
        }
        let mut price = worse(side, base, ext(side, &secs[i]));
        // Only the literal next clock second is a candidate (review finding
        // 6, 2026-11-12): a gap means nothing traded in that second.
        if let Some(n) = secs.get(i + 1)
            && n.sec == secs[i].sec + 1
        {
            price = worse(side, price, ext(side, n));
        }
        let cap = secs[i].volume * u64::from(p.participation_bps) / 10_000;
        let n = cap.min(remaining);
        for _ in 0..n {
            let idx = shares.len() as u64;
            let at = match book {
                None => price,
                Some(b) => {
                    let mut cum = b.best_qty;
                    if idx < cum {
                        price
                    } else {
                        let mut found = None;
                        for &(lp, lq) in b.levels {
                            cum += lq;
                            if idx < cum {
                                found = Some(worse(side, price, lp));
                                break;
                            }
                        }
                        found.unwrap_or_else(|| {
                            let deepest = b.levels.last().map_or(b.best_paise, |l| l.0);
                            let k = (b.levels.len() as i64 + 1) * p.tick_paise;
                            against(side, worse(side, price, deepest), k)
                        })
                    }
                }
            };
            shares.push((secs[i].sec, against(side, at, p.tick_paise)));
        }
        remaining -= n;
    }
    shares
}

#[derive(Debug, Clone)]
struct Case {
    side: Side,
    qty: u64,
    tick: i64,
    max_seconds: u32,
    participation_bps: u32,
    /// Seconds after the decision the first fill may come from (>= 1).
    first_fill_after: i64,
    db: SecondBar,
    secs: Vec<SecondBar>,
    book: Option<(i64, u64, Vec<(i64, u64)>)>,
}

fn bar_strategy() -> impl Strategy<Value = (i64, i64, u64)> {
    (
        1_000i64..2_000,
        0i64..50,
        prop_oneof![Just(0u64), 0u64..400, 0u64..200_000],
    )
        .prop_map(|(low, spread, vol)| (low + spread, low, vol))
}

fn case_strategy() -> impl Strategy<Value = Case> {
    (
        any::<bool>(),
        1u64..300,
        1i64..10,
        1u32..10,
        bar_strategy(),
        prop::collection::vec((bar_strategy(), 1i64..3), 1..8),
        prop::option::of((
            1_000i64..2_000,
            1u64..80,
            prop::collection::vec((1i64..30, 1u64..60), 0..4),
        )),
        prop_oneof![Just(2_500u32), Just(10_000u32), Just(1u32), 1u32..=10_000],
        1i64..4,
    )
        .prop_map(
            |(buy, qty, tick, max_seconds, db, raw, book, participation_bps, first_fill_after)| {
                let side = if buy { Side::Buy } else { Side::Sell };
                // Bars start at the first fill second or later.
                let mut sec = DECISION + first_fill_after - 1;
                let secs = raw
                    .into_iter()
                    .map(|((h, l, v), gap)| {
                        sec += gap;
                        SecondBar {
                            sec,
                            high_paise: h,
                            low_paise: l,
                            volume: v,
                        }
                    })
                    .collect();
                let book = book.map(|(best, best_qty, steps)| {
                    let mut price = best;
                    let levels = steps
                        .into_iter()
                        .map(|(step, q)| {
                            price = match side {
                                Side::Buy => price + step,
                                Side::Sell => price - step,
                            };
                            (price, q)
                        })
                        .collect();
                    (best, best_qty, levels)
                });
                Case {
                    side,
                    qty,
                    tick,
                    max_seconds,
                    participation_bps,
                    first_fill_after,
                    db: SecondBar {
                        sec: DECISION,
                        high_paise: db.0,
                        low_paise: db.1,
                        volume: db.2,
                    },
                    secs,
                    book,
                }
            },
        )
}

fn params_of(c: &Case) -> FillParams {
    let mut p = FillParams::new(c.side, c.qty, c.tick, DECISION, c.max_seconds);
    p.participation_bps = c.participation_bps;
    p.first_fill_sec = DECISION + c.first_fill_after;
    p
}

fn book_of(c: &Case) -> Option<BookAtDecision<'_>> {
    c.book.as_ref().map(|(b, q, l)| BookAtDecision {
        best_paise: *b,
        best_qty: *q,
        levels: l,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn worst_fill_matches_brute_force_and_invariants(c in case_strategy()) {
        let p = params_of(&c);
        let book = book_of(&c);
        let levels = c.book.as_ref().map_or(0, |b| b.2.len());
        let mut out = FillSchedule::with_capacity(FillSchedule::required_capacity(c.max_seconds, levels));
        let r = worst_fill(&p, &c.db, &c.secs, book.as_ref(), &mut out);
        prop_assert!(r.is_ok(), "refused {:?}", r);
        let s = r.unwrap_or_else(|_| unreachable!());

        // Brute force, share by share.
        let expect = reference(&p, &c.db, &c.secs, book.as_ref());
        let got: Vec<(i64, i64)> = out
            .slices()
            .iter()
            .flat_map(|x| std::iter::repeat_n((x.sec, x.price_paise), x.qty as usize))
            .collect();
        prop_assert_eq!(&got, &expect);

        // filled + remainder == qty.
        prop_assert_eq!(s.filled_qty + s.unfilled_qty, c.qty);
        prop_assert_eq!(s.filled_qty, expect.len() as u64);

        // Spec bounds, written without the model's formulas: a buy never
        // fills below the decision bar's high + one tick, the book's best ask
        // + one tick, or its own second's high + one tick (mirror for a sell).
        for x in out.slices() {
            prop_assert!(x.sec >= p.first_fill_sec);
            prop_assert!(x.sec < p.first_fill_sec + i64::from(c.max_seconds));
            let own = c.secs.iter().find(|b| b.sec == x.sec);
            prop_assert!(own.is_some(), "slice in a second with no bar");
            let mut floors = vec![ext(c.side, &c.db)];
            if let Some(b) = &book {
                floors.push(b.best_paise);
            }
            if let Some(o) = own {
                floors.push(ext(c.side, o));
            }
            for f in floors {
                match c.side {
                    Side::Buy => prop_assert!(x.price_paise >= f + c.tick),
                    Side::Sell => prop_assert!(x.price_paise <= f - c.tick),
                }
            }
        }

        for (i, sb) in c.secs.iter().enumerate() {
            let in_sec: u64 = out.slices().iter().filter(|x| x.sec == sb.sec).map(|x| x.qty).sum();
            // Never more than the participation share of any second.
            prop_assert!(
                u128::from(in_sec) * 10_000 <= u128::from(sb.volume) * u128::from(c.participation_bps)
            );
            // The literal next second, when it traded, is a floor too.
            if let Some(n) = c.secs.get(i + 1).filter(|n| n.sec == sb.sec + 1) {
                for x in out.slices().iter().filter(|x| x.sec == sb.sec) {
                    match c.side {
                        Side::Buy => prop_assert!(x.price_paise >= n.high_paise + c.tick),
                        Side::Sell => prop_assert!(x.price_paise <= n.low_paise - c.tick),
                    }
                }
            }
        }

        // VWAP rounded against us.
        if let Some(v) = s.vwap_paise {
            let cost: i128 = expect.iter().map(|&(_, pr)| i128::from(pr)).sum();
            let f = expect.len() as i128;
            match c.side {
                Side::Buy => prop_assert!(i128::from(v) * f >= cost && i128::from(v - 1) * f < cost),
                Side::Sell => prop_assert!(i128::from(v) * f <= cost && i128::from(v + 1) * f > cost),
            }
        } else {
            prop_assert_eq!(s.filled_qty, 0);
        }
    }

    /// Spec property (a): a slice's price depends only on bars up to one
    /// second after it. Rewriting any bar more than 1 s later (its high, low
    /// and volume) leaves every earlier slice exactly as it was.
    #[test]
    fn worst_fill_slice_ignores_bars_more_than_one_second_later(
        c in case_strategy(),
        pick in any::<prop::sample::Index>(),
        new_bar in bar_strategy(),
    ) {
        let p = params_of(&c);
        let book = book_of(&c);
        let levels = c.book.as_ref().map_or(0, |b| b.2.len());
        let cap = FillSchedule::required_capacity(c.max_seconds, levels);
        let j = pick.index(c.secs.len());
        let mut changed = c.secs.clone();
        changed[j].high_paise = new_bar.0;
        changed[j].low_paise = new_bar.1;
        changed[j].volume = new_bar.2;
        let mut a = FillSchedule::with_capacity(cap);
        let mut b = FillSchedule::with_capacity(cap);
        prop_assert!(worst_fill(&p, &c.db, &c.secs, book.as_ref(), &mut a).is_ok());
        prop_assert!(worst_fill(&p, &c.db, &changed, book.as_ref(), &mut b).is_ok());
        let cut = c.secs[j].sec - 1;
        let before = |s: &FillSchedule| -> Vec<(i64, u64, i64)> {
            s.slices()
                .iter()
                .filter(|x| x.sec < cut)
                .map(|x| (x.sec, x.qty, x.price_paise))
                .collect()
        };
        prop_assert_eq!(before(&a), before(&b));
    }

    #[test]
    fn marketable_limit_is_on_grid_bounded_and_marketable(
        buy in any::<bool>(),
        ltp in 1i64..10_000_000,
        band in 1u32..10_000,
        tick in 1i64..100,
        qty in 1u64..10_000,
        slot in prop::option::of(1i64..1_000_000_000_000),
        circuit in prop::option::of((0i64..1_000_000, 0i64..1_000_000)),
    ) {
        let side = if buy { Side::Buy } else { Side::Sell };
        let circuit = circuit.map(|(down, up)| ((ltp - down).max(1), ltp + up));
        let band_hi = i128::from(ltp) * (10_000 + i128::from(band)) / 10_000;
        let band_lo = (i128::from(ltp) * (10_000 - i128::from(band)) + 9_999) / 10_000;
        let slot_cap = slot.map(|s| i128::from(s) * 5 / i128::from(qty));
        match marketable_limit(side, ltp, band, tick, qty, slot, circuit) {
            Ok(l) => {
                let p = i128::from(l.price_paise);
                prop_assert_eq!(l.price_paise % tick, 0);
                match side {
                    Side::Buy => {
                        prop_assert!(l.price_paise >= ltp);
                        prop_assert!(p <= band_hi);
                        if let Some(cap) = slot_cap { prop_assert!(p <= cap); }
                        if let Some((_, up)) = circuit { prop_assert!(l.price_paise <= up); }
                    }
                    Side::Sell => {
                        prop_assert!(l.price_paise <= ltp);
                        prop_assert!(p >= band_lo);
                        prop_assert_ne!(l.bound, LimitBound::SlotCap);
                        if let Some((lo, _)) = circuit { prop_assert!(l.price_paise >= lo); }
                    }
                }
            }
            Err(PriceRefusal::NotMarketable { .. }) => {
                // Refused only when no grid price lies between LTP and the bound.
                let t = i128::from(tick);
                let l = i128::from(ltp);
                match side {
                    Side::Buy => {
                        let mut hi = band_hi;
                        if let Some(cap) = slot_cap { hi = hi.min(cap); }
                        if let Some((_, up)) = circuit { hi = hi.min(i128::from(up)); }
                        prop_assert!(hi - hi.rem_euclid(t) < l);
                    }
                    Side::Sell => {
                        let mut lo = band_lo;
                        if let Some((low, _)) = circuit { lo = lo.max(i128::from(low)); }
                        let r = lo.rem_euclid(t);
                        let up = if r == 0 { lo } else { lo + t - r };
                        prop_assert!(up > l);
                    }
                }
            }
            Err(e) => prop_assert!(false, "valid inputs refused: {:?}", e),
        }
    }

    #[test]
    fn send_queue_matches_sorted_vec(
        ops in prop::collection::vec(prop::option::of((0u32..4, 0i64..1_000)), 0..200),
    ) {
        let mut q: SendQueue<u32, 8> = SendQueue::new();
        let mut reference: Vec<(u32, u64, u32, i64)> = Vec::new();
        let mut seq = 0u64;
        let mut item = 0u32;
        let mut refused = 0u64;
        let mut peak = 0usize;
        for op in ops {
            match op {
                Some((rank, at)) => {
                    item += 1;
                    let r = q.push(item, rank, at);
                    if reference.len() >= 8 {
                        prop_assert_eq!(r, Err(QueueFull));
                        refused += 1;
                    } else {
                        prop_assert_eq!(r, Ok(()));
                        reference.push((rank, seq, item, at));
                        seq += 1;
                        peak = peak.max(reference.len());
                    }
                }
                None => {
                    reference.sort_by_key(|e| (e.0, e.1));
                    let expect = if reference.is_empty() {
                        None
                    } else {
                        let e = reference.remove(0);
                        Some((e.2, u64::try_from((1_000 - e.3).max(0)).unwrap_or(0)))
                    };
                    prop_assert_eq!(q.pop(1_000), expect);
                }
            }
            prop_assert_eq!(q.len(), reference.len());
        }
        prop_assert_eq!(q.refused_full(), refused);
        prop_assert_eq!(q.peak_len(), peak);
    }
}
