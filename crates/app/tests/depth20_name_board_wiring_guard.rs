//! The depth-20 NAME board must be REACHED, not merely present (2026-09-13).
//!
//! `depth20_name_board.rs` shipped on 2026-09-11 with 785 lines, thirty unit
//! tests, five compile-time assertions — and **zero production call sites**.
//! Its only mention outside itself was `pub mod depth20_name_board;` in
//! `lib.rs`. That is the skeleton-PR shape `audit-findings-2026-04-17.md`
//! Rule 14 forbids, and it is the shape this repository has recorded
//! repeatedly: `scan_silence` sat seeded and observed with no production
//! reader for weeks, reading greener than dead code because every part of it
//! looked connected.
//!
//! Its own unit tests prove the ARITHMETIC. They prove nothing about whether
//! anything calls it. This file is the other half.
//!
//! | Pinned here | What its loss costs |
//! |---|---|
//! | the board is reached from the steering loop | the module is dormant again, and nothing says so |
//! | the board is PRIMARY, ahead of the volume ranking | the 2026-09-11 authorization is recorded and not applied |
//! | `held_names` is carried forward | the exit band is computed and discarded — churn control that reports control it does not have |
//! | the slot arithmetic still sums to 238 ≤ 250 | `plan_pool` refuses the WHOLE pool fail-closed: a session with no depth at all |
//! | NIFTY and BANKNIFTY cannot be displaced | the two deepest books lose their sockets to a mover |
//! | a missing price ranks `None`, never `0` | an unpriced name ties with a genuinely flat one and takes a socket it did not earn |
//! | the comparator takes an INTEGER | a `NaN` comparator is non-transitive and `sort_unstable_by` corrupts the slice WHOLESALE |
//!
//! # How the source scans below are made non-vacuous
//!
//! Three rules, because this repository has recorded **eleven** guards that
//! were satisfied by the very assertion text naming them:
//!
//! 1. the file's own `#[cfg(test)]` module is CUT before searching, so a
//!    fixture or an assertion message can never satisfy a production scan;
//! 2. every positional `find()` is preceded by an occurrence COUNT, so a
//!    second match cannot silently move the anchor;
//! 3. [`guard_self_test`] bite-proves that each scan can FAIL.

use std::fs;

use tickvault_app::depth_rebalance::MoverRow;
use tickvault_app::depth20_layout::DEPTH_20_SOCKETS;
use tickvault_app::depth20_name_board::{
    DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE, DEPTH20_INSTRUMENT_BUDGET, DEPTH20_NAME_ENTRY_RANK,
    DEPTH20_NAME_EXIT_RANK, DEPTH20_NAME_STOCK_SOCKETS, DEPTH20_PER_SOCKET,
    DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE, NameBoardPlan, NameMove, board_slot_cost,
    build_name_layout, future_index, move_bps, move_bps_from_pct, name_moves, rank_names,
    slots_for_index_name, slots_for_stock_name,
};
use tickvault_app::dhan_contract_universe::ContractRow;
use tickvault_app::dhan_depth_universe::DepthCandidate;
use tickvault_common::types::ExchangeSegment;

/// Production source only: line comments removed AND the file's own
/// `#[cfg(test)]` module cut off.
///
/// Both halves are load-bearing and neither is tidiness:
///
/// * **Comments.** A call site deleted and left behind as
///   `// build_name_layout(...)` would keep every `contains` assertion below
///   green while the board is dormant.
/// * **The test module.** Every string this guard searches for is ALSO written
///   inside `depth_rebalance.rs`'s own test module — the fixtures added with
///   this change name `cap_depth20_socket_swaps` directly. Scanning the whole
///   file would let a test satisfy a production assertion, which is precisely
///   the vacuous shape this repository has recorded eleven times.
fn production_source(path: &str) -> String {
    let body = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    // `\n#[cfg(test)]` anchors on a line start, so an inner `#[cfg(test)]`
    // attribute on an indented item does not truncate the file early.
    let body = match body.find("\n#[cfg(test)]") {
        Some(at) => body[..at].to_owned(),
        None => body,
    };
    body.lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Asserts `needle` occurs EXACTLY `want` times, then returns its first byte
/// offset — so no positional assertion is ever taken against an anchor a
/// second occurrence could have moved.
fn offset_of_only(haystack: &str, needle: &str, want: usize) -> usize {
    let found = haystack.matches(needle).count();
    assert_eq!(
        found, want,
        "expected {want} occurrence(s) of {needle:?} in the scanned production \
         source, found {found} — a positional assertion against a moved anchor \
         proves nothing"
    );
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not found"))
}

// ---------------------------------------------------------------------------
// 1. The board is REACHED, and it is PRIMARY.
// ---------------------------------------------------------------------------

#[test]
fn the_steering_loop_builds_the_name_board_every_minute() {
    let loop_body = production_source("src/depth_rebalance.rs");
    offset_of_only(
        &loop_body,
        "crate::depth20_name_board::build_name_layout(",
        1,
    );
    offset_of_only(&loop_body, "crate::depth20_name_board::future_index(", 1);
    assert!(
        loop_body.contains("name_plan.is_steerable()"),
        "the steering loop no longer asks the name board whether it can steer — \
         the module is dormant again and nothing says so"
    );
}

#[test]
fn the_name_board_is_primary_and_the_older_engines_are_the_fallback() {
    // The 2026-09-11 (THIRD) authorization makes the name board the depth-20
    // engine. If the volume ranking were consulted FIRST the authorization
    // would be recorded and not applied — a change that reads as shipped.
    let loop_body = production_source("src/depth_rebalance.rs");
    let steerable = offset_of_only(&loop_body, "if name_plan.is_steerable() {", 1);
    let fallback = offset_of_only(&loop_body, "match ranking_20.as_deref() {", 1);
    assert!(
        steerable < fallback,
        "the volume ranking is being consulted before the name board — the \
         name board must be PRIMARY, with the older engines behind it for the \
         pre-09:07 window in which it cannot yet produce a layout"
    );
    // and the fallback must still exist: it is the ONLY engine between 09:00
    // and the first resolvable movers ranking.
    for arm in [
        "plan_depth20_ranked_minute(",
        "seed_until_first_ranking",
        "layout_until_first_ranking",
    ] {
        assert!(
            loop_body.contains(arm),
            "the {arm} fallback arm is gone — the pre-open window has no engine"
        );
    }
}

#[test]
fn the_exit_band_governs_the_next_minute_because_the_choice_is_carried_forward() {
    // THE hysteresis wiring. `NameBoard::keeps` is inert unless the names the
    // board chose are fed back in as `held`: without this line the band is
    // computed and discarded every minute, the board re-orders freely, and the
    // swap counters report churn control it does not have.
    let loop_body = production_source("src/depth_rebalance.rs");
    offset_of_only(&loop_body, "held_names = name_plan.chosen_keys();", 1);
    let declared = offset_of_only(&loop_body, "let mut held_names:", 1);
    let consumed = offset_of_only(&loop_body, "&held_names,", 1);
    assert!(
        declared < consumed,
        "`held_names` must OUTLIVE the per-minute iteration; declared inside \
         the loop it would be empty every minute and the band would never bite"
    );
    // A COMPILE-TIME assertion, not a runtime one: both sides are consts, so
    // a runtime `assert!` here could never fail a test run that compiled. This
    // fails the BUILD if someone narrows the band to the entry set - which
    // would leave `keeps` equal to `admits` and the hysteresis inert while
    // every name above still reported that churn was controlled.
    const {
        assert!(
            DEPTH20_NAME_EXIT_RANK > DEPTH20_NAME_ENTRY_RANK,
            "a band no wider than the entry set is not a band"
        );
    }
}

#[test]
fn the_plan_is_capped_to_what_one_connection_can_queue() {
    // Rotating one name is 24 contracts out and 24 in against a command
    // channel four deep. Uncapped, the excess becomes a `channel_full` refusal
    // — the swap is lost either way, and capping makes the loss deliberate,
    // deterministic and countable rather than a queue overflow.
    let loop_body = production_source("src/depth_rebalance.rs");
    offset_of_only(&loop_body, "cap_depth20_socket_swaps(", 2);
    assert!(
        loop_body.contains("MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE"),
        "the cap must be the socket's own command-channel depth, never a \
         literal that can drift away from it"
    );
}

// ---------------------------------------------------------------------------
// 2. The slot arithmetic.
// ---------------------------------------------------------------------------

#[test]
fn the_board_still_sums_to_238_of_250() {
    // The operator asked directly: "will it sit under 250 slots". These are
    // the numbers, re-derived rather than quoted, so a change to either window
    // moves this test with it instead of leaving a stale literal.
    assert_eq!(
        slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE),
        47
    );
    assert_eq!(
        slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE),
        24
    );
    assert_eq!(
        board_slot_cost(),
        2 * slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
            + DEPTH20_NAME_ENTRY_RANK * slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE)
    );
    assert_eq!(board_slot_cost(), 238);
    assert!(
        board_slot_cost() <= DEPTH20_INSTRUMENT_BUDGET,
        "over the budget `plan_pool` refuses the WHOLE pool fail-closed — a \
         session with no depth at all, not a narrower one"
    );
    // An index name must fit ONE socket or its ladder splits across two
    // connections and neither line carries the whole book.
    assert!(slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE) <= DEPTH20_PER_SOCKET);
    // And the stock half must fit the three sockets it is packed across.
    assert!(
        DEPTH20_NAME_ENTRY_RANK * slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE)
            <= DEPTH20_NAME_STOCK_SOCKETS * DEPTH20_PER_SOCKET
    );
    assert_eq!(2 + DEPTH20_NAME_STOCK_SOCKETS, DEPTH_20_SOCKETS);
}

// ---------------------------------------------------------------------------
// 3. NIFTY and BANKNIFTY are unconditional.
// ---------------------------------------------------------------------------

fn candidate(underlying: &str, strike: f64, leg: &str, id: i64, spot: f64) -> DepthCandidate {
    DepthCandidate {
        underlying: underlying.to_owned(),
        contract_security_id: id,
        expiry_micros: 1_900_000_000_000_000,
        strike,
        spot,
        leg: leg.to_owned(),
        is_index_option: underlying == "NIFTY" || underlying == "BANKNIFTY",
    }
}

fn chain(
    underlying: &str,
    spot: f64,
    step: f64,
    strikes: i64,
    id_base: i64,
) -> Vec<DepthCandidate> {
    let mut out = Vec::new();
    let half = strikes / 2;
    for k in -half..=half {
        #[expect(clippy::cast_precision_loss, reason = "k is a small strike index")]
        let strike = (spot / step).round() * step + (k as f64) * step;
        out.push(candidate(underlying, strike, "CE", id_base + k * 2, spot));
        out.push(candidate(
            underlying,
            strike,
            "PE",
            id_base + k * 2 + 1,
            spot,
        ));
    }
    out
}

fn mover(id: u64, symbol: &str, pct: f64) -> MoverRow {
    MoverRow {
        security_id: id,
        segment: ExchangeSegment::NseEquity,
        symbol: symbol.to_owned(),
        pct_change: pct,
    }
}

fn future(underlying: &str, id: u64, class: &str) -> ContractRow {
    ContractRow {
        i: id,
        x: "NSE".to_owned(),
        c: class.to_owned(),
        e: 20_260_925,
        s: 0,
        l: String::new(),
        u: underlying.to_owned(),
        z: 1,
    }
}

/// Two index chains, eight stock chains, and the movers that rank them.
fn fixture() -> (Vec<DepthCandidate>, Vec<MoverRow>, Vec<ContractRow>) {
    let mut candidates = chain("NIFTY", 24_000.0, 50.0, 40, 100_000);
    candidates.extend(chain("BANKNIFTY", 52_000.0, 100.0, 40, 200_000));
    let mut movers = Vec::new();
    let mut rows = vec![
        future("NIFTY", 1_001, "FUTIDX"),
        future("BANKNIFTY", 1_002, "FUTIDX"),
    ];
    for k in 0..8u64 {
        let symbol = format!("STK{k}");
        #[expect(clippy::cast_precision_loss, reason = "k is 0..8")]
        let pct = 9.0 - (k as f64);
        candidates.extend(chain(
            &symbol,
            100.0,
            5.0,
            40,
            300_000 + i64::try_from(k).unwrap_or(0) * 1_000,
        ));
        movers.push(mover(700 + k, &symbol, pct));
        rows.push(future(&symbol, 2_000 + k, "FUTSTK"));
    }
    (candidates, movers, rows)
}

fn plan_with(movers: &[MoverRow], held: &std::collections::BTreeSet<(u64, u8)>) -> NameBoardPlan {
    let (candidates, _, rows) = fixture();
    let futures = future_index(&rows, 20_260_913);
    build_name_layout(&candidates, movers, &futures, held)
}

#[test]
fn no_mover_can_displace_nifty_or_banknifty() {
    let (_, mut movers, _) = fixture();
    // A stock moving harder than anything else on the board, and a second one
    // carrying an index's own numeric id — the I-P1-11 collision shape, where
    // id 13 is NIFTY in IDX_I and an unrelated cash stock in NSE_EQ.
    movers.push(mover(9_999, "RUNAWAY", 9.99));
    movers.push(mover(13, "COLLIDER", 9.98));
    let plan = plan_with(&movers, &std::collections::BTreeSet::new());
    assert_eq!(plan.layout.sockets.len(), DEPTH_20_SOCKETS);
    assert_eq!(plan.layout.sockets[0].underlying.as_deref(), Some("NIFTY"));
    assert_eq!(
        plan.layout.sockets[1].underlying.as_deref(),
        Some("BANKNIFTY")
    );
    for index_socket in &plan.layout.sockets[..2] {
        assert_eq!(
            index_socket.instruments.len(),
            slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE),
            "an index name holds a whole socket and is never ranked against a \
             mover — it cannot be shortened by one"
        );
    }
    assert!(
        plan.chosen.len() <= DEPTH20_NAME_ENTRY_RANK,
        "the stock half must never exceed the entry rank"
    );
    assert!(plan.is_steerable());
}

// ---------------------------------------------------------------------------
// 4. A missing price ranks `None`, never 0.
// ---------------------------------------------------------------------------

#[test]
fn an_unpriced_name_is_not_rankable_and_is_never_ranked_zero() {
    // The 09:00–09:07 case. ~750 equities have no print at all until the
    // auction, and ranking them at 0% would tie them with a genuinely flat
    // name and hand one a socket it did not earn.
    assert_eq!(move_bps(None, Some(10_000)), None);
    assert_eq!(move_bps(Some(10_000), None), None);
    assert_eq!(move_bps(Some(10_000), Some(0)), None);
    assert_eq!(move_bps_from_pct(f64::NAN), None);
    assert_eq!(move_bps_from_pct(f64::INFINITY), None);
    // and a flat name IS rankable, so the two states stay distinguishable.
    assert_eq!(move_bps(Some(10_000), Some(10_000)), Some(0));
    assert_eq!(move_bps_from_pct(0.0), Some(0));

    // An unrankable row must leave the board entirely rather than arriving at
    // zero — the difference between "no price" and "flat".
    let rows = vec![mover(1, "AAA", f64::NAN), mover(2, "BBB", 0.0)];
    let moves = name_moves(&rows);
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].underlying_id, 2);
    assert_eq!(moves[0].move_bps, 0);

    // and the unrankable name must not reach the layout either.
    let plan = plan_with(&rows, &std::collections::BTreeSet::new());
    assert!(plan.chosen.iter().all(|c| c.underlying_id != 1));
}

// ---------------------------------------------------------------------------
// 5. The comparator takes an INTEGER.
// ---------------------------------------------------------------------------

#[test]
fn the_rank_key_is_an_integer_and_the_comparator_is_total() {
    // Compile-time: these coercions fail to build if either signature ever
    // grows a float. A source scan alone could not say this.
    let _integer_key: fn(Option<i64>, Option<i64>) -> Option<i64> = move_bps;
    let _integer_from_pct: fn(f64) -> Option<i64> = move_bps_from_pct;
    let _total_sort: fn(Vec<NameMove>) -> Vec<NameMove> = rank_names;
    let probe = NameMove {
        underlying_id: 1,
        segment: ExchangeSegment::NseEquity,
        move_bps: 0,
    };
    let _integer_field: i64 = probe.move_bps;

    // Source: the comparator must use `Ord::cmp`, never `partial_cmp`. A
    // comparator that can return `None`/`NaN` is non-transitive, and
    // `sort_unstable_by` then corrupts the slice WHOLESALE rather than
    // misplacing one entry — the defect the 2026-09-07 lock bans by name.
    let board = production_source("src/depth20_name_board.rs");
    let sort_at = offset_of_only(&board, "names.sort_unstable_by(", 1);
    let window = &board[sort_at..board.len().min(sort_at + 400)];
    assert!(
        !window.contains("partial_cmp"),
        "the name comparator uses partial_cmp — a NaN comparator is \
         non-transitive and corrupts the whole slice"
    );
    assert!(window.contains(".cmp(&a.move_bps)"));
}

// ---------------------------------------------------------------------------
// The bite-proof.
// ---------------------------------------------------------------------------

/// Proves every scan above can actually FAIL.
///
/// This repository has recorded **eleven** vacuous guards — source scans
/// satisfied by the assertion text that names them. A guard is a claim, and an
/// unproven guard is an unproven claim.
#[test]
fn guard_self_test() {
    // (a) The comment stripper must kill a commented-out call site...
    let commented = "    // crate::depth20_name_board::build_name_layout(&c, &m);\n    let y = 1;";
    let stripped: String = commented
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !stripped.contains("build_name_layout("),
        "a commented-out call site must not satisfy the wiring assertions"
    );
    // ...and must NOT take a live call site with a trailing comment.
    let live = "    let p = build_name_layout(&c, &m); // the board";
    let live_stripped = match live.find("//") {
        Some(at) => &live[..at],
        None => live,
    };
    assert!(live_stripped.contains("build_name_layout("));

    // (b) The `#[cfg(test)]` cut must remove a fixture that would otherwise
    //     satisfy a production scan. This is the half that matters most here:
    //     `depth_rebalance.rs`'s own test module names
    //     `cap_depth20_socket_swaps` directly.
    let with_tests = "fn prod() {}\n#[cfg(test)]\nmod tests {\n  cap_depth20_socket_swaps(x);\n}";
    let cut = match with_tests.find("\n#[cfg(test)]") {
        Some(at) => &with_tests[..at],
        None => with_tests,
    };
    assert!(
        !cut.contains("cap_depth20_socket_swaps("),
        "a test-module fixture must not satisfy a production assertion"
    );
    assert!(cut.contains("fn prod()"));

    // (c) `offset_of_only` must REFUSE a second occurrence rather than
    //     silently anchoring on the first.
    let twice = "needle ... needle";
    let caught = std::panic::catch_unwind(|| offset_of_only(twice, "needle", 1));
    assert!(
        caught.is_err(),
        "a positional anchor with two matches must fail, not pick one"
    );
    assert_eq!(offset_of_only("a needle b", "needle", 1), 2);

    // (d) The real files must still satisfy what the fixtures describe — so a
    //     scan that can fail is also a scan that currently passes for the
    //     right reason.
    let loop_body = production_source("src/depth_rebalance.rs");
    assert!(loop_body.contains("crate::depth20_name_board::build_name_layout("));
    assert!(production_source("src/depth20_name_board.rs").contains("names.sort_unstable_by("));

    // (e) And the arithmetic assertions must be sensitive: widening the stock
    //     window by one breaches the budget, which is why ±5 is a ceiling.
    let at_six = 2 * slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
        + DEPTH20_NAME_ENTRY_RANK * slots_for_stock_name(6);
    assert!(at_six > DEPTH20_INSTRUMENT_BUDGET);
}
