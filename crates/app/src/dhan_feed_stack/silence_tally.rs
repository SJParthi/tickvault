//! The 30 s silence arm's per-report tally (moved out of `scan_silence_named_at`
//! unchanged, 2026-11-17, so `dhan_feed_stack.rs` stays under its size pin).
//! O(1) per report plus the bounded `rank_silent` insertion (K = 8), no
//! allocation. Pinned against a frozen copy of the arm by
//! `proptest_scan_silence_named_matches_frozen_oracle`.

use tickvault_core::pipeline::tick_gap_detector::SilenceReport;

use super::{ClassLiveness, SilenceVerdict, SilentInstrument, WORST_SILENT_NAMED, rank_silent};

/// What one silence scan counts: silent, never-ticked, named, and the class
/// rollup.
#[derive(Debug, Default)]
pub(super) struct SilenceTally {
    pub(super) silent: u64,
    pub(super) never: u64,
    pub(super) named: usize,
    pub(super) classes: ClassLiveness,
}

impl SilenceTally {
    /// Folds one report. O(1) plus the bounded ranking insertion.
    pub(super) fn observe(
        &mut self,
        worst: &mut [SilentInstrument; WORST_SILENT_NAMED],
        report: &SilenceReport,
    ) {
        // CLASS ROLLUP — folded here, BEFORE the alarm filter below, and
        // deliberately not behind `counts_toward_alarm()`.
        //
        // That filter is `!sparse && (Exceeded | NeverTicked)`, so it
        // drops HEALTHY instruments. Reusing it as the class denominator
        // would leave a denominator of "only the troubled ones", making
        // `never == eligible` true the moment any instrument in a segment
        // had never ticked — the detector would fire on a healthy lane
        // every sweep. The rollup therefore does its own classification
        // from the raw report, and rides this sweep rather than adding a
        // second O(n) pass over the universe.
        self.classes.observe(
            report.key.1,
            report.sparse,
            report.verdict == SilenceVerdict::NeverTicked,
            report.silent_millis > report.expected_millis,
        );
        if !report.counts_toward_alarm() {
            return;
        }
        // `NeverTicked` is returned by `classify_silence` the instant an
        // instrument is seeded, with NO elapsed-time condition — it means
        // "has never ticked", full stop. Counting it raw would report
        // every instrument in the book as silent for the whole window
        // between subscribing and the first tick, which is a false alarm
        // on every single boot. `silent_millis` is time-since-seeding for
        // this verdict, so requiring it to clear the same quiet ceiling
        // the other verdicts are judged against gives the subscribe a
        // fair chance to produce something first.
        //
        // Found by a test, not by review: the first version asserted a
        // freshly-seeded instrument reports nothing and it reported (1,1).
        if report.silent_millis <= report.expected_millis {
            return;
        }
        self.silent = self.silent.saturating_add(1);
        if report.verdict == SilenceVerdict::NeverTicked {
            self.never = self.never.saturating_add(1);
        }
        self.named = rank_silent(
            worst,
            self.named,
            SilentInstrument {
                security_id: report.key.0,
                segment: report.key.1,
                silent_millis: report.silent_millis,
                expected_millis: report.expected_millis,
                never_ticked: report.verdict == SilenceVerdict::NeverTicked,
            },
        );
    }
}
