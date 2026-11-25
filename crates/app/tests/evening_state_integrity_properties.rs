//! Regression: 2026-11-02 — integrity and recovery boundaries for evening state.
//! Independent adversarial cases; local filesystem fixtures only.
use tickvault_app::evening_drain::{
    EVENING_DRAIN_FIRST_SECS_IST, EVENING_DRAIN_LAST_RESTART_SECS_IST, EVENING_DRAIN_RESTARTS_MAX,
    EVENING_DRAIN_SPACING_SECS, EveningState, EveningStep, evening_step,
};

const DAY: i32 = 739_910;
const NOW: i64 = 1_792_000_000;

fn boundary_states() -> Vec<EveningState> {
    vec![
        EveningState::default(),
        EveningState {
            day: DAY,
            restarts: 1,
            last_unix: NOW,
            backlog_before: 1,
            pending_unix: NOW,
            gave_up: false,
        },
        EveningState {
            day: DAY,
            restarts: EVENING_DRAIN_RESTARTS_MAX,
            last_unix: NOW,
            backlog_before: 1_696,
            pending_unix: 0,
            gave_up: true,
        },
        EveningState {
            day: i32::MIN,
            restarts: u32::MAX,
            last_unix: i64::MIN,
            backlog_before: u64::MAX,
            pending_unix: i64::MAX,
            gave_up: false,
        },
        EveningState {
            day: i32::MAX,
            restarts: u32::MAX,
            last_unix: i64::MAX,
            backlog_before: u64::MAX,
            pending_unix: i64::MIN,
            gave_up: true,
        },
    ]
}

#[test]
fn independent_versioned_record_never_accepts_a_changed_single_bit_state() {
    let mut attempts = 0;
    for original in boundary_states() {
        let line = original.to_line();
        assert!(
            line.starts_with("v2 "),
            "new writes must carry integrity metadata"
        );
        assert_eq!(EveningState::from_line(&line), Some(original));
        for index in 0..line.len() {
            for bit in 0..8 {
                let mut damaged = line.as_bytes().to_vec();
                damaged[index] ^= 1 << bit;
                if let Ok(text) = std::str::from_utf8(&damaged) {
                    assert!(
                        EveningState::from_line(text).is_none_or(|s| s == original),
                        "state changed after bit flip at byte {index}, bit {bit}: {text:?}"
                    );
                }
                attempts += 1;
            }
        }
    }
    assert!(attempts > 1_000, "insufficient mutation cases: {attempts}");
}

#[test]
fn independent_all_truncations_reject_or_keep_the_entire_original_state() {
    let mut attempts = 0;
    for state in boundary_states() {
        let line = state.to_line();
        for cut in 0..=line.len() {
            let parsed = EveningState::from_line(&line[..cut]);
            assert!(
                parsed.is_none_or(|s| s == state),
                "truncation changed fields at offset {cut}"
            );
            if cut == line.len() {
                assert_eq!(parsed, Some(state), "positive complete-record control");
            }
            attempts += 1;
        }
        for suffix in [" 0", " garbage", "\nv2 0 0 0 0 0 0 00000000", "\0"] {
            assert_eq!(
                EveningState::from_line(&(line.clone() + suffix)),
                None,
                "extra content accepted: {suffix:?}"
            );
        }
    }
    assert!(attempts > 100);
}

#[test]
fn independent_round_trip_covers_integer_boundaries_without_narrowing() {
    let mut cases = 0;
    for day in [i32::MIN, 0, DAY, i32::MAX] {
        for restarts in [0, 1, EVENING_DRAIN_RESTARTS_MAX, u32::MAX] {
            for last_unix in [i64::MIN, -1, 0, NOW, i64::MAX] {
                for backlog_before in [0, 1, u64::MAX] {
                    for pending_unix in [i64::MIN, 0, NOW, i64::MAX] {
                        for gave_up in [false, true] {
                            let state = EveningState {
                                day,
                                restarts,
                                last_unix,
                                backlog_before,
                                pending_unix,
                                gave_up,
                            };
                            assert_eq!(EveningState::from_line(&state.to_line()), Some(state));
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 1_920);
}

#[test]
fn independent_cap_latch_and_clock_never_allow_a_restart() {
    let times = [
        i64::MIN,
        NOW - 1,
        NOW,
        NOW + EVENING_DRAIN_SPACING_SECS - 1,
        NOW + EVENING_DRAIN_SPACING_SECS,
        i64::MAX,
    ];
    let clocks = [
        0,
        EVENING_DRAIN_LAST_RESTART_SECS_IST - 1,
        EVENING_DRAIN_LAST_RESTART_SECS_IST,
        12 * 3_600,
        EVENING_DRAIN_FIRST_SECS_IST - 1,
        EVENING_DRAIN_FIRST_SECS_IST,
        86_399,
    ];
    let mut cases = 0;
    for restarts in [0, 1, EVENING_DRAIN_RESTARTS_MAX, u32::MAX] {
        for gave_up in [false, true] {
            let state = EveningState {
                day: DAY,
                restarts,
                last_unix: NOW,
                backlog_before: 10,
                pending_unix: 0,
                gave_up,
            };
            for now in times {
                for secs in clocks {
                    for backlog in [None, Some(0), Some(1), Some(10), Some(u64::MAX)] {
                        for other_done in [false, true] {
                            for settled in [false, true] {
                                let step = evening_step(
                                    &state, DAY, other_done, settled, backlog, now, secs,
                                );
                                if gave_up {
                                    assert_eq!(step, EveningStep::GaveUp);
                                }
                                if restarts >= EVENING_DRAIN_RESTARTS_MAX
                                    || !other_done
                                    || !settled
                                    || (EVENING_DRAIN_LAST_RESTART_SECS_IST
                                        ..EVENING_DRAIN_FIRST_SECS_IST)
                                        .contains(&secs)
                                {
                                    assert_ne!(step, EveningStep::Restart);
                                }
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 6_720);
    assert_eq!(
        evening_step(
            &EveningState::default(),
            DAY,
            true,
            true,
            Some(10),
            NOW,
            EVENING_DRAIN_FIRST_SECS_IST
        ),
        EveningStep::Restart,
        "positive restart control prevents a blanket no-op implementation"
    );
}

#[test]
fn independent_legacy_records_cannot_drive_current_runtime_decisions() {
    for state in boundary_states() {
        let legacy = format!(
            "v1 {} {} {} {} {} {}\n",
            state.day,
            state.restarts,
            state.last_unix,
            state.backlog_before,
            state.pending_unix,
            u8::from(state.gave_up)
        );
        assert_eq!(
            EveningState::from_line(&legacy),
            None,
            "legacy data lacks integrity evidence and must not restore counters"
        );
    }
}
