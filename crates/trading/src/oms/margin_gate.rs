//! 🔷 DHAN pre-trade margin gate (umbrella plan cluster E2).
//!
//! Cold path — the gate is consulted per ORDER (~1-100/day), never per tick;
//! allocations are acceptable here.
//!
//! # Shared-account contract
//! The Dhan account is pooled with the BruteX co-tenant, so `fundlimit`
//! reflects the WHOLE account. `insufficientBalance == 0` alone is therefore
//! NOT authorization to spend: the gate additionally applies a PER-ENTRY cap
//! of `tenant_budget_percent` (≤ 50) of the THEN-CURRENT pooled
//! `availabelBalance`. HONEST LIMIT — the cap is per-entry, NOT cumulative:
//! sequential entries each re-read the balance, so they can cumulatively
//! consume more of the pool (geometrically), and the gate cannot distinguish
//! OUR utilized margin from the co-tenant's (`fundlimit` is pooled), so no
//! cumulative ledger exists yet (a cumulative tenant ledger is a flagged
//! follow-up for the OMS-wiring PR). The funds/margin REST legs are
//! self-capped at ≤ 10 req/sec (50% of Dhan's 20/sec non-trading budget) —
//! PER GATE INSTANCE: the OMS-wiring PR must construct exactly ONE gate per
//! process (and pin that with its own ratchet); until then, multi-instance
//! construction could exceed the documented process-wide budget. BruteX may
//! still consume margin between our check and our order — that TOCTOU
//! window is irreducible; the check is advisory-best-effort and the broker
//! is the final arbiter.
//!
//! # The OFF-switch lattice (config AND code lock)
//! The REST legs fire only when `[dhan_margin_gate] enabled = true` AND the
//! code-change master lock
//! [`tickvault_common::constants::DHAN_MARGIN_GATE_REST_ALLOWED`] is `true`.
//! The const is `false` today (the umbrella plan's cluster-E2 hold: the live
//! funds/margin REST call awaits the operator grant) — config flips alone
//! can never turn the REST legs on.
//!
//! # Consumer contract
//! - EXITS ARE NEVER MARGIN-GATED: [`MarginGate::check_exit`] is
//!   structurally REST-free (no token read, no REST call, no limiter touch).
//! - Entry-check unavailability fails CLOSED for entries, OPEN for exits.
//! - LIVE (`dry_run = false`) consumers MUST use
//!   [`MarginVerdict::permits_live_entry`] — a disabled gate blocks live
//!   entries fail-closed. Paper consumers use
//!   [`MarginVerdict::permits_paper_entry`] (a disabled gate equals today's
//!   paper behaviour).
//!
//! The `RiskEngine::check_order` integration takes
//! [`super::types::OrderIntent`] and is a follow-up that rebases after
//! cluster A's risk-engine round merges. A typed error-code variant is
//! deliberately deferred to the first production emit site (the OMS-wiring
//! PR) — the rule-file cross-reference contract requires the runbook and the
//! variant to land together.

use std::num::NonZeroU32;
use std::sync::Arc;

use governor::{Quota, RateLimiter, clock::DefaultClock, state::InMemoryState, state::NotKeyed};
use secrecy::ExposeSecret;
use tickvault_common::config::DhanMarginGateConfig;
use tickvault_common::constants::DHAN_MARGIN_GATE_REST_ALLOWED;
use tickvault_common::sanitize::capture_rest_error_body;
use tracing::{error, warn};

use super::api_client::OrderApiClient;
use super::engine::TokenProvider;
use super::types::{
    FundLimitResponse, MarginCalculatorRequest, MarginCalculatorResponse, OrderIntent,
};

// ---------------------------------------------------------------------------
// Money helpers (integer paise; i128 intermediates for products)
// ---------------------------------------------------------------------------

/// Max plausible rupee magnitude (1e12 = ₹1 lakh crore) — beyond this the
/// response is corrupt, not a real account value.
const MAX_PLAUSIBLE_RUPEES: f64 = 1.0e12;

/// One entry check issues TWO REST calls (margin calculator + fund limit)
/// as a single burst against the self-cap.
const ENTRY_REST_BURST: NonZeroU32 = NonZeroU32::MIN.saturating_add(1);

/// Converts a NON-NEGATIVE rupee amount to integer paise.
///
/// Returns `None` (fail-closed) when the value is not finite, negative, or
/// implausibly large (> [`MAX_PLAUSIBLE_RUPEES`]) — a garbage broker value
/// must refuse, never round-trip into a verdict.
pub fn to_paise(rupees: f64) -> Option<i64> {
    if !rupees.is_finite() || !(0.0..=MAX_PLAUSIBLE_RUPEES).contains(&rupees) {
        return None;
    }
    // Bounded by MAX_PLAUSIBLE_RUPEES * 100 << i64::MAX — the cast cannot
    // overflow or truncate past the plausibility gate above.
    #[allow(clippy::cast_possible_truncation)] // APPROVED: bounded by MAX_PLAUSIBLE_RUPEES
    Some((rupees * 100.0).round() as i64)
}

/// Converts a rupee amount to integer paise, allowing NEGATIVE values
/// (debit balances are real on a fund-limit response).
///
/// Returns `None` when the value is not finite or its magnitude exceeds
/// [`MAX_PLAUSIBLE_RUPEES`].
pub fn to_paise_signed(rupees: f64) -> Option<i64> {
    if !rupees.is_finite() || rupees.abs() > MAX_PLAUSIBLE_RUPEES {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)] // APPROVED: bounded by MAX_PLAUSIBLE_RUPEES
    Some((rupees * 100.0).round() as i64)
}

// ---------------------------------------------------------------------------
// Verdict types
// ---------------------------------------------------------------------------

/// The paise-integer numbers behind an entry verdict (forensic payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginSnapshot {
    /// Margin the broker says this entry requires.
    pub required_paise: i64,
    /// The POOLED account's available balance (shared with the co-tenant).
    pub available_paise: i64,
    /// OUR budgeted share of the pooled balance
    /// (`available * tenant_budget_percent / 100`; 0 when not computed).
    pub usable_paise: i64,
    /// SPAN margin component — informational; lenient conversion
    /// (unparsable → 0, never gates the verdict).
    pub span_paise: i64,
    /// Exposure margin component — informational; lenient conversion.
    pub exposure_paise: i64,
    /// Broker-reported shortfall (0 when the whole account can afford it).
    pub insufficient_paise: i64,
}

/// Which leg of the entry check degraded (fail-closed for entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedStage {
    /// No access token was available.
    Token,
    /// The margin-calculator REST call failed.
    MarginCall,
    /// The fund-limit REST call failed.
    FundLimitCall,
}

impl DegradedStage {
    /// Stable wire-format label for logs/metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::MarginCall => "margin_call",
            Self::FundLimitCall => "fund_limit_call",
        }
    }
}

/// Why a broker response was refused as implausible (fail-closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplausibleReason {
    /// A margin value was non-finite, negative, or implausibly large.
    NonFiniteOrNegativeMargin,
    /// The broker-reported shortfall (`insufficientBalance`) was
    /// non-finite, negative, or implausibly large — distinct from a bad
    /// `totalMargin` so triage can tell WHICH field was garbage.
    NonFiniteInsufficientBalance,
    /// `totalMargin == 0` — a real F&O entry never costs zero; zero usually
    /// means serde-default backfill of an absent/renamed field.
    ZeroTotalMargin,
    /// The fund-limit balance was non-finite or implausibly large.
    NonFiniteFunds,
    /// The fund-limit response named a DIFFERENT client id — a
    /// wrong-account response must never authorize spend.
    ClientIdMismatch,
}

impl ImplausibleReason {
    /// Stable wire-format label for logs/metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NonFiniteOrNegativeMargin => "non_finite_or_negative_margin",
            Self::NonFiniteInsufficientBalance => "non_finite_insufficient_balance",
            Self::ZeroTotalMargin => "zero_total_margin",
            Self::NonFiniteFunds => "non_finite_funds",
            Self::ClientIdMismatch => "client_id_mismatch",
        }
    }
}

/// The margin gate's verdict for one order intent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MarginVerdict {
    /// The intent is an exit — NEVER margin-gated, always allowed.
    ExitAlwaysAllowed,
    /// The gate is disabled (config off OR the REST master lock is held) —
    /// no REST call was made. Paper entries proceed (today's behaviour);
    /// LIVE entries are BLOCKED fail-closed by
    /// [`MarginVerdict::permits_live_entry`].
    SkippedDisabled,
    /// Entry affordable inside OUR tenant budget of the pooled balance.
    Allowed(MarginSnapshot),
    /// The broker says even the WHOLE pooled account cannot afford it.
    RefusedInsufficient(MarginSnapshot),
    /// Affordable for the pooled account, but over OUR tenant budget share.
    RefusedTenantBudget(MarginSnapshot),
    /// The funds/margin REST self-cap refused the burst — a delayed
    /// pre-trade verdict is a stale verdict, so the gate refuses instead of
    /// queueing.
    RefusedSelfCap,
    /// One REST/token leg failed — entry refused fail-closed.
    RefusedDegraded {
        /// Which leg failed.
        stage: DegradedStage,
    },
    /// A broker value was implausible — entry refused fail-closed.
    RefusedImplausible {
        /// What was implausible.
        reason: ImplausibleReason,
    },
}

impl MarginVerdict {
    /// Stable wire-format label for logs/metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExitAlwaysAllowed => "exit_allowed",
            Self::SkippedDisabled => "skipped_disabled",
            Self::Allowed(_) => "allowed",
            Self::RefusedInsufficient(_) => "refused_insufficient",
            Self::RefusedTenantBudget(_) => "refused_tenant_budget",
            Self::RefusedSelfCap => "refused_self_cap",
            Self::RefusedDegraded { .. } => "refused_degraded",
            Self::RefusedImplausible { .. } => "refused_implausible",
        }
    }

    /// Whether a LIVE (`dry_run = false`) consumer may place this entry.
    ///
    /// LIVE consumers MUST use this — a DISABLED gate blocks live entries
    /// fail-closed (`SkippedDisabled` is NOT a live authorization).
    #[must_use]
    pub fn permits_live_entry(&self) -> bool {
        matches!(self, Self::Allowed(_) | Self::ExitAlwaysAllowed)
    }

    /// Whether a PAPER consumer may place this entry.
    ///
    /// Adds `SkippedDisabled` to the live set — a feature-dark gate equals
    /// today's paper behaviour (no margin gating at all).
    #[must_use]
    pub fn permits_paper_entry(&self) -> bool {
        matches!(
            self,
            Self::Allowed(_) | Self::ExitAlwaysAllowed | Self::SkippedDisabled
        )
    }

    /// Maps an authorizing verdict to the [`OrderIntent`] the future
    /// `RiskEngine::check_order` integration consumes. Refusals map to
    /// `None`.
    pub fn to_order_intent(&self) -> Option<OrderIntent> {
        match self {
            Self::Allowed(snapshot) => Some(OrderIntent::Entry {
                required_paise: snapshot.required_paise,
            }),
            Self::ExitAlwaysAllowed => Some(OrderIntent::Exit),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure decision core
// ---------------------------------------------------------------------------

/// Builds the forensic snapshot for a verdict. SPAN/exposure are
/// informational only — lenient conversion (unparsable → 0) never gates.
fn build_snapshot(
    required_paise: i64,
    available_paise: i64,
    usable_paise: i64,
    insufficient_paise: i64,
    margin: &MarginCalculatorResponse,
) -> MarginSnapshot {
    MarginSnapshot {
        required_paise,
        available_paise,
        usable_paise,
        span_paise: to_paise(margin.span_margin).unwrap_or(0),
        exposure_paise: to_paise(margin.exposure_margin).unwrap_or(0),
        insufficient_paise,
    }
}

/// Pure fail-closed entry decision over the two broker responses.
///
/// Shared-account rationale: `insufficientBalance == 0` is NOT
/// authorization on a pooled account — the budget fraction of
/// `availabelBalance` is the only co-tenant-safe primitive. BruteX may
/// still consume margin between our check and our order (irreducible
/// TOCTOU — the check is advisory-best-effort; the broker is the final
/// arbiter).
pub fn decide_entry(
    margin: &MarginCalculatorResponse,
    funds: &FundLimitResponse,
    expected_client_id: &str,
    tenant_budget_percent: u8,
) -> MarginVerdict {
    // (1) Wrong-account response must never authorize spend. An EMPTY
    // client id means the field was absent (serde default) — tolerated.
    if !funds.dhan_client_id.is_empty() && funds.dhan_client_id != expected_client_id {
        return MarginVerdict::RefusedImplausible {
            reason: ImplausibleReason::ClientIdMismatch,
        };
    }

    // (2) Required margin — non-finite/negative/implausible refuses.
    let Some(required) = to_paise(margin.total_margin) else {
        return MarginVerdict::RefusedImplausible {
            reason: ImplausibleReason::NonFiniteOrNegativeMargin,
        };
    };

    // (3) A real F&O entry never costs zero — zero usually means
    // serde-default backfill of an absent/renamed field.
    if required == 0 {
        return MarginVerdict::RefusedImplausible {
            reason: ImplausibleReason::ZeroTotalMargin,
        };
    }

    // (4) Broker-reported shortfall — even the WHOLE pooled account cannot
    // afford it; short-circuits before the budget stage (the snapshot's
    // usable_paise is 0 because the budget was never computed).
    let Some(insufficient) = to_paise(margin.insufficient_balance) else {
        return MarginVerdict::RefusedImplausible {
            reason: ImplausibleReason::NonFiniteInsufficientBalance,
        };
    };
    if insufficient > 0 {
        let available = to_paise_signed(funds.availabel_balance).unwrap_or(0);
        return MarginVerdict::RefusedInsufficient(build_snapshot(
            required,
            available,
            0,
            insufficient,
            margin,
        ));
    }

    // (5) Pooled available balance — SIGNED (debit balances are real).
    let Some(available) = to_paise_signed(funds.availabel_balance) else {
        return MarginVerdict::RefusedImplausible {
            reason: ImplausibleReason::NonFiniteFunds,
        };
    };

    // (6) OUR tenant budget share. i128 intermediate because
    // available * percent can overflow i64 near the plausibility ceiling;
    // try_from is unreachable-but-fail-closed (the product / 100 always
    // fits back into i64 for percent <= 100), so a failure degrades the
    // budget to 0 (refuse) rather than panicking.
    let usable = if available <= 0 {
        0
    } else {
        i64::try_from(i128::from(available) * i128::from(tenant_budget_percent) / 100).unwrap_or(0)
    };

    // (7) The budget verdict.
    let snapshot = build_snapshot(required, available, usable, insufficient, margin);
    if required <= usable {
        MarginVerdict::Allowed(snapshot)
    } else {
        MarginVerdict::RefusedTenantBudget(snapshot)
    }
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// 🔷 DHAN pre-trade margin gate — see the module docs for the full
/// contract (shared account, OFF-switch lattice, consumer rules).
pub struct MarginGate {
    /// Dhan REST client (shared with the OMS).
    api: Arc<OrderApiClient>,
    /// Access-token source (shared with the OMS).
    tokens: Arc<dyn TokenProvider>,
    /// Our own Dhan client id — a fund-limit response naming a different
    /// id is refused as a wrong-account response.
    expected_client_id: String,
    /// `[dhan_margin_gate]` config.
    cfg: DhanMarginGateConfig,
    /// Funds/margin REST self-cap (≤ 10 req/sec — half of Dhan's 20/sec
    /// non-trading budget; the account is shared with the co-tenant).
    self_cap: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
    /// Snapshot of the code-change master lock
    /// [`DHAN_MARGIN_GATE_REST_ALLOWED`] taken at construction — tests
    /// bypass it via `allow_rest_for_test` (cfg(test) only).
    rest_allowed: bool,
}

impl MarginGate {
    /// Builds the gate. Never issues a REST call by itself.
    pub fn new(
        api: Arc<OrderApiClient>,
        tokens: Arc<dyn TokenProvider>,
        expected_client_id: String,
        cfg: DhanMarginGateConfig,
    ) -> Self {
        // Config validation already bounds the cap to 2..=10; the clamp is
        // belt-and-braces so a hand-built config can never widen the quota.
        let cap = cfg.rest_self_cap_per_sec.clamp(2, 10);
        // NonZeroU32::new is Some for cap >= 2; the fallback keeps the
        // constructor unwrap-free (fail-closed to the 2-call minimum).
        let quota = Quota::per_second(NonZeroU32::new(cap).unwrap_or(ENTRY_REST_BURST));
        // Belt-and-braces mirror of the rest_self_cap clamp above:
        // ApplicationConfig::validate already bounds the TOML path to
        // 1..=50, so this only bites a hand-built config — which must
        // never widen the tenant share past half the pooled balance.
        // 0 stays as-is: 0 usable ⇒ refuse everything = fail-closed.
        let mut cfg = cfg;
        cfg.tenant_budget_percent = cfg.tenant_budget_percent.min(50);
        Self {
            api,
            tokens,
            expected_client_id,
            cfg,
            self_cap: RateLimiter::direct(quota),
            rest_allowed: DHAN_MARGIN_GATE_REST_ALLOWED,
        }
    }

    /// Test-only bypass of the code-change master lock, so the REST legs
    /// can be exercised against a local mock. NEVER available in
    /// production builds.
    #[cfg(test)]
    pub(crate) fn allow_rest_for_test(&mut self) {
        self.rest_allowed = true;
    }

    /// Whether the gate's REST legs may fire — the AND-composition of the
    /// config gate and the code-change master lock.
    pub fn is_enabled(&self) -> bool {
        self.cfg.enabled && self.rest_allowed
    }

    /// Records a verdict on the counter and returns it (every gate return
    /// path funnels through here).
    fn record(verdict: MarginVerdict) -> MarginVerdict {
        metrics::counter!("tv_margin_gate_verdicts_total", "verdict" => verdict.as_str())
            .increment(1);
        verdict
    }

    /// Exit check — EXITS ARE NEVER MARGIN-GATED. Structurally REST-free:
    /// touches no token, no REST call, no limiter (an exit must always be
    /// placeable, even with the whole REST surface down).
    pub fn check_exit(&self) -> MarginVerdict {
        Self::record(MarginVerdict::ExitAlwaysAllowed)
    }

    /// Entry check — margin calculator + fund limit + the pure
    /// [`decide_entry`] core. Fails CLOSED for entries on any degrade.
    ///
    /// The caller-built request is validated against the gate's expected
    /// client id BEFORE any permit/token/REST work, so a miswired caller
    /// can never get a verdict computed for the wrong account.
    pub async fn check_entry(&self, request: &MarginCalculatorRequest) -> MarginVerdict {
        // (1) The OFF-switch lattice: config AND the code master lock.
        if !self.is_enabled() {
            return Self::record(MarginVerdict::SkippedDisabled);
        }

        // (2) Request-side client-id validation — STRICT equality: an
        // EMPTY request id also refuses (the request must carry the real
        // client id for the broker anyway). Runs before the self-cap so a
        // miswired caller never burns a REST permit.
        if request.dhan_client_id != self.expected_client_id {
            // The mismatch FACT only — neither id (nor the request) is
            // logged.
            warn!(
                "dhan margin gate: entry request carried a client id different from the \
                 one this gate was built for — refusing fail-closed (a verdict must \
                 never be computed for the wrong account)"
            );
            return Self::record(MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::ClientIdMismatch,
            });
        }

        // (3) Self-cap: one entry check issues two REST calls as one
        // burst. REFUSE (never block/queue) when the cap is hit — a
        // delayed pre-trade verdict is a stale verdict.
        if !self.reserve_entry_permits() {
            warn!(
                "dhan margin gate: funds/margin self-cap hit — refusing entry check \
                 (never queued; a delayed pre-trade verdict is stale)"
            );
            return Self::record(MarginVerdict::RefusedSelfCap);
        }

        // (4) Token — no token means no REST leg can run.
        let token = match self.tokens.get_access_token() {
            Ok(token) => token,
            Err(err) => {
                // A typed error-code variant lands with the first
                // production emit site (the OMS-wiring PR). The error
                // text is bounded + redacted through the house sanitize
                // choke point (uniform with the REST legs below).
                error!(
                    stage = DegradedStage::Token.as_str(),
                    sanitized = %capture_rest_error_body(&err.to_string()),
                    "dhan margin gate: entry check degraded — no access token; refusing \
                     entry fail-closed (exits are unaffected)"
                );
                return Self::record_degraded(DegradedStage::Token);
            }
        };

        // (5) Margin calculator leg.
        let margin = match self
            .api
            .calculate_margin(token.expose_secret(), request)
            .await
        {
            Ok(margin) => margin,
            Err(err) => {
                // The error string can embed the raw broker body —
                // bounded + redacted per the house sanitize choke point.
                error!(
                    stage = DegradedStage::MarginCall.as_str(),
                    sanitized = %capture_rest_error_body(&err.to_string()),
                    "dhan margin gate: entry check degraded — margin calculator call \
                     failed; refusing entry fail-closed (exits are unaffected)"
                );
                return Self::record_degraded(DegradedStage::MarginCall);
            }
        };

        // (6) Fund-limit leg.
        let funds = match self.api.get_fund_limit(token.expose_secret()).await {
            Ok(funds) => funds,
            Err(err) => {
                // The error string can embed the raw broker body —
                // bounded + redacted per the house sanitize choke point.
                error!(
                    stage = DegradedStage::FundLimitCall.as_str(),
                    sanitized = %capture_rest_error_body(&err.to_string()),
                    "dhan margin gate: entry check degraded — fund limit call failed; \
                     refusing entry fail-closed (exits are unaffected)"
                );
                return Self::record_degraded(DegradedStage::FundLimitCall);
            }
        };

        // (7) The pure decision.
        let verdict = decide_entry(
            &margin,
            &funds,
            &self.expected_client_id,
            self.cfg.tenant_budget_percent,
        );
        match &verdict {
            MarginVerdict::Allowed(_) => {}
            MarginVerdict::RefusedInsufficient(snapshot)
            | MarginVerdict::RefusedTenantBudget(snapshot) => {
                warn!(
                    verdict = verdict.as_str(),
                    required_paise = snapshot.required_paise,
                    available_paise = snapshot.available_paise,
                    usable_paise = snapshot.usable_paise,
                    insufficient_paise = snapshot.insufficient_paise,
                    "dhan margin gate: entry refused — the pooled account balance (or \
                     our tenant budget share of it) cannot cover the required margin"
                );
            }
            MarginVerdict::RefusedImplausible { reason } => {
                warn!(
                    verdict = verdict.as_str(),
                    reason = reason.as_str(),
                    "dhan margin gate: entry refused — a broker response value was \
                     implausible; refusing fail-closed rather than trusting it"
                );
            }
            // The remaining variants are produced earlier in this fn, never
            // by decide_entry.
            _ => {}
        }
        Self::record(verdict)
    }

    /// Records a degraded verdict on BOTH counters and returns it.
    fn record_degraded(stage: DegradedStage) -> MarginVerdict {
        metrics::counter!("tv_margin_gate_degraded_total", "stage" => stage.as_str()).increment(1);
        Self::record(MarginVerdict::RefusedDegraded { stage })
    }

    /// Reserves the two-REST-call burst for one entry check against the
    /// funds/margin self-cap. Returns `false` on refusal — BOTH governor
    /// error variants (insufficient capacity right now, and burst larger
    /// than the quota) refuse; the gate never blocks/queues.
    fn reserve_entry_permits(&self) -> bool {
        matches!(self.self_cap.check_n(ENTRY_REST_BURST), Ok(Ok(())))
    }

    /// Test-only view of the permit reservation, so self-cap tests can
    /// drain permits deterministically (back-to-back sync calls) without
    /// HTTP round-trips inside the timing-sensitive window.
    #[cfg(test)]
    pub(crate) fn try_reserve_entry_permits_for_test(&self) -> bool {
        self.reserve_entry_permits()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use reqwest::Client;
    use secrecy::SecretString;

    use super::*;
    use crate::oms::types::OmsError;

    // -- test doubles -------------------------------------------------------

    /// Token provider returning a fixed fake token.
    struct StaticTokens;
    impl TokenProvider for StaticTokens {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            Ok(SecretString::from("fake-token"))
        }
    }

    /// Token provider that PANICS — proves a code path never touches it.
    struct PanickingTokens;
    impl TokenProvider for PanickingTokens {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            panic!("token provider must never be touched on this path")
        }
    }

    /// Token provider that fails — exercises the degraded token arm.
    struct FailingTokens;
    impl TokenProvider for FailingTokens {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            Err(OmsError::NoToken)
        }
    }

    /// Routing mock: serves the margin-calculator and fund-limit routes
    /// with independent status/body pairs, counting every request hit.
    async fn start_routing_mock(
        margin_status: u16,
        margin_body: &str,
        funds_status: u16,
        funds_body: &str,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let margin_body = margin_body.to_string();
        let funds_body = funds_body.to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_inner = Arc::clone(&hits);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                hits_inner.fetch_add(1, Ordering::SeqCst);
                let (status, body) = if request.contains("margincalculator") {
                    (margin_status, margin_body.clone())
                } else if request.contains("fundlimit") {
                    (funds_status, funds_body.clone())
                } else {
                    (404, "{}".to_string())
                };
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

        (base_url, hits, handle)
    }

    /// Builds a gate pointed at `base_url` with the given token double.
    fn make_gate(
        base_url: &str,
        tokens: Arc<dyn TokenProvider>,
        cfg: DhanMarginGateConfig,
    ) -> MarginGate {
        let http = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let api = Arc::new(OrderApiClient::new(
            http,
            base_url.to_owned(),
            "TEST-100".to_owned(),
        ));
        MarginGate::new(api, tokens, "TEST-100".to_owned(), cfg)
    }

    fn enabled_cfg() -> DhanMarginGateConfig {
        DhanMarginGateConfig {
            enabled: true,
            ..DhanMarginGateConfig::default()
        }
    }

    fn make_margin(total_rupees: f64, insufficient_rupees: f64) -> MarginCalculatorResponse {
        MarginCalculatorResponse {
            total_margin: total_rupees,
            span_margin: 0.0,
            exposure_margin: 0.0,
            available_balance: 0.0,
            variable_margin: 0.0,
            insufficient_balance: insufficient_rupees,
            brokerage: 0.0,
            leverage: String::new(),
        }
    }

    fn make_funds(client_id: &str, available_rupees: f64) -> FundLimitResponse {
        FundLimitResponse {
            dhan_client_id: client_id.to_owned(),
            availabel_balance: available_rupees,
            sod_limit: 0.0,
            collateral_amount: 0.0,
            receiveable_amount: 0.0,
            utilized_amount: 0.0,
            blocked_payout_amount: 0.0,
            withdrawable_balance: 0.0,
        }
    }

    fn make_calc_request() -> MarginCalculatorRequest {
        MarginCalculatorRequest {
            dhan_client_id: "TEST-100".to_owned(),
            exchange_segment: "NSE_FNO".to_owned(),
            transaction_type: "BUY".to_owned(),
            quantity: 50,
            product_type: "INTRADAY".to_owned(),
            security_id: "52432".to_owned(),
            price: 245.50,
            trigger_price: 0.0,
        }
    }

    const HAPPY_MARGIN_BODY: &str = r#"{
        "totalMargin": 5000.0,
        "spanMargin": 1200.0,
        "exposureMargin": 1000.0,
        "availableBalance": 100000.0,
        "variableMargin": 0.0,
        "insufficientBalance": 0.0,
        "brokerage": 20.0,
        "leverage": "4.00"
    }"#;

    const HAPPY_FUNDS_BODY: &str = r#"{
        "dhanClientId": "TEST-100",
        "availabelBalance": 100000.0,
        "sodLimit": 113642.0,
        "collateralAmount": 0.0,
        "receiveableAmount": 0.0,
        "utilizedAmount": 0.0,
        "blockedPayoutAmount": 0.0,
        "withdrawableBalance": 98310.0
    }"#;

    /// Margin body with a broker-reported shortfall (₹250 short).
    const INSUFFICIENT_MARGIN_BODY: &str = r#"{
        "totalMargin": 5000.0,
        "spanMargin": 1200.0,
        "exposureMargin": 1000.0,
        "availableBalance": 100000.0,
        "variableMargin": 0.0,
        "insufficientBalance": 250.0,
        "brokerage": 20.0,
        "leverage": "4.00"
    }"#;

    /// Margin body with `totalMargin == 0` — the serde-default-backfill
    /// implausibility class.
    const ZERO_MARGIN_BODY: &str = r#"{
        "totalMargin": 0.0,
        "spanMargin": 0.0,
        "exposureMargin": 0.0,
        "availableBalance": 100000.0,
        "variableMargin": 0.0,
        "insufficientBalance": 0.0,
        "brokerage": 20.0,
        "leverage": "4.00"
    }"#;

    // -- money helpers -------------------------------------------------------

    #[test]
    fn test_to_paise_boundary_zero_negative_nan_inf_and_cap() {
        assert_eq!(to_paise(0.0), Some(0));
        assert_eq!(to_paise(-0.01), None);
        assert_eq!(to_paise(f64::NAN), None);
        assert_eq!(to_paise(f64::INFINITY), None);
        assert_eq!(to_paise(f64::NEG_INFINITY), None);
        // At and beyond the plausibility ceiling.
        assert_eq!(to_paise(1.0e12), Some(100_000_000_000_000));
        assert_eq!(to_paise(1.0e12 + 1.0e6), None);
        // Rounding.
        assert_eq!(to_paise(0.005), Some(1));
        assert_eq!(to_paise(123.456), Some(12_346));
    }

    #[test]
    fn test_to_paise_signed_negative_allowed_boundary() {
        assert_eq!(to_paise_signed(-100.0), Some(-10_000));
        assert_eq!(to_paise_signed(0.0), Some(0));
        assert_eq!(to_paise_signed(-1.0e12), Some(-100_000_000_000_000));
        assert_eq!(to_paise_signed(-1.0e12 - 1.0e6), None);
        assert_eq!(to_paise_signed(f64::NAN), None);
        assert_eq!(to_paise_signed(f64::NEG_INFINITY), None);
    }

    // -- decide_entry --------------------------------------------------------

    #[test]
    fn test_decide_entry_exact_budget_boundary() {
        // available 100,000.00 → usable at 50% = 5,000,000 paise.
        // required exactly 50,000.00 → 5,000,000 paise → Allowed.
        let funds = make_funds("TEST-100", 100_000.0);
        let at_budget = make_margin(50_000.0, 0.0);
        let verdict = decide_entry(&at_budget, &funds, "TEST-100", 50);
        match verdict {
            MarginVerdict::Allowed(snapshot) => {
                assert_eq!(snapshot.required_paise, 5_000_000);
                assert_eq!(snapshot.usable_paise, 5_000_000);
            }
            other => panic!("expected Allowed at exact budget, got {other:?}"),
        }
        // One paise over → RefusedTenantBudget.
        let over_budget = make_margin(50_000.01, 0.0);
        let verdict = decide_entry(&over_budget, &funds, "TEST-100", 50);
        match verdict {
            MarginVerdict::RefusedTenantBudget(snapshot) => {
                assert_eq!(snapshot.required_paise, 5_000_001);
                assert_eq!(snapshot.usable_paise, 5_000_000);
            }
            other => panic!("expected RefusedTenantBudget one paise over, got {other:?}"),
        }
    }

    #[test]
    fn test_decide_entry_zero_total_margin_refused_implausible() {
        let funds = make_funds("TEST-100", 100_000.0);
        let margin = make_margin(0.0, 0.0);
        assert_eq!(
            decide_entry(&margin, &funds, "TEST-100", 50),
            MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::ZeroTotalMargin
            }
        );
    }

    #[test]
    fn test_decide_entry_insufficient_balance_short_circuits() {
        // Broker shortfall > 0 refuses BEFORE the budget stage even though
        // the tenant budget would have covered it.
        let funds = make_funds("TEST-100", 1_000_000.0);
        let margin = make_margin(5_000.0, 250.0);
        match decide_entry(&margin, &funds, "TEST-100", 50) {
            MarginVerdict::RefusedInsufficient(snapshot) => {
                assert_eq!(snapshot.insufficient_paise, 25_000);
                assert_eq!(snapshot.required_paise, 500_000);
                // Budget stage never ran on the short-circuit.
                assert_eq!(snapshot.usable_paise, 0);
            }
            other => panic!("expected RefusedInsufficient, got {other:?}"),
        }
    }

    #[test]
    fn test_decide_entry_negative_available_balance_zero_usable() {
        // A debit pooled balance → usable 0 → any entry refused.
        let funds = make_funds("TEST-100", -100.0);
        let margin = make_margin(1_000.0, 0.0);
        match decide_entry(&margin, &funds, "TEST-100", 50) {
            MarginVerdict::RefusedTenantBudget(snapshot) => {
                assert_eq!(snapshot.available_paise, -10_000);
                assert_eq!(snapshot.usable_paise, 0);
            }
            other => panic!("expected RefusedTenantBudget on debit balance, got {other:?}"),
        }
    }

    #[test]
    fn test_decide_entry_nan_margin_refused_implausible() {
        let funds = make_funds("TEST-100", 100_000.0);
        let margin = make_margin(f64::NAN, 0.0);
        assert_eq!(
            decide_entry(&margin, &funds, "TEST-100", 50),
            MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::NonFiniteOrNegativeMargin
            }
        );
        // A NaN broker SHORTFALL refuses with its OWN distinct reason —
        // triage must know WHICH field was garbage.
        let funds = make_funds("TEST-100", 100_000.0);
        let margin = make_margin(5_000.0, f64::NAN);
        assert_eq!(
            decide_entry(&margin, &funds, "TEST-100", 50),
            MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::NonFiniteInsufficientBalance
            }
        );
        // Non-finite funds refuse too (after a plausible margin).
        let margin = make_margin(5_000.0, 0.0);
        let funds = make_funds("TEST-100", f64::INFINITY);
        assert_eq!(
            decide_entry(&margin, &funds, "TEST-100", 50),
            MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::NonFiniteFunds
            }
        );
    }

    #[test]
    fn test_decide_entry_client_id_mismatch_refused() {
        let margin = make_margin(5_000.0, 0.0);
        let funds = make_funds("OTHER-999", 100_000.0);
        assert_eq!(
            decide_entry(&margin, &funds, "TEST-100", 50),
            MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::ClientIdMismatch
            }
        );
        // An EMPTY client id (field absent under serde default) is
        // tolerated — the check proceeds to the budget verdict.
        let funds = make_funds("", 100_000.0);
        assert!(matches!(
            decide_entry(&margin, &funds, "TEST-100", 50),
            MarginVerdict::Allowed(_)
        ));
    }

    #[test]
    fn test_decide_entry_tenant_budget_percent_bounds() {
        let funds = make_funds("TEST-100", 100_000.0); // 10,000,000 paise pooled
        let margin = make_margin(900.0, 0.0); // 90,000 paise required
        // 1% → usable 100,000 paise → 90,000 fits.
        match decide_entry(&margin, &funds, "TEST-100", 1) {
            MarginVerdict::Allowed(snapshot) => assert_eq!(snapshot.usable_paise, 100_000),
            other => panic!("expected Allowed at 1%, got {other:?}"),
        }
        // 50% → usable 5,000,000 paise.
        match decide_entry(&margin, &funds, "TEST-100", 50) {
            MarginVerdict::Allowed(snapshot) => assert_eq!(snapshot.usable_paise, 5_000_000),
            other => panic!("expected Allowed at 50%, got {other:?}"),
        }
        // 1% with a required over the 1% share refuses.
        let big = make_margin(1_100.0, 0.0); // 110,000 paise > 100,000
        assert!(matches!(
            decide_entry(&big, &funds, "TEST-100", 1),
            MarginVerdict::RefusedTenantBudget(_)
        ));
    }

    // -- gate: exit path -----------------------------------------------------

    #[test]
    fn test_check_exit_never_calls_rest_or_token() {
        // PanickingTokens + an unroutable base URL: if check_exit touched
        // either, this test would panic/fail.
        let gate = make_gate(
            "http://127.0.0.1:1",
            Arc::new(PanickingTokens),
            enabled_cfg(),
        );
        assert_eq!(gate.check_exit(), MarginVerdict::ExitAlwaysAllowed);
    }

    // -- gate: OFF-switch lattice ---------------------------------------------

    #[tokio::test]
    async fn test_check_entry_disabled_makes_zero_rest_calls() {
        let (base_url, hits, handle) =
            start_routing_mock(200, HAPPY_MARGIN_BODY, 200, HAPPY_FUNDS_BODY).await;
        let gate = make_gate(
            &base_url,
            Arc::new(StaticTokens),
            DhanMarginGateConfig::default(), // enabled = false
        );
        let verdict = gate.check_entry(&make_calc_request()).await;
        assert_eq!(verdict, MarginVerdict::SkippedDisabled);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        handle.abort();
    }

    #[tokio::test]
    async fn test_check_entry_config_enabled_but_const_locked_makes_zero_rest_calls() {
        // cfg.enabled = true but NO allow_rest_for_test bypass — the
        // constants.rs master lock alone must keep the REST legs dark.
        let (base_url, hits, handle) =
            start_routing_mock(200, HAPPY_MARGIN_BODY, 200, HAPPY_FUNDS_BODY).await;
        let gate = make_gate(&base_url, Arc::new(StaticTokens), enabled_cfg());
        let verdict = gate.check_entry(&make_calc_request()).await;
        assert_eq!(verdict, MarginVerdict::SkippedDisabled);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        handle.abort();
    }

    // -- gate: REST legs (test-only master-lock bypass) ------------------------

    #[tokio::test]
    async fn test_check_entry_happy_path_allowed() {
        let (base_url, hits, handle) =
            start_routing_mock(200, HAPPY_MARGIN_BODY, 200, HAPPY_FUNDS_BODY).await;
        let mut gate = make_gate(&base_url, Arc::new(StaticTokens), enabled_cfg());
        gate.allow_rest_for_test();
        match gate.check_entry(&make_calc_request()).await {
            MarginVerdict::Allowed(snapshot) => {
                assert_eq!(snapshot.required_paise, 500_000);
                assert_eq!(snapshot.available_paise, 10_000_000);
                assert_eq!(snapshot.usable_paise, 5_000_000);
            }
            other => panic!("expected Allowed, got {other:?}"),
        }
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        handle.abort();
    }

    #[tokio::test]
    async fn test_check_entry_margin_call_5xx_refused_degraded() {
        let (base_url, hits, handle) = start_routing_mock(500, "{}", 200, HAPPY_FUNDS_BODY).await;
        let mut gate = make_gate(&base_url, Arc::new(StaticTokens), enabled_cfg());
        gate.allow_rest_for_test();
        assert_eq!(
            gate.check_entry(&make_calc_request()).await,
            MarginVerdict::RefusedDegraded {
                stage: DegradedStage::MarginCall
            }
        );
        // The margin failure SHORT-CIRCUITS — fundlimit is never called.
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn test_check_entry_fundlimit_malformed_json_refused_degraded() {
        let (base_url, hits, handle) =
            start_routing_mock(200, HAPPY_MARGIN_BODY, 200, "not json at all").await;
        let mut gate = make_gate(&base_url, Arc::new(StaticTokens), enabled_cfg());
        gate.allow_rest_for_test();
        assert_eq!(
            gate.check_entry(&make_calc_request()).await,
            MarginVerdict::RefusedDegraded {
                stage: DegradedStage::FundLimitCall
            }
        );
        // Both routes were reached — the margin leg succeeded first.
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        handle.abort();
    }

    #[tokio::test]
    async fn test_check_entry_token_error_refused_degraded() {
        let (base_url, hits, handle) =
            start_routing_mock(200, HAPPY_MARGIN_BODY, 200, HAPPY_FUNDS_BODY).await;
        let mut gate = make_gate(&base_url, Arc::new(FailingTokens), enabled_cfg());
        gate.allow_rest_for_test();
        assert_eq!(
            gate.check_entry(&make_calc_request()).await,
            MarginVerdict::RefusedDegraded {
                stage: DegradedStage::Token
            }
        );
        // The token failed BEFORE any REST leg.
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        handle.abort();
    }

    #[test]
    fn test_check_entry_self_cap_refuses_not_blocks() {
        // Deterministic permit-drain: NO HTTP inside the timing-sensitive
        // window — two back-to-back SYNC calls (a microsecond apart) on a
        // 2-permit quota. DefaultClock is the house precedent
        // (rate_limiter.rs); the residual flake vector is a >= 1s stall
        // BETWEEN two adjacent sync calls — negligible.
        let cfg = DhanMarginGateConfig {
            enabled: true,
            rest_self_cap_per_sec: 2,
            ..DhanMarginGateConfig::default()
        };
        let gate = make_gate("http://127.0.0.1:1", Arc::new(StaticTokens), cfg);
        // First reservation consumes the whole 2-call burst.
        assert!(gate.try_reserve_entry_permits_for_test());
        // Immediate second reservation is REFUSED (never queued/blocked).
        assert!(!gate.try_reserve_entry_permits_for_test());
    }

    #[tokio::test]
    async fn test_check_entry_self_cap_refused_after_permit_drain_zero_rest_calls() {
        let (base_url, hits, handle) =
            start_routing_mock(200, HAPPY_MARGIN_BODY, 200, HAPPY_FUNDS_BODY).await;
        let cfg = DhanMarginGateConfig {
            enabled: true,
            rest_self_cap_per_sec: 2,
            ..DhanMarginGateConfig::default()
        };
        let mut gate = make_gate(&base_url, Arc::new(StaticTokens), cfg);
        gate.allow_rest_for_test();
        // Pre-drain the whole 2-permit quota via the sync helper (no HTTP).
        assert!(gate.try_reserve_entry_permits_for_test());
        // The drained gate REFUSES the entry check with ZERO REST hits.
        assert_eq!(
            gate.check_entry(&make_calc_request()).await,
            MarginVerdict::RefusedSelfCap
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        handle.abort();
    }

    #[tokio::test]
    async fn test_check_entry_request_client_id_mismatch_refused_zero_rest_calls() {
        let (base_url, hits, handle) =
            start_routing_mock(200, HAPPY_MARGIN_BODY, 200, HAPPY_FUNDS_BODY).await;
        let mut gate = make_gate(&base_url, Arc::new(StaticTokens), enabled_cfg());
        gate.allow_rest_for_test();
        let mut request = make_calc_request();
        request.dhan_client_id = "WRONG".to_owned();
        assert_eq!(
            gate.check_entry(&request).await,
            MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::ClientIdMismatch
            }
        );
        // Refused BEFORE any permit/token/REST work.
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        // Strict equality: an EMPTY request id also refuses (the request
        // must carry the real client id for the broker anyway).
        let mut request = make_calc_request();
        request.dhan_client_id = String::new();
        assert_eq!(
            gate.check_entry(&request).await,
            MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::ClientIdMismatch
            }
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        handle.abort();
    }

    #[tokio::test]
    async fn test_check_entry_insufficient_balance_body_refused() {
        // The broker names a shortfall — refused AFTER both fetches (the
        // pure decision runs over both responses), so hits == 2.
        let (base_url, hits, handle) =
            start_routing_mock(200, INSUFFICIENT_MARGIN_BODY, 200, HAPPY_FUNDS_BODY).await;
        let mut gate = make_gate(&base_url, Arc::new(StaticTokens), enabled_cfg());
        gate.allow_rest_for_test();
        match gate.check_entry(&make_calc_request()).await {
            MarginVerdict::RefusedInsufficient(snapshot) => {
                assert_eq!(snapshot.insufficient_paise, 25_000);
                assert_eq!(snapshot.required_paise, 500_000);
            }
            other => panic!("expected RefusedInsufficient, got {other:?}"),
        }
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        handle.abort();
    }

    #[tokio::test]
    async fn test_check_entry_zero_total_margin_body_refused_implausible() {
        // decide_entry runs after BOTH fetches — hits == 2 even though the
        // zero-margin refusal needs only the margin response.
        let (base_url, hits, handle) =
            start_routing_mock(200, ZERO_MARGIN_BODY, 200, HAPPY_FUNDS_BODY).await;
        let mut gate = make_gate(&base_url, Arc::new(StaticTokens), enabled_cfg());
        gate.allow_rest_for_test();
        assert_eq!(
            gate.check_entry(&make_calc_request()).await,
            MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::ZeroTotalMargin
            }
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        handle.abort();
    }

    #[tokio::test]
    async fn test_margin_gate_new_clamps_tenant_budget_percent_over_50() {
        // A hand-built config past the shared-account ceiling is clamped
        // at construction — the verdict math must use 50%, never 200%.
        let (base_url, _hits, handle) =
            start_routing_mock(200, HAPPY_MARGIN_BODY, 200, HAPPY_FUNDS_BODY).await;
        let cfg = DhanMarginGateConfig {
            enabled: true,
            tenant_budget_percent: 200,
            ..DhanMarginGateConfig::default()
        };
        let mut gate = make_gate(&base_url, Arc::new(StaticTokens), cfg);
        gate.allow_rest_for_test();
        match gate.check_entry(&make_calc_request()).await {
            MarginVerdict::Allowed(snapshot) => {
                // available 100,000.00 → 10,000,000 paise; clamped 50% →
                // usable 5,000,000 paise (200% would read 20,000,000).
                assert_eq!(snapshot.usable_paise, 5_000_000);
            }
            other => panic!("expected Allowed with the clamped budget, got {other:?}"),
        }
        handle.abort();
    }

    // -- verdict surface -------------------------------------------------------

    #[test]
    fn test_margin_verdict_as_str_labels_are_stable() {
        let snapshot = MarginSnapshot {
            required_paise: 1,
            available_paise: 2,
            usable_paise: 1,
            span_paise: 0,
            exposure_paise: 0,
            insufficient_paise: 0,
        };
        assert_eq!(MarginVerdict::ExitAlwaysAllowed.as_str(), "exit_allowed");
        assert_eq!(MarginVerdict::SkippedDisabled.as_str(), "skipped_disabled");
        assert_eq!(MarginVerdict::Allowed(snapshot).as_str(), "allowed");
        assert_eq!(
            MarginVerdict::RefusedInsufficient(snapshot).as_str(),
            "refused_insufficient"
        );
        assert_eq!(
            MarginVerdict::RefusedTenantBudget(snapshot).as_str(),
            "refused_tenant_budget"
        );
        assert_eq!(MarginVerdict::RefusedSelfCap.as_str(), "refused_self_cap");
        assert_eq!(
            MarginVerdict::RefusedDegraded {
                stage: DegradedStage::Token
            }
            .as_str(),
            "refused_degraded"
        );
        assert_eq!(
            MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::ZeroTotalMargin
            }
            .as_str(),
            "refused_implausible"
        );
        // Stage/reason labels.
        assert_eq!(DegradedStage::Token.as_str(), "token");
        assert_eq!(DegradedStage::MarginCall.as_str(), "margin_call");
        assert_eq!(DegradedStage::FundLimitCall.as_str(), "fund_limit_call");
        assert_eq!(
            ImplausibleReason::NonFiniteOrNegativeMargin.as_str(),
            "non_finite_or_negative_margin"
        );
        assert_eq!(
            ImplausibleReason::NonFiniteInsufficientBalance.as_str(),
            "non_finite_insufficient_balance"
        );
        assert_eq!(
            ImplausibleReason::ZeroTotalMargin.as_str(),
            "zero_total_margin"
        );
        assert_eq!(
            ImplausibleReason::NonFiniteFunds.as_str(),
            "non_finite_funds"
        );
        assert_eq!(
            ImplausibleReason::ClientIdMismatch.as_str(),
            "client_id_mismatch"
        );
    }

    #[test]
    fn test_permits_live_entry_truth_table() {
        let snapshot = MarginSnapshot {
            required_paise: 1,
            available_paise: 2,
            usable_paise: 1,
            span_paise: 0,
            exposure_paise: 0,
            insufficient_paise: 0,
        };
        assert!(MarginVerdict::Allowed(snapshot).permits_live_entry());
        assert!(MarginVerdict::ExitAlwaysAllowed.permits_live_entry());
        // A DISABLED gate blocks live entries fail-closed.
        assert!(!MarginVerdict::SkippedDisabled.permits_live_entry());
        assert!(!MarginVerdict::RefusedInsufficient(snapshot).permits_live_entry());
        assert!(!MarginVerdict::RefusedTenantBudget(snapshot).permits_live_entry());
        assert!(!MarginVerdict::RefusedSelfCap.permits_live_entry());
        assert!(
            !MarginVerdict::RefusedDegraded {
                stage: DegradedStage::Token
            }
            .permits_live_entry()
        );
        assert!(
            !MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::NonFiniteFunds
            }
            .permits_live_entry()
        );
    }

    #[test]
    fn test_permits_paper_entry_truth_table() {
        let snapshot = MarginSnapshot {
            required_paise: 1,
            available_paise: 2,
            usable_paise: 1,
            span_paise: 0,
            exposure_paise: 0,
            insufficient_paise: 0,
        };
        assert!(MarginVerdict::Allowed(snapshot).permits_paper_entry());
        assert!(MarginVerdict::ExitAlwaysAllowed.permits_paper_entry());
        // Feature-dark equals today's paper behaviour.
        assert!(MarginVerdict::SkippedDisabled.permits_paper_entry());
        assert!(!MarginVerdict::RefusedInsufficient(snapshot).permits_paper_entry());
        assert!(!MarginVerdict::RefusedTenantBudget(snapshot).permits_paper_entry());
        assert!(!MarginVerdict::RefusedSelfCap.permits_paper_entry());
        assert!(
            !MarginVerdict::RefusedDegraded {
                stage: DegradedStage::MarginCall
            }
            .permits_paper_entry()
        );
        assert!(
            !MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::ClientIdMismatch
            }
            .permits_paper_entry()
        );
    }

    #[test]
    fn test_to_order_intent_mapping_zero_and_negative_refusals() {
        let snapshot = MarginSnapshot {
            required_paise: 500_000,
            available_paise: 10_000_000,
            usable_paise: 5_000_000,
            span_paise: 0,
            exposure_paise: 0,
            insufficient_paise: 0,
        };
        assert_eq!(
            MarginVerdict::Allowed(snapshot).to_order_intent(),
            Some(OrderIntent::Entry {
                required_paise: 500_000
            })
        );
        assert_eq!(
            MarginVerdict::ExitAlwaysAllowed.to_order_intent(),
            Some(OrderIntent::Exit)
        );
        // Every refusal — including the zero-margin and negative/degraded
        // classes — maps to None.
        assert_eq!(MarginVerdict::SkippedDisabled.to_order_intent(), None);
        assert_eq!(
            MarginVerdict::RefusedImplausible {
                reason: ImplausibleReason::ZeroTotalMargin
            }
            .to_order_intent(),
            None
        );
        assert_eq!(
            MarginVerdict::RefusedTenantBudget(snapshot).to_order_intent(),
            None
        );
        assert_eq!(MarginVerdict::RefusedSelfCap.to_order_intent(), None);
    }

    // -- gate construction defaults ---------------------------------------------

    #[test]
    fn test_margin_gate_new_defaults_to_rest_locked() {
        let gate = make_gate(
            "http://127.0.0.1:1",
            Arc::new(StaticTokens),
            DhanMarginGateConfig::default(),
        );
        assert!(!gate.is_enabled());
    }

    #[test]
    fn test_is_enabled_requires_both_config_and_const() {
        // enabled = true but the const master lock holds → disabled.
        let gate = make_gate("http://127.0.0.1:1", Arc::new(StaticTokens), enabled_cfg());
        assert!(!gate.is_enabled());
        // enabled = true + test bypass → enabled.
        let mut gate = make_gate("http://127.0.0.1:1", Arc::new(StaticTokens), enabled_cfg());
        gate.allow_rest_for_test();
        assert!(gate.is_enabled());
        // enabled = false + bypass → still disabled (config half).
        let mut gate = make_gate(
            "http://127.0.0.1:1",
            Arc::new(StaticTokens),
            DhanMarginGateConfig::default(),
        );
        gate.allow_rest_for_test();
        assert!(!gate.is_enabled());
    }
}
