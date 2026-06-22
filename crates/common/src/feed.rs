//! Canonical market-data **feed identity** — the single source of truth for
//! which feeds exist and their stable wire labels.
//!
//! SP1 of the common-feed-engine convergence (operator lock 2026-06-22: "only
//! feed live ticks will be fetched and pulled … from there everything is same …
//! make everything as common runtime dynamic scalable approach"). Previously the
//! `Feed` enum lived in `api::feed_state` (the WRONG layer — `core`/`trading`/
//! `storage` all sit BELOW `api` in the dependency flow `common ← core ← trading
//! ← storage ← api ← app`, so they could not import it and duplicated the
//! `"dhan"`/`"groww"` labels as scattered raw consts). Moving it to `common` —
//! which every crate depends on — gives ONE enum + ONE label fn that the writers,
//! aggregators, parity engine, and API all share.
//!
//! ## `Feed::ALL` — the single-source list (anti-regression)
//!
//! Every "iterate the feeds" / "allowed-feed list" site MUST build from
//! [`Feed::ALL`] and every `match feed { … }` MUST stay exhaustive (no `_` arm).
//! That makes adding a future feed a COMPILE error at every site that forgot it —
//! the exact mechanical guard that the NTM 2-role→3-role boot panic taught us
//! (a hardcoded 2-element assumption silently dropped the 3rd). Adding `Feed::X`
//! forces every list + match to be updated before the build passes.

/// The market-data feeds this product can ingest / report / toggle.
///
/// Feed-specific code is ONLY the live-tick producer (wire protocol) + the
/// historical/backtest fetcher + the instrument-master URL/column-map. Everything
/// downstream (1-minute → all-21-TF candle generation, `candles_*` persistence,
/// parity, audit, alerts) is common and parameterized by this label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feed {
    /// Dhan (feed #1) — the primary trading feed. Binary WebSocket producer.
    Dhan,
    /// Groww (feed #2) — sidecar NDJSON producer. Default OFF.
    Groww,
}

impl Feed {
    /// The single-source list of every feed. Build every iteration / allowed-list
    /// from this — never a hand-written `[Feed::Dhan, Feed::Groww]` literal — so a
    /// future feed cannot be silently dropped from a list (NTM 2→3 lesson).
    pub const ALL: &'static [Feed] = &[Feed::Dhan, Feed::Groww];

    /// The number of feeds — derived from [`Feed::ALL`] so fixed-size per-feed
    /// arrays (e.g. the live-feed health registry) grow automatically with a new
    /// feed, no hand-counted length.
    pub const COUNT: usize = Self::ALL.len();

    /// Dense 0-based index for this feed, for indexing per-feed arrays. Stays in
    /// lockstep with [`Feed::ALL`] order (pinned by a test). A new feed adds an
    /// arm here (exhaustive match → compile error if forgotten).
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Dhan => 0,
            Self::Groww => 1,
        }
    }

    /// The stable wire-format label (`"dhan"` / `"groww"`). `const fn` so it can
    /// seed `const` label declarations in the storage/core writers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dhan => "dhan",
            Self::Groww => "groww",
        }
    }

    /// Parse a feed name (case-sensitive — the API is machine-facing). Returns
    /// `None` for anything that is not exactly a known feed label. Implemented via
    /// [`Feed::ALL`] so a new variant is automatically parseable with no edit here.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.as_str() == name)
    }

    /// Whether this feed may be toggled at runtime. BOTH Dhan and Groww are
    /// runtime-toggleable as of PR-E (2026-06-21, operator-authorized — see
    /// `websocket-connection-scope-lock.md` "DHAN RUNTIME-TOGGLE AUTHORIZED").
    /// The Dhan *disable* direction is additionally safety-gated (orders-live) in
    /// the handler via `FeedRuntimeState::can_disable_dhan`.
    #[must_use]
    pub const fn is_runtime_toggleable(self) -> bool {
        matches!(self, Self::Groww | Self::Dhan)
    }

    /// Human-readable display name for operator-facing UI (the feed-control page
    /// renders its switch label from this — single source, so a future feed's row
    /// appears with zero page edits).
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Dhan => "Dhan",
            Self::Groww => "Groww",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_as_str_and_parse_round_trip_for_every_feed() {
        // Iterate Feed::ALL so a new variant is automatically covered.
        for &feed in Feed::ALL {
            assert_eq!(Feed::parse(feed.as_str()), Some(feed));
        }
        assert_eq!(Feed::parse("DHAN"), None, "parse is case-sensitive");
        assert_eq!(Feed::parse("groww_live"), None);
        assert_eq!(Feed::parse(""), None);
    }

    #[test]
    fn test_all_list_has_unique_labels_and_no_dupes() {
        // Guards against a future variant accidentally re-using a label or being
        // omitted from ALL (the list IS the single source).
        let labels: Vec<&str> = Feed::ALL.iter().map(|f| f.as_str()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "feed labels must be unique");
        assert!(Feed::ALL.contains(&Feed::Dhan));
        assert!(Feed::ALL.contains(&Feed::Groww));
    }

    #[test]
    fn test_both_feeds_runtime_toggleable() {
        for &feed in Feed::ALL {
            assert!(
                feed.is_runtime_toggleable(),
                "{} must be runtime-toggleable (PR-E)",
                feed.as_str()
            );
        }
    }

    #[test]
    fn test_labels_are_stable_wire_format() {
        // Pin the exact wire labels — storage DEDUP keys + the API depend on them.
        assert_eq!(Feed::Dhan.as_str(), "dhan");
        assert_eq!(Feed::Groww.as_str(), "groww");
    }

    #[test]
    fn test_every_feed_has_a_non_empty_display_name() {
        // The feed-control page renders its switch label from display_name; every
        // feed in ALL must have one so a new feed's row is never blank.
        for &feed in Feed::ALL {
            assert!(
                !feed.display_name().is_empty(),
                "{} must have a display_name",
                feed.as_str()
            );
        }
        assert_eq!(Feed::Dhan.display_name(), "Dhan");
        assert_eq!(Feed::Groww.display_name(), "Groww");
    }

    #[test]
    fn test_index_is_dense_and_in_lockstep_with_all() {
        // index() must be a dense 0..COUNT bijection matching ALL order, so per-feed
        // arrays indexed by index() line up with Feed::ALL.
        assert_eq!(Feed::COUNT, Feed::ALL.len());
        for (i, &feed) in Feed::ALL.iter().enumerate() {
            assert_eq!(
                feed.index(),
                i,
                "{} index must match ALL order",
                feed.as_str()
            );
            assert!(feed.index() < Feed::COUNT);
        }
    }
}
