//! Process-global `index_constituency` ts-pin boot migration — the KEPT half
//! of the former NTM Map-B boot module.
//!
//! PR-C3 (2026-07-14, operator retirement directive 2026-07-13 —
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A/§B): the
//! LANE-MAPPING half of this module (`persist_index_constituency_mapping` +
//! `build_index_constituency_rows` + `OwnedConstituencyRow`) DIED with the
//! Dhan instrument-download chain — its inputs (the niftyindices
//! ConstituencyDownloader consumed via the Dhan master join, the Dhan
//! CsvDownloader/parser, `resolve_constituents`) are all deleted. The
//! `index_constituency` TABLE is retained (SEBI never-delete; the Groww
//! shared-master writer keeps writing `feed='groww'` rows).
//!
//! What SURVIVES here — UN-GATED (the `daily_universe_fetcher` feature died
//! with the chain) and LOAD-BEARING for the Groww feed:
//! [`run_index_constituency_ts_pin_migration_at_boot`], the process-global
//! boot-prefix task that runs the marker-gated, exactly-once
//! `TRUNCATE TABLE index_constituency` ts-pin migration and marks the
//! `index_constituency_migration_gate` the Groww shared-master writer
//! AWAITS before its `feed='groww'` append (F13/F14 hardening, 2026-07-05 —
//! `groww-shared-master-error-codes.md` §0). Its main.rs spawn stays
//! ordered BEFORE the Groww activation watcher (the F14 source-order
//! ratchet below).

use tracing::{info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_storage::index_constituency_persistence::{
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

// PR-C3 (2026-07-14): the lane-mapping half (`OwnedConstituencyRow` +
// `build_index_constituency_rows` + `persist_index_constituency_mapping`)
// was deleted here — see the module doc. The defense-in-depth migration
// call it carried is superseded by the boot-prefix task above being the
// SOLE (and always-spawned) migration site.

#[cfg(test)]
mod tests {
    use super::*;

    // PR-C3 (2026-07-14): the mapping-half unit tests
    // (`build_index_constituency_rows_*` / `build_rows_*`) and the FIX-13a
    // in-module ordering ratchet (`ratchet_migration_runs_before_csv_fetch`)
    // retired with the deleted lane-mapping half — the boot-prefix task is
    // now the sole migration site, so there is no in-module CSV fetch to
    // order against. The F14 main.rs source-order ratchet + the F13 bound
    // pin SURVIVE below (the MigrationGate contract is unchanged).

    /// F14 ratchet (source-scan on main.rs — the now-retired `wal_reinject`
    /// ordering-ratchet pattern; that module was deleted 2026-07-17,
    /// evidence-audit Fix PR C),
    /// retuned 2026-07-15 (Groww live-feed deletion): the activation watcher
    /// died with the live lane; the surviving gate consumer is the
    /// `[groww_universe]` daily rider (persist_groww_instruments), so the
    /// process-global boot-prefix ts-pin migration task MUST still be spawned
    /// BEFORE the rider spawn in `main()` source order — the Groww writer's
    /// bounded 120s migration-gate wait depends on it.
    #[test]
    fn ratchet_ts_pin_migration_spawns_before_groww_universe_rider() {
        let src = include_str!("main.rs");
        let migration = src
            .find("run_index_constituency_ts_pin_migration_at_boot(")
            .expect("the boot-prefix ts-pin migration spawn must exist in main.rs");
        let rider = src
            .find("spawn_groww_universe_rider(")
            .expect("the [groww_universe] daily rider spawn must exist in main.rs");
        assert!(
            migration < rider,
            "F13/F14 regression: the process-global ts-pin migration task must be \
             spawned BEFORE the [groww_universe] daily rider in main.rs source order"
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
