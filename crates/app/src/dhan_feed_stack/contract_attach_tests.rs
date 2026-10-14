//! Contract-attach retry timing tests, moved out of `dhan_feed_stack.rs` on
//! 2026-10-14 (ITEM 57) to keep that file under its size pin; unchanged.

use super::{DEPTH_ATTACH_PREOPEN_RETRY_SECS, DEPTH_ATTACH_RETRY_SECS, preopen_retry_secs};

/// The four boundaries that decide whether the operator's 09:12 line is
/// met. Asserted as a table because an off-by-one at either edge is
/// invisible in production: the loop simply runs at the other cadence and
/// nothing logs which one it chose.
/// The deadline must fall BEFORE the minute the operator names as ready,
/// and after ticks start persisting — a deadline earlier than 09:00 could
/// never be met because no price exists to locate at-the-money against.
#[test]
fn preopen_ready_deadline_sits_between_first_tick_and_the_open() {
    use super::PREOPEN_READY_DEADLINE_IST_SECS as D;
    let persist = tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST;
    assert!(
        D > persist,
        "a deadline before the first tick can never be met"
    );
    assert_eq!(D, 9 * 3_600 + 12 * 60, "the deadline is 09:12:00 IST");
    assert!(
        u64::from(D) < super::CONTINUOUS_SESSION_START_SECS_OF_DAY_IST,
        "readiness must precede the open, not coincide with it"
    );
    // Inside the fast-poll window, or the deadline would be approached on
    // the 60s grid it exists to defeat.
    assert_eq!(
        super::preopen_retry_secs(D),
        super::DEPTH_ATTACH_PREOPEN_RETRY_SECS,
        "the deadline must be approached at the fast cadence"
    );
}

/// THE test for this change. `out_of_time` drives two arms — the quorum
/// wait and the GIVE-UP arm — and only the first wants a 09:12 deadline.
/// Folding it in would abandon contracts and depth at 09:12 on a morning
/// where nothing had resolved, which is the opposite of the requirement.
///
/// A source scan because the loop is `TEST-EXEMPT` async I/O: it dials
/// sockets. This asserts the exact structural property that keeps the two
/// meanings apart, which no unit test of a pure function can reach.
#[test]
fn preopen_ready_deadline_never_reaches_the_give_up_arm() {
    // PRODUCTION half only — `include_str!` carries this test module too,
    // and the first draft counted its own assertion text (5 instead of 2).
    // The house `test_marker` split is what keeps a source scan measuring
    // the code rather than measuring itself.
    let full = include_str!("../dhan_feed_stack.rs");
    let test_marker = concat!("#[cfg(", "test)]");
    let src = full.split(test_marker).next().unwrap_or(full);
    assert!(
        src.contains("let out_of_time = past_hard_stop || past_deadline_and_window;"),
        "out_of_time gained a term — if that term is the 09:12 deadline, the give-up \
         arm now abandons the session at 09:12"
    );
    assert!(
        src.contains(
            "let past_preopen_ready_deadline = now_ist >= PREOPEN_READY_DEADLINE_IST_SECS;"
        ),
        "the readiness deadline must stay its own flag"
    );
    // And it must be spent on the quorum wait, nowhere else.
    assert_eq!(
        src.matches("past_preopen_ready_deadline").count(),
        2,
        "the readiness flag is declared once and read once (the quorum wait). A third \
         use means it has reached an arm that was never reviewed for it"
    );
    assert!(
        src.contains("&& !past_preopen_ready_deadline;"),
        "the quorum wait must actually consult the deadline"
    );
}

/// A mid-session restart is not a late pre-open, and must not be reported
/// as one.
///
/// MEASURED 2026-08-25: the morning attach completed at 09:08:04 and MET
/// the 09:12 deadline, and five afternoon restarts each fired `WS-GAP-02`
/// and drove `tv-<env>-preopen-ready-late` into ALARM. The alarm is
/// ungated by design, so it stayed armed. Every one of those pages was
/// reporting a redeploy.
#[test]
fn the_preopen_deadline_does_not_apply_to_a_mid_session_restart() {
    // The real morning: the loop began well before the deadline.
    assert!(
        super::preopen_deadline_applies(8 * 3_600 + 35 * 60),
        "an attach that begins at 08:35 is racing the open and IS judged"
    );
    // The five real restarts from that afternoon, by their start second.
    for (label, started) in [
        ("12:37", 12 * 3_600 + 37 * 60),
        ("16:17", 16 * 3_600 + 17 * 60),
        ("17:33", 17 * 3_600 + 33 * 60),
        ("18:17", 18 * 3_600 + 17 * 60),
        ("19:21", 19 * 3_600 + 21 * 60),
    ] {
        assert!(
            !super::preopen_deadline_applies(started),
            "the {label} restart began after 09:12 and cannot be judged against it"
        );
    }
}

/// Keyed on the START, never the finish.
///
/// An attach that begins at 09:11 and drags past 09:12 is the exact case
/// the deadline exists for; excusing it would hollow the alarm out from
/// the other side while fixing the false pages.
#[test]
fn an_attach_that_begins_before_the_deadline_is_still_judged_if_it_overruns() {
    let d = super::PREOPEN_READY_DEADLINE_IST_SECS;
    assert!(
        super::preopen_deadline_applies(d - 1),
        "one second before the deadline still counts as racing it"
    );
    assert!(
        !super::preopen_deadline_applies(d),
        "an attach that begins AT the deadline has already lost the race it would be \
         judged on — it is a restart, not a late pre-open"
    );
    assert!(!super::preopen_deadline_applies(d + 1));
}

/// The alarm-side half of the fix, pinned in source.
///
/// The CloudWatch metric filter is `{ $.fields.ready_at_ist_secs = * }`.
/// The mid-session arm must therefore NOT carry that field name, or the
/// datapoint still lands and the gate changes nothing that the operator
/// can see. Bite-proven: renaming `attached_at_ist_secs` back to
/// `ready_at_ist_secs` fails this test.
#[test]
fn the_mid_session_arm_does_not_emit_the_field_the_alarm_filters_on() {
    let full = include_str!("../dhan_feed_stack.rs");
    let test_marker = concat!("#[cfg(", "test)]");
    let src = full.split(test_marker).next().unwrap_or(full);

    let gate = src
        .find("if !preopen_deadline_applies(attach_started_ist) {")
        .expect("the mid-session gate must exist");
    let els = src[gate..]
        .find("} else {")
        .expect("the judged arm must be the else of that gate")
        + gate;
    // Comment lines are stripped before asserting: this arm's own comment
    // QUOTES the CloudWatch filter pattern verbatim, so a raw substring
    // scan matches the documentation rather than the code. Caught by this
    // test failing on its first run, which is the right way round.
    let mid_arm: String = src[gate..els]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let mid_arm = mid_arm.as_str();

    assert!(
        mid_arm.contains("attached_at_ist_secs = ready_at"),
        "the mid-session arm must report its completion second under a DIFFERENT field"
    );
    assert!(
        !mid_arm.contains("ready_at_ist_secs ="),
        "the mid-session arm must not emit `ready_at_ist_secs` — that field is what the \
         CloudWatch filter matches, so emitting it re-creates the false page"
    );
    assert!(!mid_arm.contains("error!"), "a restart is not an error");
    assert!(
        !mid_arm.contains("metrics::gauge!(PREOPEN_READY_GAUGE)"),
        "there is no readiness second on a restart; publishing the wall clock as one \
         would read as a permanently missed deadline"
    );
}

/// The readiness second is published once, at the only point where both
/// halves are provably on the wire — never on a give-up path, where there
/// is no readiness second and a placeholder would read as a met deadline.
#[test]
fn preopen_ready_gauge_is_published_only_on_the_both_halves_success_arm() {
    let full = include_str!("../dhan_feed_stack.rs");
    let test_marker = concat!("#[cfg(", "test)]");
    let src = full.split(test_marker).next().unwrap_or(full);
    assert_eq!(
        src.matches("metrics::gauge!(PREOPEN_READY_GAUGE)").count(),
        1,
        "the readiness gauge must have exactly one emit site"
    );
    let emit = src
        .find("metrics::gauge!(PREOPEN_READY_GAUGE)")
        .expect("emit site");
    // The arm gained a `!readiness_published` latch on 2026-08-22: the
    // loop can now outlive the both-halves moment to chase stocks that
    // priced after the contract dial, so the `return` no longer guarantees
    // one publication per session. The latch does. Anchoring on the FULL
    // condition is deliberate — anchoring on the prefix would silently
    // match the terminal return block further down and invert this
    // assertion, which is exactly how this test first failed.
    // RE-ANCHORED 2026-09-01: `depth_done` → `depth_ready`. The gate did
    // not move; what `depth_done` MEANS did. It now means the COMPLETE
    // depth set, and the fifth depth-200 socket is the day's biggest mover
    // — which needs traded prices and cannot exist before 09:15, so gating
    // the 09:12 pre-open verdict on it would miss that deadline every
    // trading morning by construction. `depth_ready` is the first
    // successful depth plan, i.e. exactly what `depth_done` used to mean
    // here, so this arm fires on the same iteration it always did.
    let arm = src
        .find("if contracts_done && depth_ready && !readiness_published {")
        .expect("success arm");
    assert!(emit > arm, "the gauge is published outside the success arm");
    assert_eq!(
        src.matches("readiness_published = true;").count(),
        1,
        "one latch set, inside that arm — a second would let a lingering loop republish \
         the readiness second and overwrite a met deadline with a later one"
    );
    // Measured AFTER the dials, not from the top of the iteration — the
    // dials take real time and the earlier reading would flatter it.
    assert!(
        src.contains("let ready_at = ist_second_of_day_now();"),
        "the readiness second must be read fresh at completion"
    );
}

#[test]
fn preopen_retry_secs_switches_exactly_at_the_persistence_and_session_edges() {
    const NINE: u32 = 9 * 3_600;
    for (secs, want, why) in [
        (
            NINE - 1,
            DEPTH_ATTACH_RETRY_SECS,
            "08:59:59 — before ticks persist",
        ),
        (
            NINE,
            DEPTH_ATTACH_PREOPEN_RETRY_SECS,
            "09:00:00 — first tick can persist",
        ),
        (
            NINE + 8 * 60,
            DEPTH_ATTACH_PREOPEN_RETRY_SECS,
            "09:08 — Friday's real attach",
        ),
        (
            NINE + 12 * 60,
            DEPTH_ATTACH_PREOPEN_RETRY_SECS,
            "09:12 — the operator's line",
        ),
        (
            NINE + 15 * 60 - 1,
            DEPTH_ATTACH_PREOPEN_RETRY_SECS,
            "09:14:59 — still pre-open",
        ),
        (
            NINE + 15 * 60,
            DEPTH_ATTACH_RETRY_SECS,
            "09:15:00 — open; nothing races now",
        ),
        (
            14 * 3_600,
            DEPTH_ATTACH_RETRY_SECS,
            "14:00 — mid-session restart",
        ),
        (
            0,
            DEPTH_ATTACH_RETRY_SECS,
            "midnight — no fast poll overnight",
        ),
    ] {
        assert_eq!(preopen_retry_secs(secs), want, "{why}");
    }
}

/// The fast cadence must be strictly faster, and bounded away from zero —
/// a 0 would spin the loop against QuestDB with no sleep at all.
#[test]
fn preopen_retry_secs_is_faster_than_the_session_cadence_and_never_zero() {
    assert!(
        DEPTH_ATTACH_PREOPEN_RETRY_SECS > 0,
        "a zero sleep is a spin loop"
    );
    assert!(
        DEPTH_ATTACH_PREOPEN_RETRY_SECS < DEPTH_ATTACH_RETRY_SECS,
        "the pre-open cadence must be the faster one, or the window buys nothing"
    );
    // 15 minutes of window / 5s = 180 reads. Pinned so a later tightening
    // has to face the read count it creates.
    let reads = (15 * 60) / DEPTH_ATTACH_PREOPEN_RETRY_SECS;
    assert!(
        reads <= 200,
        "pre-open window would issue {reads} QuestDB reads"
    );
}

use super::*;

#[test]
fn remaining_capacity_is_counted_in_whole_connections() {
    // The spot universe is ~4,565 instruments — one connection carrying
    // 4,565 of its 5,000. The 435 unused slots on that socket are
    // unreachable to a second planning pass, so the contract set gets 4
    // connections, not 20,435 instruments.
    assert_eq!(remaining_main_feed_capacity(1), 20_000);
    assert_eq!(remaining_main_feed_capacity(0), 25_000);
    assert_eq!(remaining_main_feed_capacity(4), 5_000);
}

#[test]
fn a_full_pool_leaves_no_room_rather_than_overflowing_it() {
    // Every connection spent means NO contracts. Returning anything
    // non-zero here would size a set the pool cannot dial, and `plan_pool`
    // refuses the WHOLE pool — costing the session its depth as well.
    assert_eq!(remaining_main_feed_capacity(5), 0);
    assert_eq!(
        remaining_main_feed_capacity(9),
        0,
        "saturating, never wraps"
    );
}

#[test]
fn connection_count_mirrors_the_main_feed_pack_policy() {
    // History, because this line has now been wrong in BOTH directions and
    // the reason is the same each time: it must mirror `plan_pool`'s
    // main-feed arm, and nothing else.
    //
    //  - originally PACKED while plan_pool SPREAD  -> understated by 4,
    //    the attach asked for room that did not exist, the pool refused
    //    everything and depth was never planned;
    //  - briefly SPREAD to match                   -> honest, but left
    //    literally zero room for contracts;
    //  - now PACKED, with plan_pool packing too    -> both passes fit.
    let max = usize::from(tickvault_core::websocket::pool_budget::MAX_MAIN_FEED_CONNECTIONS);
    let cap = usize::try_from(
        tickvault_core::websocket::pool_budget::MAIN_FEED_INSTRUMENTS_PER_CONNECTION,
    )
    .unwrap_or(usize::MAX);

    assert_eq!(
        main_feed_connections_for(0, max),
        0,
        "nothing occupies nothing"
    );
    assert_eq!(main_feed_connections_for(1, max), 1);
    assert_eq!(main_feed_connections_for(3, max), 1, "three fit one socket");
    assert_eq!(
        main_feed_connections_for(cap, max),
        1,
        "exactly full is still one"
    );
    assert_eq!(
        main_feed_connections_for(cap + 1, max),
        2,
        "one over spills"
    );
    assert_eq!(
        main_feed_connections_for(4_565, max),
        1,
        "today's spot universe fits one socket — leaving four for contracts"
    );
    assert_eq!(main_feed_connections_for(25_000, max), max, "target scale");

    // The clamp. Reporting more than the pool has free is what earns a
    // BudgetRefused on the WHOLE pool.
    assert_eq!(main_feed_connections_for(25_000, 2), 2);
    assert_eq!(main_feed_connections_for(25_000, 0), 0);
}

#[test]
fn the_two_helpers_can_never_together_exceed_the_authorized_pool() {
    // The 5-connection main-feed lock has to be arithmetic, not a hope:
    // for every reachable spot-universe size, the connections the spots
    // occupy plus the connections the remaining capacity could need must
    // stay inside the cap.
    let max = usize::from(tickvault_core::websocket::pool_budget::MAX_MAIN_FEED_CONNECTIONS);
    for spot in [0usize, 1, 3, 4_565, 5_000, 5_001, 12_000, 24_999, 25_000] {
        let used = main_feed_connections_for(spot, max);
        let remaining = remaining_main_feed_capacity(used);
        // Contracts may only occupy what the spots LEFT.
        let would_need = main_feed_connections_for(remaining, max.saturating_sub(used));
        assert!(
            used + would_need <= max,
            "spot={spot} used={used} + contracts={would_need} exceeds {max}"
        );
    }

    // And the property the whole fix exists for, stated directly: after
    // today's spot universe, contracts still fit — and together they fill
    // every authorized main-feed socket. Under the old arithmetic this
    // line read 0 connections for contracts and a refused pool.
    let used = main_feed_connections_for(4_565, max);
    assert_eq!(used, 1);
    let room = remaining_main_feed_capacity(used);
    assert_eq!(room, 20_000, "four connections x 5,000 remain");
    assert_eq!(
        used + main_feed_connections_for(room, max.saturating_sub(used)),
        max,
        "spots + contracts together fill all five main-feed sockets"
    );
}

#[test]
fn ymd_from_ist_date_refuses_a_malformed_date_rather_than_selecting_an_expired_one() {
    assert_eq!(ymd_from_ist_date("2026-08-19"), 2026_08_19);
    // Every refusal returns 0, and 0 makes every expiry compare as "before
    // today" — so the fail-closed direction is "no contracts", never "an
    // expired one", whose silence looks exactly like a quiet book.
    assert_eq!(ymd_from_ist_date(""), 0);
    assert_eq!(ymd_from_ist_date("19-08-2026"), 0);
    assert_eq!(ymd_from_ist_date("2026-13-01"), 0);
    assert_eq!(ymd_from_ist_date("2026-08-32"), 0);
    assert_eq!(ymd_from_ist_date("not-a-date"), 0);
}

#[test]
fn the_date_packing_matches_the_one_expiries_are_stored_in() {
    // Both sides of the expiry comparison must pack identically or the
    // whole >= today filter is meaningless. Same input, same answer.
    for d in ["2026-08-19", "2026-01-01", "2026-12-31"] {
        assert_eq!(
            ymd_from_ist_date(d),
            tickvault_core::instrument::master_csv::parse_expiry_ymd(d),
            "the attach and the parser must agree on {d}"
        );
    }
}
