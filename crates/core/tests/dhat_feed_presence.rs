//! Scoreboard PR-D — DHAT zero-alloc test for the per-instrument presence
//! fold (`feed_presence::record_presence`).
//!
//! The hot-path contract: after the ONE-TIME registry init (the two fixed
//! 192-KiB-total word planes + the papaya slot maps, allocated OUTSIDE the
//! profiler window below), every `record_presence` call is one relaxed
//! enabled check + pure minute math + one lock-free papaya read + one
//! relaxed `fetch_or` — ZERO heap allocation. A regression that adds
//! `.clone()`, `format!()`, or a locking map to the fold would blow the
//! budget immediately across 10K calls.
//!
//! HONEST COVERAGE: the profiled loop feeds registered in-session keys —
//! the steady-state admitted arm. The unregistered arm is a relaxed
//! counter bump (no metrics macro, no allocation) but is not inside the
//! profiler window; the out-of-session arm returns before any lookup.
//!
//! Budget: ≤ 1 KiB / ≤ 8 blocks across 10,000 calls — same class as
//! `dhat_moneyness.rs` (the former `dhat_feed_lag_ring.rs` peer was
//! deleted 2026-07-17 with the dead Dhan-lag ring).
//!
//! CI ENFORCEMENT: deliberately UN-gated (no `#![cfg(feature = "dhat")]`)
//! — the house pattern — so the normal Test (core) nextest lane runs it on
//! every PR; the Coverage lane skips it BY TEST NAME (the `dhat_` prefix).
//!
//! Run: `cargo test -p tickvault-core --test dhat_feed_presence`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::feed::Feed;
use tickvault_core::pipeline::feed_presence::{
    self, PairingKey, PresenceRegistration, init_feed_presence, record_presence,
};

mod dhat_support;

const DAY: u64 = 20_644;
const SESSION_OPEN_IST_SECS: u32 = 9 * 3600 + 15 * 60;

#[test]
fn dhat_record_presence_hot_path_zero_allocation() {
    // One-time global init + registration OUTSIDE the profiler window
    // (the word planes + papaya tables are the documented one-time cost).
    init_feed_presence(true);
    let regs = vec![
        PresenceRegistration {
            security_id: 13,
            segment_code: 0,
            segment_label: "IDX_I",
            symbol: "NIFTY".to_string(),
            pairing: Some(PairingKey::Index("NIFTY".to_string())),
        },
        PresenceRegistration {
            security_id: 2885,
            segment_code: 1,
            segment_label: "NSE_EQ",
            symbol: "RELIANCE".to_string(),
            pairing: Some(PairingKey::Isin("INE002A01018".to_string())),
        },
    ];
    feed_presence::register_instruments(Feed::Dhan, DAY, &regs);
    let base_secs = u32::try_from(DAY * 86_400).unwrap_or(0) + SESSION_OPEN_IST_SECS;
    // Warm the fold once (first papaya pin on this thread may lazily
    // initialize thread-local reclamation state — one-time, not per-call).
    record_presence(Feed::Dhan, 13, 0, base_secs);

    let _profiler = dhat::Profiler::builder().testing().build();

    const BUDGET_BYTES: u64 = 1024;
    const BUDGET_BLOCKS: u64 = 8;

    let (new_bytes, new_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u32 {
                // Walk the whole session minute range across both slots.
                let ts = base_secs + (i % (375 * 60));
                let (sid, seg) = if i % 2 == 0 {
                    (13u64, 0u8)
                } else {
                    (2885u64, 1u8)
                };
                record_presence(Feed::Dhan, sid, seg, ts);
                std::hint::black_box(i);
            }
        },
    );

    assert!(
        new_bytes <= BUDGET_BYTES && new_blocks <= BUDGET_BLOCKS,
        "presence fold allocated on the hot path: {new_bytes} bytes / \
         {new_blocks} blocks across 10,000 calls (budget {BUDGET_BYTES} B / \
         {BUDGET_BLOCKS} blocks) — an allocation or a lock entered \
         record_presence"
    );
}
