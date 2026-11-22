//! UTC-SAFETY (daily-universe §28.7): DHAT zero-allocation proof for the
//! trade-safety switch. `SafetySwitch::engage` (one CAS loop, run on the
//! frame drain when the feed-time page fires) and `SafetySwitch::blocked_on`
//! (one Acquire load, read by every entry decision) must allocate nothing.
//!
//! Budget: 0 bytes / 0 blocks across 20,000 calls (strict: both are a few
//! atomics, there is nothing to warm up).
//!
//! CI ENFORCEMENT: un-gated (the house pattern); the Coverage lane skips it
//! by test name (the `dhat_` prefix).
//!
//! Run: `cargo test -p tickvault-app --test dhat_trade_safety`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_app::trade_safety::{CAUSE_A, CAUSE_B, SafetySwitch};

mod dhat_support;

const BUDGET_BYTES: u64 = 0;
const BUDGET_BLOCKS: u64 = 0;

#[test]
fn dhat_trade_safety_engage_and_check_zero_alloc() {
    let switch = SafetySwitch::new();
    let day = 20_777_i64;
    let _profiler = dhat::Profiler::builder().testing().build();
    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_i64 {
                // Alternate days and causes so both CAS arms run (OR into the
                // same day, replace another day).
                let d = day + (i % 3);
                let cause = if i % 2 == 0 { CAUSE_A } else { CAUSE_B };
                std::hint::black_box(switch.engage(d, cause));
                std::hint::black_box(switch.blocked_on(day + (i % 5)));
            }
        },
    );
    assert!(
        bytes == BUDGET_BYTES && blocks == BUDGET_BLOCKS,
        "the trade-safety switch allocated {bytes} bytes / {blocks} blocks over \
         20,000 calls (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks): engage and \
         blocked_on must stay a few atomics"
    );
    assert!(switch.blocked_on(day + 2).is_some() || switch.blocked_on(day).is_some());
}
