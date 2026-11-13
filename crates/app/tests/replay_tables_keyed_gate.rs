//! B1 review 2 (2026-11-13): the lane's replay check
//! (`LiveIngest::replay_tables_keyed`, which the boot refold and every WAL
//! catch-up round wait on) reads the REAL process-wide live write gate.
//!
//! Its own binary because arming the gate is process-wide and never undone.
//! Armed with nothing keyed (a boot whose live-table DDL was refused), the
//! check must read not keyed; once `ticks` and `tick_quotes` are keyed it
//! reads keyed, whether or not `feed_aux_packets` is (the tick writer sets
//! those rows aside to the spill on its own).

use tickvault_app::dhan_feed_stack::{DepthIngest, LiveIngest};
use tickvault_common::feed::Feed;
use tickvault_storage::ilp_table_gate::{GatedTable, arm_live_write_gate, mark_table_keyed};
use tickvault_storage::tick_persistence::TickWriter;

#[test]
fn test_replay_check_reads_the_armed_gate_of_this_process() {
    let plain = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    let depth = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_inline_depth(DepthIngest::for_test());
    assert!(
        plain.replay_tables_keyed(),
        "unarmed: a replay posts as before"
    );

    arm_live_write_gate();
    assert!(
        !plain.replay_tables_keyed(),
        "armed with nothing keyed: the refold and catch-up must not post"
    );
    assert!(!depth.replay_tables_keyed());

    mark_table_keyed(GatedTable::Ticks);
    mark_table_keyed(GatedTable::TickQuotes);
    assert!(
        plain.replay_tables_keyed(),
        "ticks and tick_quotes keyed: an unkeyed feed_aux_packets does not hold it"
    );
    assert!(
        !depth.replay_tables_keyed(),
        "the inline depth table is still not keyed"
    );
    mark_table_keyed(GatedTable::MarketDepth);
    assert!(depth.replay_tables_keyed());
}
