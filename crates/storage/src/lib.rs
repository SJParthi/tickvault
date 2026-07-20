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
// PR-E (2026-05-26): candle_persistence retired.
// Stage-2 dead-WS sweep (2026-07-17): `tick_persistence` (the live-tick ILP
// writer) retired with the deleted Dhan tick chain — zero production callers
// since PR-C2/C3 + the Groww live-feed retirement (2026-07-15). The `ticks`
// TABLE is retained in QuestDB (SEBI); nothing writes it anymore.
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
/// Cadence cross-fill visibility (operator 2026-07-20): one forensic row per
/// cross-fill / Groww-fallback event with the precise minute + latency —
/// the "every day, week, month, precisely at what time" system-of-record.
/// See `.claude/rules/project/cadence-error-codes.md` §4 (CADENCE-04).
pub mod cross_fill_audit_persistence;
/// Dual-feed scoreboard (operator 2026-07-10): one classified row per feed
/// EPISODE (disconnect / stall / process death) with the blame verdict
/// persisted — the month-end "who caused it" system-of-record.
pub mod feed_episode_audit_persistence;
/// Dual-feed scoreboard (operator 2026-07-10): the per-day per-feed
/// scoreboard row + the per-instrument coverage detail table.
pub mod feed_scoreboard_persistence;
/// Daily timeframe-consistency verifier (operator 2026-07-13): one row per
/// finding cell where a stored higher-TF candle disagrees with its
/// recomputed-from-1m value (TF-VERIFY-01/02).
pub mod tf_consistency_audit_persistence;
// Tick-conservation retirement (2026-07-18, dead-WS sweep follow-up):
// `tick_conservation_audit_persistence` module DELETED — the Rust WRITER
// only; the `tick_conservation_audit` QuestDB TABLE is RETAINED on disk
// per SEBI 5y retention (no DDL drop anywhere). The 15:40 IST audit's
// inputs all died with the live-WS retirements + the stage-2 tick-chain
// deletion (#1631).
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
// (SP5) The Groww live-vs-backtest 1m parity audit writer was merged into a
// unified `feed_parity_1m_audit_persistence` module, then DELETED 2026-07-15
// with the Groww live feed (parity track cancelled §33.2; zero callers).
// The `feed_parity_1m_audit` + `groww_cross_verify_1m_audit` TABLES are
// retained on disk (SEBI history; partition_manager DAY rows keep them).
// Dead-code sweep batch 1 (2026-07-18): `instrument_fetch_audit_persistence`
// DELETED — zero callers since its Phase-C feeders died (PR-C3, 2026-07-14).
// The `instrument_fetch_audit` TABLE stays (SEBI forensic — retained via the
// partition_manager sweep-list string, per the retirement banner in
// daily-universe-instr-fetch-error-codes.md).
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
// Full-fidelity order/position push-event capture (design 2026-07-18;
// ORDER-EVT-01): one row per received broker push event, BOTH feeds —
// the capture companions of the lossy 11-field BrokerOrderEvent seam.
pub mod order_update_events_persistence;
pub mod position_update_events_persistence;
// Cluster-C order-side observability (2026-07-14): daily P&L snapshot
// audit — rebuild of the Phase-0 Item-25 table deleted in #T4 (2026-05-20);
// the OnEod row is the daily heartbeat/denominator (STORAGE-GAP-03).
pub mod pnl_audit_persistence;
// Dead-code sweep batch 1 (2026-07-18): `prev_day_ohlcv_persistence` DELETED
// — zero callers since its sole feeder `prev_day_ohlcv_boot.rs` died in
// PR-C3 (2026-07-14). The `prev_day_ohlcv` TABLE stays read-only (forensic;
// partition_manager sweep string retains it).
pub mod questdb_health;
pub mod seal_absorption;
pub mod seal_dlq;
pub mod seal_spill;
pub mod seal_writer_loop;
pub mod seal_writer_runner;
pub mod seal_writer_task;
// Dead-code sweep batch 1 (2026-07-18): the SP4 feed-parameterized candle
// writer facade (`generic_candle_writer`) DELETED — the parity plan SP5–SP7
// was cancelled (groww-second-feed-scope §33.2) and the facade never gained
// a caller.
pub mod shadow_candle_writer;
pub mod shadow_persistence;
pub mod shadow_seal_columns;
// Per-minute spot 1m REST pipeline (operator grant 2026-07-12, PR-2 — the
// SPOT half; SPOT1M-02): the `spot_1m_rest` table DDL + ILP-over-HTTP writer.
pub mod spot_1m_rest_persistence;
pub mod spot_crossverify_persistence;
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
// Stage-2 dead-WS sweep (2026-07-17): `tick_flush_worker` / `tick_persistence`
// / `tick_row_builder` / `tick_spill_drain` DELETED with the dead Dhan tick
// chain (`run_tick_processor` died in PR-C2/C3; the Groww live feed retired
// 2026-07-15) — zero production callers re-verified before deletion. The
// TICK-FLUSH-01 / HOT-PATH-01/02 ErrorCode variants are RETAINED (crossref);
// the `tv_ticks_dropped_total` CloudWatch alarm becomes a dead monitor —
// terraform/EMF retirement is dashboard-PR scope. `ws_frame_spill` (the WAL
// durable floor) is KEPT — it is a separate, live surface.
// `valkey_cache` module DELETED in #O4 (2026-05-24) — no production caller
// remained after PR #764 migrated the dual-instance lock to SSM.
pub mod ws_frame_spill;

// Stage-2 dead-WS sweep (2026-07-17): the `tick_persistence_testing` shim and
// `spill_dir_test_lock` (both consumed only by the deleted tick benches/DHAT
// tests) retired with the tick chain.
