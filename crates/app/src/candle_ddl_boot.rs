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
//! - [`tickvault_storage::console_views::drop_retired_views`] — removes
//!   every console view this repository ever created (2026-09-22: the app
//!   creates NO view; `candles_10m` is a real table).
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
//! readiness → `drop_retired_views` → `drop_legacy_candle_objects` →
//! `ensure_shadow_candle_tables`) behind a bounded QUIET readiness probe (the
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

/// Run the retired-view drop + retired-object sweep + candle-table ensure DDL,
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
             candle DDL SKIPPED this boot. Consequence: the candle writer refuses \
             to send until the tables are confirmed keyed, so sealed candles go to \
             the disk spill meanwhile; the ensure re-runs in the background; the \
             spill replays after the live flushes are clean again, or at the next boot. The retired-object sweep marker \
             is not written, so the sweep retries next boot."
        );
        // Audit PR31a: the candle writer refuses to send until these tables are
        // keyed, so re-run the skipped steps in the background rather than
        // wait a day.
        drop(spawn_ensure_until_keyed(
            questdb.clone(),
            DdlTables::CandlesWithSkippedSweep,
        ));
        return;
    }

    // 2026-09-22 — the ONE-SHOT fresh-start reset (`2026-09-19-fresh-start`,
    // scope lock "THIS TIME ALONE"). FIRST, before every other DDL: it drops
    // the allowlisted tables and views, and everything below recreates them
    // on the new schema in this same boot. `build_shared_infra` awaits this
    // fn before the feed stack spawns, so no writer is live during the drops.
    // After the id is logged this is one count query per boot, forever.
    tickvault_storage::fresh_start_reset::run_fresh_start_reset_at_boot(questdb).await;

    // 2026-09-22 (SECOND) — NO VIEWS ANYWHERE. Drop every retired console view
    // BEFORE any table DDL: a surviving `candles_10m` VIEW occupies the name the
    // `candles_10m` TABLE needs, and the CREATE below would be refused on it.
    tickvault_storage::console_views::drop_retired_views(questdb).await;

    // Order is load-bearing (the pre-#1522 main.rs contract): the drop
    // sweep must free any legacy matview squatting a `candles_<tf>` name
    // BEFORE the CREATE TABLE loop.
    tickvault_storage::shadow_persistence::drop_legacy_candle_objects(questdb).await;

    // 2026-09-18 operator directive — the fifteen non-emitting candle tables are
    // dropped BEFORE the ensure, which is itself filtered to the emitted nine. The
    // two halves are one change: dropping without filtering the CREATE loop would be
    // undone in this same boot, and filtering without dropping would leave the
    // retired tables resident forever.
    tickvault_storage::shadow_persistence::drop_retired_candle_tables(questdb).await;

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
             attempt. Consequence: the candle writer refuses to send until they \
             are, so sealed candles go to the disk spill meanwhile. The ensure \
             keeps re-running in the background; the spill replays after the live \
             flushes are clean again, or at the next boot."
        );
        // Audit PR31a: the candle writer refuses to send until these tables are
        // keyed, so keep re-running the ensure for this session.
        drop(spawn_ensure_until_keyed(
            questdb.clone(),
            DdlTables::Candles,
        ));
    }

    info!(
        keyed,
        "candle DDL boot complete — retired views dropped + retired-object sweep + candle ensure attempted"
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

/// Ensure the LIVE-writer tables — `ticks`, `market_depth` and the four
/// direct `top_volume_<tf>` tables (since 2026-09-22) — with their DEDUP keys, retrying a refusal instead of running the session on
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
/// Returns `true` when every live table was ensured on some attempt. On
/// exhaustion it returns `false` after a coded `error!` naming the
/// consequence — never a panic, and never a silent continue.
// TEST-EXEMPT: network I/O orchestration — the retry bound is unit-tested below, the give-up path is exercised against an unreachable port, and the boot call site is pinned by crates/app/tests/ensure_ddl_boot_wiring_guard.rs.
pub async fn run_live_table_ddl_at_boot(questdb: &QuestDbConfig) -> bool {
    for attempt in 1..=LIVE_TABLE_DDL_ATTEMPTS {
        let ticks_ok = tickvault_storage::tick_persistence::ensure_ticks_table(questdb).await;
        let depth_ok =
            tickvault_storage::depth_persistence::ensure_market_depth_table(questdb).await;
        // The four direct `top_volume_<tf>` tables (2026-09-22; before that ONE
        // `top_volume` table viewed four ways). Their offload writer appends
        // from the first ranking sweep, so an un-ensured table would be
        // ILP-auto-created with no DEDUP key (`ts, tf, family, feed,
        // security_id, segment`) and every replayed snapshot would duplicate.
        let volume_ok =
            tickvault_storage::top_volume_rank_persistence::ensure_top_volume_tables(questdb).await;
        // 2026-09-22 (SECOND): no view re-ensure here any more. The app
        // creates NO view; the retired ones are dropped once, before any table
        // DDL, by `run_candle_ddl_at_boot`.
        if ticks_ok && depth_ok && volume_ok {
            info!(
                attempt,
                "live-table DDL boot complete — ticks (5-key DEDUP) + market_depth \
                 (depth_kind DEDUP) + top_volume_1s/3s/5s/1m (6-key DEDUP each) ensured."
            );
            return true;
        }
        if attempt < LIVE_TABLE_DDL_ATTEMPTS {
            warn!(
                attempt,
                attempts = LIVE_TABLE_DDL_ATTEMPTS,
                ticks_ok,
                depth_ok,
                volume_ok,
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
        "live-table DDL boot EXHAUSTED — ticks, market_depth and/or a top_volume_<tf> table could not be \
         ensured. Consequence: the first ILP write may auto-create the table \
         WITHOUT its DEDUP key — a replay then duplicates ticks and the two depth \
         pools overwrite each other's levels. The ensure keeps re-running in the \
         background (the boot spawns it), and top_volume rows are refused until it \
         succeeds. \
         Check QuestDB's WAL state (`wal_tables()`) before the next session."
    );
    false
}

/// First delay of the background re-run of a table ensure the boot gave up on.
///
/// Audit PR31a (2026-09-27). Before this, a boot whose ensure was refused
/// left the tables un-keyed for the whole session: nothing ran the DDL again
/// until the next boot. The candle and `top_volume_<tf>` writers now REFUSE to
/// send until their tables are keyed, so without a re-run a transient refusal
/// at 09:00 would send every candle to the disk spill until the next morning.
pub const DDL_BACKGROUND_RETRY_FIRST_SECS: u64 = 30;
/// Ceiling on the background re-run's doubling delay. Every failed ensure
/// writes its own coded errors, so a fixed 30 s would log them ~2,900 times a
/// day against a database that keeps refusing; doubling to ten minutes keeps a
/// long outage to a few dozen episodes while a short one still clears fast.
pub const DDL_BACKGROUND_RETRY_MAX_SECS: u64 = 600;

/// Which ensure a background re-run repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DdlTables {
    /// Every emitted `candles_<tf>` table ([`run_candle_ddl_at_boot`]'s ensure).
    Candles,
    /// The same, for a boot that SKIPPED the candle DDL because QuestDB never
    /// answered its readiness probe: each attempt first runs the three drop
    /// steps that boot skipped (retired views, the legacy-object sweep, the
    /// retired candle tables), because a legacy object squatting a
    /// `candles_<tf>` name would otherwise refuse the CREATE all session.
    /// Safe with the lane live: the candle writer sends nothing until the
    /// tables are keyed. The one-shot fresh-start reset is NOT re-run here —
    /// it drops live-writer tables and belongs to the boot only.
    CandlesWithSkippedSweep,
    /// `ticks`, `market_depth` and the four `top_volume_<tf>` tables
    /// ([`run_live_table_ddl_at_boot`]'s ensures).
    Live,
}

async fn ensure_tables_once(questdb: &QuestDbConfig, tables: DdlTables) -> bool {
    match tables {
        DdlTables::Candles => {
            tickvault_storage::shadow_persistence::ensure_shadow_candle_tables(questdb).await
        }
        DdlTables::CandlesWithSkippedSweep => {
            tickvault_storage::console_views::drop_retired_views(questdb).await;
            tickvault_storage::shadow_persistence::drop_legacy_candle_objects(questdb).await;
            tickvault_storage::shadow_persistence::drop_retired_candle_tables(questdb).await;
            tickvault_storage::shadow_persistence::ensure_shadow_candle_tables(questdb).await
        }
        DdlTables::Live => {
            let ticks_ok = tickvault_storage::tick_persistence::ensure_ticks_table(questdb).await;
            let depth_ok =
                tickvault_storage::depth_persistence::ensure_market_depth_table(questdb).await;
            let volume_ok =
                tickvault_storage::top_volume_rank_persistence::ensure_top_volume_tables(questdb)
                    .await;
            ticks_ok && depth_ok && volume_ok
        }
    }
}

/// Re-runs the ensure for `tables` until every statement is accepted, waiting
/// `first_delay` after the first refusal and doubling up to `max_delay`.
/// Returns the number of attempts it took.
///
/// Never gives up: the tables it ensures are ones whose writers refuse to send
/// until they are keyed, so giving up would park those writes for the session.
/// Every statement is `IF NOT EXISTS` or an idempotent `DEDUP ENABLE`, so a
/// re-run against a table the boot already created is free, and a
/// `DEDUP ENABLE` re-run is what repairs a table ILP auto-created without its
/// key.
pub async fn ensure_until_keyed(
    questdb: &QuestDbConfig,
    tables: DdlTables,
    first_delay: std::time::Duration,
    max_delay: std::time::Duration,
) -> u32 {
    let mut attempt: u32 = 0;
    let mut delay = first_delay;
    loop {
        attempt = attempt.saturating_add(1);
        if ensure_tables_once(questdb, tables).await {
            info!(
                ?tables,
                attempt, "DDL background retry: tables confirmed keyed — writes to them resume"
            );
            return attempt;
        }
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2).min(max_delay);
    }
}

/// Spawns [`ensure_until_keyed`] for `tables` on the production delays. Called
/// when the boot's bounded ensure gives up, so the session keys its tables as
/// soon as QuestDB accepts the DDL instead of on the next boot.
pub fn spawn_ensure_until_keyed(
    questdb: QuestDbConfig,
    tables: DdlTables,
) -> tokio::task::JoinHandle<u32> {
    warn!(
        ?tables,
        first_retry_in_secs = DDL_BACKGROUND_RETRY_FIRST_SECS,
        "DDL background retry started — the boot could not confirm these tables keyed"
    );
    tokio::spawn(async move {
        ensure_until_keyed(
            &questdb,
            tables,
            std::time::Duration::from_secs(DDL_BACKGROUND_RETRY_FIRST_SECS),
            std::time::Duration::from_secs(DDL_BACKGROUND_RETRY_MAX_SECS),
        )
        .await
    })
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
            // Split so the literal is not itself a declaration: the pub-fn
            // test guard scans source line by line, and a bare
            // `pub async fn <name>` inside a string reads to it as a NEW
            // untested function. Assembling it keeps the assertion identical
            // and stops a test about the scanner tripping the scanner.
            .split(concat!("pub async ", "fn run_candle_ddl_at_boot"))
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

    fn unreachable_questdb() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    /// A loopback QuestDB stand-in that accepts every statement.
    async fn spawn_accept_everything_http() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                            .await;
                    });
                }
            }
        });
        port
    }

    /// Audit PR31a: the background re-run never gives up on a database that
    /// keeps refusing — an hour of paused time spans several doubled delays
    /// and the future is still pending.
    #[tokio::test(start_paused = true)]
    async fn ensure_until_keyed_keeps_retrying_a_database_that_refuses() {
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(3_600),
            ensure_until_keyed(
                &unreachable_questdb(),
                DdlTables::Candles,
                std::time::Duration::from_secs(DDL_BACKGROUND_RETRY_FIRST_SECS),
                std::time::Duration::from_secs(DDL_BACKGROUND_RETRY_MAX_SECS),
            ),
        )
        .await;
        assert!(outcome.is_err(), "it must still be retrying after an hour");
    }

    /// Against a database that accepts every statement the re-run returns
    /// after its first attempt, for both table sets.
    #[tokio::test]
    async fn ensure_until_keyed_returns_once_every_statement_is_accepted() {
        let port = spawn_accept_everything_http().await;
        let questdb = QuestDbConfig {
            http_port: port,
            ..unreachable_questdb()
        };
        let quick = std::time::Duration::from_millis(1);
        assert_eq!(
            ensure_until_keyed(&questdb, DdlTables::Candles, quick, quick).await,
            1
        );
        assert_eq!(
            ensure_until_keyed(&questdb, DdlTables::Live, quick, quick).await,
            1
        );
        assert_eq!(
            ensure_until_keyed(&questdb, DdlTables::CandlesWithSkippedSweep, quick, quick).await,
            1
        );
    }

    /// The spawned form runs the same loop to completion on its own task.
    #[tokio::test]
    async fn spawn_ensure_until_keyed_finishes_once_the_tables_are_keyed() {
        let port = spawn_accept_everything_http().await;
        let questdb = QuestDbConfig {
            http_port: port,
            ..unreachable_questdb()
        };
        let attempts = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            spawn_ensure_until_keyed(questdb, DdlTables::Candles),
        )
        .await
        .expect("an accepting database keys the tables at once")
        .expect("the task must not panic");
        assert_eq!(attempts, 1);
    }

    /// The doubling delay starts at half a minute and stops at ten.
    #[test]
    fn ddl_background_retry_delays_are_pinned() {
        assert_eq!(DDL_BACKGROUND_RETRY_FIRST_SECS, 30);
        assert_eq!(DDL_BACKGROUND_RETRY_MAX_SECS, 600);
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
