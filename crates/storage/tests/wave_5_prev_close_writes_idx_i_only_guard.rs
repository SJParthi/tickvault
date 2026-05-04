//! Wave 5 Item 15 — `previous_close` ILP write IDX_I-only ratchet.
//!
//! Pre-Wave-5, the tick processor wrote rows into the `previous_close`
//! QuestDB table for THREE segments:
//!   - IDX_I via the standalone code-6 PrevClose packet
//!   - NSE_EQ via the Quote packet's `close` field (bytes 38-41)
//!   - NSE_FNO + BSE_FNO via the Full packet's `close` field (bytes 50-53)
//!
//! Per Dhan Ticket #5525125 the Quote/Full `close` field IS the previous
//! trading session's close — it rides every Quote/Full tick. So writing
//! a separate `previous_close` row for those segments was pure
//! redundancy: the in-memory cache feeding `OptionMoversWriter` /
//! `StockMoversWriter` already updates from the same field on every
//! tick, and restart recovery can rehydrate from a single
//! `SELECT ... FROM ticks` against today's last-tick-per-instrument.
//!
//! Wave 5 Item 15 dropped the Quote+Full writes. This guard pins the
//! deletion: `tick_processor.rs` MUST contain at most ONE call to
//! `prev_close_persist::try_record_global`, and that single call MUST
//! sit inside the `exchange_segment_code == 0` (IDX_I) gate using
//! `PrevCloseSource::Code6`.

use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn tick_processor_source() -> String {
    let path = workspace_root()
        .join("crates")
        .join("core")
        .join("src")
        .join("pipeline")
        .join("tick_processor.rs");
    fs::read_to_string(&path).expect("tick_processor.rs must be readable")
}

#[test]
fn test_prev_close_persist_skips_quote_and_full_sources() {
    let src = tick_processor_source();
    let quote_count = src.matches("PrevCloseSource::QuoteClose").count();
    let full_count = src.matches("PrevCloseSource::FullClose").count();
    assert_eq!(
        quote_count, 0,
        "Wave 5 Item 15: PrevCloseSource::QuoteClose must NOT appear in \
         tick_processor.rs — Quote-packet close field rides every tick \
         already and should not be persisted to `previous_close` table. \
         Found {quote_count} occurrences."
    );
    assert_eq!(
        full_count, 0,
        "Wave 5 Item 15: PrevCloseSource::FullClose must NOT appear in \
         tick_processor.rs — Full-packet close field rides every tick \
         already and should not be persisted to `previous_close` table. \
         Found {full_count} occurrences."
    );
}

#[test]
fn test_prev_close_table_only_contains_idx_i_rows() {
    // Source-scan equivalent of "the table only gets IDX_I rows": there
    // is exactly ONE remaining call site for `try_record_global`, it
    // sits inside the `exchange_segment_code == 0` gate, and uses the
    // Code6 source enum.
    let src = tick_processor_source();
    let total_calls = src.matches("prev_close_persist::try_record_global").count();
    assert_eq!(
        total_calls, 1,
        "Wave 5 Item 15: tick_processor.rs must contain exactly 1 \
         `prev_close_persist::try_record_global` call (the IDX_I code-6 \
         path). Found {total_calls}. Adding a new call requires updating \
         this guard explicitly."
    );

    let code6_count = src.matches("PrevCloseSource::Code6").count();
    assert_eq!(
        code6_count, 1,
        "Wave 5 Item 15: the single remaining `try_record_global` call \
         must use `PrevCloseSource::Code6`. Found {code6_count} \
         Code6-tagged sites."
    );

    // Pin the IDX_I gate sits in the same file.
    assert!(
        src.contains("exchange_segment_code == 0"),
        "Wave 5 Item 15: the IDX_I gate `exchange_segment_code == 0` \
         must remain — it is what restricts `previous_close` writes \
         to IDX_I-only."
    );
}

// PR #457 (2026-05-04): test_movers_writer_reads_day_close_from_ticks_via_in_memory_cache
// REMOVED. The legacy in-memory `top_movers` / `option_movers` trackers
// have been deleted in this PR — the unified `/api/movers` endpoint
// reads `movers_5s` mat-view directly (PR #450), and the per-tick
// `update_prev_close` cache hand-off no longer exists. Wave 5 Item 15's
// invariant is now enforced by the QuestDB `movers_1s` ILP path which
// has its own coverage in `crates/storage/tests/movers_persistence_*.rs`.
