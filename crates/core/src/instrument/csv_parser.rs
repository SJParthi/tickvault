//! Instrument CSV column auto-detection, row parsing, and filtering.
//!
//! Parses Dhan's instrument master CSV (~276K rows, ~40MB) into typed
//! [`ParsedInstrumentRow`] records. Only rows matching the filter
//! (NSE I/E/D + BSE I/D) are kept.

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use tracing::{debug, info, warn};

use dhan_live_trader_common::constants::*;
use dhan_live_trader_common::error::ApplicationError;
use dhan_live_trader_common::types::{Exchange, OptionType, SecurityId};

// ---------------------------------------------------------------------------
// Column Indices (auto-detected from header)
// ---------------------------------------------------------------------------

/// Column indices auto-detected from the CSV header row.
#[derive(Debug)]
struct CsvColumnIndices {
    exch_id: usize,
    segment: usize,
    security_id: usize,
    instrument: usize,
    underlying_security_id: usize,
    underlying_symbol: usize,
    symbol_name: usize,
    display_name: usize,
    series: usize,
    lot_size: usize,
    expiry_date: usize,
    strike_price: usize,
    option_type: usize,
    tick_size: usize,
    expiry_flag: usize,
}

// ---------------------------------------------------------------------------
// Parsed Row
// ---------------------------------------------------------------------------

/// A parsed, validated row from the instrument CSV.
///
/// Only rows passing the (NSE I/E/D + BSE I/D) filter are represented.
#[derive(Debug, Clone)]
pub struct ParsedInstrumentRow {
    /// Exchange (NSE or BSE).
    pub exchange: Exchange,
    /// Segment character: 'I' (index), 'E' (equity), or 'D' (derivative).
    pub segment: char,
    /// This row's security ID.
    pub security_id: SecurityId,
    /// Instrument name (e.g., "INDEX", "EQUITY", "FUTIDX", "OPTIDX").
    pub instrument: String,
    /// Underlying security ID (for derivatives and indices).
    pub underlying_security_id: SecurityId,
    /// Underlying symbol (e.g., "NIFTY", "RELIANCE").
    pub underlying_symbol: String,
    /// Trading symbol name (e.g., "NIFTY-Mar2026-18000-CE").
    pub symbol_name: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Series (e.g., "EQ" for equities, "NA" for others).
    pub series: String,
    /// Contract lot size.
    pub lot_size: u32,
    /// Expiry date (None for non-derivatives).
    pub expiry_date: Option<NaiveDate>,
    /// Strike price (0.0 for futures, negative for N/A).
    pub strike_price: f64,
    /// Option type (None for non-options).
    pub option_type: Option<OptionType>,
    /// Minimum tick size.
    pub tick_size: f64,
    /// Expiry flag (e.g., "M" for monthly, "W" for weekly).
    pub expiry_flag: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse the entire CSV text into filtered, validated rows.
///
/// # Algorithm
/// 1. Read header row, auto-detect column indices by matching header names.
/// 2. Iterate all data rows.
/// 3. For each row: extract exchange + segment.
/// 4. Filter: keep only (NSE,I), (NSE,E), (NSE,D), (BSE,I), (BSE,D). Skip all others.
/// 5. Parse remaining fields into typed [`ParsedInstrumentRow`].
///
/// # Returns
/// Tuple of (total_csv_rows, filtered_rows).
pub fn parse_instrument_csv(csv_text: &str) -> Result<(usize, Vec<ParsedInstrumentRow>)> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true) // Dhan CSV has trailing comma → extra empty field
        .trim(csv::Trim::All)
        .from_reader(csv_text.as_bytes());

    let headers = reader
        .headers()
        .context("failed to read CSV header row")?
        .clone();

    let indices = detect_column_indices(&headers)?;

    let mut parsed_rows = Vec::with_capacity(INSTRUMENT_CSV_MIN_ROWS);
    let mut total_row_count: usize = 0;
    let mut skipped_filter_count: usize = 0;
    let mut skipped_parse_error_count: usize = 0;

    for result in reader.records() {
        total_row_count += 1;
        let record = match result {
            Ok(record) => record,
            Err(error) => {
                warn!(row = total_row_count, %error, "skipping malformed CSV row");
                skipped_parse_error_count += 1;
                continue;
            }
        };

        // Extract exchange and segment for filtering
        let exchange_str = record.get(indices.exch_id).unwrap_or("");
        let segment_str = record.get(indices.segment).unwrap_or("");

        if !should_include_row(exchange_str, segment_str) {
            skipped_filter_count += 1;
            continue;
        }

        match parse_row(&record, &indices) {
            Ok(row) => parsed_rows.push(row),
            Err(error) => {
                debug!(
                    row = total_row_count,
                    %error,
                    "skipping row due to parse error"
                );
                skipped_parse_error_count += 1;
            }
        }
    }

    info!(
        total_csv_rows = total_row_count,
        parsed_rows = parsed_rows.len(),
        skipped_filter = skipped_filter_count,
        skipped_parse_errors = skipped_parse_error_count,
        "instrument CSV parsing complete"
    );

    if total_row_count < INSTRUMENT_CSV_MIN_ROWS {
        bail!(ApplicationError::InstrumentParseFailed {
            row: 0,
            reason: format!(
                "CSV has only {} rows, expected at least {}",
                total_row_count, INSTRUMENT_CSV_MIN_ROWS
            ),
        });
    }

    Ok((total_row_count, parsed_rows))
}

// ---------------------------------------------------------------------------
// Internal Functions
// ---------------------------------------------------------------------------

/// Auto-detect column indices from the CSV header row.
fn detect_column_indices(headers: &csv::StringRecord) -> Result<CsvColumnIndices> {
    let find = |name: &str| -> Result<usize> {
        headers
            .iter()
            .position(|header| header.trim() == name)
            .ok_or_else(|| {
                ApplicationError::CsvColumnMissing {
                    column: name.to_owned(),
                }
                .into()
            })
    };

    Ok(CsvColumnIndices {
        exch_id: find(CSV_COLUMN_EXCH_ID)?,
        segment: find(CSV_COLUMN_SEGMENT)?,
        security_id: find(CSV_COLUMN_SECURITY_ID)?,
        instrument: find(CSV_COLUMN_INSTRUMENT)?,
        underlying_security_id: find(CSV_COLUMN_UNDERLYING_SECURITY_ID)?,
        underlying_symbol: find(CSV_COLUMN_UNDERLYING_SYMBOL)?,
        symbol_name: find(CSV_COLUMN_SYMBOL_NAME)?,
        display_name: find(CSV_COLUMN_DISPLAY_NAME)?,
        series: find(CSV_COLUMN_SERIES)?,
        lot_size: find(CSV_COLUMN_LOT_SIZE)?,
        expiry_date: find(CSV_COLUMN_EXPIRY_DATE)?,
        strike_price: find(CSV_COLUMN_STRIKE_PRICE)?,
        option_type: find(CSV_COLUMN_OPTION_TYPE)?,
        tick_size: find(CSV_COLUMN_TICK_SIZE)?,
        expiry_flag: find(CSV_COLUMN_EXPIRY_FLAG)?,
    })
}

/// Check if a row should be included based on (exchange, segment) filter.
///
/// Only keeps: (NSE, I), (NSE, E), (NSE, D), (BSE, I), (BSE, D).
fn should_include_row(exchange_str: &str, segment_str: &str) -> bool {
    match exchange_str {
        CSV_EXCHANGE_NSE => matches!(
            segment_str,
            CSV_SEGMENT_INDEX | CSV_SEGMENT_EQUITY | CSV_SEGMENT_DERIVATIVE
        ),
        CSV_EXCHANGE_BSE => matches!(segment_str, CSV_SEGMENT_INDEX | CSV_SEGMENT_DERIVATIVE),
        _ => false,
    }
}

/// Parse a single CSV record into a [`ParsedInstrumentRow`].
fn parse_row(
    record: &csv::StringRecord,
    indices: &CsvColumnIndices,
) -> Result<ParsedInstrumentRow> {
    let get = |idx: usize| -> &str { record.get(idx).unwrap_or("") };

    let exchange_str = get(indices.exch_id);
    let exchange = match exchange_str {
        CSV_EXCHANGE_NSE => Exchange::NationalStockExchange,
        CSV_EXCHANGE_BSE => Exchange::BombayStockExchange,
        other => bail!("unknown exchange: {}", other),
    };

    let segment_str = get(indices.segment);
    let segment = segment_str.chars().next().context("empty segment field")?;

    let security_id_str = get(indices.security_id);
    let security_id: SecurityId = security_id_str
        .parse()
        .with_context(|| format!("invalid security_id: '{}'", security_id_str))?;

    let underlying_security_id_str = get(indices.underlying_security_id);
    let underlying_security_id: SecurityId = underlying_security_id_str.parse().unwrap_or(0); // 0 is valid for equity rows; derivative validation happens in universe builder

    let lot_size_str = get(indices.lot_size);
    let lot_size: u32 = parse_lot_size(lot_size_str)
        .with_context(|| format!("invalid lot_size: '{}'", lot_size_str))?;

    let strike_price_str = get(indices.strike_price);
    let strike_price: f64 = parse_strike_price(strike_price_str);

    let tick_size_str = get(indices.tick_size);
    let tick_size: f64 = parse_tick_size(tick_size_str)
        .with_context(|| format!("invalid tick_size: '{}'", tick_size_str))?;

    let option_type_str = get(indices.option_type);
    let option_type = match option_type_str {
        CSV_OPTION_TYPE_CALL => Some(OptionType::Call),
        CSV_OPTION_TYPE_PUT => Some(OptionType::Put),
        _ => None,
    };

    let expiry_date_str = get(indices.expiry_date);
    let expiry_date = parse_expiry_date(expiry_date_str);

    Ok(ParsedInstrumentRow {
        exchange,
        segment,
        security_id,
        instrument: get(indices.instrument).to_owned(),
        underlying_security_id,
        underlying_symbol: get(indices.underlying_symbol).to_owned(),
        symbol_name: get(indices.symbol_name).to_owned(),
        display_name: get(indices.display_name).to_owned(),
        series: get(indices.series).to_owned(),
        lot_size,
        expiry_date,
        strike_price,
        option_type,
        tick_size,
        expiry_flag: get(indices.expiry_flag).to_owned(),
    })
}

/// Parse lot size from CSV string (Dhan stores as float, e.g., "75.0").
///
/// # Validation
/// - Must parse as finite f64 > 0.
/// - Must fit within u32 range after rounding.
/// - NaN, infinity, zero, and negative values are rejected.
fn parse_lot_size(value: &str) -> Result<u32> {
    if value.is_empty() {
        bail!("lot_size is empty");
    }
    let float_val: f64 = value.parse().context("lot_size is not a valid number")?;
    if !float_val.is_finite() || float_val <= 0.0 {
        bail!("lot_size must be finite and positive, got {}", float_val);
    }
    let rounded = float_val.round();
    if rounded > f64::from(u32::MAX) {
        bail!("lot_size {} exceeds u32::MAX", rounded);
    }
    // APPROVED: prechecked — exceeds u32::MAX guard validates before cast
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(rounded as u32)
}

/// Parse strike price from CSV string.
///
/// Returns 0.0 for empty, negative sentinel (Dhan uses -0.01 for futures),
/// or unparsable values. Actual strike price validation happens in the
/// universe builder when attaching contracts to option chains.
fn parse_strike_price(value: &str) -> f64 {
    if value.is_empty() {
        return 0.0;
    }
    match value.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => v,
        _ => 0.0, // Sentinel: futures use -0.01 or 0.0 — both map to 0.0
    }
}

/// Parse tick size from CSV string.
///
/// # Validation
/// - Must parse as finite f64 > 0.
/// - NaN, infinity, zero, and negative values are rejected.
fn parse_tick_size(value: &str) -> Result<f64> {
    if value.is_empty() {
        bail!("tick_size is empty");
    }
    let float_val: f64 = value.parse().context("tick_size is not a valid number")?;
    if !float_val.is_finite() || float_val <= 0.0 {
        bail!("tick_size must be finite and positive, got {}", float_val);
    }
    Ok(float_val)
}

/// Parse Dhan's expiry date format (e.g., "2026-03-27") into NaiveDate.
///
/// Returns None for sentinel values like "0001-01-01" or empty strings.
/// Logs a warning for non-sentinel strings that fail to parse.
fn parse_expiry_date(date_str: &str) -> Option<NaiveDate> {
    if date_str.is_empty() || date_str == "0001-01-01" {
        return None;
    }
    match NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
        Ok(date) => Some(date),
        Err(error) => {
            warn!(date_str, %error, "unparsable expiry date — treating as None");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn build_mock_csv_header() -> String {
        "EXCH_ID,SEGMENT,SECURITY_ID,ISIN,INSTRUMENT,UNDERLYING_SECURITY_ID,\
         UNDERLYING_SYMBOL,SYMBOL_NAME,DISPLAY_NAME,INSTRUMENT_TYPE,SERIES,\
         LOT_SIZE,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,EXPIRY_FLAG,"
            .to_owned()
    }

    fn build_nse_index_row(security_id: u32, symbol: &str) -> String {
        format!(
            "NSE,I,{},NA,INDEX,{},{},{},{} Index,INDEX,NA,1.0,0001-01-01,,XX,0.0500,N,",
            security_id, security_id, symbol, symbol, symbol
        )
    }

    fn build_nse_equity_row(security_id: u32, symbol: &str) -> String {
        format!(
            "NSE,E,{},INE000A00000,EQUITY,,{},{} LTD,{},ES,EQ,1.0,,,,5.0000,NA,",
            security_id, symbol, symbol, symbol
        )
    }

    fn build_nse_futidx_row(
        security_id: u32,
        underlying_id: u32,
        symbol: &str,
        expiry: &str,
        lot_size: u32,
    ) -> String {
        format!(
            "NSE,D,{},NA,FUTIDX,{},{},{}-{}-FUT,{} FUT,FUT,NA,{}.0,{},-0.01000,XX,5.0000,M,",
            security_id, underlying_id, symbol, symbol, expiry, symbol, lot_size, expiry
        )
    }

    fn build_nse_futstk_row(
        security_id: u32,
        underlying_id: u32,
        symbol: &str,
        expiry: &str,
        lot_size: u32,
    ) -> String {
        format!(
            "NSE,D,{},NA,FUTSTK,{},{},{}-{}-FUT,{} FUT,FUT,NA,{}.0,{},-0.01000,XX,10.0000,M,",
            security_id, underlying_id, symbol, symbol, expiry, symbol, lot_size, expiry
        )
    }

    fn build_nse_optidx_row(
        security_id: u32,
        underlying_id: u32,
        symbol: &str,
        expiry: &str,
        strike: f64,
        opt_type: &str,
        lot_size: u32,
    ) -> String {
        format!(
            "NSE,D,{},NA,OPTIDX,{},{},{}-{}-{:.0}-{},{} {} CALL,OP,NA,{}.0,{},{:.5},{},5.0000,M,",
            security_id,
            underlying_id,
            symbol,
            symbol,
            expiry,
            strike,
            opt_type,
            symbol,
            expiry,
            lot_size,
            expiry,
            strike,
            opt_type
        )
    }

    fn build_mcx_row() -> String {
        "MCX,D,99999,NA,FUTCOM,500,CRUDEOIL,CRUDEOIL-FUT,CRUDE OIL FUT,FUT,NA,100.0,2026-03-28,-0.01,XX,1.0,M,".to_owned()
    }

    /// Build a complete mock CSV with all segment types.
    pub fn build_mock_csv() -> String {
        let mut lines = vec![build_mock_csv_header()];

        // NSE Indices (Pass 1)
        lines.push(build_nse_index_row(13, "NIFTY"));
        lines.push(build_nse_index_row(25, "BANKNIFTY"));
        lines.push(build_nse_index_row(27, "FINNIFTY"));
        lines.push(build_nse_index_row(442, "MIDCPNIFTY"));
        lines.push(build_nse_index_row(38, "NIFTY NEXT 50"));

        // BSE Indices
        lines.push("BSE,I,51,NA,INDEX,51,SENSEX,SENSEX,S&P BSE SENSEX,INDEX,NA,1.0,0001-01-01,,XX,0.0100,N,".to_owned());
        lines.push("BSE,I,69,NA,INDEX,69,BANKEX,BANKEX,S&P BSE BANKEX,INDEX,NA,1.0,0001-01-01,,XX,0.0100,N,".to_owned());
        lines.push("BSE,I,83,NA,INDEX,83,SNSX50,SNSX50,S&P BSE SENSEX 50,INDEX,NA,1.0,0001-01-01,,XX,0.0100,N,".to_owned());

        // NSE Equities (Pass 2)
        lines.push(build_nse_equity_row(2885, "RELIANCE"));
        lines.push(build_nse_equity_row(1333, "HDFCBANK"));
        lines.push(build_nse_equity_row(1594, "INFY"));
        lines.push(build_nse_equity_row(11536, "TCS"));
        lines.push(build_nse_equity_row(5258, "SBIN"));

        // NSE FUTIDX (Pass 3 — index underlyings)
        lines.push(build_nse_futidx_row(
            51700,
            26000,
            "NIFTY",
            "2026-03-30",
            75,
        ));
        lines.push(build_nse_futidx_row(
            51701,
            26009,
            "BANKNIFTY",
            "2026-03-30",
            30,
        ));
        lines.push(build_nse_futidx_row(
            51712,
            26037,
            "FINNIFTY",
            "2026-03-30",
            60,
        ));
        lines.push(build_nse_futidx_row(
            51713,
            26074,
            "MIDCPNIFTY",
            "2026-03-30",
            120,
        ));

        // BSE FUTIDX (SENSEX)
        lines.push("BSE,D,60000,NA,FUTIDX,1,SENSEX,SENSEX-2026-03-30-FUT,SENSEX FUT,FUT,NA,20.0,2026-03-30,-0.01000,XX,5.0000,M,".to_owned());

        // NSE FUTSTK (Pass 3 — stock underlyings)
        lines.push(build_nse_futstk_row(
            52023,
            2885,
            "RELIANCE",
            "2026-03-30",
            500,
        ));
        lines.push(build_nse_futstk_row(
            52024,
            1333,
            "HDFCBANK",
            "2026-03-30",
            550,
        ));
        lines.push(build_nse_futstk_row(52025, 1594, "INFY", "2026-03-30", 400));
        lines.push(build_nse_futstk_row(52026, 11536, "TCS", "2026-03-30", 175));
        lines.push(build_nse_futstk_row(
            52027,
            5258,
            "SBIN",
            "2026-03-30",
            1500,
        ));

        // NSE OPTIDX — NIFTY options (Pass 5)
        lines.push(build_nse_optidx_row(
            70001,
            26000,
            "NIFTY",
            "2026-03-30",
            22000.0,
            "CE",
            75,
        ));
        lines.push(build_nse_optidx_row(
            70002,
            26000,
            "NIFTY",
            "2026-03-30",
            22000.0,
            "PE",
            75,
        ));
        lines.push(build_nse_optidx_row(
            70003,
            26000,
            "NIFTY",
            "2026-03-30",
            22500.0,
            "CE",
            75,
        ));
        lines.push(build_nse_optidx_row(
            70004,
            26000,
            "NIFTY",
            "2026-03-30",
            22500.0,
            "PE",
            75,
        ));
        lines.push(build_nse_optidx_row(
            70005,
            26000,
            "NIFTY",
            "2026-04-30",
            22000.0,
            "CE",
            75,
        ));
        lines.push(build_nse_optidx_row(
            70006,
            26000,
            "NIFTY",
            "2026-04-30",
            22000.0,
            "PE",
            75,
        ));

        // MCX row (should be filtered out)
        lines.push(build_mcx_row());

        // TEST instrument (should be skipped in Pass 3/5)
        lines.push(build_nse_futstk_row(
            99998,
            99999,
            "TESTSTOCK",
            "2026-03-30",
            100,
        ));

        // Expired derivative (should be filtered in Pass 5)
        lines.push(build_nse_futidx_row(
            51600,
            26000,
            "NIFTY",
            "2025-01-30",
            75,
        ));

        lines.join("\n")
    }

    #[test]
    fn test_should_include_row_nse_index_returns_true() {
        assert!(should_include_row("NSE", "I"));
    }

    #[test]
    fn test_should_include_row_nse_equity_returns_true() {
        assert!(should_include_row("NSE", "E"));
    }

    #[test]
    fn test_should_include_row_nse_derivative_returns_true() {
        assert!(should_include_row("NSE", "D"));
    }

    #[test]
    fn test_should_include_row_bse_index_returns_true() {
        assert!(should_include_row("BSE", "I"));
    }

    #[test]
    fn test_should_include_row_bse_derivative_returns_true() {
        assert!(should_include_row("BSE", "D"));
    }

    #[test]
    fn test_should_include_row_bse_equity_returns_false() {
        assert!(!should_include_row("BSE", "E"));
    }

    #[test]
    fn test_should_include_row_mcx_returns_false() {
        assert!(!should_include_row("MCX", "D"));
    }

    #[test]
    fn test_should_include_row_unknown_exchange_returns_false() {
        assert!(!should_include_row("UNKNOWN", "I"));
    }

    #[test]
    fn test_parse_expiry_date_valid_format() {
        let date = parse_expiry_date("2026-03-30");
        assert_eq!(date, Some(NaiveDate::from_ymd_opt(2026, 3, 30).unwrap()));
    }

    #[test]
    fn test_parse_expiry_date_sentinel_value() {
        assert_eq!(parse_expiry_date("0001-01-01"), None);
    }

    #[test]
    fn test_parse_expiry_date_empty_string() {
        assert_eq!(parse_expiry_date(""), None);
    }

    #[test]
    fn test_parse_expiry_date_invalid_format() {
        assert_eq!(parse_expiry_date("not-a-date"), None);
    }

    #[test]
    fn test_detect_column_indices_valid_header() {
        let header_str = build_mock_csv_header();
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(header_str.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers);
        assert!(indices.is_ok());
        let indices = indices.unwrap();
        assert_eq!(indices.exch_id, 0);
        assert_eq!(indices.segment, 1);
        assert_eq!(indices.security_id, 2);
    }

    #[test]
    fn test_detect_column_indices_missing_column_returns_error() {
        let header = "EXCH_ID,SEGMENT,MISSING_COLUMN";
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .from_reader(header.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let result = detect_column_indices(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_mock_csv_filters_correctly() {
        let csv_text = build_mock_csv();
        // We test filtering logic by counting manually (real CSV has >100K rows,
        // but unit tests verify logic, not count).

        // Instead, test the filtering logic by counting manually
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let mut included = 0;
        let mut excluded = 0;
        for result in reader.records() {
            let record = result.unwrap();
            let exchange = record.get(indices.exch_id).unwrap_or("");
            let segment = record.get(indices.segment).unwrap_or("");
            if should_include_row(exchange, segment) {
                included += 1;
            } else {
                excluded += 1;
            }
        }

        // MCX row should be excluded
        assert!(excluded >= 1, "expected at least 1 excluded row (MCX)");
        // All NSE and BSE rows should be included
        assert!(
            included >= 30,
            "expected at least 30 included rows, got {}",
            included
        );
    }

    #[test]
    fn test_parse_row_nse_index() {
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            build_nse_index_row(13, "NIFTY")
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let row = parse_row(&record, &indices).unwrap();

        assert_eq!(row.exchange, Exchange::NationalStockExchange);
        assert_eq!(row.segment, 'I');
        assert_eq!(row.security_id, 13);
        assert_eq!(row.underlying_symbol, "NIFTY");
        assert_eq!(row.expiry_date, None);
    }

    #[test]
    fn test_parse_row_nse_equity() {
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            build_nse_equity_row(2885, "RELIANCE")
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let row = parse_row(&record, &indices).unwrap();

        assert_eq!(row.exchange, Exchange::NationalStockExchange);
        assert_eq!(row.segment, 'E');
        assert_eq!(row.security_id, 2885);
        assert_eq!(row.series, "EQ");
    }

    #[test]
    fn test_parse_row_nse_futidx() {
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            build_nse_futidx_row(51700, 26000, "NIFTY", "2026-03-30", 75)
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let row = parse_row(&record, &indices).unwrap();

        assert_eq!(row.segment, 'D');
        assert_eq!(row.instrument, "FUTIDX");
        assert_eq!(row.underlying_security_id, 26000);
        assert_eq!(row.underlying_symbol, "NIFTY");
        assert_eq!(row.lot_size, 75);
        assert_eq!(
            row.expiry_date,
            Some(NaiveDate::from_ymd_opt(2026, 3, 30).unwrap())
        );
        assert!(row.option_type.is_none());
    }

    #[test]
    fn test_parse_row_nse_optidx() {
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            build_nse_optidx_row(70001, 26000, "NIFTY", "2026-03-30", 22000.0, "CE", 75)
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let row = parse_row(&record, &indices).unwrap();

        assert_eq!(row.instrument, "OPTIDX");
        assert_eq!(row.strike_price, 22000.0);
        assert_eq!(row.option_type, Some(OptionType::Call));
    }

    // -----------------------------------------------------------------------
    // parse_lot_size edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_lot_size_valid_integer() {
        assert_eq!(parse_lot_size("75").unwrap(), 75);
    }

    #[test]
    fn test_parse_lot_size_valid_float() {
        assert_eq!(parse_lot_size("75.0").unwrap(), 75);
    }

    #[test]
    fn test_parse_lot_size_rounds_to_nearest() {
        assert_eq!(parse_lot_size("74.6").unwrap(), 75);
        assert_eq!(parse_lot_size("75.4").unwrap(), 75);
    }

    #[test]
    fn test_parse_lot_size_empty_string_fails() {
        assert!(parse_lot_size("").is_err());
    }

    #[test]
    fn test_parse_lot_size_zero_fails() {
        assert!(parse_lot_size("0").is_err());
    }

    #[test]
    fn test_parse_lot_size_negative_fails() {
        assert!(parse_lot_size("-10").is_err());
    }

    #[test]
    fn test_parse_lot_size_nan_fails() {
        assert!(parse_lot_size("NaN").is_err());
    }

    #[test]
    fn test_parse_lot_size_inf_fails() {
        assert!(parse_lot_size("inf").is_err());
    }

    #[test]
    fn test_parse_lot_size_not_a_number_fails() {
        assert!(parse_lot_size("abc").is_err());
    }

    #[test]
    fn test_parse_lot_size_very_large_fails() {
        // Value larger than u32::MAX
        assert!(parse_lot_size("5000000000").is_err());
    }

    #[test]
    fn test_parse_lot_size_u32_max_boundary() {
        // u32::MAX = 4294967295
        assert_eq!(parse_lot_size("4294967295").unwrap(), u32::MAX);
    }

    // -----------------------------------------------------------------------
    // parse_strike_price edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_strike_price_valid() {
        assert_eq!(parse_strike_price("22000.0"), 22000.0);
    }

    #[test]
    fn test_parse_strike_price_empty_returns_zero() {
        assert_eq!(parse_strike_price(""), 0.0);
    }

    #[test]
    fn test_parse_strike_price_negative_sentinel_returns_zero() {
        assert_eq!(parse_strike_price("-0.01"), 0.0);
    }

    #[test]
    fn test_parse_strike_price_zero_returns_zero() {
        assert_eq!(parse_strike_price("0.0"), 0.0);
    }

    #[test]
    fn test_parse_strike_price_nan_returns_zero() {
        assert_eq!(parse_strike_price("NaN"), 0.0);
    }

    #[test]
    fn test_parse_strike_price_inf_returns_zero() {
        assert_eq!(parse_strike_price("inf"), 0.0);
    }

    #[test]
    fn test_parse_strike_price_garbage_returns_zero() {
        assert_eq!(parse_strike_price("not_a_number"), 0.0);
    }

    #[test]
    fn test_parse_strike_price_small_positive() {
        assert_eq!(parse_strike_price("0.05"), 0.05);
    }

    // -----------------------------------------------------------------------
    // parse_tick_size edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_tick_size_valid() {
        assert_eq!(parse_tick_size("0.05").unwrap(), 0.05);
    }

    #[test]
    fn test_parse_tick_size_valid_integer() {
        assert_eq!(parse_tick_size("1").unwrap(), 1.0);
    }

    #[test]
    fn test_parse_tick_size_empty_fails() {
        assert!(parse_tick_size("").is_err());
    }

    #[test]
    fn test_parse_tick_size_zero_fails() {
        assert!(parse_tick_size("0").is_err());
    }

    #[test]
    fn test_parse_tick_size_negative_fails() {
        assert!(parse_tick_size("-0.05").is_err());
    }

    #[test]
    fn test_parse_tick_size_nan_fails() {
        assert!(parse_tick_size("NaN").is_err());
    }

    #[test]
    fn test_parse_tick_size_inf_fails() {
        assert!(parse_tick_size("inf").is_err());
    }

    #[test]
    fn test_parse_tick_size_garbage_fails() {
        assert!(parse_tick_size("xyz").is_err());
    }

    // -----------------------------------------------------------------------
    // parse_expiry_date additional edge cases
    // (basic tests already exist above)
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_expiry_date_impossible_date() {
        // Feb 30 doesn't exist
        assert_eq!(parse_expiry_date("2026-02-30"), None);
    }

    #[test]
    fn test_parse_expiry_date_garbage_string() {
        assert_eq!(parse_expiry_date("not-a-date"), None);
    }

    // -----------------------------------------------------------------------
    // parse_instrument_csv() public API tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_instrument_csv_below_min_rows_returns_error() {
        // build_mock_csv() has ~33 data rows — far below INSTRUMENT_CSV_MIN_ROWS (100,000).
        // parse_instrument_csv should return an error about insufficient row count.
        let csv_text = build_mock_csv();
        let result = parse_instrument_csv(&csv_text);
        assert!(result.is_err(), "expected error for CSV with too few rows");
        let error_message = format!("{}", result.unwrap_err());
        assert!(
            error_message.contains("rows"),
            "error should mention row count, got: {}",
            error_message
        );
        assert!(
            error_message.contains(&INSTRUMENT_CSV_MIN_ROWS.to_string()),
            "error should mention the minimum threshold {}, got: {}",
            INSTRUMENT_CSV_MIN_ROWS,
            error_message
        );
    }

    #[test]
    fn test_parse_row_unknown_exchange_returns_error() {
        // Build a single-row CSV with exchange "MCX" (not NSE/BSE).
        // parse_row should return an error about unknown exchange.
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            "MCX,D,99999,NA,FUTCOM,500,CRUDEOIL,CRUDEOIL-FUT,CRUDE OIL FUT,FUT,NA,\
             100.0,2026-03-28,-0.01,XX,1.0,M,"
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let result = parse_row(&record, &indices);
        assert!(result.is_err(), "expected error for unknown exchange MCX");
        let error_message = format!("{}", result.unwrap_err());
        assert!(
            error_message.contains("unknown exchange"),
            "error should mention unknown exchange, got: {}",
            error_message
        );
    }

    #[test]
    fn test_parse_row_empty_segment_field_returns_error() {
        // Build a row where the segment field is empty.
        // parse_row should return an error about empty segment.
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            // Exchange is NSE (valid), but segment is empty
            "NSE,,12345,NA,INDEX,12345,NIFTY,NIFTY,NIFTY Index,INDEX,NA,\
             1.0,0001-01-01,,XX,0.0500,N,"
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let result = parse_row(&record, &indices);
        assert!(result.is_err(), "expected error for empty segment field");
        let error_message = format!("{}", result.unwrap_err());
        assert!(
            error_message.contains("empty segment"),
            "error should mention empty segment, got: {}",
            error_message
        );
    }

    #[test]
    fn test_parse_row_unparsable_security_id_returns_error() {
        // Build a row where SEM_SMST_SECURITY_ID is "abc" (not a number).
        // parse_row should return an error about invalid security_id.
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            "NSE,I,abc,NA,INDEX,13,NIFTY,NIFTY,NIFTY Index,INDEX,NA,\
             1.0,0001-01-01,,XX,0.0500,N,"
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let result = parse_row(&record, &indices);
        assert!(
            result.is_err(),
            "expected error for non-numeric security_id"
        );
        let error_message = format!("{}", result.unwrap_err());
        assert!(
            error_message.contains("invalid security_id"),
            "error should mention invalid security_id, got: {}",
            error_message
        );
    }

    #[test]
    fn test_parse_instrument_csv_skips_malformed_rows_gracefully() {
        // Build a CSV with a mix of valid and malformed rows.
        // The function should not panic. It will return Err due to
        // INSTRUMENT_CSV_MIN_ROWS, but the important thing is it processes
        // gracefully without panicking on malformed data.
        let header = build_mock_csv_header();
        let valid_row = build_nse_index_row(13, "NIFTY");
        // Malformed row: unbalanced quotes cause CSV parse error
        let malformed_row = r#"NSE,I,"unclosed quote,NA,INDEX,13,NIFTY,NIFTY,NIFTY,INDEX,NA,1.0,0001-01-01,,XX,0.05,N,"#;
        let another_valid_row = build_nse_index_row(25, "BANKNIFTY");

        let csv_text = format!(
            "{}\n{}\n{}\n{}",
            header, valid_row, malformed_row, another_valid_row
        );

        // Must not panic — graceful handling of malformed rows is the goal.
        let result = parse_instrument_csv(&csv_text);

        // The function returns Err because total rows < INSTRUMENT_CSV_MIN_ROWS,
        // but the key assertion is that we reached here without panicking.
        assert!(
            result.is_err(),
            "expected error due to insufficient row count"
        );
        let error_message = format!("{}", result.unwrap_err());
        assert!(
            error_message.contains("rows"),
            "error should be about row count (not a panic from malformed data), got: {}",
            error_message
        );
    }

    // --- Additional coverage tests ---

    #[test]
    fn test_parse_instrument_csv_empty_string_returns_error() {
        // Completely empty CSV — no header
        let result = parse_instrument_csv("");
        assert!(result.is_err(), "empty CSV should fail");
    }

    #[test]
    fn test_parse_instrument_csv_header_only_returns_error() {
        // Header only, no data rows — row count = 0 < INSTRUMENT_CSV_MIN_ROWS
        let csv_text = build_mock_csv_header();
        let result = parse_instrument_csv(&csv_text);
        assert!(
            result.is_err(),
            "header-only CSV should fail due to min rows"
        );
    }

    #[test]
    fn test_parse_instrument_csv_missing_required_column_returns_error() {
        // Header missing SECURITY_ID column
        let bad_header = "EXCH_ID,SEGMENT,MISSING_COL,ISIN";
        let csv_text = format!("{}\nNSE,I,13,NA", bad_header);
        let result = parse_instrument_csv(&csv_text);
        assert!(result.is_err(), "missing required column should fail");
    }

    #[test]
    fn test_parse_row_bse_exchange_parsed_correctly() {
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            "BSE,I,51,NA,INDEX,51,SENSEX,SENSEX,S&P BSE SENSEX,INDEX,NA,1.0,0001-01-01,,XX,0.0100,N,"
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let row = parse_row(&record, &indices).unwrap();

        assert_eq!(row.exchange, Exchange::BombayStockExchange);
        assert_eq!(row.segment, 'I');
        assert_eq!(row.security_id, 51);
    }

    #[test]
    fn test_parse_row_invalid_lot_size_returns_error() {
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            "NSE,I,13,NA,INDEX,13,NIFTY,NIFTY,NIFTY Index,INDEX,NA,abc,0001-01-01,,XX,0.0500,N,"
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let result = parse_row(&record, &indices);
        assert!(result.is_err(), "invalid lot_size should fail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("lot_size"),
            "error should mention lot_size: {}",
            err_msg
        );
    }

    #[test]
    fn test_parse_row_invalid_tick_size_returns_error() {
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            "NSE,I,13,NA,INDEX,13,NIFTY,NIFTY,NIFTY Index,INDEX,NA,1.0,0001-01-01,,XX,xyz,N,"
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let result = parse_row(&record, &indices);
        assert!(result.is_err(), "invalid tick_size should fail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("tick_size"),
            "error should mention tick_size: {}",
            err_msg
        );
    }

    #[test]
    fn test_parse_row_put_option_type() {
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            build_nse_optidx_row(70002, 26000, "NIFTY", "2026-03-30", 22000.0, "PE", 75)
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let row = parse_row(&record, &indices).unwrap();

        assert_eq!(row.option_type, Some(OptionType::Put));
    }

    #[test]
    fn test_parse_row_unknown_option_type_is_none() {
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            "NSE,D,70099,NA,OPTIDX,26000,NIFTY,NIFTY-2026-03-30-22000-XX,NIFTY OPT,OP,NA,75.0,2026-03-30,22000.00000,XX,5.0000,M,"
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let row = parse_row(&record, &indices).unwrap();

        assert_eq!(
            row.option_type, None,
            "XX is not CE or PE, so option_type should be None"
        );
    }

    #[test]
    fn test_parse_row_underlying_security_id_unparseable_defaults_to_zero() {
        // When underlying_security_id cannot be parsed, it defaults to 0
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            "NSE,E,2885,INE000A00000,EQUITY,abc,RELIANCE,RELIANCE LTD,RELIANCE,ES,EQ,1.0,,,,5.0000,NA,"
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let row = parse_row(&record, &indices).unwrap();

        assert_eq!(
            row.underlying_security_id, 0,
            "unparseable underlying_security_id should default to 0"
        );
    }

    #[test]
    fn test_parse_row_expiry_date_parsed_correctly() {
        let csv_text = format!(
            "{}\n{}",
            build_mock_csv_header(),
            build_nse_futidx_row(51700, 26000, "NIFTY", "2026-03-30", 75)
        );
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(csv_text.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        let record = reader.records().next().unwrap().unwrap();
        let row = parse_row(&record, &indices).unwrap();

        assert_eq!(
            row.expiry_date,
            Some(NaiveDate::from_ymd_opt(2026, 3, 30).unwrap())
        );
    }

    #[test]
    fn test_parse_row_zero_strike_returns_zero() {
        assert_eq!(parse_strike_price("0"), 0.0);
    }

    #[test]
    fn test_parse_row_negative_one_strike_returns_zero() {
        assert_eq!(parse_strike_price("-1.0"), 0.0);
    }

    #[test]
    fn test_parse_lot_size_negative_inf_fails() {
        assert!(parse_lot_size("-inf").is_err());
    }

    #[test]
    fn test_detect_column_indices_all_columns_found() {
        let header_str = build_mock_csv_header();
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(header_str.as_bytes());

        let headers = reader.headers().unwrap().clone();
        let indices = detect_column_indices(&headers).unwrap();

        // Verify all expected column indices
        assert_eq!(indices.exch_id, 0);
        assert_eq!(indices.segment, 1);
        assert_eq!(indices.security_id, 2);
        assert_eq!(indices.instrument, 4);
        assert_eq!(indices.underlying_security_id, 5);
        assert_eq!(indices.underlying_symbol, 6);
        assert_eq!(indices.symbol_name, 7);
        assert_eq!(indices.display_name, 8);
        assert_eq!(indices.series, 10);
        assert_eq!(indices.lot_size, 11);
        assert_eq!(indices.expiry_date, 12);
        assert_eq!(indices.strike_price, 13);
        assert_eq!(indices.option_type, 14);
        assert_eq!(indices.tick_size, 15);
        assert_eq!(indices.expiry_flag, 16);
    }

    #[test]
    fn test_should_include_row_nse_unknown_segment_returns_false() {
        assert!(!should_include_row("NSE", "X"));
    }

    #[test]
    fn test_should_include_row_bse_unknown_segment_returns_false() {
        assert!(!should_include_row("BSE", "X"));
    }

    #[test]
    fn test_should_include_row_empty_exchange_returns_false() {
        assert!(!should_include_row("", "I"));
    }

    #[test]
    fn test_should_include_row_empty_segment_returns_false() {
        assert!(!should_include_row("NSE", ""));
    }

    #[test]
    fn test_parse_expiry_date_partial_date_returns_none() {
        assert_eq!(parse_expiry_date("2026-03"), None);
    }

    #[test]
    fn test_parse_expiry_date_wrong_format_dd_mm_yyyy_returns_none() {
        assert_eq!(parse_expiry_date("30-03-2026"), None);
    }

    #[test]
    fn test_parse_lot_size_small_positive_rounds_to_one() {
        assert_eq!(parse_lot_size("0.5").unwrap(), 1);
    }

    #[test]
    fn test_parse_tick_size_very_small_positive() {
        assert_eq!(parse_tick_size("0.001").unwrap(), 0.001);
    }

    #[test]
    fn test_parse_strike_price_positive_negative_boundary() {
        // Positive value should return the value
        assert!(parse_strike_price("0.01") > 0.0);
        // Exactly zero returns 0
        assert_eq!(parse_strike_price("0.0"), 0.0);
    }

    #[test]
    fn test_parse_instrument_csv_with_parse_errors_in_rows() {
        // CSV with valid header but rows that will fail parse_row
        // (e.g., unparseable security_id)
        let header = build_mock_csv_header();
        let valid_row = build_nse_index_row(13, "NIFTY");
        // Row with bad security_id
        let bad_row =
            "NSE,I,abc,NA,INDEX,13,NIFTY,NIFTY,NIFTY Index,INDEX,NA,1.0,0001-01-01,,XX,0.05,N,";
        let csv_text = format!("{}\n{}\n{}", header, valid_row, bad_row);

        // Should not panic — error in parse_row is handled gracefully
        let result = parse_instrument_csv(&csv_text);
        // Will still fail due to < INSTRUMENT_CSV_MIN_ROWS
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_instrument_csv_malformed_csv_record_triggers_warn_and_skip() {
        // Trigger a csv::Error in the records() iterator (lines 118-121).
        // A field starting with a double-quote that is never closed before
        // the record delimiter causes the csv crate to produce an error.
        let header = build_mock_csv_header();
        let valid_row = build_nse_index_row(13, "NIFTY");

        // The segment field starts with `"` but never closes — this causes
        // a csv parse error when the parser expects a closing quote.
        let malformed =
            "NSE,\"I,13,NA,INDEX,13,NIFTY,NIFTY,NIFTY,INDEX,NA,1.0,0001-01-01,,XX,0.05,N,";
        let another_valid = build_nse_index_row(25, "BANKNIFTY");

        let csv_text = format!("{header}\n{valid_row}\n{malformed}\n{another_valid}");

        // parse_instrument_csv should handle gracefully (not panic).
        let result = parse_instrument_csv(&csv_text);
        // Returns Err due to row count < INSTRUMENT_CSV_MIN_ROWS
        assert!(result.is_err(), "expected error due to insufficient rows");
    }
}
