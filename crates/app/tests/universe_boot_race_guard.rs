//! Guard: the live lane must WAIT for today's mapping artifact before it
//! resolves its subscription set.
//!
//! # The production incident this pins
//!
//! The daily `[dhan_universe]` rider WRITES `dhan-nse-mapping-<today>.json`;
//! the live lane READS it. Both start from the same boot with no ordering
//! between them, and the filename is date-stamped so yesterday's file cannot
//! stand in. Measured on the prod box:
//!
//! | Time (IST) | Event |
//! |---|---|
//! | 2026-08-18 08:11:48 | lane reads the artifact — `NotFound` — falls back to **4 instruments** |
//! | 2026-08-18 08:11:57 | rider writes it: `resolved: 4567` — **9 seconds too late** |
//! | 2026-08-18 08:31:29 | box happens to boot again; now the file exists → **4,565 subscribed** |
//!
//! On 2026-08-17 there was only ONE boot (08:31:37). It lost the race, and the
//! **entire trading session ran on 4 instruments instead of 4,565** — silently,
//! because `tv_dhan_live_universe_fallback_total` has no alarm attached.
//!
//! So the failure is not intermittent: a box that boots once — the normal case
//! — loses every single day. Today's green reading was luck, not design.
//!
//! These tests fail the build if the wait is removed, if it stops being
//! awaited, or if it drifts back below the resolve call.

const MAIN_RS: &str = include_str!("../src/main.rs");
const LIVE_UNIVERSE_RS: &str = include_str!("../src/dhan_live_universe.rs");

/// Strip `//` line comments so a prose mention of a symbol cannot satisfy a
/// scan that is meant to find real code.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn boot_awaits_the_mapping_artifact_before_resolving_the_universe() {
    let code = code_only(MAIN_RS);

    let wait_at = code.find("await_mapping_artifact").unwrap_or_else(|| {
        panic!(
            "crates/app/src/main.rs no longer calls `await_mapping_artifact`. Without it the \
             lane races the daily rider and collapses to 4 instruments on every single-boot \
             day — the 2026-08-17 incident in this file's header."
        )
    });

    let resolve_at = code.find("resolve_live_universe").unwrap_or_else(|| {
        panic!("crates/app/src/main.rs no longer calls `resolve_live_universe`")
    });

    assert!(
        wait_at < resolve_at,
        "`await_mapping_artifact` must run BEFORE `resolve_live_universe` in main.rs. \
         Resolving first is precisely the race: the resolve reads a file the rider has not \
         written yet."
    );
}

#[test]
fn the_wait_is_actually_awaited() {
    let code = code_only(MAIN_RS);
    let idx = code
        .find("await_mapping_artifact")
        .expect("call site must exist — covered by the test above");
    let tail = &code[idx..];
    // The call spans several lines (formatted args), so look at a generous
    // window rather than the single line.
    let window: String = tail.chars().take(400).collect();
    assert!(
        window.contains(".await"),
        "`await_mapping_artifact` is called but never awaited — the future would be dropped \
         and the wait would silently do nothing, which reads exactly like the bug it fixes. \
         Window inspected:\n{window}"
    );
}

#[test]
fn the_wait_is_bounded_and_fail_soft() {
    let src = LIVE_UNIVERSE_RS;

    assert!(
        src.contains("MAPPING_WAIT_DEADLINE_SECS"),
        "the wait must carry an explicit deadline constant — an unbounded wait would hang \
         boot behind a vendor outage, which is strictly worse than the fallback it avoids"
    );

    let code = code_only(src);
    assert!(
        code.contains("started.elapsed() < deadline"),
        "the poll loop must be deadline-bounded"
    );

    // Fail-soft: the function returns rather than panicking or looping forever.
    assert!(
        !code.contains("panic!") && !code.contains("unwrap()"),
        "the boot wait must never panic — a failed wait has to degrade to the index universe, \
         not abort the process"
    );
}

#[test]
fn every_wait_outcome_is_counted() {
    let src = LIVE_UNIVERSE_RS;
    assert!(
        src.contains("MAPPING_WAIT_COUNTER"),
        "the wait must publish a counter; without it a boot that timed out is \
         indistinguishable from one that never needed to wait"
    );
    for outcome in [
        "not_requested",
        "rider_disabled",
        "already_present",
        "became_ready",
        "timed_out",
        "pre_open_cutoff",
    ] {
        assert!(
            src.contains(outcome),
            "wait outcome `{outcome}` is not emitted — an unlabelled exit is an unobservable one"
        );
    }
}

#[test]
fn a_disabled_rider_does_not_burn_the_whole_deadline() {
    let code = code_only(LIVE_UNIVERSE_RS);
    let disabled_at = code
        .find("if !cfg.enabled")
        .expect("the wait must short-circuit when the rider that writes the artifact is off");
    let loop_at = code
        .find("while started.elapsed()")
        .expect("the poll loop must exist");
    assert!(
        disabled_at < loop_at,
        "the `[dhan_universe] enabled = false` check must come BEFORE the poll loop. With no \
         writer, waiting the full deadline reaches the same fallback having wasted the time."
    );
}

const DHAN_UNIVERSE_RS: &str = include_str!("../src/dhan_universe.rs");

/// The lane blocks at boot on `write_mapping_atomic`. Anything slow placed
/// ahead of it spends the lane's budget on work nothing is waiting for.
///
/// # The regression this pins (2026-08-21)
///
/// `write_dhan_lifecycle` — a ~150,000-row QuestDB round trip — landed in
/// #1773 on 2026-08-20 and was inserted BEFORE the artifact write. The lane's
/// 120 s budget had been derived on 2026-08-18 from a 9-second build that did
/// not contain it. The two numbers were never reconciled, and the cost of
/// losing that race is the whole session: the lane reads the artifact once at
/// boot, so a timeout pins it to 4 index SIDs until a restart.
///
/// The file states the rule itself, for `persist_constituents`: "Disk first,
/// then the table. The artifact is the cheaper, more reliable record."
#[test]
fn no_questdb_write_runs_before_the_mapping_artifact() {
    let code = code_only(DHAN_UNIVERSE_RS);

    let artifact_at = code
        .find("write_mapping_atomic(date")
        .expect("`build_once` no longer writes the mapping artifact");

    for slow in ["write_dhan_lifecycle", "persist_constituents(questdb"] {
        let at = code
            .find(slow)
            .unwrap_or_else(|| panic!("`{slow}` call site not found in dhan_universe.rs"));
        assert!(
            artifact_at < at,
            "`{slow}` runs BEFORE `write_mapping_atomic`. It is a QuestDB round trip, and the \
             live lane blocks at boot waiting for that artifact \
             (`dhan_live_universe::await_mapping_artifact`). Putting a network write in front \
             of it spends the lane's whole budget on work nothing waits for — and when the \
             budget runs out the session collapses to 4 index SIDs for the day. Move it below \
             the artifact write, as `persist_constituents` already is."
        );
    }
}

/// A duration alone cannot bound this wait safely: a late boot would still be
/// waiting when the market opens, and dialing nothing at 09:15 is worse than
/// dialing a partial set.
#[test]
fn the_wait_is_bounded_by_the_clock_as_well_as_the_duration() {
    let src = LIVE_UNIVERSE_RS;
    let code = code_only(src);

    assert!(
        src.contains("MAPPING_WAIT_NEVER_PAST_IST_SECS"),
        "the boot wait must carry a wall-clock cutoff as well as a duration. With only a \
         duration, a box that boots at 09:05 is still waiting when the bell rings."
    );
    assert!(
        code.contains("MAPPING_WAIT_NEVER_PAST_IST_SECS"),
        "the cutoff constant is declared but never USED in code — a bound that is not \
         consulted is not a bound"
    );
    assert!(
        code.contains(r#""outcome" => "pre_open_cutoff""#),
        "the cutoff must be counted; an unlabelled exit is an unobservable one"
    );

    // The cutoff has to sit before the 09:15 open with room to dial.
    assert!(
        code.contains("9 * 3_600 + 10 * 60"),
        "the pre-open cutoff must remain 09:10 IST — five minutes before the 09:15 open, \
         which is the room the lane needs to resolve the universe, plan the pool and dial \
         its sockets. Moving it later eats that room; moving it earlier throws away wait \
         budget that costs nothing on a normal morning."
    );
}

/// The lane's budget must not silently drift back below the build it waits on.
#[test]
fn the_wait_budget_is_not_the_stale_nine_second_figure() {
    let code = code_only(LIVE_UNIVERSE_RS);
    assert!(
        code.contains("MAPPING_WAIT_DEADLINE_SECS: u64 = 600"),
        "the boot wait budget changed. It is sized against the SLOW path — up to 49 \
         sequential index-constituent CSV fetches — not the 9-second build measured on \
         2026-08-18 before those existed. `dhan_universe::BUILD_DEADLINE_SECS` budgets 900 s \
         for the same work; a lane budget far below it means the lane gives up while the \
         build it is waiting for is still healthy and running. If you lower this, re-measure \
         the build first and say so here."
    );
}
