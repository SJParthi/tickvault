//! Boot-time candle-table DDL + retired-object sweep (Track A, 2026-07-18).
//!
//! ## Why this module exists (the fresh-volume no-DEDUP bug)
//!
//! The Dhan live-WS lane deletion (PR-C2 #1522) and the Groww live-feed
//! deletion (#1581) removed the ONLY call sites of three storage DDL fns:
//!
//! - [`tickvault_storage::shadow_persistence::drop_legacy_candle_objects`]
//!   — the one-shot retired-object sweep (legacy matviews, retired tables,
//!   the movers grid), marker-gated + versioned.
//! - [`tickvault_storage::shadow_persistence::ensure_shadow_candle_tables`]
//!   — `CREATE TABLE IF NOT EXISTS candles_<tf>` + `DEDUP ENABLE UPSERT
//!   KEYS` for all 21 Engine-B candle tables.
//! - [`tickvault_storage::console_views::ensure_named_views`] — the
//!   read-only analyst console views.
//!
//! Meanwhile the REST-era bar-fold (`rest_candle_fold`) KEPT writing the
//! `candles_*` tables through the shared seal-writer chain. On a FRESH
//! QuestDB volume the first ILP write would therefore auto-create every
//! candle table WITHOUT `DEDUP ENABLE UPSERT KEYS` — a silent
//! duplicate-row window (the HTTP-CLIENT-01 runbook's documented
//! consequence class) with NOTHING left to ever repair it, because no
//! boot path ran the ensure DDL anymore.
//!
//! This module re-homes the old main.rs wiring (pre-#1522 order:
//! readiness → `drop_legacy_candle_objects` → `ensure_shadow_candle_tables`
//! → `ensure_named_views`) behind a bounded QUIET readiness probe (the
//! `index_constituency_boot` ts-pin precedent — 12 × 5s via
//! `shared_probe_client`, never the paging BOOT-01/02 `wait_for_questdb_ready`).
//!
//! ## Ordering contract (pinned by `ensure_ddl_boot_wiring_guard.rs`)
//!
//! `build_shared_infra` AWAITS this fn INLINE, BEFORE
//! `spawn_seal_writer_loop` installs the process-wide seal sender — so the
//! candle DDL (with DEDUP) lands before the first fold seal can reach ILP.
//! Worst-case inline cost is the 60s probe bound; on probe exhaustion the
//! DDL is SKIPPED LOUDLY (attempting 45+ DROP/CREATE HTTP calls against a
//! down QuestDB would serialize ~10s timeouts each into a multi-minute
//! boot stall for zero benefit) and the next boot retries.

use tickvault_common::config::QuestDbConfig;
use tracing::{error, info, warn};

/// Quiet-probe attempts before giving up on QuestDB readiness (× backoff
/// = 60s worst case — the ts-pin migration precedent).
pub const CANDLE_DDL_READINESS_ATTEMPTS: u32 = 12;
/// Seconds between readiness probe attempts.
pub const CANDLE_DDL_READINESS_BACKOFF_SECS: u64 = 5;

/// Run the retired-object sweep + candle-table ensure DDL + named views,
/// gated on a bounded quiet readiness probe.
///
/// Degrade-safe, never blocks boot indefinitely:
/// - probe client build failure → proceed to the DDL anyway (the DDL fns
///   build their own clients and degrade per HTTP-CLIENT-01);
/// - probe exhausted (QuestDB down) → SKIP the DDL loudly and return —
///   the first ILP write may then auto-create `candles_*` WITHOUT DEDUP
///   UPSERT KEYS (duplicate-row window until a later boot's ensure
///   succeeds); the drop-sweep marker is not written, so the sweep also
///   retries next boot.
// TEST-EXEMPT: network I/O orchestration — the call order + boot wiring are pinned by crates/app/tests/ensure_ddl_boot_wiring_guard.rs; the probe-bound constants are unit-tested below; the underlying DDL fns carry their own unit tests in tickvault-storage.
pub async fn run_candle_ddl_at_boot(questdb: &QuestDbConfig) {
    let probe_url = format!(
        "http://{}:{}/exec?query=SELECT%201",
        questdb.host, questdb.http_port
    );
    let mut ready = false;
    match tickvault_storage::http_client::shared_probe_client() {
        Ok(client) => {
            for attempt in 1..=CANDLE_DDL_READINESS_ATTEMPTS {
                match client.get(&probe_url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        ready = true;
                        break;
                    }
                    Ok(_) | Err(_) => {
                        if attempt < CANDLE_DDL_READINESS_ATTEMPTS {
                            tokio::time::sleep(std::time::Duration::from_secs(
                                CANDLE_DDL_READINESS_BACKOFF_SECS,
                            ))
                            .await;
                        }
                    }
                }
            }
        }
        Err(err) => {
            // HTTP-CLIENT-01 class — the shared probe client could not be
            // built; the DDL fns below build their own clients (and emit
            // their own coded degrades), so proceed without a probe.
            warn!(
                %err,
                "candle DDL boot: probe client build failed — proceeding to the DDL \
                 without a readiness probe"
            );
            ready = true; // unknown, not "down" — attempt the DDL
        }
    }
    if !ready {
        error!(
            attempts = CANDLE_DDL_READINESS_ATTEMPTS,
            backoff_secs = CANDLE_DDL_READINESS_BACKOFF_SECS,
            "candle DDL boot: QuestDB not ready within the quiet probe bound — \
             candle DDL SKIPPED this boot. Consequence: if the candle tables do \
             not exist yet, the first ILP write may auto-create them WITHOUT \
             DEDUP UPSERT KEYS (duplicate-row window until a later boot's ensure \
             succeeds). The retired-object sweep marker is not written, so the \
             sweep also retries next boot."
        );
        return;
    }

    // Order is load-bearing (the pre-#1522 main.rs contract): the drop
    // sweep must free any legacy matview squatting a `candles_<tf>` name
    // BEFORE the CREATE TABLE loop, and the named views validate their
    // column references against the ensured tables.
    tickvault_storage::shadow_persistence::drop_legacy_candle_objects(questdb).await;
    tickvault_storage::shadow_persistence::ensure_shadow_candle_tables(questdb).await;
    tickvault_storage::console_views::ensure_named_views(questdb).await;
    info!("candle DDL boot complete — retired-object sweep + candle ensure + named views done");
}

/// Attempts at the `ticks` + `market_depth` DDL before the boot gives up.
///
/// Six attempts five seconds apart is thirty seconds — the same order as the
/// candle probe's bound, and deliberately NOT unbounded: a QuestDB that will
/// not accept a `CREATE TABLE` in half a minute is the boot-probe escalation
/// codes' problem, and holding the lane behind it would turn a schema gap into
/// a dark session.
pub const LIVE_TABLE_DDL_ATTEMPTS: u32 = 6;
/// Seconds between `ticks` / `market_depth` DDL attempts.
pub const LIVE_TABLE_DDL_BACKOFF_SECS: u64 = 5;

/// Ensure the two LIVE-writer tables — `ticks` and `market_depth` — with
/// their DEDUP keys, retrying a refusal instead of running the session on
/// whatever ILP auto-creates.
///
/// # Why a retry, when the candle DDL above gets one probe and one shot
///
/// Before 2026-09-08 `main.rs` awaited each ensure ONCE and discarded the
/// verdict. The readiness probe in [`run_candle_ddl_at_boot`] answers
/// `SELECT 1`, which a QuestDB still replaying its own WAL, or holding a
/// table in a suspended state, answers happily — and then refuses the DDL.
/// Every refusal was logged and counted, and the boot continued: the first
/// ILP row auto-created `ticks` without `capture_seq` and `feed` in its key
/// (a replay then DUPLICATES instead of collapsing) and `market_depth` without
/// `depth_kind` (the two depth pools then OVERWRITE each other's levels).
/// Both are silent — rows land, counts look right — and both last the
/// session, because nothing re-ran the DDL.
///
/// The ensure fns now return whether every statement was accepted, and this
/// loop re-runs BOTH until they are or the bound is spent. Re-running an
/// already-accepted table is free: every statement is `IF NOT EXISTS` or an
/// idempotent `DEDUP ENABLE`.
///
/// Returns `true` when both tables were ensured on some attempt. On
/// exhaustion it returns `false` after a coded `error!` naming the
/// consequence — never a panic, and never a silent continue.
// TEST-EXEMPT: network I/O orchestration — the retry bound is unit-tested below, the give-up path is exercised against an unreachable port, and the boot call site is pinned by crates/app/tests/ensure_ddl_boot_wiring_guard.rs.
pub async fn run_live_table_ddl_at_boot(questdb: &QuestDbConfig) -> bool {
    for attempt in 1..=LIVE_TABLE_DDL_ATTEMPTS {
        let ticks_ok = tickvault_storage::tick_persistence::ensure_ticks_table(questdb).await;
        let depth_ok =
            tickvault_storage::depth_persistence::ensure_market_depth_table(questdb).await;
        if ticks_ok && depth_ok {
            info!(
                attempt,
                "live-table DDL boot complete — ticks (5-key DEDUP) + market_depth \
                 (depth_kind DEDUP) ensured"
            );
            return true;
        }
        if attempt < LIVE_TABLE_DDL_ATTEMPTS {
            warn!(
                attempt,
                attempts = LIVE_TABLE_DDL_ATTEMPTS,
                ticks_ok,
                depth_ok,
                backoff_secs = LIVE_TABLE_DDL_BACKOFF_SECS,
                "live-table DDL refused — retrying so the session does not run on an \
                 ILP-auto-created table with the DEDUP key missing"
            );
            tokio::time::sleep(std::time::Duration::from_secs(LIVE_TABLE_DDL_BACKOFF_SECS)).await;
        }
    }
    error!(
        code = tickvault_common::error_code::ErrorCode::HotPath02WriterQueueDrop.code_str(),
        attempts = LIVE_TABLE_DDL_ATTEMPTS,
        backoff_secs = LIVE_TABLE_DDL_BACKOFF_SECS,
        "live-table DDL boot EXHAUSTED — ticks and/or market_depth could not be \
         ensured. Consequence: the first ILP write may auto-create the table \
         WITHOUT its DEDUP key — a replay then duplicates ticks and the two depth \
         pools overwrite each other's levels — until a later boot's ensure succeeds. \
         Check QuestDB's WAL state (`wal_tables()`) before the next session."
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The retry bound stays inside the same half-minute class as the
    /// candle probe: attempts × backoff must never grow into a boot stall.
    #[test]
    fn live_table_ddl_retry_bound_is_thirty_seconds() {
        assert_eq!(
            u64::from(LIVE_TABLE_DDL_ATTEMPTS) * LIVE_TABLE_DDL_BACKOFF_SECS,
            30
        );
        assert!(LIVE_TABLE_DDL_ATTEMPTS >= 2, "one attempt is not a retry");
    }

    /// Against a port nothing listens on, every attempt fails, the loop
    /// spends its bound, and the verdict is `false` — never a hang and
    /// never a panic. Paused tokio time makes the five-second backoffs free.
    #[tokio::test(start_paused = true)]
    async fn run_live_table_ddl_at_boot_gives_up_after_the_bound_and_reports_false() {
        let questdb = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        assert!(!run_live_table_ddl_at_boot(&questdb).await);
    }

    /// The inline await in `build_shared_infra` is bounded by the quiet
    /// probe: attempts × backoff must stay ≤ 60s so a down QuestDB can
    /// never stall boot past the ts-pin precedent's bound.
    #[test]
    fn test_candle_ddl_probe_bound_is_sixty_seconds() {
        assert_eq!(
            u64::from(CANDLE_DDL_READINESS_ATTEMPTS) * CANDLE_DDL_READINESS_BACKOFF_SECS,
            60
        );
    }
}
