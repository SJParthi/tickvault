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
//! wheel's design).

use serde::Deserialize;

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
    /// Writer-side cold-path provenance: the Groww `groww_symbol` for
    /// indices (e.g. `NSE-NIFTY`) — absent for stocks. Read since
    /// 2026-07-13 (Groww spot-leg INDIA VIX scope) so the per-minute REST
    /// leg can RUNTIME-resolve the VIX candles identity from the day's
    /// master instead of guessing a literal. Missing field → `None`
    /// (additive JSON — old files parse unchanged).
    #[serde(default)]
    pub index_name: Option<String>,
    /// Writer-side cold-path provenance: the Groww DISPLAY name (e.g.
    /// "India Vix") — one of the two canonicalization keys the VIX
    /// resolution matches on. Missing field → `None`.
    #[serde(default)]
    pub symbol_name: Option<String>,
    /// Writer-side cold-path provenance: the constituent ISIN this stock
    /// resolved from (`None` for indices/derivatives — no ISIN). The writer
    /// (`instruments.rs::WatchEntry`) has serialized it for stocks since
    /// PR-A; read since 2026-07-18 so the ORDER-EVT-01 capture consumer can
    /// best-effort resolve a push event's `security_id` by ISIN (review
    /// round 1 Fix 5). Missing field → `None` (additive JSON — old files
    /// parse unchanged).
    #[serde(default)]
    pub isin: Option<String>,
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

/// Why the watch file could not be used. Fail-closed: any problem means the
/// shadow client does NOT subscribe a guessed universe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchReadError {
    /// The JSON did not parse into the documented shape.
    Malformed(String),
}

impl core::fmt::Display for WatchReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "groww watch file: malformed: {e}"),
        }
    }
}

impl std::error::Error for WatchReadError {}

/// Parse the watch-file JSON. Pure.
pub fn parse_watch_file(json: &str) -> Result<WatchFileDoc, WatchReadError> {
    serde_json::from_str(json).map_err(|e| WatchReadError::Malformed(e.to_string()))
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

    /// The reader parses the writer's exact contract (field names +
    /// snake_case kind). (The NATS subject-map builder was retired
    /// 2026-07-15 with the Groww live feed.)
    #[test]
    fn test_parse_watch_file_contract() {
        let doc = parse_watch_file(SAMPLE).expect("sample parses");
        assert_eq!(doc.trading_date_ist, "2026-07-04");
        assert_eq!(doc.entries.len(), 3);
        // Additive JSON (2026-07-13): pre-existing files without the
        // provenance keys still parse — the optional fields read None.
        assert_eq!(doc.entries[2].index_name, None);
        assert_eq!(doc.entries[2].symbol_name, None);
        assert_eq!(doc.entries[0].isin, None);

        assert_eq!(doc.entries[0].security_id, 2885);
        assert_eq!(doc.entries[0].kind, WatchFileKind::Ltp);
        assert_eq!(doc.entries[0].exchange, "NSE");
        assert_eq!(doc.entries[1].security_id, 500325);
        assert_eq!(doc.entries[1].exchange, "BSE");
        assert_eq!(doc.entries[2].kind, WatchFileKind::IndexValue);
        assert_eq!(doc.entries[2].security_id, 4_611_686_018_427_387_917);
        assert_eq!(doc.entries[2].exchange_token, "NIFTY");
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
        // 2026-07-13 (Groww spot-leg INDIA VIX scope): the reader now
        // surfaces the writer's cold-path provenance keys the runtime VIX
        // resolution matches on — index_name (the groww_symbol) +
        // symbol_name (the display name); absent fields parse to None.
        assert_eq!(doc.entries[0].index_name, None);
        assert_eq!(doc.entries[0].symbol_name.as_deref(), Some("TEST"));
        assert_eq!(doc.entries[1].index_name.as_deref(), Some("BSE-SENSEX"));
        assert_eq!(doc.entries[1].symbol_name, None);
        // 2026-07-18 (ORDER-EVT-01 Fix 5): the writer's stock ISIN now
        // round-trips so the capture consumer can resolve security_id by
        // ISIN; index entries without one read None.
        assert_eq!(doc.entries[0].isin.as_deref(), Some("INE000A01001"));
        assert_eq!(doc.entries[1].isin, None);
        assert_eq!(doc.entries.len(), 2);
        assert_eq!(doc.entries[0].exchange_token, "1234");
        assert_eq!(doc.entries[0].kind, WatchFileKind::Ltp);
        assert_eq!(doc.entries[1].kind, WatchFileKind::IndexValue);
        assert_eq!(doc.entries[1].security_id, 51);
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
        assert_eq!(doc.entries.len(), 3);
        assert_eq!(doc.entries[0].segment, "FNO");
        assert_eq!(doc.entries[0].security_id, 61001);
        assert_eq!(doc.entries[1].exchange, "BSE");
        assert_eq!(doc.entries[1].security_id, 71001);
        assert_eq!(doc.entries[2].segment, "CASH");
    }

    #[test]
    fn test_watch_file_malformed_and_empty_fail_closed() {
        assert!(matches!(
            parse_watch_file("not json"),
            Err(WatchReadError::Malformed(_))
        ));
        let doc = parse_watch_file(r#"{"trading_date_ist": "2026-07-04", "entries": []}"#)
            .expect("parses");
        assert!(
            doc.entries.is_empty(),
            "empty entries parse to an empty doc"
        );
    }
}
