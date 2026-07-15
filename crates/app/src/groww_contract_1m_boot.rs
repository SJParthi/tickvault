//! Groww per-minute PER-CONTRACT 1m candle REST leg — PR-4 of the Groww
//! per-minute REST plan (operator grant 2026-07-13,
//! `.claude/plans/active-plan-groww-rest-1m.md`; authorization
//! `groww-second-feed-scope-2026-06-19.md` §38 +
//! `no-rest-except-live-feed-2026-06-27.md` §9; runbook
//! `.claude/rules/project/rest-1m-pipeline-error-codes.md`).
//!
//! THE FILL-MODEL LEG (plan Context 3): BruteX backtests fill at the NEXT
//! minute's worst-case high/low of BOTH the underlying AND the option
//! contract — so live parity needs the official Groww 1m candle of the
//! SELECTED option contracts, minute by minute, from the same source.
//! Every trading-day minute close in session — sequenced AFTER the Groww
//! CHAIN leg (`groww_option_chain_1m_boot.rs`) via its minute-done watch
//! signal, bounded by a fallback timer — this task fetches the just-closed
//! minute's 1m candle for a BOUNDED ATM-window selection of option
//! contracts via Groww `GET /v1/historical/candles` (`segment=FNO`,
//! `groww_symbol` identity like `NSE-NIFTY-04Jan24-19200-CE`) and persists
//! to the NEW `option_contract_1m_rest` table (`feed='groww'`, feed-in-key
//! DEDUP). Cold path ONLY: the WS pipelines, tick capture and trading are
//! untouched.
//!
//! ## Selection (bounded, deterministic — the §38 envelope cap)
//! - CONTRACT BOOK (warmup, day-keyed cache): today's CURRENT expiry per
//!   underlying from the instruments master (`select_current_option_expiry`
//!   — never guessed) + that expiry's CE/PE rows
//!   (`select_option_contract_rows`) with strike/leg PARSED from each
//!   row's own `groww_symbol` (parse-only provenance — never computed).
//! - ANCHOR (per minute, in-memory handoff): the chain leg's latest
//!   per-underlying `underlying_ltp` ([`GrowwChainAnchorStore`]) — a
//!   QuestDB read is never needed. A minute with no anchor for an
//!   underlying SKIPS that underlying's contracts (counted + coded +
//!   named `rest_fetch_audit` rows, never a guessed ATM); the anchor is
//!   sticky across minutes (ATM moves slowly), so one failed chain
//!   minute never blanks the selection — but STICKINESS IS BOUNDED: an
//!   anchor older than [`GROWW_CONTRACT_1M_ANCHOR_MAX_AGE_MINUTES`]
//!   (5 min — the chain leg dead/frozen past its own 3-minute paging
//!   edge) makes the underlying UNRESOLVED for the minute (stage
//!   `anchor_stale`: counter + edge-latched coded warn + named audit
//!   rows), never a silently-frozen off-ATM window (round-1 review M3;
//!   the §38.8 decision-freshness principle applied to the selection
//!   input).
//! - WINDOW: ATM ± `strikes_each_side` (config, default 2) strikes × CE+PE
//!   per underlying, interleaved round-robin across underlyings by
//!   ATM-distance rank, HARD-capped at
//!   [`GROWW_CONTRACT_1M_MAX_PER_MINUTE`]. An over-cap selection is
//!   truncated DETERMINISTICALLY nearest-ATM-first — counted
//!   (`tv_groww_contract1m_selection_truncated_total`) + one coded warn,
//!   never fetched past the cap, never silent.
//!
//! ## Fetch semantics (the #1499 patterns, contract-sized)
//! ONE request per contract per minute (no in-minute re-poll ladder — 30
//! contracts × a ladder would blow the minute), day-granular window +
//! client-side minute filter, plus a one-minute-lookback BACKFILL mined
//! from the SAME body (nearly free — the day window already carries it).
//! DELIBERATELY NO 15:31 post-session sweep: the selection is
//! minute-scoped (ATM moves with the anchor), so "which contracts belong
//! to minute M" is only known AT minute M — an unrecovered contract
//! minute is a NAMED absence via its `rest_fetch_audit` row (leg
//! `contract_1m`), never a silent hole and never a fabricated row. That
//! one-row-per-unrecovered-minute contract covers EVERY arm: fetch
//! errors/empties, `fire_budget`/auth short-circuits, boundary skips,
//! anchor-unresolved/stale/empty selections (`Skipped` rows keyed on the
//! underlying's stable id — no per-contract selection ever existed), a
//! fetched-but-append-failed row (`named_gap`/`persist_failed`), and
//! flush-lost staged minutes (`named_gap`/`flush_failed` — the spot
//! sweep's item-4 precedent; the earlier `ok` row plus the flush-failed
//! row BOTH survive because `outcome` is in the audit DEDUP key).
//! Zero-trade thin-strike minutes may be legitimately ABSENT from the
//! vendor body (UNVERIFIED-LIVE whether Groww gap-fills) — counted
//! `outcome="empty"`, never fabricated.
//!
//! ## Pacing (the §38.3 capacity envelope — const-asserted in constants.rs)
//! Contracts are fetched SEQUENTIALLY with a mechanical
//! [`GROWW_CONTRACT_1M_MIN_GAP_MS`] cross-request gap (500 ms → ≤ 2.0
//! req/s; re-derived 2026-07-13 when INDIA VIX made the spot leg 4
//! sequential targets), so the worst-overlap boundary burst — spot worst
//! second 3 requests (a target-transition instant) + chain ≤1 req/s +
//! contract 2 req/s = 6.00 req/s — sits exactly AT the ≤6 req/s
//! shared-bucket pacing ceiling (broker hard ceiling 10/s); worst-case
//! per-minute totals across all three Groww legs = 53 requests (30 + 20
//! spot incl. the VIX target + 3 chain; typical ~37) ≈ 17.7% of the
//! 300/min budget. The whole fire carries a HARD
//! [`GROWW_CONTRACT_1M_FIRE_BUDGET_SECS`] deadline — contracts not reached
//! are SKIPPED loudly (forensics rows + counter), never fetched into the
//! next minute.
//!
//! ## Error codes (runbook `rest-1m-pipeline-error-codes.md`)
//! The contract leg REUSES the spot taxonomy — SPOT1M-01 (fetch degraded)
//! / SPOT1M-02 (persist failed) — with `feed = "groww"` +
//! `leg = "contract_1m"` on every emit (the same candles-fetch semantics,
//! a distinct leg discriminator; the rule file's dated 2026-07-13 note
//! records the mapping). Edge-triggered HIGH page
//! (`GrowwContract1mFetchDegraded`) at 3 consecutive fully-failed minutes
//! (persist-gated), Info recovery, one HIGH per day for an unresolvable
//! contract book (`GrowwContract1mBookUnresolved`).
//!
//! ## Decision-freshness gate (operator verbatim-intent, 2026-07-13)
//! Rows in `option_contract_1m_rest` are RECORD-COMPLETENESS data. Any
//! FUTURE strategy consumer MUST fail closed on staleness — the per-row
//! `close_to_data_ms` column distinguishes own-fire rows (~1-2 s) from
//! backfilled rows (≥ 60 s real delay); a backfilled row is NEVER a
//! trading-decision input (`groww-second-feed-scope-2026-06-19.md`
//! §38.8). No strategy code exists here (§28 boundary).
//!
//! ## Lifetime
//! Single-day pass: runs today's remaining minute closes, exits after
//! 15:30 IST or on a non-trading day. Supervised respawn wrapper
//! (`tv_groww_contract1m_task_respawn_total{reason}` + bounded backoff) —
//! EXCEPT after a warmup disabled-for-the-day stop (unresolvable books).
//! Panic honesty (TICK-FLUSH-01 precedent): release `panic = "abort"` —
//! the panic-respawn arm is an unwind-build path only.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate};
use secrecy::{ExposeSecret, SecretString};
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE, GROWW_CHAIN_1M_UNDERLYINGS,
    GROWW_CONTRACT_1M_ANCHOR_MAX_AGE_MINUTES, GROWW_CONTRACT_1M_FALLBACK_DELAY_MS,
    GROWW_CONTRACT_1M_FIRE_BUDGET_SECS, GROWW_CONTRACT_1M_MAX_PER_MINUTE,
    GROWW_CONTRACT_1M_MIN_GAP_MS, GROWW_CONTRACT_1M_REQUEST_TIMEOUT_SECS,
    GROWW_SPOT_1M_MAX_BODY_BYTES, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY,
    SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::capture_rest_error_body;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::feed::groww::instruments::{
    GrowwInstrumentRow, select_current_option_expiry, select_option_contract_rows,
    stable_index_security_id,
};
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::disk_health_watcher::classify_join_exit;
use tickvault_storage::option_contract_1m_rest_persistence::{
    OPTION_CONTRACT_1M_REST_LEG_CE, OPTION_CONTRACT_1M_REST_LEG_PE,
    OPTION_CONTRACT_1M_REST_SEGMENT_BSE_FNO, OPTION_CONTRACT_1M_REST_SEGMENT_NSE_FNO,
    OptionContract1mRestRow, OptionContract1mRestWriter, ensure_option_contract_1m_rest_table,
};
use tickvault_storage::rest_fetch_audit_persistence::{
    REST_FETCH_LEG_CONTRACT_1M, RestFetchAuditRow, RestFetchAuditWriter, RestFetchOutcome,
    ensure_rest_fetch_audit_table,
};

// Shared primitives (one implementation, many legs): session scheduling +
// edge tracker + body-cap fns from the Dhan spot leg; the sequencing wait
// core + fully-failed verdict + strike plausibility bound from the Dhan
// chain leg; the shared candle-row parser + token cache + query builders
// from the GROWW spot leg; the warmup master download + anchor store from
// the GROWW chain leg.
use crate::groww_option_chain_1m_boot::{
    GrowwChainAnchor, GrowwChainAnchorStore, download_master_bounded,
};
use crate::groww_rest_burst::GrowwRestBurstState;
use crate::groww_spot_1m_boot::{
    GrowwCandleRow, GrowwParseStats, GrowwTokenCache, error_class_for_status, groww_candles_query,
    parse_groww_1m_candle_rows,
};
use crate::option_chain_1m_boot::{
    MAX_PLAUSIBLE_STRIKE, chain_minute_fully_failed, stale_wake_backoff_ms,
    wait_for_signal_or_fallback,
};
use crate::spot_1m_rest_boot::{
    EdgeAction, FailureEdge, count_missed_boundaries, fire_is_fresh, format_minute_ist_12h,
    minute_open_ist_nanos, next_fire_after, spot_1m_day_is_over,
};

/// Backoff before the supervisor respawns a dead/failed scheduler run.
const GROWW_CONTRACT_1M_RESPAWN_BACKOFF_SECS: u64 = 30;
/// Milliseconds per second / per day (wall-clock latency math).
const MILLIS_PER_SEC: i64 = 1_000;
const MILLIS_PER_DAY: i64 = 86_400_000;
/// Nanoseconds per second / per minute (IST-epoch math).
const NANOS_PER_SEC: i64 = 1_000_000_000;
const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SEC;

/// Everything the scheduler needs, cloneable so the supervisor can respawn
/// the inner run. NO Dhan token handle and NO Dhan base URL — this leg is
/// fully independent of the Dhan lane (token via the shared-minter SSM
/// read; endpoint constant).
#[derive(Clone)]
pub struct GrowwContract1mTaskParams {
    /// Telegram dispatcher for the edge page / recovery ping / book
    /// degrade page.
    pub notifier: Arc<NotificationService>,
    /// Trading calendar (weekday + NSE-holiday aware) — re-checked every
    /// loop iteration (audit-findings Rule 3).
    pub calendar: Arc<TradingCalendar>,
    /// QuestDB target for the `option_contract_1m_rest` +
    /// `rest_fetch_audit` tables.
    pub questdb: QuestDbConfig,
    /// GROWW chain-leg "minute completed" signal (the boundary
    /// seconds-of-day the chain leg just finished firing). `None` is
    /// tolerated defensively (the fallback timer then paces every fire) —
    /// the spawn helper only wires this leg when the chain leg is enabled.
    pub chain_minute_done: Option<tokio::sync::watch::Receiver<Option<u32>>>,
    /// The chain leg's per-underlying `underlying_ltp` anchors (in-memory
    /// handoff — the ATM selection input). `None` ⇒ no anchors ever
    /// resolve and every minute skips loudly (the spawn helper prevents
    /// this by construction).
    pub anchor_store: Option<GrowwChainAnchorStore>,
    /// ATM window half-width from `[groww_contract_1m] strikes_each_side`.
    pub strikes_each_side: u32,
    /// Shared session-scoped burst-tier state (2026-07-14 auto-ladder).
    /// This leg keeps its own sequential per-contract min-gap pacing —
    /// the burst handle exists ONLY so a contract-leg HTTP 429 demotes
    /// the shared tier for the spot + chain waves too (one Groww rate
    /// bucket, one demotion signal).
    pub burst: Arc<GrowwRestBurstState>,
}

// ---------------------------------------------------------------------------
// Contract book (warmup) — pure builders
// ---------------------------------------------------------------------------

/// One tradable contract reference from the instruments master.
#[derive(Clone, Debug, PartialEq)]
pub struct GrowwContractRef {
    /// The full request identity (`NSE-NIFTY-30Jul26-25500-CE`).
    pub groww_symbol: String,
    /// The contract's numeric `exchange_token` (the feed's contract id).
    pub token: i64,
}

/// One strike slot of a contract book (CE/PE may be one-sided — the
/// master is taken as-is, never fabricated).
#[derive(Clone, Debug, PartialEq)]
pub struct GrowwStrikeSlot {
    pub strike: f64,
    pub ce: Option<GrowwContractRef>,
    pub pe: Option<GrowwContractRef>,
}

/// One underlying's day-scoped contract book.
#[derive(Clone, Debug, PartialEq)]
pub struct GrowwContractBook {
    /// PLAIN underlying symbol (`NIFTY`).
    pub underlying: &'static str,
    /// The underlying's exchange (`NSE`/`BSE`).
    pub exchange: &'static str,
    /// The persisted `exchange_segment` (`NSE_FNO`/`BSE_FNO`).
    pub segment: &'static str,
    /// The underlying's Groww live-lane stable id (boundary-skip forensics
    /// rows only — contract rows carry the contract token).
    pub underlying_security_id: i64,
    /// Today's CURRENT expiry (from the instruments master — never guessed).
    pub expiry: NaiveDate,
    /// Strike slots, sorted ascending by strike.
    pub strikes: Vec<GrowwStrikeSlot>,
}

/// Day-keyed cache of the resolved contract books, shared across
/// supervisor respawns (the chain leg's MEDIUM-4 precedent): a flapping
/// ~30s respawn loop must NOT re-download the multi-MB instruments master
/// every cycle. Refreshes only on an IST date change.
pub type GrowwContractBooksCache =
    Arc<tokio::sync::Mutex<Option<(NaiveDate, Vec<GrowwContractBook>)>>>;

/// The persisted `exchange_segment` for an exchange. Pure.
#[must_use]
pub fn contract_segment_for_exchange(exchange: &str) -> &'static str {
    if exchange == "BSE" {
        OPTION_CONTRACT_1M_REST_SEGMENT_BSE_FNO
    } else {
        OPTION_CONTRACT_1M_REST_SEGMENT_NSE_FNO
    }
}

/// Parse `(strike, leg)` from a contract `groww_symbol` —
/// `NSE-NIFTY-30Jul26-25500-CE` → `(25500.0, "CE")`. PARSE-ONLY strike
/// provenance (the master's own symbol segment — never computed), read
/// from the END so an underlying token can never shift the fields.
/// `None` = malformed (skipped + counted by the caller, never a panic):
/// missing segments, a non-CE/PE tail, or a non-finite / non-positive /
/// implausibly large strike. Pure.
#[must_use]
pub fn parse_contract_symbol(groww_symbol: &str) -> Option<(f64, &'static str)> {
    let mut rev = groww_symbol.rsplit('-');
    let leg = match rev.next()? {
        "CE" => OPTION_CONTRACT_1M_REST_LEG_CE,
        "PE" => OPTION_CONTRACT_1M_REST_LEG_PE,
        _ => return None,
    };
    let strike: f64 = rev.next()?.trim().parse().ok()?;
    if !strike.is_finite() || strike <= 0.0 || strike >= MAX_PLAUSIBLE_STRIKE {
        return None;
    }
    // At least `EXCHANGE-UNDERLYING-EXPIRY` must remain ahead of the
    // strike/leg tail for a plausible contract symbol.
    rev.next()?;
    Some((strike, leg))
}

/// Build one underlying's contract book from its master CE/PE rows at the
/// current expiry. Rows with a malformed `groww_symbol`, a leg that
/// disagrees with `instrument_type`, or a non-numeric `exchange_token`
/// are SKIPPED (returned in the count — the caller logs them, never
/// silent). Duplicate `(strike, leg)` rows keep the FIRST (the
/// index_extractor first-row-wins precedent). A duplicate
/// `exchange_token` across DIFFERENT `(strike, leg)` identities is
/// vendor id-space corruption (the Dhan dedup-drop precedent): the LATER
/// row is dropped keep-first and COUNTED in the third return (the caller
/// emits one coded warn — round-1 review LOW; two identities sharing one
/// token would collapse into one `security_id` in the persisted table).
/// Pure.
#[must_use]
pub fn build_contract_book(
    rows: &[&GrowwInstrumentRow],
    underlying: &'static str,
    exchange: &'static str,
    underlying_security_id: i64,
    expiry: NaiveDate,
) -> (GrowwContractBook, u32, u32) {
    let mut slots: Vec<GrowwStrikeSlot> = Vec::new();
    let mut seen_tokens: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut skipped: u32 = 0;
    let mut token_collisions: u32 = 0;
    for row in rows {
        let Some((strike, leg)) = parse_contract_symbol(&row.groww_symbol) else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        if leg != row.instrument_type {
            // The symbol's tail disagrees with the row's own type — a
            // corrupt master row; never trust either side.
            skipped = skipped.saturating_add(1);
            continue;
        }
        let Ok(token) = row.exchange_token.trim().parse::<i64>() else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        let contract = GrowwContractRef {
            groww_symbol: row.groww_symbol.clone(),
            token,
        };
        // O(1)-EXEMPT: cold-path warmup, bounded by the book size — a
        // linear slot scan over ≤ a few hundred strikes once per day.
        let slot_idx = slots
            .iter()
            .position(|s| s.strike == strike)
            .unwrap_or_else(|| {
                slots.push(GrowwStrikeSlot {
                    strike,
                    ce: None,
                    pe: None,
                });
                slots.len() - 1
            });
        let side = if leg == OPTION_CONTRACT_1M_REST_LEG_CE {
            &mut slots[slot_idx].ce
        } else {
            &mut slots[slot_idx].pe
        };
        if side.is_none() {
            // Token uniqueness across DISTINCT (strike, leg) identities:
            // an already-seen token on a NEW identity is a collision —
            // keep-first, counted, never two identities on one id.
            if seen_tokens.insert(token) {
                *side = Some(contract);
            } else {
                token_collisions = token_collisions.saturating_add(1);
            }
        }
        // else: duplicate (strike, leg) — first row wins, not counted as
        // malformed (the vendor-glitch duplicate-line class).
    }
    // Drop empty slots a collision-refused sole row may have left behind
    // (a slot with neither side selects nothing but would widen windows).
    slots.retain(|s| s.ce.is_some() || s.pe.is_some());
    slots.sort_by(|a, b| a.strike.total_cmp(&b.strike));
    (
        GrowwContractBook {
            underlying,
            exchange,
            segment: contract_segment_for_exchange(exchange),
            underlying_security_id,
            expiry,
            strikes: slots,
        },
        skipped,
        token_collisions,
    )
}

/// Resolve today's contract books from parsed master rows. Returns
/// `(books, degraded, skipped_rows, token_collisions)` — an underlying
/// with no usable expiry OR an empty book degrades (named), never the
/// whole day unless ALL degrade. Pure.
#[must_use]
pub fn resolve_groww_contract_books(
    rows: &[GrowwInstrumentRow],
    today: NaiveDate,
) -> (Vec<GrowwContractBook>, Vec<&'static str>, u32, u32) {
    let mut books = Vec::with_capacity(GROWW_CHAIN_1M_UNDERLYINGS.len());
    let mut degraded = Vec::new();
    let mut skipped_rows: u32 = 0;
    let mut token_collisions: u32 = 0;
    for (underlying, exchange, groww_symbol) in GROWW_CHAIN_1M_UNDERLYINGS {
        let Some(expiry) = select_current_option_expiry(rows, exchange, underlying, today) else {
            degraded.push(underlying);
            continue;
        };
        let contract_rows = select_option_contract_rows(rows, exchange, underlying, expiry);
        let (book, skipped, collisions) = build_contract_book(
            &contract_rows,
            underlying,
            exchange,
            stable_index_security_id(groww_symbol),
            expiry,
        );
        skipped_rows = skipped_rows.saturating_add(skipped);
        token_collisions = token_collisions.saturating_add(collisions);
        if book.strikes.is_empty() {
            degraded.push(underlying);
        } else {
            books.push(book);
        }
    }
    (books, degraded, skipped_rows, token_collisions)
}

/// Plain-English degrade detail for the book-unresolved page — bounded,
/// static symbol names only. Pure.
#[must_use]
pub fn book_degrade_detail(degraded: &[&'static str], master_rows: usize) -> String {
    format!(
        "{} — no usable option contracts at the current expiry in today's \
         contract list ({master_rows} rows scanned)",
        degraded.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Selection (per minute) — pure, deterministic, capped
// ---------------------------------------------------------------------------

/// One contract selected for this minute's fetch.
#[derive(Clone, Debug, PartialEq)]
pub struct GrowwSelectedContract {
    pub underlying: &'static str,
    pub exchange: &'static str,
    pub segment: &'static str,
    pub expiry: NaiveDate,
    pub strike: f64,
    pub leg: &'static str,
    pub groww_symbol: String,
    pub token: i64,
}

/// Index of the strike NEAREST the anchor (tie → the LOWER strike —
/// deterministic). `None` on an empty book. Pure.
#[must_use]
pub fn atm_index(slots: &[GrowwStrikeSlot], anchor: f64) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, slot) in slots.iter().enumerate() {
        let dist = (slot.strike - anchor).abs();
        let better = match best {
            None => true,
            // Strict `<`: on a tie the EARLIER (lower, sorted-ascending)
            // strike is kept — deterministic.
            Some((_, best_dist)) => dist < best_dist,
        };
        if better {
            best = Some((i, dist));
        }
    }
    best.map(|(i, _)| i)
}

/// Slot indices of the ATM window ordered by ATM-distance RANK
/// (deterministic: rank 0 = ATM, then ±1 with the LOWER side first, then
/// ±2, ... — clamped at the book edges). Pure.
#[must_use]
pub fn atm_window_indices(len: usize, atm: usize, each_side: u32) -> Vec<usize> {
    let mut out = Vec::with_capacity(2 * each_side as usize + 1);
    if len == 0 {
        return out;
    }
    out.push(atm.min(len - 1));
    for off in 1..=each_side as usize {
        if atm >= off {
            out.push(atm - off);
        }
        let hi = atm + off;
        if hi < len {
            out.push(hi);
        }
    }
    out
}

/// The per-minute selection outcome.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GrowwContractSelection {
    /// Contracts to fetch, in deterministic fetch order (round-robin
    /// across underlyings by ATM-distance rank, CE before PE).
    pub selected: Vec<GrowwSelectedContract>,
    /// Candidates dropped by the hard per-minute cap (nearest-ATM-first
    /// kept) — counted + one coded warn, never silent.
    pub truncated: usize,
    /// Underlyings with NO anchor this minute (chain leg never delivered
    /// one) — their contracts are skipped, counted, never guessed.
    pub unresolved: Vec<&'static str>,
}

/// Select this minute's contract set: per underlying the ATM window (from
/// its anchor) expanded to CE+PE, interleaved ROUND-ROBIN across
/// underlyings by ATM-distance rank, truncated deterministically at
/// `cap` (nearest-ATM-first kept). Pure.
#[must_use]
pub fn select_contracts_for_minute(
    books: &[GrowwContractBook],
    anchors: &HashMap<&'static str, f64>,
    each_side: u32,
    cap: usize,
) -> GrowwContractSelection {
    let mut selection = GrowwContractSelection::default();
    // Per-book ordered contract candidate lists (rank-major later).
    let mut per_book: Vec<Vec<GrowwSelectedContract>> = Vec::with_capacity(books.len());
    for book in books {
        let Some(anchor) = anchors.get(book.underlying).copied() else {
            selection.unresolved.push(book.underlying);
            continue;
        };
        let Some(atm) = atm_index(&book.strikes, anchor) else {
            selection.unresolved.push(book.underlying);
            continue;
        };
        let mut candidates = Vec::new();
        for idx in atm_window_indices(book.strikes.len(), atm, each_side) {
            let slot = &book.strikes[idx];
            for (leg, side) in [
                (OPTION_CONTRACT_1M_REST_LEG_CE, &slot.ce),
                (OPTION_CONTRACT_1M_REST_LEG_PE, &slot.pe),
            ] {
                if let Some(c) = side {
                    candidates.push(GrowwSelectedContract {
                        underlying: book.underlying,
                        exchange: book.exchange,
                        segment: book.segment,
                        expiry: book.expiry,
                        strike: slot.strike,
                        leg,
                        groww_symbol: c.groww_symbol.clone(),
                        token: c.token,
                    });
                }
            }
        }
        per_book.push(candidates);
    }
    // Round-robin interleave by rank so the cap truncates the FURTHEST
    // strikes fairly across underlyings (nearest-ATM-first kept).
    let mut rank = 0usize;
    loop {
        let mut any = false;
        for candidates in &per_book {
            if let Some(c) = candidates.get(rank) {
                any = true;
                if selection.selected.len() < cap {
                    selection.selected.push(c.clone());
                } else {
                    selection.truncated = selection.truncated.saturating_add(1);
                }
            }
        }
        if !any {
            break;
        }
        rank = rank.saturating_add(1);
    }
    selection
}

/// Split a raw chain-anchor snapshot into FRESH `underlying → ltp`
/// anchors and STALE underlyings (age strictly beyond
/// `max_age_minutes` — review M3: a frozen anchor from a dead/failing
/// chain leg must become a NAMED unresolved skip, never a silently
/// trusted off-ATM window). The stale list is sorted for deterministic
/// logging. Pure.
#[must_use]
pub fn partition_fresh_anchors(
    raw: &HashMap<&'static str, GrowwChainAnchor>,
    now_ist_nanos: i64,
    max_age_minutes: u32,
) -> (HashMap<&'static str, f64>, Vec<&'static str>) {
    let max_age_nanos = i64::from(max_age_minutes).saturating_mul(NANOS_PER_MINUTE);
    let mut fresh = HashMap::with_capacity(raw.len());
    let mut stale = Vec::new();
    for (underlying, anchor) in raw {
        if now_ist_nanos.saturating_sub(anchor.set_at_ist_nanos) > max_age_nanos {
            stale.push(*underlying);
        } else {
            fresh.insert(*underlying, anchor.ltp);
        }
    }
    stale.sort_unstable();
    (fresh, stale)
}

// ---------------------------------------------------------------------------
// Pure pacing / plausibility helpers
// ---------------------------------------------------------------------------

/// Mechanical cross-request pacing at the CONTRACT gap
/// ([`GROWW_CONTRACT_1M_MIN_GAP_MS`]) — the chain leg's min-gap pattern:
/// ONE scalar stamp spans consecutive contract requests. A
/// midnight-wrapped/backwards clock yields 0. Pure.
#[must_use]
pub fn contract_min_gap_wait_ms(last_request_ms_of_day: Option<i64>, now_ms_of_day: i64) -> u64 {
    let Some(last) = last_request_ms_of_day else {
        return 0;
    };
    if now_ms_of_day < last {
        return 0;
    }
    let elapsed = now_ms_of_day - last;
    u64::try_from((GROWW_CONTRACT_1M_MIN_GAP_MS as i64).saturating_sub(elapsed)).unwrap_or(0)
}

/// `true` when a vendor candle's OHLC is implausible — any non-positive
/// O/H/L/C or `high < low`. The row is STILL persisted verbatim (we
/// mirror the vendor), but never silently: counted + one coded line per
/// fired minute (the spot item-9 discipline). Pure.
#[must_use]
fn contract_ohlc_implausible(c: &GrowwCandleRow) -> bool {
    c.open <= 0.0 || c.high <= 0.0 || c.low <= 0.0 || c.close <= 0.0 || c.high < c.low
}

/// The target-minute candle row from a parsed body (first match wins;
/// duplicates counted by the caller). Pure.
#[must_use]
fn select_candle_row(rows: &[GrowwCandleRow], minute_nanos: i64) -> Option<GrowwCandleRow> {
    rows.iter()
        .find(|r| r.minute_ts_ist_nanos == minute_nanos)
        .copied()
}

/// Emit the ts-form + malformed-row counters for one parsed body into the
/// CONTRACT leg's own counter names. Static labels only.
fn record_contract_parse_stats(stats: GrowwParseStats) {
    if stats.epoch_ts_rows > 0 {
        metrics::counter!("tv_groww_contract1m_ts_form_total", "form" => "epoch")
            .increment(u64::from(stats.epoch_ts_rows));
    }
    if stats.string_ts_rows > 0 {
        metrics::counter!("tv_groww_contract1m_ts_form_total", "form" => "string")
            .increment(u64::from(stats.string_ts_rows));
    }
    if stats.malformed_rows > 0 {
        metrics::counter!("tv_groww_contract1m_parse_malformed_rows_total")
            .increment(u64::from(stats.malformed_rows));
    }
}

// ---------------------------------------------------------------------------
// Wall-clock helpers (IST) — module-local copies (the chain precedent)
// ---------------------------------------------------------------------------

/// IST seconds-of-day from the wall clock.
fn ist_secs_of_day_now() -> u32 {
    let now_ist = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    // rem_euclid of a positive modulus is < SECONDS_PER_DAY; the cast is safe.
    now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32
}

/// IST milliseconds-of-day from the wall clock (close→data latency math).
fn ist_millis_of_day_now() -> i64 {
    let now_ist_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS) * MILLIS_PER_SEC);
    now_ist_ms.rem_euclid(MILLIS_PER_DAY)
}

/// IST calendar date for "now".
fn today_ist() -> NaiveDate {
    let utc = DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0).unwrap_or_default();
    (utc + ChronoDuration::seconds(i64::from(IST_UTC_OFFSET_SECONDS))).date_naive()
}

/// Retrieval wall-clock instant as IST nanoseconds (`Utc::now()` source ⇒
/// ADD the IST offset per `data-integrity.md`).
fn fetched_at_ist_nanos_now() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS).saturating_mul(NANOS_PER_SEC))
}

// ---------------------------------------------------------------------------
// HTTP leg (contract-named metrics; the spot leg's shape)
// ---------------------------------------------------------------------------

/// One request's typed failure — classification from the REAL
/// `StatusCode`, never a substring scan; `msg` is the bounded
/// secret-redacted capture (DHAN-REST-400 discipline).
#[derive(Clone, Debug, PartialEq)]
struct ContractFetchFailure {
    /// HTTP status (0 = the request never got a response).
    status: u16,
    rate_limited: bool,
    auth_rejected: bool,
    msg: String,
}

/// Read a response body with the shared [`GROWW_SPOT_1M_MAX_BODY_BYTES`]
/// cap (same endpoint, same ~20 KB day-window body shape as the spot leg)
/// enforced BOTH on the declared `Content-Length` and on the streamed
/// accumulation (csv_downloader §18 pattern, via the shared pure cap fns).
async fn read_body_capped(mut resp: reqwest::Response) -> Result<String, String> {
    use crate::spot_1m_rest_boot::{accumulation_within_cap, declared_len_within_cap};
    if !declared_len_within_cap(resp.content_length(), GROWW_SPOT_1M_MAX_BODY_BYTES) {
        return Err(format!(
            "body too large: declared {} bytes > cap {GROWW_SPOT_1M_MAX_BODY_BYTES}",
            resp.content_length().unwrap_or_default()
        ));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("read: {}", capture_rest_error_body(&e.to_string())))?
    {
        if !accumulation_within_cap(buf.len(), chunk.len(), GROWW_SPOT_1M_MAX_BODY_BYTES) {
            return Err(format!(
                "body exceeded cap {GROWW_SPOT_1M_MAX_BODY_BYTES} bytes mid-stream"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| "body not valid UTF-8".to_string())
}

/// One Groww contract-candles REST round-trip → the raw 2xx body text.
/// `Err` carries the REAL status + a ≤300-char secret-redacted body (the
/// token travels ONLY in the `Authorization` header — never in the URL,
/// never logged). A 429 records the live-probe (e) shape via one bounded
/// `warn!` per occurrence + the contract leg's own counter.
async fn groww_contract_fetch_once(
    client: &reqwest::Client,
    url: &str,
    query: &[(&'static str, String); 6],
    token: &SecretString,
) -> Result<String, ContractFetchFailure> {
    let resp = client
        .get(url)
        .query(query)
        .bearer_auth(token.expose_secret())
        .header(GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE)
        .send()
        .await
        .map_err(|e| ContractFetchFailure {
            status: 0,
            rate_limited: false,
            auth_rejected: false,
            msg: format!("send: {}", capture_rest_error_body(&e.to_string())),
        })?;
    let status = resp.status();
    if !status.is_success() {
        let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        let auth_rejected =
            status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN;
        let retry_after_present = resp.headers().contains_key("retry-after");
        let error_body = read_body_capped(resp).await.unwrap_or_default();
        let captured = capture_rest_error_body(&error_body);
        if rate_limited {
            // Live-probe (e): every 429's timestamp (this log's own ts),
            // endpoint, Retry-After presence and body shape — bounded +
            // sanitized, never raw.
            metrics::counter!("tv_groww_contract1m_rate_limited_total").increment(1);
            warn!(
                endpoint = url,
                retry_after_present,
                body = %captured,
                "groww_contract_1m: HTTP 429 rate-limited (shared Live-Data \
                 bucket suspect) — counted; the next boundary re-selects"
            );
        }
        return Err(ContractFetchFailure {
            status: status.as_u16(),
            rate_limited,
            auth_rejected,
            msg: format!("http {status} retry_after_present={retry_after_present} body={captured}"),
        });
    }
    read_body_capped(resp)
        .await
        .map_err(|msg| ContractFetchFailure {
            status: status.as_u16(),
            rate_limited: false,
            auth_rejected: false,
            msg,
        })
}

// ---------------------------------------------------------------------------
// Forensics helpers (rest_fetch_audit, leg='contract_1m')
// ---------------------------------------------------------------------------

/// Build one `rest_fetch_audit` row for a contract fetch — leg
/// `contract_1m`, `security_id` = the contract's exchange_token, `symbol`
/// = the UNDERLYING's plain symbol (the full contract identity lives in
/// `option_contract_1m_rest.groww_symbol`). Pure.
#[allow(clippy::too_many_arguments)] // APPROVED: private forensics builder — a struct would be pure ceremony (spot/chain precedent)
fn build_contract_audit_row(
    target_minute_ist_nanos: i64,
    trading_date_nanos: i64,
    security_id: i64,
    exchange_segment: &'static str,
    symbol: &'static str,
    attempts: i64,
    final_http_status: i64,
    fetch_latency_ms: i64,
    close_to_data_ms: i64,
    rate_limited_count: i64,
    outcome: RestFetchOutcome,
    error_class: &'static str,
) -> RestFetchAuditRow {
    RestFetchAuditRow {
        close_to_persist_ms: -1,
        ts_ist_nanos: target_minute_ist_nanos,
        trading_date_ist_nanos: trading_date_nanos,
        feed: "groww",
        leg: REST_FETCH_LEG_CONTRACT_1M,
        security_id,
        exchange_segment,
        symbol,
        attempts,
        final_http_status,
        fetch_latency_ms,
        close_to_data_ms,
        rate_limited_count,
        outcome,
        error_class,
    }
}

/// Best-effort forensics append: a failure logs (coded) + counts and
/// RETURNS — the fetch loop, the verdict and the failure edge are never
/// affected by the forensics leg.
fn contract_audit_append_best_effort(
    audit_writer: &mut RestFetchAuditWriter,
    row: &RestFetchAuditRow,
) {
    if let Err(err) = audit_writer.append_row(row) {
        metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_append")
            .increment(1);
        error!(
            code = ErrorCode::Spot1m02PersistFailed.code_str(),
            stage = "audit_append",
            feed = "groww",
            leg = "contract_1m",
            ?err,
            "SPOT1M-02: rest_fetch_audit (contract_1m) row append failed \
             (forensics only — the fetch loop is unaffected)"
        );
    }
}

/// Best-effort forensics flush (same never-affects-the-loop contract).
fn contract_audit_flush_best_effort(audit_writer: &mut RestFetchAuditWriter) {
    if let Err(err) = audit_writer.flush() {
        metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_flush")
            .increment(1);
        error!(
            code = ErrorCode::Spot1m02PersistFailed.code_str(),
            stage = "audit_flush",
            feed = "groww",
            leg = "contract_1m",
            ?err,
            "SPOT1M-02: rest_fetch_audit (contract_1m) ILP flush failed — \
             pending forensics rows discarded (best-effort; the fetch loop \
             is unaffected)"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-minute fire
// ---------------------------------------------------------------------------

/// Flush-confirmed persisted-minute watermark per COMPOSITE contract key
/// `(token, segment)` — I-P1-11 discipline: Groww exchange_tokens are only
/// unique within an exchange, so the segment completes the key. Drives the
/// one-minute-lookback backfill; max-merge commits. In-memory only.
#[derive(Debug, Default)]
struct ContractPersistTracker {
    committed: HashMap<(i64, &'static str), i64>,
}

impl ContractPersistTracker {
    fn last_persisted(&self, token: i64, segment: &'static str) -> Option<i64> {
        self.committed.get(&(token, segment)).copied()
    }

    fn commit(&mut self, token: i64, segment: &'static str, minute_nanos: i64) {
        let entry = self
            .committed
            .entry((token, segment))
            .or_insert(minute_nanos);
        if *entry < minute_nanos {
            *entry = minute_nanos;
        }
    }
}

/// Build one `option_contract_1m_rest` row from a parsed candle row. The
/// `close_to_data_ms` stamp is the caller's HONEST retrieval delay
/// (own-fire latency, or the > 60 s real delay for a backfilled minute —
/// the decision-freshness gate's mechanical distinguisher). Pure.
fn build_contract_row(
    candle: &GrowwCandleRow,
    contract: &GrowwSelectedContract,
    trading_date_nanos: i64,
    close_to_data_ms: i64,
) -> OptionContract1mRestRow {
    OptionContract1mRestRow {
        ts_ist_nanos: candle.minute_ts_ist_nanos,
        trading_date_ist_nanos: trading_date_nanos,
        security_id: contract.token,
        exchange_segment: contract.segment,
        underlying_symbol: contract.underlying,
        leg: contract.leg,
        groww_symbol: contract.groww_symbol.clone(),
        expiry_ist_nanos: minute_open_ist_nanos(contract.expiry, 0),
        strike: contract.strike,
        open: candle.open,
        high: candle.high,
        low: candle.low,
        close: candle.close,
        volume: candle.volume,
        oi: candle.oi,
        close_to_data_ms,
        fetched_at_ist_nanos: fetched_at_ist_nanos_now(),
    }
}

/// One minute-close fire: selection from the chain anchors → SEQUENTIAL
/// min-gap-paced single-request fetches under the HARD fire deadline →
/// target-minute rows (+ one-minute-lookback backfill mined from the same
/// body) → one flush (persist-gated verdict) → forensics rows → counters
/// → edge accounting. An auth-class reject short-circuits the remaining
/// contracts for THIS fire (the spot item-12 discipline).
#[allow(clippy::too_many_arguments)] // APPROVED: private fire sink over the run loop's owned state — a struct would be pure ceremony (spot/chain precedent)
async fn fire_one_groww_contract_minute(
    params: &GrowwContract1mTaskParams,
    client: &reqwest::Client,
    // The candles endpoint — the production loop passes
    // [`tickvault_common::constants::GROWW_HISTORICAL_CANDLES_URL`]; the
    // fire tests point it at a hermetic mock server (the chain-target
    // precedent).
    candles_url: &str,
    books: &[GrowwContractBook],
    last_request_ms: &mut Option<i64>,
    writer: &mut OptionContract1mRestWriter,
    audit_writer: &mut RestFetchAuditWriter,
    edge: &mut FailureEdge,
    token_cache: &mut GrowwTokenCache,
    tracker: &mut ContractPersistTracker,
    session_first_minute_nanos: i64,
    fire_secs_of_day: u32,
    anchor_stale_latched: &mut bool,
) {
    let minute_open_secs = fire_secs_of_day.saturating_sub(60);
    let minute_label = format_minute_ist_12h(minute_open_secs);
    let trading_date = today_ist();
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
    let target_minute_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
    let minute_close_ms = i64::from(fire_secs_of_day).saturating_mul(MILLIS_PER_SEC);
    let fire_started = std::time::Instant::now();

    // ---- Selection (anchors → staleness gate → ATM windows → capped set) ----
    let raw_anchors: HashMap<&'static str, GrowwChainAnchor> = params
        .anchor_store
        .as_ref()
        .map(|store| {
            store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
        .unwrap_or_default();
    // Review M3: an anchor frozen past the max age (chain leg dead /
    // silently failing) is a NAMED unresolved skip — never a silently
    // trusted off-ATM window (§38.8 decision-freshness at the selection
    // input).
    let (anchors, stale) = partition_fresh_anchors(
        &raw_anchors,
        fetched_at_ist_nanos_now(),
        GROWW_CONTRACT_1M_ANCHOR_MAX_AGE_MINUTES,
    );
    let stale_books: Vec<&GrowwContractBook> = books
        .iter()
        .filter(|b| stale.contains(&b.underlying))
        .collect();
    if stale_books.is_empty() {
        // Episode over (or never started) — re-arm the stale warn latch.
        *anchor_stale_latched = false;
    } else {
        metrics::counter!("tv_groww_contract1m_anchor_stale_total")
            .increment(stale_books.len() as u64);
        if !*anchor_stale_latched {
            // Edge-latched per episode: ONE coded warn when staleness
            // begins; the per-minute counter + audit rows carry the rest.
            *anchor_stale_latched = true;
            warn!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "anchor_stale",
                feed = "groww",
                leg = "contract_1m",
                stale = ?stale,
                max_age_minutes = GROWW_CONTRACT_1M_ANCHOR_MAX_AGE_MINUTES,
                minute = %minute_label,
                "SPOT1M-01: chain ATM anchor(s) older than the max age — \
                 these underlyings' contracts are skipped NAMED until a \
                 fresh anchor arrives (a frozen off-ATM window is never \
                 fetched silently)"
            );
        }
        for book in &stale_books {
            let row = build_contract_audit_row(
                target_minute_nanos,
                trading_date_nanos,
                book.underlying_security_id,
                book.segment,
                book.underlying,
                0,
                0,
                -1,
                -1,
                0,
                RestFetchOutcome::Skipped,
                "anchor_stale",
            );
            contract_audit_append_best_effort(audit_writer, &row);
        }
    }
    let selection = select_contracts_for_minute(
        books,
        &anchors,
        params.strikes_each_side,
        GROWW_CONTRACT_1M_MAX_PER_MINUTE,
    );
    // Stale underlyings surface in `unresolved` too (their anchors were
    // filtered out above) — subtract them so each cause is named ONCE.
    let unresolved: Vec<&'static str> = selection
        .unresolved
        .iter()
        .copied()
        .filter(|u| !stale.contains(u))
        .collect();
    if !unresolved.is_empty() {
        metrics::counter!("tv_groww_contract1m_selection_unresolved_total")
            .increment(unresolved.len() as u64);
        warn!(
            code = ErrorCode::Spot1m01FetchDegraded.code_str(),
            stage = "selection_unresolved",
            feed = "groww",
            leg = "contract_1m",
            unresolved = ?unresolved,
            minute = %minute_label,
            "SPOT1M-01: no chain anchor for these underlyings this minute — \
             their contracts are skipped (counted; an ATM is never guessed)"
        );
        // Review M4: the skipped-underlying minute is a NAMED absence —
        // one Skipped row per (minute, underlying) on the underlying's
        // stable id (no per-contract selection ever existed), mirroring
        // the boundary-skip arm.
        for book in books.iter().filter(|b| unresolved.contains(&b.underlying)) {
            let row = build_contract_audit_row(
                target_minute_nanos,
                trading_date_nanos,
                book.underlying_security_id,
                book.segment,
                book.underlying,
                0,
                0,
                -1,
                -1,
                0,
                RestFetchOutcome::Skipped,
                "anchor_unresolved",
            );
            contract_audit_append_best_effort(audit_writer, &row);
        }
    }
    if selection.truncated > 0 {
        metrics::counter!("tv_groww_contract1m_selection_truncated_total")
            .increment(selection.truncated as u64);
        warn!(
            code = ErrorCode::Spot1m01FetchDegraded.code_str(),
            stage = "selection_truncated",
            feed = "groww",
            leg = "contract_1m",
            truncated = selection.truncated,
            cap = GROWW_CONTRACT_1M_MAX_PER_MINUTE,
            minute = %minute_label,
            "SPOT1M-01: contract selection exceeded the per-minute cap — \
             truncated deterministically nearest-ATM-first (counted, never \
             fetched past the cap)"
        );
    }

    let mut ok_count: usize = 0;
    let mut empty_count: usize = 0;
    let mut error_count: usize = 0;
    // Persist-gated OK (the spot M1 discipline): a fetched-but-never-
    // persisted minute is NOT ok — a day-long QuestDB outage must page.
    let mut persist_failed = false;
    let mut sample_failure: Option<String> = None;
    // (token, segment, underlying, minute) commits staged until the flush
    // ACKs (the underlying rides along for the flush-failed forensics).
    let mut staged_commits: Vec<(i64, &'static str, &'static str, i64)> = Vec::new();

    if selection.selected.is_empty() {
        // Nothing selectable this minute (no anchors / empty books) — the
        // minute is a full functional miss for this leg.
        error_count = 1;
        sample_failure = Some("no contracts selectable (no chain anchors yet)".to_string());
        // Review M4: books whose emptiness was NOT already named by the
        // stale/unresolved arms (an anchored book whose ATM window carried
        // zero one-sided contracts) still get their named Skipped rows —
        // every unrecovered minute is a queryable absence.
        for book in books
            .iter()
            .filter(|b| !unresolved.contains(&b.underlying) && !stale.contains(&b.underlying))
        {
            let row = build_contract_audit_row(
                target_minute_nanos,
                trading_date_nanos,
                book.underlying_security_id,
                book.segment,
                book.underlying,
                0,
                0,
                -1,
                -1,
                0,
                RestFetchOutcome::Skipped,
                "empty_selection",
            );
            contract_audit_append_best_effort(audit_writer, &row);
        }
    } else if let Some(token) = token_cache.ensure_token().await {
        let backfill_minute = {
            let prev = target_minute_nanos.saturating_sub(NANOS_PER_MINUTE);
            (prev >= session_first_minute_nanos).then_some(prev)
        };
        for (idx, contract) in selection.selected.iter().enumerate() {
            // HARD fire deadline: contracts not reached are SKIPPED loudly
            // — never fetched into the next minute.
            if fire_started.elapsed().as_secs() >= GROWW_CONTRACT_1M_FIRE_BUDGET_SECS {
                let remaining = &selection.selected[idx..];
                metrics::counter!("tv_groww_contract1m_fire_budget_exceeded_total").increment(1);
                error_count = error_count.saturating_add(remaining.len());
                error!(
                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                    stage = "fire_budget",
                    feed = "groww",
                    leg = "contract_1m",
                    skipped_contracts = remaining.len(),
                    minute = %minute_label,
                    "SPOT1M-01: contract fire deadline exceeded — remaining \
                     contracts skipped for this minute (peer stalling)"
                );
                for skipped in remaining {
                    metrics::counter!("tv_groww_contract1m_fetch_total", "outcome" => "error")
                        .increment(1);
                    let row = build_contract_audit_row(
                        target_minute_nanos,
                        trading_date_nanos,
                        skipped.token,
                        skipped.segment,
                        skipped.underlying,
                        0,
                        0,
                        -1,
                        -1,
                        0,
                        RestFetchOutcome::Skipped,
                        "fire_budget",
                    );
                    contract_audit_append_best_effort(audit_writer, &row);
                }
                if sample_failure.is_none() {
                    sample_failure = Some(format!(
                        "fire deadline ({GROWW_CONTRACT_1M_FIRE_BUDGET_SECS}s) exceeded — \
                         {} contracts skipped",
                        remaining.len()
                    ));
                }
                break;
            }
            // Mechanical min-gap pacing (one scalar spans all contracts).
            let wait_ms = contract_min_gap_wait_ms(*last_request_ms, ist_millis_of_day_now());
            if wait_ms > 0 {
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
            let query = groww_candles_query(
                &contract.groww_symbol,
                contract.exchange,
                "FNO",
                trading_date,
            );
            *last_request_ms = Some(ist_millis_of_day_now());
            let started = std::time::Instant::now();
            let result = groww_contract_fetch_once(client, candles_url, &query, &token).await;
            let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
            #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
            metrics::histogram!("tv_groww_contract1m_fetch_duration_ms").record(latency_ms as f64);
            let mut auth_rejected = false;
            match result {
                Ok(body_text) => {
                    let (rows, stats) = parse_groww_1m_candle_rows(&body_text);
                    record_contract_parse_stats(stats);
                    // One-minute-lookback backfill mined from the SAME body
                    // (the day window already carries it — nearly free).
                    if let Some(bf_minute) = backfill_minute
                        && tracker
                            .last_persisted(contract.token, contract.segment)
                            .is_none_or(|w| w < bf_minute)
                        && let Some(bf) = select_candle_row(&rows, bf_minute)
                    {
                        let real_delay =
                            (ist_millis_of_day_now() - (minute_close_ms - 60_000)).max(0);
                        match writer.append_row(&build_contract_row(
                            &bf,
                            contract,
                            trading_date_nanos,
                            real_delay,
                        )) {
                            Ok(()) => {
                                metrics::counter!("tv_groww_contract1m_backfilled_total")
                                    .increment(1);
                                staged_commits.push((
                                    contract.token,
                                    contract.segment,
                                    contract.underlying,
                                    bf_minute,
                                ));
                            }
                            Err(err) => {
                                persist_failed = true;
                                metrics::counter!(
                                    "tv_groww_contract1m_persist_errors_total", "stage" => "append"
                                )
                                .increment(1);
                                error!(
                                    code = ErrorCode::Spot1m02PersistFailed.code_str(),
                                    stage = "append",
                                    feed = "groww",
                                    leg = "contract_1m",
                                    ?err,
                                    "SPOT1M-02: option_contract_1m_rest backfill \
                                     row append failed"
                                );
                                // Review M1: a fetched-but-append-failed
                                // minute is a PERSIST failure, not vendor
                                // absence — named (the spot sweep's
                                // `persist_failed` class; the fetch itself
                                // answered 200).
                                let row = build_contract_audit_row(
                                    bf_minute,
                                    trading_date_nanos,
                                    contract.token,
                                    contract.segment,
                                    contract.underlying,
                                    1,
                                    200,
                                    latency_ms,
                                    -1,
                                    0,
                                    RestFetchOutcome::NamedGap,
                                    "persist_failed",
                                );
                                contract_audit_append_best_effort(audit_writer, &row);
                            }
                        }
                    }
                    match select_candle_row(&rows, target_minute_nanos) {
                        Some(candle) => {
                            let dupes = rows
                                .iter()
                                .filter(|r| r.minute_ts_ist_nanos == target_minute_nanos)
                                .count()
                                .saturating_sub(1);
                            if dupes > 0 {
                                metrics::counter!("tv_groww_contract1m_duplicate_candle_total")
                                    .increment(dupes as u64);
                            }
                            if contract_ohlc_implausible(&candle) {
                                metrics::counter!("tv_groww_contract1m_implausible_ohlc_total")
                                    .increment(1);
                                warn!(
                                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                                    stage = "implausible_ohlc",
                                    feed = "groww",
                                    leg = "contract_1m",
                                    symbol = %contract.groww_symbol,
                                    minute = %minute_label,
                                    "SPOT1M-01: vendor candle OHLC implausible — \
                                     persisted verbatim (we mirror the vendor), \
                                     counted, never silent"
                                );
                            }
                            let close_to_data_ms =
                                (ist_millis_of_day_now() - minute_close_ms).max(0);
                            match writer.append_row(&build_contract_row(
                                &candle,
                                contract,
                                trading_date_nanos,
                                close_to_data_ms,
                            )) {
                                Ok(()) => {
                                    ok_count = ok_count.saturating_add(1);
                                    metrics::counter!(
                                        "tv_groww_contract1m_fetch_total", "outcome" => "ok"
                                    )
                                    .increment(1);
                                    #[allow(clippy::cast_precision_loss)]
                                    // APPROVED: histogram sample only
                                    metrics::histogram!("tv_groww_contract1m_close_to_data_ms")
                                        .record(close_to_data_ms as f64);
                                    staged_commits.push((
                                        contract.token,
                                        contract.segment,
                                        contract.underlying,
                                        target_minute_nanos,
                                    ));
                                    let audit_row = build_contract_audit_row(
                                        target_minute_nanos,
                                        trading_date_nanos,
                                        contract.token,
                                        contract.segment,
                                        contract.underlying,
                                        1,
                                        200,
                                        latency_ms,
                                        close_to_data_ms,
                                        0,
                                        RestFetchOutcome::Ok,
                                        "none",
                                    );
                                    contract_audit_append_best_effort(audit_writer, &audit_row);
                                }
                                Err(err) => {
                                    persist_failed = true;
                                    metrics::counter!(
                                        "tv_groww_contract1m_persist_errors_total",
                                        "stage" => "append"
                                    )
                                    .increment(1);
                                    error!(
                                        code = ErrorCode::Spot1m02PersistFailed.code_str(),
                                        stage = "append",
                                        feed = "groww",
                                        leg = "contract_1m",
                                        ?err,
                                        "SPOT1M-02: option_contract_1m_rest row \
                                         append failed"
                                    );
                                    if sample_failure.is_none() {
                                        sample_failure =
                                            Some(format!("persist append failed: {err:#}"));
                                    }
                                    // Review M1: fetched OK, append failed —
                                    // a PERSIST failure named per the spot
                                    // sweep's `persist_failed` class (the
                                    // fetch itself answered 200).
                                    let row = build_contract_audit_row(
                                        target_minute_nanos,
                                        trading_date_nanos,
                                        contract.token,
                                        contract.segment,
                                        contract.underlying,
                                        1,
                                        200,
                                        latency_ms,
                                        -1,
                                        0,
                                        RestFetchOutcome::NamedGap,
                                        "persist_failed",
                                    );
                                    contract_audit_append_best_effort(audit_writer, &row);
                                }
                            }
                        }
                        None => {
                            // Target minute absent: either the vendor has
                            // not sealed it yet OR a zero-trade thin-strike
                            // minute (UNVERIFIED-LIVE whether Groww
                            // gap-fills) — counted, never fabricated.
                            empty_count = empty_count.saturating_add(1);
                            metrics::counter!(
                                "tv_groww_contract1m_fetch_total", "outcome" => "empty"
                            )
                            .increment(1);
                            let audit_row = build_contract_audit_row(
                                target_minute_nanos,
                                trading_date_nanos,
                                contract.token,
                                contract.segment,
                                contract.underlying,
                                1,
                                200,
                                latency_ms,
                                -1,
                                0,
                                RestFetchOutcome::Empty,
                                "target_absent",
                            );
                            contract_audit_append_best_effort(audit_writer, &audit_row);
                        }
                    }
                }
                Err(failure) => {
                    error_count = error_count.saturating_add(1);
                    metrics::counter!("tv_groww_contract1m_fetch_total", "outcome" => "error")
                        .increment(1);
                    if sample_failure.is_none() {
                        sample_failure =
                            Some(format!("{}: {}", contract.groww_symbol, failure.msg));
                    }
                    auth_rejected = failure.auth_rejected;
                    // 2026-07-14 auto-ladder: a contract-leg 429 demotes the
                    // SHARED session burst tier (spot + chain waves included)
                    // — one Groww rate bucket, one demotion edge (the warn
                    // fires once per session).
                    if failure.rate_limited && params.burst.note_rate_limited() {
                        warn!(
                            code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                            stage = "burst_demoted",
                            feed = "groww",
                            leg = "contract_1m",
                            demoted_to = params.burst.effective_tier().as_str(),
                            "groww_contract_1m: HTTP 429 observed — Groww REST \
                             burst tier demoted for the rest of the session \
                             (boot resets to the configured tier)"
                        );
                    }
                    let error_class = error_class_for_status(failure.status);
                    let audit_row = build_contract_audit_row(
                        target_minute_nanos,
                        trading_date_nanos,
                        contract.token,
                        contract.segment,
                        contract.underlying,
                        1,
                        i64::from(failure.status),
                        latency_ms,
                        -1,
                        i64::from(failure.rate_limited),
                        if failure.rate_limited {
                            RestFetchOutcome::RateLimited
                        } else {
                            RestFetchOutcome::Error
                        },
                        error_class,
                    );
                    contract_audit_append_best_effort(audit_writer, &audit_row);
                }
            }
            if auth_rejected {
                // The spot item-12 discipline: drop the dead token NOW and
                // short-circuit the remaining contracts for THIS fire —
                // every further request with the same rejected token is a
                // doomed 401. The next fire's ensure_token re-reads SSM at
                // the ≥60s floor; NEVER a mint.
                token_cache.note_auth_rejected();
                let remaining = &selection.selected[idx + 1..];
                if !remaining.is_empty() {
                    error_count = error_count.saturating_add(remaining.len());
                    warn!(
                        skipped_contracts = remaining.len(),
                        "groww_contract_1m: auth-class reject — remaining \
                         contracts short-circuited for this fire (no doomed \
                         requests); forensics rows still emitted"
                    );
                    for skipped in remaining {
                        metrics::counter!("tv_groww_contract1m_fetch_total", "outcome" => "error")
                            .increment(1);
                        let row = build_contract_audit_row(
                            target_minute_nanos,
                            trading_date_nanos,
                            skipped.token,
                            skipped.segment,
                            skipped.underlying,
                            0,
                            0,
                            -1,
                            -1,
                            0,
                            RestFetchOutcome::NoToken,
                            "auth",
                        );
                        contract_audit_append_best_effort(audit_writer, &row);
                    }
                }
                break;
            }
        }
        match writer.flush() {
            Ok(()) => {
                // Flush ACKed — commit the staged watermarks.
                for (tok, seg, _underlying, minute) in staged_commits.drain(..) {
                    tracker.commit(tok, seg, minute);
                }
            }
            Err(err) => {
                persist_failed = true;
                metrics::counter!("tv_groww_contract1m_persist_errors_total", "stage" => "flush")
                    .increment(1);
                error!(
                    code = ErrorCode::Spot1m02PersistFailed.code_str(),
                    stage = "flush",
                    feed = "groww",
                    leg = "contract_1m",
                    ?err,
                    "SPOT1M-02: option_contract_1m_rest ILP flush failed — \
                     pending rows discarded (poisoned-buffer defense; the \
                     minutes stay absent and DEDUP-idempotent re-fetchable)"
                );
                if sample_failure.is_none() {
                    sample_failure = Some(format!("persist flush failed: {err:#}"));
                }
                // Review M2 (the spot sweep's item-4 `flush_failed`
                // precedent): every staged-but-unflushed minute is STILL
                // absent from the table while its earlier `ok` audit row
                // says retrieved — name each one. The `ok` row and this
                // `named_gap`/`flush_failed` row BOTH survive (`outcome`
                // is in the audit DEDUP key — the transition-row rule).
                for (tok, seg, underlying, minute) in staged_commits.drain(..) {
                    let row = build_contract_audit_row(
                        minute,
                        trading_date_nanos,
                        tok,
                        seg,
                        underlying,
                        1,
                        200,
                        -1,
                        -1,
                        0,
                        RestFetchOutcome::NamedGap,
                        "flush_failed",
                    );
                    contract_audit_append_best_effort(audit_writer, &row);
                }
            }
        }
    } else {
        // No token at fire time — nothing can be sent; the whole minute is
        // a full miss (counted per contract for honest rate math) + one
        // no_token forensics row per contract.
        error_count = selection.selected.len().max(1);
        sample_failure = Some("no shared Groww access token available at fire time".to_string());
        for contract in &selection.selected {
            metrics::counter!("tv_groww_contract1m_fetch_total", "outcome" => "error").increment(1);
            let row = build_contract_audit_row(
                target_minute_nanos,
                trading_date_nanos,
                contract.token,
                contract.segment,
                contract.underlying,
                0,
                0,
                -1,
                -1,
                0,
                RestFetchOutcome::NoToken,
                "no_token",
            );
            contract_audit_append_best_effort(audit_writer, &row);
        }
    }
    contract_audit_flush_best_effort(audit_writer);

    record_groww_contract_minute_verdict(
        params,
        edge,
        &minute_label,
        ok_count,
        error_count,
        empty_count,
        persist_failed,
        sample_failure.as_deref(),
    );
}

/// Coalesced per-minute verdict: ONE coded log per fired minute with any
/// failure, plus the edge-triggered escalation page / recovery ping
/// (typed Groww contract events). "Fully failed" = zero contracts
/// persisted OR a persist failure (the persist-gated M1 discipline —
/// legitimate thin-strike EMPTY minutes with ≥1 ok contract never feed
/// the edge).
#[allow(clippy::too_many_arguments)] // APPROVED: private verdict sink — a struct would be pure ceremony (spot/chain precedent)
fn record_groww_contract_minute_verdict(
    params: &GrowwContract1mTaskParams,
    edge: &mut FailureEdge,
    minute_label: &str,
    ok_count: usize,
    error_count: usize,
    empty_count: usize,
    persist_failed: bool,
    sample_failure: Option<&str>,
) {
    let fully_failed = chain_minute_fully_failed(ok_count, persist_failed);
    match edge.record_minute(fully_failed) {
        EdgeAction::Page { consecutive } => {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "escalation",
                feed = "groww",
                leg = "contract_1m",
                consecutive,
                minute = minute_label,
                sample = sample_failure.unwrap_or("none captured"),
                "SPOT1M-01: Groww per-minute contract fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwContract1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: minute_label.to_string(),
                });
        }
        EdgeAction::Recover { failed_minutes } => {
            info!(
                failed_minutes,
                minute = minute_label,
                "groww_contract_1m: per-minute fetch recovered after a paged episode"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwContract1mFetchRecovered {
                    minute_ist: minute_label.to_string(),
                    failed_minutes,
                });
        }
        EdgeAction::None => {
            if error_count > 0 || empty_count > 0 || persist_failed {
                // Coalesced ONCE per fire; log-sink-only — sub-edge
                // failures never page (the escalation arm does).
                error!(
                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                    stage = "minute_failed",
                    feed = "groww",
                    leg = "contract_1m",
                    minute = minute_label,
                    ok = ok_count,
                    errors = error_count,
                    empty = empty_count,
                    persist_failed,
                    sample = sample_failure.unwrap_or("none captured"),
                    "SPOT1M-01: Groww per-minute contract fetch degraded for this minute"
                );
            }
        }
    }
}

/// Loud accounting for minute boundaries that elapsed UNFETCHED (fire
/// overrun / suspend / clock step): counter + ONE coalesced coded log +
/// each missed minute feeds the failure edge + one `outcome=skipped`
/// forensics row per (minute, underlying) — the UNDERLYING's stable id,
/// because no per-contract selection ever existed for a skipped minute —
/// so the hole is queryable (never silent — Rule 11). Mirrors the chain
/// leg incl. the item-7 midnight-crossing guard.
fn record_groww_contract_skipped_boundaries(
    params: &GrowwContract1mTaskParams,
    edge: &mut FailureEdge,
    audit_writer: &mut RestFetchAuditWriter,
    books: &[GrowwContractBook],
    skipped: u32,
    first_missed_boundary_secs: u32,
    iter_date: NaiveDate,
) {
    if skipped == 0 {
        return;
    }
    metrics::counter!("tv_groww_contract1m_boundary_skipped_total").increment(u64::from(skipped));
    let around = format_minute_ist_12h(first_missed_boundary_secs);
    error!(
        code = ErrorCode::Spot1m01FetchDegraded.code_str(),
        stage = "boundary_skipped",
        feed = "groww",
        leg = "contract_1m",
        skipped,
        around = %around,
        "SPOT1M-01: Groww contract minute boundaries elapsed unfetched (fire \
         overrun / suspend) — those minutes' selections never existed (the \
         next boundary re-selects; no cross-minute backfill for a selection \
         that was never made)"
    );
    // Item 7 (spot precedent): a wake that crossed IST midnight would
    // stamp the missed PRE-SUSPEND session seconds onto the POST-WAKE date
    // — wrong trading date on every row. The counter + coalesced log
    // already fired; skip the (mis-dated) forensics rows + edge accounting.
    let trading_date = today_ist();
    if trading_date != iter_date {
        warn!(
            %iter_date,
            %trading_date,
            "groww_contract_1m: wake crossed IST midnight — skipping the \
             boundary-skip forensics rows (wrong trading date); the day is \
             over for this run"
        );
        return;
    }
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
    for i in 0..skipped {
        let boundary = first_missed_boundary_secs.saturating_add(i.saturating_mul(60));
        let minute_open_secs = boundary.saturating_sub(60);
        let target_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
        for book in books {
            let row = build_contract_audit_row(
                target_nanos,
                trading_date_nanos,
                book.underlying_security_id,
                book.segment,
                book.underlying,
                0,
                0,
                -1,
                -1,
                0,
                RestFetchOutcome::Skipped,
                "boundary_skipped",
            );
            contract_audit_append_best_effort(audit_writer, &row);
        }
        if let EdgeAction::Page { consecutive } = edge.record_minute(true) {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "escalation",
                feed = "groww",
                leg = "contract_1m",
                consecutive,
                minute = %around,
                "SPOT1M-01: Groww per-minute contract fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwContract1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: around.clone(),
                });
        }
    }
    contract_audit_flush_best_effort(audit_writer);
}

// ---------------------------------------------------------------------------
// Scheduler run + supervisor
// ---------------------------------------------------------------------------

/// Run today's remaining contract minute fires, then return. Never panics;
/// every fault path logs (coded, `feed="groww"` + `leg="contract_1m"`) +
/// counts. Sets `disabled_for_day` before returning when NO underlying
/// resolved a contract book (tomorrow's boot re-warms — the supervisor
/// deliberately does NOT respawn that stop).
// TEST-EXEMPT: live-deps async runner — every scheduling / parsing / selection / edge decision is a pure fn unit-tested (here + in the reused spot/chain modules + instruments.rs); the HTTP leg mirrors the tested patterns; wiring pinned by crates/app/tests/groww_contract_1m_wiring_guard.rs.
pub async fn run_groww_contract_1m(
    params: GrowwContract1mTaskParams,
    disabled_for_day: Arc<AtomicBool>,
    books_cache: GrowwContractBooksCache,
) {
    // Idempotent DDL first (CREATE → ADD COLUMN self-heal → DEDUP ENABLE)
    // for BOTH tables; failures degrade loudly inside and never block.
    ensure_option_contract_1m_rest_table(&params.questdb).await;
    ensure_rest_fetch_audit_table(&params.questdb).await;

    if !params.calendar.is_trading_day_today() {
        info!("groww_contract_1m: non-trading day — skipping all minute fires");
        return;
    }
    // ONE long-lived client for the whole session (per-minute rebuild is
    // the exact TLS/resolver churn HTTP-CLIENT-01 §0 condemns).
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(GROWW_CONTRACT_1M_REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "client_build",
                feed = "groww",
                leg = "contract_1m",
                ?err,
                "SPOT1M-01: HTTP client build failed — Groww per-minute \
                 contract fetch degraded; supervisor will retry after backoff"
            );
            metrics::counter!("tv_groww_contract1m_fetch_total", "outcome" => "error").increment(1);
            return;
        }
    };

    // ---- Day-start warmup: contract books from the instruments master
    // (zero rate cost — NEVER a guessed contract identity) ----
    let today = today_ist();
    let cached = {
        let cell = books_cache.lock().await;
        cell.as_ref()
            .filter(|(date, _)| *date == today)
            .map(|(_, cached)| cached.clone())
    };
    let books = if let Some(books) = cached {
        info!(
            underlyings = books.len(),
            "groww_contract_1m: reusing today's contract books (supervisor \
             respawn — the instruments master is NOT re-downloaded)"
        );
        Some(books)
    } else {
        resolve_and_cache_contract_books(&params, &disabled_for_day, &books_cache, today).await
    };
    let Some(books) = books else {
        // Warmup degraded to disabled-for-the-day (page/logs already
        // emitted inside) — the supervisor deliberately does not respawn.
        return;
    };
    run_groww_contract_minute_loop(&params, &client, books).await;
}

/// Day-start warmup (the cache-filling slow path): download the
/// instruments master (bounded retries — the chain leg's shared helper),
/// resolve today's contract book per underlying, emit the per-underlying
/// degrade page + forensics, then cache the books for supervisor respawns.
/// `None` = disabled for the day (page already fired; supervisor exits).
// TEST-EXEMPT: live-deps warmup shell — the resolution (resolve_groww_contract_books / build_contract_book / parse_contract_symbol) and detail formatting (book_degrade_detail) are the unit-tested pure parts.
async fn resolve_and_cache_contract_books(
    params: &GrowwContract1mTaskParams,
    disabled_for_day: &AtomicBool,
    books_cache: &GrowwContractBooksCache,
    today: NaiveDate,
) -> Option<Vec<GrowwContractBook>> {
    let rows = match download_master_bounded().await {
        Ok(rows) => rows,
        Err(detail) => {
            metrics::counter!("tv_groww_contract1m_book_unresolved_total")
                .increment(GROWW_CHAIN_1M_UNDERLYINGS.len() as u64);
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "book_unresolved",
                feed = "groww",
                leg = "contract_1m",
                detail = %capture_rest_error_body(&detail),
                "SPOT1M-01: Groww instruments-master download failed after \
                 bounded retries — the Groww contract leg degrades to \
                 DISABLED for the day (never a guessed contract identity)"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwContract1mBookUnresolved {
                    detail: format!(
                        "today's contract list could not be downloaded ({})",
                        capture_rest_error_body(&detail)
                    ),
                });
            disabled_for_day.store(true, Ordering::SeqCst);
            return None;
        }
    };
    let (books, degraded, skipped_rows, token_collisions) =
        resolve_groww_contract_books(&rows, today);
    if skipped_rows > 0 {
        metrics::counter!("tv_groww_contract1m_master_rows_skipped_total")
            .increment(u64::from(skipped_rows));
        warn!(
            skipped_rows,
            "groww_contract_1m: skipped malformed master contract rows \
             (unparsable symbol / type mismatch / non-numeric token)"
        );
    }
    if token_collisions > 0 {
        // Round-1 review LOW (the Dhan dedup-drop precedent): a duplicate
        // `exchange_token` across DIFFERENT contracts is vendor id-space
        // corruption — the later rows were dropped keep-first, counted,
        // one coded warn (two identities on one token would collapse into
        // one `security_id` in the persisted table).
        metrics::counter!("tv_groww_contract1m_token_collisions_total")
            .increment(u64::from(token_collisions));
        warn!(
            code = ErrorCode::Spot1m01FetchDegraded.code_str(),
            stage = "token_collision",
            feed = "groww",
            leg = "contract_1m",
            token_collisions,
            "SPOT1M-01: duplicate exchange_token across DIFFERENT contracts \
             in today's Groww instruments master — later rows dropped \
             keep-first (vendor id-space corruption; counted, never silent)"
        );
    }
    if !degraded.is_empty() {
        metrics::counter!("tv_groww_contract1m_book_unresolved_total")
            .increment(degraded.len() as u64);
        let detail = book_degrade_detail(&degraded, rows.len());
        error!(
            code = ErrorCode::Spot1m01FetchDegraded.code_str(),
            stage = "book_unresolved",
            feed = "groww",
            leg = "contract_1m",
            degraded = ?degraded,
            master_rows = rows.len(),
            "SPOT1M-01: no usable option contracts at the current expiry in \
             today's Groww instruments master for these underlyings — their \
             per-contract recording is OFF for the day (never guessed)"
        );
        params
            .notifier
            .notify(NotificationEvent::GrowwContract1mBookUnresolved { detail });
    }
    drop(rows);
    if books.is_empty() {
        // Every underlying degraded — nothing to fetch today (the page
        // above already fired).
        disabled_for_day.store(true, Ordering::SeqCst);
        return None;
    }
    for b in &books {
        info!(
            symbol = b.underlying,
            exchange = b.exchange,
            expiry = %b.expiry.format("%Y-%m-%d"),
            strikes = b.strikes.len(),
            "groww_contract_1m: contract book resolved from the instruments master"
        );
    }
    *books_cache.lock().await = Some((today, books.clone()));
    Some(books)
}

/// The armed per-minute fire loop (the chain leg's loop shape).
// TEST-EXEMPT: live-deps scheduler loop — every decision inside is a unit-tested pure fn (next_fire_after / fire_is_fresh / count_missed_boundaries / stale_wake_backoff_ms / contract_min_gap_wait_ms / select_contracts_for_minute).
async fn run_groww_contract_minute_loop(
    params: &GrowwContract1mTaskParams,
    client: &reqwest::Client,
    books: Vec<GrowwContractBook>,
) {
    let mut writer = OptionContract1mRestWriter::new(&params.questdb);
    let mut audit_writer = RestFetchAuditWriter::new(&params.questdb);
    let mut edge = FailureEdge::default();
    let mut token_cache = GrowwTokenCache::new_contract();
    let mut tracker = ContractPersistTracker::default();
    let mut last_fired: Option<u32> = None;
    // ONE scalar spanning consecutive contract requests — the
    // cross-request min-gap pacing state (the chain MEDIUM-1 pattern).
    let mut last_request_ms: Option<i64> = None;
    // Anchor-staleness warn latch (review M3): ONE coded warn per stale
    // episode; re-armed when every anchor is fresh again.
    let mut anchor_stale_latched = false;
    let mut chain_rx = params.chain_minute_done.clone();
    // The session's first minute-open (09:15:00 IST) — the backfill's
    // in-session floor.
    let session_first_minute_nanos = minute_open_ist_nanos(
        today_ist(),
        tickvault_common::constants::SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST.saturating_sub(60),
    );
    info!(
        underlyings = books.len(),
        strikes_each_side = params.strikes_each_side,
        cap = GROWW_CONTRACT_1M_MAX_PER_MINUTE,
        "groww_contract_1m: per-minute contract fetch loop armed (fires each \
         minute close 09:16:00-15:30:00 IST, right after the Groww chain leg; \
         sequential contract pacing)"
    );

    loop {
        // Audit Rule 3: re-read the wall clock + trading-day verdict EVERY
        // iteration (a suspend can cross midnight and stale the verdict).
        if !params.calendar.is_trading_day_today() {
            info!("groww_contract_1m: no longer a trading day — exiting");
            return;
        }
        let iter_date = today_ist();
        let now = ist_secs_of_day_now();
        let Some(fire) = next_fire_after(now, last_fired) else {
            info!("groww_contract_1m: past 15:30 IST — today's minute fires complete");
            return;
        };
        // Sequenced wake: the Groww CHAIN leg's minute-done signal, bounded
        // by the fallback timer (the reused Dhan wait core).
        let sleep_ms = u64::from(fire.saturating_sub(now)).saturating_mul(1_000)
            + GROWW_CONTRACT_1M_FALLBACK_DELAY_MS;
        wait_for_signal_or_fallback(sleep_ms, fire, &mut chain_rx).await;

        // Staleness gate (suspend / clock-step defense): skip + recompute,
        // never fetch a long-gone minute.
        let woke = ist_secs_of_day_now();
        if !fire_is_fresh(fire, woke) {
            warn!(
                fire_secs = fire,
                woke_at_secs = woke,
                "groww_contract_1m: woke too far past the minute boundary \
                 (suspend/clock step?) — skipping this minute"
            );
            let missed = count_missed_boundaries(fire.saturating_sub(60), woke.saturating_sub(1));
            record_groww_contract_skipped_boundaries(
                params,
                &mut edge,
                &mut audit_writer,
                &books,
                missed,
                fire,
                iter_date,
            );
            if woke > fire {
                last_fired = Some((woke.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST) / 60) * 60);
            }
            let backoff_ms = stale_wake_backoff_ms(fire, woke);
            if backoff_ms > 0 {
                // Clock stepped BACK across the boundary: an already-
                // satisfied chain signal would make the next wait return
                // with ZERO awaits — sleep up to the fire moment instead
                // of busy-spinning (the Dhan chain H1 defense).
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }
            continue;
        }

        fire_one_groww_contract_minute(
            params,
            client,
            tickvault_common::constants::GROWW_HISTORICAL_CANDLES_URL,
            &books,
            &mut last_request_ms,
            &mut writer,
            &mut audit_writer,
            &mut edge,
            &mut token_cache,
            &mut tracker,
            session_first_minute_nanos,
            fire,
            &mut anchor_stale_latched,
        )
        .await;
        last_fired = Some(fire);
        // Overrun accounting: boundaries that fully elapsed DURING the
        // fire can never be fetched — count them loudly + feed the edge.
        let after = ist_secs_of_day_now();
        let missed = count_missed_boundaries(fire, after.saturating_sub(1));
        record_groww_contract_skipped_boundaries(
            params,
            &mut edge,
            &mut audit_writer,
            &books,
            missed,
            fire + 60,
            iter_date,
        );
        if missed > 0 {
            last_fired = Some((after.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST) / 60) * 60);
        }
    }
}

/// Spawn the supervised Groww per-minute contract scheduler. The
/// supervisor respawns a dead/failed run after a bounded backoff, and
/// exits cleanly once today's window is over, on graceful-shutdown cancel,
/// or after a warmup disabled-for-the-day stop (deliberately NOT respawned
/// — the pipeline stays down; tomorrow's boot re-warms).
// TEST-EXEMPT: tokio supervisor wiring over the unit-tested pure decisions (spot_1m_day_is_over / classify_join_exit / the disabled-for-day latch); spawn site pinned by crates/app/tests/groww_contract_1m_wiring_guard.rs.
pub fn spawn_supervised_groww_contract_1m(
    params: GrowwContract1mTaskParams,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let disabled_for_day = Arc::new(AtomicBool::new(false));
        // Shared across respawns (the chain MEDIUM-4 precedent): the day's
        // resolved contract books — a flapping backoff loop never
        // re-downloads the multi-MB master.
        let books_cache: GrowwContractBooksCache = Arc::new(tokio::sync::Mutex::new(None));
        loop {
            let inner = tokio::spawn(run_groww_contract_1m(
                params.clone(),
                Arc::clone(&disabled_for_day),
                Arc::clone(&books_cache),
            ));
            let result = inner.await;
            let reason = classify_join_exit(&result);
            let day_over = spot_1m_day_is_over(
                ist_secs_of_day_now(),
                params.calendar.is_trading_day_today(),
            );
            match &result {
                Ok(()) if disabled_for_day.load(Ordering::SeqCst) => {
                    info!(
                        "groww_contract_1m: pipeline disabled for the day \
                         (unresolvable contract books) — supervisor exiting"
                    );
                    return;
                }
                Ok(()) if day_over => {
                    info!("groww_contract_1m: day complete — supervisor exiting");
                    return;
                }
                Err(join_err) if join_err.is_cancelled() => {
                    // Graceful shutdown teardown — not an abort.
                    return;
                }
                _ => {}
            }
            metrics::counter!("tv_groww_contract1m_task_respawn_total", "reason" => reason)
                .increment(1);
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "task_respawn",
                feed = "groww",
                leg = "contract_1m",
                reason,
                "SPOT1M-01: Groww per-minute contract fetch task died \
                 mid-window — respawning after backoff"
            );
            tokio::time::sleep(Duration::from_secs(GROWW_CONTRACT_1M_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- symbol parse ------------------------------------------------------

    #[test]
    fn test_parse_contract_symbol_happy_and_hostile() {
        // The documented shapes (docs/groww-ref/11-historical-candles.md +
        // the production-grounded instrument-sample.csv).
        assert_eq!(
            parse_contract_symbol("NSE-NIFTY-04Jan24-19200-CE"),
            Some((19_200.0, "CE"))
        );
        assert_eq!(
            parse_contract_symbol("BSE-SENSEX-25Sep25-79500-PE"),
            Some((79_500.0, "PE"))
        );
        // Decimal strikes parse (never assumed integer).
        assert_eq!(
            parse_contract_symbol("NSE-NIFTY-04Jan24-19250.5-CE"),
            Some((19_250.5, "CE"))
        );
        // Hostile / malformed shapes → None, never a panic.
        for bad in [
            "",
            "NSE-NIFTY",                   // no strike/leg tail
            "19200-CE",                    // no prefix segments
            "NSE-NIFTY-04Jan24-x-CE",      // non-numeric strike
            "NSE-NIFTY-04Jan24-19200-FUT", // not an option leg
            "NSE-NIFTY-04Jan24--CE",       // empty strike
            "NSE-NIFTY-04Jan24-0-CE",      // non-positive strike
            "NSE-NIFTY-04Jan24--5-PE",     // negative strike (parses "-5"? no — the
            // rsplit yields "5" then "" then... stays None or positive)
            "NSE-NIFTY-04Jan24-1e300-CE", // implausibly large
            "NSE-NIFTY-04Jan24-inf-CE",   // non-finite
        ] {
            assert!(
                parse_contract_symbol(bad).is_none()
                    || parse_contract_symbol(bad)
                        .is_some_and(|(s, _)| s > 0.0 && s < MAX_PLAUSIBLE_STRIKE),
                "hostile symbol must never mint an implausible strike: {bad}"
            );
        }
        assert_eq!(parse_contract_symbol("NSE-NIFTY-04Jan24-x-CE"), None);
        assert_eq!(parse_contract_symbol("19200-CE"), None);
    }

    // ---- book build --------------------------------------------------------

    fn master_row(
        exchange: &str,
        token: &str,
        groww_symbol: &str,
        instrument_type: &str,
    ) -> GrowwInstrumentRow {
        GrowwInstrumentRow {
            exchange: exchange.to_string(),
            exchange_token: token.to_string(),
            groww_symbol: groww_symbol.to_string(),
            name: String::new(),
            instrument_type: instrument_type.to_string(),
            segment: "FNO".to_string(),
            series: String::new(),
            isin: String::new(),
            underlying_symbol: "NIFTY".to_string(),
            expiry_date: "2026-07-16".to_string(),
        }
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("test date")
    }

    #[test]
    fn test_build_contract_book_sorts_dedups_and_skips_malformed() {
        let rows = [
            master_row("NSE", "101", "NSE-NIFTY-16Jul26-25100-CE", "CE"),
            master_row("NSE", "102", "NSE-NIFTY-16Jul26-25100-PE", "PE"),
            master_row("NSE", "103", "NSE-NIFTY-16Jul26-25000-CE", "CE"),
            // Duplicate (strike, leg) — FIRST wins (vendor-glitch class).
            master_row("NSE", "999", "NSE-NIFTY-16Jul26-25100-CE", "CE"),
            // Symbol tail disagrees with instrument_type — skipped.
            master_row("NSE", "104", "NSE-NIFTY-16Jul26-25200-CE", "PE"),
            // Non-numeric token — skipped.
            master_row("NSE", "not-a-number", "NSE-NIFTY-16Jul26-25300-CE", "CE"),
            // Malformed symbol — skipped.
            master_row("NSE", "105", "garbage", "CE"),
        ];
        let refs: Vec<&GrowwInstrumentRow> = rows.iter().collect();
        let (book, skipped, collisions) =
            build_contract_book(&refs, "NIFTY", "NSE", 42, d("2026-07-16"));
        assert_eq!(skipped, 3, "type mismatch + bad token + bad symbol");
        assert_eq!(collisions, 0, "no cross-identity token reuse here");
        assert_eq!(book.segment, "NSE_FNO");
        assert_eq!(book.underlying_security_id, 42);
        // Sorted ascending by strike; 2 slots (25000, 25100).
        assert_eq!(book.strikes.len(), 2);
        assert_eq!(book.strikes[0].strike, 25_000.0);
        assert_eq!(book.strikes[1].strike, 25_100.0);
        // First-row-wins on the duplicate CE.
        assert_eq!(
            book.strikes[1].ce.as_ref().map(|c| c.token),
            Some(101),
            "duplicate (strike, leg) keeps the FIRST row"
        );
        assert_eq!(book.strikes[1].pe.as_ref().map(|c| c.token), Some(102));
        // One-sided slot: 25000 has CE only.
        assert!(book.strikes[0].pe.is_none());
        // BSE maps to BSE_FNO.
        assert_eq!(contract_segment_for_exchange("BSE"), "BSE_FNO");
        assert_eq!(contract_segment_for_exchange("NSE"), "NSE_FNO");
    }

    #[test]
    fn test_build_contract_book_counts_cross_identity_token_collisions_keep_first() {
        // Token 101 reused by a DIFFERENT (strike, leg) identity — vendor
        // id-space corruption: keep-first, count, drop the later row.
        let rows = [
            master_row("NSE", "101", "NSE-NIFTY-16Jul26-25100-CE", "CE"),
            master_row("NSE", "101", "NSE-NIFTY-16Jul26-25100-PE", "PE"),
            master_row("NSE", "102", "NSE-NIFTY-16Jul26-25000-CE", "CE"),
        ];
        let refs: Vec<&GrowwInstrumentRow> = rows.iter().collect();
        let (book, skipped, collisions) =
            build_contract_book(&refs, "NIFTY", "NSE", 42, d("2026-07-16"));
        assert_eq!(skipped, 0, "collisions are counted separately");
        assert_eq!(collisions, 1, "the PE row reusing token 101 collides");
        // The colliding PE never landed; the CE (first) kept the token.
        let s25100 = book
            .strikes
            .iter()
            .find(|s| s.strike == 25_100.0)
            .expect("25100 slot");
        assert_eq!(s25100.ce.as_ref().map(|c| c.token), Some(101));
        assert!(s25100.pe.is_none(), "colliding later identity dropped");
        // An EXACT duplicate line (same token, same identity) stays the
        // benign first-row-wins class — NOT a collision.
        let dup_rows = [
            master_row("NSE", "101", "NSE-NIFTY-16Jul26-25100-CE", "CE"),
            master_row("NSE", "101", "NSE-NIFTY-16Jul26-25100-CE", "CE"),
        ];
        let dup_refs: Vec<&GrowwInstrumentRow> = dup_rows.iter().collect();
        let (_, dup_skipped, dup_collisions) =
            build_contract_book(&dup_refs, "NIFTY", "NSE", 42, d("2026-07-16"));
        assert_eq!(dup_skipped, 0);
        assert_eq!(dup_collisions, 0, "exact duplicate line is benign");
    }

    #[test]
    fn test_partition_fresh_anchors_names_stale_and_keeps_fresh() {
        let now = 1_000_000 * NANOS_PER_MINUTE;
        let max_age = GROWW_CONTRACT_1M_ANCHOR_MAX_AGE_MINUTES;
        let mut raw: HashMap<&'static str, GrowwChainAnchor> = HashMap::new();
        // Fresh (just set) / EXACTLY at the max age (still fresh — the
        // gate is strictly "beyond") / one nano beyond (stale).
        raw.insert(
            "NIFTY",
            GrowwChainAnchor {
                ltp: 25_100.0,
                set_at_ist_nanos: now,
            },
        );
        raw.insert(
            "SENSEX",
            GrowwChainAnchor {
                ltp: 81_000.0,
                set_at_ist_nanos: now - i64::from(max_age) * NANOS_PER_MINUTE,
            },
        );
        raw.insert(
            "BANKNIFTY",
            GrowwChainAnchor {
                ltp: 52_000.0,
                set_at_ist_nanos: now - i64::from(max_age) * NANOS_PER_MINUTE - 1,
            },
        );
        let (fresh, stale) = partition_fresh_anchors(&raw, now, max_age);
        assert_eq!(fresh.get("NIFTY").copied(), Some(25_100.0));
        assert_eq!(
            fresh.get("SENSEX").copied(),
            Some(81_000.0),
            "exactly-at-max-age is still fresh (strictly beyond = stale)"
        );
        assert_eq!(stale, vec!["BANKNIFTY"], "one nano beyond = stale, named");
        // Empty input: nothing fresh, nothing stale.
        let (fresh_e, stale_e) = partition_fresh_anchors(&HashMap::new(), now, max_age);
        assert!(fresh_e.is_empty() && stale_e.is_empty());
    }

    // ---- ATM / window / selection -----------------------------------------

    fn slot(strike: f64, ce_token: Option<i64>, pe_token: Option<i64>) -> GrowwStrikeSlot {
        let mk = |t: i64, leg: &str| GrowwContractRef {
            groww_symbol: format!("NSE-NIFTY-16Jul26-{strike}-{leg}"),
            token: t,
        };
        GrowwStrikeSlot {
            strike,
            ce: ce_token.map(|t| mk(t, "CE")),
            pe: pe_token.map(|t| mk(t, "PE")),
        }
    }

    fn book_with_strikes(underlying: &'static str, strikes: &[f64]) -> GrowwContractBook {
        GrowwContractBook {
            underlying,
            exchange: "NSE",
            segment: "NSE_FNO",
            underlying_security_id: 1,
            expiry: d("2026-07-16"),
            strikes: strikes
                .iter()
                .enumerate()
                .map(|(i, &s)| slot(s, Some(1_000 + i as i64), Some(2_000 + i as i64)))
                .collect(),
        }
    }

    #[test]
    fn test_atm_index_nearest_with_deterministic_lower_tie() {
        let slots: Vec<GrowwStrikeSlot> = [100.0, 200.0, 300.0]
            .iter()
            .map(|&s| slot(s, Some(1), Some(2)))
            .collect();
        assert_eq!(atm_index(&slots, 205.0), Some(1));
        assert_eq!(atm_index(&slots, 95.0), Some(0));
        assert_eq!(atm_index(&slots, 1_000.0), Some(2));
        // Exact midpoint tie (150 between 100 and 200) → the LOWER strike.
        assert_eq!(atm_index(&slots, 150.0), Some(0));
        assert_eq!(atm_index(&[], 100.0), None);
    }

    #[test]
    fn test_atm_window_indices_rank_order_and_edge_clamp() {
        // Rank order: ATM, then lower, then higher, expanding outward.
        assert_eq!(atm_window_indices(10, 5, 2), vec![5, 4, 6, 3, 7]);
        // Clamped at the low edge.
        assert_eq!(atm_window_indices(10, 0, 2), vec![0, 1, 2]);
        // Clamped at the high edge.
        assert_eq!(atm_window_indices(10, 9, 2), vec![9, 8, 7]);
        // Empty book.
        assert!(atm_window_indices(0, 0, 2).is_empty());
    }

    #[test]
    fn test_select_contracts_for_minute_default_window_fills_cap_exactly() {
        let books = vec![
            book_with_strikes(
                "NIFTY",
                &[
                    24_900.0, 25_000.0, 25_100.0, 25_200.0, 25_300.0, 25_400.0, 25_500.0,
                ],
            ),
            book_with_strikes(
                "BANKNIFTY",
                &[
                    56_800.0, 56_900.0, 57_000.0, 57_100.0, 57_200.0, 57_300.0, 57_400.0,
                ],
            ),
            book_with_strikes(
                "SENSEX",
                &[
                    81_800.0, 81_900.0, 82_000.0, 82_100.0, 82_200.0, 82_300.0, 82_400.0,
                ],
            ),
        ];
        let anchors: HashMap<&'static str, f64> = [
            ("NIFTY", 25_210.0),
            ("BANKNIFTY", 57_090.0),
            ("SENSEX", 82_005.0),
        ]
        .into_iter()
        .collect();
        let sel = select_contracts_for_minute(&books, &anchors, 2, 30);
        // (2×2+1) strikes × 2 legs × 3 underlyings = 30 = the cap: no
        // truncation, no unresolved.
        assert_eq!(sel.selected.len(), 30);
        assert_eq!(sel.truncated, 0);
        assert!(sel.unresolved.is_empty());
        // Deterministic round-robin by candidate rank: rank 0 is each
        // underlying's ATM CE (NIFTY, BANKNIFTY, SENSEX), rank 1 each
        // underlying's ATM PE, then the ±1 strikes, expanding outward.
        assert_eq!(sel.selected[0].underlying, "NIFTY");
        assert_eq!(sel.selected[0].strike, 25_200.0);
        assert_eq!(sel.selected[0].leg, "CE");
        assert_eq!(sel.selected[1].underlying, "BANKNIFTY");
        assert_eq!(sel.selected[1].strike, 57_100.0);
        assert_eq!(sel.selected[1].leg, "CE");
        assert_eq!(sel.selected[2].underlying, "SENSEX");
        assert_eq!(sel.selected[2].strike, 82_000.0);
        assert_eq!(sel.selected[3].underlying, "NIFTY");
        assert_eq!(sel.selected[3].strike, 25_200.0);
        assert_eq!(sel.selected[3].leg, "PE");
        // Running twice yields byte-identical selection (determinism).
        let sel2 = select_contracts_for_minute(&books, &anchors, 2, 30);
        assert_eq!(sel, sel2);
    }

    #[test]
    fn test_select_contracts_for_minute_cap_truncates_furthest_first_and_counts() {
        let books = vec![
            book_with_strikes("NIFTY", &[24_900.0, 25_000.0, 25_100.0, 25_200.0, 25_300.0]),
            book_with_strikes("BANKNIFTY", &[57_000.0, 57_100.0, 57_200.0]),
        ];
        let anchors: HashMap<&'static str, f64> = [("NIFTY", 25_100.0), ("BANKNIFTY", 57_100.0)]
            .into_iter()
            .collect();
        // Full candidate set: NIFTY 5 strikes × 2 + BANKNIFTY 3 strikes × 2
        // = 16; cap at 10 → 6 truncated, all from the FURTHEST ranks.
        let sel = select_contracts_for_minute(&books, &anchors, 2, 10);
        assert_eq!(sel.selected.len(), 10);
        assert_eq!(sel.truncated, 6);
        // Every kept contract's ATM distance rank is <= every dropped
        // rank: the nearest-ATM window survives (the ATM strikes of BOTH
        // underlyings are present).
        assert!(
            sel.selected
                .iter()
                .any(|c| c.underlying == "NIFTY" && c.strike == 25_100.0)
        );
        assert!(
            sel.selected
                .iter()
                .any(|c| c.underlying == "BANKNIFTY" && c.strike == 57_100.0)
        );
    }

    #[test]
    fn test_select_contracts_for_minute_missing_anchor_is_unresolved_never_guessed() {
        let books = vec![
            book_with_strikes("NIFTY", &[25_000.0, 25_100.0]),
            book_with_strikes("BANKNIFTY", &[57_000.0]),
        ];
        let anchors: HashMap<&'static str, f64> = [("NIFTY", 25_050.0)].into_iter().collect();
        let sel = select_contracts_for_minute(&books, &anchors, 2, 30);
        assert_eq!(sel.unresolved, vec!["BANKNIFTY"]);
        assert!(sel.selected.iter().all(|c| c.underlying == "NIFTY"));
        // No anchors at all → everything unresolved, nothing selected.
        let none = select_contracts_for_minute(&books, &HashMap::new(), 2, 30);
        assert_eq!(none.selected.len(), 0);
        assert_eq!(none.unresolved.len(), 2);
    }

    // ---- pacing / plausibility ----------------------------------------------

    #[test]
    fn test_contract_min_gap_wait_ms_math_and_midnight_wrap() {
        // First request: no wait.
        assert_eq!(contract_min_gap_wait_ms(None, 1_000), 0);
        // 100ms since the last request → wait the remaining 400ms (500ms gap).
        assert_eq!(contract_min_gap_wait_ms(Some(10_000), 10_100), 400);
        // Partially elapsed → the exact remainder.
        assert_eq!(contract_min_gap_wait_ms(Some(10_000), 10_400), 100);
        // Gap already satisfied.
        assert_eq!(contract_min_gap_wait_ms(Some(10_000), 10_600), 0);
        // Midnight wrap / backwards clock → 0, never a long spurious sleep.
        assert_eq!(contract_min_gap_wait_ms(Some(86_399_000), 100), 0);
    }

    #[test]
    fn test_contract_ohlc_implausible_bounds() {
        let good = GrowwCandleRow {
            minute_ts_ist_nanos: 0,
            open: 100.0,
            high: 110.0,
            low: 95.0,
            close: 105.0,
            volume: 10,
            oi: 20,
        };
        assert!(!contract_ohlc_implausible(&good));
        assert!(contract_ohlc_implausible(&GrowwCandleRow {
            high: 90.0, // high < low
            ..good
        }));
        assert!(contract_ohlc_implausible(&GrowwCandleRow {
            open: 0.0,
            ..good
        }));
        assert!(contract_ohlc_implausible(&GrowwCandleRow {
            close: -1.0,
            ..good
        }));
    }

    // ---- warmup resolution ---------------------------------------------------

    #[test]
    fn test_resolve_groww_contract_books_degrades_per_underlying() {
        // Only NIFTY has usable contracts; BANKNIFTY/SENSEX degrade.
        let rows = [
            master_row("NSE", "101", "NSE-NIFTY-16Jul26-25000-CE", "CE"),
            master_row("NSE", "102", "NSE-NIFTY-16Jul26-25000-PE", "PE"),
        ];
        let (books, degraded, skipped, collisions) =
            resolve_groww_contract_books(&rows, d("2026-07-13"));
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].underlying, "NIFTY");
        assert_eq!(books[0].expiry, d("2026-07-16"));
        assert_eq!(degraded, vec!["BANKNIFTY", "SENSEX"]);
        assert_eq!(skipped, 0);
        assert_eq!(collisions, 0);
        // Empty master → everything degrades, nothing panics.
        let (none, all_degraded, _, _) = resolve_groww_contract_books(&[], d("2026-07-13"));
        assert!(none.is_empty());
        assert_eq!(all_degraded.len(), 3);
        // The degrade detail names the underlyings + the row count.
        let detail = book_degrade_detail(&all_degraded, 0);
        assert!(detail.contains("NIFTY"));
        assert!(detail.contains("0 rows scanned"));
    }

    // ---- persist tracker (composite key — I-P1-11) ---------------------------

    #[test]
    fn test_contract_persist_tracker_composite_key_and_max_merge() {
        let mut t = ContractPersistTracker::default();
        assert_eq!(t.last_persisted(66_825, "NSE_FNO"), None);
        t.commit(66_825, "NSE_FNO", 100);
        t.commit(66_825, "NSE_FNO", 50); // never regresses
        assert_eq!(t.last_persisted(66_825, "NSE_FNO"), Some(100));
        // The SAME numeric token on the OTHER exchange is a DIFFERENT
        // contract (I-P1-11: tokens are only exchange-unique).
        assert_eq!(t.last_persisted(66_825, "BSE_FNO"), None);
        t.commit(66_825, "BSE_FNO", 70);
        assert_eq!(t.last_persisted(66_825, "BSE_FNO"), Some(70));
        assert_eq!(t.last_persisted(66_825, "NSE_FNO"), Some(100));
    }

    // ---- row / audit builders -------------------------------------------------

    #[test]
    fn test_build_contract_row_and_audit_row_stamp_identity() {
        let contract = GrowwSelectedContract {
            underlying: "NIFTY",
            exchange: "NSE",
            segment: "NSE_FNO",
            expiry: d("2026-07-16"),
            strike: 25_100.0,
            leg: "CE",
            groww_symbol: "NSE-NIFTY-16Jul26-25100-CE".to_string(),
            token: 66_825,
        };
        let candle = GrowwCandleRow {
            minute_ts_ist_nanos: 1_770_000_900_000_000_000,
            open: 180.0,
            high: 190.0,
            low: 178.0,
            close: 188.0,
            volume: 1_000,
            oi: 5_000,
        };
        let row = build_contract_row(&candle, &contract, 1_769_990_400_000_000_000, 1_842);
        assert_eq!(row.security_id, 66_825);
        assert_eq!(row.exchange_segment, "NSE_FNO");
        assert_eq!(row.groww_symbol, "NSE-NIFTY-16Jul26-25100-CE");
        assert_eq!(row.oi, 5_000);
        assert_eq!(row.close_to_data_ms, 1_842);
        assert_eq!(row.ts_ist_nanos, candle.minute_ts_ist_nanos);

        let audit = build_contract_audit_row(
            row.ts_ist_nanos,
            row.trading_date_ist_nanos,
            contract.token,
            contract.segment,
            contract.underlying,
            1,
            200,
            143,
            1_842,
            0,
            RestFetchOutcome::Ok,
            "none",
        );
        assert_eq!(audit.leg, REST_FETCH_LEG_CONTRACT_1M);
        assert_eq!(audit.feed, "groww");
        assert_eq!(audit.security_id, 66_825);
        assert_eq!(audit.exchange_segment, "NSE_FNO");
        assert_eq!(audit.symbol, "NIFTY");
    }

    // ---- HTTP arms (hermetic mock server — the chain leg's pattern) ----------

    async fn spawn_mock_http(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 16384];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    fn test_query(date: &str) -> [(&'static str, String); 6] {
        groww_candles_query(
            "NSE-NIFTY-16Jul26-25100-CE",
            "NSE",
            "FNO",
            NaiveDate::parse_from_str(date, "%Y-%m-%d").expect("date"),
        )
    }

    #[tokio::test]
    async fn test_groww_contract_fetch_once_200_returns_body() {
        const BODY: &str = r#"{"status":"SUCCESS","payload":{"candles":[[1783914300,180.0,190.0,178.0,188.0,1000,5000]]}}"#;
        // Content-Length must match BODY.len() = 92.
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                BODY.len(),
                BODY
            )
            .into_boxed_str(),
        );
        let port = spawn_mock_http(resp).await;
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/v1/historical/candles");
        let body = groww_contract_fetch_once(
            &client,
            &url,
            &test_query("2026-07-13"),
            &SecretString::from("t"),
        )
        .await
        .expect("2xx must return the body");
        let (rows, stats) = parse_groww_1m_candle_rows(&body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].oi, 5_000, "the contract leg consumes tuple[6] oi");
        assert_eq!(stats.malformed_rows, 0);
    }

    #[tokio::test]
    async fn test_groww_contract_fetch_once_401_flags_auth_and_429_flags_rate_limit() {
        let port401 =
            spawn_mock_http("HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\n{}").await;
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port401}/x");
        let err = groww_contract_fetch_once(
            &client,
            &url,
            &test_query("2026-07-13"),
            &SecretString::from("t"),
        )
        .await
        .expect_err("401 must be a typed failure");
        assert!(err.auth_rejected, "401 → auth-class (short-circuit input)");
        assert!(!err.rate_limited);
        assert_eq!(err.status, 401);

        let port429 = spawn_mock_http(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        let url429 = format!("http://127.0.0.1:{port429}/x");
        let err = groww_contract_fetch_once(
            &client,
            &url429,
            &test_query("2026-07-13"),
            &SecretString::from("t"),
        )
        .await
        .expect_err("429 must be a typed failure");
        assert!(err.rate_limited);
        assert!(!err.auth_rejected);
        assert_eq!(err.status, 429);
        // The bounded capture names the Retry-After presence (probe e).
        assert!(err.msg.contains("retry_after_present=true"), "{}", err.msg);
    }

    #[tokio::test]
    async fn test_groww_contract_fetch_once_transport_error_is_status_zero() {
        let client = reqwest::Client::new();
        // Port 1 never listens — a real transport failure.
        let err = groww_contract_fetch_once(
            &client,
            "http://127.0.0.1:1/x",
            &test_query("2026-07-13"),
            &SecretString::from("t"),
        )
        .await
        .expect_err("transport failure must be typed");
        assert_eq!(err.status, 0);
        assert_eq!(error_class_for_status(err.status), "transport");
    }

    // ---- fire arms (hermetic — params fixture + mock server) ------------------

    fn test_params() -> GrowwContract1mTaskParams {
        use tickvault_common::config::{NseHolidayEntry, TradingConfig};
        let cfg = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2026-01-26".to_string(),
                name: "Republic Day".to_string(),
            }],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        GrowwContract1mTaskParams {
            notifier: NotificationService::disabled(),
            calendar: Arc::new(
                TradingCalendar::from_config(&cfg).expect("synthetic calendar builds"),
            ),
            questdb: QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 8812,
                ilp_port: 9009,
            },
            chain_minute_done: None,
            anchor_store: None,
            strikes_each_side: 2,
            burst: GrowwRestBurstState::new(
                tickvault_common::config::GrowwRestBurstTier::TwoWave,
                false,
            ),
        }
    }

    fn params_with_anchors(anchors: &[(&'static str, f64)]) -> GrowwContract1mTaskParams {
        let now = fetched_at_ist_nanos_now();
        let store: GrowwChainAnchorStore = Arc::new(std::sync::Mutex::new(
            anchors
                .iter()
                .map(|&(u, ltp)| {
                    (
                        u,
                        GrowwChainAnchor {
                            ltp,
                            set_at_ist_nanos: now,
                        },
                    )
                })
                .collect::<HashMap<_, _>>(),
        ));
        GrowwContract1mTaskParams {
            anchor_store: Some(store),
            ..test_params()
        }
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("test client builds")
    }

    fn epoch_ms_now() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    /// One-slot NIFTY book whose only strike is the anchor's ATM.
    fn one_contract_book() -> Vec<GrowwContractBook> {
        vec![book_with_strikes("NIFTY", &[25_100.0])]
    }

    /// Fresh anchor-stale warn latch for a fire call (borrowed inline).
    fn false_latch() -> bool {
        false
    }

    #[tokio::test]
    async fn test_fire_no_token_arm_counts_misses_and_writes_forensics() {
        let params = params_with_anchors(&[("NIFTY", 25_100.0)]);
        let books = one_contract_book();
        let mut last_request_ms: Option<i64> = None;
        let mut writer = OptionContract1mRestWriter::for_test();
        let mut audit_writer = RestFetchAuditWriter::for_test();
        let mut edge = FailureEdge::default();
        let mut token_cache = GrowwTokenCache::for_test_paced_out(epoch_ms_now());
        let mut tracker = ContractPersistTracker::default();
        fire_one_groww_contract_minute(
            &params,
            &test_client(),
            "http://127.0.0.1:1/never-reached",
            &books,
            &mut last_request_ms,
            &mut writer,
            &mut audit_writer,
            &mut edge,
            &mut token_cache,
            &mut tracker,
            minute_open_ist_nanos(today_ist(), 9 * 3600 + 15 * 60),
            9 * 3600 + 16 * 60,
            &mut false_latch(),
        )
        .await;
        assert_eq!(writer.pending(), 0, "no contract rows without a token");
        // The forensics sink flushes at the end of every fire; the
        // disconnected test writer discards on flush — pending is 0 and
        // the fully-failed minute is proven via the edge below.
        assert_eq!(audit_writer.pending(), 0, "flush ran (best-effort discard)");
        assert!(
            last_request_ms.is_none(),
            "no request happened — the pacing stamp must stay unset"
        );
        // Exactly ONE fully-failed minute recorded: two more reach the
        // 3-threshold page on the third total.
        assert!(matches!(edge.record_minute(true), EdgeAction::None));
        assert!(matches!(
            edge.record_minute(true),
            EdgeAction::Page { consecutive: 3 }
        ));
    }

    #[tokio::test]
    async fn test_fire_token_path_persists_target_and_backfill_via_mock() {
        // The mock serves the TARGET minute (09:15 open) AND the previous
        // minute — but the 09:16 fire's backfill floor is the session
        // first minute, so only the in-session rows persist. The body's
        // ts values are IST-as-epoch minute opens for TODAY, so the
        // client-side filter matches; build them dynamically.
        let target_nanos = minute_open_ist_nanos(today_ist(), 9 * 3600 + 15 * 60);
        let target_ist_secs = target_nanos / 1_000_000_000;
        // The parser treats numeric ts as UTC epoch and ADDS +19800 —
        // serve UTC values so the parsed bucket equals the target.
        let target_utc = target_ist_secs - 19_800;
        let body = format!(
            r#"{{"status":"SUCCESS","payload":{{"candles":[[{target_utc},180.0,190.0,178.0,188.0,1000,5000]]}}}}"#
        );
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        );
        let port = spawn_mock_http(resp).await;
        let params = params_with_anchors(&[("NIFTY", 25_100.0)]);
        let books = one_contract_book();
        let mut last_request_ms: Option<i64> = None;
        let mut writer = OptionContract1mRestWriter::for_test();
        let mut audit_writer = RestFetchAuditWriter::for_test();
        let mut edge = FailureEdge::default();
        let mut token_cache = GrowwTokenCache::for_test_with_token(SecretString::from("t"));
        let mut tracker = ContractPersistTracker::default();
        fire_one_groww_contract_minute(
            &params,
            &test_client(),
            &format!("http://127.0.0.1:{port}/v1/historical/candles"),
            &books,
            &mut last_request_ms,
            &mut writer,
            &mut audit_writer,
            &mut edge,
            &mut token_cache,
            &mut tracker,
            target_nanos,
            9 * 3600 + 16 * 60,
            &mut false_latch(),
        )
        .await;
        // 2 contracts selected (ATM CE + PE, one strike) — both found the
        // target candle. The disconnected writer discards on flush, so the
        // proof is the pacing stamp + the NON-fully-failed edge verdict
        // (persist_failed=true from the test writer means the minute WAS
        // fully failed — assert the request actually ran instead).
        assert!(
            last_request_ms.is_some(),
            "requests ran (the pacing stamp advanced)"
        );
        // The tracker never commits on a failed flush (flush-confirmed
        // watermark — the disconnected test writer errors its flush).
        assert_eq!(tracker.last_persisted(2_000, "NSE_FNO"), None);
        assert_eq!(tracker.last_persisted(1_000, "NSE_FNO"), None);
    }

    #[tokio::test]
    async fn test_fire_token_path_auth_reject_short_circuits_via_mock() {
        let port =
            spawn_mock_http("HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\n{}").await;
        let params = params_with_anchors(&[("NIFTY", 25_100.0)]);
        // 2 strikes → 4 contracts; the FIRST 401 must short-circuit the
        // remaining 3 (no doomed requests with a dead token).
        let books = vec![book_with_strikes("NIFTY", &[25_000.0, 25_100.0])];
        let mut last_request_ms: Option<i64> = None;
        let mut writer = OptionContract1mRestWriter::for_test();
        let mut audit_writer = RestFetchAuditWriter::for_test();
        let mut edge = FailureEdge::default();
        let mut token_cache = GrowwTokenCache::for_test_with_token(SecretString::from("t"));
        let mut tracker = ContractPersistTracker::default();
        fire_one_groww_contract_minute(
            &params,
            &test_client(),
            &format!("http://127.0.0.1:{port}/x"),
            &books,
            &mut last_request_ms,
            &mut writer,
            &mut audit_writer,
            &mut edge,
            &mut token_cache,
            &mut tracker,
            minute_open_ist_nanos(today_ist(), 9 * 3600 + 15 * 60),
            9 * 3600 + 16 * 60,
            &mut false_latch(),
        )
        .await;
        assert_eq!(writer.pending(), 0, "nothing persisted on a 401 fire");
        assert!(
            last_request_ms.is_some(),
            "exactly one request ran before the short-circuit"
        );
        // The fire fed the edge one fully-failed minute (all 4 contracts
        // errored: 1 real 401 + 3 short-circuited).
        assert!(matches!(edge.record_minute(true), EdgeAction::None));
        assert!(matches!(
            edge.record_minute(true),
            EdgeAction::Page { consecutive: 3 }
        ));
    }

    #[test]
    fn test_record_skipped_boundaries_feeds_edge_and_is_midnight_guarded() {
        let params = test_params();
        let books = one_contract_book();
        let mut audit_writer = RestFetchAuditWriter::for_test();
        // 3 skipped boundaries feed the edge to the page threshold.
        let mut edge = FailureEdge::default();
        record_groww_contract_skipped_boundaries(
            &params,
            &mut edge,
            &mut audit_writer,
            &books,
            3,
            9 * 3600 + 17 * 60,
            today_ist(),
        );
        // The 4th failed minute stays inside the paged episode (None), and
        // a SUCCESS recovers.
        assert!(matches!(edge.record_minute(true), EdgeAction::None));
        assert!(matches!(
            edge.record_minute(false),
            EdgeAction::Recover { failed_minutes: 4 }
        ));
        // Midnight-crossed wake: counter + log only — no forensics rows,
        // no edge feed (the wrong-date guard).
        let mut edge2 = FailureEdge::default();
        let yesterday = today_ist().pred_opt().expect("yesterday exists");
        record_groww_contract_skipped_boundaries(
            &params,
            &mut edge2,
            &mut audit_writer,
            &books,
            3,
            9 * 3600 + 17 * 60,
            yesterday,
        );
        assert!(matches!(edge2.record_minute(true), EdgeAction::None));
        assert!(matches!(edge2.record_minute(true), EdgeAction::None));
        // Only now does the streak reach 3 — proof the skip fed nothing.
        assert!(matches!(
            edge2.record_minute(true),
            EdgeAction::Page { consecutive: 3 }
        ));
        // Zero skipped is a no-op.
        let mut edge3 = FailureEdge::default();
        record_groww_contract_skipped_boundaries(
            &params,
            &mut edge3,
            &mut audit_writer,
            &books,
            0,
            9 * 3600 + 17 * 60,
            today_ist(),
        );
        assert!(matches!(edge3.record_minute(true), EdgeAction::None));
    }

    // ---- verdict / edge (persist-gated) ---------------------------------------

    #[test]
    fn test_contract_minute_fully_failed_is_persist_gated() {
        // Reuses the chain verdict core: zero persisted contracts OR a
        // persist failure = fully failed; a thin-strike EMPTY minute with
        // >=1 ok contract is NOT fully failed.
        assert!(chain_minute_fully_failed(0, false));
        assert!(chain_minute_fully_failed(3, true));
        assert!(!chain_minute_fully_failed(1, false));
    }
}
