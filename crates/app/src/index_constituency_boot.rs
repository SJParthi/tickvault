//! `index_constituency` boot population — NTM Sub-PR "Map-B" (operator
//! 2026-06-06, "full per-index mapping … guarantee and assurance").
//!
//! Populates the Map-A `index_constituency` table with the FULL §31-item-2
//! mapping: for ALL ~46 tracked indices, every constituent stock resolved to its
//! Dhan `security_id` (ISIN-primary), one row per `(index, stock)`.
//!
//! ## Why this is a SEPARATE, decoupled step (operator's "don't change existing
//! logic")
//!
//! The live subscription path (NTM-union-only fetch + fold) is UNTOUCHED. This
//! module runs independently AFTER the universe build: it re-fetches the full
//! constituency map (all 46 slugs) + the Dhan master, resolves per-index, and
//! writes the mapping table. It is **map-only** — it NEVER feeds the WebSocket
//! subscription. Spawned fire-and-forget so it never delays boot or the live feed.
//!
//! ## Degrade-safe (never blocks, never panics)
//!
//! Any failure (downloader init, niftyindices fetch, Dhan CSV fetch/parse,
//! resolve) logs `warn!` and returns — the live trading path is unaffected. The
//! mapping is a best-effort observability surface, refreshed next boot.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21.

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::HashMap;

use chrono::NaiveDate;
use tracing::{info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::INDEX_CONSTITUENCY_SLUGS;
use tickvault_common::instrument_types::IndexConstituencyMap;
use tickvault_core::instrument::constituent_resolver::{ResolvedConstituent, resolve_constituents};
use tickvault_core::instrument::csv_downloader::CsvDownloader;
use tickvault_core::instrument::csv_parser::parse_detailed_csv;
use tickvault_core::instrument::index_constituency::build_constituency_map;
use tickvault_core::instrument::index_constituency::downloader::ConstituencyDownloader;
use tickvault_storage::index_constituency_persistence::{
    INDEX_CONSTITUENCY_FEED_DHAN, IndexConstituencyRow, append_index_constituency_rows,
    ensure_index_constituency_table, migrate_index_constituency_truncate_once,
};

/// Quiet QuestDB readiness probe attempts for the boot-prefix ts-pin
/// migration task (F13/F14 hardening, 2026-07-05). 12 × 5s = 60s bound —
/// deliberately NOT `wait_for_questdb_ready` (that fn emits BOOT-01/BOOT-02
/// pages reserved for the boot path; this is cold-path forensic-master work).
pub const TS_PIN_MIGRATION_READINESS_ATTEMPTS: u32 = 12;

/// Backoff between readiness probe attempts (seconds).
pub const TS_PIN_MIGRATION_READINESS_BACKOFF_SECS: u64 = 5;

/// Process-global boot-prefix ts-pin migration (F13 + F14 hardening,
/// 2026-07-05).
///
/// The one-shot, marker-gated `TRUNCATE TABLE index_constituency` migration
/// previously ran ONLY inside the Dhan lane (`persist_index_constituency_mapping`,
/// spawned at the tail of `cold_build_daily_universe`) — behind the §4
/// infinite-retry CSV boot, TOTP auth, QuestDB wait, and the ~219K-row
/// lifecycle reconcile. That let the Groww writer's bounded 120s gate wait
/// expire BEFORE the truncate (Monday cold boot, F13), and a runtime Dhan
/// enable / lane cold-start retry ran the FIRST-EVER truncate hours after the
/// Groww append (F14). This task runs the SAME gate-aware, exactly-once
/// migration in the process-global boot prefix — spawned from `main()` BEFORE
/// the Groww activation watcher, regardless of `feeds.dhan_enabled` (it needs
/// only the QuestDB config) — so the gate is genuinely marked within
/// ~75s worst case (60s quiet probe + one bounded TRUNCATE HTTP attempt),
/// inside the Groww writer's 120s wait, on EVERY boot mode.
///
/// Degrade-safe: probe exhaustion or a failed TRUNCATE logs + marks the gate
/// (appends proceed; marker not written; the NEXT process boot re-runs the
/// migration in ITS boot prefix, again BEFORE that boot's feed appends). The
/// Dhan-lane call site remains as defense-in-depth; the exactly-once latch in
/// the wrapper makes it a no-op once this task has run.
// TEST-EXEMPT: network I/O orchestration (quiet probe + live QuestDB TRUNCATE) — the ordering is pinned by the main.rs source-order ratchet below; the exactly-once wrapper + gate primitives are unit-tested in the storage crate; the probe-bound constants are unit-tested here.
pub async fn run_index_constituency_ts_pin_migration_at_boot(questdb: QuestDbConfig) {
    let probe_url = format!(
        "http://{}:{}/exec?query=SELECT%201",
        questdb.host, questdb.http_port
    );
    let mut ready = false;
    match tickvault_storage::http_client::shared_probe_client() {
        Ok(client) => {
            for attempt in 1..=TS_PIN_MIGRATION_READINESS_ATTEMPTS {
                match client.get(&probe_url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        ready = true;
                        break;
                    }
                    Ok(_) | Err(_) => {
                        if attempt < TS_PIN_MIGRATION_READINESS_ATTEMPTS {
                            tokio::time::sleep(std::time::Duration::from_secs(
                                TS_PIN_MIGRATION_READINESS_BACKOFF_SECS,
                            ))
                            .await;
                        }
                    }
                }
            }
        }
        Err(err) => {
            // HTTP-CLIENT-01 class — the probe client could not be built; the
            // migration attempt below builds its own client, so proceed.
            warn!(
                %err,
                "ts-pin boot migration: probe client build failed — proceeding to the \
                 migration attempt without a readiness probe"
            );
            ready = true; // unknown, not "down" — attempt the migration
        }
    }
    if !ready {
        warn!(
            attempts = TS_PIN_MIGRATION_READINESS_ATTEMPTS,
            backoff_secs = TS_PIN_MIGRATION_READINESS_BACKOFF_SECS,
            "ts-pin boot migration: QuestDB not ready within the quiet probe bound — \
             attempting the migration anyway (a failure marks the gate, keeps the \
             marker unwritten, and the next boot retries in ITS boot prefix)"
        );
    }
    ensure_index_constituency_table(&questdb).await;
    migrate_index_constituency_truncate_once(&questdb).await;
    info!(
        questdb_ready = ready,
        "ts-pin boot migration task complete — index_constituency migration gate marked"
    );
}

/// One fully-resolved (index, stock) mapping row — OWNED so the pure builder is
/// unit-testable without borrows; the writer converts to the borrowing
/// [`IndexConstituencyRow`] for the ILP flush.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedConstituencyRow {
    pub index_name: String,
    pub security_id: i64,
    pub exchange_segment: String,
    pub symbol_name: String,
    pub isin: String,
    pub via_isin: bool,
}

/// PURE: turn the full constituency map + the flat resolved set into per-index
/// rows. For each index, each constituent symbol is looked up (O(1)) in the
/// resolved set; unresolved constituents are SKIPPED (counted by the caller via
/// the returned-vs-input delta — they are the dangling symbols the resolver
/// already reported). A resolved `security_id` that does not parse as `i64` is
/// skipped (defensive; Dhan SIDs are numeric).
///
/// O(C) where C = total (index, constituent) pairs — cold path, runs once per
/// boot. No network, no QuestDB → fully unit-tested.
#[must_use]
pub fn build_index_constituency_rows(
    full_map: &IndexConstituencyMap,
    resolved: &[ResolvedConstituent],
) -> Vec<OwnedConstituencyRow> {
    // O(1) symbol → resolved lookup (resolved is deduped by symbol upstream).
    let by_symbol: HashMap<&str, &ResolvedConstituent> =
        resolved.iter().map(|r| (r.symbol.as_str(), r)).collect();

    let mut out: Vec<OwnedConstituencyRow> = Vec::new();
    for (index_name, constituents) in &full_map.index_to_constituents {
        for c in constituents {
            let Some(r) = by_symbol.get(c.symbol.as_str()) else {
                continue; // dangling — resolver already counted it
            };
            let Ok(security_id) = r.security_id.trim().parse::<i64>() else {
                continue; // non-numeric SID (defensive)
            };
            out.push(OwnedConstituencyRow {
                index_name: index_name.clone(),
                security_id,
                exchange_segment: r.exchange_segment.clone(),
                symbol_name: r.symbol.clone(),
                isin: r.isin.clone(),
                via_isin: r.via_isin,
            });
        }
    }
    out
}

/// Fetch the full per-index mapping + persist it to `index_constituency`.
///
/// Degrade-safe: returns `()` and logs `warn!` on any failure. Spawn this
/// fire-and-forget from boot — it NEVER blocks the live feed or trading path.
// TEST-EXEMPT: network + QuestDB I/O orchestration — the pure mapping logic is unit-tested via `build_index_constituency_rows`; the resolver/parser/downloader are tested in their own crates.
pub async fn persist_index_constituency_mapping(
    questdb: QuestDbConfig,
    today: NaiveDate,
    trading_date_ist_nanos: i64,
    dry_run: bool,
) {
    // 0. FIX 13a (2026-07-04): ensure the table + run the one-time, marker-gated
    //    ts-pin TRUNCATE migration FIRST — before the niftyindices CSV fetch —
    //    so (a) the truncate→write order is preserved (the append happens later
    //    in this same fn) and (b) the `index_constituency_migration_gate` opens
    //    regardless of any CSV-fetch/parse/resolve early return below. The
    //    Groww shared-master writer awaits that gate before persisting its
    //    `feed='groww'` rows, so the truncate can never wipe another feed's
    //    just-written rows. The migration marks the gate on EVERY exit path.
    //    F14 hardening (2026-07-05): the PRIMARY migration site is now the
    //    process-global boot-prefix task
    //    (`run_index_constituency_ts_pin_migration_at_boot`, spawned before the
    //    Groww watcher); this call is defense-in-depth — the wrapper's
    //    exactly-once latch makes it a no-op once the boot-prefix run finished,
    //    and it can never fire a LATE first truncate on a runtime Dhan enable.
    ensure_index_constituency_table(&questdb).await;
    migrate_index_constituency_truncate_once(&questdb).await;

    // 1. Full constituency map (all ~46 tracked indices) — map-only, best-effort.
    let downloader = match ConstituencyDownloader::new() {
        Ok(d) => d,
        Err(err) => {
            warn!(
                ?err,
                "index_constituency map: downloader init failed — skipping mapping"
            );
            return;
        }
    };
    let full_map = build_constituency_map(&downloader, INDEX_CONSTITUENCY_SLUGS, today).await;
    if full_map.index_to_constituents.is_empty() {
        warn!(
            indices_failed = full_map.build_metadata.indices_failed,
            "index_constituency map: no indices fetched — skipping mapping this boot"
        );
        return;
    }

    // 2. Dhan master (re-fetched independently so the live subscription path is
    //    untouched). Cold path, best-effort.
    let csv_downloader = match CsvDownloader::new() {
        Ok(d) => d,
        Err(err) => {
            warn!(
                ?err,
                "index_constituency map: Dhan CSV downloader init failed"
            );
            return;
        }
    };
    let bytes = match csv_downloader.fetch_csv().await {
        Ok(b) => b,
        Err(err) => {
            warn!(?err, "index_constituency map: Dhan CSV fetch failed");
            return;
        }
    };
    let rows = match parse_detailed_csv(&bytes) {
        Ok(r) => r,
        Err(err) => {
            warn!(?err, "index_constituency map: Dhan CSV parse failed");
            return;
        }
    };

    // 3. Resolve every constituent across all indices → security_id (ISIN-primary).
    let outcome = match resolve_constituents(&full_map, &rows) {
        Ok(o) => o,
        Err(err) => {
            warn!(?err, "index_constituency map: constituent resolve failed");
            return;
        }
    };

    // 4. Per-index rows (pure) → ILP rows.
    let owned = build_index_constituency_rows(&full_map, &outcome.resolved);
    let ilp_rows: Vec<IndexConstituencyRow<'_>> = owned
        .iter()
        .map(|r| IndexConstituencyRow {
            trading_date_ist_nanos,
            index_name: &r.index_name,
            security_id: r.security_id,
            exchange_segment: &r.exchange_segment,
            symbol_name: &r.symbol_name,
            isin: &r.isin,
            via_isin: r.via_isin,
            source: "niftyindices",
            dry_run,
            // operator override 2026-06-28: feed in-key (Dhan-resolved SIDs today).
            feed: INDEX_CONSTITUENCY_FEED_DHAN,
        })
        .collect();

    // 5. Persist (idempotent UPSERT). The table ensure + one-time ts-pin
    //    migration already ran at step 0 (FIX 13a) — truncate → write order
    //    holds because this append is later in the same fn. Degrade-safe —
    //    never blocks.
    match append_index_constituency_rows(&questdb, &ilp_rows).await {
        Ok(()) => {
            info!(
                indices = full_map.index_to_constituents.len(),
                rows = ilp_rows.len(),
                unresolved = outcome.unresolved_symbols.len(),
                "index_constituency mapping persisted (map-only; subscription untouched)"
            );
        }
        Err(err) => {
            warn!(
                ?err,
                rows = ilp_rows.len(),
                "index_constituency map: persist failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::instrument_types::{
        ConstituencyBuildMetadata, IndexConstituencyMap, IndexConstituent,
    };

    fn constituent(symbol: &str, isin: &str) -> IndexConstituent {
        IndexConstituent {
            index_name: String::new(),
            symbol: symbol.to_string(),
            isin: isin.to_string(),
            weight: 0.0,
            sector: String::new(),
            last_updated: NaiveDate::from_ymd_opt(2026, 6, 6).unwrap(),
        }
    }

    fn resolved(symbol: &str, isin: &str, sid: &str, via_isin: bool) -> ResolvedConstituent {
        ResolvedConstituent {
            symbol: symbol.to_string(),
            isin: isin.to_string(),
            security_id: sid.to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            via_isin,
        }
    }

    fn map_with(indices: &[(&str, &[(&str, &str)])]) -> IndexConstituencyMap {
        let mut index_to_constituents = HashMap::new();
        let mut stock_to_indices: HashMap<String, Vec<String>> = HashMap::new();
        for (idx, members) in indices {
            let cs: Vec<IndexConstituent> =
                members.iter().map(|(s, i)| constituent(s, i)).collect();
            for (s, _) in *members {
                stock_to_indices
                    .entry((*s).to_string())
                    .or_default()
                    .push((*idx).to_string());
            }
            index_to_constituents.insert((*idx).to_string(), cs);
        }
        IndexConstituencyMap {
            index_to_constituents,
            stock_to_indices,
            build_metadata: ConstituencyBuildMetadata::default(),
        }
    }

    #[test]
    fn build_index_constituency_rows_maps_each_index_membership() {
        // RELIANCE in 2 indices → 2 rows; INFY in 1 → 1 row. 3 rows total.
        let map = map_with(&[
            (
                "Nifty 50",
                &[("RELIANCE", "INE002A01018"), ("INFY", "INE009A01021")],
            ),
            ("Nifty Bank", &[("RELIANCE", "INE002A01018")]),
        ]);
        let res = vec![
            resolved("RELIANCE", "INE002A01018", "2885", true),
            resolved("INFY", "INE009A01021", "1594", true),
        ];
        let rows = build_index_constituency_rows(&map, &res);
        assert_eq!(rows.len(), 3, "RELIANCE×2 + INFY×1");
        let reliance: Vec<_> = rows
            .iter()
            .filter(|r| r.symbol_name == "RELIANCE")
            .collect();
        assert_eq!(reliance.len(), 2, "RELIANCE in both indices");
        assert!(reliance.iter().all(|r| r.security_id == 2885));
        let idx_names: Vec<&str> = reliance.iter().map(|r| r.index_name.as_str()).collect();
        assert!(idx_names.contains(&"Nifty 50") && idx_names.contains(&"Nifty Bank"));
    }

    #[test]
    fn build_rows_skips_unresolved_constituent() {
        // ZEEL has no resolved entry (dangling) → no row for it.
        let map = map_with(&[(
            "Nifty 50",
            &[("RELIANCE", "INE002A01018"), ("ZEEL", "INE256A01028")],
        )]);
        let res = vec![resolved("RELIANCE", "INE002A01018", "2885", true)];
        let rows = build_index_constituency_rows(&map, &res);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol_name, "RELIANCE");
    }

    #[test]
    fn build_rows_skips_non_numeric_security_id() {
        let map = map_with(&[("Nifty 50", &[("RELIANCE", "INE002A01018")])]);
        let res = vec![resolved("RELIANCE", "INE002A01018", "NOTANUMBER", true)];
        let rows = build_index_constituency_rows(&map, &res);
        assert!(rows.is_empty(), "non-numeric SID skipped defensively");
    }

    #[test]
    fn build_rows_preserves_via_isin_flag() {
        let map = map_with(&[("Nifty 50", &[("RELIANCE", "INE002A01018")])]);
        let res = vec![resolved("RELIANCE", "INE002A01018", "2885", false)];
        let rows = build_index_constituency_rows(&map, &res);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].via_isin, "symbol-fallback flag preserved");
    }

    #[test]
    fn build_rows_empty_map_is_empty() {
        let map = map_with(&[]);
        let rows = build_index_constituency_rows(&map, &[]);
        assert!(rows.is_empty());
    }

    /// FIX 13a ratchet (source-scan, wal_reinject pattern): the one-time
    /// TRUNCATE migration must run BEFORE the niftyindices CSV fetch inside
    /// `persist_index_constituency_mapping`, so the migration gate opens on
    /// every boot path (early CSV-fetch returns included) and the
    /// truncate→write order can never be inverted by a refactor.
    #[test]
    fn ratchet_migration_runs_before_csv_fetch() {
        let src = include_str!("index_constituency_boot.rs");
        let migrate = src
            .find("migrate_index_constituency_truncate_once(&questdb)")
            .expect("migration call must exist in persist_index_constituency_mapping");
        let fetch = src
            .find("build_constituency_map(&downloader")
            .expect("constituency CSV fetch call must exist");
        assert!(
            migrate < fetch,
            "FIX 13a regression: the ts-pin TRUNCATE migration must be awaited \
             BEFORE the constituency CSV fetch so the migration gate opens on \
             every boot path"
        );
    }

    /// F14 ratchet (source-scan on main.rs, wal_reinject ordering pattern):
    /// the process-global boot-prefix ts-pin migration task MUST be spawned
    /// BEFORE the Groww activation watcher in `main()` source order, so the
    /// gate the Groww writer awaits is driven by a task that exists on EVERY
    /// boot mode (Dhan ON, Dhan OFF, Groww-only) — never only behind the Dhan
    /// lane's serial auth/CSV/reconcile chain (F13) or a runtime Dhan enable
    /// hours later (F14).
    #[test]
    fn ratchet_ts_pin_migration_spawns_before_groww_watcher() {
        let src = include_str!("main.rs");
        let migration = src
            .find("run_index_constituency_ts_pin_migration_at_boot(")
            .expect("the boot-prefix ts-pin migration spawn must exist in main.rs");
        let watcher = src
            .find("run_groww_activation_watcher(")
            .expect("the Groww activation watcher spawn must exist in main.rs");
        assert!(
            migration < watcher,
            "F13/F14 regression: the process-global ts-pin migration task must be \
             spawned BEFORE the Groww activation watcher in main.rs source order"
        );
    }

    /// F13 bound pin: the boot-prefix quiet probe (attempts × backoff) plus
    /// one bounded TRUNCATE attempt must fit inside the Groww writer's 120s
    /// migration-gate wait, or the gate wait becomes a guaranteed timeout on
    /// slow-QuestDB boots.
    #[test]
    fn test_ts_pin_readiness_constants_bound_inside_groww_gate_wait() {
        let probe_bound_secs = u64::from(TS_PIN_MIGRATION_READINESS_ATTEMPTS)
            * TS_PIN_MIGRATION_READINESS_BACKOFF_SECS;
        // 120 = CONSTITUENCY_MIGRATION_GATE_TIMEOUT_SECS in the core crate
        // (not importable here without a dep cycle); leave ≥ 30s headroom for
        // the migration's own bounded TRUNCATE HTTP attempt.
        assert!(
            probe_bound_secs + 30 <= 120,
            "quiet probe bound ({probe_bound_secs}s) + 30s TRUNCATE headroom must \
             fit inside the Groww writer's 120s migration-gate wait"
        );
    }
}
