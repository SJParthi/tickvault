//! Ratchet guard for the per-minute REST episode families (2026-07-15
//! coordinator-relayed Telegram-cleanliness directive, S4).
//!
//! Pins the FULL routing table: every surviving REST Degraded/Recovered
//! pair folds into a `DhanRest` episode bubble with the
//! designed `(family, conn)` slot; every `*Recovered` variant is the
//! bubble's `Resolve` edge; the once-per-day pages with no recovery edge
//! stay on the legacy lane (`episode_key() == None`); and the family-(3)
//! token Criticals stay legacy FOREVER (Critical never episode-folds).
//!
//! A future refactor that silently drops one of these arms fails HERE,
//! not in production as a restored 2-messages-per-flap storm.

use tickvault_core::notification::EpisodeRole;
use tickvault_core::notification::episode::{EpisodeConfig, EpisodeFamily, episode_config_for};
use tickvault_core::notification::events::Severity;

// ---------------------------------------------------------------------------
// The ROUTING half of this guard is RETIRED 2026-09-17
// ---------------------------------------------------------------------------
//
// Deleted here: the 16-variant routing table, the per-symbol and
// per-underlying slot-map pins, the DISJOINTNESS pin, the open/resolve
// same-bubble pin, the legacy once-per-day lane pin, the token-Critical
// legacy pin, and the first-page severity-shape pin — eight tests and
// two fixtures, all keyed on the ELEVEN `Spot1m*` / `Chain*` variants.
//
// Those variants are deleted with Dhan Telegram families 1 and 2: the
// operator's SOCKETS-ONLY narrowing removed their producers, so none had
// a production constructor left. Record, including why they were deleted
// rather than allowlisted onto `NO_PRODUCTION_DISPATCHER`:
// `dhan-rest-only-noise-lock-2026-07-14.md` §2.4.
//
// WHAT SURVIVES, and why this file was narrowed rather than deleted:
// `EpisodeFamily::DhanRest` is NOT retired. It is a slot-numbering
// identity the episode FSM persists into snapshots, so its label
// round-trip, its broker badge, its down-stale-expiry exemption, the
// three-condition envelope behind that exemption, its slot descriptions
// and its timing config are all still live surfaces with real
// regression risk — and every one of those tests is UNTOUCHED below.
//
// WHAT THIS LEAVES UNWATCHED, stated rather than implied: nothing now
// re-checks that a FUTURE per-minute family routes to a designed slot,
// that its Recovered arm is the Resolve edge, or that two legs' slot
// sets stay disjoint. The disjointness pin in particular guarded a real
// regression (F1, 2026-07-15: a spot recovery green-closing a chain
// leg's edge-latched bubble is a permanent Rule-11 false recovery). If
// such a family is ever built, this is the shape to restore.
// ---------------------------------------------------------------------------
// Family surface pins (snapshot round-trip, badge, phrasing, config)
// ---------------------------------------------------------------------------

#[test]
fn guard_rest_family_snapshot_labels_round_trip() {
    {
        let (family, label) = (EpisodeFamily::DhanRest, "dhan_rest");
        assert_eq!(family.snapshot_label(), label);
        assert_eq!(EpisodeFamily::from_snapshot_label(label), Some(family));
    }
}

#[test]
fn guard_rest_family_badges_and_descriptions_name_the_broker() {
    assert!(EpisodeFamily::DhanRest.badge().contains("DHAN"));
    assert_eq!(
        EpisodeFamily::DhanRest.feed_desc(),
        "Per-minute candle pull"
    );
}

#[test]
fn guard_rest_families_are_down_stale_expiry_exempt() {
    // G1 (fix round 2, 2026-07-15), NARROWED by R1 (fix round 3): the
    // routed REST Degraded emitters are once-per-episode edge-latched, so
    // a Down bubble legitimately gets ZERO further events for hours
    // mid-outage — stale-expiring it would edit the phone's last word to
    // "alert closed" while the leg is STILL failing (Rule-11 false
    // recovery). Only Resolve may close them WHILE the exemption envelope
    // holds; the family predicate is condition 1 of 3 (see the envelope
    // guard below).
    assert!(EpisodeFamily::DhanRest.down_stale_expiry_exempt());
    // The fold-fed families keep expiring (unchanged semantics).
    assert!(!EpisodeFamily::MainFeedWs.down_stale_expiry_exempt());
    assert!(!EpisodeFamily::OrderUpdateWs.down_stale_expiry_exempt());
}

#[test]
fn guard_rest_stale_expiry_exemption_is_the_three_condition_envelope() {
    // R1 (fix round 3, 2026-07-15): the exemption's premise ("event-less
    // = still failing") holds only IN-PROCESS + IN-SESSION — the emitter
    // edge latches are process-local and the pulls stop at 15:30 IST. The
    // envelope is family-eligible AND reconfirmed AND in-session; losing
    // ANY leg of it re-opens one of: an immortal rehydrated false-alarm
    // bubble that swallows the next outage's page/SMS, or a session-end
    // bubble that can never close.
    use tickvault_core::notification::episode::{
        EPISODE_DOWN_STALE_EXPIRE_SECS, EpisodeKey, EpisodeRegistry, in_ist_session,
    };
    let in_session_ms = {
        use chrono::TimeZone;
        let offset = chrono::FixedOffset::east_opt(19_800).unwrap();
        u64::try_from(
            offset
                .with_ymd_and_hms(2026, 7, 15, 11, 0, 0)
                .unwrap()
                .timestamp_millis(),
        )
        .unwrap()
    };
    assert!(
        in_ist_session(in_session_ms),
        "test anchor must be in-session"
    );
    let post_close_ms = in_session_ms + 5 * 3600 * 1000; // 16:00 IST
    assert!(!in_ist_session(post_close_ms));
    let stale = EPISODE_DOWN_STALE_EXPIRE_SECS * 1000;

    let k = EpisodeKey {
        family: EpisodeFamily::DhanRest,
        conn: 12,
    };
    // (1) In-process + in-session: exempt — never expires.
    let reg = EpisodeRegistry::new();
    let _ = reg.apply_event(
        k,
        EpisodeRole::Open,
        Severity::High,
        0,
        in_session_ms,
        &EpisodeConfig::default(),
    );
    assert!(
        reg.tick(in_session_ms + 2 * stale, &EpisodeConfig::default())
            .expired
            .is_empty()
    );
    // (2) Rehydrated-unconfirmed (restart edge): expires the stale window
    //     after the RESTART even in-session — a recovered-while-down leg
    //     never shows "failing" all day.
    let reg2 = EpisodeRegistry::new();
    reg2.rehydrate(reg.snapshot(), in_session_ms);
    assert_eq!(
        reg2.tick(in_session_ms + stale, &EpisodeConfig::default())
            .expired
            .len(),
        1
    );
    // (3) Out-of-session: even a reconfirmed in-process bubble closes ~30
    //     min after its last event once the pulls stop firing.
    assert_eq!(
        reg.tick(post_close_ms, &EpisodeConfig::default())
            .expired
            .len(),
        1
    );
}

#[test]
fn guard_rest_slot_descs_match_the_routing_slot_table() {
    // G2 (fix round 2): the per-slot bubble qualifier must not drift — a
    // drifted label mis-names which pull an edit/close refers to.
    //
    // ⚠ 2026-09-17: this comment said the labels stay "lockstep with the
    // events.rs slot table (rest_slot / chain_rest_slot)". Those two const
    // mappers are DELETED with the REST episode arms that called them, so
    // there is no second table to be lockstep WITH: these labels are now
    // the ONLY place slots 0..=15 are named, and this test pins them as
    // literals rather than as a cross-check. Kept because a rehydrated
    // snapshot from a session before the removal still carries these conn
    // numbers, and the renderer must keep naming them the same way.
    for family in [EpisodeFamily::DhanRest] {
        for (conn, label) in [
            (0_u8, "spot pull"),
            (1, "chain pull"),
            (2, "contract pull"),
            (7, "chain pull (other index)"),
            (8, "spot pull (NIFTY)"),
            (9, "spot pull (BANKNIFTY)"),
            (10, "spot pull (SENSEX)"),
            (11, "spot pull (INDIA VIX)"),
            (12, "chain pull (NIFTY)"),
            (13, "chain pull (BANKNIFTY)"),
            (14, "chain pull (SENSEX)"),
            (15, "spot pull (other index)"),
        ] {
            assert_eq!(
                family.slot_desc(conn),
                label,
                "{family:?} slot {conn} label drifted"
            );
        }
        // Unknown future slot: honest generic, never a panic or empty.
        assert_eq!(family.slot_desc(3), "candle pull");
    }
    // Non-REST families carry no slot qualifier (renders stay generic).
    assert_eq!(EpisodeFamily::MainFeedWs.slot_desc(0), "");
}

#[test]
fn guard_rest_family_config_is_default() {
    // EpisodeConfig carries no PartialEq — Debug-compare pins the values.
    let default = format!("{:?}", EpisodeConfig::default());
    {
        let family = EpisodeFamily::DhanRest;
        assert_eq!(
            format!("{:?}", episode_config_for(family)),
            default,
            "{family:?} must keep the default episode timing knobs"
        );
    }
}
