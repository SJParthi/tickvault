//! Groww watch-list builder — PURE CORE (PR-B1, operator lock §31/§32, 2026-06-20).
//!
//! Builds the ~779-instrument Groww subscription set (NIFTY-Total-Market stocks +
//! NSE indices) by joining the NTM constituent **ISINs** to Groww's own
//! `exchange_token`s from Groww's master `instrument.csv`. The resolved set is
//! written to a watch file the Python sidecar reads (subscribe_ltp for stocks +
//! subscribe_index_value for indices).
//!
//! This module is the **pure, fully-unit-tested core**: CSV parse (header-name
//! based — Groww adds columns), the O(1) ISIN→token map, the ISIN join with the
//! 2% NTM tolerance, dedup by `(exchange_token, segment)`, the `[100,1200]`
//! universe envelope, the 1000-subscription Groww cap, and the deterministic
//! `max_subscribe` first-run cap (indices first, then stocks by ISIN ascending).
//! The network download + atomic watch-file write + boot wiring are the
//! TEST-EXEMPT orchestration that calls these primitives (next slice of PR-B1).
//!
//! Honest envelope (operator §F): this builder guarantees **correct, idempotent,
//! fail-closed token resolution**. It cannot guarantee Groww lists every NTM
//! stock — the 2% tolerance + by-name logging make any gap VISIBLE, never silent.
//! Volume is Option A (price-only): the watch set carries no volume concept.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use tracing::{info, warn};

/// Groww master instrument CSV (public static asset, no auth) — re-exported from
/// `tickvault_common::constants` so the URL lives in the single constants source.
pub use tickvault_common::constants::GROWW_INSTRUMENT_CSV_URL;

/// Groww live-feed hard cap: at most this many instruments per subscribe session
/// (verified `07-feed-websocket.md` / `01-introduction-auth.md`). The resolved
/// set MUST NOT exceed this.
pub const GROWW_MAX_SUBSCRIPTIONS: usize = 1000;

/// Lower bound of the sane resolved-universe envelope (§31). Below this, the
/// Groww master was almost certainly truncated/partial → fail-closed.
pub const GROWW_MIN_UNIVERSE: usize = 100;

/// Upper bound of the sane resolved-universe envelope (§31, NTM expansion).
pub const GROWW_MAX_UNIVERSE: usize = 1200;

/// NTM membership tolerance (§31.1(4), operator lock 2026-06-08): if MORE than
/// this fraction of NTM constituents fail to resolve to a Groww token, the build
/// is rejected (degrade decision is the caller's per `NTM-CONSTITUENCY-01`).
pub const GROWW_NTM_DANGLING_TOLERANCE: f64 = 0.02;

/// Which Groww live-feed subscription a watch entry uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchKind {
    /// Cash equity / derivative → `subscribe_ltp`.
    Ltp,
    /// Index → `subscribe_index_value`.
    IndexValue,
}

/// One instrument the sidecar should subscribe. Serializes to the watch-file
/// contract the Python sidecar reads:
/// `{exchange, segment, exchange_token, kind, security_id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WatchEntry {
    /// Groww exchange (`NSE` / `BSE`).
    pub exchange: String,
    /// Groww segment (`CASH` / `FNO`).
    pub segment: String,
    /// Groww exchange token used to SUBSCRIBE — numeric string for stocks,
    /// index NAME for NSE indices (e.g. `NIFTY`), numeric for BSE indices.
    pub exchange_token: String,
    /// LTP vs index-value subscription.
    pub kind: WatchKind,
    /// The integer `security_id` STORED in the shared `ticks` table. For stocks
    /// this is the numeric `exchange_token`; for indices (whose token may be a
    /// name) this is a Groww-native stable id (operator decision 2026-06-21) —
    /// Rust is the single source so the sidecar never re-derives it.
    pub security_id: i64,
    /// COLD-PATH provenance (PR-A) — the constituent ISIN this stock resolved
    /// from (`None` for indices, which have no ISIN). Retained here ONLY for the
    /// daily `instrument_lifecycle` / `index_constituency` master-row build; the
    /// sidecar watch-file contract is unchanged (skipped when `None`, and stocks
    /// always carry it). `WatchEntry` is a cold-path daily-build struct (NOT the
    /// per-tick path — verified: referenced only in this module + tests), so the
    /// owned `String` here costs nothing on the hot path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isin: Option<String>,
    /// COLD-PATH provenance (PR-A) — the human ticker (NTM `Symbol` for stocks).
    /// `None` for indices (they carry `index_name` instead). Cold-path only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    /// COLD-PATH provenance (PR-A) — the Groww `groww_symbol` for indices (e.g.
    /// `NSE-NIFTY`, `BSE-SENSEX`), used as the `index_name` + `symbol_name` of
    /// the master row. `None` for stocks. Cold-path only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_name: Option<String>,
}

/// The assembled Groww watch set + resolution provenance for observability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GrowwWatchSet {
    /// The instruments to SUBSCRIBE live (deduped, **capped**, envelope-checked).
    /// This is the live-feed set the sidecar reads — bounded by `max_subscribe`
    /// (clamped to the Groww 1000 hard cap).
    pub entries: Vec<WatchEntry>,
    /// The FULL deduped, envelope-checked resolved universe BEFORE the optional
    /// `max_subscribe` cap (COLD-PATH provenance, PR — full-universe master). The
    /// shared `instrument_lifecycle` / `index_constituency` master tables iterate
    /// THIS, not `entries`, so the master always records the entire ~767-instrument
    /// universe regardless of the (smaller) live-subscribe cap — exactly as the Dhan
    /// side persists the full `DailyUniverse` independent of subscription. When no
    /// sub-cap is applied (the default), `master_entries == entries`. Cold-path
    /// only (daily build), so the owned clone costs nothing on the hot path.
    pub master_entries: Vec<WatchEntry>,
    /// Count of NTM stocks that resolved to a Groww token.
    pub resolved_stocks: usize,
    /// NTM stock symbols that did NOT resolve (logged by name, never silent).
    pub unresolved_stocks: Vec<String>,
    /// Count of indices resolved.
    pub indices: usize,
}

/// Why a Groww watch-set build fails (fail-closed). Copy where possible; carries
/// counts so the operator alert names the exact problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchBuildError {
    /// Groww master CSV had no usable rows / no header.
    GrowwMasterEmpty,
    /// A mandatory Groww CSV column is missing (header drift) — fail-closed (§26).
    MissingColumn(&'static str),
    /// More than `GROWW_NTM_DANGLING_TOLERANCE` of NTM constituents unresolved.
    NtmDanglingExceeded {
        /// Unresolved constituent count.
        unresolved: usize,
        /// Total NTM constituents considered.
        total: usize,
    },
    /// Resolved universe outside the `[GROWW_MIN_UNIVERSE, GROWW_MAX_UNIVERSE]`
    /// envelope (after the optional cap is NOT applied — this checks the true set).
    UniverseSizeOutOfBounds {
        /// Actual resolved count.
        actual: usize,
        /// Min allowed.
        min: usize,
        /// Max allowed.
        max: usize,
    },
    /// A required CSV (Groww master or NTM list) could not be fetched.
    FetchFailed(String),
    /// The watch file could not be written.
    WriteFailed(String),
}

/// One parsed Groww master row (only the columns the builder needs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrowwInstrumentRow {
    /// `exchange` column (`NSE`/`BSE`).
    pub exchange: String,
    /// `exchange_token` column.
    pub exchange_token: String,
    /// `groww_symbol` column (e.g. `NSE-NIFTY`, `BSE-SENSEX`) — the stable,
    /// globally-unique Groww identity used to derive an index `security_id`.
    pub groww_symbol: String,
    /// `instrument_type` column (`EQ`/`IDX`/`FUT`/`CE`/`PE`).
    pub instrument_type: String,
    /// `segment` column (`CASH`/`FNO`/`COMMODITY`).
    pub segment: String,
    /// `series` column (`EQ`/...), may be empty.
    pub series: String,
    /// `isin` column, may be empty (indices/derivatives have none).
    pub isin: String,
}

/// Resolves a header row to the index of each required column BY NAME (Groww
/// has already added columns, so position-based parsing is banned — §26 M5).
fn header_index(header: &str) -> Result<HashMap<String, usize>, WatchBuildError> {
    let mut idx = HashMap::new();
    for (i, col) in split_csv_line(header).iter().enumerate() {
        idx.insert(col.trim().to_string(), i);
    }
    // Mandatory columns for the builder.
    for required in [
        "exchange",
        "exchange_token",
        "instrument_type",
        "segment",
        "series",
        "isin",
        "groww_symbol",
    ] {
        if !idx.contains_key(required) {
            return Err(WatchBuildError::MissingColumn(match required {
                "exchange" => "exchange",
                "exchange_token" => "exchange_token",
                "instrument_type" => "instrument_type",
                "segment" => "segment",
                "series" => "series",
                "isin" => "isin",
                _ => "groww_symbol",
            }));
        }
    }
    Ok(idx)
}

/// Minimal CSV line splitter handling double-quoted fields with embedded commas.
/// Pure; tolerant of the simple Groww master format (no embedded quotes/newlines).
fn split_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(std::mem::take(&mut cur));
            }
            '\r' => {}
            other => cur.push(other),
        }
    }
    fields.push(cur);
    fields
}

/// Parses the Groww master CSV text into the rows the builder needs. Strips a
/// leading UTF-8 BOM; resolves columns by header name. Rows shorter than the
/// header are skipped (counted as malformed by the caller via the returned len).
fn parse_groww_master(csv: &str) -> Result<Vec<GrowwInstrumentRow>, WatchBuildError> {
    let csv = csv.strip_prefix('\u{feff}').unwrap_or(csv);
    let mut lines = csv.lines();
    let header = lines.next().ok_or(WatchBuildError::GrowwMasterEmpty)?;
    let idx = header_index(header)?;
    let get = |fields: &[String], name: &str| -> String {
        idx.get(name)
            .and_then(|&i| fields.get(i))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let mut rows = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields = split_csv_line(line);
        rows.push(GrowwInstrumentRow {
            exchange: get(&fields, "exchange"),
            exchange_token: get(&fields, "exchange_token"),
            groww_symbol: get(&fields, "groww_symbol"),
            instrument_type: get(&fields, "instrument_type"),
            segment: get(&fields, "segment"),
            series: get(&fields, "series"),
            isin: get(&fields, "isin"),
        });
    }
    if rows.is_empty() {
        return Err(WatchBuildError::GrowwMasterEmpty);
    }
    Ok(rows)
}

/// Builds the O(1) `ISIN → exchange_token` map over NSE cash-equity rows only.
/// Strict filter: `exchange=NSE & segment=CASH & instrument_type=EQ & series=EQ`
/// with a non-empty ISIN + numeric token. An ISIN that maps to MORE than one
/// token is AMBIGUOUS → excluded from the map and returned in `ambiguous` (never
/// silently pick one — §C3). Single O(n) pass.
#[must_use]
fn build_isin_token_map(rows: &[GrowwInstrumentRow]) -> (HashMap<String, String>, Vec<String>) {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut collisions: HashMap<String, usize> = HashMap::new();
    for r in rows {
        if r.exchange == "NSE"
            && r.segment == "CASH"
            && r.instrument_type == "EQ"
            && r.series == "EQ"
            && !r.isin.is_empty()
            && r.exchange_token.parse::<i64>().is_ok()
        {
            match map.get(&r.isin) {
                Some(existing) if existing != &r.exchange_token => {
                    *collisions.entry(r.isin.clone()).or_insert(1) += 1;
                }
                _ => {
                    map.insert(r.isin.clone(), r.exchange_token.clone());
                }
            }
        }
    }
    // Remove every ISIN that ever collided — ambiguous is excluded, not guessed.
    let mut ambiguous: Vec<String> = Vec::new();
    for isin in collisions.keys() {
        map.remove(isin);
        ambiguous.push(isin.clone());
    }
    ambiguous.sort();
    (map, ambiguous)
}

/// The one BSE index we track (`groww_symbol`), per operator §31: NIFTY-set +
/// indices + exactly ONE BSE SENSEX. Identified by its stable `groww_symbol`
/// (token is the numeric `"1"`, which alone is not distinctive — the symbol is).
const BSE_SENSEX_GROWW_SYMBOL: &str = "BSE-SENSEX";

/// Bit set on every index `security_id` to put it in the `[2^62, 2^63)` range,
/// guaranteeing — STRUCTURALLY, not statistically — that an index id can never
/// collide with a numeric stock `exchange_token` (which are small, far below
/// `2^32`). Bit 63 stays clear so the value is always a positive `i64`.
const INDEX_SECURITY_ID_BIT: i64 = 1 << 62;

/// Derives a deterministic, stable, positive `security_id` for an index from its
/// globally-unique `groww_symbol` (e.g. `NSE-NIFTY`, `BSE-SENSEX`) via FNV-1a
/// (64-bit), then forces it into the `[2^62, 2^63)` band via
/// `INDEX_SECURITY_ID_BIT`. Operator decision 2026-06-21 ("Groww-native stable
/// IDs"): Rust is the single source — the same symbol always yields the same id
/// across boots, and the sidecar never re-derives it. Because every index id has
/// bit 62 set and stock ids (numeric tokens) are far below `2^32`, the two ranges
/// are DISJOINT by construction — collision is impossible, not merely unlikely.
#[must_use]
fn stable_index_security_id(groww_symbol: &str) -> i64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in groww_symbol.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // Keep the low 62 bits of the hash (distinct symbols → distinct ids) and set
    // bit 62 → range [2^62, 2^63): positive, and disjoint from stock tokens.
    ((hash & 0x3fff_ffff_ffff_ffff) as i64) | INDEX_SECURITY_ID_BIT
}

/// Extracts index watch entries from the master: every `exchange=NSE &
/// instrument_type=IDX` row PLUS the single `BSE-SENSEX` index (operator §31).
/// Indices subscribe via index_value with `segment=CASH`; the subscribe token is
/// the Groww `exchange_token` (a NAME for NSE, the numeric `"1"` for BSE SENSEX),
/// while the stored `security_id` is the Groww-native stable id derived from the
/// `groww_symbol`. Deterministic order (token asc), deduped by token.
#[must_use]
fn extract_index_entries(rows: &[GrowwInstrumentRow]) -> Vec<WatchEntry> {
    let mut entries: Vec<WatchEntry> = rows
        .iter()
        .filter(|r| {
            r.instrument_type == "IDX"
                && !r.exchange_token.is_empty()
                && (r.exchange == "NSE"
                    || (r.exchange == "BSE" && r.groww_symbol == BSE_SENSEX_GROWW_SYMBOL))
        })
        .map(|r| WatchEntry {
            exchange: r.exchange.clone(),
            segment: "CASH".to_string(),
            exchange_token: r.exchange_token.clone(),
            kind: WatchKind::IndexValue,
            security_id: stable_index_security_id(&r.groww_symbol),
            // PR-A cold-path provenance: indices have no ISIN; carry the stable
            // `groww_symbol` as the master-row `index_name`/`symbol_name`.
            isin: None,
            symbol_name: None,
            index_name: Some(r.groww_symbol.clone()),
        })
        .collect();
    entries.sort_by(|a, b| a.exchange_token.cmp(&b.exchange_token));
    entries.dedup_by(|a, b| a.exchange_token == b.exchange_token);
    entries
}

/// Cross-checks the resolved Groww NSE index set against Dhan's
/// [`NSE_INDEX_ALLOWLIST`] and returns the canonical names of the
/// Dhan-tracked indices that have NO matching Groww IDX row, in allowlist
/// order. This makes the genuine Groww-master limitation (10 sectoral /
/// broad indices Groww does not publish as an `IDX` row, 2026-06-28)
/// VISIBLE rather than silently dropped — mirroring the Dhan-side
/// `allowlist_misses` audit. Reuses `index_extractor`'s own
/// `canonicalize_index_symbol` so Dhan renames/aliases resolve identically
/// on both feeds; no parallel matcher is introduced.
///
/// O(1) EXEMPT: cold-path daily build only (once per Groww master load),
/// not the per-tick path. Bounded by the 32-entry allowlist × resolved
/// NSE index count.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn groww_indices_absent_vs_dhan(index_entries: &[WatchEntry]) -> Vec<&'static str> {
    use crate::instrument::index_extractor::{NSE_INDEX_ALLOWLIST, canonicalize_index_symbol};

    // Canonicalize every resolved Groww NSE index name (the bare NSE name is the
    // `exchange_token` for NSE indices). BSE SENSEX (`exchange == "BSE"`) is
    // excluded — the allowlist is NSE-only, SENSEX is tracked separately.
    let resolved_canonical: std::collections::HashSet<String> = index_entries
        .iter()
        .filter(|e| e.exchange == "NSE")
        .map(|e| canonicalize_index_symbol(&e.exchange_token))
        .collect();

    NSE_INDEX_ALLOWLIST
        .iter()
        .filter(|allowed| !resolved_canonical.contains(&canonicalize_index_symbol(allowed)))
        .copied()
        .collect()
}

/// Resolves NTM constituents (symbol, ISIN) to Groww stock watch entries via the
/// ISIN map. Returns the resolved entries (sorted by token for determinism) plus
/// the symbols that did not resolve (logged by name). O(1) per constituent.
#[must_use]
fn resolve_stock_entries(
    constituents: &[(String, String)],
    isin_map: &HashMap<String, String>,
) -> (Vec<WatchEntry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut unresolved = Vec::new();
    for (symbol, isin) in constituents {
        match isin_map.get(isin) {
            // `token` is guaranteed numeric by `build_isin_token_map`'s filter, so
            // it is the stock's stored `security_id` directly (parse never 0 here).
            Some(token) => entries.push(WatchEntry {
                exchange: "NSE".to_string(),
                segment: "CASH".to_string(),
                exchange_token: token.clone(),
                kind: WatchKind::Ltp,
                security_id: token.parse::<i64>().unwrap_or(0),
                // PR-A cold-path provenance: retain the constituent ISIN + ticker
                // for the daily master-row build (was discarded at this boundary).
                isin: Some(isin.clone()),
                symbol_name: Some(symbol.clone()),
                index_name: None,
            }),
            None => unresolved.push(symbol.clone()),
        }
    }
    entries.sort_by(|a, b| a.exchange_token.cmp(&b.exchange_token));
    unresolved.sort();
    (entries, unresolved)
}

/// Assembles the final watch set: enforces the 2% NTM tolerance, dedups by
/// `(exchange_token, segment)` (I-P1-11 analogue), checks the `[100,1200]`
/// envelope on the TRUE resolved size, then applies the optional deterministic
/// `max_subscribe` first-run cap (indices first — small + high value — then
/// stocks by token ascending). `max_subscribe` is also clamped to the Groww
/// 1000-subscription hard cap.
fn assemble_watch_set(
    index_entries: Vec<WatchEntry>,
    stock_entries: Vec<WatchEntry>,
    unresolved_stocks: Vec<String>,
    ntm_total: usize,
    max_subscribe: Option<usize>,
) -> Result<GrowwWatchSet, WatchBuildError> {
    // 2% NTM tolerance (fail-closed here; the caller decides degrade vs halt).
    if ntm_total > 0 {
        let frac = unresolved_stocks.len() as f64 / ntm_total as f64;
        if frac > GROWW_NTM_DANGLING_TOLERANCE {
            return Err(WatchBuildError::NtmDanglingExceeded {
                unresolved: unresolved_stocks.len(),
                total: ntm_total,
            });
        }
    }

    let resolved_stocks = stock_entries.len();
    let indices = index_entries.len();

    // Dedup by (exchange, exchange_token, segment): a stock can appear in both the
    // index and stock lists only by error, and an F&O-underlying may duplicate a
    // constituent. `exchange` is in the key so the BSE SENSEX token `"1"` can never
    // collide with an NSE stock whose numeric token is also `"1"`. Indices first so
    // an index token wins its slot deterministically.
    let mut seen: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    let mut deduped: Vec<WatchEntry> = Vec::new();
    for entry in index_entries.into_iter().chain(stock_entries) {
        let key = (
            entry.exchange.clone(),
            entry.exchange_token.clone(),
            entry.segment.clone(),
        );
        if seen.insert(key) {
            deduped.push(entry);
        }
    }

    // Envelope on the TRUE resolved size (before the first-run cap).
    let actual = deduped.len();
    if !(GROWW_MIN_UNIVERSE..=GROWW_MAX_UNIVERSE).contains(&actual) {
        return Err(WatchBuildError::UniverseSizeOutOfBounds {
            actual,
            min: GROWW_MIN_UNIVERSE,
            max: GROWW_MAX_UNIVERSE,
        });
    }

    // Capture the FULL pre-cap universe for the master tables BEFORE truncating the
    // live-subscribe set. The master (`instrument_lifecycle` / `index_constituency`)
    // must record the entire ~767-instrument universe regardless of the live-feed
    // cap, exactly as Dhan persists the full `DailyUniverse` independent of its
    // subscription. Cold-path daily build, so this one owned clone is free.
    let master_entries = deduped.clone();

    // Deterministic cap: clamp to min(max_subscribe, 1000). `deduped` is already
    // indices-first then token-asc, so a prefix take is deterministic.
    let cap = max_subscribe
        .unwrap_or(GROWW_MAX_SUBSCRIPTIONS)
        .min(GROWW_MAX_SUBSCRIPTIONS);
    if deduped.len() > cap {
        deduped.truncate(cap);
    }

    Ok(GrowwWatchSet {
        entries: deduped,
        master_entries,
        resolved_stocks,
        unresolved_stocks,
        indices,
    })
}

/// niftyindices slug for the NIFTY Total Market constituent list (§31.1).
const NTM_SLUG: &str = "ind_niftytotalmarket_list";

/// Default live-subscription cap. Set to the Groww hard cap
/// ([`GROWW_MAX_SUBSCRIPTIONS`] = 1000) so the FULL resolved universe (~767, which
/// fits under 1000) streams live by default — matching the Dhan side, which
/// subscribes its full universe. There is therefore NO artificial sub-cap below
/// the Groww hard cap; `assemble_watch_set` still clamps to that 1000 hard cap
/// (`cap.min(GROWW_MAX_SUBSCRIPTIONS)`), so this can never exceed Groww's limit.
/// Operator can still cap the live set at boot via the `GROWW_MAX_SUBSCRIBE` env
/// override (e.g. `60`) — the master tables stay full-universe regardless, because
/// they iterate `master_entries` (the pre-cap set), not `entries`.
///
/// HONEST envelope: ~767 LIVE subscriptions is the INTENDED config; it is
/// unverified at full scale until the next market open. It is config, not a
/// proven-at-scale claim.
pub const GROWW_DEFAULT_MAX_SUBSCRIBE: usize = GROWW_MAX_SUBSCRIPTIONS;

/// Parses the niftyindices NIFTY-Total-Market constituent CSV into `(symbol,
/// isin)` pairs (uppercased/trimmed). Columns resolved BY NAME (`Symbol`,
/// `ISIN Code`). Rows missing either are skipped. Pure + testable.
fn parse_ntm_constituents(csv: &str) -> Result<Vec<(String, String)>, WatchBuildError> {
    let csv = csv.strip_prefix('\u{feff}').unwrap_or(csv);
    let mut lines = csv.lines();
    let header = lines.next().ok_or(WatchBuildError::GrowwMasterEmpty)?;
    let idx: HashMap<String, usize> = split_csv_line(header)
        .iter()
        .enumerate()
        .map(|(i, c)| (c.trim().to_string(), i))
        .collect();
    let sym_idx = *idx
        .get("Symbol")
        .ok_or(WatchBuildError::MissingColumn("Symbol"))?;
    let isin_idx = *idx
        .get("ISIN Code")
        .ok_or(WatchBuildError::MissingColumn("ISIN Code"))?;
    let mut out = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields = split_csv_line(line);
        let symbol = fields
            .get(sym_idx)
            .map(|s| s.trim().to_uppercase())
            .unwrap_or_default();
        let isin = fields
            .get(isin_idx)
            .map(|s| s.trim().to_uppercase())
            .unwrap_or_default();
        if !symbol.is_empty() && !isin.is_empty() {
            out.push((symbol, isin));
        }
    }
    Ok(out)
}

/// Composes the full watch set from the two raw CSV texts. Pure + fully testable
/// (the network is the only non-deterministic part, kept in the orchestrator).
fn build_groww_watch_from_csvs(
    groww_csv: &str,
    ntm_csv: &str,
    max_subscribe: Option<usize>,
) -> Result<GrowwWatchSet, WatchBuildError> {
    let rows = parse_groww_master(groww_csv)?;
    let (isin_map, ambiguous) = build_isin_token_map(&rows);
    if !ambiguous.is_empty() {
        warn!(
            count = ambiguous.len(),
            "groww watch: excluded ambiguous ISINs (>1 token) — not guessed"
        );
    }
    let index_entries = extract_index_entries(&rows);
    // FIX C (2026-06-28): audit Groww vs Dhan index coverage. Emit ONE boot
    // line naming the Dhan-tracked indices that Groww's master does not carry
    // as an IDX row, so the genuine Groww limitation is VISIBLE, never silently
    // dropped. Cold-path, once per master load; feature-gated because the Dhan
    // `index_extractor` (the allowlist + canonicalizer) lives behind it.
    #[cfg(feature = "daily_universe_fetcher")]
    {
        use crate::instrument::index_extractor::NSE_INDEX_ALLOWLIST;
        let absent = groww_indices_absent_vs_dhan(&index_entries);
        let groww_resolved = index_entries.iter().filter(|e| e.exchange == "NSE").count();
        if absent.is_empty() {
            info!(
                groww_resolved,
                dhan_tracked = NSE_INDEX_ALLOWLIST.len(),
                absent_on_groww = 0,
                "Groww index coverage vs Dhan allowlist — full coverage"
            );
        } else {
            metrics::counter!("tv_groww_index_absent_total").increment(absent.len() as u64);
            warn!(
                groww_resolved,
                dhan_tracked = NSE_INDEX_ALLOWLIST.len(),
                absent_on_groww = absent.len(),
                absent_names = %absent.join(","),
                "Groww index coverage vs Dhan allowlist — indices absent on Groww (genuine Groww-master limitation)"
            );
        }
    }
    let ntm = parse_ntm_constituents(ntm_csv)?;
    let ntm_total = ntm.len();
    let (stock_entries, unresolved) = resolve_stock_entries(&ntm, &isin_map);
    assemble_watch_set(
        index_entries,
        stock_entries,
        unresolved,
        ntm_total,
        max_subscribe,
    )
}

/// On-disk watch-file shape the Python sidecar reads.
#[derive(Serialize)]
struct WatchFile<'a> {
    /// IST trading date this watch set was built for (staleness guard).
    trading_date_ist: &'a str,
    /// Provenance label.
    feed: &'a str,
    /// Total entries to subscribe.
    count: usize,
    /// Resolved NTM stock count (pre-cap).
    resolved_stocks: usize,
    /// Index count (pre-cap).
    indices: usize,
    /// The instruments to subscribe.
    entries: &'a [WatchEntry],
}

/// Serializes the watch set to the sidecar watch-file JSON. Pure + testable.
fn serialize_watch_file(
    set: &GrowwWatchSet,
    trading_date_ist: &str,
) -> Result<String, WatchBuildError> {
    let file = WatchFile {
        trading_date_ist,
        feed: "groww",
        count: set.entries.len(),
        resolved_stocks: set.resolved_stocks,
        indices: set.indices,
        entries: &set.entries,
    };
    serde_json::to_string_pretty(&file).map_err(|e| WatchBuildError::WriteFailed(e.to_string()))
}

/// Atomically writes `content` to `path` (write `.tmp` → rename). Creates parent
/// dirs. Testable via tempdir.
fn write_watch_file_atomic(path: &Path, content: &str) -> Result<(), WatchBuildError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| WatchBuildError::WriteFailed(e.to_string()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, content).map_err(|e| WatchBuildError::WriteFailed(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| WatchBuildError::WriteFailed(e.to_string()))?;
    Ok(())
}

/// Builds the §18-hardened HTTP client (no redirects, bounded timeouts, HTTPS).
// TEST-EXEMPT: constructs a reqwest client (TLS root load); mirrors the audited csv_downloader hardening.
fn hardened_client() -> Result<reqwest::Client, WatchBuildError> {
    reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(
            tickvault_common::constants::CSV_CONNECT_TIMEOUT_SECS,
        ))
        .timeout(Duration::from_secs(
            tickvault_common::constants::INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS,
        ))
        .https_only(true)
        .build()
        .map_err(|e| WatchBuildError::FetchFailed(e.to_string()))
}

/// One bounded GET → UTF-8 text, body-size-capped at `MAX_CSV_BODY_BYTES`.
// TEST-EXEMPT: live network GET; the body cap + status check are the hardening, the parse/resolve primitives are unit-tested.
async fn fetch_text_hardened(
    client: &reqwest::Client,
    url: &str,
) -> Result<String, WatchBuildError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| WatchBuildError::FetchFailed(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(WatchBuildError::FetchFailed(format!(
            "http {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| WatchBuildError::FetchFailed(e.to_string()))?;
    if bytes.len() > tickvault_common::constants::MAX_CSV_BODY_BYTES {
        return Err(WatchBuildError::FetchFailed(format!(
            "body {} bytes exceeds cap",
            bytes.len()
        )));
    }
    String::from_utf8(bytes.to_vec()).map_err(|e| WatchBuildError::FetchFailed(e.to_string()))
}

/// Strict `YYYY-MM-DD` check (digits + dashes only, fixed positions). Used to
/// guard the watch-file path against traversal: `trading_date_ist` is
/// concatenated into a filename, so a value like `../../etc/x` MUST be rejected
/// (defense-in-depth, mirrors `instrument_snapshot::is_valid_trading_date`).
#[must_use]
fn is_valid_trading_date(date: &str) -> bool {
    let b = date.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, &c)| {
            if i == 4 || i == 7 {
                c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

/// PR-B1 production entry (boot call site): download the Groww master CSV + the
/// NTM constituent list, resolve the watch set by ISIN, and atomically write the
/// watch file at `cache_dir/groww-watch-<trading_date>.json`. One attempt; the
/// boot wiring wraps this in the pull-until-success retry loop. Returns the
/// built set for logging. The Groww master + NTM are BOTH required (fail-closed
/// on either; the caller decides degrade-vs-retry).
// TEST-EXEMPT: network download + filesystem write orchestration; every pure primitive it composes (parse_groww_master / build_isin_token_map / extract_index_entries / parse_ntm_constituents / resolve_stock_entries / assemble_watch_set / serialize_watch_file / write_watch_file_atomic) is unit-tested below.
pub async fn build_and_write_groww_watch(
    cache_dir: &Path,
    trading_date_ist: &str,
    max_subscribe: Option<usize>,
) -> Result<GrowwWatchSet, WatchBuildError> {
    // Path-traversal guard: trading_date_ist is concatenated into the filename.
    if !is_valid_trading_date(trading_date_ist) {
        return Err(WatchBuildError::WriteFailed(format!(
            "invalid trading_date_ist (expected YYYY-MM-DD): {trading_date_ist}"
        )));
    }
    let client = hardened_client()?;
    info!("groww watch: downloading Groww master instrument.csv");
    let groww_csv = fetch_text_hardened(&client, GROWW_INSTRUMENT_CSV_URL).await?;
    let ntm_url = format!(
        "{}{}.csv",
        tickvault_common::constants::INDEX_CONSTITUENCY_BASE_URL,
        NTM_SLUG
    );
    info!("groww watch: downloading NIFTY-Total-Market constituent list");
    let ntm_csv = fetch_text_hardened(&client, &ntm_url).await?;
    let set = build_groww_watch_from_csvs(&groww_csv, &ntm_csv, max_subscribe)?;
    let path = cache_dir.join(format!("groww-watch-{trading_date_ist}.json"));
    let content = serialize_watch_file(&set, trading_date_ist)?;
    write_watch_file_atomic(&path, &content)?;
    info!(
        entries = set.entries.len(),
        master_entries = set.master_entries.len(),
        resolved_stocks = set.resolved_stocks,
        indices = set.indices,
        unresolved = set.unresolved_stocks.len(),
        path = %path.display(),
        "groww watch: watch file written"
    );
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "exchange,exchange_token,trading_symbol,groww_symbol,name,instrument_type,segment,series,isin,underlying_symbol,underlying_exchange_token,expiry_date,strike_price,lot_size,tick_size,freeze_quantity,is_reserved,buy_allowed,sell_allowed,internal_trading_symbol,is_intraday";

    fn eq_row(token: &str, isin: &str) -> String {
        format!(
            "NSE,{token},RELIANCE,NSE-RELIANCE,Reliance,EQ,CASH,EQ,{isin},,,,,1,0.05,0,0,1,1,RELIANCE,0"
        )
    }
    fn idx_row(name_token: &str) -> String {
        format!(
            "NSE,{name_token},{name_token},NSE-{name_token},,IDX,CASH,,,,,,,,,,0,0,0,{name_token},0"
        )
    }
    /// BSE SENSEX master row: numeric token `1`, groww_symbol `BSE-SENSEX`.
    fn bse_sensex_row() -> String {
        "BSE,1,SENSEX,BSE-SENSEX,,IDX,CASH,,,,,,,,,,0,0,0,SENSEX,0".to_string()
    }

    #[test]
    fn test_parse_groww_master_header_by_name_and_bom() {
        let csv = format!("\u{feff}{HEADER}\n{}\n", eq_row("2885", "INE002A01018"));
        let rows = parse_groww_master(&csv).expect("parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].exchange_token, "2885");
        assert_eq!(rows[0].isin, "INE002A01018");
        assert_eq!(rows[0].instrument_type, "EQ");
    }

    #[test]
    fn test_parse_groww_master_missing_column_fails_closed() {
        let bad = "exchange,exchange_token,segment\nNSE,1,CASH";
        assert_eq!(
            parse_groww_master(bad),
            Err(WatchBuildError::MissingColumn("instrument_type"))
        );
    }

    #[test]
    fn test_parse_groww_master_empty_fails() {
        assert_eq!(
            parse_groww_master(""),
            Err(WatchBuildError::GrowwMasterEmpty)
        );
        let header_only = format!("{HEADER}\n");
        assert_eq!(
            parse_groww_master(&header_only),
            Err(WatchBuildError::GrowwMasterEmpty)
        );
    }

    #[test]
    fn test_build_isin_token_map_filters_to_nse_cash_eq() {
        let csv = format!(
            "{HEADER}\n{}\n{}\n",
            eq_row("2885", "INE002A01018"),
            // an FNO row with same-looking isin must be ignored
            "NSE,99,X,Y,,FUT,FNO,,INE002A01018,X,1,,,1,0.05,0,0,1,1,X,0"
        );
        let rows = parse_groww_master(&csv).unwrap();
        let (map, ambiguous) = build_isin_token_map(&rows);
        assert_eq!(map.get("INE002A01018"), Some(&"2885".to_string()));
        assert!(ambiguous.is_empty());
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn test_build_isin_token_map_excludes_ambiguous_isin() {
        let csv = format!(
            "{HEADER}\n{}\n{}\n",
            eq_row("2885", "INE002A01018"),
            eq_row("2886", "INE002A01018") // same ISIN, different token
        );
        let rows = parse_groww_master(&csv).unwrap();
        let (map, ambiguous) = build_isin_token_map(&rows);
        assert!(map.get("INE002A01018").is_none(), "ambiguous ISIN excluded");
        assert_eq!(ambiguous, vec!["INE002A01018".to_string()]);
    }

    #[test]
    fn test_extract_index_entries_picks_idx_rows() {
        let csv = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            idx_row("NIFTY"),
            idx_row("BANKNIFTY"),
            eq_row("2885", "INE002A01018")
        );
        let rows = parse_groww_master(&csv).unwrap();
        let idx = extract_index_entries(&rows);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[0].exchange_token, "BANKNIFTY"); // sorted asc
        assert_eq!(idx[0].kind, WatchKind::IndexValue);
        assert_eq!(idx[0].segment, "CASH");
    }

    #[test]
    fn test_extract_index_entries_includes_bse_sensex() {
        let csv = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            idx_row("NIFTY"),
            bse_sensex_row(),
            // a BSE index that is NOT sensex must be excluded.
            "BSE,99,BANKEX,BSE-BANKEX,,IDX,CASH,,,,,,,,,,0,0,0,BANKEX,0"
        );
        let rows = parse_groww_master(&csv).unwrap();
        let idx = extract_index_entries(&rows);
        assert_eq!(
            idx.len(),
            2,
            "NSE NIFTY + BSE SENSEX only (BANKEX excluded)"
        );
        let sensex = idx
            .iter()
            .find(|e| e.exchange == "BSE")
            .expect("BSE SENSEX present");
        assert_eq!(sensex.exchange_token, "1");
        assert_eq!(sensex.kind, WatchKind::IndexValue);
        assert_eq!(
            sensex.security_id,
            stable_index_security_id("BSE-SENSEX"),
            "BSE SENSEX uses the Groww-native stable id, not its token `1`"
        );
        let nifty = idx
            .iter()
            .find(|e| e.exchange == "NSE")
            .expect("NSE NIFTY present");
        assert_eq!(nifty.exchange_token, "NIFTY");
        assert_eq!(nifty.security_id, stable_index_security_id("NSE-NIFTY"));
    }

    #[test]
    fn test_stable_index_security_id_is_deterministic_positive_and_distinct() {
        let nifty = stable_index_security_id("NSE-NIFTY");
        let sensex = stable_index_security_id("BSE-SENSEX");
        assert!(nifty > 0 && sensex > 0, "always positive");
        assert_eq!(
            nifty,
            stable_index_security_id("NSE-NIFTY"),
            "deterministic"
        );
        assert_ne!(nifty, sensex, "distinct symbols → distinct ids");
        // STRUCTURAL disjointness: every index id is in [2^62, 2^63), so it can
        // never collide with a numeric stock token (which is far below 2^32).
        assert!(nifty >= INDEX_SECURITY_ID_BIT && sensex >= INDEX_SECURITY_ID_BIT);
        assert!(nifty > i64::from(u32::MAX) && sensex > i64::from(u32::MAX));
    }

    #[test]
    fn test_resolve_stock_entries_join_and_unresolved() {
        let mut map = HashMap::new();
        map.insert("INE002A01018".to_string(), "2885".to_string());
        let constituents = vec![
            ("RELIANCE".to_string(), "INE002A01018".to_string()),
            ("GHOST".to_string(), "INE000000000".to_string()),
        ];
        let (entries, unresolved) = resolve_stock_entries(&constituents, &map);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].exchange_token, "2885");
        assert_eq!(entries[0].kind, WatchKind::Ltp);
        // Stock security_id is the numeric token itself.
        assert_eq!(entries[0].security_id, 2885);
        assert_eq!(unresolved, vec!["GHOST".to_string()]);
    }

    #[test]
    fn test_resolve_stock_entries_retains_isin_and_symbol() {
        // PR-A: the resolver MUST retain the constituent ISIN + ticker on the
        // (cold-path) WatchEntry so the daily master-row build can populate
        // `index_constituency` / `instrument_lifecycle`.
        let mut map = HashMap::new();
        map.insert("INE002A01018".to_string(), "2885".to_string());
        let constituents = vec![("RELIANCE".to_string(), "INE002A01018".to_string())];
        let (entries, _unresolved) = resolve_stock_entries(&constituents, &map);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].isin.as_deref(), Some("INE002A01018"));
        assert_eq!(entries[0].symbol_name.as_deref(), Some("RELIANCE"));
        // Stocks carry no index name.
        assert_eq!(entries[0].index_name, None);
    }

    #[test]
    fn test_extract_index_entries_retains_index_name() {
        // PR-A: index entries MUST retain the stable `groww_symbol` as the
        // master-row `index_name`, and carry no ISIN/symbol_name.
        let csv = format!("{HEADER}\n{}", idx_row("NIFTY"));
        let rows = parse_groww_master(&csv).unwrap();
        let entries = extract_index_entries(&rows);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].index_name.as_deref(), Some("NSE-NIFTY"));
        assert_eq!(entries[0].isin, None);
        assert_eq!(entries[0].symbol_name, None);
    }

    fn stock_entry(token: &str) -> WatchEntry {
        WatchEntry {
            exchange: "NSE".to_string(),
            segment: "CASH".to_string(),
            exchange_token: token.to_string(),
            kind: WatchKind::Ltp,
            security_id: token.parse::<i64>().unwrap_or(0),
            isin: Some(format!("INE{token:0>9}")),
            symbol_name: Some(format!("SYM{token}")),
            index_name: None,
        }
    }
    fn index_entry(name: &str) -> WatchEntry {
        WatchEntry {
            exchange: "NSE".to_string(),
            segment: "CASH".to_string(),
            exchange_token: name.to_string(),
            kind: WatchKind::IndexValue,
            security_id: stable_index_security_id(&format!("NSE-{name}")),
            isin: None,
            symbol_name: None,
            index_name: Some(format!("NSE-{name}")),
        }
    }

    #[test]
    fn test_assemble_rejects_when_ntm_tolerance_exceeded() {
        // 5 unresolved of 100 = 5% > 2% → reject.
        let stocks: Vec<WatchEntry> = (0..95).map(|i| stock_entry(&i.to_string())).collect();
        let unresolved: Vec<String> = (0..5).map(|i| format!("U{i}")).collect();
        let err = assemble_watch_set(vec![], stocks, unresolved, 100, None).unwrap_err();
        assert!(matches!(
            err,
            WatchBuildError::NtmDanglingExceeded {
                unresolved: 5,
                total: 100
            }
        ));
    }

    #[test]
    fn test_assemble_envelope_too_small_fails_closed() {
        let stocks: Vec<WatchEntry> = (0..50).map(|i| stock_entry(&i.to_string())).collect();
        let err = assemble_watch_set(vec![], stocks, vec![], 50, None).unwrap_err();
        assert!(matches!(
            err,
            WatchBuildError::UniverseSizeOutOfBounds { actual: 50, .. }
        ));
    }

    #[test]
    fn test_assemble_dedups_by_token_segment() {
        // 100 unique stocks + 1 duplicate token → deduped to 100.
        let mut stocks: Vec<WatchEntry> = (0..100).map(|i| stock_entry(&i.to_string())).collect();
        stocks.push(stock_entry("0")); // dup
        let set = assemble_watch_set(vec![], stocks, vec![], 100, None).unwrap();
        assert_eq!(set.entries.len(), 100);
    }

    #[test]
    fn test_assemble_deterministic_cap_indices_first() {
        let indices = vec![index_entry("AAA"), index_entry("BBB")];
        let stocks: Vec<WatchEntry> = (0..200).map(|i| stock_entry(&format!("{i:04}"))).collect();
        // cap to 5 → 2 indices first, then 3 stocks (token asc: 0000,0001,0002).
        let set = assemble_watch_set(indices, stocks, vec![], 200, Some(5)).unwrap();
        assert_eq!(set.entries.len(), 5);
        assert_eq!(set.entries[0].kind, WatchKind::IndexValue);
        assert_eq!(set.entries[1].kind, WatchKind::IndexValue);
        assert_eq!(set.entries[2].exchange_token, "0000");
        assert_eq!(set.entries[4].exchange_token, "0002");
        // resolved_stocks/indices provenance preserved (pre-cap counts).
        assert_eq!(set.indices, 2);
        assert_eq!(set.resolved_stocks, 200);
    }

    #[test]
    fn test_assemble_cap_clamped_to_groww_1000_hard_cap() {
        let stocks: Vec<WatchEntry> = (0..1100).map(|i| stock_entry(&format!("{i:05}"))).collect();
        // 1100 is within [100,1200] envelope; ask for max 5000 → clamps to 1000.
        let set = assemble_watch_set(vec![], stocks, vec![], 1100, Some(5000)).unwrap();
        assert_eq!(set.entries.len(), GROWW_MAX_SUBSCRIPTIONS);
        // The master records the FULL pre-cap universe (1100), even though the live
        // subscribe set is clamped to the 1000 hard cap.
        assert_eq!(
            set.master_entries.len(),
            1100,
            "master_entries holds the full pre-cap universe, not the 1000-clamped live set"
        );
    }

    #[test]
    fn test_assemble_master_entries_full_when_capped() {
        // Decoupling proof: a small explicit cap shrinks the live `entries` but
        // NEVER the master. 200 stocks, cap=10 → entries=10, master_entries=200.
        let stocks: Vec<WatchEntry> = (0..200).map(|i| stock_entry(&format!("{i:04}"))).collect();
        let set = assemble_watch_set(vec![], stocks, vec![], 200, Some(10)).unwrap();
        assert_eq!(set.entries.len(), 10, "live subscribe set is capped to 10");
        assert_eq!(
            set.master_entries.len(),
            200,
            "master_entries is the FULL pre-cap universe regardless of the live cap"
        );
        // The capped `entries` is a prefix of the (indices-first, token-asc) master.
        assert_eq!(set.entries, set.master_entries[..10].to_vec());
    }

    #[test]
    fn test_assemble_master_equals_entries_when_uncapped() {
        // With no sub-cap below the universe size, the live set and master set are
        // identical — the production default (no GROWW_MAX_SUBSCRIBE override).
        let stocks: Vec<WatchEntry> = (0..150).map(|i| stock_entry(&format!("{i:04}"))).collect();
        let set = assemble_watch_set(vec![], stocks, vec![], 150, None).unwrap();
        assert_eq!(set.entries, set.master_entries);
        assert_eq!(set.entries.len(), 150);
    }

    #[test]
    fn test_default_max_subscribe_is_groww_hard_cap() {
        // FIX B: the boot default is now the Groww 1000 hard cap (no artificial
        // sub-cap below it), so a ~767-style universe streams live in full — it is
        // NOT truncated, and entries == master_entries.
        assert_eq!(GROWW_DEFAULT_MAX_SUBSCRIBE, GROWW_MAX_SUBSCRIPTIONS);
        // 767 stocks + the default cap (1000) → no truncation.
        let stocks: Vec<WatchEntry> = (0..767).map(|i| stock_entry(&format!("{i:04}"))).collect();
        let set = assemble_watch_set(
            vec![],
            stocks,
            vec![],
            767,
            Some(GROWW_DEFAULT_MAX_SUBSCRIBE),
        )
        .unwrap();
        assert_eq!(
            set.entries.len(),
            767,
            "full universe streams live (not capped at 60)"
        );
        assert_eq!(set.entries, set.master_entries);
    }

    #[test]
    fn test_split_csv_line_handles_quoted_comma() {
        let f = split_csv_line(r#"a,"b,c",d"#);
        assert_eq!(f, vec!["a".to_string(), "b,c".to_string(), "d".to_string()]);
    }

    #[test]
    fn test_watch_entry_serializes_to_sidecar_contract() {
        let e = stock_entry("2885");
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains("\"exchange_token\":\"2885\""));
        assert!(j.contains("\"kind\":\"ltp\""));
        assert!(j.contains("\"segment\":\"CASH\""));
        assert!(j.contains("\"security_id\":2885"));
    }

    const NTM_CSV: &str = "Company Name,Industry,Symbol,Series,ISIN Code\nReliance Industries,Energy,RELIANCE,EQ,INE002A01018\nInfosys,IT,INFY,EQ,INE009A01021\n";

    #[test]
    fn test_parse_ntm_constituents_by_header_name() {
        let ntm = parse_ntm_constituents(NTM_CSV).expect("parse ntm");
        assert_eq!(ntm.len(), 2);
        assert_eq!(ntm[0], ("RELIANCE".to_string(), "INE002A01018".to_string()));
        assert_eq!(ntm[1], ("INFY".to_string(), "INE009A01021".to_string()));
    }

    #[test]
    fn test_parse_ntm_constituents_missing_isin_column_fails() {
        let bad = "Company Name,Symbol\nReliance,RELIANCE";
        assert_eq!(
            parse_ntm_constituents(bad),
            Err(WatchBuildError::MissingColumn("ISIN Code"))
        );
    }

    #[test]
    fn test_build_groww_watch_from_csvs_end_to_end() {
        // Groww master: 1 index + 120 EQ rows (clears the 100 floor). NTM list
        // references all 120 by ISIN → 1 index + 120 stocks = 121 entries.
        let mut groww = format!("{HEADER}\n{}\n", idx_row("NIFTY"));
        let mut ntm = String::from("Company Name,Industry,Symbol,Series,ISIN Code\n");
        for i in 0..120 {
            let isin = format!("INE{i:09}");
            groww.push_str(&format!("{}\n", eq_row(&format!("{}", 1000 + i), &isin)));
            ntm.push_str(&format!("Co{i},Ind,SYM{i},EQ,{isin}\n"));
        }
        let set = build_groww_watch_from_csvs(&groww, &ntm, None).expect("build ok");
        assert_eq!(set.indices, 1);
        assert_eq!(set.resolved_stocks, 120);
        assert!(set.unresolved_stocks.is_empty());
        assert_eq!(set.entries.len(), 121);
        // Index is first (deterministic order).
        assert_eq!(set.entries[0].kind, WatchKind::IndexValue);
    }

    #[test]
    fn test_build_groww_watch_from_csvs_unresolved_under_tolerance_ok() {
        // 200 NTM stocks, but only 198 exist in Groww master → 2 unresolved =
        // 1% < 2% tolerance → build succeeds, unresolved logged by name.
        let mut groww = format!("{HEADER}\n{}\n", idx_row("NIFTY"));
        let mut ntm = String::from("Company Name,Industry,Symbol,Series,ISIN Code\n");
        for i in 0..200 {
            let isin = format!("INE{i:09}");
            // Only the first 198 go into the Groww master.
            if i < 198 {
                groww.push_str(&format!("{}\n", eq_row(&format!("{}", 1000 + i), &isin)));
            }
            ntm.push_str(&format!("Co{i},Ind,SYM{i},EQ,{isin}\n"));
        }
        let set =
            build_groww_watch_from_csvs(&groww, &ntm, None).expect("build ok under tolerance");
        assert_eq!(set.resolved_stocks, 198);
        assert_eq!(set.unresolved_stocks.len(), 2);
    }

    #[test]
    fn test_build_groww_watch_from_csvs_envelope_floor_rejects_tiny() {
        let groww = format!(
            "{HEADER}\n{}\n{}\n",
            idx_row("NIFTY"),
            eq_row("2885", "INE002A01018")
        );
        // NTM has only RELIANCE → 1 index + 1 stock = 2 < 100 floor → reject.
        let err = build_groww_watch_from_csvs(
            &groww,
            "Company Name,Industry,Symbol,Series,ISIN Code\nR,E,RELIANCE,EQ,INE002A01018\n",
            None,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            WatchBuildError::UniverseSizeOutOfBounds { actual: 2, .. }
        ));
    }

    #[test]
    fn test_serialize_watch_file_has_date_feed_entries() {
        let entries = vec![stock_entry("2885"), index_entry("NIFTY")];
        let set = GrowwWatchSet {
            master_entries: entries.clone(),
            entries,
            resolved_stocks: 1,
            unresolved_stocks: vec![],
            indices: 1,
        };
        let json = serialize_watch_file(&set, "2026-06-21").expect("serialize");
        assert!(json.contains("\"trading_date_ist\": \"2026-06-21\""));
        assert!(json.contains("\"feed\": \"groww\""));
        assert!(json.contains("\"count\": 2"));
        assert!(json.contains("\"exchange_token\": \"2885\""));
        assert!(json.contains("\"kind\": \"index_value\""));
    }

    #[test]
    fn test_is_valid_trading_date_rejects_traversal_and_malformed() {
        assert!(is_valid_trading_date("2026-06-21"));
        assert!(!is_valid_trading_date("../../etc/passwd"));
        assert!(!is_valid_trading_date("2026/06/21"));
        assert!(!is_valid_trading_date("2026-6-21"));
        assert!(!is_valid_trading_date("2026-06-21x"));
        assert!(!is_valid_trading_date(""));
    }

    #[test]
    fn test_write_watch_file_atomic_creates_and_writes() {
        let dir = std::env::temp_dir().join(format!("tv-groww-watch-test-{}", std::process::id()));
        let path = dir.join("groww-watch-2026-06-21.json");
        write_watch_file_atomic(&path, "{\"hello\":1}").expect("write");
        let read = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(read, "{\"hello\":1}");
        // tmp file is gone after the rename.
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    // FIX C (2026-06-28): the 10 Dhan-allowlisted indices Groww's master does
    // NOT publish as an IDX row (genuine Groww limitation). Canonical
    // (allowlist) spelling — the assertion is canonicalized so it is resilient
    // to whitespace/alias differences.
    #[cfg(feature = "daily_universe_fetcher")]
    const KNOWN_ABSENT_ON_GROWW: &[&str] = &[
        "NIFTY 200",
        "GIFTNIFTY",
        "NIFTY ENERGY",
        "NIFTYINFRA",
        "NIFTY MNC",
        "NIFTY CONSUMPTION",
        "NIFTY SERV SECTOR",
        "NIFTY MID100 FREE",
        "NIFTY SMALLCAP 50",
        "NIFTY MICROCAP250",
    ];

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_indices_absent_vs_dhan_is_exactly_the_ten() {
        use crate::instrument::index_extractor::{NSE_INDEX_ALLOWLIST, canonicalize_index_symbol};

        let absent_canon: std::collections::HashSet<String> = KNOWN_ABSENT_ON_GROWW
            .iter()
            .map(|n| canonicalize_index_symbol(n))
            .collect();

        // Build a Groww master that carries EVERY allowlisted NSE index EXCEPT
        // the 10 known-absent ones — exactly the live-master situation.
        let present: Vec<&&str> = NSE_INDEX_ALLOWLIST
            .iter()
            .filter(|name| !absent_canon.contains(&canonicalize_index_symbol(name)))
            .collect();
        let mut csv = String::from(HEADER);
        for name in &present {
            csv.push('\n');
            csv.push_str(&idx_row(name));
        }
        // BSE SENSEX is present on Groww but is NOT an NSE-allowlist member — it
        // must not appear in the absent set either way.
        csv.push('\n');
        csv.push_str(&bse_sensex_row());
        csv.push('\n');

        let rows = parse_groww_master(&csv).unwrap();
        let index_entries = extract_index_entries(&rows);

        // No resolved index was dropped: every NSE name we put in resolves back.
        let resolved_canon: std::collections::HashSet<String> = index_entries
            .iter()
            .filter(|e| e.exchange == "NSE")
            .map(|e| canonicalize_index_symbol(&e.exchange_token))
            .collect();
        assert_eq!(
            resolved_canon.len(),
            present.len(),
            "every present Groww NSE index must remain resolved (none dropped)"
        );

        let absent = groww_indices_absent_vs_dhan(&index_entries);
        let absent_set: std::collections::HashSet<String> = absent
            .iter()
            .map(|n| canonicalize_index_symbol(n))
            .collect();
        assert_eq!(
            absent_set, absent_canon,
            "absent_on_groww must be EXACTLY the 10 known indices (canonicalized)"
        );
        assert_eq!(absent.len(), 10, "exactly 10 indices absent on Groww");
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_indices_absent_vs_dhan_empty_when_full_coverage() {
        use crate::instrument::index_extractor::NSE_INDEX_ALLOWLIST;

        // A (hypothetical) Groww master carrying every allowlisted NSE index →
        // no absences. Proves the audit does not over-report.
        let mut csv = String::from(HEADER);
        for name in NSE_INDEX_ALLOWLIST {
            csv.push('\n');
            csv.push_str(&idx_row(name));
        }
        csv.push('\n');
        let rows = parse_groww_master(&csv).unwrap();
        let index_entries = extract_index_entries(&rows);
        let absent = groww_indices_absent_vs_dhan(&index_entries);
        assert!(
            absent.is_empty(),
            "full coverage must report zero absent, got {absent:?}"
        );
    }
}
