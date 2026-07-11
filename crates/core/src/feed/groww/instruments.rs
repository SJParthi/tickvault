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
///
/// §36 (2026-07-08) / §36.7 (2026-07-10): the watch set additionally carries
/// ALL monthly-expiry index futures of the 4 underlyings
/// (NIFTY/BANKNIFTY/MIDCPNIFTY on NSE, SENSEX on BSE; segment FNO, kind=ltp)
/// — selected via the SAME shared never-roll expiry function
/// the Dhan orchestrator uses (`crate::instrument::index_futures`).
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
    /// COLD-PATH provenance (§36 2026-07-08) — ISO `YYYY-MM-DD` expiry for the
    /// 4 index-future entries; `None` (and skipped on write) for everything
    /// else, so existing entries stay byte-stable. The Python sidecar reads
    /// only `{exchange, segment, exchange_token, kind, security_id}` and the
    /// native watch reader ignores unknown fields — additive JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry_date: Option<String>,
    /// COLD-PATH provenance (§36 hostile-review round 2, 2026-07-08) — the
    /// EXACT-match canonical underlying (`NIFTY`/`BANKNIFTY`/`MIDCPNIFTY`/
    /// `SENSEX`) for the 4 index-future entries, threaded from
    /// `GrowwIndexFuture.canonical` so the `feed='groww'` FUTIDX master rows
    /// carry a queryable `underlying_symbol` (SEBI forensic completeness —
    /// mirror of the Dhan-side lifecycle rows). `None` (and skipped on
    /// write) for everything else; the sidecar/native watch readers ignore
    /// unknown fields — additive JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub underlying_symbol: Option<String>,
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
    /// `name` column — the human DISPLAY name (e.g. "NIFTY 50", "Nifty Next 50",
    /// "NIFTY Auto"). For NSE indices this is the field that canonicalizes to the
    /// descriptive Dhan-allowlist entries (`NIFTY AUTO`, `NIFTY NEXT 50`); the
    /// `exchange_token` (short code `NIFTYJR`/`NIFTYAUTO`) covers the
    /// trading-symbol allowlist entries (`NIFTY`, `BANKNIFTY`). May be empty.
    pub name: String,
    /// `instrument_type` column (`EQ`/`IDX`/`FUT`/`CE`/`PE`).
    pub instrument_type: String,
    /// `segment` column (`CASH`/`FNO`/`COMMODITY`).
    pub segment: String,
    /// `series` column (`EQ`/...), may be empty.
    pub series: String,
    /// `isin` column, may be empty (indices/derivatives have none).
    pub isin: String,
    /// `underlying_symbol` column (§36 2026-07-08) — OPTIONAL header: empty
    /// when the column is absent, so a header regression degrades ONLY the
    /// futures extraction, never the cash/index master parse. The structural
    /// FUT-row match key (never `trading_symbol` regex).
    pub underlying_symbol: String,
    /// `expiry_date` column (§36) — ISO `YYYY-MM-DD` on FNO rows. OPTIONAL
    /// header, same degrade semantics as `underlying_symbol`.
    pub expiry_date: String,
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
            // `name` is OPTIONAL (defaults empty if the column is absent), so it
            // is NOT a mandatory header column — its absence only degrades the
            // index audit to token-only matching, it never fails the build.
            name: get(&fields, "name"),
            instrument_type: get(&fields, "instrument_type"),
            segment: get(&fields, "segment"),
            series: get(&fields, "series"),
            isin: get(&fields, "isin"),
            // §36: OPTIONAL columns (empty when the header lacks them) — a
            // header regression degrades futures extraction only.
            underlying_symbol: get(&fields, "underlying_symbol"),
            expiry_date: get(&fields, "expiry_date"),
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
            // `groww_symbol` as the master-row `index_name`. FIX (2026-06-28):
            // also carry the Groww DISPLAY `name` (e.g. "NIFTY Auto",
            // "Nifty Next 50") as `symbol_name` so `groww_indices_absent_vs_dhan`
            // can canonicalize it against the descriptive Dhan-allowlist entries
            // (the short `exchange_token` only covers the trading-symbol entries).
            // `None` if the master row has no `name` value (token-only fallback).
            isin: None,
            symbol_name: (!r.name.is_empty()).then(|| r.name.clone()),
            index_name: Some(r.groww_symbol.clone()),
            expiry_date: None,
            underlying_symbol: None,
        })
        .collect();
    entries.sort_by(|a, b| a.exchange_token.cmp(&b.exchange_token));
    entries.dedup_by(|a, b| a.exchange_token == b.exchange_token);
    entries
}

/// §36 (2026-07-08) / §36.7 (2026-07-10): extracts ALL monthly-expiry
/// index-future watch entries (one per (underlying, month `>= today`)) from
/// the Groww master. ISIN-less resolution (FUT rows carry an empty `isin` —
/// verified `docs/groww-ref/instrument-sample.csv`):
///
/// 1. Filter `instrument_type == "FUT" && segment == "FNO"`.
/// 2. Match `(row.exchange, canonicalize(row.underlying_symbol))` against
///    [`crate::instrument::index_futures::INDEX_FUTURES_UNDERLYINGS`] —
///    SENSEX only from BSE, the NSE three only from NSE. NEVER match by
///    `trading_symbol` regex (naming-drift hazard) and NEVER by
///    `underlying_exchange_token` (unverified id space).
/// 3. Require a numeric `exchange_token` (non-numeric → skip + count).
/// 4. Per underlying: parse expiry, sort asc, dedup, pick ALL via the SAME
///    shared [`crate::instrument::index_futures::select_index_future_expiries`]
///    — a Dhan/Groww boundary-rule divergence is structurally impossible;
///    > [`crate::instrument::index_futures::MAX_MONTHLY_EXPIRIES_PER_UNDERLYING`]
///    distinct serials → whole-underlying `MonthlySerialFlood` miss.
/// 5. Per (underlying, month): candidate-flood cap → exact-dup token
///    collapse → ≥2 TRULY-DISTINCT tokens → fail-closed miss for THAT month
///    only (`AmbiguousDuplicateExpiry` / `SameExpiryCandidateFlood`).
///
/// Misses are returned; the caller pages FUTIDX-01 (feed=groww) — degrade,
/// the watch build stays valid. Cold path, once per activation.
///
/// Each chosen future is returned as a [`GrowwIndexFuture`] carrying the
/// EXACT-MATCH canonical from step 2 — the parity recorder consumes THAT
/// canonical directly; re-deriving it from `symbol_name` substrings is
/// FORBIDDEN (hostile-review round 1, 2026-07-08: `"BANKNIFTY…".contains
/// ("NIFTY")` mislabeled BANKNIFTY/MIDCPNIFTY as NIFTY → false FUTIDX-02
/// pages every dual-feed boot).
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
pub fn extract_index_future_entries(
    rows: &[GrowwInstrumentRow],
    today_ist: chrono::NaiveDate,
) -> (
    Vec<GrowwIndexFuture>,
    Vec<crate::instrument::index_futures::IndexFutureMiss>,
) {
    use crate::instrument::index_extractor::canonicalize_index_symbol;
    use crate::instrument::index_futures::{
        INDEX_FUTURES_UNDERLYINGS, IndexFutureMiss, IndexFutureMissReason,
        select_index_future_expiries,
    };

    let mut entries: Vec<GrowwIndexFuture> = Vec::new();
    let mut misses: Vec<IndexFutureMiss> = Vec::new();

    for target in &INDEX_FUTURES_UNDERLYINGS {
        let mut candidates: Vec<(chrono::NaiveDate, &GrowwInstrumentRow)> = Vec::new();
        // Distinct flags so the FUTIDX-01 reason names the RIGHT arm
        // (hostile-review round 1: token-invalid rows were misreported as
        // BadExpiryFormat, sending the operator down the wrong triage arm).
        let mut saw_bad_token = false;
        let mut saw_bad_expiry = false;
        for r in rows {
            if r.instrument_type != "FUT" || r.segment != "FNO" || r.exchange != target.exch_id {
                continue;
            }
            if canonicalize_index_symbol(&r.underlying_symbol) != target.canonical {
                continue;
            }
            // Numeric token required (the sidecar's ltp path skips
            // non-numeric tokens; fail here loudly instead of silently there).
            if r.exchange_token.parse::<i64>().is_err() {
                saw_bad_token = true;
                continue;
            }
            match chrono::NaiveDate::parse_from_str(r.expiry_date.trim(), "%Y-%m-%d") {
                Ok(d) => candidates.push((d, r)),
                Err(_) => saw_bad_expiry = true,
            }
        }
        if candidates.is_empty() {
            misses.push(IndexFutureMiss {
                canonical: target.canonical,
                reason: if saw_bad_expiry {
                    IndexFutureMissReason::BadExpiryFormat
                } else if saw_bad_token {
                    IndexFutureMissReason::BadNativeToken
                } else {
                    IndexFutureMissReason::NoFutRows
                },
                expiry: None,
            });
            continue;
        }
        candidates.sort_by_key(|(d, _)| *d);
        // §36.7 (2026-07-10): the SERIAL count is distinct-keyed — duplicate
        // rows at one expiry are handled per-month below.
        let mut dates: Vec<chrono::NaiveDate> = candidates.iter().map(|(d, _)| *d).collect();
        dates.dedup();
        let expiries = select_index_future_expiries(&dates, today_ist);
        if expiries.is_empty() {
            misses.push(IndexFutureMiss {
                canonical: target.canonical,
                reason: IndexFutureMissReason::AllExpiriesPast,
                expiry: None,
            });
            continue;
        }
        // §36.7 serial-flood envelope: more distinct future expiries than a
        // legitimate master can list = corrupt/flooded file → the WHOLE
        // underlying degrades fail-closed (never truncated-and-trusted) —
        // this also bounds the cap-priority prefix so a flooded master can
        // never eat the 1000-cap spot universe.
        if expiries.len() > crate::instrument::index_futures::MAX_MONTHLY_EXPIRIES_PER_UNDERLYING {
            misses.push(IndexFutureMiss {
                canonical: target.canonical,
                reason: IndexFutureMissReason::MonthlySerialFlood,
                expiry: None,
            });
            continue;
        }
        for chosen in expiries {
            let at_chosen: Vec<&GrowwInstrumentRow> = candidates
                .iter()
                .filter(|(d, _)| *d == chosen)
                .map(|(_, r)| *r)
                .collect();
            // Hostile-review round 3 (2026-07-08): envelope cap FIRST — mirror of
            // the Dhan selector; a same-(underlying, expiry) candidate flood
            // beyond the cap is corrupt vendor data and degrades fail-closed
            // (§36.7: per-MONTH — the other months of this underlying still
            // process).
            if at_chosen.len() > crate::instrument::index_futures::FUTIDX_SAME_EXPIRY_CANDIDATE_CAP
            {
                misses.push(IndexFutureMiss {
                    canonical: target.canonical,
                    reason: IndexFutureMissReason::SameExpiryCandidateFlood,
                    expiry: Some(chosen),
                });
                continue;
            }
            // Hostile-review round 2 (2026-07-08): collapse vendor-glitch
            // EXACT-duplicate master lines (SAME `exchange_token` at the chosen
            // expiry) first-row-wins BEFORE the ambiguity count — mirror of the
            // Dhan-side SECURITY_ID dedup in `select_index_future_contracts`.
            // Only TRULY-DISTINCT tokens at the same expiry stay fail-closed
            // (per-MONTH, §36.7). Round 3: HashSet-based O(n) dedup (was an
            // O(n²) scan) — keyed on the bare `exchange_token`,
            // I-P1-11-SAFE HERE because every row in `at_chosen` is
            // FNO-segment FUT for ONE underlying/exchange by construction
            // (single-segment set).
            let mut seen_tokens: std::collections::HashSet<&str> =
                std::collections::HashSet::with_capacity(at_chosen.len());
            let mut distinct: Vec<&GrowwInstrumentRow> = Vec::with_capacity(at_chosen.len());
            for row in &at_chosen {
                if seen_tokens.insert(row.exchange_token.as_str()) {
                    distinct.push(row);
                }
            }
            if distinct.len() > 1 {
                misses.push(IndexFutureMiss {
                    canonical: target.canonical,
                    reason: IndexFutureMissReason::AmbiguousDuplicateExpiry,
                    expiry: Some(chosen),
                });
                continue;
            }
            let r = distinct[0];
            let security_id = r.exchange_token.parse::<i64>().unwrap_or(0);
            entries.push(GrowwIndexFuture {
                entry: WatchEntry {
                    exchange: r.exchange.clone(),
                    segment: "FNO".to_string(),
                    exchange_token: r.exchange_token.clone(),
                    kind: WatchKind::Ltp,
                    security_id,
                    isin: None, // FUT rows are ISIN-less by design
                    symbol_name: Some(r.groww_symbol.clone()),
                    index_name: None,
                    expiry_date: Some(chosen.format("%Y-%m-%d").to_string()),
                    underlying_symbol: Some(target.canonical.to_string()),
                },
                canonical: target.canonical,
                expiry: chosen,
            });
        }
    }
    (entries, misses)
}

/// One chosen Groww index future: the watch entry PLUS the exact-match
/// canonical + parsed expiry it was selected under. The parity recorder
/// consumes `canonical`/`expiry` verbatim — never re-derived from symbol
/// strings (§36; hostile-review round 1, 2026-07-08).
#[cfg(feature = "daily_universe_fetcher")]
#[derive(Debug, Clone)]
pub struct GrowwIndexFuture {
    /// The live-subscription watch entry (segment `FNO`, kind `Ltp`).
    pub entry: WatchEntry,
    /// The [`crate::instrument::index_futures::INDEX_FUTURES_UNDERLYINGS`]
    /// canonical this row EXACT-matched on (`canonicalize_index_symbol`).
    pub canonical: &'static str,
    /// The chosen expiry (parity unit alongside `canonical`).
    pub expiry: chrono::NaiveDate,
}

/// Bounded alias-drift evidence for the Groww FUTIDX-01 payload: distinct
/// `underlying_symbol` literals among `FUT`/`FNO` rows (any exchange),
/// capped at [`crate::instrument::index_futures::MAX_UNDERLYING_SYMBOLS_EVIDENCE`]
/// — the Groww mirror of the Dhan `fut_underlying_symbols_seen` evidence the
/// FUTIDX-01 runbook promises on BOTH feeds (hostile-review round 1,
/// 2026-07-08: the Groww emit carried no evidence exactly where symbol
/// drift is most likely). Cold path, once per degraded activation.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
pub fn collect_fut_underlying_symbols_seen(rows: &[GrowwInstrumentRow]) -> Vec<String> {
    use crate::instrument::index_futures::MAX_UNDERLYING_SYMBOLS_EVIDENCE;
    let mut seen: Vec<String> = Vec::new();
    for r in rows {
        if r.instrument_type != "FUT" || r.segment != "FNO" {
            continue;
        }
        if seen.len() >= MAX_UNDERLYING_SYMBOLS_EVIDENCE {
            break;
        }
        if !seen.iter().any(|s| s == &r.underlying_symbol) {
            seen.push(r.underlying_symbol.clone());
        }
    }
    seen
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
/// FIX (2026-06-28): the Dhan `NSE_INDEX_ALLOWLIST` mixes *trading symbols*
/// (`NIFTY`, `BANKNIFTY`, `MIDCPNIFTY`, `NIFTYMCAP50`, `NIFTYIT`) and
/// *descriptive names* (`NIFTY AUTO`, `NIFTY NEXT 50`). A Groww index row
/// carries BOTH a short `exchange_token` (e.g. `NIFTYJR`, `NIFTYAUTO`) AND a
/// human display `name` (e.g. "Nifty Next 50", "NIFTY Auto"). Neither field
/// ALONE canonicalizes to every allowlist entry — the token matches the
/// trading-symbol entries, the name matches the descriptive entries. So an
/// allowlist entry is PRESENT if EITHER the canonicalized token OR the
/// canonicalized display name resolves to it. (The previous version
/// canonicalized only the short token, falsely flagging ~22 present indices
/// absent — the live `absent_on_groww=28` bug.)
///
/// O(1) EXEMPT: cold-path daily build only (once per Groww master load),
/// not the per-tick path. Bounded by the 32-entry allowlist × resolved
/// NSE index count.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn groww_indices_absent_vs_dhan(index_entries: &[WatchEntry]) -> Vec<&'static str> {
    use crate::instrument::index_extractor::{NSE_INDEX_ALLOWLIST, canonicalize_index_symbol};

    // Canonicalize BOTH the short subscribe token AND the display name of every
    // resolved Groww NSE index into one present-set. BSE SENSEX
    // (`exchange == "BSE"`) is excluded — the allowlist is NSE-only, SENSEX is
    // tracked separately. The 3 spelling bridges that neither field resolves
    // directly (Nifty Midcap Select → MIDCPNIFTY, NIFTY Midcap 50 → NIFTYMCAP50,
    // Nifty Total Market → NIFTY TOTAL MKT) live in the shared
    // `INDEX_SYMBOL_ALIASES`, so `canonicalize_index_symbol` resolves them.
    let resolved_canonical: std::collections::HashSet<String> = index_entries
        .iter()
        .filter(|e| e.exchange == "NSE")
        .flat_map(|e| {
            let mut keys = vec![canonicalize_index_symbol(&e.exchange_token)];
            if let Some(name) = e.symbol_name.as_deref() {
                keys.push(canonicalize_index_symbol(name));
            }
            keys
        })
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
                expiry_date: None,
                underlying_symbol: None,
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
/// `max_subscribe` first-run cap. Cap-priority order: §36/§36.7 futures FIRST
/// (all monthly serials, operator-mandated — must survive the prefix
/// truncate), then indices (small + high value), then stocks by token
/// ascending. `max_subscribe` is also clamped to the Groww 1000-subscription
/// hard cap.
fn assemble_watch_set(
    index_entries: Vec<WatchEntry>,
    stock_entries: Vec<WatchEntry>,
    future_entries: Vec<WatchEntry>,
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
    // collide with an NSE stock whose numeric token is also `"1"`. Indices before
    // stocks so an index token wins its slot deterministically.
    let mut seen: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    let mut deduped: Vec<WatchEntry> = Vec::new();
    // §36/§36.7: the operator-mandated monthly futures (cap-priority, ALL of
    // them — ~12 typical, ≤24 by the serial envelope) are PREPENDED — the
    // live-subscribe cap below is a prefix truncate, so futures must never
    // be the first rows dropped under universe growth (hostile-review
    // round 1, 2026-07-08: appended-last futures were silently truncated
    // first while the gauge still reported 4). The dedup key includes
    // `segment`, so an FNO future can never steal a CASH/index slot.
    for entry in future_entries
        .into_iter()
        .chain(index_entries)
        .chain(stock_entries)
    {
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
    // futures-first, then indices, then stocks token-asc, so a prefix take is
    // deterministic AND cap-prioritizes the §36 futures.
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
    today_ist: chrono::NaiveDate,
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

    // §36/§36.7 (2026-07-10): ALL monthly-expiry index futures of the 4
    // underlyings. DEGRADE ONLY —
    // a miss pages FUTIDX-01 (feed=groww) and the watch build stays valid.
    // Feature-gated because the shared selector lives with the Dhan
    // instrument modules; the feature is default-ON in the app crate.
    //
    // Hostile-review round 4 (2026-07-08): this block EXTRACTS only — every
    // §36 emission (FUTIDX-01 errors + counters, boot-evidence lines, the
    // parity recording, the dedup-collapse error) is DEFERRED until AFTER
    // `assemble_watch_set` succeeds below, mirroring the Dhan Step-3d
    // post-build ordering. Pre-assemble emission meant a day where assemble
    // persistently fails (NtmDanglingExceeded — live precedent 2026-06-08 —
    // or UniverseSizeOutOfBounds) had the ≤300s activation pull-until-success
    // retry re-record the Groww parity entry FOREVER: a genuine cross-feed
    // divergence re-paged FUTIDX-02 every retry (non-edge-triggered, audit
    // Rule 4 violation), and the matching case logged "parity OK" all day
    // for a feed that subscribed NOTHING (false-OK, audit Rule 11).
    #[cfg(feature = "daily_universe_fetcher")]
    let (future_entries, future_misses, groww_selection) = {
        let (futures, misses) = extract_index_future_entries(&rows, today_ist);
        // Parity units: the canonical + expiry come VERBATIM from the
        // exact-match extraction — never re-derived from symbol substrings
        // ("BANKNIFTY".contains("NIFTY") mislabeled 2 of 4 pre-fix).
        let mut groww_selection: Vec<crate::instrument::index_futures::FeedFutureSelection> =
            Vec::with_capacity(futures.len());
        for f in &futures {
            groww_selection.push(crate::instrument::index_futures::FeedFutureSelection {
                canonical: f.canonical,
                expiry: f.expiry,
                native_id: f.entry.exchange_token.clone(),
                segment: f.entry.segment.clone(),
            });
        }
        let entries = futures.into_iter().map(|f| f.entry).collect::<Vec<_>>();
        (entries, misses, groww_selection)
    };
    #[cfg(not(feature = "daily_universe_fetcher"))]
    let future_entries = {
        let _ = today_ist; // futures require the shared selector (feature-gated)
        Vec::<WatchEntry>::new()
    };

    // Hostile-review round 2 (2026-07-08): `expected` is the DISTINCT
    // (exchange, exchange_token, segment) count — the SAME key
    // `assemble_watch_set`'s dedup uses — so an intra-futures dedup collapse
    // (duplicate exchange_token across underlyings = vendor master
    // corruption) is never misattributed to an operator cap override. The
    // collapse itself is loudly reported as its own FUTIDX-01 cause below
    // (post-assemble, round 4).
    let raw_futures = future_entries.len();
    let expected_futures = distinct_future_key_count(&future_entries);
    let set = assemble_watch_set(
        index_entries,
        stock_entries,
        future_entries,
        unresolved,
        ntm_total,
        max_subscribe,
    )?;
    // §36 emissions — POST-assemble-success ONLY (round 4, see the block
    // comment above): a failed build attempt records/emits NOTHING, so the
    // activation retry loop can never spam FUTIDX-02 or log a false
    // "parity OK" for a feed that subscribed nothing.
    #[cfg(feature = "daily_universe_fetcher")]
    {
        use tickvault_common::error_code::ErrorCode;
        if !future_misses.is_empty() {
            // Alias-drift evidence (bounded) — the FUTIDX-01 runbook promises
            // `candidates_seen` on BOTH feeds; collected only on the degrade
            // path (cold, once per degraded activation).
            let candidates_seen = collect_fut_underlying_symbols_seen(&rows);
            for miss in &future_misses {
                tracing::error!(
                    code = ErrorCode::Futidx01SelectionDegraded.code_str(),
                    feed = "groww",
                    underlying = miss.canonical,
                    reason = ?miss.reason,
                    expiry = %miss
                        .expiry
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "ALL".into()),
                    candidates_seen = ?candidates_seen,
                    "index-future selection degraded — the Groww watch set runs WITHOUT that \
                     future (or that month) today; cash/index build unaffected"
                );
                metrics::counter!(
                    "tv_index_futures_selection_missing_total",
                    "feed" => "groww",
                    "underlying" => miss.canonical
                )
                .increment(1);
            }
        }
        // Boot-evidence lines + parity recording (one per SUCCESSFUL build).
        for sel in &groww_selection {
            info!(
                feed = "groww",
                underlying = sel.canonical,
                expiry = %sel.expiry,
                native_id = %sel.native_id,
                segment = %sel.segment,
                "index-futures selection"
            );
        }
        crate::instrument::index_futures::record_index_future_selection(
            "groww",
            today_ist,
            groww_selection,
        );
        if expected_futures < raw_futures {
            tracing::error!(
                code = ErrorCode::Futidx01SelectionDegraded.code_str(),
                feed = "groww",
                raw = raw_futures,
                distinct = expected_futures,
                "duplicate exchange_token among the selected index futures collapsed by the \
                 watch-set dedup — vendor master id-space corruption; the folded contract is \
                 NOT subscribed"
            );
            metrics::counter!("tv_index_futures_dedup_dropped_total", "feed" => "groww")
                .increment((raw_futures - expected_futures) as u64);
        }
        // §36 observability is POST-cap and honest: the gauge reports what is
        // actually in the LIVE subscribe set, and any operator-mandated future
        // dropped by a sub-4 cap (env override) is LOUD, never silent
        // (hostile-review round 1, 2026-07-08: the gauge was set pre-cap and
        // could report 4 while 0 survived the truncate).
        let live_futures = set.entries.iter().filter(|e| e.segment == "FNO").count();
        metrics::gauge!("tv_index_futures_selected", "feed" => "groww").set(live_futures as f64);
        if live_futures < expected_futures {
            tracing::error!(
                code = ErrorCode::Futidx01SelectionDegraded.code_str(),
                feed = "groww",
                expected = expected_futures,
                live = live_futures,
                "index-future(s) dropped from the LIVE subscribe set by the subscription cap — \
                 the §36 futures are cap-priority (prepended), so this fires only when an \
                 operator cap override drops a mandated future; master tables still carry the \
                 full selection"
            );
            metrics::counter!("tv_index_futures_cap_dropped_total", "feed" => "groww")
                .increment((expected_futures - live_futures) as u64);
        }
    }
    #[cfg(not(feature = "daily_universe_fetcher"))]
    let _ = (raw_futures, expected_futures);
    Ok(set)
}

/// Distinct-(exchange, token, segment) count over the selected future
/// entries — the SAME composite key `assemble_watch_set` dedups on, so the
/// cap-drop check's `expected` can never be inflated by an exact-duplicate
/// token (hostile-review round 2, 2026-07-08). O(n²) over ≤24 entries
/// (§36.7 envelope) — trivially fine on the cold path.
fn distinct_future_key_count(entries: &[WatchEntry]) -> usize {
    let mut distinct: Vec<(&str, &str, &str)> = Vec::with_capacity(entries.len());
    for e in entries {
        let key = (
            e.exchange.as_str(),
            e.exchange_token.as_str(),
            e.segment.as_str(),
        );
        if !distinct.contains(&key) {
            distinct.push(key);
        }
    }
    distinct.len()
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

/// §34 auto-scale PR-2 (Item 5): writes ONE SHARD's watch file — the same
/// on-disk contract the sidecar already reads (`WatchFile` JSON), scoped to
/// the shard's entries. Each per-conn sidecar polls its OWN `GROWW_WATCH_DIR`
/// for this file, so the ladder writes one per connection per trading day.
/// Atomic (tmp + rename), same as the daily single-conn watch file.
///
/// # Errors
/// [`WatchBuildError::WriteFailed`] on serialization or filesystem failure.
pub fn write_watch_entries_file(
    path: &Path,
    entries: &[WatchEntry],
    trading_date_ist: &str,
) -> Result<(), WatchBuildError> {
    let indices = entries
        .iter()
        .filter(|e| matches!(e.kind, WatchKind::IndexValue))
        .count();
    let file = WatchFile {
        trading_date_ist,
        feed: "groww",
        count: entries.len(),
        resolved_stocks: entries.len().saturating_sub(indices),
        indices,
        entries,
    };
    let content = serde_json::to_string_pretty(&file)
        .map_err(|e| WatchBuildError::WriteFailed(e.to_string()))?;
    write_watch_file_atomic(path, &content)
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
/// Test-only bridge to the private serializer so the native shadow client's
/// watch READER (`native::watch_reader`, PR-R1 2026-07-04) round-trips against
/// the REAL writer instead of a hand-copied fixture.
#[cfg(test)]
// TEST-EXEMPT: cfg(test)-only shim over the private serializer; exercised by the watch_reader round-trip test.
pub(crate) fn serialize_watch_file_for_test(set: &GrowwWatchSet, trading_date_ist: &str) -> String {
    serialize_watch_file(set, trading_date_ist).expect("test watch-file serialization")
}

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

/// Allowed response `Content-Type` values for a CSV body. Rejecting `text/html`
/// (a WAF block page) and `application/json` (a CDN/error body) prevents feeding
/// a non-CSV payload to the parser. Mirrors the audited Dhan downloader
/// (`csv_downloader.rs`, §18 hardening). A MISSING header is allowed — Groww /
/// niftyindices static CDNs may omit it, and upper-layer row/column validation
/// still guards.
const ALLOWED_GROWW_CSV_CONTENT_TYPES: &[&str] =
    &["text/csv", "application/octet-stream", "text/plain"];

/// Validate a response `Content-Type` against `ALLOWED_GROWW_CSV_CONTENT_TYPES`.
/// Pure + unit-tested. Missing header → `Ok` (static-CDN omission). Present →
/// strip the `; charset=…` suffix, lowercase, and allowlist-check.
fn validate_groww_content_type(
    header: Option<&reqwest::header::HeaderValue>,
) -> Result<(), WatchBuildError> {
    let Some(value) = header else {
        return Ok(());
    };
    let raw = value
        .to_str()
        .map_err(|_| WatchBuildError::FetchFailed("content-type: invalid utf-8".to_string()))?;
    let primary = raw.split(';').next().unwrap_or("").trim().to_lowercase();
    if ALLOWED_GROWW_CSV_CONTENT_TYPES
        .iter()
        .any(|&allowed| allowed == primary)
    {
        Ok(())
    } else {
        Err(WatchBuildError::FetchFailed(format!(
            "rejected content-type: {primary}"
        )))
    }
}

/// One bounded GET → UTF-8 text, body-size-capped at `MAX_CSV_BODY_BYTES`.
// TEST-EXEMPT: live network GET; the body cap + status + content-type checks are the hardening, the parse/resolve primitives are unit-tested.
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
    // §18-parity: reject a non-CSV Content-Type (WAF/error body) BEFORE reading
    // the body, so a 200-OK `text/html` block page never reaches the CSV parser.
    validate_groww_content_type(resp.headers().get(reqwest::header::CONTENT_TYPE))?;
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
    // §36/§36.7: the SAME validated IST trading date drives the all-months
    // futures selection (already strict-format-checked above).
    let today_ist = chrono::NaiveDate::parse_from_str(trading_date_ist, "%Y-%m-%d")
        .map_err(|e| WatchBuildError::WriteFailed(format!("trading_date parse: {e}")))?;
    let set = build_groww_watch_from_csvs(&groww_csv, &ntm_csv, max_subscribe, today_ist)?;
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

// ---------------------------------------------------------------------------
// local-runtime MAX-SCALE LAB (operator 2026-07-04, `local-runtime` branch
// ONLY — never merges to main). The §34 ladder shards whatever watch set it
// is handed; the daily indices+NTM set (~767, [100,1200]-enveloped) can never
// need more than 1 connection. To exercise the 100K goal the scale lane needs
// the ENTIRE Groww master as its watch set. These two fns provide exactly
// that, behind `feeds.groww.scale.full_master_universe = true` (default OFF).
// The [100,1200] envelope is DELIBERATELY not applied here — it bounds the
// production subscription universe, not the lab's discovery set; the ladder's
// own `effective_ceiling` + `target_connections × instruments_per_conn`
// bound the subscribed subset (subscribe what exists, record actual).
// ---------------------------------------------------------------------------

/// Pure full-master watch-set builder (max-scale lab). Maps EVERY usable
/// Groww master row to a [`WatchEntry`]:
///
/// - `instrument_type == "IDX"` → [`WatchKind::IndexValue`] with the stable
///   Groww-native index id (`stable_index_security_id`), same as the daily
///   builder — an index tick from the lab set folds identically.
/// - every other row with a NUMERIC `exchange_token` → [`WatchKind::Ltp`]
///   with `security_id = token` (stocks + FNO derivatives).
/// - rows with an empty or non-numeric token (non-IDX) are SKIPPED and
///   counted — never a `security_id = 0` entry.
/// - **operator scope 2026-07-05 (FIX 19 item 8):** only FNO (NSE+BSE),
///   NSE CASH (EQ spots + IDX indices) and BSE CASH IDX (SENSEX family)
///   rows are kept; COMMODITY (NSE+MCX) and BSE CASH EQ rows are SKIPPED
///   and counted in `skipped` (the unfiltered master was 141,109 rows —
///   the scoped set is ~100K, inside the 100-shard ceiling).
///
/// Output is deduped by the shard cutter's fail-closed identity
/// `(exchange, segment, security_id)` and sorted in the cutter's own
/// deterministic order, so `cut_shards` can never reject the set for
/// upstream duplicates (GROWW-SCALE-03 stays a genuine-bug signal).
///
/// Returns `(entries, skipped_rows)` — `skipped_rows` counts unusable +
/// duplicate-identity rows so the caller can log the honest delta between
/// master rows and subscribable entries.
///
/// O(1) EXEMPT: cold-path, once per scale-lab boot (~100K rows), never the
/// per-tick path.
///
/// # Errors
/// [`WatchBuildError::GrowwMasterEmpty`] when the CSV parses to zero usable
/// entries (missing header propagates the parser's own error).
pub fn full_master_entries_from_csv(
    groww_csv: &str,
) -> Result<(Vec<WatchEntry>, usize), WatchBuildError> {
    let rows = parse_groww_master(groww_csv)?;
    let total = rows.len();
    let mut entries: Vec<WatchEntry> = Vec::with_capacity(total);
    for r in rows {
        if r.exchange_token.is_empty() || r.exchange.is_empty() {
            continue;
        }
        // operator scope 2026-07-05 (FIX 19 item 8 BUG A): spots + indices +
        // index/stock F&O only. The unfiltered master was 141,109 rows —
        // NSE+BSE+MCX across CASH+FNO+COMMODITY — which overflowed the
        // 100-shard ceiling and silently dropped 42 shards. In scope:
        // FNO (NSE+BSE) + NSE CASH (EQ spots + IDX indices) + BSE CASH IDX
        // (the SENSEX family). Excluded: COMMODITY (NSE+MCX), BSE CASH EQ.
        // An empty segment normalizes to CASH (same default the entry
        // mapping below applies). Excluded rows are counted in `skipped`.
        let seg = if r.segment.is_empty() {
            "CASH"
        } else {
            r.segment.as_str()
        };
        let in_scope = match (r.exchange.as_str(), seg) {
            (_, "FNO") => true,
            ("NSE", "CASH") => true,
            ("BSE", "CASH") => r.instrument_type == "IDX",
            _ => false,
        };
        if !in_scope {
            continue; // counted in `skipped`
        }
        if r.instrument_type == "IDX" {
            entries.push(WatchEntry {
                exchange: r.exchange.clone(),
                segment: if r.segment.is_empty() {
                    "CASH".to_string()
                } else {
                    r.segment.clone()
                },
                exchange_token: r.exchange_token.clone(),
                kind: WatchKind::IndexValue,
                security_id: stable_index_security_id(&r.groww_symbol),
                isin: None,
                symbol_name: (!r.name.is_empty()).then(|| r.name.clone()),
                index_name: Some(r.groww_symbol.clone()),
                expiry_date: None,
                underlying_symbol: None,
            });
        } else if let Ok(token) = r.exchange_token.parse::<i64>() {
            if token <= 0 {
                continue; // 0/negative token can never be a valid identity
            }
            entries.push(WatchEntry {
                exchange: r.exchange.clone(),
                segment: if r.segment.is_empty() {
                    "CASH".to_string()
                } else {
                    r.segment.clone()
                },
                exchange_token: r.exchange_token.clone(),
                kind: WatchKind::Ltp,
                security_id: token,
                isin: (!r.isin.is_empty()).then(|| r.isin.clone()),
                symbol_name: (!r.name.is_empty()).then(|| r.name.clone()),
                index_name: None,
                expiry_date: None,
                underlying_symbol: None,
            });
        }
        // non-IDX row with a non-numeric token: skipped (counted below).
    }
    // Cutter-order sort + fail-closed-identity dedup (keep first occurrence).
    entries.sort_by(|a, b| {
        (a.exchange.as_str(), a.segment.as_str(), a.security_id).cmp(&(
            b.exchange.as_str(),
            b.segment.as_str(),
            b.security_id,
        ))
    });
    entries.dedup_by(|a, b| {
        a.exchange == b.exchange && a.segment == b.segment && a.security_id == b.security_id
    });
    if entries.is_empty() {
        return Err(WatchBuildError::GrowwMasterEmpty);
    }
    let skipped = total.saturating_sub(entries.len());
    Ok((entries, skipped))
}

/// Downloads the Groww master CSV (hardened client, same §18-parity policy as
/// the daily builder) and builds the full-master lab watch set. One attempt;
/// the ladder wraps this in its bounded retry + loud fallback.
///
/// # Errors
/// [`WatchBuildError::FetchFailed`] on download failure; builder errors
/// propagate from [`full_master_entries_from_csv`].
// TEST-EXEMPT: network download orchestration; the pure builder it composes (full_master_entries_from_csv) is unit-tested below.
pub async fn fetch_full_master_entries() -> Result<(Vec<WatchEntry>, usize), WatchBuildError> {
    let client = hardened_client()?;
    info!("groww max-scale lab: downloading FULL Groww master instrument.csv");
    let csv = fetch_text_hardened(&client, GROWW_INSTRUMENT_CSV_URL).await?;
    full_master_entries_from_csv(&csv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groww_test_today() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 7, 8).expect("valid date")
    }

    #[test]
    fn test_write_watch_entries_file_roundtrips_shard_entries() {
        // §34 PR-2: the ladder writes one per-conn shard watch file per day;
        // the JSON must carry the entries + counts the sidecar contract needs.
        let dir = std::env::temp_dir().join(format!("tv-shard-watch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir temp shard dir");
        let path = dir.join("groww-watch-2026-07-03.json");
        let entries = vec![
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
            WatchEntry {
                exchange: "NSE".to_owned(),
                segment: "CASH".to_owned(),
                exchange_token: "NIFTY".to_owned(),
                kind: WatchKind::IndexValue,
                security_id: 1,
                isin: None,
                symbol_name: None,
                index_name: Some("NSE-NIFTY".to_owned()),
                expiry_date: None,
                underlying_symbol: None,
            },
        ];
        write_watch_entries_file(&path, &entries, "2026-07-03").expect("write shard watch file");
        let raw = std::fs::read_to_string(&path).expect("read back");
        let json: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(json["trading_date_ist"], "2026-07-03");
        assert_eq!(json["feed"], "groww");
        assert_eq!(json["count"], 2);
        assert_eq!(json["indices"], 1);
        assert_eq!(json["resolved_stocks"], 1);
        assert_eq!(json["entries"].as_array().map(Vec::len), Some(2));
        assert_eq!(json["entries"][0]["exchange_token"], "2885");
        assert_eq!(json["entries"][1]["exchange_token"], "NIFTY");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── local-runtime max-scale lab: full-master builder (2026-07-04) ──

    const LAB_HEADER: &str =
        "exchange,exchange_token,groww_symbol,name,instrument_type,segment,series,isin";

    fn lab_csv(rows: &[&str]) -> String {
        let mut s = String::from(LAB_HEADER);
        for r in rows {
            s.push('\n');
            s.push_str(r);
        }
        s
    }

    #[test]
    fn test_full_master_entries_from_csv_builds_all_kinds() {
        // test coverage (pub-fn-test-guard line): full_master_entries_from_csv
        let csv = lab_csv(&[
            "NSE,NIFTY,NSE-NIFTY,NIFTY 50,IDX,CASH,,",
            "NSE,2885,NSE-RELIANCE,Reliance,EQ,CASH,EQ,INE002A01018",
            "NSE,55555,NSE-NIFTY-FUT,Nifty Fut,FUT,FNO,,",
            "NSE,66666,NSE-NIFTY-CE,Nifty CE,CE,FNO,,",
            "BSE,1,BSE-SENSEX,SENSEX,IDX,CASH,,",
        ]);
        let (entries, skipped) = full_master_entries_from_csv(&csv).expect("builds");
        assert_eq!(entries.len(), 5);
        assert_eq!(skipped, 0);
        let idx: Vec<&WatchEntry> = entries
            .iter()
            .filter(|e| matches!(e.kind, WatchKind::IndexValue))
            .collect();
        assert_eq!(idx.len(), 2, "both IDX rows become index_value entries");
        assert!(idx.iter().all(|e| e.security_id >= INDEX_SECURITY_ID_BIT));
        let fno = entries.iter().filter(|e| e.segment == "FNO").count();
        assert_eq!(fno, 2, "derivatives are included in the lab set");
        let rel = entries
            .iter()
            .find(|e| e.exchange_token == "2885")
            .expect("stock present");
        assert_eq!(rel.security_id, 2885);
        assert!(matches!(rel.kind, WatchKind::Ltp));
        assert_eq!(rel.isin.as_deref(), Some("INE002A01018"));
    }

    #[test]
    fn test_full_master_entries_dedup_and_order() {
        // Duplicate (exchange, segment, security_id) must collapse to ONE
        // entry so cut_shards' fail-closed identity check can never reject
        // the lab set for upstream duplicates; order must match the cutter's
        // (exchange, segment, security_id) sort.
        // FIX 19 item 8: the BSE row is FNO (BSE CASH EQ is now out of the
        // operator scope) — still proves exchange-in-key dedup.
        let csv = lab_csv(&[
            "NSE,300,NSE-B,B,EQ,CASH,EQ,",
            "NSE,100,NSE-A,A,EQ,CASH,EQ,",
            "NSE,100,NSE-A-DUP,A dup,EQ,CASH,EQ,",
            "BSE,100,BSE-A,A bse fut,FUT,FNO,,",
        ]);
        let (entries, skipped) = full_master_entries_from_csv(&csv).expect("builds");
        assert_eq!(
            entries.len(),
            3,
            "NSE dup collapsed; BSE 100 kept (exchange in key)"
        );
        assert_eq!(skipped, 1);
        let ids: Vec<(String, i64)> = entries
            .iter()
            .map(|e| (e.exchange.clone(), e.security_id))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("BSE".to_string(), 100),
                ("NSE".to_string(), 100),
                ("NSE".to_string(), 300),
            ],
            "deterministic cutter order (exchange, segment, security_id)"
        );
        // The set must be cutter-clean: cut_shards accepts it.
        let shards = crate::feed::groww::shard_cutter::cut_shards(&entries, 2).expect("cuttable");
        assert_eq!(shards.len(), 2);
    }

    #[test]
    fn test_full_master_entries_operator_scope_2026_07_05() {
        // FIX 19 item 8 BUG A: the lab set is spots + indices + index/stock
        // F&O ONLY. In scope: FNO (NSE+BSE), NSE CASH (EQ + IDX), BSE CASH
        // IDX. Excluded: COMMODITY (NSE+MCX), BSE CASH EQ. The unfiltered
        // 141,109-row master overflowed the 100-shard ceiling and silently
        // dropped 42 shards on the 2026-07-05 live probe run.
        let csv = lab_csv(&[
            // in scope
            "NSE,2885,NSE-RELIANCE,Reliance,EQ,CASH,EQ,", // NSE CASH EQ
            "NSE,NIFTY,NSE-NIFTY,NIFTY 50,IDX,CASH,,",    // NSE CASH IDX
            "NSE,55555,NSE-NIFTY-FUT,Nifty Fut,FUT,FNO,,", // NSE FNO
            "BSE,66666,BSE-SENSEX-FUT,Sensex Fut,FUT,FNO,,", // BSE FNO
            "BSE,SENSEX,BSE-SENSEX,SENSEX,IDX,CASH,,",    // BSE CASH IDX
            // out of scope
            "BSE,500325,BSE-RELIANCE,Reliance bse,EQ,CASH,A,", // BSE CASH EQ
            "MCX,77777,MCX-GOLD,Gold,FUT,COMMODITY,,",         // MCX COMMODITY
            "NSE,88888,NSE-GOLD,Gold nse,FUT,COMMODITY,,",     // NSE COMMODITY
        ]);
        let (entries, skipped) = full_master_entries_from_csv(&csv).expect("builds");
        assert_eq!(entries.len(), 5, "exactly the in-scope rows survive");
        assert_eq!(skipped, 3, "out-of-scope rows are counted, not silent");
        assert!(
            entries.iter().all(|e| e.segment != "COMMODITY"),
            "no commodity row survives"
        );
        assert!(
            !entries.iter().any(|e| e.exchange == "BSE"
                && e.segment == "CASH"
                && matches!(e.kind, WatchKind::Ltp)),
            "no BSE cash equity survives"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.exchange == "BSE" && matches!(e.kind, WatchKind::IndexValue)),
            "the BSE SENSEX-family index IS kept"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.exchange == "BSE" && e.segment == "FNO"),
            "BSE F&O IS kept"
        );
    }

    #[test]
    fn test_full_master_entries_skips_non_numeric_non_idx() {
        let csv = lab_csv(&[
            "NSE,2885,NSE-RELIANCE,Reliance,EQ,CASH,EQ,",
            "NSE,NOTNUM,NSE-WEIRD,Weird,EQ,CASH,EQ,",
            "NSE,0,NSE-ZERO,Zero,EQ,CASH,EQ,",
            "NSE,,NSE-EMPTY,Empty,EQ,CASH,EQ,",
        ]);
        let (entries, skipped) = full_master_entries_from_csv(&csv).expect("builds");
        assert_eq!(entries.len(), 1, "only the numeric-token stock survives");
        assert_eq!(skipped, 3);
        assert_eq!(entries[0].security_id, 2885);
    }

    #[test]
    fn test_full_master_entries_empty_csv_fails() {
        // Header-only → zero usable entries → fail-closed.
        assert_eq!(
            full_master_entries_from_csv(LAB_HEADER),
            Err(WatchBuildError::GrowwMasterEmpty)
        );
        // No usable rows (all tokens bad) → fail-closed too.
        let csv = lab_csv(&["NSE,BAD,NSE-X,X,EQ,CASH,EQ,"]);
        assert_eq!(
            full_master_entries_from_csv(&csv),
            Err(WatchBuildError::GrowwMasterEmpty)
        );
    }

    // §18-parity Content-Type allowlist guard (2026-07-01 security review).
    fn ct(v: &'static str) -> reqwest::header::HeaderValue {
        reqwest::header::HeaderValue::from_static(v)
    }

    #[test]
    fn test_content_type_missing_is_accepted() {
        // Static CDNs (Groww / niftyindices) may omit the header — accept.
        assert!(validate_groww_content_type(None).is_ok());
    }

    #[test]
    fn test_content_type_text_csv_accepted() {
        assert!(validate_groww_content_type(Some(&ct("text/csv"))).is_ok());
        assert!(validate_groww_content_type(Some(&ct("application/octet-stream"))).is_ok());
        assert!(validate_groww_content_type(Some(&ct("text/plain"))).is_ok());
    }

    #[test]
    fn test_content_type_charset_suffix_stripped() {
        assert!(validate_groww_content_type(Some(&ct("text/csv; charset=utf-8"))).is_ok());
    }

    #[test]
    fn test_content_type_uppercase_accepted() {
        assert!(validate_groww_content_type(Some(&ct("TEXT/CSV"))).is_ok());
    }

    #[test]
    fn test_content_type_html_rejected() {
        // A WAF block page (200 OK + text/html) must NOT reach the CSV parser.
        assert!(validate_groww_content_type(Some(&ct("text/html"))).is_err());
        assert!(validate_groww_content_type(Some(&ct("text/html; charset=utf-8"))).is_err());
    }

    #[test]
    fn test_content_type_json_rejected() {
        assert!(validate_groww_content_type(Some(&ct("application/json"))).is_err());
    }

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
    /// Index row carrying a SHORT subscribe `exchange_token` AND a separate
    /// human DISPLAY `name` — mirrors the REAL Groww master (e.g. token `NIFTYJR`
    /// + name "Nifty Next 50"). The 5th column is `name`.
    fn idx_row_named(token: &str, name: &str) -> String {
        format!("NSE,{token},{token},NSE-{token},{name},IDX,CASH,,,,,,,,,,0,0,0,{token},0")
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
    fn test_parse_groww_master_parses_name_column() {
        // FIX (2026-06-28): the `name` display column is now parsed (was discarded).
        let csv = format!("{HEADER}\n{}\n", idx_row_named("NIFTYJR", "Nifty Next 50"));
        let rows = parse_groww_master(&csv).expect("parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].exchange_token, "NIFTYJR");
        assert_eq!(rows[0].name, "Nifty Next 50");
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
        // Empty `name` column → `symbol_name` stays None (token-only fallback).
        assert_eq!(entries[0].symbol_name, None);
    }

    #[test]
    fn test_extract_index_entries_retains_display_name() {
        // FIX (2026-06-28): a Groww index row with a display `name` carries it as
        // `symbol_name` so the coverage audit can canonicalize it.
        let csv = format!("{HEADER}\n{}", idx_row_named("NIFTYAUTO", "NIFTY Auto"));
        let rows = parse_groww_master(&csv).unwrap();
        let entries = extract_index_entries(&rows);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].exchange_token, "NIFTYAUTO");
        assert_eq!(entries[0].symbol_name.as_deref(), Some("NIFTY Auto"));
        assert_eq!(entries[0].index_name.as_deref(), Some("NSE-NIFTYAUTO"));
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
            expiry_date: None,
            underlying_symbol: None,
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
            expiry_date: None,
            underlying_symbol: None,
        }
    }

    #[test]
    fn test_assemble_rejects_when_ntm_tolerance_exceeded() {
        // 5 unresolved of 100 = 5% > 2% → reject.
        let stocks: Vec<WatchEntry> = (0..95).map(|i| stock_entry(&i.to_string())).collect();
        let unresolved: Vec<String> = (0..5).map(|i| format!("U{i}")).collect();
        let err = assemble_watch_set(vec![], stocks, vec![], unresolved, 100, None).unwrap_err();
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
        let err = assemble_watch_set(vec![], stocks, vec![], vec![], 50, None).unwrap_err();
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
        let set = assemble_watch_set(vec![], stocks, vec![], vec![], 100, None).unwrap();
        assert_eq!(set.entries.len(), 100);
    }

    #[test]
    fn test_assemble_deterministic_cap_indices_first() {
        let indices = vec![index_entry("AAA"), index_entry("BBB")];
        let stocks: Vec<WatchEntry> = (0..200).map(|i| stock_entry(&format!("{i:04}"))).collect();
        // cap to 5 → 2 indices first, then 3 stocks (token asc: 0000,0001,0002).
        let set = assemble_watch_set(indices, stocks, vec![], vec![], 200, Some(5)).unwrap();
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
        let set = assemble_watch_set(vec![], stocks, vec![], vec![], 1100, Some(5000)).unwrap();
        assert_eq!(set.entries.len(), GROWW_MAX_SUBSCRIPTIONS);
        // The master records the FULL pre-cap universe (1100), even though the live
        // subscribe set is clamped to the 1000 hard cap.
        assert_eq!(
            set.master_entries.len(),
            1100,
            "master_entries holds the full pre-cap universe, not the 1000-clamped live set"
        );
    }

    fn future_entry(token: &str) -> WatchEntry {
        WatchEntry {
            exchange: "NSE".to_string(),
            segment: "FNO".to_string(),
            exchange_token: token.to_string(),
            kind: WatchKind::Ltp,
            security_id: token.parse::<i64>().unwrap_or(0),
            isin: None,
            symbol_name: Some(format!("NSE-FUT-{token}")),
            index_name: None,
            expiry_date: Some("2026-07-30".to_string()),
            underlying_symbol: None,
        }
    }

    #[test]
    fn test_assemble_cap_pressure_keeps_all_futures_in_live_set() {
        // Hostile-review round 1 (2026-07-08) regression, extended §36.7:
        // universe ≥ the 1000 hard cap → ALL monthly §36 futures (3 months
        // × 4 underlyings here) MUST survive the prefix truncate (pre-fix
        // they were appended LAST and silently dropped FIRST while the
        // pre-cap gauge still reported the full count).
        let stocks: Vec<WatchEntry> = (0..1100).map(|i| stock_entry(&format!("{i:05}"))).collect();
        let futures: Vec<WatchEntry> = (0..12)
            .map(|i| future_entry(&format!("{}", 61_001 + i)))
            .collect();
        let set = assemble_watch_set(vec![], stocks, futures, vec![], 1100, None).unwrap();
        assert_eq!(set.entries.len(), GROWW_MAX_SUBSCRIPTIONS, "hard cap");
        let live_futures = set.entries.iter().filter(|e| e.segment == "FNO").count();
        assert_eq!(live_futures, 12, "all §36.7 futures survive cap pressure");
        // Cap-priority: the futures are the deterministic PREFIX.
        assert!(set.entries[..12].iter().all(|e| e.segment == "FNO"));
        // Master carries the full pre-cap universe (1112).
        assert_eq!(set.master_entries.len(), 1112);
    }

    /// Hostile-review round 2 (2026-07-08, F3): `expected_futures` for the
    /// cap-drop attribution is the DISTINCT (exchange, token, segment) count
    /// — an intra-futures duplicate-token collapse must never be blamed on
    /// an operator cap override.
    #[test]
    fn test_distinct_future_key_count_folds_duplicate_tokens() {
        let futures = vec![
            future_entry("61001"),
            future_entry("61001"), // duplicate token — vendor corruption
            future_entry("61002"),
        ];
        assert_eq!(distinct_future_key_count(&futures), 2);
        // And the assembled live set holds exactly the 2 distinct futures —
        // matching `expected`, so the cap-drop arm does NOT fire for this.
        let stocks: Vec<WatchEntry> = (0..100).map(|i| stock_entry(&format!("{i:04}"))).collect();
        let set = assemble_watch_set(vec![], stocks, futures, vec![], 100, None).unwrap();
        let live_futures = set.entries.iter().filter(|e| e.segment == "FNO").count();
        assert_eq!(live_futures, 2);
    }

    #[test]
    fn test_assemble_master_entries_full_when_capped() {
        // Decoupling proof: a small explicit cap shrinks the live `entries` but
        // NEVER the master. 200 stocks, cap=10 → entries=10, master_entries=200.
        let stocks: Vec<WatchEntry> = (0..200).map(|i| stock_entry(&format!("{i:04}"))).collect();
        let set = assemble_watch_set(vec![], stocks, vec![], vec![], 200, Some(10)).unwrap();
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
        let set = assemble_watch_set(vec![], stocks, vec![], vec![], 150, None).unwrap();
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
        let set =
            build_groww_watch_from_csvs(&groww, &ntm, None, groww_test_today()).expect("build ok");
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
        let set = build_groww_watch_from_csvs(&groww, &ntm, None, groww_test_today())
            .expect("build ok under tolerance");
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
            groww_test_today(),
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

    /// The 24 REAL Groww NSE index `(exchange_token, display name)` pairs from
    /// the live master (2026-06-28). The short tokens (`NIFTYJR`, `NIFTYAUTO`)
    /// match the trading-symbol allowlist entries; the display names
    /// ("Nifty Next 50", "NIFTY Auto") match the descriptive entries. The OLD
    /// audit (token-only) would falsely flag ~22 of these absent → the live
    /// `absent_on_groww=28` bug. This is the regression the synthetic FIX-C test
    /// missed (it fed allowlist-spelled tokens, so token-only matched trivially).
    #[cfg(feature = "daily_universe_fetcher")]
    const REAL_GROWW_NSE_INDICES: &[(&str, &str)] = &[
        ("NIFTY", "NIFTY 50"),
        ("BANKNIFTY", "NIFTY Bank"),
        ("FINNIFTY", "Nifty Financial Services"),
        ("INDIAVIX", "India Vix"),
        ("NIFTYJR", "Nifty Next 50"),
        ("MIDCAP50", "NIFTY MIDCAP 50"),
        ("NIFTY100", "NIFTY 100"),
        ("NIFTY500", "NIFTY 500"),
        ("NIFTYAUTO", "NIFTY Auto"),
        ("NIFTYCDTY", "NIFTY Commodities"),
        ("NIFTYFMCG", "NIFTY FMCG"),
        ("NIFTYIT", "NIFTY IT"),
        ("NIFTYMEDIA", "NIFTY Media"),
        ("NIFTYMETAL", "NIFTY Metal"),
        ("NIFTYMIDCAP", "NIFTY Midcap 100"),
        ("NIFTYMIDCAP150", "NIFTY Midcap 150"),
        ("NIFTYMIDSELECT", "Nifty Midcap Select"),
        ("NIFTYPHARMA", "NIFTY Pharma"),
        ("NIFTYPSUBANK", "NIFTY PSU Bank"),
        ("NIFTYPVTBANK", "NIFTY Pvt Bank"),
        ("NIFTYREALTY", "NIFTY Realty"),
        ("NIFTYSMALL", "NIFTY Smallcap 100"),
        ("NIFTYSMALLCAP250", "NIFTY Smallcap 250"),
        ("NIFTYTOTALMCAP", "Nifty Total Market"),
    ];

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_indices_absent_vs_dhan_is_exactly_the_ten_real_spellings() {
        use crate::instrument::index_extractor::canonicalize_index_symbol;

        // Build a Groww master from the REAL 24 NSE index rows (short token +
        // display name) plus BSE SENSEX (present on Groww, NOT an NSE-allowlist
        // member → must not appear in the absent set).
        let mut csv = String::from(HEADER);
        for (token, name) in REAL_GROWW_NSE_INDICES {
            csv.push('\n');
            csv.push_str(&idx_row_named(token, name));
        }
        csv.push('\n');
        csv.push_str(&bse_sensex_row());
        csv.push('\n');

        let rows = parse_groww_master(&csv).unwrap();
        let index_entries = extract_index_entries(&rows);

        let absent = groww_indices_absent_vs_dhan(&index_entries);
        let absent_set: std::collections::HashSet<String> = absent
            .iter()
            .map(|n| canonicalize_index_symbol(n))
            .collect();
        let expected: std::collections::HashSet<String> = KNOWN_ABSENT_ON_GROWW
            .iter()
            .map(|n| canonicalize_index_symbol(n))
            .collect();

        // The exact operator-confirmed truth: 10, NOT the buggy 28.
        assert_eq!(
            absent.len(),
            10,
            "real Groww short-code spellings must yield EXACTLY 10 absent \
             (the live bug reported 28); got {absent:?}"
        );
        assert_eq!(
            absent_set, expected,
            "absent_on_groww must be EXACTLY the 10 known-absent indices"
        );
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

    // ----- §36 (2026-07-08): FUTIDX-4 in the Groww watch set -----

    /// A realistic Groww FUT master row (column shape verified against
    /// `docs/groww-ref/instrument-sample.csv` line 196:
    /// `NSE,61095,360ONE26JULFUT,NSE-360ONE-28Jul26-FUT,,FUT,FNO,,,360ONE,13061,2026-07-28,-0.01,500,0.1,20001,0,1,1,360ONE26JULFUT,0`).
    /// The exact live index-future tokens/expiries are UNVERIFIED-LIVE
    /// (sandbox egress to the master CSV is proxy-blocked) — these fixtures
    /// pin the RESOLUTION CONTRACT, not live identifiers.
    fn fut_row(exchange: &str, token: &str, underlying: &str, expiry: &str) -> String {
        format!(
            "{exchange},{token},{underlying}26JULFUT,{exchange}-{underlying}-30Jul26-FUT,,FUT,FNO,,,{underlying},26000,{expiry},-0.01,75,0.05,1800,0,1,1,{underlying}26JULFUT,0"
        )
    }

    fn fut_rows_all_four(expiry: &str) -> String {
        format!(
            "{}\n{}\n{}\n{}\n",
            fut_row("NSE", "61001", "NIFTY", expiry),
            fut_row("NSE", "61002", "BANKNIFTY", expiry),
            fut_row("NSE", "61003", "MIDCPNIFTY", expiry),
            fut_row("BSE", "71001", "SENSEX", expiry),
        )
    }

    #[test]
    fn test_groww_row_parses_underlying_and_expiry_columns() {
        let csv = format!(
            "{HEADER}\n{}\n",
            fut_row("NSE", "61001", "NIFTY", "2026-07-30")
        );
        let rows = parse_groww_master(&csv).expect("parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].instrument_type, "FUT");
        assert_eq!(rows[0].segment, "FNO");
        assert_eq!(rows[0].underlying_symbol, "NIFTY");
        assert_eq!(rows[0].expiry_date, "2026-07-30");
        assert!(rows[0].isin.is_empty(), "FUT rows are ISIN-less");
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_row_missing_new_headers_degrades_not_fails_master() {
        // A master WITHOUT the underlying_symbol/expiry_date columns still
        // parses (cash/index build untouched); futures extraction degrades
        // to 4 misses — never a build failure.
        let short_header = "exchange,exchange_token,trading_symbol,groww_symbol,name,instrument_type,segment,series,isin";
        let csv = format!(
            "{short_header}\nNSE,2885,RELIANCE,NSE-RELIANCE,Reliance,EQ,CASH,EQ,INE002A01018\nNSE,61001,NIFTY26JULFUT,NSE-NIFTY-30Jul26-FUT,,FUT,FNO,,\n"
        );
        let rows = parse_groww_master(&csv).expect("short header still parses");
        assert_eq!(rows.len(), 2);
        assert!(
            rows[1].underlying_symbol.is_empty(),
            "absent column → empty"
        );
        let (entries, misses) = extract_index_future_entries(&rows, groww_test_today());
        assert!(entries.is_empty());
        assert_eq!(misses.len(), 4, "all 4 underlyings degrade — build valid");
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_index_future_entries_records_exact_canonicals() {
        // Hostile-review round 1 (2026-07-08) regression: the parity
        // canonical must come from the EXACT match, never a symbol-substring
        // re-derivation ("NSE-BANKNIFTY-…" contains "NIFTY" → BANKNIFTY and
        // MIDCPNIFTY were mislabeled NIFTY → false FUTIDX-02 pages). The
        // recorded canonicals from a full 4-underlying fixture must be
        // EXACTLY the 4 pinned underlyings, each once.
        let csv = format!("{HEADER}\n{}", fut_rows_all_four("2026-07-30"));
        let rows = parse_groww_master(&csv).expect("parse");
        let (entries, misses) = extract_index_future_entries(&rows, groww_test_today());
        assert!(misses.is_empty(), "misses: {misses:?}");
        let canonicals: Vec<&str> = entries.iter().map(|f| f.canonical).collect();
        assert_eq!(
            canonicals,
            vec!["NIFTY", "BANKNIFTY", "MIDCPNIFTY", "SENSEX"],
            "exact-match canonicals — BANKNIFTY/MIDCPNIFTY must NOT collapse into NIFTY"
        );
        // The carried expiry is the parsed chosen expiry (parity unit).
        assert!(
            entries
                .iter()
                .all(|f| f.expiry.format("%Y-%m-%d").to_string() == "2026-07-30")
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_index_future_entries_bad_token_reason_is_bad_native_token() {
        // Hostile-review round 1: all candidates token-invalid with VALID
        // expiries must report BadNativeToken, not BadExpiryFormat.
        let row = "NSE,NOTANUMBER,NIFTY26JULFUT,NSE-NIFTY-30Jul26-FUT,,FUT,FNO,,,NIFTY,26000,2026-07-30,-0.01,75,0.05,1800,0,1,1,NIFTY26JULFUT,0";
        let csv = format!("{HEADER}\n{row}\n");
        let rows = parse_groww_master(&csv).expect("parse");
        let (entries, misses) = extract_index_future_entries(&rows, groww_test_today());
        assert!(entries.is_empty());
        assert!(misses.iter().any(|m| {
            m.canonical == "NIFTY"
                && m.reason
                    == crate::instrument::index_futures::IndexFutureMissReason::BadNativeToken
        }));
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_collect_fut_underlying_symbols_seen_bounded_distinct() {
        // The Groww FUTIDX-01 evidence mirror: distinct FUT/FNO
        // underlying_symbol literals, deduped, non-FUT rows excluded.
        let csv = format!(
            "{HEADER}\n{}\n{}\n{}\n{}\n",
            fut_row("NSE", "61001", "NIFTY", "2026-07-30"),
            fut_row("NSE", "62001", "NIFTY", "2026-08-27"),
            fut_row("NSE", "61003", "MIDCPNIFTY", "2026-07-30"),
            eq_row("2885", "INE002A01018"),
        );
        let rows = parse_groww_master(&csv).expect("parse");
        let seen = collect_fut_underlying_symbols_seen(&rows);
        assert_eq!(seen, vec!["NIFTY".to_string(), "MIDCPNIFTY".to_string()]);
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_index_future_entries_takes_all_months_four_underlyings() {
        // §36.7: 2 expiries per underlying → 8 entries (BOTH months per
        // canonical), kind=ltp, segment=FNO, numeric tokens.
        let csv = format!(
            "{HEADER}\n{}{}",
            fut_rows_all_four("2026-08-27"),
            fut_rows_all_four("2026-07-30"),
        );
        // The two waves share tokens; give the August wave distinct tokens.
        let csv = csv
            .replacen("61001", "62001", 1)
            .replacen("61002", "62002", 1)
            .replacen("61003", "62003", 1)
            .replacen("71001", "72001", 1);
        let rows = parse_groww_master(&csv).expect("parse");
        let (entries, misses) = extract_index_future_entries(&rows, groww_test_today());
        assert!(misses.is_empty(), "misses: {misses:?}");
        assert_eq!(entries.len(), 8, "all months × all underlyings (§36.7)");
        for canonical in ["NIFTY", "BANKNIFTY", "MIDCPNIFTY", "SENSEX"] {
            let mut expiries: Vec<&str> = entries
                .iter()
                .filter(|f| f.canonical == canonical)
                .filter_map(|f| f.entry.expiry_date.as_deref())
                .collect();
            expiries.sort_unstable();
            assert_eq!(
                expiries,
                vec!["2026-07-30", "2026-08-27"],
                "{canonical}: both months kept"
            );
        }
        for f in &entries {
            let e = &f.entry;
            assert_eq!(e.kind, WatchKind::Ltp);
            assert_eq!(e.segment, "FNO");
            assert!(e.exchange_token.parse::<i64>().is_ok(), "numeric token");
        }
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_index_future_entries_sensex_on_bse_others_nse() {
        // A (bogus) NSE-listed SENSEX future must be skipped; the BSE row wins.
        let csv = format!(
            "{HEADER}\n{}{}\n",
            fut_rows_all_four("2026-07-30"),
            fut_row("NSE", "88888", "SENSEX", "2026-07-24"),
        );
        let rows = parse_groww_master(&csv).expect("parse");
        let (entries, misses) = extract_index_future_entries(&rows, groww_test_today());
        assert!(misses.is_empty());
        let sensex: Vec<&GrowwIndexFuture> =
            entries.iter().filter(|f| f.canonical == "SENSEX").collect();
        assert_eq!(sensex.len(), 1);
        assert_eq!(sensex[0].entry.exchange, "BSE", "SENSEX future is BSE-only");
        assert!(
            entries
                .iter()
                .filter(|f| f.entry.exchange == "NSE")
                .all(|f| f.canonical != "SENSEX"),
            "cross-exchange SENSEX listing skipped"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_index_future_entries_uses_underlying_symbol_not_trading_symbol() {
        // Row A: NIFTY-looking trading_symbol but underlying FINNIFTY → NOT
        // selected. Row B: exotic trading_symbol with underlying NIFTY →
        // selected. The match key is structural (underlying_symbol), never a
        // trading_symbol regex.
        let row_a = "NSE,90001,NIFTY26JULFUT,NSE-FINNIFTY-30Jul26-FUT,,FUT,FNO,,,FINNIFTY,26037,2026-07-30,-0.01,65,0.05,1800,0,1,1,NIFTY26JULFUT,0";
        let row_b = "NSE,90002,XXWEIRD26JULFUT,NSE-NIFTY-30Jul26-FUT,,FUT,FNO,,,NIFTY,26000,2026-07-30,-0.01,75,0.05,1800,0,1,1,XXWEIRD26JULFUT,0";
        let csv = format!("{HEADER}\n{row_a}\n{row_b}\n");
        let rows = parse_groww_master(&csv).expect("parse");
        let (entries, _misses) = extract_index_future_entries(&rows, groww_test_today());
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].entry.exchange_token, "90002",
            "underlying NIFTY wins"
        );
        assert_eq!(entries[0].canonical, "NIFTY");
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_index_future_entries_missing_underlying_degrades() {
        // Only 3 of 4 present → 3 entries + 1 whole-underlying miss
        // (expiry: None); build stays valid.
        let csv = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            fut_row("NSE", "61001", "NIFTY", "2026-07-30"),
            fut_row("NSE", "61002", "BANKNIFTY", "2026-07-30"),
            fut_row("BSE", "71001", "SENSEX", "2026-07-31"),
        );
        let rows = parse_groww_master(&csv).expect("parse");
        let (entries, misses) = extract_index_future_entries(&rows, groww_test_today());
        assert_eq!(entries.len(), 3);
        assert_eq!(misses.len(), 1);
        assert_eq!(misses[0].canonical, "MIDCPNIFTY");
        assert_eq!(misses[0].expiry, None, "whole-underlying miss");
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_index_future_entries_ambiguous_duplicate_fails_closed() {
        // Two NIFTY FUT rows at the SAME chosen expiry, different tokens →
        // fail-closed miss for THAT MONTH (never guess a token); the clean
        // second month of the SAME underlying IS kept (§36.7 per-month
        // degrade).
        let csv = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            fut_row("NSE", "61001", "NIFTY", "2026-07-30"),
            fut_row("NSE", "61099", "NIFTY", "2026-07-30"),
            fut_row("NSE", "62001", "NIFTY", "2026-08-27"),
        );
        let rows = parse_groww_master(&csv).expect("parse");
        let (entries, misses) = extract_index_future_entries(&rows, groww_test_today());
        assert_eq!(entries.len(), 1, "only the clean August month survives");
        assert_eq!(entries[0].entry.exchange_token, "62001");
        assert_eq!(entries[0].entry.expiry_date.as_deref(), Some("2026-08-27"));
        assert!(misses.iter().any(|m| {
            m.canonical == "NIFTY"
                && m.reason
                    == crate::instrument::index_futures::IndexFutureMissReason::AmbiguousDuplicateExpiry
                && m.expiry
                    == chrono::NaiveDate::from_ymd_opt(2026, 7, 30)
        }));
    }

    /// Hostile-review round 2 (2026-07-08): a vendor-glitch EXACT-duplicate
    /// master line (SAME exchange_token at the chosen expiry) collapses
    /// first-row-wins — never classified ambiguous; only truly-distinct
    /// tokens at the same expiry stay fail-closed (previous test above).
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_dedups_exact_duplicate_token_before_ambiguity() {
        let csv = format!(
            "{HEADER}\n{}{}\n",
            fut_rows_all_four("2026-07-30"),
            // Exact-duplicate NIFTY line: SAME token, SAME expiry.
            fut_row("NSE", "61001", "NIFTY", "2026-07-30"),
        );
        let rows = parse_groww_master(&csv).expect("parse");
        let (entries, misses) = extract_index_future_entries(&rows, groww_test_today());
        assert_eq!(misses, vec![], "duplicate token is not ambiguity");
        assert_eq!(entries.len(), 4, "NIFTY kept via first-row-wins");
        let nifty: Vec<_> = entries.iter().filter(|f| f.canonical == "NIFTY").collect();
        assert_eq!(nifty.len(), 1);
        assert_eq!(nifty[0].entry.exchange_token, "61001");
        // F0 (round 2): the canonical is threaded into the watch entry so the
        // feed='groww' FUTIDX master rows carry a queryable underlying.
        assert_eq!(nifty[0].entry.underlying_symbol.as_deref(), Some("NIFTY"));
    }

    /// Hostile-review round 3 (2026-07-08): mirror of the Dhan flood cap —
    /// a same-(underlying, expiry) candidate flood beyond
    /// `FUTIDX_SAME_EXPIRY_CANDIDATE_CAP` degrades fail-closed with its own
    /// reason; corrupt vendor data is never processed.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_flood_beyond_cap_degrades_fail_closed() {
        use crate::instrument::index_futures::FUTIDX_SAME_EXPIRY_CANDIDATE_CAP;
        let mut csv = format!("{HEADER}\n{}", fut_rows_all_four("2026-07-30"));
        for i in 0..=FUTIDX_SAME_EXPIRY_CANDIDATE_CAP {
            csv.push_str(&fut_row("NSE", &format!("9{i:04}"), "NIFTY", "2026-07-30"));
            csv.push('\n');
        }
        // A CLEAN second NIFTY month — §36.7 per-month degrade keeps it.
        csv.push_str(&fut_row("NSE", "62001", "NIFTY", "2026-08-27"));
        csv.push('\n');
        let rows = parse_groww_master(&csv).expect("parse");
        let (entries, misses) = extract_index_future_entries(&rows, groww_test_today());
        assert!(
            entries.iter().all(|f| !(f.canonical == "NIFTY"
                && f.entry.expiry_date.as_deref() == Some("2026-07-30"))),
            "the flooded NIFTY July dropped fail-closed"
        );
        assert!(
            entries
                .iter()
                .any(|f| f.canonical == "NIFTY"
                    && f.entry.expiry_date.as_deref() == Some("2026-08-27")),
            "§36.7 per-month degrade: the clean second month IS kept"
        );
        assert_eq!(entries.len(), 4, "3 others + NIFTY August");
        assert!(misses.iter().any(|m| {
            m.canonical == "NIFTY"
                && m.reason
                    == crate::instrument::index_futures::IndexFutureMissReason::SameExpiryCandidateFlood
                && m.expiry == chrono::NaiveDate::from_ymd_opt(2026, 7, 30)
        }));
    }

    /// §36.7: more distinct future expiries than the envelope allows =
    /// corrupt/flooded master → the WHOLE underlying degrades fail-closed;
    /// the other 3 underlyings are intact.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_monthly_serial_flood_degrades_whole_underlying() {
        use crate::instrument::index_futures::IndexFutureMissReason;
        let mut csv = format!("{HEADER}\n{}", fut_rows_all_four("2026-07-30"));
        // 6 EXTRA NIFTY months → 7 distinct future expiries for NIFTY.
        for (i, exp) in [
            "2026-08-27",
            "2026-09-24",
            "2026-10-29",
            "2026-11-26",
            "2026-12-31",
            "2027-01-28",
        ]
        .iter()
        .enumerate()
        {
            csv.push_str(&fut_row("NSE", &format!("63{i:03}"), "NIFTY", exp));
            csv.push('\n');
        }
        let rows = parse_groww_master(&csv).expect("parse");
        let (entries, misses) = extract_index_future_entries(&rows, groww_test_today());
        assert!(
            entries.iter().all(|f| f.canonical != "NIFTY"),
            "zero NIFTY entries — whole underlying fail-closed"
        );
        assert_eq!(entries.len(), 3, "the other 3 underlyings intact");
        assert!(misses.iter().any(|m| {
            m.canonical == "NIFTY"
                && m.reason == IndexFutureMissReason::MonthlySerialFlood
                && m.expiry.is_none()
        }));
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_extract_index_future_entries_never_rolls_on_expiry_day() {
        // T-0: today == the near expiry → the EXPIRING month stays chosen
        // ALONGSIDE the next month (§36.7; proves the SHARED chooser is
        // used, not a re-implementation).
        let csv = format!(
            "{HEADER}\n{}\n{}\n",
            fut_row("NSE", "61001", "NIFTY", "2026-07-30"),
            fut_row("NSE", "62001", "NIFTY", "2026-08-27"),
        );
        let rows = parse_groww_master(&csv).expect("parse");
        let t_zero = chrono::NaiveDate::from_ymd_opt(2026, 7, 30).expect("date");
        let (entries, misses) = extract_index_future_entries(&rows, t_zero);
        // Only NIFTY rows exist in this fixture — the other 3 degrade.
        assert_eq!(misses.len(), 3);
        assert!(misses.iter().all(|m| m.canonical != "NIFTY"));
        assert_eq!(entries.len(), 2, "expiring month + next month (§36.7)");
        assert!(
            entries.iter().any(|f| {
                f.entry.exchange_token == "61001"
                    && f.entry.expiry_date.as_deref() == Some("2026-07-30")
            }),
            "T-0 keeps the expiring month — the never-roll invariant"
        );
        assert!(
            entries
                .iter()
                .any(|f| f.entry.expiry_date.as_deref() == Some("2026-08-27")),
            "the next month is ALSO present"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_watch_set_includes_every_monthly_fno_future() {
        // End-to-end (§36.7): indices + stocks + 2 months × 4 FNO futures;
        // a CASH stock whose numeric token equals a future's token stays
        // distinct (the dedup key includes segment); envelope + 1000-cap
        // green.
        let mut groww = format!("{HEADER}\n{}\n", idx_row("NIFTY"));
        let mut ntm = String::from("Company Name,Industry,Symbol,Series,ISIN Code\n");
        for i in 0..120 {
            let isin = format!("INE{i:09}");
            groww.push_str(&format!("{}\n", eq_row(&format!("{}", 1000 + i), &isin)));
            ntm.push_str(&format!("Co{i},Ind,SYM{i},EQ,{isin}\n"));
        }
        // A CASH stock with token 61001 — numerically equal to the NIFTY
        // future token below.
        groww.push_str(&format!("{}\n", eq_row("61001", "INE999999999")));
        ntm.push_str("CoX,Ind,SYMX,EQ,INE999999999\n");
        groww.push_str(&fut_rows_all_four("2026-07-30"));
        // The August wave with distinct tokens.
        groww.push_str(
            &fut_rows_all_four("2026-08-27")
                .replacen("61001", "62001", 1)
                .replacen("61002", "62002", 1)
                .replacen("61003", "62003", 1)
                .replacen("71001", "72001", 1),
        );
        let set =
            build_groww_watch_from_csvs(&groww, &ntm, None, groww_test_today()).expect("build ok");
        let fno: Vec<&WatchEntry> = set.entries.iter().filter(|e| e.segment == "FNO").collect();
        assert_eq!(fno.len(), 8, "every monthly FNO future (2 months × 4)");
        assert!(fno.iter().all(|e| e.kind == WatchKind::Ltp));
        // The colliding CASH token survives alongside the FNO token.
        let colliding: Vec<&WatchEntry> = set
            .entries
            .iter()
            .filter(|e| e.exchange_token == "61001")
            .collect();
        assert_eq!(colliding.len(), 2, "CASH + FNO with same token both kept");
        // 1 index + 121 stocks + 8 futures.
        assert_eq!(set.entries.len(), 130);
        assert_eq!(set.resolved_stocks, 121);
        assert_eq!(set.indices, 1);
    }

    /// Hostile-review round 4 (2026-07-08) source-order ratchet: EVERY §36
    /// emission (FUTIDX-01 miss errors, boot-evidence lines, the parity
    /// recording) must run AFTER `assemble_watch_set` succeeds — mirroring
    /// the Dhan Step-3d post-build ordering. Pre-assemble recording let the
    /// ≤300s activation pull-until-success retry re-fire the FUTIDX-02
    /// comparator forever on a persistently-failing assemble day
    /// (NtmDanglingExceeded / UniverseSizeOutOfBounds) and log "parity OK"
    /// while Groww subscribed NOTHING. Style of
    /// `ratchet_watch_date_rederived_per_attempt_inside_loop`.
    #[test]
    fn ratchet_groww_futidx_emissions_run_after_assemble_watch_set() {
        let src: String = include_str!("instruments.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        // Needles split via concat! so this test's own source never
        // satisfies the scan vacuously.
        let extract_marker = concat!("extract_index_future_entries(&rows,", "today_ist)");
        let assemble_marker = concat!("letset=assemble_", "watch_set(");
        let record_marker = concat!("record_index_future_", "selection(\"groww\",");
        let miss_emit_marker = concat!("underlying=miss.", "canonical,");
        let extract_pos = src.find(extract_marker).expect("extraction present");
        let assemble_pos = src.find(assemble_marker).expect("assemble call present");
        let record_pos = src
            .find(record_marker)
            .expect("groww parity record present");
        let miss_pos = src
            .find(miss_emit_marker)
            .expect("FUTIDX-01 miss emit present");
        assert!(
            extract_pos < assemble_pos,
            "extraction must precede assemble (its entries feed the set)"
        );
        assert!(
            assemble_pos < record_pos,
            "the Groww parity recording must run AFTER assemble_watch_set succeeds — \
             a failing assemble day must never re-record per retry attempt"
        );
        assert!(
            assemble_pos < miss_pos,
            "the FUTIDX-01 miss emissions must run AFTER assemble_watch_set succeeds — \
             a failing assemble day must never re-page per retry attempt"
        );
    }
}
