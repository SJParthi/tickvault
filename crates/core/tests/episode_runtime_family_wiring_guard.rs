//! GrowwFeed runtime-incident episode wiring guard (2026-07-14 operator
//! noise directive — fold the persistent Groww reject storm into ONE
//! live-edited Telegram bubble).
//!
//! Build-failing pins:
//! (a) `GrowwSidecarRejected` + the Groww `FeedDown`/`FeedRecovered` arms
//!     ARE episode-routed (`episode_key()` = the GrowwFeed family, conn 0)
//!     with the Open/Open/Resolve role table — deleting any mapping would
//!     silently restore the one-page-per-cooldown storm (RC-C).
//! (b) Non-Groww `FeedDown`/`FeedRecovered` keep the legacy immediate lane
//!     (a future feed #3 is never wrongly folded).
//! (c) The Groww BOOT pings keep mapping to the Boot family — the runtime
//!     incident family never collides with the boot bubble.
//! (d) The GrowwFeed family uses the default episode config (same throttle
//!     / stability / reopen / stale-expire as the WS families).
//!
//! House pattern: unit-level contract pins (see
//! `crates/core/tests/episode_edit_wiring_guard.rs` for the source-scan
//! companion covering the shared dispatch machinery).

use tickvault_core::notification::episode::{
    BOOT_EPISODE_KEY, EpisodeAction, EpisodeConfig, EpisodeFamily, EpisodeRole, episode_config_for,
    next_episode_action,
};
use tickvault_core::notification::events::{NotificationEvent, Severity};

// ===========================================================================
// MERGE-NOTE (G9b, fix round 2 — DELETE THIS BLOCK with the Groww live-WS
// variants): the in-flight `claude/groww-live-off-rest-only` branch DELETES
// `GrowwSidecarRejected` / `FeedDown` / `FeedRecovered` from events.rs
// (its 149-file sweep). EVERY construction of those variants in this guard
// lives in the helper fns of THIS delimited block (plus one fleet-summary
// flavor, `groww_reject_fleet`) — when that branch lands, delete this block
// and the tests that call these helpers, keeping the DhanRest/GrowwRest
// REST-family pins (which construct only surviving variants). Landing
// order + resolution rules: the plan file's "Merge coordination" section.
// ===========================================================================

fn groww_reject() -> NotificationEvent {
    NotificationEvent::GrowwSidecarRejected {
        reason: "authentication rejected".to_string(),
        fleet_summary: false,
        detail: None,
    }
}

fn groww_reject_fleet() -> NotificationEvent {
    NotificationEvent::GrowwSidecarRejected {
        reason: "3 of 10 connections rejected".to_string(),
        fleet_summary: true,
        detail: None,
    }
}

fn feed_down(feed: &str) -> NotificationEvent {
    NotificationEvent::FeedDown {
        feed: feed.to_string(),
        reason: "the feed stopped streaming".to_string(),
        market_open: true,
        operator_initiated: false,
    }
}

fn feed_down_full(feed: &str, market_open: bool, operator_initiated: bool) -> NotificationEvent {
    NotificationEvent::FeedDown {
        feed: feed.to_string(),
        reason: "the feed stopped streaming".to_string(),
        market_open,
        operator_initiated,
    }
}

fn feed_recovered(feed: &str) -> NotificationEvent {
    NotificationEvent::FeedRecovered {
        feed: feed.to_string(),
        down_secs: 120,
    }
}

// ==================== end MERGE-NOTE delimited block ======================

// (a) The Groww runtime incident events are episode-routed with the
//     Open/Open/Resolve role table.
#[test]
fn guard_groww_runtime_events_are_episode_routed() {
    let cases: [(NotificationEvent, EpisodeRole); 3] = [
        (groww_reject(), EpisodeRole::Open),
        (feed_down("Groww"), EpisodeRole::Open),
        (feed_recovered("Groww"), EpisodeRole::Resolve),
    ];
    for (event, expected_role) in cases {
        let key = event.episode_key().unwrap_or_else(|| {
            panic!(
                "{} must be episode-routed (GrowwFeed family) — a None here \
                 restores the one-page-per-cooldown reject storm",
                event.topic()
            )
        });
        assert_eq!(
            key.family,
            EpisodeFamily::GrowwFeed,
            "{} must map to the GrowwFeed family",
            event.topic()
        );
        assert_eq!(key.conn, 0, "one logical Groww feed — conn is always 0");
        assert_eq!(
            event.episode_role(),
            expected_role,
            "{} role drifted",
            event.topic()
        );
    }
    // Case-insensitive feed-name compare (mirrors the boot-ping compare).
    assert_eq!(
        feed_down("groww").episode_key().map(|k| k.family),
        Some(EpisodeFamily::GrowwFeed)
    );
    // The fleet-summary flavor folds into the SAME bubble.
    let fleet = groww_reject_fleet();
    assert_eq!(
        fleet.episode_key().map(|k| k.family),
        Some(EpisodeFamily::GrowwFeed)
    );
}

// (b) A non-Groww feed keeps the legacy immediate lane.
#[test]
fn guard_non_groww_feed_incidents_stay_legacy() {
    for event in [
        feed_down("Dhan"),
        feed_down("SomeOtherFeed"),
        feed_recovered("SomeOtherFeed"),
    ] {
        assert!(
            event.episode_key().is_none(),
            "non-Groww {} must keep the legacy lane",
            event.topic()
        );
    }
}

// (c) The Groww BOOT pings still map to the Boot family — no collision
//     with the runtime incident family.
#[test]
fn guard_groww_boot_pings_unchanged() {
    for event in [
        NotificationEvent::FeedAuthOk {
            feed: "Groww".to_string(),
        },
        NotificationEvent::FeedConnectedAwaitingTicks {
            feed: "Groww".to_string(),
            subscribed: 768,
            market_open: true,
        },
    ] {
        assert_eq!(
            event.episode_key(),
            Some(BOOT_EPISODE_KEY),
            "{} must keep folding into the boot bubble",
            event.topic()
        );
    }
}

// (d) The GrowwFeed family runs the default (WS-identical) episode config.
//
// FIX-E note (hostile review 2026-07-14, pre-existing): the runtime
// dispatch/tick sites in service.rs currently hardcode
// `EpisodeConfig::default()` rather than calling `episode_config_for`
// (the Boot family has its own dispatcher with its own zero-throttle
// config), so for non-Boot families `episode_config_for` is a
// FORWARD-LOOKING lever — this pin guarantees that IF it is wired later,
// the GrowwFeed behavior is unchanged (default == what runtime uses today).
#[test]
fn guard_groww_family_uses_default_episode_config() {
    let cfg = episode_config_for(EpisodeFamily::GrowwFeed);
    let default = EpisodeConfig::default();
    assert_eq!(cfg.edit_min_interval_secs, default.edit_min_interval_secs);
    assert_eq!(cfg.stability_secs, default.stability_secs);
    assert_eq!(cfg.reopen_secs, default.reopen_secs);
    assert_eq!(cfg.down_stale_expire_secs, default.down_stale_expire_secs);
}

// (e) FIX-A (hostile review 2026-07-14): a DELIBERATE feeds-page disable is
//     NEVER episode-routed — an in-place "retrying automatically" edit (or
//     a silently-throttled fold) would falsely claim a disabled feed
//     retries. The legacy lane carries the honest body naming the ONE
//     action that fixes it (re-enable from the feeds page).
#[test]
fn guard_operator_initiated_feed_down_stays_legacy() {
    for market_open in [true, false] {
        let event = feed_down_full("Groww", market_open, true);
        assert!(
            event.episode_key().is_none(),
            "operator-initiated FeedDown (market_open={market_open}) must keep the legacy lane"
        );
        let body = event.to_message();
        assert!(
            body.contains("re-enable"),
            "the legacy body must still name the re-enable action: {body:?}"
        );
    }
}

// (f) FIX-D (hostile review 2026-07-14): the off-hours Low-severity Groww
//     FeedDown keeps its pre-existing legacy 60s-coalescer path — only a
//     PAGING (>= High, in-market) FeedDown opens/folds the incident bubble.
#[test]
fn guard_off_hours_low_feed_down_stays_legacy() {
    let event = feed_down_full("Groww", false, false);
    assert_eq!(event.severity(), Severity::Low, "off-hours FeedDown is Low");
    assert!(
        event.episode_key().is_none(),
        "Low off-hours FeedDown must keep the legacy coalescer path"
    );
    // The in-market High flavor IS routed (the incident bubble).
    let paging = feed_down_full("Groww", true, false);
    assert_eq!(paging.severity(), Severity::High);
    assert!(paging.episode_key().is_some());
}

// (g) A Groww FeedRecovered with NO open episode (e.g. the Down was the
//     Low/off-hours flavor that never opened a bubble) is DELIVERED via
//     the FSM's SendLegacy arm — never dropped (the legacy_passthrough
//     arm in the dispatch shell).
#[test]
fn guard_recovery_without_open_episode_delivers_via_legacy_lane() {
    let action = next_episode_action(
        None,
        None,
        EpisodeRole::Resolve,
        Severity::Medium,
        1_751_900_000_000,
        &EpisodeConfig::default(),
    );
    assert_eq!(
        action,
        EpisodeAction::SendLegacy,
        "an untracked recovery must route through the legacy immediate lane"
    );
}

// (h) FIX-G advisory pin (hostile review 2026-07-14): NO Critical-severity
//     event maps to the GrowwFeed family today, so the un-exercised
//     Critical branch of the High→Critical escalation edge stays
//     unreachable. Adding a Critical Groww event later must consciously
//     revisit this pin (and exercise that edge).
#[test]
fn guard_no_critical_event_maps_to_any_feed_family() {
    // Renamed from guard_no_critical_event_maps_to_groww_family (2026-07-15
    // S4): the pin now covers EVERY feed-incident family — GrowwFeed plus
    // the DhanRest/GrowwRest REST families — because a Critical event in
    // any of them would exercise the untested escalation edge. Token
    // Criticals stay legacy forever (episode_rest_family_wiring_guard.rs).
    let routed = [
        (groww_reject(), EpisodeFamily::GrowwFeed),
        (groww_reject_fleet(), EpisodeFamily::GrowwFeed),
        (
            feed_down_full("Groww", true, false),
            EpisodeFamily::GrowwFeed,
        ),
        (feed_recovered("Groww"), EpisodeFamily::GrowwFeed),
        (
            NotificationEvent::Spot1mFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: "10:42 AM".to_string(),
            },
            EpisodeFamily::DhanRest,
        ),
        (
            NotificationEvent::Chain1mUnderlyingNotServed {
                underlying: "NIFTY",
                empty_minutes: 10,
            },
            EpisodeFamily::DhanRest,
        ),
        (
            NotificationEvent::GrowwSpot1mFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: "10:42 AM".to_string(),
            },
            EpisodeFamily::GrowwRest,
        ),
        (
            NotificationEvent::GrowwContract1mFetchRecovered {
                minute_ist: "10:45 AM".to_string(),
                failed_minutes: 3,
            },
            EpisodeFamily::GrowwRest,
        ),
    ];
    for (event, family) in routed {
        assert_eq!(
            event.episode_key().map(|k| k.family),
            Some(family),
            "{} must be episode-routed for this pin to be meaningful",
            event.topic()
        );
        assert!(
            event.severity() < Severity::Critical,
            "{} must stay below Critical — a Critical feed-family event \
             would exercise the untested escalation edge; revisit this pin \
             first",
            event.topic()
        );
    }
}
