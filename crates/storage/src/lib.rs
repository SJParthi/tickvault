// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
// Phase 0.2: no dropped Result/JoinHandle/must-use values (silent error swallowing).
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![cfg_attr(test, allow(clippy::assertions_on_constants))]
#![allow(missing_docs)]
// TODO: enforce after adding docs to all public items
// APPROVED: clippy 1.95 tightened these doc-formatting lints; the codebase
// predates them. Allow rather than churn ~100 doc comments for a cosmetic
// markdown-rendering nicety with zero runtime/behavior impact.
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]

//! Data persistence layer — QuestDB for time-series.
//!
//! # Modules
//! - `instrument_persistence` — daily instrument snapshot to QuestDB (Block 01.1)
//! - `tick_persistence` — batched ILP writer for live ticks
// PR-E (2026-05-26): candle_persistence retired.
//!
//! # Boot Sequence Position
//! OMS -> **QuestDB** -> HTTP API
//!
//! Note: the legacy Valkey cache layer was deleted in #O4 (2026-05-24).
//! The dual-instance lock moved to AWS SSM Parameter Store in PR #764;
//! the file-based token cache (`crates/core/src/auth/token_cache.rs`)
//! covers crash-restart speed. No remaining production caller needs an
//! in-memory KV store.

/// Wave 2 — global QuestDB config handle so any module (e.g.,
/// `connection.rs` reconnect-success) can emit audit rows without
/// holding a reference. Set once at boot via
/// `set_global_questdb_config()`.
static GLOBAL_QUESTDB_CONFIG: std::sync::OnceLock<tickvault_common::config::QuestDbConfig> =
    std::sync::OnceLock::new();

/// Install the global QuestDB config. Idempotent. Returns `true` on
/// first install.
pub fn set_global_questdb_config(cfg: tickvault_common::config::QuestDbConfig) -> bool {
    GLOBAL_QUESTDB_CONFIG.set(cfg).is_ok()
}

/// Read-only accessor for the global QuestDB config.
#[must_use]
pub fn global_questdb_config() -> Option<&'static tickvault_common::config::QuestDbConfig> {
    GLOBAL_QUESTDB_CONFIG.get()
}

#[cfg(test)]
mod global_qcfg_tests {
    use super::*;
    use tickvault_common::config::QuestDbConfig;

    #[test]
    fn test_set_global_questdb_config_is_idempotent() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let first = set_global_questdb_config(cfg.clone());
        let second = set_global_questdb_config(cfg);
        assert!(
            !(first && second),
            "set_global_questdb_config must be idempotent"
        );
    }

    #[test]
    fn test_global_questdb_config_accessor_returns_option() {
        let _: Option<&'static QuestDbConfig> = global_questdb_config();
    }
}

pub mod boot_probe;
// Human-readable analyst console views (`ticks_named` / `candles_named`) —
// plain QuestDB views LEFT-joining ticks/candles_1m against the
// instrument_lifecycle master. Cold-path console tooling only (O(join) at
// SELECT time, honestly O(N); zero hot-path impact). (The
// `daily_universe_fetcher` feature that once gated the lifecycle-ensure
// call inside was deleted in PR-C3, 2026-07-14 — everything here is
// unconditional now.)
pub mod console_views;
// C2 (2026-07-03): HTTP-CLIENT-01 — panic-free reqwest client construction.
// Shared OnceLock probe client for the repeating QuestDB readiness probes
// (boot + every-5s pool watchdog + every-10s SLO scheduler); typed
// HttpClientBuildError replaces the `unwrap_or_else(|_| Client::new())`
// panic fallback (Client::new() panics on TLS/resolver/fd init failure —
// a silent tokio-task death).
pub mod http_client;
// SP5 (parity plan, operator directive 2026-06-02 narrowed cross-verify): ONE
// unified post-market live-vs-backtest 1m parity audit table + writer + CSV
// (`feed` IN the DEDUP key). Merges the two former siloed modules
// (`cross_verify_1m_audit_persistence` Dhan + `groww_cross_verify_audit_persistence`
// Groww — both DELETED in SP5). Both feeds write here. See live-feed-purity.md
// rule 11 + docs/design/sp5-unified-parity-audit-design.md. The two old physical
// QuestDB tables are RETAINED on disk (SEBI 5y) but no longer written.
/// BruteX↔TickVault daily cross-verification (operator 2026-07-11): the two
/// forensic tables — one row per divergent CELL + one per-day summary row
/// with a keep-better outcome guard — written via ILP-over-HTTP (per-flush
/// server ACK). See `.claude/rules/project/brutex-crossverify-error-codes.md`.
pub mod brutex_crossverify_persistence;
/// Dual-feed scoreboard (operator 2026-07-10): one classified row per feed
/// EPISODE (disconnect / stall / process death) with the blame verdict
/// persisted — the month-end "who caused it" system-of-record.
pub mod feed_episode_audit_persistence;
pub mod feed_gap_audit_persistence;
pub mod feed_parity_1m_audit_persistence;
/// Dual-feed scoreboard (operator 2026-07-10): the per-day per-feed
/// scoreboard row + the per-instrument coverage detail table.
pub mod feed_scoreboard_persistence;
/// Groww auto-scale ladder forensic chain (§34, auto-scale PR-2 Item 8) —
/// one row per ladder transition; feeds restart rehydration.
pub mod groww_scale_audit_persistence;
/// Daily timeframe-consistency verifier (operator 2026-07-13): one row per
/// finding cell where a stored higher-TF candle disagrees with its
/// recomputed-from-1m value (TF-VERIFY-01/02).
pub mod tf_consistency_audit_persistence;
pub mod tick_conservation_audit_persistence;
pub mod ws_event_audit_persistence;
// PR-E (2026-05-26): `candle_persistence` module deleted alongside the
// `historical_candles` QuestDB table. The module had no live consumers
// after PR-D removed candle_fetcher.rs; LiveCandleWriter had been
// superseded by `shadow_candle_writer` long ago.
// PR #4 (2026-05-19): deep_depth_persistence + depth_dynamic_diff_audit
// + depth_rebalance_audit modules DELETED. Audit tables stay on disk
// per SEBI 5y retention.
pub mod disk_health_watcher;
// BP-07 (2026-07-01): PROC-01 OOM-kill monitor — reads cgroup-v2
// `memory.events` `oom_kill` vs boot baseline; mirrors disk_health_watcher.
pub mod oom_monitor;
// BP-08 (2026-07-01): RESOURCE-01/02/03 fd / RSS / spill-free early-warning
// monitors — supervised poll mirroring oom_monitor + disk_health_watcher.
pub mod resource_monitor;
// W2 PR#6 (2026-07-10, audit follow-up row 10): WAL-SUSPEND-01 per-table
// QuestDB WAL-apply suspension probe — supervised 60s wal_tables() poll
// mirroring disk_health_watcher / oom_monitor / resource_monitor. A
// suspended table (post disk-full / apply error) silently stops applying
// ILP-ACKed rows; this is the ONLY probe that sees it (boot probe +
// questdb_health check reachability/connection, not per-table apply).
pub mod wal_suspension_watcher;
// Sub-PR #10b-ε (2026-05-27): instrument_fetch_audit table contract —
// schema constants + DEDUP key + FetchOutcome enum. Feature-gated under
// `daily_universe_fetcher` per
// `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
// Groww second-feed live-tick persistence (operator lock 2026-06-19 §32).
// Isolated `groww_*` namespace; the Dhan path is untouched.
pub mod groww_persistence;
// Groww second-feed 1-minute candle persistence (operator lock §32).
pub mod groww_candle_persistence;
// (SP5) The Groww live-vs-backtest 1m parity audit module was MERGED into the
// unified `feed_parity_1m_audit_persistence` above (one table, `feed` in the
// DEDUP key). Its empty `groww_cross_verify_1m_audit` table is retained on disk.
// Instrument fetch-audit table (SEBI forensic — retained per the PR-C3
// retirement banner in daily-universe-instr-fetch-error-codes.md).
// PR-C3 (2026-07-14): compiles unconditionally — the `daily_universe_fetcher`
// feature gate was deleted with the Dhan instrument chain.
pub mod instrument_fetch_audit_persistence;
// Lifecycle table contracts (§5/§6 SEBI never-delete) — schema constants
// + DEDUP keys + LifecycleState/TransitionKind enums + DDL/Row/append
// helpers. PR-C3 (2026-07-14): compiles unconditionally (feature gate
// deleted); the live writer is the Groww shared_master_writer.
pub mod instrument_lifecycle_persistence;
// §31 item 2 (NTM Map-A, 2026-06-06): full index→constituents mapping table —
// queryable both directions. Map-only (does NOT change the subscription).
// PR-C3 (2026-07-14): compiles unconditionally (feature gate deleted).
pub mod index_constituency_persistence;
// Daily lifecycle reconciler — PURE decision logic (§5/§6/§23). PR-C3
// (2026-07-14): compiles unconditionally (feature gate deleted); consumed
// by the Groww shared_master_writer's classify_transition path.
pub mod lifecycle_reconciler;
// PR #3 (2026-05-19): `greeks_persistence` module DELETED. greeks
// pipeline retired alongside the indices-only universe. The
// option_greeks / pcr_snapshots / dhan_option_chain_raw / greeks_verification
// QuestDB tables are dropped by scripts/migrate-drop-greeks-tables.sql.
// PR-D (2026-05-26): `historical_fetch_marker` module DELETED. The
// idempotency-marker file was only consumed by candle_fetcher.rs, which
// is gone alongside the Dhan historical fetch chain.
// Candle-engine re-architecture #T1b: `materialized_views` (Engine C —
// `candles_1s` + the 9 `candles_<tf>` matviews) DELETED. The 21 plain
// `candles_<tf>` tables are created by
// `shadow_persistence::ensure_shadow_candle_tables`; legacy candle
// objects are dropped at boot by
// `shadow_persistence::drop_legacy_candle_objects`. The 25 retired
// `movers_*` matviews + `movers_1s` base table are no longer recreated
// either (the old `drop_bug3_retired_views` lived in this module).
pub mod partition_manager;
// 2026-07-13 disk-pressure remediation: partition archive→verify→drop —
// the S3-archival leg partition_manager's honest boundary documented as
// missing. Fail-closed: a partition is dropped ONLY after its S3 copy is
// row-count- and size-verified; gated on [partition_retention]
// archive_enabled (serde default false).
pub mod partition_archive;
// Cluster-C order-side observability (2026-07-14): SEBI 5y order-lifecycle
// audit — rebuild of the table deleted in #T4 (2026-05-20) on the modern
// ILP-over-HTTP template with event-in-key DEDUP (AUDIT-06).
pub mod order_audit_persistence;
// Cluster-C order-side observability (2026-07-14): daily P&L snapshot
// audit — rebuild of the Phase-0 Item-25 table deleted in #T4 (2026-05-20);
// the OnEod row is the daily heartbeat/denominator (STORAGE-GAP-03).
pub mod pnl_audit_persistence;
pub mod prev_day_ohlcv_persistence;
pub mod questdb_health;
pub mod seal_absorption;
pub mod seal_dlq;
pub mod seal_spill;
pub mod seal_writer_loop;
pub mod seal_writer_runner;
pub mod seal_writer_task;
// SP4 (groww-live-backtest-parity): the ONE feed-parameterized candle writer,
// named as an explicit `GenericCandle1mWriter(feed)` facade over the per-seal
// `ShadowCandleWriter` (the already-converged shared seal-writer chain).
pub mod generic_candle_writer;
pub mod shadow_candle_writer;
pub mod shadow_persistence;
pub mod shadow_seal_columns;
// Per-minute spot 1m REST pipeline (operator grant 2026-07-12, PR-2 — the
// SPOT half; SPOT1M-02): the `spot_1m_rest` table DDL + ILP-over-HTTP writer.
pub mod spot_1m_rest_persistence;
// Per-minute option-chain REST pipeline (operator grant 2026-07-12, PR-3 —
// the OPTION-CHAIN half; CHAIN-03): the `option_chain_1m` table DDL +
// ILP-over-HTTP writer.
pub mod option_chain_1m_persistence;
// Per-contract 1m candle leg of the Groww per-minute REST pipeline
// (operator grant 2026-07-13, PR-4 — the fill-model leg): the
// `option_contract_1m_rest` table DDL + ILP-over-HTTP writer (feed in the
// DEDUP key; retention registered in partition_manager.rs).
pub mod option_contract_1m_rest_persistence;
// Per-fetch forensics for the per-minute REST legs (operator scope addition
// 2026-07-13, Groww REST plan PR-2): the `rest_fetch_audit` table DDL +
// ILP-over-HTTP writer — one row per (target minute, symbol, feed, leg).
pub mod rest_fetch_audit_persistence;
// B6 (2026-07-03): off-thread tick ILP flush worker — keeps the blocking
// questdb TCP flush off the tick-consumer thread (TICK-FLUSH-01).
pub(crate) mod tick_flush_worker;
pub mod tick_persistence;
pub mod tick_row_builder;
pub mod tick_spill_drain;
// `valkey_cache` module DELETED in #O4 (2026-05-24) — no production caller
// remained after PR #764 migrated the dual-instance lock to SSM.
pub mod ws_frame_spill;

/// Test support: re-exports internal functions for DHAT and benchmark tests.
pub mod tick_persistence_testing {
    /// Re-export of `f32_to_f64_clean` for DHAT and Criterion benchmarks.
    pub fn f32_to_f64_clean_pub(v: f32) -> f64 {
        crate::tick_persistence::f32_to_f64_clean(v)
    }

    /// Re-export of the private `build_tick_row_seq` (the 17-column ILP row
    /// encoder on the tick hot path) for the `full_tick_processing` Criterion
    /// bench in `crates/core/benches/`. Measurement-only surface — production
    /// code keeps calling the writer methods, never this.
    ///
    /// # Errors
    /// Propagates the underlying ILP buffer error (table/column append failure).
    pub fn build_tick_row_seq_pub(
        buffer: &mut questdb::ingress::Buffer,
        tick: &tickvault_common::tick_types::ParsedTick,
        capture_seq: i64,
    ) -> anyhow::Result<()> {
        crate::tick_persistence::build_tick_row_seq(buffer, tick, capture_seq)
    }

    /// Constructs an ILP V1 buffer of the same protocol version the live
    /// `TickPersistenceWriter` uses — lets cross-crate benches build rows
    /// without naming `questdb` types (questdb-rs is not a dep of core).
    #[must_use]
    pub fn new_ilp_buffer_pub() -> questdb::ingress::Buffer {
        questdb::ingress::Buffer::new(questdb::ingress::ProtocolVersion::V1)
    }
}

/// Shared process-wide mutex for tests that touch the real global
/// `data/spill/` directory (TICK_SPILL_DIR / CANDLE_SPILL_DIR).
///
/// The recovery test
///   - `tick_persistence::tests::test_recover_skips_current_active_spill`
///
/// calls `recover_stale_spill_files()`, which drains EVERY matching
/// `{ticks,candles}-*.bin` file under the global spill directory. When
/// cargo runs these tests in parallel inside the same binary, they race
/// on filesystem state: one test drains the other test's artefacts
/// before the assertion runs, causing flaky CI failures under
/// `cargo test --workspace`.
///
/// Any test that creates files under `data/spill/` MUST acquire this
/// lock first. Holding it for the duration of the test serializes
/// access without needing a new dev-dep.
#[cfg(test)]
pub(crate) fn spill_dir_test_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::tick_persistence_testing::f32_to_f64_clean_pub;

    #[test]
    fn test_f32_to_f64_clean_pub_preserves_price() {
        assert_eq!(f32_to_f64_clean_pub(24500.5), 24500.5);
    }

    #[test]
    fn test_f32_to_f64_clean_pub_zero() {
        assert_eq!(f32_to_f64_clean_pub(0.0), 0.0);
    }

    #[test]
    fn test_f32_to_f64_clean_pub_nan() {
        assert!(f32_to_f64_clean_pub(f32::NAN).is_nan());
    }

    #[test]
    fn test_f32_to_f64_clean_pub_infinity() {
        assert_eq!(f32_to_f64_clean_pub(f32::INFINITY), f64::INFINITY);
        assert_eq!(f32_to_f64_clean_pub(f32::NEG_INFINITY), f64::NEG_INFINITY);
    }

    #[test]
    fn test_f32_to_f64_clean_pub_dhan_index_price() {
        // STORAGE-GAP-02: f32→f64 precision test via public wrapper.
        // 21004.95_f32 must produce 21004.95_f64, not 21004.94921875.
        let result = f32_to_f64_clean_pub(21004.95_f32);
        assert_eq!(result, 21004.95_f64);
    }

    #[test]
    fn test_f32_to_f64_clean_pub_negative() {
        assert_eq!(f32_to_f64_clean_pub(-100.5), -100.5);
    }

    #[test]
    fn test_new_ilp_buffer_pub_starts_empty() {
        let buffer = super::tick_persistence_testing::new_ilp_buffer_pub();
        assert_eq!(buffer.row_count(), 0);
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn test_build_tick_row_seq_pub_appends_one_row() {
        let mut buffer = super::tick_persistence_testing::new_ilp_buffer_pub();
        let tick = tickvault_common::tick_types::ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            last_traded_price: 23_146.45,
            last_trade_quantity: 0,
            exchange_timestamp: 1_770_000_000,
            received_at_nanos: 1_770_000_000_000_000_000,
            average_traded_price: 0.0,
            volume: 0,
            total_sell_quantity: 0,
            total_buy_quantity: 0,
            day_open: 23_100.0,
            day_close: 23_050.0,
            day_high: 23_200.0,
            day_low: 23_000.0,
            open_interest: 0,
            oi_day_high: 0,
            oi_day_low: 0,
            iv: f64::NAN,
            delta: f64::NAN,
            gamma: f64::NAN,
            theta: f64::NAN,
            vega: f64::NAN,
        };
        super::tick_persistence_testing::build_tick_row_seq_pub(&mut buffer, &tick, 42)
            .expect("row build must succeed for a valid tick");
        assert_eq!(buffer.row_count(), 1);
        assert!(buffer.len() > 0, "encoded ILP row must be non-empty");
    }
}
