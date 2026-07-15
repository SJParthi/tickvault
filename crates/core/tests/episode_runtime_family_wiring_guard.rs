//! GrowwFeed episode wiring guard — RETUNED 2026-07-15.
//!
//! The GrowwFeed RUNTIME-incident routing this guard originally pinned
//! (`GrowwSidecarRejected` + the Groww `FeedDown`/`FeedRecovered` arms,
//! 2026-07-14 operator noise directive) was RETIRED 2026-07-15 with the
//! Groww live feed (operator directive: "remove the whole Groww live feed;
//! keep only spot 1m and option chain for both brokers") — those variants
//! and their `EpisodeFamily::GrowwFeed` routing arms are deleted.
//!
//! What SURVIVES and stays pinned here: the retained
//! `EpisodeFamily::GrowwFeed` family (renderer + historical snapshots still
//! name it) keeps the default episode config, so any future re-wire inherits
//! unchanged behavior. (The Groww boot pings `FeedAuthOk` /
//! `FeedInstrumentsLoaded` were deleted 2026-07-15 fix-round-1b —
//! constructor-less since the rider replaced the activation lane.)

use tickvault_core::notification::episode::{EpisodeConfig, EpisodeFamily, episode_config_for};

// The retained GrowwFeed family runs the default episode config.
#[test]
fn guard_groww_family_uses_default_episode_config() {
    let cfg = episode_config_for(EpisodeFamily::GrowwFeed);
    let default = EpisodeConfig::default();
    assert_eq!(cfg.edit_min_interval_secs, default.edit_min_interval_secs);
    assert_eq!(cfg.stability_secs, default.stability_secs);
    assert_eq!(cfg.reopen_secs, default.reopen_secs);
    assert_eq!(cfg.down_stale_expire_secs, default.down_stale_expire_secs);
}
