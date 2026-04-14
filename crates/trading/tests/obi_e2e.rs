//! OBI (Order Book Imbalance) end-to-end integration tests.
//!
//! Tests the full OBI pipeline: depth levels → compute_obi → ObiSnapshot.
//! Verifies formula correctness, edge cases, wall detection, and
//! consistency across multiple sequential snapshots.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use tickvault_common::tick_types::DeepDepthLevel;
use tickvault_trading::indicator::obi::{ObiSnapshot, compute_obi};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Creates a realistic 20-level order book with decreasing qty from best.
fn make_realistic_book(base_price: f64, step: f64, base_qty: u32) -> Vec<DeepDepthLevel> {
    (0..20)
        .map(|i| DeepDepthLevel {
            price: base_price + step * i as f64,
            quantity: base_qty.saturating_sub(i * (base_qty / 25)),
            orders: (20 - i) as u32,
        })
        .collect()
}

/// Creates a book with a wall at a specific level.
fn make_book_with_wall(
    base_price: f64,
    step: f64,
    normal_qty: u32,
    wall_level: usize,
    wall_qty: u32,
) -> Vec<DeepDepthLevel> {
    (0..20)
        .map(|i| DeepDepthLevel {
            price: base_price + step * i as f64,
            quantity: if i == wall_level {
                wall_qty
            } else {
                normal_qty
            },
            orders: 10,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// E2E: Realistic order book scenarios
// ---------------------------------------------------------------------------

#[test]
fn e2e_obi_realistic_balanced_book() {
    let bids = make_realistic_book(24500.0, -0.5, 1000);
    let asks = make_realistic_book(24500.5, 0.5, 1000);
    let snap = compute_obi(49081, 2, &bids, &asks);

    assert!(snap.obi.is_finite());
    assert!(
        snap.obi.abs() < 0.1,
        "realistic balanced book: OBI should be near 0"
    );
    assert!(snap.spread > 0.0, "spread should be positive");
    assert!((snap.spread - 0.5).abs() < 1e-10, "spread should be 0.5");
    assert_eq!(snap.bid_levels, 20);
    assert_eq!(snap.ask_levels, 20);
}

#[test]
fn e2e_obi_aggressive_buying() {
    // Heavy bids, thin asks → positive OBI
    let bids = make_realistic_book(24500.0, -0.5, 5000);
    let asks = make_realistic_book(24500.5, 0.5, 500);
    let snap = compute_obi(49081, 2, &bids, &asks);

    assert!(snap.obi > 0.5, "heavy bids → positive OBI: {}", snap.obi);
    assert!(snap.weighted_obi > 0.5, "weighted should also be positive");
    assert!(snap.total_bid_qty > snap.total_ask_qty);
}

#[test]
fn e2e_obi_aggressive_selling() {
    // Thin bids, heavy asks → negative OBI
    let bids = make_realistic_book(24500.0, -0.5, 500);
    let asks = make_realistic_book(24500.5, 0.5, 5000);
    let snap = compute_obi(49081, 2, &bids, &asks);

    assert!(snap.obi < -0.5, "heavy asks → negative OBI: {}", snap.obi);
    assert!(snap.weighted_obi < -0.5, "weighted should also be negative");
    assert!(snap.total_ask_qty > snap.total_bid_qty);
}

// ---------------------------------------------------------------------------
// E2E: Wall detection scenarios
// ---------------------------------------------------------------------------

#[test]
fn e2e_obi_bid_wall_detected() {
    // Normal bids with one massive wall at level 5
    let bids = make_book_with_wall(24500.0, -0.5, 100, 5, 50000);
    let asks = make_realistic_book(24500.5, 0.5, 1000);
    let snap = compute_obi(49081, 2, &bids, &asks);

    assert!(snap.max_bid_wall_qty > 0, "bid wall should be detected");
    assert_eq!(snap.max_bid_wall_qty, 50000);
    assert!((snap.max_bid_wall_price - (24500.0 - 2.5)).abs() < 1e-10);
}

#[test]
fn e2e_obi_ask_wall_detected() {
    let bids = make_realistic_book(24500.0, -0.5, 1000);
    let asks = make_book_with_wall(24500.5, 0.5, 100, 3, 40000);
    let snap = compute_obi(49081, 2, &bids, &asks);

    assert!(snap.max_ask_wall_qty > 0, "ask wall should be detected");
    assert_eq!(snap.max_ask_wall_qty, 40000);
}

#[test]
fn e2e_obi_no_wall_uniform_book() {
    // All levels equal → no wall (need >10x average)
    let bids: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 100.0 - i as f64 * 0.1,
            quantity: 1000,
            orders: 10,
        })
        .collect();
    let asks: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 100.1 + i as f64 * 0.1,
            quantity: 1000,
            orders: 10,
        })
        .collect();
    let snap = compute_obi(1, 1, &bids, &asks);

    assert_eq!(snap.max_bid_wall_qty, 0, "no wall in uniform book");
    assert_eq!(snap.max_ask_wall_qty, 0, "no wall in uniform book");
}

// ---------------------------------------------------------------------------
// E2E: Sequential snapshots (simulate real-time flow)
// ---------------------------------------------------------------------------

#[test]
fn e2e_obi_sequential_snapshots_trend() {
    // Simulate: buying pressure increasing over 5 snapshots
    let mut obi_values: Vec<f64> = Vec::with_capacity(5);

    for multiplier in 1..=5 {
        let bid_qty = 1000 * multiplier as u32;
        let ask_qty = 1000;
        let bids = vec![
            DeepDepthLevel {
                price: 100.0,
                quantity: bid_qty,
                orders: 10,
            };
            20
        ];
        let asks = vec![
            DeepDepthLevel {
                price: 100.5,
                quantity: ask_qty,
                orders: 10,
            };
            20
        ];
        let snap = compute_obi(1, 2, &bids, &asks);
        obi_values.push(snap.obi);
    }

    // OBI should be monotonically increasing
    for i in 1..obi_values.len() {
        assert!(
            obi_values[i] > obi_values[i - 1],
            "OBI should increase: snap[{}]={} vs snap[{}]={}",
            i - 1,
            obi_values[i - 1],
            i,
            obi_values[i]
        );
    }
}

#[test]
fn e2e_obi_snapshot_symmetry() {
    // Swapping bid and ask should negate OBI
    let levels_a: Vec<DeepDepthLevel> = (0..10)
        .map(|i| DeepDepthLevel {
            price: 100.0 + i as f64,
            quantity: 1000 + i * 100,
            orders: 5,
        })
        .collect();
    let levels_b: Vec<DeepDepthLevel> = (0..10)
        .map(|i| DeepDepthLevel {
            price: 110.0 + i as f64,
            quantity: 500 + i * 50,
            orders: 3,
        })
        .collect();

    let snap_normal = compute_obi(1, 2, &levels_a, &levels_b);
    let snap_flipped = compute_obi(1, 2, &levels_b, &levels_a);

    assert!(
        (snap_normal.obi + snap_flipped.obi).abs() < f64::EPSILON,
        "swapped sides should negate OBI: {} + {} != 0",
        snap_normal.obi,
        snap_flipped.obi
    );
}

// ---------------------------------------------------------------------------
// E2E: Weighted OBI vs simple OBI divergence
// ---------------------------------------------------------------------------

#[test]
fn e2e_obi_weighted_diverges_from_simple() {
    // Back-heavy bids: small qty at best, huge qty at worst levels.
    // Simple OBI sees total bid > ask. Weighted OBI sees less bid near top.
    let bids: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 100.0 - i as f64 * 0.5,
            quantity: 10 + i as u32 * 500, // 10 at best, 9510 at worst
            orders: 5,
        })
        .collect();
    // Asks: uniform 500 each
    let asks: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 100.5 + i as f64 * 0.5,
            quantity: 500,
            orders: 5,
        })
        .collect();

    let snap = compute_obi(1, 2, &bids, &asks);

    // Simple OBI should be strongly positive (total bids >> total asks)
    assert!(
        snap.obi > 0.5,
        "simple OBI should be positive: {}",
        snap.obi
    );
    // Weighted OBI should be less positive (best bid level has tiny qty)
    assert!(
        snap.weighted_obi < snap.obi,
        "weighted should be less than simple when bids are back-heavy: w={}, s={}",
        snap.weighted_obi,
        snap.obi
    );
}

// ---------------------------------------------------------------------------
// E2E: ObiSnapshot Copy + Default traits
// ---------------------------------------------------------------------------

#[test]
fn e2e_obi_snapshot_copy_semantics() {
    let bids = make_realistic_book(24500.0, -0.5, 1000);
    let asks = make_realistic_book(24500.5, 0.5, 1000);
    let snap = compute_obi(49081, 2, &bids, &asks);

    // Copy trait: both copies should be identical
    let copy = snap;
    assert_eq!(copy.security_id, snap.security_id);
    assert!((copy.obi - snap.obi).abs() < f64::EPSILON);
    assert!((copy.weighted_obi - snap.weighted_obi).abs() < f64::EPSILON);
    assert_eq!(copy.total_bid_qty, snap.total_bid_qty);
    assert_eq!(copy.total_ask_qty, snap.total_ask_qty);
    assert_eq!(copy.bid_levels, snap.bid_levels);
    assert_eq!(copy.ask_levels, snap.ask_levels);
    assert!((copy.spread - snap.spread).abs() < f64::EPSILON);
}

#[test]
fn e2e_obi_snapshot_default() {
    let snap = ObiSnapshot::default();
    assert_eq!(snap.security_id, 0);
    assert!((snap.obi).abs() < f64::EPSILON);
    assert!((snap.weighted_obi).abs() < f64::EPSILON);
    assert_eq!(snap.total_bid_qty, 0);
    assert_eq!(snap.total_ask_qty, 0);
    assert_eq!(snap.bid_levels, 0);
    assert_eq!(snap.ask_levels, 0);
    assert!((snap.spread).abs() < f64::EPSILON);
    assert_eq!(snap.max_bid_wall_qty, 0);
    assert_eq!(snap.max_ask_wall_qty, 0);
}

// ---------------------------------------------------------------------------
// E2E: Sparse books (gaps in depth levels)
// ---------------------------------------------------------------------------

#[test]
fn e2e_obi_sparse_book_alternating_empty() {
    // Every other level is empty (zero qty)
    let bids: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 100.0 - i as f64 * 0.5,
            quantity: if i % 2 == 0 { 1000 } else { 0 },
            orders: if i % 2 == 0 { 10 } else { 0 },
        })
        .collect();
    let asks: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 100.5 + i as f64 * 0.5,
            quantity: if i % 2 == 0 { 1000 } else { 0 },
            orders: if i % 2 == 0 { 10 } else { 0 },
        })
        .collect();

    let snap = compute_obi(1, 2, &bids, &asks);
    assert_eq!(snap.bid_levels, 10, "10 non-empty out of 20");
    assert_eq!(snap.ask_levels, 10, "10 non-empty out of 20");
    assert!((snap.obi).abs() < f64::EPSILON, "balanced sparse book");
}

// ---------------------------------------------------------------------------
// E2E: Deduplication / uniqueness guarantees
// ---------------------------------------------------------------------------

#[test]
fn e2e_obi_same_instrument_rapid_updates_last_wins() {
    // Simulates same instrument receiving two Bid+Ask pairs in quick succession.
    // The second OBI should reflect the LATEST state, not the first.
    // QuestDB UPSERT with key (ts, security_id, segment) means last-write-wins.

    // First snapshot: heavy bids
    let bids_v1: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 100.0 - i as f64 * 0.1,
            quantity: 5000,
            orders: 10,
        })
        .collect();
    let asks_v1: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 100.1 + i as f64 * 0.1,
            quantity: 500,
            orders: 5,
        })
        .collect();
    let snap_v1 = compute_obi(49081, 2, &bids_v1, &asks_v1);

    // Second snapshot: market flipped, heavy asks
    let bids_v2: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 100.0 - i as f64 * 0.1,
            quantity: 500,
            orders: 5,
        })
        .collect();
    let asks_v2: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 100.1 + i as f64 * 0.1,
            quantity: 5000,
            orders: 10,
        })
        .collect();
    let snap_v2 = compute_obi(49081, 2, &bids_v2, &asks_v2);

    // V1: positive OBI (more bids), V2: negative OBI (more asks)
    assert!(snap_v1.obi > 0.5, "v1 should be bid-heavy");
    assert!(snap_v2.obi < -0.5, "v2 should be ask-heavy");
    // In QuestDB: with same (ts, security_id=49081, segment=2), v2 UPSERTs over v1.
    // This is CORRECT: latest depth state is what matters for trading decisions.
    assert!(
        (snap_v1.obi - snap_v2.obi).abs() > 1.0,
        "v1 and v2 should be drastically different — proving last-write-wins matters"
    );
}

#[test]
fn e2e_obi_different_instruments_same_timestamp_are_distinct() {
    // Two different instruments produce OBI at the same nanosecond.
    // Dedup key includes security_id → both rows persist, no collision.
    let bids = vec![
        DeepDepthLevel {
            price: 100.0,
            quantity: 1000,
            orders: 10,
        };
        20
    ];
    let asks = vec![
        DeepDepthLevel {
            price: 101.0,
            quantity: 500,
            orders: 5,
        };
        20
    ];

    let snap_a = compute_obi(49081, 2, &bids, &asks); // NIFTY instrument
    let snap_b = compute_obi(49082, 2, &bids, &asks); // Different instrument

    // Same OBI value (same book), but different security_id
    assert!((snap_a.obi - snap_b.obi).abs() < f64::EPSILON);
    assert_ne!(snap_a.security_id, snap_b.security_id);
    // In QuestDB: (ts, 49081, NSE_FNO) ≠ (ts, 49082, NSE_FNO) → both stored
}

#[test]
fn e2e_obi_deterministic_replay() {
    // Same input ALWAYS produces same output — no randomness, no state.
    let bids: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 24500.0 - i as f64 * 0.5,
            quantity: 1000 + i as u32 * 100,
            orders: 20 - i as u32,
        })
        .collect();
    let asks: Vec<DeepDepthLevel> = (0..20)
        .map(|i| DeepDepthLevel {
            price: 24500.5 + i as f64 * 0.5,
            quantity: 800 + i as u32 * 50,
            orders: 15_u32.saturating_sub(i as u32).max(1),
        })
        .collect();

    let snap1 = compute_obi(49081, 2, &bids, &asks);
    let snap2 = compute_obi(49081, 2, &bids, &asks);
    let snap3 = compute_obi(49081, 2, &bids, &asks);

    // All three must be bit-for-bit identical
    assert!((snap1.obi - snap2.obi).abs() < f64::EPSILON);
    assert!((snap2.obi - snap3.obi).abs() < f64::EPSILON);
    assert!((snap1.weighted_obi - snap3.weighted_obi).abs() < f64::EPSILON);
    assert_eq!(snap1.total_bid_qty, snap3.total_bid_qty);
    assert_eq!(snap1.total_ask_qty, snap3.total_ask_qty);
    assert_eq!(snap1.max_bid_wall_qty, snap3.max_bid_wall_qty);
    assert_eq!(snap1.max_ask_wall_qty, snap3.max_ask_wall_qty);
    assert!((snap1.spread - snap3.spread).abs() < f64::EPSILON);
}

// ---------------------------------------------------------------------------
// E2E: O(1) contract verification
// ---------------------------------------------------------------------------

#[test]
fn e2e_obi_o1_input_clamped_at_20() {
    // Passing 200 levels — only first 20 processed
    let bids: Vec<DeepDepthLevel> = (0..200)
        .map(|i| DeepDepthLevel {
            price: 100.0 + i as f64,
            quantity: 100,
            orders: 1,
        })
        .collect();
    let asks: Vec<DeepDepthLevel> = (0..200)
        .map(|i| DeepDepthLevel {
            price: 300.0 + i as f64,
            quantity: 100,
            orders: 1,
        })
        .collect();

    let snap = compute_obi(1, 2, &bids, &asks);
    // Only 20 levels counted, not 200
    assert_eq!(snap.bid_levels, 20, "must clamp to MAX_DEPTH_LEVELS=20");
    assert_eq!(snap.ask_levels, 20, "must clamp to MAX_DEPTH_LEVELS=20");
    assert_eq!(snap.total_bid_qty, 2000, "20 × 100");
    assert_eq!(snap.total_ask_qty, 2000, "20 × 100");
}
