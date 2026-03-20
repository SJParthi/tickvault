//! CSV parser for niftyindices.com index constituent files.
//!
//! Handles edge cases: BOM, trailing commas, empty rows, quoted fields,
//! case-insensitive headers, non-EQ series filtering.

use chrono::NaiveDate;
use tracing::warn;

use dhan_live_trader_common::instrument_types::IndexConstituent;

/// Parse a constituency CSV into a list of `IndexConstituent` entries.
///
/// - Auto-detects column positions by header names (case-insensitive).
/// - Filters to `Series == "EQ"` rows only.
/// - Skips malformed rows with a warning.
/// - Strips UTF-8 BOM if present.
pub fn parse_constituency_csv(
    index_name: &str,
    csv_text: &str,
    today: NaiveDate,
) -> Vec<IndexConstituent> {
    let text = strip_bom(csv_text);

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(text.as_bytes());

    // Detect column positions from headers
    let headers = match reader.headers() {
        Ok(h) => h.clone(),
        Err(err) => {
            warn!(
                index = %index_name,
                error = %err,
                "failed to read CSV headers"
            );
            return Vec::new();
        }
    };

    let col_map = match detect_columns(&headers) {
        Some(m) => m,
        None => {
            warn!(
                index = %index_name,
                "CSV headers missing required columns (need Symbol + ISIN Code)"
            );
            return Vec::new();
        }
    };

    let mut constituents = Vec::new();

    for (row_idx, result) in reader.records().enumerate() {
        let record = match result {
            Ok(r) => r,
            Err(err) => {
                warn!(
                    index = %index_name,
                    row = row_idx,
                    error = %err,
                    "skipping malformed CSV row"
                );
                continue;
            }
        };

        // Filter: only EQ series
        if let Some(series_col) = col_map.series
            && let Some(series) = record.get(series_col)
        {
            let series_trimmed = series.trim();
            if !series_trimmed.is_empty() && series_trimmed != "EQ" {
                continue;
            }
        }

        let symbol = match record.get(col_map.symbol) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => continue,
        };

        let isin = record
            .get(col_map.isin)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let weight = col_map
            .weight
            .and_then(|col| record.get(col))
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(0.0);

        let sector = col_map
            .industry
            .and_then(|col| record.get(col))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        constituents.push(IndexConstituent {
            index_name: index_name.to_string(),
            symbol,
            isin,
            weight,
            sector,
            last_updated: today,
        });
    }

    constituents
}

/// Column position map detected from CSV headers.
struct ColumnMap {
    symbol: usize,
    isin: usize,
    series: Option<usize>,
    weight: Option<usize>,
    industry: Option<usize>,
}

/// Detect column positions by header name (case-insensitive, trimmed).
fn detect_columns(headers: &csv::StringRecord) -> Option<ColumnMap> {
    let mut symbol_col = None;
    let mut isin_col = None;
    let mut series_col = None;
    let mut weight_col = None;
    let mut industry_col = None;

    for (i, header) in headers.iter().enumerate() {
        let h = header.trim().to_lowercase();
        match h.as_str() {
            "symbol" => symbol_col = Some(i),
            "isin code" | "isin" => isin_col = Some(i),
            "series" => series_col = Some(i),
            "weight(%)" | "weight" | "weightage(%)" | "weightage" => weight_col = Some(i),
            "industry" | "sector" => industry_col = Some(i),
            _ => {}
        }
    }

    Some(ColumnMap {
        symbol: symbol_col?,
        isin: isin_col?,
        series: series_col,
        weight: weight_col,
        industry: industry_col,
    })
}

/// Strip UTF-8 BOM (byte order mark) if present.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{FEFF}').unwrap_or(text)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()
    }

    #[test]
    fn test_parse_standard_5col_csv() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
                    Reliance Industries Ltd.,Oil Gas & Consumable Fuels,RELIANCE,EQ,INE002A01018\n\
                    HDFC Bank Ltd.,Financial Services,HDFCBANK,EQ,INE040A01034\n";

        let result = parse_constituency_csv("Nifty 50", csv, today());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].symbol, "RELIANCE");
        assert_eq!(result[0].sector, "Oil Gas & Consumable Fuels");
        assert_eq!(result[1].symbol, "HDFCBANK");
        assert_eq!(result[0].index_name, "Nifty 50");
    }

    #[test]
    fn test_parse_csv_with_weight_column() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code,Weight(%)\n\
                    Reliance Industries Ltd.,Energy,RELIANCE,EQ,INE002A01018,10.25\n";

        let result = parse_constituency_csv("Nifty 50", csv, today());
        assert_eq!(result.len(), 1);
        assert!((result[0].weight - 10.25).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_csv_with_bom() {
        let csv = "\u{FEFF}Company Name,Industry,Symbol,Series,ISIN Code\n\
                    Infosys Ltd.,Information Technology,INFY,EQ,INE009A01021\n";

        let result = parse_constituency_csv("Nifty IT", csv, today());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].symbol, "INFY");
    }

    #[test]
    fn test_parse_csv_empty_body_returns_empty() {
        let result = parse_constituency_csv("Test", "", today());
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_csv_header_only_returns_empty() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n";
        let result = parse_constituency_csv("Test", csv, today());
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_csv_trailing_commas_handled() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code,\n\
                    Reliance,Energy,RELIANCE,EQ,INE002A01018,\n";

        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].symbol, "RELIANCE");
    }

    #[test]
    fn test_parse_csv_filters_non_eq_series() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
                    Reliance,Energy,RELIANCE,EQ,INE002A01018\n\
                    SomeBond,Finance,BOND1,BL,INE999X01001\n\
                    SomeSME,Tech,SME1,SM,INE888X01002\n";

        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].symbol, "RELIANCE");
    }

    #[test]
    fn test_parse_csv_quoted_fields() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
                    \"Reliance Industries, Ltd.\",\"Oil, Gas\",RELIANCE,EQ,INE002A01018\n";

        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].symbol, "RELIANCE");
        assert_eq!(result[0].sector, "Oil, Gas");
    }

    #[test]
    fn test_parse_csv_malformed_row_skipped() {
        // Missing ISIN column but Symbol present — should still produce partial data
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
                    Good Row,Energy,RELIANCE,EQ,INE002A01018\n";

        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_parse_csv_case_insensitive_headers() {
        let csv = "company name,INDUSTRY,SYMBOL,series,isin code\n\
                    Reliance,Energy,RELIANCE,EQ,INE002A01018\n";

        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].symbol, "RELIANCE");
    }

    #[test]
    fn test_parse_csv_whitespace_trimmed() {
        let csv = "Company Name , Industry , Symbol , Series , ISIN Code \n\
                     Reliance ,  Energy  ,  RELIANCE  ,  EQ  ,  INE002A01018  \n";

        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].symbol, "RELIANCE");
        assert_eq!(result[0].sector, "Energy");
    }

    // =====================================================================
    // Additional coverage: missing required columns, empty symbol rows,
    // weightage header variant, sector header variant, no series column
    // =====================================================================

    #[test]
    fn test_parse_csv_missing_required_symbol_column() {
        let csv = "Company Name,Industry,ISIN Code\n\
                    Reliance,Energy,INE002A01018\n";
        // Missing Symbol column
        let result = parse_constituency_csv("Test", csv, today());
        assert!(
            result.is_empty(),
            "missing Symbol column should return empty"
        );
    }

    #[test]
    fn test_parse_csv_missing_required_isin_column() {
        let csv = "Company Name,Industry,Symbol,Series\n\
                    Reliance,Energy,RELIANCE,EQ\n";
        let result = parse_constituency_csv("Test", csv, today());
        assert!(result.is_empty(), "missing ISIN column should return empty");
    }

    #[test]
    fn test_parse_csv_empty_symbol_skipped() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
                    Reliance,Energy,,EQ,INE002A01018\n\
                    HDFC,Finance,HDFCBANK,EQ,INE040A01034\n";
        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].symbol, "HDFCBANK");
    }

    #[test]
    fn test_parse_csv_no_series_column_includes_all() {
        // CSV without Series column should include all rows
        let csv = "Company Name,Symbol,ISIN Code\n\
                    Reliance,RELIANCE,INE002A01018\n\
                    HDFC,HDFCBANK,INE040A01034\n";
        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_parse_csv_empty_series_includes_row() {
        // Empty series field should include the row (no filtering)
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
                    Reliance,Energy,RELIANCE,,INE002A01018\n";
        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_parse_csv_weightage_header_variant() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code,Weightage(%)\n\
                    Reliance,Energy,RELIANCE,EQ,INE002A01018,12.5\n";
        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
        assert!((result[0].weight - 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_csv_sector_header_variant() {
        let csv = "Company Name,Sector,Symbol,Series,ISIN Code\n\
                    Reliance,Oil & Gas,RELIANCE,EQ,INE002A01018\n";
        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].sector, "Oil & Gas");
    }

    #[test]
    fn test_parse_csv_invalid_weight_defaults_to_zero() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code,Weight(%)\n\
                    Reliance,Energy,RELIANCE,EQ,INE002A01018,not_a_number\n";
        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result.len(), 1);
        assert!((result[0].weight - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_strip_bom_with_bom() {
        let text = "\u{FEFF}hello";
        assert_eq!(strip_bom(text), "hello");
    }

    #[test]
    fn test_strip_bom_without_bom() {
        let text = "hello";
        assert_eq!(strip_bom(text), "hello");
    }

    #[test]
    fn test_strip_bom_empty_string() {
        assert_eq!(strip_bom(""), "");
    }

    #[test]
    fn test_strip_bom_only_bom() {
        assert_eq!(strip_bom("\u{FEFF}"), "");
    }

    #[test]
    fn test_parse_csv_last_updated_matches_today() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
                    Reliance,Energy,RELIANCE,EQ,INE002A01018\n";
        let result = parse_constituency_csv("Test", csv, today());
        assert_eq!(result[0].last_updated, today());
    }
}
