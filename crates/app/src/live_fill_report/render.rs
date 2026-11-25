//! Renders a [`DailyReport`] as the day's five files (plan item LF-7):
//! `report-<D>.json`, `orders-<D>.csv`, `fill-seconds-<D>.csv`,
//! `latency-<D>.csv`, `rejects-<D>.csv`. Deterministic: no clock, fixed field
//! order, sorted maps. O(bytes).

use super::build::DailyReport;

/// The five file names of a day, in upload order. Pure.
#[must_use]
pub fn report_file_names(day: &str) -> [String; 5] {
    [
        format!("report-{day}.json"),
        format!("orders-{day}.csv"),
        format!("fill-seconds-{day}.csv"),
        format!("latency-{day}.csv"),
        format!("rejects-{day}.csv"),
    ]
}

/// The JSON report, pretty-printed, ending in a newline. O(bytes).
#[must_use]
pub fn render_json(r: &DailyReport) -> String {
    let mut s = serde_json::to_string_pretty(r).unwrap_or_else(|_| "{}".to_owned());
    s.push('\n');
    s
}

/// One CSV field: quoted (with `"` doubled) when it holds a comma, a quote,
/// a CR or a LF. O(len).
#[must_use]
pub fn csv_field(v: &str) -> String {
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_owned()
    }
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

/// The four CSV files, in [`report_file_names`] order after the JSON.
/// O(bytes).
#[must_use]
pub fn render_csvs(r: &DailyReport) -> [String; 4] {
    let mut orders = String::from(
        "signal_id,security_id,segment,symbol,recipe,version,side,qty,candle_end,decision_ts,\
         limit_paise,fill_qty,avg_paise,model_paise,recheck_qty,recheck_avg_paise,gap_ticks,\
         remainder,outcome,verdict\n",
    );
    for o in &r.orders {
        let cells = [
            o.signal_id.to_string(),
            o.security_id.to_string(),
            csv_field(&o.segment),
            csv_field(&o.symbol),
            csv_field(&o.recipe),
            o.version.to_string(),
            csv_field(&o.side),
            o.qty.to_string(),
            o.candle_end.clone(),
            o.decision_ts.clone(),
            opt(o.limit_paise),
            o.fill_qty.to_string(),
            opt(o.avg_paise),
            opt(o.model_paise),
            opt(o.recheck_qty),
            opt(o.recheck_avg_paise),
            opt(o.gap_ticks),
            o.remainder.to_string(),
            csv_field(&o.outcome),
            o.verdict.to_owned(),
        ];
        orders.push_str(&cells.join(","));
        orders.push('\n');
    }
    let mut fill = String::from("offset_seconds,shares\n");
    let last = r.fill_seconds.len().saturating_sub(1);
    for (k, q) in r.fill_seconds.iter().enumerate() {
        if k == last {
            fill.push_str(&format!("unfilled,{q}\n"));
        } else {
            fill.push_str(&format!("{k},{q}\n"));
        }
    }
    let mut latency = String::from("series,n,p50_us,p90_us,p99_us,max_us,negative\n");
    for l in &r.latency {
        latency.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            l.series,
            l.n,
            opt(l.p50),
            opt(l.p90),
            opt(l.p99),
            opt(l.max),
            l.negative
        ));
    }
    let mut rejects = String::from("reason,count,qty\n");
    for (reason, t) in &r.rejects {
        rejects.push_str(&format!("{},{},{}\n", csv_field(reason), t.count, t.qty));
    }
    [orders, fill, latency, rejects]
}

/// The five files of a day: `(file name, contents)`. O(bytes).
#[must_use]
pub fn render_files(r: &DailyReport) -> Vec<(String, String)> {
    let names = report_file_names(&r.day);
    let [orders, fill, latency, rejects] = render_csvs(r);
    names
        .into_iter()
        .zip([render_json(r), orders, fill, latency, rejects])
        .collect()
}

/// One plain line for the log. O(1).
#[must_use]
pub fn summary_plain(r: &DailyReport) -> String {
    let c = &r.counts;
    let mut s = format!(
        "live paper fill report {}: {} ({}), {} orders: {} filled, {} part filled, \
         {} not filled, {} skipped, {} rejected; worst-case check: {} match, {} worse, \
         {} better, {} not checked",
        r.day,
        r.light,
        r.status,
        c.orders,
        c.filled,
        c.partial_cancelled,
        c.unfilled,
        c.skipped,
        c.rejected,
        c.matched,
        c.worse_than_worst + c.stored_worse,
        c.better_than_worst,
        c.recheck_incomplete + c.recheck_unavailable + c.recheck_skipped
    );
    if let Some(why) = r.no_orders_reason {
        s.push_str(&format!("; no orders: {why}"));
    }
    if !r.status_reasons.is_empty() {
        s.push_str(&format!(
            "; partial because: {}",
            r.status_reasons.join(", ")
        ));
    }
    s
}
