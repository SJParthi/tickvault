//! 🟢 GROWW pre-trade margin surface (§39.3 area slot #4, operator
//! authorization 2026-07-14; area build 2026-07-15).
//!
//! Cold path — the fetcher polls once per minute in-session and the gate is
//! consulted per ORDER SIGNAL / probe evaluation (never per tick);
//! allocations are acceptable here.
//!
//! # The snapshot-poller model (deliberate divergence from the Dhan gate)
//! The 🔷 DHAN margin gate ([`crate::oms::margin_gate`]) issues per-entry
//! REST calls — freshness by construction. THIS gate is SNAPSHOT-CACHED by
//! coordinator/build-lead directive: a 60s in-session poll of the Groww
//! user-margin detail endpoint feeds a RAM snapshot, and the per-order
//! decision NEVER does I/O (one RAM read + one cold-path ledger sum —
//! the RAM-first law). Freshness is enforced by the staleness clock
//! instead: a snapshot older than `stale_secs` (default 180s), absent, or
//! from a previous IST date fails CLOSED for entries. WHY: Groww's
//! calculator shape is Unknown/unverified, the §10.2 grant demands the
//! cold-path scheduled-read discipline, and RAM-first wants a cached
//! decision input.
//!
//! # Shared-account contract
//! The Groww account is POOLED with the BruteX co-tenant and Groww
//! documents NEITHER balance freshness NOR margin-shortfall semantics, so
//! the balance can change inside ANY poll window (irreducible TOCTOU — the
//! check is advisory-best-effort; the broker is the final arbiter). The
//! bounding layers: the `buffer_pct` safety cushion (default 20% — BruteX
//! drain + M2M noise + MARKET-order slippage), the carry-across-swap
//! [`PendingLedger`] (HOLD-EXPIRY-ONLY reservations — same-window
//! multi-approval consumption; NEVER zeroed on a snapshot swap and NEVER
//! released on a balance drop: on a shared account a balance decrement is
//! UNATTRIBUTABLE, so releasing on it under-reserves — over-reservation
//! for ≤ `ledger_hold_secs` is the fail-closed direction), the
//! `floor_paise` min-free floor (default ₹5,000), and the
//! `premium_floor_paise` bad-input floor (a poisoned/zero premium can
//! never neutralize the gate).
//!
//! # The OFF-switch lattice (AND-gated)
//! The REST poll fires only when `[groww_margin_gate] enabled` AND
//! `[groww_orders] margin_read` are BOTH true AND the wall clock is inside
//! the market-hours window — on top of the order-side 4-gate live-fire
//! lattice (this whole subtree compiles only under the non-default
//! `groww_orders` cargo feature; see `oms/groww/mod.rs`). While the gates
//! are off, ZERO HTTP reaches the margin endpoints — ratchet-tested with
//! zero-hit mocks below.
//!
//! # Consumer contract
//! - EXITS ARE NEVER MARGIN-GATED: [`OrderIntent::Exit`] is the only
//!   bypass — structurally I/O-free and snapshot-free (an exit must always
//!   be placeable, even with the whole REST surface down). Mirrors
//!   `MarginGate::check_exit`'s structurally-REST-free discipline.
//! - Entry evaluation fails CLOSED on stale / absent / previous-day /
//!   bad-input data; `enforce = false` labels the refusal
//!   [`GrowwMarginVerdict::WouldReject`] (observe mode) where
//!   `enforce = true` would label it
//!   [`GrowwMarginVerdict::RejectedInsufficient`] — the arithmetic is
//!   IDENTICAL (ratchet-tested label-only difference).
//! - LIVE consumers MUST use [`GrowwMarginVerdict::permits_live_entry`] —
//!   a disabled gate blocks live entries fail-closed. Paper consumers use
//!   [`GrowwMarginVerdict::permits_paper_entry`] (a feature-dark gate
//!   equals today's paper behaviour; an observe-mode would-reject records
//!   while paper proceeds).
//! - The `RiskEngine::check_order` integration (gate #4 via
//!   [`OrderIntent`]) is a deferred wiring slot that rebases after cluster
//!   A's risk-engine round; the app-side poll-loop spawn (BOTH boot arms +
//!   wiring guard), the audit tables (storage sits above trading), the
//!   typed Telegram events, and the per-SID GROWW-MARG-04 episode latch
//!   land with those follow-up PRs. Runbook:
//!   `.claude/rules/project/groww-margin-error-codes.md`.
//!
//! # URL consts live IN THIS FILE (deliberate)
//! The order-side lattice Gate-5 scan bans the margin endpoint path
//! strings outside `oms/groww/` — a deliberate exception to the
//! "constants.rs is the single REST-URL source" convention.
//!
//! # OWNED-FILES-ONLY posture (build-lead scope ruling, 2026-07-15)
//! This area PR ships ONLY this file (+ its `pub mod margin;` line). The
//! shared-file content rides the build lead's central contract-stubs PR:
//!
//! - **Error codes** — the `GROWW-MARG-01..05` `ErrorCode` variants + the
//!   runbook (`.claude/rules/project/groww-margin-error-codes.md`) + the
//!   triage rules land there. Until they land, every emit site below is
//!   CODE-LESS (stage-field only — the `margin_gate.rs` house precedent of
//!   deferring the typed variant) and carries a
//!   `// GROWW-MARG-0N (contract-stubs PR pending)` marker.
//!   WIRING-TODO: at the final rebase AFTER the stubs PR merges, flip each
//!   marked emit to `code = ErrorCode::GrowwMargNN...code_str()`.
//! - **Config** — the `[groww_margin_gate]` section is OPERATOR-HELD with
//!   the stubs PR. Until it lands, the module-local
//!   [`GrowwMarginGateParams`] (same 9 knobs, same bounds, same fail-safe
//!   disabled defaults) is the ONLY config surface; the module compiles
//!   and defaults OFF with no config section anywhere.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Datelike;
use governor::{Quota, RateLimiter, clock::DefaultClock, state::InMemoryState, state::NotKeyed};
use parking_lot::{Mutex, RwLock};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer};
use tickvault_common::broker_order_events::{BrokerOrderEvent, BrokerOrderStatus};
use tickvault_common::constants::{
    GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE, IST_UTC_OFFSET_SECONDS_I64,
};
use tickvault_common::feed::Feed;
use tickvault_common::sanitize::capture_rest_error_body;
use tracing::{debug, error, info};

use crate::oms::engine::TokenProvider;
use crate::oms::margin_gate::{to_paise, to_paise_signed};
use crate::oms::types::OrderIntent;

// ---------------------------------------------------------------------------
// Constants (Gate-5 carve-out: the endpoint URL lives here, NOT constants.rs)
// ---------------------------------------------------------------------------

/// Groww user-margin detail endpoint (read-only GET; the §10.2 KEEP-GATED
/// grant row). Gate-5 sanctioned location: this path string may exist ONLY
/// under `oms/groww/`.
pub const GROWW_USER_MARGIN_URL: &str = "https://api.groww.in/v1/margins/detail/user"; // APPROVED: Gate-5 carve-out — the margin endpoint literal lives ONLY under oms/groww/ (lattice-guard pinned)

/// Response body size cap (256 KiB) — the user-margin payload is ~1 KB;
/// anything larger is corrupt/hostile and is refused before parse.
pub const MARGIN_MAX_BODY_BYTES: u64 = 262_144;

/// Per-request connect + read timeout (seconds) for the hardened client.
pub const MARGIN_HTTP_TIMEOUT_SECS: u64 = 5;

/// Whole-poll bound (seconds): token read + GET + retry must finish inside
/// this window or the poll classifies `timeout`.
pub const MARGIN_POLL_OVERALL_TIMEOUT_SECS: u64 = 15;

/// Delay before the single bounded transport-class retry (seconds).
pub const MARGIN_TRANSPORT_RETRY_DELAY_SECS: u64 = 5;

/// Carry-across-swap ledger capacity — an approval burst past this is
/// itself a fault (fail-closed reject at the caller).
pub const MARGIN_LEDGER_CAP: usize = 64;

/// Consecutive fully-failed polls that fire the GROWW-MARG-01
/// `stage="escalation"` edge (once per episode).
pub const MARGIN_CONSECUTIVE_FAILURE_ESCALATION: u32 = 3;

/// Defensive governor belt: at most one fetch per this many seconds (the
/// 60s interval IS the limiter; the belt is the floor under a bugged loop).
pub const MARGIN_POLL_BELT_PERIOD_SECS: u64 = 30;

/// Balance sanity ceiling: ₹10 crore in integer paise. A user-margin
/// balance above this is corrupt, not a real retail account value
/// (stricter than the generic `to_paise` ₹1e12 plausibility cap).
pub const MARGIN_MAX_PLAUSIBLE_BALANCE_PAISE: i64 = 10_000_000_000;

/// Poll window open (IST seconds-of-day): 09:00 IST — the §10.2
/// market-hours-only discipline with the pre-open prime margin.
pub const MARGIN_POLL_WINDOW_START_IST_SECS: u32 = 9 * 3600;

/// Poll window close (IST seconds-of-day, exclusive): 15:35 IST — covers
/// the 15:30 close + the post-close settle reads.
pub const MARGIN_POLL_WINDOW_END_IST_SECS: u32 = 15 * 3600 + 35 * 60;

/// IST offset as unsigned seconds for epoch arithmetic.
// The i64 constant is a positive 19,800; the cast cannot lose value.
#[allow(clippy::cast_sign_loss)] // APPROVED: positive constant, compile-time
const IST_OFFSET_SECS_U64: u64 = IST_UTC_OFFSET_SECONDS_I64 as u64;

// ---------------------------------------------------------------------------
// Gate params (module-local — the OPERATOR-HELD config surface for now)
// ---------------------------------------------------------------------------

/// 🟢 GROWW margin gate tuning knobs — the module-local mirror of the
/// operator-held `[groww_margin_gate]` config section (which rides the
/// central contract-stubs PR; see the module header). Same 9 knobs, same
/// documented bounds, same fail-safe defaults: `Default` is fully
/// DISABLED (`enabled = false`, `enforce = false`) with the safe tuning
/// values, so the module is dark until the deferred wiring constructs an
/// enabled instance from real config.
///
/// `enforce` gates verdict LABELING only (`RejectedInsufficient` vs
/// `WouldReject` — the pure [`evaluate`] arithmetic is identical,
/// ratchet-pinned below).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowwMarginGateParams {
    /// Master gate for the margin surface (poll + evaluation). Default
    /// FALSE. Even when true, the REST poll additionally requires
    /// `[groww_orders] margin_read` (AND-gated — neither flag alone opens
    /// the REST leg).
    pub enabled: bool,
    /// Enforce mode: `true` labels an insufficient-funds verdict
    /// `RejectedInsufficient`; `false` (default) labels it `WouldReject`
    /// — observe-only. `enforce = true` with `enabled = false` is refused
    /// by [`Self::validate`] (inconsistent — combos #2/#6 of the 8-combo
    /// boot-guard matrix).
    pub enforce: bool,
    /// Snapshot poll cadence (seconds). Default 60; bounds [30, 300].
    pub poll_secs: u64,
    /// Snapshot age beyond which the entry gate fails CLOSED. Default 180
    /// (= 3 missed polls); bounds [2 × poll_secs, 900].
    pub stale_secs: u64,
    /// Balance-side safety buffer percent — covers BruteX co-tenant drain
    /// + mark-to-market noise + MARKET-order slippage across the snapshot
    /// window. Default 20; bounds [0, 90].
    pub buffer_pct: u8,
    /// Minimum free balance that must REMAIN after an accepted entry, in
    /// integer PAISE. Default 500_000 (₹5,000); bounds >= 0.
    pub floor_paise: i64,
    /// Carry-across-swap ledger hold (seconds): an approved entry
    /// reserves its required amount until HOLD-EXPIRY (or an id-matched
    /// fill release) — a balance drop never releases (unattributable on
    /// the shared account; over-reservation for the hold window is the
    /// fail-closed direction). Default 120 (= 2 polls); bounds
    /// [poll_secs, 600].
    pub ledger_hold_secs: u64,
    /// Bad-input floor in integer PAISE: an entry whose required amount
    /// is below this is `FailClosedBadInput`, never a Pass. Default 500
    /// paise (₹5) — a REAL index-option BUY entry (premium × lot size ×
    /// lots) is never cheaper than ₹5, so anything below it indicates a
    /// poisoned/zero premium input that must not neutralize the gate.
    /// Bounds > 0.
    pub premium_floor_paise: i64,
    /// Dated operator waiver for running LIVE without the margin gate
    /// (the 8-combo boot-guard matrix, combos #5/#7). Default EMPTY = no
    /// waiver. When set, it MUST carry the dated operator quote; a waived
    /// boot is loud (HIGH page at the wiring site). Never required for
    /// paper mode.
    pub live_without_margin_gate_operator_ack: String,
}

impl Default for GrowwMarginGateParams {
    /// Fail-safe: fully disabled with the safe tuning defaults (the
    /// `DhanMarginGateConfig` default discipline — never zeroed knobs).
    fn default() -> Self {
        Self {
            enabled: false,
            enforce: false,
            poll_secs: 60,
            stale_secs: 180,
            buffer_pct: 20,
            floor_paise: 500_000,
            ledger_hold_secs: 120,
            premium_floor_paise: 500,
            live_without_margin_gate_operator_ack: String::new(),
        }
    }
}

impl GrowwMarginGateParams {
    /// Boot-time bounds validation — a mis-set knob must never invert the
    /// fail-closed gate into fail-open. Violation refuses (the wiring
    /// caller refuses boot) naming the exact key (the
    /// `DhanMarginGateConfig::validate` precedent).
    ///
    /// # Errors
    /// Returns a descriptive message when any knob is outside its
    /// documented range (see each field's doc-comment), or when
    /// `enforce = true` is combined with `enabled = false` (the
    /// unconditionally-inconsistent combos #2/#6 of the boot-guard
    /// matrix — enforcement without a running gate protects nothing).
    // WIRING-EXEMPT: caller is the deferred app-side config wiring slot
    pub fn validate(&self) -> Result<(), String> {
        if self.enforce && !self.enabled {
            return Err(
                "groww_margin_gate.enforce = true requires groww_margin_gate.enabled = true — \
                 enforcement without a running margin gate is inconsistent (boot-guard \
                 combos #2/#6)"
                    .to_owned(),
            );
        }
        if !(30..=300).contains(&self.poll_secs) {
            return Err(format!(
                "groww_margin_gate.poll_secs ({}) must be within 30..=300",
                self.poll_secs
            ));
        }
        let min_stale = self.poll_secs.saturating_mul(2);
        if !(min_stale..=900).contains(&self.stale_secs) {
            return Err(format!(
                "groww_margin_gate.stale_secs ({}) must be within 2 x poll_secs ({})..=900 — \
                 a stale window shorter than two polls would flap the gate closed on a \
                 single missed poll",
                self.stale_secs, min_stale
            ));
        }
        if self.buffer_pct > 90 {
            return Err(format!(
                "groww_margin_gate.buffer_pct ({}) must be within 0..=90 — beyond 90 the \
                 usable balance degenerates to noise",
                self.buffer_pct
            ));
        }
        if self.floor_paise < 0 {
            return Err(format!(
                "groww_margin_gate.floor_paise ({}) must be >= 0 — a negative min-free \
                 floor would invert fail-closed into fail-open",
                self.floor_paise
            ));
        }
        if !(self.poll_secs..=600).contains(&self.ledger_hold_secs) {
            return Err(format!(
                "groww_margin_gate.ledger_hold_secs ({}) must be within \
                 poll_secs ({})..=600 — a hold shorter than one poll voids the \
                 carry-across-swap protection",
                self.ledger_hold_secs, self.poll_secs
            ));
        }
        if self.premium_floor_paise <= 0 {
            return Err(format!(
                "groww_margin_gate.premium_floor_paise ({}) must be > 0 — the bad-input \
                 floor is load-bearing (a zero floor lets a poisoned premium pass)",
                self.premium_floor_paise
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Time helpers (pure — injected-clock testable)
// ---------------------------------------------------------------------------

/// IST calendar date (`YYYYMMDD` as u32) for a UTC epoch instant.
///
/// Fail-closed: an unrepresentable instant returns 0, which never equals a
/// real trading date — downstream staleness checks then refuse (never
/// silently pass).
// WIRING-EXEMPT: consumed by run_margin_poll_loop + the deferred app wiring
pub fn ist_date_from_epoch_secs(epoch_secs: u64) -> u32 {
    let Ok(secs) = i64::try_from(epoch_secs) else {
        return 0;
    };
    let Some(utc) = chrono::DateTime::from_timestamp(secs, 0) else {
        return 0;
    };
    let ist = utc + chrono::TimeDelta::seconds(IST_UTC_OFFSET_SECONDS_I64);
    let date = ist.date_naive();
    let Ok(year) = u32::try_from(date.year()) else {
        return 0;
    };
    year * 10_000 + date.month() * 100 + date.day()
}

/// IST seconds-of-day for a UTC epoch instant (pure; for the app-side
/// market-hours predicate composition).
// WIRING-EXEMPT: helper for the deferred app-side in_market_hours predicate
pub fn ist_secs_of_day_from_epoch_secs(epoch_secs: u64) -> u32 {
    u32::try_from(epoch_secs.saturating_add(IST_OFFSET_SECS_U64) % 86_400).unwrap_or(0)
}

/// Pure poll-window predicate over IST seconds-of-day: `[09:00, 15:35)`.
// WIRING-EXEMPT: composed with the trading-day check by the app wiring
pub fn poll_window_accepts(ist_secs_of_day: u32) -> bool {
    (MARGIN_POLL_WINDOW_START_IST_SECS..MARGIN_POLL_WINDOW_END_IST_SECS).contains(&ist_secs_of_day)
}

// ---------------------------------------------------------------------------
// Wire types (every field Option + serde default; lenient string-numbers)
// ---------------------------------------------------------------------------

/// Lenient number deserializer: accepts a JSON number, a string-wrapped
/// number (`"5000.25"`), or null/absent. A NON-numeric string or a
/// non-finite value fails the WHOLE deserialize — type drift converges to
/// the `parse` fail-closed class (the SEC-L2 mechanics), never a silent 0.
fn de_opt_lenient_f64<'de, D>(de: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Num(f64),
        Text(String),
    }
    let raw = Option::<Raw>::deserialize(de)?;
    let value = match raw {
        None => return Ok(None),
        Some(Raw::Num(value)) => value,
        Some(Raw::Text(text)) => text.parse::<f64>().map_err(|_| {
            // Bounded echo — chars (not bytes) so the cut is UTF-8-safe.
            let echoed: String = text.chars().take(32).collect();
            serde::de::Error::custom(format!(
                "non-numeric margin value {echoed:?} — refusing to coerce"
            ))
        })?,
    };
    if !value.is_finite() {
        return Err(serde::de::Error::custom(
            "non-finite margin value — refusing to coerce",
        ));
    }
    Ok(Some(value))
}

/// GA-style failure body of the Groww envelope. NEVER logged verbatim —
/// only through the [`capture_rest_error_body`] sanitize choke point.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GrowwApiError {
    /// Broker error code (GA-class), when present.
    #[serde(default)]
    pub code: Option<String>,
    /// Broker error message, when present.
    #[serde(default)]
    pub message: Option<String>,
}

/// F&O margin sub-object of the user-margin payload. Nesting placement is
/// ASSUMED (the live wire example carried placeholders) — a wrong
/// assumption surfaces as `shape_incomplete`, fail-closed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FnoMarginDetails {
    /// THE decision field — funds available for option BUY entries.
    #[serde(default, deserialize_with = "de_opt_lenient_f64")]
    pub option_buy_balance_available: Option<f64>,
}

/// The user-margin payload (decision + observability subset — the other
/// broker fields are deliberately not modeled; unknown keys are ignored).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserMarginPayload {
    /// Nested F&O margin details (carries the decision field).
    #[serde(default)]
    pub fno_margin_details: Option<FnoMarginDetails>,
    /// Observability only — clear cash balance (never gates).
    #[serde(default, deserialize_with = "de_opt_lenient_f64")]
    pub clear_cash: Option<f64>,
    /// Observability only — net margin used (never gates).
    #[serde(default, deserialize_with = "de_opt_lenient_f64")]
    pub net_margin_used: Option<f64>,
}

/// Groww response envelope: `{"status":"SUCCESS","payload":{...}}` or a
/// failure with a GA error body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserMarginEnvelope {
    /// `SUCCESS` / `FAILURE` (anything but SUCCESS refuses).
    #[serde(default)]
    pub status: Option<String>,
    /// The payload on success.
    #[serde(default)]
    pub payload: Option<UserMarginPayload>,
    /// The GA error body on failure.
    #[serde(default)]
    pub error: Option<GrowwApiError>,
}

// ---------------------------------------------------------------------------
// Snapshot + cell
// ---------------------------------------------------------------------------

/// The validated RAM decision input (integer paise). Name deliberately
/// distinct from the Dhan `MarginSnapshot` re-exported by `oms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrowwMarginSnapshot {
    /// When the response LANDED (epoch secs, stamped post-await) — OUR
    /// staleness clock.
    pub fetched_at_epoch_secs: u64,
    /// When the fetch was INITIATED (epoch secs, pre-fetch) —
    /// DIAGNOSTICS ONLY (fetch latency = landed − initiated). NOT
    /// consumed by any decision path: the ledger is hold-expiry-only, so
    /// the balance-reflection math this field was designed for no longer
    /// exists.
    pub fetch_initiated_at_epoch_secs: u64,
    /// IST calendar date (`YYYYMMDD`) the snapshot was fetched on —
    /// previous-date snapshots are stale regardless of age.
    pub fetched_on_ist_date: u32,
    /// THE decision field, integer paise.
    pub option_buy_avail_paise: i64,
    /// Observability only (never gates).
    pub clear_cash_paise: Option<i64>,
    /// Observability only (never gates).
    pub net_margin_used_paise: Option<i64>,
}

/// The single-value snapshot holder.
///
/// DELTA from the design's ArcSwap: arc-swap is not in this crate's
/// dependency set, and evaluations are per-signal/per-probe (never
/// per-tick), so an uncontended `parking_lot::RwLock` is O(1)-enough on
/// this cold path and keeps the PR inside one file. The tick hot path
/// never touches this cell.
#[derive(Debug, Default)]
pub struct MarginSnapshotCell(RwLock<Option<Arc<GrowwMarginSnapshot>>>);

impl MarginSnapshotCell {
    /// Empty cell (no snapshot yet — the gate fails closed for entries).
    pub fn new() -> Self {
        Self(RwLock::new(None))
    }

    /// Current snapshot, if any.
    pub fn load(&self) -> Option<Arc<GrowwMarginSnapshot>> {
        self.0.read().clone()
    }

    /// Swaps in a fresh snapshot (never clears — an invalid poll simply
    /// does not store, so the prior snapshot ages toward the stale edge).
    pub fn store(&self, snapshot: GrowwMarginSnapshot) {
        *self.0.write() = Some(Arc::new(snapshot));
    }
}

// ---------------------------------------------------------------------------
// Validator
// ---------------------------------------------------------------------------

/// Why a 2xx payload was refused (fail-closed; the snapshot is NOT stored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeIssue {
    /// The decision field is absent (nesting drift / entitlement shape).
    MissingDecisionField,
    /// A sanity clamp fired — the payload named WHICH clamp.
    Implausible(&'static str),
}

impl ShapeIssue {
    /// GROWW-MARG-01 `stage` label for this refusal class.
    pub fn stage(&self) -> &'static str {
        match self {
            Self::MissingDecisionField => "shape_incomplete",
            Self::Implausible(_) => "sanity",
        }
    }

    /// The named clamp (field-NAME class only — never a balance value).
    pub fn detail(&self) -> &'static str {
        match self {
            Self::MissingDecisionField => "option_buy_balance_available_missing",
            Self::Implausible(which) => which,
        }
    }
}

/// Validates a parsed payload into a snapshot.
///
/// Decision field required + sanity-clamped (non-negative, finite, at most
/// the ₹10-crore ceiling — violation refuses, fail-closed). The
/// observability fields are lenient: an unparsable value degrades to
/// `None`, never gates.
pub fn validate_payload(
    payload: &UserMarginPayload,
    fetch_initiated_at_epoch_secs: u64,
    fetched_at_epoch_secs: u64,
    fetched_on_ist_date: u32,
) -> Result<GrowwMarginSnapshot, ShapeIssue> {
    let Some(decision_rupees) = payload
        .fno_margin_details
        .as_ref()
        .and_then(|fno| fno.option_buy_balance_available)
    else {
        return Err(ShapeIssue::MissingDecisionField);
    };
    // to_paise refuses non-finite / negative / > ₹1e12 (fail-closed None)
    // — the label names all three refusal classes (a 1e308-class huge
    // number lands here, not under the ₹10-crore ceiling below).
    let Some(option_buy_avail_paise) = to_paise(decision_rupees) else {
        return Err(ShapeIssue::Implausible(
            "non_finite_negative_or_implausible_balance",
        ));
    };
    if option_buy_avail_paise > MARGIN_MAX_PLAUSIBLE_BALANCE_PAISE {
        return Err(ShapeIssue::Implausible("above_ten_crore_ceiling"));
    }
    Ok(GrowwMarginSnapshot {
        fetched_at_epoch_secs,
        fetch_initiated_at_epoch_secs,
        fetched_on_ist_date,
        option_buy_avail_paise,
        clear_cash_paise: payload.clear_cash.and_then(to_paise_signed),
        net_margin_used_paise: payload.net_margin_used.and_then(to_paise_signed),
    })
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// The paise-integer numbers behind an entry verdict (forensic payload for
/// the future `margin_gate_audit` row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvalNumbers {
    /// Margin the caller computed for this entry.
    pub required_paise: i64,
    /// The snapshot's option-buy available balance.
    pub avail_paise: i64,
    /// Post-buffer usable balance (`avail × (100 − buffer_pct) / 100`).
    pub usable_paise: i64,
    /// Carry-across-swap ledger consumption at decision time.
    pub pending_consumed_paise: i64,
    /// `usable − required − pending` (negative on a refusal).
    pub headroom_paise: i64,
    /// Snapshot age at decision time (seconds).
    pub snapshot_age_secs: u64,
}

/// The Groww margin gate's verdict for one order intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrowwMarginVerdict {
    /// The intent is an exit — NEVER margin-gated, always allowed.
    ExitAlwaysAllowed,
    /// The gate is disabled (`[groww_margin_gate] enabled = false`) — no
    /// snapshot was consulted. Paper entries proceed (today's behaviour);
    /// LIVE entries are BLOCKED fail-closed by
    /// [`GrowwMarginVerdict::permits_live_entry`].
    SkippedDisabled,
    /// Entry affordable inside the buffered balance + ledger + floor.
    Pass(EvalNumbers),
    /// OBSERVE mode (`enforce = false`): the arithmetic would reject —
    /// recorded, never blocking paper flow.
    WouldReject(EvalNumbers),
    /// ENFORCE mode: entry rejected — insufficient usable funds
    /// (GROWW-MARG-04).
    RejectedInsufficient(EvalNumbers),
    /// Snapshot too old / previous IST date — entry refused fail-closed.
    FailClosedStale {
        /// Snapshot age at decision time (seconds).
        age_secs: u64,
    },
    /// No snapshot has ever landed — entry refused fail-closed.
    FailClosedNoSnapshot,
    /// The CALLER's required amount was invalid (zero / negative / below
    /// the premium floor) — a poisoned input must never Pass.
    FailClosedBadInput {
        /// Which bad-input class fired.
        reason: &'static str,
    },
}

impl GrowwMarginVerdict {
    /// Stable wire-format label for logs/metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExitAlwaysAllowed => "exit_allowed",
            Self::SkippedDisabled => "skipped_disabled",
            Self::Pass(_) => "pass",
            Self::WouldReject(_) => "would_reject",
            Self::RejectedInsufficient(_) => "rejected_insufficient",
            Self::FailClosedStale { .. } => "fail_closed_stale",
            Self::FailClosedNoSnapshot => "fail_closed_no_snapshot",
            Self::FailClosedBadInput { .. } => "fail_closed_bad_input",
        }
    }

    /// Whether a LIVE (`dry_run = false`) consumer may place this entry.
    ///
    /// ONLY `Pass` and `ExitAlwaysAllowed` — a DISABLED gate, an
    /// observe-mode would-reject, and every fail-closed arm ALL block live
    /// (the margin_gate `permits_live_entry` discipline).
    #[must_use]
    // WIRING-EXEMPT: consumed by the deferred RiskEngine check_order wiring
    pub fn permits_live_entry(&self) -> bool {
        matches!(self, Self::Pass(_) | Self::ExitAlwaysAllowed)
    }

    /// Whether a PAPER consumer may place this entry.
    ///
    /// Adds `SkippedDisabled` (a feature-dark gate equals today's paper
    /// behaviour) and `WouldReject` (observe-mode paper proceeds while the
    /// would-reject is recorded) to the live set.
    #[must_use]
    // WIRING-EXEMPT: consumed by the deferred RiskEngine check_order wiring
    pub fn permits_paper_entry(&self) -> bool {
        matches!(
            self,
            Self::Pass(_) | Self::ExitAlwaysAllowed | Self::SkippedDisabled | Self::WouldReject(_)
        )
    }
}

// ---------------------------------------------------------------------------
// Pure decision core
// ---------------------------------------------------------------------------

/// Post-buffer usable balance: `max(avail, 0) × (100 − min(buffer, 100))
/// / 100` in i128 intermediates — the ONE buffer formula, shared by
/// [`evaluate`] and the poll-time headroom gauge (no divergence).
fn usable_paise_after_buffer(avail_paise: i64, buffer_pct: u8) -> i64 {
    let buffer = i128::from(buffer_pct.min(100));
    let usable = i128::from(avail_paise.max(0)) * (100 - buffer) / 100;
    i64::try_from(usable).unwrap_or(0)
}

/// Non-sensitive relative funding signal (the SEC-GM-1 replacement for
/// the retired raw-balance gauge): the buffered USABLE balance as a
/// percentage of the min-free floor, CLAMPED to `[0, 100]` — deliberately
/// SATURATING, so the shipped series can never be inverted back to the
/// account balance once usable ≥ floor. 100 = the entry gate is fundable
/// above the floor; < 100 = entries are floor-blocked.
fn compute_headroom_pct(usable_paise: i64, floor_paise: i64) -> f64 {
    let floor = i128::from(floor_paise.max(1));
    let pct = (i128::from(usable_paise.max(0)) * 100 / floor).clamp(0, 100);
    // Clamped to 0..=100 — the f64 conversion is exact.
    #[allow(clippy::cast_precision_loss)] // APPROVED: clamped 0..=100
    {
        pct as f64
    }
}

/// The PURE gate decision — mode-agnostic, shared VERBATIM by observe /
/// enforce / reference-probe / selftest callers (the observe/enforce split
/// is a LABEL, ratchet-tested identical arithmetic).
///
/// Decision order: exit bypass → disabled → no snapshot → staleness
/// (previous IST date OR `age > stale_secs`; age EXACTLY at the threshold
/// still evaluates) → bad input (non-positive or below
/// `premium_floor_paise`) → the buffered-balance arithmetic:
///
/// ```text
/// usable  = avail × (100 − buffer_pct) / 100          (i128 intermediates)
/// accept iff required + pending ≤ usable
///        AND usable − required − pending ≥ floor_paise
/// ```
pub fn evaluate(
    snapshot: Option<&GrowwMarginSnapshot>,
    intent: OrderIntent,
    pending_consumed_paise: i64,
    cfg: &GrowwMarginGateParams,
    now_epoch_secs: u64,
    today_ist: u32,
) -> GrowwMarginVerdict {
    // (1) EXITS ARE NEVER MARGIN-GATED — the only bypass, even when the
    // gate is disabled or the snapshot is stale/absent.
    let required = match intent {
        OrderIntent::Exit => return GrowwMarginVerdict::ExitAlwaysAllowed,
        OrderIntent::Entry { required_paise } => required_paise,
    };

    // (2) Disabled gate: no snapshot machinery runs (paper proceeds; live
    // is blocked by permits_live_entry — never a silent live pass).
    if !cfg.enabled {
        return GrowwMarginVerdict::SkippedDisabled;
    }

    // (3) Fail-closed on absent data.
    let Some(snap) = snapshot else {
        return GrowwMarginVerdict::FailClosedNoSnapshot;
    };

    // (4) Staleness: previous IST date is stale regardless of age; age
    // EXACTLY at the threshold still evaluates (boundary inclusive).
    let age_secs = now_epoch_secs.saturating_sub(snap.fetched_at_epoch_secs);
    if snap.fetched_on_ist_date != today_ist || age_secs > cfg.stale_secs {
        return GrowwMarginVerdict::FailClosedStale { age_secs };
    }

    // (5) Bad-input floor: a zero/negative/below-floor required amount is
    // a caller-data fault — never a Pass, never dressed as insufficiency.
    if required <= 0 {
        return GrowwMarginVerdict::FailClosedBadInput {
            reason: "non_positive_required",
        };
    }
    if required < cfg.premium_floor_paise {
        return GrowwMarginVerdict::FailClosedBadInput {
            reason: "below_premium_floor",
        };
    }

    // (6) Buffered-balance arithmetic in i128 (no overflow near the
    // plausibility ceiling). A negative pending sum is clamped to 0
    // (defensive — the ledger never produces one).
    let avail = snap.option_buy_avail_paise;
    let usable = usable_paise_after_buffer(avail, cfg.buffer_pct);
    let pending = pending_consumed_paise.max(0);
    let need = i128::from(required) + i128::from(pending);
    let headroom = i128::from(usable) - need;
    let numbers = EvalNumbers {
        required_paise: required,
        avail_paise: avail,
        usable_paise: usable,
        pending_consumed_paise: pending,
        headroom_paise: i64::try_from(headroom).unwrap_or(i64::MIN),
        snapshot_age_secs: age_secs,
    };

    // (7) The layered acceptance condition.
    if need <= i128::from(usable) && headroom >= i128::from(cfg.floor_paise) {
        GrowwMarginVerdict::Pass(numbers)
    } else if cfg.enforce {
        GrowwMarginVerdict::RejectedInsufficient(numbers)
    } else {
        GrowwMarginVerdict::WouldReject(numbers)
    }
}

/// [`evaluate`] + the observability funnel: verdict counter (labelled by
/// `source` ∈ `signal` / `reference_probe` / `selftest`) + the
/// GROWW-MARG-04 emission on an enforce-mode rejection.
///
/// Delivery boundary (honest): the design's per-SID episode latch lands
/// with the RiskEngine wiring PR (which has the SID in hand) — until then
/// this emits once per enforce-mode rejection, bounded by the evaluation
/// cadence itself (per signal/probe, never per tick).
#[allow(clippy::too_many_arguments)] // APPROVED: cold-path funnel mirrors evaluate + source
// WIRING-EXEMPT: consumed by the deferred RiskEngine / probe / selftest wiring
pub fn evaluate_and_record(
    snapshot: Option<&GrowwMarginSnapshot>,
    intent: OrderIntent,
    pending_consumed_paise: i64,
    cfg: &GrowwMarginGateParams,
    now_epoch_secs: u64,
    today_ist: u32,
    source: &'static str,
) -> GrowwMarginVerdict {
    let verdict = evaluate(
        snapshot,
        intent,
        pending_consumed_paise,
        cfg,
        now_epoch_secs,
        today_ist,
    );
    metrics::counter!(
        "tv_groww_margin_gate_verdicts_total",
        "verdict" => verdict.as_str(),
        "source" => source,
    )
    .increment(1);
    if let GrowwMarginVerdict::RejectedInsufficient(numbers) = &verdict {
        // The full paise arithmetic is DELIBERATE on this line (recorded
        // decision — see the runbook §4): the operator needs the numbers
        // to act; the SEC-M1 field-names-only redaction applies to the
        // fetch-side lines, not this forensic verdict.
        // GROWW-MARG-04 (contract-stubs PR pending)
        error!(
            source,
            required_paise = numbers.required_paise,
            avail_paise = numbers.avail_paise,
            usable_paise = numbers.usable_paise,
            pending_consumed_paise = numbers.pending_consumed_paise,
            headroom_paise = numbers.headroom_paise,
            snapshot_age_secs = numbers.snapshot_age_secs,
            "groww margin gate: entry REJECTED — insufficient usable funds after \
             the safety buffer + pending ledger + min-free floor on the shared \
             account (exits are unaffected; the broker remains the final arbiter)"
        );
    }
    verdict
}

// ---------------------------------------------------------------------------
// 8-combo boot guard (pure; wiring = the shared config-validation slot)
// ---------------------------------------------------------------------------

/// Boot-guard verdict for the live/paper × enabled × enforce matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootGuardVerdict {
    /// The combination is consistent — boot proceeds.
    Allowed,
    /// LIVE without the gate, waived by the dated
    /// `live_without_margin_gate_operator_ack` key — boot proceeds but the
    /// caller MUST page loudly (HIGH) naming the waiver.
    AllowedWithWaiver,
    /// The combination is refused — the payload names the exact rule.
    Refused(&'static str),
}

/// The full 8-combination boot-guard matrix (design §4.1):
///
/// | # | mode  | enabled | enforce | verdict |
/// |---|-------|---------|---------|---------|
/// | 1 | paper | false   | false   | Allowed |
/// | 2 | paper | false   | true    | Refused (inconsistent, no waiver) |
/// | 3 | paper | true    | false   | Allowed (observe) |
/// | 4 | paper | true    | true    | Allowed (enforce against paper) |
/// | 5 | live  | false   | false   | Refused unless the dated ack key |
/// | 6 | live  | false   | true    | Refused (inconsistent, no waiver) |
/// | 7 | live  | true    | false   | Refused unless the dated ack key |
/// | 8 | live  | true    | true    | Allowed |
///
/// `effectively_live` is supplied by the caller — the shared
/// `is_effectively_live()` helper does not exist on main yet, so this
/// stays a pure validated function until the wiring PR builds it.
// WIRING-EXEMPT: caller is the deferred shared config-validation wiring slot
pub fn boot_guard_verdict(effectively_live: bool, cfg: &GrowwMarginGateParams) -> BootGuardVerdict {
    // Combos #2/#6: enforce without a running gate protects nothing —
    // refused unconditionally (no waiver applies). Also enforced at
    // config load by GrowwMarginGateParams::validate.
    if cfg.enforce && !cfg.enabled {
        return BootGuardVerdict::Refused("enforce_without_enabled");
    }
    if !effectively_live {
        // Combos #1/#3/#4.
        return BootGuardVerdict::Allowed;
    }
    // Combo #8: live, fully gated.
    if cfg.enabled && cfg.enforce {
        return BootGuardVerdict::Allowed;
    }
    // Combos #5/#7: live without protection — only the LOUD dated waiver
    // key allows it.
    if cfg.live_without_margin_gate_operator_ack.trim().is_empty() {
        BootGuardVerdict::Refused("live_requires_enabled_and_enforce_or_dated_ack")
    } else {
        BootGuardVerdict::AllowedWithWaiver
    }
}

// ---------------------------------------------------------------------------
// Carry-across-swap pending ledger
// ---------------------------------------------------------------------------

/// One approved-entry reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    /// When the approval happened (epoch secs).
    pub approved_at_epoch_secs: u64,
    /// The reserved amount (integer paise). MUST be positive — a
    /// non-positive reservation is refused at [`PendingLedger::push`]
    /// (it would REDUCE the reserved sum: fail-open).
    pub required_paise: i64,
    /// The order's push-side reference id (the Groww
    /// `order_reference_id` / intent id), when the caller has one —
    /// carried NOW so the wiring PR's id-MATCHED fill release
    /// ([`PendingLedger::on_fill_event`]) never needs to reshape this
    /// type. `None` = release by hold-expiry only.
    pub reference_id: Option<String>,
}

/// Why the ledger refused a push. The caller MUST fail-close the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerPushRefused {
    /// The ledger is at [`MARGIN_LEDGER_CAP`] — an approval burst past
    /// the cap is itself a fault.
    Full,
    /// The entry's `required_paise` was zero/negative — a non-positive
    /// reservation would reduce the reserved sum (fail-open direction)
    /// and is a caller-data fault, never dressed as a reservation.
    NonPositiveRequired,
    /// The entry's `Some(reference_id)` is already held by a live
    /// reservation (refuter R1-1, 2026-07-15). Per-reservation reference
    /// ids MUST be unique (the broker itself requires
    /// `order_reference_id` uniqueness): with duplicates, a REPLAYED
    /// terminal fill — and poll re-observation of a completed order is
    /// the NORMAL case, not an edge — would drain one sibling reservation
    /// per replay (fail-open). Refusing the duplicate at push time makes
    /// that structurally impossible; the caller fail-closes the entry
    /// (a duplicate id is a wiring/caller bug, never a funds verdict).
    DuplicateReference,
}

/// Carry-across-swap pessimistic ledger — **HOLD-EXPIRY-ONLY**: approved
/// entries reserve their required amount until age-out
/// (`> ledger_hold_secs`) or an id-MATCHED fill release, and are NEVER
/// zeroed on a snapshot swap (the H H1 boundary-race fix).
///
/// There is deliberately NO balance-reflection release: on the SHARED
/// account a snapshot-to-snapshot balance drop is UNATTRIBUTABLE (our
/// fill vs a BruteX co-tenant drain vs M2M), and a same-snapshot
/// approval cohort shares one baseline — one decrement must never free
/// multiple/foreign reservations (hostile-review C1/C2). Over-reserving
/// for ≤ `ledger_hold_secs` (default 120s) is the fail-closed direction.
///
/// Hot-path purity: touched only per signal/probe evaluation (never per
/// tick) — an uncontended `parking_lot::Mutex` over ≤ 64 entries on that
/// cold path is not a hot-path violation.
#[derive(Debug, Default)]
pub struct PendingLedger {
    entries: Mutex<std::collections::VecDeque<LedgerEntry>>,
}

impl PendingLedger {
    /// Empty ledger.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(std::collections::VecDeque::with_capacity(MARGIN_LEDGER_CAP)),
        }
    }

    /// Reserves an approved entry. Refused at the cap
    /// ([`LedgerPushRefused::Full`], coded loud — GROWW-MARG-04
    /// `stage="ledger_full"`), on a non-positive amount
    /// ([`LedgerPushRefused::NonPositiveRequired`]), or on a duplicate
    /// `Some(reference_id)` already held by a live reservation
    /// ([`LedgerPushRefused::DuplicateReference`] — refuter R1-1: with
    /// duplicate ids, a replayed terminal fill would drain one sibling
    /// per replay) — the caller fail-closes in every case.
    ///
    /// # Errors
    /// [`LedgerPushRefused`] names which refusal fired.
    // WIRING-EXEMPT: caller is the deferred RiskEngine approval wiring slot
    pub fn push(&self, entry: LedgerEntry) -> Result<(), LedgerPushRefused> {
        if entry.required_paise <= 0 {
            // Caller-data fault (uncoded by design — the wiring caller
            // attaches the SID/context; never dressed as insufficiency).
            error!(
                refused_required_paise = entry.required_paise,
                "groww margin ledger: non-positive reservation refused fail-closed \
                 (a non-positive amount would REDUCE the reserved sum — caller-data \
                 fault)"
            );
            return Err(LedgerPushRefused::NonPositiveRequired);
        }
        let mut entries = self.entries.lock();
        if let Some(reference) = entry.reference_id.as_deref()
            && entries
                .iter()
                .any(|held| held.reference_id.as_deref() == Some(reference))
        {
            // Caller-data fault (uncoded by design, like the non-positive
            // arm): duplicate reference ids also violate the broker's own
            // order_reference_id uniqueness. Refusing here makes a
            // replayed id-matched fill release structurally single-shot.
            error!(
                "groww margin ledger: duplicate reference-id reservation refused \
                 fail-closed (per-reservation ids must be unique — a replayed \
                 terminal fill must never drain a sibling reservation; caller-data \
                 fault)"
            );
            return Err(LedgerPushRefused::DuplicateReference);
        }
        if entries.len() >= MARGIN_LEDGER_CAP {
            // GROWW-MARG-04 (contract-stubs PR pending)
            error!(
                stage = "ledger_full",
                cap = MARGIN_LEDGER_CAP,
                dropped_required_paise = entry.required_paise,
                "groww margin ledger: capacity overflow — refusing the reservation \
                 fail-closed (an approval burst past the cap is itself a fault)"
            );
            metrics::counter!("tv_groww_margin_gate_ledger_full_total").increment(1);
            return Err(LedgerPushRefused::Full);
        }
        entries.push_back(entry);
        Self::set_gauge(entries.len());
        Ok(())
    }

    /// Prunes hold-expired entries and returns the surviving reserved
    /// sum. HOLD-EXPIRY is the ONLY time-based release — a balance drop
    /// never releases (see the type-level doc).
    ///
    /// Age boundary: an entry EXACTLY at `hold_secs` is retained; one
    /// second past it is dropped.
    ///
    /// Honest residual (refuter R1-3, accepted): the prune is DESTRUCTIVE
    /// at query time — a BOOT-03-class FORWARD wall-clock spike
    /// (> `hold_secs`) permanently releases every reservation even if the
    /// clock later steps back (a pruned entry never resurrects). Inherent
    /// to wall-clock age-out; the boot clock-skew guard owns host clock
    /// sanity, and a pure-backwards clock retains (fail-closed,
    /// `saturating_sub`).
    // WIRING-EXEMPT: caller is the deferred RiskEngine evaluation wiring slot
    pub fn pending_consumed_paise(&self, now_epoch_secs: u64, hold_secs: u64) -> i64 {
        let mut entries = self.entries.lock();
        entries.retain(|entry| {
            now_epoch_secs.saturating_sub(entry.approved_at_epoch_secs) <= hold_secs
        });
        Self::set_gauge(entries.len());
        let sum: i128 = entries.iter().map(|e| i128::from(e.required_paise)).sum();
        i64::try_from(sum).unwrap_or(i64::MAX)
    }

    /// Typed hook for the future Orders-area consumer: a Groww `Filled`
    /// broker event releases the ONE reservation whose `reference_id`
    /// MATCHES the fill's `reference_id` — and releases NOTHING
    /// otherwise. An UNMATCHED release is worse than none on a shared
    /// account (an EXIT fill, an amount-mismatched fill, or a co-tenant
    /// event must never free a foreign entry reservation —
    /// hostile-review C3), so hold-expiry remains the only unconditional
    /// release path.
    // WIRING-TODO: the wiring PR threads the push-side reference id (the
    // order's `order_reference_id`, set at approval time) into
    // `LedgerEntry::reference_id` so this id-MATCHED release engages;
    // entries pushed without a reference id are released ONLY by
    // hold-expiry. Per-reservation ids are UNIQUE by construction —
    // `push` refuses a duplicate `Some(reference_id)` (refuter R1-1), so
    // a REPLAYED terminal fill (poll re-observation of a completed order
    // is the NORM) releases at most the one matched entry, never a
    // sibling; the wiring consumer therefore needs NO transition-edge
    // dedup of its own for ledger safety.
    // WIRING-EXEMPT: caller is the deferred Orders-area poll/push consumer
    pub fn on_fill_event(&self, event: &BrokerOrderEvent) {
        if !matches!(event.broker, Feed::Groww)
            || !matches!(event.status, BrokerOrderStatus::Filled)
        {
            return;
        }
        let Some(fill_reference) = event.reference_id.as_deref() else {
            return; // no id on the fill → hold-expiry releases, not this.
        };
        let mut entries = self.entries.lock();
        if let Some(index) = entries
            .iter()
            .position(|entry| entry.reference_id.as_deref() == Some(fill_reference))
        {
            entries.remove(index);
        }
        Self::set_gauge(entries.len());
    }

    /// Current reservation count (feeds the ledger gauge).
    // WIRING-EXEMPT: forensic accessor for the deferred audit-row wiring
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// True when no reservations are held.
    // WIRING-EXEMPT: forensic accessor for the deferred audit-row wiring
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }

    fn set_gauge(len: usize) {
        // usize→f64 is lossless for any realistic count (cap is 64).
        #[allow(clippy::cast_precision_loss)] // APPROVED: bounded by MARGIN_LEDGER_CAP
        metrics::gauge!("tv_groww_margin_gate_ledger_entries").set(len as f64);
    }
}

// ---------------------------------------------------------------------------
// Fetch primitives
// ---------------------------------------------------------------------------

/// Fetch failure taxonomy → the GROWW-MARG-01 in-file `stage` labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchIssue {
    /// The token provider had no access token (the app-side impl owns
    /// SSM re-reads + 401 cache drops — NEVER a mint).
    NoToken,
    /// Transport-class send/read failure (the one retryable class).
    Transport {
        /// Bounded, secret-redacted error text.
        sanitized: String,
    },
    /// The whole-poll bound elapsed.
    Timeout,
    /// Non-2xx status outside the auth/rate classes.
    Status {
        /// The HTTP status code.
        code: u16,
        /// Bounded, secret-redacted failure body.
        sanitized: String,
    },
    /// 401/403 — the broker rejected our bearer. HONEST BOUNDARY: the
    /// shared `TokenProvider` trait (#1559) has exactly ONE method
    /// (`get_access_token`) — there is NO per-call rejection feedback
    /// channel, so this poll cannot tell a caching provider to drop.
    /// The INJECTED provider MUST self-refresh on its own timer
    /// (time-based SSM re-read at ≥60s pacing, independent of failure
    /// signals — NEVER a mint, per the token-minter lock); until it
    /// does, every poll fails `auth`, the snapshot ages, and the gate
    /// CLOSES fail-closed at the stale edge (the safe direction).
    AuthRejected {
        /// The HTTP status code (401 or 403).
        code: u16,
    },
    /// 429 — counted, NEVER out-polled (pooled bucket shared with BruteX).
    RateLimited,
    /// The response body exceeded [`MARGIN_MAX_BODY_BYTES`].
    Oversize,
    /// The 2xx body failed the whole-struct deserialize (type drift).
    Parse {
        /// Bounded, secret-redacted parse error text.
        sanitized: String,
    },
    /// The envelope reported FAILURE (or carried no payload).
    FailureEnvelope {
        /// Bounded, secret-redacted GA error fields.
        sanitized: String,
    },
}

impl FetchIssue {
    /// GROWW-MARG-01 `stage` label for this failure class.
    pub fn stage(&self) -> &'static str {
        match self {
            Self::NoToken => "no_token",
            Self::Transport { .. } => "transport",
            Self::Timeout => "timeout",
            Self::Status { .. } => "status",
            Self::AuthRejected { .. } => "auth",
            Self::RateLimited => "rate_limited",
            Self::Oversize => "oversize",
            Self::Parse { .. } => "parse",
            Self::FailureEnvelope { .. } => "failure_envelope",
        }
    }

    /// Bounded sanitized detail (empty where none applies). Field names +
    /// failure class only — NEVER balance values (SEC-M1).
    pub fn detail(&self) -> &str {
        match self {
            Self::Transport { sanitized }
            | Self::Status { sanitized, .. }
            | Self::Parse { sanitized }
            | Self::FailureEnvelope { sanitized } => sanitized,
            _ => "",
        }
    }

    /// `tv_groww_margin_fetch_total` outcome label for this class.
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::AuthRejected { .. } => "auth",
            Self::Parse { .. } | Self::FailureEnvelope { .. } => "shape_drift",
            _ => "error",
        }
    }
}

/// Hardened default HTTP client for the margin poll: 5s connect/read
/// timeouts + `redirect::Policy::none()` (a 3xx on a bearer-authed GET is
/// a hard failure, never followed — the §18 hardened-downloader contract).
/// Callers may inject their own client instead (the margin_gate
/// injected-client shape).
///
/// # Errors
/// Propagates the builder error (HTTP-CLIENT-01 class — the app-side
/// wiring degrades loudly, never panics).
// WIRING-EXEMPT: caller is the deferred app-side poll-loop spawn wiring
pub fn build_margin_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(MARGIN_HTTP_TIMEOUT_SECS))
        .timeout(Duration::from_secs(MARGIN_HTTP_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// One GET of the user-margin detail endpoint. Bearer + API-version +
/// Accept headers; 401/403 → `auth`; 429 → `rate_limited` (never
/// retried); body size cap; envelope decode. Failure bodies pass through
/// the [`capture_rest_error_body`] sanitize choke point.
///
/// # Errors
/// Returns the classified [`FetchIssue`] — the caller logs ONCE per poll
/// with the `stage` field (single emission choke point).
pub async fn fetch_user_margin(
    client: &reqwest::Client,
    token: &SecretString,
) -> Result<UserMarginPayload, FetchIssue> {
    fetch_user_margin_from(client, GROWW_USER_MARGIN_URL, token).await
}

/// URL-parameterized body of [`fetch_user_margin`] — the production fn
/// pins the const endpoint; tests point this SAME classify path at a
/// local mock (zero live HTTP while the gates are off, even in tests).
async fn fetch_user_margin_from(
    client: &reqwest::Client,
    url: &str,
    token: &SecretString,
) -> Result<UserMarginPayload, FetchIssue> {
    let response = client
        .get(url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token.expose_secret()),
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .header(GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE)
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                FetchIssue::Timeout
            } else {
                FetchIssue::Transport {
                    sanitized: capture_rest_error_body(&err.to_string()),
                }
            }
        })?;

    let status = response.status();
    if status.as_u16() == 429 {
        return Err(FetchIssue::RateLimited);
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(FetchIssue::AuthRejected {
            code: status.as_u16(),
        });
    }
    if let Some(declared) = response.content_length()
        && declared > MARGIN_MAX_BODY_BYTES
    {
        return Err(FetchIssue::Oversize);
    }
    let body = response.bytes().await.map_err(|err| {
        if err.is_timeout() {
            FetchIssue::Timeout
        } else {
            FetchIssue::Transport {
                sanitized: capture_rest_error_body(&err.to_string()),
            }
        }
    })?;
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > MARGIN_MAX_BODY_BYTES {
        return Err(FetchIssue::Oversize);
    }
    if !status.is_success() {
        return Err(FetchIssue::Status {
            code: status.as_u16(),
            sanitized: capture_rest_error_body(&String::from_utf8_lossy(&body)),
        });
    }
    let envelope: UserMarginEnvelope =
        serde_json::from_slice(&body).map_err(|err| FetchIssue::Parse {
            sanitized: capture_rest_error_body(&err.to_string()),
        })?;
    let success = envelope.status.as_deref() == Some("SUCCESS");
    match (success, envelope.payload) {
        (true, Some(payload)) => Ok(payload),
        _ => {
            let ga = envelope.error.unwrap_or_default();
            let raw = format!(
                "envelope status={:?} ga_code={:?} ga_message={:?}",
                envelope.status, ga.code, ga.message
            );
            Err(FetchIssue::FailureEnvelope {
                sanitized: capture_rest_error_body(&raw),
            })
        }
    }
}

/// [`fetch_user_margin_from`] with ONE bounded, delayed retry on the
/// transport class ONLY (every other class is terminal for the poll).
/// Production reaches this solely through [`poll_turn`], which pins the
/// const endpoint; tests drive the retry semantics against a local mock.
async fn fetch_user_margin_with_transport_retry_from(
    client: &reqwest::Client,
    url: &str,
    token: &SecretString,
    retry_delay: Duration,
) -> Result<UserMarginPayload, FetchIssue> {
    match fetch_user_margin_from(client, url, token).await {
        Err(FetchIssue::Transport { sanitized }) => {
            debug!(
                sanitized = %sanitized,
                "groww margin fetch: transport-class failure — one bounded retry"
            );
            tokio::time::sleep(retry_delay).await;
            fetch_user_margin_from(client, url, token).await
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Poll turn + loop
// ---------------------------------------------------------------------------

/// What one poll turn did (the loop's edge bookkeeping input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// A gate was closed (`enabled` / `margin_read` / market hours) — no
    /// token read, no REST call.
    SkippedGatesClosed,
    /// A fresh snapshot was validated and stored.
    Stored,
    /// The poll failed — the payload is the `stage` label already emitted.
    Failed(&'static str),
}

/// Borrowed dependencies for one poll turn.
pub struct PollContext<'a> {
    /// The (injected, hardened) HTTP client.
    pub client: &'a reqwest::Client,
    /// Access-token source (app-side impl owns SSM read/drop semantics).
    pub tokens: &'a Arc<dyn TokenProvider>,
    /// The snapshot cell polls store into.
    pub cell: &'a MarginSnapshotCell,
    /// The `[groww_margin_gate]` config.
    pub cfg: &'a GrowwMarginGateParams,
}

/// One poll turn: gate check → token → bounded fetch (+ one transport
/// retry) → validate → store. The single emission choke point for the
/// per-poll GROWW-MARG-01 lines and the fetch-outcome counter.
///
/// While ANY gate is closed (`[groww_margin_gate] enabled`,
/// `[groww_orders] margin_read`, market hours) this returns
/// [`PollOutcome::SkippedGatesClosed`] having touched NEITHER the token
/// provider NOR the network — ratchet-tested zero-hit behaviour.
///
/// `now_epoch_secs` / `today_ist` are the PRE-fetch instant (stamped as
/// `fetch_initiated_at`); the stored snapshot's `fetched_at` +
/// `fetched_on_ist_date` are re-captured at response-LANDED time
/// (post-await), so the staleness clock starts when the data arrived —
/// never up to ~15s early.
// WIRING-EXEMPT: driven by run_margin_poll_loop + the deferred app wiring
pub async fn poll_turn(
    ctx: &PollContext<'_>,
    margin_read_enabled: bool,
    in_market_hours: bool,
    now_epoch_secs: u64,
    today_ist: u32,
) -> PollOutcome {
    poll_turn_from(
        ctx,
        GROWW_USER_MARGIN_URL,
        margin_read_enabled,
        in_market_hours,
        now_epoch_secs,
        today_ist,
    )
    .await
}

/// URL-parameterized body of [`poll_turn`] — the production fn pins the
/// const endpoint (Gate-5); tests drive the FULL fetch→validate→store
/// composition against a local mock through this seam (the
/// `fetch_user_margin_from` discipline; private, never a `pub` URL
/// injection surface).
async fn poll_turn_from(
    ctx: &PollContext<'_>,
    url: &str,
    margin_read_enabled: bool,
    in_market_hours: bool,
    now_epoch_secs: u64,
    today_ist: u32,
) -> PollOutcome {
    if !(ctx.cfg.enabled && margin_read_enabled && in_market_hours) {
        return PollOutcome::SkippedGatesClosed;
    }

    let token = match ctx.tokens.get_access_token() {
        Ok(token) => token,
        Err(err) => {
            let issue = FetchIssue::NoToken;
            // GROWW-MARG-01 (contract-stubs PR pending)
            error!(
                stage = issue.stage(),
                sanitized = %capture_rest_error_body(&err.to_string()),
                "groww margin fetch degraded — no access token; the snapshot ages \
                 toward the fail-closed stale edge (exits are unaffected)"
            );
            metrics::counter!("tv_groww_margin_fetch_total", "outcome" => issue.outcome())
                .increment(1);
            return PollOutcome::Failed(issue.stage());
        }
    };

    let fetch = fetch_user_margin_with_transport_retry_from(
        ctx.client,
        url,
        &token,
        Duration::from_secs(MARGIN_TRANSPORT_RETRY_DELAY_SECS),
    );
    let result =
        match tokio::time::timeout(Duration::from_secs(MARGIN_POLL_OVERALL_TIMEOUT_SECS), fetch)
            .await
        {
            Ok(result) => result,
            Err(_elapsed) => Err(FetchIssue::Timeout),
        };

    let payload = match result {
        Ok(payload) => payload,
        Err(issue) => {
            // GROWW-MARG-01 (contract-stubs PR pending)
            error!(
                stage = issue.stage(),
                sanitized = %issue.detail(),
                "groww margin fetch degraded — this poll is lost; the prior \
                 snapshot ages toward the fail-closed stale edge (429 is never \
                 out-polled; exits are unaffected)"
            );
            metrics::counter!("tv_groww_margin_fetch_total", "outcome" => issue.outcome())
                .increment(1);
            return PollOutcome::Failed(issue.stage());
        }
    };

    // P1: stamp `fetched_at` + the IST date at response-LANDED time
    // (post-await — the fetch can take up to ~15s and can cross IST
    // midnight); `now_epoch_secs` stays the pre-fetch `fetch_initiated_at`
    // diagnostic. The max() guards a non-monotonic wall clock (landed
    // never precedes initiated).
    let landed_epoch_secs = epoch_now_secs().max(now_epoch_secs);
    let landed_ist_date = match ist_date_from_epoch_secs(landed_epoch_secs) {
        0 => today_ist, // unrepresentable instant — keep the caller's date
        date => date,
    };
    match validate_payload(&payload, now_epoch_secs, landed_epoch_secs, landed_ist_date) {
        Ok(snapshot) => {
            let age_at_store_secs =
                landed_epoch_secs.saturating_sub(snapshot.fetched_at_epoch_secs);
            let usable =
                usable_paise_after_buffer(snapshot.option_buy_avail_paise, ctx.cfg.buffer_pct);
            ctx.cell.store(snapshot);
            metrics::counter!("tv_groww_margin_fetch_total", "outcome" => "ok").increment(1);
            // Age relative to the landed clock (0 by construction — the
            // stamp IS the landed instant; computed, never hardcoded).
            #[allow(clippy::cast_precision_loss)] // APPROVED: gauge display only
            metrics::gauge!("tv_groww_margin_snapshot_age_secs").set(age_at_store_secs as f64);
            // SEC-GM-1: the raw account balance is NEVER shipped as a
            // metric — the CW agent's prometheus collector scrapes the
            // WHOLE /metrics endpoint into the CloudWatch metrics log
            // group, so a raw-balance gauge WOULD stream the account
            // balance off-box every poll (the earlier "not
            // CloudWatch-shipped" claim was false for the log leg). The
            // shipped signal is the saturating 0-100 headroom-vs-floor
            // percentage instead; exact paise live only on the
            // GROWW-MARG-04 verdict line (a recorded decision — runbook
            // §4).
            metrics::gauge!("tv_groww_margin_headroom_pct")
                .set(compute_headroom_pct(usable, ctx.cfg.floor_paise));
            PollOutcome::Stored
        }
        Err(issue) => {
            // Field NAMES + clamp class only — NEVER the fetched balance
            // value (the SEC-M1 redaction rule for fetch-side lines).
            // GROWW-MARG-01 (contract-stubs PR pending)
            error!(
                stage = issue.stage(),
                detail = issue.detail(),
                "groww margin payload refused — snapshot NOT stored; the prior \
                 snapshot ages toward the fail-closed stale edge (shape drift and \
                 outage collapse into one fail-closed path)"
            );
            metrics::counter!("tv_groww_margin_fetch_total", "outcome" => "shape_drift")
                .increment(1);
            PollOutcome::Failed(issue.stage())
        }
    }
}

/// Consecutive-failure escalation edge (pure): fires exactly once when the
/// streak reaches [`MARGIN_CONSECUTIVE_FAILURE_ESCALATION`]; re-armed by a
/// success.
#[derive(Debug, Default)]
pub struct FailureEdge {
    consecutive: u32,
    escalated: bool,
}

impl FailureEdge {
    /// Fresh edge (no failures observed).
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a failed poll; `true` exactly when the escalation edge
    /// fires (once per episode).
    pub fn record_failure(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive >= MARGIN_CONSECUTIVE_FAILURE_ESCALATION && !self.escalated {
            self.escalated = true;
            return true;
        }
        false
    }

    /// Records a successful poll; `true` when this recovers a failing
    /// episode (the falling-edge Info line).
    pub fn record_success(&mut self) -> bool {
        let recovered = self.consecutive > 0;
        self.consecutive = 0;
        self.escalated = false;
        recovered
    }

    /// Current consecutive-failure streak.
    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

/// Staleness episode edge (pure): `Some(true)` on the rising edge into
/// stale, `Some(false)` on the falling edge back to fresh, `None` while
/// unchanged.
#[derive(Debug, Default)]
pub struct StaleEdge {
    open: bool,
}

impl StaleEdge {
    /// Fresh edge (not in a stale episode).
    pub fn new() -> Self {
        Self::default()
    }

    /// Observes the current staleness and reports edges.
    pub fn observe(&mut self, is_stale: bool) -> Option<bool> {
        match (self.open, is_stale) {
            (false, true) => {
                self.open = true;
                Some(true)
            }
            (true, false) => {
                self.open = false;
                Some(false)
            }
            _ => None,
        }
    }
}

/// Wall-clock epoch seconds (cold path; 0 on a pre-1970 clock — which the
/// staleness math then treats fail-closed).
fn epoch_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The supervisable 60s in-session poll loop.
///
/// Semantics: the interval IS the rate limiter (~1 req/min) with a
/// governor 1-per-[`MARGIN_POLL_BELT_PERIOD_SECS`] defensive belt under
/// it; each turn re-reads the AND-gate (`cfg.enabled` — boot-frozen
/// config — AND the injected `margin_read_enabled` mirror of
/// `[groww_orders] margin_read` AND the injected market-hours predicate);
/// a failed poll never stores (the snapshot ages to the stale edge); the
/// 3-consecutive escalation + staleness episode edges are emitted here,
/// coded, once per episode. Exit via the `shutdown` watch channel.
// WIRING-EXEMPT: the spawn site is the deferred app-integration PR (BOTH
// boot arms + the groww_margin wiring guard, per the margin design §E)
pub async fn run_margin_poll_loop<M>(
    client: reqwest::Client,
    tokens: Arc<dyn TokenProvider>,
    cell: Arc<MarginSnapshotCell>,
    cfg: GrowwMarginGateParams,
    margin_read_enabled: Arc<AtomicBool>,
    in_market_hours: M,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) where
    M: Fn() -> bool + Send + 'static,
{
    let belt: RateLimiter<NotKeyed, InMemoryState, DefaultClock> = RateLimiter::direct(
        Quota::with_period(Duration::from_secs(MARGIN_POLL_BELT_PERIOD_SECS))
            .unwrap_or_else(|| Quota::per_second(NonZeroU32::MIN)),
    );
    let mut interval = tokio::time::interval(Duration::from_secs(cfg.poll_secs.max(1)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut failure_edge = FailureEdge::new();
    let mut stale_edge = StaleEdge::new();
    // No snapshot yet: the age gauge reads −1 (the honest "never fetched").
    metrics::gauge!("tv_groww_margin_snapshot_age_secs").set(-1.0);

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    info!("groww margin poll loop: shutdown observed — exiting");
                    return;
                }
                continue;
            }
        }
        if *shutdown.borrow() {
            info!("groww margin poll loop: shutdown observed — exiting");
            return;
        }

        let now = epoch_now_secs();
        let today = ist_date_from_epoch_secs(now);
        let gates_open =
            cfg.enabled && margin_read_enabled.load(Ordering::Relaxed) && in_market_hours();

        // Age gauge every turn (−1 before the first snapshot).
        let age = cell.load().map(|snap| {
            (
                now.saturating_sub(snap.fetched_at_epoch_secs),
                snap.fetched_on_ist_date,
            )
        });
        match age {
            #[allow(clippy::cast_precision_loss)] // APPROVED: gauge display only
            Some((age_secs, _)) => {
                metrics::gauge!("tv_groww_margin_snapshot_age_secs").set(age_secs as f64);
            }
            None => metrics::gauge!("tv_groww_margin_snapshot_age_secs").set(-1.0),
        }

        if !gates_open {
            continue;
        }

        // Defensive belt under the interval — refuse, never queue.
        if belt.check().is_err() {
            debug!("groww margin poll: belt refused this turn (defensive floor)");
            continue;
        }

        let ctx = PollContext {
            client: &client,
            tokens: &tokens,
            cell: &cell,
            cfg: &cfg,
        };
        let outcome = poll_turn(&ctx, true, true, now, today).await;
        match &outcome {
            PollOutcome::Stored => {
                if failure_edge.record_success() {
                    info!(
                        "groww margin fetch recovered — a fresh snapshot landed and the \
                         entry gate re-opens on it"
                    );
                }
            }
            PollOutcome::Failed(_stage) => {
                if failure_edge.record_failure() {
                    // GROWW-MARG-01 (contract-stubs PR pending)
                    error!(
                        stage = "escalation",
                        consecutive = failure_edge.consecutive(),
                        "groww margin fetch: consecutive-failure escalation edge — the \
                         snapshot is approaching (or past) the fail-closed stale edge"
                    );
                }
            }
            PollOutcome::SkippedGatesClosed => {}
        }

        // Staleness episode edge (only meaningful while gates are open).
        // ONE cell load — the edge decision and its reason/age derive
        // from the SAME snapshot view (no load-vs-store race skew).
        let stale_view = cell.load();
        let (is_stale, age_secs, reason) = match stale_view.as_deref() {
            None => (true, 0, "no_snapshot"),
            Some(snap) => {
                let age = now.saturating_sub(snap.fetched_at_epoch_secs);
                if snap.fetched_on_ist_date != today {
                    (true, age, "prev_ist_date")
                } else {
                    (age > cfg.stale_secs, age, "stale_age")
                }
            }
        };
        match stale_edge.observe(is_stale) {
            Some(true) => {
                // GROWW-MARG-03 (contract-stubs PR pending)
                error!(
                    age_secs,
                    reason,
                    stale_secs = cfg.stale_secs,
                    "groww margin snapshot STALE — the entry gate is CLOSED fail-closed \
                     until a fresh poll lands (exits are unaffected)"
                );
            }
            Some(false) => {
                info!(
                    "groww margin snapshot fresh again — the entry gate re-opened on a \
                     new balance reading"
                );
            }
            None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::oms::types::OmsError;

    // -- test doubles ---------------------------------------------------------

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

    /// Token provider that fails — exercises the no_token arm.
    struct FailingTokens;
    impl TokenProvider for FailingTokens {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            Err(OmsError::NoToken)
        }
    }

    /// Single-route raw-TCP mock (the margin_gate routing-mock pattern):
    /// serves every request with `status` + `body`, counting hits.
    async fn start_margin_mock(
        status: u16,
        body: &str,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let body = body.to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_inner = Arc::clone(&hits);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await.unwrap_or(0);
                hits_inner.fetch_add(1, Ordering::SeqCst);
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

    /// Mock that accepts and immediately closes (transport-class failure).
    async fn start_closing_mock() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_inner = Arc::clone(&hits);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                hits_inner.fetch_add(1, Ordering::SeqCst);
                drop(stream); // close without any response bytes
            }
        });
        (base_url, hits, handle)
    }

    /// Mock that writes an arbitrary RAW HTTP response byte-for-byte
    /// (for header-shaped attacks the templated mock cannot express —
    /// e.g. a hostile declared Content-Length or a close-delimited body
    /// with no Content-Length at all).
    async fn start_raw_mock(raw: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await.unwrap_or(0);
                let _ = stream.write_all(&raw).await;
                let _ = stream.shutdown().await;
            }
        });
        (base_url, handle)
    }

    /// A client pointed at the mock: same hardening as the default
    /// builder, but the URL const targets the real host — so tests build
    /// their own client with the mock as a proxy-free base by overriding
    /// via reqwest is impossible; instead we exercise `fetch_user_margin`
    /// against the mock through a rewritten request path helper below.
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    // The HTTP-path tests below drive the PRODUCTION classify body
    // (`fetch_user_margin_from` / `fetch_user_margin_with_transport_retry_from`)
    // against local raw-TCP mocks via the injectable-URL parameter — the
    // public fns pin the const endpoint, so no test ever performs live
    // HTTP to the real margin surface.

    const HAPPY_BODY: &str = r#"{
        "status": "SUCCESS",
        "payload": {
            "clear_cash": 41250.50,
            "net_margin_used": 1000.0,
            "fno_margin_details": {
                "option_buy_balance_available": 25000.25
            }
        }
    }"#;

    const FAILURE_ENVELOPE_BODY: &str = r#"{
        "status": "FAILURE",
        "error": { "code": "GA001", "message": "bad token" }
    }"#;

    fn enabled_cfg() -> GrowwMarginGateParams {
        GrowwMarginGateParams {
            enabled: true,
            ..GrowwMarginGateParams::default()
        }
    }

    fn enforce_cfg() -> GrowwMarginGateParams {
        GrowwMarginGateParams {
            enabled: true,
            enforce: true,
            ..GrowwMarginGateParams::default()
        }
    }

    /// A fresh snapshot: ₹25,000.00 available, fetched now, today.
    fn fresh_snapshot(now: u64, today: u32) -> GrowwMarginSnapshot {
        GrowwMarginSnapshot {
            fetched_at_epoch_secs: now,
            fetch_initiated_at_epoch_secs: now,
            fetched_on_ist_date: today,
            option_buy_avail_paise: 2_500_000,
            clear_cash_paise: Some(4_125_050),
            net_margin_used_paise: Some(100_000),
        }
    }

    const NOW: u64 = 1_800_000_000;
    const TODAY: u32 = 20_260_715;

    fn entry(required_paise: i64) -> OrderIntent {
        OrderIntent::Entry { required_paise }
    }

    // -- time helpers ---------------------------------------------------------

    #[test]
    fn test_ist_date_from_epoch_secs_known_instants() {
        // 2026-07-15 00:00:00 IST == 2026-07-14 18:30:00 UTC == 1784053800.
        assert_eq!(ist_date_from_epoch_secs(1_784_053_800), 20_260_715);
        // One second earlier is still 2026-07-14 IST.
        assert_eq!(ist_date_from_epoch_secs(1_784_053_799), 20_260_714);
        // Unrepresentable → 0 (fail-closed sentinel).
        assert_eq!(ist_date_from_epoch_secs(u64::MAX), 0);
    }

    #[test]
    fn test_ist_secs_of_day_from_epoch_secs() {
        // 1784053800 is IST midnight → 0 secs-of-day.
        assert_eq!(ist_secs_of_day_from_epoch_secs(1_784_053_800), 0);
        // + 09:00:00 → the window opens.
        assert_eq!(
            ist_secs_of_day_from_epoch_secs(1_784_053_800 + 32_400),
            32_400
        );
    }

    #[test]
    fn test_poll_window_accepts_boundaries() {
        assert!(!poll_window_accepts(9 * 3600 - 1), "08:59:59 must refuse");
        assert!(poll_window_accepts(9 * 3600), "09:00:00 must accept");
        assert!(
            poll_window_accepts(15 * 3600 + 35 * 60 - 1),
            "15:34:59 must accept"
        );
        assert!(
            !poll_window_accepts(15 * 3600 + 35 * 60),
            "15:35:00 must refuse (exclusive close)"
        );
    }

    // -- gate params (module-local operator-held config surface) ----------------

    /// 🟢 The params default is FAIL-SAFE: fully disabled with the safe
    /// tuning defaults intact — the module is dark until the deferred
    /// wiring constructs an enabled instance (the config-section twin of
    /// this test rides the central contract-stubs PR).
    #[test]
    fn test_groww_margin_gate_params_default_is_disabled() {
        let d = GrowwMarginGateParams::default();
        assert!(!d.enabled, "groww margin gate must default OFF (fail-safe)");
        assert!(!d.enforce, "enforce must default OFF (observe-only)");
        assert_eq!(d.poll_secs, 60);
        assert_eq!(d.stale_secs, 180);
        assert_eq!(d.buffer_pct, 20);
        assert_eq!(d.floor_paise, 500_000);
        assert_eq!(d.ledger_hold_secs, 120);
        assert_eq!(d.premium_floor_paise, 500);
        assert!(d.live_without_margin_gate_operator_ack.is_empty());
        assert!(d.validate().is_ok(), "the defaults must validate");
    }

    /// Bounds validation: every knob's documented range is enforced, and
    /// `enforce = true` without `enabled = true` is the unconditionally-
    /// inconsistent combo (#2/#6 of the 8-combo boot-guard matrix).
    #[test]
    fn test_groww_margin_gate_params_validate_bounds_every_knob() {
        // enforce without enabled — inconsistent, refused.
        let cfg = GrowwMarginGateParams {
            enforce: true,
            ..GrowwMarginGateParams::default()
        };
        assert!(
            cfg.validate().is_err(),
            "enforce = true with enabled = false must be refused"
        );
        let cfg = GrowwMarginGateParams {
            enabled: true,
            enforce: true,
            ..GrowwMarginGateParams::default()
        };
        assert!(cfg.validate().is_ok(), "enabled + enforce is consistent");

        // poll_secs 30..=300.
        for bad in [0_u64, 29, 301] {
            let cfg = GrowwMarginGateParams {
                poll_secs: bad,
                // keep dependent knobs consistent so THIS bound trips
                stale_secs: 900,
                ledger_hold_secs: 600,
                ..GrowwMarginGateParams::default()
            };
            assert!(cfg.validate().is_err(), "poll_secs {bad} must be rejected");
        }
        for good in [30_u64, 300] {
            let cfg = GrowwMarginGateParams {
                poll_secs: good,
                stale_secs: 900,
                ledger_hold_secs: 600,
                ..GrowwMarginGateParams::default()
            };
            assert!(cfg.validate().is_ok(), "poll_secs {good} must pass");
        }

        // stale_secs [2 x poll_secs, 900] — 119 (< 2 x 60) and 901 refuse;
        // the exact 120 and 900 boundaries pass.
        for bad in [0_u64, 119, 901] {
            let cfg = GrowwMarginGateParams {
                stale_secs: bad,
                ..GrowwMarginGateParams::default()
            };
            assert!(cfg.validate().is_err(), "stale_secs {bad} must be rejected");
        }
        for good in [120_u64, 180, 900] {
            let cfg = GrowwMarginGateParams {
                stale_secs: good,
                ..GrowwMarginGateParams::default()
            };
            assert!(cfg.validate().is_ok(), "stale_secs {good} must pass");
        }

        // buffer_pct 0..=90.
        for bad in [91_u8, 100, 255] {
            let cfg = GrowwMarginGateParams {
                buffer_pct: bad,
                ..GrowwMarginGateParams::default()
            };
            assert!(cfg.validate().is_err(), "buffer_pct {bad} must be rejected");
        }
        for good in [0_u8, 20, 90] {
            let cfg = GrowwMarginGateParams {
                buffer_pct: good,
                ..GrowwMarginGateParams::default()
            };
            assert!(cfg.validate().is_ok(), "buffer_pct {good} must pass");
        }

        // floor_paise >= 0.
        let cfg = GrowwMarginGateParams {
            floor_paise: -1,
            ..GrowwMarginGateParams::default()
        };
        assert!(cfg.validate().is_err(), "negative floor_paise must refuse");
        let cfg = GrowwMarginGateParams {
            floor_paise: 0,
            ..GrowwMarginGateParams::default()
        };
        assert!(cfg.validate().is_ok(), "floor_paise 0 is legal");

        // ledger_hold_secs [poll_secs, 600].
        for bad in [0_u64, 59, 601] {
            let cfg = GrowwMarginGateParams {
                ledger_hold_secs: bad,
                ..GrowwMarginGateParams::default()
            };
            assert!(
                cfg.validate().is_err(),
                "ledger_hold_secs {bad} must be rejected"
            );
        }
        for good in [60_u64, 120, 600] {
            let cfg = GrowwMarginGateParams {
                ledger_hold_secs: good,
                ..GrowwMarginGateParams::default()
            };
            assert!(cfg.validate().is_ok(), "ledger_hold_secs {good} must pass");
        }

        // premium_floor_paise > 0.
        for bad in [0_i64, -500] {
            let cfg = GrowwMarginGateParams {
                premium_floor_paise: bad,
                ..GrowwMarginGateParams::default()
            };
            assert!(
                cfg.validate().is_err(),
                "premium_floor_paise {bad} must be rejected"
            );
        }
        let cfg = GrowwMarginGateParams {
            premium_floor_paise: 1,
            ..GrowwMarginGateParams::default()
        };
        assert!(cfg.validate().is_ok(), "premium_floor_paise 1 is legal");
    }

    // -- evaluate: truth tables -----------------------------------------------

    #[test]
    fn test_exit_intent_always_allowed_even_stale_disabled_and_no_snapshot() {
        // No snapshot + disabled gate: an exit still passes.
        let disabled = GrowwMarginGateParams::default();
        assert_eq!(
            evaluate(None, OrderIntent::Exit, 0, &disabled, NOW, TODAY),
            GrowwMarginVerdict::ExitAlwaysAllowed
        );
        // A badly-stale snapshot: an exit still passes.
        let snap = fresh_snapshot(NOW - 100_000, TODAY - 1);
        assert_eq!(
            evaluate(
                Some(&snap),
                OrderIntent::Exit,
                0,
                &enforce_cfg(),
                NOW,
                TODAY
            ),
            GrowwMarginVerdict::ExitAlwaysAllowed
        );
    }

    #[test]
    fn test_disabled_gate_skips_before_snapshot_machinery() {
        let disabled = GrowwMarginGateParams::default();
        assert_eq!(
            evaluate(None, entry(1_000), 0, &disabled, NOW, TODAY),
            GrowwMarginVerdict::SkippedDisabled
        );
    }

    #[test]
    fn test_no_snapshot_fails_closed() {
        assert_eq!(
            evaluate(None, entry(1_000), 0, &enabled_cfg(), NOW, TODAY),
            GrowwMarginVerdict::FailClosedNoSnapshot
        );
    }

    #[test]
    fn test_stale_boundary_age_equal_threshold_passes_plus_one_fails() {
        let cfg = enabled_cfg();
        // age == stale_secs (180): still evaluates (boundary inclusive).
        let snap = fresh_snapshot(NOW - cfg.stale_secs, TODAY);
        assert!(matches!(
            evaluate(Some(&snap), entry(1_000), 0, &cfg, NOW, TODAY),
            GrowwMarginVerdict::Pass(_)
        ));
        // age == stale_secs + 1: fail-closed.
        let snap = fresh_snapshot(NOW - cfg.stale_secs - 1, TODAY);
        assert_eq!(
            evaluate(Some(&snap), entry(1_000), 0, &cfg, NOW, TODAY),
            GrowwMarginVerdict::FailClosedStale {
                age_secs: cfg.stale_secs + 1
            }
        );
    }

    #[test]
    fn test_prev_ist_date_fails_closed_regardless_of_age() {
        // Fetched "just now" but stamped with yesterday's IST date.
        let snap = fresh_snapshot(NOW, TODAY - 1);
        assert_eq!(
            evaluate(Some(&snap), entry(1_000), 0, &enabled_cfg(), NOW, TODAY),
            GrowwMarginVerdict::FailClosedStale { age_secs: 0 }
        );
    }

    #[test]
    fn test_bad_input_zero_negative_and_below_floor_fail_closed() {
        let snap = fresh_snapshot(NOW, TODAY);
        let cfg = enabled_cfg();
        assert_eq!(
            evaluate(Some(&snap), entry(0), 0, &cfg, NOW, TODAY),
            GrowwMarginVerdict::FailClosedBadInput {
                reason: "non_positive_required"
            }
        );
        assert_eq!(
            evaluate(Some(&snap), entry(-5_000), 0, &cfg, NOW, TODAY),
            GrowwMarginVerdict::FailClosedBadInput {
                reason: "non_positive_required"
            }
        );
        // Below the ₹5 premium floor (default 500 paise).
        assert_eq!(
            evaluate(
                Some(&snap),
                entry(cfg.premium_floor_paise - 1),
                0,
                &cfg,
                NOW,
                TODAY
            ),
            GrowwMarginVerdict::FailClosedBadInput {
                reason: "below_premium_floor"
            }
        );
        // Exactly at the floor: evaluates (and passes on this snapshot).
        assert!(matches!(
            evaluate(
                Some(&snap),
                entry(cfg.premium_floor_paise),
                0,
                &cfg,
                NOW,
                TODAY
            ),
            GrowwMarginVerdict::Pass(_)
        ));
    }

    #[test]
    fn test_buffer_and_floor_boundary_paise_exact() {
        // avail 2,500,000 paise, buffer 20% → usable 2,000,000. floor 0,
        // no pending: required == usable passes; +1 paisa refuses.
        let snap = fresh_snapshot(NOW, TODAY);
        let cfg = GrowwMarginGateParams {
            enabled: true,
            floor_paise: 0,
            ..GrowwMarginGateParams::default()
        };
        match evaluate(Some(&snap), entry(2_000_000), 0, &cfg, NOW, TODAY) {
            GrowwMarginVerdict::Pass(n) => {
                assert_eq!(n.usable_paise, 2_000_000);
                assert_eq!(n.headroom_paise, 0);
            }
            other => panic!("expected Pass at the exact buffer boundary, got {other:?}"),
        }
        match evaluate(Some(&snap), entry(2_000_001), 0, &cfg, NOW, TODAY) {
            GrowwMarginVerdict::WouldReject(n) => {
                assert_eq!(n.required_paise, 2_000_001);
                assert_eq!(n.usable_paise, 2_000_000);
                assert_eq!(n.headroom_paise, -1);
            }
            other => panic!("expected WouldReject one paisa over, got {other:?}"),
        }
    }

    #[test]
    fn test_min_free_floor_boundary_exact() {
        // usable 2,000,000; floor default 500,000 → required 1,500,000
        // leaves headroom EXACTLY at the floor → Pass; +1 refuses.
        let snap = fresh_snapshot(NOW, TODAY);
        let cfg = enabled_cfg();
        match evaluate(Some(&snap), entry(1_500_000), 0, &cfg, NOW, TODAY) {
            GrowwMarginVerdict::Pass(n) => assert_eq!(n.headroom_paise, 500_000),
            other => panic!("expected Pass with headroom == floor, got {other:?}"),
        }
        assert!(matches!(
            evaluate(Some(&snap), entry(1_500_001), 0, &cfg, NOW, TODAY),
            GrowwMarginVerdict::WouldReject(_)
        ));
    }

    #[test]
    fn test_pending_ledger_consumption_shifts_the_boundary_exactly() {
        let snap = fresh_snapshot(NOW, TODAY);
        let cfg = GrowwMarginGateParams {
            enabled: true,
            floor_paise: 0,
            ..GrowwMarginGateParams::default()
        };
        // required 1,900,000 + pending 100,000 == usable 2,000,000 → Pass.
        assert!(matches!(
            evaluate(Some(&snap), entry(1_900_000), 100_000, &cfg, NOW, TODAY),
            GrowwMarginVerdict::Pass(_)
        ));
        // One more paisa of pending refuses.
        assert!(matches!(
            evaluate(Some(&snap), entry(1_900_000), 100_001, &cfg, NOW, TODAY),
            GrowwMarginVerdict::WouldReject(_)
        ));
    }

    #[test]
    fn test_enforce_vs_observe_identical_arithmetic_label_only() {
        let snap = fresh_snapshot(NOW, TODAY);
        let observe = enabled_cfg();
        let enforce = enforce_cfg();
        let observe_verdict = evaluate(Some(&snap), entry(9_999_999), 0, &observe, NOW, TODAY);
        let enforce_verdict = evaluate(Some(&snap), entry(9_999_999), 0, &enforce, NOW, TODAY);
        let GrowwMarginVerdict::WouldReject(observe_numbers) = observe_verdict else {
            panic!("observe mode must label WouldReject, got {observe_verdict:?}");
        };
        let GrowwMarginVerdict::RejectedInsufficient(enforce_numbers) = enforce_verdict else {
            panic!("enforce mode must label RejectedInsufficient, got {enforce_verdict:?}");
        };
        // The shared-evaluate ratchet: byte-identical numbers.
        assert_eq!(observe_numbers, enforce_numbers);
        // And a Pass is a Pass in both modes.
        assert!(matches!(
            evaluate(Some(&snap), entry(1_000), 0, &observe, NOW, TODAY),
            GrowwMarginVerdict::Pass(_)
        ));
        assert!(matches!(
            evaluate(Some(&snap), entry(1_000), 0, &enforce, NOW, TODAY),
            GrowwMarginVerdict::Pass(_)
        ));
    }

    #[test]
    fn test_usable_i128_no_overflow_near_plausibility_ceiling() {
        // avail at the to_paise ceiling (₹1e12 → 1e14 paise): the i128
        // intermediate must not panic and the verdict must be sane.
        let snap = GrowwMarginSnapshot {
            option_buy_avail_paise: 100_000_000_000_000,
            ..fresh_snapshot(NOW, TODAY)
        };
        let verdict = evaluate(Some(&snap), entry(1_000), 0, &enabled_cfg(), NOW, TODAY);
        assert!(matches!(verdict, GrowwMarginVerdict::Pass(_)));
        // Negative available balance → usable 0 → refuse.
        let snap = GrowwMarginSnapshot {
            option_buy_avail_paise: -10_000,
            ..fresh_snapshot(NOW, TODAY)
        };
        match evaluate(Some(&snap), entry(1_000), 0, &enabled_cfg(), NOW, TODAY) {
            GrowwMarginVerdict::WouldReject(n) => assert_eq!(n.usable_paise, 0),
            other => panic!("expected WouldReject on a debit balance, got {other:?}"),
        }
    }

    #[test]
    fn test_evaluate_and_record_returns_the_same_verdict() {
        let snap = fresh_snapshot(NOW, TODAY);
        let cfg = enforce_cfg();
        let direct = evaluate(Some(&snap), entry(9_999_999), 0, &cfg, NOW, TODAY);
        let recorded = evaluate_and_record(
            Some(&snap),
            entry(9_999_999),
            0,
            &cfg,
            NOW,
            TODAY,
            "selftest",
        );
        assert_eq!(direct, recorded);
    }

    #[test]
    fn test_headroom_pct_saturating_and_non_invertible() {
        // SEC-GM-1: usable ≥ floor saturates at 100 — the shipped gauge
        // can never be inverted back to the account balance.
        assert_eq!(compute_headroom_pct(500_000, 500_000), 100.0);
        assert_eq!(compute_headroom_pct(500_000_000, 500_000), 100.0);
        // Below the floor it is proportional (floor-blocked visibility).
        assert_eq!(compute_headroom_pct(250_000, 500_000), 50.0);
        assert_eq!(compute_headroom_pct(0, 500_000), 0.0);
        // Negative usable clamps to 0; a zero floor never divides by 0.
        assert_eq!(compute_headroom_pct(-1, 500_000), 0.0);
        assert_eq!(compute_headroom_pct(1_000, 0), 100.0);
    }

    #[test]
    fn test_usable_after_buffer_matches_evaluate_formula() {
        // The ONE shared buffer formula: 2,500,000 × 80% = 2,000,000
        // (the same numbers the evaluate boundary tests pin).
        assert_eq!(usable_paise_after_buffer(2_500_000, 20), 2_000_000);
        assert_eq!(usable_paise_after_buffer(-10_000, 20), 0);
        assert_eq!(usable_paise_after_buffer(2_500_000, 255), 0);
        // GENUINE cross-check (refuter R1-4): the usable number evaluate
        // records must equal this helper's output for the same
        // (avail, buffer) inputs — across several buffers, incl. a debit
        // balance.
        for (avail, buffer) in [
            (2_500_000_i64, 20_u8),
            (2_500_000, 0),
            (2_500_000, 90),
            (999_999, 33),
            (-10_000, 20),
        ] {
            let cfg = GrowwMarginGateParams {
                enabled: true,
                buffer_pct: buffer,
                floor_paise: 0,
                ..GrowwMarginGateParams::default()
            };
            let snap = GrowwMarginSnapshot {
                option_buy_avail_paise: avail,
                ..fresh_snapshot(NOW, TODAY)
            };
            let verdict = evaluate(Some(&snap), entry(1_000), 0, &cfg, NOW, TODAY);
            let numbers = match verdict {
                GrowwMarginVerdict::Pass(n)
                | GrowwMarginVerdict::WouldReject(n)
                | GrowwMarginVerdict::RejectedInsufficient(n) => n,
                other => panic!("expected an arithmetic verdict, got {other:?}"),
            };
            assert_eq!(
                numbers.usable_paise,
                usable_paise_after_buffer(avail, buffer),
                "evaluate must use THE shared buffer formula (avail={avail}, buffer={buffer})"
            );
        }
    }

    // -- proptest: totality + monotonicity --------------------------------------

    proptest::proptest! {
        #[test]
        fn prop_evaluate_never_panics(
            required in proptest::num::i64::ANY,
            avail in proptest::num::i64::ANY,
            pending in proptest::num::i64::ANY,
            age_offset in 0_u64..1_000_000,
            buffer in 0_u8..=255,
            date in proptest::num::u32::ANY,
        ) {
            let cfg = GrowwMarginGateParams {
                enabled: true,
                buffer_pct: buffer,
                ..GrowwMarginGateParams::default()
            };
            let snap = GrowwMarginSnapshot {
                fetched_at_epoch_secs: NOW.saturating_sub(age_offset),
                fetch_initiated_at_epoch_secs: NOW.saturating_sub(age_offset),
                fetched_on_ist_date: date,
                option_buy_avail_paise: avail,
                clear_cash_paise: None,
                net_margin_used_paise: None,
            };
            // Total function: never panics on arbitrary inputs.
            let _ = evaluate(Some(&snap), entry(required), pending, &cfg, NOW, TODAY);
            let _ = evaluate(None, entry(required), pending, &cfg, NOW, TODAY);
        }

        #[test]
        fn prop_more_pending_never_flips_reject_to_pass(
            required in 500_i64..10_000_000,
            avail in 0_i64..100_000_000,
            pending in 0_i64..10_000_000,
            extra in 1_i64..10_000_000,
        ) {
            let cfg = enabled_cfg();
            let snap = GrowwMarginSnapshot {
                option_buy_avail_paise: avail,
                ..fresh_snapshot(NOW, TODAY)
            };
            let base = evaluate(Some(&snap), entry(required), pending, &cfg, NOW, TODAY);
            let more = evaluate(
                Some(&snap),
                entry(required),
                pending.saturating_add(extra),
                &cfg,
                NOW,
                TODAY,
            );
            // Monotonic: adding pending can never turn a refusal into a Pass.
            if matches!(base, GrowwMarginVerdict::WouldReject(_)) {
                proptest::prop_assert!(matches!(more, GrowwMarginVerdict::WouldReject(_)));
            }
        }
    }

    // -- ledger (HOLD-EXPIRY-ONLY — no balance-reflection release) --------------

    fn ledger_entry(at: u64, required: i64) -> LedgerEntry {
        LedgerEntry {
            approved_at_epoch_secs: at,
            required_paise: required,
            reference_id: None,
        }
    }

    fn ledger_entry_ref(at: u64, required: i64, reference: &str) -> LedgerEntry {
        LedgerEntry {
            approved_at_epoch_secs: at,
            required_paise: required,
            reference_id: Some(reference.to_owned()),
        }
    }

    fn groww_filled_event(reference_id: Option<&str>) -> BrokerOrderEvent {
        BrokerOrderEvent {
            broker: Feed::Groww,
            broker_order_id: "GRW-1".to_owned(),
            reference_id: reference_id.map(str::to_owned),
            status: BrokerOrderStatus::Filled,
            raw_status: "EXECUTED".to_owned(),
            filled_qty: 75,
            remaining_qty: Some(0),
            avg_fill_price_paise: Some(1_333),
            segment: "FNO".to_owned(),
            exchange_ts_ms: None,
            received_at_ms: 0,
        }
    }

    #[test]
    fn test_ledger_push_and_sum() {
        let ledger = PendingLedger::new();
        assert!(ledger.is_empty());
        ledger.push(ledger_entry(NOW, 100_000)).unwrap();
        ledger.push(ledger_entry(NOW, 50_000)).unwrap();
        assert_eq!(ledger.len(), 2);
        // No age-out: full sum.
        assert_eq!(ledger.pending_consumed_paise(NOW, 120), 150_000);
    }

    #[test]
    fn test_ledger_push_refuses_non_positive_required() {
        // With balance-reflection gone there is no incidental pruning of
        // a poisoned entry — a non-positive reservation (which would
        // REDUCE the reserved sum, fail-open) is a TYPED refusal.
        let ledger = PendingLedger::new();
        assert_eq!(
            ledger.push(ledger_entry(NOW, 0)),
            Err(LedgerPushRefused::NonPositiveRequired)
        );
        assert_eq!(
            ledger.push(ledger_entry(NOW, -100_000)),
            Err(LedgerPushRefused::NonPositiveRequired)
        );
        assert!(ledger.is_empty(), "a refused push never lands");
        // A positive sibling is unaffected.
        ledger.push(ledger_entry(NOW, 100_000)).unwrap();
        assert_eq!(ledger.pending_consumed_paise(NOW, 120), 100_000);
    }

    #[test]
    fn test_ledger_cap_overflow_returns_err() {
        let ledger = PendingLedger::new();
        for i in 0..MARGIN_LEDGER_CAP {
            ledger
                .push(ledger_entry(NOW, i64::try_from(i).unwrap() + 1))
                .unwrap();
        }
        assert_eq!(ledger.len(), MARGIN_LEDGER_CAP);
        assert_eq!(
            ledger.push(ledger_entry(NOW, 1)),
            Err(LedgerPushRefused::Full)
        );
        assert_eq!(
            ledger.len(),
            MARGIN_LEDGER_CAP,
            "a refused push never grows"
        );
    }

    #[test]
    fn test_ledger_age_out_at_exactly_hold_secs_retained_plus_one_dropped() {
        let ledger = PendingLedger::new();
        ledger.push(ledger_entry(NOW, 100_000)).unwrap();
        // age == hold_secs: retained.
        assert_eq!(ledger.pending_consumed_paise(NOW + 120, 120), 100_000);
        // age == hold_secs + 1: dropped.
        assert_eq!(ledger.pending_consumed_paise(NOW + 121, 120), 0);
        assert!(ledger.is_empty(), "the aged-out entry is pruned");
    }

    #[test]
    fn test_ledger_no_release_before_hold_expiry_c1_narrative() {
        // The C1/C2 pin: two SAME-snapshot approvals; a balance drop
        // (ours OR a BruteX co-tenant drain) releases NOTHING — a
        // balance decrement is not even an input to the ledger anymore.
        // BOTH reservations are held for the FULL hold window and
        // release ONLY at hold-expiry.
        let ledger = PendingLedger::new();
        ledger.push(ledger_entry(NOW, 100_000)).unwrap();
        ledger.push(ledger_entry(NOW, 100_000)).unwrap();
        // Immediately after "a fill would have landed" on a fresh poll:
        // still both held.
        assert_eq!(ledger.pending_consumed_paise(NOW + 1, 120), 200_000);
        // Mid-window: still both held.
        assert_eq!(ledger.pending_consumed_paise(NOW + 60, 120), 200_000);
        // At the hold boundary: still both held.
        assert_eq!(ledger.pending_consumed_paise(NOW + 120, 120), 200_000);
        // One past the hold: both expire together (same approval instant).
        assert_eq!(ledger.pending_consumed_paise(NOW + 121, 120), 0);
    }

    #[test]
    fn test_ledger_mixed_ages_only_expired_entries_drop() {
        let ledger = PendingLedger::new();
        ledger.push(ledger_entry(NOW - 200, 50_000)).unwrap();
        ledger.push(ledger_entry(NOW, 100_000)).unwrap();
        // The 200s-old entry aged out (> 120); the fresh one is held.
        assert_eq!(ledger.pending_consumed_paise(NOW, 120), 100_000);
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn test_ledger_survives_snapshot_swap_never_zeroed() {
        // The H H1 non-reset pin: a cell store() has NO channel into the
        // ledger — entries persist across swaps until hold-expiry.
        let ledger = PendingLedger::new();
        let cell = MarginSnapshotCell::new();
        ledger.push(ledger_entry(NOW, 100_000)).unwrap();
        cell.store(fresh_snapshot(NOW + 1, TODAY));
        cell.store(fresh_snapshot(NOW + 2, TODAY));
        assert_eq!(ledger.len(), 1, "snapshot swaps never zero the ledger");
        assert_eq!(ledger.pending_consumed_paise(NOW + 3, 120), 100_000);
    }

    #[test]
    fn test_ledger_on_fill_event_exit_or_unmatched_fill_releases_nothing() {
        // The C3 pin: our EXIT orders are never pushed to the ledger, so
        // an exit fill arrives with a reference id matching NO entry (or
        // none at all) — it must release NOTHING. Same for a co-tenant /
        // amount-mismatched fill.
        let ledger = PendingLedger::new();
        ledger
            .push(ledger_entry_ref(NOW, 500_000, "entry-1"))
            .unwrap();
        // Exit fill: no reference id → nothing released.
        ledger.on_fill_event(&groww_filled_event(None));
        assert_eq!(ledger.len(), 1, "an id-less (exit) fill releases nothing");
        // A fill carrying a FOREIGN reference id → nothing released.
        ledger.on_fill_event(&groww_filled_event(Some("exit-77")));
        assert_eq!(ledger.len(), 1, "an unmatched fill releases nothing");
        // A Dhan fill never touches the Groww ledger.
        let mut dhan = groww_filled_event(Some("entry-1"));
        dhan.broker = Feed::Dhan;
        ledger.on_fill_event(&dhan);
        assert_eq!(ledger.len(), 1);
        // A non-terminal Groww event never releases.
        let mut open = groww_filled_event(Some("entry-1"));
        open.status = BrokerOrderStatus::Open;
        ledger.on_fill_event(&open);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.pending_consumed_paise(NOW + 1, 120), 500_000);
    }

    #[test]
    fn test_ledger_on_fill_event_id_matched_release_exact_entry_only() {
        let ledger = PendingLedger::new();
        ledger
            .push(ledger_entry_ref(NOW, 100_000, "entry-1"))
            .unwrap();
        ledger
            .push(ledger_entry_ref(NOW + 1, 50_000, "entry-2"))
            .unwrap();
        ledger.push(ledger_entry(NOW + 2, 25_000)).unwrap(); // id-less
        // The MATCHED entry releases — never the oldest, never an id-less
        // sibling.
        ledger.on_fill_event(&groww_filled_event(Some("entry-2")));
        assert_eq!(ledger.len(), 2);
        assert_eq!(
            ledger.pending_consumed_paise(NOW + 3, 120),
            125_000,
            "only the id-matched reservation was released"
        );
        // A repeat of the same fill is a no-op (already released).
        ledger.on_fill_event(&groww_filled_event(Some("entry-2")));
        assert_eq!(ledger.len(), 2);
        // On an empty ledger the hook never panics.
        let empty = PendingLedger::new();
        empty.on_fill_event(&groww_filled_event(Some("entry-1")));
        assert!(empty.is_empty());
    }

    #[test]
    fn test_ledger_push_refuses_duplicate_reference_id_replay_cannot_drain_siblings() {
        // Refuter R1-1: two live reservations under ONE reference id would
        // let a REPLAYED terminal fill (poll re-observation is the normal
        // case) drain one sibling per replay. The duplicate is refused at
        // push time, so the replay-drain is structurally impossible.
        let ledger = PendingLedger::new();
        ledger.push(ledger_entry_ref(NOW, 100_000, "dup")).unwrap();
        assert_eq!(
            ledger.push(ledger_entry_ref(NOW, 200_000, "dup")),
            Err(LedgerPushRefused::DuplicateReference)
        );
        assert_eq!(ledger.len(), 1, "the duplicate push never lands");
        // Distinct ids and id-less entries are unaffected.
        ledger.push(ledger_entry_ref(NOW, 50_000, "other")).unwrap();
        ledger.push(ledger_entry(NOW, 25_000)).unwrap();
        ledger.push(ledger_entry(NOW, 25_000)).unwrap(); // id-less repeat OK
        assert_eq!(ledger.pending_consumed_paise(NOW, 120), 200_000);
        // The refuter's replay narrative, now single-shot by construction:
        // the first Filled releases THE one "dup" entry; the replay finds
        // no match and releases nothing further.
        ledger.on_fill_event(&groww_filled_event(Some("dup")));
        assert_eq!(ledger.pending_consumed_paise(NOW, 120), 100_000);
        ledger.on_fill_event(&groww_filled_event(Some("dup")));
        assert_eq!(
            ledger.pending_consumed_paise(NOW, 120),
            100_000,
            "a replayed fill must release nothing further"
        );
        // After the release, the id may legitimately be reused.
        ledger.push(ledger_entry_ref(NOW, 10_000, "dup")).unwrap();
        assert_eq!(ledger.pending_consumed_paise(NOW, 120), 110_000);
    }

    #[test]
    fn test_ledger_concurrent_push_under_mutex_no_loss() {
        let ledger = Arc::new(PendingLedger::new());
        let a = Arc::clone(&ledger);
        let b = Arc::clone(&ledger);
        let t1 = std::thread::spawn(move || {
            for _ in 0..20 {
                a.push(ledger_entry(NOW, 1)).unwrap();
            }
        });
        let t2 = std::thread::spawn(move || {
            for _ in 0..20 {
                b.push(ledger_entry(NOW, 1)).unwrap();
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(ledger.len(), 40);
        assert_eq!(ledger.pending_consumed_paise(NOW, 120), 40);
    }

    // -- validator + wire parsing -----------------------------------------------

    #[test]
    fn test_validator_happy_path_paise_exact() {
        let envelope: UserMarginEnvelope = serde_json::from_str(HAPPY_BODY).unwrap();
        let payload = envelope.payload.unwrap();
        let snap = validate_payload(&payload, NOW, NOW, TODAY).unwrap();
        assert_eq!(snap.option_buy_avail_paise, 2_500_025);
        assert_eq!(snap.clear_cash_paise, Some(4_125_050));
        assert_eq!(snap.net_margin_used_paise, Some(100_000));
        assert_eq!(snap.fetched_on_ist_date, TODAY);
    }

    #[test]
    fn test_validator_missing_decision_field_refuses() {
        // Nesting drift: the fno object exists but the field is absent.
        let payload: UserMarginPayload =
            serde_json::from_str(r#"{ "fno_margin_details": {}, "clear_cash": 100.0 }"#).unwrap();
        assert_eq!(
            validate_payload(&payload, NOW, NOW, TODAY),
            Err(ShapeIssue::MissingDecisionField)
        );
        // The whole fno object absent.
        let payload: UserMarginPayload =
            serde_json::from_str(r#"{ "clear_cash": 100.0 }"#).unwrap();
        assert_eq!(
            validate_payload(&payload, NOW, NOW, TODAY),
            Err(ShapeIssue::MissingDecisionField)
        );
    }

    #[test]
    fn test_validator_lenient_string_number_accepted() {
        let payload: UserMarginPayload = serde_json::from_str(
            r#"{ "fno_margin_details": { "option_buy_balance_available": "5000.25" } }"#,
        )
        .unwrap();
        let snap = validate_payload(&payload, NOW, NOW, TODAY).unwrap();
        assert_eq!(snap.option_buy_avail_paise, 500_025);
    }

    #[test]
    fn test_validator_type_drift_fails_whole_parse() {
        // A NON-numeric string fails the WHOLE deserialize (SEC-L2
        // mechanics) — classified `parse`, never coerced to 0.
        let result: Result<UserMarginPayload, _> = serde_json::from_str(
            r#"{ "fno_margin_details": { "option_buy_balance_available": "not-a-number" } }"#,
        );
        assert!(result.is_err(), "type drift must fail the whole parse");
    }

    #[test]
    fn test_validator_negative_and_ten_crore_sanity_refuse() {
        // Negative decision balance → refused (fail-closed, no clamp).
        let payload: UserMarginPayload = serde_json::from_str(
            r#"{ "fno_margin_details": { "option_buy_balance_available": -1.0 } }"#,
        )
        .unwrap();
        assert_eq!(
            validate_payload(&payload, NOW, NOW, TODAY),
            Err(ShapeIssue::Implausible(
                "non_finite_negative_or_implausible_balance"
            ))
        );
        // Above the ₹10-crore ceiling → refused.
        let payload: UserMarginPayload = serde_json::from_str(
            r#"{ "fno_margin_details": { "option_buy_balance_available": 200000000.0 } }"#,
        )
        .unwrap();
        assert_eq!(
            validate_payload(&payload, NOW, NOW, TODAY),
            Err(ShapeIssue::Implausible("above_ten_crore_ceiling"))
        );
        // Exactly ₹10 crore: accepted (boundary inclusive).
        let payload: UserMarginPayload = serde_json::from_str(
            r#"{ "fno_margin_details": { "option_buy_balance_available": 100000000.0 } }"#,
        )
        .unwrap();
        assert_eq!(
            validate_payload(&payload, NOW, NOW, TODAY)
                .unwrap()
                .option_buy_avail_paise,
            MARGIN_MAX_PLAUSIBLE_BALANCE_PAISE
        );
    }

    #[test]
    fn test_validator_observability_fields_lenient_never_gate() {
        // clear_cash absent + net_margin_used null: snapshot still lands.
        let payload: UserMarginPayload = serde_json::from_str(
            r#"{ "fno_margin_details": { "option_buy_balance_available": 100.0 },
                 "net_margin_used": null }"#,
        )
        .unwrap();
        let snap = validate_payload(&payload, NOW, NOW, TODAY).unwrap();
        assert_eq!(snap.clear_cash_paise, None);
        assert_eq!(snap.net_margin_used_paise, None);
    }

    #[test]
    fn test_validator_nan_string_and_huge_number_fail_closed() {
        // SEC-GM-4: a "NaN" STRING parses to f64::NAN — the is_finite()
        // arm must fail the WHOLE deserialize (never a stored
        // non-finite value).
        let result: Result<UserMarginPayload, _> = serde_json::from_str(
            r#"{ "fno_margin_details": { "option_buy_balance_available": "NaN" } }"#,
        );
        assert!(result.is_err(), "\"NaN\" string must fail the whole parse");
        let result: Result<UserMarginPayload, _> = serde_json::from_str(
            r#"{ "fno_margin_details": { "option_buy_balance_available": "inf" } }"#,
        );
        assert!(result.is_err(), "\"inf\" string must fail the whole parse");
        // A JSON-literal NaN is invalid JSON → the parse-class refusal
        // fires upstream of the validator.
        let result: Result<UserMarginPayload, _> = serde_json::from_str(
            r#"{ "fno_margin_details": { "option_buy_balance_available": NaN } }"#,
        );
        assert!(result.is_err(), "a JSON NaN literal is invalid JSON");
        // A finite-but-absurd 1e308 deserializes, then to_paise refuses
        // (> the ₹1e12 plausibility cap) — fail-closed at the validator.
        let payload: UserMarginPayload = serde_json::from_str(
            r#"{ "fno_margin_details": { "option_buy_balance_available": 1e308 } }"#,
        )
        .unwrap();
        assert_eq!(
            validate_payload(&payload, NOW, NOW, TODAY),
            Err(ShapeIssue::Implausible(
                "non_finite_negative_or_implausible_balance"
            ))
        );
    }

    #[test]
    fn test_shape_issue_stage_labels_stable() {
        assert_eq!(ShapeIssue::MissingDecisionField.stage(), "shape_incomplete");
        assert_eq!(ShapeIssue::Implausible("x").stage(), "sanity");
        assert_eq!(
            ShapeIssue::MissingDecisionField.detail(),
            "option_buy_balance_available_missing"
        );
    }

    // -- fetch / HTTP classification ---------------------------------------------

    #[tokio::test]
    async fn test_fetch_happy_path_parses_payload() {
        let (base, hits, handle) = start_margin_mock(200, HAPPY_BODY).await;
        let payload = fetch_user_margin_from(&test_client(), &base, &SecretString::from("t"))
            .await
            .unwrap();
        assert_eq!(
            payload
                .fno_margin_details
                .unwrap()
                .option_buy_balance_available,
            Some(25_000.25)
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn test_fetch_429_classified_rate_limited_single_attempt() {
        let (base, hits, handle) = start_margin_mock(429, "{}").await;
        let issue = fetch_user_margin_from(&test_client(), &base, &SecretString::from("t"))
            .await
            .unwrap_err();
        assert_eq!(issue, FetchIssue::RateLimited);
        assert_eq!(issue.stage(), "rate_limited");
        assert_eq!(issue.outcome(), "rate_limited");
        // ONE attempt — 429 is never retried/out-polled.
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn test_fetch_401_403_classified_auth_no_write_path() {
        for status in [401_u16, 403] {
            let (base, hits, handle) = start_margin_mock(status, "{}").await;
            let issue = fetch_user_margin_from(&test_client(), &base, &SecretString::from("t"))
                .await
                .unwrap_err();
            assert_eq!(issue, FetchIssue::AuthRejected { code: status });
            assert_eq!(issue.stage(), "auth");
            assert_eq!(issue.outcome(), "auth");
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            handle.abort();
        }
    }

    #[tokio::test]
    async fn test_fetch_500_classified_status() {
        let (base, _hits, handle) = start_margin_mock(500, r#"{"oops":true}"#).await;
        let issue = fetch_user_margin_from(&test_client(), &base, &SecretString::from("t"))
            .await
            .unwrap_err();
        assert!(matches!(issue, FetchIssue::Status { code: 500, .. }));
        assert_eq!(issue.stage(), "status");
        assert_eq!(issue.outcome(), "error");
        handle.abort();
    }

    #[tokio::test]
    async fn test_fetch_garbage_body_classified_parse() {
        let (base, _hits, handle) = start_margin_mock(200, "not json at all").await;
        let issue = fetch_user_margin_from(&test_client(), &base, &SecretString::from("t"))
            .await
            .unwrap_err();
        assert!(matches!(issue, FetchIssue::Parse { .. }));
        assert_eq!(issue.outcome(), "shape_drift");
        handle.abort();
    }

    #[tokio::test]
    async fn test_fetch_failure_envelope_classified() {
        let (base, _hits, handle) = start_margin_mock(200, FAILURE_ENVELOPE_BODY).await;
        let issue = fetch_user_margin_from(&test_client(), &base, &SecretString::from("t"))
            .await
            .unwrap_err();
        assert!(matches!(issue, FetchIssue::FailureEnvelope { .. }));
        assert_eq!(issue.stage(), "failure_envelope");
        assert_eq!(issue.outcome(), "shape_drift");
        handle.abort();
    }

    #[tokio::test]
    async fn test_fetch_transport_class_on_closed_connection() {
        let (base, hits, handle) = start_closing_mock().await;
        let issue = fetch_user_margin_from(&test_client(), &base, &SecretString::from("t"))
            .await
            .unwrap_err();
        assert!(matches!(issue, FetchIssue::Transport { .. }));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn test_fetch_oversize_pre_read_declared_content_length_refused() {
        // SEC-GM-2 arm 1: a DECLARED Content-Length over the cap is
        // refused from the header alone (the mock never sends a body —
        // a body read would hang until the client timeout, so a fast
        // Oversize here proves the pre-read guard fired).
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MARGIN_MAX_BODY_BYTES + 1
        )
        .into_bytes();
        let (base, handle) = start_raw_mock(raw).await;
        let issue = fetch_user_margin_from(&test_client(), &base, &SecretString::from("t"))
            .await
            .unwrap_err();
        assert_eq!(issue, FetchIssue::Oversize);
        assert_eq!(issue.stage(), "oversize");
        handle.abort();
    }

    #[tokio::test]
    async fn test_fetch_oversize_post_read_undeclared_body_refused() {
        // SEC-GM-2 arm 2: NO Content-Length (close-delimited body) over
        // the cap — the post-read `body.len()` cap refuses after
        // buffering (the documented chunked/close-delimited residual:
        // bounded by the request timeout, refused before parse).
        let mut raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                .to_vec();
        let body_len = usize::try_from(MARGIN_MAX_BODY_BYTES).unwrap() + 1;
        raw.resize(raw.len() + body_len, b'x');
        let (base, handle) = start_raw_mock(raw).await;
        let issue = fetch_user_margin_from(&test_client(), &base, &SecretString::from("t"))
            .await
            .unwrap_err();
        assert_eq!(issue, FetchIssue::Oversize);
        handle.abort();
    }

    #[test]
    fn test_fetch_issue_stage_and_outcome_labels_stable() {
        assert_eq!(FetchIssue::NoToken.stage(), "no_token");
        assert_eq!(FetchIssue::Timeout.stage(), "timeout");
        assert_eq!(FetchIssue::Oversize.stage(), "oversize");
        assert_eq!(
            FetchIssue::Transport {
                sanitized: String::new()
            }
            .stage(),
            "transport"
        );
        assert_eq!(FetchIssue::NoToken.outcome(), "error");
        assert_eq!(FetchIssue::Timeout.outcome(), "error");
        assert_eq!(FetchIssue::Oversize.outcome(), "error");
        assert_eq!(FetchIssue::NoToken.detail(), "");
    }

    #[test]
    fn test_fetch_url_const_is_gate5_sanctioned_literal() {
        // The endpoint path lives in THIS file (the Gate-5 carve-out) and
        // targets the documented user-margin detail route.
        assert!(GROWW_USER_MARGIN_URL.starts_with("https://api.groww.in/"));
        assert!(GROWW_USER_MARGIN_URL.ends_with("/v1/margins/detail/user"));
    }

    #[test]
    fn test_build_margin_http_client_builds() {
        let client = build_margin_http_client();
        assert!(client.is_ok(), "the hardened builder must construct");
    }

    // -- poll_turn -----------------------------------------------------------------

    /// Gate tests use an unroutable client + PANICKING token provider to
    /// prove ZERO token/REST work; the FULL fetch→validate→store
    /// composition is driven against local mocks through the private
    /// `poll_turn_from` URL seam (the `fetch_user_margin_from`
    /// discipline — the public `poll_turn` pins the Gate-5 const URL, so
    /// no test ever performs live HTTP).
    fn panicking_ctx<'a>(
        client: &'a reqwest::Client,
        tokens: &'a Arc<dyn TokenProvider>,
        cell: &'a MarginSnapshotCell,
        cfg: &'a GrowwMarginGateParams,
    ) -> PollContext<'a> {
        PollContext {
            client,
            tokens,
            cell,
            cfg,
        }
    }

    #[tokio::test]
    async fn test_poll_turn_gates_closed_zero_token_and_rest_work() {
        let client = test_client();
        let tokens: Arc<dyn TokenProvider> = Arc::new(PanickingTokens);
        let cell = MarginSnapshotCell::new();

        // Gate A: [groww_margin_gate] enabled = false.
        let disabled = GrowwMarginGateParams::default();
        let ctx = panicking_ctx(&client, &tokens, &cell, &disabled);
        assert_eq!(
            poll_turn(&ctx, true, true, NOW, TODAY).await,
            PollOutcome::SkippedGatesClosed
        );

        // Gate B: margin_read = false.
        let enabled = enabled_cfg();
        let ctx = panicking_ctx(&client, &tokens, &cell, &enabled);
        assert_eq!(
            poll_turn(&ctx, false, true, NOW, TODAY).await,
            PollOutcome::SkippedGatesClosed
        );

        // Gate C: outside market hours.
        assert_eq!(
            poll_turn(&ctx, true, false, NOW, TODAY).await,
            PollOutcome::SkippedGatesClosed
        );
        assert!(cell.load().is_none(), "nothing may have been stored");
    }

    #[tokio::test]
    async fn test_poll_turn_token_failure_classified_no_token() {
        let client = test_client();
        let tokens: Arc<dyn TokenProvider> = Arc::new(FailingTokens);
        let cell = MarginSnapshotCell::new();
        let cfg = enabled_cfg();
        let ctx = panicking_ctx(&client, &tokens, &cell, &cfg);
        assert_eq!(
            poll_turn(&ctx, true, true, NOW, TODAY).await,
            PollOutcome::Failed("no_token")
        );
        assert!(cell.load().is_none());
    }

    #[tokio::test]
    async fn test_poll_turn_success_path_fetch_validate_store() {
        // L1: the FULL poll composition — gates open → token → fetch →
        // validate → STORE — against a live local mock.
        let (base, hits, handle) = start_margin_mock(200, HAPPY_BODY).await;
        let client = test_client();
        let tokens: Arc<dyn TokenProvider> = Arc::new(StaticTokens);
        let cell = MarginSnapshotCell::new();
        let cfg = enabled_cfg();
        let ctx = panicking_ctx(&client, &tokens, &cell, &cfg);
        let outcome = poll_turn_from(&ctx, &base, true, true, NOW, TODAY).await;
        assert_eq!(outcome, PollOutcome::Stored);
        let snap = cell.load().expect("the snapshot must be stored");
        assert_eq!(snap.option_buy_avail_paise, 2_500_025);
        assert_eq!(
            snap.fetch_initiated_at_epoch_secs, NOW,
            "the injected pre-fetch instant is the initiated stamp"
        );
        assert!(
            snap.fetched_at_epoch_secs >= NOW,
            "the landed stamp never precedes the initiated stamp (P1)"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn test_poll_500_not_stored_prior_snapshot_retained() {
        // L1: a failed poll NEVER overwrites — the pre-populated
        // snapshot is retained and simply ages toward the stale edge.
        let (base, hits, handle) = start_margin_mock(500, r#"{"oops":true}"#).await;
        let client = test_client();
        let tokens: Arc<dyn TokenProvider> = Arc::new(StaticTokens);
        let cell = MarginSnapshotCell::new();
        cell.store(fresh_snapshot(NOW, TODAY));
        let cfg = enabled_cfg();
        let ctx = panicking_ctx(&client, &tokens, &cell, &cfg);
        let outcome = poll_turn_from(&ctx, &base, true, true, NOW + 60, TODAY).await;
        assert_eq!(outcome, PollOutcome::Failed("status"));
        let snap = cell
            .load()
            .expect("the PRIOR snapshot must be retained after a failed poll");
        assert_eq!(
            snap.fetched_at_epoch_secs, NOW,
            "the failed poll must not have replaced the prior snapshot"
        );
        assert_eq!(snap.option_buy_avail_paise, 2_500_000);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn test_transport_retry_exactly_one_bounded_retry() {
        // The closing mock produces a transport-class failure every time:
        // the production retry wrapper must attempt EXACTLY twice
        // (1 + 1 bounded retry) and surface the transport class.
        let (base, hits, handle) = start_closing_mock().await;
        let client = test_client();
        let token = SecretString::from("t");
        let issue = fetch_user_margin_with_transport_retry_from(
            &client,
            &base,
            &token,
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(issue, FetchIssue::Transport { .. }));
        assert_eq!(hits.load(Ordering::SeqCst), 2, "exactly 1 + 1 retry");
        handle.abort();
    }

    #[tokio::test]
    async fn test_transport_retry_wrapper_never_retries_non_transport_classes() {
        // The retry wrapper retries ONLY the transport class: a
        // RateLimited (429) result returns after ONE attempt, and a
        // happy-path payload also completes in ONE attempt.
        let client = test_client();
        let token = SecretString::from("t");
        let (base429, hits429, handle429) = start_margin_mock(429, "{}").await;
        let issue = fetch_user_margin_with_transport_retry_from(
            &client,
            &base429,
            &token,
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert_eq!(issue, FetchIssue::RateLimited);
        assert_eq!(hits429.load(Ordering::SeqCst), 1, "429 is never retried");
        handle429.abort();
        let (base_ok, hits_ok, handle_ok) = start_margin_mock(200, HAPPY_BODY).await;
        let payload = fetch_user_margin_with_transport_retry_from(
            &client,
            &base_ok,
            &token,
            Duration::from_millis(1),
        )
        .await
        .expect("happy path through the retry wrapper");
        assert!(payload.fno_margin_details.is_some());
        assert_eq!(hits_ok.load(Ordering::SeqCst), 1, "success is one attempt");
        handle_ok.abort();
    }

    // -- edges ------------------------------------------------------------------

    #[test]
    fn test_failure_edge_fires_once_at_three_and_rearms_on_success() {
        let mut edge = FailureEdge::new();
        assert!(!edge.record_failure()); // 1
        assert!(!edge.record_failure()); // 2
        assert!(edge.record_failure()); // 3 — the edge fires
        assert!(!edge.record_failure()); // 4 — once per episode
        assert_eq!(edge.consecutive(), 4);
        assert!(edge.record_success()); // recovery reported
        assert!(!edge.record_success()); // steady-state success is silent
        assert!(!edge.record_failure()); // 1 again
        assert!(!edge.record_failure()); // 2
        assert!(edge.record_failure()); // 3 — a NEW episode fires again
    }

    #[test]
    fn test_stale_edge_rising_falling_only() {
        let mut edge = StaleEdge::new();
        assert_eq!(edge.observe(false), None);
        assert_eq!(edge.observe(true), Some(true)); // rising
        assert_eq!(edge.observe(true), None); // held
        assert_eq!(edge.observe(false), Some(false)); // falling
        assert_eq!(edge.observe(false), None);
    }

    // -- verdict surface -----------------------------------------------------------

    fn sample_numbers() -> EvalNumbers {
        EvalNumbers {
            required_paise: 1,
            avail_paise: 2,
            usable_paise: 1,
            pending_consumed_paise: 0,
            headroom_paise: 0,
            snapshot_age_secs: 3,
        }
    }

    #[test]
    fn test_verdict_as_str_labels_stable() {
        let n = sample_numbers();
        assert_eq!(
            GrowwMarginVerdict::ExitAlwaysAllowed.as_str(),
            "exit_allowed"
        );
        assert_eq!(
            GrowwMarginVerdict::SkippedDisabled.as_str(),
            "skipped_disabled"
        );
        assert_eq!(GrowwMarginVerdict::Pass(n).as_str(), "pass");
        assert_eq!(GrowwMarginVerdict::WouldReject(n).as_str(), "would_reject");
        assert_eq!(
            GrowwMarginVerdict::RejectedInsufficient(n).as_str(),
            "rejected_insufficient"
        );
        assert_eq!(
            GrowwMarginVerdict::FailClosedStale { age_secs: 1 }.as_str(),
            "fail_closed_stale"
        );
        assert_eq!(
            GrowwMarginVerdict::FailClosedNoSnapshot.as_str(),
            "fail_closed_no_snapshot"
        );
        assert_eq!(
            GrowwMarginVerdict::FailClosedBadInput { reason: "x" }.as_str(),
            "fail_closed_bad_input"
        );
    }

    #[test]
    fn test_permits_live_entry_truth_table() {
        let n = sample_numbers();
        assert!(GrowwMarginVerdict::Pass(n).permits_live_entry());
        assert!(GrowwMarginVerdict::ExitAlwaysAllowed.permits_live_entry());
        // A DISABLED gate blocks live entries fail-closed.
        assert!(!GrowwMarginVerdict::SkippedDisabled.permits_live_entry());
        // An observe-mode would-reject blocks LIVE too (never a live pass).
        assert!(!GrowwMarginVerdict::WouldReject(n).permits_live_entry());
        assert!(!GrowwMarginVerdict::RejectedInsufficient(n).permits_live_entry());
        assert!(!GrowwMarginVerdict::FailClosedStale { age_secs: 1 }.permits_live_entry());
        assert!(!GrowwMarginVerdict::FailClosedNoSnapshot.permits_live_entry());
        assert!(!GrowwMarginVerdict::FailClosedBadInput { reason: "x" }.permits_live_entry());
    }

    #[test]
    fn test_permits_paper_entry_truth_table() {
        let n = sample_numbers();
        assert!(GrowwMarginVerdict::Pass(n).permits_paper_entry());
        assert!(GrowwMarginVerdict::ExitAlwaysAllowed.permits_paper_entry());
        // Feature-dark equals today's paper behaviour.
        assert!(GrowwMarginVerdict::SkippedDisabled.permits_paper_entry());
        // Observe-mode paper proceeds while the would-reject is recorded.
        assert!(GrowwMarginVerdict::WouldReject(n).permits_paper_entry());
        // Every enforce/fail-closed arm blocks paper too.
        assert!(!GrowwMarginVerdict::RejectedInsufficient(n).permits_paper_entry());
        assert!(!GrowwMarginVerdict::FailClosedStale { age_secs: 1 }.permits_paper_entry());
        assert!(!GrowwMarginVerdict::FailClosedNoSnapshot.permits_paper_entry());
        assert!(!GrowwMarginVerdict::FailClosedBadInput { reason: "x" }.permits_paper_entry());
    }

    // -- boot guard: the full 8-combo matrix ----------------------------------------

    #[test]
    fn test_boot_guard_all_eight_combinations() {
        let cfg = |enabled: bool, enforce: bool, ack: &str| GrowwMarginGateParams {
            enabled,
            enforce,
            live_without_margin_gate_operator_ack: ack.to_owned(),
            ..GrowwMarginGateParams::default()
        };
        // #1 paper / disabled / observe → OK (pre-feature baseline).
        assert_eq!(
            boot_guard_verdict(false, &cfg(false, false, "")),
            BootGuardVerdict::Allowed
        );
        // #2 paper / disabled / enforce → REFUSED (inconsistent, no waiver).
        assert_eq!(
            boot_guard_verdict(false, &cfg(false, true, "2026-07-15 quote")),
            BootGuardVerdict::Refused("enforce_without_enabled")
        );
        // #3 paper / enabled / observe → OK (Phase 1).
        assert_eq!(
            boot_guard_verdict(false, &cfg(true, false, "")),
            BootGuardVerdict::Allowed
        );
        // #4 paper / enabled / enforce → OK (Phase 2).
        assert_eq!(
            boot_guard_verdict(false, &cfg(true, true, "")),
            BootGuardVerdict::Allowed
        );
        // #5 live / disabled / observe → REFUSED unless the dated ack.
        assert_eq!(
            boot_guard_verdict(true, &cfg(false, false, "")),
            BootGuardVerdict::Refused("live_requires_enabled_and_enforce_or_dated_ack")
        );
        assert_eq!(
            boot_guard_verdict(true, &cfg(false, false, "2026-07-15 quote")),
            BootGuardVerdict::AllowedWithWaiver
        );
        // #6 live / disabled / enforce → REFUSED (inconsistent — the ack
        // can NEVER waive an inconsistent combo).
        assert_eq!(
            boot_guard_verdict(true, &cfg(false, true, "2026-07-15 quote")),
            BootGuardVerdict::Refused("enforce_without_enabled")
        );
        // #7 live / enabled / observe → REFUSED unless the dated ack.
        assert_eq!(
            boot_guard_verdict(true, &cfg(true, false, "")),
            BootGuardVerdict::Refused("live_requires_enabled_and_enforce_or_dated_ack")
        );
        assert_eq!(
            boot_guard_verdict(true, &cfg(true, false, "2026-07-15 quote")),
            BootGuardVerdict::AllowedWithWaiver
        );
        // #8 live / enabled / enforce → OK (Phase 3).
        assert_eq!(
            boot_guard_verdict(true, &cfg(true, true, "")),
            BootGuardVerdict::Allowed
        );
        // A whitespace-only ack is NOT a waiver.
        assert_eq!(
            boot_guard_verdict(true, &cfg(true, false, "   ")),
            BootGuardVerdict::Refused("live_requires_enabled_and_enforce_or_dated_ack")
        );
    }

    // -- snapshot cell -------------------------------------------------------------

    #[test]
    fn test_snapshot_cell_load_store_swap() {
        let cell = MarginSnapshotCell::new();
        assert!(cell.load().is_none());
        cell.store(fresh_snapshot(NOW, TODAY));
        assert_eq!(cell.load().unwrap().fetched_at_epoch_secs, NOW);
        // A newer store swaps in place (single-value semantics).
        cell.store(fresh_snapshot(NOW + 60, TODAY));
        assert_eq!(cell.load().unwrap().fetched_at_epoch_secs, NOW + 60);
    }

    // -- poll loop (smoke: gates closed => zero REST; shutdown exits) ----------------

    #[tokio::test]
    async fn test_run_margin_poll_loop_gates_closed_zero_rest_then_shutdown() {
        // margin_read = false: the loop must tick (the interval's FIRST
        // tick fires immediately, and poll_secs = 1 real second gives a
        // second turn) without touching the (panicking) token provider
        // or the network, then exit promptly on shutdown.
        let client = test_client();
        let tokens: Arc<dyn TokenProvider> = Arc::new(PanickingTokens);
        let cell = Arc::new(MarginSnapshotCell::new());
        let mut cfg = enabled_cfg();
        cfg.poll_secs = 1; // real-time test cadence (validate() is a boot gate)
        let margin_read = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(run_margin_poll_loop(
            client,
            tokens,
            Arc::clone(&cell),
            cfg,
            Arc::clone(&margin_read),
            || true,
            shutdown_rx,
        ));
        // Let the immediate first tick + one 1s tick elapse in real time.
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        assert!(cell.load().is_none(), "gates closed: nothing stored");
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the loop must exit promptly on shutdown")
            .unwrap();
    }
}
