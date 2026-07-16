//! Ratchet guard for the per-minute REST episode families (2026-07-15
//! coordinator-relayed Telegram-cleanliness directive, S4).
//!
//! Pins the FULL routing table: every surviving REST Degraded/Recovered
//! pair folds into a `DhanRest` / `GrowwRest` episode bubble with the
//! designed `(family, conn)` slot; every `*Recovered` variant is the
//! bubble's `Resolve` edge; the once-per-day pages with no recovery edge
//! stay on the legacy lane (`episode_key() == None`); and the family-(3)
//! token Criticals stay legacy FOREVER (Critical never episode-folds).
//!
//! A future refactor that silently drops one of these arms fails HERE,
//! not in production as a restored 2-messages-per-flap storm.

use tickvault_core::notification::EpisodeRole;
use tickvault_core::notification::episode::{EpisodeConfig, EpisodeFamily, episode_config_for};
use tickvault_core::notification::events::{NotificationEvent, Severity};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn m(s: &str) -> String {
    s.to_string()
}

/// The full 16-variant routing table: (event, family, conn, is_resolve).
fn routed_rest_events() -> Vec<(NotificationEvent, EpisodeFamily, u8, bool)> {
    vec![
        (
            NotificationEvent::Spot1mFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: m("10:42 AM"),
            },
            EpisodeFamily::DhanRest,
            0,
            false,
        ),
        (
            NotificationEvent::Spot1mFetchRecovered {
                minute_ist: m("10:45 AM"),
                failed_minutes: 3,
            },
            EpisodeFamily::DhanRest,
            0,
            true,
        ),
        (
            NotificationEvent::ChainFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: m("10:42 AM"),
            },
            EpisodeFamily::DhanRest,
            1,
            false,
        ),
        (
            NotificationEvent::ChainFetchRecovered {
                minute_ist: m("10:45 AM"),
                failed_minutes: 3,
            },
            EpisodeFamily::DhanRest,
            1,
            true,
        ),
        (
            NotificationEvent::Spot1mSidNotServed {
                symbol: m("INDIA VIX"),
                consecutive_minutes: 10,
            },
            EpisodeFamily::DhanRest,
            11,
            false,
        ),
        (
            NotificationEvent::Spot1mSidServedRecovered {
                symbol: m("INDIA VIX"),
                not_served_minutes: 10,
            },
            EpisodeFamily::DhanRest,
            11,
            true,
        ),
        (
            NotificationEvent::Chain1mUnderlyingNotServed {
                underlying: "NIFTY",
                empty_minutes: 10,
            },
            EpisodeFamily::DhanRest,
            12,
            false,
        ),
        (
            NotificationEvent::Chain1mUnderlyingServedRecovered {
                underlying: "NIFTY",
                empty_minutes: 10,
            },
            EpisodeFamily::DhanRest,
            12,
            true,
        ),
        (
            NotificationEvent::GrowwSpot1mFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: m("10:42 AM"),
            },
            EpisodeFamily::GrowwRest,
            0,
            false,
        ),
        (
            NotificationEvent::GrowwSpot1mFetchRecovered {
                minute_ist: m("10:45 AM"),
                failed_minutes: 3,
            },
            EpisodeFamily::GrowwRest,
            0,
            true,
        ),
        (
            NotificationEvent::GrowwChain1mFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: m("10:42 AM"),
            },
            EpisodeFamily::GrowwRest,
            1,
            false,
        ),
        (
            NotificationEvent::GrowwChain1mFetchRecovered {
                minute_ist: m("10:45 AM"),
                failed_minutes: 3,
            },
            EpisodeFamily::GrowwRest,
            1,
            true,
        ),
        (
            NotificationEvent::GrowwContract1mFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: m("10:42 AM"),
            },
            EpisodeFamily::GrowwRest,
            2,
            false,
        ),
        (
            NotificationEvent::GrowwContract1mFetchRecovered {
                minute_ist: m("10:45 AM"),
                failed_minutes: 3,
            },
            EpisodeFamily::GrowwRest,
            2,
            true,
        ),
        (
            NotificationEvent::GrowwChain1mUnderlyingNotServed {
                underlying: "BANKNIFTY",
                empty_minutes: 10,
            },
            EpisodeFamily::GrowwRest,
            13,
            false,
        ),
        (
            NotificationEvent::GrowwChain1mUnderlyingServedRecovered {
                underlying: "BANKNIFTY",
                empty_minutes: 10,
            },
            EpisodeFamily::GrowwRest,
            13,
            true,
        ),
    ]
}

/// Once-per-day REST pages with NO recovery edge — legacy lane forever.
fn legacy_once_per_day_events() -> Vec<NotificationEvent> {
    vec![
        NotificationEvent::ChainEntitlementAbsent {
            pipeline_enabled: true,
            detail: m("no option-chain data subscription"),
        },
        NotificationEvent::ChainEntitlementConfirmed,
        NotificationEvent::ChainExpirylistFailed {
            detail: m("expiry list failed after bounded tries"),
        },
        NotificationEvent::GrowwChain1mExpiryUnresolved {
            detail: m("no usable option rows for NIFTY"),
        },
        NotificationEvent::GrowwChain1mProbeVerdict {
            ok: true,
            detail: m("all chains answered"),
        },
        NotificationEvent::GrowwContract1mBookUnresolved {
            detail: m("no usable contracts at the current expiry"),
        },
    ]
}

// ---------------------------------------------------------------------------
// Routing table pins
// ---------------------------------------------------------------------------

#[test]
fn guard_rest_pairs_route_to_designed_family_and_conn() {
    for (event, family, conn, _) in routed_rest_events() {
        let key = event
            .episode_key()
            .unwrap_or_else(|| panic!("{} must be episode-routed", event.topic()));
        assert_eq!(key.family, family, "{} family drifted", event.topic());
        assert_eq!(key.conn, conn, "{} conn slot drifted", event.topic());
    }
}

#[test]
fn guard_rest_recovered_variants_are_resolve_edges() {
    for (event, _, _, is_resolve) in routed_rest_events() {
        let expected = if is_resolve {
            EpisodeRole::Resolve
        } else {
            EpisodeRole::Open
        };
        assert_eq!(
            event.episode_role(),
            expected,
            "{} role drifted",
            event.topic()
        );
    }
}

#[test]
fn guard_rest_slot_map_per_symbol_and_unknown_catch_all() {
    // The pinned per-symbol slots: NIFTY=8, BANKNIFTY=9, SENSEX=10,
    // INDIA VIX=11; anything else shares the honest catch-all 15.
    for (symbol, slot) in [
        ("NIFTY", 8_u8),
        ("BANKNIFTY", 9),
        ("SENSEX", 10),
        ("INDIA VIX", 11),
        ("MIDCPNIFTY", 15),
        ("", 15),
    ] {
        let event = NotificationEvent::Spot1mSidNotServed {
            symbol: symbol.to_string(),
            consecutive_minutes: 10,
        };
        let key = event
            .episode_key()
            .unwrap_or_else(|| panic!("Spot1mSidNotServed({symbol}) must be routed"));
        assert_eq!(key.conn, slot, "slot for {symbol:?} drifted");
    }
}

#[test]
fn guard_chain_slot_map_per_underlying_and_distinct_catch_all() {
    // F1 (2026-07-15 fix round): the CHAIN not-served slots are the spot
    // slot + 4 (NIFTY=12, BANKNIFTY=13, SENSEX=14); everything else —
    // including INDIA VIX, const-asserted out of every chain leg upstream
    // — shares the chain catch-all 7, DISTINCT from the spot catch-all 15.
    for (underlying, slot) in [
        ("NIFTY", 12_u8),
        ("BANKNIFTY", 13),
        ("SENSEX", 14),
        ("INDIA VIX", 7),
        ("MIDCPNIFTY", 7),
        ("", 7),
    ] {
        for (label, key) in [
            (
                "Chain1mUnderlyingNotServed",
                NotificationEvent::Chain1mUnderlyingNotServed {
                    underlying,
                    empty_minutes: 10,
                }
                .episode_key(),
            ),
            (
                "GrowwChain1mUnderlyingNotServed",
                NotificationEvent::GrowwChain1mUnderlyingNotServed {
                    underlying,
                    empty_minutes: 10,
                }
                .episode_key(),
            ),
        ] {
            let key = key.unwrap_or_else(|| panic!("{label}({underlying}) must be routed"));
            assert_eq!(
                key.conn, slot,
                "{label} chain slot for {underlying:?} drifted"
            );
        }
    }
}

#[test]
fn guard_spot_and_chain_slot_sets_are_disjoint() {
    // F1 DISJOINTNESS pin (the regression itself, not just the literals):
    // the spot leg's per-symbol recovery must NEVER share an EpisodeKey
    // conn with the chain leg's not-served bubble — a shared slot lets a
    // spot recovery green-close a still-open chain incident (Rule-11
    // false recovery; the chain emit is edge-latched upstream so it never
    // re-fires). Probe every pinned symbol PLUS unknown catch-alls and
    // assert the two conn sets do not intersect anywhere.
    let probes = [
        "NIFTY",
        "BANKNIFTY",
        "SENSEX",
        "INDIA VIX",
        "MIDCPNIFTY",
        "",
    ];
    let spot_slots: std::collections::BTreeSet<u8> = probes
        .iter()
        .map(|s| {
            NotificationEvent::Spot1mSidNotServed {
                symbol: (*s).to_string(),
                consecutive_minutes: 10,
            }
            .episode_key()
            .expect("spot not-served must be routed")
            .conn
        })
        .collect();
    let chain_slots: std::collections::BTreeSet<u8> = probes
        .iter()
        .flat_map(|s| {
            [
                NotificationEvent::Chain1mUnderlyingNotServed {
                    underlying: s,
                    empty_minutes: 10,
                }
                .episode_key()
                .expect("dhan chain not-served must be routed")
                .conn,
                NotificationEvent::GrowwChain1mUnderlyingNotServed {
                    underlying: s,
                    empty_minutes: 10,
                }
                .episode_key()
                .expect("groww chain not-served must be routed")
                .conn,
            ]
        })
        .collect();
    let overlap: Vec<u8> = spot_slots.intersection(&chain_slots).copied().collect();
    assert!(
        overlap.is_empty(),
        "spot and chain not-served slot sets must be DISJOINT — overlap: {overlap:?}"
    );
    // The whole-leg slots (0..=2) must not collide with either set.
    for leg in 0..=2_u8 {
        assert!(
            !spot_slots.contains(&leg) && !chain_slots.contains(&leg),
            "whole-leg slot {leg} collides with a per-symbol slot set"
        );
    }
}

#[test]
fn guard_open_and_resolve_share_one_bubble_key_per_leg() {
    // The whole point of the fold: the Degraded and its Recovered twin
    // MUST share one EpisodeKey, or the recovery lands in a fresh bubble.
    let events = routed_rest_events();
    for pair in events.chunks(2) {
        let [(open, ..), (resolve, ..)] = pair else {
            panic!("routing table must stay Open/Resolve paired");
        };
        assert_eq!(
            open.episode_key(),
            resolve.episode_key(),
            "{} / {} must share one bubble key",
            open.topic(),
            resolve.topic()
        );
    }
}

// ---------------------------------------------------------------------------
// Legacy-lane pins
// ---------------------------------------------------------------------------

#[test]
fn guard_once_per_day_rest_pages_stay_legacy() {
    for event in legacy_once_per_day_events() {
        assert!(
            event.episode_key().is_none(),
            "{} has no recovery edge — it must stay on the legacy lane",
            event.topic()
        );
    }
}

#[test]
fn guard_token_criticals_stay_legacy_forever() {
    // The noise-lock family-(3) once-per-episode Critical page is
    // byte-unchanged: Critical never episode-folds.
    for event in [
        NotificationEvent::AuthenticationFailed {
            reason: m("terminal re-mint failure"),
        },
        NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: m("renewal loop halted"),
        },
    ] {
        assert_eq!(event.severity(), Severity::Critical);
        assert!(
            event.episode_key().is_none(),
            "{} is a family-(3) token Critical — legacy lane forever",
            event.topic()
        );
    }
}

#[test]
fn guard_no_routed_rest_event_is_critical_and_first_page_severity_shape() {
    // SMS fires on the FIRST page only because the Open edges are High
    // (the SMS gate is >= High) and every Resolve edge is Info — a
    // severity drift here silently changes the paging contract.
    for (event, _, _, is_resolve) in routed_rest_events() {
        assert!(
            event.severity() < Severity::Critical,
            "{} must stay below Critical — a Critical would exercise the \
             untested escalation edge; revisit the routing first",
            event.topic()
        );
        if is_resolve {
            assert_eq!(
                event.severity(),
                Severity::Info,
                "{} resolve edge must stay Info (no SMS)",
                event.topic()
            );
        } else {
            assert_eq!(
                event.severity(),
                Severity::High,
                "{} open edge must stay High (pages + SMS once)",
                event.topic()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Family surface pins (snapshot round-trip, badge, phrasing, config)
// ---------------------------------------------------------------------------

#[test]
fn guard_rest_family_snapshot_labels_round_trip() {
    for (family, label) in [
        (EpisodeFamily::DhanRest, "dhan_rest"),
        (EpisodeFamily::GrowwRest, "groww_rest"),
    ] {
        assert_eq!(family.snapshot_label(), label);
        assert_eq!(EpisodeFamily::from_snapshot_label(label), Some(family));
    }
}

#[test]
fn guard_rest_family_badges_and_descriptions_name_the_broker() {
    assert!(EpisodeFamily::DhanRest.badge().contains("DHAN"));
    assert!(EpisodeFamily::GrowwRest.badge().contains("GROWW"));
    assert_eq!(
        EpisodeFamily::DhanRest.feed_desc(),
        "Per-minute candle pull"
    );
    assert_eq!(
        EpisodeFamily::GrowwRest.feed_desc(),
        "Groww per-minute candle pull"
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
    assert!(EpisodeFamily::GrowwRest.down_stale_expiry_exempt());
    // The fold-fed families keep expiring (unchanged semantics).
    assert!(!EpisodeFamily::MainFeedWs.down_stale_expiry_exempt());
    assert!(!EpisodeFamily::OrderUpdateWs.down_stale_expiry_exempt());
    assert!(!EpisodeFamily::GrowwFeed.down_stale_expiry_exempt());
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
    // G2 (fix round 2): the per-slot bubble qualifier must stay lockstep
    // with the events.rs slot table (rest_slot / chain_rest_slot) — a
    // drifted label would mis-name which pull an edit/close refers to.
    for family in [EpisodeFamily::DhanRest, EpisodeFamily::GrowwRest] {
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
    assert_eq!(EpisodeFamily::GrowwFeed.slot_desc(0), "");
}

#[test]
fn guard_rest_family_config_is_default() {
    // EpisodeConfig carries no PartialEq — Debug-compare pins the values.
    let default = format!("{:?}", EpisodeConfig::default());
    for family in [EpisodeFamily::DhanRest, EpisodeFamily::GrowwRest] {
        assert_eq!(
            format!("{:?}", episode_config_for(family)),
            default,
            "{family:?} must keep the default episode timing knobs"
        );
    }
}
