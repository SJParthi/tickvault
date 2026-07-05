//! Per-feed identity badge for Telegram messages — 🔷 DHAN vs 🟢 GROWW.
//!
//! Operator directive 2026-07-05 (verbatim): "why dhan messages and groww
//! messages are not same... dhan and groww should be uniquely seen to view
//! easily".
//!
//! With two live market-data feeds posting into the SAME Telegram chat, a
//! feed-scoped message ("Auth OK", "instruments loaded", "feed connected",
//! WebSocket lifecycle) is ambiguous without a feed tag — the operator
//! cannot tell at a glance WHICH feed the message is about. Every
//! feed-scoped Telegram message therefore carries a feed badge at the start
//! of its body: `🔷 DHAN` or `🟢 GROWW`.
//!
//! # Placement (the 10 Telegram commandments hold)
//!
//! The severity emoji stays at the very start of the first line
//! (commandment 10 — the prefix layer in `service.rs` is untouched); the
//! feed badge leads the message BODY, applied in
//! `NotificationEvent::to_message` so BOTH dispatch paths (immediate bypass
//! and coalescer samples) carry it identically and per-event edits are
//! never needed. The badge is one emoji + one plain word — no file paths,
//! no library names (commandments 1–3).
//!
//! # Honest envelope
//!
//! Only events that are provably scoped to exactly one feed carry a badge.
//! Events with a dynamic `feed` field resolve their badge from that field;
//! an unknown future feed name resolves to `None` and the message renders
//! exactly as before — never a wrong badge. Non-feed events (QuestDB, boot,
//! risk, orders, self-test) are byte-identical to before.
//!
//! # Performance
//!
//! Cold path only (notification formatting). `badge()` is `const`; name
//! resolution is a case-insensitive comparison against two literals — O(1),
//! zero allocation.

/// Which feed a feed-scoped Telegram message belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedBadge {
    /// Dhan — feed #1 (main-feed + order-update WebSockets, Dhan JWT,
    /// instrument master).
    Dhan,
    /// Groww — feed #2 (sidecar/bridge lane, shared token minter,
    /// watch-list).
    Groww,
}

impl FeedBadge {
    /// The badge string placed at the start of a feed-scoped message body.
    ///
    /// Plain-English per the 10 Telegram commandments: one emoji + one
    /// word, visually distinct per feed (blue diamond vs green circle) so
    /// the two feeds are "uniquely seen to view easily".
    pub const fn badge(&self) -> &'static str {
        match self {
            Self::Dhan => "\u{1f537} DHAN",   // 🔷
            Self::Groww => "\u{1f7e2} GROWW", // 🟢
        }
    }
}

/// Resolve a feed display name (e.g. the `feed` field on
/// `FeedAuthOk` / `FeedInstrumentsLoaded` / `FeedConnectedAwaitingTicks`)
/// to its badge. Case-insensitive; an unknown feed name returns `None` so
/// the message renders un-badged rather than wrongly badged (honest — no
/// guessing).
pub fn feed_badge_for_name(name: &str) -> Option<FeedBadge> {
    if name.eq_ignore_ascii_case("dhan") {
        Some(FeedBadge::Dhan)
    } else if name.eq_ignore_ascii_case("groww") {
        Some(FeedBadge::Groww)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feed_badge_dhan_and_groww_are_distinct_short_plain() {
        // One glance on a phone: the two badges must never be confusable,
        // must stay a single short token, and must carry distinct emoji.
        let dhan = FeedBadge::Dhan.badge();
        let groww = FeedBadge::Groww.badge();
        assert_eq!(dhan, "🔷 DHAN");
        assert_eq!(groww, "🟢 GROWW");
        assert_ne!(dhan, groww);
        assert!(dhan.contains("DHAN") && !dhan.contains("GROWW"));
        assert!(groww.contains("GROWW") && !groww.contains("DHAN"));
        assert!(dhan.chars().count() <= 10);
        assert!(groww.chars().count() <= 10);
        // Commandments 1–3: no paths, no jargon, no version numbers.
        for b in [dhan, groww] {
            assert!(!b.contains('/'));
            assert!(!b.contains(".rs"));
        }
    }

    #[test]
    fn test_feed_badge_for_name_case_insensitive_and_unknown_none() {
        assert_eq!(feed_badge_for_name("Dhan"), Some(FeedBadge::Dhan));
        assert_eq!(feed_badge_for_name("dhan"), Some(FeedBadge::Dhan));
        assert_eq!(feed_badge_for_name("DHAN"), Some(FeedBadge::Dhan));
        assert_eq!(feed_badge_for_name("Groww"), Some(FeedBadge::Groww));
        assert_eq!(feed_badge_for_name("groww"), Some(FeedBadge::Groww));
        assert_eq!(feed_badge_for_name("GROWW"), Some(FeedBadge::Groww));
        // Unknown / future feed → un-badged, never wrongly badged.
        assert_eq!(feed_badge_for_name("Zerodha"), None);
        assert_eq!(feed_badge_for_name(""), None);
    }
}
