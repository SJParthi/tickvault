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
//!
//! Those three were not enough, and this file supplied the proof. On
//! 2026-09-13 an adversarial hunt found the **thirteenth** vacuous guard HERE:
//! the carried-forward test asserted `declared < consumed`, an ordering Rust
//! already guarantees, so it could not fail for the regression its own message
//! described. See [`held_names_outlives_the_loop`] for the corrected property
//! and `guard_self_test` block `(c2)` for the fixture that bite-proves both
//! the withdrawn assertion's vacuity and the replacement's bite. The fourth
//! rule the finding adds: **name the property in code, not only in the failure
//! message** — an assertion whose message describes something stronger than
//! the expression evaluates is a vacuous guard wearing a correct comment.

use std::fs;

use tickvault_app::depth_rebalance::{MoverRow, REBALANCE_INTERVAL_SECS};
use tickvault_app::depth20_layout::DEPTH_20_SOCKETS;
use tickvault_app::depth20_name_board::{
    DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE, DEPTH20_INSTRUMENT_BUDGET, DEPTH20_NAME_ENTRY_RANK,
    DEPTH20_NAME_EXIT_RANK, DEPTH20_NAME_STOCK_SOCKETS, DEPTH20_PER_SOCKET,
    DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE, NameBoardPlan, NameMove, board_slot_cost,
    build_name_layout, future_index, move_bps, move_bps_from_pct, name_moves, rank_names,
    slots_for_index_name, slots_for_stock_name,
};
use tickvault_app::depth20_ranked_steer::{
    DEPTH_SWAP_COMMAND_CHANNEL_DEPTH, DEPTH200_SWAP_COMMAND_CHANNEL_DEPTH,
    MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE,
};
use tickvault_app::dhan_contract_universe::ContractRow;
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

/// [`offset_of_only`] as a `Result`, so a checker built on it can be
/// bite-proven against a fixture without `catch_unwind`.
fn only_offset(haystack: &str, needle: &str) -> Result<usize, String> {
    let found = haystack.matches(needle).count();
    if found != 1 {
        return Err(format!(
            "expected exactly 1 occurrence of {needle:?}, found {found} — a \
             positional assertion against a moved or missing anchor proves nothing"
        ));
    }
    haystack
        .find(needle)
        .ok_or_else(|| format!("{needle:?} not found"))
}

/// The carried-forward check, as a pure function over source text so
/// [`guard_self_test`] can bite-prove it against a fixture in which the
/// declaration has been moved INSIDE the loop.
///
/// # The thirteenth vacuous guard (found and corrected 2026-09-13)
///
/// Until today this property was asserted as `declared < consumed` — the
/// offset of `let mut held_names:` against the offset of `&held_names,`.
/// **That ordering cannot fail for the regression it names.** Rust already
/// forbids use-before-declaration, so `declared < consumed` holds whether the
/// declaration sits above `loop {` or is its first statement. PROVEN: moving
/// the declaration inside the loop in `depth_rebalance.rs` left all nine tests
/// green, `guard_self_test` included.
///
/// The property is `declared < loop_start` — the declaration must precede the
/// `loop {` TOKEN, not merely precede its use. `\n    loop {` is the steering
/// loop's own four-space indentation and occurs exactly once in the scanned
/// production source; the inner `loop {` of the heartbeat spawn is eight-space
/// indented and cannot match, which `only_offset`'s count assertion pins.
fn held_names_outlives_the_loop(body: &str) -> Result<(), String> {
    let declared = only_offset(body, "let mut held_names:")?;
    let loop_start = only_offset(body, "\n    loop {")?;
    let consumed = only_offset(body, "&held_names,")?;
    if declared >= loop_start {
        return Err(format!(
            "`held_names` is declared at byte {declared}, at or after the \
             steering `loop {{` at byte {loop_start} — declared INSIDE the loop \
             it is a fresh empty set every minute, `NameBoard::keeps` is handed \
             nothing, `DEPTH20_NAME_EXIT_RANK` never bites, and the swap \
             counters report churn control the code does not have"
        ));
    }
    if loop_start >= consumed {
        return Err(format!(
            "`&held_names,` at byte {consumed} is not inside the steering loop \
             that starts at byte {loop_start} — the board is not being told \
             what it chose last minute"
        ));
    }
    Ok(())
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
    // `declared < consumed` is NOT this property — Rust forbids
    // use-before-declaration, so that ordering holds on BOTH sides of the
    // regression. The declaration must precede the `loop {` TOKEN.
    if let Err(why) = held_names_outlives_the_loop(&loop_body) {
        panic!("{why}");
    }
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

#[test]
fn the_band_is_cleared_the_moment_the_board_stops_driving() {
    // `held_names` is written from the board.s CHOICE, not from the wire. That
    // is right while the board IS the engine and wrong the instant another one
    // takes over: a fallback minute rewrites every socket from the volume
    // ranking, so a name the board chose before the fallback is no longer
    // subscribed anywhere. Carried forward, the next steerable minute would
    // PREFER those phantom incumbents over names that genuinely out-moved
    // them, and the exit band would be protecting contracts nothing holds.
    //
    // The property is POSITIONAL and it is the whole test: the clear must sit
    // on the FALLBACK arm. A `clear()` moved into the steerable arm compiles,
    // keeps a `contains` assertion green, and destroys the hysteresis outright
    // by emptying the band on the very minutes it exists to govern.
    let body = production_source("src/depth_rebalance.rs");
    let chose = offset_of_only(&body, "held_names = name_plan.chosen_keys();", 1);
    let steerable_arm_ends = offset_of_only(&body, "(planned, \"name_board\")", 1);
    let cleared = offset_of_only(&body, "held_names.clear();", 1);
    let fallback_begins_work = offset_of_only(&body, "match ranking_20.as_deref() {", 1);
    assert!(
        chose < steerable_arm_ends,
        "the carry-forward write must be inside the steerable arm"
    );
    assert!(
        steerable_arm_ends < cleared && cleared < fallback_begins_work,
        "held_names.clear() must sit on the FALLBACK arm — after the steerable \
         arm.s own tail expression and before the fallback picks an engine — \
         or the band is emptied on the minutes it is supposed to govern"
    );
}

#[test]
fn every_planning_minute_reaches_the_name_board_counters() {
    // The board became the PRIMARY depth-20 engine on 2026-09-13 and shipped
    // with no metric of its own, while the ranked counters it displaced freeze
    // the moment it takes over. So the one signal an operator had for depth-20
    // steering went flat exactly when the engine changed — green by absence.
    //
    // Two call sites, not one, and that is the defect this pins. The board is
    // BUILT every minute and only DRIVES on some; the three counters that
    // explain a refusal (`names_unresolved`, `index_unresolved`,
    // `spots_missing`) can therefore only move on a minute it did NOT drive.
    // Recording just the steerable arm would leave them at their seeded zero
    // forever — a refusal metric structurally unable to report a refusal.
    let body = production_source("src/depth_rebalance.rs");
    offset_of_only(&body, "pre_register_name_board_counters();", 1);
    let first = offset_of_only(&body, "record_name_board_plan(", 2);
    let last = body
        .rfind("record_name_board_plan(")
        .expect("counted two occurrences above");
    let steerable_arm_ends = offset_of_only(&body, "(planned, \"name_board\")", 1);
    assert!(
        first < steerable_arm_ends,
        "the steerable minute must record its own plan and swap counts"
    );
    assert!(
        last > steerable_arm_ends,
        "the FALLBACK minute must record too, or the refusal counters can \
         never leave zero"
    );
}

#[test]
fn pre_register_name_board_counters_seeds_every_label_the_recorder_can_emit() {
    // The CloudWatch agent computes a counter as the delta between consecutive
    // samples and DROPS the first sample of a series it has never seen. A label
    // the recorder can emit but the seeder never touches therefore loses its
    // FIRST increment — and for these six that first increment is usually the
    // only one the day ever produces. The series then reads zero on exactly the
    // session it was built to explain.
    //
    // The reverse direction is checked too: a label seeded and never emitted is
    // a permanently-flat series an operator reads as "this never happens".
    let src = production_source("src/depth20_name_board.rs");

    let array = src
        .split_once("DEPTH20_NAME_BOARD_OUTCOME_LABELS: [&str; 6] = [")
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
    assert_eq!(seeded.len(), 6, "extracted the wrong thing from the array");
    assert_eq!(
        emitted.len(),
        6,
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
/// while a name costs twenty-four. This pins the repaired figure at ONE.
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
    // 48 of 60 today. Stated rather than asserted loosely, because the next
    // raise of the cap has to be checked against THIS margin — and because the
    // figure is a CEILING: `SWAP_WIRE_BUDGET` is a timeout, and this
    // repository has recorded three times that a bound is not a measurement.
    assert_eq!(worst_case_secs, 48);
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

    // (c2) THE THIRTEENTH VACUOUS GUARD, bite-proven in both directions.
    //
    //      `held_names` declared as the loop's FIRST STATEMENT is the exact
    //      regression the carried-forward test names: a fresh empty set every
    //      minute, so the exit band never bites. Both fixtures below are
    //      byte-identical apart from where that one line sits.
    let carried = concat!(
        "fn steer() {\n",
        "    let mut held_names: BTreeSet<(u64, u8)> = BTreeSet::new();\n",
        "    loop {\n",
        "        let plan = build(\n",
        "            &held_names,\n",
        "        );\n",
        "        held_names = plan.chosen_keys();\n",
        "    }\n",
        "}\n",
    );
    let relocated = concat!(
        "fn steer() {\n",
        "    loop {\n",
        "        let mut held_names: BTreeSet<(u64, u8)> = BTreeSet::new();\n",
        "        let plan = build(\n",
        "            &held_names,\n",
        "        );\n",
        "        held_names = plan.chosen_keys();\n",
        "    }\n",
        "}\n",
    );
    // The WITHDRAWN assertion — `declared < consumed` — is TRUE of the
    // relocated fixture. That is the whole finding: it could not fail for the
    // regression its own message described, because Rust forbids
    // use-before-declaration whichever side of `loop {` the declaration is on.
    let stale_declared = relocated
        .find("let mut held_names:")
        .unwrap_or_else(|| panic!("fixture must declare held_names"));
    let stale_consumed = relocated
        .find("&held_names,")
        .unwrap_or_else(|| panic!("fixture must consume held_names"));
    assert!(
        stale_declared < stale_consumed,
        "the withdrawn assertion must still hold on the relocated fixture — if \
         it does not, this fixture is not reproducing the 2026-09-13 finding"
    );
    // The replacement MUST reject it.
    assert!(
        held_names_outlives_the_loop(relocated).is_err(),
        "a declaration moved inside the steering loop must FAIL the check — \
         this is the regression the guard exists to catch"
    );
    assert!(
        held_names_outlives_the_loop(carried).is_ok(),
        "the carried-forward shape must PASS — a guard that rejects correct \
         code is abandoned, and an abandoned guard enforces nothing"
    );
    // ...and a missing anchor must fail rather than silently proving nothing.
    let no_loop =
        "fn steer() {\n    let mut held_names: BTreeSet<(u64, u8)> = x;\n    &held_names,\n}\n";
    assert!(
        held_names_outlives_the_loop(no_loop).is_err(),
        "an absent `loop {{` anchor must fail the check, never pass it"
    );

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

    // (f) The per-minute cap scans must bite in BOTH directions.
    //
    //     The minutes helper is the property the operator named, so it has to
    //     be sensitive to the cap rather than constant across it — a helper
    //     that returned 1 for every input would satisfy the assertion above
    //     while proving nothing.
    assert_eq!(minutes_to_rotate_a_name(1), 24);
    assert_eq!(minutes_to_rotate_a_name(12), 2);
    assert_eq!(minutes_to_rotate_a_name(24), 1);
    // A cap of zero must not divide by zero — a guard that panics on an
    // absurd input is a guard someone deletes rather than reads.
    assert_eq!(minutes_to_rotate_a_name(0), 24);

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
