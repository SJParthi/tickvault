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
    FULL_QUOTE_PACKET_SIZE, PREV_CLOSE_OFFSET_OI, PREV_CLOSE_OFFSET_PRICE,
    PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE, RESPONSE_CODE_FULL,
    RESPONSE_CODE_PREVIOUS_CLOSE, RESPONSE_CODE_QUOTE,
};

// ===========================================================================
// LOCKED FACT 1 — 20-level / 200-level depth WebSocket invariants
//
// PR #4 (2026-05-19): All Ticket #5519522 / 2026-04-23 depth-URL ratchets
// retired alongside the deleted depth-20 / depth-200 WebSocket
// infrastructure. Operator-locked per
// `.claude/rules/project/websocket-connection-scope-lock.md`: under the
// 4-IDX_I LOCKED_UNIVERSE we use ONLY 1 main-feed conn + 1 order-update
// conn forever — no separate depth feeds. The dependent URL constants
// (`DHAN_TWENTY_DEPTH_WS_BASE_URL`, `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL`)
// are RETAINED as ORPHANED pub consts in constants.rs — zero consumers
// anywhere, measured by
// `dhan_api_coverage.rs::test_depth_ws_url_constants_are_orphaned`
// (round-8 truth-sync 2026-07-15: this comment previously narrated the
// constants deleted while both are live declarations on the tree). If
// depth ever returns (a scope-lock rule edit FIRST), re-open Ticket
// #5519522 and restore these ratchets alongside the new consumers.
// ===========================================================================

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

// PR #4 (2026-05-19): `locked_rule_file_documents_2026_04_23_reversal` +
// `locked_no_twohundreddepth_path_in_200_depth_url_source` retired alongside
// the deleted depth-20 / depth-200 WebSocket infrastructure + the deletion
// of `.claude/rules/dhan/full-market-depth.md`. See LOCKED FACT 1 above.
