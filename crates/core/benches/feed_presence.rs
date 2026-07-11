//! Scoreboard PR-D — Criterion benchmark for the per-instrument presence
//! fold (`feed_presence::record_presence`).
//!
//! One HOT-PATH target: `feed_presence/record` — one relaxed enabled
//! check + pure minute math + one lock-free papaya read + one relaxed
//! `fetch_or`. Budget: `feed_presence_record` in
//! `quality/benchmark-budgets.toml` (≤ 100 ns — the feed_lag hot-target
//! class; the papaya hash + read dominates).
//!
//! Run: `cargo bench --bench feed_presence -p tickvault-core`

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::feed::Feed;
use tickvault_core::pipeline::feed_presence::{
    self, PairingKey, PresenceRegistration, init_feed_presence, record_presence,
};

const DAY: u64 = 20_644;
const SESSION_OPEN_IST_SECS: u32 = 9 * 3600 + 15 * 60;

/// Steady-state hot-path fold on the process-global registry (the exact
/// production entry point). HONEST COVERAGE: measures the registered,
/// in-session admitted arm — the steady state at every persist site.
fn bench_record_presence(c: &mut Criterion) {
    init_feed_presence(true);
    feed_presence::register_instruments(
        Feed::Dhan,
        DAY,
        &[PresenceRegistration {
            security_id: 2885,
            segment_code: 1,
            segment_label: "NSE_EQ",
            symbol: "RELIANCE".to_string(),
            pairing: Some(PairingKey::Isin("INE002A01018".to_string())),
        }],
    );
    let base_secs = u32::try_from(DAY * 86_400).unwrap_or(0) + SESSION_OPEN_IST_SECS;
    record_presence(Feed::Dhan, 2885, 1, base_secs);

    c.bench_function("feed_presence/record", |b| {
        let mut i: u32 = 0;
        b.iter(|| {
            i = i.wrapping_add(37);
            let ts = base_secs + (i % (375 * 60));
            record_presence(
                black_box(Feed::Dhan),
                black_box(2885),
                black_box(1),
                black_box(ts),
            );
        });
    });
}

criterion_group!(benches, bench_record_presence);
criterion_main!(benches);
