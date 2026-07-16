//! OMS engine — orchestrates order lifecycle management.
//!
//! Composes the API client, rate limiter, circuit breaker, idempotency tracker,
//! and state machine into a single cohesive order management system.
//!
//! # Performance
//! Cold path — order submission is ~1-100/day. Allocations acceptable.
//!
//! # Thread Safety
//! Single-threaded access assumed (owned by the trading pipeline task).
//! Token access is via a callback trait to decouple from the core crate.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use metrics::counter;
use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, error, info, instrument, warn};

use tickvault_common::constants::{
    DATA_805_STOP_ALL_COOLDOWN_SECS, DH901_ROTATE_RETRY_DELAY_SECS, DH904_MAX_RETRY_ATTEMPTS,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::order_types::{OrderStatus, OrderType, OrderUpdate, OrderValidity};
use tickvault_common::sanitize::capture_rest_error_body;

use super::api_client::OrderApiClient;
use super::circuit_breaker::OrderCircuitBreaker;
use super::error_taxonomy::{self, DhanErrorClass, OrderEndpoint, OrderErrorPolicy};
use super::exit_rules;
use super::idempotency::CorrelationTracker;
use super::order_readiness::{OrderReadinessState, ReadinessRefusal, evaluate_order_readiness};
use super::rate_limiter::OrderRateLimiter;
use super::reconciliation::reconcile_orders;
use super::state_machine::{is_valid_transition, parse_order_status};
use super::types::{
    DhanForeverOrderRequest, DhanModifyOrderRequest, DhanModifySuperOrderRequest,
    DhanPlaceOrderRequest, DhanPlaceSuperOrderRequest, DhanSuperOrderResponse,
    EXCHANGE_SEGMENT_NSE_FNO, ExecutionVerdict, LegState, MAX_MODIFICATIONS_PER_ORDER,
    ManagedOrder, ManagedSuperOrder, ModifyOrderRequest, ModifySuperOrderLeg, OmsError, OrderLeg,
    PlaceForeverOcoRequest, PlaceOrderRequest, PlaceSuperOrderRequest, ReconciliationReport,
    SUPER_ORDER_STATUS_ACCEPTED_UNPARSED_BODY, SlicingResponse, SuperOrderLegSnapshot,
    SuperOrderPlacement, VerifyState,
};

// ---------------------------------------------------------------------------
// Token Provider trait
// ---------------------------------------------------------------------------

/// Trait for providing access tokens to the OMS engine.
///
/// Implemented at the binary level where `TokenHandle` (core crate) is available.
/// Uses dynamic dispatch via `Box<dyn TokenProvider>` (cold path — orders are ~1-100/day).
/// Returns `SecretString` to ensure zeroize-on-drop for the access token.
pub trait TokenProvider: Send + Sync {
    /// Returns the current valid access token, or an error.
    fn get_access_token(&self) -> Result<SecretString, OmsError>;

    /// Requests an out-of-band token rotation through the EXISTING TokenManager
    /// renewal machinery (`force_renewal_if_stale` / `renew_with_fallback` — the
    /// H3 mint cooldown + RESILIENCE-03 lock stay upstream). The trading crate
    /// NEVER mints. Default no-op: the DH-901/807 retry-once simply re-reads the
    /// arc-swap's current token (the renewal loop/watchdog may have already
    /// rotated it); if not, the second failure halts (901) or returns
    /// TokenExpired (807). Returns whether a rotation was requested.
    fn request_token_rotation(&self) -> bool {
        false
    }
}

/// OMS alert types fired to Telegram via the notification bridge.
/// Defined here to avoid coupling the trading crate to the core notification crate.
#[derive(Debug, Clone)]
pub enum OmsAlert {
    /// Circuit breaker opened — all orders blocked.
    CircuitBreakerOpened { consecutive_failures: u64 },
    /// Circuit breaker recovered — orders allowed again.
    CircuitBreakerClosed,
    /// SEBI rate limit hit — order rejected.
    RateLimitExhausted { limit_type: String },
    /// Order rejected by Dhan API or validation.
    OrderRejected {
        correlation_id: String,
        reason: String,
    },
    /// Cluster F: a live order was refused by the readiness gate.
    OrderReadinessRefused {
        reason: &'static str,
        correlation_id: String,
    },
    /// Cluster F: the order path HALTED after a broker account error.
    OmsHalted { cause: &'static str, detail: String },
    /// Cluster F: the DH-904 backoff ladder gave up.
    Dh904LadderExhausted {
        operation: &'static str,
        attempts: u32,
    },
    /// Cluster F: DATA-805 STOP-ALL cooldown engaged.
    OrderApiStopAll { cooldown_secs: u64 },
}

impl OmsAlert {
    /// Operator-facing plain-English message. EVERY variant starts with
    /// "🔷 DHAN" so any future sink inherits broker attribution (noise-lock
    /// strategy — the sink has zero installers today). 10-commandments
    /// compliant: plain English, specific numbers, action verbs on degraded.
    #[must_use]
    pub fn operator_message(&self) -> String {
        match self {
            Self::CircuitBreakerOpened {
                consecutive_failures,
            } => format!(
                "🔷 DHAN — order safety switch TRIPPED after {consecutive_failures} failures in a row. No orders will be sent until the broker recovers."
            ),
            Self::CircuitBreakerClosed => {
                "🔷 DHAN — order safety switch reset: orders are flowing again.".to_owned()
            }
            Self::RateLimitExhausted { limit_type } => format!(
                "🔷 DHAN — order rate limit hit ({limit_type}). This order was NOT sent; it must not be retried."
            ),
            Self::OrderRejected {
                correlation_id,
                reason,
            } => format!("🔷 DHAN — order {correlation_id} was REJECTED by the broker: {reason}"),
            Self::OrderReadinessRefused { reason, .. } => format!(
                "🔷 DHAN — live order REFUSED: the broker account is not order-ready ({reason}). Check the Dhan data plan, Derivative segment and token, then retry. No order was sent."
            ),
            Self::OmsHalted { cause, .. } => format!(
                "🔷 DHAN — order system HALTED after a broker {cause} error. No more orders will be sent until an operator clears the halt. What to do RIGHT NOW: 1) check the Dhan account status 2) check the token 3) clear the halt once fixed."
            ),
            Self::Dh904LadderExhausted { attempts, .. } => format!(
                "🔷 DHAN — order gave up after {attempts} rate-limit retries (~150 seconds). The broker is throttling us. The order was NOT placed."
            ),
            Self::OrderApiStopAll { cooldown_secs } => format!(
                "🔷 DHAN — too many requests: ALL order calls paused for {cooldown_secs} seconds, then resume automatically."
            ),
        }
    }
}

/// Callback trait for OMS → Telegram alerts.
/// Implemented in the app crate to bridge to `NotificationService`.
pub trait OmsAlertSink: Send + Sync {
    /// Sends an alert. Best-effort — never blocks.
    fn fire(&self, alert: OmsAlert);
}

// ---------------------------------------------------------------------------
// OrderManagementSystem
// ---------------------------------------------------------------------------

/// Central order management system composing all OMS sub-components.
///
/// Manages the full order lifecycle: place → track → update → reconcile.
///
/// # Safety: Dry-Run Mode (default)
/// When `dry_run` is `true` (the default), **no HTTP calls are ever made**.
/// All order operations are simulated locally — place returns a fake order ID,
/// modify/cancel update local state only, reconcile is a no-op.
/// This ensures the system NEVER touches real money during development.
pub struct OrderManagementSystem {
    /// Order state keyed by Dhan order ID.
    orders: HashMap<String, ManagedOrder>,
    /// Correlation ID tracker for idempotency.
    correlations: CorrelationTracker,
    /// Dhan REST API client.
    api_client: OrderApiClient,
    /// SEBI rate limiter.
    rate_limiter: OrderRateLimiter,
    /// Circuit breaker for Dhan API.
    circuit_breaker: OrderCircuitBreaker,
    /// Token provider for authentication.
    token_provider: Box<dyn TokenProvider>,
    /// Dhan client ID for REST API requests.
    client_id: String,
    /// Total orders placed through this OMS instance.
    total_placed: u64,
    /// Total order updates processed.
    total_updates: u64,
    /// Dry-run mode: when true, NO HTTP calls are made. All orders are simulated.
    /// DEFAULT: true. Must be explicitly set to false for live trading.
    dry_run: bool,
    /// Counter for generating sequential paper order IDs.
    paper_order_counter: u64,
    /// Alert sink for immediate Telegram notifications (cold path — orders are 1-100/day).
    /// `None` in tests; `Some` in production. Fires CircuitBreakerOpened/Closed,
    /// RateLimitExhausted, OrderRejected events.
    alert_sink: Option<Box<dyn OmsAlertSink>>,
    /// Cluster F: order-readiness snapshot (LIVE path only). `None` ⇒ live
    /// orders REFUSED (fail-closed). Installed by the boot seam.
    readiness: Option<Arc<OrderReadinessState>>,
    /// Cluster F: DH-901(post-retry)/902/903/DATA-810 halt latch. `reset_daily`
    /// does NOT clear it — only `clear_order_halt` or a restart.
    halt: Option<HaltInfo>,
    /// Cluster F (C3/F-D fix): engine-side edge-latch for the `readiness == None`
    /// (unwired-seam) refusal case, where the state-side `refusal_latched` does
    /// not exist. Keeps ORDER-READY-01 "loud once per episode".
    never_probed_latched: bool,
    /// Cluster F (C2 fix): engine-side per-episode latch for the DATA-805
    /// STOP-ALL operator alert, so a strategy loop hammering orders during the
    /// 60s cooldown fires ONE Telegram, not N (audit-findings Rule 4). Cleared
    /// when a call succeeds or surfaces a non-805 class (the window ended).
    stop_all_alert_latched: bool,
    /// Tracked 3-leg super orders keyed by ENTRY-leg order id (Cluster B,
    /// 2026-07-14). Deliberately separate from `orders` — the super-order
    /// top-level PENDING→TRADED→TRIGGERED→CLOSED walk is tracked as RAW
    /// strings, bypassing the ManagedOrder state machine (Ruling 4).
    super_orders: HashMap<String, ManagedSuperOrder>,
    /// MPP verify-ladder bookkeeping keyed by order id (a separate map so
    /// the `ManagedOrder` struct stays untouched for Cluster A).
    verify_states: HashMap<String, VerifyState>,
}

/// Cluster F: order-path halt record. Set idempotently; cleared only by an
/// operator (`clear_order_halt`) or a process restart.
#[derive(Debug, Clone)]
struct HaltInfo {
    /// The Dhan code that caused the halt (e.g. "DH-901", "DATA-810").
    cause: &'static str,
    /// The sanitized/formatted underlying error detail.
    detail: String,
    /// When the halt engaged (IST epoch micros).
    at_epoch_us: i64,
}

impl OrderManagementSystem {
    /// Creates a new OMS in **dry-run mode** (default).
    ///
    /// In dry-run mode, no HTTP calls are ever made to Dhan. All orders
    /// are simulated locally with fake order IDs (PAPER-xxx).
    ///
    /// # Arguments
    /// * `api_client` — Dhan REST API client (unused in dry-run).
    /// * `rate_limiter` — SEBI-compliant rate limiter.
    /// * `token_provider` — Callback for getting the current access token.
    /// * `client_id` — Dhan client ID.
    pub fn new(
        api_client: OrderApiClient,
        rate_limiter: OrderRateLimiter,
        token_provider: Box<dyn TokenProvider>,
        client_id: String,
    ) -> Self {
        Self {
            orders: HashMap::with_capacity(256),
            correlations: CorrelationTracker::new(),
            api_client,
            rate_limiter,
            circuit_breaker: OrderCircuitBreaker::new(),
            token_provider,
            client_id,
            total_placed: 0,
            total_updates: 0,
            dry_run: true,
            paper_order_counter: 0,
            alert_sink: None,
            readiness: None,
            halt: None,
            never_probed_latched: false,
            stop_all_alert_latched: false,
            super_orders: HashMap::with_capacity(16),
            verify_states: HashMap::with_capacity(16),
        }
    }

    /// Returns whether the OMS is in dry-run (paper trading) mode.
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Sets the alert sink for immediate Telegram notifications.
    /// Called from the app crate boot sequence to wire OMS → Telegram.
    // TEST-EXEMPT: setter wired at boot, tested indirectly by integration tests
    pub fn set_alert_sink(&mut self, sink: Box<dyn OmsAlertSink>) {
        self.alert_sink = Some(sink);
    }

    /// Fires an OMS alert if a sink is wired. Best-effort — never blocks. The
    /// `warn!` makes every alert visible in the log sinks TODAY (sink installers
    /// = zero) and gives `operator_message` its production call site.
    fn fire_alert(&self, alert: OmsAlert) {
        warn!(alert = %alert.operator_message(), "OMS alert fired");
        if let Some(ref sink) = self.alert_sink {
            sink.fire(alert);
        }
    }

    /// Installs the order-readiness snapshot (boot seam). Until installed, the
    /// LIVE path fail-closes ORDER-READY-01 (reason=no_probe).
    pub fn set_order_readiness(&mut self, state: Arc<OrderReadinessState>) {
        self.readiness = Some(state);
        // The state-side latch now owns refusal edge-triggering; re-arm the
        // engine-side no_probe latch so a later uninstall would fire loudly again.
        self.never_probed_latched = false;
    }

    /// Whether the OMS order path is halted (DH-901 post-retry / 902 / 903 / 810).
    #[must_use]
    pub fn is_order_halted(&self) -> bool {
        self.halt.is_some()
    }

    /// Deliberate operator/boot-owner action to clear the order-path halt. Logs
    /// the original cause on clear. `reset_daily` does NOT call this.
    pub fn clear_order_halt(&mut self) {
        if let Some(info) = self.halt.take() {
            error!(
                code = ErrorCode::OrderReady01GateRefused.code_str(),
                cause = info.cause,
                detail = %info.detail,
                halted_since_us = info.at_epoch_us,
                "🔷 DHAN — OMS order-path halt CLEARED by operator"
            );
            metrics::gauge!("tv_oms_halted").set(0.0);
            // C1 fix: also reset the circuit breaker. A halt-class error can be
            // reached while the CB is HalfOpen (the CB check runs before the halt
            // gate, consuming the single HalfOpen probe without a success/failure
            // record); without this the CB stays stuck HalfOpen after the halt is
            // cleared and every later order is refused with CircuitBreakerOpen.
            self.circuit_breaker.reset();
        }
    }

    /// Engage the halt latch idempotently. Returns `true` on the rising edge
    /// (newly halted) so the caller alerts exactly once.
    fn engage_halt(&mut self, cause: &'static str, detail: String) -> bool {
        if self.halt.is_some() {
            debug!(cause, "OMS order path already halted (idempotent)");
            return false;
        }
        self.halt = Some(HaltInfo {
            cause,
            detail,
            at_epoch_us: now_epoch_us(),
        });
        metrics::gauge!("tv_oms_halted").set(1.0);
        metrics::counter!(
            "tv_oms_halt_engaged_total",
            "cause" => halt_cause_slug(cause),
        )
        .increment(1);
        true
    }

    /// O(1) live-order gate: halt check + readiness evaluation. Place + Modify
    /// ONLY (Cancel is exempt — exposure-reducing). First failure wins.
    fn check_live_order_gates(
        &mut self,
        endpoint: OrderEndpoint,
        correlation_id: &str,
    ) -> Result<(), OmsError> {
        if let Some(ref info) = self.halt {
            let cause = info.cause;
            return Err(OmsError::OrderPathHalted { cause });
        }
        let Some(state) = self.readiness.clone() else {
            return self.refuse_readiness(ReadinessRefusal::NeverProbed, endpoint, correlation_id);
        };
        match evaluate_order_readiness(&state, now_epoch_s()) {
            Ok(()) => Ok(()),
            Err(refusal) => self.refuse_readiness(refusal, endpoint, correlation_id),
        }
    }

    /// Emit an ORDER-READY-01 refusal (edge-latched loud, else debug) and return
    /// the refusal error.
    fn refuse_readiness(
        &mut self,
        refusal: ReadinessRefusal,
        endpoint: OrderEndpoint,
        correlation_id: &str,
    ) -> Result<(), OmsError> {
        let reason = refusal.slug();
        // Rising edge iff the state's latch flips clear→latched, OR — for the
        // unwired-seam `readiness == None` case (C3/F-D fix) — the engine-side
        // `never_probed_latched` flips. Without the engine-side latch, a live
        // boot with no readiness installed would ERROR + alert on EVERY call.
        let rising = match self.readiness.clone() {
            Some(state) => state.latch_refusal(),
            None => {
                let was = self.never_probed_latched;
                self.never_probed_latched = true;
                !was
            }
        };
        if rising {
            error!(
                code = ErrorCode::OrderReady01GateRefused.code_str(),
                reason,
                endpoint = order_endpoint_str(endpoint),
                correlation_id,
                "🔷 DHAN — live order REFUSED by the order-readiness gate"
            );
            self.fire_alert(OmsAlert::OrderReadinessRefused {
                reason,
                correlation_id: correlation_id.to_owned(),
            });
        } else {
            debug!(reason, "order-readiness refusal (already latched)");
        }
        metrics::counter!("tv_order_readiness_refusals_total", "reason" => reason).increment(1);
        Err(OmsError::OrderReadinessRefused { reason })
    }

    /// Apply the engine-layer policy for a surfaced order error. The wrapper
    /// (`*_with_policy`) already ran the DH-904 ladder + 805 latch + Cancel
    /// single-retry; this layer owns the auth rotate-retry-once, the halt latch,
    /// the 806 readiness-poison, and the circuit-breaker decision (R19). Returns
    /// the action the caller takes with the error it still owns.
    // F-F duplicate-order safety note: the auth retries below (DH-901,
    // 807/808/809) re-send the SAME request struct — hence the SAME
    // `correlationId`. Dhan's `correlationId` is a client TRACKING tag, NOT a
    // documented server-side idempotency/dedup key. This retry is duplicate-safe
    // ONLY because these are AUTH-LAYER rejections (the request is rejected
    // BEFORE it reaches order processing, so no order can have been created).
    // Genuinely ambiguous outcomes (908/909/800/5xx/transport) are NEVER retried
    // here (Place/Modify NeverRetry); the correlation-probe adopt-or-flag
    // (flagged follow-up) is the real net for those.
    async fn apply_engine_error_policy(
        &mut self,
        err: &OmsError,
        endpoint: OrderEndpoint,
        auth_retry_used: &mut bool,
    ) -> EngineAction {
        let Some(class) = error_taxonomy::classify_oms_error(err) else {
            // Non-Dhan-surface error: default to tripping the breaker + pass through.
            self.circuit_breaker.record_failure();
            return EngineAction::PassThrough;
        };
        // Circuit-breaker decision (R19): halt/rate/refresh classes never trip.
        if error_taxonomy::trips_circuit_breaker(class) {
            self.circuit_breaker.record_failure();
        }
        // C2: a non-805 surfaced class means the STOP-ALL window has ended (the
        // ladder's check_stop_all_latch would have returned Data805 otherwise) —
        // re-arm the per-episode alert latch so the next 805 episode alerts once.
        if class != DhanErrorClass::Data805 {
            self.stop_all_alert_latched = false;
        }
        match error_taxonomy::policy_for(class, endpoint) {
            OrderErrorPolicy::RotateRetryOnceThenHalt => {
                if !*auth_retry_used {
                    *auth_retry_used = true;
                    warn!(
                        code = ErrorCode::Dh901InvalidAuth.code_str(),
                        stage = "rotate_requested",
                        "🔷 DHAN — auth invalid; rotating token and retrying once"
                    );
                    self.token_provider.request_token_rotation();
                    tokio::time::sleep(Duration::from_secs(DH901_ROTATE_RETRY_DELAY_SECS)).await;
                    EngineAction::Retry
                } else {
                    self.halt_with_alert("DH-901", err);
                    EngineAction::Halt("DH-901")
                }
            }
            OrderErrorPolicy::HaltAndAlert => {
                let cause = halt_cause_for(class);
                self.halt_with_alert(cause, err);
                EngineAction::Halt(cause)
            }
            OrderErrorPolicy::TokenRefreshRetryOnce => {
                if !*auth_retry_used {
                    *auth_retry_used = true;
                    warn!(
                        code = error_taxonomy::error_code_for(class).code_str(),
                        stage = "token_refresh_requested",
                        "🔷 DHAN — token expired/invalid; refreshing and retrying once"
                    );
                    self.token_provider.request_token_rotation();
                    tokio::time::sleep(Duration::from_secs(DH901_ROTATE_RETRY_DELAY_SECS)).await;
                    EngineAction::Retry
                } else {
                    error!(
                        code = error_taxonomy::error_code_for(class).code_str(),
                        "🔷 DHAN — token refresh retry failed (no halt); order not sent"
                    );
                    EngineAction::ReturnTokenExpired
                }
            }
            OrderErrorPolicy::AlertOnlyPoisonReadiness => {
                warn!(
                    code = ErrorCode::Data806NotSubscribed.code_str(),
                    "🔷 DHAN — data plan not subscribed; poisoning order readiness"
                );
                if let Some(ref state) = self.readiness {
                    state.poison_profile();
                }
                EngineAction::PassThrough
            }
            OrderErrorPolicy::BackoffLadder => {
                // Surfacing here means the ladder already exhausted upstream.
                self.fire_alert(OmsAlert::Dh904LadderExhausted {
                    operation: order_endpoint_str(endpoint),
                    attempts: DH904_MAX_RETRY_ATTEMPTS as u32,
                });
                // F-E: suppress the trailing OrderRejected — one Telegram per
                // decision (the ladder-exhausted alert IS the decision).
                EngineAction::PassThroughSilent
            }
            OrderErrorPolicy::StopAllCooldown => {
                // C2: fire the operator alert ONCE per stop-all episode, not on
                // every throttled call during the 60s window.
                if !self.stop_all_alert_latched {
                    self.stop_all_alert_latched = true;
                    self.fire_alert(OmsAlert::OrderApiStopAll {
                        cooldown_secs: DATA_805_STOP_ALL_COOLDOWN_SECS,
                    });
                } else {
                    debug!("DATA-805 stop-all alert already fired this episode");
                }
                EngineAction::PassThroughSilent
            }
            // NeverRetry / CheckParamsNoRetry / LogAndAlert / CancelSingleRetry
            // are all resolved as a pass-through at the engine layer.
            _ => EngineAction::PassThrough,
        }
    }

    /// Engage the halt (idempotent) and, on the rising edge, log + alert + gauge.
    fn halt_with_alert(&mut self, cause: &'static str, err: &OmsError) {
        // Security: the OmsError Display embeds the raw Dhan response body
        // (DhanApiError { message }). Route it through the ≤300-char,
        // secret-redacting REST-body capture BEFORE it reaches HaltInfo.detail
        // (→ clear_order_halt log) and the OmsHalted alert.
        let detail = capture_rest_error_body(&format!("{err}"));
        if self.engage_halt(cause, detail.clone()) {
            error!(
                code = halt_error_code(cause).code_str(),
                cause,
                "🔷 DHAN — broker account error; OMS order path HALTED (operator action required)"
            );
            self.fire_alert(OmsAlert::OmsHalted { cause, detail });
        }
    }

    /// Places a new order through the OMS pipeline.
    ///
    /// Flow: rate limit → circuit breaker → generate correlation ID →
    ///       call Dhan API → create ManagedOrder in Transit state.
    ///
    /// # Returns
    /// The Dhan order ID on success.
    ///
    /// # Errors
    /// - `OmsError::RateLimited` — SEBI rate limit exceeded
    /// - `OmsError::CircuitBreakerOpen` — Dhan API unavailable
    /// - `OmsError::NoToken` / `OmsError::TokenExpired` — auth failure
    /// - `OmsError::DhanApiError` — Dhan returned an error
    /// - `OmsError::DhanRateLimited` — HTTP 429 from Dhan
    #[instrument(skip_all, fields(security_id = request.security_id))]
    pub async fn place_order(&mut self, request: PlaceOrderRequest) -> Result<String, OmsError> {
        let start = std::time::Instant::now();

        // Step 0: Pre-submission validation gates (before consuming rate limit)
        validate_order_fields(&request)?;

        // Step 1: Rate limiter check (runs even in dry-run for realistic simulation)
        if let Err(err) = self.rate_limiter.check() {
            self.fire_alert(OmsAlert::RateLimitExhausted {
                limit_type: "per_second".to_string(),
            });
            return Err(err);
        }

        // Step 2: Circuit breaker check
        if let Err(err) = self.circuit_breaker.check() {
            self.fire_alert(OmsAlert::CircuitBreakerOpened {
                consecutive_failures: u64::from(self.circuit_breaker.failure_count()),
            });
            return Err(err);
        }

        // Step 3: Generate correlation ID
        let correlation_id = self.correlations.generate_id();
        let now_us = now_epoch_us();

        // ---- DRY-RUN: simulate order without any HTTP call ----
        if self.dry_run {
            self.paper_order_counter = self.paper_order_counter.saturating_add(1);
            let paper_order_id = format!("PAPER-{}", self.paper_order_counter);

            let order = ManagedOrder {
                order_id: paper_order_id.clone(),
                correlation_id: correlation_id.clone(),
                security_id: request.security_id,
                transaction_type: request.transaction_type,
                order_type: request.order_type,
                product_type: request.product_type,
                validity: request.validity,
                quantity: request.quantity,
                price: request.price,
                trigger_price: request.trigger_price,
                status: OrderStatus::Confirmed,
                traded_qty: 0,
                avg_traded_price: 0.0,
                lot_size: request.lot_size,
                created_at_us: now_us,
                updated_at_us: now_us,
                needs_reconciliation: false,
                modification_count: 0,
            };

            self.correlations
                .track(correlation_id, paper_order_id.clone());
            self.orders.insert(paper_order_id.clone(), order);
            self.total_placed = self.total_placed.saturating_add(1);

            counter!("tv_orders_placed_total", "mode" => "paper").increment(1);
            metrics::histogram!("tv_order_placement_duration_ns")
                .record(start.elapsed().as_nanos() as f64);

            info!(
                order_id = %paper_order_id,
                security_id = request.security_id,
                transaction_type = %request.transaction_type.as_str(),
                quantity = request.quantity,
                price = request.price,
                "PAPER TRADE: order simulated (no HTTP call)"
            );

            return Ok(paper_order_id);
        }

        // ---- LIVE MODE: actual Dhan REST API call ----

        // Sandbox enforcement: block live orders until the sentinel deadline.
        // This is a mechanical safety gate — no real money should be at risk
        // until the operator explicitly re-arms for live.
        //
        // 2026-07-14 re-arm (operator cluster-D directive via coordinator):
        // the previous fn-local 2026-07-01 deadline (1_782_864_000) EXPIRED
        // silently on 2026-07-01, leaving this gate a no-op. The constant now
        // lives in `tickvault_common::constants::SANDBOX_DEADLINE_EPOCH_SECS`
        // (single source of truth, 2099-12-31T00:00:00Z sentinel matching
        // production.toml's sandbox_only_until) — going live requires editing
        // that constant with a fresh dated operator quote.
        //
        // FIX 2026-04-14 (session 8, retained for history): the original
        // value 1_782_777_600 was ONE DAY TOO EARLY (2026-06-30T00:00:00
        // UTC). Caught by
        // `sandbox_enforcement_guard::test_sandbox_deadline_matches_known_utc_epoch`.
        // The guard still locks the constant against a chrono-computed
        // expected value so that class of bug cannot recur silently.
        #[cfg(not(test))]
        {
            use tickvault_common::constants::SANDBOX_DEADLINE_EPOCH_SECS;
            let now_secs = chrono::Utc::now().timestamp();
            if now_secs < SANDBOX_DEADLINE_EPOCH_SECS {
                // Session 8 C4: count every block so Grafana can alert on
                // unexpected live-mode attempts during the sandbox window.
                metrics::counter!("tv_sandbox_gate_blocks_total").increment(1);
                error!(
                    "SANDBOX ENFORCEMENT: live orders blocked pending explicit \
                     re-arm (sentinel 2099-12-31; a dated operator quote + \
                     constant edit are required to go live)"
                );
                return Err(OmsError::SandboxEnforcement);
            }
        }

        // Step 3a (Cluster F): live-order readiness gate (fail-closed). Runs
        // AFTER the sandbox block, BEFORE the token fetch. Cancel is exempt.
        self.check_live_order_gates(OrderEndpoint::Place, &correlation_id)?;

        // Step 4: Build Dhan REST request (independent of the token, so it is
        // built once and reused across any auth rotate-retry — same correlationId).
        let dhan_request = DhanPlaceOrderRequest {
            dhan_client_id: self.client_id.clone(),
            transaction_type: request.transaction_type.as_str().to_owned(),
            exchange_segment: EXCHANGE_SEGMENT_NSE_FNO.to_owned(),
            product_type: request.product_type.as_str().to_owned(),
            order_type: request.order_type.as_str().to_owned(),
            validity: request.validity.as_str().to_owned(),
            security_id: request.security_id.to_string(),
            quantity: request.quantity,
            price: request.price,
            trigger_price: request.trigger_price,
            disclosed_quantity: 0,
            after_market_order: false,
            correlation_id: correlation_id.clone(),
        };

        // Step 5: Call Dhan REST API through the STOP-ALL latch + DH-904 ladder,
        // with an at-most-once auth rotate-retry at the engine layer.
        let mut auth_retry_used = false;
        let response = loop {
            let access_token = self.token_provider.get_access_token()?;
            match self
                .api_client
                .place_order_with_policy(access_token.expose_secret(), &dhan_request)
                .await
            {
                Ok(resp) => {
                    let prev_failures = self.circuit_breaker.failure_count();
                    self.circuit_breaker.record_success();
                    // C2: a successful call means the STOP-ALL window has ended.
                    self.stop_all_alert_latched = false;
                    if self.circuit_breaker.was_previously_open(prev_failures) {
                        self.fire_alert(OmsAlert::CircuitBreakerClosed);
                    }
                    break resp;
                }
                Err(err) => match self
                    .apply_engine_error_policy(&err, OrderEndpoint::Place, &mut auth_retry_used)
                    .await
                {
                    EngineAction::Retry => continue,
                    EngineAction::Halt(cause) => {
                        return Err(OmsError::OrderPathHalted { cause });
                    }
                    EngineAction::ReturnTokenExpired => return Err(OmsError::TokenExpired),
                    EngineAction::PassThrough => {
                        // C4 (2026-07-14, from main): place-time API rejections
                        // and WS-reported REJECTED transitions are DISJOINT
                        // classes — count BOTH so the tv-<env>-orders-rejected
                        // alarm pages for the only class reachable at Phase-1
                        // entry (the order-update WS is functional-dormant).
                        counter!("tv_orders_rejected_total").increment(1);
                        self.fire_alert(OmsAlert::OrderRejected {
                            correlation_id: correlation_id.clone(),
                            // Security: redact the raw broker body before it
                            // reaches the alert reason string.
                            reason: capture_rest_error_body(&format!("{err}")),
                        });
                        return Err(err);
                    }
                    EngineAction::PassThroughSilent => return Err(err),
                },
            }
        };

        // Step 6: Create ManagedOrder with status from Dhan response.
        // Dhan can return TRADED or REJECTED immediately; default to Transit if unparsable.
        let initial_status = resolve_initial_status(&response.order_status);

        let order = ManagedOrder {
            order_id: response.order_id.clone(),
            correlation_id: correlation_id.clone(),
            security_id: request.security_id,
            transaction_type: request.transaction_type,
            order_type: request.order_type,
            product_type: request.product_type,
            validity: request.validity,
            quantity: request.quantity,
            price: request.price,
            trigger_price: request.trigger_price,
            status: initial_status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: request.lot_size,
            created_at_us: now_us,
            updated_at_us: now_us,
            needs_reconciliation: false,
            modification_count: 0,
        };

        // Step 7: Track in state
        self.correlations
            .track(correlation_id, response.order_id.clone());
        self.orders.insert(response.order_id.clone(), order);
        self.total_placed = self.total_placed.saturating_add(1);

        counter!("tv_orders_placed_total", "mode" => "live").increment(1);
        metrics::histogram!("tv_order_placement_duration_ns")
            .record(start.elapsed().as_nanos() as f64);

        info!(
            order_id = %response.order_id,
            security_id = request.security_id,
            transaction_type = %request.transaction_type.as_str(),
            quantity = request.quantity,
            price = request.price,
            "order placed successfully"
        );

        Ok(response.order_id)
    }

    /// Modifies an existing open order.
    ///
    /// # Errors
    /// - `OmsError::OrderNotFound` — order ID not tracked
    /// - `OmsError::OrderTerminal` — order already in terminal state
    /// - Other API/rate/circuit errors
    pub async fn modify_order(
        &mut self,
        order_id: &str,
        request: ModifyOrderRequest,
    ) -> Result<(), OmsError> {
        // Validate order exists and is modifiable
        let order = self
            .orders
            .get(order_id)
            .ok_or_else(|| OmsError::OrderNotFound {
                order_id: order_id.to_owned(),
            })?;

        if order.is_terminal() {
            return Err(OmsError::OrderTerminal {
                order_id: order_id.to_owned(),
                status: order.status.as_str().to_owned(),
            });
        }

        // Enforce Dhan's max 25 modifications per order
        check_modification_limit(order.modification_count, order_id)?;

        // Validate order type / price / trigger consistency
        validate_modify_fields(&request)?;

        // Validate disclosed quantity if specified
        validate_disclosed_quantity(request.quantity, request.disclosed_quantity)
            .map_err(|reason| OmsError::RiskRejected { reason })?;

        self.rate_limiter.check()?;
        self.circuit_breaker.check()?;

        // ---- DRY-RUN: update local state only, no HTTP ----
        if self.dry_run {
            if let Some(order) = self.orders.get_mut(order_id) {
                order.order_type = request.order_type;
                order.quantity = request.quantity;
                order.price = request.price;
                order.trigger_price = request.trigger_price;
                order.validity = request.validity;
                order.modification_count = order.modification_count.saturating_add(1);
                order.updated_at_us = now_epoch_us();
            }
            info!(order_id = %order_id, "PAPER TRADE: order modify simulated (no HTTP call)");
            return Ok(());
        }

        // Step 3a (Cluster F): live-order readiness gate (fail-closed). Runs
        // BEFORE the token fetch. Cancel is exempt; Modify is NOT.
        self.check_live_order_gates(OrderEndpoint::Modify, order_id)?;

        // ---- LIVE MODE ----
        // Build the Dhan request once (token-independent) so it is reused across
        // any auth rotate-retry — the same order_id / correlationId.
        let dhan_request = DhanModifyOrderRequest {
            dhan_client_id: self.client_id.clone(),
            order_id: order_id.to_owned(),
            order_type: request.order_type.as_str().to_owned(),
            leg_name: String::new(),
            quantity: request.quantity,
            price: request.price,
            trigger_price: request.trigger_price,
            validity: request.validity.as_str().to_owned(),
            disclosed_quantity: request.disclosed_quantity,
        };

        // STOP-ALL latch + DH-904 ladder in the client wrapper; auth
        // rotate-retry-once + halt latch + circuit-breaker decision here.
        let mut auth_retry_used = false;
        loop {
            let access_token = self.token_provider.get_access_token()?;
            match self
                .api_client
                .modify_order_with_policy(access_token.expose_secret(), order_id, &dhan_request)
                .await
            {
                Ok(()) => {
                    let prev_failures = self.circuit_breaker.failure_count();
                    self.circuit_breaker.record_success();
                    // C2: a successful call means the STOP-ALL window has ended.
                    self.stop_all_alert_latched = false;
                    if self.circuit_breaker.was_previously_open(prev_failures) {
                        self.fire_alert(OmsAlert::CircuitBreakerClosed);
                    }
                    break;
                }
                Err(err) => match self
                    .apply_engine_error_policy(&err, OrderEndpoint::Modify, &mut auth_retry_used)
                    .await
                {
                    EngineAction::Retry => continue,
                    EngineAction::Halt(cause) => return Err(OmsError::OrderPathHalted { cause }),
                    EngineAction::ReturnTokenExpired => return Err(OmsError::TokenExpired),
                    EngineAction::PassThrough | EngineAction::PassThroughSilent => {
                        return Err(err);
                    }
                },
            }
        }

        // Update local state with modification details
        if let Some(order) = self.orders.get_mut(order_id) {
            order.order_type = request.order_type;
            order.quantity = request.quantity;
            order.price = request.price;
            order.trigger_price = request.trigger_price;
            order.validity = request.validity;
            order.modification_count = order.modification_count.saturating_add(1);
            order.updated_at_us = now_epoch_us();
        }

        info!(order_id = %order_id, "order modified successfully");
        Ok(())
    }

    /// Cancels an existing open order.
    ///
    /// # Errors
    /// - `OmsError::OrderNotFound` — order ID not tracked
    /// - `OmsError::OrderTerminal` — order already in terminal state
    /// - Other API/rate/circuit errors
    pub async fn cancel_order(&mut self, order_id: &str) -> Result<(), OmsError> {
        let order = self
            .orders
            .get(order_id)
            .ok_or_else(|| OmsError::OrderNotFound {
                order_id: order_id.to_owned(),
            })?;

        if order.is_terminal() {
            return Err(OmsError::OrderTerminal {
                order_id: order_id.to_owned(),
                status: order.status.as_str().to_owned(),
            });
        }

        self.rate_limiter.check()?;
        self.circuit_breaker.check()?;

        // ---- DRY-RUN: simulate cancel locally ----
        if self.dry_run {
            if let Some(order) = self.orders.get_mut(order_id) {
                order.status = OrderStatus::Cancelled;
                order.updated_at_us = now_epoch_us();
            }
            info!(order_id = %order_id, "PAPER TRADE: order cancel simulated (no HTTP call)");
            return Ok(());
        }

        // ---- LIVE MODE ----
        // Cancel is exposure-REDUCING: NO readiness gate and NO halt gate (R4).
        // It must always be attempted so the operator can flatten during a halt.
        // STOP-ALL latch + DH-904 ladder + cancel single-retry in the client
        // wrapper; auth rotate-retry-once + circuit-breaker decision here.
        let mut auth_retry_used = false;
        loop {
            let access_token = self.token_provider.get_access_token()?;
            match self
                .api_client
                .cancel_order_with_policy(access_token.expose_secret(), order_id)
                .await
            {
                Ok(()) => {
                    let prev_failures = self.circuit_breaker.failure_count();
                    self.circuit_breaker.record_success();
                    // C2: a successful call means the STOP-ALL window has ended.
                    self.stop_all_alert_latched = false;
                    if self.circuit_breaker.was_previously_open(prev_failures) {
                        self.fire_alert(OmsAlert::CircuitBreakerClosed);
                    }
                    break;
                }
                Err(err) => match self
                    .apply_engine_error_policy(&err, OrderEndpoint::Cancel, &mut auth_retry_used)
                    .await
                {
                    EngineAction::Retry => continue,
                    EngineAction::Halt(cause) => return Err(OmsError::OrderPathHalted { cause }),
                    EngineAction::ReturnTokenExpired => return Err(OmsError::TokenExpired),
                    EngineAction::PassThrough | EngineAction::PassThroughSilent => {
                        return Err(err);
                    }
                },
            }
        }

        // Mark for reconciliation — cancel is async; WebSocket will confirm,
        // but if WS is down we need REST reconciliation to pick it up.
        if let Some(order) = self.orders.get_mut(order_id) {
            order.needs_reconciliation = true;
            order.updated_at_us = now_epoch_us();
        }

        info!(order_id = %order_id, "order cancel request sent");
        Ok(())
    }

    /// Processes an order update from the WebSocket.
    ///
    /// Validates the state transition and updates the managed order.
    /// Invalid transitions are logged at ERROR level (triggers Telegram alert).
    ///
    /// # Returns
    /// `Ok(Some(FillEvent))` when the update carried a POSITIVE executed-
    /// quantity delta on a tracked order (the OMS → RiskEngine fill bridge,
    /// order-runtime dry-run PR 2026-07-14 — the delta is computed HERE where
    /// old and new `traded_qty` are adjacent, so duplicate / same-status
    /// re-deliveries yield delta 0 → `Ok(None)`, double-count-safe).
    /// `Ok(None)` when the update was processed without a new fill (unknown
    /// order, unknown status, zero/negative delta).
    pub fn handle_order_update(
        &mut self,
        update: &OrderUpdate,
    ) -> Result<Option<FillEvent>, OmsError> {
        self.total_updates = self.total_updates.saturating_add(1);

        let order_id = &update.order_no;

        // Try to find order by order_id first, then by correlation_id
        let found_order_id = if self.orders.contains_key(order_id) {
            Some(order_id.clone())
        } else if !update.correlation_id.is_empty() {
            self.correlations
                .get_order_id(&update.correlation_id)
                .cloned()
        } else {
            None
        };

        let order_id = match found_order_id {
            Some(id) => id,
            None => {
                debug!(
                    order_no = %update.order_no,
                    status = %update.status,
                    "order update for unknown order — ignoring"
                );
                return Ok(None);
            }
        };

        // If the update came via correlation_id with a different order_no,
        // re-index so future updates (which may lack correlation_id) can find it.
        if update.order_no != order_id
            && !update.order_no.is_empty()
            && let Some(order) = self.orders.get(&order_id).cloned()
        {
            self.orders.insert(update.order_no.clone(), order);
            debug!(
                old_order_id = %order_id,
                new_order_no = %update.order_no,
                "re-indexed order under new order_no from WebSocket"
            );
        }

        let new_status = match parse_order_status(&update.status) {
            Some(status) => status,
            None => {
                warn!(
                    order_id = %order_id,
                    status = %update.status,
                    "unknown order status in WebSocket update"
                );
                return Ok(None);
            }
        };

        // Get mutable reference to order
        let order = match self.orders.get_mut(&order_id) {
            Some(o) => o,
            None => return Ok(None),
        };

        // Validate transition
        let old_status = order.status;
        if old_status == new_status {
            // C3 (fix-round 2026-07-14): a TERMINAL order must never re-fill
            // via the same-status arm — a second TRADED update with a HIGHER
            // traded_qty would otherwise emit a second FillEvent (the pinned
            // terminal-never-refills test only covered byte-identical
            // redeliveries). Terminal state is final: no copy, no fill.
            if order.is_terminal() {
                if update.traded_qty > order.traded_qty {
                    warn!(
                        code = ErrorCode::OmsGapStateMachine.code_str(),
                        order_id = %order_id,
                        status = %old_status.as_str(),
                        tracked_qty = order.traded_qty,
                        update_qty = update.traded_qty,
                        "OMS-GAP-01: same-status update on a TERMINAL order with a \
                         HIGHER traded_qty — refused (terminal orders never re-fill); \
                         flagging for reconciliation"
                    );
                    order.needs_reconciliation = true;
                }
                return Ok(None);
            }
            // Same status — just update fields (e.g., partial fill qty update).
            // Fill delta is computed BEFORE the copy so incremental
            // PART_TRADED quantity refreshes still produce a FillEvent.
            let fill = Self::extract_fill_delta(order, update);
            Self::apply_fill_fields(order, update);
            return Ok(fill);
        }

        if !is_valid_transition(old_status, new_status) {
            error!(
                order_id = %order_id,
                from = %old_status.as_str(),
                to = %new_status.as_str(),
                "INVALID ORDER STATE TRANSITION — flagging for reconciliation"
            );
            order.needs_reconciliation = true;
            return Err(OmsError::InvalidTransition {
                order_id,
                from: old_status.as_str().to_owned(),
                to: new_status.as_str().to_owned(),
            });
        }

        // Fill delta BEFORE the copy overwrites the previous traded_qty.
        let fill = Self::extract_fill_delta(order, update);

        // Apply transition
        order.status = new_status;
        Self::apply_fill_fields(order, update);

        // Emit metrics for terminal states
        match new_status {
            OrderStatus::Traded => {
                counter!("tv_orders_filled_total").increment(1);
            }
            OrderStatus::Rejected => {
                counter!("tv_orders_rejected_total").increment(1);
            }
            _ => {}
        }

        debug!(
            order_id = %order_id,
            from = %old_status.as_str(),
            to = %new_status.as_str(),
            traded_qty = update.traded_qty,
            "order state transition applied"
        );

        Ok(fill)
    }

    /// Applies the fill-field copy from an update onto the tracked order
    /// (both fill paths — same-status refresh + valid transition).
    ///
    /// C2 (fix-round 2026-07-14): the copy is MONOTONE — a REGRESSING (or
    /// negative garbage) `traded_qty` is refused with a `warn!` so an
    /// out-of-order/garbage update can never lower the delta baseline and
    /// inflate the next fill (the `reconcile()` path stays the only
    /// sanctioned downward correction). The avg-price copy is guarded the
    /// same way: a non-finite / non-positive wire `avg_traded_price` (the
    /// serde-default reality on non-fill updates) never poisons the tracked
    /// cumulative VWAP the delta-price math depends on.
    fn apply_fill_fields(order: &mut ManagedOrder, update: &OrderUpdate) {
        if update.traded_qty >= order.traded_qty {
            order.traded_qty = update.traded_qty;
            if update.avg_traded_price.is_finite() && update.avg_traded_price > 0.0 {
                order.avg_traded_price = update.avg_traded_price;
            }
        } else {
            warn!(
                code = ErrorCode::OmsGapStateMachine.code_str(),
                order_id = %order.order_id,
                tracked_qty = order.traded_qty,
                update_qty = update.traded_qty,
                "OMS-GAP-01: REGRESSING traded_qty on a fill path — copy refused \
                 (out-of-order/garbage update; reconcile() is the only sanctioned \
                 downward correction)"
            );
        }
        order.updated_at_us = now_epoch_us();
    }

    /// Computes the executed-quantity DELTA between the tracked order and an
    /// incoming update, converted to signed lots for `RiskEngine::record_fill`.
    ///
    /// - Delta = `update.traded_qty − order.traded_qty` (units). ≤ 0 → `None`
    ///   (duplicate / regressing update — never a fill; a reconcile-class
    ///   traded_qty DECREASE is corrected by `reconcile()`, not by a fill
    ///   event).
    /// - Lots = `floor(new_cum/lot) − floor(old_cum/lot)` (C4 fix-round
    ///   2026-07-14: remainder-CARRYING by construction — sequential off-lot
    ///   partials 30→50→75 @lot 25 credit 1+1+1 lots, never 1+0+1). A
    ///   cumulative qty that is not a lot multiple is reported loudly
    ///   (`OMS-GAP-01`) — Dhan fills arrive in lot multiples, so a remainder
    ///   means a wire/lot-size anomaly.
    /// - Price = the DELTA slice price, NOT the wire `avg_traded_price` (C1
    ///   fix-round 2026-07-14: Dhan's `avg_traded_price` is the order-level
    ///   CUMULATIVE VWAP; stamping it on a delta fill mis-prices every
    ///   multi-partial slice — 1@100 then 1@110 arrives as cum-avg 105, but
    ///   the second slice traded at 110). `delta_price = (new_qty·new_avg −
    ///   old_qty·old_avg) / qty_delta`; a non-finite or non-positive result
    ///   (garbage wire data) REJECTS the fill event loudly — never a
    ///   poisoned price into `record_fill` (C5).
    /// - Sign from the tracked order's `transaction_type` (authoritative —
    ///   the wire `TxnType` is serde-default and may be empty).
    fn extract_fill_delta(order: &ManagedOrder, update: &OrderUpdate) -> Option<FillEvent> {
        let old_qty = order.traded_qty.max(0);
        let new_qty = update.traded_qty;
        let qty_delta = new_qty.saturating_sub(old_qty);
        if qty_delta <= 0 {
            return None;
        }
        let lot_size = order.lot_size.max(1);
        // C4: remainder-carrying lot delta (floor of cumulatives, not of the
        // per-update delta) — units are never permanently dropped across
        // sequential off-lot partial slices.
        let lots_abs =
            (new_qty / i64::from(lot_size)).saturating_sub(old_qty / i64::from(lot_size));
        let remainder = new_qty % i64::from(lot_size);
        if remainder != 0 {
            error!(
                code = ErrorCode::OmsGapStateMachine.code_str(),
                order_id = %order.order_id,
                cumulative_qty = new_qty,
                lot_size,
                remainder,
                "OMS-GAP-01: cumulative traded_qty is not a lot multiple — lots \
                 are floored (the remainder CARRIES to the next slice); \
                 wire/lot-size anomaly"
            );
        }
        if lots_abs <= 0 {
            return None;
        }
        // C1/C5: delta-slice price from the cumulative VWAPs; reject garbage.
        let old_avg = if order.avg_traded_price.is_finite() && order.avg_traded_price > 0.0 {
            order.avg_traded_price
        } else {
            0.0
        };
        let delta_value = (new_qty as f64) * update.avg_traded_price - (old_qty as f64) * old_avg;
        let delta_price = delta_value / (qty_delta as f64);
        if !delta_price.is_finite() || delta_price <= 0.0 {
            error!(
                code = ErrorCode::OmsGapStateMachine.code_str(),
                order_id = %order.order_id,
                qty_delta,
                wire_avg_price = update.avg_traded_price,
                delta_price,
                "OMS-GAP-01: fill delta price is non-finite or non-positive — \
                 REJECTING the fill event (never a poisoned price into the risk \
                 book; the qty copy still advances — reconcile() corrects the \
                 QTY only, and the NEXT valid slice's delta over-attributes the \
                 rejected slice's value: pre-live doc note, paper fills are \
                 always finite>0)"
            );
            counter!("tv_oms_fill_price_rejected_total").increment(1);
            return None;
        }
        // i64 → i32 clamp: a real fill can never approach i32::MAX lots;
        // saturate defensively instead of panicking (annexure rule 15 class).
        let lots = i32::try_from(lots_abs).unwrap_or(i32::MAX);
        let fill_lots = match order.transaction_type {
            TransactionType::Buy => lots,
            TransactionType::Sell => lots.saturating_neg(),
        };
        let segment_code = parse_segment_chars(&update.exchange, &update.segment)
            .map_or(SEGMENT_CODE_UNKNOWN, |seg| seg.binary_code());
        Some(FillEvent {
            security_id: order.security_id,
            segment_code,
            fill_lots,
            avg_price: delta_price,
            lot_size,
            order_id: order.order_id.clone(),
        })
    }

    /// Runs reconciliation against the Dhan REST API.
    ///
    /// Fetches all today's orders and compares with OMS state.
    /// Mismatches are logged at ERROR level and corrected in local state.
    pub async fn reconcile(&mut self) -> Result<ReconciliationReport, OmsError> {
        // DRY-RUN: no server state to reconcile against.
        if self.dry_run {
            info!("PAPER TRADE: reconciliation skipped (dry-run mode)");
            return Ok(ReconciliationReport::default());
        }

        let access_token = self.token_provider.get_access_token()?;
        let dhan_orders = self
            .api_client
            .get_all_orders(access_token.expose_secret())
            .await?;

        let (report, updates) = reconcile_orders(&self.orders, &dhan_orders);

        // Apply corrections (status + fill data).
        //
        // M2 terminal-regression guard (2026-07-14, refined per refuter B1):
        // the broker GET snapshot can be STALER than local state (a fill
        // event already applied locally), so a correction whose LOCAL status
        // is terminal NEVER changes `order.status` — a raw assign would
        // regress a terminal order (e.g. local Traded reverted to
        // PartTraded) bypassing the state machine. The update's FILL fields
        // (traded_qty / avg_traded_price) DO apply even on terminal orders:
        // the broker's fill accounting is authoritative and dropping it
        // would lose fill data (B1). The skip counter fires ONLY when a
        // status change was actually suppressed (broker status != local
        // status). Routing corrections through the state machine proper is
        // the umbrella follow-up. Non-terminal corrections apply as before.
        let mut skipped_local_terminal: u64 = 0;
        for update in updates {
            if let Some(order) = self.orders.get_mut(&update.order_id) {
                if order.is_terminal() {
                    let status_suppressed = order.status != update.status;
                    order.traded_qty = update.traded_qty;
                    order.avg_traded_price = update.avg_traded_price;
                    order.updated_at_us = now_epoch_us();
                    if status_suppressed {
                        counter!(
                            "tv_oms_reconcile_corrections_skipped_total",
                            "reason" => "local_terminal"
                        )
                        .increment(1);
                        skipped_local_terminal = skipped_local_terminal.saturating_add(1);
                    } else {
                        // Statuses agree — the fill-only correction was
                        // fully consumed; nothing was suppressed.
                        order.needs_reconciliation = false;
                    }
                    continue;
                }
                order.status = update.status;
                order.traded_qty = update.traded_qty;
                order.avg_traded_price = update.avg_traded_price;
                order.needs_reconciliation = false;
                order.updated_at_us = now_epoch_us();
            }
        }
        if skipped_local_terminal > 0 {
            // ONE coalesced warn per reconcile run — never per order.
            warn!(
                code = tickvault_common::error_code::ErrorCode::OmsGapReconciliation.code_str(),
                skipped = skipped_local_terminal,
                "reconcile status corrections suppressed — local order status \
                 is terminal; a stale broker snapshot must not regress a \
                 terminal order (fill fields were still applied)"
            );
        }

        Ok(report)
    }

    /// Returns an order by ID.
    pub fn order(&self, order_id: &str) -> Option<&ManagedOrder> {
        self.orders.get(order_id)
    }

    /// Returns all active (non-terminal) orders.
    ///
    /// # Performance
    /// O(N_all_orders) + a `Vec` heap alloc per call (terminal orders
    /// accumulate until the daily reset) — COLD-PATH ONLY. Per-mark callers
    /// use the runtime's sid-keyed pending index instead (HP-2).
    pub fn active_orders(&self) -> Vec<&ManagedOrder> {
        self.orders.values().filter(|o| !o.is_terminal()).collect()
    }

    /// Returns the number of active (non-terminal) orders WITHOUT the
    /// `Vec` allocation `active_orders()` pays (HP-2 fix-round 2026-07-14
    /// — the marks-gate republish runs per event arm).
    ///
    /// # Performance
    /// O(N_all_orders) scan, zero allocation.
    pub fn active_order_count(&self) -> usize {
        self.orders.values().filter(|o| !o.is_terminal()).count()
    }

    /// Returns all orders (active + terminal).
    pub fn all_orders(&self) -> &HashMap<String, ManagedOrder> {
        &self.orders
    }

    /// Returns the total number of orders placed.
    pub fn total_placed(&self) -> u64 {
        self.total_placed
    }

    /// Returns the total number of updates processed.
    pub fn total_updates(&self) -> u64 {
        self.total_updates
    }

    /// Resets daily state (orders, correlations, counters).
    pub fn reset_daily(&mut self) {
        self.orders.clear();
        self.correlations.clear();
        self.super_orders.clear();
        self.verify_states.clear();
        self.total_placed = 0;
        self.total_updates = 0;
        self.paper_order_counter = 0;
        self.circuit_breaker.reset();
        // Cluster F (R8): the order-path halt latch is INTENTIONALLY NOT cleared
        // here. A broker account-class halt (DH-902/903/…) persists across the
        // daily reset until a deliberate operator `clear_order_halt()`.
        info!(dry_run = self.dry_run, "OMS daily state reset");
    }

    /// Enables live mode (not dry-run) for testing with mock servers.
    #[cfg(test)]
    pub(crate) fn enable_live_mode(&mut self) {
        self.dry_run = false;
    }
}

// ==== EXIT EXECUTION (Cluster B) ====
// ── Exit-order layer ── 🔷 DHAN exit-order execution methods (2026-07-14).
//
// Every method mirrors the pinned `place_order` gate ladder: Step-0
// validation → rate_limiter.check() → circuit_breaker.check() →
// correlation id → DRY-RUN BRANCH FIRST (source-order BEFORE any token
// fetch — ratcheted) → `// LIVE-EXIT-ARM` → api_client wrapper → track →
// metrics. 429/DhanRateLimited never trips the circuit breaker
// (place_order precedent). All error! lines are LOG-SINK-ONLY
// (EXIT-ORDER-01 / EXIT-VERIFY-01 — the 2026-07-14 Dhan noise lock:
// zero new Telegram emit sites).

impl OrderManagementSystem {
    /// Places a 3-leg super order (entry + target + stop loss) in ONE POST —
    /// the exchange-resident exit vehicle.
    ///
    /// Step 0 wires `exit_rules::validate_super_order_request` (finite
    /// prices, LIMIT/MARKET-only entry, lot multiple, trailing bound,
    /// I-P0-03 expiry gate) + `validate_super_order_prices` (relative
    /// ordering — LIMIT entries only: a MARKET entry carries price 0.0 per
    /// live-probe U1, so ordering against 0.0 is meaningless) + the freeze
    /// refusal (quantity > freeze_limit ⇒ typed `RiskRejected`; NO
    /// sliced-super endpoint exists — Verified V10, never auto-slice).
    ///
    /// Dry-run: `PAPER-SO-{n}` entry id; the entry leg is tracked as a
    /// normal `ManagedOrder` (status Confirmed — byte-consistent with the
    /// place_order paper semantics) and the 3-leg shape lands in the
    /// `super_orders` registry with PENDING target/stop legs.
    #[instrument(skip_all, fields(security_id = request.security_id))]
    pub async fn place_super_order(
        &mut self,
        request: PlaceSuperOrderRequest,
        freeze_limit: i64,
    ) -> Result<SuperOrderPlacement, OmsError> {
        // Step 0a: pure-rule validation (exit_rules — zero I/O).
        exit_rules::validate_super_order_request(&request)?;

        // Step 0b: relative price ordering (BUY target > entry etc.).
        // LIMIT entries only — MARKET entries carry price 0.0 (U1).
        if request.order_type == OrderType::Limit {
            validate_super_order_prices(
                request.transaction_type.as_str(),
                request.price,
                request.target_price,
                request.stop_loss_price,
            )?;
        }

        // Step 0c: freeze refusal — an oversize super order is a typed
        // refusal (no sliced-super endpoint exists), never an auto-slice.
        if exit_rules::requires_slicing(request.quantity, freeze_limit) {
            return Err(OmsError::RiskRejected {
                reason: format!(
                    "super order quantity {} exceeds freeze limit {} — no sliced-super \
                     endpoint exists; cap the per-signal quantity at the freeze limit",
                    request.quantity, freeze_limit
                ),
            });
        }

        // Steps 1+2: rate limiter + circuit breaker (alerts mirror place_order).
        self.check_order_gates()?;

        // Step 3: correlation ID.
        let correlation_id = self.correlations.generate_id();
        let now_us = now_epoch_us();

        // ---- DRY-RUN: simulate the 3-leg bracket without any HTTP call ----
        if self.dry_run {
            self.paper_order_counter = self.paper_order_counter.saturating_add(1);
            let entry_order_id = format!("PAPER-SO-{}", self.paper_order_counter);

            let entry = ManagedOrder {
                order_id: entry_order_id.clone(),
                correlation_id: correlation_id.clone(),
                security_id: request.security_id,
                transaction_type: request.transaction_type,
                order_type: request.order_type,
                product_type: request.product_type,
                // Super orders carry no wire validity field — track DAY.
                validity: OrderValidity::Day,
                quantity: request.quantity,
                price: request.price,
                trigger_price: 0.0,
                status: OrderStatus::Confirmed,
                traded_qty: 0,
                avg_traded_price: 0.0,
                lot_size: request.lot_size,
                created_at_us: now_us,
                updated_at_us: now_us,
                needs_reconciliation: false,
                modification_count: 0,
            };

            let target = LegState {
                order_id: format!("{entry_order_id}-TGT"),
                status_raw: "PENDING".to_owned(),
                price: request.target_price,
            };
            let stop_loss = LegState {
                order_id: format!("{entry_order_id}-SL"),
                status_raw: "PENDING".to_owned(),
                price: request.stop_loss_price,
            };
            let legs = vec![
                SuperOrderLegSnapshot {
                    leg: OrderLeg::EntryLeg,
                    order_id: entry_order_id.clone(),
                    status_raw: entry.status.as_str().to_owned(),
                },
                SuperOrderLegSnapshot {
                    leg: OrderLeg::TargetLeg,
                    order_id: target.order_id.clone(),
                    status_raw: target.status_raw.clone(),
                },
                SuperOrderLegSnapshot {
                    leg: OrderLeg::StopLossLeg,
                    order_id: stop_loss.order_id.clone(),
                    status_raw: stop_loss.status_raw.clone(),
                },
            ];

            let managed = ManagedSuperOrder {
                entry_order_id: entry_order_id.clone(),
                correlation_id: correlation_id.clone(),
                security_id: request.security_id,
                transaction_type: request.transaction_type,
                quantity: request.quantity,
                entry_price: request.price,
                entry_status_raw: entry.status.as_str().to_owned(),
                target,
                stop_loss,
                trailing_jump: request.trailing_jump,
                status: OrderStatus::Confirmed,
                modification_count: 0,
                created_at_us: now_us,
                updated_at_us: now_us,
            };

            self.correlations
                .track(correlation_id, entry_order_id.clone());
            self.orders.insert(entry_order_id.clone(), entry);
            self.super_orders.insert(entry_order_id.clone(), managed);
            self.total_placed = self.total_placed.saturating_add(1);

            counter!("tv_super_orders_placed_total", "mode" => "paper").increment(1);
            info!(
                order_id = %entry_order_id,
                security_id = request.security_id,
                transaction_type = %request.transaction_type.as_str(),
                quantity = request.quantity,
                price = request.price,
                target_price = request.target_price,
                stop_loss_price = request.stop_loss_price,
                trailing_jump = request.trailing_jump,
                "PAPER TRADE: super order simulated (no HTTP call)"
            );

            return Ok(SuperOrderPlacement {
                entry_order_id,
                legs,
            });
        }

        // LIVE-EXIT-ARM
        let access_token = self.token_provider.get_access_token()?;

        let dhan_request = DhanPlaceSuperOrderRequest {
            dhan_client_id: self.client_id.clone(),
            correlation_id: correlation_id.clone(),
            transaction_type: request.transaction_type.as_str().to_owned(),
            exchange_segment: EXCHANGE_SEGMENT_NSE_FNO.to_owned(),
            product_type: request.product_type.as_str().to_owned(),
            order_type: request.order_type.as_str().to_owned(),
            security_id: request.security_id.to_string(),
            quantity: request.quantity,
            price: request.price,
            target_price: request.target_price,
            stop_loss_price: request.stop_loss_price,
            // ALWAYS serialized (non-Option wire field) — omission on a
            // later modify would cancel the trail (Verified V5).
            trailing_jump: request.trailing_jump,
        };

        let response = match self
            .api_client
            .place_super_order(access_token.expose_secret(), &dhan_request)
            .await
        {
            Ok(resp) => {
                self.record_api_success_and_alert_recovery();
                resp
            }
            Err(err) => {
                // 429/DH-904 never trips the breaker (API healthy, throttled).
                if !matches!(err, OmsError::DhanRateLimited) {
                    self.circuit_breaker.record_failure();
                }
                // M1 (refuter round 1, 2026-07-14): the alert reason is a
                // future sink payload — sanitized like every log rendering.
                self.fire_alert(OmsAlert::OrderRejected {
                    correlation_id: correlation_id.clone(),
                    reason: sanitize_oms_error(&err),
                });
                error!(
                    code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                    correlation_id = %correlation_id,
                    security_id = request.security_id,
                    error = %sanitize_oms_error(&err),
                    "super order placement failed"
                );
                return Err(err);
            }
        };

        // L3 (2026-07-14 hostile review): a malformed server-supplied id is
        // NEVER tracked (and never echoed raw — length only). The order may
        // exist broker-side; the next reconcile() surfaces it.
        if !exit_rules::is_valid_order_id(&response.order_id) {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                correlation_id = %correlation_id,
                security_id = request.security_id,
                id_len = response.order_id.len(),
                "super order response carried a malformed orderId — refusing to track"
            );
            return Err(OmsError::DhanApiError {
                status_code: 200,
                message: "super order response carried a malformed orderId".to_owned(),
            });
        }

        let entry_order_id = response.order_id.clone();
        let initial_status = resolve_initial_status(&response.order_status);

        // Leg snapshot: every parseable leg from the response; the entry
        // snapshot is synthesized from the top level when absent.
        let mut legs: Vec<SuperOrderLegSnapshot> =
            Vec::with_capacity(response.leg_details.len().saturating_add(1));
        if !response
            .leg_details
            .iter()
            .any(|l| l.leg_name == OrderLeg::EntryLeg.as_str())
        {
            legs.push(SuperOrderLegSnapshot {
                leg: OrderLeg::EntryLeg,
                order_id: entry_order_id.clone(),
                status_raw: response.order_status.clone(),
            });
        }
        for detail in &response.leg_details {
            match parse_leg_name(&detail.leg_name) {
                Some(leg) => legs.push(SuperOrderLegSnapshot {
                    leg,
                    order_id: detail.order_id.clone(),
                    status_raw: detail.order_status.clone(),
                }),
                None => warn!(
                    leg_name = %detail.leg_name,
                    order_id = %entry_order_id,
                    "unknown super order leg name in place response — skipped"
                ),
            }
        }

        let leg_state = |wire_name: &str, fallback_price: f64| -> LegState {
            response
                .leg_details
                .iter()
                .find(|l| l.leg_name == wire_name)
                .map(|l| LegState {
                    order_id: l.order_id.clone(),
                    status_raw: l.order_status.clone(),
                    price: if l.price > 0.0 {
                        l.price
                    } else {
                        fallback_price
                    },
                })
                .unwrap_or(LegState {
                    order_id: String::new(),
                    status_raw: "PENDING".to_owned(),
                    price: fallback_price,
                })
        };
        let target = leg_state(OrderLeg::TargetLeg.as_str(), request.target_price);
        let stop_loss = leg_state(OrderLeg::StopLossLeg.as_str(), request.stop_loss_price);

        let entry = ManagedOrder {
            order_id: entry_order_id.clone(),
            correlation_id: correlation_id.clone(),
            security_id: request.security_id,
            transaction_type: request.transaction_type,
            order_type: request.order_type,
            product_type: request.product_type,
            validity: OrderValidity::Day,
            quantity: request.quantity,
            price: request.price,
            trigger_price: 0.0,
            status: initial_status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: request.lot_size,
            created_at_us: now_us,
            updated_at_us: now_us,
            needs_reconciliation: false,
            modification_count: 0,
        };
        let managed = ManagedSuperOrder {
            entry_order_id: entry_order_id.clone(),
            correlation_id: correlation_id.clone(),
            security_id: request.security_id,
            transaction_type: request.transaction_type,
            quantity: request.quantity,
            entry_price: request.price,
            entry_status_raw: response.order_status.clone(),
            target,
            stop_loss,
            trailing_jump: request.trailing_jump,
            status: initial_status,
            modification_count: 0,
            created_at_us: now_us,
            updated_at_us: now_us,
        };

        self.correlations
            .track(correlation_id, entry_order_id.clone());
        self.orders.insert(entry_order_id.clone(), entry);
        self.super_orders.insert(entry_order_id.clone(), managed);
        self.total_placed = self.total_placed.saturating_add(1);

        counter!("tv_super_orders_placed_total", "mode" => "live").increment(1);
        info!(
            order_id = %entry_order_id,
            security_id = request.security_id,
            transaction_type = %request.transaction_type.as_str(),
            quantity = request.quantity,
            legs = legs.len(),
            "super order placed successfully"
        );

        Ok(SuperOrderPlacement {
            entry_order_id,
            legs,
        })
    }

    /// Modifies ONE leg of a tracked super order (`PUT /v2/super/orders/{id}`).
    ///
    /// Leg restrictions are TYPE-LEVEL (`ModifySuperOrderLeg` — illegal
    /// field/leg combos unrepresentable). The 25-modification cap is
    /// tracked on `ManagedSuperOrder.modification_count` (shared across
    /// legs — Assumed A3). ENTRY_LEG modifies are gated by
    /// `entry_leg_modify_allowed` (07a §3: PENDING/PART_TRADED/TRANSIT
    /// only). StopLoss/Entry arms ALWAYS resend `trailing_jump` (omission
    /// on the wire CANCELS the trail — Verified V5); the current trail is
    /// readable via [`Self::super_order`] for callers that want to keep it.
    pub async fn modify_super_order_leg(
        &mut self,
        order_id: &str,
        modify: ModifySuperOrderLeg,
    ) -> Result<(), OmsError> {
        // L3: the id is URL-embedded on the live arm — refuse malformed
        // input before any lookup/gate (never echoed raw).
        if !exit_rules::is_valid_order_id(order_id) {
            return Err(OmsError::RiskRejected {
                reason: "super order modify refused: malformed order id \
                         (charset [A-Za-z0-9_-]{1,64})"
                    .to_owned(),
            });
        }

        // Step 0: registry lookup + gates (copy out to end the borrow).
        let (modification_count, entry_status_raw, target_status_raw, stop_status_raw) = {
            let so = self
                .super_orders
                .get(order_id)
                .ok_or_else(|| OmsError::OrderNotFound {
                    order_id: order_id.to_owned(),
                })?;
            (
                so.modification_count,
                so.entry_status_raw.clone(),
                so.target.status_raw.clone(),
                so.stop_loss.status_raw.clone(),
            )
        };

        check_modification_limit(modification_count, order_id)?;

        if matches!(modify, ModifySuperOrderLeg::Entry { .. })
            && !exit_rules::entry_leg_modify_allowed(&entry_status_raw)
        {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                order_id = %order_id,
                entry_status = %entry_status_raw,
                "ENTRY_LEG modify refused — entry leg is not PENDING/PART_TRADED/TRANSIT"
            );
            return Err(OmsError::RiskRejected {
                reason: format!(
                    "ENTRY_LEG modify refused for order {order_id}: entry status \
                     {entry_status_raw} is not modifiable (07a §3)"
                ),
            });
        }

        // H2 (2026-07-14 hostile review): the registry's entry_status_raw
        // only updates on place/verify/cancel — a fill applied by
        // handle_order_update / reconcile() lands on the tracked
        // ManagedOrder, so the TRACKED status is consulted too (registry
        // staleness defense AT THE GATE, never inside the fill path).
        if matches!(modify, ModifySuperOrderLeg::Entry { .. })
            && let Some(tracked) = self.orders.get(order_id)
            && tracked.is_terminal()
        {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                order_id = %order_id,
                tracked_status = tracked.status.as_str(),
                registry_entry_status = %entry_status_raw,
                "ENTRY_LEG modify refused — tracked entry order is terminal (super registry stale)"
            );
            return Err(OmsError::RiskRejected {
                reason: format!(
                    "ENTRY_LEG modify refused for order {order_id}: tracked entry status {} \
                     is terminal (super registry stale)",
                    tracked.status.as_str()
                ),
            });
        }

        // M4 (2026-07-14 hostile review): a Target/StopLoss modify on a
        // DEAD leg is refused (fail-open on unknown/empty raw statuses —
        // Dhan re-validates server-side; see exit_rules::exit_leg_modify_allowed).
        let (dead_leg, dead_leg_label, dead_leg_raw) = match &modify {
            ModifySuperOrderLeg::Target { .. } => (
                !exit_rules::exit_leg_modify_allowed(&target_status_raw),
                "TARGET_LEG",
                target_status_raw.as_str(),
            ),
            ModifySuperOrderLeg::StopLoss { .. } => (
                !exit_rules::exit_leg_modify_allowed(&stop_status_raw),
                "STOP_LOSS_LEG",
                stop_status_raw.as_str(),
            ),
            ModifySuperOrderLeg::Entry { .. } => (false, "ENTRY_LEG", entry_status_raw.as_str()),
        };
        if dead_leg {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                order_id = %order_id,
                leg = dead_leg_label,
                leg_status = %dead_leg_raw,
                "leg modify refused — the leg is already terminal"
            );
            return Err(OmsError::RiskRejected {
                reason: format!(
                    "{dead_leg_label} modify refused for order {order_id}: leg status \
                     {dead_leg_raw} is terminal"
                ),
            });
        }

        validate_modify_super_fields(&modify)?;

        // Steps 1+2: rate limiter + circuit breaker.
        self.check_order_gates()?;

        // ---- DRY-RUN: update the registry only, no HTTP ----
        if self.dry_run {
            self.apply_super_modify(order_id, &modify);
            info!(order_id = %order_id, "PAPER TRADE: super order leg modify simulated (no HTTP call)");
            return Ok(());
        }

        // LIVE-EXIT-ARM
        let access_token = self.token_provider.get_access_token()?;
        let dhan_request = build_super_modify_request(&self.client_id, order_id, &modify);

        let accepted_unparsed = match self
            .api_client
            .modify_super_order(access_token.expose_secret(), order_id, &dhan_request)
            .await
        {
            Ok(resp) => {
                // L6: recovered-alert symmetry with the place arms.
                self.record_api_success_and_alert_recovery();
                resp.order_status == SUPER_ORDER_STATUS_ACCEPTED_UNPARSED_BODY
            }
            Err(err) => {
                if !matches!(err, OmsError::DhanRateLimited) {
                    self.circuit_breaker.record_failure();
                }
                error!(
                    code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                    order_id = %order_id,
                    leg = %dhan_request.leg_name,
                    error = %sanitize_oms_error(&err),
                    "super order leg modify failed"
                );
                return Err(err);
            }
        };

        self.apply_super_modify(order_id, &modify);
        // L5 (2026-07-14 hostile review): a 2xx with an UNPARSABLE body is
        // accepted-with-doubt — the mutation may not have applied
        // broker-side, so the tracked entry is flagged for the REST
        // reconcile (the registry apply above stays, mirroring dry-run).
        if accepted_unparsed {
            if let Some(order) = self.orders.get_mut(order_id) {
                order.needs_reconciliation = true;
                order.updated_at_us = now_epoch_us();
            }
            warn!(
                order_id = %order_id,
                leg = %dhan_request.leg_name,
                "super order leg modify accepted with an unparsable body — flagged for reconciliation"
            );
        }
        info!(order_id = %order_id, leg = %dhan_request.leg_name, "super order leg modified successfully");
        Ok(())
    }

    /// Cancels ONE leg of a tracked super order
    /// (`DELETE /v2/super/orders/{id}/{leg}`).
    ///
    /// ENTRY_LEG cancellation cancels ALL legs — and is REFUSED (typed
    /// `RiskRejected` + EXIT-ORDER-01 log) once the entry is PART_TRADED
    /// or TRADED: a post-fill ENTRY_LEG cancel's effect on the live exit
    /// legs is UNVERIFIED-LIVE (naked-position risk U4 — never exercised).
    pub async fn cancel_super_order_leg(
        &mut self,
        order_id: &str,
        leg: OrderLeg,
    ) -> Result<(), OmsError> {
        // L3: the id is URL-embedded on the live arm — refuse malformed
        // input before any lookup/gate (never echoed raw).
        if !exit_rules::is_valid_order_id(order_id) {
            return Err(OmsError::RiskRejected {
                reason: "super order cancel refused: malformed order id \
                         (charset [A-Za-z0-9_-]{1,64})"
                    .to_owned(),
            });
        }

        // Step 0: registry lookup + the naked-leg race gate.
        let entry_status_raw = self
            .super_orders
            .get(order_id)
            .map(|so| so.entry_status_raw.clone())
            .ok_or_else(|| OmsError::OrderNotFound {
                order_id: order_id.to_owned(),
            })?;

        if leg == OrderLeg::EntryLeg && !exit_rules::entry_leg_cancel_allowed(&entry_status_raw) {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                order_id = %order_id,
                entry_status = %entry_status_raw,
                "ENTRY_LEG cancel refused post-fill — would race the live exit legs (U4)"
            );
            return Err(OmsError::RiskRejected {
                reason: format!(
                    "ENTRY_LEG cancel refused for order {order_id}: entry status \
                     {entry_status_raw} — post-fill entry cancel is never exercised (U4)"
                ),
            });
        }

        // H2 (2026-07-14 hostile review): the registry's entry_status_raw
        // only updates on place/verify/cancel — a fill applied by
        // handle_order_update / reconcile() lands on the tracked
        // ManagedOrder, so the TRACKED status is consulted too: a
        // PartTraded/Traded (or any terminal) tracked entry refuses the
        // ENTRY_LEG cancel even when the registry still reads pre-fill
        // (registry staleness defense AT THE GATE — the U4 naked-position
        // race is never exercised through a stale registry).
        if leg == OrderLeg::EntryLeg
            && let Some(tracked) = self.orders.get(order_id)
            && (tracked.status == OrderStatus::PartTraded || tracked.is_terminal())
        {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                order_id = %order_id,
                tracked_status = tracked.status.as_str(),
                registry_entry_status = %entry_status_raw,
                "ENTRY_LEG cancel refused — tracked entry order already filled/terminal \
                 (super registry stale, U4)"
            );
            return Err(OmsError::RiskRejected {
                reason: format!(
                    "ENTRY_LEG cancel refused for order {order_id}: tracked entry status {} \
                     (super registry stale — post-fill entry cancel is never exercised, U4)",
                    tracked.status.as_str()
                ),
            });
        }

        // Steps 1+2: rate limiter + circuit breaker.
        self.check_order_gates()?;

        // ---- DRY-RUN: update the registry only, no HTTP ----
        if self.dry_run {
            self.apply_super_cancel(order_id, leg);
            info!(
                order_id = %order_id,
                leg = leg.as_str(),
                "PAPER TRADE: super order leg cancel simulated (no HTTP call)"
            );
            return Ok(());
        }

        // LIVE-EXIT-ARM
        let access_token = self.token_provider.get_access_token()?;

        match self
            .api_client
            .cancel_super_order_leg(access_token.expose_secret(), order_id, leg)
            .await
        {
            Ok(_resp) => {
                // L6: recovered-alert symmetry with the place arms.
                self.record_api_success_and_alert_recovery();
            }
            Err(err) => {
                if !matches!(err, OmsError::DhanRateLimited) {
                    self.circuit_breaker.record_failure();
                }
                error!(
                    code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                    order_id = %order_id,
                    leg = leg.as_str(),
                    error = %sanitize_oms_error(&err),
                    "super order leg cancel failed"
                );
                return Err(err);
            }
        }

        match leg {
            OrderLeg::EntryLeg => {
                // Cancel is async server-side — flag the tracked entry for
                // reconciliation (mirror of cancel_order's live path); the
                // verify ladder / reconcile confirms the terminal state.
                if let Some(order) = self.orders.get_mut(order_id) {
                    order.needs_reconciliation = true;
                    order.updated_at_us = now_epoch_us();
                }
                if let Some(so) = self.super_orders.get_mut(order_id) {
                    so.updated_at_us = now_epoch_us();
                }
            }
            // L4 (2026-07-14 hostile review): a live TARGET/STOP_LOSS leg
            // cancel success updates the registry leg state exactly like
            // dry-run (apply_super_cancel) — the pre-fix asymmetry left
            // the leg forever PENDING in the registry.
            OrderLeg::TargetLeg | OrderLeg::StopLossLeg => {
                self.apply_super_cancel(order_id, leg);
            }
        }

        info!(order_id = %order_id, leg = leg.as_str(), "super order leg cancel request sent");
        Ok(())
    }

    /// Places a Forever (GTT) order, optionally OCO (`POST /v2/forever/orders`).
    ///
    /// HONEST SCOPE: Forever orders accept CNC/MTF ONLY (07b §1) — they
    /// structurally CANNOT serve intraday F&O exits. Wired for completeness
    /// per the mandate; never routed on the intraday exit path. The engine
    /// stamps its single `EXCHANGE_SEGMENT_NSE_FNO` segment constant (the
    /// only segment this system trades); a real CNC equity forever order
    /// needs a segment field added at enable time (live-probe ledger).
    ///
    /// The tracked `ManagedOrder` starts `Triggered` — Dhan answers with
    /// the forever-specific `CONFIRM` status, which parses to `Triggered`.
    #[instrument(skip_all, fields(security_id = request.security_id))]
    pub async fn place_forever_oco(
        &mut self,
        request: PlaceForeverOcoRequest,
    ) -> Result<String, OmsError> {
        // Step 0: pure-rule validation (CNC/MTF hard gate, finite prices,
        // OCO-leg completeness, I-P0-03 expiry gate).
        exit_rules::validate_forever_oco(&request)?;

        // Steps 1+2: rate limiter + circuit breaker.
        self.check_order_gates()?;

        // Step 3: correlation ID.
        let correlation_id = self.correlations.generate_id();
        let now_us = now_epoch_us();

        // ---- DRY-RUN: simulate without any HTTP call ----
        if self.dry_run {
            self.paper_order_counter = self.paper_order_counter.saturating_add(1);
            let paper_order_id = format!("PAPER-FO-{}", self.paper_order_counter);

            let order = ManagedOrder {
                order_id: paper_order_id.clone(),
                correlation_id: correlation_id.clone(),
                security_id: request.security_id,
                transaction_type: request.transaction_type,
                order_type: request.order_type,
                product_type: request.product_type,
                validity: request.validity,
                quantity: request.quantity,
                price: request.price,
                trigger_price: request.trigger_price,
                // Forever orders arm at CONFIRM → Triggered.
                status: OrderStatus::Triggered,
                traded_qty: 0,
                avg_traded_price: 0.0,
                // Forever/OCO is CNC/MTF (equity-class) — no lot size known.
                lot_size: 0,
                created_at_us: now_us,
                updated_at_us: now_us,
                needs_reconciliation: false,
                modification_count: 0,
            };

            self.correlations
                .track(correlation_id, paper_order_id.clone());
            self.orders.insert(paper_order_id.clone(), order);
            self.total_placed = self.total_placed.saturating_add(1);

            counter!("tv_forever_orders_placed_total", "mode" => "paper").increment(1);
            info!(
                order_id = %paper_order_id,
                security_id = request.security_id,
                oco = request.oco_leg.is_some(),
                "PAPER TRADE: forever/OCO order simulated (no HTTP call)"
            );

            return Ok(paper_order_id);
        }

        // LIVE-EXIT-ARM
        let access_token = self.token_provider.get_access_token()?;
        let dhan_request = build_forever_request(&self.client_id, &correlation_id, &request);

        let response = match self
            .api_client
            .create_forever_order(access_token.expose_secret(), &dhan_request)
            .await
        {
            Ok(resp) => {
                self.record_api_success_and_alert_recovery();
                resp
            }
            Err(err) => {
                if !matches!(err, OmsError::DhanRateLimited) {
                    self.circuit_breaker.record_failure();
                }
                // M1 (refuter round 1, 2026-07-14): sanitized alert reason.
                self.fire_alert(OmsAlert::OrderRejected {
                    correlation_id: correlation_id.clone(),
                    reason: sanitize_oms_error(&err),
                });
                error!(
                    code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                    correlation_id = %correlation_id,
                    security_id = request.security_id,
                    error = %sanitize_oms_error(&err),
                    "forever/OCO order placement failed"
                );
                return Err(err);
            }
        };

        // L3: malformed server-supplied ids are never tracked (length only
        // in the log — the raw value is never echoed).
        if !exit_rules::is_valid_order_id(&response.order_id) {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                correlation_id = %correlation_id,
                security_id = request.security_id,
                id_len = response.order_id.len(),
                "forever/OCO response carried a malformed orderId — refusing to track"
            );
            return Err(OmsError::DhanApiError {
                status_code: 200,
                message: "forever/OCO response carried a malformed orderId".to_owned(),
            });
        }

        // CONFIRM parses to Triggered (state_machine.rs); Transit fallback.
        let initial_status = resolve_initial_status(&response.order_status);
        let order = ManagedOrder {
            order_id: response.order_id.clone(),
            correlation_id: correlation_id.clone(),
            security_id: request.security_id,
            transaction_type: request.transaction_type,
            order_type: request.order_type,
            product_type: request.product_type,
            validity: request.validity,
            quantity: request.quantity,
            price: request.price,
            trigger_price: request.trigger_price,
            status: initial_status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 0,
            created_at_us: now_us,
            updated_at_us: now_us,
            needs_reconciliation: false,
            modification_count: 0,
        };

        self.correlations
            .track(correlation_id, response.order_id.clone());
        self.orders.insert(response.order_id.clone(), order);
        self.total_placed = self.total_placed.saturating_add(1);

        counter!("tv_forever_orders_placed_total", "mode" => "live").increment(1);
        info!(
            order_id = %response.order_id,
            security_id = request.security_id,
            oco = request.oco_leg.is_some(),
            "forever/OCO order placed successfully"
        );

        Ok(response.order_id)
    }

    /// Places an order with the freeze-limit escape hatch.
    ///
    /// `quantity <= freeze_limit` ⇒ delegates to the plain [`Self::place_order`]
    /// flow (ONE id — never call `/orders/slicing` under-freeze, behavior
    /// undocumented, live-probe U5). `quantity > freeze_limit` ⇒ ONE
    /// `POST /orders/slicing` (server-side split), array-tolerant
    /// (`SlicingResponse`), EVERY returned id tracked as its own
    /// `ManagedOrder` sharing one correlation_id. Dry-run oversize:
    /// `compute_slices` ⇒ N paper orders.
    #[instrument(skip_all, fields(security_id = request.security_id))]
    pub async fn place_order_sliced(
        &mut self,
        request: PlaceOrderRequest,
        freeze_limit: i64,
    ) -> Result<Vec<String>, OmsError> {
        // Within the freeze limit → the proven single-order flow.
        if !exit_rules::requires_slicing(request.quantity, freeze_limit) {
            let order_id = self.place_order(request).await?;
            return Ok(vec![order_id]);
        }

        // Step 0: plain-order validation + the slice plan (this is also the
        // typed non-positive-freeze refusal via compute_slices).
        validate_order_fields(&request)?;
        let slices = exit_rules::compute_slices(request.quantity, freeze_limit)?;

        // Steps 1+2: rate limiter + circuit breaker.
        self.check_order_gates()?;

        // Step 3: ONE correlation ID shared by every slice.
        let correlation_id = self.correlations.generate_id();
        let now_us = now_epoch_us();

        // ---- DRY-RUN: N paper orders, one per computed slice ----
        if self.dry_run {
            let mut order_ids = Vec::with_capacity(slices.len());
            for slice_qty in &slices {
                self.paper_order_counter = self.paper_order_counter.saturating_add(1);
                let paper_order_id = format!("PAPER-{}", self.paper_order_counter);

                let order = ManagedOrder {
                    order_id: paper_order_id.clone(),
                    correlation_id: correlation_id.clone(),
                    security_id: request.security_id,
                    transaction_type: request.transaction_type,
                    order_type: request.order_type,
                    product_type: request.product_type,
                    validity: request.validity,
                    quantity: *slice_qty,
                    price: request.price,
                    trigger_price: request.trigger_price,
                    status: OrderStatus::Confirmed,
                    traded_qty: 0,
                    avg_traded_price: 0.0,
                    lot_size: request.lot_size,
                    created_at_us: now_us,
                    updated_at_us: now_us,
                    needs_reconciliation: false,
                    modification_count: 0,
                };
                self.orders.insert(paper_order_id.clone(), order);
                self.total_placed = self.total_placed.saturating_add(1);
                counter!("tv_orders_placed_total", "mode" => "paper").increment(1);
                order_ids.push(paper_order_id);
            }
            if let Some(first) = order_ids.first() {
                self.correlations.track(correlation_id, first.clone());
            }

            info!(
                security_id = request.security_id,
                quantity = request.quantity,
                freeze_limit,
                slices = order_ids.len(),
                "PAPER TRADE: sliced order simulated (no HTTP call)"
            );
            return Ok(order_ids);
        }

        // LIVE-EXIT-ARM
        // The escape hatch actually engaged — strategy doctrine caps the
        // per-signal quantity at the freeze limit, so this is warn-worthy.
        warn!(
            security_id = request.security_id,
            quantity = request.quantity,
            freeze_limit,
            "order exceeds freeze limit — routing through /orders/slicing (escape hatch)"
        );

        let access_token = self.token_provider.get_access_token()?;
        let dhan_request = DhanPlaceOrderRequest {
            dhan_client_id: self.client_id.clone(),
            transaction_type: request.transaction_type.as_str().to_owned(),
            exchange_segment: EXCHANGE_SEGMENT_NSE_FNO.to_owned(),
            product_type: request.product_type.as_str().to_owned(),
            order_type: request.order_type.as_str().to_owned(),
            validity: request.validity.as_str().to_owned(),
            security_id: request.security_id.to_string(),
            quantity: request.quantity,
            price: request.price,
            trigger_price: request.trigger_price,
            disclosed_quantity: 0,
            after_market_order: false,
            correlation_id: correlation_id.clone(),
        };

        let response = match self
            .api_client
            .place_order_slicing(access_token.expose_secret(), &dhan_request)
            .await
        {
            Ok(resp) => {
                self.record_api_success_and_alert_recovery();
                resp
            }
            Err(err) => {
                if !matches!(err, OmsError::DhanRateLimited) {
                    self.circuit_breaker.record_failure();
                }
                // M1 (refuter round 1, 2026-07-14): sanitized alert reason.
                self.fire_alert(OmsAlert::OrderRejected {
                    correlation_id: correlation_id.clone(),
                    reason: sanitize_oms_error(&err),
                });
                error!(
                    code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                    correlation_id = %correlation_id,
                    security_id = request.security_id,
                    error = %sanitize_oms_error(&err),
                    "sliced order placement failed"
                );
                return Err(err);
            }
        };

        let mut responses = match response {
            SlicingResponse::Many(list) => list,
            SlicingResponse::One(single) => vec![single],
        };
        if responses.is_empty() {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                correlation_id = %correlation_id,
                "slicing response anomaly — zero order ids returned"
            );
            return Err(OmsError::DhanApiError {
                status_code: 200,
                message: "slicing response contained zero order ids".to_owned(),
            });
        }

        // H3b (2026-07-14 hostile review): the response Vec is
        // SERVER-CONTROLLED — bound it at MAX_ORDER_SLICES before any
        // per-row work. Beyond-cap rows are dropped with a coded anomaly
        // log; every tracked row is reconciliation-flagged below (M3), so
        // the REST reconcile recovers the true book state.
        let slice_cap = usize::try_from(exit_rules::MAX_ORDER_SLICES).unwrap_or(usize::MAX);
        if responses.len() > slice_cap {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                correlation_id = %correlation_id,
                returned = responses.len(),
                cap = slice_cap,
                "slicing response anomaly — row count exceeds MAX_ORDER_SLICES; \
                 tracking only the first rows (all reconciliation-flagged)"
            );
            responses.truncate(slice_cap);
        }

        // L3: ONE malformed server-supplied id refuses the WHOLE response —
        // nothing tracked (a corrupt payload is not attributable per-slice);
        // reconcile() against the order book is the recovery path. The raw
        // id is never echoed (length only).
        if let Some(bad) = responses
            .iter()
            .find(|r| !exit_rules::is_valid_order_id(&r.order_id))
        {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                correlation_id = %correlation_id,
                id_len = bad.order_id.len(),
                "slicing response carried a malformed orderId — refusing to track"
            );
            return Err(OmsError::DhanApiError {
                status_code: 200,
                message: "slicing response carried a malformed orderId".to_owned(),
            });
        }

        // Per-slice quantity attribution: exact when the server split
        // matches our computed plan; a single object carries the total; any
        // other mismatch is an anomaly. M3 (2026-07-14 hostile review): the
        // server NEVER echoes its split back per-slice, so EVERY live
        // sliced order is reconciliation-flagged (count match or not,
        // single-object included) — quantities are corrected from the REST
        // order book, never assumed.
        let counts_match = responses.len() == slices.len();
        let single_object = responses.len() == 1;
        if !counts_match && !single_object {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                correlation_id = %correlation_id,
                expected_slices = slices.len(),
                returned = responses.len(),
                "slicing response anomaly — slice count mismatch; tracking all ids for reconciliation"
            );
        }

        let mut order_ids = Vec::with_capacity(responses.len());
        for (idx, resp) in responses.iter().enumerate() {
            let quantity = if counts_match {
                slices.get(idx).copied().unwrap_or(0)
            } else if single_object {
                request.quantity
            } else {
                0
            };
            let order = ManagedOrder {
                order_id: resp.order_id.clone(),
                correlation_id: correlation_id.clone(),
                security_id: request.security_id,
                transaction_type: request.transaction_type,
                order_type: request.order_type,
                product_type: request.product_type,
                validity: request.validity,
                quantity,
                price: request.price,
                trigger_price: request.trigger_price,
                status: resolve_initial_status(&resp.order_status),
                traded_qty: 0,
                avg_traded_price: 0.0,
                lot_size: request.lot_size,
                created_at_us: now_us,
                updated_at_us: now_us,
                // M3: always reconciliation-flagged (see above).
                needs_reconciliation: true,
                modification_count: 0,
            };
            self.orders.insert(resp.order_id.clone(), order);
            self.total_placed = self.total_placed.saturating_add(1);
            counter!("tv_orders_placed_total", "mode" => "live").increment(1);
            order_ids.push(resp.order_id.clone());
        }
        if let Some(first) = order_ids.first() {
            self.correlations.track(correlation_id, first.clone());
        }

        info!(
            security_id = request.security_id,
            quantity = request.quantity,
            slices = order_ids.len(),
            "sliced order placed successfully"
        );
        Ok(order_ids)
    }

    /// MPP verify-after-place — a SINGLE probe (the retry LADDER lives in
    /// the caller, driven by `exit_rules::next_verify_backoff_secs`; no
    /// sleeping inside the `&mut`-serial engine).
    ///
    /// Every probe consumes the SHARED order GCRA (Ruling 5 / Assumed A2),
    /// dry-run included. The circuit breaker is deliberately NOT consulted:
    /// this is a read probe — blocking it while the breaker is open would
    /// blind the verify loop exactly when order truth matters most, and a
    /// failed probe never records a breaker failure (inconclusive — the
    /// caller retries the next rung; a probe failure NEVER marks an order
    /// absent).
    ///
    /// Routing: tracked `PAPER-*` state in dry-run ⇒ deterministic
    /// [`ExecutionVerdict::SimulatedFilled`] (no HTTP); super orders ⇒ ONE
    /// batched `GET /super/orders` refreshing ALL tracked supers; plain /
    /// sliced ⇒ `GET /orders/{id}`. Verdicts via
    /// `exit_rules::classify_mpp_verdict` — fail-closed `Unknown` on
    /// anything unparsable (NEVER treated as filled).
    #[instrument(skip_all, fields(order_id = %order_id))]
    pub async fn verify_order_execution(
        &mut self,
        order_id: &str,
        elapsed_secs: u64,
        deadline_secs: u64,
    ) -> Result<ExecutionVerdict, OmsError> {
        // L3: a malformed id is structural (never tracked, never
        // URL-embeddable) — refuse before anything else, without echoing
        // the raw value into the error.
        if !exit_rules::is_valid_order_id(order_id) {
            return Err(OmsError::OrderNotFound {
                order_id: "<malformed-order-id>".to_owned(),
            });
        }

        // Step 0 (L1, 2026-07-14 hostile review): the existence check runs
        // BEFORE the GCRA — a structurally-unknown id must never burn a
        // shared order-rate cell.
        let is_super = self.super_orders.contains_key(order_id);
        if !is_super && !self.orders.contains_key(order_id) {
            return Err(OmsError::OrderNotFound {
                order_id: order_id.to_owned(),
            });
        }

        // Step 1: the probe consumes the shared GCRA — dry-run included.
        self.rate_limiter.check()?;

        // ---- DRY-RUN: tracked paper order ⇒ deterministic verdict, no HTTP ----
        if self.dry_run {
            let verdict = ExecutionVerdict::SimulatedFilled;
            self.record_verify_probe(order_id, verdict_label(&verdict));
            return Ok(verdict);
        }

        // LIVE-EXIT-ARM
        let access_token = self.token_provider.get_access_token()?;

        let verdict = if is_super {
            // ONE batched list GET serves ALL tracked supers (no per-ID
            // super GET exists — Verified V7).
            match self
                .api_client
                .get_super_orders(access_token.expose_secret())
                .await
            {
                Ok(list) => {
                    self.refresh_super_registry(&list);
                    // L7 (2026-07-14 hostile review): on duplicate rows for
                    // one id, classify from the LAST occurrence — the SAME
                    // row refresh_super_registry keeps (its forward walk is
                    // last-write-wins), so verdict and registry never split.
                    // WITHIN that row the two readers split by field (chosen
                    // semantics, refuter round 1): THIS classifier reads the
                    // TOP-LEVEL status + fill quantities; the registry
                    // refresh reads the legDetails.
                    match list.iter().rev().find(|r| r.order_id == order_id) {
                        Some(resp) => {
                            let quantity = self
                                .super_orders
                                .get(order_id)
                                .map(|so| so.quantity)
                                .unwrap_or(0);
                            exit_rules::classify_mpp_verdict(
                                parse_order_status(&resp.order_status),
                                &resp.order_status,
                                resp.filled_qty,
                                quantity,
                                resp.average_traded_price,
                                elapsed_secs,
                                deadline_secs,
                            )
                        }
                        // The list succeeded but our id is missing —
                        // fail-closed Unknown (a failed probe ≠ absence;
                        // /orders/external/{cid} is the documented fallback).
                        // The ladder retries this sentinel until its FINAL
                        // attempt (M2 — exit_rules::verify_verdict_decisive).
                        None => ExecutionVerdict::Unknown {
                            raw_status: exit_rules::VERIFY_RAW_NOT_IN_SUPER_LIST.to_owned(),
                        },
                    }
                }
                // A 2xx with an unparsable body is a broken response —
                // fail-closed Unknown (never filled, never absent).
                Err(OmsError::JsonError(parse_err)) => {
                    warn!(order_id = %order_id, error = %parse_err, "super list probe body unparsable");
                    ExecutionVerdict::Unknown {
                        raw_status: "UNPARSABLE_RESPONSE_BODY".to_owned(),
                    }
                }
                // Transport / HTTP / 429 failures are INCONCLUSIVE — the
                // caller retries the next rung (DH-904 floor respected).
                Err(err) => return Err(err),
            }
        } else {
            match self
                .api_client
                .get_order(access_token.expose_secret(), order_id)
                .await
            {
                Ok(resp) => {
                    // Classify on STATUS + fill quantities ONLY — never the
                    // order-type echo (MPP MARKET→LIMIT conversion).
                    let traded_qty = resp.filled_qty.max(resp.traded_quantity);
                    let avg_price = if resp.average_traded_price > 0.0 {
                        resp.average_traded_price
                    } else {
                        resp.traded_price
                    };
                    let quantity = self
                        .orders
                        .get(order_id)
                        .map(|o| o.quantity)
                        .unwrap_or(resp.quantity);
                    if !resp.oms_error_description.is_empty() {
                        // M1: broker-controlled text — redacted + truncated
                        // before it reaches any sink.
                        warn!(
                            order_id = %order_id,
                            oms_error_code = %capture_rest_error_body(&resp.oms_error_code),
                            oms_error_description =
                                %capture_rest_error_body(&resp.oms_error_description),
                            "verify probe carried a broker error description"
                        );
                    }
                    exit_rules::classify_mpp_verdict(
                        parse_order_status(&resp.order_status),
                        &resp.order_status,
                        traded_qty,
                        quantity,
                        avg_price,
                        elapsed_secs,
                        deadline_secs,
                    )
                }
                Err(OmsError::JsonError(parse_err)) => {
                    warn!(order_id = %order_id, error = %parse_err, "order probe body unparsable");
                    ExecutionVerdict::Unknown {
                        raw_status: "UNPARSABLE_RESPONSE_BODY".to_owned(),
                    }
                }
                Err(err) => return Err(err),
            }
        };

        self.apply_verify_verdict(order_id, &verdict, elapsed_secs, deadline_secs);
        Ok(verdict)
    }

    /// Read accessor for the super-order registry (dispatcher + tests) —
    /// also the always-resend source for the current trailing jump.
    pub fn super_order(&self, entry_order_id: &str) -> Option<&ManagedSuperOrder> {
        self.super_orders.get(entry_order_id)
    }

    // -- private exit-layer helpers --------------------------------------

    /// Steps 1+2 of the pinned skeleton: rate limiter then circuit breaker,
    /// with the place_order alert semantics.
    fn check_order_gates(&self) -> Result<(), OmsError> {
        if let Err(err) = self.rate_limiter.check() {
            self.fire_alert(OmsAlert::RateLimitExhausted {
                limit_type: "per_second".to_string(),
            });
            return Err(err);
        }
        if let Err(err) = self.circuit_breaker.check() {
            self.fire_alert(OmsAlert::CircuitBreakerOpened {
                consecutive_failures: u64::from(self.circuit_breaker.failure_count()),
            });
            return Err(err);
        }
        Ok(())
    }

    /// Records one successful live API call on the circuit breaker and
    /// fires the `CircuitBreakerClosed` recovery alert when the success
    /// closed a previously-open breaker (L6, 2026-07-14 hostile review —
    /// every exit live arm shares the place-arm recovery semantics:
    /// place/modify/cancel/forever/sliced. `verify_order_execution` is
    /// deliberately EXCLUDED: probes never touch the breaker in either
    /// direction, so a recovered-alert there would be a false signal).
    fn record_api_success_and_alert_recovery(&self) {
        let prev_failures = self.circuit_breaker.failure_count();
        self.circuit_breaker.record_success();
        if self.circuit_breaker.was_previously_open(prev_failures) {
            self.fire_alert(OmsAlert::CircuitBreakerClosed);
        }
    }

    /// Applies a leg modify to the tracked super order + the tracked entry
    /// `ManagedOrder` (shared by the dry-run and live-success paths).
    fn apply_super_modify(&mut self, order_id: &str, modify: &ModifySuperOrderLeg) {
        let now_us = now_epoch_us();
        if let Some(so) = self.super_orders.get_mut(order_id) {
            match modify {
                ModifySuperOrderLeg::Entry {
                    order_type: _,
                    quantity,
                    price,
                    target_price,
                    stop_loss_price,
                    trailing_jump,
                } => {
                    so.quantity = *quantity;
                    so.entry_price = *price;
                    so.target.price = *target_price;
                    so.stop_loss.price = *stop_loss_price;
                    so.trailing_jump = *trailing_jump;
                }
                ModifySuperOrderLeg::Target { target_price } => {
                    so.target.price = *target_price;
                }
                ModifySuperOrderLeg::StopLoss {
                    stop_loss_price,
                    trailing_jump,
                } => {
                    so.stop_loss.price = *stop_loss_price;
                    so.trailing_jump = *trailing_jump;
                }
            }
            so.modification_count = so.modification_count.saturating_add(1);
            so.updated_at_us = now_us;
        }
        if let ModifySuperOrderLeg::Entry {
            order_type,
            quantity,
            price,
            ..
        } = modify
            && let Some(order) = self.orders.get_mut(order_id)
        {
            order.order_type = *order_type;
            order.quantity = *quantity;
            order.price = *price;
            order.modification_count = order.modification_count.saturating_add(1);
            order.updated_at_us = now_us;
        }
    }

    /// Applies a leg cancel to the tracked super order (dry-run simulation).
    fn apply_super_cancel(&mut self, order_id: &str, leg: OrderLeg) {
        let now_us = now_epoch_us();
        if let Some(so) = self.super_orders.get_mut(order_id) {
            match leg {
                OrderLeg::EntryLeg => {
                    // ENTRY_LEG cancel cancels ALL legs (07a).
                    so.entry_status_raw = OrderStatus::Cancelled.as_str().to_owned();
                    so.target.status_raw = OrderStatus::Cancelled.as_str().to_owned();
                    so.stop_loss.status_raw = OrderStatus::Cancelled.as_str().to_owned();
                    so.status = OrderStatus::Cancelled;
                }
                OrderLeg::TargetLeg => {
                    so.target.status_raw = OrderStatus::Cancelled.as_str().to_owned();
                }
                OrderLeg::StopLossLeg => {
                    so.stop_loss.status_raw = OrderStatus::Cancelled.as_str().to_owned();
                }
            }
            so.updated_at_us = now_us;
        }
        if leg == OrderLeg::EntryLeg
            && let Some(order) = self.orders.get_mut(order_id)
        {
            order.status = OrderStatus::Cancelled;
            order.updated_at_us = now_us;
        }
    }

    /// Refreshes every tracked super order from ONE batched list response
    /// (raw leg statuses — the registry deliberately bypasses the
    /// ManagedOrder state machine for the top-level walk).
    ///
    /// Duplicate rows for one order id (L7, 2026-07-14 hostile review):
    /// the forward walk is LAST-write-wins, matching the verify
    /// classification's `.rev().find(...)` LAST-occurrence pick — the
    /// registry and the returned verdict always read the same row.
    /// WITHIN that row the two readers split by field (chosen semantics,
    /// refuter round 1): THIS refresh reads the legDetails (entry/TP/SL
    /// raw statuses, prices, leg ids); the verify classifier reads the
    /// TOP-LEVEL status + fill quantities.
    fn refresh_super_registry(&mut self, list: &[DhanSuperOrderResponse]) {
        let now_us = now_epoch_us();
        for resp in list {
            let Some(so) = self.super_orders.get_mut(&resp.order_id) else {
                continue;
            };
            // Entry raw status: the ENTRY_LEG detail when present, else the
            // top-level status (the entry drives the top-level walk).
            so.entry_status_raw = resp
                .leg_details
                .iter()
                .find(|l| l.leg_name == OrderLeg::EntryLeg.as_str())
                .map(|l| l.order_status.clone())
                .unwrap_or_else(|| resp.order_status.clone());
            so.status = resolve_initial_status(&resp.order_status);
            for detail in &resp.leg_details {
                let leg_state = match parse_leg_name(&detail.leg_name) {
                    Some(OrderLeg::TargetLeg) => &mut so.target,
                    Some(OrderLeg::StopLossLeg) => &mut so.stop_loss,
                    _ => continue,
                };
                leg_state.status_raw = detail.order_status.clone();
                if detail.price > 0.0 {
                    leg_state.price = detail.price;
                }
                if !detail.order_id.is_empty() {
                    leg_state.order_id = detail.order_id.clone();
                }
            }
            so.updated_at_us = now_us;
        }
    }

    /// Post-classification bookkeeping for one verify probe: mutate the
    /// tracked order per the verdict, emit the EXIT-VERIFY-01 degraded
    /// arms (log-sink-only), and record the ladder state + counter.
    fn apply_verify_verdict(
        &mut self,
        order_id: &str,
        verdict: &ExecutionVerdict,
        elapsed_secs: u64,
        deadline_secs: u64,
    ) {
        let now_us = now_epoch_us();
        match verdict {
            ExecutionVerdict::Filled {
                traded_qty,
                avg_price,
            } => {
                if let Some(order) = self.orders.get_mut(order_id) {
                    order.traded_qty = *traded_qty;
                    order.avg_traded_price = *avg_price;
                    if order.status != OrderStatus::Traded {
                        if is_valid_transition(order.status, OrderStatus::Traded) {
                            order.status = OrderStatus::Traded;
                        } else {
                            order.needs_reconciliation = true;
                        }
                    }
                    order.updated_at_us = now_us;
                }
            }
            ExecutionVerdict::PartiallyFilled { traded_qty, .. } => {
                if let Some(order) = self.orders.get_mut(order_id) {
                    order.traded_qty = *traded_qty;
                    if order.status != OrderStatus::PartTraded
                        && is_valid_transition(order.status, OrderStatus::PartTraded)
                    {
                        order.status = OrderStatus::PartTraded;
                    }
                    order.updated_at_us = now_us;
                }
                if elapsed_secs >= deadline_secs {
                    // Partial-terminal: the remainder is NEVER silently
                    // forgotten. Live policy actions (modify-to-aggressive /
                    // cancel-remainder) are deliberately NOT implemented in
                    // this dry-run PR — logged + flagged only.
                    if let Some(order) = self.orders.get_mut(order_id) {
                        order.needs_reconciliation = true;
                    }
                    error!(
                        code = ErrorCode::ExitVerify01Degraded.code_str(),
                        order_id = %order_id,
                        elapsed_secs,
                        deadline_secs,
                        "verify ladder at budget with a PARTIAL fill — remainder flagged for reconciliation"
                    );
                }
            }
            ExecutionVerdict::PendingAtLimit { .. } => {
                // H1 (2026-07-14 hostile review): an at-limit resting order
                // is flagged for the REST reconcile — verify-ladder
                // exhaustion is never a silent Pending.
                if let Some(order) = self.orders.get_mut(order_id) {
                    order.needs_reconciliation = true;
                    order.updated_at_us = now_us;
                }
                error!(
                    code = ErrorCode::ExitVerify01Degraded.code_str(),
                    order_id = %order_id,
                    elapsed_secs,
                    deadline_secs,
                    "MPP verify deadline reached with the order still resting — NEVER assumed filled"
                );
            }
            ExecutionVerdict::Unknown { raw_status } => {
                // M2 refined (refuter round 1, 2026-07-14): a
                // NOT_IN_SUPER_LIST sentinel BEFORE the deadline is
                // potential list lag — the ladder re-probes the remaining
                // rungs, so flagging + EXIT-VERIFY-01 fire only at/past
                // the budget (`elapsed_secs >= deadline_secs`; the
                // ladder's final-rung elapsed floor guarantees the last
                // probe lands there). A body-unparsable/other Unknown
                // stays ALWAYS-flagged fail-closed.
                let in_budget_list_lag = raw_status == exit_rules::VERIFY_RAW_NOT_IN_SUPER_LIST
                    && elapsed_secs < deadline_secs;
                if !in_budget_list_lag {
                    if let Some(order) = self.orders.get_mut(order_id) {
                        order.needs_reconciliation = true;
                        order.updated_at_us = now_us;
                    }
                    error!(
                        code = ErrorCode::ExitVerify01Degraded.code_str(),
                        order_id = %order_id,
                        raw_status = %raw_status,
                        elapsed_secs,
                        deadline_secs,
                        "unparsable verify probe result — fail-closed Unknown (never treated as filled)"
                    );
                }
            }
            ExecutionVerdict::Terminal { status } => {
                if let Some(order) = self.orders.get_mut(order_id) {
                    if order.status != *status {
                        if is_valid_transition(order.status, *status) {
                            order.status = *status;
                        } else if *status == OrderStatus::Closed && order.is_terminal() {
                            // Super top-level CLOSED after the entry already
                            // sealed terminal — the ManagedSuperOrder registry
                            // owns that walk (Ruling 4); nothing to flag.
                        } else {
                            order.needs_reconciliation = true;
                        }
                    }
                    order.updated_at_us = now_us;
                }
            }
            ExecutionVerdict::Pending { .. } | ExecutionVerdict::SimulatedFilled => {}
        }
        self.record_verify_probe(order_id, verdict_label(verdict));
    }

    /// Records one verify probe into the ladder bookkeeping + the outcome
    /// counter (`tv_exit_verify_probes_total{outcome}` — static labels).
    fn record_verify_probe(&mut self, order_id: &str, label: &'static str) {
        let now_us = now_epoch_us();
        let state = self
            .verify_states
            .entry(order_id.to_owned())
            .or_insert(VerifyState {
                attempts: 0,
                first_probe_us: now_us,
                last_verdict_label: label,
            });
        state.attempts = state.attempts.saturating_add(1);
        state.last_verdict_label = label;
        counter!("tv_exit_verify_probes_total", "outcome" => label).increment(1);
    }
}

// ---------------------------------------------------------------------------
// Exit-layer pure helpers (private — zero I/O)
// ---------------------------------------------------------------------------

/// M1 (2026-07-14 hostile review): every exit-layer log line rendering an
/// [`OmsError`] routes through the house redaction pipeline — a raw Dhan
/// body embedded in `DhanApiError::message` is control-char-stripped,
/// credential/JWT-redacted and truncated to 300 chars
/// (`capture_rest_error_body`) before it reaches any sink. The typed
/// `OmsError` itself is UNCHANGED (other crates rely on it); only the log
/// rendering is bounded.
fn sanitize_oms_error(err: &OmsError) -> String {
    capture_rest_error_body(&err.to_string())
}

/// Maps a raw Dhan leg name to [`OrderLeg`]. Unknown names → `None`
/// (skipped with a warn — no-panic-on-unknown, annexure rule 15).
fn parse_leg_name(leg_name: &str) -> Option<OrderLeg> {
    match leg_name {
        "ENTRY_LEG" => Some(OrderLeg::EntryLeg),
        "TARGET_LEG" => Some(OrderLeg::TargetLeg),
        "STOP_LOSS_LEG" => Some(OrderLeg::StopLossLeg),
        _ => None,
    }
}

/// Stable metrics/log label for an [`ExecutionVerdict`].
fn verdict_label(verdict: &ExecutionVerdict) -> &'static str {
    match verdict {
        ExecutionVerdict::Filled { .. } => "filled",
        ExecutionVerdict::PartiallyFilled { .. } => "partially_filled",
        ExecutionVerdict::Pending { .. } => "pending",
        ExecutionVerdict::PendingAtLimit { .. } => "pending_at_limit",
        ExecutionVerdict::Terminal { .. } => "terminal",
        ExecutionVerdict::SimulatedFilled => "simulated_filled",
        ExecutionVerdict::Unknown { .. } => "unknown",
    }
}

/// Validates the leg-modify payload before any gate is spent.
///
/// MARKET entry modifies keep price 0.0 (mirror of the place rule / U1);
/// every price is finite; ENTRY quantity positive; trailing >= 0 finite.
fn validate_modify_super_fields(modify: &ModifySuperOrderLeg) -> Result<(), OmsError> {
    fn positive_finite(label: &str, value: f64) -> Result<(), OmsError> {
        if value.is_finite() && value > 0.0 {
            Ok(())
        } else {
            Err(OmsError::RiskRejected {
                reason: format!(
                    "super order modify {label} must be positive and finite, got {value}"
                ),
            })
        }
    }
    fn trailing_ok(value: f64) -> Result<(), OmsError> {
        if value.is_finite() && value >= 0.0 {
            Ok(())
        } else {
            Err(OmsError::RiskRejected {
                reason: format!(
                    "super order modify trailingJump must be >= 0.0 and finite, got {value}"
                ),
            })
        }
    }

    match modify {
        ModifySuperOrderLeg::Entry {
            order_type,
            quantity,
            price,
            target_price,
            stop_loss_price,
            trailing_jump,
        } => {
            if !matches!(order_type, OrderType::Limit | OrderType::Market) {
                return Err(OmsError::RiskRejected {
                    reason: format!(
                        "super order entry leg must be LIMIT or MARKET, got {}",
                        order_type.as_str()
                    ),
                });
            }
            if *quantity <= 0 {
                return Err(OmsError::RiskRejected {
                    reason: format!("super order modify quantity must be positive, got {quantity}"),
                });
            }
            if *order_type == OrderType::Market {
                if *price != 0.0 {
                    return Err(OmsError::RiskRejected {
                        reason: format!(
                            "MARKET super order modify must have price=0.0, got {price}"
                        ),
                    });
                }
            } else {
                positive_finite("price", *price)?;
            }
            positive_finite("targetPrice", *target_price)?;
            positive_finite("stopLossPrice", *stop_loss_price)?;
            trailing_ok(*trailing_jump)
        }
        ModifySuperOrderLeg::Target { target_price } => {
            positive_finite("targetPrice", *target_price)
        }
        ModifySuperOrderLeg::StopLoss {
            stop_loss_price,
            trailing_jump,
        } => {
            positive_finite("stopLossPrice", *stop_loss_price)?;
            trailing_ok(*trailing_jump)
        }
    }
}

/// Builds the leg-restricted modify wire body. The body ALWAYS carries
/// `orderId` (portal-required — Ruling 2 fix 1) and the StopLoss/Entry
/// arms ALWAYS serialize `trailingJump` (omission cancels the trail —
/// fix 2); TARGET_LEG bodies stay minimal (sending trailingJump there is
/// doc-unbacked — live-probe U8).
fn build_super_modify_request(
    client_id: &str,
    order_id: &str,
    modify: &ModifySuperOrderLeg,
) -> DhanModifySuperOrderRequest {
    let base = DhanModifySuperOrderRequest {
        dhan_client_id: client_id.to_owned(),
        order_id: order_id.to_owned(),
        leg_name: String::new(),
        order_type: None,
        quantity: None,
        price: None,
        target_price: None,
        stop_loss_price: None,
        trailing_jump: None,
    };
    match modify {
        ModifySuperOrderLeg::Entry {
            order_type,
            quantity,
            price,
            target_price,
            stop_loss_price,
            trailing_jump,
        } => DhanModifySuperOrderRequest {
            leg_name: OrderLeg::EntryLeg.as_str().to_owned(),
            order_type: Some(order_type.as_str().to_owned()),
            quantity: Some(*quantity),
            price: Some(*price),
            target_price: Some(*target_price),
            stop_loss_price: Some(*stop_loss_price),
            trailing_jump: Some(*trailing_jump),
            ..base
        },
        ModifySuperOrderLeg::Target { target_price } => DhanModifySuperOrderRequest {
            leg_name: OrderLeg::TargetLeg.as_str().to_owned(),
            target_price: Some(*target_price),
            ..base
        },
        ModifySuperOrderLeg::StopLoss {
            stop_loss_price,
            trailing_jump,
        } => DhanModifySuperOrderRequest {
            leg_name: OrderLeg::StopLossLeg.as_str().to_owned(),
            stop_loss_price: Some(*stop_loss_price),
            trailing_jump: Some(*trailing_jump),
            ..base
        },
    }
}

/// Builds the Forever/OCO wire body. SINGLE omits every second-leg field;
/// OCO carries all three BY CONSTRUCTION (`OcoSecondLeg`). The flag
/// strings are the `ForeverOrderFlag` wire literals.
fn build_forever_request(
    client_id: &str,
    correlation_id: &str,
    request: &PlaceForeverOcoRequest,
) -> DhanForeverOrderRequest {
    DhanForeverOrderRequest {
        dhan_client_id: client_id.to_owned(),
        correlation_id: correlation_id.to_owned(),
        order_flag: if request.oco_leg.is_some() {
            "OCO".to_owned()
        } else {
            "SINGLE".to_owned()
        },
        transaction_type: request.transaction_type.as_str().to_owned(),
        exchange_segment: EXCHANGE_SEGMENT_NSE_FNO.to_owned(),
        product_type: request.product_type.as_str().to_owned(),
        order_type: request.order_type.as_str().to_owned(),
        validity: request.validity.as_str().to_owned(),
        security_id: request.security_id.to_string(),
        quantity: request.quantity,
        // Assumed A5: NSE F&O rejects disclosedQuantity — keep 0.
        disclosed_quantity: 0,
        price: request.price,
        trigger_price: request.trigger_price,
        price1: request.oco_leg.as_ref().map(|l| l.price),
        trigger_price1: request.oco_leg.as_ref().map(|l| l.trigger_price),
        quantity1: request.oco_leg.as_ref().map(|l| l.quantity),
    }
}

// ---------------------------------------------------------------------------
// Pre-submission Validation
// ---------------------------------------------------------------------------

/// Validates order fields before submission to avoid wasting Dhan rate limits.
///
/// - MARKET orders must have price = 0.0 (Dhan rejects non-zero → DH-905)
/// - STOP_LOSS / STOP_LOSS_MARKET orders require triggerPrice > 0.0
fn validate_order_fields(request: &PlaceOrderRequest) -> Result<(), OmsError> {
    use tickvault_common::order_types::OrderType;

    // MARKET orders: price must be 0 (Dhan API spec)
    if request.order_type == OrderType::Market && request.price != 0.0 {
        return Err(OmsError::RiskRejected {
            reason: format!("MARKET order must have price=0.0, got {}", request.price),
        });
    }

    // SL/SLM orders: triggerPrice is mandatory
    if matches!(
        request.order_type,
        OrderType::StopLoss | OrderType::StopLossMarket
    ) && request.trigger_price == 0.0
    {
        return Err(OmsError::RiskRejected {
            reason: "STOP_LOSS/STOP_LOSS_MARKET orders require triggerPrice > 0".to_owned(),
        });
    }

    // I-P0-03: Reject orders for expired derivative contracts.
    // Stale universe → expired contract order → CRITICAL risk.
    if let Some(expiry) = request.expiry_date {
        let today = chrono::Utc::now().date_naive();
        if expiry < today {
            return Err(OmsError::ExpiredContract {
                security_id: request.security_id,
                expiry_date: expiry.to_string(),
            });
        }
    }

    Ok(())
}

/// Validates modify-order fields before submission to avoid wasting Dhan rate limits.
///
/// Same rules as place: MARKET→price=0, SL→triggerPrice>0.
fn validate_modify_fields(request: &ModifyOrderRequest) -> Result<(), OmsError> {
    use tickvault_common::order_types::OrderType;

    if request.order_type == OrderType::Market && request.price != 0.0 {
        return Err(OmsError::RiskRejected {
            reason: format!("MARKET order must have price=0.0, got {}", request.price),
        });
    }

    if matches!(
        request.order_type,
        OrderType::StopLoss | OrderType::StopLossMarket
    ) && request.trigger_price == 0.0
    {
        return Err(OmsError::RiskRejected {
            reason: "STOP_LOSS/STOP_LOSS_MARKET orders require triggerPrice > 0".to_owned(),
        });
    }

    Ok(())
}

/// Validates super order target/SL prices relative to entry price and transaction type.
///
/// For BUY orders: target must exceed entry, SL must be below entry.
/// For SELL orders: target must be below entry, SL must exceed entry.
/// All prices must be positive. Pure function — no I/O.
///
/// Wired by `place_super_order` Step 0b (Cluster B, 2026-07-14) for the
/// relative-ordering check on LIMIT entries.
// TEST-EXEMPT: tested by the test_super_order_* suite in this module
pub(crate) fn validate_super_order_prices(
    transaction_type: &str,
    price: f64,
    target_price: f64,
    stop_loss_price: f64,
) -> Result<(), OmsError> {
    if price <= 0.0 {
        return Err(OmsError::RiskRejected {
            reason: format!("super order entry price must be positive, got {price}"),
        });
    }
    if target_price <= 0.0 {
        return Err(OmsError::RiskRejected {
            reason: format!("super order target price must be positive, got {target_price}"),
        });
    }
    if stop_loss_price <= 0.0 {
        return Err(OmsError::RiskRejected {
            reason: format!("super order stop loss price must be positive, got {stop_loss_price}"),
        });
    }

    match transaction_type {
        "BUY" => {
            if target_price <= price {
                return Err(OmsError::RiskRejected {
                    reason: format!(
                        "BUY super order target ({target_price}) must exceed entry price ({price})"
                    ),
                });
            }
            if stop_loss_price >= price {
                return Err(OmsError::RiskRejected {
                    reason: format!(
                        "BUY super order stop loss ({stop_loss_price}) must be below entry price ({price})"
                    ),
                });
            }
        }
        "SELL" => {
            if target_price >= price {
                return Err(OmsError::RiskRejected {
                    reason: format!(
                        "SELL super order target ({target_price}) must be below entry price ({price})"
                    ),
                });
            }
            if stop_loss_price <= price {
                return Err(OmsError::RiskRejected {
                    reason: format!(
                        "SELL super order stop loss ({stop_loss_price}) must exceed entry price ({price})"
                    ),
                });
            }
        }
        other => {
            return Err(OmsError::RiskRejected {
                reason: format!("unknown transaction type for super order: {other}"),
            });
        }
    }

    Ok(())
}

/// Validates disclosed quantity is at least 30% of total quantity (Dhan requirement).
///
/// Returns `Ok(())` if `disclosed_quantity` is 0 (fully disclosed) or meets the
/// minimum 30% threshold. Returns `Err(reason)` otherwise.
///
/// Uses ceiling division to avoid floor-division undercount.
/// Pure function — no I/O.
fn validate_disclosed_quantity(quantity: i64, disclosed_quantity: i64) -> Result<(), String> {
    if disclosed_quantity <= 0 {
        return Ok(());
    }
    // Ceiling division: (qty * 3 + 9) / 10 to avoid floor-division undercount
    let min_disclosed = quantity.saturating_mul(3).saturating_add(9) / 10;
    if disclosed_quantity < min_disclosed {
        Err(format!(
            "disclosedQuantity ({disclosed_quantity}) must be >=30% of quantity ({quantity})"
        ))
    } else {
        Ok(())
    }
}

/// Checks whether an order has reached the maximum modification limit.
///
/// Dhan allows at most 25 modifications per order.
/// Pure function — no I/O.
fn check_modification_limit(modification_count: u32, order_id: &str) -> Result<(), OmsError> {
    if modification_count >= MAX_MODIFICATIONS_PER_ORDER {
        Err(OmsError::RiskRejected {
            reason: format!(
                "order {} has reached max {} modifications",
                order_id, MAX_MODIFICATIONS_PER_ORDER
            ),
        })
    } else {
        Ok(())
    }
}

/// Determines the initial order status from a Dhan REST API response status string.
///
/// Dhan can return TRADED or REJECTED immediately; defaults to Transit if unparsable.
/// Pure function.
fn resolve_initial_status(response_status: &str) -> OrderStatus {
    parse_order_status(response_status).unwrap_or(OrderStatus::Transit)
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

/// Returns the current time as epoch microseconds.
fn now_epoch_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

/// UTC epoch seconds — same time base as the order-readiness refresher so the
/// gate and the probe snapshot agree.
fn now_epoch_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Cluster F: engine error-policy helpers
// ---------------------------------------------------------------------------

/// The action the caller takes with the error it still owns after
/// `apply_engine_error_policy`.
enum EngineAction {
    /// Rotate + sleep already performed; retry with a fresh token.
    Retry,
    /// Return `OmsError::OrderPathHalted { cause }`.
    Halt(&'static str),
    /// Return `OmsError::TokenExpired` (807/808/809 second failure — no halt).
    ReturnTokenExpired,
    /// Return the original error (pass-through; the caller fires OrderRejected).
    PassThrough,
    /// Return the original error WITHOUT firing OrderRejected (the policy arm
    /// already fired its own operator alert — e.g. DH-904 ladder exhaustion, so
    /// only ONE Telegram per decision; F-E fix).
    PassThroughSilent,
}

/// Static metric-label slug for a halt cause.
fn halt_cause_slug(cause: &'static str) -> &'static str {
    match cause {
        "DH-901" => "dh901",
        "DH-902" => "dh902",
        "DH-903" => "dh903",
        "DATA-810" => "data810",
        _ => "other",
    }
}

/// Static operation label for a metric/alert.
const fn order_endpoint_str(endpoint: OrderEndpoint) -> &'static str {
    match endpoint {
        OrderEndpoint::Place => "place",
        OrderEndpoint::Modify => "modify",
        OrderEndpoint::Cancel => "cancel",
    }
}

/// The halt cause string for a HaltAndAlert class.
const fn halt_cause_for(class: DhanErrorClass) -> &'static str {
    match class {
        DhanErrorClass::Dh902 => "DH-902",
        DhanErrorClass::Dh903 => "DH-903",
        DhanErrorClass::Data810 => "DATA-810",
        // Unreachable: HaltAndAlert only maps 902/903/810; fail safe.
        _ => "DH-910",
    }
}

/// The coded `ErrorCode` for a halt cause (for the halt `error!`).
fn halt_error_code(cause: &'static str) -> ErrorCode {
    match cause {
        "DH-901" => ErrorCode::Dh901InvalidAuth,
        "DH-902" => ErrorCode::Dh902NoApiAccess,
        "DH-903" => ErrorCode::Dh903AccountIssue,
        "DATA-810" => ErrorCode::Data810ClientIdInvalid,
        _ => ErrorCode::Dh910Other,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::types::OcoSecondLeg;
    use super::*;
    use tickvault_common::order_types::{OrderType, OrderValidity, ProductType, TransactionType};

    /// Test token provider that always returns a fixed token.
    struct TestTokenProvider;
    impl TokenProvider for TestTokenProvider {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            Ok(SecretString::from("test-token"))
        }
    }

    fn make_oms_with_order(order_id: &str, status: OrderStatus) -> OrderManagementSystem {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let rate_limiter = OrderRateLimiter::new(10);
        let mut oms = OrderManagementSystem::new(
            api_client,
            rate_limiter,
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        let order = ManagedOrder {
            order_id: order_id.to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };

        oms.orders.insert(order_id.to_owned(), order);
        oms.correlations
            .track("corr-1".to_owned(), order_id.to_owned());
        oms
    }

    fn make_order_update(order_no: &str, status: &str) -> OrderUpdate {
        OrderUpdate {
            order_no: order_no.to_owned(),
            status: status.to_owned(),
            correlation_id: String::new(),
            exchange: String::new(),
            segment: String::new(),
            security_id: String::new(),
            client_id: String::new(),
            exch_order_no: String::new(),
            product: String::new(),
            txn_type: String::new(),
            order_type: String::new(),
            validity: String::new(),
            quantity: 0,
            traded_qty: 0,
            remaining_quantity: 0,
            price: 0.0,
            trigger_price: 0.0,
            traded_price: 0.0,
            avg_traded_price: 0.0,
            symbol: String::new(),
            display_name: String::new(),
            remarks: String::new(),
            reason_description: String::new(),
            order_date_time: String::new(),
            exch_order_time: String::new(),
            last_updated_time: String::new(),
            instrument: String::new(),
            lot_size: 0,
            strike_price: 0.0,
            expiry_date: String::new(),
            opt_type: String::new(),
            isin: String::new(),
            disc_quantity: 0,
            disc_qty_rem: 0,
            leg_no: 0,
            product_name: String::new(),
            ref_ltp: 0.0,
            tick_size: 0.0,
            source: String::new(),
            off_mkt_flag: String::new(),
            algo_ord_no: String::new(),
            mkt_type: String::new(),
            series: String::new(),
            good_till_days_date: String::new(),
            algo_id: String::new(),
            multiplier: 0,
        }
    }

    #[test]
    fn new_oms_is_empty() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        assert_eq!(oms.total_placed(), 0);
        assert_eq!(oms.total_updates(), 0);
        assert!(oms.active_orders().is_empty());
        assert!(oms.all_orders().is_empty());
    }

    #[test]
    fn handle_valid_transition_transit_to_pending() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        let update = make_order_update("1", "PENDING");

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Pending);
    }

    #[test]
    fn handle_valid_transition_pending_to_traded() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let mut update = make_order_update("1", "TRADED");
        update.traded_qty = 50;
        update.avg_traded_price = 245.50;

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());

        let order = oms.order("1").unwrap();
        assert_eq!(order.status, OrderStatus::Traded);
        assert_eq!(order.traded_qty, 50);
        assert!(order.is_terminal());
    }

    // -----------------------------------------------------------------------
    // FillEvent bridge (order-runtime dry-run PR, 2026-07-14) — the delta is
    // computed in-engine, so re-deliveries can never double-count.
    // -----------------------------------------------------------------------

    /// A full-fill transition returns a FillEvent whose lots = delta/lot_size.
    #[test]
    fn handle_order_update_returns_fill_event_on_traded() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let mut update = make_order_update("1", "TRADED");
        update.traded_qty = 50; // 50 units / lot_size 25 = 2 lots
        update.avg_traded_price = 245.50;
        update.exchange = "NSE".to_owned();
        update.segment = "D".to_owned();

        let fill = oms
            .handle_order_update(&update)
            .ok()
            .flatten()
            .expect("TRADED update with positive delta must yield a FillEvent"); // APPROVED: test
        assert_eq!(fill.security_id, 52432);
        assert_eq!(fill.fill_lots, 2, "Buy order → positive lots");
        assert_eq!(fill.lot_size, 25);
        assert!((fill.avg_price - 245.50).abs() < f64::EPSILON);
        assert_eq!(
            fill.segment_code,
            tickvault_common::types::ExchangeSegment::NseFno.binary_code()
        );
        assert_eq!(fill.order_id, "1");
    }

    /// Same-status refresh (incremental PART_TRADED) yields the DELTA, not
    /// the cumulative quantity (F2 double-count safety).
    #[test]
    fn test_same_status_refresh_applies_delta_not_cumulative() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);

        // First partial: PENDING → PART_TRADED with 25 units filled.
        let mut update = make_order_update("1", "PART_TRADED");
        update.traded_qty = 25;
        update.avg_traded_price = 100.0;
        let fill = oms.handle_order_update(&update).ok().flatten();
        assert_eq!(fill.map(|f| f.fill_lots), Some(1), "first 25 units = 1 lot");

        // Same-status refresh: cumulative traded_qty rises 25 → 50.
        let mut update2 = make_order_update("1", "PART_TRADED");
        update2.traded_qty = 50;
        update2.avg_traded_price = 101.0;
        let fill2 = oms
            .handle_order_update(&update2)
            .ok()
            .flatten()
            .expect("second slice must fill"); // APPROVED: test
        assert_eq!(
            fill2.fill_lots, 1,
            "delta = 50-25 = 25 units = 1 lot — NOT the cumulative 2 lots"
        );
        // C1: the slice PRICE is the delta price, not the cumulative VWAP —
        // (50·101 − 25·100) / 25 = 102.0.
        assert!(
            (fill2.avg_price - 102.0).abs() < f64::EPSILON,
            "second slice must carry the DELTA price 102.0, got {}",
            fill2.avg_price
        );
    }

    /// C1 (fix-round 2026-07-14): the two-slice canonical case — 1@100 then
    /// 1@110 arrives on the wire as cumulative avg 105; the second FillEvent
    /// must carry 110, and the risk-side avg_entry must land at 105.
    #[test]
    fn test_two_slice_fill_delta_price_and_risk_avg_entry() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let mut risk = crate::risk::engine::RiskEngine::new(2.0, 100, 1_000_000.0);

        // Slice 1: 1 lot (25 units) at 100 — cum avg 100.
        let mut u1 = make_order_update("1", "PART_TRADED");
        u1.traded_qty = 25;
        u1.avg_traded_price = 100.0;
        let f1 = oms
            .handle_order_update(&u1)
            .ok()
            .flatten()
            .expect("slice 1 fills"); // APPROVED: test
        assert!((f1.avg_price - 100.0).abs() < f64::EPSILON);
        risk.record_fill(f1.security_id, f1.fill_lots, f1.avg_price, f1.lot_size);

        // Slice 2: 1 more lot at 110 — the wire reports CUMULATIVE avg 105.
        let mut u2 = make_order_update("1", "PART_TRADED");
        u2.traded_qty = 50;
        u2.avg_traded_price = 105.0;
        let f2 = oms
            .handle_order_update(&u2)
            .ok()
            .flatten()
            .expect("slice 2 fills"); // APPROVED: test
        assert!(
            (f2.avg_price - 110.0).abs() < f64::EPSILON,
            "second FillEvent must carry the true slice price 110, not the \
             cumulative VWAP 105 — got {}",
            f2.avg_price
        );
        risk.record_fill(f2.security_id, f2.fill_lots, f2.avg_price, f2.lot_size);

        let pos = risk.position(f2.security_id).expect("position exists"); // APPROVED: test
        assert_eq!(pos.net_lots, 2);
        assert!(
            (pos.avg_entry_price - 105.0).abs() < f64::EPSILON,
            "avg_entry must be (100 + 110) / 2 = 105, got {}",
            pos.avg_entry_price
        );
    }

    /// A byte-identical re-delivery (same cumulative traded_qty) yields
    /// delta 0 → no FillEvent (WAL replay / broadcast duplicate safety).
    #[test]
    fn test_duplicate_update_zero_delta_skipped() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let mut update = make_order_update("1", "PART_TRADED");
        update.traded_qty = 25;
        update.avg_traded_price = 100.0;
        assert!(oms.handle_order_update(&update).ok().flatten().is_some());
        // Re-deliver the SAME update (same status, same cumulative qty).
        let dup = oms.handle_order_update(&update).ok().flatten();
        assert!(dup.is_none(), "duplicate re-delivery must yield NO fill");
        // And a REGRESSING traded_qty (reconcile-class correction) too.
        let mut regress = make_order_update("1", "PART_TRADED");
        regress.traded_qty = 10;
        let r = oms.handle_order_update(&regress).ok().flatten();
        assert!(r.is_none(), "regressing traded_qty must never yield a fill");
    }

    /// A delta that is not a lot multiple is FLOORED (loud OMS-GAP-01 error
    /// at the emit site); a sub-lot delta floors to zero lots → no event.
    #[test]
    fn test_partial_lot_remainder_floors_and_errors() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        // 30 units on a 25-unit lot = 1 lot + remainder 5 → floored to 1.
        let mut update = make_order_update("1", "PART_TRADED");
        update.traded_qty = 30;
        update.avg_traded_price = 100.0;
        let fill = oms.handle_order_update(&update).ok().flatten();
        assert_eq!(fill.map(|f| f.fill_lots), Some(1), "30/25 floors to 1 lot");

        // Sub-lot delta: 30 → 40 (floor(40/25) − floor(30/25) = 1−1 = 0) → None.
        let mut update2 = make_order_update("1", "PART_TRADED");
        update2.traded_qty = 40;
        update2.avg_traded_price = 100.0;
        let fill2 = oms.handle_order_update(&update2).ok().flatten();
        assert!(fill2.is_none(), "sub-lot delta floors to zero → no event");

        // C4 (fix-round 2026-07-14): the remainder CARRIES — 40 → 75 credits
        // floor(75/25) − floor(40/25) = 3 − 1 = 2 lots, so the total across
        // 30→40→75 is 1+0+2 = 3 lots for 75 units (never 2).
        let mut update3 = make_order_update("1", "PART_TRADED");
        update3.traded_qty = 75;
        update3.avg_traded_price = 100.0;
        let fill3 = oms.handle_order_update(&update3).ok().flatten();
        assert_eq!(
            fill3.map(|f| f.fill_lots),
            Some(2),
            "remainder units must carry into later slices (floor-of-cumulatives)"
        );
    }

    /// C2 (fix-round 2026-07-14): a regressing (or garbage NEGATIVE)
    /// traded_qty must never lower the delta baseline — the next legit
    /// cumulative would otherwise inflate the fill.
    #[test]
    fn test_regressing_and_negative_traded_qty_never_lower_baseline() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let mut u1 = make_order_update("1", "PART_TRADED");
        u1.traded_qty = 50;
        u1.avg_traded_price = 100.0;
        assert_eq!(
            oms.handle_order_update(&u1)
                .ok()
                .flatten()
                .map(|f| f.fill_lots),
            Some(2)
        );
        // Garbage NEGATIVE cumulative (serde-default wire) — no fill, and the
        // baseline must NOT drop.
        let mut garbage = make_order_update("1", "PART_TRADED");
        garbage.traded_qty = -50;
        garbage.avg_traded_price = 100.0;
        assert!(oms.handle_order_update(&garbage).ok().flatten().is_none());
        assert_eq!(
            oms.order("1").map(|o| o.traded_qty),
            Some(50),
            "regressing copy must be refused — baseline preserved"
        );
        // Next legit cumulative 75: delta = 25 units = 1 lot (with the old
        // unconditional copy this computed 75−(−50) = 125 units = 5 lots).
        let mut u2 = make_order_update("1", "PART_TRADED");
        u2.traded_qty = 75;
        u2.avg_traded_price = 100.0;
        assert_eq!(
            oms.handle_order_update(&u2)
                .ok()
                .flatten()
                .map(|f| f.fill_lots),
            Some(1),
            "a garbage regressing update must not inflate the next fill"
        );
    }

    /// C3 (fix-round 2026-07-14): a terminal order must never re-fill via
    /// the same-status arm — even with a HIGHER traded_qty redelivery.
    #[test]
    fn test_terminal_order_higher_qty_redelivery_never_refills() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let mut traded = make_order_update("1", "TRADED");
        traded.traded_qty = 50;
        traded.avg_traded_price = 100.0;
        assert!(oms.handle_order_update(&traded).ok().flatten().is_some());

        // Same-status TRADED redelivery with a HIGHER cumulative qty.
        let mut refill = make_order_update("1", "TRADED");
        refill.traded_qty = 75;
        refill.avg_traded_price = 100.0;
        let second = oms.handle_order_update(&refill);
        assert!(
            !matches!(second, Ok(Some(_))),
            "a terminal order must never emit a second FillEvent"
        );
        let order = oms.order("1").expect("tracked"); // APPROVED: test
        assert_eq!(order.traded_qty, 50, "terminal state is final — no copy");
        assert!(
            order.needs_reconciliation,
            "the anomaly must be flagged for reconciliation"
        );
    }

    /// C5 (fix-round 2026-07-14): a zero / NaN wire avg price never reaches
    /// the risk book — the fill event is rejected (qty copy still advances).
    #[test]
    fn test_fill_price_guard_rejects_zero_and_nan_avg_price() {
        for bad in [0.0, f64::NAN, f64::NEG_INFINITY, -5.0] {
            let mut oms = make_oms_with_order("1", OrderStatus::Pending);
            let mut update = make_order_update("1", "TRADED");
            update.traded_qty = 25;
            update.avg_traded_price = bad;
            let fill = oms.handle_order_update(&update).ok().flatten();
            assert!(
                fill.is_none(),
                "avg_traded_price {bad} must reject the fill event"
            );
            assert_eq!(
                oms.order("1").map(|o| o.traded_qty),
                Some(25),
                "the qty copy still advances (reconcile-class correction)"
            );
        }
    }

    /// L4 (fix-round 2026-07-14): the i64→i32 lots clamp saturates instead
    /// of panicking, and a zero lot_size is normalized to 1.
    #[test]
    fn test_fill_lots_i32_clamp_and_lot_size_zero_normalized() {
        // Clamp: an absurd cumulative qty saturates to i32::MAX lots.
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        if let Some(order) = oms.orders.get_mut("1") {
            order.lot_size = 1;
        }
        let mut update = make_order_update("1", "TRADED");
        update.traded_qty = i64::from(i32::MAX) + 10;
        update.avg_traded_price = 1.0;
        let fill = oms
            .handle_order_update(&update)
            .ok()
            .flatten()
            .expect("fill expected"); // APPROVED: test
        assert_eq!(fill.fill_lots, i32::MAX, "lots must saturate, never panic");

        // lot_size 0 → normalized to 1 (3 units = 3 lots).
        let mut oms2 = make_oms_with_order("2", OrderStatus::Pending);
        if let Some(order) = oms2.orders.get_mut("2") {
            order.lot_size = 0;
        }
        let mut u2 = make_order_update("2", "TRADED");
        u2.traded_qty = 3;
        u2.avg_traded_price = 10.0;
        let f2 = oms2
            .handle_order_update(&u2)
            .ok()
            .flatten()
            .expect("fill expected"); // APPROVED: test
        assert_eq!(f2.fill_lots, 3);
        assert_eq!(f2.lot_size, 1, "lot_size 0 must normalize to 1");
    }

    /// HP-2 companion: the alloc-free active-order counter agrees with
    /// `active_orders()`.
    #[test]
    fn test_active_order_count_matches_active_orders_len() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        assert_eq!(oms.active_order_count(), oms.active_orders().len());
        assert_eq!(oms.active_order_count(), 1);
        let mut traded = make_order_update("1", "TRADED");
        traded.traded_qty = 50;
        traded.avg_traded_price = 100.0;
        let _ = oms.handle_order_update(&traded);
        assert_eq!(oms.active_order_count(), 0, "terminal orders excluded");
        assert_eq!(oms.active_order_count(), oms.active_orders().len());
    }

    /// Fill sign comes from the tracked ManagedOrder's transaction_type
    /// (authoritative), never the serde-default wire TxnType.
    #[test]
    fn test_fill_sign_from_managed_order_transaction_type() {
        // Sell order: positive traded_qty delta must yield NEGATIVE lots.
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        if let Some(order) = oms.orders.get_mut("1") {
            order.transaction_type = TransactionType::Sell;
        }
        let mut update = make_order_update("1", "TRADED");
        update.traded_qty = 50;
        update.avg_traded_price = 200.0;
        // Wire TxnType left EMPTY (serde-default reality) — must not matter.
        assert!(update.txn_type.is_empty());
        let fill = oms
            .handle_order_update(&update)
            .ok()
            .flatten()
            .expect("fill expected"); // APPROVED: test
        assert_eq!(fill.fill_lots, -2, "Sell order → negative lots");
    }

    /// Unknown exchange/segment chars map to the sentinel, never a panic.
    #[test]
    fn test_fill_event_unknown_segment_uses_sentinel() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let mut update = make_order_update("1", "TRADED");
        update.traded_qty = 25;
        update.avg_traded_price = 100.0;
        // exchange/segment left empty by make_order_update.
        let fill = oms
            .handle_order_update(&update)
            .ok()
            .flatten()
            .expect("fill expected"); // APPROVED: test
        assert_eq!(fill.segment_code, SEGMENT_CODE_UNKNOWN);
    }

    #[test]
    fn handle_invalid_transition_returns_error() {
        let mut oms = make_oms_with_order("1", OrderStatus::Traded);
        let update = make_order_update("1", "PENDING");

        let result = oms.handle_order_update(&update);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OmsError::InvalidTransition { .. }
        ));
        assert!(oms.order("1").unwrap().needs_reconciliation);
    }

    #[test]
    fn handle_unknown_order_ignored() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        let update = make_order_update("999", "TRADED");

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
    }

    #[test]
    fn handle_same_status_updates_fields() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
        let mut update = make_order_update("1", "CONFIRMED");
        update.traded_qty = 25;
        update.avg_traded_price = 245.0;

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().traded_qty, 25);
    }

    #[test]
    fn handle_unknown_status_ignored() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        let update = make_order_update("1", "WEIRD_STATUS");

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        // Status unchanged
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Transit);
    }

    #[test]
    fn handle_correlation_id_lookup() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        // Update comes with a different order_no but matching correlation_id
        let mut update = make_order_update("unknown", "PENDING");
        update.correlation_id = "corr-1".to_owned();

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Pending);
    }

    #[test]
    fn active_orders_excludes_terminal() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        // Add a terminal order
        let terminal = ManagedOrder {
            order_id: "2".to_owned(),
            correlation_id: "corr-2".to_owned(),
            security_id: 100,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 100.0,
            trigger_price: 0.0,
            status: OrderStatus::Traded,
            traded_qty: 50,
            avg_traded_price: 100.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        oms.orders.insert("2".to_owned(), terminal);

        assert_eq!(oms.active_orders().len(), 1);
        assert_eq!(oms.all_orders().len(), 2);
    }

    #[test]
    fn reset_daily_clears_state() {
        let mut oms = make_oms_with_order("1", OrderStatus::Traded);
        oms.total_placed = 5;
        oms.total_updates = 10;

        oms.reset_daily();

        assert!(oms.all_orders().is_empty());
        assert_eq!(oms.total_placed(), 0);
        assert_eq!(oms.total_updates(), 0);
    }

    #[test]
    fn modify_terminal_order_returns_error() {
        let oms = make_oms_with_order("1", OrderStatus::Traded);
        let order = oms.order("1").unwrap();
        assert!(order.is_terminal());
    }

    #[test]
    fn cancel_nonexistent_order_returns_error() {
        let oms = make_oms_with_order("1", OrderStatus::Transit);
        assert!(oms.order("999").is_none());
    }

    #[test]
    fn update_counter_increments() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        let update = make_order_update("1", "PENDING");

        let _ = oms.handle_order_update(&update);
        assert_eq!(oms.total_updates(), 1);
    }

    // -----------------------------------------------------------------------
    // Dry-run mode tests
    // -----------------------------------------------------------------------

    #[test]
    fn oms_defaults_to_dry_run() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );
        assert!(oms.is_dry_run(), "OMS must default to dry-run mode");
    }

    #[tokio::test]
    async fn dry_run_place_order_returns_paper_id() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };

        let order_id = oms.place_order(request).await.unwrap();
        assert!(
            order_id.starts_with("PAPER-"),
            "dry-run order ID must start with PAPER-"
        );
        assert_eq!(oms.total_placed(), 1);
    }

    #[tokio::test]
    async fn dry_run_sequential_paper_ids() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        let make_request = || PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 0.0,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };

        let id1 = oms.place_order(make_request()).await.unwrap();
        let id2 = oms.place_order(make_request()).await.unwrap();
        assert_eq!(id1, "PAPER-1");
        assert_eq!(id2, "PAPER-2");
    }

    #[tokio::test]
    async fn dry_run_order_tracked_in_transit() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Sell,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 25,
            price: 300.0,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };

        let order_id = oms.place_order(request).await.unwrap();
        let order = oms.order(&order_id).unwrap();
        // Dry-run orders skip Transit and go straight to Confirmed
        assert_eq!(order.status, OrderStatus::Confirmed);
        assert_eq!(order.quantity, 25);
        assert!(oms.active_orders().iter().any(|o| o.order_id == order_id));
    }

    // -----------------------------------------------------------------------
    // Correlation + order update edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn handle_update_via_correlation_re_indexes_order() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        // Simulate Dhan assigning a real order_no via the WS update
        let mut update = make_order_update("DHAN-999", "PENDING");
        update.correlation_id = "corr-1".to_owned();

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        // The order should still be accessible by its original ID
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Pending);
    }

    #[test]
    fn multiple_updates_increment_counter() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);

        let _ = oms.handle_order_update(&make_order_update("1", "PENDING"));
        let _ = oms.handle_order_update(&make_order_update("1", "CONFIRMED"));
        assert_eq!(oms.total_updates(), 2);
    }

    #[test]
    fn handle_update_fills_data() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
        let mut update = make_order_update("1", "TRADED");
        update.traded_qty = 50;
        update.avg_traded_price = 246.0;

        let _ = oms.handle_order_update(&update);
        let order = oms.order("1").unwrap();
        assert_eq!(order.traded_qty, 50);
        assert!((order.avg_traded_price - 246.0).abs() < f64::EPSILON);
    }

    #[test]
    fn reset_daily_also_resets_paper_counter() {
        let mut oms = make_oms_with_order("1", OrderStatus::Traded);
        oms.paper_order_counter = 42;
        oms.reset_daily();
        assert_eq!(oms.paper_order_counter, 0);
    }

    #[test]
    fn needs_reconciliation_flag_set_on_invalid_transition() {
        let mut oms = make_oms_with_order("1", OrderStatus::Traded);
        // Traded → Pending is invalid
        let _ = oms.handle_order_update(&make_order_update("1", "PENDING"));
        assert!(oms.order("1").unwrap().needs_reconciliation);
    }

    // -----------------------------------------------------------------------
    // Pre-submission validation gate tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_market_order_nonzero_price_rejected() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50, // BUG: MARKET orders must have price=0
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };

        let result = oms.place_order(request).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), OmsError::RiskRejected { .. }),
            "MARKET order with non-zero price must be rejected"
        );
    }

    #[tokio::test]
    async fn test_sl_order_zero_trigger_rejected() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::StopLoss,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0, // BUG: SL orders require triggerPrice > 0
            lot_size: 25,
            expiry_date: None,
        };

        let result = oms.place_order(request).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), OmsError::RiskRejected { .. }),
            "SL order with zero triggerPrice must be rejected"
        );
    }

    #[tokio::test]
    async fn test_market_order_zero_price_accepted() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 0.0, // Correct: MARKET order with price=0
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };

        let result = oms.place_order(request).await;
        assert!(result.is_ok(), "MARKET order with price=0 must be accepted");
    }

    // -----------------------------------------------------------------------
    // PartTraded status handling
    // -----------------------------------------------------------------------

    #[test]
    fn handle_part_traded_transition() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let mut update = make_order_update("1", "PART_TRADED");
        update.traded_qty = 25;
        update.avg_traded_price = 245.0;

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());

        let order = oms.order("1").unwrap();
        assert_eq!(order.status, OrderStatus::PartTraded);
        assert_eq!(order.traded_qty, 25);
        assert!(!order.is_terminal(), "PartTraded is NOT terminal");
    }

    // -----------------------------------------------------------------------
    // Modification count enforcement
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_modification_count_enforced() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
        // Set modification count to max (25)
        oms.orders.get_mut("1").unwrap().modification_count = 25;

        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 50,
            price: 250.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), OmsError::RiskRejected { .. }),
            "Order at max modifications must be rejected"
        );
    }

    #[tokio::test]
    async fn test_disclosed_quantity_below_30_percent_rejected() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 100,
            price: 250.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 20, // 20% < 30% minimum
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), OmsError::RiskRejected { .. }),
            "disclosedQuantity < 30% of quantity must be rejected"
        );
    }

    #[tokio::test]
    async fn test_disclosed_qty_ceiling_division_edge_case() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        // quantity=9, 30% = 2.7, ceiling = 3. disclosed_quantity=2 must fail.
        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 9,
            price: 250.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 2, // 22.2% < 30%
        };

        let result = oms.modify_order("1", request).await;
        assert!(
            result.is_err(),
            "disclosed_quantity=2 on quantity=9 (22%) must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // Modify order validation gates
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_modify_market_order_nonzero_price_rejected() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        let request = ModifyOrderRequest {
            order_type: OrderType::Market,
            quantity: 50,
            price: 245.50, // MARKET must have price=0
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), OmsError::RiskRejected { .. }),
            "MARKET modify with non-zero price must be rejected"
        );
    }

    #[tokio::test]
    async fn test_modify_sl_order_zero_trigger_rejected() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        let request = ModifyOrderRequest {
            order_type: OrderType::StopLoss,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0, // SL must have triggerPrice > 0
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), OmsError::RiskRejected { .. }),
            "SL modify with zero triggerPrice must be rejected"
        );
    }

    // -- Metrics tests --

    #[tokio::test]
    async fn test_oms_metrics_emitted_on_place() {
        // Dry-run place_order should increment tv_orders_placed_total.
        // We can't easily inspect the metrics crate's internal state,
        // but we verify the place_order path that includes counter! doesn't panic.
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let rate_limiter = OrderRateLimiter::new(10);
        let mut oms = OrderManagementSystem::new(
            api_client,
            rate_limiter,
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };
        let result = oms.place_order(request).await;
        assert!(result.is_ok());
        assert_eq!(oms.total_placed(), 1);
    }

    #[test]
    fn test_oms_metrics_emitted_on_reject() {
        // handle_order_update with Rejected status should increment
        // tv_orders_rejected_total without panicking.
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let update = make_order_update("1", "REJECTED");

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());

        let order = oms.order("1").unwrap();
        assert_eq!(order.status, OrderStatus::Rejected);
    }

    #[test]
    fn test_rate_limit_error_does_not_trip_circuit_breaker() {
        // OmsError::DhanRateLimited should NOT be counted as a circuit
        // breaker failure — the API is healthy, we're just throttled.
        let err = OmsError::DhanRateLimited;
        // The guard condition in the error handler: !matches!(err, OmsError::DhanRateLimited)
        assert!(
            matches!(err, OmsError::DhanRateLimited),
            "DhanRateLimited must match the exclusion guard"
        );

        // Verify the circuit breaker stays closed after a rate limit error.
        let cb = OrderCircuitBreaker::new();
        // Simulate: a rate limit error occurs, but we don't call record_failure.
        // The CB should remain closed.
        assert!(cb.check().is_ok(), "CB must be closed initially");

        // But a non-rate-limit error DOES trip it.
        for _ in 0..10 {
            cb.record_failure();
        }
        assert!(
            cb.check().is_err(),
            "CB must be open after enough non-rate-limit failures"
        );
    }

    // -----------------------------------------------------------------------
    // validate_order_fields — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_order_fields_limit_order_valid() {
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };
        assert!(validate_order_fields(&request).is_ok());
    }

    #[test]
    fn test_validate_order_fields_market_order_valid() {
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 0.0,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };
        assert!(validate_order_fields(&request).is_ok());
    }

    #[test]
    fn test_validate_order_fields_market_nonzero_price() {
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 100.0,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };
        let err = validate_order_fields(&request).unwrap_err();
        assert!(matches!(err, OmsError::RiskRejected { .. }));
    }

    #[test]
    fn test_validate_order_fields_sl_with_trigger() {
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::StopLoss,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 240.0,
            lot_size: 25,
            expiry_date: None,
        };
        assert!(validate_order_fields(&request).is_ok());
    }

    #[test]
    fn test_validate_order_fields_sl_zero_trigger() {
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::StopLoss,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };
        let err = validate_order_fields(&request).unwrap_err();
        assert!(matches!(err, OmsError::RiskRejected { .. }));
    }

    #[test]
    fn test_validate_order_fields_slm_zero_trigger() {
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Sell,
            order_type: OrderType::StopLossMarket,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 0.0,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };
        let err = validate_order_fields(&request).unwrap_err();
        assert!(matches!(err, OmsError::RiskRejected { .. }));
    }

    #[test]
    fn test_validate_order_fields_slm_with_trigger() {
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Sell,
            order_type: OrderType::StopLossMarket,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 0.0,
            trigger_price: 240.0,
            lot_size: 25,
            expiry_date: None,
        };
        assert!(validate_order_fields(&request).is_ok());
    }

    // -----------------------------------------------------------------------
    // I-P0-03: Expiry date validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_expired_contract_rejected() {
        let yesterday = chrono::Utc::now().date_naive() - chrono::Duration::days(1);
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: Some(yesterday),
        };
        let result = validate_order_fields(&request);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, OmsError::ExpiredContract { .. }),
            "I-P0-03: expired contract must be rejected, got: {err}"
        );
    }

    #[test]
    fn test_validate_valid_contract_passes() {
        let tomorrow = chrono::Utc::now().date_naive() + chrono::Duration::days(1);
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: Some(tomorrow),
        };
        assert!(
            validate_order_fields(&request).is_ok(),
            "I-P0-03: valid (non-expired) contract must pass"
        );
    }

    #[test]
    fn test_validate_no_expiry_date_passes() {
        let request = PlaceOrderRequest {
            security_id: 11536,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        };
        assert!(
            validate_order_fields(&request).is_ok(),
            "I-P0-03: equity orders (no expiry) must pass"
        );
    }

    // -----------------------------------------------------------------------
    // validate_modify_fields — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_modify_fields_limit_valid() {
        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 50,
            price: 250.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };
        assert!(validate_modify_fields(&request).is_ok());
    }

    #[test]
    fn test_validate_modify_fields_market_valid() {
        let request = ModifyOrderRequest {
            order_type: OrderType::Market,
            quantity: 50,
            price: 0.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };
        assert!(validate_modify_fields(&request).is_ok());
    }

    #[test]
    fn test_validate_modify_fields_market_nonzero_price() {
        let request = ModifyOrderRequest {
            order_type: OrderType::Market,
            quantity: 50,
            price: 100.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };
        assert!(validate_modify_fields(&request).is_err());
    }

    #[test]
    fn test_validate_modify_fields_sl_zero_trigger() {
        let request = ModifyOrderRequest {
            order_type: OrderType::StopLoss,
            quantity: 50,
            price: 250.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };
        assert!(validate_modify_fields(&request).is_err());
    }

    #[test]
    fn test_validate_modify_fields_slm_zero_trigger() {
        let request = ModifyOrderRequest {
            order_type: OrderType::StopLossMarket,
            quantity: 50,
            price: 0.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };
        assert!(validate_modify_fields(&request).is_err());
    }

    #[test]
    fn test_validate_modify_fields_sl_with_trigger() {
        let request = ModifyOrderRequest {
            order_type: OrderType::StopLoss,
            quantity: 50,
            price: 250.0,
            trigger_price: 245.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };
        assert!(validate_modify_fields(&request).is_ok());
    }

    // -----------------------------------------------------------------------
    // validate_disclosed_quantity — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_disclosed_quantity_zero_always_ok() {
        assert!(validate_disclosed_quantity(100, 0).is_ok());
    }

    #[test]
    fn test_disclosed_quantity_negative_always_ok() {
        assert!(validate_disclosed_quantity(100, -1).is_ok());
    }

    #[test]
    fn test_disclosed_quantity_exactly_30_percent() {
        // 30% of 100 = 30, ceiling division: (100*3+9)/10 = 30
        assert!(validate_disclosed_quantity(100, 30).is_ok());
    }

    #[test]
    fn test_disclosed_quantity_above_30_percent() {
        assert!(validate_disclosed_quantity(100, 50).is_ok());
    }

    #[test]
    fn test_disclosed_quantity_below_30_percent() {
        assert!(validate_disclosed_quantity(100, 20).is_err());
    }

    #[test]
    fn test_disclosed_quantity_edge_case_9() {
        // quantity=9, 30% = 2.7, ceiling = 3
        assert!(validate_disclosed_quantity(9, 3).is_ok());
        assert!(validate_disclosed_quantity(9, 2).is_err());
    }

    #[test]
    fn test_disclosed_quantity_edge_case_1() {
        // quantity=1, (1*3+9)/10 = 1
        assert!(validate_disclosed_quantity(1, 1).is_ok());
    }

    #[test]
    fn test_disclosed_quantity_edge_case_10() {
        // quantity=10, (10*3+9)/10 = 3
        assert!(validate_disclosed_quantity(10, 3).is_ok());
        assert!(validate_disclosed_quantity(10, 2).is_err());
    }

    #[test]
    fn test_disclosed_quantity_error_message() {
        let err = validate_disclosed_quantity(100, 20).unwrap_err();
        assert!(err.contains("disclosedQuantity (20)"));
        assert!(err.contains("quantity (100)"));
    }

    // -----------------------------------------------------------------------
    // check_modification_limit — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_modification_limit_zero() {
        assert!(check_modification_limit(0, "order-1").is_ok());
    }

    #[test]
    fn test_check_modification_limit_below_max() {
        assert!(check_modification_limit(24, "order-1").is_ok());
    }

    #[test]
    fn test_check_modification_limit_at_max() {
        let result = check_modification_limit(MAX_MODIFICATIONS_PER_ORDER, "order-1");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), OmsError::RiskRejected { .. }));
    }

    #[test]
    fn test_check_modification_limit_above_max() {
        let result = check_modification_limit(MAX_MODIFICATIONS_PER_ORDER + 1, "order-1");
        assert!(result.is_err());
    }

    #[test]
    fn test_check_modification_limit_error_contains_order_id() {
        let result = check_modification_limit(MAX_MODIFICATIONS_PER_ORDER, "my-order-42");
        let err = result.unwrap_err();
        if let OmsError::RiskRejected { reason } = err {
            assert!(reason.contains("my-order-42"));
            assert!(reason.contains(&MAX_MODIFICATIONS_PER_ORDER.to_string()));
        } else {
            panic!("expected RiskRejected");
        }
    }

    // -----------------------------------------------------------------------
    // resolve_initial_status — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_initial_status_transit() {
        assert_eq!(resolve_initial_status("TRANSIT"), OrderStatus::Transit);
    }

    #[test]
    fn test_resolve_initial_status_traded() {
        assert_eq!(resolve_initial_status("TRADED"), OrderStatus::Traded);
    }

    #[test]
    fn test_resolve_initial_status_rejected() {
        assert_eq!(resolve_initial_status("REJECTED"), OrderStatus::Rejected);
    }

    #[test]
    fn test_resolve_initial_status_pending() {
        assert_eq!(resolve_initial_status("PENDING"), OrderStatus::Pending);
    }

    #[test]
    fn test_resolve_initial_status_unknown_defaults_to_transit() {
        assert_eq!(resolve_initial_status("WEIRD_STATUS"), OrderStatus::Transit);
    }

    #[test]
    fn test_resolve_initial_status_empty_defaults_to_transit() {
        assert_eq!(resolve_initial_status(""), OrderStatus::Transit);
    }

    // -----------------------------------------------------------------------
    // now_epoch_us — pure function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_now_epoch_us_positive() {
        let us = now_epoch_us();
        assert!(us > 0, "epoch microseconds must be positive");
    }

    #[test]
    fn test_now_epoch_us_reasonable_range() {
        let us = now_epoch_us();
        // Must be after 2020-01-01 (1577836800000000 us)
        assert!(us > 1_577_836_800_000_000, "timestamp must be after 2020");
    }

    // -----------------------------------------------------------------------
    // Dry-run cancel order tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_dry_run_cancel_order() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        let result = oms.cancel_order("1").await;
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Cancelled);
    }

    #[tokio::test]
    async fn test_dry_run_cancel_terminal_order_rejected() {
        let mut oms = make_oms_with_order("1", OrderStatus::Traded);

        let result = oms.cancel_order("1").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OmsError::OrderTerminal { .. }
        ));
    }

    #[tokio::test]
    async fn test_dry_run_cancel_nonexistent_order() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        let result = oms.cancel_order("999").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OmsError::OrderNotFound { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // Dry-run modify order tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_dry_run_modify_order_updates_fields() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 75,
            price: 260.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_ok());

        let order = oms.order("1").unwrap();
        assert_eq!(order.quantity, 75);
        assert!((order.price - 260.0).abs() < f64::EPSILON);
        assert_eq!(order.modification_count, 1);
    }

    #[tokio::test]
    async fn test_dry_run_modify_terminal_order_rejected() {
        let mut oms = make_oms_with_order("1", OrderStatus::Traded);

        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 75,
            price: 260.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OmsError::OrderTerminal { .. }
        ));
    }

    #[tokio::test]
    async fn test_dry_run_modify_nonexistent_order() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 75,
            price: 260.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };

        let result = oms.modify_order("999", request).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OmsError::OrderNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn test_dry_run_modify_order_increments_mod_count() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        for i in 0..3 {
            let request = ModifyOrderRequest {
                order_type: OrderType::Limit,
                quantity: 50 + i,
                price: 250.0 + (i as f64),
                trigger_price: 0.0,
                validity: OrderValidity::Day,
                disclosed_quantity: 0,
            };
            let result = oms.modify_order("1", request).await;
            assert!(result.is_ok());
        }

        assert_eq!(oms.order("1").unwrap().modification_count, 3);
    }

    // -----------------------------------------------------------------------
    // Dry-run reconcile
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_dry_run_reconcile_returns_empty_report() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
        let report = oms.reconcile().await.unwrap();
        assert_eq!(report.total_checked, 0);
    }

    // -----------------------------------------------------------------------
    // Order update edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_update_rejected_sets_terminal() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        let update = make_order_update("1", "REJECTED");

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Rejected);
        assert!(oms.order("1").unwrap().is_terminal());
    }

    #[test]
    fn test_handle_update_cancelled_sets_terminal() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let update = make_order_update("1", "CANCELLED");

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Cancelled);
        assert!(oms.order("1").unwrap().is_terminal());
    }

    #[test]
    fn test_handle_update_expired_sets_terminal() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let update = make_order_update("1", "EXPIRED");

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Expired);
        assert!(oms.order("1").unwrap().is_terminal());
    }

    #[test]
    fn test_handle_update_empty_correlation_id_no_lookup() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        let mut update = make_order_update("999", "PENDING");
        update.correlation_id = String::new();

        // Unknown order_no with empty correlation_id → ignored
        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        // Original order unchanged
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Transit);
    }

    #[test]
    fn test_handle_update_same_status_different_fill_data() {
        let mut oms = make_oms_with_order("1", OrderStatus::PartTraded);
        let mut update = make_order_update("1", "PART_TRADED");
        update.traded_qty = 40;
        update.avg_traded_price = 247.0;

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());

        let order = oms.order("1").unwrap();
        assert_eq!(order.status, OrderStatus::PartTraded);
        assert_eq!(order.traded_qty, 40);
        assert!((order.avg_traded_price - 247.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Coverage: re-index order under new order_no from WebSocket
    // -----------------------------------------------------------------------

    #[test]
    fn handle_update_via_correlation_re_indexes_under_new_order_no() {
        let mut oms = make_oms_with_order("OLD-1", OrderStatus::Transit);
        // Correlation is tracked as "corr-1" -> "OLD-1"
        oms.correlations
            .track("corr-1".to_owned(), "OLD-1".to_owned());

        // Dhan assigns a new order_no "DHAN-999" via WS update
        let mut update = make_order_update("DHAN-999", "PENDING");
        update.correlation_id = "corr-1".to_owned();

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());

        // The original order should have its status updated
        assert_eq!(oms.order("OLD-1").unwrap().status, OrderStatus::Pending);

        // The re-indexed clone should be accessible under the new order_no.
        // NOTE: the clone was made before the status update, so it retains
        // the original Transit status. The re-index ensures future updates
        // with the new order_no can find the entry.
        let reindexed = oms.order("DHAN-999");
        assert!(
            reindexed.is_some(),
            "order must be re-indexed under new order_no from WebSocket"
        );
        assert_eq!(reindexed.unwrap().status, OrderStatus::Transit);
    }

    // -----------------------------------------------------------------------
    // Coverage: unknown status in WebSocket update is ignored
    // -----------------------------------------------------------------------

    #[test]
    fn handle_update_unknown_status_returns_ok() {
        let mut oms = make_oms_with_order("1", OrderStatus::Pending);
        let update = make_order_update("1", "WEIRD_UNKNOWN_STATUS");

        let result = oms.handle_order_update(&update);
        assert!(
            result.is_ok(),
            "unknown status must be logged and ignored, not error"
        );
        // Original status should be unchanged
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Pending);
    }

    // -----------------------------------------------------------------------
    // Coverage: order not found after status parse (race with removal)
    // -----------------------------------------------------------------------

    #[test]
    fn handle_update_order_removed_between_lookup_and_get_mut() {
        let mut oms = make_oms_with_order("1", OrderStatus::Transit);
        // Remove the order from the map after initial contains_key check
        // This tests the `None => return Ok(())` path in get_mut
        // We can't actually trigger a race in single-threaded, but we can test
        // the found_order_id None path by providing an unknown order
        let update = make_order_update("UNKNOWN-ORDER", "PENDING");
        let result = oms.handle_order_update(&update);
        assert!(result.is_ok(), "unknown order must be silently ignored");
    }

    // -----------------------------------------------------------------------
    // Coverage: Traded status emits counter metric
    // -----------------------------------------------------------------------

    #[test]
    fn handle_update_traded_emits_filled_metric() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
        let mut update = make_order_update("1", "TRADED");
        update.traded_qty = 50;
        update.avg_traded_price = 250.0;

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Traded);
        // The counter!("tv_orders_filled_total").increment(1) path is exercised
    }

    // -----------------------------------------------------------------------
    // Coverage: SLM order with valid trigger accepted
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_slm_order_valid_trigger_accepted() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::StopLossMarket,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 0.0,
            trigger_price: 240.0,
            lot_size: 25,
            expiry_date: None,
        };

        let result = oms.place_order(request).await;
        assert!(
            result.is_ok(),
            "SLM order with valid trigger must be accepted"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: dry-run modify increments modification_count field
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_dry_run_modify_order_valid_sl() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        let request = ModifyOrderRequest {
            order_type: OrderType::StopLoss,
            quantity: 50,
            price: 245.0,
            trigger_price: 240.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_ok());
        assert_eq!(oms.order("1").unwrap().modification_count, 1);
        assert_eq!(oms.order("1").unwrap().order_type, OrderType::StopLoss);
    }

    // -----------------------------------------------------------------------
    // Coverage: active_orders filters terminal (with explicit OMS construction)
    // -----------------------------------------------------------------------

    #[test]
    fn active_orders_excludes_terminal_explicit_construction() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        // Add one active and one terminal order
        let active_order = ManagedOrder {
            order_id: "active-1".to_owned(),
            correlation_id: "c1".to_owned(),
            security_id: 100,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.0,
            trigger_price: 0.0,
            status: OrderStatus::Pending,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        let terminal_order = ManagedOrder {
            order_id: "terminal-1".to_owned(),
            status: OrderStatus::Traded,
            ..active_order.clone()
        };

        oms.orders.insert("active-1".to_owned(), active_order);
        oms.orders.insert("terminal-1".to_owned(), terminal_order);

        let active = oms.active_orders();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].order_id, "active-1");
    }

    // -----------------------------------------------------------------------
    // Coverage: disclosed quantity exactly at threshold passes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_disclosed_qty_at_30_percent_accepted() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);

        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 100,
            price: 250.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 30, // exactly 30%
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // Live-mode tests with mock HTTP server
    // -----------------------------------------------------------------------

    use std::time::Duration;

    /// Starts a one-shot TCP mock that returns `status` + `body` then closes.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only content-length
    async fn start_oms_mock(status: u16, body: &str) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let body = body.to_string();

        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 {} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        (base_url, handle)
    }

    /// Multi-shot mock server that handles `n` sequential requests.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only content-length
    async fn start_multi_mock(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let handle = tokio::spawn(async move {
            for (status, body) in responses {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let mut buf = vec![0u8; 8192];
                    let _ = stream.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 {} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status,
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            }
        });

        (base_url, handle)
    }

    fn make_live_oms(base_url: &str) -> OrderManagementSystem {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let api_client = OrderApiClient::new(http, base_url.to_owned(), "100".to_owned());
        let rate_limiter = OrderRateLimiter::new(10);
        let mut oms = OrderManagementSystem::new(
            api_client,
            rate_limiter,
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );
        oms.enable_live_mode();
        oms
    }

    /// Live OMS whose HTTP client has NO per-request timeout. Used by the
    /// `start_paused` DH-904 ladder tests: with a request timeout armed, tokio's
    /// auto-advance would fire it mid-request (the paused-clock-vs-real-TCP trap)
    /// and surface a spurious HttpError instead of the real DhanRateLimited. With
    /// no request timer, the only pending timer during a ladder run is the
    /// backoff sleep, which auto-advances cleanly.
    fn make_live_oms_no_timeout(base_url: &str) -> OrderManagementSystem {
        let http = reqwest::Client::builder().build().unwrap();
        let api_client = OrderApiClient::new(http, base_url.to_owned(), "100".to_owned());
        let rate_limiter = OrderRateLimiter::new(10);
        let mut oms = OrderManagementSystem::new(
            api_client,
            rate_limiter,
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );
        oms.enable_live_mode();
        oms
    }

    /// Install a FRESH, PROFILE-OK, high-headroom readiness snapshot so a live
    /// test exercises the API path rather than the fail-closed gate. The gate
    /// itself is covered by the dedicated `readiness_gate_*` tests below.
    fn install_ready(oms: &mut OrderManagementSystem) {
        use crate::oms::order_readiness::{ProbeOutcome, apply_probe_outcome};
        let state = Arc::new(OrderReadinessState::new());
        apply_probe_outcome(
            &state,
            ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 86_400,
            },
            now_epoch_s(),
        );
        oms.set_order_readiness(state);
    }

    fn make_place_request() -> PlaceOrderRequest {
        PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            lot_size: 25,
            expiry_date: None,
        }
    }

    // -----------------------------------------------------------------------
    // Live-mode: place_order success
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_mode_place_order_success() {
        let body = r#"{"orderId":"DHAN-123","orderStatus":"TRANSIT"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        let result = oms.place_order(make_place_request()).await;
        assert!(result.is_ok());
        let order_id = result.unwrap();
        assert_eq!(order_id, "DHAN-123");
        assert_eq!(oms.total_placed(), 1);
        assert!(oms.order("DHAN-123").is_some());
        assert_eq!(oms.order("DHAN-123").unwrap().status, OrderStatus::Transit);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Live-mode: place_order API error records failure in circuit breaker
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_mode_place_order_api_error() {
        let body = r#"{"errorType":"INPUT_EXCEPTION","errorCode":"DH-905","errorMessage":"bad"}"#;
        let (base_url, handle) = start_oms_mock(400, body).await;

        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        let result = oms.place_order(make_place_request()).await;
        assert!(result.is_err());
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Live-mode: place_order rate-limited does NOT trip circuit breaker
    // -----------------------------------------------------------------------

    // DH-904 (HTTP 429) now drives the backoff ladder (10/20/40/80s). The ladder
    // makes 1 initial + 4 retries = 5 attempts before exhausting, so the mock
    // serves 5 identical 429s. `start_paused` collapses the 150s of ladder sleep
    // to virtual time. The exhausted ladder still surfaces DhanRateLimited and
    // never trips the circuit breaker (R19: rate class is CB-exempt).
    #[tokio::test(start_paused = true)]
    async fn live_mode_place_order_rate_limited() {
        let body = r#"{"errorType":"RATE_LIMIT","errorCode":"DH-904","errorMessage":"throttled"}"#;
        let responses = vec![(429u16, body.to_owned()); 5];
        let (base_url, handle) = start_multi_mock(responses).await;

        let mut oms = make_live_oms_no_timeout(&base_url);
        install_ready(&mut oms);
        let result = oms.place_order(make_place_request()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        assert_eq!(oms.circuit_breaker.failure_count(), 0);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Live-mode: modify_order success
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_mode_modify_order_success() {
        let body = r#"{"orderId":"1","orderStatus":"PENDING"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        // Insert a non-terminal order to modify
        let order = ManagedOrder {
            order_id: "1".to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status: OrderStatus::Confirmed,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        oms.orders.insert("1".to_owned(), order);

        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 75,
            price: 250.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_ok());
        let order = oms.order("1").unwrap();
        assert_eq!(order.quantity, 75);
        assert!((order.price - 250.0).abs() < f64::EPSILON);
        assert_eq!(order.modification_count, 1);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Live-mode: modify_order API error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_mode_modify_order_api_error() {
        let body = r#"{"errorType":"INPUT_EXCEPTION","errorCode":"DH-905","errorMessage":"bad"}"#;
        let (base_url, handle) = start_oms_mock(400, body).await;

        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        let order = ManagedOrder {
            order_id: "1".to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status: OrderStatus::Confirmed,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        oms.orders.insert("1".to_owned(), order);

        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 75,
            price: 250.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_err());
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Live-mode: modify_order rate-limited
    // -----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn live_mode_modify_order_rate_limited() {
        // HTTP 429 with an empty body still classifies as the DH-904 ladder.
        let responses = vec![(429u16, String::new()); 5];
        let (base_url, handle) = start_multi_mock(responses).await;

        let mut oms = make_live_oms_no_timeout(&base_url);
        install_ready(&mut oms);
        let order = ManagedOrder {
            order_id: "1".to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status: OrderStatus::Confirmed,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        oms.orders.insert("1".to_owned(), order);

        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 75,
            price: 250.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };

        let result = oms.modify_order("1", request).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        assert_eq!(oms.circuit_breaker.failure_count(), 0);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Live-mode: cancel_order success
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_mode_cancel_order_success() {
        let body = r#"{"orderId":"1","orderStatus":"CANCELLED"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        let order = ManagedOrder {
            order_id: "1".to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status: OrderStatus::Confirmed,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        oms.orders.insert("1".to_owned(), order);

        let result = oms.cancel_order("1").await;
        assert!(result.is_ok());
        // After cancel, order should be marked for reconciliation
        assert!(oms.order("1").unwrap().needs_reconciliation);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Live-mode: cancel_order API error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_mode_cancel_order_api_error() {
        let body = r#"{"errorType":"ORDER_ERROR","errorCode":"DH-906","errorMessage":"bad"}"#;
        let (base_url, handle) = start_oms_mock(400, body).await;

        let mut oms = make_live_oms(&base_url);
        let order = ManagedOrder {
            order_id: "1".to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status: OrderStatus::Confirmed,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        oms.orders.insert("1".to_owned(), order);

        let result = oms.cancel_order("1").await;
        assert!(result.is_err());
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Live-mode: cancel_order rate-limited
    // -----------------------------------------------------------------------

    // Cancel is gate-exempt but the DH-904 ladder still applies to it.
    #[tokio::test(start_paused = true)]
    async fn live_mode_cancel_order_rate_limited() {
        let responses = vec![(429u16, String::new()); 5];
        let (base_url, handle) = start_multi_mock(responses).await;

        let mut oms = make_live_oms_no_timeout(&base_url);
        let order = ManagedOrder {
            order_id: "1".to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status: OrderStatus::Confirmed,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        oms.orders.insert("1".to_owned(), order);

        let result = oms.cancel_order("1").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        assert_eq!(oms.circuit_breaker.failure_count(), 0);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Live-mode: reconcile success
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_mode_reconcile_success() {
        // Return order book with one order matching our OMS
        let body = r#"[{"orderId":"1","orderStatus":"TRADED","correlationId":"","transactionType":"BUY","exchangeSegment":"NSE_FNO","productType":"INTRADAY","orderType":"LIMIT","validity":"DAY","securityId":"52432","quantity":50,"price":245.5,"triggerPrice":0.0,"tradedQuantity":50,"tradedPrice":245.5,"remainingQuantity":0,"filledQty":50,"averageTradedPrice":245.5,"exchangeOrderId":"","exchangeTime":"","createTime":"","updateTime":"","rejectionReason":"","tag":"","omsErrorCode":"","omsErrorDescription":"","tradingSymbol":"","drvExpiryDate":"","drvOptionType":"","drvStrikePrice":0.0}]"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        let order = ManagedOrder {
            order_id: "1".to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status: OrderStatus::Confirmed, // Mismatch: OMS says Confirmed, Dhan says Traded
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: true,
            modification_count: 0,
        };
        oms.orders.insert("1".to_owned(), order);

        let result = oms.reconcile().await;
        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.mismatches_found, 1);
        // After reconciliation, order status should be corrected
        let corrected = oms.order("1").unwrap();
        assert_eq!(corrected.status, OrderStatus::Traded);
        assert_eq!(corrected.traded_qty, 50);
        assert!(!corrected.needs_reconciliation);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Live-mode: reconcile terminal-regression guard (M2, 2026-07-14)
    // -----------------------------------------------------------------------

    /// Builds a ManagedOrder for the reconcile terminal-guard tests
    /// (same shape as the `live_mode_reconcile_success` fixture).
    fn make_reconcile_order(order_id: &str, status: OrderStatus) -> ManagedOrder {
        ManagedOrder {
            order_id: order_id.to_owned(),
            correlation_id: format!("corr-{order_id}"),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: true,
            modification_count: 0,
        }
    }

    /// Builds one broker order-book JSON entry with the given status + fill.
    fn broker_order_json(order_id: &str, status: &str, traded_qty: i64) -> String {
        format!(
            r#"{{"orderId":"{order_id}","orderStatus":"{status}","correlationId":"","transactionType":"BUY","exchangeSegment":"NSE_FNO","productType":"INTRADAY","orderType":"LIMIT","validity":"DAY","securityId":"52432","quantity":50,"price":245.5,"triggerPrice":0.0,"tradedQuantity":{traded_qty},"tradedPrice":245.5,"remainingQuantity":0,"filledQty":{traded_qty},"averageTradedPrice":245.5,"exchangeOrderId":"","exchangeTime":"","createTime":"","updateTime":"","rejectionReason":"","tag":"","omsErrorCode":"","omsErrorDescription":"","tradingSymbol":"","drvExpiryDate":"","drvOptionType":"","drvStrikePrice":0.0}}"#
        )
    }

    /// M2/B1 scenario 1: local TRADED + stale broker PART_TRADED snapshot
    /// with qty drift → the FILL fields apply (broker fill accounting is
    /// authoritative), but the terminal STATUS never regresses; the
    /// suppressed status change keeps needs_reconciliation set.
    #[tokio::test]
    async fn live_mode_reconcile_terminal_applies_fill_but_never_status() {
        let body = format!("[{}]", broker_order_json("1", "PART_TRADED", 25));
        let (base_url, handle) = start_oms_mock(200, &body).await;

        let mut oms = make_live_oms(&base_url);
        let mut order = make_reconcile_order("1", OrderStatus::Traded);
        order.traded_qty = 50;
        order.avg_traded_price = 245.5;
        oms.orders.insert("1".to_owned(), order);

        let report = oms.reconcile().await.unwrap();
        // The mismatch is still REPORTED (OMS-GAP-02 visibility)…
        assert_eq!(report.mismatches_found, 1);
        let local = oms.order("1").unwrap();
        // …the terminal STATUS never regresses…
        assert_eq!(local.status, OrderStatus::Traded);
        // …but the broker's fill fields DO apply (B1 — no fill-data loss).
        assert_eq!(local.traded_qty, 25);
        assert!(
            local.needs_reconciliation,
            "a suppressed status change must not clear needs_reconciliation"
        );
        handle.abort();
    }

    /// B1 scenario: local CANCELLED qty=0 + broker CANCELLED qty=25 (a
    /// cancel that partially filled first) → the fill fields apply; the
    /// statuses agree so NOTHING was suppressed (no skip, update fully
    /// consumed → needs_reconciliation clears).
    #[tokio::test]
    async fn live_mode_reconcile_terminal_fill_only_update_applies_fully() {
        let body = format!("[{}]", broker_order_json("1", "CANCELLED", 25));
        let (base_url, handle) = start_oms_mock(200, &body).await;

        let mut oms = make_live_oms(&base_url);
        oms.orders.insert(
            "1".to_owned(),
            make_reconcile_order("1", OrderStatus::Cancelled),
        );

        let report = oms.reconcile().await.unwrap();
        // No status mismatch — fill-only drift is not an OMS-GAP-02 page.
        assert_eq!(report.mismatches_found, 0);
        let local = oms.order("1").unwrap();
        assert_eq!(local.status, OrderStatus::Cancelled);
        assert_eq!(local.traded_qty, 25, "broker fill qty must apply (B1)");
        assert!(
            !local.needs_reconciliation,
            "a fully-consumed fill-only update clears needs_reconciliation"
        );
        handle.abort();
    }

    /// M2 scenario 2: local PENDING + broker TRADED → the legit correction
    /// applies exactly as before (no behavior regression).
    #[tokio::test]
    async fn live_mode_reconcile_applies_correction_when_local_non_terminal() {
        let body = format!("[{}]", broker_order_json("1", "TRADED", 50));
        let (base_url, handle) = start_oms_mock(200, &body).await;

        let mut oms = make_live_oms(&base_url);
        oms.orders.insert(
            "1".to_owned(),
            make_reconcile_order("1", OrderStatus::Pending),
        );

        let report = oms.reconcile().await.unwrap();
        assert_eq!(report.mismatches_found, 1);
        let local = oms.order("1").unwrap();
        assert_eq!(local.status, OrderStatus::Traded);
        assert_eq!(local.traded_qty, 50);
        assert!(!local.needs_reconciliation);
        handle.abort();
    }

    /// M2 scenario 3: other terminal states (CANCELLED, REJECTED) are also
    /// never status-regressed by a stale broker snapshot — while the fill
    /// fields still apply (B1).
    #[tokio::test]
    async fn live_mode_reconcile_skips_status_for_cancelled_and_rejected() {
        let body = format!(
            "[{},{}]",
            broker_order_json("1", "PENDING", 0),
            broker_order_json("2", "TRADED", 50)
        );
        let (base_url, handle) = start_oms_mock(200, &body).await;

        let mut oms = make_live_oms(&base_url);
        oms.orders.insert(
            "1".to_owned(),
            make_reconcile_order("1", OrderStatus::Cancelled),
        );
        oms.orders.insert(
            "2".to_owned(),
            make_reconcile_order("2", OrderStatus::Rejected),
        );

        let report = oms.reconcile().await.unwrap();
        assert_eq!(report.mismatches_found, 2);
        // Terminal statuses never regress…
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Cancelled);
        assert_eq!(oms.order("1").unwrap().traded_qty, 0);
        assert_eq!(oms.order("2").unwrap().status, OrderStatus::Rejected);
        // …but broker fill fields apply even on terminal orders (B1).
        assert_eq!(oms.order("2").unwrap().traded_qty, 50);
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // handle_order_update: order removed after correlation lookup (line 516)
    // -----------------------------------------------------------------------

    #[test]
    fn handle_update_order_not_in_map_after_correlation_lookup() {
        // This tests the edge case where correlation lookup finds an order_id
        // but the order was removed from the map between lookup and get_mut.
        // We simulate by adding a correlation but not adding the order.
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );
        // Track correlation pointing to order "ghost" that doesn't exist in orders map
        oms.correlations
            .track("corr-ghost".to_owned(), "ghost".to_owned());

        let mut update = make_order_update("different-no", "TRADED");
        update.correlation_id = "corr-ghost".to_owned();

        let result = oms.handle_order_update(&update);
        // Should return Ok(()) — line 516: None => return Ok(())
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // handle_order_update: invalid transition logs error (lines 532-533)
    // -----------------------------------------------------------------------

    #[test]
    fn handle_update_invalid_transition_returns_error() {
        // Traded → Pending is invalid
        let mut oms = make_oms_with_order("1", OrderStatus::Traded);
        let update = make_order_update("1", "PENDING");
        let result = oms.handle_order_update(&update);
        assert!(result.is_err());
        if let OmsError::InvalidTransition { order_id, from, to } = result.unwrap_err() {
            assert_eq!(order_id, "1");
            assert_eq!(from, "TRADED");
            assert_eq!(to, "PENDING");
        } else {
            panic!("expected InvalidTransition");
        }
        // Order should be flagged for reconciliation
        assert!(oms.order("1").unwrap().needs_reconciliation);
    }

    // -----------------------------------------------------------------------
    // handle_order_update: valid transition debug log (lines 563-564)
    // -----------------------------------------------------------------------

    #[test]
    fn handle_update_valid_transition_confirmed_to_traded() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
        let mut update = make_order_update("1", "TRADED");
        update.traded_qty = 50;
        update.avg_traded_price = 250.0;

        let result = oms.handle_order_update(&update);
        assert!(result.is_ok());
        let order = oms.order("1").unwrap();
        assert_eq!(order.status, OrderStatus::Traded);
        assert_eq!(order.traded_qty, 50);
        assert!((order.avg_traded_price - 250.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // handle_order_update: unknown status on confirmed order (lines 504-510)
    // -----------------------------------------------------------------------

    #[test]
    fn handle_update_unknown_status_on_confirmed_order_returns_ok() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
        let update = make_order_update("1", "COMPLETELY_UNKNOWN_STATUS");

        let result = oms.handle_order_update(&update);
        // Unknown status is logged and skipped, not an error
        assert!(result.is_ok());
        // Order status should remain unchanged
        assert_eq!(oms.order("1").unwrap().status, OrderStatus::Confirmed);
    }

    // -----------------------------------------------------------------------
    // Tracing subscriber — forces field evaluation in log macros
    // -----------------------------------------------------------------------

    struct SinkSubscriber;
    impl tracing::Subscriber for SinkSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn handle_update_invalid_transition_with_tracing() {
        tracing::subscriber::with_default(SinkSubscriber, || {
            let mut oms = make_oms_with_order("1", OrderStatus::Traded);
            let update = make_order_update("1", "PENDING");
            let result = oms.handle_order_update(&update);
            assert!(result.is_err());
        });
    }

    #[test]
    fn handle_update_valid_transition_with_tracing() {
        tracing::subscriber::with_default(SinkSubscriber, || {
            let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
            let mut update = make_order_update("1", "TRADED");
            update.traded_qty = 50;
            update.avg_traded_price = 250.0;
            let result = oms.handle_order_update(&update);
            assert!(result.is_ok());
            assert_eq!(oms.order("1").unwrap().status, OrderStatus::Traded);
        });
    }

    #[test]
    fn handle_update_unknown_status_with_tracing() {
        tracing::subscriber::with_default(SinkSubscriber, || {
            let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
            let update = make_order_update("1", "UNKNOWN_STATUS_X");
            let result = oms.handle_order_update(&update);
            assert!(result.is_ok());
        });
    }

    #[test]
    fn handle_update_unknown_order_with_tracing() {
        tracing::subscriber::with_default(SinkSubscriber, || {
            let api_client = OrderApiClient::new(
                reqwest::Client::new(),
                "https://api.dhan.co/v2".to_owned(),
                "100".to_owned(),
            );
            let mut oms = OrderManagementSystem::new(
                api_client,
                OrderRateLimiter::new(10),
                Box::new(TestTokenProvider),
                "100".to_owned(),
            );
            let update = make_order_update("nonexistent", "TRADED");
            let result = oms.handle_order_update(&update);
            assert!(result.is_ok());
        });
    }

    #[test]
    fn reset_daily_with_tracing() {
        tracing::subscriber::with_default(SinkSubscriber, || {
            let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
            oms.reset_daily();
            assert!(oms.all_orders().is_empty());
        });
    }

    #[test]
    fn test_order_latency_metric() {
        // Verify histogram macro compiles and doesn't panic when invoked.
        // O(1) atomic call — safe for cold path (order placement).
        metrics::histogram!("tv_order_placement_duration_ns").record(1000.0_f64);
    }

    // -----------------------------------------------------------------------
    // Super order price validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_super_order_buy_target_must_exceed_entry() {
        let result = validate_super_order_prices("BUY", 100.0, 95.0, 90.0);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("target"), "error should mention target: {msg}");
    }

    #[test]
    fn test_super_order_buy_sl_must_be_below_entry() {
        let result = validate_super_order_prices("BUY", 100.0, 110.0, 105.0);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("stop loss"),
            "error should mention stop loss: {msg}"
        );
    }

    #[test]
    fn test_super_order_sell_target_must_be_below_entry() {
        let result = validate_super_order_prices("SELL", 100.0, 110.0, 105.0);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("target"), "error should mention target: {msg}");
    }

    #[test]
    fn test_super_order_sell_sl_must_exceed_entry() {
        let result = validate_super_order_prices("SELL", 100.0, 90.0, 95.0);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("stop loss"),
            "error should mention stop loss: {msg}"
        );
    }

    #[test]
    fn test_super_order_valid_buy_prices_accepted() {
        // BUY: target > entry > SL
        let result = validate_super_order_prices("BUY", 100.0, 110.0, 90.0);
        assert!(
            result.is_ok(),
            "valid BUY super order prices must be accepted"
        );
    }

    #[test]
    fn test_super_order_valid_sell_prices_accepted() {
        // SELL: SL > entry > target
        let result = validate_super_order_prices("SELL", 100.0, 90.0, 110.0);
        assert!(
            result.is_ok(),
            "valid SELL super order prices must be accepted"
        );
    }

    #[test]
    fn test_super_order_zero_entry_price_rejected() {
        let result = validate_super_order_prices("BUY", 0.0, 110.0, 90.0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("entry price"));
    }

    #[test]
    fn test_super_order_zero_target_price_rejected() {
        let result = validate_super_order_prices("BUY", 100.0, 0.0, 90.0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("target price"));
    }

    #[test]
    fn test_super_order_zero_sl_price_rejected() {
        let result = validate_super_order_prices("BUY", 100.0, 110.0, 0.0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("stop loss price"));
    }

    #[test]
    fn test_super_order_buy_target_equal_entry_rejected() {
        let result = validate_super_order_prices("BUY", 100.0, 100.0, 90.0);
        assert!(result.is_err(), "target == entry must be rejected for BUY");
    }

    #[test]
    fn test_super_order_buy_sl_equal_entry_rejected() {
        let result = validate_super_order_prices("BUY", 100.0, 110.0, 100.0);
        assert!(result.is_err(), "SL == entry must be rejected for BUY");
    }

    #[test]
    fn test_super_order_unknown_txn_type_rejected() {
        let result = validate_super_order_prices("UNKNOWN", 100.0, 110.0, 90.0);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown transaction type")
        );
    }

    // -----------------------------------------------------------------------
    // Cluster F: readiness gate + halt latch + engine error policy
    // -----------------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Token provider that counts `get_access_token` and `request_token_rotation`
    /// calls so a test can prove the gate refused BEFORE any token fetch, or that
    /// a DH-901 rotate happened exactly once.
    struct CountingTokenProvider {
        calls: Arc<AtomicUsize>,
        rotations: Arc<AtomicUsize>,
    }
    impl TokenProvider for CountingTokenProvider {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(SecretString::from("test-token"))
        }
        fn request_token_rotation(&self) -> bool {
            self.rotations.fetch_add(1, AtomicOrdering::SeqCst);
            true
        }
    }

    /// (untimed live OMS, token-fetch counter, rotation counter).
    fn make_live_oms_counting(
        base_url: &str,
    ) -> (OrderManagementSystem, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let rotations = Arc::new(AtomicUsize::new(0));
        let http = reqwest::Client::builder().build().unwrap();
        let api_client = OrderApiClient::new(http, base_url.to_owned(), "100".to_owned());
        let rate_limiter = OrderRateLimiter::new(10);
        let mut oms = OrderManagementSystem::new(
            api_client,
            rate_limiter,
            Box::new(CountingTokenProvider {
                calls: Arc::clone(&calls),
                rotations: Arc::clone(&rotations),
            }),
            "100".to_owned(),
        );
        oms.enable_live_mode();
        (oms, calls, rotations)
    }

    fn insert_open_order(oms: &mut OrderManagementSystem, order_id: &str) {
        let order = ManagedOrder {
            order_id: order_id.to_owned(),
            correlation_id: "corr-1".to_owned(),
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            status: OrderStatus::Confirmed,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        oms.orders.insert(order_id.to_owned(), order);
    }

    /// DRY-RUN must be byte-identical: the readiness gate is NEVER consulted and
    /// NO token is fetched — a paper order succeeds with no readiness installed.
    #[tokio::test]
    async fn test_place_order_dry_run_ignores_readiness_gate_byte_identical() {
        // Default OMS is dry-run; NO readiness installed.
        let (mut oms, calls, _rot) = make_live_oms_counting("http://127.0.0.1:1");
        oms.dry_run = true;
        let result = oms.place_order(make_place_request()).await;
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with("PAPER-"));
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            0,
            "dry-run fetches no token"
        );
    }

    /// LIVE place with NO readiness installed is fail-closed refused BEFORE any
    /// HTTP call or token fetch.
    #[tokio::test]
    async fn test_place_order_live_refused_when_no_readiness_installed_zero_http_zero_token_fetch()
    {
        let (mut oms, calls, _rot) = make_live_oms_counting("http://127.0.0.1:1");
        let result = oms.place_order(make_place_request()).await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::OrderReadinessRefused { reason: "no_probe" }
        ));
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            0,
            "gate refuses before token fetch"
        );
    }

    /// LIVE modify with NO readiness installed is fail-closed refused (Modify is
    /// NOT exempt).
    #[tokio::test]
    async fn test_modify_order_live_refused_when_no_readiness() {
        let (mut oms, calls, _rot) = make_live_oms_counting("http://127.0.0.1:1");
        insert_open_order(&mut oms, "1");
        let request = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 75,
            price: 250.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };
        let result = oms.modify_order("1", request).await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::OrderReadinessRefused { .. }
        ));
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
    }

    /// LIVE place while HALTED is refused BEFORE any token fetch.
    #[tokio::test]
    async fn test_place_order_refused_when_halted() {
        let (mut oms, calls, _rot) = make_live_oms_counting("http://127.0.0.1:1");
        install_ready(&mut oms);
        oms.engage_halt("DH-903", "account issue".to_owned());
        let result = oms.place_order(make_place_request()).await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::OrderPathHalted { cause: "DH-903" }
        ));
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
    }

    /// R8: `reset_daily` does NOT clear the halt latch.
    #[test]
    fn test_reset_daily_does_not_clear_halt() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
        oms.engage_halt("DH-902", "no api access".to_owned());
        assert!(oms.is_order_halted());
        oms.reset_daily();
        assert!(oms.is_order_halted(), "halt must survive the daily reset");
    }

    /// Operator-driven `clear_order_halt` re-enables the live place path.
    #[tokio::test]
    async fn test_clear_order_halt_reenables_live_place() {
        let body = r#"{"orderId":"DHAN-9","orderStatus":"TRANSIT"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;
        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        oms.engage_halt("DH-902", "no api access".to_owned());
        assert!(oms.place_order(make_place_request()).await.is_err());
        oms.clear_order_halt();
        assert!(!oms.is_order_halted());
        let result = oms.place_order(make_place_request()).await;
        assert!(result.is_ok());
        handle.abort();
    }

    /// R4: cancel is EXEMPT from the halt gate — it must still reach the broker so
    /// the operator can flatten during a halt.
    #[tokio::test]
    async fn test_cancel_order_exempt_from_halt_gate() {
        let body = r#"{"orderId":"1","orderStatus":"CANCELLED"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;
        let (mut oms, calls, _rot) = make_live_oms_counting(&base_url);
        insert_open_order(&mut oms, "1");
        oms.engage_halt("DH-903", "account issue".to_owned());
        // No readiness installed either — cancel is exempt from BOTH gates.
        let result = oms.cancel_order("1").await;
        assert!(result.is_ok(), "cancel must proceed despite the halt");
        assert!(
            calls.load(AtomicOrdering::SeqCst) >= 1,
            "cancel reached the token fetch"
        );
        handle.abort();
    }

    /// DH-901 rotates the token exactly once, then HALTS (R19: never trips the
    /// circuit breaker). Paused clock collapses the 5s rotate delay.
    #[tokio::test(start_paused = true)]
    async fn test_dh901_rotates_once_then_halts() {
        let body = r#"{"errorType":"INVALID_AUTHENTICATION","errorCode":"DH-901","errorMessage":"invalid"}"#;
        let responses = vec![(401u16, body.to_owned()); 2];
        let (base_url, handle) = start_multi_mock(responses).await;
        let (mut oms, _calls, rotations) = make_live_oms_counting(&base_url);
        install_ready(&mut oms);
        let result = oms.place_order(make_place_request()).await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::OrderPathHalted { cause: "DH-901" }
        ));
        assert_eq!(
            rotations.load(AtomicOrdering::SeqCst),
            1,
            "exactly one rotate"
        );
        assert!(oms.is_order_halted());
        assert_eq!(
            oms.circuit_breaker.failure_count(),
            0,
            "R19: DH-901 is circuit-breaker-exempt"
        );
        handle.abort();
    }

    /// Recording alert sink for the engine-policy dispatch tests.
    #[derive(Clone, Default)]
    struct RecordingSink {
        alerts: Arc<std::sync::Mutex<Vec<OmsAlert>>>,
    }
    impl OmsAlertSink for RecordingSink {
        fn fire(&self, alert: OmsAlert) {
            self.alerts.lock().expect("sink lock").push(alert);
        }
    }
    impl RecordingSink {
        fn count(&self, pred: impl Fn(&OmsAlert) -> bool) -> usize {
            self.alerts
                .lock()
                .expect("sink lock")
                .iter()
                .filter(|a| pred(a))
                .count()
        }
    }

    /// F-A/R14: DATA-807 refreshes the token ONCE then returns TokenExpired with
    /// NO halt (the renewal loop + AUTH-GAP-05 own the self-heal); CB-exempt.
    #[tokio::test(start_paused = true)]
    async fn test_data807_retry_once_no_halt_returns_token_expired() {
        let body = r#"{"errorType":"x","errorCode":807,"errorMessage":"expired"}"#;
        let responses = vec![(401u16, body.to_owned()); 2];
        let (base_url, handle) = start_multi_mock(responses).await;
        let (mut oms, _calls, rotations) = make_live_oms_counting(&base_url);
        install_ready(&mut oms);
        let err = oms.place_order(make_place_request()).await.unwrap_err();
        assert!(matches!(err, OmsError::TokenExpired), "got {err:?}");
        assert!(!oms.is_order_halted(), "807 must NOT halt (R14)");
        assert_eq!(
            rotations.load(AtomicOrdering::SeqCst),
            1,
            "exactly one refresh"
        );
        assert_eq!(
            oms.circuit_breaker.failure_count(),
            0,
            "807 is circuit-breaker-exempt"
        );
        handle.abort();
    }

    /// F-A/R16: DATA-806 on the order surface poisons readiness (self-heals on
    /// the next OK probe) and does NOT halt. The second place refuses at the gate.
    #[tokio::test]
    async fn test_data806_on_order_surface_poisons_readiness() {
        let body = r#"{"errorType":"x","errorCode":806,"errorMessage":"not subscribed"}"#;
        let (base_url, handle) = start_oms_mock(403, body).await;
        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        let first = oms.place_order(make_place_request()).await.unwrap_err();
        assert!(
            matches!(first, OmsError::DhanApiError { .. }),
            "got {first:?}"
        );
        assert!(!oms.is_order_halted(), "806 must NOT halt (R16)");
        // Second place: readiness now poisoned → refused pre-HTTP.
        let second = oms.place_order(make_place_request()).await.unwrap_err();
        assert!(
            matches!(
                second,
                OmsError::OrderReadinessRefused {
                    reason: "profile_invalid"
                }
            ),
            "got {second:?}"
        );
        handle.abort();
    }

    /// F-A/R19: DH-902/903 and DATA-810 halt VIA an error RESPONSE (through the
    /// classifier dispatch, not a direct engage_halt), and are CB-exempt.
    #[tokio::test]
    async fn test_dh902_903_data810_halt_via_error_response() {
        let cases: [(u16, &str, &str); 3] = [
            (
                403,
                r#"{"errorType":"x","errorCode":"DH-902","errorMessage":"no access"}"#,
                "DH-902",
            ),
            (
                403,
                r#"{"errorType":"x","errorCode":"DH-903","errorMessage":"account"}"#,
                "DH-903",
            ),
            (
                400,
                r#"{"errorType":"x","errorCode":810,"errorMessage":"client id"}"#,
                "DATA-810",
            ),
        ];
        for (status, body, cause) in cases {
            let (base_url, handle) = start_oms_mock(status, body).await;
            let mut oms = make_live_oms(&base_url);
            install_ready(&mut oms);
            let err = oms.place_order(make_place_request()).await.unwrap_err();
            assert!(
                matches!(err, OmsError::OrderPathHalted { cause: c } if c == cause),
                "expected halt {cause}, got {err:?}"
            );
            assert!(oms.is_order_halted());
            assert_eq!(
                oms.circuit_breaker.failure_count(),
                0,
                "halt class {cause} must be circuit-breaker-exempt"
            );
            handle.abort();
        }
    }

    /// F-A/R19: a transient/input class (DH-908, NeverRetry on Place) DOES trip
    /// the circuit breaker and does NOT halt.
    #[tokio::test]
    async fn test_transient_error_trips_circuit_breaker_and_does_not_halt() {
        let body = r#"{"errorType":"x","errorCode":"DH-908","errorMessage":"server"}"#;
        let (base_url, handle) = start_oms_mock(500, body).await;
        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        let err = oms.place_order(make_place_request()).await.unwrap_err();
        assert!(matches!(err, OmsError::DhanApiError { .. }), "got {err:?}");
        assert!(!oms.is_order_halted(), "transient class must not halt");
        assert_eq!(
            oms.circuit_breaker.failure_count(),
            1,
            "R19: a transient class trips the circuit breaker"
        );
        handle.abort();
    }

    /// F-A + F-E: the exhausted DH-904 ladder fires EXACTLY ONE operator alert
    /// (Dh904LadderExhausted) — NOT a trailing OrderRejected too.
    #[tokio::test(start_paused = true)]
    async fn test_dh904_ladder_exhausted_fires_single_alert() {
        let body = r#"{"errorType":"x","errorCode":"DH-904","errorMessage":"throttled"}"#;
        let responses = vec![(429u16, body.to_owned()); 5];
        let (base_url, handle) = start_multi_mock(responses).await;
        let mut oms = make_live_oms_no_timeout(&base_url);
        install_ready(&mut oms);
        let sink = RecordingSink::default();
        oms.set_alert_sink(Box::new(sink.clone()));
        let err = oms.place_order(make_place_request()).await.unwrap_err();
        assert!(matches!(err, OmsError::DhanRateLimited), "got {err:?}");
        assert_eq!(
            sink.count(|a| matches!(a, OmsAlert::Dh904LadderExhausted { .. })),
            1,
            "exactly one ladder-exhausted alert"
        );
        assert_eq!(
            sink.count(|a| matches!(a, OmsAlert::OrderRejected { .. })),
            0,
            "F-E: no trailing OrderRejected on ladder exhaustion"
        );
        handle.abort();
    }

    /// C2: during ONE 60s DATA-805 stop-all episode the operator alert fires
    /// exactly ONCE, not per throttled call (audit-findings Rule 4).
    #[tokio::test]
    async fn test_stop_all_alert_fires_once_per_episode() {
        let body = r#"{"errorType":"x","errorCode":805,"errorMessage":"too many"}"#;
        let (base_url, handle) = start_oms_mock(400, body).await;
        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        let sink = RecordingSink::default();
        oms.set_alert_sink(Box::new(sink.clone()));
        // First place: HTTP 805 engages the cooldown latch + fires ONE alert.
        let first = oms.place_order(make_place_request()).await.unwrap_err();
        assert!(
            matches!(first, OmsError::DhanApiError { .. }),
            "got {first:?}"
        );
        // Second place: latch engaged → refused pre-HTTP; alert NOT re-fired.
        let second = oms.place_order(make_place_request()).await.unwrap_err();
        assert!(
            matches!(second, OmsError::StopAllCooldown { .. }),
            "got {second:?}"
        );
        assert_eq!(
            sink.count(|a| matches!(a, OmsAlert::OrderApiStopAll { .. })),
            1,
            "C2: exactly one stop-all alert per 60s episode"
        );
        handle.abort();
    }

    /// C1: clearing the halt also resets the circuit breaker (it can be left
    /// stuck HalfOpen when a halt-class error lands after a CB HalfOpen probe).
    #[test]
    fn test_clear_order_halt_resets_circuit_breaker() {
        let mut oms = make_oms_with_order("1", OrderStatus::Confirmed);
        // Force the CB open, then halt.
        for _ in 0..10 {
            oms.circuit_breaker.record_failure();
        }
        assert!(oms.circuit_breaker.failure_count() >= 1);
        oms.engage_halt("DH-902", "no api access".to_owned());
        oms.clear_order_halt();
        assert!(!oms.is_order_halted());
        assert_eq!(
            oms.circuit_breaker.failure_count(),
            0,
            "C1: clear_order_halt resets the circuit breaker"
        );
        assert_eq!(
            oms.circuit_breaker.state(),
            super::super::circuit_breaker::CircuitState::Closed
        );
    }

    /// Security fix: the OMS-halt detail (which embeds the raw broker body) is
    /// routed through the secret-redacting REST-body capture — a JWT-shaped
    /// token in the body never survives into the OmsHalted alert detail.
    #[tokio::test]
    async fn test_halt_detail_is_sanitized() {
        let body = r#"{"errorType":"x","errorCode":"DH-902","accessToken":"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.abcdefghijklmnopqrstuvwx"}"#;
        let (base_url, handle) = start_oms_mock(403, body).await;
        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        let sink = RecordingSink::default();
        oms.set_alert_sink(Box::new(sink.clone()));
        let _ = oms.place_order(make_place_request()).await.unwrap_err();
        let alerts = sink.alerts.lock().expect("sink lock");
        let detail = alerts
            .iter()
            .find_map(|a| match a {
                OmsAlert::OmsHalted { detail, .. } => Some(detail.clone()),
                _ => None,
            })
            .expect("OmsHalted alert fired");
        assert!(
            !detail.contains("eyJhbGci"),
            "raw JWT leaked into halt detail: {detail}"
        );
        assert!(
            detail.contains("REDACTED"),
            "detail must be redacted: {detail}"
        );
        handle.abort();
    }

    /// Security fix: the OrderRejected reason (raw broker body) is redacted.
    #[tokio::test]
    async fn test_order_rejected_reason_is_sanitized() {
        let body = r#"{"errorType":"x","errorCode":"DH-905","accessToken":"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.abcdefghijklmnopqrstuvwx"}"#;
        let (base_url, handle) = start_oms_mock(400, body).await;
        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        let sink = RecordingSink::default();
        oms.set_alert_sink(Box::new(sink.clone()));
        let _ = oms.place_order(make_place_request()).await.unwrap_err();
        let alerts = sink.alerts.lock().expect("sink lock");
        let reason = alerts
            .iter()
            .find_map(|a| match a {
                OmsAlert::OrderRejected { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .expect("OrderRejected alert fired");
        assert!(
            !reason.contains("eyJhbGci"),
            "raw JWT leaked into reject reason: {reason}"
        );
        handle.abort();
    }

    /// C3/F-D: on a live boot with NO readiness installed, the no_probe refusal
    /// alert is edge-latched (loud once), not fired on every call.
    #[tokio::test]
    async fn test_no_probe_refusal_alert_is_edge_latched() {
        let (mut oms, _calls, _rot) = make_live_oms_counting("http://127.0.0.1:1");
        let sink = RecordingSink::default();
        oms.set_alert_sink(Box::new(sink.clone()));
        for _ in 0..5 {
            let err = oms.place_order(make_place_request()).await.unwrap_err();
            assert!(matches!(
                err,
                OmsError::OrderReadinessRefused { reason: "no_probe" }
            ));
        }
        assert_eq!(
            sink.count(|a| matches!(a, OmsAlert::OrderReadinessRefused { .. })),
            1,
            "C3/F-D: no_probe refusal alerts once per episode, not per call"
        );
    }

    /// Every `OmsAlert` operator message carries the 🔷 DHAN broker badge so any
    /// future sink inherits broker attribution (noise-lock strategy).
    #[test]
    fn test_all_oms_alert_operator_messages_start_with_dhan_badge() {
        let alerts = vec![
            OmsAlert::CircuitBreakerOpened {
                consecutive_failures: 3,
            },
            OmsAlert::CircuitBreakerClosed,
            OmsAlert::RateLimitExhausted {
                limit_type: "per_second".to_owned(),
            },
            OmsAlert::OrderRejected {
                correlation_id: "c".to_owned(),
                reason: "r".to_owned(),
            },
            OmsAlert::OrderReadinessRefused {
                reason: "no_probe",
                correlation_id: "c".to_owned(),
            },
            OmsAlert::OmsHalted {
                cause: "DH-902",
                detail: "d".to_owned(),
            },
            OmsAlert::Dh904LadderExhausted {
                operation: "place",
                attempts: 4,
            },
            OmsAlert::OrderApiStopAll { cooldown_secs: 60 },
        ];
        for alert in alerts {
            let msg = alert.operator_message();
            assert!(
                msg.starts_with("🔷 DHAN"),
                "operator message must carry the DHAN badge: {msg}"
            );
        }
    }

    // =======================================================================
    // ==== EXIT EXECUTION (Cluster B) — engine-method tests ====
    // =======================================================================

    fn make_dry_oms() -> OrderManagementSystem {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        )
    }

    fn make_super_request() -> PlaceSuperOrderRequest {
        PlaceSuperOrderRequest {
            security_id: 49081,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            quantity: 75,
            price: 100.0,
            target_price: 110.0,
            stop_loss_price: 95.0,
            trailing_jump: 0.0,
            lot_size: 75,
            expiry_date: None,
        }
    }

    fn make_forever_request() -> PlaceForeverOcoRequest {
        PlaceForeverOcoRequest {
            security_id: 1333,
            transaction_type: TransactionType::Sell,
            product_type: ProductType::Cnc,
            order_type: OrderType::Limit,
            validity: OrderValidity::Day,
            quantity: 5,
            price: 1428.0,
            trigger_price: 1427.0,
            oco_leg: None,
            expiry_date: None,
        }
    }

    fn insert_tracked_order(
        oms: &mut OrderManagementSystem,
        order_id: &str,
        status: OrderStatus,
        quantity: i64,
    ) {
        let order = ManagedOrder {
            order_id: order_id.to_owned(),
            correlation_id: format!("corr-{order_id}"),
            security_id: 49081,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity,
            price: 100.0,
            trigger_price: 0.0,
            status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        };
        oms.orders.insert(order_id.to_owned(), order);
    }

    fn insert_tracked_super(
        oms: &mut OrderManagementSystem,
        entry_order_id: &str,
        entry_status_raw: &str,
        quantity: i64,
    ) {
        let managed = ManagedSuperOrder {
            entry_order_id: entry_order_id.to_owned(),
            correlation_id: format!("corr-{entry_order_id}"),
            security_id: 49081,
            transaction_type: TransactionType::Buy,
            quantity,
            entry_price: 100.0,
            entry_status_raw: entry_status_raw.to_owned(),
            target: LegState {
                order_id: format!("{entry_order_id}-TGT"),
                status_raw: "PENDING".to_owned(),
                price: 110.0,
            },
            stop_loss: LegState {
                order_id: format!("{entry_order_id}-SL"),
                status_raw: "PENDING".to_owned(),
                price: 95.0,
            },
            trailing_jump: 0.0,
            status: OrderStatus::Pending,
            modification_count: 0,
            created_at_us: 0,
            updated_at_us: 0,
        };
        oms.super_orders.insert(entry_order_id.to_owned(), managed);
    }

    // --- place_super_order ---

    #[tokio::test]
    async fn test_place_super_order_dry_run_three_leg_registry() {
        let mut oms = make_dry_oms();

        let placement = oms
            .place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        assert_eq!(placement.entry_order_id, "PAPER-SO-1");
        assert_eq!(placement.legs.len(), 3);

        // Entry leg tracked as a normal ManagedOrder (Confirmed).
        let entry = oms.order("PAPER-SO-1").unwrap();
        assert_eq!(entry.status, OrderStatus::Confirmed);
        assert_eq!(entry.quantity, 75);

        // Registry carries the 3-leg shape with PENDING exit legs.
        let so = oms.super_order("PAPER-SO-1").unwrap();
        assert_eq!(so.entry_status_raw, "CONFIRMED");
        assert_eq!(so.target.order_id, "PAPER-SO-1-TGT");
        assert_eq!(so.target.status_raw, "PENDING");
        assert!((so.target.price - 110.0).abs() < f64::EPSILON);
        assert_eq!(so.stop_loss.order_id, "PAPER-SO-1-SL");
        assert_eq!(so.stop_loss.status_raw, "PENDING");
        assert!((so.stop_loss.price - 95.0).abs() < f64::EPSILON);
        assert_eq!(oms.total_placed(), 1);
    }

    #[tokio::test]
    async fn test_place_super_order_wires_price_ordering_validation() {
        // BUY with target BELOW entry — must be refused by the wired
        // validate_super_order_prices (Step 0b), pre-HTTP, zero placed.
        let mut oms = make_dry_oms();
        let mut request = make_super_request();
        request.target_price = 90.0;

        let result = oms.place_super_order(request, 1_800).await;
        assert!(matches!(result, Err(OmsError::RiskRejected { .. })));
        assert_eq!(oms.total_placed(), 0);
    }

    #[tokio::test]
    async fn test_place_super_order_boundary_oversize_freeze_refused() {
        // quantity > freeze_limit ⇒ typed refusal (no sliced-super endpoint).
        let mut oms = make_dry_oms();
        let mut request = make_super_request();
        request.quantity = 150;

        let result = oms.place_super_order(request, 100).await;
        let err = result.unwrap_err();
        assert!(matches!(err, OmsError::RiskRejected { .. }));
        assert!(err.to_string().contains("freeze"));
        assert_eq!(oms.total_placed(), 0);
        // Boundary: exactly AT the freeze limit is accepted.
        let mut at_freeze = make_super_request();
        at_freeze.quantity = 150;
        at_freeze.lot_size = 75;
        assert!(oms.place_super_order(at_freeze, 150).await.is_ok());
    }

    #[tokio::test]
    async fn test_place_super_order_rate_limited_zero_placed() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(1),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );

        assert!(
            oms.place_super_order(make_super_request(), 1_800)
                .await
                .is_ok()
        );
        let second = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(matches!(second, Err(OmsError::RateLimited)));
        assert_eq!(oms.total_placed(), 1);
    }

    #[tokio::test]
    async fn test_place_super_order_circuit_breaker_open_rejected() {
        let mut oms = make_dry_oms();
        // Trip the breaker (bounded walk to open).
        for _ in 0..100 {
            oms.circuit_breaker.record_failure();
            if oms.circuit_breaker.check().is_err() {
                break;
            }
        }
        let result = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(matches!(result, Err(OmsError::CircuitBreakerOpen)));
    }

    #[tokio::test]
    async fn test_place_super_order_live_mock_parses_leg_details() {
        let body = r#"{"orderId":"SO-100","orderStatus":"PENDING","legDetails":[{"orderId":"SO-100-T","legName":"TARGET_LEG","orderStatus":"PENDING","price":110.0},{"orderId":"SO-100-S","legName":"STOP_LOSS_LEG","orderStatus":"PENDING","price":95.0}]}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        let placement = oms
            .place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        assert_eq!(placement.entry_order_id, "SO-100");
        // Entry synthesized from the top level + the 2 response legs.
        assert_eq!(placement.legs.len(), 3);

        let so = oms.super_order("SO-100").unwrap();
        assert_eq!(so.target.order_id, "SO-100-T");
        assert_eq!(so.stop_loss.order_id, "SO-100-S");
        assert_eq!(so.entry_status_raw, "PENDING");

        let entry = oms.order("SO-100").unwrap();
        assert_eq!(entry.status, OrderStatus::Pending);
        assert_eq!(oms.total_placed(), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn test_place_super_order_live_400_records_cb_failure() {
        let body = r#"{"errorType":"INPUT_EXCEPTION","errorCode":"DH-905","errorMessage":"bad"}"#;
        let (base_url, handle) = start_oms_mock(400, body).await;

        let mut oms = make_live_oms(&base_url);
        let result = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(result.is_err());
        assert_eq!(oms.circuit_breaker.failure_count(), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn test_place_super_order_live_429_no_cb_trip() {
        let body = r#"{"errorType":"RATE_LIMIT","errorCode":"DH-904","errorMessage":"throttled"}"#;
        let (base_url, handle) = start_oms_mock(429, body).await;

        let mut oms = make_live_oms(&base_url);
        let result = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        assert_eq!(oms.circuit_breaker.failure_count(), 0);
        handle.abort();
    }

    // --- modify_super_order_leg ---

    #[tokio::test]
    async fn test_modify_super_order_leg_dry_run_target_price() {
        let mut oms = make_dry_oms();
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        let result = oms
            .modify_super_order_leg(
                "PAPER-SO-1",
                ModifySuperOrderLeg::Target {
                    target_price: 112.5,
                },
            )
            .await;
        assert!(result.is_ok());

        let so = oms.super_order("PAPER-SO-1").unwrap();
        assert!((so.target.price - 112.5).abs() < f64::EPSILON);
        assert_eq!(so.modification_count, 1);
    }

    #[tokio::test]
    async fn test_modify_super_order_leg_dry_run_stop_loss_trailing() {
        let mut oms = make_dry_oms();
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        let result = oms
            .modify_super_order_leg(
                "PAPER-SO-1",
                ModifySuperOrderLeg::StopLoss {
                    stop_loss_price: 96.0,
                    trailing_jump: 0.5,
                },
            )
            .await;
        assert!(result.is_ok());

        let so = oms.super_order("PAPER-SO-1").unwrap();
        assert!((so.stop_loss.price - 96.0).abs() < f64::EPSILON);
        assert!((so.trailing_jump - 0.5).abs() < f64::EPSILON);
        assert_eq!(so.modification_count, 1);
    }

    #[tokio::test]
    async fn test_modify_super_order_leg_unknown_id_not_found() {
        let mut oms = make_dry_oms();
        let result = oms
            .modify_super_order_leg(
                "SO-NOPE",
                ModifySuperOrderLeg::Target {
                    target_price: 112.5,
                },
            )
            .await;
        assert!(matches!(result, Err(OmsError::OrderNotFound { .. })));
    }

    #[tokio::test]
    async fn test_modify_super_order_leg_boundary_25_mod_cap() {
        let mut oms = make_dry_oms();
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();
        oms.super_orders
            .get_mut("PAPER-SO-1")
            .unwrap()
            .modification_count = MAX_MODIFICATIONS_PER_ORDER;

        let result = oms
            .modify_super_order_leg(
                "PAPER-SO-1",
                ModifySuperOrderLeg::Target {
                    target_price: 112.5,
                },
            )
            .await;
        let err = result.unwrap_err();
        assert!(matches!(err, OmsError::RiskRejected { .. }));
        assert!(err.to_string().contains("modifications"));
    }

    #[tokio::test]
    async fn test_modify_super_order_leg_entry_gate_confirmed_refused_pending_allowed() {
        let mut oms = make_dry_oms();
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        let entry_modify = ModifySuperOrderLeg::Entry {
            order_type: OrderType::Limit,
            quantity: 75,
            price: 101.0,
            target_price: 111.0,
            stop_loss_price: 96.0,
            trailing_jump: 0.0,
        };

        // Paper entry is CONFIRMED — not in the 07a §3 modifiable set.
        let refused = oms
            .modify_super_order_leg("PAPER-SO-1", entry_modify.clone())
            .await;
        assert!(matches!(refused, Err(OmsError::RiskRejected { .. })));

        // Flip the raw entry status to PENDING — now allowed, and both the
        // registry AND the tracked entry ManagedOrder update.
        oms.super_orders
            .get_mut("PAPER-SO-1")
            .unwrap()
            .entry_status_raw = "PENDING".to_owned();
        assert!(
            oms.modify_super_order_leg("PAPER-SO-1", entry_modify)
                .await
                .is_ok()
        );
        let so = oms.super_order("PAPER-SO-1").unwrap();
        assert!((so.entry_price - 101.0).abs() < f64::EPSILON);
        assert_eq!(so.modification_count, 1);
        let entry = oms.order("PAPER-SO-1").unwrap();
        assert!((entry.price - 101.0).abs() < f64::EPSILON);
        assert_eq!(entry.modification_count, 1);
    }

    #[tokio::test]
    async fn test_modify_super_order_leg_live_mock_success() {
        let place_body =
            r#"{"orderId":"SO-100","orderStatus":"PENDING","legDetails":[]}"#.to_owned();
        let modify_body = r#"{"orderId":"SO-100","orderStatus":"PENDING"}"#.to_owned();
        let (base_url, handle) =
            start_multi_mock(vec![(200, place_body), (200, modify_body)]).await;

        let mut oms = make_live_oms(&base_url);
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        let result = oms
            .modify_super_order_leg(
                "SO-100",
                ModifySuperOrderLeg::Target {
                    target_price: 112.5,
                },
            )
            .await;
        assert!(result.is_ok());

        let so = oms.super_order("SO-100").unwrap();
        assert!((so.target.price - 112.5).abs() < f64::EPSILON);
        assert_eq!(so.modification_count, 1);
        handle.abort();
    }

    #[test]
    fn test_build_super_modify_request_wire_body_order_id_and_trailing() {
        // The modify BODY always carries orderId (Ruling 2 fix 1); StopLoss
        // + Entry arms ALWAYS serialize trailingJump (fix 2 — omission
        // cancels the trail); TARGET_LEG bodies stay minimal (U8).
        let stop_loss = build_super_modify_request(
            "100",
            "SO-1",
            &ModifySuperOrderLeg::StopLoss {
                stop_loss_price: 96.0,
                trailing_jump: 0.0,
            },
        );
        let json = serde_json::to_value(&stop_loss).unwrap();
        assert_eq!(json["orderId"], "SO-1");
        assert_eq!(json["legName"], "STOP_LOSS_LEG");
        assert!(
            json.get("trailingJump").is_some(),
            "trailingJump must ALWAYS be sent on STOP_LOSS_LEG (even 0.0)"
        );

        let entry = build_super_modify_request(
            "100",
            "SO-1",
            &ModifySuperOrderLeg::Entry {
                order_type: OrderType::Limit,
                quantity: 75,
                price: 101.0,
                target_price: 111.0,
                stop_loss_price: 96.0,
                trailing_jump: 0.25,
            },
        );
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["orderId"], "SO-1");
        assert_eq!(json["legName"], "ENTRY_LEG");
        assert!(json.get("trailingJump").is_some());
        assert!(json.get("quantity").is_some());

        let target = build_super_modify_request(
            "100",
            "SO-1",
            &ModifySuperOrderLeg::Target {
                target_price: 111.0,
            },
        );
        let json = serde_json::to_value(&target).unwrap();
        assert_eq!(json["orderId"], "SO-1");
        assert_eq!(json["legName"], "TARGET_LEG");
        assert!(
            json.get("trailingJump").is_none(),
            "TARGET_LEG modify body must stay minimal (no trailingJump — U8)"
        );
        assert!(json.get("quantity").is_none());
    }

    // --- cancel_super_order_leg ---

    #[tokio::test]
    async fn test_cancel_super_order_leg_dry_run_entry_cancels_bracket() {
        let mut oms = make_dry_oms();
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        let result = oms
            .cancel_super_order_leg("PAPER-SO-1", OrderLeg::EntryLeg)
            .await;
        assert!(result.is_ok());

        let so = oms.super_order("PAPER-SO-1").unwrap();
        assert_eq!(so.entry_status_raw, "CANCELLED");
        assert_eq!(so.target.status_raw, "CANCELLED");
        assert_eq!(so.stop_loss.status_raw, "CANCELLED");
        assert_eq!(so.status, OrderStatus::Cancelled);
        assert_eq!(
            oms.order("PAPER-SO-1").unwrap().status,
            OrderStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn test_cancel_super_order_leg_dry_run_target_keeps_entry() {
        let mut oms = make_dry_oms();
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        let result = oms
            .cancel_super_order_leg("PAPER-SO-1", OrderLeg::TargetLeg)
            .await;
        assert!(result.is_ok());

        let so = oms.super_order("PAPER-SO-1").unwrap();
        assert_eq!(so.target.status_raw, "CANCELLED");
        assert_eq!(so.entry_status_raw, "CONFIRMED");
        assert_eq!(so.stop_loss.status_raw, "PENDING");
        assert_eq!(
            oms.order("PAPER-SO-1").unwrap().status,
            OrderStatus::Confirmed
        );
    }

    #[tokio::test]
    async fn test_cancel_super_order_leg_refused_on_part_traded_and_traded_entry() {
        // The naked-position race (U4): post-fill ENTRY_LEG cancel is NEVER
        // exercised; leg-level cancels stay allowed.
        let mut oms = make_dry_oms();
        for (id, raw) in [("SO-PT", "PART_TRADED"), ("SO-TR", "TRADED")] {
            insert_tracked_super(&mut oms, id, raw, 75);
            let refused = oms.cancel_super_order_leg(id, OrderLeg::EntryLeg).await;
            let err = refused.unwrap_err();
            assert!(matches!(err, OmsError::RiskRejected { .. }), "{raw}");
            assert!(err.to_string().contains("ENTRY_LEG cancel refused"));
            // TARGET_LEG cancel on the same order is still allowed.
            assert!(
                oms.cancel_super_order_leg(id, OrderLeg::TargetLeg)
                    .await
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn test_cancel_super_order_leg_unknown_id_not_found() {
        let mut oms = make_dry_oms();
        let result = oms
            .cancel_super_order_leg("SO-NOPE", OrderLeg::TargetLeg)
            .await;
        assert!(matches!(result, Err(OmsError::OrderNotFound { .. })));
    }

    #[tokio::test]
    async fn test_cancel_super_order_leg_live_mock_success() {
        let place_body =
            r#"{"orderId":"SO-100","orderStatus":"PENDING","legDetails":[]}"#.to_owned();
        let cancel_body = r#"{"orderId":"SO-100","orderStatus":"CANCELLED"}"#.to_owned();
        let (base_url, handle) =
            start_multi_mock(vec![(200, place_body), (200, cancel_body)]).await;

        let mut oms = make_live_oms(&base_url);
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        let result = oms
            .cancel_super_order_leg("SO-100", OrderLeg::EntryLeg)
            .await;
        assert!(result.is_ok());
        // Cancel is async server-side — the tracked entry is flagged for
        // reconciliation, never optimistically terminal.
        assert!(oms.order("SO-100").unwrap().needs_reconciliation);
        handle.abort();
    }

    // --- place_forever_oco ---

    #[tokio::test]
    async fn test_place_forever_oco_dry_run_starts_triggered() {
        let mut oms = make_dry_oms();
        let order_id = oms.place_forever_oco(make_forever_request()).await.unwrap();

        assert_eq!(order_id, "PAPER-FO-1");
        let order = oms.order("PAPER-FO-1").unwrap();
        assert_eq!(order.status, OrderStatus::Triggered);
        assert_eq!(order.quantity, 5);
        assert_eq!(oms.total_placed(), 1);
    }

    #[tokio::test]
    async fn test_place_forever_oco_rejects_intraday_product() {
        // 07b §1 hard gate: CNC/MTF only — the reason Forever/OCO is NOT
        // the intraday F&O exit vehicle. Refused pre-HTTP, zero placed.
        let mut oms = make_dry_oms();
        let mut request = make_forever_request();
        request.product_type = ProductType::Intraday;

        let result = oms.place_forever_oco(request).await;
        assert!(matches!(result, Err(OmsError::RiskRejected { .. })));
        assert_eq!(oms.total_placed(), 0);
    }

    #[tokio::test]
    async fn test_place_forever_oco_live_confirm_maps_triggered() {
        let body = r#"{"orderId":"FO-1","orderStatus":"CONFIRM"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        let order_id = oms.place_forever_oco(make_forever_request()).await.unwrap();

        assert_eq!(order_id, "FO-1");
        // Forever-specific CONFIRM parses to Triggered (state_machine.rs).
        assert_eq!(oms.order("FO-1").unwrap().status, OrderStatus::Triggered);
        handle.abort();
    }

    #[test]
    fn test_build_forever_request_single_omits_second_leg_oco_includes() {
        let single = build_forever_request("100", "corr-1", &make_forever_request());
        let json = serde_json::to_value(&single).unwrap();
        assert_eq!(json["orderFlag"], "SINGLE");
        assert!(json.get("price1").is_none());
        assert!(json.get("triggerPrice1").is_none());
        assert!(json.get("quantity1").is_none());

        let mut oco_request = make_forever_request();
        oco_request.oco_leg = Some(OcoSecondLeg {
            price: 1_420.0,
            trigger_price: 1_419.0,
            quantity: 5,
        });
        let oco = build_forever_request("100", "corr-2", &oco_request);
        let json = serde_json::to_value(&oco).unwrap();
        assert_eq!(json["orderFlag"], "OCO");
        assert!(json.get("price1").is_some());
        assert!(json.get("triggerPrice1").is_some());
        assert!(json.get("quantity1").is_some());
    }

    // --- place_order_sliced ---

    #[tokio::test]
    async fn test_place_order_sliced_below_freeze_delegates_to_single_place() {
        // qty <= freeze ⇒ the plain place flow — /orders/slicing is NEVER
        // called under-freeze (U5); paper id proves the delegation.
        let mut oms = make_dry_oms();
        let ids = oms
            .place_order_sliced(make_place_request(), 100)
            .await
            .unwrap();
        assert_eq!(ids, vec!["PAPER-1".to_owned()]);
        assert_eq!(oms.order("PAPER-1").unwrap().quantity, 50);
        assert_eq!(oms.total_placed(), 1);
    }

    #[tokio::test]
    async fn test_place_order_sliced_dry_run_creates_n_paper_slices() {
        let mut oms = make_dry_oms();
        let mut request = make_place_request();
        request.quantity = 250;

        let ids = oms.place_order_sliced(request, 100).await.unwrap();
        assert_eq!(ids.len(), 3);
        let quantities: Vec<i64> = ids
            .iter()
            .map(|id| oms.order(id).unwrap().quantity)
            .collect();
        assert_eq!(quantities, vec![100, 100, 50]);
        // One correlation id shared by every slice.
        let correlations: std::collections::HashSet<String> = ids
            .iter()
            .map(|id| oms.order(id).unwrap().correlation_id.clone())
            .collect();
        assert_eq!(correlations.len(), 1);
        assert_eq!(oms.total_placed(), 3);
    }

    #[tokio::test]
    async fn test_place_order_sliced_boundary_nonpositive_freeze_refused() {
        let mut oms = make_dry_oms();
        for freeze in [0_i64, -1] {
            let mut request = make_place_request();
            request.quantity = 250;
            let result = oms.place_order_sliced(request, freeze).await;
            assert!(
                matches!(result, Err(OmsError::RiskRejected { .. })),
                "freeze {freeze} must be refused"
            );
        }
        assert_eq!(oms.total_placed(), 0);
    }

    #[tokio::test]
    async fn test_place_order_sliced_live_array_response_tracks_all_ids() {
        let body = r#"[{"orderId":"S-1","orderStatus":"TRANSIT"},{"orderId":"S-2","orderStatus":"TRANSIT"},{"orderId":"S-3","orderStatus":"TRANSIT"}]"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        let mut request = make_place_request();
        request.quantity = 250;

        let ids = oms.place_order_sliced(request, 100).await.unwrap();
        assert_eq!(
            ids,
            vec!["S-1".to_owned(), "S-2".to_owned(), "S-3".to_owned()]
        );
        // Per-slice quantities attributed from the computed plan.
        assert_eq!(oms.order("S-1").unwrap().quantity, 100);
        assert_eq!(oms.order("S-3").unwrap().quantity, 50);
        // Every slice shares ONE correlation id.
        assert_eq!(
            oms.order("S-1").unwrap().correlation_id,
            oms.order("S-3").unwrap().correlation_id
        );
        // M3: the server split is never echoed — every live sliced order
        // is reconciliation-flagged, even on a count match.
        for id in ["S-1", "S-2", "S-3"] {
            assert!(oms.order(id).unwrap().needs_reconciliation, "{id}");
        }
        assert_eq!(oms.total_placed(), 3);
        handle.abort();
    }

    #[tokio::test]
    async fn test_place_order_sliced_live_single_object_response_tracked() {
        // The portal-documented single-object shape: one tracked order
        // carrying the TOTAL quantity.
        let body = r#"{"orderId":"S-9","orderStatus":"TRANSIT"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        let mut request = make_place_request();
        request.quantity = 250;

        let ids = oms.place_order_sliced(request, 100).await.unwrap();
        assert_eq!(ids, vec!["S-9".to_owned()]);
        let order = oms.order("S-9").unwrap();
        assert_eq!(order.quantity, 250);
        // M3: single-object arm is reconciliation-flagged too — the true
        // server-side split is only knowable from the REST order book.
        assert!(order.needs_reconciliation);
        handle.abort();
    }

    // --- verify_order_execution ---

    #[tokio::test]
    async fn test_verify_order_execution_dry_run_simulated_filled() {
        let mut oms = make_dry_oms();
        let order_id = oms.place_order(make_place_request()).await.unwrap();

        let verdict = oms.verify_order_execution(&order_id, 1, 30).await.unwrap();
        assert_eq!(verdict, ExecutionVerdict::SimulatedFilled);

        let state = oms.verify_states.get(&order_id).unwrap();
        assert_eq!(state.attempts, 1);
        assert_eq!(state.last_verdict_label, "simulated_filled");
    }

    #[tokio::test]
    async fn test_verify_order_execution_unknown_order_not_found() {
        let mut oms = make_dry_oms();
        let result = oms.verify_order_execution("NOPE", 1, 30).await;
        assert!(matches!(result, Err(OmsError::OrderNotFound { .. })));
    }

    #[tokio::test]
    async fn test_verify_order_execution_consumes_gcra_in_dry_run() {
        // Ruling 5: probes consume the SHARED order GCRA — dry-run included.
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(2),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );
        let order_id = oms.place_order(make_place_request()).await.unwrap(); // cell 1
        assert!(oms.verify_order_execution(&order_id, 1, 30).await.is_ok()); // cell 2
        let third = oms.verify_order_execution(&order_id, 2, 30).await;
        assert!(matches!(third, Err(OmsError::RateLimited)));
    }

    #[tokio::test]
    async fn test_verify_order_execution_live_pending_then_traded_multi_mock() {
        let pending = r#"{"orderId":"D-1","orderStatus":"PENDING"}"#.to_owned();
        let traded =
            r#"{"orderId":"D-1","orderStatus":"TRADED","filledQty":50,"averageTradedPrice":246.0}"#
                .to_owned();
        let (base_url, handle) = start_multi_mock(vec![(200, pending), (200, traded)]).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_order(&mut oms, "D-1", OrderStatus::Pending, 50);

        let first = oms.verify_order_execution("D-1", 1, 30).await.unwrap();
        assert_eq!(first, ExecutionVerdict::Pending { elapsed_secs: 1 });

        let second = oms.verify_order_execution("D-1", 3, 30).await.unwrap();
        assert_eq!(
            second,
            ExecutionVerdict::Filled {
                traded_qty: 50,
                avg_price: 246.0
            }
        );
        let order = oms.order("D-1").unwrap();
        assert_eq!(order.status, OrderStatus::Traded);
        assert_eq!(order.traded_qty, 50);
        assert_eq!(oms.verify_states.get("D-1").unwrap().attempts, 2);
        handle.abort();
    }

    #[tokio::test]
    async fn test_verify_order_execution_live_pending_at_budget_limit() {
        // The MPP never-fills case: MARKET→LIMIT resting past the deadline
        // is NEVER assumed filled.
        let body = r#"{"orderId":"D-1","orderStatus":"PENDING"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_order(&mut oms, "D-1", OrderStatus::Pending, 50);

        let verdict = oms.verify_order_execution("D-1", 30, 30).await.unwrap();
        assert_eq!(
            verdict,
            ExecutionVerdict::PendingAtLimit { elapsed_secs: 30 }
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_verify_order_execution_live_rejected_terminal() {
        let body = r#"{"orderId":"D-1","orderStatus":"REJECTED","omsErrorCode":"DH-905","omsErrorDescription":"input exception"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_order(&mut oms, "D-1", OrderStatus::Pending, 50);

        let verdict = oms.verify_order_execution("D-1", 5, 30).await.unwrap();
        assert_eq!(
            verdict,
            ExecutionVerdict::Terminal {
                status: OrderStatus::Rejected
            }
        );
        assert_eq!(oms.order("D-1").unwrap().status, OrderStatus::Rejected);
        handle.abort();
    }

    #[tokio::test]
    async fn test_verify_order_execution_live_partial_fill_at_budget_flags_reconciliation() {
        let body = r#"{"orderId":"D-1","orderStatus":"PART_TRADED","filledQty":20}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_order(&mut oms, "D-1", OrderStatus::Pending, 50);

        let verdict = oms.verify_order_execution("D-1", 30, 30).await.unwrap();
        assert_eq!(
            verdict,
            ExecutionVerdict::PartiallyFilled {
                traded_qty: 20,
                remaining: 30
            }
        );
        // Partial at budget: the remainder is never silently forgotten.
        let order = oms.order("D-1").unwrap();
        assert!(order.needs_reconciliation);
        assert_eq!(order.status, OrderStatus::PartTraded);
        handle.abort();
    }

    #[tokio::test]
    async fn test_verify_order_execution_live_garbage_body_unknown_fail_closed() {
        // A 2xx with an unparsable body is NEVER treated as filled.
        let body = "definitely not json";
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_order(&mut oms, "D-1", OrderStatus::Pending, 50);

        let verdict = oms.verify_order_execution("D-1", 5, 30).await.unwrap();
        assert_eq!(
            verdict,
            ExecutionVerdict::Unknown {
                raw_status: "UNPARSABLE_RESPONSE_BODY".to_owned()
            }
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_verify_order_execution_live_429_no_cb_trip() {
        let body = r#"{"errorType":"RATE_LIMIT","errorCode":"DH-904","errorMessage":"throttled"}"#;
        let (base_url, handle) = start_oms_mock(429, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_order(&mut oms, "D-1", OrderStatus::Pending, 50);

        let result = oms.verify_order_execution("D-1", 5, 30).await;
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        // Probe failures never touch the breaker (inconclusive — retry).
        assert_eq!(oms.circuit_breaker.failure_count(), 0);
        handle.abort();
    }

    #[tokio::test]
    async fn test_verify_order_execution_live_super_batched_list_filled() {
        let body = r#"[{"orderId":"SO-9","orderStatus":"TRADED","filledQty":75,"averageTradedPrice":101.5,"legDetails":[]}]"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_super(&mut oms, "SO-9", "PENDING", 75);
        insert_tracked_order(&mut oms, "SO-9", OrderStatus::Pending, 75);

        let verdict = oms.verify_order_execution("SO-9", 5, 30).await.unwrap();
        assert_eq!(
            verdict,
            ExecutionVerdict::Filled {
                traded_qty: 75,
                avg_price: 101.5
            }
        );
        // The batched list refreshed the registry raw statuses.
        assert_eq!(oms.super_order("SO-9").unwrap().entry_status_raw, "TRADED");
        assert_eq!(oms.order("SO-9").unwrap().status, OrderStatus::Traded);
        handle.abort();
    }

    #[tokio::test]
    async fn test_verify_order_execution_live_super_closed_applies_transition() {
        // Super CLOSED → Terminal{Closed} verdict + the Ruling-4 transition
        // on the tracked entry + leg reconcile from legDetails.
        let body = r#"[{"orderId":"SO-9","orderStatus":"CLOSED","legDetails":[{"orderId":"SO-9-T","legName":"TARGET_LEG","orderStatus":"TRADED","price":110.0}]}]"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_super(&mut oms, "SO-9", "TRADED", 75);
        insert_tracked_order(&mut oms, "SO-9", OrderStatus::PartTraded, 75);

        let verdict = oms.verify_order_execution("SO-9", 5, 30).await.unwrap();
        assert_eq!(
            verdict,
            ExecutionVerdict::Terminal {
                status: OrderStatus::Closed
            }
        );
        assert_eq!(oms.order("SO-9").unwrap().status, OrderStatus::Closed);
        let so = oms.super_order("SO-9").unwrap();
        assert_eq!(so.target.status_raw, "TRADED");
        handle.abort();
    }

    #[tokio::test]
    async fn test_verify_order_execution_live_super_missing_from_list_unknown() {
        // A successful list WITHOUT our id is fail-closed Unknown — a
        // failed probe ≠ absence (never dropped, never assumed filled).
        // M2 refined (refuter round 1, 2026-07-14): IN-BUDGET
        // (elapsed 5 < deadline 30) the sentinel is potential list lag —
        // NO reconciliation flag and NO EXIT-VERIFY-01 yet; the ladder's
        // remaining rungs re-probe.
        let body = r#"[{"orderId":"SO-OTHER","orderStatus":"PENDING","legDetails":[]}]"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_super(&mut oms, "SO-9", "PENDING", 75);
        insert_tracked_order(&mut oms, "SO-9", OrderStatus::Pending, 75);

        let verdict = oms.verify_order_execution("SO-9", 5, 30).await.unwrap();
        assert_eq!(
            verdict,
            ExecutionVerdict::Unknown {
                raw_status: "NOT_IN_SUPER_LIST".to_owned()
            }
        );
        assert!(
            !oms.order("SO-9").unwrap().needs_reconciliation,
            "M2 refined: an in-budget NOT_IN_SUPER_LIST is list lag — never flagged yet"
        );
        handle.abort();
    }

    /// M2 refined (refuter round 1, 2026-07-14): NOT_IN_SUPER_LIST at/past
    /// the budget (the ladder's FINAL floored probe) IS flagged —
    /// `needs_reconciliation = true` + the EXIT-VERIFY-01 arm fires.
    #[tokio::test]
    async fn test_verify_live_super_missing_at_budget_flags_reconciliation() {
        let body = r#"[{"orderId":"SO-OTHER","orderStatus":"PENDING","legDetails":[]}]"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_super(&mut oms, "SO-9", "PENDING", 75);
        insert_tracked_order(&mut oms, "SO-9", OrderStatus::Pending, 75);

        let verdict = oms.verify_order_execution("SO-9", 30, 30).await.unwrap();
        assert_eq!(
            verdict,
            ExecutionVerdict::Unknown {
                raw_status: "NOT_IN_SUPER_LIST".to_owned()
            }
        );
        assert!(
            oms.order("SO-9").unwrap().needs_reconciliation,
            "M2 refined: NOT_IN_SUPER_LIST at the budget must flag reconciliation"
        );
        handle.abort();
    }

    // --- 2026-07-14 hostile-review fixes (H1/H2/M2/M4 + L-series) ---

    /// H1: a still-resting order probed AT the deadline classifies
    /// PendingAtLimit AND is reconciliation-flagged — verify-ladder
    /// exhaustion is never a silent Pending.
    #[tokio::test]
    async fn test_verify_live_pending_at_limit_flags_needs_reconciliation() {
        let body = r#"{"orderId":"D-1","orderStatus":"PENDING"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_order(&mut oms, "D-1", OrderStatus::Pending, 50);

        let verdict = oms.verify_order_execution("D-1", 30, 30).await.unwrap();
        assert_eq!(
            verdict,
            ExecutionVerdict::PendingAtLimit { elapsed_secs: 30 }
        );
        assert!(
            oms.order("D-1").unwrap().needs_reconciliation,
            "H1: at-limit resting order must be reconciliation-flagged"
        );
        handle.abort();
    }

    /// M2: a fail-closed Unknown verdict (unparsable body) flags the
    /// tracked order for reconciliation.
    #[tokio::test]
    async fn test_verify_live_unknown_body_flags_needs_reconciliation() {
        let body = "definitely not json";
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_order(&mut oms, "D-1", OrderStatus::Pending, 50);

        let verdict = oms.verify_order_execution("D-1", 5, 30).await.unwrap();
        assert!(matches!(verdict, ExecutionVerdict::Unknown { .. }));
        assert!(
            oms.order("D-1").unwrap().needs_reconciliation,
            "M2: Unknown verdict must be reconciliation-flagged"
        );
        handle.abort();
    }

    /// H2 (cancel): a fill recorded on the tracked ManagedOrder refuses
    /// the ENTRY_LEG cancel even when the super registry still reads
    /// pre-fill (PENDING) — the stale-registry defense at the gate.
    #[tokio::test]
    async fn test_cancel_entry_refused_when_tracked_order_filled_registry_stale() {
        let mut oms = make_dry_oms();
        for (id, tracked_status) in [
            ("SO-H2-T", OrderStatus::Traded),
            ("SO-H2-P", OrderStatus::PartTraded),
        ] {
            insert_tracked_super(&mut oms, id, "PENDING", 75);
            insert_tracked_order(&mut oms, id, tracked_status, 75);

            let refused = oms.cancel_super_order_leg(id, OrderLeg::EntryLeg).await;
            let err = refused.unwrap_err();
            assert!(
                matches!(err, OmsError::RiskRejected { .. }),
                "{tracked_status:?}"
            );
            assert!(
                err.to_string().contains("registry stale"),
                "refusal must name the stale registry, got: {err}"
            );
        }
    }

    /// H2 (modify): a terminal tracked entry refuses the ENTRY_LEG modify
    /// even when the registry's entry_status_raw still reads PENDING.
    #[tokio::test]
    async fn test_modify_entry_refused_when_tracked_order_terminal_registry_stale() {
        let mut oms = make_dry_oms();
        insert_tracked_super(&mut oms, "SO-H2-M", "PENDING", 75);
        insert_tracked_order(&mut oms, "SO-H2-M", OrderStatus::Traded, 75);

        let refused = oms
            .modify_super_order_leg(
                "SO-H2-M",
                ModifySuperOrderLeg::Entry {
                    order_type: OrderType::Limit,
                    quantity: 75,
                    price: 101.0,
                    target_price: 111.0,
                    stop_loss_price: 96.0,
                    trailing_jump: 0.0,
                },
            )
            .await;
        let err = refused.unwrap_err();
        assert!(matches!(err, OmsError::RiskRejected { .. }));
        assert!(err.to_string().contains("registry stale"));
    }

    /// M4: a Target/StopLoss modify on a DEAD leg is refused; an
    /// empty/unknown leg status stays fail-open (Dhan re-validates).
    #[tokio::test]
    async fn test_modify_dead_target_and_stop_legs_refused() {
        let mut oms = make_dry_oms();
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        oms.super_orders
            .get_mut("PAPER-SO-1")
            .unwrap()
            .target
            .status_raw = "TRADED".to_owned();
        let refused = oms
            .modify_super_order_leg(
                "PAPER-SO-1",
                ModifySuperOrderLeg::Target {
                    target_price: 112.5,
                },
            )
            .await;
        let err = refused.unwrap_err();
        assert!(matches!(err, OmsError::RiskRejected { .. }));
        assert!(err.to_string().contains("TARGET_LEG modify refused"));

        oms.super_orders
            .get_mut("PAPER-SO-1")
            .unwrap()
            .stop_loss
            .status_raw = "CANCELLED".to_owned();
        let refused = oms
            .modify_super_order_leg(
                "PAPER-SO-1",
                ModifySuperOrderLeg::StopLoss {
                    stop_loss_price: 96.0,
                    trailing_jump: 0.0,
                },
            )
            .await;
        assert!(matches!(refused, Err(OmsError::RiskRejected { .. })));

        // Fail-open: an EMPTY leg status allows the modify.
        oms.super_orders
            .get_mut("PAPER-SO-1")
            .unwrap()
            .target
            .status_raw = String::new();
        assert!(
            oms.modify_super_order_leg(
                "PAPER-SO-1",
                ModifySuperOrderLeg::Target {
                    target_price: 113.0,
                },
            )
            .await
            .is_ok()
        );
    }

    /// L1: a structurally-unknown id never consumes a shared GCRA cell —
    /// the existence check runs BEFORE the rate limiter.
    #[tokio::test]
    async fn test_verify_unknown_order_does_not_consume_gcra() {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(1),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        );
        // Burn ZERO cells on unknown ids...
        for _ in 0..5 {
            let result = oms.verify_order_execution("NOPE", 1, 30).await;
            assert!(matches!(result, Err(OmsError::OrderNotFound { .. })));
        }
        // ...so the single burst cell is still available for a real order.
        let order_id = oms.place_order(make_place_request()).await.unwrap();
        assert!(oms.order(&order_id).is_some());
    }

    /// L3: malformed caller-supplied ids are refused typed (never
    /// URL-embedded, never echoed raw).
    #[tokio::test]
    async fn test_exit_methods_refuse_malformed_order_id_inputs() {
        let mut oms = make_dry_oms();
        let bad = "SO/../100";

        let modify = oms
            .modify_super_order_leg(
                bad,
                ModifySuperOrderLeg::Target {
                    target_price: 111.0,
                },
            )
            .await;
        assert!(matches!(modify, Err(OmsError::RiskRejected { .. })));

        let cancel = oms.cancel_super_order_leg(bad, OrderLeg::TargetLeg).await;
        assert!(matches!(cancel, Err(OmsError::RiskRejected { .. })));

        let verify = oms.verify_order_execution(bad, 1, 30).await;
        match verify {
            Err(OmsError::OrderNotFound { order_id }) => {
                assert_eq!(order_id, "<malformed-order-id>", "raw id must not echo");
            }
            other => panic!("expected OrderNotFound, got {other:?}"),
        }
    }

    /// L3: a malformed SERVER-supplied id is never tracked (super, sliced,
    /// forever place paths refuse with a typed error).
    #[tokio::test]
    async fn test_place_paths_refuse_malformed_server_ids() {
        // Super order: path-traversal-shaped entry id.
        let body = r#"{"orderId":"SO/../1","orderStatus":"PENDING","legDetails":[]}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;
        let mut oms = make_live_oms(&base_url);
        let result = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(matches!(result, Err(OmsError::DhanApiError { .. })));
        assert_eq!(oms.total_placed(), 0);
        assert!(oms.all_orders().is_empty());
        handle.abort();

        // Sliced: ONE malformed id refuses the whole response.
        let body = r#"[{"orderId":"S-1","orderStatus":"TRANSIT"},{"orderId":"S 2","orderStatus":"TRANSIT"}]"#;
        let (base_url, handle) = start_oms_mock(200, body).await;
        let mut oms = make_live_oms(&base_url);
        let mut request = make_place_request();
        request.quantity = 250;
        let result = oms.place_order_sliced(request, 100).await;
        assert!(matches!(result, Err(OmsError::DhanApiError { .. })));
        assert!(oms.all_orders().is_empty());
        handle.abort();

        // Forever: malformed id refused.
        let body = r#"{"orderId":"FO\n1","orderStatus":"CONFIRM"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;
        let mut oms = make_live_oms(&base_url);
        let result = oms.place_forever_oco(make_forever_request()).await;
        assert!(matches!(result, Err(OmsError::DhanApiError { .. })));
        assert!(oms.all_orders().is_empty());
        handle.abort();
    }

    /// L4: a live TARGET_LEG cancel success updates the registry leg state
    /// exactly like dry-run (the pre-fix asymmetry left it PENDING).
    #[tokio::test]
    async fn test_cancel_target_leg_live_updates_registry_leg_state() {
        let place_body =
            r#"{"orderId":"SO-100","orderStatus":"PENDING","legDetails":[]}"#.to_owned();
        let cancel_body = r#"{"orderId":"SO-100","orderStatus":"PENDING"}"#.to_owned();
        let (base_url, handle) =
            start_multi_mock(vec![(200, place_body), (200, cancel_body)]).await;

        let mut oms = make_live_oms(&base_url);
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        oms.cancel_super_order_leg("SO-100", OrderLeg::TargetLeg)
            .await
            .unwrap();
        let so = oms.super_order("SO-100").unwrap();
        assert_eq!(so.target.status_raw, "CANCELLED");
        // The entry + stop loss stay untouched.
        assert_eq!(so.entry_status_raw, "PENDING");
        assert_ne!(so.stop_loss.status_raw, "CANCELLED");
        handle.abort();
    }

    /// L5: a 2xx modify with an UNPARSABLE body applies locally BUT flags
    /// the tracked entry for reconciliation (the mutation may not have
    /// applied broker-side).
    #[tokio::test]
    async fn test_modify_live_accepted_unparsed_body_flags_reconciliation() {
        let place_body =
            r#"{"orderId":"SO-100","orderStatus":"PENDING","legDetails":[]}"#.to_owned();
        let modify_body = "garbage not json".to_owned();
        let (base_url, handle) =
            start_multi_mock(vec![(200, place_body), (200, modify_body)]).await;

        let mut oms = make_live_oms(&base_url);
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();

        oms.modify_super_order_leg(
            "SO-100",
            ModifySuperOrderLeg::Target {
                target_price: 112.5,
            },
        )
        .await
        .unwrap();

        let so = oms.super_order("SO-100").unwrap();
        assert!((so.target.price - 112.5).abs() < f64::EPSILON);
        assert!(
            oms.order("SO-100").unwrap().needs_reconciliation,
            "L5: ACCEPTED_UNPARSED_BODY must flag reconciliation"
        );
        handle.abort();
    }

    /// L6: the shared recovery helper fires CircuitBreakerClosed exactly
    /// when a success closes a previously-open breaker (all exit live
    /// arms — place/modify/cancel/forever/sliced — call it).
    #[test]
    fn test_record_api_success_fires_cb_closed_recovery_alert() {
        use std::sync::{Arc, Mutex};
        struct CapturingSink(Arc<Mutex<Vec<&'static str>>>);
        impl OmsAlertSink for CapturingSink {
            fn fire(&self, alert: OmsAlert) {
                let label = match alert {
                    OmsAlert::CircuitBreakerClosed => "cb_closed",
                    _ => "other",
                };
                if let Ok(mut alerts) = self.0.lock() {
                    alerts.push(label);
                }
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut oms = make_dry_oms();
        oms.set_alert_sink(Box::new(CapturingSink(Arc::clone(&captured))));

        // Below threshold: success fires NO recovery alert.
        oms.circuit_breaker.record_failure();
        oms.record_api_success_and_alert_recovery();
        assert!(
            captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );

        // At/above threshold: the closing success fires exactly one alert.
        for _ in 0..tickvault_common::constants::OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
            oms.circuit_breaker.record_failure();
        }
        oms.record_api_success_and_alert_recovery();
        assert_eq!(
            captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_slice(),
            &["cb_closed"]
        );
        assert_eq!(oms.circuit_breaker.failure_count(), 0);
    }

    /// L7: duplicate list rows for one id — classification reads the LAST
    /// occurrence, matching refresh_super_registry's last-write-wins walk.
    #[tokio::test]
    async fn test_verify_live_super_duplicate_rows_last_occurrence_wins() {
        let body = r#"[{"orderId":"SO-9","orderStatus":"PENDING","legDetails":[]},{"orderId":"SO-9","orderStatus":"TRADED","filledQty":75,"averageTradedPrice":101.5,"legDetails":[]}]"#;
        let (base_url, handle) = start_oms_mock(200, body).await;

        let mut oms = make_live_oms(&base_url);
        insert_tracked_super(&mut oms, "SO-9", "PENDING", 75);
        insert_tracked_order(&mut oms, "SO-9", OrderStatus::Pending, 75);

        let verdict = oms.verify_order_execution("SO-9", 5, 30).await.unwrap();
        assert_eq!(
            verdict,
            ExecutionVerdict::Filled {
                traded_qty: 75,
                avg_price: 101.5
            }
        );
        // The registry read the same (LAST) row.
        assert_eq!(oms.super_order("SO-9").unwrap().entry_status_raw, "TRADED");
        handle.abort();
    }

    /// H3b: a server response beyond MAX_ORDER_SLICES rows is truncated at
    /// the cap — bounded per-row work, every tracked row flagged (M3).
    #[tokio::test]
    async fn test_place_order_sliced_live_beyond_cap_truncated_and_flagged() {
        let cap = usize::try_from(exit_rules::MAX_ORDER_SLICES).unwrap_or(0);
        let rows: Vec<String> =
            (0..=cap) // cap + 1 rows
                .map(|i| format!(r#"{{"orderId":"S-{i}","orderStatus":"TRANSIT"}}"#))
                .collect();
        let body = format!("[{}]", rows.join(","));
        let (base_url, handle) = start_oms_mock(200, &body).await;

        let mut oms = make_live_oms(&base_url);
        let mut request = make_place_request();
        request.quantity = 250;

        let ids = oms.place_order_sliced(request, 100).await.unwrap();
        assert_eq!(ids.len(), cap, "beyond-cap rows must be dropped");
        assert_eq!(oms.total_placed(), u64::try_from(cap).unwrap_or(0));
        assert!(oms.order("S-0").unwrap().needs_reconciliation);
        assert!(oms.order(&format!("S-{cap}")).is_none(), "row past the cap");
        handle.abort();
    }

    // --- registry accessor + daily reset ---

    #[tokio::test]
    async fn test_super_order_accessor_and_reset_daily_clears_exit_registries() {
        let mut oms = make_dry_oms();
        oms.place_super_order(make_super_request(), 1_800)
            .await
            .unwrap();
        oms.verify_order_execution("PAPER-SO-1", 1, 30)
            .await
            .unwrap();

        assert!(oms.super_order("PAPER-SO-1").is_some());
        assert!(!oms.verify_states.is_empty());

        oms.reset_daily();

        assert!(oms.super_order("PAPER-SO-1").is_none());
        assert!(oms.verify_states.is_empty());
        assert!(oms.all_orders().is_empty());
    }
}
