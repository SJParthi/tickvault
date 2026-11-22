//! The replay's files (plan item LF-8): `decisions.csv`, `events.csv`, the
//! daily calibration report in its LF-7 form (built by
//! `live_fill_report::build::build_report` and rendered by
//! `live_fill_report::render::render_files`, the same code the daily report
//! runs), `summary.txt`, and with `--compare` a `diff.csv`. Every file is a
//! pure function of the replay's output: no clock, no host name, so a replay
//! of the same input writes the same bytes.

// O(1) EXEMPT: begin: cold offline CLI: O(rows log rows) to build the report, O(bytes) to write.
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use tickvault_storage::live_order_events_persistence::{
    LiveOrderEvent, worse_than_model_wire, write_csv_header, write_csv_record,
};
use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::worst_fill::SecondBar;

use super::drive::{DecisionTiming, ReplayOutput, ReplayStats};
use super::scripted::parse_segment;
use crate::live_fill::BootOutcome;
use crate::live_fill_report::build::{
    DailyReport, ReportContext, build_report, held_share_known, priced_signal_ids,
};
use crate::live_fill_report::recheck::{OrderSeconds, Recheck, recheck_walk, walk_items};
use crate::live_fill_report::render::{csv_field, render_files};
use crate::live_fill_report::rows::EventRow;

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// IST epoch nanos as `YYYY-MM-DDTHH:MM:SS.nnnnnnnnn` (IST wall clock). O(1).
#[must_use]
pub fn ist_text(nanos: i64) -> String {
    let secs = nanos.div_euclid(NANOS_PER_SEC);
    let sub = u32::try_from(nanos.rem_euclid(NANOS_PER_SEC)).unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, sub).map_or_else(
        || nanos.to_string(),
        |d| d.format("%Y-%m-%dT%H:%M:%S%.9f").to_string(),
    )
}

/// Nanoseconds as milliseconds with three decimals (`-0.250`). O(1).
#[must_use]
pub fn ms_text(nanos: i64) -> String {
    let sign = if nanos < 0 { "-" } else { "" };
    let a = nanos.unsigned_abs();
    format!("{sign}{}.{:03}", a / 1_000_000, (a % 1_000_000) / 1_000)
}

/// `events.csv`: the LF-5 columns, one row per event, in the order the
/// chain and the paper rows produced them. O(rows).
#[must_use]
pub fn events_csv(events: &[LiveOrderEvent]) -> Vec<u8> {
    let mut out = Vec::with_capacity(events.len().saturating_mul(400).max(512));
    // A write into a Vec cannot fail.
    let _ok = write_csv_header(&mut out);
    for e in events {
        let _ok = write_csv_record(e, &mut out);
    }
    out
}

/// `decisions.csv`: one row per decision, see [`DecisionTiming`]. O(rows).
#[must_use]
pub fn decisions_csv(decisions: &[DecisionTiming]) -> String {
    let mut s = String::from(
        "signal_id,security_id,segment,symbol,frame_secs,candle_start_ist,candle_end_ist,\
         close_by,last_tick_exchange_ist,last_tick_receipt_ist,release_ist,decision_ist,\
         decision_minus_end_ms,wait_used_ms,opening_clamp,late_price_skip,taint\n",
    );
    for d in decisions {
        let opt = |v: Option<i64>| v.map(ist_text).unwrap_or_default();
        let _ok = writeln!(
            s,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            d.signal_id,
            d.security_id,
            d.segment,
            csv_field(&d.symbol),
            d.frame_secs,
            ist_text(d.candle_start_ns),
            ist_text(d.candle_end_ns),
            d.close_by,
            opt(d.last_tick_exchange_ns),
            opt(d.last_tick_receipt_ns),
            ist_text(d.release_ns),
            ist_text(d.decision_ns),
            ms_text(d.decision_ns.saturating_sub(d.candle_end_ns)),
            ms_text(d.release_ns.saturating_sub(d.candle_end_ns)),
            d.opening_clamp,
            d.late_price_skip,
            d.taint.as_deref().unwrap_or(""),
        );
    }
    s
}

/// One event as the daily report reads it from `live_order_events`
/// (timestamps cut to microseconds, as QuestDB stores them). O(1).
#[must_use]
pub fn event_row_of(e: &LiveOrderEvent) -> EventRow {
    let us = |v: i64| v.div_euclid(1_000).saturating_mul(1_000);
    EventRow {
        ts_ns: us(e.ts_ist_nanos),
        event: e.event.as_str().to_owned(),
        signal_id: e.signal_id,
        seq: i64::from(e.seq),
        recipe: e.recipe.clone(),
        recipe_version: i64::from(e.recipe_version),
        security_id: e.security_id,
        segment: e.segment.as_str().to_owned(),
        symbol: e.symbol.clone(),
        side: e.side.as_str().to_owned(),
        qty: e.qty,
        candle_end_ns: e.candle_end_ist_nanos.map(us),
        decision_ns: e.decision_ist_nanos.map(us),
        best_bid_paise: e.best_bid_paise,
        best_bid_qty: e.best_bid_qty,
        best_ask_paise: e.best_ask_paise,
        best_ask_qty: e.best_ask_qty,
        ltp_paise: e.ltp_paise,
        limit_paise: e.limit_paise,
        queue_wait_us: e.queue_wait_us,
        feed_lag_us: e.feed_lag_us,
        api_rtt_us: e.api_rtt_us,
        fill_qty: e.fill_qty,
        avg_price_paise: e.avg_price_paise,
        remainder_qty: e.remainder_qty,
        model_price_paise: e.model_price_paise,
        worse_than_model: worse_than_model_wire(e.worse_than_model).map(str::to_owned),
        reason: e.reason.clone().filter(|r| !r.is_empty()),
        tick_paise: e.tick_paise,
        lot_size: e.lot_size,
        participation_bps: e.participation_bps.map(i64::from),
        max_fill_seconds: e.max_fill_seconds.map(i64::from),
    }
}

/// Re-prices every priced order from the fold's 1 s bars, exactly as the
/// daily report re-prices from stored `candles_1s`: the same walk
/// (`recheck_walk`), so held orders of one instrument share its seconds the
/// way the chain shares them (LF-9 finding 3). O(orders × window).
#[must_use]
pub fn rechecks(
    rows: &[EventRow],
    seconds: &HashMap<InstrumentKey, BTreeMap<i64, SecondBar>>,
) -> Vec<(i64, Recheck)> {
    let targets = priced_signal_ids(rows);
    let held = held_share_known(rows);
    let items = walk_items(rows, &targets, &held);
    let mut got = BTreeMap::new();
    for item in &items {
        if !item.wants_seconds() {
            continue;
        }
        let Some(inp) = item.inputs.as_ref() else {
            continue;
        };
        let key = parse_segment(&inp.segment).map(|s| InstrumentKey {
            security_id: u64::try_from(inp.security_id).unwrap_or(0),
            exchange_segment: s.binary_code(),
        });
        let bars = key.and_then(|k| seconds.get(&k));
        let window: Vec<SecondBar> = bars
            .map(|b| {
                b.range(inp.decision_sec..=inp.horizon_end())
                    .map(|(_, s)| *s)
                    .collect()
            })
            .unwrap_or_default();
        got.insert(item.row.signal_id, OrderSeconds::Seconds(window));
    }
    recheck_walk(&items, &mut got)
}

/// The day's report from the replay's rows, with the LF-7 builder.
/// `incomplete_reasons` is what `incomplete_reasons` in the replay module
/// found: a day the replay itself judges incomplete is not settled, so its
/// report reads partial (Amber, `unsettled`), never complete and Green. Pure.
#[must_use]
pub fn day_report(day: &str, out: &ReplayOutput, incomplete_reasons: &[String]) -> DailyReport {
    let mut rows: Vec<EventRow> = Vec::with_capacity(out.events.len());
    for e in &out.events {
        rows.push(event_row_of(e));
    }
    let checks = rechecks(&rows, &out.seconds);
    let ctx = ReportContext {
        boot: Some(BootOutcome::Running),
        // When the replayed process started: the instant the chain was built
        // (the boot, or the first frame when that came earlier), never the
        // first frame, which comes after a pre-market boot and would read as
        // a restart after 09:00.
        process_started_ist_ns: out.stats.chain_start_ist_ns.unwrap_or(0),
        // A replay with no decision proves nothing (the scheduled bars never
        // closed, or no frame of the day was read), and a day the replay
        // found incomplete (its head or tail missing, a silence inside the
        // session, an unknown boot) is not the whole day: neither may read
        // Green or `complete`. Not settled reads partial and Amber, and
        // `summary.txt` names the reasons.
        settled: !out.decisions.is_empty() && incomplete_reasons.is_empty(),
        truncated: false,
        catch_up: false,
    };
    build_report(day, &rows, &checks, ctx)
}

/// The report's five LF-7 files `(name, contents)`. O(bytes).
#[must_use]
pub fn report_files(report: &DailyReport) -> Vec<(String, String)> {
    render_files(report)
}

/// The events the self-check refuses: a fill worse than the worst-case model
/// it was priced from. O(rows).
#[must_use]
pub fn worse_than_worst_rows(events: &[LiveOrderEvent]) -> Vec<&LiveOrderEvent> {
    events
        .iter()
        .filter(|e| e.worse_than_model == Some(true))
        .collect()
}

/// The query that exports the live day's decisions for `--compare`. Pure.
#[must_use]
pub fn compare_sql(day: &str) -> String {
    format!(
        "SELECT event, security_id, segment, candle_start, candle_end, decision_ts FROM \
         live_order_events WHERE ts IN '{day}' AND event = 'decision' \
         ORDER BY candle_end, security_id, candle_start, decision_ts"
    )
}

/// One live decision read from a `--compare` file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveDecision {
    security_id: i64,
    segment: String,
    candle_start_ns: i64,
    candle_end_ns: i64,
    decision_ns: i64,
}

/// A timestamp cell: RFC 3339 with a zone (QuestDB's export, IST written as
/// Z) or this tool's own `YYYY-MM-DDTHH:MM:SS.f` text. O(len).
fn parse_ts_cell(v: &str) -> Option<i64> {
    let v = v.trim().trim_matches('"');
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(v) {
        return d.timestamp_nanos_opt();
    }
    chrono::NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .and_then(|d| d.and_utc().timestamp_nanos_opt())
}

/// Reads a live decisions export (a header naming `event`, `security_id`,
/// `segment`, `candle_start`, `candle_end` and `decision_ts`; other columns ignored; only
/// `decision` rows kept). O(rows).
fn parse_live_decisions(text: &str) -> Result<Vec<LiveDecision>, String> {
    let mut lines = text.lines();
    let header: Vec<&str> = lines
        .next()
        .ok_or("compare file is empty")?
        .split(',')
        .map(|h| h.trim().trim_matches('"'))
        .collect();
    let col = |n: &str| {
        header
            .iter()
            .position(|h| *h == n)
            .ok_or_else(|| format!("compare file has no `{n}` column"))
    };
    let (ev, sid, seg, start, end, dec) = (
        col("event")?,
        col("security_id")?,
        col("segment")?,
        col("candle_start")?,
        col("candle_end")?,
        col("decision_ts")?,
    );
    let mut out = Vec::new();
    for (n, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let cells: Vec<&str> = line
            .split(',')
            .map(|c| c.trim().trim_matches('"'))
            .collect();
        let get = |i: usize| cells.get(i).copied().unwrap_or("");
        if get(ev) != "decision" {
            continue;
        }
        let bad = || format!("compare file line {}: unreadable", n + 2);
        out.push(LiveDecision {
            security_id: get(sid).parse().map_err(|_| bad())?,
            segment: get(seg).to_owned(),
            candle_start_ns: parse_ts_cell(get(start)).ok_or_else(bad)?,
            candle_end_ns: parse_ts_cell(get(end)).ok_or_else(bad)?,
            decision_ns: parse_ts_cell(get(dec)).ok_or_else(bad)?,
        });
    }
    Ok(out)
}

/// `diff.csv`: replay against live, matched on (security id, segment, candle
/// start, candle end) (signal ids differ between processes), so bars of
/// different sizes ending at the same instant never share a row. Several
/// decisions on one candle (two rules on one bar) are each kept: the k-th
/// earliest live decision is paired with the k-th earliest replay one, one
/// row per pair; `replay_minus_live_ms` is empty when only one side has that
/// decision. O(rows log rows).
///
/// # Errors
/// The compare file could not be read as a decisions export.
pub fn compare_csv(live_text: &str, decisions: &[DecisionTiming]) -> Result<String, String> {
    let live = parse_live_decisions(live_text)?;
    type Key = (i64, String, i64, i64);
    let mut both: BTreeMap<Key, (Vec<i64>, Vec<i64>)> = BTreeMap::new();
    for l in live {
        both.entry((l.security_id, l.segment, l.candle_end_ns, l.candle_start_ns))
            .or_default()
            .0
            .push(l.decision_ns);
    }
    for d in decisions {
        both.entry((
            d.security_id,
            d.segment.to_owned(),
            d.candle_end_ns,
            d.candle_start_ns,
        ))
        .or_default()
        .1
        .push(d.decision_ns);
    }
    let mut s = String::from(
        "security_id,segment,candle_start_ist,candle_end_ist,live_decision_ist,\
         replay_decision_ist,replay_minus_live_ms\n",
    );
    for ((sid, seg, end, start), (live, replay)) in &mut both {
        live.sort_unstable();
        replay.sort_unstable();
        for k in 0..live.len().max(replay.len()) {
            let (l, r) = (live.get(k).copied(), replay.get(k).copied());
            let diff = match (l, r) {
                (Some(l), Some(r)) => ms_text(r.saturating_sub(l)),
                _ => String::new(),
            };
            let _ok = writeln!(
                s,
                "{sid},{},{},{},{},{},{diff}",
                csv_field(seg),
                ist_text(*start),
                ist_text(*end),
                l.map(ist_text).unwrap_or_default(),
                r.map(ist_text).unwrap_or_default(),
            );
        }
    }
    Ok(s)
}

/// What `summary.txt` reports beyond the replay's own counts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryInputs {
    pub day: String,
    pub weekday: String,
    pub incomplete_reasons: Vec<String>,
    pub largest_segment_bytes: u64,
    /// `(file name, sha-256 hex)` of every other output file.
    pub file_hashes: Vec<(String, String)>,
}

/// The honest limits, printed in `summary.txt` every time (plan item LF-8,
/// C.9).
pub const HONEST_LIMITS: [&str; 9] = [
    "A process restart on the recorded day is not reproduced: live started a new chain, a new \
     candle fold and a flat paper position at each boot (orders it held were lost, and an exit \
     after the boot found no position), while the replay keeps one chain, one fold and one \
     paper position for the whole day. A restart shows only as a silence in the frames (named \
     in the incomplete reason when over 30 s); read events after it with care.",
    "No live losses are replayed: the loss count moves only where the replay itself lost frames \
     (a damaged stretch of a segment, the rest of a segment whose read stopped, or an \
     abandoned frame tail); the live tap's own ring drops and frames shed before the drain \
     are not reproduced, so live can refuse a candle the replay trusts.",
    "Main-feed socket outages are invisible: a socket that was down delivered no frames, so they \
     are not in the recorded WAL either, and a replayed frame carries no socket. Live counts a \
     loss while any main-feed socket is down and refuses the candles it touched; the replay \
     builds that socket's instruments' bars short (missing the trades of the outage), trusts \
     them, and can decide and fill on them. The other sockets keep the stream busy, so the \
     feed-gap watch does not fire either. Check the day's socket audit before reading a \
     decision as one live would have made.",
    "Frame order is the WAL's write order, which can differ from the live drain's order by up \
     to about a second (the reorder slack between sockets).",
    "Decision times are on the replay's pass grid (1 ms by default); the live thread wakes on \
     each new tick and otherwise passes at least every 1 ms, or 5 ms after 50 idle passes, so \
     a decision only the clock releases can land up to about 5 ms later live. --compare shows it.",
    "No risk check and no order gates: the risk engine's pre-trade check and the OMS rate, \
     budget and circuit gates are skipped, so an order the runtime would refuse shows as \
     placed and filled.",
    "Backup copies: without --backup-history the second copy of a backed-up contract's packet \
     is not dropped; its book quote can overwrite once (the fold already prevents double volume).",
    "Cost: fetching a day from the bucket costs data transfer, plus a retrieval fee for frames \
     older than 30 days; the download cap and the printed total bound it.",
    "No real decisions: orders come only from the scripted test schedule, never a strategy. This \
     measures candle timing and the worst-case fill model, not any strategy.",
];

/// `summary.txt`. Pure. O(rows).
#[must_use]
pub fn summary_text(out: &ReplayOutput, report: &DailyReport, inp: &SummaryInputs) -> String {
    let s: &ReplayStats = &out.stats;
    let c = &report.counts;
    let worse = worse_than_worst_rows(&out.events).len();
    let mut t = String::new();
    let _ok = writeln!(
        t,
        "Replay of {} {}: {} pretend orders on {} candles; {} filled, {} part filled and \
         cancelled, {} not filled, {} skipped, {} rejected. Nothing real was sent: no broker, \
         no order system, no database, no bucket write.",
        inp.weekday,
        inp.day,
        c.orders,
        out.decisions.len(),
        c.filled,
        c.partial_cancelled,
        c.unfilled,
        c.skipped,
        c.rejected
    );
    let _ok = writeln!(
        t,
        "Worst-case check: {worse} fills worse than the model they were priced from \
         (must be 0); re-priced from the replay's own 1 s bars: {} match, {} worse, {} better, \
         {} not checked. Report light: {} ({}).",
        c.matched,
        c.worse_than_worst + c.stored_worse,
        c.better_than_worst,
        c.recheck_incomplete + c.recheck_unavailable + c.recheck_skipped,
        report.light,
        report.status
    );
    if inp.incomplete_reasons.is_empty() {
        let _ok = writeln!(t, "Day: complete.");
    } else {
        let _ok = writeln!(
            t,
            "Day: INCOMPLETE ({}).",
            inp.incomplete_reasons.join("; ")
        );
    }
    t.push('\n');
    let boot = s.chain_start_ist_ns.map_or_else(
        || "none (no frame of the day)".to_owned(),
        |ns| {
            let how = if s.boot_assumed {
                "the scheduled 08:30 boot assumed, or the first frame if earlier; the WAL does \
                 not record the boot"
            } else {
                "--boot-ist, or the first frame if earlier"
            };
            format!("{} ({how})", ist_text(ns))
        },
    );
    let rows: [(&str, String); 26] = [
        ("chain started at", boot),
        ("frames before --boot-ist", s.frames_before_boot.to_string()),
        ("segments", s.segments.to_string()),
        ("damaged segments", s.damaged_segments.to_string()),
        (
            "largest segment read, bytes",
            inp.largest_segment_bytes.to_string(),
        ),
        ("frames", s.frames.to_string()),
        (
            "records of another transport (TrueData, old order updates), skipped",
            s.other_transport_frames.to_string(),
        ),
        ("frames from another day", s.other_day_frames.to_string()),
        (
            "frames with no receipt (refused)",
            s.no_receipt_frames.to_string(),
        ),
        ("packets", s.packets.to_string()),
        ("abandoned frame bytes", s.abandoned_bytes.to_string()),
        ("ticks", s.ticks.to_string()),
        ("backup copies dropped", s.backup_dropped.to_string()),
        ("ticks folded", s.folded.to_string()),
        ("ticks tapped", s.tapped.to_string()),
        ("ticks kept as rows only", s.candle_only.to_string()),
        ("ticks refused", s.refused.to_string()),
        ("loss count at the end", s.drop_seq.to_string()),
        (
            "losses wholly before the day's chain (not handed to it)",
            s.losses_before_chain.to_string(),
        ),
        ("chain passes", s.passes.to_string()),
        ("decisions", out.decisions.len().to_string()),
        (
            "decisions skipped for a late price",
            out.decisions
                .iter()
                .filter(|d| d.late_price_skip)
                .count()
                .to_string(),
        ),
        (
            "decisions refused on an untrusted stretch",
            out.decisions
                .iter()
                .filter(|d| d.taint.is_some())
                .count()
                .to_string(),
        ),
        ("paper orders booked", s.executions.to_string()),
        ("marks", s.marks.to_string()),
        (
            "longest in-session silence, ms",
            ms_text(s.max_session_gap_ns),
        ),
    ];
    for (k, v) in rows {
        let _ok = writeln!(t, "{k}: {v}");
    }
    let mut taints: BTreeMap<&str, u64> = BTreeMap::new();
    for d in &out.decisions {
        if let Some(r) = d.taint.as_deref() {
            *taints.entry(r).or_default() += 1;
        }
    }
    for (r, n) in taints {
        let _ok = writeln!(t, "refused for {r}: {n}");
    }
    t.push_str("\nHonest limits:\n");
    for l in HONEST_LIMITS {
        let _ok = writeln!(t, "- {l}");
    }
    let _ok = writeln!(
        t,
        "\nTo compare with the live day, export its decisions as CSV with:\n  {}\nand pass it \
         with --compare <file>.",
        compare_sql(&inp.day)
    );
    t.push_str("\nFiles (sha-256):\n");
    for (name, hash) in &inp.file_hashes {
        let _ok = writeln!(t, "{hash}  {name}");
    }
    t
}
// O(1) EXEMPT: end
