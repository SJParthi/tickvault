//! Public-endpoint DoS guard — GCRA rate limiter for the two
//! unauthenticated QuestDB-backed endpoints (2026-07-09 audit directive).
//!
//! `GET /api/stats` runs 5 sequential QuestDB HTTP round-trips per hit and
//! `GET /api/quote/{security_id}` runs 1-2; both are exposed on the public
//! internet via the Tailscale funnel on port 3001. This module caps them
//! behind one shared GCRA cell (the exact house pattern of
//! `crates/trading/src/oms/rate_limiter.rs`) applied as an axum
//! `route_layer` on a sub-router containing ONLY those two routes — the
//! layer structurally cannot wrap `/api/feeds`, `/api/feeds/health`,
//! `/health`, the debug routes, or the bearer-gated POST (all locked
//! contracts per `websocket-connection-scope-lock.md` 2026-07-04 banner).
//!
//! # Why GLOBAL, not per-IP (honest design choice)
//! The funnel terminates TLS upstream, so client IPs reach the app only via
//! spoofable forwarded headers — a per-IP limiter keyed on attacker-chosen
//! headers is bypassed by header rotation. A global cap protects QuestDB
//! regardless of attacker IP-spread. Trade-off: an attacker can 429-starve
//! the operator's dashboard of THESE TWO endpoints, but the DB stays
//! protected and every other route stays unlimited. Budget: the
//! `/dashboard` page polls `/api/stats` every ~5s per tab (0.2 req/s) and
//! MCP calls are occasional — 5 req/s sustained + burst 10 is >10x headroom.
//!
//! # Log-amplification defence (audit Rule class)
//! A limited request logs at `debug!` ONLY — never `error!`/`warn!` — so an
//! attacker cannot use the limiter itself to flood errors.jsonl/CloudWatch.
//! The operator signal is the `tv_api_rate_limited_total{endpoint}` counter
//! (static label values only).

use std::num::NonZeroU32;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use governor::{Quota, RateLimiter, clock::DefaultClock, state::InMemoryState, state::NotKeyed};
use tracing::debug;

/// Sustained request budget (per second) shared by the two public
/// QuestDB-backed endpoints. See module docs for the budget analysis.
pub const PUBLIC_API_RATE_PER_SEC: u32 = 5;

/// Burst capacity on top of the sustained rate (GCRA max cells).
pub const PUBLIC_API_BURST: u32 = 10;

/// `Retry-After` value on a 429 — one second (the GCRA cell replenishes
/// every 1/[`PUBLIC_API_RATE_PER_SEC`] s, so 1s always restores budget).
const RETRY_AFTER_SECS: &str = "1";

// RESIDUAL CLOSED (2026-07-09 follow-up, coordinator-approved): the #1458
// review flagged `GET /api/board/data` (3 QuestDB queries/hit, /board polls
// every ~3s) as the remaining unlimited public DB-backed route. It now rides
// the SAME shared cell + a 2s TTL cache slot. Budget math (verified against
// board_page.rs/dashboard_page.rs): one /board tab (1 req/3s = 0.33/s) + one
// /dashboard tab (1 req/5s = 0.2/s) = ~0.53 req/s vs the 5 req/s sustained
// budget (~11%); even 5 tabs of each = ~2.7 req/s — no legit starvation. All
// three public DB-backed routes are now covered; the /board + /dashboard
// HTML shells stay unlimited (static, no DB).

/// GCRA-based limiter shared by `/api/stats` + `/api/quote/{security_id}`
/// + `/api/board/data` (2026-07-09 follow-up).
///
/// Cold path — one atomic GCRA check per public API request; never on the
/// tick hot path.
pub struct PublicEndpointLimiter {
    limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
}

impl PublicEndpointLimiter {
    /// Creates the limiter with the locked constants
    /// ([`PUBLIC_API_RATE_PER_SEC`] sustained, [`PUBLIC_API_BURST`] burst).
    pub fn new() -> Self {
        Self::with_limits(PUBLIC_API_RATE_PER_SEC, PUBLIC_API_BURST)
    }

    /// Creates a limiter with explicit limits (unit tests use tiny budgets).
    ///
    /// Panic-free by construction: a zero argument is clamped to 1 (the
    /// strictest possible budget — fail-CLOSED, never fail-open). Both
    /// production call sites pass the non-zero compile-time constants
    /// [`PUBLIC_API_RATE_PER_SEC`] / [`PUBLIC_API_BURST`], pinned non-zero
    /// by `test_locked_constants_are_nonzero`.
    pub fn with_limits(rate_per_sec: u32, burst: u32) -> Self {
        let rate = NonZeroU32::new(rate_per_sec).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(burst).unwrap_or(NonZeroU32::MIN);

        let quota = Quota::per_second(rate).allow_burst(burst);
        Self {
            limiter: RateLimiter::direct(quota),
        }
    }

    /// Returns `true` when the request is within budget.
    pub fn check(&self) -> bool {
        self.limiter.check().is_ok()
    }
}

impl Default for PublicEndpointLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps a request path to a STATIC endpoint label for the
/// `tv_api_rate_limited_total{endpoint}` counter. Total function —
/// attacker-controlled paths can only ever produce one of four fixed
/// `&'static str` values (no per-request label allocation, no cardinality
/// explosion).
fn endpoint_label(path: &str) -> &'static str {
    if path.starts_with("/api/stats") {
        "stats"
    } else if path.starts_with("/api/quote") {
        "quote"
    } else if path.starts_with("/api/board") {
        "board"
    } else {
        "other"
    }
}

/// axum middleware — applied via `route_layer(from_fn_with_state(...))` on
/// the sub-router holding ONLY the two public QuestDB-backed routes.
///
/// Ordering: rate limit runs BEFORE the handler-level TTL caches — the
/// limiter protects CPU + the metric pipeline too, and limit-first is the
/// stricter envelope (documented in the plan's Design §3).
pub async fn public_rate_limit(
    State(limiter): State<Arc<PublicEndpointLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    if limiter.check() {
        return next.run(request).await;
    }

    let endpoint = endpoint_label(request.uri().path());
    metrics::counter!("tv_api_rate_limited_total", "endpoint" => endpoint).increment(1);
    // debug! ONLY — see module docs (log-amplification defence). The
    // counter is the operator signal; a per-request error!/warn! would let
    // an attacker flood errors.jsonl/CloudWatch through the limiter itself.
    debug!(endpoint, "public API rate limit hit — returning 429");

    (
        StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::RETRY_AFTER, RETRY_AFTER_SECS)],
        axum::Json(serde_json::json!({
            "error": "rate limited — retry shortly"
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_with_limits_allows_within_burst() {
        let limiter = PublicEndpointLimiter::with_limits(1, 3);
        assert!(limiter.check());
        assert!(limiter.check());
        assert!(limiter.check());
    }

    #[test]
    fn test_with_limits_denies_after_burst() {
        let limiter = PublicEndpointLimiter::with_limits(1, 3);
        assert!(limiter.check());
        assert!(limiter.check());
        assert!(limiter.check());
        assert!(!limiter.check(), "4th rapid request must be denied");
    }

    #[test]
    fn test_limiter_default_uses_locked_constants() {
        // The locked budget: burst of PUBLIC_API_BURST rapid checks passes,
        // the next one is denied.
        let limiter = PublicEndpointLimiter::new();
        for i in 0..PUBLIC_API_BURST {
            assert!(limiter.check(), "request {i} within burst must pass");
        }
        assert!(!limiter.check(), "burst+1 must be denied");
    }

    #[test]
    fn test_default_impl_matches_new() {
        let limiter = PublicEndpointLimiter::default();
        assert!(limiter.check());
    }

    #[test]
    fn test_endpoint_label_is_static_and_total() {
        assert_eq!(endpoint_label("/api/stats"), "stats");
        assert_eq!(endpoint_label("/api/quote/12345"), "quote");
        // 2026-07-09 follow-up: the board snapshot route is limited too.
        assert_eq!(endpoint_label("/api/board/data"), "board");
        // Attacker-shaped garbage maps to the fixed "other" label — never
        // an attacker-controlled label value.
        assert_eq!(endpoint_label("/api/quote"), "quote");
        assert_eq!(endpoint_label("/evil\ninjected"), "other");
        assert_eq!(endpoint_label(""), "other");
    }

    /// Direct middleware test: within budget the request reaches the
    /// handler; past the budget the middleware short-circuits with the
    /// static 429 + Retry-After shape (never touching the handler).
    #[tokio::test]
    async fn test_public_rate_limit_middleware_429_shape() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        async fn mock_handler() -> &'static str {
            "ok"
        }

        let limiter = Arc::new(PublicEndpointLimiter::with_limits(1, 2));
        let app = Router::new()
            .route("/api/stats", get(mock_handler))
            .route_layer(axum::middleware::from_fn_with_state(
                limiter,
                public_rate_limit,
            ));

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/stats")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let limited = app
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            limited
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1"),
        );
    }

    #[test]
    fn test_locked_constants_are_nonzero() {
        // with_limits panics on 0 — pin the constants themselves non-zero
        // so the APPROVED expect() stays unreachable.
        assert!(PUBLIC_API_RATE_PER_SEC > 0);
        assert!(PUBLIC_API_BURST > 0);
    }
}
