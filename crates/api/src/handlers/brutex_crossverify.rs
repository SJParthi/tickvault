//! Public read-only BruteX ⇄ live cross-verify report page (`GET /crossverify`).
//!
//! Renders the daily BruteX-vs-live 1-minute comparison verdict from the two
//! forensic QuestDB tables written by the cross-verify runner
//! (`crates/storage/src/brutex_crossverify_persistence.rs`):
//!
//! - `brutex_crossverify_daily` — per-symbol summary rows + one `__RUN__`
//!   aggregate row per trading day.
//! - `brutex_crossverify_cell_audit` — per-minute per-field drill-down cells
//!   (diverged / missing / tail-unsealed / out-of-session / unmapped).
//!
//! Query params (both optional, both fail-closed with HTTP 400):
//! - `date`   — `YYYY-MM-DD`; defaults to the latest recorded trading day.
//! - `symbol` — ≤64 chars of `[A-Za-z0-9 ._-]`; when present the page adds a
//!   per-minute drill-down table for that symbol, filtered to the day's
//!   `__RUN__` row `observed_at` so stale cells from superseded reruns are
//!   excluded.
//!
//! Security posture: the page is READ-ONLY and carries no secrets (same
//! public tier as `/feeds` + the `GET /api/feeds` read precedent — NO bearer,
//! NO mutating endpoint). Because every hit costs QuestDB queries it is
//! registered on the RATE-LIMITED public sub-router (the 2026-07-09 audit
//! hardening posture for unauthenticated QuestDB-backed endpoints). Every
//! interpolated value passes through [`escape_html`]; SQL literals are either
//! validated integers or single-quote-doubled strings from an allowlisted
//! charset.
//!
//! Honesty (audit Rule 11): a missing table / empty day renders a LOUD
//! "no cross-verify runs recorded yet" state — never a fabricated clean page.
//! `-1` sentinel values render as `?`, never as a misleading `0`.

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use tickvault_storage::brutex_crossverify_persistence::{
    BRUTEX_CROSSVERIFY_CELL_AUDIT_TABLE, BRUTEX_CROSSVERIFY_DAILY_TABLE,
    BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL,
};

use crate::state::SharedAppState;

/// Content-Security-Policy for the page (mirror of the `/feeds` page CSP):
/// same-origin only, inline style allowed (self-contained page, no CDN, no
/// script at all), `frame-ancestors 'self'` blocks click-jacking.
const CROSSVERIFY_PAGE_CSP: &str = "default-src 'self'; script-src 'none'; \
     style-src 'unsafe-inline'; img-src 'self'; connect-src 'self'; \
     frame-ancestors 'self'; base-uri 'none'; form-action 'none'";

/// The fixed honesty line (operator contract — neither side is ground truth).
const HONESTY_LINE: &str = "Neither side is ground truth — expected fluctuation = \
     within tolerance; real divergence = beyond. Live side only bars minutes that \
     ticked; vendor side fills.";

/// Drill-down cell cap (mirrors the query LIMIT).
const CELL_DRILLDOWN_LIMIT: u32 = 2000;

/// Query parameters for `GET /crossverify`.
#[derive(Debug, Deserialize)]
pub struct CrossverifyParams {
    /// Trading day `YYYY-MM-DD`; defaults to the latest recorded day.
    date: Option<String>,
    /// Symbol for the per-minute drill-down table.
    symbol: Option<String>,
}

/// One parsed `brutex_crossverify_daily` row (per-symbol or `__RUN__`).
#[derive(Clone, Debug)]
pub(crate) struct DailyRow {
    symbol: String,
    security_id: i64,
    exchange_segment: String,
    outcome: String,
    minutes_compared: i64,
    cells_compared: i64,
    diverged_cells: i64,
    missing_live: i64,
    missing_brutex: i64,
    tail_unsealed: i64,
    out_of_session: i64,
    unmapped_symbols: i64,
    run_partial: bool,
    observed_at: String,
    note: String,
}

/// One parsed `brutex_crossverify_cell_audit` drill-down row.
#[derive(Clone, Debug)]
pub(crate) struct CellRow {
    minute_ts_ist: String,
    kind: String,
    field: String,
    brutex_paise: i64,
    live_paise: i64,
    brutex_volume: i64,
    live_volume: i64,
}

// ---------------------------------------------------------------------------
// Pure validation / formatting helpers (unit-tested below)
// ---------------------------------------------------------------------------

/// HTML-escapes every character that could open a tag/attribute context.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Strict `^\d{4}-\d{2}-\d{2}$` shape check (no regex crate needed).
fn is_valid_date_shape(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0usize, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| b[i].is_ascii_digit())
}

/// Symbol param allowlist: 1..=64 chars of `[A-Za-z0-9 ._&-]`.
/// FIX ROUND 3 (2026-07-12): `&` added in lockstep with the CSV parse
/// charset (`symbol_charset_ok` in `brutex_crossverify_compare.rs`) —
/// real NSE F&O symbols carry it (M&M, M&MFIN, GMRP&UI); every sink
/// escapes it (`%26` in hrefs, `&amp;` in HTML, single-quoted in SQL).
fn is_valid_symbol_param(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b'_' | b'-' | b'&'))
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses a validated `YYYY-MM-DD` into IST-midnight epoch MICROSECONDS
/// (QuestDB integer TIMESTAMP literals are microseconds; the repo stores IST
/// wall-clock as-if-UTC). Rejects impossible months/days (fail-closed).
fn date_param_to_micros(s: &str) -> Option<i64> {
    if !is_valid_date_shape(s) {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if leap {
                29
            } else {
                28
            }
        }
    };
    if !(1..=dim).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400_000_000)
}

/// SQL string-literal escape: single quotes doubled. The inputs are already
/// allowlist-validated; this is defense in depth.
fn sql_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// Shape check for a QuestDB-returned ISO timestamp before splicing it back
/// into a SQL literal (digits + `T Z : . -` only).
fn is_iso_ts_shape(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'T' | b'Z' | b':' | b'.' | b'-'))
}

/// Renders a DB-sourced paise value as rupees with 2 decimals; the `-1`
/// sentinel (value absent) renders `?`.
fn paise_cell(p: i64) -> String {
    if p == -1 {
        return "?".to_string();
    }
    paise_signed(p)
}

/// Renders a COMPUTED paise value (e.g. a diff) as signed rupees — no
/// sentinel handling (a genuine -1 paise diff must render as "-0.01").
fn paise_signed(p: i64) -> String {
    let a = p.unsigned_abs();
    format!("{}{}.{:02}", if p < 0 { "-" } else { "" }, a / 100, a % 100)
}

/// Renders a DB-sourced count; the `-1` sentinel renders `?`.
fn count_str(v: i64) -> String {
    if v == -1 {
        "?".to_string()
    } else {
        v.to_string()
    }
}

/// Extracts `HH:MM` from an ISO timestamp (`....-..-..THH:MM:...`); falls
/// back to the escaped full string on unexpected shapes.
fn minute_hhmm(iso: &str) -> String {
    let b = iso.as_bytes();
    if b.len() >= 16 && b[10] == b'T' {
        escape_html(&iso[11..16])
    } else {
        escape_html(iso)
    }
}

/// URL-encodes a symbol for the drill-down link (allowlisted charset: the
/// space and `&` need encoding among `[A-Za-z0-9 ._&-]` — `&` → `%26` so an
/// M&M-class symbol can never split the query string; everything else is
/// percent-encoded anyway as defense in depth).
fn url_encode_symbol(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Maps a daily outcome wire label to (banner CSS class, loud banner text).
fn outcome_banner(outcome: &str) -> (&'static str, String) {
    match outcome {
        "clean" => (
            "ok",
            "CLEAN — every compared cell within tolerance".to_string(),
        ),
        "diverged" => (
            "warn",
            "⚠ DIVERGED — real divergence beyond tolerance found".to_string(),
        ),
        "partial" => (
            "bad",
            "⚠ PARTIAL — incomplete evidence; treat every count as a floor".to_string(),
        ),
        "no_data" => (
            "bad",
            "⚠ NO DATA — the vendor side produced nothing for this day".to_string(),
        ),
        "blind" => (
            "bad",
            "⚠ BLIND — the live side produced nothing to compare against".to_string(),
        ),
        "degraded" => (
            "bad",
            "⚠ DEGRADED — the run itself degraded; do not trust the counts".to_string(),
        ),
        other => ("bad", format!("⚠ UNKNOWN OUTCOME ({})", escape_html(other))),
    }
}

// ---------------------------------------------------------------------------
// Pure SQL builders (unit-tested below)
// ---------------------------------------------------------------------------

/// SQL for the latest recorded trading day.
fn max_date_sql() -> String {
    format!("SELECT max(trading_date_ist) FROM {BRUTEX_CROSSVERIFY_DAILY_TABLE}")
}

/// SQL for all daily rows (per-symbol + `__RUN__`) of one trading day.
/// Defensive `LIMIT` (mirrors [`CELL_DRILLDOWN_LIMIT`]) bounds a hostile /
/// corrupted table — a legitimate day is ≤ ~800 rows (universe + run row).
fn daily_select_sql(date_micros: i64) -> String {
    format!(
        "SELECT symbol, security_id, exchange_segment, outcome, minutes_compared, \
         cells_compared, diverged_cells, missing_live, missing_brutex, tail_unsealed, \
         out_of_session, unmapped_symbols, run_partial, observed_at, note \
         FROM {BRUTEX_CROSSVERIFY_DAILY_TABLE} WHERE trading_date_ist = {date_micros} \
         LIMIT {CELL_DRILLDOWN_LIMIT}"
    )
}

/// SQL for one symbol's drill-down cells, pinned to the day's run
/// `observed_at` (excludes stale cells from superseded reruns).
fn cell_select_sql(date_micros: i64, symbol: &str, observed_at_iso: &str) -> String {
    format!(
        "SELECT minute_ts_ist, kind, field, brutex_paise, live_paise, brutex_volume, \
         live_volume FROM {BRUTEX_CROSSVERIFY_CELL_AUDIT_TABLE} \
         WHERE trading_date_ist = {date_micros} AND symbol = '{}' \
         AND observed_at = '{}' ORDER BY minute_ts_ist LIMIT {CELL_DRILLDOWN_LIMIT}",
        sql_quote(symbol),
        sql_quote(observed_at_iso)
    )
}

// ---------------------------------------------------------------------------
// Pure /exec response parsers (unit-tested below)
// ---------------------------------------------------------------------------

/// `NULL`/absent numeric cells map to the `-1` sentinel (rendered `?`),
/// never a misleading `0`.
fn cell_i64(row: &[serde_json::Value], idx: usize) -> i64 {
    row.get(idx)
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-1)
}

fn cell_str(row: &[serde_json::Value], idx: usize) -> String {
    row.get(idx)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Parses one daily-summary /exec row (column order = [`daily_select_sql`]).
fn parse_daily_row(row: &serde_json::Value) -> Option<DailyRow> {
    let r = row.as_array()?;
    Some(DailyRow {
        symbol: cell_str(r, 0),
        security_id: cell_i64(r, 1),
        exchange_segment: cell_str(r, 2),
        outcome: cell_str(r, 3),
        minutes_compared: cell_i64(r, 4),
        cells_compared: cell_i64(r, 5),
        diverged_cells: cell_i64(r, 6),
        missing_live: cell_i64(r, 7),
        missing_brutex: cell_i64(r, 8),
        tail_unsealed: cell_i64(r, 9),
        out_of_session: cell_i64(r, 10),
        unmapped_symbols: cell_i64(r, 11),
        run_partial: r
            .get(12)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        observed_at: cell_str(r, 13),
        note: cell_str(r, 14),
    })
}

/// Parses one cell-audit /exec row (column order = [`cell_select_sql`]).
fn parse_cell_row(row: &serde_json::Value) -> Option<CellRow> {
    let r = row.as_array()?;
    Some(CellRow {
        minute_ts_ist: cell_str(r, 0),
        kind: cell_str(r, 1),
        field: cell_str(r, 2),
        brutex_paise: cell_i64(r, 3),
        live_paise: cell_i64(r, 4),
        brutex_volume: cell_i64(r, 5),
        live_volume: cell_i64(r, 6),
    })
}

/// Extracts the first cell of the first row as a string (the `max()` query).
fn extract_first_string(dataset: &[serde_json::Value]) -> Option<String> {
    dataset
        .first()?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_string)
}

/// Splits the day's rows into (`__RUN__` aggregate row, per-symbol rows).
fn split_run_row(rows: Vec<DailyRow>) -> (Option<DailyRow>, Vec<DailyRow>) {
    let mut run = None;
    let mut symbols = Vec::with_capacity(rows.len());
    for r in rows {
        if r.symbol == BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL {
            if run.is_none() {
                run = Some(r);
            }
        } else {
            symbols.push(r);
        }
    }
    (run, symbols)
}

/// Sorts per-symbol rows worst-offender-first (diverged desc, then symbol).
fn sort_symbol_rows(rows: &mut [DailyRow]) {
    rows.sort_by(|a, b| {
        b.diverged_cells
            .cmp(&a.diverged_cells)
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
}

// ---------------------------------------------------------------------------
// Pure page renderer (unit-tested below)
// ---------------------------------------------------------------------------

const PAGE_CSS: &str = r"
  :root { color-scheme: light dark; }
  body { font-family: system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
         margin: 0; padding: 24px; max-width: 1100px; margin-inline: auto;
         line-height: 1.5; }
  h1 { font-size: 1.4rem; margin: 0 0 4px; }
  h2 { font-size: 1.1rem; margin: 24px 0 8px; }
  .sub { color: #888; margin: 0 0 16px; font-size: 0.9rem; }
  .banner { padding: 12px 16px; border-radius: 10px; font-weight: 600;
            margin: 0 0 16px; border: 1px solid #8884; }
  .banner.ok   { background: #14532d22; border-color: #16a34a; color: #16a34a; }
  .banner.warn { background: #78350f22; border-color: #d97706; color: #d97706; }
  .banner.bad  { background: #7f1d1d22; border-color: #dc2626; color: #dc2626;
                 font-size: 1.05rem; }
  .honesty { font-style: italic; color: #888; margin: 0 0 16px; font-size: 0.9rem; }
  .noise { margin: 0 0 16px; font-size: 0.9rem; }
  table { border-collapse: collapse; width: 100%; font-size: 0.9rem; }
  th, td { border: 1px solid #8884; padding: 6px 8px; text-align: right; }
  th { background: #8881; }
  td:first-child, th:first-child { text-align: left; }
  td.l { text-align: left; }
  a { color: inherit; }
  .footer { margin-top: 24px; color: #888; font-size: 0.85rem; }
  .empty { padding: 24px; border: 1px dashed #8886; border-radius: 10px;
           color: #888; text-align: center; }
";

/// Renders the full self-contained HTML page. Pure — no I/O.
fn render_crossverify_page(
    date_label: &str,
    run_row: Option<&DailyRow>,
    symbol_rows: &[DailyRow],
    drill: Option<(&str, &[CellRow])>,
) -> String {
    let date_esc = escape_html(date_label);
    let mut page = String::with_capacity(16 * 1024);
    page.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    page.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    page.push_str(&format!(
        "<title>TickVault — BruteX Cross-Verify {date_esc}</title>\n"
    ));
    page.push_str(&format!("<style>{PAGE_CSS}</style>\n</head>\n<body>\n"));
    page.push_str(&format!(
        "<h1>BruteX ⇄ Live Cross-Verify — {date_esc}</h1>\n"
    ));
    page.push_str(
        "<p class=\"sub\">Read-only daily 1-minute comparison report \
         (vendor BruteX bars vs our live-captured bars).</p>\n",
    );

    let empty = run_row.is_none() && symbol_rows.is_empty();
    if empty {
        page.push_str(
            "<div class=\"empty\">no cross-verify runs recorded yet — the daily \
             comparison writes its first rows after the first post-close run</div>\n",
        );
        page.push_str(&format!("<p class=\"honesty\">{HONESTY_LINE}</p>\n"));
        push_footer(&mut page);
        page.push_str("</body>\n</html>\n");
        return page;
    }

    // Outcome banner from the __RUN__ row (fallback: unknown).
    let outcome = run_row.map_or("", |r| r.outcome.as_str());
    let (class, text) = outcome_banner(outcome);
    page.push_str(&format!("<div class=\"banner {class}\">{text}</div>\n"));
    page.push_str(&format!("<p class=\"honesty\">{HONESTY_LINE}</p>\n"));

    if let Some(run) = run_row {
        page.push_str(&format!(
            "<p class=\"noise\">Run totals: minutes compared {mc} · cells {cc} · \
             diverged {dv} · missing live {ml} · missing BruteX {mb} · tail unsealed {tu} \
             · out of session {oos} · unmapped symbols {um}{partial}</p>\n",
            mc = count_str(run.minutes_compared),
            cc = count_str(run.cells_compared),
            dv = count_str(run.diverged_cells),
            ml = count_str(run.missing_live),
            mb = count_str(run.missing_brutex),
            tu = count_str(run.tail_unsealed),
            oos = count_str(run.out_of_session),
            um = count_str(run.unmapped_symbols),
            partial = if run.run_partial {
                " · <strong>partial evidence</strong>"
            } else {
                ""
            },
        ));
        if !run.note.is_empty() {
            page.push_str(&format!(
                "<p class=\"noise\">Noise band / run note: {}</p>\n",
                escape_html(&run.note)
            ));
        }
    }

    // Per-symbol table (worst offenders first; caller pre-sorts).
    page.push_str("<h2>Per-symbol summary</h2>\n<table>\n<thead><tr>");
    for h in [
        "Symbol",
        "Segment",
        "Outcome",
        "Minutes",
        "Cells",
        "Diverged",
        "Missing live",
        "Missing BruteX",
        "Tail unsealed",
        "Out of session",
    ] {
        page.push_str(&format!("<th>{h}</th>"));
    }
    page.push_str("</tr></thead>\n<tbody>\n");
    for r in symbol_rows {
        let sym_esc = escape_html(&r.symbol);
        page.push_str(&format!(
            "<tr><td class=\"l\"><a href=\"/crossverify?date={date_esc}&amp;symbol={}\">{sym_esc}</a></td>\
             <td class=\"l\" title=\"security id {}\">{}</td><td class=\"l\">{}</td>\
             <td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            url_encode_symbol(&r.symbol),
            count_str(r.security_id),
            escape_html(&r.exchange_segment),
            escape_html(&r.outcome),
            count_str(r.minutes_compared),
            count_str(r.cells_compared),
            count_str(r.diverged_cells),
            count_str(r.missing_live),
            count_str(r.missing_brutex),
            count_str(r.tail_unsealed),
            count_str(r.out_of_session),
        ));
    }
    if symbol_rows.is_empty() {
        page.push_str(
            "<tr><td class=\"l\" colspan=\"10\">no per-symbol rows for this day</td></tr>\n",
        );
    }
    page.push_str("</tbody>\n</table>\n");

    // Optional drill-down.
    if let Some((symbol, cells)) = drill {
        let sym_esc = escape_html(symbol);
        page.push_str(&format!(
            "<h2>Drill-down — {sym_esc}</h2>\n\
             <p class=\"sub\">Per-minute audit cells for this symbol (capped at \
             {CELL_DRILLDOWN_LIMIT} rows; pinned to the day's run so superseded \
             reruns are excluded). Prices in rupees.</p>\n",
        ));
        page.push_str("<table>\n<thead><tr>");
        for h in [
            "Minute (IST)",
            "Kind",
            "Field",
            "BruteX ₹",
            "Live ₹",
            "Diff ₹",
            "BruteX vol",
            "Live vol",
        ] {
            page.push_str(&format!("<th>{h}</th>"));
        }
        page.push_str("</tr></thead>\n<tbody>\n");
        for c in cells {
            let diff = if c.brutex_paise != -1 && c.live_paise != -1 {
                paise_signed(c.brutex_paise - c.live_paise)
            } else {
                "?".to_string()
            };
            page.push_str(&format!(
                "<tr><td class=\"l\">{}</td><td class=\"l\">{}</td><td class=\"l\">{}</td>\
                 <td>{}</td><td>{}</td><td>{diff}</td><td>{}</td><td>{}</td></tr>\n",
                minute_hhmm(&c.minute_ts_ist),
                escape_html(&c.kind),
                escape_html(&c.field),
                paise_cell(c.brutex_paise),
                paise_cell(c.live_paise),
                count_str(c.brutex_volume),
                count_str(c.live_volume),
            ));
        }
        if cells.is_empty() {
            page.push_str(
                "<tr><td class=\"l\" colspan=\"8\">no audit cells recorded for this \
                 symbol on this day (a clean symbol writes no cells)</td></tr>\n",
            );
        }
        page.push_str("</tbody>\n</table>\n");
        page.push_str(&format!(
            "<p><a href=\"/crossverify?date={date_esc}\">← back to all symbols</a></p>\n"
        ));
    }

    push_footer(&mut page);
    page.push_str("</body>\n</html>\n");
    page
}

/// Shared "how to read this" footer + link back.
fn push_footer(page: &mut String) {
    page.push_str(
        "<div class=\"footer\"><strong>How to read this:</strong> \
         <em>diverged</em> = both sides had the minute and a price field differed \
         beyond tolerance; <em>missing live</em> = BruteX has the minute, our live \
         capture does not (the live side only bars minutes that ticked); \
         <em>missing BruteX</em> = we have the minute, the vendor file does not; \
         <em>tail unsealed</em> = 15:28/15:29 close-seal timing, not a defect; \
         <em>out of session</em> = vendor rows outside [09:15, 15:30) IST; \
         <em>?</em> = not measured (never rendered as 0). \
         <a href=\"/crossverify\">latest day</a> · <a href=\"/dashboard\">dashboard</a>\
         </div>\n",
    );
}

// ---------------------------------------------------------------------------
// Thin I/O shell
// ---------------------------------------------------------------------------

/// Runs one QuestDB `/exec` query and returns the `dataset` rows. `None` on
/// any transport / non-JSON / missing-table failure (the caller renders the
/// honest empty state).
///
/// Covered by the mock-QuestDB handler tests below (happy path, connection
/// refused, non-JSON body, missing `dataset` key).
async fn query_dataset(
    client: &reqwest::Client,
    base_url: &str,
    sql: &str,
) -> Option<Vec<serde_json::Value>> {
    let url = format!("{base_url}/exec");
    let resp = client
        .get(&url)
        .query(&[("query", sql)])
        .send()
        .await
        .ok()?;
    let mut body: serde_json::Value = resp.json().await.ok()?;
    match body.get_mut("dataset").map(serde_json::Value::take) {
        Some(serde_json::Value::Array(rows)) => Some(rows),
        _ => None,
    }
}

/// `GET /crossverify` — public read-only BruteX ⇄ live cross-verify report.
///
/// Thin async shell: validate params (fail-closed 400), run the QuestDB
/// queries, hand everything to the pure renderers above.
///
/// Covered end-to-end by the mock-QuestDB handler tests below (400 params,
/// latest-date resolution, explicit-date drill-down, degraded/empty states).
pub async fn crossverify_page(
    State(state): State<SharedAppState>,
    Query(params): Query<CrossverifyParams>,
) -> Response {
    // Fail-closed param validation.
    let date_micros_from_param = match &params.date {
        Some(d) => match date_param_to_micros(d) {
            Some(m) => Some(m),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    "invalid date parameter — expected YYYY-MM-DD",
                )
                    .into_response();
            }
        },
        None => None,
    };
    if let Some(sym) = &params.symbol
        && !is_valid_symbol_param(sym)
    {
        return (
            StatusCode::BAD_REQUEST,
            "invalid symbol parameter — max 64 chars of [A-Za-z0-9 ._&-]",
        )
            .into_response();
    }

    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = state.questdb_http_client();

    // Resolve the trading day: explicit param, else the latest recorded day.
    let (date_label, date_micros) = match (&params.date, date_micros_from_param) {
        (Some(d), Some(m)) => (d.clone(), Some(m)),
        _ => {
            let latest = query_dataset(client, &base_url, &max_date_sql())
                .await
                .as_deref()
                .and_then(extract_first_string)
                .and_then(|iso| {
                    let day = iso.get(0..10)?.to_string();
                    let micros = date_param_to_micros(&day)?;
                    Some((day, micros))
                });
            match latest {
                Some((day, micros)) => (day, Some(micros)),
                None => ("latest".to_string(), None),
            }
        }
    };

    // Daily rows (None date ⇒ table missing/empty ⇒ honest empty state).
    let (run_row, mut symbol_rows) = match date_micros {
        Some(micros) => {
            let rows = query_dataset(client, &base_url, &daily_select_sql(micros))
                .await
                .map(|rs| rs.iter().filter_map(parse_daily_row).collect::<Vec<_>>())
                .unwrap_or_default();
            split_run_row(rows)
        }
        None => (None, Vec::new()),
    };
    sort_symbol_rows(&mut symbol_rows);

    // Optional drill-down, pinned to the run row's observed_at.
    let drill_cells = match (&params.symbol, date_micros) {
        (Some(sym), Some(micros)) => {
            let observed = run_row
                .as_ref()
                .map(|r| r.observed_at.as_str())
                .filter(|o| is_iso_ts_shape(o));
            match observed {
                Some(obs) => query_dataset(client, &base_url, &cell_select_sql(micros, sym, obs))
                    .await
                    .map(|rs| rs.iter().filter_map(parse_cell_row).collect::<Vec<_>>())
                    .unwrap_or_default(),
                None => Vec::new(),
            }
        }
        _ => Vec::new(),
    };
    let drill = params
        .symbol
        .as_deref()
        .map(|sym| (sym, drill_cells.as_slice()));

    let html = render_crossverify_page(&date_label, run_row.as_ref(), &symbol_rows, drill);
    (
        [
            (header::X_FRAME_OPTIONS, "SAMEORIGIN"),
            (header::CONTENT_SECURITY_POLICY, CROSSVERIFY_PAGE_CSP),
        ],
        Html(html),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Tests (pure fns only — the I/O shell is TEST-EXEMPT per repo convention)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn daily(symbol: &str, outcome: &str, diverged: i64) -> DailyRow {
        DailyRow {
            symbol: symbol.to_string(),
            security_id: 11536,
            exchange_segment: "NSE_EQ".to_string(),
            outcome: outcome.to_string(),
            minutes_compared: 375,
            cells_compared: 1500,
            diverged_cells: diverged,
            missing_live: 2,
            missing_brutex: 0,
            tail_unsealed: 1,
            out_of_session: 0,
            unmapped_symbols: -1,
            run_partial: false,
            observed_at: "2026-07-12T10:20:00.000000Z".to_string(),
            note: String::new(),
        }
    }

    fn run_row(outcome: &str, note: &str) -> DailyRow {
        let mut r = daily(BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL, outcome, 3);
        r.security_id = -1;
        r.exchange_segment = "none".to_string();
        r.unmapped_symbols = 1;
        r.note = note.to_string();
        r
    }

    // ---- escape_html ----

    #[test]
    fn escape_html_neutralizes_script_payload() {
        let out = escape_html("<script>alert(1)</script>");
        assert_eq!(out, "&lt;script&gt;alert(1)&lt;/script&gt;");
        assert!(!out.contains("<script>"));
    }

    #[test]
    fn escape_html_covers_all_five_metacharacters() {
        assert_eq!(escape_html(r#"&<>"'"#), "&amp;&lt;&gt;&quot;&#39;");
        assert_eq!(escape_html("plain-Symbol_1.NS"), "plain-Symbol_1.NS");
    }

    // ---- date validation ----

    #[test]
    fn date_shape_accepts_valid_and_rejects_garbage() {
        assert!(is_valid_date_shape("2026-07-12"));
        assert!(!is_valid_date_shape("2026-7-12"));
        assert!(!is_valid_date_shape("2026-07-12T00"));
        assert!(!is_valid_date_shape("abcd-ef-gh"));
        assert!(!is_valid_date_shape(""));
        assert!(!is_valid_date_shape("2026/07/12"));
    }

    #[test]
    fn date_param_to_micros_known_values() {
        // 1970-01-01 = epoch.
        assert_eq!(date_param_to_micros("1970-01-01"), Some(0));
        // 2020-01-01 = 18262 days since epoch.
        assert_eq!(
            date_param_to_micros("2020-01-01"),
            Some(18_262 * 86_400_000_000)
        );
    }

    #[test]
    fn date_param_to_micros_rejects_impossible_dates() {
        assert_eq!(date_param_to_micros("2026-13-01"), None);
        assert_eq!(date_param_to_micros("2026-00-01"), None);
        assert_eq!(date_param_to_micros("2026-02-30"), None);
        assert_eq!(date_param_to_micros("2026-04-31"), None);
        assert_eq!(date_param_to_micros("2025-02-29"), None); // not a leap year
        assert!(date_param_to_micros("2024-02-29").is_some()); // leap year
    }

    // ---- symbol validation ----

    #[test]
    fn symbol_param_allowlist() {
        assert!(is_valid_symbol_param("NIFTY 50"));
        assert!(is_valid_symbol_param("TCS.NS"));
        assert!(is_valid_symbol_param("M_M-EQ"));
        // FIX ROUND 3: `&` accepted in lockstep with the parse charset —
        // real NSE symbols M&M / M&MFIN / GMRP&UI must drill down.
        assert!(is_valid_symbol_param("M&M"));
        assert!(is_valid_symbol_param("GMRP&UI"));
        assert!(!is_valid_symbol_param(""));
        assert!(!is_valid_symbol_param("a'b"));
        assert!(!is_valid_symbol_param("A/B"));
        assert!(!is_valid_symbol_param("<script>"));
        assert!(!is_valid_symbol_param(&"x".repeat(65)));
        assert!(is_valid_symbol_param(&"x".repeat(64)));
    }

    // ---- SQL builders / quoting ----

    #[test]
    fn sql_quote_doubles_single_quotes() {
        assert_eq!(sql_quote("a'b"), "a''b");
        assert_eq!(sql_quote("clean"), "clean");
    }

    #[test]
    fn daily_select_sql_uses_micros_literal_and_table() {
        let sql = daily_select_sql(1_777_000_000_000_000);
        assert!(sql.contains("FROM brutex_crossverify_daily"));
        assert!(sql.contains("trading_date_ist = 1777000000000000"));
        assert!(sql.contains("run_partial"));
        assert!(sql.contains("LIMIT 2000"), "defensive daily-row cap");
    }

    #[test]
    fn cell_select_sql_pins_observed_at_and_escapes_symbol() {
        let sql = cell_select_sql(42, "a'b", "2026-07-12T10:20:00.000000Z");
        assert!(sql.contains("FROM brutex_crossverify_cell_audit"));
        assert!(sql.contains("symbol = 'a''b'"));
        assert!(sql.contains("observed_at = '2026-07-12T10:20:00.000000Z'"));
        assert!(sql.contains("LIMIT 2000"));
        assert!(sql.contains("ORDER BY minute_ts_ist"));
        // FIX ROUND 3: `&` rides inside the single-quoted literal untouched.
        let sql_amp = cell_select_sql(42, "M&M", "2026-07-12T10:20:00.000000Z");
        assert!(sql_amp.contains("symbol = 'M&M'"));
    }

    #[test]
    fn max_date_sql_targets_daily_table() {
        assert!(max_date_sql().contains("max(trading_date_ist)"));
        assert!(max_date_sql().contains("brutex_crossverify_daily"));
    }

    #[test]
    fn iso_ts_shape_gate() {
        assert!(is_iso_ts_shape("2026-07-12T10:20:00.000000Z"));
        assert!(!is_iso_ts_shape(""));
        assert!(!is_iso_ts_shape("2026-07-12'; DROP TABLE x"));
    }

    // ---- formatting ----

    #[test]
    fn paise_formatting_rupees_and_sentinel() {
        assert_eq!(paise_cell(12345), "123.45");
        assert_eq!(paise_cell(5), "0.05");
        assert_eq!(paise_cell(-1), "?");
        assert_eq!(paise_signed(-230), "-2.30");
        assert_eq!(paise_signed(-1), "-0.01"); // computed diff — NOT a sentinel
        assert_eq!(paise_signed(0), "0.00");
    }

    #[test]
    fn count_str_sentinel() {
        assert_eq!(count_str(-1), "?");
        assert_eq!(count_str(0), "0");
        assert_eq!(count_str(375), "375");
    }

    #[test]
    fn minute_hhmm_extracts_time() {
        assert_eq!(minute_hhmm("2026-07-12T09:15:00.000000Z"), "09:15");
        assert_eq!(minute_hhmm("garbage"), "garbage");
    }

    #[test]
    fn url_encode_symbol_encodes_space() {
        assert_eq!(url_encode_symbol("NIFTY 50"), "NIFTY%2050");
        assert_eq!(url_encode_symbol("TCS.NS"), "TCS.NS");
        // FIX ROUND 3: `&` percent-encodes so it can never split the query.
        assert_eq!(url_encode_symbol("M&M"), "M%26M");
        assert_eq!(url_encode_symbol("GMRP&UI"), "GMRP%26UI");
    }

    // ---- parsers ----

    #[test]
    fn parse_daily_row_maps_columns_and_null_sentinels() {
        let row = json!([
            "TCS",
            11536,
            "NSE_EQ",
            "diverged",
            375,
            1500,
            7,
            2,
            0,
            1,
            0,
            null,
            true,
            "2026-07-12T10:20:00.000000Z",
            "n"
        ]);
        let r = parse_daily_row(&row).unwrap();
        assert_eq!(r.symbol, "TCS");
        assert_eq!(r.diverged_cells, 7);
        assert_eq!(r.unmapped_symbols, -1); // null → sentinel, never 0
        assert!(r.run_partial);
        assert_eq!(r.note, "n");
        assert!(parse_daily_row(&json!("not-an-array")).is_none());
    }

    #[test]
    fn parse_cell_row_maps_columns() {
        let row = json!([
            "2026-07-12T09:15:00.000000Z",
            "diverged",
            "close",
            12345,
            12340,
            null,
            900
        ]);
        let c = parse_cell_row(&row).unwrap();
        assert_eq!(c.field, "close");
        assert_eq!(c.brutex_paise, 12345);
        assert_eq!(c.brutex_volume, -1);
        assert_eq!(c.live_volume, 900);
    }

    #[test]
    fn extract_first_string_reads_max_query_shape() {
        let ds = vec![json!(["2026-07-12T00:00:00.000000Z"])];
        assert_eq!(
            extract_first_string(&ds).as_deref(),
            Some("2026-07-12T00:00:00.000000Z")
        );
        assert!(extract_first_string(&[]).is_none());
        assert!(extract_first_string(&[json!([null])]).is_none());
    }

    // ---- split / sort ----

    #[test]
    fn split_run_row_separates_aggregate() {
        let rows = vec![
            daily("TCS", "clean", 0),
            run_row("clean", ""),
            daily("INFY", "diverged", 3),
        ];
        let (run, symbols) = split_run_row(rows);
        assert_eq!(run.unwrap().symbol, BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL);
        assert_eq!(symbols.len(), 2);
    }

    #[test]
    fn sort_symbol_rows_diverged_desc_then_symbol() {
        let mut rows = vec![
            daily("ZEE", "diverged", 3),
            daily("TCS", "clean", 0),
            daily("INFY", "diverged", 3),
        ];
        sort_symbol_rows(&mut rows);
        assert_eq!(rows[0].symbol, "INFY"); // tie broken alphabetically
        assert_eq!(rows[1].symbol, "ZEE");
        assert_eq!(rows[2].symbol, "TCS");
    }

    // ---- renderer ----

    #[test]
    fn render_empty_state_is_honest() {
        let page = render_crossverify_page("latest", None, &[], None);
        assert!(page.contains("no cross-verify runs recorded yet"));
        assert!(page.contains("Neither side is ground truth"));
        assert!(page.contains("How to read this"));
    }

    #[test]
    fn render_clean_day_shows_ok_banner_and_honesty_line() {
        let run = run_row("clean", "noise p95=2 max=4 paise");
        let rows = vec![daily("TCS", "clean", 0)];
        let page = render_crossverify_page("2026-07-12", Some(&run), &rows, None);
        assert!(page.contains("banner ok"));
        assert!(page.contains("CLEAN — every compared cell within tolerance"));
        assert!(page.contains("Neither side is ground truth"));
        assert!(page.contains("Noise band / run note: noise p95=2 max=4 paise"));
        assert!(page.contains("2026-07-12"));
        // symbol link carries date + symbol for the drill-down
        assert!(page.contains("/crossverify?date=2026-07-12&amp;symbol=TCS"));
        // the __RUN__ row never appears in the tbody (caller splits it out)
        assert!(!page.contains(">__RUN__<"));
    }

    #[test]
    fn render_no_data_banner_is_loud() {
        let run = run_row("no_data", "");
        let page = render_crossverify_page("2026-07-12", Some(&run), &[], None);
        assert!(page.contains("banner bad"));
        assert!(page.contains("NO DATA — the vendor side produced nothing"));
    }

    #[test]
    fn render_diverged_day_with_drilldown_rows() {
        let run = run_row("diverged", "");
        let mut rows = vec![
            daily("TCS", "clean", 0),
            daily("INFY", "diverged", 9),
            daily("ZEE", "diverged", 2),
        ];
        sort_symbol_rows(&mut rows);
        let cells = vec![
            CellRow {
                minute_ts_ist: "2026-07-12T09:15:00.000000Z".to_string(),
                kind: "diverged".to_string(),
                field: "close".to_string(),
                brutex_paise: 12345,
                live_paise: 12115,
                brutex_volume: -1,
                live_volume: 900,
            },
            CellRow {
                minute_ts_ist: "2026-07-12T09:16:00.000000Z".to_string(),
                kind: "missing_live".to_string(),
                field: "none".to_string(),
                brutex_paise: 12345,
                live_paise: -1,
                brutex_volume: -1,
                live_volume: -1,
            },
        ];
        let page = render_crossverify_page("2026-07-12", Some(&run), &rows, Some(("INFY", &cells)));
        // offender ordering: INFY (9) before ZEE (2) before TCS (0)
        let (i, z, t) = (
            page.find(">INFY<").unwrap(),
            page.find(">ZEE<").unwrap(),
            page.find(">TCS<").unwrap(),
        );
        assert!(i < z && z < t, "worst offender must render first");
        // rupee formatting: 12345 paise → 123.45; diff 230 paise → 2.30
        assert!(page.contains("123.45"));
        assert!(page.contains(">2.30<"));
        // sentinel -1 renders "?" for missing live price + volumes
        assert!(page.contains(">?<"));
        assert!(page.contains("09:15"));
        assert!(page.contains("Drill-down — INFY"));
        assert!(page.contains("back to all symbols"));
    }

    #[test]
    fn render_escapes_hostile_symbol_and_note() {
        let mut run = run_row("degraded", "<script>alert(1)</script>");
        run.note = "<script>alert(1)</script>".to_string();
        let hostile = daily("BAD", "clean", 0);
        // symbol charset is validated at the handler, but render must be safe
        // by construction anyway — inject via note + outcome fields.
        let mut evil = hostile;
        evil.outcome = "<img src=x onerror=alert(1)>".to_string();
        let page = render_crossverify_page("2026-07-12", Some(&run), &[evil], None);
        assert!(!page.contains("<script>"));
        assert!(!page.contains("<img src=x"));
        assert!(page.contains("&lt;script&gt;"));
    }

    /// FIX ROUND 3: an M&M-class symbol renders HTML-escaped (`M&amp;M`)
    /// and its drill-down href carries the percent-encoded form (`M%26M`).
    #[test]
    fn render_ampersand_symbol_escapes_html_and_encodes_href() {
        let run = run_row("clean", "");
        let rows = vec![daily("M&M", "clean", 0)];
        let page = render_crossverify_page("2026-07-12", Some(&run), &rows, None);
        assert!(page.contains("M&amp;M"));
        assert!(page.contains("symbol=M%26M"));
        assert!(!page.contains(">M&M<"), "raw ampersand must never render");
    }

    #[test]
    fn render_drilldown_empty_cells_message() {
        let run = run_row("clean", "");
        let rows = vec![daily("TCS", "clean", 0)];
        let page = render_crossverify_page("2026-07-12", Some(&run), &rows, Some(("TCS", &[])));
        assert!(page.contains("no audit cells recorded for this symbol"));
    }

    #[test]
    fn outcome_banner_covers_all_wire_labels() {
        assert_eq!(outcome_banner("clean").0, "ok");
        assert_eq!(outcome_banner("diverged").0, "warn");
        for o in ["partial", "no_data", "blind", "degraded", "???"] {
            assert_eq!(outcome_banner(o).0, "bad", "{o} must be loud");
        }
        assert!(outcome_banner("<x>").1.contains("&lt;x&gt;"));
    }

    // ---- async handler shell (in-process mock QuestDB — board.rs pattern) ----

    use crate::feed_state::FeedRuntimeState;
    use std::sync::Arc;
    use tickvault_common::config::{DhanConfig, FeedsConfig, InstrumentConfig, QuestDbConfig};

    /// State pointing QuestDB at a specific port (1 = nothing listens →
    /// connection refused fast, exercising the honest degraded path).
    fn test_state_with_port(http_port: u16) -> SharedAppState {
        let health = Arc::new(crate::state::SystemHealthStatus::new());
        SharedAppState::new_with_feed_runtime(
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port,
                pg_port: 8812,
                ilp_port: 9009,
            },
            DhanConfig {
                websocket_url: "wss://api-feed.dhan.co".to_string(),
                order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
                rest_api_base_url: "https://api.dhan.co/v2".to_string(),
                auth_base_url: "https://auth.dhan.co".to_string(),
                instrument_csv_url: "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"
                    .to_string(),
                instrument_csv_fallback_url: "https://images.dhan.co/api-data/api-scrip-master.csv"
                    .to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
                sandbox_base_url: String::new(),
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            health,
            Arc::new(FeedRuntimeState::from_config(&FeedsConfig::default())),
        )
    }

    /// Multi-request mock QuestDB serving one canned response per connection,
    /// then EXHAUSTING (further connections refused) — the board.rs / #1458
    /// house pattern.
    async fn start_multi_mock_server(responses: Vec<&'static str>) -> u16 {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let port = listener.local_addr().expect("local_addr").port();

        tokio::spawn(async move {
            for body in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        port
    }

    async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        String::from_utf8(bytes.to_vec()).expect("utf8 body")
    }

    /// Canned daily /exec body: one `__RUN__` aggregate + one symbol row
    /// (column order = `daily_select_sql`).
    const DAILY_BODY: &str = r#"{"dataset":[
        ["__RUN__",0,"","diverged",375,1500,3,1,0,2,0,0,false,"2026-07-11T15:50:00.000000Z","tolerance 0 paise"],
        ["RELIANCE",2885,"NSE_EQ","diverged",375,1500,3,1,0,2,0,0,false,"2026-07-11T15:50:00.000000Z",""]
    ]}"#;
    const MAX_DATE_BODY: &str = r#"{"dataset":[["2026-07-11T00:00:00.000000Z"]]}"#;
    const CELLS_BODY: &str = r#"{"dataset":[
        ["2026-07-11T10:15:00.000000Z","diverged","close",123450,123460,0,0]
    ]}"#;

    #[tokio::test]
    async fn crossverify_page_rejects_bad_date_with_400() {
        let resp = crossverify_page(
            State(test_state_with_port(1)),
            Query(CrossverifyParams {
                date: Some("2026/07/11".to_string()),
                symbol: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn crossverify_page_rejects_bad_symbol_with_400() {
        let resp = crossverify_page(
            State(test_state_with_port(1)),
            Query(CrossverifyParams {
                date: Some("2026-07-11".to_string()),
                symbol: Some("a'b".to_string()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn crossverify_page_questdb_down_renders_honest_empty_state() {
        // Port 1: connection refused → both queries return None → honest
        // "no runs yet" state with the "latest" label (never a fake clean page).
        let resp = crossverify_page(
            State(test_state_with_port(1)),
            Query(CrossverifyParams {
                date: None,
                symbol: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("no cross-verify runs recorded yet"));
        assert!(body.contains("latest"));
    }

    #[tokio::test]
    async fn crossverify_page_latest_date_path_renders_run_and_symbol_rows() {
        // No date param → 2 queries: max(trading_date_ist), then daily rows.
        let port = start_multi_mock_server(vec![MAX_DATE_BODY, DAILY_BODY]).await;
        let resp = crossverify_page(
            State(test_state_with_port(port)),
            Query(CrossverifyParams {
                date: None,
                symbol: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::X_FRAME_OPTIONS)
                .map(|v| v.as_bytes()),
            Some(b"SAMEORIGIN".as_slice())
        );
        let body = body_text(resp).await;
        assert!(body.contains("2026-07-11"), "resolved latest day in title");
        assert!(body.contains("DIVERGED"), "run-row banner");
        assert!(body.contains("RELIANCE"), "per-symbol row");
        assert!(body.contains("Run totals"), "run-row totals line");
        assert!(body.contains("tolerance 0 paise"), "run note rendered");
    }

    #[tokio::test]
    async fn crossverify_page_explicit_date_with_symbol_renders_drilldown() {
        // Explicit date + symbol → 2 queries: daily rows, then audit cells
        // pinned to the run row's observed_at.
        let port = start_multi_mock_server(vec![DAILY_BODY, CELLS_BODY]).await;
        let resp = crossverify_page(
            State(test_state_with_port(port)),
            Query(CrossverifyParams {
                date: Some("2026-07-11".to_string()),
                symbol: Some("RELIANCE".to_string()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("Drill-down — RELIANCE"));
        assert!(body.contains("10:15"), "cell minute rendered HH:MM IST");
        assert!(body.contains("close"), "cell field rendered");
        assert!(body.contains("1234.50"), "paise rendered as rupees");
    }

    #[tokio::test]
    async fn crossverify_page_symbol_without_run_row_skips_cell_query() {
        // Daily body has NO __RUN__ row → observed_at gate fails closed →
        // the cell query is never issued (mock serves exactly ONE response).
        const SYMBOL_ONLY_BODY: &str = r#"{"dataset":[
            ["TCS",11536,"NSE_EQ","clean",375,1500,0,0,0,0,0,0,false,"2026-07-11T15:50:00.000000Z",""]
        ]}"#;
        let port = start_multi_mock_server(vec![SYMBOL_ONLY_BODY]).await;
        let resp = crossverify_page(
            State(test_state_with_port(port)),
            Query(CrossverifyParams {
                date: Some("2026-07-11".to_string()),
                symbol: Some("TCS".to_string()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("no audit cells recorded for this symbol"));
    }

    #[tokio::test]
    async fn crossverify_page_malformed_exec_body_renders_empty_state() {
        let port = start_multi_mock_server(vec!["definitely not json"]).await;
        let resp = crossverify_page(
            State(test_state_with_port(port)),
            Query(CrossverifyParams {
                date: Some("2026-07-11".to_string()),
                symbol: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("no cross-verify runs recorded yet"));
    }

    #[tokio::test]
    async fn crossverify_page_missing_dataset_key_renders_empty_state() {
        let port = start_multi_mock_server(vec![r#"{"query":"x","count":0}"#]).await;
        let resp = crossverify_page(
            State(test_state_with_port(port)),
            Query(CrossverifyParams {
                date: Some("2026-07-11".to_string()),
                symbol: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("no cross-verify runs recorded yet"));
    }
}
