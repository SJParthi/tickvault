//! Canonical market-data **feed identity** — the single source of truth for
//! which feeds exist and their stable wire labels.
//!
//! SP1 of the common-feed-engine convergence (operator lock 2026-06-22: "only
//! feed live ticks will be fetched and pulled … from there everything is same …
//! make everything as common runtime dynamic scalable approach"). Previously the
//! `Feed` enum lived in `api::feed_state` (the WRONG layer — `core`/`trading`/
//! `storage` all sit BELOW `api` in the dependency flow `common ← core ← trading
//! ← storage ← api ← app`, so they could not import it and duplicated the
//! `"dhan"` labels as scattered raw consts). Moving it to `common` —
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
    /// TrueData (feed #4) — the intended LIVE-TICK source (operator lock
    /// 2026-07-24, `truedata-feed-scope-2026-07-24.md`). Native Rust
    /// `wss://push.truedata.in` binary-tick producer, default OFF; its
    /// live WS is NOT retired.
    Truedata,
}

impl Feed {
    /// The single-source list of every feed. Build every iteration / allowed-list
    /// from this — never a hand-written `[Feed::Dhan]` literal — so a
    /// future feed cannot be silently dropped from a list (NTM 2→3 lesson).
    pub const ALL: &'static [Feed] = &[Feed::Dhan, Feed::Truedata];

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
            Self::Truedata => 1,
        }
    }

    /// The stable wire-format label (`"dhan"`). `const fn` so it can
    /// seed `const` label declarations in the storage/core writers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dhan => "dhan",
            Self::Truedata => "truedata",
        }
    }

    /// Parse a feed name (case-sensitive — the API is machine-facing). Returns
    /// `None` for anything that is not exactly a known feed label. Implemented via
    /// [`Feed::ALL`] so a new variant is automatically parseable with no edit here.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.as_str() == name)
    }

    /// Whether this feed may be toggled at runtime. Dhan is
    /// runtime-toggleable as of PR-E (2026-06-21, operator-authorized — see
    /// `websocket-connection-scope-lock.md` "DHAN RUNTIME-TOGGLE AUTHORIZED").
    /// The Dhan *disable* direction is additionally safety-gated (orders-live) in
    /// the handler via `FeedRuntimeState::can_disable_dhan`.
    #[must_use]
    pub const fn is_runtime_toggleable(self) -> bool {
        matches!(self, Self::Dhan | Self::Truedata)
    }

    /// True for lanes whose LIVE-WS transport was RETIRED by operator
    /// directive — market data for such a lane arrives via the per-minute
    /// REST cadence pulls instead. Drives the honest "off by design" wording
    /// on the /feeds panel rather than the scary "switched off by operator"
    /// (operator-scare fix, 2026-07-20).
    ///
    /// **CORRECTED 2026-08-22: no feed is retired, so every arm is `false`.**
    /// This returned `true` for Dhan from the 2026-07-13 retirement until
    /// today — eleven days after that retirement was REVERSED. The operator's
    /// 2026-08-09 quotes revived the Dhan live WS (and authorized 16
    /// connections), the 2026-08-11 quote flipped `[feeds] dhan_enabled` and
    /// `TICKVAULT_DHAN_LIVE_FEED` ON, and `dhan_live_ws_retired_guard.rs` was
    /// re-blessed on 2026-08-11 to REQUIRE the revived modules. Every other
    /// surface moved; this predicate did not.
    ///
    /// What that cost: a Dhan lane that is off is now a FAULT, and this made
    /// the operator panel render it as "off by design" — a broken state
    /// wearing an intentional label, which is precisely the false-OK class
    /// rule 11 forbids. With the arm corrected, a disabled Dhan lane reads
    /// "switched off by operator", which is true.
    ///
    /// The predicate is KEPT rather than deleted: it is the seam a future
    /// retirement flips, and both call sites (`feeds_page::feed_note`,
    /// `feed_health::evaluate_feed_health`) stay correct with no edit. A
    /// predicate that is currently false everywhere is not dead — it is a
    /// switch in the off position.
    #[must_use]
    pub const fn live_ws_retired(self) -> bool {
        match self {
            // Revived 2026-08-09/11 by dated operator quote; see the doc above.
            Self::Dhan => false,
            // Its live WS is the intended tick source (operator lock 2026-07-24).
            Self::Truedata => false,
        }
    }

    /// Human-readable display name for operator-facing UI (the feed-control page
    /// renders its switch label from this — single source, so a future feed's row
    /// appears with zero page edits).
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Dhan => "Dhan",
            Self::Truedata => "TrueData",
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
        assert!(Feed::ALL.contains(&Feed::Truedata));
    }

    #[test]
    fn test_both_feeds_runtime_toggleable() {
        for &feed in Feed::ALL {
            let name = feed.as_str();
            assert!(
                feed.is_runtime_toggleable(),
                "{name} must be toggleable (PR-E)"
            );
        }
    }

    #[test]
    fn test_no_feed_reports_a_retired_live_ws_lane() {
        // CORRECTED 2026-08-22. This test asserted `Feed::Dhan.live_ws_retired()`
        // and passed for eleven days after the retirement it pinned had been
        // REVERSED — the operator's 2026-08-09 revival quotes and the
        // 2026-08-11 config flip. A ratchet that keeps asserting a fact the
        // operator has withdrawn does not protect the invariant; it protects
        // the stale claim, and it is why the /feeds panel was still calling a
        // FAULTED Dhan lane "off by design".
        //
        // Neither lane is retired today, so both arms are false. The predicate
        // stays because it is the seam a future retirement flips.
        assert!(
            !Feed::Dhan.live_ws_retired(),
            "the Dhan live WS was revived 2026-08-09/11 — a disabled lane is a \
             fault now, and must not read as 'off by design'"
        );
        assert!(
            !Feed::Truedata.live_ws_retired(),
            "TrueData live-WS lane is the intended tick source, not retired"
        );
        // Every variant, so a new feed cannot silently arrive claiming
        // retirement without this test being read again.
        for f in Feed::ALL {
            assert!(
                !f.live_ws_retired(),
                "{f:?} claims a retired live-WS lane; no retirement is in force \
                 — add the dated operator quote to the doc before flipping it"
            );
        }
    }

    #[test]
    fn test_labels_are_stable_wire_format() {
        // Pin the exact wire labels — storage DEDUP keys + the API depend on them.
        assert_eq!(Feed::Dhan.as_str(), "dhan");
        assert_eq!(Feed::Truedata.as_str(), "truedata");
    }

    #[test]
    fn test_every_feed_has_a_non_empty_display_name() {
        // The feed-control page renders its switch label from display_name; every
        // feed in ALL must have one so a new feed's row is never blank.
        for &feed in Feed::ALL {
            let name = feed.as_str();
            assert!(!feed.display_name().is_empty(), "{name} needs display_name");
        }
        assert_eq!(Feed::Dhan.display_name(), "Dhan");
        assert_eq!(Feed::Truedata.display_name(), "TrueData");
    }

    #[test]
    fn test_index_is_dense_and_in_lockstep_with_all() {
        // index() must be a dense 0..COUNT bijection matching ALL order, so per-feed
        // arrays indexed by index() line up with Feed::ALL.
        assert_eq!(Feed::COUNT, Feed::ALL.len());
        for (i, &feed) in Feed::ALL.iter().enumerate() {
            let name = feed.as_str();
            assert_eq!(feed.index(), i, "{name} index must match ALL order");
            assert!(feed.index() < Feed::COUNT);
        }
    }
}
