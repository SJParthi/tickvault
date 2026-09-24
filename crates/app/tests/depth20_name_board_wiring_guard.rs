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
//! | depth-20 is NOT steered — no name board, no ranked list, no per-minute swap (inverted 2026-09-24) | a swap whose unsubscribe Dhan ignores comes back, and the "static all day" set churns |
//! | the attach dials the static day set exactly once | the replacement engine is dormant and depth-20 opens nothing |
//! | the slot arithmetic still sums to 230 ≤ 250 | `plan_pool` refuses the WHOLE pool fail-closed: a session with no depth at all |
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
//!
//! Those three were not enough, and this file supplied the proof. On
//! 2026-09-13 an adversarial hunt found the **thirteenth** vacuous guard HERE:
//! the carried-forward test asserted `declared < consumed`, an ordering Rust
//! already guarantees, so it could not fail for the regression its own message
//! described. The corrected check (`held_names_outlives_the_loop`) was
//! RETIRED on 2026-09-24 together with the steering it guarded. The fourth
//! rule the finding adds still binds: **name the property in code, not only in
//! the failure message** — an assertion whose message describes something
//! stronger than the expression evaluates is a vacuous guard wearing a correct
//! comment.
//!
//! # 2026-09-24 — the wiring half is INVERTED
//!
//! The operator retired per-minute depth-20 steering: depth-20 is now a STATIC
//! day set (every F&O spot from 09:00, NIFTY and BANKNIFTY ATM ±k once the
//! pre-open price is final, no swap and no unsubscribe all day). The board's
//! arithmetic, ranking and comparator tests below stay — the module is
//! retained — but the "reached every minute" pins are replaced by the opposite
//! property: nothing in the steering loop steers depth-20 any more, and the
//! attach dials `depth20_static::build_static_depth20` exactly once.

use std::fs;

use tickvault_app::depth_rebalance::{MoverRow, REBALANCE_INTERVAL_SECS};
use tickvault_app::depth20_layout::DEPTH_20_SOCKETS;
use tickvault_app::depth20_name_board::{
    DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE, DEPTH20_INSTRUMENT_BUDGET, DEPTH20_NAME_ENTRY_RANK,
    DEPTH20_NAME_EXIT_RANK, DEPTH20_NAME_STOCK_SOCKETS, DEPTH20_PER_SOCKET,
    DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE, NameBoardPlan, NameMove, board_slot_cost,
    build_name_layout, move_bps, move_bps_from_pct, name_moves, rank_names, slots_for_index_name,
    slots_for_stock_name,
};
use tickvault_app::depth20_ranked_steer::{
    DEPTH_SWAP_COMMAND_CHANNEL_DEPTH, DEPTH200_SWAP_COMMAND_CHANNEL_DEPTH,
    MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE,
};
use tickvault_app::dhan_depth_universe::DepthCandidate;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SWAP_WIRE_BUDGET;

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

/// Every call that would STEER depth-20 once a minute.
///
/// Since 2026-09-24 depth-20 is a STATIC day set (`websocket-connection-scope-lock.md`
/// "2026-09-24 — DEPTH-20 IS A STATIC DAY SET"): every F&O spot from the 09:00
/// attach, NIFTY and BANKNIFTY at-the-money ±k added once the pre-open price is
/// final, and no swap and no unsubscribe for the rest of the session. The scope
/// lock's REJECT list names "re-adds any per-minute depth-20 steering, swap, or
/// unsubscribe" — these are the calls that would do it.
const DEPTH20_STEERING_CALLS: [&str; 6] = [
    "build_name_layout(",
    "plan_depth20_ranked_minute(",
    "plan_depth20_minute(",
    "build_depth20_layout(",
    "cap_depth20_socket_swaps(",
    "held_names",
];

/// The steering calls present in `body`, as a pure function so
/// [`guard_self_test`] can bite-prove the scan against a fixture that DOES
/// steer.
fn depth20_steering_calls_in(body: &str) -> Vec<&'static str> {
    DEPTH20_STEERING_CALLS
        .iter()
        .copied()
        .filter(|call| body.contains(call))
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Depth-20 is a STATIC day set. The name board is no longer reached.
// ---------------------------------------------------------------------------

#[test]
fn the_steering_loop_no_longer_steers_depth20() {
    // The 2026-09-13 guard pinned the OPPOSITE: that the board was reached every
    // minute. The operator's 2026-09-24 directive retired per-minute depth-20
    // steering outright ("our depth 20 entire day shdou leb completely fixed
    // static"), so what must now never come back is the steering itself.
    let loop_body = production_source("src/depth_rebalance.rs");
    let present = depth20_steering_calls_in(&loop_body);
    assert!(
        present.is_empty(),
        "depth-20 is a static day set, but the steering loop still calls {present:?} \
         — a per-minute depth-20 swap is a REJECT under the 2026-09-24 scope lock"
    );
    // Nor may the loop keep seeding the retired board's counters: a series
    // seeded every boot and never incremented reads as "this never happens".
    assert!(
        !loop_body.contains("pre_register_name_board_counters("),
        "the name board no longer drives anything; seeding its counters \
         publishes a permanently-flat series"
    );
}

#[test]
fn the_attach_dials_the_static_day_set() {
    // The replacement engine must be REACHED, exactly once, from the attach.
    let stack = production_source("src/dhan_feed_stack.rs");
    offset_of_only(&stack, "crate::depth20_static::build_static_depth20(", 1);
    // ...and the attach must not fall back to a steered layout either.
    let present = depth20_steering_calls_in(&stack);
    assert!(
        present.is_empty(),
        "the attach still calls {present:?} — the static day set is the only \
         depth-20 engine"
    );
}
#[test]
fn pre_register_name_board_counters_seeds_every_label_the_recorder_can_emit() {
    // The CloudWatch agent computes a counter as the delta between consecutive
    // samples and DROPS the first sample of a series it has never seen. A label
    // the recorder can emit but the seeder never touches therefore loses its
    // FIRST increment — and for these five that first increment is usually the
    // only one the day ever produces. The series then reads zero on exactly the
    // session it was built to explain.
    //
    // The reverse direction is checked too: a label seeded and never emitted is
    // a permanently-flat series an operator reads as "this never happens".
    let src = production_source("src/depth20_name_board.rs");

    let array = src
        .split_once("DEPTH20_NAME_BOARD_OUTCOME_LABELS: [&str; 5] = [")
        .and_then(|(_, rest)| rest.split_once("];"))
        .map(|(inside, _)| inside.to_owned())
        .expect("the label array must be a literal the seeder can loop over");
    let mut seeded: Vec<String> = array
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect();

    let recorder = src
        .split_once("pub fn record_name_board_plan")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body.to_owned())
        .expect("the recorder must be a free function in production source");
    let mut emitted: Vec<String> = recorder
        .split("\"outcome\" => \"")
        .skip(1)
        .filter_map(|tail| tail.split_once('"').map(|(label, _)| label.to_owned()))
        .collect();

    // Anti-vacuity: two empty vectors compare equal, so an extractor that
    // silently matched nothing would pass this test against ANY source.
    assert_eq!(seeded.len(), 5, "extracted the wrong thing from the array");
    assert_eq!(
        emitted.len(),
        5,
        "extracted the wrong thing from the recorder body"
    );

    seeded.sort();
    emitted.sort();
    assert_eq!(
        seeded, emitted,
        "the seeder and the recorder disagree about the label set: a label only \
         the recorder knows loses its first sample to the agent's delta rule, and \
         a label only the seeder knows is a flat series that reads as never-happens"
    );

    // And the seeder must loop the array rather than spell the six out again —
    // a second hand-written list is a third place for them to drift.
    assert!(
        src.contains("for outcome in DEPTH20_NAME_BOARD_OUTCOME_LABELS"),
        "the seeding loop must read the same array this test just compared"
    );
}

// ---------------------------------------------------------------------------
// 2. The slot arithmetic.
// ---------------------------------------------------------------------------

#[test]
fn the_board_still_sums_to_230_of_250() {
    // The operator asked directly: "will it sit under 250 slots". These are
    // the numbers, re-derived rather than quoted, so a change to either window
    // moves this test with it instead of leaving a stale literal.
    assert_eq!(
        slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE),
        46
    );
    assert_eq!(
        slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE),
        23
    );
    assert_eq!(
        board_slot_cost(),
        2 * slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
            + DEPTH20_NAME_ENTRY_RANK * slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE)
    );
    assert_eq!(board_slot_cost(), 230);
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

/// Two index chains, eight stock chains, and the movers that rank them.
///
/// No `ContractRow` futures since 2026-09-18: `build_name_layout` no longer
/// takes a futures index, so a fixture carrying them could not feed one in and
/// would prove nothing about their absence.
fn fixture() -> (Vec<DepthCandidate>, Vec<MoverRow>) {
    let mut candidates = chain("NIFTY", 24_000.0, 50.0, 40, 100_000);
    candidates.extend(chain("BANKNIFTY", 52_000.0, 100.0, 40, 200_000));
    let mut movers = Vec::new();
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
    }
    (candidates, movers)
}

fn plan_with(movers: &[MoverRow], held: &std::collections::BTreeSet<(u64, u8)>) -> NameBoardPlan {
    let (candidates, _) = fixture();
    build_name_layout(&candidates, movers, held)
}

#[test]
fn no_mover_can_displace_nifty_or_banknifty() {
    let (_, mut movers) = fixture();
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

/// How many steering minutes a whole-name rotation takes at a given cap.
///
/// A free function rather than an inline expression because the property the
/// operator asked for — "per minute" — is a NUMBER OF MINUTES, and an
/// assertion that compares two constants describes something weaker than the
/// message it carries. This is the fourth rule this file's header records:
/// name the property in code, not only in the failure message.
fn minutes_to_rotate_a_name(cap: usize) -> usize {
    let cost = slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE);
    cost.div_ceil(cap.max(1))
}

/// The operator's requirement expressed as the quantity he named.
///
/// The board has recomputed and re-planned every minute since it shipped; what
/// was not per-minute was the APPLICATION, throttled to four swaps a socket
/// while a name costs twenty-three. This pins the repaired figure at ONE.
#[test]
fn a_name_rotation_takes_exactly_one_steering_minute() {
    assert_eq!(
        minutes_to_rotate_a_name(MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE),
        1,
        "depth-20 is a per-MINUTE board: a cap below one name's swap cost \
         leaves the wire minutes behind a board that already chose correctly"
    );
    // The regression, named so the assertion above cannot be read as trivially
    // true of any cap.
    assert_eq!(
        minutes_to_rotate_a_name(4),
        6,
        "the pre-2026-09-13 cap took six minutes to rotate one name"
    );
}

/// The cap must DRAIN inside the interval that planned it.
///
/// A cap whose worst case outlives its own minute is a backlog with a number
/// on it: the next iteration's `try_send` meets a queue that never emptied,
/// and the pool falls permanently further behind. The const-assert in
/// `depth20_ranked_steer` fails the build on this; the test states the
/// arithmetic in the open so the margin is a number somebody can read.
#[test]
fn the_per_socket_plan_drains_inside_one_steering_interval() {
    let worst_case_secs =
        (MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE as u64) * 2 * SWAP_WIRE_BUDGET.as_secs();
    assert!(
        worst_case_secs < REBALANCE_INTERVAL_SECS,
        "a per-socket plan of {MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE} \
         swaps is {worst_case_secs}s at the wire ceiling, which does not fit a \
         {REBALANCE_INTERVAL_SECS}s steering interval"
    );
    // 46 of 60 today, 48 before the 2026-09-18 futures removal took one slot
    // off the cap. Stated rather than asserted loosely, because the next
    // raise of the cap has to be checked against THIS margin — and because the
    // figure is a CEILING: `SWAP_WIRE_BUDGET` is a timeout, and this
    // repository has recorded three times that a bound is not a measurement.
    assert_eq!(worst_case_secs, 46);
    assert_eq!(REBALANCE_INTERVAL_SECS, 60);
}

/// Depth-200 keeps the shallower queue, and the frame stack actually SPLITS.
///
/// The two pools shared one constant until 2026-09-13. Raising it for
/// depth-20's name rotation would have carried depth-200 with it — a pool that
/// swaps one instrument per socket per minute, whose only use for the depth is
/// the wedge signal, which a six-times-deeper queue delays six times longer.
#[test]
fn depth200_keeps_its_shallow_queue_and_the_stack_picks_by_endpoint() {
    assert!(
        DEPTH200_SWAP_COMMAND_CHANNEL_DEPTH < DEPTH_SWAP_COMMAND_CHANNEL_DEPTH,
        "depth-200's queue exists to surface a wedge, not to hold a plan"
    );
    let stack = production_source("src/dhan_feed_stack.rs");
    offset_of_only(
        &stack,
        "crate::depth20_ranked_steer::DEPTH200_SWAP_COMMAND_CHANNEL_DEPTH",
        1,
    );
    let by_endpoint = offset_of_only(
        &stack,
        "if matches!(endpoint, DhanEndpointType::Depth20) {",
        1,
    );
    let channel = offset_of_only(&stack, "tokio::sync::mpsc::channel(channel_depth)", 1);
    assert!(
        by_endpoint < channel,
        "the endpoint test must CHOOSE the depth before the channel is built, \
         or both pools get whichever constant was evaluated"
    );
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

    // (c2) The "not steered" scan must bite in BOTH directions.
    //
    //      A loop that steers depth-20 once a minute is exactly the regression
    //      the 2026-09-24 scope lock forbids, so a fixture that does it must be
    //      caught, and one that does not must pass. A scan that could only
    //      return empty would satisfy the real-file test while enforcing
    //      nothing.
    let steered = concat!(
        "fn steer() {\n",
        "    let mut held_names = BTreeSet::new();\n",
        "    loop {\n",
        "        let plan = crate::depth20_name_board::build_name_layout(&c, &m, &held_names);\n",
        "        cap_depth20_socket_swaps(&mut plan, 24);\n",
        "    }\n",
        "}\n",
    );
    let caught_calls = depth20_steering_calls_in(steered);
    assert!(caught_calls.contains(&"build_name_layout("));
    assert!(caught_calls.contains(&"cap_depth20_socket_swaps("));
    assert!(caught_calls.contains(&"held_names"));
    let static_only = "fn attach() {\n    let set = build_static_depth20(&spots, &legs);\n}\n";
    assert!(
        depth20_steering_calls_in(static_only).is_empty(),
        "the static day set must not read as steering — a guard that rejects \
         correct code is abandoned, and an abandoned guard enforces nothing"
    );

    // (d) The real files must still satisfy what the fixtures describe — so a
    //     scan that can fail is also a scan that currently passes for the
    //     right reason.
    let loop_body = production_source("src/depth_rebalance.rs");
    assert!(depth20_steering_calls_in(&loop_body).is_empty());
    assert!(
        production_source("src/dhan_feed_stack.rs")
            .contains("crate::depth20_static::build_static_depth20(")
    );
    assert!(production_source("src/depth20_name_board.rs").contains("names.sort_unstable_by("));

    // (e) And the arithmetic assertions must be sensitive: widening the stock
    //     window by one breaches the budget, which is why ±5 is a ceiling.
    let at_six = 2 * slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
        + DEPTH20_NAME_ENTRY_RANK * slots_for_stock_name(6);
    assert!(at_six > DEPTH20_INSTRUMENT_BUDGET);

    // (f) The per-minute cap scans must bite in BOTH directions.
    //
    //     The minutes helper is the property the operator named, so it has to
    //     be sensitive to the cap rather than constant across it — a helper
    //     that returned 1 for every input would satisfy the assertion above
    //     while proving nothing.
    assert_eq!(minutes_to_rotate_a_name(1), 23);
    assert_eq!(minutes_to_rotate_a_name(12), 2);
    assert_eq!(minutes_to_rotate_a_name(24), 1);
    // A cap of zero must not divide by zero — a guard that panics on an
    // absurd input is a guard someone deletes rather than reads.
    assert_eq!(minutes_to_rotate_a_name(0), 23);

    // ...and the endpoint-split scan must fail on a stack that shares one
    //    constant between the pools, which is exactly the pre-2026-09-13 shape.
    let shared = "let (tx, rx) = tokio::sync::mpsc::channel(\n    crate::depth20_ranked_steer::DEPTH_SWAP_COMMAND_CHANNEL_DEPTH,\n);\n";
    assert_eq!(
        shared
            .matches("tokio::sync::mpsc::channel(channel_depth)")
            .count(),
        0,
        "the pre-split shape must NOT satisfy the endpoint-split scan"
    );
    // The real file must satisfy it, so a scan that can fail also currently
    // passes for the right reason.
    let stack = production_source("src/dhan_feed_stack.rs");
    assert_eq!(
        stack
            .matches("tokio::sync::mpsc::channel(channel_depth)")
            .count(),
        1
    );
}
