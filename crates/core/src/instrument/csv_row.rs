//! The shared instrument-row TYPE that outlived the Dhan CSV parser.
//!
//! PR-C3 (2026-07-14, operator retirement directive 2026-07-13 per
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A/§B —
//! Q3: "hereafter no Dhan instrument download/parsing"): the Dhan Detailed
//! instrument-master CSV downloader + parser (`csv_downloader.rs` +
//! `csv_parser.rs`) are DELETED. [`CsvRow`] is the C1-mandated SPLIT-OUT
//! survivor (the old `csv_parser.rs` mod.rs note: "the Phase C3 deletion of
//! the Dhan CSV chain will split `CsvRow` out before removing this module"):
//! it is the compile-time row shape consumed by the two KEEP modules of the
//! scope-lock §B seam —
//!
//! - `index_extractor.rs` (NSE_INDEX_ALLOWLIST + `canonicalize_index_symbol`
//!   — the Groww watch build consumes the canonicalizer), and
//! - `index_futures.rs` (the §36.7 shared FUTIDX expiry selector — the
//!   GROWW futures leg STANDS after the Dhan retirement).
//!
//! No parse logic lives here — this is a plain data shape (struct + derives,
//! moved verbatim from the deleted parser module).

/// A single instrument row in the Dhan Detailed-CSV field shape. Field
/// names match the Dhan schema (PascalCase columns rendered into
/// snake_case Rust fields).
// NOTE: `Eq` was dropped when the f64 detail fields (`tick_size`,
// `strike_price`) were added — `f64: !Eq`. `CsvRow` is only ever a map
// VALUE / `Vec<CsvRow>`, never a key, so `Eq`/`Hash` are not required.
// `Default` is derived so the (test-only) construction sites can spread
// `..Default::default()` for the optional detail fields.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CsvRow {
    pub security_id: String,
    pub exch_id: String,
    pub segment: String,
    pub instrument: String,
    pub symbol_name: String,
    /// Empty string for non-derivative rows.
    pub underlying_security_id: String,
    // ---- Optional detail columns (Dhan Detailed CSV shape) ----
    // Not mandatory: indices have no lot/strike/expiry; constructors
    // default each to empty/0 when unknown.
    /// `UNDERLYING_SYMBOL` — symbol of the underlying (derivatives only).
    pub underlying_symbol: String,
    /// `DISPLAY_NAME` — human-readable Dhan display name.
    pub display_name: String,
    /// `LOT_SIZE` — trading lot (0 if absent/unparseable).
    pub lot_size: i32,
    /// `TICK_SIZE` — minimum price increment (0.0 if absent/unparseable).
    pub tick_size: f64,
    /// `SM_EXPIRY_DATE` — raw `YYYY-MM-DD` string (empty for spot/index).
    pub expiry_date: String,
    /// `STRIKE_PRICE` — options only (0.0 if absent/unparseable).
    pub strike_price: f64,
    /// `OPTION_TYPE` — `CE` / `PE` / empty for non-options.
    pub option_type: String,
    /// `ISIN` — the 12-char security identity (e.g. `INE002A01018`).
    /// Empty when absent.
    pub isin: String,
}
