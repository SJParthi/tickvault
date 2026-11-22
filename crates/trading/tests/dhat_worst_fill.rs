//! DHAT allocation test for the brutex host's order-side helpers:
//! `worst_fill`, `marketable_limit` and `SendQueue` push/pop.
//!
//! Each is called 100,000 times inside one measured region; the budget is
//! zero blocks and zero bytes. The schedule and the queue are built before
//! the region (their one allocation is construction, by design). Non-vacuity:
//! every call's result is checked inside the region (a refused fill or an
//! empty pop would measure the cheap early-return path).
//!
//! ONE profiler per test binary (`dhat::Profiler` is process-global), so the
//! three helpers are measured in a single test.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_trading::brutex_host::engine::Side;
use tickvault_trading::brutex_host::order_price::{DEFAULT_BAND_BPS, marketable_limit};
use tickvault_trading::brutex_host::send_queue::SendQueue;
use tickvault_trading::brutex_host::worst_fill::{
    BookAtDecision, FillParams, FillSchedule, SecondBar, worst_fill,
};

mod dhat_support;

const DECISION: i64 = 33_305;
const CALLS: u32 = 100_000;

const SECONDS: [SecondBar; 5] = [
    SecondBar {
        sec: DECISION + 1,
        high_paise: 10_000,
        low_paise: 9_995,
        volume: 400,
    },
    SecondBar {
        sec: DECISION + 2,
        high_paise: 10_005,
        low_paise: 9_995,
        volume: 400,
    },
    SecondBar {
        sec: DECISION + 3,
        high_paise: 10_000,
        low_paise: 9_990,
        volume: 0,
    },
    SecondBar {
        sec: DECISION + 4,
        high_paise: 10_020,
        low_paise: 9_980,
        volume: 400,
    },
    SecondBar {
        sec: DECISION + 5,
        high_paise: 10_000,
        low_paise: 9_995,
        volume: 400,
    },
];
const LEVELS: [(i64, u64); 4] = [(10_020, 30), (10_040, 20), (10_060, 20), (10_080, 20)];

#[test]
fn dhat_worst_fill_order_price_send_queue_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let decision = SecondBar {
        sec: DECISION,
        high_paise: 10_010,
        low_paise: 9_990,
        volume: 1_000,
    };
    let book = BookAtDecision {
        best_paise: 10_000,
        best_qty: 50,
        levels: &LEVELS,
    };
    let mut out = FillSchedule::with_capacity(FillSchedule::required_capacity(5, LEVELS.len()));
    let mut queue: SendQueue<u64, 64> = SendQueue::new();
    let mut checksum: i64 = 0;

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for i in 0..CALLS {
                let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
                let qty = 50 + u64::from(i % 300);
                let params = FillParams::new(side, qty, 5, DECISION, 5);
                let book_ref = if side == Side::Buy { Some(&book) } else { None };
                let r = worst_fill(&params, &decision, &SECONDS, book_ref, &mut out);
                assert!(r.is_ok_and(|s| s.filled_qty > 0), "fill refused or empty");
                checksum = checksum.wrapping_add(out.slices().len() as i64);

                let l = marketable_limit(
                    side,
                    10_000 + i64::from(i % 97),
                    DEFAULT_BAND_BPS,
                    5,
                    qty,
                    Some(10_000_000),
                    Some((9_000, 12_000)),
                );
                assert!(l.is_ok(), "limit refused");
                checksum = checksum.wrapping_add(l.map_or(0, |x| x.price_paise));

                let pushed = queue.push(u64::from(i), i % 7, i64::from(i));
                assert!(pushed.is_ok(), "queue full");
                if queue.len() >= 48 {
                    assert!(queue.pop(i64::from(i)).is_some(), "empty pop");
                }
            }
            while queue.pop(0).is_some() {}
        },
    );
    eprintln!("dhat_worst_fill: {bytes} B / {blocks} blocks over {CALLS} calls of each helper");
    assert!(checksum != 0, "nothing was computed");
    assert_eq!(
        (bytes, blocks),
        (0, 0),
        "worst_fill / marketable_limit / SendQueue allocated {blocks} blocks ({bytes} B) over {CALLS} calls each"
    );
}
