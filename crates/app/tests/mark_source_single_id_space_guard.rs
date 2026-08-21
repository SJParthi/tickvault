//! The paper book must be marked from exactly ONE broker.
//!
//! Background, because this guard inverts a previous rule rather than
//! adding a new one. Until 2026-08-21 `cadence_boot.rs` carried the
//! opposite instruction verbatim: *"threaded into the GROWW executor ONLY
//! — the Dhan executor must NEVER carry it (Dhan sids 13/25/51 are a
//! different id space than the Groww-native u64s the paper book keys on;
//! cross-feeding would double-key instruments invisibly to the
//! first-seen-segment tripwire)."*
//!
//! That rule was right, and it is not overruled here — its premise was
//! removed. It described TWO live brokers marking ONE paper book across two
//! id spaces, where the same NIFTY is filed under two different keys and a
//! position opened against one of them is never marked again. The
//! operator's 2026-08-21 directive removes Groww, leaving one id space and
//! nothing to collide with.
//!
//! What survives from the old rule is the invariant underneath it: **one
//! marking broker at a time.** That is what this guard pins, in a form that
//! stays true whichever broker it is. The dangerous state is not "Dhan
//! marks" — it is "both mark", and that state has no compile error, no
//! failing test elsewhere, and no log line. It shows up only as a paper
//! position that quietly stops being valued.

/// The single wiring site must hand the tap to exactly one executor.
#[test]
fn exactly_one_executor_receives_the_mark_tap() {
    let boot = include_str!("../src/cadence_boot.rs");
    let prod = boot.split("#[cfg(test)]").next().unwrap_or_default();

    let dhan_start = prod
        .find("DhanCadenceExecutor::new(")
        .expect("Dhan executor construction present");
    let groww_start = prod
        .find("GrowwCadenceExecutor::new(")
        .expect("Groww executor construction present");
    assert!(
        dhan_start < groww_start,
        "this guard assumes Dhan is constructed first; re-derive the windows if that changes"
    );

    let dhan_args = &prod[dhan_start..groww_start];
    let groww_args = &prod[groww_start..];
    let groww_args = &groww_args[..groww_args
        .find("leg_identity_index")
        .expect("groww arg list ends at leg_identity_index")];

    assert!(
        dhan_args.contains("mark_forwarder,"),
        "the Dhan executor must receive the mark tap"
    );
    assert!(
        !groww_args.contains("mark_forwarder"),
        "the Groww executor must NOT receive the mark tap — two marking brokers means \
         one instrument filed under two keys, which fails silently"
    );
}

/// The Groww lane's refusal must be structural, not a config flag.
#[test]
fn the_groww_lane_cannot_be_re_armed_by_configuration() {
    let boot = include_str!("../src/cadence_boot.rs");
    let prod = boot.split("#[cfg(test)]").next().unwrap_or_default();
    let groww_start = prod
        .find("GrowwCadenceExecutor::new(")
        .expect("Groww executor construction present");
    let window = &prod[groww_start..];
    let window = &window[..window
        .find("leg_identity_index")
        .expect("groww arg list ends at leg_identity_index")];

    // A literal `None` cannot be flipped by editing a TOML file. An
    // `Option` threaded from config could be, and the failure mode of
    // getting that wrong is invisible at runtime.
    assert!(
        window.contains("None,"),
        "the Groww executor must be handed a literal None, so no config edit can re-arm it"
    );
}

/// There must be exactly one forwarder parameter to give away.
#[test]
fn there_is_only_one_forwarder_to_hand_out() {
    let boot = include_str!("../src/cadence_boot.rs");
    let prod = boot.split("#[cfg(test)]").next().unwrap_or_default();

    // The old signature named it `groww_mark_forwarder`. A broker-neutral
    // name is not cosmetic: a broker-prefixed parameter invites a second
    // one beside it, and two parameters is the shape that makes
    // "both brokers mark" expressible in the first place.
    assert!(
        !prod.contains("groww_mark_forwarder"),
        "the parameter must not be broker-prefixed — a broker-named tap invites a second one"
    );
    let count = prod.matches("mark_forwarder: Option<").count();
    assert_eq!(
        count, 1,
        "exactly one mark-forwarder parameter must exist; found {count}"
    );
}
