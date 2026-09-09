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

/// Attempts at the candle-table ensure before the boot gives up.
///
/// Six attempts five seconds apart is thirty seconds — deliberately the SAME
/// bound as [`LIVE_TABLE_DDL_ATTEMPTS`], because it answers the same question
/// about the same database. Not unbounded: a QuestDB that will not accept a
/// `CREATE TABLE` in half a minute is the boot-probe escalation codes' problem,
/// and holding the boot behind it would turn a schema gap into a dark session.
pub const CANDLE_ENSURE_ATTEMPTS: u32 = 6;
/// Seconds between candle-ensure attempts.
pub const CANDLE_ENSURE_BACKOFF_SECS: u64 = 5;
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

    // RETRY the candle ensure, 2026-09-09.
    //
    // Until today this awaited the ensure ONCE and discarded the verdict —
    // the same shape `run_live_table_ddl_at_boot` was given a six-attempt loop
    // for on 2026-09-08, and this path is the one where a refusal costs most.
    // `drop_legacy_candle_objects` runs immediately above and DROPS
    // `candles_1s` on any boot whose sweep version moved, so between that drop
    // and a refused CREATE the table simply does not exist: the seal writer's
    // first ILP row auto-creates it with NO dedup key, and every replayed or
    // re-flushed bar duplicates into it for the life of the table, silently.
    //
    // The readiness probe above cannot prevent this — it answers `SELECT 1`,
    // which a QuestDB still replaying its own WAL answers happily and then
    // refuses the DDL.
    //
    // Re-running an accepted table is free: every statement is `IF NOT EXISTS`
    // or an idempotent `DEDUP ENABLE`.
    let mut keyed = false;
    for attempt in 1..=CANDLE_ENSURE_ATTEMPTS {
        if tickvault_storage::shadow_persistence::ensure_shadow_candle_tables(questdb).await {
            keyed = true;
            if attempt > 1 {
                info!(attempt, "candle DDL boot: candle tables keyed on retry");
            }
            break;
        }
        if attempt < CANDLE_ENSURE_ATTEMPTS {
            warn!(
                attempt,
                next_in_secs = CANDLE_ENSURE_BACKOFF_SECS,
                "candle DDL boot: a candle table's DEDUP-bearing statement was \
                 refused — retrying rather than running the session on whatever \
                 the first ILP row auto-creates"
            );
            tokio::time::sleep(std::time::Duration::from_secs(CANDLE_ENSURE_BACKOFF_SECS)).await;
        }
    }
    if !keyed {
        error!(
            code = tickvault_common::error_code::ErrorCode::HotPath02WriterQueueDrop.code_str(),
            attempts = CANDLE_ENSURE_ATTEMPTS,
            "candle DDL boot: candle tables NOT confirmed keyed after every \
             attempt. Consequence: any candle table that does not exist will be \
             auto-created by the first ILP row WITHOUT its DEDUP UPSERT KEYS, \
             and every replayed bar then duplicates into it instead of \
             collapsing — silently, for the life of that table. The next boot \
             re-runs this ensure; a table already created key-less is NOT \
             repaired by it."
        );
    }

    tickvault_storage::console_views::ensure_named_views(questdb).await;
    info!(
        keyed,
        "candle DDL boot complete — retired-object sweep + candle ensure attempted + named views"
    );
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

/// Ensure the three LIVE-writer tables — `ticks`, `market_depth` and
/// `top_volume_rank` — with
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
/// loop re-runs ALL of them until they are or the bound is spent. Re-running an
/// already-accepted table is free: every statement is `IF NOT EXISTS` or an
/// idempotent `DEDUP ENABLE`.
///
/// Returns `true` when all three tables were ensured on some attempt. On
/// exhaustion it returns `false` after a coded `error!` naming the
/// consequence — never a panic, and never a silent continue.
// TEST-EXEMPT: network I/O orchestration — the retry bound is unit-tested below, the give-up path is exercised against an unreachable port, and the boot call site is pinned by crates/app/tests/ensure_ddl_boot_wiring_guard.rs.
pub async fn run_live_table_ddl_at_boot(questdb: &QuestDbConfig) -> bool {
    for attempt in 1..=LIVE_TABLE_DDL_ATTEMPTS {
        let ticks_ok = tickvault_storage::tick_persistence::ensure_ticks_table(questdb).await;
        let depth_ok =
            tickvault_storage::depth_persistence::ensure_market_depth_table(questdb).await;
        // top_volume_rank — the 1 s / 5 s volume-ranking snapshot table
        // (2026-09-06). Its offload writer appends every second from the
        // first ranking sweep; until 2026-09-08 NOTHING ensured the table,
        // so on a fresh volume the first ILP row would have auto-created it
        // with no DEDUP key (`ts, tf, family, feed, security_id, segment`)
        // and every replayed or re-flushed snapshot would have duplicated.
        let rank_ok =
            tickvault_storage::top_volume_rank_persistence::ensure_top_volume_rank_table(questdb)
                .await;
        if ticks_ok && depth_ok && rank_ok {
            // Re-ensure the named views now that `top_volume_rank` EXISTS.
            //
            // `run_candle_ddl_at_boot` already ran `ensure_named_views`, and it
            // runs FIRST — before this function creates `top_volume_rank`. So on
            // any boot where that table was absent (a fresh volume, or the
            // 2026-09-08 nuke) the two per-cadence views
            // `top_volume_rank_1s` / `top_volume_rank_5s` referenced a table
            // that did not yet exist, their DDL warn-failed, and nothing retried
            // it in-boot: the base table then filled all session while
            // `SELECT * FROM top_volume_rank_1s` answered "table does not
            // exist". The operator's own words for this table were *"only using
            // db i can see this"*, so the views failing is the failure mode he
            // would actually meet.
            //
            // Re-ensuring rather than MOVING the first call is deliberate: the
            // candle ordering above is load-bearing (the legacy-matview drop
            // sweep must precede the CREATE TABLE loop, and the candle views
            // validate against those tables), and every statement here is
            // `CREATE OR REPLACE`, so a second pass on an already-correct view
            // is free. Additive beats re-ordering on a boot path.
            tickvault_storage::console_views::ensure_named_views(questdb).await;
            info!(
                attempt,
                "live-table DDL boot complete — ticks (5-key DEDUP) + market_depth \
                 (depth_kind DEDUP) + top_volume_rank (6-key DEDUP) ensured. The named \
                 views were then RE-ATTEMPTED against the now-existing rank table; \
                 that call reports its own outcome per view and returns nothing, so \
                 this line claims the attempt, never its success."
            );
            return true;
        }
        if attempt < LIVE_TABLE_DDL_ATTEMPTS {
            warn!(
                attempt,
                attempts = LIVE_TABLE_DDL_ATTEMPTS,
                ticks_ok,
                depth_ok,
                rank_ok,
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
        "live-table DDL boot EXHAUSTED — ticks, market_depth and/or top_volume_rank could not be \
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

    /// The candle ensure gets the SAME retry bound as the live-table ensure,
    /// because it answers the same question about the same database — and
    /// because the boot DROPS `candles_1s` immediately before it, so a single
    /// refused attempt leaves the seal writer to auto-create that table with
    /// no dedup key for the life of the table.
    ///
    /// Asserted as an EQUALITY against its sibling rather than as its own
    /// literal: two bounds for one database is two things to keep true, and a
    /// literal here would let them drift apart silently.
    #[test]
    fn candle_ensure_retry_bound_matches_the_live_table_one() {
        assert_eq!(CANDLE_ENSURE_ATTEMPTS, LIVE_TABLE_DDL_ATTEMPTS);
        assert_eq!(CANDLE_ENSURE_BACKOFF_SECS, LIVE_TABLE_DDL_BACKOFF_SECS);
        assert!(CANDLE_ENSURE_ATTEMPTS >= 2, "one attempt is not a retry");
    }

    /// The candle boot must RETRY the ensure and must not treat a refusal as
    /// success.
    ///
    /// A source pin rather than a behavioural one, because the ensure needs a
    /// live QuestDB. It asserts the three things that make the retry real: the
    /// loop exists over the bound, the verdict is consulted, and the exhausted
    /// path is a coded `error!` rather than a silent continue.
    #[test]
    fn the_candle_boot_retries_the_ensure_and_reports_exhaustion() {
        let src = include_str!("candle_ddl_boot.rs");
        let body = src
            .split("pub async fn run_candle_ddl_at_boot")
            .nth(1)
            .expect("the candle boot fn must exist");
        let body = body
            .split("\n/// Attempts at the `ticks`")
            .next()
            .expect("the candle boot fn must end before the live-table section");
        assert!(
            body.contains("for attempt in 1..=CANDLE_ENSURE_ATTEMPTS"),
            "the ensure must be retried over the bound, not awaited once"
        );
        assert!(
            body.contains("ensure_shadow_candle_tables(questdb).await {"),
            "the ensure's verdict must be CONSULTED — awaiting and discarding \
             it is what let a refused CREATE run the whole session"
        );
        assert!(
            body.contains("if !keyed {"),
            "exhaustion must have its own arm"
        );
        assert!(
            body.contains("HotPath02WriterQueueDrop"),
            "exhaustion must be CODED, so a metric filter can read it — the \
             same code the live-table exhaustion arm uses for the same \
             consequence"
        );
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
