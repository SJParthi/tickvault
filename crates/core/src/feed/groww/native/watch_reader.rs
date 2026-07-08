//! Reader for the Rust-built Groww watch file
//! (`data/groww/groww-watch-<date>.json`) — the SAME single-source file the
//! Python sidecar subscribes from, so the shadow client's universe can never
//! drift from the sidecar's.
//!
//! Writer counterpart: `crate::feed::groww::instruments::serialize_watch_file`
//! (this reader is round-trip tested against it). The shadow client never
//! builds its own universe — one source, one file, both consumers.
//!
//! Output: the NATS subject → tick identity map the read loop uses per MSG
//! (identity is NOT in the tick payload — it comes from the subject, per the
//! wheel's design; see `crate::feed::groww::subjects`).

use std::collections::HashMap;

use serde::Deserialize;
use tickvault_common::types::ExchangeSegment;

use crate::feed::groww::subjects;

/// Canonical segment strings (the bridge contract — mirrors the sidecar's
/// `SEGMENT_MAP` + `CANONICAL_INDEX_SEGMENT`).
pub const SEGMENT_IDX: &str = "IDX_I";

/// A watch-file entry as READ (the writer-side struct in `instruments.rs`
/// carries extra optional provenance fields we ignore here).
#[derive(Debug, Deserialize)]
pub struct WatchFileEntry {
    /// Groww exchange (`NSE` / `BSE`).
    pub exchange: String,
    /// Groww segment (`CASH` / `FNO`).
    pub segment: String,
    /// Groww exchange token (numeric string for stocks, NAME for NSE indices).
    pub exchange_token: String,
    /// `ltp` (stocks/F&O) vs `index_value` (indices).
    pub kind: WatchFileKind,
    /// The integer id stored in the shared `ticks` table / NDJSON lines.
    pub security_id: i64,
}

/// Mirror of the writer's `WatchKind` (`#[serde(rename_all = "snake_case")]`).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WatchFileKind {
    /// Cash equity / derivative → live-price subject.
    Ltp,
    /// Index → index-value subject.
    IndexValue,
}

/// The watch file document (`serialize_watch_file` output).
#[derive(Debug, Deserialize)]
pub struct WatchFileDoc {
    /// The IST trading date this file was built for (`YYYY-MM-DD`).
    pub trading_date_ist: String,
    /// The subscribe entries.
    pub entries: Vec<WatchFileEntry>,
}

/// The identity a delivered MSG maps to. `Copy` — hot-path lookup value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickIdentity {
    /// The `security_id` the NDJSON line carries.
    pub security_id: i64,
    /// Canonical segment string (`NSE_EQ` / `NSE_FNO` / `BSE_EQ` / `BSE_FNO`
    /// / `IDX_I`) — `&'static str` so the hot path never allocates.
    pub segment: &'static str,
}

/// Why the watch file could not be used. Fail-closed: any problem means the
/// shadow client does NOT subscribe a guessed universe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchReadError {
    /// The JSON did not parse into the documented shape.
    Malformed(String),
    /// The file parsed but produced zero usable subjects.
    EmptySubjects,
}

impl core::fmt::Display for WatchReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "groww watch file: malformed: {e}"),
            Self::EmptySubjects => f.write_str("groww watch file: zero usable subjects"),
        }
    }
}

impl std::error::Error for WatchReadError {}

/// Parse the watch-file JSON. Pure.
pub fn parse_watch_file(json: &str) -> Result<WatchFileDoc, WatchReadError> {
    serde_json::from_str(json).map_err(|e| WatchReadError::Malformed(e.to_string()))
}

/// Map a stock/F&O entry's `(exchange, segment)` to our typed segment.
/// Mirrors the sidecar's `SEGMENT_MAP`. `None` = unknown combination
/// (skipped + counted by the caller, never a panic).
#[must_use]
pub fn stock_segment(exchange: &str, segment: &str) -> Option<ExchangeSegment> {
    match (exchange, segment) {
        ("NSE", "CASH") => Some(ExchangeSegment::NseEquity),
        ("NSE", "FNO") => Some(ExchangeSegment::NseFno),
        ("BSE", "CASH") => Some(ExchangeSegment::BseEquity),
        ("BSE", "FNO") => Some(ExchangeSegment::BseFno),
        _ => None,
    }
}

/// Canonical NDJSON segment string for a stock segment.
#[must_use]
pub const fn canonical_segment_str(segment: ExchangeSegment) -> &'static str {
    match segment {
        ExchangeSegment::NseEquity => "NSE_EQ",
        ExchangeSegment::NseFno => "NSE_FNO",
        ExchangeSegment::BseEquity => "BSE_EQ",
        ExchangeSegment::BseFno => "BSE_FNO",
        _ => SEGMENT_IDX,
    }
}

/// Build the subject → identity map (and the subject list to `SUB`).
///
/// Cold path (once per connect). Unknown exchange/segment combinations are
/// skipped and counted in the returned `skipped`; duplicate subjects keep the
/// FIRST entry (deterministic) and count as skipped.
pub fn build_subject_map(
    doc: &WatchFileDoc,
) -> Result<(HashMap<String, TickIdentity>, usize), WatchReadError> {
    let mut map: HashMap<String, TickIdentity> = HashMap::with_capacity(doc.entries.len());
    let mut skipped = 0usize;
    for entry in &doc.entries {
        let subject = match entry.kind {
            WatchFileKind::Ltp => match stock_segment(&entry.exchange, &entry.segment) {
                Some(seg) => match subjects::live_price_subject(seg, &entry.exchange_token) {
                    Some(subject) => (subject, canonical_segment_str(seg)),
                    None => {
                        skipped += 1;
                        continue;
                    }
                },
                None => {
                    skipped += 1;
                    continue;
                }
            },
            WatchFileKind::IndexValue => (
                subjects::index_value_subject(entry.exchange == "BSE", &entry.exchange_token),
                SEGMENT_IDX,
            ),
        };
        let (subject, segment) = subject;
        if map.contains_key(&subject) {
            skipped += 1;
            continue;
        }
        map.insert(
            subject,
            TickIdentity {
                security_id: entry.security_id,
                segment,
            },
        );
    }
    if map.is_empty() {
        return Err(WatchReadError::EmptySubjects);
    }
    Ok((map, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "trading_date_ist": "2026-07-04",
        "feed": "groww",
        "count": 3,
        "resolved_stocks": 2,
        "indices": 1,
        "entries": [
            {"exchange": "NSE", "segment": "CASH", "exchange_token": "2885",
             "kind": "ltp", "security_id": 2885},
            {"exchange": "BSE", "segment": "CASH", "exchange_token": "500325",
             "kind": "ltp", "security_id": 500325},
            {"exchange": "NSE", "segment": "CASH", "exchange_token": "NIFTY",
             "kind": "index_value", "security_id": 4611686018427387917}
        ]
    }"#;

    /// The reader parses the writer's exact contract (field names + snake_case
    /// kind) and builds the subject map with the wheel-verified prefixes.
    #[test]
    fn test_parse_watch_file_and_build_subject_map() {
        let doc = parse_watch_file(SAMPLE).expect("sample parses");
        assert_eq!(doc.trading_date_ist, "2026-07-04");
        assert_eq!(doc.entries.len(), 3);

        let (map, skipped) = build_subject_map(&doc).expect("map builds");
        assert_eq!(skipped, 0);
        assert_eq!(
            map.get("/ld/eq/nse/price.2885"),
            Some(&TickIdentity {
                security_id: 2885,
                segment: "NSE_EQ"
            })
        );
        assert_eq!(
            map.get("/ld/eq/bse/price.500325"),
            Some(&TickIdentity {
                security_id: 500325,
                segment: "BSE_EQ"
            })
        );
        assert_eq!(
            map.get("/ld/indices/nse/price.NIFTY"),
            Some(&TickIdentity {
                security_id: 4611686018427387917,
                segment: "IDX_I"
            })
        );
    }

    /// Round-trip against the REAL writer: serialize a watch set with the
    /// production serializer, read it back with this reader.
    #[test]
    fn test_watch_file_roundtrip_with_serializer() {
        use crate::feed::groww::instruments::{GrowwWatchSet, WatchEntry, WatchKind};
        let set = GrowwWatchSet {
            entries: vec![
                WatchEntry {
                    exchange: "NSE".to_owned(),
                    segment: "CASH".to_owned(),
                    exchange_token: "1234".to_owned(),
                    kind: WatchKind::Ltp,
                    security_id: 1234,
                    isin: Some("INE000A01001".to_owned()),
                    symbol_name: Some("TEST".to_owned()),
                    index_name: None,
                    expiry_date: None,
                    underlying_symbol: None,
                },
                WatchEntry {
                    exchange: "BSE".to_owned(),
                    segment: "CASH".to_owned(),
                    exchange_token: "1".to_owned(),
                    kind: WatchKind::IndexValue,
                    security_id: 51,
                    isin: None,
                    symbol_name: None,
                    index_name: Some("BSE-SENSEX".to_owned()),
                    expiry_date: None,
                    underlying_symbol: None,
                },
            ],
            master_entries: vec![],
            resolved_stocks: 1,
            unresolved_stocks: vec![],
            indices: 1,
        };
        let json =
            crate::feed::groww::instruments::serialize_watch_file_for_test(&set, "2026-07-04");
        let doc = parse_watch_file(&json).expect("writer output parses");
        let (map, skipped) = build_subject_map(&doc).expect("map builds");
        assert_eq!(skipped, 0);
        assert!(map.contains_key("/ld/eq/nse/price.1234"));
        assert_eq!(
            map.get("/ld/indices/bse/price.1"),
            Some(&TickIdentity {
                security_id: 51,
                segment: "IDX_I"
            })
        );
    }

    /// §36 (2026-07-08): an FNO index-future entry round-trips through the
    /// REAL writer → reader → subject map: segment NSE_FNO/BSE_FNO, subject
    /// `/ld/fo/{nse,bse}/price.<token>`; the new optional `expiry_date`
    /// provenance field is skipped-when-None for old entries and IGNORED by
    /// this reader (additive JSON).
    #[test]
    fn test_watch_file_roundtrip_with_fno_entries() {
        use crate::feed::groww::instruments::{GrowwWatchSet, WatchEntry, WatchKind};
        let set = GrowwWatchSet {
            entries: vec![
                WatchEntry {
                    exchange: "NSE".to_owned(),
                    segment: "FNO".to_owned(),
                    exchange_token: "61001".to_owned(),
                    kind: WatchKind::Ltp,
                    security_id: 61001,
                    isin: None,
                    symbol_name: Some("NSE-NIFTY-30Jul26-FUT".to_owned()),
                    index_name: None,
                    expiry_date: Some("2026-07-30".to_owned()),
                    underlying_symbol: None,
                },
                WatchEntry {
                    exchange: "BSE".to_owned(),
                    segment: "FNO".to_owned(),
                    exchange_token: "71001".to_owned(),
                    kind: WatchKind::Ltp,
                    security_id: 71001,
                    isin: None,
                    symbol_name: Some("BSE-SENSEX-31Jul26-FUT".to_owned()),
                    index_name: None,
                    expiry_date: Some("2026-07-31".to_owned()),
                    underlying_symbol: None,
                },
                // Old-shape stock entry: expiry_date None → field absent on wire.
                WatchEntry {
                    exchange: "NSE".to_owned(),
                    segment: "CASH".to_owned(),
                    exchange_token: "2885".to_owned(),
                    kind: WatchKind::Ltp,
                    security_id: 2885,
                    isin: Some("INE002A01018".to_owned()),
                    symbol_name: Some("RELIANCE".to_owned()),
                    index_name: None,
                    expiry_date: None,
                    underlying_symbol: None,
                },
            ],
            master_entries: vec![],
            resolved_stocks: 1,
            unresolved_stocks: vec![],
            indices: 0,
        };
        let json =
            crate::feed::groww::instruments::serialize_watch_file_for_test(&set, "2026-07-08");
        assert!(
            json.contains("\"expiry_date\": \"2026-07-30\""),
            "FNO entry carries the provenance expiry"
        );
        let doc = parse_watch_file(&json).expect("writer output parses");
        let (map, skipped) = build_subject_map(&doc).expect("map builds");
        assert_eq!(skipped, 0);
        assert_eq!(
            map.get("/ld/fo/nse/price.61001"),
            Some(&TickIdentity {
                security_id: 61001,
                segment: "NSE_FNO"
            })
        );
        assert_eq!(
            map.get("/ld/fo/bse/price.71001"),
            Some(&TickIdentity {
                security_id: 71001,
                segment: "BSE_FNO"
            })
        );
    }

    /// Unknown segment combos + duplicate subjects are skipped-and-counted,
    /// never a panic; an all-skipped file fails closed.
    #[test]
    fn test_watch_file_skips_unknown_and_duplicates() {
        let doc = parse_watch_file(
            r#"{
            "trading_date_ist": "2026-07-04",
            "entries": [
                {"exchange": "MCX", "segment": "COMM", "exchange_token": "9",
                 "kind": "ltp", "security_id": 9},
                {"exchange": "NSE", "segment": "CASH", "exchange_token": "7",
                 "kind": "ltp", "security_id": 7},
                {"exchange": "NSE", "segment": "CASH", "exchange_token": "7",
                 "kind": "ltp", "security_id": 8}
            ]
        }"#,
        )
        .expect("parses");
        let (map, skipped) = build_subject_map(&doc).expect("one usable subject");
        assert_eq!(map.len(), 1);
        assert_eq!(skipped, 2);
        // The FIRST entry for a duplicate subject wins (deterministic).
        assert_eq!(
            map.get("/ld/eq/nse/price.7").map(|i| i.security_id),
            Some(7)
        );
    }

    #[test]
    fn test_watch_file_malformed_and_empty_fail_closed() {
        assert!(matches!(
            parse_watch_file("not json"),
            Err(WatchReadError::Malformed(_))
        ));
        let doc = parse_watch_file(r#"{"trading_date_ist": "2026-07-04", "entries": []}"#)
            .expect("parses");
        assert_eq!(build_subject_map(&doc), Err(WatchReadError::EmptySubjects));
    }

    #[test]
    fn test_stock_segment_mapping_matches_sidecar_segment_map() {
        assert_eq!(
            stock_segment("NSE", "CASH"),
            Some(ExchangeSegment::NseEquity)
        );
        assert_eq!(stock_segment("NSE", "FNO"), Some(ExchangeSegment::NseFno));
        assert_eq!(
            stock_segment("BSE", "CASH"),
            Some(ExchangeSegment::BseEquity)
        );
        assert_eq!(stock_segment("BSE", "FNO"), Some(ExchangeSegment::BseFno));
        assert_eq!(stock_segment("MCX", "COMM"), None);
    }
}
