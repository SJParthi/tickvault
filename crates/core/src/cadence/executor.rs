//! The LOCKED cadence executor seam (design §8).
//!
//! This PR ships NO REST caller: the real broker executors land later in
//! `crates/app` (`dhan_cadence_executor.rs` per the design's governance
//! split — that PR also owns the dated rule-file re-authorization for the
//! cadence decision-path fires). Until then BOTH lanes run the
//! [`DryRunLoggingExecutor`], which logs each fire and returns
//! `Err(Empty)` — dry-run decisions honest-skip; prices are NEVER
//! synthesized (judge ruling, design §0 "DryRun executor behavior").
//!
//! Executor CONTRACT (locked with the Dhan capture session): ONE bounded
//! request per call; the deadline rides in the request; NO self-scheduling
//! or re-poll ladder inside impls; a broker 429 is typed
//! [`CadenceFetchError::RateLimited`] so it arms the failure ladder and is
//! never blind-retried.

use std::future::Future;

use tickvault_common::feed::Feed;
use tracing::info;

use crate::pipeline::chain_snapshot::ChainUnderlying;

/// One bounded option-chain fetch request (single underlying).
#[derive(Clone, Copy, Debug)]
pub struct ChainFetchRequest {
    /// The broker lane issuing the fetch.
    pub feed: Feed,
    /// Which chain underlying (NIFTY / BANKNIFTY / SENSEX).
    pub underlying: ChainUnderlying,
    /// The decided minute's MINUTE-OPEN, IST seconds-of-day (T − 60 for
    /// the cycle closing at boundary T).
    pub cycle_minute_ist: u32,
    /// Absolute deadline for this ONE request, epoch milliseconds — the
    /// impl must give up (return [`CadenceFetchError::Timeout`]) at/before
    /// it; the runner never waits past the lane cutoff for it.
    pub deadline_epoch_ms: i64,
}

/// The 4 cadence spot targets. INDIA VIX is spot-only (no chain) and
/// ADVISORY in the data-complete predicate (design §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpotTarget {
    /// NIFTY (Dhan IDX_I 13 / Groww `NSE-NIFTY`).
    Nifty,
    /// BANKNIFTY (Dhan IDX_I 25 / Groww `NSE-BANKNIFTY`).
    BankNifty,
    /// SENSEX (Dhan IDX_I 51 / Groww `BSE-SENSEX`).
    Sensex,
    /// INDIA VIX (Dhan IDX_I 21) — spot-only, advisory.
    IndiaVix,
}

impl SpotTarget {
    /// The single-source list, in the locked slot order (:03.0 / :03.4 /
    /// :03.8 / :04.2 on the Dhan lane).
    pub const ALL: &'static [SpotTarget] = &[
        SpotTarget::Nifty,
        SpotTarget::BankNifty,
        SpotTarget::Sensex,
        SpotTarget::IndiaVix,
    ];

    /// The cross-feed plain symbol (matches
    /// [`ChainUnderlying::as_str`] for the 3 chain underlyings).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nifty => "NIFTY",
            Self::BankNifty => "BANKNIFTY",
            Self::Sensex => "SENSEX",
            Self::IndiaVix => "INDIA VIX",
        }
    }

    /// The matching chain underlying — `None` for INDIA VIX (spot-only).
    #[must_use]
    pub const fn chain_underlying(self) -> Option<ChainUnderlying> {
        match self {
            Self::Nifty => Some(ChainUnderlying::Nifty),
            Self::BankNifty => Some(ChainUnderlying::Banknifty),
            Self::Sensex => Some(ChainUnderlying::Sensex),
            Self::IndiaVix => None,
        }
    }
}

/// One bounded spot fetch request (single target — the judge's
/// single-target ruling for BOTH brokers; Groww's parallelism is the
/// runner issuing 7 concurrent executor calls, never batching inside
/// an impl).
#[derive(Clone, Copy, Debug)]
pub struct SpotFetchRequest {
    /// The broker lane issuing the fetch.
    pub feed: Feed,
    /// Which spot target.
    pub target: SpotTarget,
    /// The decided minute's MINUTE-OPEN, IST seconds-of-day.
    pub cycle_minute_ist: u32,
    /// Absolute deadline for this ONE request, epoch milliseconds.
    pub deadline_epoch_ms: i64,
}

/// Typed fetch failure — the ladder + retry policy dispatch on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CadenceFetchError {
    /// The broker rate-limited us (HTTP 429) DESPITE the gates — arms the
    /// ladder (strongest signal), never blind-retried, and additionally a
    /// gate-bug signal (design §4).
    RateLimited {
        /// The broker's Retry-After hint, ms, when present.
        retry_after_ms: Option<i64>,
    },
    /// The bounded request hit its deadline.
    Timeout,
    /// A 2xx response carrying NO usable data (the Dhan 200-with-zero-
    /// candles class) — does NOT arm the ladder (Assumed, design §0).
    Empty,
    /// An auth-class reject (dead token / entitlement) — the token
    /// machinery owns recovery; does not arm the ladder.
    Auth,
    /// Transport-class failure (connect/TLS/DNS/reset) OR an HTTP 5xx —
    /// arms the ladder.
    Transport,
    /// A 2xx response that failed to parse (schema drift) — does not arm
    /// the ladder.
    Malformed,
}

impl CadenceFetchError {
    /// Stable label for counters/logs (static — no allocation).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "rate_limited",
            Self::Timeout => "timeout",
            Self::Empty => "empty",
            Self::Auth => "auth",
            Self::Transport => "transport",
            Self::Malformed => "malformed",
        }
    }
}

/// A successful spot fetch.
#[derive(Clone, Copy, Debug)]
pub struct SpotSnapshot {
    /// The fetched price, rupees (guarded to paise downstream — the
    /// assembly consumes `tickvault_common::moneyness`, never raw floats).
    pub price: f64,
    /// The minute the price belongs to (MINUTE-OPEN, IST seconds-of-day).
    pub source_minute_ist: u32,
    /// Receipt instant, epoch milliseconds.
    pub received_at_epoch_ms: i64,
}

/// A successful chain fetch. The chain ROWS are published by the impl to
/// the `pipeline::chain_snapshot` registry (the capture legs own
/// classify + publish — the cadence drives WHEN they fire, never the
/// publish); the runner only needs the embedded underlying spot (the
/// third rung of the spot-provenance order) and the publish confirmation.
#[derive(Clone, Copy, Debug)]
pub struct ChainFetchOk {
    /// The chain response's own embedded underlying spot (Dhan
    /// `data.last_price` / Groww `underlying_ltp`), rupees — genuinely
    /// optional on the Groww side (absence tracked, design §5).
    pub underlying_spot: Option<f64>,
    /// TRUE when the impl published the classified snapshot to the
    /// `chain_snapshot` registry for this (feed, underlying).
    pub published_to_registry: bool,
}

/// The LOCKED broker seam — native RPITIT async (no `async_trait` dep;
/// `+ Send` so the runner can drive calls from spawned tasks). Not
/// dyn-safe: the runner is generic `<D: CadenceExecutor, G:
/// CadenceExecutor>`.
pub trait CadenceExecutor: Send + Sync {
    /// Fetch ONE underlying's option chain (bounded by the request's
    /// deadline; no internal retries/re-polls).
    fn fetch_chain(
        &self,
        req: ChainFetchRequest,
    ) -> impl Future<Output = Result<ChainFetchOk, CadenceFetchError>> + Send;

    /// Fetch ONE spot target (bounded by the request's deadline; no
    /// internal retries/re-polls).
    fn fetch_spot(
        &self,
        req: SpotFetchRequest,
    ) -> impl Future<Output = Result<SpotSnapshot, CadenceFetchError>> + Send;
}

/// The dry-run executor both lanes ship with in this PR: logs the fire
/// (proving the cadence timing end-to-end in the logs) and returns
/// `Err(Empty)` — decisions honest-skip; prices are NEVER synthesized.
#[derive(Clone, Copy, Debug, Default)]
pub struct DryRunLoggingExecutor;

impl CadenceExecutor for DryRunLoggingExecutor {
    async fn fetch_chain(&self, req: ChainFetchRequest) -> Result<ChainFetchOk, CadenceFetchError> {
        info!(
            feed = req.feed.as_str(),
            underlying = req.underlying.as_str(),
            cycle_minute_ist = req.cycle_minute_ist,
            deadline_epoch_ms = req.deadline_epoch_ms,
            "cadence dry-run: chain fire (no REST call — returning Empty)"
        );
        Err(CadenceFetchError::Empty)
    }

    async fn fetch_spot(&self, req: SpotFetchRequest) -> Result<SpotSnapshot, CadenceFetchError> {
        info!(
            feed = req.feed.as_str(),
            target = req.target.as_str(),
            cycle_minute_ist = req.cycle_minute_ist,
            deadline_epoch_ms = req.deadline_epoch_ms,
            "cadence dry-run: spot fire (no REST call — returning Empty)"
        );
        Err(CadenceFetchError::Empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cadence_spot_target_chain_underlying_mapping() {
        // The 3 chain underlyings map 1:1; VIX is spot-only.
        assert_eq!(
            SpotTarget::Nifty.chain_underlying(),
            Some(ChainUnderlying::Nifty)
        );
        assert_eq!(
            SpotTarget::BankNifty.chain_underlying(),
            Some(ChainUnderlying::Banknifty)
        );
        assert_eq!(
            SpotTarget::Sensex.chain_underlying(),
            Some(ChainUnderlying::Sensex)
        );
        assert_eq!(SpotTarget::IndiaVix.chain_underlying(), None);
        assert_eq!(SpotTarget::ALL.len(), 4);
        // The 3 chain-backed targets' labels join with ChainUnderlying's.
        for t in SpotTarget::ALL {
            if let Some(u) = t.chain_underlying() {
                assert_eq!(t.as_str(), u.as_str());
            }
        }
    }

    #[test]
    fn test_cadence_fetch_error_as_str_stable() {
        assert_eq!(
            CadenceFetchError::RateLimited {
                retry_after_ms: None
            }
            .as_str(),
            "rate_limited"
        );
        assert_eq!(CadenceFetchError::Timeout.as_str(), "timeout");
        assert_eq!(CadenceFetchError::Empty.as_str(), "empty");
        assert_eq!(CadenceFetchError::Auth.as_str(), "auth");
        assert_eq!(CadenceFetchError::Transport.as_str(), "transport");
        assert_eq!(CadenceFetchError::Malformed.as_str(), "malformed");
    }

    #[tokio::test]
    async fn test_cadence_dry_run_executor_never_synthesizes_fetch_chain_fetch_spot() {
        // The dry-run executor logs and returns Empty — NEVER a price.
        let ex = DryRunLoggingExecutor;
        let chain = ex
            .fetch_chain(ChainFetchRequest {
                feed: Feed::Dhan,
                underlying: ChainUnderlying::Nifty,
                cycle_minute_ist: 33_300,
                deadline_epoch_ms: 1,
            })
            .await;
        assert_eq!(chain.unwrap_err(), CadenceFetchError::Empty);
        let spot = ex
            .fetch_spot(SpotFetchRequest {
                feed: Feed::Groww,
                target: SpotTarget::IndiaVix,
                cycle_minute_ist: 33_300,
                deadline_epoch_ms: 1,
            })
            .await;
        assert!(matches!(spot, Err(CadenceFetchError::Empty)));
    }
}
