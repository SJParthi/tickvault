//! Run-on-demand proof and timing for the two-decimal fast path in
//! `price_precision::f32_to_f64_clean` (plan ITEM DT, 2026-11-17).
//!
//! Both tests are `#[ignore]`d: the all-2^32 comparison takes about 7
//! minutes in release (far longer in debug), and the timing harness reads a
//! wall clock. They live here, outside `src`, so code CI never runs is not
//! counted in the source file's coverage. The gate that runs in every suite
//! is in the source file's own tests (the bound-band sweep and two
//! proptests against the same frozen copy).
//!
//! Run:
//!   cargo test -p tickvault-common --release --test price_fast_path_harness -- --ignored --nocapture
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;

use tickvault_common::price_precision::f32_to_f64_clean;

/// The fast path's bound: every f32 below it with a two-decimal value is
/// served by the scale-100 candidate. Mirrors the private constant.
const TWO_DECIMAL_FAST_PATH_MAX_MAGNITUDE: f32 = 131_072.0;
const TWO_DECIMAL_SCALE: f64 = 1e2;
const DECIMAL_SCALES: [f64; 10] = [1.0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9];
const ARITHMETIC_PATH_MAX_MAGNITUDE: f32 = 16_777_216.0;

fn same_bits(left: f64, right: f64) -> bool {
    left.to_bits() == right.to_bits() || (left.is_nan() && right.is_nan())
}

/// The per-scale step, copied: the nearest candidate, then the other neighbour.
fn hit_at_scale(v: f32, widened: f64, scale: f64) -> Option<f64> {
    let scaled = widened * scale;
    let nearest = scaled.round();
    let candidate = nearest / scale;
    #[allow(clippy::cast_possible_truncation)]
    // APPROVED: narrowing is the round-trip test itself
    if candidate as f32 == v {
        return Some(candidate);
    }
    let other = if scaled > nearest {
        nearest + 1.0
    } else {
        nearest - 1.0
    };
    let candidate = other / scale;
    #[allow(clippy::cast_possible_truncation)]
    // APPROVED: narrowing is the round-trip test itself
    if candidate as f32 == v {
        return Some(candidate);
    }
    None
}

/// The format-and-parse reference, copied.
fn via_text(v: f32) -> f64 {
    if v == 0.0 || !v.is_finite() {
        return f64::from(v);
    }
    let mut buf = [0u8; 24];
    let mut cursor = std::io::Cursor::new(&mut buf[..]);
    drop(write!(cursor, "{v}"));
    #[allow(clippy::cast_possible_truncation)]
    // APPROVED: cursor position of a 24-byte buf always fits usize
    let n = cursor.position() as usize;
    std::str::from_utf8(&buf[..n])
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(f64::from(v))
}

/// `f32_to_f64_clean` exactly as it was before the two-decimal fast path
/// (2026-11-17), copied from the frozen copy in the source file's tests.
/// Frozen; never edit it to make a test pass.
fn f32_to_f64_clean_frozen_before_fast_path(v: f32) -> f64 {
    let magnitude = v.abs();
    if magnitude > 0.0 && magnitude < ARITHMETIC_PATH_MAX_MAGNITUDE {
        let widened = f64::from(v);
        for scale in DECIMAL_SCALES {
            if let Some(hit) = hit_at_scale(v, widened, scale) {
                return hit;
            }
        }
    }
    via_text(v)
}

/// The frozen copy here and the source file's frozen copy are the same
/// function: checked on a stride of every bit pattern plus the bound band.
#[test]
fn harness_frozen_copy_agrees_with_the_library_on_a_stride() {
    let mut checked = 0_u64;
    for bits in (0..=u32::MAX).step_by(65_537) {
        let v = f32::from_bits(bits);
        assert!(
            same_bits(
                f32_to_f64_clean(v),
                f32_to_f64_clean_frozen_before_fast_path(v)
            ),
            "{v}"
        );
        checked += 1;
    }
    let bound_bits = TWO_DECIMAL_FAST_PATH_MAX_MAGNITUDE.to_bits();
    for bits in bound_bits - 2_000..bound_bits + 2_000 {
        let v = f32::from_bits(bits);
        assert!(
            same_bits(
                f32_to_f64_clean(v),
                f32_to_f64_clean_frozen_before_fast_path(v)
            ),
            "{v}"
        );
        assert!(
            same_bits(
                f32_to_f64_clean(-v),
                f32_to_f64_clean_frozen_before_fast_path(-v)
            ),
            "{v}"
        );
        checked += 2;
    }
    assert!(checked > 70_000, "{checked}");
}

#[test]
#[ignore = "exhaustive: about 7 minutes in release, far longer in debug"]
fn test_f32_to_f64_clean_matches_frozen_search_for_every_f32() {
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    let threads = u64::try_from(threads).unwrap_or(4);
    let span = 1_u64 << 32;
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            std::thread::spawn(move || {
                let (low, high) = (span * t / threads, span * (t + 1) / threads);
                let mut first_difference = None;
                let mut differences = 0_u64;
                let mut fast_path_inputs = 0_u64;
                for bits in low..high {
                    #[allow(clippy::cast_possible_truncation)]
                    // APPROVED: bits < 2^32
                    let v = f32::from_bits(bits as u32);
                    let magnitude = v.abs();
                    if magnitude > 0.0 && magnitude < TWO_DECIMAL_FAST_PATH_MAX_MAGNITUDE {
                        fast_path_inputs += 1;
                    }
                    let (new, old) = (
                        f32_to_f64_clean(v),
                        f32_to_f64_clean_frozen_before_fast_path(v),
                    );
                    if !same_bits(new, old) {
                        differences += 1;
                        first_difference.get_or_insert((v, new, old));
                    }
                }
                (differences, first_difference, fast_path_inputs)
            })
        })
        .collect();
    let (mut differences, mut first, mut fast_path_inputs) = (0, None, 0);
    for handle in handles {
        let (count, example, fast) = handle.join().expect("worker thread panicked");
        differences += count;
        first = first.or(example);
        fast_path_inputs += fast;
    }
    // Non-vacuity: the fast-path region is most of the f32 space.
    assert!(fast_path_inputs > 2_000_000_000, "{fast_path_inputs}");
    assert_eq!(differences, 0, "first difference: {first:?}");
}

/// Release timing of the new conversion against the frozen search on
/// a price-like mix. Prints; asserts nothing about speed.
#[test]
#[ignore = "wall-clock timing harness, release only"]
fn test_f32_to_f64_clean_fast_path_timing() {
    // 4,096 inputs: 70% two-decimal prices 0.05..1,00,000 on a
    // 0.05 tick, 15% one-decimal, 10% integers, 5% odd values.
    let mut inputs = Vec::with_capacity(4_096);
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for i in 0..4_096_u32 {
        let r = next();
        #[allow(clippy::cast_precision_loss)]
        // APPROVED: test input generation
        let v = match i % 20 {
            0..=13 => ((r % 2_000_000) + 1) as f32 * 5.0 / 100.0,
            14..=16 => ((r % 1_000_000) + 1) as f32 / 10.0,
            17 | 18 => ((r % 100_000) + 1) as f32,
            _ => ((r % 10_000_000) + 1) as f32 / 1_000.0 + 0.000_7,
        };
        inputs.push(v);
    }
    // The mix must give identical output under both functions, and
    // most of it must be the fast path's two-decimal shape.
    for &v in &inputs {
        let (new, old) = (
            f32_to_f64_clean(v),
            f32_to_f64_clean_frozen_before_fast_path(v),
        );
        assert!(same_bits(new, old), "{v}: {new} != {old}");
    }
    let fast_hits = inputs
        .iter()
        .filter(|&&v| hit_at_scale(v, f64::from(v), TWO_DECIMAL_SCALE).is_some())
        .count();
    assert!(
        fast_hits * 10 >= inputs.len() * 9,
        "fast-path hits {fast_hits}"
    );
    let rounds = 2_000;
    let time = |f: fn(f32) -> f64| {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let mut sum = 0.0_f64;
            for _ in 0..rounds {
                for &v in &inputs {
                    sum += f(std::hint::black_box(v));
                }
            }
            std::hint::black_box(sum);
            #[allow(clippy::cast_precision_loss)]
            // APPROVED: timing arithmetic
            let per_call = start.elapsed().as_nanos() as f64 / (rounds * inputs.len()) as f64;
            best = best.min(per_call);
        }
        best
    };
    let old = time(f32_to_f64_clean_frozen_before_fast_path);
    let new = time(f32_to_f64_clean);
    let old_again = time(f32_to_f64_clean_frozen_before_fast_path);
    let new_again = time(f32_to_f64_clean);
    assert!(old > 0.0 && new > 0.0 && old_again > 0.0 && new_again > 0.0);
    #[allow(clippy::print_stderr)]
    {
        eprintln!(
            "f32_to_f64_clean per call (best of 5, ns): frozen search {old:.2} / {old_again:.2}, fast path {new:.2} / {new_again:.2}"
        );
    }
    // Miss-heavy mix: 3-decimal values up to 10,000 and 4-decimal
    // values up to 1,000 (all below 2^17), where the
    // fast path misses and the search runs. Measures the cost of a
    // miss, which the price mix above barely reaches.
    let mut misses = Vec::with_capacity(4_096);
    for i in 0..4_096_u32 {
        let r = next();
        #[allow(clippy::cast_precision_loss)]
        // APPROVED: test input generation
        let v = if i % 2 == 0 {
            ((r % 10_000_000) + 1) as f32 / 1_000.0
        } else {
            ((r % 10_000_000) + 1) as f32 / 10_000.0
        };
        misses.push(v);
    }
    for &v in &misses {
        let (new, old) = (
            f32_to_f64_clean(v),
            f32_to_f64_clean_frozen_before_fast_path(v),
        );
        assert!(same_bits(new, old), "{v}: {new} != {old}");
    }
    let miss_count = misses
        .iter()
        .filter(|&&v| hit_at_scale(v, f64::from(v), TWO_DECIMAL_SCALE).is_none())
        .count();
    assert!(
        miss_count * 10 >= misses.len() * 8,
        "fast-path misses {miss_count}"
    );
    let time_misses = |f: fn(f32) -> f64| {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let mut sum = 0.0_f64;
            for _ in 0..rounds {
                for &v in &misses {
                    sum += f(std::hint::black_box(v));
                }
            }
            std::hint::black_box(sum);
            #[allow(clippy::cast_precision_loss)]
            // APPROVED: timing arithmetic
            let per_call = start.elapsed().as_nanos() as f64 / (rounds * misses.len()) as f64;
            best = best.min(per_call);
        }
        best
    };
    let miss_old = time_misses(f32_to_f64_clean_frozen_before_fast_path);
    let miss_new = time_misses(f32_to_f64_clean);
    let miss_old_again = time_misses(f32_to_f64_clean_frozen_before_fast_path);
    let miss_new_again = time_misses(f32_to_f64_clean);
    assert!(miss_old > 0.0 && miss_new > 0.0 && miss_old_again > 0.0 && miss_new_again > 0.0);
    #[allow(clippy::print_stderr)]
    {
        eprintln!(
            "f32_to_f64_clean miss-heavy mix ({miss_count} of {} miss the fast path), per call (best of 5, ns): frozen search {miss_old:.2} / {miss_old_again:.2}, fast path {miss_new:.2} / {miss_new_again:.2}",
            misses.len()
        );
    }
}
