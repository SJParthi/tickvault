//! GrowwFeed episode wiring guard — RETUNED 2026-07-15.
//!
//! The GrowwFeed RUNTIME-incident routing this guard originally pinned
//! (`GrowwSidecarRejected` + the Groww `FeedDown`/`FeedRecovered` arms,
//! 2026-07-14 operator noise directive) was RETIRED 2026-07-15 with the
//! Groww live feed (operator directive: "remove the whole Groww live feed;
//! keep only spot 1m and option chain for both brokers") — those variants
//! and their `EpisodeFamily::GrowwFeed` routing arms are deleted.
//!
//! What SURVIVES and stays pinned here:
//! (a) the Groww BOOT pings (`FeedAuthOk` / `FeedInstrumentsLoaded`) still
//!     fold into the Boot bubble (the boot family), and
//! (b) the retained `EpisodeFamily::GrowwFeed` family (renderer +
//!     historical snapshots still name it) keeps the default episode
//!     config, so any future re-wire inherits unchanged behavior.

use tickvault_core::notification::episode::{
    BOOT_EPISODE_KEY, EpisodeConfig, EpisodeFamily, episode_config_for,
};
use tickvault_core::notification::events::NotificationEvent;

// (a) The Groww boot pings still map to the Boot family.
#[test]
fn guard_groww_boot_pings_unchanged() {
    for event in [
        NotificationEvent::FeedAuthOk {
            feed: "Groww".to_string(),
        },
        NotificationEvent::FeedInstrumentsLoaded {
            feed: "Groww".to_string(),
            subscribed: 768,
            indices: 2,
            stocks: 766,
            skipped: 0,
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

// (b) The retained GrowwFeed family runs the default episode config.
#[test]
fn guard_groww_family_uses_default_episode_config() {
    let cfg = episode_config_for(EpisodeFamily::GrowwFeed);
    let default = EpisodeConfig::default();
    assert_eq!(cfg.edit_min_interval_secs, default.edit_min_interval_secs);
    assert_eq!(cfg.stability_secs, default.stability_secs);
    assert_eq!(cfg.reopen_secs, default.reopen_secs);
    assert_eq!(cfg.down_stale_expire_secs, default.down_stale_expire_secs);
}
