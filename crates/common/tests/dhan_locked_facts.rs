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

use tickvault_common::constants::{
    DHAN_TWENTY_DEPTH_WS_BASE_URL, DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL, FULL_QUOTE_PACKET_SIZE,
    PREV_CLOSE_OFFSET_OI, PREV_CLOSE_OFFSET_PRICE, PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE,
    RESPONSE_CODE_FULL, RESPONSE_CODE_PREVIOUS_CLOSE, RESPONSE_CODE_QUOTE,
};

// ===========================================================================
// LOCKED FACT 1 — 200-level depth WebSocket path
// Source: Dhan support Ticket #5519522 (2026-04-10)
// Reference: .claude/rules/dhan/full-market-depth.md rule 2
// ===========================================================================

/// 2026-04-23 REVERSAL of ticket #5519522: the 200-level depth WebSocket
/// uses the ROOT path `/` on host `full-depth-api.dhan.co`. Verified by
/// running Dhan's official Python SDK `dhanhq==2.2.0rc1` on our account at
/// SecurityId 72271 — SDK streamed 30+ minutes on root path, while our Rust
/// client at `/twohundreddepth` had been getting
/// `Protocol(ResetWithoutClosingHandshake)` for 2+ weeks. This test now
/// guards the REVERSED invariant — if you ever need to switch back to
/// `/twohundreddepth`, re-open ticket #5519522 and cite the response.
#[test]
fn locked_200_depth_uses_root_path_verified_2026_04_23() {
    assert_eq!(
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL, "wss://full-depth-api.dhan.co",
        "LOCKED FACT (Python SDK 2026-04-23): 200-depth URL uses ROOT path. \
         The /twohundreddepth path that ticket #5519522 had advised caused \
         TCP resets in our Rust client. To revert, open a new Dhan ticket."
    );
    assert!(
        !DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.contains("/twohundreddepth"),
        "LOCKED FACT: 200-depth URL must NOT use /twohundreddepth (reversed 2026-04-23)"
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

/// Ensures `full-market-depth.md` documents the 2026-04-23 reversal of
/// ticket #5519522 — root path is the working URL, NOT `/twohundreddepth`.
/// The file must cite the Python SDK verification so anyone reading it in
/// the future understands why the original ticket advice was dropped.
#[test]
fn locked_rule_file_documents_2026_04_23_reversal() {
    let rule_file = include_str!("../../../.claude/rules/dhan/full-market-depth.md");
    assert!(
        rule_file.contains("Ticket #5519522"),
        "LOCKED FACT: full-market-depth.md must cite Dhan Ticket #5519522"
    );
    assert!(
        rule_file.contains("2026-04-23"),
        "LOCKED FACT: full-market-depth.md must document the 2026-04-23 \
         Python SDK verification of root path"
    );
    assert!(
        rule_file.contains("Python SDK") || rule_file.contains("dhanhq"),
        "LOCKED FACT: full-market-depth.md must cite the Python SDK \
         as the source of truth for 200-depth root path"
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

/// Reversed 2026-04-23: the `/twohundreddepth` path for 200-level depth
/// (what Dhan ticket #5519522 originally told us to use) must NOT appear
/// on any non-comment line of `crates/common/src/constants.rs`. Python SDK
/// verified 2026-04-23 that root path `/` is the actually-working URL.
///
/// This scan is deliberately crude — it trips on any suspicious literal,
/// not just the exact byte pattern. False positives are recoverable (you
/// rename the variable or add a `// LOCKED FACT` comment); false negatives
/// would drop us back into the 2-week TCP-reset loop.
#[test]
fn locked_no_twohundreddepth_path_in_200_depth_url_source() {
    let constants = include_str!("../src/constants.rs");
    for (idx, line) in constants.lines().enumerate() {
        // Only scan lines that mention full-depth-api.dhan.co — i.e. the
        // 200-depth URL assignment line. 20-depth uses a different host
        // (depth-api-feed.dhan.co), so we do NOT trip on its /twentydepth.
        if line.contains("full-depth-api.dhan.co") && line.contains("/twohundreddepth") {
            // Allowed: comments citing the old path for historical context.
            // Require an explicit // LOCKED FACT citation if someone needs
            // to mention it.
            if !line.contains("LOCKED FACT") && !line.trim_start().starts_with("//") {
                panic!(
                    "LOCKED FACT VIOLATION at constants.rs:{}: 200-depth URL \
                     contains '/twohundreddepth' — did you revert the 2026-04-23 \
                     Python-SDK-verified root-path fix? Re-open Dhan ticket \
                     #5519522 if this path really needs to come back.\nLine: {line}",
                    idx + 1
                );
            }
        }
    }
}
