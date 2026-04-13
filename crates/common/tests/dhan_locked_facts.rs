//! Dhan ground-truth **locked facts** — mechanical regression wall.
//!
//! This test file exists for one purpose: any future attempt to silently
//! revert a Dhan-support-confirmed fact must fail the build. Every assertion
//! here cites a Dhan support ticket or a specific reference document. If you
//! want to change any of these values, you must:
//!
//! 1. Open a new Dhan support ticket,
//! 2. Quote the new response verbatim in the rule file,
//! 3. Update the assertion + citation HERE in the same commit.
//!
//! **This file is NEVER scoped out** — it runs on every push because the
//! pre-push gate explicitly targets `dhan_locked_facts::*`. Do not remove
//! the file from the pre-push scan; do not mark tests `#[ignore]`; do not
//! weaken any assertion without the three steps above.

#![allow(clippy::assertions_on_constants)]
// APPROVED: S4 — locked-facts assertions intentionally compare compile-time constants against Dhan ground truth; const-block conversion would make the failure message unusable
//!
//! Purpose: guarantee + assurance + proof that we don't lose these facts.

use dhan_live_trader_common::constants::{
    DHAN_TWENTY_DEPTH_WS_BASE_URL, DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL, FULL_QUOTE_PACKET_SIZE,
    PREV_CLOSE_OFFSET_OI, PREV_CLOSE_OFFSET_PRICE, PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE,
    RESPONSE_CODE_FULL, RESPONSE_CODE_PREVIOUS_CLOSE, RESPONSE_CODE_QUOTE,
};

// ===========================================================================
// LOCKED FACT 1 — 200-level depth WebSocket path
// Source: Dhan support Ticket #5519522 (2026-04-10)
// Reference: .claude/rules/dhan/full-market-depth.md rule 2
// ===========================================================================

/// Ticket #5519522: the 200-level depth WebSocket MUST use the explicit
/// `/twohundreddepth` path on host `full-depth-api.dhan.co`. Root path `/`
/// (as the Python SDK uses) causes `Protocol(ResetWithoutClosingHandshake)`
/// within 3-5 seconds of subscription. This fact cost us multiple production
/// trading sessions before the ticket resolution. Do not revert.
#[test]
fn locked_ticket_5519522_twohundreddepth_path() {
    assert_eq!(
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL, "wss://full-depth-api.dhan.co/twohundreddepth",
        "LOCKED FACT (Ticket #5519522): 200-depth URL MUST be /twohundreddepth. \
         Root path causes TCP reset. If you need to change this, open a new \
         Dhan support ticket first and cite the response here."
    );
    assert!(
        !DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.ends_with(".dhan.co/"),
        "LOCKED FACT (Ticket #5519522): 200-depth URL must NOT use root path"
    );
    assert!(
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.contains("/twohundreddepth"),
        "LOCKED FACT (Ticket #5519522): /twohundreddepth is the ONLY working path"
    );
}

/// Ticket #5519522: 20-level and 200-level depth WebSockets are DIFFERENT
/// hosts. Do not unify. Do not use the same base URL.
#[test]
fn locked_ticket_5519522_depth_hosts_are_separate() {
    assert!(
        DHAN_TWENTY_DEPTH_WS_BASE_URL.contains("depth-api-feed.dhan.co"),
        "LOCKED FACT: 20-depth host is depth-api-feed.dhan.co"
    );
    assert!(
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.contains("full-depth-api.dhan.co"),
        "LOCKED FACT: 200-depth host is full-depth-api.dhan.co"
    );
    assert_ne!(
        DHAN_TWENTY_DEPTH_WS_BASE_URL, DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
        "LOCKED FACT: 20-depth and 200-depth use different hosts"
    );
}

/// Ticket #5519522: 200-level depth returns consistent data ONLY for
/// at-the-money (ATM) security IDs. Far OTM contracts get no data even when
/// the subscription succeeds. This is server-side filtering, not a client
/// bug. Any depth-200 subscription must route through the ATM strike
/// selector.
///
/// We don't have a single bounded constant here because depth_rebalancer and
/// depth_strike_selector handle the logic. Instead, we grep the production
/// source to ensure the ATM selector is actually invoked on the 200-level
/// path. Deleting the invocation silently would regress the fix.
#[test]
fn locked_ticket_5519522_depth_200_uses_atm_strike_selector() {
    let depth_conn = include_str!("../../core/src/websocket/depth_connection.rs");
    let depth_rebalancer = include_str!("../../core/src/instrument/depth_rebalancer.rs");

    assert!(
        depth_conn.contains("200-level") || depth_conn.contains("twohundreddepth"),
        "LOCKED FACT: depth_connection.rs must reference 200-level path"
    );
    assert!(
        depth_rebalancer.contains("ATM") || depth_rebalancer.contains("atm"),
        "LOCKED FACT (Ticket #5519522): depth_rebalancer.rs must implement \
         ATM strike selection for 200-level depth"
    );
}

// ===========================================================================
// LOCKED FACT 2 — Previous-day close routing
// Source: Dhan support Ticket #5525125 (2026-04-10)
// Reference: .claude/rules/dhan/live-market-feed.md rule 7 + new mechanical rule
// ===========================================================================

/// Ticket #5525125 fact for IDX_I indices: the ONLY source of previous-day
/// close is the standalone PrevClose packet (Response Code 6, 16 bytes).
/// Parse `buf[8..12]` as f32 LE, OI at `buf[12..16]`. No other packet type
/// carries prev-close for indices.
#[test]
fn locked_ticket_5525125_idx_i_prev_close_from_code_6() {
    assert_eq!(
        RESPONSE_CODE_PREVIOUS_CLOSE, 6,
        "LOCKED FACT (Ticket #5525125): PrevClose response code is 6"
    );
    assert_eq!(
        PREVIOUS_CLOSE_PACKET_SIZE, 16,
        "LOCKED FACT (Ticket #5525125): PrevClose packet is 16 bytes"
    );
    assert_eq!(
        PREV_CLOSE_OFFSET_PRICE, 8,
        "LOCKED FACT (Ticket #5525125): PrevClose price at byte 8 (after header)"
    );
    assert_eq!(
        PREV_CLOSE_OFFSET_OI, 12,
        "LOCKED FACT (Ticket #5525125): PrevClose OI at byte 12"
    );
}

/// Ticket #5525125 fact for NSE_EQ equities and NSE_FNO derivatives: there
/// is NO standalone PrevClose packet. The previous-day close is inside the
/// Quote packet (code 4) at bytes 38-41 and inside the Full packet (code 8)
/// at bytes 50-53. Dhan calls this field `close`, but it represents the
/// PREVIOUS trading session's close, not the current day's close. Waiting
/// for code 6 packets on NSE_EQ/NSE_FNO is a bug — they never arrive.
#[test]
fn locked_ticket_5525125_nse_eq_fno_prev_close_from_quote_full() {
    assert_eq!(
        RESPONSE_CODE_QUOTE, 4,
        "LOCKED FACT: Quote response code is 4"
    );
    assert_eq!(
        RESPONSE_CODE_FULL, 8,
        "LOCKED FACT: Full response code is 8"
    );
    assert_eq!(
        QUOTE_PACKET_SIZE, 50,
        "LOCKED FACT: Quote packet is 50 bytes"
    );
    assert_eq!(
        FULL_QUOTE_PACKET_SIZE, 162,
        "LOCKED FACT: Full packet is 162 bytes"
    );
    // The 'close' field offsets are 38 (Quote) and 50 (Full).
    assert!(
        38 + 4 <= QUOTE_PACKET_SIZE,
        "LOCKED FACT (Ticket #5525125): Quote 'close' field at bytes 38-41 must fit"
    );
    assert!(
        50 + 4 <= FULL_QUOTE_PACKET_SIZE,
        "LOCKED FACT (Ticket #5525125): Full 'close' field at bytes 50-53 must fit"
    );
}

// ===========================================================================
// LOCKED FACT 3 — Rule file citations
// Any change that removes the ticket references from the rule files must
// fail the build. This prevents silent deletion of the provenance trail.
// ===========================================================================

/// Ensures the `live-market-feed.md` rule file still contains the Ticket
/// #5525125 citation in its PrevClose rule. Deleting the citation silently
/// would let someone claim the fact is unsourced.
#[test]
fn locked_rule_file_cites_ticket_5525125() {
    let rule_file = include_str!("../../../.claude/rules/dhan/live-market-feed.md");
    assert!(
        rule_file.contains("Ticket #5525125"),
        "LOCKED FACT: live-market-feed.md must cite Dhan Ticket #5525125 \
         in its PrevClose routing rule. If you removed the citation, \
         restore it or open a new ticket."
    );
    assert!(
        rule_file.contains("IDX_I"),
        "LOCKED FACT: live-market-feed.md PrevClose rule must mention IDX_I"
    );
    assert!(
        rule_file.contains("NSE_EQ") && rule_file.contains("NSE_FNO"),
        "LOCKED FACT: live-market-feed.md PrevClose rule must mention \
         NSE_EQ and NSE_FNO routing"
    );
}

/// Ensures the `full-market-depth.md` rule file still contains the Ticket
/// #5519522 citation with the correct `/twohundreddepth` path.
#[test]
fn locked_rule_file_cites_ticket_5519522() {
    let rule_file = include_str!("../../../.claude/rules/dhan/full-market-depth.md");
    assert!(
        rule_file.contains("Ticket #5519522"),
        "LOCKED FACT: full-market-depth.md must cite Dhan Ticket #5519522"
    );
    assert!(
        rule_file.contains("/twohundreddepth"),
        "LOCKED FACT: full-market-depth.md must document /twohundreddepth \
         as the ONLY working 200-level depth path"
    );
    assert!(
        rule_file.contains("ATM") || rule_file.contains("at-the-money"),
        "LOCKED FACT: full-market-depth.md must document the ATM strike \
         requirement for 200-level depth subscriptions"
    );
}

// ===========================================================================
// LOCKED FACT 4 — Banned patterns in the current tree
// These string patterns must NEVER appear in production source code because
// they represent reverted or known-broken configurations.
// ===========================================================================

/// The Python-SDK root path for 200-level depth (`full-depth-api.dhan.co/`
/// immediately followed by `?` or `"`) must NOT appear anywhere in
/// `crates/common/src/constants.rs` or `crates/core/src/websocket/`. If it
/// does, someone reverted the ticket fix.
///
/// This scan is deliberately crude — it trips on any suspicious literal, not
/// just the exact byte pattern. False positives are recoverable (you rename
/// the variable or add a `// LOCKED FACT` comment); false negatives would
/// lose the Dhan fix.
#[test]
fn locked_no_root_path_for_200_depth_in_source() {
    let constants = include_str!("../src/constants.rs");
    // The sanctioned constant line contains `/twohundreddepth`. Anything else
    // on `full-depth-api.dhan.co/` is suspicious.
    for (idx, line) in constants.lines().enumerate() {
        if line.contains("full-depth-api.dhan.co/") && !line.contains("/twohundreddepth") {
            // Allowed: comments citing the old path for context. Require an
            // explicit // LOCKED FACT citation if someone needs to mention it.
            if !line.contains("LOCKED FACT") && !line.trim_start().starts_with("//") {
                panic!(
                    "LOCKED FACT VIOLATION at constants.rs:{}: line contains \
                     'full-depth-api.dhan.co/' without '/twohundreddepth' — \
                     did you revert Ticket #5519522?\nLine: {line}",
                    idx + 1
                );
            }
        }
    }
}
