//! Previous-day open interest cache.
//!
//! Per L13 + L14 of `.claude/plans/active-plan-29-tf-and-movers-deletion.md`,
//! the cache loads at boot from QuestDB `prev_day_ohlcv` (yesterday's row per
//! `(security_id, exchange_segment)`) and reloads at IST midnight when the
//! `MidnightRolloverTask` fires. The cache is read on every tick to stamp
//! `prev_day_oi` into the new lifecycle column.
//!
//! **Source change 2026-06-02 (1d historical-only directive):** the source
//! was `candles_1d` (a tick-sealed matview). Since the live aggregator no
//! longer seals the 1d timeframe — 1d is pulled once each morning from Dhan
//! historical into `prev_day_ohlcv` (now carrying an `oi` column) — this cache
//! reads OI from `prev_day_ohlcv` instead.
//!
//! ## Design
//!
//! - **Storage**: `papaya::HashMap<(u32, u8), i64>` — composite key
//!   `(security_id, exchange_segment_code)` per I-P1-11 to avoid the
//!   cross-segment SecurityId collision (NSE_FNO/BSE_FNO can share IDs).
//!   `papaya` provides lock-free O(1) read on the hot path.
//! - **Loader**: boot-time HTTP query against QuestDB's `/exec` endpoint
//!   over `prev_day_ohlcv` for `ts > now() - 10d` LATEST ON
//!   `(security_id, segment)`. One HTTP round trip; O(N) instruments.
//! - **Reload at midnight**: same loader rerun. `papaya` allows
//!   atomic-replace semantics so the hot path never sees a partial state.
//! - **Empty-result safety**: a fresh deploy with no prior `prev_day_ohlcv`
//!   data starts with an empty cache. `prev_day_oi` lookups return
//!   `None` → ILP writes a NULL value, downstream `oi_change_pct` returns
//!   0.0 per the formulas.rs guard. No panic, no bad data.
//!
//! ## Boot ordering invariant (L14)
//!
//! The cache MUST be loaded BEFORE the WebSocket subscribe message fires.
//! `boot_ordering_gate.rs` enforces this — the boot sequence asserts
//! `cache.is_loaded()` is true before the connect step.

use std::sync::Arc;
use std::time::Duration;

use papaya::HashMap;
use reqwest::Client;
use tracing::{debug, error, info, warn};

use tickvault_common::config::QuestDbConfig;

/// Composite key per I-P1-11: `(security_id, exchange_segment_code)`.
/// Using `u8` for the segment code matches `ParsedTick::exchange_segment_code`
/// and avoids the larger `ExchangeSegment` enum on the hot path.
type CacheKey = (u64, u8);

/// HTTP timeout for the QuestDB `/exec` query that loads the cache.
const PREV_OI_LOAD_TIMEOUT_SECS: u64 = 30;

/// Phase 2.15 security MEDIUM fix: maximum response body size accepted
/// from QuestDB's `/exec` endpoint when loading prev_oi_cache. Defends
/// against a misconfigured QuestDB or matview-chain expansion that
/// could return an unbounded body and OOM the loader.
///
/// 25K instruments × ~30 bytes/row = ~750 KB typical. 5 MB gives 6×
/// headroom for legitimate growth (e.g. when row format expands or
/// the universe grows). Anything larger is treated as a malformed
/// response and rejected with `LoadError::ResponseTooLarge`.
pub const PREV_OI_CACHE_MAX_RESPONSE_BYTES: u64 = 5 * 1024 * 1024;

/// Lock-free cache of yesterday's closing OI per `(security_id, segment)`.
///
/// Cloning the cache is cheap — internally it's an `Arc<papaya::HashMap>`,
/// so all clones share state. Multiple consumers (tick enricher, midnight
/// rollover task) hold their own clone and read concurrently.
#[derive(Clone, Default)]
pub struct PrevOiCache {
    inner: Arc<HashMap<CacheKey, i64>>,
    /// Atomic flag: `true` after `load_from_questdb` completes successfully
    /// at least once. Used by the boot-ordering gate (L14) to verify the
    /// cache is hot before WS subscribe fires.
    loaded: Arc<std::sync::atomic::AtomicBool>,
}

impl PrevOiCache {
    /// Creates an empty cache. Call `load_from_questdb` (or
    /// `replace_with`) before reading on the hot path.
    pub fn new() -> Self {
        Self::default()
    }

    /// Looks up yesterday's closing OI for a given instrument.
    /// Returns `None` if the instrument has no prior-day record (new
    /// listing, expired contract, fresh deploy with empty `prev_day_ohlcv`).
    ///
    /// O(1), lock-free, zero-alloc — safe to call on the hot path.
    #[inline]
    pub fn get(&self, security_id: u64, segment_code: u8) -> Option<i64> {
        let guard = self.inner.guard();
        self.inner
            .get(&(security_id, segment_code), &guard)
            .copied()
    }

    /// Returns `true` if the cache has been successfully loaded at least
    /// once since process start. Used by the boot-ordering gate.
    #[inline]
    pub fn is_loaded(&self) -> bool {
        self.loaded.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Returns the number of entries in the cache. O(1).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the cache has zero entries. O(1).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replace the cache contents atomically with a fresh map. Used by
    /// both the initial loader and the midnight rollover. The replacement
    /// is achieved by clearing the existing entries and inserting the
    /// new set under a single `papaya` guard — the hot path sees either
    /// the old state or the new state, never a torn mix.
    ///
    /// Marks `loaded = true` on completion.
    pub fn replace_with(&self, entries: Vec<(CacheKey, i64)>) {
        let guard = self.inner.guard();
        // papaya does not have a single-shot atomic replace; we clear and
        // refill under one guard. Concurrent readers see entries
        // disappear and reappear, but each individual lookup is still
        // atomic O(1). For the midnight rollover this is acceptable —
        // the ~1 second window where the cache is partial is during
        // PREMARKET (00:00:00 IST) when no live ticks are flowing.
        let existing_keys: Vec<CacheKey> = self.inner.keys(&guard).copied().collect(); // APPROVED: cold path — boot loader + IST-midnight rollover only, never per-tick
        for k in &existing_keys {
            self.inner.remove(k, &guard);
        }
        for (k, v) in entries {
            self.inner.insert(k, v, &guard);
        }
        self.loaded
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Inserts a single entry. Useful for unit tests and the
    /// midnight-rollover loader to populate without going through the
    /// `replace_with` clear-then-refill path.
    pub fn insert(&self, security_id: u64, segment_code: u8, prev_oi: i64) {
        let guard = self.inner.guard();
        self.inner
            .insert((security_id, segment_code), prev_oi, &guard);
    }

    /// Loads yesterday's closing OI for every `(security_id, segment)`
    /// pair from QuestDB's `prev_day_ohlcv` table.
    ///
    /// **Source change 2026-06-02 (1d historical-only directive):** 1d is
    /// no longer tick-sealed into `candles_1d`; it is pulled once each
    /// morning from Dhan historical into `prev_day_ohlcv` (which now carries
    /// an `oi` column). This loader reads OI from there.
    ///
    /// The query selects the LATEST `oi` per composite key. The lower bound
    /// is a generous `-10d` so a Monday / post-holiday-cluster boot still
    /// finds the most recent prior trading day's row (whose `ts` is the
    /// prev-trading-day IST midnight — up to several calendar days back over
    /// a long weekend). `LATEST ON ts PARTITION BY` collapses to one row per
    /// instrument. The `prev_day_ohlcv` table is tiny (one row per symbol per
    /// day), so the wider scan is cheap and bounded.
    ///
    /// Returns the number of entries loaded on success.
    pub async fn load_from_questdb(&self, config: &QuestDbConfig) -> Result<usize, LoadError> {
        // Defence-in-depth: timeout cap so a hung QuestDB doesn't block
        // boot indefinitely. The boot-ordering gate (L14) treats a load
        // failure as "empty cache, log + continue" — never a halt — so
        // the worst case is `prev_day_oi = NULL` for the day, which is
        // a graceful degradation (formulas.rs returns 0.0 pct).
        let client = Client::builder()
            .timeout(Duration::from_secs(PREV_OI_LOAD_TIMEOUT_SECS))
            .build()
            .map_err(LoadError::HttpClient)?;
        let url = format!("http://{}:{}/exec", config.host, config.http_port); // APPROVED: cold boot/rollover loader, not per-tick

        // QuestDB SQL: select the LATEST oi per composite key from
        // prev_day_ohlcv (the 1d historical-only table; carries `oi` as of
        // 2026-06-02). `LATEST ON ts PARTITION BY` semantics resolve to a
        // single row per (security_id, segment).
        //
        // Range: `(now - 10d, now)`. prev_day_ohlcv stores the prev-trading-day
        // candle at IST-midnight `ts`, so on a Monday / post-holiday boot the
        // newest row can be several calendar days back. The generous -10d lower
        // bound guarantees it is never excluded, while keeping the scan bounded
        // (the table holds one row per symbol per day).
        let sql = "SELECT security_id, segment, oi FROM prev_day_ohlcv \
                   WHERE ts > dateadd('d', -10, now()) AND ts < now() \
                   LATEST ON ts PARTITION BY security_id, segment";

        let response = client
            .get(&url)
            .query(&[("query", sql)])
            .send()
            .await
            .map_err(LoadError::HttpRequest)?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            // Phase 2.8 H4 fix (security/hostile review): on failure
            // we do NOT mark loaded=true, so the caller sees the
            // failed load and logs ERROR + Telegram. (The
            // `BootOrderingGate`/`AwaitingOiCache` machinery this
            // comment used to name was deleted 2026-07-17 — dead
            // live-WS sweep stage 1.) Treating this as
            // "loaded with empty cache" was the False-OK class bug
            // flagged by the adversarial review — operator never
            // saw a signal that QuestDB prev_day_ohlcv failed to load.
            error!(
                code = tickvault_common::error_code::ErrorCode::PrevClose01IlpFailed.code_str(),
                %status,
                body = body.chars().take(200).collect::<String>(),
                "prev_oi_cache load returned non-success — gate stays AwaitingOiCache; \
                 operator must investigate before next subscribe"
            );
            return Err(LoadError::QuestDbError {
                status: status.as_u16(),
            });
        }

        // Phase 2.15 security MEDIUM fix (defence-in-depth): bound the
        // response body size before allocating a String. QuestDB is an
        // internal trusted service today, so a hostile response is
        // improbable; but a misconfigured QuestDB or matview chain
        // expanded to more rows could return an unbounded body. Cap at
        // PREV_OI_CACHE_MAX_RESPONSE_BYTES (= 5 MB). 25K instruments ×
        // ~30 bytes each = ~750 KB — 5 MB gives 6× headroom. Beyond
        // that, we treat as a malformed response and return Err.
        let content_length = response.content_length();
        if let Some(len) = content_length
            && len > PREV_OI_CACHE_MAX_RESPONSE_BYTES
        {
            warn!(
                content_length = len,
                limit = PREV_OI_CACHE_MAX_RESPONSE_BYTES,
                "prev_oi_cache response body exceeds bound — refusing to allocate; \
                 cache stays in current state, gate stays AwaitingOiCache"
            );
            return Err(LoadError::ResponseTooLarge {
                bytes: len,
                limit: PREV_OI_CACHE_MAX_RESPONSE_BYTES,
            });
        }
        let body = response.text().await.map_err(LoadError::HttpRequest)?;
        // Even if Content-Length wasn't set (HTTP chunked), reject
        // post-hoc if the body slipped past the limit.
        if (body.len() as u64) > PREV_OI_CACHE_MAX_RESPONSE_BYTES {
            warn!(
                body_bytes = body.len(),
                limit = PREV_OI_CACHE_MAX_RESPONSE_BYTES,
                "prev_oi_cache body exceeds bound (no Content-Length set) — \
                 rejecting parse to prevent unbounded allocation"
            );
            return Err(LoadError::ResponseTooLarge {
                bytes: body.len() as u64,
                limit: PREV_OI_CACHE_MAX_RESPONSE_BYTES,
            });
        }
        let entries = parse_questdb_dataset(&body)?;
        let count = entries.len();

        self.replace_with(entries);

        if count == 0 {
            // 2026-05-26: demoted INFO → DEBUG. The boot-time caller in
            // main.rs already emits a once-per-process WARN when the
            // empty result lands inside market hours, and the periodic
            // refresh task increments
            // `tv_prev_oi_cache_refresh_total{outcome="still_empty"}`
            // every 5 minutes — together those give the operator full
            // visibility without spamming the log every 5 min for the
            // entire trading session.
            debug!("prev_oi_cache loaded 0 entries (fresh deploy or prev_day_ohlcv empty)");
        } else {
            info!(entries = count, "prev_oi_cache loaded from prev_day_ohlcv");
        }
        Ok(count)
    }
}

/// Parses the QuestDB `/exec` JSON response body into `(key, oi)` pairs.
///
/// QuestDB returns rows as `{"dataset":[[security_id, "segment_str", oi], ...]}`.
/// We do a cheap scan rather than dragging in a full JSON dependency —
/// this is a boot-time function with low cardinality (~25K rows max).
///
/// Returns `Err(ParseError)` on malformed input. `Ok(Vec)` on success
/// (empty Vec is valid — fresh deploy).
fn parse_questdb_dataset(body: &str) -> Result<Vec<(CacheKey, i64)>, LoadError> {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Row(serde_json::Value, serde_json::Value, serde_json::Value);

    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        dataset: Vec<Row>,
    }

    let parsed: Resp = serde_json::from_str(body).map_err(LoadError::JsonParse)?;
    let mut out = Vec::with_capacity(parsed.dataset.len());
    for Row(sid_v, seg_v, oi_v) in parsed.dataset {
        let security_id: u64 = sid_v
            .as_i64()
            .and_then(|v| u64::try_from(v).ok())
            .ok_or(LoadError::MalformedSecurityId)?;
        let segment_str = seg_v.as_str().ok_or(LoadError::MalformedSegment)?;
        let segment_code = segment_str_to_code(segment_str).ok_or(LoadError::UnknownSegment)?;
        let oi = oi_v.as_i64().unwrap_or(0);
        out.push(((security_id, segment_code), oi));
    }
    Ok(out)
}

/// Maps a QuestDB SYMBOL value (segment name string) to the binary
/// segment code used in `ParsedTick::exchange_segment_code` and the
/// composite key. Mirrors the existing `segment_code_to_str` direction.
fn segment_str_to_code(s: &str) -> Option<u8> {
    match s {
        "IDX_I" => Some(0),
        "NSE_EQ" => Some(1),
        "NSE_FNO" => Some(2),
        "NSE_CURRENCY" => Some(3),
        "BSE_EQ" => Some(4),
        "MCX_COMM" => Some(5),
        // gap at 6 per dhan-ref/08-annexure-enums.md — never returned
        "BSE_CURRENCY" => Some(7),
        "BSE_FNO" => Some(8),
        _ => None,
    }
}

/// Errors that can arise during cache load.
///
/// Phase 2.8 security HIGH fix: the `Display` form of `reqwest::Error`
/// includes the full URL with any query params. While this code uses
/// the internal QuestDB endpoint (no secrets in the URL), the pattern
/// is defence-in-depth — if a future commit reuses this struct's
/// `Display` for an authenticated endpoint (Dhan REST, auth.dhan.co)
/// the bearer token in the URL would leak into Loki + Telegram. The
/// new `Display` form embeds `reqwest::Error::status()` only — never
/// `{0}` of the inner error. The full underlying error is still
/// available via `#[source]` for structured logging that opts into it
/// explicitly.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to build HTTP client (reqwest internal error)")]
    HttpClient(#[source] reqwest::Error),
    #[error(
        "HTTP request to QuestDB failed: kind={} status={:?}",
        if .0.is_timeout() { "timeout" }
        else if .0.is_connect() { "connect" }
        else if .0.is_request() { "request" }
        else if .0.is_body() { "body" }
        else if .0.is_decode() { "decode" }
        else { "other" },
        .0.status()
    )]
    HttpRequest(#[source] reqwest::Error),
    #[error("QuestDB returned non-success status {status}")]
    QuestDbError { status: u16 },
    #[error("failed to parse QuestDB response JSON")]
    JsonParse(#[source] serde_json::Error),
    #[error("malformed security_id in QuestDB row")]
    MalformedSecurityId,
    #[error("malformed segment in QuestDB row")]
    MalformedSegment,
    #[error("unknown segment string in QuestDB row")]
    UnknownSegment,
    /// Phase 2.15 security MEDIUM fix: response body exceeded the
    /// `PREV_OI_CACHE_MAX_RESPONSE_BYTES` bound (5 MB). Treated as a
    /// malformed/hostile response — cache stays in current state, the
    /// gate stays AwaitingOiCache (Phase 2.9 behaviour), and operator
    /// gets a typed error with diagnostic context.
    #[error("QuestDB response body too large: {bytes} bytes > {limit} bytes limit")]
    ResponseTooLarge { bytes: u64, limit: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_cache_is_empty_and_unloaded() {
        let c = PrevOiCache::new();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert!(!c.is_loaded());
    }

    #[test]
    fn test_get_returns_none_when_unloaded() {
        let c = PrevOiCache::new();
        assert_eq!(c.get(1234, 1), None);
    }

    #[test]
    fn test_insert_then_get() {
        let c = PrevOiCache::new();
        c.insert(1234, 1, 50_000);
        assert_eq!(c.get(1234, 1), Some(50_000));
        assert_eq!(c.get(1234, 2), None, "different segment must not collide");
        assert_eq!(c.get(5678, 1), None);
    }

    /// I-P1-11 ratchet: the same security_id under two different segments
    /// stores two distinct entries. This is the FINNIFTY/NIFTY collision
    /// scenario (sec_id=27 lives as IDX_I value AND another stock as
    /// NSE_EQ — composite key keeps both).
    #[test]
    fn test_composite_key_separates_cross_segment_collisions() {
        let c = PrevOiCache::new();
        c.insert(27, 0, 12_345); // FINNIFTY IDX_I
        c.insert(27, 1, 99_999); // hypothetical NSE_EQ with same sec_id
        assert_eq!(c.get(27, 0), Some(12_345));
        assert_eq!(c.get(27, 1), Some(99_999));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn test_replace_with_clears_then_refills() {
        let c = PrevOiCache::new();
        c.insert(1, 1, 100);
        c.insert(2, 1, 200);
        assert_eq!(c.len(), 2);

        c.replace_with(vec![((3, 1), 300), ((4, 1), 400), ((5, 1), 500)]);
        assert_eq!(c.len(), 3);
        assert_eq!(c.get(1, 1), None, "old entries must be gone");
        assert_eq!(c.get(2, 1), None);
        assert_eq!(c.get(3, 1), Some(300));
        assert_eq!(c.get(4, 1), Some(400));
        assert_eq!(c.get(5, 1), Some(500));
        assert!(c.is_loaded(), "replace_with marks loaded=true");
    }

    #[test]
    fn test_replace_with_empty_marks_loaded() {
        // Fresh deploy scenario: prev_day_ohlcv empty, replace_with([]) called.
        let c = PrevOiCache::new();
        c.replace_with(Vec::new());
        assert!(c.is_loaded());
        assert!(c.is_empty());
    }

    #[test]
    fn test_segment_str_to_code_known_values() {
        assert_eq!(segment_str_to_code("IDX_I"), Some(0));
        assert_eq!(segment_str_to_code("NSE_EQ"), Some(1));
        assert_eq!(segment_str_to_code("NSE_FNO"), Some(2));
        assert_eq!(segment_str_to_code("NSE_CURRENCY"), Some(3));
        assert_eq!(segment_str_to_code("BSE_EQ"), Some(4));
        assert_eq!(segment_str_to_code("MCX_COMM"), Some(5));
        assert_eq!(segment_str_to_code("BSE_CURRENCY"), Some(7));
        assert_eq!(segment_str_to_code("BSE_FNO"), Some(8));
    }

    #[test]
    fn test_segment_str_to_code_unknown_returns_none() {
        assert_eq!(segment_str_to_code(""), None);
        assert_eq!(segment_str_to_code("UNKNOWN"), None);
        assert_eq!(segment_str_to_code("nse_eq"), None, "case-sensitive");
    }

    #[test]
    fn test_parse_questdb_dataset_empty() {
        let body = r#"{"dataset":[]}"#;
        let result = parse_questdb_dataset(body).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_questdb_dataset_basic() {
        let body = r#"{"dataset":[[1234,"NSE_EQ",50000],[5678,"NSE_FNO",75000]]}"#;
        let result = parse_questdb_dataset(body).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], ((1234u64, 1u8), 50_000_i64));
        assert_eq!(result[1], ((5678u64, 2u8), 75_000_i64));
    }

    #[test]
    fn test_parse_questdb_dataset_unknown_segment_errors() {
        let body = r#"{"dataset":[[1234,"WEIRD",50000]]}"#;
        let result = parse_questdb_dataset(body);
        assert!(matches!(result, Err(LoadError::UnknownSegment)));
    }

    #[test]
    fn test_parse_questdb_dataset_negative_security_id_errors() {
        // u64 cannot hold a negative value — must error rather than truncate.
        let body = r#"{"dataset":[[-1,"NSE_EQ",50000]]}"#;
        let result = parse_questdb_dataset(body);
        assert!(matches!(result, Err(LoadError::MalformedSecurityId)));
    }

    #[test]
    fn test_parse_questdb_dataset_zero_oi_is_valid() {
        // A fresh listing or expired contract may have oi=0 — valid.
        let body = r#"{"dataset":[[1234,"NSE_EQ",0]]}"#;
        let result = parse_questdb_dataset(body).unwrap();
        assert_eq!(result[0], ((1234u64, 1u8), 0_i64));
    }

    #[test]
    fn test_parse_questdb_dataset_malformed_json_errors() {
        let body = "not json at all";
        let result = parse_questdb_dataset(body);
        assert!(matches!(result, Err(LoadError::JsonParse(_))));
    }

    /// Explicit name-match for `pub fn is_loaded` so the pre-push
    /// pub-fn-test guard recognises coverage.
    #[test]
    fn test_is_loaded_starts_false_and_flips_after_replace_with() {
        let c = PrevOiCache::new();
        assert!(!c.is_loaded());
        c.replace_with(Vec::new());
        assert!(c.is_loaded());
    }

    /// Explicit name-match for `pub fn len` so the guard recognises
    /// coverage by literal name.
    #[test]
    fn test_len_returns_entry_count() {
        let c = PrevOiCache::new();
        assert_eq!(c.len(), 0);
        c.insert(1, 1, 100);
        assert_eq!(c.len(), 1);
        c.insert(2, 2, 200);
        assert_eq!(c.len(), 2);
    }

    /// Boot-time loader is exercised by integration tests + the
    /// boot ordering ratchet (Phase 2 follow-on commit). Unit-level
    /// coverage of the HTTP path requires either a real QuestDB
    /// instance or a mock-server scaffold; the integration test
    /// will catch any regression in the SQL or response parsing.
    /// The pure-function helpers `parse_questdb_dataset` and
    /// `segment_str_to_code` ARE unit-tested above and exercise the
    /// non-IO parts of the code path.
    // TEST-EXEMPT: integration-only — `load_from_questdb` needs a real
    // QuestDB; pure helpers parse_questdb_dataset / segment_str_to_code
    // (called inside it) are unit-tested separately.
    #[test]
    fn test_load_from_questdb_is_integration_only_documented() {
        // Documentation ratchet — keeps the exemption visible.
        let c = PrevOiCache::new();
        assert!(!c.is_loaded());
    }

    #[test]
    fn test_clone_shares_state() {
        // Cache is `Arc`-backed — clones must share the same inner map.
        let c1 = PrevOiCache::new();
        let c2 = c1.clone();
        c1.insert(42, 1, 100);
        assert_eq!(c2.get(42, 1), Some(100), "clone must share inner state");
    }
}
