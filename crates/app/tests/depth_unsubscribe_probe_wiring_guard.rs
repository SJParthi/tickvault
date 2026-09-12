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

/// Comment-stripped source, so a call site someone commented out cannot keep
/// this guard green.
///
/// The identical hazard the sibling guards record: `depth_first_packet`'s
/// wiring guard was the one member of its family scanning RAW source, and a
/// deleted call site left behind as `// run_configured(...)` would have kept
/// every assertion passing.
fn stripped(path: &str) -> String {
    let src = fs::read_to_string(path).unwrap_or_else(|err| panic!("read {path}: {err}"));
    src.lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The probe RUNS: the steering loop calls it, and the config reaches it from
/// the boot. Without this the whole module is a pub surface with no caller —
/// the skeleton-PR shape Rule 14 names.
#[test]
fn the_probe_is_reachable_from_the_steering_loop_and_from_boot() {
    let rebalance = stripped("src/depth_rebalance.rs");
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
    let main = stripped("src/main.rs");
    assert!(
        main.contains("depth_unsubscribe_probe: config.depth_unsubscribe_probe"),
        "boot no longer hands the probe its config, so an operator who armed it in \
         base.toml would get silence and no explanation"
    );
}

/// It runs at most ONCE per process, and only inside the session.
///
/// Both halves matter. A probe that could run twice would empty a second
/// depth-200 socket; a probe that could run pre-open would find a book that
/// has not traded yet, report `inconclusive_thin_book`, and burn the
/// session's one shot on a market that was shut.
#[test]
fn the_probe_is_one_shot_and_in_session_only() {
    let rebalance = stripped("src/depth_rebalance.rs");
    assert!(
        rebalance.contains("if !probe_run") && rebalance.contains("probe_run = true;"),
        "the once-per-process latch is gone — a second run empties a second depth-200 socket"
    );
    let gate_at = rebalance
        .find("if !probe_run")
        .expect("the latch is asserted above");
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
