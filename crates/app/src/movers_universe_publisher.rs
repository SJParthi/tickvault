//! Publishes the Stocks movers universe for the operator Lambda (plan PR MV,
//! 2026-11-06 live incident).
//!
//! Measured live on 2026-11-06: every portal Stocks movers click took 2.6 to
//! 8.0 s, almost all of it the Lambda → SSM → box round trip; the database
//! part was 0.1 to 0.4 s. This task builds the live universe (the same one
//! `/api/stock-movers` answers every filter from) and writes it to
//! `s3://tv-<env>-live-view/movers/universe.json`, so the Lambda reads one
//! small object and selects in memory. Any problem here only makes the Lambda
//! fall back to the SSM path it used before.
//!
//! # Cadence
//!
//! Every [`PUBLISH_INTERVAL_SESSION_SECS`] (5 s) inside the trading session
//! (09:00–15:40 IST on a trading day, `market_hours::is_trading_session_now`),
//! every [`PUBLISH_INTERVAL_IDLE_SECS`] (60 s) otherwise.
//!
//! # Failure handling
//!
//! - A universe whose price query failed or whose shape is invalid is NOT
//!   published (`outcome="build_failed"`): the previous object ages out and
//!   the Lambda falls back to the box, which retries the query itself.
//! - A failed or timed-out PutObject is `outcome="put_failed"`.
//! - No bucket (no `TV_ENVIRONMENT` / `ENVIRONMENT` and no configured name):
//!   the task is not spawned, counted once as `outcome="skipped"`.
//! - One `warn!` when a failure episode starts and one `info!` when it ends,
//!   never one per attempt and never a page: the portal still works through
//!   the fallback.
//!
//! # Hot path
//!
//! Never on the frame drain: its own tokio task; the JSON encoding (about a
//! quarter MiB) runs on the blocking pool. Cold path: per cycle one bounded
//! QuestDB query, O(n) encoding, one PutObject.
//!
//! # Cost (Assumed, not billed yet)
//!
//! About 4,900 PutObject a box-day (6 h 40 min × 720 + the rest × 60), about
//! 108,000 a month, ~$0.55 at the ap-south-1 PUT price; storage is one small
//! object with a 2-day expiry.

use std::time::{Duration, Instant};

use tickvault_api::state::SharedAppState;
use tickvault_common::config::OperatorMoversConfig;
use tickvault_common::operator_movers_universe::{
    MAX_UNIVERSE_BYTES, MOVERS_UNIVERSE_OBJECT_KEY, MoversUniverse, UniversePrices,
    live_view_bucket_for,
};
use tracing::{info, warn};

/// Publish cadence inside the trading session.
pub const PUBLISH_INTERVAL_SESSION_SECS: u64 = 5;
/// Publish cadence outside the trading session.
pub const PUBLISH_INTERVAL_IDLE_SECS: u64 = 60;
/// Bound on one universe build (lists + one query).
const BUILD_TIMEOUT_SECS: u64 = 10;
/// Bound on one PutObject.
const PUT_TIMEOUT_SECS: u64 = 4;
/// Supervisor backoff between respawns of a dead publisher loop.
const RESPAWN_BACKOFF_SECS: u64 = 5;
/// The counter every outcome lands on.
const PUBLISH_COUNTER: &str = "tv_movers_universe_publish_total";
/// Gauge: milliseconds the last cycle took (build + encode + put).
const PUBLISH_DURATION_GAUGE: &str = "tv_movers_universe_publish_duration_ms";

/// What one publish cycle did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    Ok,
    BuildFailed,
    PutFailed,
    Skipped,
}

impl PublishOutcome {
    /// The `outcome` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::BuildFailed => "build_failed",
            Self::PutFailed => "put_failed",
            Self::Skipped => "skipped",
        }
    }
}

const ALL_OUTCOMES: [PublishOutcome; 4] = [
    PublishOutcome::Ok,
    PublishOutcome::BuildFailed,
    PublishOutcome::PutFailed,
    PublishOutcome::Skipped,
];

/// Cadence for the session state.
#[must_use]
fn publish_interval(session_open: bool) -> Duration {
    Duration::from_secs(if session_open {
        PUBLISH_INTERVAL_SESSION_SECS
    } else {
        PUBLISH_INTERVAL_IDLE_SECS
    })
}

/// The bucket: an explicit configured name wins, else `tv-<env>-live-view`
/// only when the environment is explicitly set (fail closed: never a guessed
/// prod bucket from a dev box).
#[must_use]
fn resolve_live_view_bucket(configured: &str, environment: Option<&str>) -> Option<String> {
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }
    environment.map(live_view_bucket_for)
}

/// The bytes to publish, or `None` when this universe must not be published
/// (failed price query, invalid shape, over the size bound). O(n).
fn encode_for_publish(universe: &MoversUniverse) -> Option<Vec<u8>> {
    if matches!(
        universe.prices,
        UniversePrices::Unreachable | UniversePrices::QueryFailed
    ) || !universe.valid()
    {
        return None;
    }
    serde_json::to_vec(universe)
        .ok()
        .filter(|bytes| bytes.len() <= MAX_UNIVERSE_BYTES)
}

/// Where the universe goes. The production store is S3; tests use a fake.
trait UniverseStore {
    fn put(&self, body: Vec<u8>) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

struct S3UniverseStore {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl UniverseStore for S3UniverseStore {
    async fn put(&self, body: Vec<u8>) -> Result<(), String> {
        let request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(MOVERS_UNIVERSE_OBJECT_KEY)
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

/// One cycle after the build: encode off the async workers, then put.
async fn publish_universe<S: UniverseStore>(store: &S, universe: MoversUniverse) -> PublishOutcome {
    let encoded = tokio::task::spawn_blocking(move || encode_for_publish(&universe)).await;
    let Ok(Some(body)) = encoded else {
        return PublishOutcome::BuildFailed;
    };
    match store.put(body).await {
        Ok(()) => PublishOutcome::Ok,
        Err(_) => PublishOutcome::PutFailed,
    }
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
    fn observe(&mut self, outcome: PublishOutcome) -> EpisodeEdge {
        match (outcome, self.failing) {
            (PublishOutcome::Ok, None) => EpisodeEdge::None,
            (PublishOutcome::Ok, Some(_)) => {
                let failures = self.failures;
                self.failing = None;
                self.failures = 0;
                EpisodeEdge::Ended { failures }
            }
            (_, None) => {
                self.failing = Some(outcome);
                self.failures = 1;
                EpisodeEdge::Started
            }
            (_, Some(_)) => {
                self.failures = self.failures.saturating_add(1);
                EpisodeEdge::None
            }
        }
    }
}

async fn run_publisher(state: SharedAppState, store: S3UniverseStore) {
    let mut episode = FailureEpisode::default();
    info!(
        bucket = %store.bucket,
        key = MOVERS_UNIVERSE_OBJECT_KEY,
        "movers universe publisher running (every 5 s in session, 60 s otherwise)"
    );
    loop {
        let started = Instant::now();
        let outcome = match tokio::time::timeout(
            Duration::from_secs(BUILD_TIMEOUT_SECS),
            tickvault_api::handlers::stock_movers::build_live_universe(&state),
        )
        .await
        {
            Ok(universe) => publish_universe(&store, universe).await,
            Err(_) => PublishOutcome::BuildFailed,
        };
        metrics::counter!(PUBLISH_COUNTER, "outcome" => outcome.as_str()).increment(1);
        metrics::gauge!(PUBLISH_DURATION_GAUGE).set(started.elapsed().as_secs_f64() * 1_000.0);
        match episode.observe(outcome) {
            EpisodeEdge::Started => warn!(
                outcome = outcome.as_str(),
                "movers universe publish failing — the portal falls back to the slower box path \
                 until it recovers"
            ),
            EpisodeEdge::Ended { failures } => {
                info!(failures, "movers universe publish recovered");
            }
            EpisodeEdge::None => {}
        }
        tokio::time::sleep(publish_interval(
            tickvault_common::market_hours::is_trading_session_now(),
        ))
        .await;
    }
}

/// Whether and where the publisher runs.
#[derive(Debug, PartialEq, Eq)]
enum PublisherTarget {
    Disabled,
    NoBucket,
    Bucket(String),
}

fn publisher_target(config: &OperatorMoversConfig, environment: Option<&str>) -> PublisherTarget {
    if !config.publish_enabled {
        return PublisherTarget::Disabled;
    }
    resolve_live_view_bucket(&config.live_view_bucket, environment)
        .map_or(PublisherTarget::NoBucket, PublisherTarget::Bucket)
}

/// Spawns the SUPERVISED publisher when enabled and a bucket resolves; the
/// supervisor respawns a dead loop after 5 s. Registers every outcome series
/// at 0 first (the first-sample-baseline rule). Called once from `main.rs`
/// where the API state is built. `None` when nothing was spawned.
pub fn spawn(
    state: SharedAppState,
    config: &OperatorMoversConfig,
) -> Option<tokio::task::JoinHandle<()>> {
    for outcome in ALL_OUTCOMES {
        metrics::counter!(PUBLISH_COUNTER, "outcome" => outcome.as_str()).increment(0);
    }
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
            metrics::counter!(PUBLISH_COUNTER, "outcome" => PublishOutcome::Skipped.as_str())
                .increment(1);
            warn!(
                "movers universe publisher not started: no live-view bucket (set TV_ENVIRONMENT or \
                 [operator_movers] live_view_bucket); the portal keeps the box path"
            );
            return None;
        }
        PublisherTarget::Bucket(bucket) => bucket,
    };
    Some(tokio::spawn(async move {
        let aws = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let client = aws_sdk_s3::Client::new(&aws);
        loop {
            let store = S3UniverseStore {
                client: client.clone(),
                bucket: bucket.clone(),
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
    use tickvault_common::operator_movers_universe::{
        UNIVERSE_DEFAULT_WINDOW_SECS, UNIVERSE_FORMAT_VERSION,
    };

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

    struct FakeStore {
        fail: bool,
        puts: std::sync::Mutex<Vec<Vec<u8>>>,
    }
    impl UniverseStore for FakeStore {
        async fn put(&self, body: Vec<u8>) -> Result<(), String> {
            self.puts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(body);
            if self.fail {
                Err("denied".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn test_publish_interval_is_five_seconds_in_session_and_a_minute_otherwise() {
        assert_eq!(publish_interval(true), Duration::from_secs(5));
        assert_eq!(publish_interval(false), Duration::from_secs(60));
    }

    #[test]
    fn test_resolve_live_view_bucket_fails_closed_without_an_environment() {
        assert_eq!(resolve_live_view_bucket("", None), None);
        assert_eq!(resolve_live_view_bucket("  ", None), None);
        assert_eq!(
            resolve_live_view_bucket("", Some("prod")).as_deref(),
            Some("tv-prod-live-view")
        );
        assert_eq!(
            resolve_live_view_bucket(" my-bucket ", Some("prod")).as_deref(),
            Some("my-bucket")
        );
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
    async fn test_publish_universe_counts_put_failures_and_never_puts_a_failed_build() {
        let ok = FakeStore {
            fail: false,
            puts: std::sync::Mutex::new(Vec::new()),
        };
        assert_eq!(
            publish_universe(&ok, universe(UniversePrices::Rows { rows: vec![] })).await,
            PublishOutcome::Ok
        );
        assert_eq!(
            publish_universe(&ok, universe(UniversePrices::QueryFailed)).await,
            PublishOutcome::BuildFailed
        );
        assert_eq!(
            ok.puts.lock().unwrap().len(),
            1,
            "a failed build is never put"
        );
        let failing = FakeStore {
            fail: true,
            puts: std::sync::Mutex::new(Vec::new()),
        };
        assert_eq!(
            publish_universe(&failing, universe(UniversePrices::BeforeSession)).await,
            PublishOutcome::PutFailed
        );
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
        assert_eq!(
            episode.observe(PublishOutcome::PutFailed),
            EpisodeEdge::None
        );
        assert_eq!(
            episode.observe(PublishOutcome::Ok),
            EpisodeEdge::Ended { failures: 3 }
        );
        assert_eq!(episode.observe(PublishOutcome::Ok), EpisodeEdge::None);
        assert_eq!(
            episode.observe(PublishOutcome::Skipped),
            EpisodeEdge::Started
        );
    }

    #[test]
    fn test_outcome_labels_are_stable() {
        let labels: Vec<_> = ALL_OUTCOMES.iter().map(|o| o.as_str()).collect();
        assert_eq!(labels, ["ok", "build_failed", "put_failed", "skipped"]);
    }

    #[test]
    fn test_spawn_target_honours_the_switch_and_fails_closed_without_a_bucket() {
        let mut config = OperatorMoversConfig::default();
        assert!(config.publish_enabled, "on by default");
        assert_eq!(publisher_target(&config, None), PublisherTarget::NoBucket);
        assert_eq!(
            publisher_target(&config, Some("prod")),
            PublisherTarget::Bucket("tv-prod-live-view".into())
        );
        config.publish_enabled = false;
        assert_eq!(
            publisher_target(&config, Some("prod")),
            PublisherTarget::Disabled
        );
    }
}
