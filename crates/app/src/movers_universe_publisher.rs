//! Publishes the Stocks movers universe for the operator Lambda (plan PR MV,
//! 2026-11-06 live incident), ON DEMAND.
//!
//! Measured live on 2026-11-06: every portal Stocks movers click took 2.6 to
//! 8.0 s, almost all of it the Lambda → SSM → box round trip; the database
//! part was 0.1 to 0.4 s. This task builds the live universe (the same one
//! `/api/stock-movers` answers every filter from) and writes it to
//! `s3://tv-<env>-live-view/movers/universe.json`, so the Lambda reads one
//! small object and selects in memory. Any problem here only makes the Lambda
//! fall back to the SSM path it used before.
//!
//! # On demand: no viewer, no database query
//!
//! The same day's stall was QuestDB's single HTTP worker starved, so this
//! task must add no standing database load. The Lambda writes a heartbeat
//! (`movers/viewer.json`, "a viewer is looking until now + 120 s") on a
//! movers call, at most once per 30 s per container. Every 5 s this task
//! reads that small object (one S3 GET, no database) and builds + publishes
//! only while it is unexpired. The first click with no viewer goes to the box
//! (slow once) and starts the 5 s cadence.
//!
//! # Load bounds
//!
//! - After the close a live read covers a fixed window (`[09:15, 15:40]` of
//!   the date), so the price rows are queried once per (IST date, window end)
//!   and re-published with a new observation time without another query; they
//!   are re-queried at most every [`CLOSED_REUSE_MAX_SECS`] so rows the
//!   evening WAL catch-up applies late still appear.
//! - No query while QuestDB reports a suspended table or a table whose apply
//!   backlog is growing, or before its first health reading
//!   ([`questdb_quiet`]): the portal uses the box path then.
//! - A failing build, put or heartbeat read backs off: 5 s × 2^n up to 120 s,
//!   reset on success ([`next_interval`]).
//!
//! # Failure handling
//!
//! - A universe whose price query failed or whose shape is invalid is NOT
//!   published (`outcome="build_failed"`): the previous object ages out and
//!   the Lambda falls back to the box, which retries the query itself.
//! - A failed or timed-out PutObject is `outcome="put_failed"`.
//! - No bucket, an invalid bucket name, or no account id (every request
//!   names the expected bucket owner): not published, counted once as
//!   `outcome="skipped"`.
//! - One `warn!` (with the reason) when a failure episode starts and one
//!   `info!` when it ends, never one per attempt and never a page.
//!
//! # Hot path
//!
//! Never on the frame drain: its own tokio task; the file reads, query-body
//! parse and JSON encoding run on the blocking pool. Cold path.
//!
//! # Cost (Assumed, not billed yet)
//!
//! One heartbeat GET every 5 s while the box runs (~5,800 a box-day, ~$0.05 a
//! month); a PutObject every 5 s only while someone views the Stocks page.

use std::time::{Duration, Instant};

use tickvault_api::handlers::stock_movers::{
    ClosedReadKey, LiveClock, build_live_universe, closed_read_key, restamped,
};
use tickvault_api::state::SharedAppState;
use tickvault_common::config::OperatorMoversConfig;
use tickvault_common::operator_movers_universe::{
    MAX_UNIVERSE_BYTES, MAX_VIEWER_BYTES, MOVERS_UNIVERSE_OBJECT_KEY, MOVERS_VIEWER_OBJECT_KEY,
    MoversUniverse, UniversePrices, ViewerHeartbeat, live_view_bucket_for, valid_account_id,
    valid_live_view_bucket_name,
};
use tracing::{info, warn};

/// Base publish cadence while a viewer is active (and the heartbeat poll).
pub const PUBLISH_INTERVAL_SECS: u64 = 5;
/// Ceiling of the failure backoff.
pub const MAX_BACKOFF_SECS: u64 = 120;
/// A closed read's price rows are reused at most this long. 60 s (was 600
/// until the 2026-11-06 combined review): each copy is re-stamped with a new
/// observation time, so this bounds how much older than its "snapshot" time
/// a shown row can be. Runs only while a viewer is active.
pub const CLOSED_REUSE_MAX_SECS: u64 = 60;
/// Bound on one universe build (lists + one query).
const BUILD_TIMEOUT_SECS: u64 = 10;
/// Bound on one PutObject.
const PUT_TIMEOUT_SECS: u64 = 4;
/// Bound on one heartbeat read.
const VIEWER_READ_TIMEOUT_SECS: u64 = 2;
/// Bound on reading this instance's account id.
const ACCOUNT_ID_TIMEOUT_SECS: u64 = 5;
/// Wait before asking for the account id again.
const ACCOUNT_ID_RETRY_SECS: u64 = 60;
/// Supervisor backoff between respawns of a dead publisher loop.
const RESPAWN_BACKOFF_SECS: u64 = 5;
/// The counter every outcome lands on.
const PUBLISH_COUNTER: &str = "tv_movers_universe_publish_total";
/// Gauge: milliseconds the last cycle took.
const PUBLISH_DURATION_GAUGE: &str = "tv_movers_universe_publish_duration_ms";

/// What one publish cycle did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    /// Built from a fresh query and published.
    Ok,
    /// Re-published from the last closed read's rows, no query.
    Reused,
    /// No active viewer: nothing built, no query.
    NoViewer,
    /// QuestDB unhealthy or unmeasured: no query.
    QuestdbBusy,
    BuildFailed,
    PutFailed,
    ViewerReadFailed,
    Skipped,
}

impl PublishOutcome {
    /// The `outcome` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Reused => "reused",
            Self::NoViewer => "no_viewer",
            Self::QuestdbBusy => "questdb_busy",
            Self::BuildFailed => "build_failed",
            Self::PutFailed => "put_failed",
            Self::ViewerReadFailed => "viewer_read_failed",
            Self::Skipped => "skipped",
        }
    }

    const fn is_success(self) -> bool {
        matches!(self, Self::Ok | Self::Reused)
    }

    const fn is_failure(self) -> bool {
        matches!(
            self,
            Self::BuildFailed | Self::PutFailed | Self::ViewerReadFailed
        )
    }
}

const ALL_OUTCOMES: [PublishOutcome; 8] = [
    PublishOutcome::Ok,
    PublishOutcome::Reused,
    PublishOutcome::NoViewer,
    PublishOutcome::QuestdbBusy,
    PublishOutcome::BuildFailed,
    PublishOutcome::PutFailed,
    PublishOutcome::ViewerReadFailed,
    PublishOutcome::Skipped,
];

/// Wait before the next cycle after `consecutive_failures` failures in a
/// row: `5 s × 2^n`, at most [`MAX_BACKOFF_SECS`]. O(1).
#[must_use]
fn next_interval(consecutive_failures: u32) -> Duration {
    let factor = 1_u64.checked_shl(consecutive_failures).unwrap_or(u64::MAX);
    Duration::from_secs(
        PUBLISH_INTERVAL_SECS
            .saturating_mul(factor)
            .min(MAX_BACKOFF_SECS),
    )
}

/// The failure count after one outcome: a failure adds one, a success or "no
/// viewer" (nothing left to retry) resets, a QuestDB hold keeps it.
#[must_use]
const fn failures_after(consecutive_failures: u32, outcome: PublishOutcome) -> u32 {
    if outcome.is_failure() {
        consecutive_failures.saturating_add(1)
    } else if matches!(outcome, PublishOutcome::QuestdbBusy) {
        consecutive_failures
    } else {
        0
    }
}

/// Whether the publisher may query QuestDB: its last health reading names
/// no suspended table and no table whose apply backlog is growing. An
/// unmeasured reading (`None`: before the first poll, or the probe went
/// blind, the starved-worker case) holds the query too. O(1).
#[must_use]
fn questdb_quiet(suspended_tables: Option<u64>, growing_tables: u32) -> bool {
    suspended_tables == Some(0) && growing_tables == 0
}

/// Where the publisher writes, or why it does not.
#[derive(Debug, PartialEq, Eq)]
enum PublisherTarget {
    Disabled,
    NoBucket,
    InvalidBucket(String),
    Bucket(String),
}

/// The bucket: an explicit configured name wins, else `tv-<env>-live-view`
/// only when the environment is explicitly set (fail closed: never a guessed
/// prod bucket from a dev box). Either must pass the S3 naming rules.
fn publisher_target(config: &OperatorMoversConfig, environment: Option<&str>) -> PublisherTarget {
    if !config.publish_enabled {
        return PublisherTarget::Disabled;
    }
    let trimmed = config.live_view_bucket.trim();
    let name = if trimmed.is_empty() {
        match environment {
            Some(env) => live_view_bucket_for(env),
            None => return PublisherTarget::NoBucket,
        }
    } else {
        trimmed.to_string()
    };
    if valid_live_view_bucket_name(&name) {
        PublisherTarget::Bucket(name)
    } else {
        PublisherTarget::InvalidBucket(name)
    }
}

/// Whether this universe may be published: a price query that answered and a
/// valid shape. O(rows).
fn publishable(universe: &MoversUniverse) -> bool {
    !matches!(
        universe.prices,
        UniversePrices::Unreachable | UniversePrices::QueryFailed
    ) && universe.valid()
}

/// The bytes to publish, or `None` when this universe must not be published
/// (failed price query, invalid shape, over the size bound). O(n).
fn encode_for_publish(universe: &MoversUniverse) -> Option<Vec<u8>> {
    if !publishable(universe) {
        return None;
    }
    serde_json::to_vec(universe)
        .ok()
        .filter(|bytes| bytes.len() <= MAX_UNIVERSE_BYTES)
}

/// The account id in an EC2 instance identity document.
fn account_id_from_identity_document(document: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(document)
        .ok()?
        .get("accountId")?
        .as_str()
        .filter(|id| valid_account_id(id))
        .map(str::to_string)
}

/// This instance's AWS account id, from the instance identity document
/// (IMDS, the same source the instance credentials come from). `None` off
/// EC2 or on any failure.
async fn instance_account_id() -> Option<String> {
    let client = aws_config::imds::Client::builder().build();
    let document = tokio::time::timeout(
        Duration::from_secs(ACCOUNT_ID_TIMEOUT_SECS),
        client.get("/latest/dynamic/instance-identity/document"),
    )
    .await
    .ok()?
    .ok()?;
    account_id_from_identity_document(document.as_ref())
}

/// Reads a response body, refusing one longer than `limit`. O(bytes).
async fn read_bounded(
    mut body: aws_sdk_s3::primitives::ByteStream,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|err| err.to_string())?;
        if chunk.len() > limit.saturating_sub(out.len()) {
            return Err(format!("object over {limit} bytes"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Where the universe goes and the heartbeat comes from. The production store
/// is S3; tests use a fake.
trait UniverseStore {
    /// The heartbeat bytes, `Ok(None)` when there is none.
    fn viewer(&self) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, String>> + Send;
    fn put(&self, body: Vec<u8>) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

struct S3UniverseStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    /// Every request names the expected owner, so a same-named bucket in
    /// another account is never written to or read from.
    owner: String,
}

impl UniverseStore for S3UniverseStore {
    async fn viewer(&self) -> Result<Option<Vec<u8>>, String> {
        use aws_sdk_s3::error::ProvideErrorMetadata;
        let out = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(MOVERS_VIEWER_OBJECT_KEY)
            .expected_bucket_owner(&self.owner)
            .send()
            .await
        {
            Ok(out) => out,
            // Without s3:ListBucket a missing object reads AccessDenied, not
            // NoSuchKey; both mean "no heartbeat" (Limitation: a missing
            // read grant also reads as no viewer, and the portal stays on
            // the box path).
            Err(err) if matches!(err.code(), Some("NoSuchKey" | "AccessDenied")) => {
                return Ok(None);
            }
            Err(err) => return Err(aws_sdk_s3::error::DisplayErrorContext(&err).to_string()),
        };
        read_bounded(out.body, MAX_VIEWER_BYTES).await.map(Some)
    }

    async fn put(&self, body: Vec<u8>) -> Result<(), String> {
        let request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(MOVERS_UNIVERSE_OBJECT_KEY)
            .expected_bucket_owner(&self.owner)
            .content_type("application/json")
            .body(aws_sdk_s3::primitives::ByteStream::from(body))
            .send();
        match tokio::time::timeout(Duration::from_secs(PUT_TIMEOUT_SECS), request).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(aws_sdk_s3::error::DisplayErrorContext(&err).to_string()),
            Err(_) => Err("PutObject timed out".to_string()),
        }
    }
}

/// What one cycle did and, for a failure, why.
#[derive(Debug, PartialEq, Eq)]
struct Cycle {
    outcome: PublishOutcome,
    reason: Option<String>,
}

impl Cycle {
    const fn of(outcome: PublishOutcome) -> Self {
        Self {
            outcome,
            reason: None,
        }
    }

    fn failed(outcome: PublishOutcome, reason: impl Into<String>) -> Self {
        Self {
            outcome,
            reason: Some(reason.into()),
        }
    }
}

/// Encode off the async workers, then put. `success` names the outcome of a
/// put that worked.
async fn publish_universe<S: UniverseStore>(
    store: &S,
    universe: MoversUniverse,
    success: PublishOutcome,
) -> Cycle {
    let encoded = tokio::task::spawn_blocking(move || encode_for_publish(&universe)).await;
    let Ok(Some(body)) = encoded else {
        return Cycle::failed(
            PublishOutcome::BuildFailed,
            "price query failed or universe invalid",
        );
    };
    match store.put(body).await {
        Ok(()) => Cycle::of(success),
        Err(reason) => Cycle::failed(PublishOutcome::PutFailed, reason),
    }
}

/// The last closed read's universe, reusable while its key holds.
struct ClosedReuse {
    key: ClosedReadKey,
    universe: MoversUniverse,
    built: tokio::time::Instant,
}

/// One cycle at `clock`: read the heartbeat; with no active viewer stop
/// (no query); else re-publish a still-valid closed read, or (QuestDB
/// permitting) build and publish. `build` runs only on that last branch.
async fn run_cycle<S, F, Fut>(
    store: &S,
    reuse: &mut Option<ClosedReuse>,
    clock: LiveClock,
    questdb_ok: bool,
    build: F,
) -> Cycle
where
    S: UniverseStore,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = MoversUniverse>,
{
    let viewer = tokio::time::timeout(
        Duration::from_secs(VIEWER_READ_TIMEOUT_SECS),
        store.viewer(),
    )
    .await;
    let bytes = match viewer {
        Err(_) => {
            return Cycle::failed(PublishOutcome::ViewerReadFailed, "heartbeat read timed out");
        }
        Ok(Err(reason)) => return Cycle::failed(PublishOutcome::ViewerReadFailed, reason),
        Ok(Ok(None)) => return Cycle::of(PublishOutcome::NoViewer),
        Ok(Ok(Some(bytes))) => bytes,
    };
    if !ViewerHeartbeat::parse(&bytes).is_some_and(|beat| beat.active_at(clock.now_epoch)) {
        return Cycle::of(PublishOutcome::NoViewer);
    }
    let key = closed_read_key(clock);
    if let (Some(key), Some(held)) = (key.as_ref(), reuse.as_ref())
        && held.key == *key
        && held.built.elapsed() < Duration::from_secs(CLOSED_REUSE_MAX_SECS)
        && let Some(universe) = restamped(&held.universe, key, clock)
    {
        return publish_universe(store, universe, PublishOutcome::Reused).await;
    }
    if !questdb_ok {
        return Cycle::of(PublishOutcome::QuestdbBusy);
    }
    let Ok(universe) = tokio::time::timeout(Duration::from_secs(BUILD_TIMEOUT_SECS), build()).await
    else {
        return Cycle::failed(PublishOutcome::BuildFailed, "universe build timed out");
    };
    *reuse = match key {
        Some(key) if publishable(&universe) => Some(ClosedReuse {
            key,
            universe: universe.clone(),
            built: tokio::time::Instant::now(),
        }),
        _ => None,
    };
    publish_universe(store, universe, PublishOutcome::Ok).await
}

/// Edge latch: one `warn!` per failure episode, one `info!` at its end.
#[derive(Default)]
struct FailureEpisode {
    failing: Option<PublishOutcome>,
    failures: u64,
}

/// What the latch decided for one outcome.
#[derive(Debug, PartialEq, Eq)]
enum EpisodeEdge {
    None,
    Started,
    Ended { failures: u64 },
}

impl FailureEpisode {
    /// A failure starts or extends the episode, a success ends it; "no
    /// viewer", a QuestDB hold or a skip leave it as it is.
    fn observe(&mut self, outcome: PublishOutcome) -> EpisodeEdge {
        if outcome.is_success() {
            let Some(_) = self.failing.take() else {
                return EpisodeEdge::None;
            };
            let failures = self.failures;
            self.failures = 0;
            return EpisodeEdge::Ended { failures };
        }
        if !outcome.is_failure() {
            return EpisodeEdge::None;
        }
        if self.failing.is_some() {
            self.failures = self.failures.saturating_add(1);
            return EpisodeEdge::None;
        }
        self.failing = Some(outcome);
        self.failures = 1;
        EpisodeEdge::Started
    }
}

async fn run_publisher(state: SharedAppState, store: S3UniverseStore) {
    let mut episode = FailureEpisode::default();
    let mut failures = 0_u32;
    let mut reuse: Option<ClosedReuse> = None;
    info!(
        bucket = %store.bucket,
        key = MOVERS_UNIVERSE_OBJECT_KEY,
        "movers universe publisher running (every 5 s while a portal viewer is active)"
    );
    loop {
        let started = Instant::now();
        let clock = LiveClock::now();
        let questdb_ok = questdb_quiet(
            tickvault_common::ingest_shed::wal_suspended_tables(),
            tickvault_common::ingest_shed::wal_apply_lag_growing(),
        );
        let cycle = run_cycle(&store, &mut reuse, clock, questdb_ok, || {
            build_live_universe(&state, clock)
        })
        .await;
        let outcome = cycle.outcome;
        metrics::counter!(PUBLISH_COUNTER, "outcome" => outcome.as_str()).increment(1);
        metrics::gauge!(PUBLISH_DURATION_GAUGE).set(started.elapsed().as_secs_f64() * 1_000.0);
        match episode.observe(outcome) {
            EpisodeEdge::Started => warn!(
                outcome = outcome.as_str(),
                reason = cycle.reason.as_deref().unwrap_or("unknown"),
                "movers universe publish failing — the portal falls back to the slower box path \
                 until it recovers"
            ),
            EpisodeEdge::Ended { failures } => {
                info!(failures, "movers universe publish recovered");
            }
            EpisodeEdge::None => {}
        }
        failures = failures_after(failures, outcome);
        tokio::time::sleep(next_interval(failures)).await;
    }
}

/// Spawns the SUPERVISED publisher when enabled and a valid bucket resolves;
/// the supervisor first reads this instance's account id (retried every
/// 60 s), then respawns a dead loop after 5 s. Registers every outcome series
/// at 0 first (the first-sample-baseline rule). Called once from `main.rs`
/// where the API state is built. `None` when nothing was spawned.
pub fn spawn(
    state: SharedAppState,
    config: &OperatorMoversConfig,
) -> Option<tokio::task::JoinHandle<()>> {
    for outcome in ALL_OUTCOMES {
        metrics::counter!(PUBLISH_COUNTER, "outcome" => outcome.as_str()).increment(0);
    }
    let skipped = || {
        metrics::counter!(PUBLISH_COUNTER, "outcome" => PublishOutcome::Skipped.as_str())
            .increment(1);
    };
    let bucket = match publisher_target(
        config,
        tickvault_storage::s3_cold::runtime_environment().as_deref(),
    ) {
        PublisherTarget::Disabled => {
            info!(
                "movers universe publisher disabled by config ([operator_movers] publish_enabled = false)"
            );
            return None;
        }
        PublisherTarget::NoBucket => {
            skipped();
            warn!(
                "movers universe publisher not started: no live-view bucket (set TV_ENVIRONMENT or \
                 [operator_movers] live_view_bucket); the portal keeps the box path"
            );
            return None;
        }
        PublisherTarget::InvalidBucket(name) => {
            skipped();
            warn!(
                bucket = %name,
                "movers universe publisher not started: the live-view bucket name breaks the S3 \
                 naming rules; the portal keeps the box path"
            );
            return None;
        }
        PublisherTarget::Bucket(bucket) => bucket,
    };
    Some(tokio::spawn(async move {
        let owner = loop {
            if let Some(owner) = instance_account_id().await {
                break owner;
            }
            skipped();
            warn!(
                "movers universe publisher waiting: this instance's account id is unreadable \
                 (every request names the expected bucket owner); retrying in 60 s"
            );
            tokio::time::sleep(Duration::from_secs(ACCOUNT_ID_RETRY_SECS)).await;
        };
        let aws = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let client = aws_sdk_s3::Client::new(&aws);
        loop {
            let store = S3UniverseStore {
                client: client.clone(),
                bucket: bucket.clone(),
                owner: owner.clone(),
            };
            let exit = tokio::spawn(run_publisher(state.clone(), store)).await;
            let reason = if exit.as_ref().is_err_and(tokio::task::JoinError::is_panic) {
                "panic"
            } else {
                "exited"
            };
            metrics::counter!("tv_movers_universe_publish_respawn_total", "reason" => reason)
                .increment(1);
            warn!(reason, "movers universe publisher exited — respawning");
            tokio::time::sleep(Duration::from_secs(RESPAWN_BACKOFF_SECS)).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tickvault_common::operator_movers_universe::{
        UNIVERSE_DEFAULT_WINDOW_SECS, UNIVERSE_FORMAT_VERSION, UNIVERSE_IST_OFFSET_SECS,
    };

    /// UTC second of an IST wall time on 2026-11-06 (a Friday).
    fn epoch_of(hh: u32, mm: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(2026, 11, 6)
            .unwrap()
            .and_hms_opt(hh, mm, 0)
            .unwrap()
            .and_utc()
            .timestamp()
            - UNIVERSE_IST_OFFSET_SECS
    }

    fn clock(hh: u32, mm: u32, market_open: bool) -> LiveClock {
        let now_epoch = epoch_of(hh, mm);
        LiveClock {
            market_open,
            now_ist: chrono::DateTime::from_timestamp(now_epoch + UNIVERSE_IST_OFFSET_SECS, 0)
                .unwrap()
                .naive_utc(),
            now_epoch,
        }
    }

    fn universe(prices: UniversePrices) -> MoversUniverse {
        MoversUniverse {
            format_version: UNIVERSE_FORMAT_VERSION,
            trading_date_ist: "2026-11-06".into(),
            observed_at_epoch_secs: 1_793_960_100,
            read_at_ist_secs: 1_793_979_900,
            market_open: true,
            window_secs: UNIVERSE_DEFAULT_WINDOW_SECS,
            prices,
            fno: None,
            membership: None,
        }
    }

    /// A closed-read universe as a build at `clock` would carry it.
    fn closed_universe(clock: LiveClock) -> MoversUniverse {
        MoversUniverse {
            trading_date_ist: clock.now_ist.format("%Y-%m-%d").to_string(),
            observed_at_epoch_secs: clock.now_epoch,
            read_at_ist_secs: clock.now_epoch + UNIVERSE_IST_OFFSET_SECS,
            market_open: false,
            window_secs: 23_100,
            ..universe(UniversePrices::Rows { rows: vec![] })
        }
    }

    struct FakeStore {
        viewer: Result<Option<Vec<u8>>, String>,
        fail_put: bool,
        puts: Mutex<Vec<Vec<u8>>>,
    }

    impl FakeStore {
        fn new(viewer: Result<Option<Vec<u8>>, String>) -> Self {
            Self {
                viewer,
                fail_put: false,
                puts: Mutex::new(Vec::new()),
            }
        }
        fn watching(now_epoch: i64) -> Self {
            Self::new(Ok(Some(
                serde_json::to_vec(&ViewerHeartbeat::written_at(now_epoch)).unwrap(),
            )))
        }
        fn put_count(&self) -> usize {
            self.puts.lock().unwrap().len()
        }
        fn last_put(&self) -> MoversUniverse {
            serde_json::from_slice(self.puts.lock().unwrap().last().unwrap()).unwrap()
        }
    }

    impl UniverseStore for FakeStore {
        async fn viewer(&self) -> Result<Option<Vec<u8>>, String> {
            self.viewer.clone()
        }
        async fn put(&self, body: Vec<u8>) -> Result<(), String> {
            self.puts.lock().unwrap().push(body);
            if self.fail_put {
                Err("AccessDenied".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn test_next_interval_doubles_from_five_seconds_to_a_two_minute_ceiling() {
        let schedule: Vec<u64> = (0..8).map(|n| next_interval(n).as_secs()).collect();
        assert_eq!(schedule, [5, 10, 20, 40, 80, 120, 120, 120]);
        assert_eq!(next_interval(63), Duration::from_secs(120));
        assert_eq!(next_interval(64), Duration::from_secs(120));
        assert_eq!(next_interval(u32::MAX), Duration::from_secs(120));
    }

    #[test]
    fn test_failures_reset_on_success_and_no_viewer_and_hold_on_questdb_busy() {
        let mut n = 0;
        for outcome in [
            PublishOutcome::BuildFailed,
            PublishOutcome::PutFailed,
            PublishOutcome::ViewerReadFailed,
        ] {
            n = failures_after(n, outcome);
        }
        assert_eq!(n, 3);
        assert_eq!(failures_after(n, PublishOutcome::QuestdbBusy), 3);
        assert_eq!(failures_after(n, PublishOutcome::Ok), 0);
        assert_eq!(failures_after(n, PublishOutcome::Reused), 0);
        assert_eq!(failures_after(n, PublishOutcome::NoViewer), 0);
        assert_eq!(
            failures_after(u32::MAX, PublishOutcome::PutFailed),
            u32::MAX
        );
    }

    #[test]
    fn test_questdb_quiet_needs_a_measured_healthy_reading() {
        assert!(questdb_quiet(Some(0), 0));
        assert!(!questdb_quiet(Some(1), 0), "a suspended table");
        assert!(!questdb_quiet(Some(0), 2), "a growing apply backlog");
        assert!(!questdb_quiet(None, 0), "unmeasured or blind probe");
    }

    #[test]
    fn test_publisher_target_validates_the_bucket_name_and_fails_closed() {
        let mut config = OperatorMoversConfig::default();
        assert!(config.publish_enabled, "on by default");
        assert_eq!(publisher_target(&config, None), PublisherTarget::NoBucket);
        assert_eq!(
            publisher_target(&config, Some("prod")),
            PublisherTarget::Bucket("tv-prod-live-view".into())
        );
        assert_eq!(
            publisher_target(&config, Some("Prod_1")),
            PublisherTarget::InvalidBucket("tv-Prod_1-live-view".into())
        );
        config.live_view_bucket = " my-bucket ".into();
        assert_eq!(
            publisher_target(&config, None),
            PublisherTarget::Bucket("my-bucket".into())
        );
        config.live_view_bucket = "my.bucket".into();
        assert_eq!(
            publisher_target(&config, Some("prod")),
            PublisherTarget::InvalidBucket("my.bucket".into())
        );
        config.live_view_bucket = "  ".into();
        assert_eq!(publisher_target(&config, None), PublisherTarget::NoBucket);
        config.publish_enabled = false;
        assert_eq!(
            publisher_target(&config, Some("prod")),
            PublisherTarget::Disabled
        );
    }

    #[test]
    fn test_account_id_is_read_from_the_identity_document_and_validated() {
        let doc = r#"{"accountId":"123456789012","region":"ap-south-1","instanceId":"i-0abc"}"#;
        assert_eq!(
            account_id_from_identity_document(doc).as_deref(),
            Some("123456789012")
        );
        for bad in [
            "",
            "{",
            "{}",
            r#"{"accountId":123456789012}"#,
            r#"{"accountId":"12345"}"#,
            r#"{"accountId":"12345678901x"}"#,
        ] {
            assert_eq!(account_id_from_identity_document(bad), None, "{bad}");
        }
    }

    #[tokio::test]
    async fn test_read_bounded_refuses_an_oversize_body_without_a_length() {
        let body = aws_sdk_s3::primitives::ByteStream::from(vec![b'x'; MAX_VIEWER_BYTES]);
        assert_eq!(
            read_bounded(body, MAX_VIEWER_BYTES).await.unwrap().len(),
            MAX_VIEWER_BYTES
        );
        let body = aws_sdk_s3::primitives::ByteStream::from(vec![b'x'; MAX_VIEWER_BYTES + 1]);
        assert!(read_bounded(body, MAX_VIEWER_BYTES).await.is_err());
    }

    #[test]
    fn test_encode_refuses_failed_queries_and_invalid_universes() {
        assert!(encode_for_publish(&universe(UniversePrices::Unreachable)).is_none());
        assert!(encode_for_publish(&universe(UniversePrices::QueryFailed)).is_none());
        let mut bad = universe(UniversePrices::Rows { rows: vec![] });
        bad.window_secs = 0;
        assert!(encode_for_publish(&bad).is_none());
        let good = universe(UniversePrices::Rows { rows: vec![] });
        let bytes = encode_for_publish(&good).unwrap();
        let back: MoversUniverse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, good);
        assert!(encode_for_publish(&universe(UniversePrices::BeforeSession)).is_some());
    }

    #[tokio::test]
    async fn test_no_active_viewer_builds_nothing_and_never_queries() {
        let now = clock(11, 0, true);
        let builds = AtomicUsize::new(0);
        let build = || async {
            builds.fetch_add(1, Ordering::Relaxed);
            universe(UniversePrices::Rows { rows: vec![] })
        };
        let expired = serde_json::to_vec(&ViewerHeartbeat {
            until_epoch_secs: now.now_epoch,
        })
        .unwrap();
        let mut reuse = None;
        for store in [
            FakeStore::new(Ok(None)),
            FakeStore::new(Ok(Some(expired))),
            FakeStore::new(Ok(Some(b"{garbage".to_vec()))),
        ] {
            let cycle = run_cycle(&store, &mut reuse, now, true, build).await;
            assert_eq!(cycle, Cycle::of(PublishOutcome::NoViewer));
            assert_eq!(store.put_count(), 0);
        }
        let failing = FakeStore::new(Err("SlowDown".into()));
        let cycle = run_cycle(&failing, &mut reuse, now, true, build).await;
        assert_eq!(
            cycle,
            Cycle::failed(PublishOutcome::ViewerReadFailed, "SlowDown")
        );
        assert_eq!(
            builds.load(Ordering::Relaxed),
            0,
            "no query without a viewer"
        );
    }

    #[tokio::test]
    async fn test_an_active_viewer_builds_and_publishes_unless_questdb_is_busy() {
        let now = clock(11, 0, true);
        let builds = AtomicUsize::new(0);
        let build = || async {
            builds.fetch_add(1, Ordering::Relaxed);
            universe(UniversePrices::Rows { rows: vec![] })
        };
        let store = FakeStore::watching(now.now_epoch);
        let mut reuse = None;
        assert_eq!(
            run_cycle(&store, &mut reuse, now, false, build).await,
            Cycle::of(PublishOutcome::QuestdbBusy)
        );
        assert_eq!(builds.load(Ordering::Relaxed), 0);
        assert_eq!(
            run_cycle(&store, &mut reuse, now, true, build).await,
            Cycle::of(PublishOutcome::Ok)
        );
        assert_eq!(builds.load(Ordering::Relaxed), 1);
        assert_eq!(store.put_count(), 1);
        assert!(reuse.is_none(), "an open read is never reused");
        // A failed query is never published, and the reason is kept.
        let cycle = run_cycle(&store, &mut reuse, now, true, || async {
            universe(UniversePrices::QueryFailed)
        })
        .await;
        assert_eq!(cycle.outcome, PublishOutcome::BuildFailed);
        assert!(cycle.reason.is_some());
        assert_eq!(store.put_count(), 1);
        // A failed put names its reason.
        let failing = FakeStore {
            fail_put: true,
            ..FakeStore::watching(now.now_epoch)
        };
        assert_eq!(
            run_cycle(&failing, &mut reuse, now, true, build).await,
            Cycle::failed(PublishOutcome::PutFailed, "AccessDenied")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_slow_build_times_out_as_a_failure() {
        let now = clock(11, 0, true);
        let store = FakeStore::watching(now.now_epoch);
        let cycle = run_cycle(&store, &mut None, now, true, || async {
            tokio::time::sleep(Duration::from_secs(BUILD_TIMEOUT_SECS + 1)).await;
            universe(UniversePrices::Rows { rows: vec![] })
        })
        .await;
        assert_eq!(cycle.outcome, PublishOutcome::BuildFailed);
        assert_eq!(store.put_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_after_the_close_one_query_serves_the_evening_until_the_key_changes() {
        let first = clock(16, 5, false);
        let builds = AtomicUsize::new(0);
        let mut reuse = None;
        let store = FakeStore::watching(first.now_epoch);
        let cycle = run_cycle(&store, &mut reuse, first, true, || async {
            builds.fetch_add(1, Ordering::Relaxed);
            closed_universe(first)
        })
        .await;
        assert_eq!(cycle, Cycle::of(PublishOutcome::Ok));
        assert!(reuse.is_some());
        // Later the same evening, QuestDB busy: still published, no query.
        let later = clock(16, 6, false);
        let store = FakeStore::watching(later.now_epoch);
        let cycle = run_cycle(&store, &mut reuse, later, false, || async {
            builds.fetch_add(1, Ordering::Relaxed);
            closed_universe(later)
        })
        .await;
        assert_eq!(cycle, Cycle::of(PublishOutcome::Reused));
        assert_eq!(builds.load(Ordering::Relaxed), 1, "rows reused");
        let published = store.last_put();
        assert_eq!(published.observed_at_epoch_secs, later.now_epoch);
        assert_eq!(
            published.read_at_ist_secs,
            later.now_epoch + UNIVERSE_IST_OFFSET_SECS
        );
        // A new date is a new read.
        let next_day = LiveClock {
            now_epoch: first.now_epoch + 86_400,
            now_ist: first.now_ist + chrono::Duration::days(1),
            market_open: false,
        };
        let store = FakeStore::watching(next_day.now_epoch);
        let cycle = run_cycle(&store, &mut reuse, next_day, true, || async {
            builds.fetch_add(1, Ordering::Relaxed);
            closed_universe(next_day)
        })
        .await;
        assert_eq!(cycle.outcome, PublishOutcome::Ok);
        assert_eq!(builds.load(Ordering::Relaxed), 2);
        // A held read older than the reuse bound is queried again.
        let store = FakeStore::watching(next_day.now_epoch);
        let cycle = run_cycle(&store, &mut reuse, next_day, true, || async {
            builds.fetch_add(1, Ordering::Relaxed);
            closed_universe(next_day)
        })
        .await;
        assert_eq!(cycle.outcome, PublishOutcome::Reused);
        assert_eq!(builds.load(Ordering::Relaxed), 2);
        tokio::time::advance(Duration::from_secs(CLOSED_REUSE_MAX_SECS)).await;
        let store = FakeStore::watching(next_day.now_epoch);
        let cycle = run_cycle(&store, &mut reuse, next_day, true, || async {
            builds.fetch_add(1, Ordering::Relaxed);
            closed_universe(next_day)
        })
        .await;
        assert_eq!(cycle.outcome, PublishOutcome::Ok);
        assert_eq!(builds.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn test_failure_episode_warns_once_and_reports_the_recovery() {
        let mut episode = FailureEpisode::default();
        assert_eq!(episode.observe(PublishOutcome::Ok), EpisodeEdge::None);
        assert_eq!(
            episode.observe(PublishOutcome::PutFailed),
            EpisodeEdge::Started
        );
        assert_eq!(
            episode.observe(PublishOutcome::BuildFailed),
            EpisodeEdge::None
        );
        assert_eq!(episode.observe(PublishOutcome::NoViewer), EpisodeEdge::None);
        assert_eq!(
            episode.observe(PublishOutcome::QuestdbBusy),
            EpisodeEdge::None
        );
        assert_eq!(
            episode.observe(PublishOutcome::ViewerReadFailed),
            EpisodeEdge::None
        );
        assert_eq!(
            episode.observe(PublishOutcome::Reused),
            EpisodeEdge::Ended { failures: 3 }
        );
        assert_eq!(episode.observe(PublishOutcome::Ok), EpisodeEdge::None);
        assert_eq!(episode.observe(PublishOutcome::Skipped), EpisodeEdge::None);
        assert_eq!(episode.observe(PublishOutcome::NoViewer), EpisodeEdge::None);
    }

    #[test]
    fn test_outcome_labels_are_stable() {
        let labels: Vec<_> = ALL_OUTCOMES.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            labels,
            [
                "ok",
                "reused",
                "no_viewer",
                "questdb_busy",
                "build_failed",
                "put_failed",
                "viewer_read_failed",
                "skipped"
            ]
        );
    }
}
