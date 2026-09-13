//! The operator-armed unsubscribe probe is WIRED, DEFAULT OFF, and cannot
//! quietly become the default (scope lock, 2026-09-12).
//!
//! # Why a guard rather than trust
//!
//! An armed run empties a depth-200 socket for about a minute and stalls the
//! depth steering for the length of the measurement. That is a fine price for
//! a measurement an operator chose; it is not a fine price for a build that
//! shipped with a flag flipped by accident. Three of the four assertions below
//! exist to make "off" a property of the tree rather than of somebody's
//! memory.
//!
//! The fourth is the opposite worry, and it is the one this repository keeps
//! re-learning: a probe that is default-off AND unreachable is a skeleton, and
//! `audit-findings-2026-04-17.md` Rule 14 forbids exactly that. So the call
//! site is asserted too — the flag being off must be a CHOICE, not the only
//! state the code can reach.

use std::fs;

/// Comment-stripped, TEST-MODULE-stripped production source.
///
/// # Why both, and why the second half was the real hole (2026-09-13)
///
/// The first version stripped comments only, and every positional assertion
/// below used `find()` — which returns the FIRST occurrence. A copy of the
/// asserted text inside this file's own `#[cfg(test)]` module, or inside the
/// scanned file's, satisfied the search while the production call site was
/// gone. That is the vacuous-guard class this repository has now recorded
/// eleven times: a source scan run against a WHOLE file is satisfiable by the
/// text that names it.
///
/// Two changes close it. The scan stops at the first `#[cfg(test)]`, so only
/// production text is searched; and every positional assertion first asserts
/// the occurrence COUNT, so a second copy fails loudly instead of silently
/// deciding which one `find()` lands on.
///
/// The comment stripper also no longer truncates at a `//` that is part of a
/// `://` scheme — `"wss://api-feed.dhan.co"` used to become `"wss:`, which
/// would make any future assertion about a URL literal unsatisfiable for a
/// reason nobody could see.
fn production_only(path: &str) -> String {
    let src = fs::read_to_string(path).unwrap_or_else(|err| panic!("read {path}: {err}"));
    let head = src
        .split_once("\n#[cfg(test)]")
        .map_or(src.as_str(), |(h, _)| h);
    head.lines()
        .map(|line| {
            let mut cut = None;
            let bytes = line.as_bytes();
            let mut i = 0;
            while i + 1 < bytes.len() {
                if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
                    cut = Some(i);
                    break;
                }
                i += 1;
            }
            match cut {
                Some(at) => &line[..at],
                None => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How many times `needle` occurs in production source — the anti-vacuity
/// floor every positional assertion below stands on.
fn count_in(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// The stripper and the counter must be able to FAIL. A guard that cannot
/// fail is worse than no guard, because it also certifies that the case is
/// covered.
#[test]
fn guard_self_test() {
    let src = "let a = 1;\nlet url = \"wss://x\";\nlet b = 2; // trailing\n#[cfg(test)]\nmod t { let a = 1; }\n";
    let head = src.split_once("\n#[cfg(test)]").map_or(src, |(h, _)| h);
    assert!(
        !head.contains("mod t"),
        "the test-module split does not cut — the hole this rewrite exists to close is open"
    );
    assert_eq!(
        count_in(head, "let a = 1;"),
        1,
        "the production half must hold exactly one copy; the test module's copy is the \
         thing that used to satisfy these assertions"
    );
    assert!(
        head.contains("wss://x"),
        "the comment stripper truncated a URL scheme — `//` inside `://` is not a comment"
    );
    assert_eq!(
        count_in(src, "let a = 1;"),
        2,
        "the UNSTRIPPED source holds both copies — this asserts the fixture actually \
         reproduces the hazard, so the test above is not passing vacuously"
    );
}

/// The probe RUNS: the steering loop calls it, and the config reaches it from
/// the boot. Without this the whole module is a pub surface with no caller —
/// the skeleton-PR shape Rule 14 names.
#[test]
fn the_probe_is_reachable_from_the_steering_loop_and_from_boot() {
    let rebalance = production_only("src/depth_rebalance.rs");
    assert!(
        rebalance
            .contains("crate::depth_unsubscribe_probe::run_configured(&probe_cfg, &mut sockets)"),
        "the depth steering loop no longer calls the probe — with no caller the module is a \
         pub surface nothing reaches, which is the skeleton-PR shape Rule 14 forbids"
    );
    assert!(
        rebalance.contains("probe_cfg: tickvault_common::config::DepthUnsubscribeProbeConfig"),
        "the steering loop must TAKE the config, not read a global — config that reaches a \
         decision through a global is config a test cannot set"
    );
    let main = production_only("src/main.rs");
    assert!(
        main.contains("depth_unsubscribe_probe: config.depth_unsubscribe_probe"),
        "boot no longer hands the probe its config, so an operator who armed it in \
         base.toml would get silence and no explanation"
    );
}

/// It runs at most ONCE per DAY, and only inside the session.
///
/// Both halves matter. A probe that could run twice would empty a second
/// depth-200 socket; a probe that could run pre-open would find a book that
/// has not traded yet, report `inconclusive_thin_book`, and burn the
/// session's one shot on a market that was shut.
///
/// # Why per-DAY and not per-process (2026-09-13)
///
/// Those are not the same thing on this box. The unit restarts on every
/// deploy and on an OOM kill, so an in-memory flag re-arms the probe on a
/// restart and spends a second socket on a day the operator authorized one
/// measurement. The scope lock's REJECT list says "more than once per
/// session", and only a persisted latch can enforce that.
#[test]
fn the_probe_is_one_shot_and_in_session_only() {
    let rebalance = production_only("src/depth_rebalance.rs");

    // Anti-vacuity first: every positional assertion below uses `find()`,
    // which silently picks the FIRST occurrence. Asserting the count turns a
    // stray second copy into a loud failure instead of a coin flip.
    assert_eq!(
        count_in(&rebalance, "if !probe_run"),
        1,
        "the once-per-session gate must appear EXACTLY once in production source — a \
         second copy makes the window assertion below decide which one it lands on"
    );
    assert_eq!(
        count_in(&rebalance, "probe_run = true;"),
        1,
        "the latch must be set in exactly one place"
    );

    // The latch is PERSISTED, so a restart cannot re-arm it.
    assert_eq!(
        count_in(&rebalance, "probe_already_ran_today("),
        1,
        "the latch is seeded from `false` again — that is once per PROCESS, and a deploy \
         or OOM restart would arm the probe a second time on the same day"
    );
    let seeded_at = rebalance
        .find("probe_already_ran_today(")
        .expect("counted exactly one above");
    assert_eq!(
        count_in(&rebalance, "mark_probe_ran_today(&probe_latch, probe_day)"),
        1,
        "the day latch is never written, so it can never be read back as spent"
    );

    let gate_at = rebalance
        .find("if !probe_run")
        .expect("the gate is asserted above");
    let mark_at = rebalance
        .find("mark_probe_ran_today(&probe_latch, probe_day)")
        .expect("the write is asserted above");
    let run_at = rebalance
        .find("depth_unsubscribe_probe::run_configured")
        .expect("the call site is asserted by the reachability test");
    assert!(
        mark_at < run_at,
        "the day latch is written AFTER the arm runs — a crash mid-probe would then leave \
         the day unspent and the next restart would empty a second socket. Fail-closed is \
         the other order: mark first, measure second."
    );

    // ADDED 2026-09-13 — the day the latch is MARKED with is read at ARM time,
    // never captured at boot.
    //
    // The seed runs once at boot and the arm can run many hours later. A single
    // `probe_day` binding hoisted above the loop reads correct and is wrong
    // across IST midnight: the box would mark yesterday.s date as spent, leave
    // today.s unspent, and arm a second time. The seed therefore sits BEFORE
    // the gate and the `probe_day` binding the mark uses sits INSIDE it.
    let probe_day_at = rebalance
        .find("let probe_day = ")
        .expect("the arm must bind the day it marks");
    assert_eq!(
        count_in(&rebalance, "let probe_day = "),
        1,
        "two `probe_day` bindings means the mark and the window can disagree about \
         which day the probe just spent"
    );
    assert!(
        seeded_at < gate_at,
        "the persisted latch must be read once at boot, before the gate"
    );
    assert!(
        gate_at < probe_day_at && probe_day_at < mark_at,
        "`probe_day` is captured outside the gate — across IST midnight the box would \
         mark yesterday as spent and arm the probe a second time today"
    );

    let window = rebalance
        .get(gate_at..gate_at.saturating_add(900))
        .unwrap_or_default();
    assert!(
        window.contains("RANKING_PUBLISH_DEADLINE_SECS_OF_DAY_IST")
            && window.contains("TOP_VOLUME_CAPTURE_END_SECS_OF_DAY_IST"),
        "the probe's in-session window is gone: it must reuse the window this loop already \
         reasons about, never invent a second market-hours notion of its own"
    );
}

/// The day latch parses only a real `YYYYMMDD`, and an unknown latch reads as
/// NOT spent.
///
/// The direction is deliberate and is the opposite of the write ordering
/// above: a corrupt or truncated file must not make an operator-armed
/// measurement permanently impossible with no way to see why. Fail-closed on
/// the WRITE (a crash spends the day), fail-open on the READ (a corrupt file
/// does not).
#[test]
fn the_day_latch_parses_only_a_real_date() {
    use tickvault_app::depth_unsubscribe_probe::parse_day_latch;
    assert_eq!(parse_day_latch("20260913"), Some(20_260_913));
    assert_eq!(parse_day_latch("  20260913\n"), Some(20_260_913));
    assert_eq!(
        parse_day_latch(""),
        None,
        "an empty file is unknown, not spent"
    );
    assert_eq!(
        parse_day_latch("2026091"),
        None,
        "a truncated write is unknown"
    );
    assert_eq!(
        parse_day_latch("202609130"),
        None,
        "nine digits is not a date"
    );
    assert_eq!(parse_day_latch("spent!!!"), None, "prose is unknown");
    assert!(
        tickvault_app::depth_unsubscribe_probe::day_latch_path()
            .to_string_lossy()
            .contains("depth-unsubscribe-probe-day"),
        "the latch path moved without this guard moving with it"
    );
}

/// DEFAULT OFF in the type, and OFF in the file that ships.
///
/// `Default` derived on three bools is off by construction; asserting it
/// anyway is cheap and catches the day someone adds a `default = "..."`
/// serde attribute. The base.toml half is the non-vacuous surface: an absent
/// section and a false one are the same to serde, but only one of them proves
/// nobody has flipped it.
#[test]
fn every_probe_flag_ships_off() {
    let cfg = tickvault_common::config::DepthUnsubscribeProbeConfig::default();
    assert!(!cfg.enabled, "the master switch must default OFF");
    assert!(!cfg.unsubscribe_arm, "Arm A must default OFF");
    assert!(!cfg.socket_close_arm, "Arm B must default OFF");

    let toml = fs::read_to_string("../../config/base.toml").expect("base.toml");
    let section = toml
        .split_once("[depth_unsubscribe_probe]")
        .map(|(_, rest)| rest.split("\n[").next().unwrap_or(rest))
        .expect("base.toml must carry the section, written out rather than absent");
    for flag in ["enabled", "unsubscribe_arm", "socket_close_arm"] {
        assert!(
            section.contains(&format!("{flag} = false")),
            "base.toml has `{flag}` set to something other than false — arming the probe is \
             an operator decision taken deliberately on the day, never a committed default"
        );
    }
    assert!(
        !section.contains("= true"),
        "a flag in [depth_unsubscribe_probe] is true in the shipped config"
    );
}

/// The watch window must end before the ghost detector's grace.
///
/// Asserted here as well as by the module's const-assert because this is the
/// one relationship that turns Arm A's verdict into Arm B's answer: past the
/// grace, the ghost detector re-dials the socket Arm A is measuring, and the
/// replay of an emptied guard produces silence that reads as "the vendor
/// honoured it".
#[test]
fn the_watch_window_cannot_outlive_the_ghost_grace() {
    let watch = i64::try_from(tickvault_app::depth_unsubscribe_probe::PROBE_WATCH_SECS)
        .expect("the watch window is a small number of seconds");
    assert!(
        watch < tickvault_app::depth_subscription_view::GHOST_GRACE_SECS,
        "the probe would still be watching when the ghost detector re-dials the socket, \
         which is Arm B's mechanism delivering Arm A's verdict"
    );
    assert!(watch > 0, "a zero-length watch measures nothing");
}
