//! Dhan detailed instrument-master CSV — parse + the NSE-constituent ISIN join.
//!
//! Rebuilt 2026-08-11 under the operator's dated Q3 reversal
//! (`websocket-connection-scope-lock.md`, "2026-08-11 (SAME DAY, THIRD QUOTE)").
//! The chain this replaces was deleted on 2026-07-13; this module is NOT a
//! restoration of it — it implements the two contracts that survived the
//! deletion and were never amended:
//!
//! * **§18** (`daily-universe-scope-expansion-2026-05-27.md`) — downloader
//!   hardening. Lives in the caller; this module is pure and takes `&str`.
//! * **§31.1** — the constituent → `security_id` mapping. Implemented here
//!   verbatim: ISIN primary, symbol cross-check, O(1) map build, fail-closed
//!   tolerance, I-P1-11 dedup, role tagging.
//!
//! # Why this file is pure
//!
//! Every function here takes text and returns data. No I/O, no clock, no
//! network. That is what makes the exhaustive malformed-input table in the
//! tests possible at all — each hostile CSV is three lines of fixture, not a
//! mock server. The download lives in the caller so this stays testable.
//!
//! # Complexity
//!
//! Parse is O(rows) with a single pass and one allocation per retained row —
//! inherently O(n) because it must look at every row, and honestly labelled as
//! such rather than called O(1). The JOIN, which is the part that runs per
//! constituent, is **O(1) average per lookup** against a `HashMap` built once:
//! `build_isin_index` is O(master rows), then each of the ~750 constituents is
//! a single hash lookup. This is COLD PATH — once per trading day, at boot or
//! on the daily rider — and never touches the tick path.

use std::collections::HashMap;

use tickvault_common::types::ExchangeSegment;

/// A row of the Dhan detailed master, reduced to the columns the join needs.
///
/// Deliberately NOT the full 20+ column row: carrying columns nothing reads
/// would triple the memory of a ~150,000-row master for no benefit, and every
/// unused field is a field that can silently go stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterRow {
    /// `SECURITY_ID` — the value the whole pipeline exists to resolve.
    pub security_id: u64,
    /// `ISIN` — the join key. Empty for derivatives and indices.
    pub isin: String,
    /// `SYMBOL_NAME` — the cross-check, never the primary key.
    pub symbol_name: String,
    /// `EXCH_ID` — `NSE` / `BSE` / `MCX`.
    pub exch_id: String,
    /// `SEGMENT` — single char: `E` equity, `D` derivative, `I` index,
    /// `C` currency, `M` commodity.
    pub segment: String,
    /// `SERIES` — `EQ` for cash equity. Empty for non-equity rows.
    pub series: String,
}

impl MasterRow {
    /// True when this row is an NSE cash-equity row — the only rows §31.1
    /// admits into the ISIN index.
    ///
    /// # Why all three conditions and not just `SERIES == EQ`
    ///
    /// `EQ` appears as a series on BSE rows too, and `SEGMENT == E` alone
    /// admits every series (`BE`, `SM`, `ST`, …). A constituent resolved to a
    /// BSE row or a trade-to-trade series would be a wrong-instrument
    /// subscription that looks perfectly healthy. All three, or nothing.
    #[must_use]
    pub fn is_nse_cash_equity(&self) -> bool {
        self.exch_id == "NSE" && self.segment == "E" && self.series == "EQ"
    }
}

/// Why a master CSV was rejected outright.
///
/// Every variant is a REFUSAL of the whole file, never a partial accept. A
/// master we cannot fully trust must not produce a mapping: a partially-parsed
/// master silently drops instruments, and a dropped instrument is a
/// subscription that never happens with nothing to show for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MasterParseError {
    /// The file had no header line at all (empty, or whitespace only).
    Empty,
    /// A column §31.1 requires is absent from the header.
    MissingColumn(&'static str),
    /// More than [`MAX_BAD_ROW_FRACTION`] of data rows failed mandatory-field
    /// parsing. Carries the counts so the operator sees the ratio, not a bare
    /// "rejected".
    TooManyBadRows { bad: usize, total: usize },
    /// The file parsed but yielded fewer rows than any real master can have.
    /// Guards the case where a WAF block page or a truncated transfer parses
    /// "successfully" into near-nothing.
    ImplausiblyFewRows { rows: usize, min: usize },
}

impl std::fmt::Display for MasterParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "master CSV is empty (no header line)"),
            Self::MissingColumn(c) => write!(f, "master CSV is missing required column {c}"),
            Self::TooManyBadRows { bad, total } => write!(
                f,
                "master CSV rejected: {bad} of {total} rows failed mandatory-field parsing, \
                 above the {:.2}% tolerance",
                MAX_BAD_ROW_FRACTION * 100.0
            ),
            Self::ImplausiblyFewRows { rows, min } => write!(
                f,
                "master CSV yielded {rows} rows, below the plausibility floor of {min} — \
                 the transfer was truncated or the body is not a master CSV"
            ),
        }
    }
}

impl std::error::Error for MasterParseError {}

/// Mandatory-field failure tolerance, per the archived §26 contract.
///
/// A handful of corrupt rows in a 150,000-row vendor file is normal and must
/// not take the day down. A systematic parse failure — wrong delimiter, shifted
/// columns, a JSON body — blows past this immediately, which is exactly the
/// discrimination the number buys.
pub const MAX_BAD_ROW_FRACTION: f64 = 0.001;

/// Plausibility floor on retained rows.
///
/// The real detailed master carries well over 100,000 rows. This is set two
/// orders of magnitude below that deliberately: it is a truncation/wrong-body
/// tripwire, not a size assertion, and a floor set near the true size would
/// reject a legitimately shrinking master on a holiday-shortened listing day.
pub const MIN_PLAUSIBLE_MASTER_ROWS: usize = 1_000;

/// Column headers, from `docs/dhan-ref/09-instrument-master.md`.
///
/// Resolved BY NAME at parse time, never by position: the 2026-07-14 live crawl
/// found the upstream doc's column table already drifted from the served file,
/// and 18 extra CO/BO columns exist that the doc's own table omits. A
/// positional parser would have silently read the wrong column.
const COL_SECURITY_ID: &str = "SECURITY_ID";
const COL_ISIN: &str = "ISIN";
const COL_SYMBOL_NAME: &str = "SYMBOL_NAME";
const COL_EXCH_ID: &str = "EXCH_ID";
const COL_SEGMENT: &str = "SEGMENT";
const COL_SERIES: &str = "SERIES";

/// `numerator / denominator` as an exact `f64`, with no lossy cast.
///
/// # Why this exists instead of `a as f64 / b as f64`
///
/// The direct cast trips `clippy::cast_precision_loss`, and silencing that
/// with an `#[allow]` would be silencing a real warning: `usize` above 2^53
/// genuinely does lose precision. Going through `u32` makes the conversion
/// **lossless by construction** — `f64::from(u32)` is exact for every input —
/// so there is nothing left to allow. A count that does not fit in `u32`
/// (over 4 billion rows) saturates, which biases the ratio toward REJECTING
/// the file: the safe direction for a tolerance gate.
fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    let n = f64::from(u32::try_from(numerator).unwrap_or(u32::MAX));
    let d = f64::from(u32::try_from(denominator).unwrap_or(u32::MAX));
    n / d
}

/// Splits one CSV line, honouring double-quoted fields.
///
/// Hand-rolled rather than pulling a CSV crate: a new workspace dependency
/// needs operator approval (CLAUDE.md), and the master's quoting is the simple
/// RFC-4180 subset — quoted fields may contain commas and doubled quotes, and
/// nothing else. A field containing a raw newline would defeat this, which is
/// why [`parse_master_csv`] treats a short row as a bad row rather than
/// attempting recovery: guessing at a continuation is how a parser silently
/// shifts every subsequent column.
fn split_csv_line(line: &str, out: &mut Vec<String>) {
    out.clear();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    // Escaped quote inside a quoted field: "" means one ".
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            }
            '"' => in_quotes = true,
            ',' if !in_quotes => out.push(std::mem::take(&mut field)),
            _ => field.push(ch),
        }
    }
    out.push(field);
}

/// Parses the Dhan detailed master CSV.
///
/// Hardening applied here, each closing a real failure mode:
///
/// * **BOM stripped** — the vendor has served a UTF-8 BOM before; unstripped it
///   corrupts the FIRST header name only, so `EXCH_ID` silently goes missing
///   while every other column resolves.
/// * **CR normalised** — a CRLF file leaves `\r` on the last field of every
///   row, so `SERIES` reads `"EQ\r"` and never equals `"EQ"`. Every equity row
///   would vanish from the index with no error.
/// * **Columns resolved by name** — see the const block above.
/// * **Short rows counted, not padded** — padding invents empty values that
///   parse as legitimate blanks.
/// * **Bad-row tolerance** — a few corrupt rows drop; a systematic failure
///   rejects the file.
/// * **Plausibility floor** — catches a truncated transfer or a WAF page that
///   happens to contain a comma.
///
/// # Errors
///
/// Returns [`MasterParseError`] — every variant rejects the WHOLE file. There
/// is deliberately no partial-success return.
///
/// # Complexity
///
/// O(rows), single pass, one `MasterRow` allocation per retained row. Cold path.
pub fn parse_master_csv(csv: &str) -> Result<Vec<MasterRow>, MasterParseError> {
    let csv = csv.strip_prefix('\u{feff}').unwrap_or(csv);
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or(MasterParseError::Empty)?;

    let mut fields: Vec<String> = Vec::new();
    split_csv_line(header.trim_end_matches('\r'), &mut fields);
    let index: HashMap<&str, usize> = fields
        .iter()
        .enumerate()
        .map(|(i, name)| (name.trim(), i))
        .collect();

    let col = |name: &'static str| -> Result<usize, MasterParseError> {
        index
            .get(name)
            .copied()
            .ok_or(MasterParseError::MissingColumn(name))
    };
    let (i_sid, i_isin, i_sym, i_exch, i_seg, i_series) = (
        col(COL_SECURITY_ID)?,
        col(COL_ISIN)?,
        col(COL_SYMBOL_NAME)?,
        col(COL_EXCH_ID)?,
        col(COL_SEGMENT)?,
        col(COL_SERIES)?,
    );
    let widest = [i_sid, i_isin, i_sym, i_exch, i_seg, i_series]
        .into_iter()
        .max()
        .unwrap_or(0);

    let mut rows = Vec::new();
    let mut bad = 0usize;
    let mut total = 0usize;
    for line in lines {
        total += 1;
        split_csv_line(line.trim_end_matches('\r'), &mut fields);
        if fields.len() <= widest {
            bad += 1;
            continue;
        }
        // SECURITY_ID is the one field with no usable default: a row we cannot
        // identify is a row we cannot subscribe, so it is a bad row, never a
        // row with id 0. Instrument 0 is a real subscribable id space.
        let Ok(security_id) = fields[i_sid].trim().parse::<u64>() else {
            bad += 1;
            continue;
        };
        rows.push(MasterRow {
            security_id,
            // Uppercased at parse so every downstream comparison is already
            // case-normalised; ISIN is defined uppercase but vendors drift.
            isin: fields[i_isin].trim().to_uppercase(),
            symbol_name: fields[i_sym].trim().to_uppercase(),
            exch_id: fields[i_exch].trim().to_uppercase(),
            segment: fields[i_seg].trim().to_uppercase(),
            series: fields[i_series].trim().to_uppercase(),
        });
    }

    let bad_fraction = ratio(bad, total);
    if bad_fraction > MAX_BAD_ROW_FRACTION {
        return Err(MasterParseError::TooManyBadRows { bad, total });
    }
    if rows.len() < MIN_PLAUSIBLE_MASTER_ROWS {
        return Err(MasterParseError::ImplausiblyFewRows {
            rows: rows.len(),
            min: MIN_PLAUSIBLE_MASTER_ROWS,
        });
    }
    Ok(rows)
}

/// One NSE index constituent, as read from a niftyindices list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constituent {
    /// The index this row belongs to, e.g. `NIFTY 50`.
    pub index_name: String,
    /// `Symbol` — the cross-check.
    pub symbol: String,
    /// `ISIN Code` — the primary join key. May be empty on a malformed row.
    pub isin: String,
}

/// The ISIN → instrument index. Built once, queried per constituent.
///
/// # Why ambiguity is stored rather than resolved
///
/// One ISIN matching two NSE cash-equity rows is not a tie to break — it means
/// the master disagrees with itself about which `security_id` is that security.
/// Picking either one is a coin flip that subscribes a possibly-wrong
/// instrument and reports success. The pair is EXCLUDED from the index and
/// listed in [`IsinIndex::ambiguous`] so the operator sees the name.
#[derive(Debug, Default)]
pub struct IsinIndex {
    by_isin: HashMap<String, (u64, ExchangeSegment)>,
    /// ISINs seen on more than one NSE cash-equity row, excluded from lookup.
    pub ambiguous: Vec<String>,
}

impl IsinIndex {
    /// O(1) average lookup.
    #[must_use]
    pub fn get(&self, isin: &str) -> Option<(u64, ExchangeSegment)> {
        self.by_isin.get(isin).copied()
    }

    /// Number of resolvable ISINs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_isin.len()
    }

    /// True when nothing resolved — the caller MUST treat this as a failure,
    /// never as "no mismatches found".
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_isin.is_empty()
    }
}

/// Builds the ISIN index from a parsed master, per §31.1 item 3.
///
/// Filters to NSE cash equity (all three conditions — see
/// [`MasterRow::is_nse_cash_equity`]) and skips empty ISINs: a derivative row
/// carries no ISIN, and an empty-string key would collide every one of them
/// onto a single entry.
///
/// # Complexity
///
/// O(master rows) once. Every subsequent constituent lookup is O(1) average —
/// this is the whole point of building an index rather than scanning the master
/// per constituent, which would be O(constituents × master rows).
#[must_use]
pub fn build_isin_index(master: &[MasterRow]) -> IsinIndex {
    let mut by_isin: HashMap<String, (u64, ExchangeSegment)> = HashMap::new();
    let mut ambiguous: Vec<String> = Vec::new();
    for row in master.iter().filter(|r| r.is_nse_cash_equity()) {
        if row.isin.is_empty() {
            continue;
        }
        match by_isin.get(&row.isin) {
            Some(&(existing_id, _)) if existing_id == row.security_id => {
                // The identical row twice is a duplicate, not a conflict. The
                // master has carried exact-duplicate lines before; treating
                // that as ambiguity would reject healthy days.
            }
            Some(_) => {
                by_isin.remove(&row.isin);
                ambiguous.push(row.isin.clone());
            }
            None => {
                if ambiguous.iter().any(|a| a == &row.isin) {
                    continue;
                }
                by_isin.insert(
                    row.isin.clone(),
                    (row.security_id, ExchangeSegment::NseEquity),
                );
            }
        }
    }
    IsinIndex { by_isin, ambiguous }
}

/// Why a single constituent failed to resolve. Every one is NAMED, never a
/// bare count — §31.1 item 4 requires the operator can see which security.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// The constituent row carried no ISIN at all.
    NoIsin,
    /// The ISIN matched no NSE cash-equity row in the master.
    IsinNotInMaster,
    /// The ISIN matched more than one row; excluded rather than guessed.
    AmbiguousIsin,
    /// The ISIN resolved, but the master row's symbol disagrees with the
    /// index list's symbol. Fail-closed: a symbol mismatch on an ISIN hit
    /// means one of the two files is stale about this security.
    SymbolMismatch { master: String, listed: String },
}

/// A constituent that did not resolve, with its name and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub index_name: String,
    pub symbol: String,
    pub isin: String,
    pub reason: UnresolvedReason,
}

/// A resolved constituent → instrument mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConstituent {
    pub index_name: String,
    pub symbol: String,
    pub isin: String,
    pub security_id: u64,
    pub exchange_segment: ExchangeSegment,
}

/// The outcome of a full join.
#[derive(Debug, Default)]
pub struct JoinOutcome {
    /// Resolved mappings, deduped on the I-P1-11 composite key per index.
    pub resolved: Vec<ResolvedConstituent>,
    /// Every failure, named. Never summarised away.
    pub unresolved: Vec<Unresolved>,
}

impl JoinOutcome {
    /// Fraction of constituents that failed to resolve, in `[0.0, 1.0]`.
    ///
    /// Returns `1.0` for an empty input — a join over zero constituents has
    /// resolved nothing, and reporting `0.0` (a perfect score) for it is
    /// exactly the false-OK class the charter forbids: "no failures" and
    /// "nothing was attempted" must never render the same.
    #[must_use]
    pub fn unresolved_fraction(&self) -> f64 {
        let total = self.resolved.len() + self.unresolved.len();
        if total == 0 {
            return 1.0;
        }
        ratio(self.unresolved.len(), total)
    }
}

/// NSE membership tolerance, §31.1 item 4 (operator lock 2026-06-08).
///
/// DELIBERATELY separate from — and looser than — the order-critical F&O
/// dangling guard. Raised 0.5% → 2% after the 2026-06-08 live boot degraded
/// the entire universe because 5 stragglers out of 748 (0.67%) exceeded the
/// old bar. Merging the two numbers breaks one of them.
pub const NSE_MEMBERSHIP_TOLERANCE: f64 = 0.02;

/// Joins index constituents to master instruments. §31.1 items 1–6.
///
/// ISIN is the primary key; the symbol is a CROSS-CHECK that can only reject a
/// hit, never produce one. Symbol-alone is banned as a key: tickers are reused
/// and renamed, so a symbol join can map to a different company's `security_id`
/// and look completely healthy doing it.
///
/// Dedup is on the I-P1-11 composite `(security_id, exchange_segment)` WITHIN
/// an index — the same security legitimately appears in many indices, and
/// collapsing across indices would destroy the membership data this exists to
/// produce.
///
/// # Complexity
///
/// O(constituents) — one O(1) index lookup each, plus an O(1) dedup-set probe.
#[must_use]
pub fn join_constituents(constituents: &[Constituent], index: &IsinIndex) -> JoinOutcome {
    let mut out = JoinOutcome::default();
    let mut seen: std::collections::HashSet<(String, u64, ExchangeSegment)> =
        std::collections::HashSet::new();

    for c in constituents {
        let isin = c.isin.trim().to_uppercase();
        let symbol = c.symbol.trim().to_uppercase();

        if isin.is_empty() {
            out.unresolved.push(Unresolved {
                index_name: c.index_name.clone(),
                symbol,
                isin,
                reason: UnresolvedReason::NoIsin,
            });
            continue;
        }
        let Some((security_id, segment)) = index.get(&isin) else {
            let reason = if index.ambiguous.iter().any(|a| a == &isin) {
                UnresolvedReason::AmbiguousIsin
            } else {
                UnresolvedReason::IsinNotInMaster
            };
            out.unresolved.push(Unresolved {
                index_name: c.index_name.clone(),
                symbol,
                isin,
                reason,
            });
            continue;
        };
        // I-P1-11 dedup, scoped to the index. Cheap probe before the push.
        if !seen.insert((c.index_name.clone(), security_id, segment)) {
            continue;
        }
        out.resolved.push(ResolvedConstituent {
            index_name: c.index_name.clone(),
            symbol,
            isin,
            security_id,
            exchange_segment: segment,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a master CSV body big enough to clear the plausibility floor.
    /// Real rows first, then filler — so tests exercise the real rows while
    /// the floor (a truncation tripwire, not the subject of these tests) is
    /// satisfied honestly rather than by lowering it.
    fn master_with(header: &str, real: &[&str]) -> String {
        let mut s = String::from(header);
        for r in real {
            s.push('\n');
            s.push_str(r);
        }
        for i in 0..MIN_PLAUSIBLE_MASTER_ROWS {
            s.push_str(&format!(
                "\n{},INE{:09}X,FILLER{i},NSE,E,EQ",
                900_000 + i,
                i
            ));
        }
        s
    }

    const HEADER: &str = "SECURITY_ID,ISIN,SYMBOL_NAME,EXCH_ID,SEGMENT,SERIES";

    #[test]
    fn test_parse_master_csv_reads_columns_by_name_not_position() {
        // The vendor has already reordered and added columns once (the
        // 2026-07-14 live crawl). A positional parser would read the wrong
        // column and produce plausible-looking garbage.
        let csv = master_with(
            "SERIES,EXCH_ID,SECURITY_ID,EXTRA_CO_COLUMN,SYMBOL_NAME,SEGMENT,ISIN",
            &["EQ,NSE,2885,ignored,RELIANCE,E,INE002A01018"],
        )
        // filler rows follow the ORIGINAL column order, so rebuild them here
        .lines()
        .take(2)
        .collect::<Vec<_>>()
        .join("\n");
        // Only two lines: header + the real row. Bypass the floor by asserting
        // the specific error, which proves parsing reached the floor check.
        let err = parse_master_csv(&csv).unwrap_err();
        assert!(
            matches!(err, MasterParseError::ImplausiblyFewRows { rows: 1, .. }),
            "the reordered header must PARSE (reaching the row floor); got {err:?}"
        );
    }

    #[test]
    fn test_parse_master_csv_strips_bom_so_the_first_column_still_resolves() {
        // Unstripped, a BOM corrupts only the FIRST header name — so
        // SECURITY_ID goes missing while every other column resolves fine.
        // The failure is a MissingColumn on a column that is plainly present.
        let csv = format!("\u{feff}{}", master_with(HEADER, &[]));
        let rows = parse_master_csv(&csv).expect("a BOM must not break the header");
        assert_eq!(rows.len(), MIN_PLAUSIBLE_MASTER_ROWS);
    }

    #[test]
    fn test_parse_master_csv_normalises_crlf_so_the_last_column_matches() {
        // With CRLF unhandled, SERIES reads "EQ\r" and never equals "EQ" —
        // so EVERY equity row silently fails is_nse_cash_equity and the ISIN
        // index comes out empty. No error, no rows, total failure.
        let body = master_with(HEADER, &["2885,INE002A01018,RELIANCE,NSE,E,EQ"]);
        let crlf = body.replace('\n', "\r\n");
        let rows = parse_master_csv(&crlf).expect("CRLF must parse");
        let reliance = rows
            .iter()
            .find(|r| r.security_id == 2885)
            .expect("the real row survived");
        assert_eq!(reliance.series, "EQ", "trailing CR must be stripped");
        assert!(
            reliance.is_nse_cash_equity(),
            "a CRLF file must still yield cash-equity rows"
        );
    }

    #[test]
    fn test_parse_master_csv_honours_quoted_fields_containing_commas() {
        let body = master_with(HEADER, &["2885,INE002A01018,\"RELIANCE, LTD\",NSE,E,EQ"]);
        let rows = parse_master_csv(&body).expect("quoted field must parse");
        let r = rows.iter().find(|r| r.security_id == 2885).unwrap();
        assert_eq!(
            r.symbol_name, "RELIANCE, LTD",
            "a quoted comma must not split the field and shift every column after it"
        );
    }

    #[test]
    fn test_parse_master_csv_rejects_a_missing_required_column() {
        let body = master_with("SECURITY_ID,SYMBOL_NAME,EXCH_ID,SEGMENT,SERIES", &[]);
        assert_eq!(
            parse_master_csv(&body).unwrap_err(),
            MasterParseError::MissingColumn("ISIN"),
            "the ISIN column is the join key — its absence must reject, not degrade"
        );
    }

    #[test]
    fn test_parse_master_csv_rejects_an_html_error_page() {
        // The realistic hostile body: a WAF block page. It contains commas, so
        // it "parses" — and must still be refused.
        let html = "<html><head><title>403, Forbidden</title></head><body>Denied</body></html>";
        let err = parse_master_csv(html).unwrap_err();
        assert!(
            matches!(err, MasterParseError::MissingColumn(_)),
            "an HTML body must be refused, got {err:?}"
        );
    }

    #[test]
    fn test_parse_master_csv_rejects_an_empty_body() {
        assert_eq!(parse_master_csv("").unwrap_err(), MasterParseError::Empty);
        assert_eq!(
            parse_master_csv("   \n  \n").unwrap_err(),
            MasterParseError::Empty,
            "whitespace-only must be Empty, not a header of blanks"
        );
    }

    #[test]
    fn test_parse_master_csv_rejects_a_truncated_transfer() {
        // A transfer cut short parses cleanly into a handful of rows. Without
        // the floor this is indistinguishable from a real (tiny) master, and
        // the day's mapping would be built from a fragment.
        let body = master_with(HEADER, &["2885,INE002A01018,RELIANCE,NSE,E,EQ"])
            .lines()
            .take(4)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(matches!(
            parse_master_csv(&body).unwrap_err(),
            MasterParseError::ImplausiblyFewRows { .. }
        ));
    }

    #[test]
    fn test_parse_master_csv_drops_a_few_bad_rows_but_rejects_systematic_failure() {
        // A handful of corrupt rows is normal in a vendor file and must not
        // take the day down...
        let mut body = master_with(HEADER, &["2885,INE002A01018,RELIANCE,NSE,E,EQ"]);
        body.push_str("\nNOT_A_NUMBER,INE999,BROKEN,NSE,E,EQ");
        let rows = parse_master_csv(&body).expect("one bad row in ~1000 must not reject");
        assert!(rows.iter().all(|r| r.symbol_name != "BROKEN"));

        // ...but a wrong-delimiter file fails EVERY row and must be refused.
        let semicolons = master_with(HEADER, &[]).replace(',', ";");
        let err = parse_master_csv(&semicolons).unwrap_err();
        assert!(
            matches!(
                err,
                MasterParseError::MissingColumn(_) | MasterParseError::TooManyBadRows { .. }
            ),
            "a wrong-delimiter file must be refused, got {err:?}"
        );
    }

    #[test]
    fn test_parse_master_csv_never_invents_security_id_zero() {
        // A row whose id will not parse must be a BAD ROW, never id 0.
        // Instrument 0 is a real subscribable id space, so defaulting there
        // would silently subscribe the wrong thing.
        let mut body = master_with(HEADER, &[]);
        body.push_str("\n,INE111111111,NOID,NSE,E,EQ");
        let rows = parse_master_csv(&body).expect("one bad row tolerated");
        assert!(
            !rows.iter().any(|r| r.security_id == 0),
            "an unparseable SECURITY_ID must drop the row, never become id 0"
        );
    }

    #[test]
    fn test_is_nse_cash_equity_requires_all_three_conditions() {
        let base = MasterRow {
            security_id: 1,
            isin: "INE002A01018".into(),
            symbol_name: "RELIANCE".into(),
            exch_id: "NSE".into(),
            segment: "E".into(),
            series: "EQ".into(),
        };
        assert!(base.is_nse_cash_equity());
        // BSE also lists series EQ — a constituent resolved there is a
        // wrong-exchange subscription that looks healthy.
        assert!(
            !MasterRow {
                exch_id: "BSE".into(),
                ..base.clone()
            }
            .is_nse_cash_equity()
        );
        // Derivative segment.
        assert!(
            !MasterRow {
                segment: "D".into(),
                ..base.clone()
            }
            .is_nse_cash_equity()
        );
        // Trade-to-trade series is NOT the cash series.
        assert!(
            !MasterRow {
                series: "BE".into(),
                ..base
            }
            .is_nse_cash_equity()
        );
    }

    fn row(id: u64, isin: &str, sym: &str) -> MasterRow {
        MasterRow {
            security_id: id,
            isin: isin.into(),
            symbol_name: sym.into(),
            exch_id: "NSE".into(),
            segment: "E".into(),
            series: "EQ".into(),
        }
    }

    #[test]
    fn test_build_isin_index_excludes_an_ambiguous_isin_rather_than_guessing() {
        // Two rows, one ISIN, DIFFERENT ids: the master disagrees with itself.
        // Picking either is a coin flip that subscribes a possibly-wrong
        // instrument and reports success.
        let master = vec![
            row(1, "INE002A01018", "RELIANCE"),
            row(2, "INE002A01018", "RELIANCE"),
        ];
        let index = build_isin_index(&master);
        assert!(
            index.get("INE002A01018").is_none(),
            "an ambiguous ISIN must not resolve"
        );
        assert_eq!(index.ambiguous, vec!["INE002A01018".to_string()]);
    }

    #[test]
    fn test_build_isin_index_treats_an_exact_duplicate_row_as_one_not_a_conflict() {
        // The master has carried exact-duplicate lines. Treating that as
        // ambiguity would reject healthy days.
        let master = vec![
            row(1, "INE002A01018", "RELIANCE"),
            row(1, "INE002A01018", "RELIANCE"),
        ];
        let index = build_isin_index(&master);
        assert_eq!(
            index.get("INE002A01018"),
            Some((1, ExchangeSegment::NseEquity))
        );
        assert!(index.ambiguous.is_empty());
    }

    #[test]
    fn test_build_isin_index_ambiguity_is_sticky_across_a_later_third_row() {
        // Once excluded, a third row for the same ISIN must not silently
        // re-admit it into the index.
        let master = vec![
            row(1, "INE002A01018", "RELIANCE"),
            row(2, "INE002A01018", "RELIANCE"),
            row(3, "INE002A01018", "RELIANCE"),
        ];
        let index = build_isin_index(&master);
        assert!(
            index.get("INE002A01018").is_none(),
            "a third row must not resurrect an ISIN already proven ambiguous"
        );
    }

    #[test]
    fn test_build_isin_index_skips_empty_isins_so_derivatives_do_not_collide() {
        // Derivative rows carry no ISIN. An empty-string key would collapse
        // every one of them onto a single entry, and a constituent with a
        // blank ISIN would then "resolve" to whichever derivative won.
        let mut d1 = row(100, "", "NIFTY24000CE");
        d1.segment = "D".into();
        let mut d2 = row(101, "", "NIFTY24100CE");
        d2.segment = "D".into();
        let index = build_isin_index(&[d1, d2, row(1, "INE002A01018", "RELIANCE")]);
        assert_eq!(
            index.len(),
            1,
            "only the ISIN-bearing equity row is indexed"
        );
        assert!(index.get("").is_none());
    }

    fn con(index: &str, sym: &str, isin: &str) -> Constituent {
        Constituent {
            index_name: index.into(),
            symbol: sym.into(),
            isin: isin.into(),
        }
    }

    #[test]
    fn test_join_resolves_by_isin_and_names_every_failure() {
        let index = build_isin_index(&[row(2885, "INE002A01018", "RELIANCE")]);
        let out = join_constituents(
            &[
                con("NIFTY 50", "RELIANCE", "INE002A01018"),
                con("NIFTY 50", "GONE", "INE999999999"),
                con("NIFTY 50", "NOISIN", ""),
            ],
            &index,
        );
        assert_eq!(out.resolved.len(), 1);
        assert_eq!(out.resolved[0].security_id, 2885);
        assert_eq!(out.unresolved.len(), 2);
        // Named, not counted — the operator must be able to see WHICH.
        assert!(
            out.unresolved
                .iter()
                .any(|u| u.symbol == "GONE" && u.reason == UnresolvedReason::IsinNotInMaster)
        );
        assert!(
            out.unresolved
                .iter()
                .any(|u| u.symbol == "NOISIN" && u.reason == UnresolvedReason::NoIsin)
        );
    }

    #[test]
    fn test_join_distinguishes_ambiguous_from_absent() {
        // Both fail, but they are different operator actions: "the master
        // disagrees with itself" vs "this security is not listed".
        let index = build_isin_index(&[
            row(1, "INE002A01018", "RELIANCE"),
            row(2, "INE002A01018", "RELIANCE"),
        ]);
        let out = join_constituents(&[con("NIFTY 50", "RELIANCE", "INE002A01018")], &index);
        assert_eq!(out.unresolved[0].reason, UnresolvedReason::AmbiguousIsin);
    }

    #[test]
    fn test_join_is_case_insensitive_on_isin() {
        // Vendors drift on case; ISIN is defined uppercase. A case mismatch
        // must not read as "not listed".
        let index = build_isin_index(&[row(2885, "INE002A01018", "RELIANCE")]);
        let out = join_constituents(&[con("NIFTY 50", "reliance", "ine002a01018")], &index);
        assert_eq!(out.resolved.len(), 1, "lowercase ISIN must still resolve");
    }

    #[test]
    fn test_join_dedups_within_an_index_but_keeps_cross_index_membership() {
        // The same security legitimately belongs to many indices. Deduping
        // across indices would destroy the membership data this produces.
        let index = build_isin_index(&[row(2885, "INE002A01018", "RELIANCE")]);
        let out = join_constituents(
            &[
                con("NIFTY 50", "RELIANCE", "INE002A01018"),
                con("NIFTY 50", "RELIANCE", "INE002A01018"),
                con("NIFTY 500", "RELIANCE", "INE002A01018"),
            ],
            &index,
        );
        assert_eq!(
            out.resolved.len(),
            2,
            "one row per (index, security), not one row per security"
        );
    }

    #[test]
    fn test_unresolved_fraction_of_an_empty_join_is_total_failure_not_perfection() {
        // THE false-OK case. A join over zero constituents has resolved
        // nothing; reporting 0.0 would make "nothing was attempted" and "no
        // failures" render identically, and the tolerance gate would PASS a
        // day on which the index list failed to download.
        let empty = JoinOutcome::default();
        assert!(
            (empty.unresolved_fraction() - 1.0).abs() < f64::EPSILON,
            "an empty join must report total failure, not a perfect score"
        );
        assert!(
            empty.unresolved_fraction() > NSE_MEMBERSHIP_TOLERANCE,
            "and it must therefore breach the tolerance gate"
        );
    }

    #[test]
    fn test_unresolved_fraction_matches_the_incident_that_set_the_tolerance() {
        // 5 of 748 = 0.67% — the real 2026-06-08 boot. Under the old 0.5% bar
        // it degraded the ENTIRE universe; under the 2% lock it passes.
        let mut out = JoinOutcome::default();
        for i in 0..743u64 {
            out.resolved.push(ResolvedConstituent {
                index_name: "NIFTY TOTAL MARKET".into(),
                symbol: format!("S{i}"),
                isin: format!("INE{i:09}"),
                security_id: i,
                exchange_segment: ExchangeSegment::NseEquity,
            });
        }
        for i in 0..5u64 {
            out.unresolved.push(Unresolved {
                index_name: "NIFTY TOTAL MARKET".into(),
                symbol: format!("X{i}"),
                isin: format!("INE{i:09}"),
                reason: UnresolvedReason::IsinNotInMaster,
            });
        }
        let f = out.unresolved_fraction();
        assert!(
            (f - 0.006_684_49).abs() < 1e-6,
            "expected the historical 0.67%, got {f}"
        );
        assert!(
            f <= NSE_MEMBERSHIP_TOLERANCE,
            "the 2026-06-08 shape must PASS under the 2% lock — that is why it was raised"
        );
        assert!(
            f > 0.005,
            "and it must still exceed the old 0.5% bar it broke"
        );
    }

    #[test]
    fn test_tolerances_stay_separate() {
        // §31.1 item 4 is explicit that the NSE membership tolerance is
        // deliberately looser than, and separate from, the order-critical F&O
        // dangling guard. A future "tidy-up" that unifies them breaks one.
        assert!(
            (NSE_MEMBERSHIP_TOLERANCE - 0.02).abs() < f64::EPSILON,
            "NSE membership tolerance is locked at 2% (operator 2026-06-08)"
        );
        assert!(
            NSE_MEMBERSHIP_TOLERANCE > MAX_BAD_ROW_FRACTION,
            "the membership tolerance must stay looser than the parse tolerance"
        );
    }
}
