#![cfg(test)]

//! Public-API differential checks for the optimized exact-ratio ranker.
//! The reference owns its own rows/baselines; it never reads implementation
//! maps, dense slots, dirty lists, or sorting helpers.
//! The 2026-09-14 oracle compares exact fractions and retains positive
//! sub-milli-lot quantities. Timings recorded before that correction describe
//! the older rounded-key behavior, not this candidate.

use super::*;

const FAMILIES: [OptionFamily; 2] = [OptionFamily::Stock, OptionFamily::Index];

fn fixture_row(index: usize, volume: u32) -> RankedContract {
    // Paired identities exercise the segment tie-break; multiplication spans
    // all eight id bytes rather than testing only small integer ids.
    RankedContract {
        security_id: ((index / 2) as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15),
        segment: if index % 2 == 0 {
            ExchangeSegment::NseFno
        } else {
            ExchangeSegment::BseFno
        },
        underlying_id: (index % 210) as u64,
        volume,
        window_lots_milli: 0,
        delta_units: 0,
        lot_size: 1 + (index as u32 % 4) * 25,
    }
}

fn reference_sort(rows: &mut [RankedContract]) {
    rows.sort_unstable_by(|a, b| {
        (u128::from(b.delta_units) * u128::from(a.lot_size))
            .cmp(&(u128::from(a.delta_units) * u128::from(b.lot_size)))
            .then_with(|| a.security_id.cmp(&b.security_id))
            .then_with(|| (a.segment as u8).cmp(&(b.segment as u8)))
    });
}

fn fallback_lot(row: &RankedContract) -> Option<u32> {
    Some(1 + (row.security_id % 17) as u32)
}

fn is_eligible(row: &RankedContract) -> bool {
    row.underlying_id % 7 != 0
}

#[derive(Clone)]
struct ReferenceEntry {
    row: RankedContract,
    baseline: [u32; SnapshotCadence::ALL.len()],
}

fn reference_rank(
    rows: &mut [ReferenceEntry],
    cadence: SnapshotCadence,
    limit: usize,
) -> Vec<RankedContract> {
    let mut output = Vec::new();
    for entry in rows {
        let mut row = entry.row;
        let delta = row.volume - entry.baseline[cadence.index()];
        entry.baseline[cadence.index()] = row.volume;
        let lot = if row.lot_size == 0 {
            fallback_lot(&row).expect("fixture lot")
        } else {
            row.lot_size
        };
        // Independent arithmetic, not the production window_lots helper.
        let milli_lots = u64::from(delta) * 1_000 / u64::from(lot);
        if delta == 0 || !is_eligible(&row) {
            continue;
        }
        row.delta_units = delta;
        row.lot_size = lot;
        row.window_lots_milli = milli_lots;
        output.push(row);
    }
    reference_sort(&mut output);
    output.truncate(limit);
    output
}

#[test]
fn optimized_exact_ratio_reference_retains_sub_milli_lots_and_extreme_close_fractions() {
    for family in FAMILIES {
        let mut lb = VolumeLeaderboard::new();
        let mut reference = Vec::new();
        for (id, delta, lot) in [
            (1, 2_001, 2_000),
            (9, 2_000, 1_999),
            (2, 1, 4_000),
            (8, 1, 2_000),
            (3, u32::MAX - 2, u32::MAX - 1),
            (7, u32::MAX - 1, u32::MAX),
        ] {
            let seed = RankedContract {
                security_id: id,
                underlying_id: 100,
                lot_size: lot,
                ..fixture_row(0, 1)
            };
            let _ = lb.observe(seed, family);
            let current = RankedContract {
                volume: 1 + delta,
                ..seed
            };
            let _ = lb.observe(current, family);
            reference.push(ReferenceEntry {
                row: current,
                baseline: [1; SnapshotCadence::ALL.len()],
            });
        }
        for cadence in SnapshotCadence::ALL {
            let expected = reference_rank(&mut reference, cadence, usize::MAX);
            assert_eq!(
                expected
                    .iter()
                    .map(|row| row.security_id)
                    .collect::<Vec<_>>(),
                vec![9, 1, 7, 3, 8, 2],
                "the reference itself must reject rounded-key identity order"
            );
            let actual = lb.rank(family, cadence, usize::MAX, fallback_lot, is_eligible);
            assert_eq!(actual, expected.as_slice());
            assert_eq!(actual[4].window_lots_milli, 0);
            assert_eq!(actual[5].window_lots_milli, 0);
        }
    }
}

#[test]
fn optimized_rank_matches_independent_full_rows_for_both_families_and_every_cadence() {
    const POPULATION: usize = 1_024;
    for permutation in 0..3 {
        for limit in [0, 1, 250, usize::MAX] {
            let mut lb = VolumeLeaderboard::new();
            let base: Vec<ReferenceEntry> = (0..POPULATION)
                .map(|i| {
                    let mut row = fixture_row(i, 100_000 + i as u32);
                    if i % 3 == 0 {
                        row.lot_size = 0;
                    }
                    ReferenceEntry {
                        baseline: [row.volume; SnapshotCadence::ALL.len()],
                        row,
                    }
                })
                .collect();
            let mut reference = [base.clone(), base];
            for family in FAMILIES {
                for step in 0..POPULATION {
                    let i = match permutation {
                        0 => step,
                        1 => POPULATION - 1 - step,
                        _ => (step * 373) % POPULATION,
                    };
                    let _ = lb.observe(reference[0][i].row, family);
                }
                assert_eq!(lb.tracked(family), POPULATION);
            }
            for round in 0..3 {
                // Interleave advances and sweeps. Every cadence retains its
                // own baseline, while both families reuse identical keys.
                for cadence in SnapshotCadence::ALL.into_iter().rev() {
                    for (family_index, family) in FAMILIES.into_iter().enumerate() {
                        for (i, entry) in reference[family_index].iter_mut().enumerate() {
                            if (i + round + cadence.index()) % 5 != 0 {
                                continue;
                            }
                            let lot = if entry.row.lot_size == 0 {
                                fallback_lot(&entry.row).expect("fixture lot")
                            } else {
                                entry.row.lot_size
                            };
                            // Numerous EXACT ties force both identity keys to
                            // participate, not just the descending volume key.
                            entry.row.volume += lot * (1 + ((i / 2 + family_index) % 9) as u32);
                            let _ = lb.observe(entry.row, family);
                            // A duplicate must not create duplicate dirty work.
                            let _ = lb.observe(entry.row, family);
                        }
                        let expected = reference_rank(&mut reference[family_index], cadence, limit);
                        let actual = lb.rank(family, cadence, limit, fallback_lot, is_eligible);
                        assert_eq!(
                            actual,
                            expected.as_slice(),
                            "permutation={permutation} limit={limit} round={round} family={family:?} cadence={cadence:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn optimized_full_rank_preserves_wide_keys_and_ties_above_small_sort_sizes() {
    const POPULATION: usize = 4_096;
    for reverse in [false, true] {
        let mut lb = VolumeLeaderboard::new();
        let mut expected = Vec::with_capacity(POPULATION);
        for family in FAMILIES {
            for step in 0..POPULATION {
                let i = if reverse { POPULATION - 1 - step } else { step };
                let seed = fixture_row(i, 1);
                let _ = lb.observe(seed, family);
                let delta = match i % 8 {
                    0 => 1,
                    1 => 255,
                    2 => 65_535,
                    3 => 1 << 24,
                    4 => 1 << 31,
                    5 => u32::MAX - 1,
                    6 => 1 << 16,
                    _ => 1 << 8,
                };
                let row = RankedContract {
                    volume: 1 + delta,
                    ..seed
                };
                let _ = lb.observe(row, family);
                if family == OptionFamily::Stock {
                    expected.push(RankedContract {
                        delta_units: delta,
                        window_lots_milli: u64::from(delta) * 1_000 / u64::from(row.lot_size),
                        ..row
                    });
                }
            }
        }
        reference_sort(&mut expected);
        for family in FAMILIES {
            for cadence in SnapshotCadence::ALL {
                let actual = lb.rank(family, cadence, usize::MAX, fallback_lot, |_| true);
                assert_eq!(actual.len(), POPULATION);
                assert_eq!(
                    actual,
                    expected.as_slice(),
                    "reverse={reverse} family={family:?} cadence={cadence:?}"
                );
            }
        }
    }
}

#[test]
fn optimized_slots_recover_across_relatch_resync_and_daily_reuse() {
    let mut lb = VolumeLeaderboard::new();
    for family in FAMILIES {
        let mut row = fixture_row(4, 1_000_000);
        row.lot_size = 1;
        let _ = lb.observe(row, family);
        row.volume += 100;
        let _ = lb.observe(row, family); // Leave every dirty list populated.
        row.volume = 100;
        for _ in 0..RELATCH_AFTER_CONSECUTIVE_LOWER {
            let _ = lb.observe(row, family);
        }
        assert_eq!(lb.relatches(family), 1);
        row.volume = 1_000_150;
        assert!(matches!(
            lb.observe(row, family),
            Observation::ResyncedToCeiling { .. }
        ));
        for cadence in SnapshotCadence::ALL {
            let ranked = lb.rank(family, cadence, usize::MAX, fallback_lot, |_| true);
            assert_eq!(ranked.len(), 1);
            assert_eq!(ranked[0].delta_units, 50);
            assert_eq!(ranked[0].window_lots_milli, 50_000);
        }
        row.volume += 10;
        let _ = lb.observe(row, family); // Stale dirty indexes must not survive reset.
    }
    lb.reset_daily();
    for family in FAMILIES {
        assert_eq!(lb.tracked(family), 0);
        for i in (20..36).rev() {
            let row = fixture_row(i, 10);
            let _ = lb.observe(row, family);
            let _ = lb.observe(
                RankedContract {
                    volume: 10 + row.lot_size,
                    ..row
                },
                family,
            );
        }
        for cadence in SnapshotCadence::ALL {
            let ranked = lb.rank(family, cadence, usize::MAX, fallback_lot, |_| true);
            assert_eq!(ranked.len(), 16);
            assert!(ranked.iter().all(|row| row.window_lots_milli == 1_000));
            let mut expected: Vec<_> = (20..36)
                .map(|i| {
                    let mut row = fixture_row(i, 10);
                    row.delta_units = row.lot_size;
                    row.volume += row.lot_size;
                    row.window_lots_milli = 1_000;
                    row
                })
                .collect();
            reference_sort(&mut expected);
            assert_eq!(ranked, expected.as_slice());
        }
    }
}

/// Run sequentially on the AWS measurement runner with --release, --ignored,
/// --exact, --nocapture and --test-threads=1. It is in-memory only. Times
/// describe rank calls, excluding updates, construction, reference sorting,
/// assertions, persistence, network and scheduler wait. No latency guarantee.
#[test]
fn optimization_latency_matrix() {
    const SAMPLES: usize = if cfg!(debug_assertions) { 3 } else { 100 };
    for family in FAMILIES {
        for population in [100_usize, 2_000, 20_220, 25_000] {
            if family == OptionFamily::Index && population == 25_000 {
                continue;
            }
            for cadence in SnapshotCadence::ALL {
                let mut lb = VolumeLeaderboard::new();
                let mut rows: Vec<_> = (0..population)
                    .map(|i| fixture_row(i, 1_000_000 + i as u32))
                    .collect();
                for row in &rows {
                    let _ = lb.observe(*row, family);
                }
                assert_eq!(lb.tracked(family), population);
                let mut elapsed_ns = Vec::with_capacity(SAMPLES);
                let mut expected = Vec::with_capacity(population);
                for sample in 0..=SAMPLES {
                    expected.clear();
                    for (i, row) in rows.iter_mut().enumerate() {
                        let delta = (1 + ((i.wrapping_mul(2_654_435_761) + sample) % 997)) as u32;
                        row.volume += delta;
                        let _ = lb.observe(*row, family);
                        let mut projected = *row;
                        projected.delta_units = delta;
                        projected.window_lots_milli =
                            u64::from(delta) * 1_000 / u64::from(row.lot_size);
                        expected.push(projected);
                    }
                    reference_sort(&mut expected);
                    let started = std::time::Instant::now();
                    let ranked = lb.rank(
                        family,
                        cadence,
                        usize::MAX,
                        |_| panic!("carried-lot fixture must not probe fallback"),
                        |_| true,
                    );
                    std::hint::black_box(ranked);
                    let elapsed = started.elapsed().as_nanos();
                    assert_eq!(
                        ranked.len(),
                        population,
                        "never time an empty or truncated board"
                    );
                    assert_eq!(
                        ranked,
                        expected.as_slice(),
                        "sample={sample} family={family:?} cadence={cadence:?}"
                    );
                    if sample != 0 {
                        elapsed_ns.push(elapsed);
                    }
                }
                assert_eq!(elapsed_ns.len(), SAMPLES);
                elapsed_ns.sort_unstable();
                println!(
                    "TV_BENCH family={} cadence={} tracked={} dirty={} output={} samples={} lot=carried p50_ns={} p95_ns={} p99_ns={} max_ns={}",
                    family.as_str(),
                    cadence.as_str(),
                    population,
                    population,
                    population,
                    SAMPLES,
                    elapsed_ns[(SAMPLES * 50).div_ceil(100) - 1],
                    elapsed_ns[(SAMPLES * 95).div_ceil(100) - 1],
                    elapsed_ns[(SAMPLES * 99).div_ceil(100) - 1],
                    elapsed_ns[SAMPLES - 1],
                );
            }
        }
    }
}
