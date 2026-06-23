//! Groww candle-table DDL bootstrap — second feed (operator lock §32 +
//! "same tables + feed column" 2026-06-19, `groww-second-feed-scope-2026-06-19.md`).
//!
//! **Candle WRITING is now the SHARED seal-writer chain — there is NO Groww-only
//! candle writer.** As of the ONE-common-candle-engine convergence (2026-06-23),
//! Groww ticks fold through the SAME 21-timeframe `MultiTfAggregator` as Dhan and
//! every sealed bar is routed through the shared `global_seal_sender` →
//! `SealWriterRunner` → `ShadowCandleWriter::append_seal`, tagged `feed='groww'`
//! via the `BufferedSeal::feed` field. The old `GrowwCandle1mWriter` /
//! `GrowwCandle1mRow` / `append_row` (a 1m-only writer) were DELETED — Groww now
//! generates ALL 21 timeframes through the one engine, not just 1-minute.
//!
//! This module retains ONLY the DDL bootstrap so a **Groww-only** run (which
//! skips the Dhan boot, where the candle tables are otherwise ensured) still has
//! the shared `candles_<tf>` tables + their `(ts, security_id, segment, feed)`
//! DEDUP keys before the shared writer appends.

use tickvault_common::config::QuestDbConfig;

/// Ensure the Groww candle target tables exist. Groww writes the SHARED
/// `candles_<tf>` tables, so this DELEGATES to the canonical candle-table DDL —
/// exactly mirroring how `groww_persistence::ensure_groww_live_ticks_table`
/// delegates to the shared ticks DDL (Step 3a). Only invoked when the Groww feed
/// is enabled, so that Groww-only mode (which skips the Dhan boot, where the
/// candle tables are otherwise ensured) still has the shared candle tables + their
/// 4-col feed DEDUP key before the shared seal-writer appends.
///
/// **Drops legacy candle matviews FIRST** (3-agent hostile-review MEDIUM,
/// 2026-06-19): a pre-`#T1` Engine-C materialized view named `candles_1m` makes
/// `CREATE TABLE IF NOT EXISTS candles_1m` a silent no-op, after which ILP
/// appends to that name FAIL (matviews are not ILP-writable). The Dhan boot
/// always calls `drop_legacy_candle_objects` before `ensure_shadow_candle_tables`
/// for exactly this reason; a Groww-ONLY run on a brownfield QuestDB volume must
/// do the same or Groww candles would silently never persist. Both calls are
/// idempotent (marker-gated drop + `IF NOT EXISTS` create), so this is safe to
/// run in every mode even when the Dhan path also runs them.
// TEST-EXEMPT: thin delegation to unit-tested drop_legacy_candle_objects + ensure_shadow_candle_tables.
pub async fn ensure_groww_candles_1m_table(questdb_config: &QuestDbConfig) {
    crate::shadow_persistence::drop_legacy_candle_objects(questdb_config).await;
    crate::shadow_persistence::ensure_shadow_candle_tables(questdb_config).await;
}
