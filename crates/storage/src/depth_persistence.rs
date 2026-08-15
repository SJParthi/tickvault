//! `market_depth` table — ONE COMMON table for depth-20 AND depth-200.
//!
//! ## Why one table, and why that is the dangerous part
//!
//! Operator directive 2026-08-15 (verbatim in
//! `.claude/rules/project/websocket-connection-scope-lock.md`, section
//! "2026-08-15 (SAME DAY, SECOND QUOTE)"): depth-20 and depth-200 are to be
//! *"shwon and vsisibil in one common atbek … we need all of them eevry ticks …
//! we cnanot miss or hdi or wipe fof nayhtign"*. Every level of both pools,
//! both sides, every update. Sampling, top-N and derived aggregates were
//! offered and explicitly rejected.
//!
//! A single table holding both pools is only safe if the DEDUP key
//! distinguishes them, and that is not obvious. Both feeds emit a **level 5
//! bid for the same instrument at the same second**, from different sockets,
//! and those are DIFFERENT observations of different books. A key without a
//! depth-kind discriminator makes them collide, and QuestDB's UPSERT semantics
//! mean one silently overwrites the other — the exact "wipe off" the directive
//! forbids, produced by the SCHEMA rather than by any code path. Hence
//! [`DEDUP_KEY_MARKET_DEPTH`] carries `depth_kind`, and
//! `depth_dedup_key_separates_the_two_pools` fails the build if it is removed.
//!
//! ## Row shape: one row per LEVEL
//!
//! A depth-200 bid packet becomes 200 rows, not one row with 200 columns. That
//! is deliberate:
//!
//! * QuestDB has no array type, so 200 columns would be the alternative — and
//!   a 20-level packet would then leave 180 of them NULL, making the two pools
//!   structurally different rows in a table whose whole point is that they are
//!   not.
//! * `level` as a column means "the top 5 of everything" is
//!   `WHERE level <= 5`, and it reads identically for both pools.
//!
//! ## The storage arithmetic, corrected 2026-08-15
//!
//! An earlier version of this note said ~70 GB/day. **That was wrong by 3.4×**,
//! and the error is worth recording because it is the easiest one to make here:
//! it multiplied the per-second rate by **86,400 seconds**, a full 24-hour day.
//! Depth frames only arrive while the sockets are up, which is the persistence
//! window `TICK_PERSIST_START_SECS_OF_DAY_IST`..`TICK_PERSIST_END_SECS_OF_DAY_IST`
//! — 09:00 to 15:40 IST, **24,000 seconds**. A market-data volume computed over
//! a calendar day rather than a session is inflated by the 62,400 seconds the
//! exchange is shut.
//!
//! Row width, derived rather than guessed: 4 SYMBOL columns (`feed`, `segment`,
//! `depth_kind`, `side`) at 4 B of interned key each = 16 B, plus 7 eight-byte
//! columns (`security_id`, `level`, `price`, `quantity`, `orders`,
//! `capture_seq`, `ts`) = 56 B. **72 B/row.**
//!
//! | Pool | Instruments | Rows/update | At 1 update/s | At 5 updates/s |
//! |---|---|---|---|---|
//! | depth-20 | 250 | 40 | 17.3 GB/day | 86 GB/day |
//! | depth-200 | 5 | 400 | 3.5 GB/day | 17 GB/day |
//! | **Total** | | | **≈ 21 GB/day** | **≈ 104 GB/day** |
//!
//! Against a 100 GB root that is ~4.8 days at the low estimate, not the ~1.4
//! the wrong figure implied. The update rate is still **Assumed** and remains
//! the 5× swing; the first live session measures it.
//!
//! FLAGGED, not taken: `level` (≤200), `quantity` and `orders` are `u32` on the
//! wire and are stored as `LONG`, costing 12 B/row — a ~17% saving is available
//! by declaring them `INT`. Not done here because no ILP-written `INT` column
//! exists anywhere in this crate to copy, and an unverified i64→INT coercion on
//! the write path that must not fail is a worse trade than 17% of disk. Measure
//! it against a live QuestDB first.
//!
//! See the rule-file section for the same-day S3 archival that keeps "nothing
//! is dropped" true regardless of which end of that range the feed lands on.
//!
//! ## Honest: `ts` is ARRIVAL time, not exchange time
//!
//! The 12-byte depth header carries `message_length`, `feed_code`,
//! `exchange_segment`, `security_id` and a `sequence_or_row_count` — and **no
//! timestamp** (`crates/core/src/parser/depth.rs::DepthPacketHeader`). Unlike
//! `ticks`, which stamps `ts` from the exchange's own `last_trade_time`, this
//! table's `ts` is when the frame reached US. Anyone comparing depth
//! timestamps against tick timestamps is comparing two different clocks, and
//! the difference is the delivery lag — measured on 2026-07-06 at p99 46 s.
//! Stated here because a reader who assumes exchange time will silently draw
//! wrong conclusions about book state at a given instant.
//!
//! ## `capture_seq` is in the key for the same reason it is in `ticks`
//!
//! Several snapshots of one instrument can arrive inside one second. Without
//! the intra-second tiebreaker every one but the last collapses.
//!
//! The value is **derived from the WAL frame sequence, not minted** —
//! `ws_frame_spill::packet_capture_seq(frame.seq, packet_index)`, narrowed by
//! `capture_seq_from_frame_seq`. Deriving rather than minting is what makes a
//! WAL-replayed depth frame collapse onto its original rows instead of landing
//! as a second copy; a freshly-minted sequence could not do that. The
//! packet-index component is what keeps two packets for the same
//! `(security_id, segment, side)` **inside one frame** distinct — a frame
//! stacks packets, so without it all eight key columns would match and one
//! would be upserted away.
//!
//! *(Corrected 2026-08-15. This paragraph previously said the sequence was
//! "minted by `tick_persistence::next_capture_seq` — the SAME process-global
//! counter the tick path uses … so a depth row and a tick row can never mint
//! the same value". Every clause of that was wrong: different atomic, not
//! minted at all, and no cross-table uniqueness claim is needed since `ticks`
//! and `market_depth` are separate tables with separate keys. It is recorded
//! rather than quietly rewritten because an auditor checking the intra-frame
//! collision would have read that paragraph and concluded it was handled.)*

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::QUESTDB_TABLE_MARKET_DEPTH;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::sanitize::sanitize_ilp_symbol;
use tickvault_common::segment::segment_code_to_str;

/// The `market_depth` table name.
pub const MARKET_DEPTH_TABLE: &str = QUESTDB_TABLE_MARKET_DEPTH;

/// The `market_depth` DEDUP UPSERT key.
///
/// Declared as a `DEDUP_KEY_*` **const** rather than an inline literal because
/// `crates/storage/tests/dedup_segment_meta_guard.rs` discovers keys by
/// scanning for that name pattern; an inline literal would put this key
/// OUTSIDE the guard that proves `segment` is present (I-P1-11).
///
/// Why each column is in the key:
///
/// * `ts` — designated timestamp, mandatory in every QuestDB DEDUP clause.
/// * `security_id` + `segment` — Dhan reuses one numeric id across segments,
///   so the pair is the only unique instrument identity (I-P1-11).
/// * **`depth_kind`** — the load-bearing one. A depth-20 level 5 and a
///   depth-200 level 5 for the same instrument-second are DIFFERENT
///   observations from DIFFERENT sockets. Without this column one silently
///   overwrites the other.
/// * `side` — bid and ask arrive as separate packets; level 5 exists on both.
/// * `level` — 200 rows per packet; the level number is what separates them.
/// * `capture_seq` — the intra-second tiebreaker. Several snapshots of one
///   book can arrive inside one second and each is a real observation.
/// * `feed` — a Dhan observation and a (future) other-feed observation of the
///   same book are distinct, never duplicates (operator override 2026-06-28).
pub const DEDUP_KEY_MARKET_DEPTH: &str =
    "ts, security_id, segment, depth_kind, side, level, capture_seq, feed";

/// `depth_kind` SYMBOL value for the 20-level feed.
pub const DEPTH_KIND_20: &str = "d20";

/// `depth_kind` SYMBOL value for the 200-level feed.
pub const DEPTH_KIND_200: &str = "d200";

/// `side` SYMBOL value for the buy side (feed response code 41).
pub const DEPTH_SIDE_BID: &str = "bid";

/// `side` SYMBOL value for the sell side (feed response code 51).
pub const DEPTH_SIDE_ASK: &str = "ask";

/// Timeout for the idempotent QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Returns the complete `market_depth` DEDUP UPSERT key.
///
/// Exposed so gap-enforcement tests can assert the discriminator without
/// reaching into the constant directly.
#[must_use]
pub fn depth_dedup_key() -> &'static str {
    DEDUP_KEY_MARKET_DEPTH
}

// ---------------------------------------------------------------------------
// Row
// ---------------------------------------------------------------------------

/// One depth level, prepared for ILP append.
///
/// Plain data, `Copy`, no heap. Built on the drain's hot path once per level,
/// so it deliberately borrows nothing and allocates nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthRow {
    /// Dhan SecurityId.
    pub security_id: i64,
    /// Exchange segment wire label (`IDX_I`, `NSE_FNO`, …).
    pub segment: &'static str,
    /// `d20` or `d200` — see [`DEDUP_KEY_MARKET_DEPTH`] for why this is in the
    /// key.
    pub depth_kind: &'static str,
    /// `bid` or `ask`.
    pub side: &'static str,
    /// 1-based level, best price first. 1..=20 for depth-20, 1..=200 for
    /// depth-200.
    pub level: i64,
    /// Level price. `f64` on the wire — depth prices are NOT the `f32` the
    /// live market feed uses (`full-market-depth.md` rule 4), so there is no
    /// widening artifact to clean here.
    pub price: f64,
    /// Level quantity.
    pub quantity: i64,
    /// Number of orders resting at this level.
    pub orders: i64,
    /// Intra-second tiebreaker; see the module docs.
    pub capture_seq: i64,
    /// Designated timestamp — ARRIVAL time, not exchange time. See module docs.
    pub ts_nanos: i64,
}

// ---------------------------------------------------------------------------
// DDL (idempotent self-heal)
// ---------------------------------------------------------------------------

/// The idempotent `CREATE TABLE` DDL for `market_depth`. Pure — no I/O.
///
/// `PARTITION BY HOUR` rather than by day, deliberately: at the recorded
/// ~70–350 GB/day this table's partitions are the archival unit, and an hourly
/// partition is what makes same-day archive → verify → drop possible without
/// dropping a partition that is still being written.
#[must_use]
pub fn market_depth_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {MARKET_DEPTH_TABLE} (\
            feed SYMBOL, \
            segment SYMBOL, \
            depth_kind SYMBOL, \
            side SYMBOL, \
            security_id LONG, \
            level LONG, \
            price DOUBLE, \
            quantity LONG, \
            orders LONG, \
            capture_seq LONG, \
            ts TIMESTAMP\
        ) TIMESTAMP(ts) PARTITION BY HOUR WAL"
    )
}

/// Every `market_depth` column with its type, for the per-column self-heal
/// ALTERs.
const MARKET_DEPTH_COLUMNS: &[(&str, &str)] = &[
    ("feed", "SYMBOL"),
    ("segment", "SYMBOL"),
    ("depth_kind", "SYMBOL"),
    ("side", "SYMBOL"),
    ("security_id", "LONG"),
    ("level", "LONG"),
    ("price", "DOUBLE"),
    ("quantity", "LONG"),
    ("orders", "LONG"),
    ("capture_seq", "LONG"),
];

/// The ordered DDL statements [`ensure_market_depth_table`] issues:
/// CREATE → per-column `ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`.
/// Never a DROP. Pure, so the statement set is unit-testable without QuestDB.
#[must_use]
pub fn market_depth_ensure_statements() -> Vec<String> {
    let mut out = vec![market_depth_create_ddl()];
    for (col, ty) in MARKET_DEPTH_COLUMNS {
        out.push(format!(
            "ALTER TABLE {MARKET_DEPTH_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty}"
        ));
    }
    out.push(format!(
        "ALTER TABLE {MARKET_DEPTH_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_MARKET_DEPTH})"
    ));
    out
}

/// Idempotently self-heals the `market_depth` schema. Best-effort: failures
/// log and continue, they never block boot.
///
/// A failed ensure leaves the table to be auto-created by the first ILP write
/// WITHOUT the DEDUP keys. For this table that is worse than for `ticks`: the
/// missing key is the depth-kind discriminator, so the two pools would begin
/// overwriting each other's levels — the failure mode this module exists to
/// prevent. The consequence is named in the log rather than left implicit.
// TEST-EXEMPT: live-QuestDB DDL runner; the statement set is unit-tested via market_depth_ensure_statements() (kept on ONE line — the guard reads only the line immediately above).
pub async fn ensure_market_depth_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            metrics::counter!("tv_depth_persist_errors_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                stage = "ensure_client_build",
                ?err,
                "market_depth table not ensured — HTTP client build failed; the first \
                 ILP write may auto-create the table WITHOUT the depth_kind DEDUP key, \
                 which makes depth-20 and depth-200 overwrite each other's levels"
            );
            return;
        }
    };
    for ddl in &market_depth_ensure_statements() {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                metrics::counter!("tv_depth_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    stage = "ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "market_depth DDL returned non-2xx — the depth_kind DEDUP key may be \
                     missing, which makes the two depth pools overwrite each other"
                );
            }
            Err(err) => {
                metrics::counter!("tv_depth_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "market_depth DDL request failed"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// ILP-over-HTTP conf: per-flush server ACK with `retry_timeout=0` (the caller
/// owns retry cadence) and a bounded `request_timeout` so a hung flush cannot
/// wedge the drain.
fn depth_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Publish a zero on the drop series before any row can be written.
///
/// The CloudWatch agent computes a counter's alarm value as the DELTA between
/// consecutive samples and DROPS the first sample of a series it has never
/// seen. `tv_depth_rows_dropped_total` increments only when buffered rows are
/// discarded, so without this the first drop episode IS the baseline sample:
/// it publishes no datapoint and any alarm on it stays silent through the very
/// episode it exists to catch. Same discipline as `TickWriter`'s
/// `register_drop_baseline`.
fn register_depth_drop_baseline(feed: Feed) {
    metrics::counter!("tv_depth_rows_dropped_total", "feed" => feed.as_str()).increment(0);
}

/// Batched `market_depth` ILP writer.
///
/// Lazy: an unreachable QuestDB at construction still builds (rows buffer
/// locally). `flush` returns `Err` and the pending buffer is discarded LOUDLY,
/// so one poisoned row cannot wedge the rest of the session.
pub struct DepthWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
    feed: Feed,
    /// Rows discarded across this writer's lifetime — the honest counterpart
    /// to "nothing is dropped". If this is non-zero, something WAS dropped and
    /// the operator is told rather than reassured.
    dropped: u64,
}

impl DepthWriter {
    /// Production constructor — ILP-over-HTTP, lazy on connect failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor; the lazy-build contract and
    // every append/flush path are covered via for_test().
    pub fn new(config: &QuestDbConfig, feed: Feed) -> Self {
        register_depth_drop_baseline(feed);
        match Sender::from_conf(depth_ilp_http_conf(config)) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                    feed,
                    dropped: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    feed = feed.as_str(),
                    "market_depth writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                    feed,
                    dropped: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer with an empty buffer.
    ///
    /// Registers the same drop baseline as [`DepthWriter::new`], deliberately:
    /// `crates/app`'s tests construct this across the crate boundary where
    /// `cfg(test)` does not reach, so a registration that lived only in `new`
    /// would be a real bypass rather than a tidiness question.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test(feed: Feed) -> Self {
        register_depth_drop_baseline(feed);
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
            feed,
            dropped: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Rows discarded on failed flushes over this writer's lifetime.
    #[must_use]
    // TEST-EXEMPT: observability accessor, asserted by the discard tests below.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Raw ILP buffer text — the ONLY way a caller can assert what was actually
    /// emitted.
    ///
    /// Deliberately `pub` and cross-crate visible (it was `#[cfg(test)]` and
    /// storage-private until 2026-08-15). The mapping that turns a parsed
    /// packet into `side` / `depth_kind` symbols lives in the APP crate's
    /// `drain_depth_frame`, so a storage-private accessor could never let a
    /// test see it — and a 2026-08-15 test audit found exactly that gap: swap
    /// the bid and ask arms and the book inverts silently while every existing
    /// assertion (`out.rows == 40`) still passes. An inverted book is the
    /// worst-consequence bug this protocol has.
    ///
    /// Exposes nothing sensitive: the buffer holds only market data, and every
    /// symbol in it is a closed `&'static str` set.
    #[must_use]
    // Its whole purpose is to be asserted against, and it is — by the append
    // tests below and by the app-crate drain tests that could not otherwise
    // see their own label mapping.
    // TEST-EXEMPT: observability accessor, asserted by the tests it enables.
    pub fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one prepared [`DepthRow`] to the ILP buffer (no flush).
    ///
    /// ILP requires every SYMBOL before any field column, so the four symbols
    /// are written first. All four come from closed `&'static str` sets, and
    /// all four are still routed through `sanitize_ilp_symbol` as defence in
    /// depth against line-protocol injection — the same discipline `ticks`
    /// applies to its own closed sets.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, row: &DepthRow) -> Result<()> {
        let feed = self.feed.as_str();
        self.buffer
            .table(MARKET_DEPTH_TABLE)
            .context("table")?
            .symbol("segment", sanitize_ilp_symbol(row.segment).as_ref())
            .context("segment")?
            .symbol("depth_kind", sanitize_ilp_symbol(row.depth_kind).as_ref())
            .context("depth_kind")?
            .symbol("side", sanitize_ilp_symbol(row.side).as_ref())
            .context("side")?
            .symbol("feed", sanitize_ilp_symbol(feed).as_ref())
            .context("feed")?
            .column_i64("security_id", row.security_id)
            .context("security_id")?
            .column_i64("level", row.level)
            .context("level")?
            .column_f64("price", row.price)
            .context("price")?
            .column_i64("quantity", row.quantity)
            .context("quantity")?
            .column_i64("orders", row.orders)
            .context("orders")?
            .column_i64("capture_seq", row.capture_seq)
            .context("capture_seq")?
            .at(TimestampNanos::new(row.ts_nanos))
            .context("designated timestamp")?;

        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Discards the pending buffer, counting and logging the loss.
    ///
    /// Returns how many rows were discarded.
    fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            self.buffer.clear();
            self.pending = 0;
            self.dropped = self.dropped.saturating_add(dropped as u64);
            metrics::counter!("tv_depth_rows_dropped_total", "feed" => self.feed.as_str())
                .increment(dropped as u64);
        }
        dropped
    }

    /// Flushes buffered rows over ILP-HTTP with a per-flush server ACK.
    ///
    /// On ANY failed flush the pending buffer is DISCARDED: a server-rejected
    /// row retained across flushes would be re-sent forever and block every
    /// later row. The discard is LOUD — counter + `error!` — because this
    /// table's whole premise is that nothing is silently lost, and a silent
    /// discard here would make that premise false while looking healthy.
    ///
    /// # Errors
    /// `Err` when disconnected or when the HTTP flush fails.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            let dropped = self.discard_pending();
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                feed = self.feed.as_str(),
                dropped,
                "market_depth flush with no QuestDB connection — {dropped} depth row(s) \
                 DISCARDED. These levels are gone from the table; the raw frames remain \
                 in the write-ahead log"
            );
            anyhow::bail!("market_depth writer disconnected; {dropped} row(s) discarded");
        };
        match sender.flush(&mut self.buffer) {
            Ok(()) => {
                self.pending = 0;
                Ok(())
            }
            Err(err) => {
                let dropped = self.discard_pending();
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    feed = self.feed.as_str(),
                    dropped,
                    ?err,
                    "market_depth flush FAILED — {dropped} depth row(s) DISCARDED so one \
                     rejected row cannot wedge the session. These levels are gone from the \
                     table; the raw frames remain in the write-ahead log"
                );
                Err(anyhow::anyhow!(err)).context(format!(
                    "market_depth flush failed; {dropped} row(s) discarded"
                ))
            }
        }
    }
}

/// Maps a numeric exchange-segment code to its wire label, for callers that
/// hold the raw byte from the depth header.
///
/// Returns `None` for an unknown code — a typed refusal, never a guess. A
/// guessed segment would produce a row under the WRONG instrument identity
/// (I-P1-11), which is worse than no row at all.
///
/// The shared [`segment_code_to_str`] returns the literal `"UNKNOWN"` for an
/// unrecognised code, which is the right answer for a log line and the WRONG
/// one for a DEDUP key: every unmappable instrument would share one segment
/// bucket, so two different instruments could collide on
/// `(security_id, "UNKNOWN")` and overwrite each other's levels. This wrapper
/// converts that sentinel into a refusal so the caller must decide, and the
/// caller's decision is to count-and-skip rather than write a poisoned row.
#[must_use]
pub fn depth_segment_label(code: u8) -> Option<&'static str> {
    match segment_code_to_str(code) {
        "UNKNOWN" => None,
        label => Some(label),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> DepthRow {
        DepthRow {
            security_id: 13,
            segment: "IDX_I",
            depth_kind: DEPTH_KIND_20,
            side: DEPTH_SIDE_BID,
            level: 1,
            price: 24_500.25,
            quantity: 750,
            orders: 12,
            capture_seq: 1,
            ts_nanos: 1_700_000_000_000_000_000,
        }
    }

    // -- the load-bearing key assertion -------------------------------------

    #[test]
    fn depth_dedup_key_separates_the_two_pools() {
        // Without `depth_kind` a depth-20 level 5 and a depth-200 level 5 for
        // the same instrument-second collide and one silently overwrites the
        // other. That is the failure this whole module exists to prevent, so
        // it is asserted before anything else.
        assert!(
            DEDUP_KEY_MARKET_DEPTH.contains("depth_kind"),
            "the DEDUP key MUST carry depth_kind or the two depth pools overwrite \
             each other: {DEDUP_KEY_MARKET_DEPTH}"
        );
    }

    #[test]
    fn depth_dedup_key_carries_the_i_p1_11_composite() {
        assert!(DEDUP_KEY_MARKET_DEPTH.contains("security_id"));
        assert!(
            DEDUP_KEY_MARKET_DEPTH.contains("segment"),
            "security_id alone is not unique (I-P1-11)"
        );
    }

    #[test]
    fn depth_dedup_key_starts_with_the_designated_timestamp() {
        // QuestDB rejects a DEDUP clause whose key omits the designated
        // timestamp (the 2026-05-18 HTTP-400 production regression).
        assert!(DEDUP_KEY_MARKET_DEPTH.starts_with("ts,"));
    }

    #[test]
    fn depth_dedup_key_separates_side_level_and_intra_second_snapshots() {
        for col in ["side", "level", "capture_seq", "feed"] {
            assert!(
                DEDUP_KEY_MARKET_DEPTH.contains(col),
                "missing {col} from the key collapses distinct observations"
            );
        }
    }

    #[test]
    fn depth_dedup_key_accessor_matches_the_constant() {
        assert_eq!(depth_dedup_key(), DEDUP_KEY_MARKET_DEPTH);
    }

    // -- DDL ----------------------------------------------------------------

    #[test]
    fn market_depth_create_ddl_is_idempotent_and_hour_partitioned() {
        let ddl = market_depth_create_ddl();
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS market_depth"));
        assert!(ddl.contains("TIMESTAMP(ts) PARTITION BY HOUR WAL"));
    }

    #[test]
    fn market_depth_create_ddl_declares_every_dedup_key_column() {
        let ddl = market_depth_create_ddl();
        for col in DEDUP_KEY_MARKET_DEPTH.split(',').map(str::trim) {
            assert!(
                ddl.contains(col),
                "DEDUP key names {col} but the CREATE TABLE does not declare it"
            );
        }
    }

    #[test]
    fn market_depth_ensure_statements_are_create_then_alters_then_dedup() {
        let stmts = market_depth_ensure_statements();
        assert!(stmts[0].starts_with("CREATE TABLE IF NOT EXISTS market_depth"));
        assert_eq!(stmts.len(), 1 + MARKET_DEPTH_COLUMNS.len() + 1);
        for s in &stmts[1..stmts.len() - 1] {
            assert!(s.contains("ADD COLUMN IF NOT EXISTS"));
        }
        let last = stmts.last().expect("non-empty");
        assert!(last.contains("DEDUP ENABLE UPSERT KEYS"));
        assert!(last.contains("depth_kind"));
    }

    #[test]
    fn market_depth_ensure_statements_never_drop() {
        for s in market_depth_ensure_statements() {
            assert!(
                !s.to_uppercase().contains("DROP"),
                "self-heal must never DROP — SEBI retention: {s}"
            );
        }
    }

    #[test]
    fn every_column_in_the_alter_list_is_in_the_create_ddl() {
        let ddl = market_depth_create_ddl();
        for (col, ty) in MARKET_DEPTH_COLUMNS {
            assert!(ddl.contains(&format!("{col} {ty}")), "{col} {ty} missing");
        }
    }

    // -- writer -------------------------------------------------------------

    #[test]
    fn append_row_writes_all_four_symbols_before_any_field() {
        let mut w = DepthWriter::for_test(Feed::Dhan);
        w.append_row(&row()).expect("append");
        let line = w.buffer_utf8();
        let first_field = line.find(' ').expect("ILP separates symbols from fields");
        let symbols = &line[..first_field];
        for s in ["segment=", "depth_kind=", "side=", "feed="] {
            assert!(symbols.contains(s), "{s} must be a SYMBOL: {line}");
        }
    }

    #[test]
    fn append_row_stamps_the_kind_and_side_it_was_given() {
        let mut w = DepthWriter::for_test(Feed::Dhan);
        let mut r = row();
        r.depth_kind = DEPTH_KIND_200;
        r.side = DEPTH_SIDE_ASK;
        w.append_row(&r).expect("append");
        let line = w.buffer_utf8();
        assert!(line.contains("depth_kind=d200"));
        assert!(line.contains("side=ask"));
    }

    #[test]
    fn append_row_counts_pending_rows() {
        let mut w = DepthWriter::for_test(Feed::Dhan);
        assert_eq!(w.pending(), 0);
        for level in 1..=200 {
            let mut r = row();
            r.level = level;
            w.append_row(&r).expect("append");
        }
        assert_eq!(w.pending(), 200, "a depth-200 side is 200 rows, not one");
    }

    #[test]
    fn a_disconnected_flush_discards_loudly_and_counts_the_loss() {
        let mut w = DepthWriter::for_test(Feed::Dhan);
        w.append_row(&row()).expect("append");
        w.append_row(&row()).expect("append");
        let err = w.flush().expect_err("disconnected writer cannot flush");
        assert!(format!("{err}").contains("2 row(s) discarded"));
        assert_eq!(w.pending(), 0, "buffer is cleared so it cannot wedge");
        assert_eq!(w.dropped(), 2, "the loss is COUNTED, never silent");
    }

    #[test]
    fn an_empty_flush_is_a_no_op_and_never_errors() {
        let mut w = DepthWriter::for_test(Feed::Dhan);
        w.flush().expect("nothing pending is not a failure");
        assert_eq!(w.dropped(), 0);
    }

    #[test]
    fn dropped_accumulates_across_flush_failures() {
        let mut w = DepthWriter::for_test(Feed::Dhan);
        w.append_row(&row()).expect("append");
        let _ = w.flush();
        w.append_row(&row()).expect("append");
        let _ = w.flush();
        assert_eq!(w.dropped(), 2);
    }

    // -- segment mapping ----------------------------------------------------

    #[test]
    fn depth_segment_label_refuses_an_unknown_code_rather_than_guessing() {
        // A guessed segment writes the row under the WRONG instrument identity
        // (I-P1-11), which is worse than writing no row.
        assert_eq!(depth_segment_label(200), None);
    }

    #[test]
    fn depth_segment_label_maps_known_codes_to_their_wire_labels() {
        assert_eq!(depth_segment_label(0), Some("IDX_I"));
        assert_eq!(depth_segment_label(2), Some("NSE_FNO"));
    }

    // -- the storage estimate's own premise --------------------------------

    #[test]
    fn the_storage_estimate_is_costed_over_a_session_not_a_calendar_day() {
        // The module docs quote ~21 GB/day. That figure is only true because
        // depth arrives during the PERSISTENCE WINDOW, not around the clock:
        // an earlier version multiplied by 86,400 and was wrong by 3.4×.
        //
        // This test pins the premise rather than the conclusion. If someone
        // widens the window, the docs' number silently becomes an
        // understatement — and the assertion message says so, which is the
        // only way a prose figure can be kept honest by the build.
        let window_secs = u64::from(
            tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST
                - tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST,
        );
        assert_eq!(
            window_secs, 24_000,
            "the depth storage estimate in this module's docs is derived from a \
             24,000-second session (09:00–15:40 IST). The window is now \
             {window_secs}s, so that figure is stale — recompute it before \
             trusting any capacity or cost decision built on it."
        );
        assert!(
            window_secs < 86_400,
            "costing a market-data stream over a calendar day credits it for \
             every second the exchange is shut — the exact 3.4× error this \
             test exists to stop recurring"
        );
    }

    #[test]
    fn the_row_width_premise_matches_the_declared_columns() {
        // The docs derive 72 B/row as 4 SYMBOL keys (4 B each) + 7 eight-byte
        // columns. If a column is added or retyped, that derivation — and
        // every GB/day figure resting on it — is stale.
        let symbols = MARKET_DEPTH_COLUMNS
            .iter()
            .filter(|(_, ty)| *ty == "SYMBOL")
            .count();
        let wide = MARKET_DEPTH_COLUMNS
            .iter()
            .filter(|(_, ty)| *ty == "LONG" || *ty == "DOUBLE")
            .count();
        // +1 for `ts`, which is the designated timestamp and therefore not in
        // the ALTER column list.
        let derived = symbols * 4 + (wide + 1) * 8;
        assert_eq!(
            (symbols, wide, derived),
            (4, 6, 72),
            "row-width premise drifted: the docs' GB/day table assumes 4 SYMBOL \
             + 7 eight-byte columns = 72 B/row"
        );
    }

    #[test]
    fn the_two_kind_labels_are_distinct_and_stable() {
        assert_ne!(DEPTH_KIND_20, DEPTH_KIND_200);
        assert_eq!(DEPTH_KIND_20, "d20");
        assert_eq!(DEPTH_KIND_200, "d200");
    }

    #[test]
    fn the_two_side_labels_are_distinct_and_stable() {
        assert_ne!(DEPTH_SIDE_BID, DEPTH_SIDE_ASK);
        assert_eq!(DEPTH_SIDE_BID, "bid");
        assert_eq!(DEPTH_SIDE_ASK, "ask");
    }
}
