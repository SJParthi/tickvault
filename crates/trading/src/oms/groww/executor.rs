//! Groww order EXECUTOR — the paper/dry-run mutation protocol wiring the pure
//! core (ledger + reference-id + state FSM + reconcile + poll tiers) to the
//! transport + rate budget (design §4.5/§4.7; ORD-PR-3).
//!
//! # The write-ahead mutation protocol (paper AND live)
//! Every mutation: generate a reference id → **fsync a `recorded` intent to
//! the ledger BEFORE any send** (a mutating send without an [`IntentReceipt`]
//! is a compile error) → send via the injected [`OrderTransport`] → classify →
//! update ledger phase + in-memory FSM → drive the ambiguity resolution ladder
//! on an ambiguous outcome.
//!
//! # ORD-PR-3 carry-notes enforced here
//! - **(a) modify/cancel on an unresolved place is REFUSED** — keyed on the
//!   INTENT (the P50 gap): an order whose place has not adopted a
//!   groww_order_id (ambiguous / unresolved) cannot be modified/cancelled.
//! - **(b) fsynced ledger calls run under `spawn_blocking`** — every
//!   `record_intent`/`append_phase` goes through [`GrowwOrderExecutor::with_ledger`]
//!   (a `spawn_blocking` closure), so a blocking fsync never stalls the async
//!   runtime. NO `block_in_place` (it would panic the current-thread test
//!   runtime); the ratchet + a current-thread behavioural test pin this.
//! - **(c) reference-id salt from a CSPRNG** — [`reference_id_salt`] draws from
//!   `uuid::Uuid::new_v4()` (a getrandom OS-CSPRNG); NO predictable source
//!   (clock/counter). Design §4.4 names `OsRng`; the workspace has no `rand`
//!   dep, and adding one needs operator approval — uuid-v4 is the equivalent
//!   OS-CSPRNG already present. The ratchet pins the CSPRNG source.
//!
//! # Live-fire gate (belt + braces over Gate 2/3)
//! [`live_send_permitted`] = `GROWW_ORDER_LIVE_FIRE && live_fire_requested` —
//! `false` today. A REAL live executor ([`GrowwOrderExecutor::new_live`])
//! refuses construction unless it passes; `ExecutionMode::Live` is otherwise
//! constructable only under `#[cfg(test)]` (the engine.rs mode-mutator
//! precedent). The paper lane uses [`NullTransport`] — ZERO HTTP.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use secrecy::SecretString;
use tickvault_common::constants::GROWW_ORDER_LIVE_FIRE;

use super::api_client::{NullTransport, OrderTransport, TransportOutcome};
use super::intent_ledger::{IntentKind, IntentLedger, IntentPhase, NewIntent};
use super::rate_budget::{GrowwRateBudget, RateFamily};
use super::reconcile::{EodAction, classify_eod_action};
use super::reference_id::{IstDate, generate_reference_id};
use super::state::{
    ObservationSource, OrderObservation, TrackedOrderState, TransitionOutcome, evaluate_transition,
    is_terminal,
};
use super::types::{
    GrowwCancelOrderReq, GrowwCreateOrderReq, GrowwModifyOrderReq, GrowwOmsError, GrowwOrderStatus,
    GrowwSegment, validate_create_order, validate_modify_order,
};

// ---------------------------------------------------------------------------
// Config + ladder constants (module-local; move to GROWW_ORDER_* in ORD-PR-1)
// ---------------------------------------------------------------------------

/// Ambiguity ladder budget seconds (auth-paused clock — design §4.7;
/// single-sourced from tickvault-common).
pub(crate) const AMBIGUITY_LADDER_MAX_SECS: u64 =
    tickvault_common::constants::GROWW_ORDER_AMBIGUITY_LADDER_MAX_SECS;
/// The fixed ladder cadence steps (single-sourced from tickvault-common).
const LADDER_STEPS_SECS: [u64; 5] =
    tickvault_common::constants::GROWW_ORDER_AMBIGUITY_LADDER_STEPS_SECS;
/// The 60s pace after the fixed steps. KEPT LOCAL: distinct concept from the
/// common `GROWW_ORDER_AMBIGUITY_LADDER_MAX_SECS` (600) — this is the post-step
/// pace, not the budget.
const LADDER_PACE_SECS: u64 = 60;
/// Recheck cadence while auth is stale — does NOT consume the ladder budget.
const AUTH_RECHECK_SECS: u64 = 30;
/// Bounded auth rechecks before demanding the operator (avoids an unbounded
/// auth-stale spin; the token-recovery re-arm is a live-flip concern).
const MAX_AUTH_RECHECKS: u32 = 20;
/// Bounded replays of a place (SAME reference id) — design §4.7
/// (single-sourced from tickvault-common).
const MAX_PLACE_REPLAYS: u32 = tickvault_common::constants::GROWW_ORDER_AMBIGUITY_REPLAY_MAX;
/// The `mode` tag written on ledger records.
const MODE_PAPER: &str = "paper";
const MODE_LIVE: &str = "live";

/// Executor config (module-local; the operator-tunable subset lands as config
/// keys in ORD-PR-1 — see the design §4.13 table).
#[derive(Debug, Clone, Copy)]
pub struct ExecutorConfig {
    /// `max_order_quantity` gate (0 = refuse ALL — the fail-closed default).
    pub max_order_quantity: i64,
    /// The ladder budget (auth-paused).
    pub ambiguity_ladder_max_secs: u64,
    /// Whether an auto-replay of a not-landed place is permitted.
    pub replay_policy_auto: bool,
    /// Operator declaration of live intent (inert without `GROWW_ORDER_LIVE_FIRE`).
    pub live_fire_requested: bool,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            max_order_quantity: 0, // fail-closed (§4.12/§4.13)
            ambiguity_ladder_max_secs: AMBIGUITY_LADDER_MAX_SECS,
            replay_policy_auto: true,
            live_fire_requested: false,
        }
    }
}

/// The live-fire permission gate — `GROWW_ORDER_LIVE_FIRE` (Gate 3, `false`
/// today) AND the operator's `live_fire_requested`. `false` unless BOTH align.
#[must_use]
pub const fn live_send_permitted(live_fire_requested: bool) -> bool {
    GROWW_ORDER_LIVE_FIRE && live_fire_requested
}

/// Paper vs live execution mode. Construction of `Live` outside `#[cfg(test)]`
/// is gated by [`live_send_permitted`] (see [`GrowwOrderExecutor::new_live`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Simulated — ZERO HTTP; outcomes drive the ledger/FSM only.
    Paper,
    /// Live — the injected transport is invoked.
    Live,
}

impl ExecutionMode {
    const fn ledger_mode(self) -> &'static str {
        match self {
            Self::Paper => MODE_PAPER,
            Self::Live => MODE_LIVE,
        }
    }
}

// ---------------------------------------------------------------------------
// Executor error (local — types.rs GrowwOmsError is a frozen core module)
// ---------------------------------------------------------------------------

/// An executor-level error (wraps the pure [`GrowwOmsError`] + the
/// transport/budget/runtime arms this layer introduces).
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// A pure-core order error (validation / ledger / serialization).
    #[error(transparent)]
    Oms(#[from] GrowwOmsError),
    /// The family's self-cap budget denied this mutation before any send.
    #[error("rate budget exceeded for {0:?}")]
    RateBudgetExceeded(RateFamily),
    /// Live sending is not permitted (Gate 2/3) — construction refused.
    #[error("live order sending is disabled (GROWW_ORDER_LIVE_FIRE / live_fire_requested)")]
    LiveDisabledAtCompileTime,
    /// The blocking ledger task failed to join (runtime shutdown / panic).
    #[error("ledger fsync task failed to join")]
    LedgerTaskFailed,
    /// A mutation was attempted outside the trading session window.
    #[error("market is closed — mutation refused")]
    MarketClosed,
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// The outcome of a place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceResult {
    /// Paper mode simulated an acceptance (synthetic order id).
    PaperAccepted {
        /// The synthetic paper order id.
        order_id: String,
    },
    /// Live: broker accepted (2xx) — order id adopted.
    Accepted {
        /// The broker order id.
        order_id: String,
    },
    /// Definitive reject (400-class + well-shaped FAILURE).
    Rejected {
        /// The GA code, if present.
        ga_code: Option<String>,
    },
    /// Ambiguous → ladder resolved LANDED (order id adopted).
    ResolvedLanded {
        /// The adopted broker order id.
        order_id: String,
    },
    /// Ambiguous → ladder resolved NOT-LANDED (provably never landed).
    ResolvedNotLanded,
    /// Ladder exhausted — operator action required (Critical class).
    // GROWW-ORD-03 emit lands with the shared variants (ORD-PR-1).
    Unresolved,
}

/// The outcome of a modify/cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationResult {
    /// The mutation was accepted / confirmed.
    Accepted,
    /// Definitive reject.
    Rejected {
        /// The GA code, if present.
        ga_code: Option<String>,
    },
    /// A cancel that lost the race to a fill — a POSITION EXISTS
    /// (hostile F-2/F-10 "position exists" page).
    // GROWW-ORD-04-class emit lands with the shared variants (ORD-PR-1).
    CancelLostRace {
        /// The filled quantity that beat the cancel.
        filled_quantity: i64,
    },
    /// Ladder exhausted — operator action required.
    // GROWW-ORD-03 emit lands with the shared variants (ORD-PR-1).
    Unresolved,
}

/// The EOD-sweep disposition (design §4.9 / F-7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EodSweepReport {
    /// Open-class orders with ZERO fill, force-expired locally.
    pub expired: Vec<String>,
    /// Open-class orders carrying a partial fill — a POSITION EXISTS.
    pub partial_fill_positions: Vec<(String, i64)>,
    /// Fill-class orders awaiting settlement — a POSITION EXISTS.
    pub filled_await_settlement: Vec<String>,
    /// `Unknown`-parked orders at EOD — Critical route (never silent expire).
    pub unknown_critical: Vec<String>,
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

/// Provenance of an order id in the executor's tracking (the P50 key: is the
/// place that produced this order settled, or still unresolved?).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaceProvenance {
    /// The place adopted this id from a 2xx/landed outcome — mutation-eligible.
    Settled,
    /// The place is ambiguous/unresolved — mutations REFUSED (P50). In this
    /// synchronous executor `place_order` awaits the full ladder before
    /// returning, so a caller never holds an id for a still-resolving place;
    /// this arm is the defensive guard the P50 test exercises directly.
    #[cfg_attr(not(test), allow(dead_code))]
    Unresolved,
}

/// The Groww order executor. Generic over the transport, so paper injects
/// [`NullTransport`] (zero HTTP) and live injects the reqwest client.
pub struct GrowwOrderExecutor<T: OrderTransport> {
    mode: ExecutionMode,
    transport: T,
    token: SecretString,
    cfg: ExecutorConfig,
    budget: GrowwRateBudget,
    /// The durable write-ahead ledger (Arc<Mutex> so fsync runs under
    /// `spawn_blocking` — carry-note (b)).
    ledger: Arc<Mutex<IntentLedger>>,
    /// Per-order tracked FSM state (keyed by broker order id).
    orders: HashMap<String, TrackedOrderState>,
    /// Provenance of each tracked order id (the P50 mutation-eligibility key).
    provenance: HashMap<String, PlaceProvenance>,
    /// Order ids whose in-flight mutation blocks further mutations.
    blocked: HashSet<String>,
    /// Monotone local counter over the ledger's replayed max sequence.
    next_seq: u32,
}

impl GrowwOrderExecutor<NullTransport> {
    /// Construct the PAPER executor — mode hardcoded to [`ExecutionMode::Paper`]
    /// with the ZERO-HTTP [`NullTransport`]. The only non-test constructor of
    /// paper mode; the mode is never mutated to Live outside `#[cfg(test)]`.
    #[must_use]
    pub fn new_paper(ledger: IntentLedger, cfg: ExecutorConfig) -> Self {
        Self::construct(
            ExecutionMode::Paper,
            NullTransport,
            SecretString::from(""),
            cfg,
            ledger,
        )
    }
}

impl<T: OrderTransport> GrowwOrderExecutor<T> {
    /// Construct a LIVE executor — REFUSED unless [`live_send_permitted`]
    /// (Gate 2/3, `false` today). This is the belt-and-braces runtime gate over
    /// the compile-time feature + the `GROWW_ORDER_LIVE_FIRE` const.
    ///
    /// # Errors
    /// [`ExecError::LiveDisabledAtCompileTime`] when live sending is not
    /// permitted.
    pub fn new_live(
        transport: T,
        token: SecretString,
        cfg: ExecutorConfig,
        ledger: IntentLedger,
    ) -> Result<Self, ExecError> {
        if !live_send_permitted(cfg.live_fire_requested) {
            return Err(ExecError::LiveDisabledAtCompileTime);
        }
        Ok(Self::construct(
            ExecutionMode::Live,
            transport,
            token,
            cfg,
            ledger,
        ))
    }

    /// The test-only Live constructor — bypasses the [`live_send_permitted`]
    /// gate so the ambiguity ladder can be exercised against a mock transport
    /// (the engine.rs `#[cfg(test)]` mode-mutator precedent). Live mode is
    /// UNREACHABLE outside `#[cfg(test)]` except via [`new_live`], which the
    /// const gate refuses today.
    #[cfg(test)]
    pub(crate) fn new_live_for_test(
        transport: T,
        token: SecretString,
        cfg: ExecutorConfig,
        ledger: IntentLedger,
    ) -> Self {
        Self::construct(ExecutionMode::Live, transport, token, cfg, ledger)
    }

    fn construct(
        mode: ExecutionMode,
        transport: T,
        token: SecretString,
        cfg: ExecutorConfig,
        ledger: IntentLedger,
    ) -> Self {
        let next_seq = ledger.max_sequence().map_or(1, |m| m.saturating_add(1));
        Self {
            mode,
            transport,
            token,
            cfg,
            budget: GrowwRateBudget::new(),
            ledger: Arc::new(Mutex::new(ledger)),
            orders: HashMap::new(),
            provenance: HashMap::new(),
            blocked: HashSet::new(),
            next_seq,
        }
    }

    /// The executor's execution mode.
    #[must_use]
    pub fn mode(&self) -> ExecutionMode {
        self.mode
    }

    /// Read a tracked order's FSM state (test/observability accessor).
    #[must_use]
    pub fn tracked_order(&self, order_id: &str) -> Option<&TrackedOrderState> {
        self.orders.get(order_id)
    }

    // -- ledger access helpers (carry-note b) -------------------------------

    /// Run a MUTATING ledger closure under `spawn_blocking` so the fsync never
    /// stalls the async runtime. The SOLE fsync-bearing ledger path.
    async fn with_ledger<F, R>(&self, f: F) -> Result<R, ExecError>
    where
        F: FnOnce(&mut IntentLedger) -> Result<R, GrowwOmsError> + Send + 'static,
        R: Send + 'static,
    {
        let l = Arc::clone(&self.ledger);
        let joined = tokio::task::spawn_blocking(move || {
            let mut g = l.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut g)
        })
        .await;
        match joined {
            Ok(r) => r.map_err(ExecError::Oms),
            Err(_join) => Err(ExecError::LedgerTaskFailed),
        }
    }

    /// A pure READ of the ledger twin (no fsync — a plain lock is fine).
    fn with_ledger_read<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&IntentLedger) -> R,
    {
        let g = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        f(&g)
    }

    // -- place --------------------------------------------------------------

    /// Place an order. `req.order_reference_id` is OVERWRITTEN with a freshly
    /// generated reference id (the executor owns idempotency). `date`/`now_ms`
    /// are the caller's IST clock; `session_open` gates mutations to the
    /// trading window.
    ///
    /// # Errors
    /// Validation, ledger, budget, or market-closed refusals.
    pub async fn place_order(
        &mut self,
        mut req: GrowwCreateOrderReq,
        date: IstDate,
        now_ms: i64,
        session_open: bool,
    ) -> Result<PlaceResult, ExecError> {
        if !session_open {
            return Err(ExecError::MarketClosed);
        }
        // Reference id: max(ledger seq, local counter) + 1, CSPRNG salt.
        let seq = self.reserve_sequence();
        let ref_id = generate_reference_id(date, seq, reference_id_salt());
        req.order_reference_id.clone_from(&ref_id);
        let segment = req.segment;
        validate_create_order(&req, self.cfg.max_order_quantity)?;

        // Write-ahead: fsync `recorded` BEFORE any send (spawn_blocking).
        let mode = self.mode.ledger_mode().to_owned();
        let intent_id = ref_id.clone();
        let new = NewIntent {
            intent_id: intent_id.clone(),
            reference_id: ref_id.clone(),
            kind: IntentKind::Place,
            groww_order_id: None,
            mode,
            ts_ms: now_ms,
            linked_intent_id: None,
        };
        let receipt = self
            .with_ledger(move |l| l.record_intent(new, date))
            .await?;

        if self.mode == ExecutionMode::Paper {
            // Paper: simulate an acceptance, ZERO HTTP. Synthetic order id.
            // The receipt proves the write-ahead; the paper lane never sends,
            // so phase appends go by intent id (append_intent).
            let _ = &receipt;
            let order_id = format!("PAPER-{ref_id}");
            self.append_intent(
                &ref_id,
                IntentPhase::Acked,
                now_ms,
                date,
                Some(order_id.clone()),
                Some("paper-simulated ack".to_owned()),
            )
            .await?;
            self.adopt_order(
                &order_id,
                GrowwOrderStatus::Open,
                0,
                PlaceProvenance::Settled,
            );
            metrics::counter!("tv_groww_orders_placed_total", "mode" => "paper").increment(1);
            return Ok(PlaceResult::PaperAccepted { order_id });
        }

        // Live: budget → sent → send → classify.
        if !self.budget.check_mutation().is_allowed() {
            metrics::counter!("tv_groww_order_budget_denied_total", "family" => "orders")
                .increment(1);
            // No send happened; the intent stays `recorded` (never-sent class
            // on replay). Surface the budget refusal.
            return Err(ExecError::RateBudgetExceeded(RateFamily::Orders));
        }
        self.append_intent(&ref_id, IntentPhase::Sent, now_ms, date, None, None)
            .await?;

        let outcome = self
            .transport
            .create_order(&req, &self.token, &receipt)
            .await;
        metrics::counter!("tv_groww_orders_placed_total", "mode" => "live").increment(1);
        match outcome {
            TransportOutcome::Success(payload) => {
                match payload.groww_order_id {
                    Some(order_id) if !order_id.trim().is_empty() => {
                        self.append_intent(
                            &ref_id,
                            IntentPhase::Acked,
                            now_ms,
                            date,
                            Some(order_id.clone()),
                            None,
                        )
                        .await?;
                        self.adopt_order(
                            &order_id,
                            GrowwOrderStatus::Open,
                            0,
                            PlaceProvenance::Settled,
                        );
                        Ok(PlaceResult::Accepted { order_id })
                    }
                    // 2xx SUCCESS lacking a usable id ⇒ AMBIGUOUS (O-2).
                    _ => {
                        self.append_intent(
                            &ref_id,
                            IntentPhase::Ambiguous,
                            now_ms,
                            date,
                            None,
                            Some("2xx missing order id".to_owned()),
                        )
                        .await?;
                        self.resolve_place(
                            &ref_id,
                            segment,
                            &req,
                            date,
                            now_ms,
                            session_open,
                            &receipt,
                        )
                        .await
                    }
                }
            }
            TransportOutcome::Rejected { ga_code, .. } => {
                self.append_intent(
                    &ref_id,
                    IntentPhase::Rejected,
                    now_ms,
                    date,
                    None,
                    ga_code.clone(),
                )
                .await?;
                Ok(PlaceResult::Rejected { ga_code })
            }
            TransportOutcome::RateLimited { .. } => {
                // hostile F-1: 429 enters the FULL ladder on the SAME reference
                // id — NEVER a short-circuit re-place.
                metrics::counter!("tv_groww_order_rate_limited_total", "family" => "orders")
                    .increment(1);
                self.append_intent(
                    &ref_id,
                    IntentPhase::Ambiguous,
                    now_ms,
                    date,
                    None,
                    Some("429".to_owned()),
                )
                .await?;
                self.resolve_place(&ref_id, segment, &req, date, now_ms, session_open, &receipt)
                    .await
            }
            TransportOutcome::AuthStale { .. } | TransportOutcome::Ambiguous(_) => {
                let detail = match &outcome {
                    TransportOutcome::AuthStale { http_status } => {
                        format!("auth_stale_{http_status}")
                    }
                    TransportOutcome::Ambiguous(r) => r.as_str().to_owned(),
                    _ => "ambiguous".to_owned(),
                };
                self.append_intent(
                    &ref_id,
                    IntentPhase::Ambiguous,
                    now_ms,
                    date,
                    None,
                    Some(detail),
                )
                .await?;
                self.resolve_place(&ref_id, segment, &req, date, now_ms, session_open, &receipt)
                    .await
            }
        }
    }

    /// The place-ambiguity resolution ladder (design §4.7): poll
    /// `GET /v1/order/status/reference/{ref}` at 2/5/10/30/60 → 60s-paced to
    /// the budget; auth-stale PAUSES the clock; a strict not-landed replays
    /// ONCE on the SAME reference id (bounded); exhaustion ⇒ Unresolved
    /// (Critical).
    #[allow(clippy::too_many_arguments)] // APPROVED: the ladder needs the full mutation context
    async fn resolve_place(
        &mut self,
        ref_id: &str,
        segment: GrowwSegment,
        req: &GrowwCreateOrderReq,
        date: IstDate,
        now_ms: i64,
        session_open: bool,
        receipt: &super::intent_ledger::IntentReceipt,
    ) -> Result<PlaceResult, ExecError> {
        metrics::counter!("tv_groww_order_ambiguous_total", "op" => "place").increment(1);
        let mut spent: u64 = 0;
        let mut step: usize = 0;
        let mut auth_rechecks: u32 = 0;
        let mut replays: u32 = 0;
        loop {
            if spent >= self.cfg.ambiguity_ladder_max_secs {
                return self.exhaust_place(ref_id, now_ms, date).await;
            }
            let delay = ladder_delay_for_step(step);
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            // Reserve a resolution read slot (never starves — reserved slice).
            let _slot = self.budget.check_resolution_read();
            let obs = self
                .transport
                .get_status_by_reference(ref_id, segment, &self.token)
                .await;
            match obs {
                TransportOutcome::Success(p) => {
                    if let Some(order_id) = p.groww_order_id.filter(|s| !s.trim().is_empty()) {
                        let filled = p.filled_quantity.unwrap_or(0);
                        self.append_intent(
                            ref_id,
                            IntentPhase::ResolvedLanded,
                            now_ms,
                            date,
                            Some(order_id.clone()),
                            None,
                        )
                        .await?;
                        let status =
                            GrowwOrderStatus::parse(p.order_status.as_deref().unwrap_or("OPEN"));
                        self.adopt_order(&order_id, status, filled, PlaceProvenance::Settled);
                        metrics::counter!("tv_groww_order_ambiguity_resolved_total", "outcome" => "landed").increment(1);
                        return Ok(PlaceResult::ResolvedLanded { order_id });
                    }
                    // 200 SUCCESS with no usable id — keep laddering (h11).
                    spent += delay;
                    step += 1;
                }
                TransportOutcome::Rejected {
                    http_status,
                    ga_code,
                    ..
                } if http_status == 404 || ga_code.as_deref() == Some("GA004") => {
                    // STRICT not-landed: 404-class AND well-shaped FAILURE.
                    if replays < MAX_PLACE_REPLAYS
                        && self.cfg.replay_policy_auto
                        && session_open
                        && intent_is_same_day(ref_id, date)
                    {
                        replays += 1;
                        metrics::counter!("tv_groww_order_replays_total").increment(1);
                        self.append_intent(
                            ref_id,
                            IntentPhase::Replayed,
                            now_ms,
                            date,
                            None,
                            Some(format!("replay {replays}")),
                        )
                        .await?;
                        // Re-send the SAME request (SAME reference id).
                        let re = self
                            .replay_place(ref_id, segment, req, date, now_ms, receipt)
                            .await?;
                        match re {
                            ReplayVerdict::Landed { order_id } => {
                                return Ok(PlaceResult::ResolvedLanded { order_id });
                            }
                            ReplayVerdict::NotLanded => {
                                self.append_intent(
                                    ref_id,
                                    IntentPhase::ResolvedNotLanded,
                                    now_ms,
                                    date,
                                    None,
                                    Some("replay confirmed not-landed".to_owned()),
                                )
                                .await?;
                                metrics::counter!("tv_groww_order_ambiguity_resolved_total", "outcome" => "not_landed").increment(1);
                                return Ok(PlaceResult::ResolvedNotLanded);
                            }
                            ReplayVerdict::StillAmbiguous => {
                                spent += delay;
                                step += 1;
                            }
                        }
                    } else {
                        self.append_intent(
                            ref_id,
                            IntentPhase::ResolvedNotLanded,
                            now_ms,
                            date,
                            None,
                            Some("not-landed (no replay)".to_owned()),
                        )
                        .await?;
                        metrics::counter!("tv_groww_order_ambiguity_resolved_total", "outcome" => "not_landed").increment(1);
                        return Ok(PlaceResult::ResolvedNotLanded);
                    }
                }
                TransportOutcome::AuthStale { .. } => {
                    // Clock PAUSE (hostile F-5): do NOT add `delay` to spent.
                    auth_rechecks += 1;
                    if auth_rechecks > MAX_AUTH_RECHECKS {
                        return self.exhaust_place(ref_id, now_ms, date).await;
                    }
                    metrics::counter!("tv_groww_order_ga_total", "code" => "auth_stale")
                        .increment(1);
                    tokio::time::sleep(std::time::Duration::from_secs(AUTH_RECHECK_SECS)).await;
                    // step/spent unchanged — the ladder resumes on recovery.
                }
                // Ambiguous / RateLimited / non-404 Rejected — keep laddering.
                _ => {
                    spent += delay;
                    step += 1;
                }
            }
        }
    }

    async fn exhaust_place(
        &mut self,
        ref_id: &str,
        now_ms: i64,
        date: IstDate,
    ) -> Result<PlaceResult, ExecError> {
        self.append_intent(
            ref_id,
            IntentPhase::Unresolved,
            now_ms,
            date,
            None,
            Some("ladder exhausted".to_owned()),
        )
        .await?;
        // GROWW-ORD-03 (Critical) emit lands with the shared variants (ORD-PR-1).
        tracing::error!(
            target: "groww_ord",
            reference_id = %ref_id,
            "groww order: ambiguity UNRESOLVED after ladder — operator action required"
        );
        metrics::counter!("tv_groww_order_ambiguity_resolved_total", "outcome" => "unresolved")
            .increment(1);
        Ok(PlaceResult::Unresolved)
    }

    /// Re-send a place with the SAME reference id and classify the replay
    /// (design §4.7: GA007 ⇒ first landed after all ⇒ adopt; Success ⇒ landed;
    /// else keep laddering). Bounded by the caller's replay counter.
    #[allow(clippy::too_many_arguments)] // APPROVED: the replay needs the full mutation context
    async fn replay_place(
        &mut self,
        ref_id: &str,
        segment: GrowwSegment,
        req: &GrowwCreateOrderReq,
        date: IstDate,
        now_ms: i64,
        receipt: &super::intent_ledger::IntentReceipt,
    ) -> Result<ReplayVerdict, ExecError> {
        // A replay re-sends the SAME request on the SAME reference id, carrying
        // the ORIGINAL intent's receipt (the write-ahead proof is unchanged);
        // the ledger keeps the linked phase history via append_intent.
        self.append_intent(
            ref_id,
            IntentPhase::Sent,
            now_ms,
            date,
            None,
            Some("replay send".to_owned()),
        )
        .await?;
        let outcome = self.transport.create_order(req, &self.token, receipt).await;
        match outcome {
            TransportOutcome::Success(p) => {
                if let Some(order_id) = p.groww_order_id.filter(|s| !s.trim().is_empty()) {
                    self.append_intent(
                        ref_id,
                        IntentPhase::ResolvedLanded,
                        now_ms,
                        date,
                        Some(order_id.clone()),
                        Some("replay landed".to_owned()),
                    )
                    .await?;
                    self.adopt_order(
                        &order_id,
                        GrowwOrderStatus::Open,
                        0,
                        PlaceProvenance::Settled,
                    );
                    Ok(ReplayVerdict::Landed { order_id })
                } else {
                    Ok(ReplayVerdict::StillAmbiguous)
                }
            }
            TransportOutcome::Rejected { ga_code, .. } if ga_code.as_deref() == Some("GA007") => {
                // Duplicate ref ⇒ the original DID land ⇒ re-resolve by reference.
                let re = self
                    .transport
                    .get_status_by_reference(ref_id, segment, &self.token)
                    .await;
                if let TransportOutcome::Success(p) = re
                    && let Some(order_id) = p.groww_order_id.filter(|s| !s.trim().is_empty())
                {
                    let filled = p.filled_quantity.unwrap_or(0);
                    self.append_intent(
                        ref_id,
                        IntentPhase::ResolvedLanded,
                        now_ms,
                        date,
                        Some(order_id.clone()),
                        Some("ga007 re-resolve".to_owned()),
                    )
                    .await?;
                    let status =
                        GrowwOrderStatus::parse(p.order_status.as_deref().unwrap_or("OPEN"));
                    self.adopt_order(&order_id, status, filled, PlaceProvenance::Settled);
                    return Ok(ReplayVerdict::Landed { order_id });
                }
                Ok(ReplayVerdict::StillAmbiguous)
            }
            TransportOutcome::Rejected {
                http_status,
                ga_code,
                ..
            } if http_status == 404 || ga_code.as_deref() == Some("GA004") => {
                Ok(ReplayVerdict::NotLanded)
            }
            _ => Ok(ReplayVerdict::StillAmbiguous),
        }
    }

    // -- modify -------------------------------------------------------------

    /// Modify an order — REFUSED on an unresolved place (carry-note a / P50)
    /// and on any partially-filled order (O-11 unproven, hostile F-4).
    ///
    /// # Errors
    /// Validation / eligibility / ledger / budget refusals.
    pub async fn modify_order(
        &mut self,
        req: GrowwModifyOrderReq,
        date: IstDate,
        now_ms: i64,
        session_open: bool,
    ) -> Result<MutationResult, ExecError> {
        if !session_open {
            return Err(ExecError::MarketClosed);
        }
        validate_modify_order(&req, self.cfg.max_order_quantity)?;
        let order_id = req.groww_order_id.clone();
        self.check_mutation_eligible(&order_id)?;
        // O-11: modify refused on any partial fill.
        if let Some(state) = self.orders.get(&order_id)
            && state.filled_quantity > 0
        {
            return Err(ExecError::Oms(GrowwOmsError::ModifyOnPartialFillUnproven {
                groww_order_id: order_id,
            }));
        }
        self.run_mutation(&order_id, MutationSend::Modify(req), date, now_ms)
            .await
    }

    // -- cancel -------------------------------------------------------------

    /// Cancel an order — REFUSED on an unresolved place (carry-note a / P50).
    ///
    /// # Errors
    /// Eligibility / ledger / budget refusals.
    pub async fn cancel_order(
        &mut self,
        req: GrowwCancelOrderReq,
        date: IstDate,
        now_ms: i64,
        session_open: bool,
    ) -> Result<MutationResult, ExecError> {
        if !session_open {
            return Err(ExecError::MarketClosed);
        }
        let order_id = req.groww_order_id.clone();
        self.check_mutation_eligible(&order_id)?;
        self.run_mutation(&order_id, MutationSend::Cancel(req), date, now_ms)
            .await
    }

    /// Shared modify/cancel body: write-ahead intent → budget → sent → send →
    /// classify (definitive reject / accepted / ambiguous → detail resolution).
    async fn run_mutation(
        &mut self,
        order_id: &str,
        send: MutationSend,
        date: IstDate,
        now_ms: i64,
    ) -> Result<MutationResult, ExecError> {
        let kind = send.kind();
        let segment = send.segment();
        // A modify/cancel rides its OWN intent, keyed on the order id (the
        // ledger's MutationInFlight serialization invariant applies).
        let seq = self.reserve_sequence();
        let ref_id = generate_reference_id(date, seq, reference_id_salt());
        let mode = self.mode.ledger_mode().to_owned();
        let oid = order_id.to_owned();
        let new = NewIntent {
            intent_id: ref_id.clone(),
            reference_id: ref_id.clone(),
            kind,
            groww_order_id: Some(oid.clone()),
            mode,
            ts_ms: now_ms,
            linked_intent_id: None,
        };
        let receipt = self
            .with_ledger(move |l| l.record_intent(new, date))
            .await?;

        if self.mode == ExecutionMode::Paper {
            self.append_intent(
                &ref_id,
                IntentPhase::Acked,
                now_ms,
                date,
                Some(oid),
                Some("paper mutation ack".to_owned()),
            )
            .await?;
            metrics::counter!("tv_groww_order_mutation_paper_total", "kind" => kind.as_str())
                .increment(1);
            return Ok(MutationResult::Accepted);
        }

        if !self.budget.check_mutation().is_allowed() {
            metrics::counter!("tv_groww_order_budget_denied_total", "family" => "orders")
                .increment(1);
            return Err(ExecError::RateBudgetExceeded(RateFamily::Orders));
        }
        self.append_intent(
            &ref_id,
            IntentPhase::Sent,
            now_ms,
            date,
            Some(order_id.to_owned()),
            None,
        )
        .await?;
        let outcome = match &send {
            MutationSend::Modify(r) => self.transport.modify_order(r, &self.token, &receipt).await,
            MutationSend::Cancel(r) => self.transport.cancel_order(r, &self.token, &receipt).await,
        };
        match outcome {
            TransportOutcome::Success(_) => {
                self.append_intent(
                    &ref_id,
                    IntentPhase::Acked,
                    now_ms,
                    date,
                    Some(order_id.to_owned()),
                    None,
                )
                .await?;
                Ok(MutationResult::Accepted)
            }
            TransportOutcome::Rejected { ga_code, .. } => {
                self.append_intent(
                    &ref_id,
                    IntentPhase::Rejected,
                    now_ms,
                    date,
                    Some(order_id.to_owned()),
                    ga_code.clone(),
                )
                .await?;
                Ok(MutationResult::Rejected { ga_code })
            }
            TransportOutcome::RateLimited { .. }
            | TransportOutcome::AuthStale { .. }
            | TransportOutcome::Ambiguous(_) => {
                metrics::counter!("tv_groww_order_ambiguous_total", "op" => kind.as_str())
                    .increment(1);
                self.append_intent(
                    &ref_id,
                    IntentPhase::Ambiguous,
                    now_ms,
                    date,
                    Some(order_id.to_owned()),
                    Some("mutation ambiguous".to_owned()),
                )
                .await?;
                self.resolve_mutation(&ref_id, order_id, segment, kind, date, now_ms)
                    .await
            }
        }
    }

    /// Modify/cancel ambiguity resolution — a bounded detail-poll ladder that
    /// feeds the observation through the FSM ([`evaluate_transition`]) so a
    /// cancel that lost the race to a fill surfaces as
    /// [`MutationResult::CancelLostRace`] (hostile F-2/F-10). Never auto-resends
    /// a modify (O-11).
    async fn resolve_mutation(
        &mut self,
        ref_id: &str,
        order_id: &str,
        segment: GrowwSegment,
        kind: IntentKind,
        date: IstDate,
        now_ms: i64,
    ) -> Result<MutationResult, ExecError> {
        let mut spent: u64 = 0;
        let mut step: usize = 0;
        loop {
            if spent >= self.cfg.ambiguity_ladder_max_secs {
                self.append_intent(
                    ref_id,
                    IntentPhase::Unresolved,
                    now_ms,
                    date,
                    Some(order_id.to_owned()),
                    Some("mutation ladder exhausted".to_owned()),
                )
                .await?;
                tracing::error!(
                    target: "groww_ord",
                    order_id = %order_id,
                    "groww order: mutation ambiguity UNRESOLVED — operator action required"
                );
                return Ok(MutationResult::Unresolved);
            }
            let delay = ladder_delay_for_step(step);
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            let _slot = self.budget.check_resolution_read();
            let obs = self
                .transport
                .get_order_detail(order_id, segment, &self.token)
                .await;
            if let TransportOutcome::Success(detail) = obs {
                let status =
                    GrowwOrderStatus::parse(detail.order_status.as_deref().unwrap_or("OPEN"));
                let filled = detail.filled_quantity.unwrap_or(0);
                let decision =
                    self.observe(order_id, &status, Some(filled), ObservationSource::Live);
                if decision.cancel_lost_race
                    || (kind == IntentKind::Cancel && decision.terminal_with_fill)
                {
                    self.append_intent(
                        ref_id,
                        IntentPhase::ResolvedLanded,
                        now_ms,
                        date,
                        Some(order_id.to_owned()),
                        Some("cancel lost race".to_owned()),
                    )
                    .await?;
                    // GROWW-ORD-04-class "position exists" emit lands in ORD-PR-1.
                    tracing::error!(
                        target: "groww_ord",
                        order_id = %order_id,
                        filled,
                        "groww order: cancel lost the race — POSITION EXISTS"
                    );
                    return Ok(MutationResult::CancelLostRace {
                        filled_quantity: filled,
                    });
                }
                if is_terminal(&status) {
                    self.append_intent(
                        ref_id,
                        IntentPhase::ResolvedLanded,
                        now_ms,
                        date,
                        Some(order_id.to_owned()),
                        Some("mutation confirmed".to_owned()),
                    )
                    .await?;
                    return Ok(MutationResult::Accepted);
                }
            }
            spent += delay;
            step += 1;
        }
    }

    // -- EOD sweep ----------------------------------------------------------

    /// The 15:35 IST EOD sweep (design §4.9 / F-7): classify + force-terminalize
    /// every non-terminal local order. Pure over the in-memory state; the
    /// per-order polling stop + audit rows are the caller's (boot layer).
    pub fn eod_sweep(&mut self) -> EodSweepReport {
        let mut report = EodSweepReport::default();
        let ids: Vec<String> = self.orders.keys().cloned().collect();
        for id in ids {
            let Some(state) = self.orders.get(&id) else {
                continue;
            };
            match classify_eod_action(state) {
                EodAction::ForceExpireLocal => {
                    report.expired.push(id.clone());
                    self.force_terminal(&id, GrowwOrderStatus::Cancelled);
                }
                EodAction::ForceExpireWithPartialFill { filled_quantity } => {
                    report
                        .partial_fill_positions
                        .push((id.clone(), filled_quantity));
                    self.force_terminal(&id, GrowwOrderStatus::Cancelled);
                }
                EodAction::FilledAwaitSettlement => {
                    report.filled_await_settlement.push(id);
                }
                EodAction::UnknownAtEodCritical => {
                    report.unknown_critical.push(id);
                }
                EodAction::AlreadyTerminal => {}
            }
        }
        report
    }

    // -- internal helpers ---------------------------------------------------

    fn reserve_sequence(&mut self) -> u32 {
        // The ledger's replayed max is the crash-safe floor; the local counter
        // advances monotonically within the session.
        let ledger_max = self.with_ledger_read(IntentLedger::max_sequence);
        let floor = ledger_max.map_or(0, |m| m.saturating_add(1));
        let seq = self.next_seq.max(floor);
        self.next_seq = seq.saturating_add(1);
        seq
    }

    async fn append_intent(
        &self,
        intent_id: &str,
        phase: IntentPhase,
        ts_ms: i64,
        date: IstDate,
        groww_order_id: Option<String>,
        detail: Option<String>,
    ) -> Result<(), ExecError> {
        let iid = intent_id.to_owned();
        self.with_ledger(move |l| l.append_phase(&iid, phase, ts_ms, date, groww_order_id, detail))
            .await
    }

    fn adopt_order(
        &mut self,
        order_id: &str,
        status: GrowwOrderStatus,
        filled: i64,
        provenance: PlaceProvenance,
    ) {
        self.orders
            .insert(order_id.to_owned(), TrackedOrderState::new(status, filled));
        self.provenance.insert(order_id.to_owned(), provenance);
        metrics::gauge!("tv_groww_orders_open").set(self.open_order_count() as f64);
    }

    /// Feed an observation through the FSM and store the adopted state.
    fn observe(
        &mut self,
        order_id: &str,
        status: &GrowwOrderStatus,
        filled: Option<i64>,
        source: ObservationSource,
    ) -> super::state::TransitionDecision {
        let current = self.orders.get(order_id).cloned().unwrap_or_else(|| {
            TrackedOrderState::new(GrowwOrderStatus::Unknown("n/a".to_owned()), 0)
        });
        let obs = OrderObservation {
            status: status.clone(),
            filled_quantity: filled,
            source,
        };
        let decision = evaluate_transition(&current, &obs);
        // Adopt only on legal transition / same-status refresh (fill upward).
        match decision.outcome {
            TransitionOutcome::Transition => {
                let f = filled
                    .unwrap_or(current.filled_quantity)
                    .max(current.filled_quantity);
                let mut next = TrackedOrderState::new(status.clone(), f);
                next.consecutive_stale_reconcile = decision.next_stale_reconcile_count;
                self.orders.insert(order_id.to_owned(), next);
            }
            TransitionOutcome::SameStatusRefresh => {
                if let Some(s) = self.orders.get_mut(order_id) {
                    if let Some(f) = filled {
                        s.filled_quantity = s.filled_quantity.max(f);
                    }
                    s.consecutive_stale_reconcile = decision.next_stale_reconcile_count;
                }
            }
            TransitionOutcome::Park | TransitionOutcome::StaleSnapshotSkip => {
                if let Some(s) = self.orders.get_mut(order_id) {
                    s.consecutive_stale_reconcile = decision.next_stale_reconcile_count;
                }
            }
        }
        decision
    }

    fn force_terminal(&mut self, order_id: &str, terminal: GrowwOrderStatus) {
        if let Some(s) = self.orders.get_mut(order_id) {
            s.status = terminal;
        }
        self.provenance
            .insert(order_id.to_owned(), PlaceProvenance::Settled);
        self.blocked.remove(order_id);
        metrics::gauge!("tv_groww_orders_open").set(self.open_order_count() as f64);
    }

    fn open_order_count(&self) -> usize {
        self.orders
            .values()
            .filter(|s| !is_terminal(&s.status))
            .count()
    }

    /// The P50 mutation-eligibility gate (carry-note a): the order must be
    /// KNOWN, its place SETTLED (adopted a broker id), and not blocked by an
    /// in-flight mutation.
    fn check_mutation_eligible(&self, order_id: &str) -> Result<(), ExecError> {
        match self.provenance.get(order_id) {
            None => Err(ExecError::Oms(GrowwOmsError::OrderParkedUnknown {
                groww_order_id: order_id.to_owned(),
            })),
            Some(PlaceProvenance::Unresolved) => {
                Err(ExecError::Oms(GrowwOmsError::MutationInFlight {
                    groww_order_id: order_id.to_owned(),
                }))
            }
            Some(PlaceProvenance::Settled) => {
                if self.blocked.contains(order_id) {
                    Err(ExecError::Oms(GrowwOmsError::MutationInFlight {
                        groww_order_id: order_id.to_owned(),
                    }))
                } else if matches!(
                    self.orders.get(order_id).map(|s| &s.status),
                    Some(GrowwOrderStatus::Unknown(_))
                ) {
                    Err(ExecError::Oms(GrowwOmsError::OrderParkedUnknown {
                        groww_order_id: order_id.to_owned(),
                    }))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Register an order whose place is AMBIGUOUS (no adopted id yet) as
    /// mutation-blocked (the P50 test hook). Keyed on a provisional id.
    #[cfg(test)]
    pub(crate) fn register_unresolved_place(&mut self, provisional_order_id: &str) {
        self.orders.insert(
            provisional_order_id.to_owned(),
            TrackedOrderState::new(GrowwOrderStatus::Unknown("ambiguous".to_owned()), 0),
        );
        self.provenance
            .insert(provisional_order_id.to_owned(), PlaceProvenance::Unresolved);
    }
}

/// Verdict of a single place replay.
enum ReplayVerdict {
    Landed { order_id: String },
    NotLanded,
    StillAmbiguous,
}

/// A modify/cancel request the shared [`GrowwOrderExecutor::run_mutation`] body
/// dispatches — avoids a generic closure-returning-future (lifetime-heavy).
enum MutationSend {
    Modify(GrowwModifyOrderReq),
    Cancel(GrowwCancelOrderReq),
}

impl MutationSend {
    const fn kind(&self) -> IntentKind {
        match self {
            Self::Modify(_) => IntentKind::Modify,
            Self::Cancel(_) => IntentKind::Cancel,
        }
    }

    fn segment(&self) -> GrowwSegment {
        match self {
            Self::Modify(r) => r.segment,
            Self::Cancel(r) => r.segment,
        }
    }
}

/// The next ladder delay for a step index: 2/5/10/30/60 then 60s-paced.
#[must_use]
pub fn ladder_delay_for_step(step: usize) -> u64 {
    LADDER_STEPS_SECS
        .get(step)
        .copied()
        .unwrap_or(LADDER_PACE_SECS)
}

/// Whether a reference id's date block equals `today` (same-day replay gate).
#[must_use]
pub fn intent_is_same_day(reference_id: &str, today: IstDate) -> bool {
    super::reference_id::decompose(reference_id).is_some_and(|p| p.yymmdd == today.yymmdd_num())
}

/// Reference-id salt from a CSPRNG (carry-note c). `uuid::Uuid::new_v4()` is a
/// getrandom OS-CSPRNG; NO predictable source (clock/counter) feeds this.
#[must_use]
pub fn reference_id_salt() -> u32 {
    // Low 32 bits of a v4 (random) UUID — OS-CSPRNG entropy.
    uuid::Uuid::new_v4().as_u128() as u32
}

#[cfg(test)]
mod tests {
    include!("executor_tests.rs");
}
