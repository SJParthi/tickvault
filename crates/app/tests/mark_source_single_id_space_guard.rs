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
//! no longer depends on a second broker existing to point at.
/// The single wiring site must hand the tap to exactly one executor.
///
/// REWRITTEN 2026-08-21. This once proved "only one" by locating the SECOND
/// executor and asserting it did NOT receive the tap. With one executor left
/// there is nothing to point at, so the property is now checked by COUNTING
/// hand-offs in the boot's production region: exactly one, wherever it goes.
/// That is the form that survives a future second executor being added —
/// the negative-window form would simply stop being written.
#[test]
fn exactly_one_executor_receives_the_mark_tap() {
    let boot = include_str!("../src/cadence_boot.rs");
    let prod = boot.split("#[cfg(test)]").next().unwrap_or_default();

    let dhan_start = prod
        .find("DhanCadenceExecutor::new(")
        .expect("Dhan executor construction present");
    let dhan_args = &prod[dhan_start..];
    let dhan_args = &dhan_args[..dhan_args
        .find("leg_identity_index")
        .expect("the executor arg list ends at leg_identity_index")];
    assert!(
        dhan_args.contains("mark_forwarder,"),
        "the Dhan executor must receive the mark tap — without it the paper \
         book and risk engine run unmarked with no error anywhere"
    );

    // Exactly ONE hand-off in the whole production region. A second marking
    // broker files one instrument under two keys, which fails silently.
    assert_eq!(
        prod.matches("mark_forwarder,").count(),
        1,
        "exactly one executor may receive the mark tap; two marking brokers \
         means one instrument filed under two keys, which fails silently"
    );
}

/// There must be exactly one forwarder parameter to give away.
///
/// The old signature named it `groww_mark_forwarder`. A broker-neutral name
/// is not cosmetic: a broker-prefixed parameter invites a second one beside
/// it, and two parameters is the shape that makes "both brokers mark"
/// expressible in the first place.
#[test]
fn there_is_only_one_forwarder_to_hand_out() {
    let boot = include_str!("../src/cadence_boot.rs");
    let prod = boot.split("#[cfg(test)]").next().unwrap_or_default();
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
