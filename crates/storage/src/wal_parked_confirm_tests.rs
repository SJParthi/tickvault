//! Tests for `wal_parked_confirm` (2026-10-27, plan item PR W4): the pure
//! state machine, the watermark wired to it, and a property test that no
//! frame acked inside or just before a suspicion episode is ever trusted
//! unless the confirmation rule held.

use super::*;
use crate::wal_applied_watermark::{
    AppliedSink, AppliedSnapshot, AppliedWatermark, REPLAY_REORDER_SLACK_SEQ,
};
use proptest::prelude::*;
use std::path::PathBuf;

const SEC: u64 = 1_000_000_000;
const LAG: u64 = WATERMARK_DURABILITY_LAG_SECS * SEC;

/// A 2026-era sequence: nanos with the 17 reserved low bits zeroed.
fn seq(secs: u64) -> u64 {
    ((1_780_000_000 + secs) * SEC) >> 17 << 17
}

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tv-parked-confirm-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn ack_both(wm: &AppliedWatermark, s: u64) {
    wm.note_ticks_acked(s);
    wm.note_depth_acked(s);
}

fn applied(wm: &AppliedWatermark, s: u64) -> bool {
    wm.snapshot().frame_is_applied(AppliedSink::Ticks, s)
}

fn covered(wm: &AppliedWatermark, s: u64) -> bool {
    wm.snapshot().range_has_unapplied(s, s)
}

/// A clean probe, then a lag-only episode with one parked ack, ended by a
/// clean probe at `t_exit`. Returns the parked ack's sequence.
fn lag_episode(wm: &AppliedWatermark, base: u64, t0: u64) -> u64 {
    ack_both(wm, seq(base));
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, t0);
    wm.note_questdb_probe_verdict_at(ProbeVerdict::LagOnly, t0 + 60 * SEC);
    let parked = seq(base + 100);
    ack_both(wm, parked);
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, t0 + 120 * SEC);
    parked
}

#[test]
fn test_parked_ack_is_confirmed_after_lag_and_two_clean_probes() {
    let dir = scratch("confirm");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    let t_exit = 120 * SEC;
    let parked = lag_episode(&wm, 1_000, 0);
    assert!(covered(&wm, seq(1_050)), "the parked stretch is pinned");
    assert!(!applied(&wm, seq(1_050)));
    // Two clean probes inside the lag: not yet.
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, t_exit + SEC);
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, t_exit + 2 * SEC);
    assert!(
        covered(&wm, seq(1_050)),
        "the durability lag has not passed"
    );
    // Past the lag, with two clean probes after the exit: confirmed.
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, t_exit + LAG);
    assert!(!covered(&wm, seq(1_050)), "the episode's range is cut");
    assert!(!covered(&wm, seq(999)), "the detection window too");
    assert!(applied(&wm, seq(1_050)), "the parked ack is trusted");
    assert!(wm.snapshot().hwm_ticks >= parked, "watermark raised");
    let on_disk = AppliedSnapshot::load(&dir).expect("persisted");
    assert_eq!(on_disk.ranges, wm.snapshot().ranges, "the cut is durable");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_parked_ack_needs_two_clean_probes_after_the_exit_not_one() {
    let dir = scratch("one-probe");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    lag_episode(&wm, 1_000, 0);
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC + 10 * LAG);
    assert!(
        covered(&wm, seq(1_050)),
        "one clean probe after the exit is not enough, however late"
    );
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC + 11 * LAG);
    assert!(!covered(&wm, seq(1_050)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_episode_before_confirmation_keeps_the_range() {
    let dir = scratch("new-episode");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    let t_exit = 120 * SEC;
    lag_episode(&wm, 1_000, 0);
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, t_exit + 30 * SEC);
    // A second episode begins before the first was confirmed.
    ack_both(&wm, seq(1_300));
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, t_exit + 40 * SEC);
    wm.note_questdb_probe_verdict_at(ProbeVerdict::LagOnly, t_exit + 50 * SEC);
    let t2 = t_exit + 100 * SEC;
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, t2);
    // The second episode is confirmed in its turn ...
    // O(1) EXEMPT: test loop
    for k in 1..=3 {
        wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, t2 + k * LAG);
    }
    assert!(!covered(&wm, seq(1_300)), "the second episode is cut");
    // ... and the first episode's stretch is still pinned.
    assert!(
        covered(&wm, seq(1_050)),
        "the first episode keeps its range"
    );
    assert!(!applied(&wm, seq(1_050)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_regression_untrusted_episode_is_never_confirmed() {
    let dir = scratch("untrusted");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    ack_both(&wm, seq(1_000));
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 0);
    wm.note_questdb_probe_verdict_at(ProbeVerdict::LagOnly, 60 * SEC);
    ack_both(&wm, seq(1_100));
    // A suspended table (or a blind probe) inside the episode.
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Untrusted, 90 * SEC);
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC);
    // O(1) EXEMPT: test loop
    for k in 1..=4 {
        wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC + k * LAG);
    }
    assert!(
        covered(&wm, seq(1_050)),
        "an untrusted episode keeps its range"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_regression_bare_unclean_probe_is_untrusted() {
    let dir = scratch("bare");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    ack_both(&wm, seq(1_000));
    wm.note_questdb_probe(true);
    wm.note_questdb_probe(false);
    ack_both(&wm, seq(1_100));
    wm.note_questdb_probe(true);
    // O(1) EXEMPT: test loop
    for _ in 0..3 {
        wm.note_questdb_probe(true);
    }
    // The bool API carries no cause, so it never confirms (and the real
    // clock has not passed the lag either way).
    assert!(covered(&wm, seq(1_050)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_regression_foreign_range_over_the_episode_keeps_it() {
    let dir = scratch("foreign");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    ack_both(&wm, seq(1_000));
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 0);
    wm.note_questdb_probe_verdict_at(ProbeVerdict::LagOnly, 60 * SEC);
    ack_both(&wm, seq(1_100));
    // Another source (a reorder violation, say) inside the parked stretch.
    wm.add_unapplied_range(seq(1_050), seq(1_050));
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC);
    // O(1) EXEMPT: test loop
    for k in 1..=3 {
        wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC + k * LAG);
    }
    assert!(covered(&wm, seq(1_050)), "the foreign range is never cut");
    assert!(
        covered(&wm, seq(1_020)),
        "nor is the episode's range around it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_regression_foreign_range_after_the_exit_keeps_it() {
    let dir = scratch("foreign-after");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    lag_episode(&wm, 1_000, 0);
    wm.add_unapplied_range(seq(1_040), seq(1_041));
    // O(1) EXEMPT: test loop
    for k in 1..=3 {
        wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC + k * LAG);
    }
    assert!(covered(&wm, seq(1_040)), "the foreign range survives");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_regression_range_present_before_the_episode_keeps_it() {
    let dir = scratch("pre");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    ack_both(&wm, seq(1_000));
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 0);
    // Already there (a settle recovery, say), inside the detection window.
    wm.add_unapplied_range(seq(999), seq(999));
    wm.note_questdb_probe_verdict_at(ProbeVerdict::LagOnly, 60 * SEC);
    ack_both(&wm, seq(1_100));
    wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC);
    // O(1) EXEMPT: test loop
    for k in 1..=3 {
        wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC + k * LAG);
    }
    assert!(covered(&wm, seq(999)), "the earlier range is never cut");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_regression_crash_before_confirmation_keeps_the_range() {
    let dir = scratch("crash");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    lag_episode(&wm, 1_000, 0);
    // The process dies before the rule is met; the next one knows nothing.
    let next = AppliedWatermark::new();
    next.bind(&dir);
    // O(1) EXEMPT: test loop
    for k in 1..=3 {
        next.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC + k * LAG);
    }
    assert!(covered(&next, seq(1_050)), "a restart keeps the range");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_note_questdb_probe_verdict_with_basis_tracks_a_lag_episode() {
    let dir = scratch("basis");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    ack_both(&wm, seq(1_000));
    let basis = wm.probe_basis();
    wm.note_questdb_probe_verdict_with_basis(ProbeVerdict::Clean, basis);
    wm.note_questdb_probe_verdict_with_basis(ProbeVerdict::LagOnly, wm.probe_basis());
    assert!(wm.is_sink_suspect());
    ack_both(&wm, seq(1_100));
    wm.note_questdb_probe_verdict_with_basis(ProbeVerdict::Clean, wm.probe_basis());
    assert!(!wm.is_sink_suspect());
    assert!(covered(&wm, seq(1_050)), "pinned until the rule is met");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the pure state machine ----

#[test]
fn test_from_readings_and_is_clean() {
    assert_eq!(ProbeVerdict::from_readings(true, 0, 0), ProbeVerdict::Clean);
    assert_eq!(
        ProbeVerdict::from_readings(true, 0, 3),
        ProbeVerdict::LagOnly
    );
    assert_eq!(
        ProbeVerdict::from_readings(true, 1, 3),
        ProbeVerdict::Untrusted
    );
    assert_eq!(
        ProbeVerdict::from_readings(false, 0, 0),
        ProbeVerdict::Untrusted
    );
    assert!(ProbeVerdict::Clean.is_clean());
    assert!(!ProbeVerdict::LagOnly.is_clean());
    assert!(!ProbeVerdict::Untrusted.is_clean());
}

#[test]
fn test_own_union_merges_touching_spans_only() {
    assert_eq!(
        own_union((Some((1, 10)), Some((11, 20)))),
        [Some((1, 20)), None]
    );
    assert_eq!(
        own_union((Some((1, 10)), Some((12, 20)))),
        [Some((1, 10)), Some((12, 20))]
    );
    assert_eq!(own_union((None, Some((5, 6)))), [None, Some((5, 6))]);
}

fn mark(at: u64, clean: u64, episodes: u64) -> ExitMark {
    ExitMark {
        at_mono: at,
        clean_probes: clean,
        episodes,
        parked_ticks: 7,
        parked_depth: 8,
    }
}

#[test]
fn test_parked_confirm_new_begin_own_spans_exit_decide() {
    let mut c = ParkedConfirm::default();
    assert!(!c.is_tracking());
    assert_eq!(c.decide(0, 0, 0, false), Decision::Idle);
    assert_eq!(c.begin(ProbeVerdict::LagOnly, false), None);
    assert!(c.is_tracking());
    c.own(Sink::Ticks, 10, 20);
    c.own(Sink::Ticks, 15, 30);
    c.own(Sink::Depth, 5, 9);
    assert_eq!(c.spans(), (Some((10, 30)), Some((5, 9))));
    assert_eq!(c.decide(0, 0, 1, true), Decision::Wait, "still open");
    c.exit(mark(100, 5, 1));
    assert_eq!(
        c.decide(100 + LAG, 6, 1, false),
        Decision::Wait,
        "one probe"
    );
    assert_eq!(
        c.decide(99 + LAG, 7, 1, false),
        Decision::Wait,
        "inside the lag"
    );
    assert_eq!(
        c.decide(100 + LAG, 7, 1, false),
        Decision::Confirm {
            ticks: Some((10, 30)),
            depth: Some((5, 9)),
            parked_ticks: 7,
            parked_depth: 8,
        }
    );
    assert!(!c.is_tracking(), "back to idle");
}

#[test]
fn test_parked_confirm_begin_drops_a_pending_episode() {
    let mut c = ParkedConfirm::new();
    c.begin(ProbeVerdict::LagOnly, false);
    c.exit(mark(0, 1, 1));
    assert_eq!(
        c.begin(ProbeVerdict::LagOnly, false),
        Some(KEPT_NEW_EPISODE)
    );
    assert_eq!(
        c.begin(ProbeVerdict::Untrusted, false),
        Some(KEPT_NEW_EPISODE)
    );
    c.exit(mark(0, 1, 2));
    assert_eq!(
        c.decide(LAG, 3, 2, false),
        Decision::Keep(KEPT_UNTRUSTED_PROBE)
    );
}

#[test]
fn test_parked_confirm_observe_unclean_taints_only_on_untrusted() {
    let mut c = ParkedConfirm::new();
    c.begin(ProbeVerdict::LagOnly, false);
    c.observe_unclean(ProbeVerdict::LagOnly);
    c.exit(mark(0, 1, 1));
    assert!(matches!(
        c.decide(LAG, 3, 1, false),
        Decision::Confirm { .. }
    ));
    c.begin(ProbeVerdict::LagOnly, false);
    c.observe_unclean(ProbeVerdict::Untrusted);
    c.exit(mark(0, 1, 2));
    assert_eq!(
        c.decide(LAG, 3, 2, false),
        Decision::Keep(KEPT_UNTRUSTED_PROBE)
    );
}

#[test]
fn test_parked_confirm_touches_note_foreign_add_and_note_foreign_present() {
    let mut c = ParkedConfirm::new();
    c.note_foreign_add(1, 100);
    c.note_foreign_present();
    assert!(!c.is_tracking(), "idle ignores both");
    c.begin(ProbeVerdict::LagOnly, false);
    c.own(Sink::Ticks, 10, 20);
    assert!(c.touches(20, 25) && c.touches(5, 10) && !c.touches(21, 30));
    assert!(!c.touches(30, 20), "an empty range touches nothing");
    c.note_foreign_add(21, 30);
    c.exit(mark(0, 1, 1));
    assert!(matches!(
        c.decide(LAG, 3, 1, false),
        Decision::Confirm { .. }
    ));
    c.begin(ProbeVerdict::LagOnly, false);
    c.own(Sink::Ticks, 10, 20);
    c.note_foreign_add(15, 15);
    c.exit(mark(0, 1, 2));
    assert_eq!(
        c.decide(LAG, 3, 2, false),
        Decision::Keep(KEPT_FOREIGN_RANGE)
    );
    c.begin(ProbeVerdict::LagOnly, true);
    c.exit(mark(0, 1, 3));
    assert_eq!(
        c.decide(LAG, 3, 3, false),
        Decision::Keep(KEPT_FOREIGN_RANGE)
    );
    c.begin(ProbeVerdict::LagOnly, false);
    c.note_foreign_present();
    c.exit(mark(0, 1, 4));
    assert_eq!(
        c.decide(LAG, 3, 4, false),
        Decision::Keep(KEPT_FOREIGN_RANGE)
    );
}

#[test]
fn test_parked_confirm_decide_keeps_after_a_later_episode_or_while_suspect() {
    let mut c = ParkedConfirm::new();
    c.begin(ProbeVerdict::LagOnly, false);
    c.exit(mark(0, 1, 1));
    assert_eq!(c.decide(LAG, 3, 2, false), Decision::Keep(KEPT_NEW_EPISODE));
    c.begin(ProbeVerdict::LagOnly, false);
    c.exit(mark(0, 1, 1));
    assert_eq!(c.decide(LAG, 3, 1, true), Decision::Keep(KEPT_NEW_EPISODE));
}

#[test]
fn test_parked_confirm_exit_only_from_open() {
    let mut c = ParkedConfirm::new();
    c.exit(mark(0, 1, 1));
    assert!(!c.is_tracking(), "no episode open: exit ignored");
}

#[test]
fn test_register_counters_names_every_kept_reason() {
    // No recorder is installed in unit tests, so this only proves it runs;
    // the reasons it seeds are the three the confirmation can return.
    register_counters();
    assert_eq!(
        KEPT_REASONS,
        [KEPT_UNTRUSTED_PROBE, KEPT_FOREIGN_RANGE, KEPT_NEW_EPISODE]
    );
}

// ---- the property test ----

#[derive(Debug, Clone, Copy)]
enum Op {
    /// Ack both sinks `secs` later than the last ack.
    Ack(u64),
    /// A probe `secs` after the last one.
    Probe(u8, u64),
    /// A range from another source over the last `secs` of acks.
    Foreign(u64),
    /// The process dies and the next one binds the same directory.
    Crash,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (1u64..40).prop_map(Op::Ack),
        5 => (0u8..10, 1u64..90).prop_map(|(k, s)| Op::Probe(k, s)),
        1 => (1u64..30).prop_map(Op::Foreign),
        1 => Just(Op::Crash),
    ]
}

/// What the oracle knows about one episode.
#[derive(Debug, Clone)]
struct Episode {
    /// Frames acked inside it or in its detection window.
    frames: Vec<u64>,
    lag_only: bool,
    /// `(instant, clean probes so far)` at its end.
    exit: Option<(u64, u64)>,
    /// A later episode began, or the process died, before the rule held.
    superseded: bool,
    /// The rule held at some step (sticky: the property then allows trust).
    eligible: bool,
}

impl Episode {
    /// Records whether the rule holds at `now` with `clean` probes so far.
    fn refresh(&mut self, now: u64, clean: u64) {
        self.eligible |= self.lag_only
            && !self.superseded
            && self
                .exit
                .is_some_and(|(at, c)| now >= at + LAG && clean >= c + 2);
    }

    /// A later episode began or the process died.
    fn supersede(&mut self, now: u64, clean: u64) {
        self.refresh(now, clean);
        if !self.eligible {
            self.superseded = true;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// No frame acked inside or just before a suspicion episode is ever
    /// trusted unless that episode was lag-only, ended, and was followed by
    /// the durability lag and two clean probes with no episode and no crash
    /// in between. And no range from another source ever loses a sequence.
    #[test]
    fn proptest_no_episode_frame_is_trusted_before_the_rule_holds(
        ops in prop::collection::vec(op(), 1..60)
    ) {
        let dir = scratch("prop");
        let mut wm = AppliedWatermark::new();
        wm.bind(&dir);
        let mut last_ack = 1_000u64;
        let mut now = 0u64;
        let mut clean = 0u64;
        // Frames acked since the last clean probe: the next episode's
        // detection window.
        let mut since_clean: Vec<u64> = Vec::new();
        let mut episodes: Vec<Episode> = Vec::new();
        let mut open: Option<usize> = None;
        let mut foreign: Vec<(u64, u64)> = Vec::new();
        ack_both(&wm, seq(last_ack));
        for op in ops {
            match op {
                Op::Ack(dt) => {
                    last_ack += dt;
                    ack_both(&wm, seq(last_ack));
                    match open {
                        Some(i) => episodes[i].frames.push(seq(last_ack)),
                        None => since_clean.push(seq(last_ack)),
                    }
                }
                Op::Probe(kind, dt) => {
                    now += dt * SEC;
                    let verdict = match kind {
                        0..=4 => ProbeVerdict::Clean,
                        5..=8 => ProbeVerdict::LagOnly,
                        _ => ProbeVerdict::Untrusted,
                    };
                    wm.note_questdb_probe_verdict_at(verdict, now);
                    if verdict.is_clean() {
                        clean += 1;
                        if let Some(i) = open.take() {
                            episodes[i].exit = Some((now, clean));
                        }
                        since_clean.clear();
                    } else if let Some(i) = open {
                        episodes[i].lag_only &= verdict == ProbeVerdict::LagOnly;
                    } else {
                        // O(1) EXEMPT: test oracle
                        for e in &mut episodes {
                            e.supersede(now, clean);
                        }
                        episodes.push(Episode {
                            frames: std::mem::take(&mut since_clean),
                            lag_only: verdict == ProbeVerdict::LagOnly,
                            exit: None,
                            superseded: false,
                            eligible: false,
                        });
                        open = Some(episodes.len() - 1);
                    }
                }
                Op::Foreign(back) => {
                    let lo = seq(last_ack.saturating_sub(back));
                    let hi = seq(last_ack);
                    wm.add_unapplied_range(lo, hi);
                    foreign.push((lo, hi));
                }
                Op::Crash => {
                    wm = AppliedWatermark::new();
                    wm.bind(&dir);
                    // The production boot fold: the boot tail covers every
                    // frame above the persisted watermark, and the counter
                    // is ratcheted past them.
                    let high = seq(last_ack);
                    last_ack += 2;
                    let _ = wm.fold_at_boot_with_spans(None, high, seq(last_ack - 1), &[]);
                    // O(1) EXEMPT: test oracle
                    for e in &mut episodes {
                        e.supersede(now, clean);
                    }
                    // The new process knows nothing about an open episode:
                    // its frames stay protected only by what is on disk.
                    open = None;
                    since_clean.clear();
                }
            }
            let snap = wm.snapshot();
            // O(1) EXEMPT: test oracle
            for &(lo, hi) in &foreign {
                prop_assert!(
                    snap.range_has_unapplied(lo, lo) && snap.range_has_unapplied(hi, hi),
                    "a foreign range lost a sequence: {lo}..{hi}"
                );
            }
            // O(1) EXEMPT: test oracle
            for e in &mut episodes {
                e.refresh(now, clean);
                if e.eligible {
                    continue;
                }
                // O(1) EXEMPT: test oracle
                for &f in &e.frames {
                    prop_assert!(
                        !snap.frame_is_applied(AppliedSink::Ticks, f)
                            && !snap.frame_is_applied(AppliedSink::Depth, f),
                        "frame {f} trusted before the rule held: {e:?}"
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The property test above is not vacuous: the same driver reaches a
/// confirmation, and the confirmed frames are then trusted.
#[test]
fn test_note_questdb_probe_verdict_at_drives_a_confirmation() {
    let dir = scratch("liveness");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    let parked = lag_episode(&wm, 1_000, 0);
    assert!(!applied(&wm, parked - REPLAY_REORDER_SLACK_SEQ));
    // O(1) EXEMPT: test loop
    for k in 1..=2 {
        wm.note_questdb_probe_verdict_at(ProbeVerdict::Clean, 120 * SEC + k * LAG);
    }
    // `skip_below` keeps one slack below the watermark.
    assert!(applied(&wm, parked - 2 * REPLAY_REORDER_SLACK_SEQ));
    let _ = std::fs::remove_dir_all(&dir);
}
