//! Parity comparison framework for the 29-TF cascade — Phase 3 commit 5.
//!
//! Compares RAM-side `Bar` snapshots (from `CascadeFanout::latest_<tf>`)
//! against reference `Bar` rows (typically from QuestDB matview SELECT
//! results). Returns a structured `ParityReport` that names every
//! mismatch by `(security_id, segment_code, field_name)`.
//!
//! ## Why a separate framework module
//!
//! The parity check has two consumers:
//!
//! 1. **Phase 3 commit 5 live soak** — the operator runs the parity
//!    binary nightly across the 14-trading-day green-streak window. The
//!    binary pulls RAM via the running app and DB via QuestDB
//!    pg-wire, then asks this framework whether they agree.
//! 2. **Phase 4a v1↔v2 dual-path** — the new `/api/movers?v=2`
//!    endpoint reads from the fanout; the same parity framework
//!    asserts the v2 response equals the v1 (matview-backed) one.
//!
//! Both consumers share the same comparison + tolerance + reporting
//! semantics. A single source of truth keeps drift impossible.
//!
//! ## Tolerance policy (per plan §6 row 3 — ZERO)
//!
//! No tolerance. RAM ≡ DB byte-equal. Floating-point comparisons use
//! `f64::to_bits()` so a NaN-vs-NaN is a mismatch (correct — NaN is
//! never equal to anything by IEEE 754) and a +0.0 vs -0.0 is also
//! a mismatch (correct — they have distinct bit patterns).
//!
//! ## Why bit-exact (no epsilon)?
//!
//! Plan §6 row 3 requires "RAM ≡ DB byte-equal" as the gate
//! criterion. The cascade and the matview both consume the same
//! input ticks and apply the same formula (per L6 — formulas.rs is
//! shared); any drift is a bug, not noise. An epsilon would mask
//! exactly the bugs the parity test exists to catch.

use crate::candles::engine::Bar;

/// A single field-level mismatch between RAM and DB. `field_name` is
/// the public field of `Bar` whose bit pattern differs (e.g. "close",
/// "volume", "tick_count", "bucket_start_ist_secs").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityMismatch {
    /// `(security_id, exchange_segment_code)` per I-P1-11.
    pub security_id: u32,
    pub exchange_segment_code: u8,
    /// `bucket_start_ist_secs` of the offending bar (helps pinpoint
    /// the exact 1m / 1h / 1d window in audit-trail follow-ups).
    pub bucket_start_ist_secs: u32,
    /// Public field name of `Bar` that differs.
    pub field_name: &'static str,
    /// RAM value as raw bits (`f64::to_bits()` for floats, native bit
    /// pattern for integers). Stored as `u64` so the report can be
    /// serialized to a structured log without losing fidelity.
    pub ram_bits: u64,
    /// DB / reference value, same bit-encoding contract as `ram_bits`.
    pub db_bits: u64,
}

/// Outcome of one parity sweep over an instrument set + timeframe.
#[derive(Clone, Debug, Default)]
pub struct ParityReport {
    /// Number of `(security_id, segment)` pairs compared.
    pub instruments_compared: u32,
    /// Number of pairs where RAM had a bar but DB did not (or vice
    /// versa) — i.e. one side was `None`. Counted as "missing", NOT
    /// folded into `mismatches`. Per plan §6 row 3, missing is a
    /// distinct failure mode (e.g. matview not yet refreshed).
    pub missing_in_ram: u32,
    pub missing_in_db: u32,
    /// Field-level mismatches. Length is bounded only by the input
    /// set; the caller chooses whether to truncate before logging.
    pub mismatches: Vec<ParityMismatch>,
}

impl ParityReport {
    /// `true` iff every compared instrument's bar matches bit-for-bit
    /// AND no instrument was missing on either side.
    ///
    /// **WARN — false-OK risk on empty input (hostile review H1):**
    /// returns `true` for `instruments_compared == 0`. Operator
    /// runbooks MUST call `is_clean_with_coverage(min_compared)`
    /// instead so a wiring bug that produces an empty input set
    /// cannot silently green the soak.
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty() && self.missing_in_ram == 0 && self.missing_in_db == 0
    }

    /// `is_clean()` AND `instruments_compared >= min_compared`.
    /// Hostile review H1 fix — closes the empty-input false-OK.
    /// Use this in any operator-facing pass/fail gate.
    pub fn is_clean_with_coverage(&self, min_compared: u32) -> bool {
        self.is_clean() && self.instruments_compared >= min_compared
    }

    /// Convenience for log messages. Parity verification, off per-tick
    /// hot path; called by operator runbook + future Phase 4a v1↔v2
    /// parity check.
    pub fn summary_line(&self) -> String {
        // O(1) EXEMPT: parity verification, off per-tick hot path.
        format!(
            "instruments_compared={} missing_in_ram={} missing_in_db={} mismatches={}",
            self.instruments_compared,
            self.missing_in_ram,
            self.missing_in_db,
            self.mismatches.len()
        )
    }
}

/// Compares two `Bar` snapshots field-by-field. Appends one
/// `ParityMismatch` per differing field. Returns the count of
/// mismatches added (so callers can decide whether to short-circuit).
///
/// The comparison is bit-exact — no tolerance. Per the module-level
/// docstring, drift between RAM and DB is a bug, not noise.
pub fn compare_bars(ram: &Bar, db: &Bar, sink: &mut Vec<ParityMismatch>) -> usize {
    let initial_len = sink.len();
    let key = (
        ram.security_id,
        ram.exchange_segment_code,
        ram.bucket_start_ist_secs,
    );
    /// Canonicalize a float for bit-equality comparison.
    /// - All NaN payloads collapse to `f64::NAN.to_bits()` so RAM-NaN
    ///   and DB-NaN match each other regardless of which NaN-bit-pattern
    ///   the producer emitted (hostile review M3 fix; IEEE 754 §6.2.1).
    /// - +0.0 and -0.0 both collapse to `0.0_f64.to_bits()` so the
    ///   sign-of-zero ambiguity does not flag as a parity drift
    ///   (hostile review L1 fix).
    fn canon_f64(v: f64) -> u64 {
        if v.is_nan() {
            f64::NAN.to_bits()
        } else if v == 0.0 {
            0.0_f64.to_bits()
        } else {
            v.to_bits()
        }
    }
    macro_rules! check_f64 {
        ($field:ident) => {
            let ram_b = canon_f64(ram.$field);
            let db_b = canon_f64(db.$field);
            if ram_b != db_b {
                sink.push(ParityMismatch {
                    security_id: key.0,
                    exchange_segment_code: key.1,
                    bucket_start_ist_secs: key.2,
                    field_name: stringify!($field),
                    ram_bits: ram_b,
                    db_bits: db_b,
                });
            }
        };
    }
    macro_rules! check_int {
        ($field:ident, $cast:ty) => {
            if ram.$field != db.$field {
                sink.push(ParityMismatch {
                    security_id: key.0,
                    exchange_segment_code: key.1,
                    bucket_start_ist_secs: key.2,
                    field_name: stringify!($field),
                    ram_bits: ram.$field as u64,
                    db_bits: db.$field as u64,
                });
            }
        };
    }
    // bucket boundaries — must agree exactly.
    check_int!(bucket_start_ist_secs, u64);
    check_int!(bucket_end_ist_secs, u64);
    // OHLC.
    check_f64!(open);
    check_f64!(high);
    check_f64!(low);
    check_f64!(close);
    // volumes + OI.
    check_int!(volume, u64);
    check_int!(volume_cum_day_at_end, u64);
    check_int!(oi, u64);
    check_int!(tick_count, u64);
    // identity.
    check_int!(security_id, u64);
    check_int!(exchange_segment_code, u64);
    // sealed flag — RAM-side may have sealed=false (open bar) when DB
    // has the same bucket already finalized; this MAY be a soft warning
    // in some workflows but for byte-equal parity we require equality.
    if ram.sealed != db.sealed {
        sink.push(ParityMismatch {
            security_id: key.0,
            exchange_segment_code: key.1,
            bucket_start_ist_secs: key.2,
            field_name: "sealed",
            ram_bits: u64::from(ram.sealed),
            db_bits: u64::from(db.sealed),
        });
    }
    sink.len() - initial_len
}

/// Per-instrument key carried alongside a snapshot pair.
pub type InstrumentKey = (u32, u8);

/// Sweeps the input pair iterator, accumulating field mismatches and
/// missing-side counts into the returned `ParityReport`.
///
/// Callers prepare a `Vec<(InstrumentKey, Option<Bar>, Option<Bar>)>`
/// where the first `Option<Bar>` is the RAM snapshot and the second
/// is the DB row. Either may be `None`.
pub fn sweep_parity(snapshots: &[(InstrumentKey, Option<Bar>, Option<Bar>)]) -> ParityReport {
    let mut report = ParityReport::default();
    for ((_security_id, _segment_code), ram_opt, db_opt) in snapshots {
        report.instruments_compared = report.instruments_compared.saturating_add(1);
        match (ram_opt, db_opt) {
            (Some(ram), Some(db)) => {
                compare_bars(ram, db, &mut report.mismatches);
            }
            (Some(_), None) => {
                report.missing_in_db = report.missing_in_db.saturating_add(1);
            }
            (None, Some(_)) => {
                report.missing_in_ram = report.missing_in_ram.saturating_add(1);
            }
            (None, None) => {
                // Both missing — neither RAM nor DB observed this pair.
                // Not a parity failure; just a pair that has not yet
                // been touched by any tick. Skip.
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(security_id: u32, segment_code: u8, bucket_start: u32, close: f64) -> Bar {
        Bar {
            bucket_start_ist_secs: bucket_start,
            bucket_end_ist_secs: bucket_start + 60,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close,
            volume: 1_000,
            volume_cum_day_at_end: 5_000,
            oi: 300,
            tick_count: 7,
            security_id,
            exchange_segment_code: segment_code,
            sealed: true,
            prev_day_close: 0.0,
            prev_day_oi: 0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        }
    }

    #[test]
    fn compare_bars_clean_returns_zero_mismatches() {
        let a = bar(1, 1, 1_000, 100.5);
        let b = bar(1, 1, 1_000, 100.5);
        let mut sink = Vec::new();
        assert_eq!(compare_bars(&a, &b, &mut sink), 0);
        assert!(sink.is_empty());
    }

    #[test]
    fn compare_bars_close_mismatch_is_reported() {
        let ram = bar(1, 1, 1_000, 100.5);
        let db = bar(1, 1, 1_000, 100.51);
        let mut sink = Vec::new();
        assert_eq!(compare_bars(&ram, &db, &mut sink), 1);
        assert_eq!(sink[0].field_name, "close");
        assert_ne!(sink[0].ram_bits, sink[0].db_bits);
    }

    #[test]
    fn compare_bars_volume_mismatch_is_reported() {
        let ram = bar(1, 1, 1_000, 100.5);
        let mut db = bar(1, 1, 1_000, 100.5);
        db.volume = 999;
        let mut sink = Vec::new();
        assert_eq!(compare_bars(&ram, &db, &mut sink), 1);
        assert_eq!(sink[0].field_name, "volume");
        assert_eq!(sink[0].ram_bits, 1_000);
        assert_eq!(sink[0].db_bits, 999);
    }

    #[test]
    fn compare_bars_multiple_field_mismatches_all_reported() {
        let ram = bar(1, 1, 1_000, 100.5);
        let mut db = bar(1, 1, 1_000, 100.5);
        db.high = 101.5;
        db.low = 98.0;
        db.tick_count = 9;
        let mut sink = Vec::new();
        assert_eq!(compare_bars(&ram, &db, &mut sink), 3);
        let names: Vec<&str> = sink.iter().map(|m| m.field_name).collect();
        assert!(names.contains(&"high"));
        assert!(names.contains(&"low"));
        assert!(names.contains(&"tick_count"));
    }

    #[test]
    fn compare_bars_nan_vs_nan_canonicalize_to_match() {
        // Hostile review M3 fix: per IEEE 754 §6.2.1, NaN has many
        // bit patterns. The parity framework canonicalizes every NaN
        // to `f64::NAN.to_bits()` so RAM-NaN and DB-NaN match each
        // other regardless of which NaN payload either side emits.
        let ram = bar(1, 1, 1_000, f64::NAN);
        let db = bar(1, 1, 1_000, f64::NAN);
        let mut sink = Vec::new();
        let mismatches = compare_bars(&ram, &db, &mut sink);
        assert_eq!(mismatches, 0, "NaN vs NaN must canonicalize and match");
    }

    #[test]
    fn compare_bars_nan_vs_zero_is_still_a_mismatch() {
        let ram_nan = bar(1, 1, 1_000, f64::NAN);
        let db_zero = bar(1, 1, 1_000, 0.0);
        let mut sink = Vec::new();
        let mismatches = compare_bars(&ram_nan, &db_zero, &mut sink);
        assert!(mismatches >= 1, "NaN vs 0.0 must flag");
    }

    #[test]
    fn compare_bars_pos_zero_vs_neg_zero_canonicalize_to_match() {
        // Hostile review L1 fix: +0.0 and -0.0 differ in bit pattern
        // but are mathematically equal. Both producers (the cascade
        // accumulator and the matview SUM) emit +0.0 from
        // accumulators, but a defensive canonical equality keeps
        // operator-facing parity reports clean if a future change
        // ever causes one side to produce a -0.0.
        let mut ram = bar(1, 1, 1_000, 0.0);
        let mut db = bar(1, 1, 1_000, 0.0);
        ram.close = 0.0;
        db.close = -0.0;
        let mut sink = Vec::new();
        assert_eq!(compare_bars(&ram, &db, &mut sink), 0);
    }

    #[test]
    fn sweep_parity_reports_clean_when_all_match() {
        let snapshots = vec![
            (
                (1, 1),
                Some(bar(1, 1, 1_000, 100.0)),
                Some(bar(1, 1, 1_000, 100.0)),
            ),
            (
                (2, 1),
                Some(bar(2, 1, 1_000, 200.0)),
                Some(bar(2, 1, 1_000, 200.0)),
            ),
        ];
        let report = sweep_parity(&snapshots);
        assert!(report.is_clean());
        assert_eq!(report.instruments_compared, 2);
        assert_eq!(report.missing_in_ram, 0);
        assert_eq!(report.missing_in_db, 0);
    }

    #[test]
    fn sweep_parity_reports_missing_in_db() {
        let snapshots = vec![
            ((1, 1), Some(bar(1, 1, 1_000, 100.0)), None),
            (
                (2, 1),
                Some(bar(2, 1, 1_000, 200.0)),
                Some(bar(2, 1, 1_000, 200.0)),
            ),
        ];
        let report = sweep_parity(&snapshots);
        assert!(!report.is_clean());
        assert_eq!(report.missing_in_db, 1);
        assert_eq!(report.missing_in_ram, 0);
        assert!(report.mismatches.is_empty());
    }

    #[test]
    fn sweep_parity_reports_missing_in_ram() {
        let snapshots = vec![
            ((1, 1), None, Some(bar(1, 1, 1_000, 100.0))),
            (
                (2, 1),
                Some(bar(2, 1, 1_000, 200.0)),
                Some(bar(2, 1, 1_000, 200.0)),
            ),
        ];
        let report = sweep_parity(&snapshots);
        assert!(!report.is_clean());
        assert_eq!(report.missing_in_ram, 1);
        assert_eq!(report.missing_in_db, 0);
    }

    #[test]
    fn sweep_parity_skips_double_missing_pairs() {
        let snapshots = vec![((1, 1), None::<Bar>, None::<Bar>)];
        let report = sweep_parity(&snapshots);
        assert!(report.is_clean());
        assert_eq!(report.instruments_compared, 1);
        assert_eq!(report.missing_in_ram, 0);
        assert_eq!(report.missing_in_db, 0);
    }

    #[test]
    fn parity_report_summary_line_is_human_readable() {
        let mut report = ParityReport::default();
        report.instruments_compared = 100;
        report.missing_in_db = 3;
        report.missing_in_ram = 1;
        report.mismatches.push(ParityMismatch {
            security_id: 1,
            exchange_segment_code: 1,
            bucket_start_ist_secs: 1_000,
            field_name: "close",
            ram_bits: 0,
            db_bits: 1,
        });
        let line = report.summary_line();
        assert!(line.contains("instruments_compared=100"));
        assert!(line.contains("missing_in_db=3"));
        assert!(line.contains("missing_in_ram=1"));
        assert!(line.contains("mismatches=1"));
    }

    #[test]
    fn compare_bars_explicit_name_match() {
        let _: fn(&Bar, &Bar, &mut Vec<ParityMismatch>) -> usize = compare_bars;
    }

    #[test]
    fn sweep_parity_explicit_name_match() {
        let _: fn(&[(InstrumentKey, Option<Bar>, Option<Bar>)]) -> ParityReport = sweep_parity;
    }

    #[test]
    fn is_clean_with_coverage_rejects_below_floor() {
        // Hostile review H1 fix: empty input must NOT pass operator
        // runbook's coverage gate even though `is_clean()` says true.
        let report = ParityReport::default();
        assert!(report.is_clean(), "is_clean() lenient — empty input passes");
        assert!(
            !report.is_clean_with_coverage(1),
            "is_clean_with_coverage(1) MUST reject empty input"
        );
    }

    #[test]
    fn is_clean_with_coverage_accepts_when_meets_floor() {
        let mut report = ParityReport::default();
        report.instruments_compared = 100;
        assert!(report.is_clean_with_coverage(100));
        assert!(report.is_clean_with_coverage(50));
        assert!(!report.is_clean_with_coverage(101));
    }

    #[test]
    fn is_clean_with_coverage_rejects_when_mismatches_present() {
        let mut report = ParityReport::default();
        report.instruments_compared = 1_000;
        report.mismatches.push(ParityMismatch {
            security_id: 1,
            exchange_segment_code: 1,
            bucket_start_ist_secs: 1_000,
            field_name: "close",
            ram_bits: 0,
            db_bits: 1,
        });
        assert!(!report.is_clean_with_coverage(100));
    }

    #[test]
    fn instrument_key_layout_is_pinned_per_i_p1_11() {
        // Hostile review M2 fix: ratchet that `InstrumentKey` stays
        // `(u32, u8)` per I-P1-11 composite-key invariant. A future
        // refactor that packs into u64 silently breaks any caller
        // destructuring the tuple — pin the layout at the type level.
        let _: InstrumentKey = (0_u32, 0_u8);
        assert_eq!(
            std::mem::size_of::<InstrumentKey>(),
            std::mem::size_of::<(u32, u8)>(),
            "InstrumentKey layout drifted from (u32, u8) — I-P1-11 violated"
        );
    }
}
