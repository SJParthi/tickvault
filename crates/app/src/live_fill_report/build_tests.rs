//! Tests of the report build and render (plan item LF-7): fixture rows to
//! exact bytes, verdict boundaries, no-orders reasons, determinism.

use super::build::*;
use super::recheck::{Recheck, RecheckKind};
use super::render::*;
use super::rows::EventRow;
use crate::live_fill::BootOutcome;

const DAY: &str = "2026-11-12";
/// 2026-11-12 00:00 IST, IST-epoch nanos.
const D0: i64 = 1_794_441_600_000_000_000;
const NS: i64 = 1_000_000_000;
/// 10:00:00 IST.
const T10: i64 = D0 + 36_000 * NS;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/live_fill_report/2026-11-12"
);

fn ctx() -> ReportContext {
    ReportContext {
        boot: Some(BootOutcome::Running),
        process_started_ist_ns: D0 + 8 * 3_600 * NS,
        settled: true,
        truncated: false,
        catch_up: false,
    }
}

fn row(signal: i64, seq: i64, event: &str, ts: i64) -> EventRow {
    EventRow {
        ts_ns: ts,
        event: event.to_owned(),
        signal_id: signal,
        seq,
        recipe: "orb_5s".to_owned(),
        recipe_version: 3,
        security_id: 40_000 + signal,
        segment: "NSE_FNO".to_owned(),
        symbol: format!("RELIANCE-Nov2026-{}-CE", 1_400 + signal),
        side: "BUY".to_owned(),
        qty: 500,
        candle_end_ns: Some(T10),
        decision_ns: Some(T10 + 2_000_000),
        limit_paise: Some(11_000),
        ..EventRow::default()
    }
}

fn decision(signal: i64) -> EventRow {
    EventRow {
        feed_lag_us: Some(800),
        tick_paise: Some(5),
        lot_size: Some(250),
        participation_bps: Some(2_500),
        max_fill_seconds: Some(60),
        ..row(signal, 0, "decision", T10 + 2_000_000)
    }
}

fn priced(filled: u64, avg: Option<i64>, offsets: Vec<(u32, u64)>) -> Recheck {
    Recheck {
        seconds_seen: 3,
        kind: RecheckKind::Priced {
            filled_qty: filled,
            avg_paise: avg,
            model_paise: avg,
            beyond_limit_qty: 0,
            fill_offsets: offsets,
        },
    }
}

/// The fixture day: a full fill, a partial fill with its remainder
/// cancelled, a late-price skip, and a paper order the OMS refused.
fn fixture() -> (Vec<EventRow>, Vec<(i64, Recheck)>) {
    // 1: filled in full.
    let mut rows = vec![decision(1)];
    rows.push(row(1, 1, "built", T10 + 2_000_000));
    rows.push(EventRow {
        api_rtt_us: Some(3_500),
        queue_wait_us: Some(15),
        ..row(1, 2, "sent", T10 + 2_400_000_000)
    });
    rows.push(row(1, 3, "ack", T10 + 2_400_500_000));
    rows.push(EventRow {
        fill_qty: Some(500),
        avg_price_paise: Some(10_105),
        model_price_paise: Some(10_105),
        remainder_qty: Some(0),
        worse_than_model: Some("not_worse".to_owned()),
        ..row(1, 4, "fill", T10 + 2_401_000_000)
    });
    // 2: partial, remainder cancelled after the window.
    rows.push(decision(2));
    rows.push(row(2, 1, "built", T10 + 2_000_000));
    rows.push(EventRow {
        api_rtt_us: Some(4_100),
        ..row(2, 2, "sent", T10 + 61_500_000_000)
    });
    rows.push(row(2, 3, "ack", T10 + 61_500_600_000));
    rows.push(EventRow {
        fill_qty: Some(250),
        avg_price_paise: Some(10_110),
        model_price_paise: Some(10_110),
        remainder_qty: Some(250),
        ..row(2, 4, "fill", T10 + 61_501_000_000)
    });
    rows.push(EventRow {
        remainder_qty: Some(250),
        reason: Some("remainder_after_window".to_owned()),
        ..row(2, 5, "cancel", T10 + 61_502_000_000)
    });
    // 3: skipped, a late price on the decision candle.
    rows.push(row(3, 0, "decision", T10 + 2_000_000));
    rows.push(EventRow {
        reason: Some("skip:late_price".to_owned()),
        ..row(3, 1, "reject", T10 + 2_000_000)
    });
    // 4: the paper runtime refused the order: its id check runs BEFORE the
    // Sent row, so a refusal carries no Sent row (the shape the runtime
    // writes; LF-7 review finding 1).
    rows.push(decision(4));
    rows.push(row(4, 1, "built", T10 + 2_000_000));
    rows.push(EventRow {
        remainder_qty: Some(500),
        reason: Some("refused:non_paper_id".to_owned()),
        ..row(4, 2, "reject", T10 + 2_300_100_000)
    });
    let rechecks = vec![
        (1, priced(500, Some(10_105), vec![(0, 200), (1, 300)])),
        (2, priced(250, Some(10_110), vec![(0, 250)])),
        (
            3,
            Recheck::not_run(RecheckKind::Unavailable("no_pricing_inputs")),
        ),
        (4, priced(0, None, vec![])),
    ];
    (rows, rechecks)
}

fn fixture_report() -> DailyReport {
    let (rows, rechecks) = fixture();
    build_report(DAY, &rows, &rechecks, ctx())
}

/// Fixture rows to exact bytes: the JSON and all four CSVs match the
/// committed golden files. Set `LF7_UPDATE_GOLDEN=1` to rewrite them (then
/// review the diff by eye).
#[test]
fn test_build_report_render_files_fixture_rows_produce_exact_report() {
    let files = render_files(&fixture_report());
    assert_eq!(files.len(), 5);
    if std::env::var("LF7_UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(FIXTURE_DIR).unwrap();
        for (name, body) in &files {
            std::fs::write(format!("{FIXTURE_DIR}/{name}"), body).unwrap();
        }
    }
    for (name, body) in &files {
        let golden = std::fs::read_to_string(format!("{FIXTURE_DIR}/{name}"))
            .expect("golden file present (LF7_UPDATE_GOLDEN=1 writes it)");
        assert_eq!(body, &golden, "{name} differs from its golden file");
    }
}

#[test]
fn test_fixture_outcomes_counts_and_light() {
    let r = fixture_report();
    assert_eq!(r.status, "complete");
    assert_eq!(r.light, "green");
    assert_eq!(r.counts.orders, 4);
    assert_eq!(r.counts.filled, 1);
    assert_eq!(r.counts.partial_cancelled, 1);
    assert_eq!(r.counts.skipped, 1);
    assert_eq!(r.counts.rejected, 1);
    assert_eq!(r.counts.matched, 2);
    let outcomes: Vec<&str> = r.orders.iter().map(|o| o.outcome.as_str()).collect();
    assert_eq!(
        outcomes,
        [
            "filled",
            "partial_cancelled",
            "skipped:late_price",
            "rejected"
        ]
    );
    assert_eq!(r.rejects.len(), 2);
    assert_eq!(r.rejects["refused:non_paper_id"].qty, 500);
    assert!(r.rejects.contains_key("skip:late_price"));
}

#[test]
fn test_no_orders_reason_per_boot_outcome_and_no_rows_report() {
    let idle = build_report(
        DAY,
        &[],
        &[],
        ReportContext {
            boot: Some(BootOutcome::IdleNoEngine),
            ..ctx()
        },
    );
    assert_eq!(idle.status, "no_orders");
    assert_eq!(
        idle.no_orders_reason,
        Some("no approved strategy is configured")
    );
    assert_eq!(idle.light, "green");
    let ran = build_report(DAY, &[], &[], ctx());
    assert_eq!(ran.no_orders_reason, Some("strategy ran, no signals"));
    // All five files are written on a no-orders day, CSVs with their headers.
    let files = render_files(&idle);
    assert_eq!(files.len(), 5);
    assert!(files[1].1.starts_with("signal_id,"));
    assert_eq!(files[1].1.lines().count(), 1);
    assert_eq!(files[4].1, "reason,count,qty\n");
    assert!(summary_plain(&idle).contains("no approved strategy is configured"));
    for boot in [
        None,
        Some(BootOutcome::Off),
        Some(BootOutcome::RefusedNoOrderRuntime),
    ] {
        assert!(!no_orders_reason(boot).is_empty());
    }
}

/// LF-7 review round 3, finding 1: an empty day is Green only when live fill
/// ran in this process; a refused, off or never-booted live fill reads Amber
/// with `live_fill_not_running`, never Green.
#[test]
fn test_live_fill_ran_gates_a_no_orders_day_per_boot_outcome() {
    for (boot, ran) in [
        (Some(BootOutcome::Running), true),
        (Some(BootOutcome::IdleNoEngine), true),
        (Some(BootOutcome::Off), false),
        (Some(BootOutcome::RefusedNoOrderRuntime), false),
        (None, false),
    ] {
        assert_eq!(live_fill_ran(boot), ran, "{boot:?}");
        let r = build_report(DAY, &[], &[], ReportContext { boot, ..ctx() });
        if ran {
            assert_eq!(r.status, "no_orders", "{boot:?}");
            assert_eq!(r.light, "green", "{boot:?}");
            assert!(r.status_reasons.is_empty(), "{boot:?}");
        } else {
            assert_eq!(r.status, "partial", "{boot:?}");
            assert_eq!(r.light, "amber", "{boot:?}");
            assert_eq!(r.status_reasons, vec!["live_fill_not_running"], "{boot:?}");
            assert_eq!(r.no_orders_reason, Some(no_orders_reason(boot)));
        }
    }
    // A day with orders is judged on its orders, whatever this process's
    // boot was (the orders prove live fill ran when they were placed).
    let with_orders = build_report(
        DAY,
        &one_fill("BUY", 10_000),
        &[],
        ReportContext {
            boot: None,
            ..ctx()
        },
    );
    assert!(
        !with_orders
            .status_reasons
            .contains(&"live_fill_not_running")
    );
}

fn one_fill(side: &str, avg: i64) -> Vec<EventRow> {
    let mut d = decision(9);
    d.side = side.to_owned();
    let mut s = row(9, 2, "sent", T10 + NS);
    s.side = side.to_owned();
    let mut f = row(9, 3, "fill", T10 + 2 * NS);
    f.side = side.to_owned();
    f.fill_qty = Some(500);
    f.avg_price_paise = Some(avg);
    vec![d, s, f]
}

#[test]
fn test_verdict_boundaries_buy_sell_equal() {
    let check = |side: &str, stored: i64, recheck: i64| {
        let r = build_report(
            DAY,
            &one_fill(side, stored),
            &[(9, priced(500, Some(recheck), vec![(0, 500)]))],
            ctx(),
        );
        (r.orders[0].verdict, r.orders[0].gap_ticks, r.light)
    };
    assert_eq!(check("BUY", 10_000, 10_000), ("match", Some(0), "green"));
    assert_eq!(
        check("BUY", 10_005, 10_000),
        ("worse_than_worst", Some(1), "red")
    );
    assert_eq!(
        check("BUY", 9_995, 10_000),
        ("better_than_worst", Some(-1), "red")
    );
    assert_eq!(check("SELL", 10_000, 10_000), ("match", Some(0), "green"));
    assert_eq!(
        check("SELL", 9_995, 10_000),
        ("worse_than_worst", Some(1), "red")
    );
    assert_eq!(
        check("SELL", 10_005, 10_000),
        ("better_than_worst", Some(-1), "red")
    );
    // A quantity difference is a finding too.
    let r = build_report(
        DAY,
        &one_fill("BUY", 10_000),
        &[(9, priced(250, Some(10_000), vec![(0, 250)]))],
        ctx(),
    );
    assert_eq!(r.orders[0].verdict, "qty_differs");
    assert_eq!(r.light, "red");
    assert_eq!(r.outside_worst_case().len(), 1);
    // A stored `worse` verdict wins.
    let mut rows = one_fill("BUY", 10_000);
    rows[2].worse_than_model = Some("worse".to_owned());
    let r = build_report(DAY, &rows, &[(9, priced(500, Some(10_000), vec![]))], ctx());
    assert_eq!(r.orders[0].verdict, "stored_worse");
    assert!(is_red_verdict("stored_worse"));
    assert!(!is_red_verdict("match"));
}

#[test]
fn test_null_recheck_is_never_a_pass() {
    for (rechecks, verdict) in [
        (vec![], "recheck_unavailable"),
        (
            vec![(9, Recheck::not_run(RecheckKind::Skipped("order_cap")))],
            "recheck_skipped",
        ),
        (
            vec![(
                9,
                Recheck::not_run(RecheckKind::Unavailable("catch_up_rebuild")),
            )],
            "recheck_unavailable",
        ),
        (
            vec![(
                9,
                Recheck {
                    seconds_seen: 0,
                    kind: RecheckKind::Refused("no traded second".to_owned()),
                },
            )],
            "recheck_incomplete",
        ),
    ] {
        let r = build_report(DAY, &one_fill("BUY", 10_000), &rechecks, ctx());
        assert_eq!(r.orders[0].verdict, verdict);
        assert_eq!(r.light, "amber", "{verdict}");
        assert_eq!(r.counts.matched, 0);
    }
}

#[test]
fn test_orphan_decision_is_orphaned_by_restart() {
    let r = build_report(DAY, &[decision(5), row(5, 1, "built", T10)], &[], ctx());
    assert_eq!(r.orders[0].outcome, "orphaned_by_restart");
    assert_eq!(r.counts.orphaned_by_restart, 1);
    assert_eq!(r.light, "amber");
}

#[test]
fn test_truncated_and_unsettled_are_amber() {
    let (rows, rechecks) = fixture();
    for (truncated, settled, reason) in [(true, true, "truncated"), (false, false, "unsettled")] {
        let r = build_report(
            DAY,
            &rows,
            &rechecks,
            ReportContext {
                truncated,
                settled,
                ..ctx()
            },
        );
        assert_eq!(r.status, "partial");
        assert_eq!(r.light, "amber");
        assert!(r.status_reasons.contains(&reason));
        assert!(summary_plain(&r).contains(reason));
    }
}

#[test]
fn test_restarted_process_is_amber() {
    let (rows, rechecks) = fixture();
    let r = build_report(
        DAY,
        &rows,
        &rechecks,
        ReportContext {
            process_started_ist_ns: D0 + 10 * 3_600 * NS,
            ..ctx()
        },
    );
    assert_eq!(r.light, "amber");
    assert!(r.status_reasons.contains(&"process_started_after_0900"));
    let catch_up = build_report(
        DAY,
        &rows,
        &rechecks,
        ReportContext {
            catch_up: true,
            ..ctx()
        },
    );
    assert!(catch_up.status_reasons.contains(&"catch_up_rebuild"));
}

#[test]
fn test_latency_stats_negative_excluded_and_counted() {
    let s = latency_stats("x", vec![5, -3, 1, -1, 9]);
    assert_eq!(s.n, 3);
    assert_eq!(s.negative, 2);
    assert_eq!(s.p50, Some(5));
    assert_eq!(s.max, Some(9));
    let empty = latency_stats("y", vec![-1]);
    assert_eq!((empty.n, empty.p99, empty.max), (0, None, None));
}

#[test]
fn test_fill_second_histogram_sums_to_filled_qty() {
    let r = fixture_report();
    let filled: u64 = r.fill_seconds[..r.fill_seconds.len() - 1].iter().sum();
    assert_eq!(filled, 750);
    assert_eq!(r.fill_seconds[0], 450);
    assert_eq!(r.fill_seconds[1], 300);
    // The unfilled bucket: order 2's 250 (order 4 was rejected, not priced).
    assert_eq!(r.fill_seconds[FILL_SECONDS_BUCKETS - 1], 250);
}

#[test]
fn test_render_csvs_and_csv_field_escape_comma_quote_newline() {
    assert_eq!(csv_field("plain"), "plain");
    assert_eq!(csv_field("a,b"), "\"a,b\"");
    assert_eq!(csv_field("say \"x\""), "\"say \"\"x\"\"\"");
    assert_eq!(csv_field("a\nb"), "\"a\nb\"");
    assert_eq!(csv_field("a\rb"), "\"a\rb\"");
    let mut rows = one_fill("BUY", 10_000);
    for r in &mut rows {
        r.symbol = "X,\"Y\"".to_owned();
    }
    let report = build_report(DAY, &rows, &[], ctx());
    let [orders, ..] = render_csvs(&report);
    assert!(orders.contains(",\"X,\"\"Y\"\"\","), "{orders}");
}

#[test]
fn test_ist_text_and_rows_day_start() {
    assert_eq!(
        ist_text(Some(T10 + 1_500_000)),
        "2026-11-12 10:00:00.001500"
    );
    assert_eq!(ist_text(None), "");
    assert_eq!(rows_day_start(DAY), Some(D0));
    assert_eq!(rows_day_start("nope"), None);
    assert_eq!(report_file_names(DAY)[0], "report-2026-11-12.json");
}

proptest::proptest! {
    /// Nearest rank equals the sorted reference `sorted[ceil(p·n) − 1]`,
    /// including n = 0, 1, 2, all-equal and i64 extremes.
    #[test]
    fn test_percentile_nearest_rank_matches_sort_reference(
        mut v in proptest::collection::vec(
            proptest::prop_oneof![
                proptest::num::i64::ANY,
                proptest::strategy::Just(0i64),
                proptest::strategy::Just(i64::MAX),
                proptest::strategy::Just(i64::MIN),
            ],
            0..200,
        ),
        pct in 1u64..=100,
    ) {
        v.sort_unstable();
        let got = nearest_rank(&v, pct);
        if v.is_empty() {
            proptest::prop_assert_eq!(got, None);
        } else {
            // Exact integer ceil(p·n/100): the earlier f64 reference read
            // 0.68 × 150 as 102.00000000000001 and ceiled it to 103 (the
            // saved regression case), a reference error, not a code one.
            let n = v.len() as u128;
            let idx = (u128::from(pct) * n).div_ceil(100);
            let idx = usize::try_from(idx.max(1)).unwrap();
            proptest::prop_assert_eq!(got, Some(v[idx - 1]));
        }
    }

    /// The same rows and re-pricings in any order give identical bytes.
    #[test]
    fn test_report_bytes_deterministic(seed in proptest::num::u64::ANY) {
        let (mut rows, mut rechecks) = fixture();
        let n = rows.len();
        for i in 0..n {
            let j = ((seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(i as u64)) as usize) % n;
            rows.swap(i, j);
        }
        rechecks.reverse();
        let a = render_files(&fixture_report());
        let b = render_files(&build_report(DAY, &rows, &rechecks, ctx()));
        proptest::prop_assert_eq!(a, b);
    }
}

#[test]
fn test_outside_worst_case_and_is_red_verdict() {
    let clean = fixture_report();
    assert!(clean.outside_worst_case().is_empty());
    let r = build_report(
        DAY,
        &one_fill("BUY", 10_005),
        &[(9, priced(500, Some(10_000), vec![(0, 500)]))],
        ctx(),
    );
    let outside = r.outside_worst_case();
    assert_eq!(outside.len(), 1);
    assert_eq!(outside[0].signal_id, 9);
    for v in [
        "worse_than_worst",
        "better_than_worst",
        "stored_worse",
        "qty_differs",
    ] {
        assert!(is_red_verdict(v), "{v}");
    }
    for v in ["match", "recheck_unavailable", "not_priced"] {
        assert!(!is_red_verdict(v), "{v}");
    }
}

#[test]
fn test_render_json_report_file_names_and_summary_plain() {
    let r = fixture_report();
    let json = render_json(&r);
    assert!(json.ends_with("}\n"));
    let back: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(back["light"], "green");
    assert_eq!(back["counts"]["orders"], 4);
    let names = report_file_names(DAY);
    assert_eq!(
        names,
        [
            "report-2026-11-12.json",
            "orders-2026-11-12.csv",
            "fill-seconds-2026-11-12.csv",
            "latency-2026-11-12.csv",
            "rejects-2026-11-12.csv",
        ]
    );
    let line = summary_plain(&r);
    assert!(
        line.starts_with("live paper fill report 2026-11-12: green (complete), 4 orders"),
        "{line}"
    );
    assert!(line.contains("2 match"), "{line}");
}

/// Review finding 1 (LF-7): the runtime cuts an exit to the paper position
/// the risk engine holds, so its Sent and Fill rows carry 250 shares where
/// the Decision asked for 500. The clamped order filled in full: not red, not
/// `qty_differs`, outcome `filled`, no remainder.
#[test]
fn test_build_report_exit_clamped_below_decision_qty_is_not_red() {
    let mut d = decision(9);
    d.side = "SELL".to_owned();
    let mut s = row(9, 2, "sent", T10 + NS);
    s.side = "SELL".to_owned();
    s.qty = 250;
    let mut f = row(9, 3, "fill", T10 + 2 * NS);
    f.side = "SELL".to_owned();
    f.qty = 250;
    f.fill_qty = Some(250);
    f.avg_price_paise = Some(10_000);
    let r = build_report(
        DAY,
        &[d, s, f],
        &[(9, priced(500, Some(10_000), vec![(0, 500)]))],
        ctx(),
    );
    let o = &r.orders[0];
    assert_eq!(o.verdict, "match", "{o:?}");
    assert_eq!(o.outcome, "filled");
    assert_eq!(o.qty, 250);
    assert_eq!(o.remainder, 0);
    assert_eq!(o.recheck_qty, Some(250));
    assert_eq!(r.light, "green");
    assert_eq!(r.counts.qty_differs, 0);
    assert!(r.outside_worst_case().is_empty());
    // A clamped order the re-pricing fills less than the cut quantity is
    // still compared on its own quantity.
    let mut rows = vec![decision(10), row(10, 2, "sent", T10 + NS)];
    rows[1].qty = 250;
    let mut part = row(10, 3, "fill", T10 + 2 * NS);
    part.fill_qty = Some(200);
    part.avg_price_paise = Some(10_000);
    rows.push(part);
    let r = build_report(
        DAY,
        &rows,
        &[(10, priced(200, Some(10_000), vec![(0, 200)]))],
        ctx(),
    );
    assert_eq!(r.orders[0].verdict, "match");
    assert_eq!(r.orders[0].outcome, "partial_cancelled");
    assert_eq!(r.orders[0].remainder, 50);
}

/// Review finding 2 (LF-7): a process started after the session (an
/// evening-drain restart at 15:52) lost nothing held in RAM, and a catch-up
/// rebuild on a later day reads only `catch_up_rebuild`.
#[test]
fn test_started_inside_session_only_flags_a_start_inside_the_session() {
    assert!(started_inside_session(DAY, D0 + 10 * 3_600 * NS));
    assert!(!started_inside_session(DAY, D0 + 8 * 3_600 * NS));
    assert!(!started_inside_session(DAY, D0 + 9 * 3_600 * NS));
    assert!(!started_inside_session(DAY, D0 + 56_400 * NS));
    assert!(!started_inside_session(
        DAY,
        D0 + (15 * 3_600 + 52 * 60) * NS
    ));
    assert!(!started_inside_session(
        DAY,
        D0 + 86_400 * NS + 10 * 3_600 * NS
    ));
    assert!(!started_inside_session("bad", D0 + 10 * 3_600 * NS));
    let (rows, rechecks) = fixture();
    let evening = build_report(
        DAY,
        &rows,
        &rechecks,
        ReportContext {
            process_started_ist_ns: D0 + (15 * 3_600 + 52 * 60) * NS,
            ..ctx()
        },
    );
    assert_eq!(evening.status, "complete");
    assert_eq!(evening.light, "green");
    assert!(evening.status_reasons.is_empty());
    let catch_up = build_report(
        DAY,
        &rows,
        &rechecks,
        ReportContext {
            process_started_ist_ns: D0 + 86_400 * NS + 15 * 3_600 * NS,
            catch_up: true,
            ..ctx()
        },
    );
    assert_eq!(catch_up.status_reasons, vec!["catch_up_rebuild"]);
}

/// A placed order the runtime refused: Decision, Built, then a Reject with
/// `reason` and no Sent row (every runtime refusal before `place_order`
/// returns a paper id writes this shape).
fn runtime_refused(signal: i64, reason: &str) -> Vec<EventRow> {
    vec![
        decision(signal),
        row(signal, 1, "built", T10 + 2_000_000),
        EventRow {
            remainder_qty: Some(500),
            reason: Some(reason.to_owned()),
            ..row(signal, 2, "reject", T10 + 2_300_000_000)
        },
    ]
}

/// A chain skip: Decision (quantity 0) and a Reject with `reason`, no Built.
fn chain_skipped(signal: i64, reason: &str) -> Vec<EventRow> {
    vec![
        EventRow {
            qty: 0,
            ..row(signal, 0, "decision", T10 + 2_000_000)
        },
        EventRow {
            reason: Some(reason.to_owned()),
            ..row(signal, 1, "reject", T10 + 2_000_000)
        },
    ]
}

/// LF-7 review finding 1: a refusal by the risk engine, the OMS or the
/// runtime's own checks reads `rejected` although no Sent row exists; the
/// chain's own skips read `skipped`. `exit_without_position` is the chain's
/// skip without a Built row and the runtime's refusal with one.
#[test]
fn test_runtime_refusals_are_rejected_and_chain_skips_are_skipped() {
    let mut rows = Vec::new();
    for (sid, reason) in [
        (1, "risk:DailyLossBreached"),
        (2, "place_refused:rate limited"),
        (3, "place_refused:rate limited;retries_exhausted"),
        (4, "place_refused:order book full;retry_queue_full"),
        (5, "refused:not_dry_run"),
        (6, "refused:non_paper_id"),
        (7, "exit_without_position"),
    ] {
        rows.extend(runtime_refused(sid, reason));
    }
    for (sid, reason) in [
        (11, "exit_without_position"),
        (12, "held_orders_full"),
        (13, "first_bar_partial"),
        (14, "skip:late_price"),
    ] {
        rows.extend(chain_skipped(sid, reason));
    }
    let r = build_report(DAY, &rows, &[], ctx());
    assert_eq!(r.counts.rejected, 7, "{:?}", r.counts);
    assert_eq!(r.counts.skipped, 4, "{:?}", r.counts);
    for o in &r.orders {
        if o.signal_id < 10 {
            assert_eq!(o.outcome, "rejected", "signal {}", o.signal_id);
        } else {
            assert!(o.outcome.starts_with("skipped:"), "{}", o.outcome);
        }
        assert_eq!(o.verdict, "not_priced");
    }
    assert_eq!(r.rejects["risk:DailyLossBreached"].count, 1);
    assert_eq!(r.rejects["place_refused:rate limited"].count, 2);
    assert_eq!(r.rejects["exit_without_position"].count, 2);
    // None of them is re-priced.
    assert!(priced_signal_ids(&rows).is_empty());
}

/// LF-7 review finding 2: a fill-model refusal is compared with the stored
/// seconds as an order that filled nothing. Stored data that would have
/// filled it is a red `qty_differs`; a stored refusal too is a match; a
/// missing re-pricing is amber. A late-price or tainted reject stays
/// unpriced.
#[test]
fn test_fill_model_refused_is_priced_against_the_stored_seconds() {
    let refused = runtime_refused(21, "fill_model_refused:no_traded_second");
    assert_eq!(
        priced_signal_ids(&refused).into_iter().collect::<Vec<_>>(),
        [21]
    );
    let check = |recheck: Option<Recheck>| {
        let checks: Vec<(i64, Recheck)> = recheck.into_iter().map(|r| (21, r)).collect();
        let r = build_report(DAY, &refused, &checks, ctx());
        assert_eq!(
            r.orders[0].outcome,
            "skipped:fill_model_refused:no_traded_second"
        );
        (r.orders[0].verdict, r.light)
    };
    assert_eq!(
        check(Some(priced(250, Some(10_000), vec![(1, 250)]))),
        ("qty_differs", "red")
    );
    assert_eq!(check(Some(priced(0, None, vec![]))), ("match", "green"));
    assert_eq!(
        check(Some(Recheck {
            seconds_seen: 0,
            kind: RecheckKind::Refused("no_traded_second".to_owned()),
        })),
        ("match", "green")
    );
    assert_eq!(check(None), ("recheck_unavailable", "amber"));
    for reason in ["skip:late_price", "feed_gap"] {
        let rows = runtime_refused(22, reason);
        let r = build_report(DAY, &rows, &[], ctx());
        assert_eq!(r.orders[0].verdict, "not_priced", "{reason}");
        assert!(priced_signal_ids(&rows).is_empty(), "{reason}");
    }
}
