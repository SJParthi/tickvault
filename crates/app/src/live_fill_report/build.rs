//! Builds one day's calibration report from the stored event rows and the
//! re-pricings (plan item LF-7). Pure: no clock, no I/O, so the same inputs
//! always give the same bytes (the input order does not matter either).
//!
//! Cold, once a day: one sort of the rows (O(rows log rows)), one pass to
//! group them, one binary search per order for its re-pricing, and one sort
//! per latency series (O(k log k), k ≤ rows). Flagged: not O(1).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::recheck::{FILL_OFFSET_BUCKETS, Recheck, RecheckKind};
use super::rows::EventRow;
use crate::live_fill::BootOutcome;
use tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const NANOS_PER_DAY: i64 = 86_400 * NANOS_PER_SEC;
/// 09:00 IST, seconds of day: a process started later held no order in RAM
/// from the session's start.
const SESSION_START_SECS_OF_DAY: i64 = 9 * 3_600;

/// Whether this process started inside the reported day's session
/// (09:00 to the 15:40 persist end, IST), so orders held in RAM before it
/// may have been lost to a restart. A start after the session (an
/// evening-drain restart) or on a later day (a catch-up rebuild) lost
/// nothing held in the session. Pure, O(1).
#[must_use]
pub fn started_inside_session(day: &str, process_started_ist_ns: i64) -> bool {
    rows_day_start(day).is_some_and(|d| {
        let open = d + SESSION_START_SECS_OF_DAY * NANOS_PER_SEC;
        let end = d + i64::from(TICK_PERSIST_END_SECS_OF_DAY_IST) * NANOS_PER_SEC;
        process_started_ist_ns > open && process_started_ist_ns < end
    })
}

/// Fill-second histogram buckets: offsets 0..=60, then the unfilled shares.
pub const FILL_SECONDS_BUCKETS: usize = FILL_OFFSET_BUCKETS + 1;

/// The latency series, in report order.
pub const LATENCY_SERIES: [&str; 7] = [
    "decision_to_built_us",
    "built_to_sent_us",
    "sent_to_ack_us",
    "decision_to_first_fill_us",
    "queue_wait_us",
    "feed_lag_us",
    "api_rtt_us",
];

/// LIVE-FILL-02 order lines logged per report before the one total line.
pub const OUTSIDE_WORST_CASE_LOG_CAP: usize = 50;

/// What the report knows beyond the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportContext {
    /// This process's live fill boot outcome (`None` before boot ran).
    pub boot: Option<BootOutcome>,
    /// This process's start, IST epoch nanos.
    pub process_started_ist_ns: i64,
    /// The readiness barriers held (rows and seconds applied, spill empty).
    pub settled: bool,
    /// The day held more rows than the read takes.
    pub truncated: bool,
    /// Rebuilt on a later day (the stored seconds are usually gone).
    pub catch_up: bool,
}

/// Every count of the day.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub orders: u64,
    pub filled: u64,
    pub partial_cancelled: u64,
    pub unfilled: u64,
    pub skipped: u64,
    pub rejected: u64,
    pub orphaned_by_restart: u64,
    pub matched: u64,
    pub worse_than_worst: u64,
    pub better_than_worst: u64,
    pub stored_worse: u64,
    pub qty_differs: u64,
    pub recheck_incomplete: u64,
    pub recheck_unavailable: u64,
    pub recheck_skipped: u64,
}

/// One would-be order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderLine {
    pub signal_id: i64,
    pub security_id: i64,
    pub segment: String,
    pub symbol: String,
    pub recipe: String,
    pub version: i64,
    pub side: String,
    /// The quantity sent: the Decision quantity, or less for an exit the
    /// runtime cut to the paper position held (the Sent row's quantity).
    pub qty: i64,
    pub candle_end: String,
    pub decision_ts: String,
    pub limit_paise: Option<i64>,
    pub fill_qty: i64,
    pub avg_paise: Option<i64>,
    pub model_paise: Option<i64>,
    pub recheck_qty: Option<u64>,
    pub recheck_avg_paise: Option<i64>,
    /// Stored average minus the re-priced one, in ticks against us
    /// (positive = the paper fill was worse), truncated toward zero.
    pub gap_ticks: Option<i64>,
    pub remainder: i64,
    pub outcome: String,
    pub verdict: &'static str,
}

/// One latency series, microseconds, nearest rank.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatencyStats {
    pub series: &'static str,
    pub n: u64,
    pub p50: Option<i64>,
    pub p90: Option<i64>,
    pub p99: Option<i64>,
    pub max: Option<i64>,
    /// Samples below zero (a clock step), left out of the figures.
    pub negative: u64,
}

/// One reject reason's total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RejectTotal {
    pub count: u64,
    /// Shares the rejects left unfilled.
    pub qty: i64,
}

/// One day's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DailyReport {
    pub day: String,
    /// `complete` / `partial` / `no_orders`.
    pub status: &'static str,
    /// Why the status is not `complete` (empty when it is).
    pub status_reasons: Vec<&'static str>,
    /// `green` / `amber` / `red`.
    pub light: &'static str,
    /// Why there were no orders (only on a `no_orders` day).
    pub no_orders_reason: Option<&'static str>,
    pub fill_seconds_source: &'static str,
    pub counts: Counts,
    pub orders: Vec<OrderLine>,
    /// Shares per fill-second offset 0..=60, then the unfilled shares, from
    /// the re-pricing.
    pub fill_seconds: Vec<u64>,
    pub latency: Vec<LatencyStats>,
    pub rejects: BTreeMap<String, RejectTotal>,
}

impl DailyReport {
    /// Order lines a LIVE-FILL-02 line is logged for.
    #[must_use]
    pub fn outside_worst_case(&self) -> Vec<&OrderLine> {
        self.orders
            .iter()
            .filter(|o| is_red_verdict(o.verdict))
            .collect()
    }
}

/// Whether a verdict turns the light red. O(1).
#[must_use]
pub fn is_red_verdict(v: &str) -> bool {
    matches!(
        v,
        "worse_than_worst" | "better_than_worst" | "stored_worse" | "qty_differs"
    )
}

/// IST epoch nanos as report text (`2026-11-12 10:00:05.123456`, IST). Pure.
#[must_use]
pub fn ist_text(ns: Option<i64>) -> String {
    ns.and_then(|n| {
        chrono::DateTime::from_timestamp(n.div_euclid(NANOS_PER_SEC), 0).map(|d| {
            let micros = n.rem_euclid(NANOS_PER_SEC) / 1_000;
            format!("{}.{micros:06}", d.format("%Y-%m-%d %H:%M:%S"))
        })
    })
    .unwrap_or_default()
}

/// Nearest-rank percentile of a SORTED slice: index `ceil(pct·n/100) − 1`.
/// `None` when empty. O(1).
#[must_use]
pub fn nearest_rank(sorted: &[i64], pct: u64) -> Option<i64> {
    let n = sorted.len() as u64;
    if n == 0 {
        return None;
    }
    let rank = (pct.saturating_mul(n)).div_ceil(100).max(1);
    sorted
        .get(usize::try_from(rank - 1).unwrap_or(usize::MAX))
        .copied()
}

/// Stats of one series: negatives counted and left out, then nearest rank.
/// O(k log k).
#[must_use]
pub fn latency_stats(series: &'static str, mut samples: Vec<i64>) -> LatencyStats {
    let before = samples.len();
    samples.retain(|v| *v >= 0);
    let negative = (before - samples.len()) as u64;
    samples.sort_unstable();
    LatencyStats {
        series,
        n: samples.len() as u64,
        p50: nearest_rank(&samples, 50),
        p90: nearest_rank(&samples, 90),
        p99: nearest_rank(&samples, 99),
        max: samples.last().copied(),
        negative,
    }
}

/// The reject reason's group: the text before the first `;`. O(len).
fn reject_key(reason: Option<&str>) -> String {
    reason
        .map(|r| r.split(';').next().unwrap_or(r))
        .filter(|r| !r.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

/// Whether live fill was running in this process (an engine or idle with no
/// approved strategy), so an empty day can be trusted as a quiet day. O(1).
#[must_use]
pub const fn live_fill_ran(boot: Option<BootOutcome>) -> bool {
    matches!(boot, Some(BootOutcome::Running | BootOutcome::IdleNoEngine))
}

/// Why a day had no orders, from the boot outcome. O(1).
#[must_use]
pub const fn no_orders_reason(boot: Option<BootOutcome>) -> &'static str {
    match boot {
        Some(BootOutcome::IdleNoEngine) => "no approved strategy is configured",
        Some(BootOutcome::Running) => "strategy ran, no signals",
        Some(BootOutcome::Off) => "live fill is off",
        Some(BootOutcome::RefusedNoOrderRuntime) => "live fill could not start (order runtime off)",
        None => "unknown (live fill did not boot in this process)",
    }
}

/// The verdict of one order against its re-pricing. A missing re-pricing is
/// never a pass. O(1).
fn verdict_of(
    side_buy: bool,
    tick: Option<i64>,
    order_qty: i64,
    fill_qty: i64,
    avg: Option<i64>,
    recheck: Option<&Recheck>,
) -> (&'static str, Option<u64>, Option<i64>, Option<i64>) {
    let Some(r) = recheck else {
        return ("recheck_unavailable", None, None, None);
    };
    match &r.kind {
        RecheckKind::Skipped(_) => ("recheck_skipped", None, None, None),
        RecheckKind::Unavailable(_) => ("recheck_unavailable", None, None, None),
        _ if fill_qty > 0 && r.seconds_seen == 0 => ("recheck_incomplete", None, None, None),
        RecheckKind::Refused(_) => {
            if fill_qty == 0 {
                ("match", Some(0), None, None)
            } else {
                ("qty_differs", Some(0), None, None)
            }
        }
        RecheckKind::Priced {
            filled_qty,
            avg_paise,
            ..
        } => {
            // The re-pricing prices the Decision quantity; an exit the
            // runtime cut to the held position can fill at most the cut
            // quantity (`order_qty`), so the check is against that.
            let cut = (*filled_qty).min(u64::try_from(order_qty).unwrap_or(0));
            let rq = Some(cut);
            if i64::try_from(cut).ok() != Some(fill_qty) {
                return ("qty_differs", rq, *avg_paise, None);
            }
            match (avg, *avg_paise) {
                (None, None) => ("match", rq, None, None),
                (Some(a), Some(b)) => {
                    let gap = if side_buy { a - b } else { b - a };
                    let ticks = tick.filter(|t| *t > 0).map(|t| gap / t);
                    let v = if gap == 0 {
                        "match"
                    } else if gap > 0 {
                        "worse_than_worst"
                    } else {
                        "better_than_worst"
                    };
                    (v, rq, Some(b), ticks)
                }
                _ => ("qty_differs", rq, *avg_paise, None),
            }
        }
    }
}

/// Reject reasons the paper order runtime writes: the chain handed the order
/// on and the risk engine, the OMS (place, cancel, its paper-id check) or the
/// runtime's own checks refused it. A given-up exit retry is
/// `place_refused:<err>;retries_exhausted` (or `;retry_queue_full`), under
/// the same prefix. Every other Reject reason is the chain's own skip.
pub const RUNTIME_REJECT_PREFIXES: [&str; 5] = [
    "risk:",
    "place_refused:",
    "refused:",
    "cancel_refused:",
    "fill_not_applied",
];

/// The chain's Reject reason prefix when the fill model refused a held order
/// (no traded second in the window, for example). Re-priced (LF-7 review
/// finding 2): the stored seconds may hold trades the chain never saw.
pub const FILL_MODEL_REFUSED_PREFIX: &str = "fill_model_refused";

/// Whether a Reject row came from the paper order runtime rather than from
/// the chain (LF-7 review finding 1). `exit_without_position` is written by
/// both: the chain's comes with no Built row (refused before it held the
/// order), the runtime's after one (the position it would close is gone).
/// O(prefixes).
fn runtime_reject(reason: Option<&str>, built: bool) -> bool {
    let r = reason.unwrap_or("");
    RUNTIME_REJECT_PREFIXES.iter().any(|p| r.starts_with(p))
        || (built && r == "exit_without_position")
}

/// The rows sorted the way the report groups them: by signal, then seq,
/// time and event. O(rows log rows).
fn sorted_rows(rows: &[EventRow]) -> Vec<&EventRow> {
    let mut sorted: Vec<&EventRow> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        (a.signal_id, a.seq, a.ts_ns, a.event.as_str()).cmp(&(
            b.signal_id,
            b.seq,
            b.ts_ns,
            b.event.as_str(),
        ))
    });
    sorted
}

/// One order's outcome, from its rows (sorted as [`sorted_rows`] sorts).
struct Classified {
    outcome: String,
    /// The report compares this order with its re-pricing.
    priced: bool,
    /// The quantity the box sent (an exit cut to the held position).
    order_qty: i64,
    fill_qty: i64,
    stored_worse: bool,
}

/// Classifies one order's rows. O(rows of the order).
fn classify(group: &[&EventRow]) -> Classified {
    let find = |ev: &str| group.iter().find(|r| r.event == ev).copied();
    let head = find("decision").unwrap_or(group[0]);
    let sent = find("sent");
    let reject = find("reject");
    // LF-7 review finding 1 (first round): the runtime cuts an exit to the
    // paper position the risk engine holds (`clamp_exit`), and every row from
    // Sent on carries the cut quantity. The order the box sent is that one,
    // so the outcome, the remainder and the fill check use it.
    let order_qty = sent
        .map(|s| s.qty)
        .filter(|q| *q > 0 && *q < head.qty)
        .unwrap_or(head.qty);
    let fill_qty: i64 = group
        .iter()
        .filter(|r| r.event == "fill")
        .filter_map(|r| r.fill_qty)
        .sum();
    let stored_worse = group
        .iter()
        .any(|r| r.worse_than_model.as_deref() == Some("worse"));
    let mut model_refused = false;
    let outcome = if fill_qty > 0 {
        if fill_qty < order_qty {
            "partial_cancelled".to_owned()
        } else {
            "filled".to_owned()
        }
    } else if let Some(r) = reject {
        let why = r.reason.as_deref();
        if sent.is_some() || runtime_reject(why, find("built").is_some()) {
            "rejected".to_owned()
        } else {
            let why = why.unwrap_or("unknown");
            model_refused = why.starts_with(FILL_MODEL_REFUSED_PREFIX);
            format!("skipped:{}", why.strip_prefix("skip:").unwrap_or(why))
        }
    } else if find("cancel").is_some() {
        "unfilled".to_owned()
    } else {
        "orphaned_by_restart".to_owned()
    };
    // LF-7 review finding 2: a fill-model refusal is a priced outcome with
    // nothing filled, so a stored re-pricing that fills reads `qty_differs`.
    let priced = model_refused
        || matches!(
            outcome.as_str(),
            "filled" | "partial_cancelled" | "unfilled"
        );
    Classified {
        outcome,
        priced,
        order_qty,
        fill_qty,
        stored_worse,
    }
}

/// The signals the report compares with a re-pricing: priced orders whose
/// stored rows are not already `worse`. The re-pricing reads only these
/// (LF-7 review finding 3), so unplaced refusals, skips and runtime
/// rejections never use up the order cap or the wall budget.
/// O(rows log rows).
#[must_use]
pub fn priced_signal_ids(rows: &[EventRow]) -> BTreeSet<i64> {
    let sorted = sorted_rows(rows);
    let mut out = BTreeSet::new();
    for group in sorted.chunk_by(|a, b| a.signal_id == b.signal_id) {
        let c = classify(group);
        if c.priced && !c.stored_worse {
            out.insert(group[0].signal_id);
        }
    }
    out
}

/// Every order the chain HELD (a Built row), by signal id, and whether its
/// share of the instrument's seconds is known (LF-9 finding 3): `true` when
/// the chain worked its fill out (an Execute reached the runtime, or the fill
/// model refused, or the runtime's channel was full), `false` when it was
/// finished unpriced (a taint or a late price refused it after it had already
/// taken part of a shared second) or its outcome is missing (a restart). The
/// re-pricing shares each instrument's seconds out in this order and reads an
/// order whose share depends on an unknown one as unavailable.
/// O(rows log rows).
///
/// LF-9 review (second pass): an order whose Built row was dropped (the
/// event channel and the outbox drop rows independently) is still held when
/// any later row shows it was: a finishing Reject (`seq` 2; a refusal before
/// the hold is `seq` 1) or an Execute's runtime row. Before, such an order was
/// left out of the walk and the later overlapping orders were re-priced as
/// if it took no share of their seconds.
#[must_use]
pub fn held_share_known(rows: &[EventRow]) -> BTreeMap<i64, bool> {
    let sorted = sorted_rows(rows);
    let mut out = BTreeMap::new();
    for group in sorted.chunk_by(|a, b| a.signal_id == b.signal_id) {
        let built = group.iter().any(|r| {
            r.event == "built"
                || (r.event == "reject" && r.seq >= 2)
                || matches!(r.event.as_str(), "sent" | "ack" | "fill" | "cancel")
        });
        if !built {
            continue;
        }
        let worked_out = group
            .iter()
            .any(|r| matches!(r.event.as_str(), "sent" | "ack" | "fill" | "cancel"))
            || group.iter().any(|r| {
                r.event == "reject"
                    && r.reason.as_deref().is_some_and(|why| {
                        runtime_reject(Some(why), true)
                            || why.starts_with(FILL_MODEL_REFUSED_PREFIX)
                            || why == crate::live_fill::PAPER_QUEUE_FULL_REASON
                    })
            });
        out.insert(group[0].signal_id, worked_out);
    }
    out
}

/// Builds the report. See the module docs for the cost. Pure.
#[must_use]
pub fn build_report(
    day: &str,
    rows: &[EventRow],
    rechecks: &[(i64, Recheck)],
    ctx: ReportContext,
) -> DailyReport {
    let sorted = sorted_rows(rows);
    let mut checks: Vec<&(i64, Recheck)> = rechecks.iter().collect();
    checks.sort_by_key(|(s, _)| *s);
    let mut counts = Counts::default();
    let mut orders = Vec::new();
    let mut fill_seconds = vec![0u64; FILL_SECONDS_BUCKETS];
    let mut series: [Vec<i64>; 7] = Default::default();
    let mut rejects: BTreeMap<String, RejectTotal> = BTreeMap::new();
    for group in sorted.chunk_by(|a, b| a.signal_id == b.signal_id) {
        let sid = group[0].signal_id;
        let find = |ev: &str| group.iter().find(|r| r.event == ev).copied();
        let head = find("decision").unwrap_or(group[0]);
        let built = find("built");
        let sent = find("sent");
        let ack = find("ack");
        let fills: Vec<&EventRow> = group
            .iter()
            .filter(|r| r.event == "fill")
            .copied()
            .collect();
        let reject = find("reject");
        let Classified {
            outcome,
            priced,
            order_qty,
            fill_qty,
            stored_worse,
        } = classify(group);
        let avg = fills.iter().rev().find_map(|f| f.avg_price_paise);
        let model = group.iter().find_map(|r| r.model_price_paise);
        counts.orders += 1;
        match outcome.as_str() {
            "filled" => counts.filled += 1,
            "partial_cancelled" => counts.partial_cancelled += 1,
            "unfilled" => counts.unfilled += 1,
            "rejected" => counts.rejected += 1,
            "orphaned_by_restart" => counts.orphaned_by_restart += 1,
            _ => counts.skipped += 1,
        }
        if let Some(r) = reject {
            let e = rejects.entry(reject_key(r.reason.as_deref())).or_default();
            e.count += 1;
            e.qty = e.qty.saturating_add(r.remainder_qty.unwrap_or(0));
        }
        let recheck = checks
            .binary_search_by_key(&sid, |(s, _)| *s)
            .ok()
            .map(|k| &checks[k].1);
        let (verdict, recheck_qty, recheck_avg, gap_ticks) = if stored_worse {
            ("stored_worse", None, None, None)
        } else if priced {
            verdict_of(
                head.side == "BUY",
                head.tick_paise,
                order_qty,
                fill_qty,
                avg,
                recheck,
            )
        } else {
            ("not_priced", None, None, None)
        };
        match verdict {
            "match" => counts.matched += 1,
            "worse_than_worst" => counts.worse_than_worst += 1,
            "better_than_worst" => counts.better_than_worst += 1,
            "stored_worse" => counts.stored_worse += 1,
            "qty_differs" => counts.qty_differs += 1,
            "recheck_incomplete" => counts.recheck_incomplete += 1,
            "recheck_unavailable" => counts.recheck_unavailable += 1,
            "recheck_skipped" => counts.recheck_skipped += 1,
            _ => {}
        }
        if priced
            && let Some(Recheck {
                kind:
                    RecheckKind::Priced {
                        filled_qty,
                        fill_offsets,
                        ..
                    },
                ..
            }) = recheck
        {
            for (off, q) in fill_offsets {
                let k = (*off as usize).min(FILL_OFFSET_BUCKETS - 1);
                fill_seconds[k] = fill_seconds[k].saturating_add(*q);
            }
            let unfilled = u64::try_from(head.qty)
                .unwrap_or(0)
                .saturating_sub(*filled_qty);
            fill_seconds[FILL_OFFSET_BUCKETS] =
                fill_seconds[FILL_OFFSET_BUCKETS].saturating_add(unfilled);
        }
        let us = |a: Option<&EventRow>, b: Option<&EventRow>| match (a, b) {
            (Some(a), Some(b)) => Some((b.ts_ns - a.ts_ns) / 1_000),
            _ => None,
        };
        let decision = find("decision");
        let samples = [
            us(decision, built),
            us(built, sent),
            us(sent, ack),
            us(decision, fills.first().copied()),
            group.iter().find_map(|r| r.queue_wait_us),
            decision.and_then(|d| d.feed_lag_us),
            sent.and_then(|s| s.api_rtt_us),
        ];
        for (k, v) in samples.into_iter().enumerate() {
            if let Some(v) = v {
                series[k].push(v);
            }
        }
        let remainder = (order_qty - fill_qty).max(0);
        orders.push(OrderLine {
            signal_id: sid,
            security_id: head.security_id,
            segment: head.segment.clone(),
            symbol: head.symbol.clone(),
            recipe: head.recipe.clone(),
            version: head.recipe_version,
            side: head.side.clone(),
            qty: order_qty,
            candle_end: ist_text(head.candle_end_ns),
            decision_ts: ist_text(head.decision_ns),
            limit_paise: head.limit_paise,
            fill_qty,
            avg_paise: avg,
            model_paise: model,
            recheck_qty,
            recheck_avg_paise: recheck_avg,
            gap_ticks,
            remainder,
            outcome,
            verdict,
        });
    }
    let latency = LATENCY_SERIES
        .iter()
        .zip(series)
        .map(|(name, s)| latency_stats(name, s))
        .collect();
    // LF-7 review finding 2: only a start inside the reported day's session
    // can have lost orders held in RAM; an evening restart or a catch-up
    // rebuild on a later day did not.
    let restarted = !ctx.catch_up && started_inside_session(day, ctx.process_started_ist_ns);
    let mut reasons = Vec::new();
    if ctx.truncated {
        reasons.push("truncated");
    }
    if !ctx.settled {
        reasons.push("unsettled");
    }
    if restarted {
        reasons.push("process_started_after_0900");
    }
    if ctx.catch_up {
        reasons.push("catch_up_rebuild");
    }
    let no_orders = orders.is_empty();
    // LF-7 review round 3, finding 1: a day with no orders proves a quiet
    // strategy only when live fill actually ran in this process. A refused,
    // off or never-booted live fill reads Amber, never Green (audit rule 11).
    if no_orders && !live_fill_ran(ctx.boot) {
        reasons.push("live_fill_not_running");
    }
    let status = if no_orders && reasons.is_empty() {
        "no_orders"
    } else if reasons.is_empty() {
        "complete"
    } else {
        "partial"
    };
    let red = counts.worse_than_worst
        + counts.better_than_worst
        + counts.stored_worse
        + counts.qty_differs
        > 0;
    let amber = !reasons.is_empty()
        || counts.recheck_incomplete + counts.recheck_unavailable + counts.recheck_skipped > 0
        || counts.orphaned_by_restart > 0;
    DailyReport {
        day: day.to_owned(),
        status,
        status_reasons: reasons,
        light: if red {
            "red"
        } else if amber {
            "amber"
        } else {
            "green"
        },
        no_orders_reason: no_orders.then(|| no_orders_reason(ctx.boot)),
        fill_seconds_source: "recheck_from_candles_1s",
        counts,
        orders,
        fill_seconds,
        latency,
        rejects,
    }
}

/// IST epoch nanos of `day`'s midnight (`YYYY-MM-DD`), or `None`. Pure.
#[must_use]
pub fn rows_day_start(day: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let secs = d.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
    secs.checked_mul(NANOS_PER_SEC)
        .filter(|n| n.rem_euclid(NANOS_PER_DAY) == 0)
}
