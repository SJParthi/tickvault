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
    BOOT_EPISODE_KEY, EpisodeConfig, EpisodeFamily, EpisodeRole, episode_config_for,
};
use tickvault_core::notification::events::NotificationEvent;

fn groww_reject() -> NotificationEvent {
    NotificationEvent::GrowwSidecarRejected {
        reason: "authentication rejected".to_string(),
        fleet_summary: false,
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

fn feed_recovered(feed: &str) -> NotificationEvent {
    NotificationEvent::FeedRecovered {
        feed: feed.to_string(),
        down_secs: 120,
    }
}

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
    let fleet = NotificationEvent::GrowwSidecarRejected {
        reason: "3 of 10 connections rejected".to_string(),
        fleet_summary: true,
    };
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
#[test]
fn guard_groww_family_uses_default_episode_config() {
    let cfg = episode_config_for(EpisodeFamily::GrowwFeed);
    let default = EpisodeConfig::default();
    assert_eq!(cfg.edit_min_interval_secs, default.edit_min_interval_secs);
    assert_eq!(cfg.stability_secs, default.stability_secs);
    assert_eq!(cfg.reopen_secs, default.reopen_secs);
    assert_eq!(cfg.down_stale_expire_secs, default.down_stale_expire_secs);
}
