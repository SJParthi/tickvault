//! The contract-to-underlying map must be published AFTER the subscription
//! selection, with the subscribed contracts ordered first.
//!
//! # Why (MEASURED 2026-09-10)
//!
//! The attach path publishes the map the top-volume ranking reads per tick.
//! Until 2026-09-10 it published BEFORE `select_contract_universe`, in
//! artifact order, and the artifact holds every expiry — 76,890 option legs
//! that day against `MAX_TRACKED_CONTRACTS` = 25,000. `build_snapshot` kept
//! the first 25,000 it met and refused 51,890 as `AtCapacity`. Artifact order
//! is not subscription order, so a contract on the wire could be one of the
//! refused, and the ranking skips an unmapped contract SILENTLY — no counter,
//! no line, no depth socket.
//!
//! These are source-order pins: the ordering function existing is worth
//! nothing if the call site still publishes before it knows what was
//! selected.

const UNIVERSE: &str = include_str!("../src/dhan_contract_universe.rs");

#[test]
fn the_map_is_published_after_the_selection_with_selected_legs_first() {
    let select = UNIVERSE
        .find("select_contract_universe(&rows, &spot, today_ymd, capacity)")
        .expect("the attach path must call select_contract_universe");
    let order = UNIVERSE
        .find("crate::contract_underlying_map::order_selected_first(")
        .expect("the attach path must order the selected legs first");
    let publish = UNIVERSE
        .find(".publish_from_legs(&legs)")
        .expect("the attach path must publish the map");

    assert!(
        select < order,
        "order_selected_first runs before the selection exists — it would be \
         ordering against an empty set, which is the 2026-09-10 defect wearing \
         a new name"
    );
    assert!(
        order < publish,
        "the map is published before the legs are ordered — the cap would again \
         fall on artifact order"
    );
}

#[test]
fn the_pre_selection_rationale_is_gone() {
    // The old block justified itself with this sentence. If it returns, the
    // publish moved back above the selection.
    assert!(
        !UNIVERSE.contains("Before the selection, deliberately"),
        "the map is again published before the selection"
    );
}
