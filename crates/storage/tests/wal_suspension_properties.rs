//! Random-input attack on the WAL-suspension probe.
//!
//! This is the ONE detector for the one failure mode where every other
//! signal reports success. A WAL-suspended QuestDB table keeps ACCEPTING and
//! ACKing ILP writes while silently not applying them: `flush()` returns Ok,
//! every loss counter reads zero, and the rows are simply not there. On
//! 2026-08-25 that put FIFTEEN tables — `ticks`, `market_depth`, and every
//! candle frame — into that state, and it was found by the operator asking
//! why a table was empty.
//!
//! So a blind probe reporting health here is strictly worse than no probe at
//! all, and this module's own history is two bugs of exactly that shape:
//!
//! - a schema drift that rendered `suspended` as a string made every row skip
//!   and returned `Ok(vec![])` — a confident ZERO from a probe that had seen
//!   nothing;
//! - the partial-observation fix that followed re-latched tables which had
//!   explicitly reported HEALTHY, turning the alarm into a page that could
//!   never clear.
//!
//! Both are directional errors in a detector, and both survived hand-written
//! tests. The generator below therefore builds `/exec` bodies adversarially:
//! shuffled column order, wrong cell types, missing cells, non-array rows,
//! absent optional columns, and duplicate names.

use proptest::prelude::*;
use serde_json::{Value, json};

use tickvault_storage::wal_suspension_watcher::{
    WalProbeFailure, WalSuspensionTracker, WalTableRow, parse_wal_tables_response,
};

/// A cell as it might arrive: the right type, a plausible wrong type, or
/// absent. A QuestDB upgrade rendering `suspended` as `"true"` is the exact
/// shape that produced the confident-zero bug.
#[derive(Debug, Clone)]
struct RowSpec {
    name: Option<String>,
    suspended: Option<bool>,
    /// Render `suspended` as a string instead of a bool — the drift shape.
    suspended_as_string: bool,
    writer_txn: Option<i64>,
    sequencer_txn: Option<i64>,
    /// Emit the row as a non-array value entirely.
    malformed: bool,
}

impl RowSpec {
    /// Whether the parser should keep this row.
    fn parseable(&self) -> bool {
        !self.malformed
            && self.name.is_some()
            && self.suspended.is_some()
            && !self.suspended_as_string
    }
}

fn row_spec() -> impl Strategy<Value = RowSpec> {
    (
        prop_oneof![
            8 => "[a-z_]{1,8}".prop_map(Some),
            1 => Just(None),
        ],
        prop_oneof![8 => any::<bool>().prop_map(Some), 1 => Just(None)],
        prop_oneof![9 => Just(false), 1 => Just(true)],
        prop::option::of(0i64..1_000_000),
        prop::option::of(0i64..1_000_000),
        prop_oneof![9 => Just(false), 1 => Just(true)],
    )
        .prop_map(
            |(name, suspended, suspended_as_string, writer_txn, sequencer_txn, malformed)| {
                RowSpec {
                    name,
                    suspended,
                    suspended_as_string,
                    writer_txn,
                    sequencer_txn,
                    malformed,
                }
            },
        )
}

/// Builds an `/exec` body with the columns in the given ORDER.
///
/// Column order is the point: the parser resolves cells BY NAME precisely so
/// the live server's order and optional-column set are free to drift. A
/// parser that quietly used position would pass every fixed-order test and
/// mis-read the first day QuestDB reordered its output.
fn body_with_order(rows: &[RowSpec], order: &[&str]) -> Value {
    let columns: Vec<Value> = order.iter().map(|n| json!({ "name": n })).collect();
    let dataset: Vec<Value> = rows
        .iter()
        .map(|r| {
            if r.malformed {
                return json!("not-an-array");
            }
            let cells: Vec<Value> = order
                .iter()
                .map(|col| match *col {
                    "name" => r.name.clone().map_or(Value::Null, Value::from),
                    "suspended" => match (r.suspended, r.suspended_as_string) {
                        (Some(s), true) => Value::from(s.to_string()),
                        (Some(s), false) => Value::from(s),
                        (None, _) => Value::Null,
                    },
                    "writerTxn" => r.writer_txn.map_or(Value::Null, Value::from),
                    "sequencerTxn" => r.sequencer_txn.map_or(Value::Null, Value::from),
                    _ => Value::Null,
                })
                .collect();
            json!(cells)
        })
        .collect();
    json!({ "columns": columns, "dataset": dataset })
}

const ORDERS: [[&str; 4]; 4] = [
    ["name", "suspended", "writerTxn", "sequencerTxn"],
    ["suspended", "name", "sequencerTxn", "writerTxn"],
    ["writerTxn", "sequencerTxn", "name", "suspended"],
    ["sequencerTxn", "writerTxn", "suspended", "name"],
];

fn rows_strategy() -> impl Strategy<Value = Vec<RowSpec>> {
    prop::collection::vec(row_spec(), 0..12)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// Column ORDER must not change the answer. That is the entire reason the
    /// parser resolves by name, and a positional parser would pass every
    /// fixed-order fixture in the suite.
    #[test]
    fn the_column_order_does_not_change_what_is_parsed(
        rows in rows_strategy(),
        a in 0usize..4,
        b in 0usize..4,
    ) {
        let first = parse_wal_tables_response(&body_with_order(&rows, &ORDERS[a]));
        let second = parse_wal_tables_response(&body_with_order(&rows, &ORDERS[b]));
        match (first, second) {
            (Ok((ra, sa)), Ok((rb, sb))) => {
                prop_assert_eq!(sa, sb, "skipped count moved with column order");
                prop_assert_eq!(ra.len(), rb.len(), "row count moved with column order");
                for (x, y) in ra.iter().zip(rb.iter()) {
                    prop_assert_eq!(&x.name, &y.name);
                    prop_assert_eq!(x.suspended, y.suspended);
                    prop_assert_eq!(x.writer_txn, y.writer_txn);
                    prop_assert_eq!(x.sequencer_txn, y.sequencer_txn);
                }
            }
            (Err(ea), Err(eb)) => prop_assert_eq!(ea, eb, "different failure by column order"),
            (x, y) => prop_assert!(false, "one order parsed and the other did not: {:?} / {:?}", x, y),
        }
    }

    /// THE CONFIDENT-ZERO GUARD. Rows arrived and every one was skipped is
    /// schema drift, never an empty answer. `Ok(vec![])` here is what set the
    /// gauge to a confident zero while the probe had seen nothing.
    #[test]
    fn rows_that_all_skip_are_an_error_never_an_empty_answer(rows in rows_strategy()) {
        let got = parse_wal_tables_response(&body_with_order(&rows, &ORDERS[0]));
        let kept = rows.iter().filter(|r| r.parseable()).count();
        if !rows.is_empty() && kept == 0 {
            prop_assert_eq!(
                got.unwrap_err(),
                WalProbeFailure::AllRowsSkipped,
                "every row skipped must be reported as drift"
            );
        }
    }

    /// The skipped count is exact. It is what tells the caller the
    /// observation was PARTIAL, and a caller that trusts a wrong number
    /// concludes recovery from silence.
    #[test]
    fn the_skipped_count_is_exactly_what_was_dropped(rows in rows_strategy()) {
        let kept = rows.iter().filter(|r| r.parseable()).count();
        if rows.is_empty() || kept > 0 {
            let (out, skipped) = parse_wal_tables_response(&body_with_order(&rows, &ORDERS[0]))
                .expect("a parseable body");
            prop_assert_eq!(out.len(), kept);
            prop_assert_eq!(skipped, rows.len() - kept);
        }
    }

    /// An empty dataset is a legitimate "no WAL tables", distinct from drift.
    #[test]
    fn an_empty_dataset_is_an_empty_answer_not_an_error(a in 0usize..4) {
        let (out, skipped) = parse_wal_tables_response(&body_with_order(&[], &ORDERS[a]))
            .expect("empty dataset parses");
        prop_assert!(out.is_empty());
        prop_assert_eq!(skipped, 0);
    }

    /// A header without the mandatory columns is drift, reported as such
    /// rather than as zero suspended tables.
    #[test]
    fn a_header_missing_a_mandatory_column_is_reported(rows in rows_strategy()) {
        for missing in [
            vec!["suspended", "writerTxn"],
            vec!["name", "writerTxn"],
            vec!["writerTxn", "sequencerTxn"],
        ] {
            let body = body_with_order(&rows, &missing);
            prop_assert_eq!(
                parse_wal_tables_response(&body).unwrap_err(),
                WalProbeFailure::MissingColumn
            );
        }
    }

    /// It never panics, on any of these shapes.
    #[test]
    fn the_parser_never_panics(rows in rows_strategy(), a in 0usize..4) {
        let _ = parse_wal_tables_response(&body_with_order(&rows, &ORDERS[a]));
        let _ = parse_wal_tables_response(&json!(null));
        let _ = parse_wal_tables_response(&json!([]));
        let _ = parse_wal_tables_response(&json!({ "columns": "nope" }));
    }

    /// The gauge equals the number of suspended tables in the observation.
    /// This is the value the CloudWatch alarm reads.
    #[test]
    fn the_gauge_counts_exactly_the_suspended_tables(
        names in prop::collection::vec("[a-z]{1,6}", 0..10),
        flags in prop::collection::vec(any::<bool>(), 0..10),
    ) {
        let rows: Vec<WalTableRow> = names
            .iter()
            .zip(flags.iter())
            .map(|(n, s)| WalTableRow {
                name: n.clone(),
                suspended: *s,
                writer_txn: None,
                sequencer_txn: None,
                error_tag: None,
                error_message: None,
            })
            .collect();
        // Distinct names only — a duplicate name is one table, not two.
        let mut distinct_suspended: Vec<&str> = rows
            .iter()
            .filter(|r| r.suspended)
            .map(|r| r.name.as_str())
            .collect();
        distinct_suspended.sort_unstable();
        distinct_suspended.dedup();

        let mut tracker = WalSuspensionTracker::new();
        let delta = tracker.observe(&rows);
        prop_assert_eq!(delta.currently_suspended, distinct_suspended.len());
        prop_assert_eq!(tracker.currently_suspended(), distinct_suspended.len());
    }

    /// The rising edge fires ONCE per episode. A table suspended for hours
    /// must page once, not every poll — Rule 4, edge-triggered alerts only.
    #[test]
    fn a_table_suspended_across_many_polls_pages_once(
        name in "[a-z]{1,6}",
        polls in 2usize..8,
    ) {
        let row = WalTableRow {
            name: name.clone(),
            suspended: true,
            writer_txn: None,
            sequencer_txn: None,
            error_tag: None,
            error_message: None,
        };
        let mut tracker = WalSuspensionTracker::new();
        let mut rising = 0usize;
        for _ in 0..polls {
            rising += tracker.observe(std::slice::from_ref(&row)).newly_suspended.len();
        }
        prop_assert_eq!(rising, 1, "the latch fired {} times", rising);
    }

    /// A PARTIAL observation must not conclude recovery from silence — but a
    /// table that explicitly reports HEALTHY on that same partial poll must
    /// still be believed.
    ///
    /// Both halves matter and the second was a real regression: the first fix
    /// restored every apparently-recovered name without checking, so with
    /// drift making every poll partial the falling edge became unreachable
    /// and the gauge stuck above zero forever — a page that can never clear.
    #[test]
    fn a_partial_view_believes_a_healthy_report_and_distrusts_silence(
        present in "[a-z]{1,5}",
        absent in "[A-Z]{1,5}",
    ) {
        prop_assume!(present != absent.to_lowercase());
        let suspended_row = |n: &str| WalTableRow {
            name: n.to_string(),
            suspended: true,
            writer_txn: None,
            sequencer_txn: None,
            error_tag: None,
            error_message: None,
        };
        let healthy_row = |n: &str| WalTableRow {
            name: n.to_string(),
            suspended: false,
            writer_txn: None,
            sequencer_txn: None,
            error_tag: None,
            error_message: None,
        };

        let mut tracker = WalSuspensionTracker::new();
        // Both suspended.
        tracker.observe(&[suspended_row(&present), suspended_row(&absent)]);
        prop_assert_eq!(tracker.currently_suspended(), 2);

        // A PARTIAL poll: `present` says healthy, `absent` is simply missing.
        let delta = tracker.observe_with_completeness(&[healthy_row(&present)], false);

        prop_assert!(
            delta.recovered.contains(&present),
            "an explicit healthy report was not believed"
        );
        prop_assert_eq!(
            tracker.currently_suspended(), 1,
            "silence about {} was read as recovery", absent
        );
    }

    /// A COMPLETE observation does conclude recovery from absence — that is
    /// how a dropped table leaves the gauge.
    #[test]
    fn a_complete_view_treats_absence_as_recovery(name in "[a-z]{1,6}") {
        let mut tracker = WalSuspensionTracker::new();
        tracker.observe(&[WalTableRow {
            name: name.clone(),
            suspended: true,
            writer_txn: None,
            sequencer_txn: None,
            error_tag: None,
            error_message: None,
        }]);
        prop_assert_eq!(tracker.currently_suspended(), 1);
        let delta = tracker.observe_with_completeness(&[], true);
        prop_assert!(delta.recovered.contains(&name));
        prop_assert_eq!(tracker.currently_suspended(), 0);
    }
}
