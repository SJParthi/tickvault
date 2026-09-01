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
//!
//! ## ⚠ CORRECTED 2026-08-28 — the table above lists TWO sources; there are THREE
//!
//! `DEPTH_KIND_5` is missing from it, and it is the LARGEST of the three.
//! Added with the 2026-08-19 Full-mode flip, it writes the 5 order-book levels
//! that ride inside **every Full-mode tick packet** — 10 rows per packet (5
//! levels x 2 sides) across the whole subscribed universe, not just the 250
//! instruments the dedicated depth-20 pool covers.
//!
//! The measured session settles the split. 1,530,651,649 rows x 72 B =
//! **110.2 GB**, decomposing as:
//!
//! | Kind | Instruments | Rows per update | Rows/session | GB | Basis |
//! |---|---|---|---:|---:|---|
//! | **d5** | the WHOLE universe (~23,000) | 10 per tick packet | **~643.5 M** | **46.3** | Derived: 64,349,753 ticks x 10 |
//! | d20 | 250 | 10,000 | ~739.3 M | 53.2 | Derived at equal cadence |
//! | d200 | 5 | <= 2,000 | ~147.9 M | 10.6 | Derived; row count is variable, so an upper bound |
//! | **Total** | | | **1,530.65 M** | **110.2** | MEASURED |
//!
//! Why the omission mattered rather than being a tidy-up: the old table's own
//! bounds are 21 GB/day low and 104 GB/day high, so the measured 110.2 GB sits
//! ABOVE the stated ceiling — and a reader reconciling that would have gone
//! looking for a cadence problem in the two pools the table names, when the
//! missing 46.3 GB is a third source that scales with the TICK count rather
//! than with either pool's instrument count. Doubling the depth-20 pool does
//! not move it; widening the subscribed universe does.
//!
//! ## The shed gate's rungs, in gigabytes
//!
//! Worth stating here because the connection is invisible from either side:
//! `ingest_shed::ShedLevel::InlineDepth` gates EXACTLY the `DEPTH_KIND_5`
//! write (`dhan_feed_stack.rs`, the `INGEST_SHED.allows_inline_depth()` arm),
//! and `AllDepth` gates the dedicated pools as well.
//!
//! | Rung | Stops | GB/session |
//! |---|---|---:|
//! | `InlineDepth` | d5 | **46.3** (42% of depth) |
//! | `AllDepth` | d5 + d20 + d200 | **110.2** (80% of the 138 GB session burn) |
//! | any rung | ticks | **never** |
//!
//! So the first rung alone is the single largest lever the box has, it needs
//! no schema change and no scope decision, and it is fully reversible on the
//! next poll. What it lacked was a trigger that fires on a day like
//! 2026-08-28, which closed at 55% free — nowhere near the 15% fractional bar
//! — while roughly one session of writing from a full disk. That is what
//! `ingest_shed::SESSION_BURN_BYTES_DEFAULT` and the runway trigger add.
//!
//! See the rule-file section for the same-day S3 archival that keeps "nothing
//! is dropped" true regardless of which end of that range the feed lands on.
//!
//! ## `ts` is ARRIVAL time in IST — corrected 2026-08-19
//!
//! Operator, 2026-08-19: *"why the market depth ts has utc time it should be
//! the precise ist"*. He was right, and this table was the only one getting it
//! wrong: the caller passed raw `Utc::now()` nanos straight into `ts_nanos`,
//! while `tick_persistence` and `partition_archive` both add
//! `IST_UTC_OFFSET_NANOS` at their own stamping sites.
//!
//! The offset belongs at the stamping site, not at the source: the value the
//! caller holds is deliberately true UTC because `ws_lag_ms` differences it
//! against the vendor's IST exchange stamp converted back to UTC. Shifting it
//! upstream would have corrupted the lag measurement to fix the column.
//!
//! Consequences of the old behaviour, for anyone reading historical rows:
//! depth written before this fix is 5h30m behind every sibling table, and
//! because `ts` is the DESIGNATED timestamp, rows captured between 18:30 and
//! 23:59 IST landed in the PREVIOUS day's partition — the same key the
//! archival and retention paths use.
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

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use crate::tick_spill_replay::SPILL_FILE_EXTENSION;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::QUESTDB_TABLE_MARKET_DEPTH;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
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

/// `depth_kind` SYMBOL value for the 5 levels carried INLINE in every Full-mode
/// tick packet (2026-08-19).
///
/// A THIRD kind, not a variant of the other two, and the DEDUP key is why:
/// `d5`, `d20` and `d200` can all describe level 3 of the same instrument at
/// the same instant from three different sources. Without a distinct label they
/// collide on the key and QuestDB silently upserts one over the others —
/// destroying two of the three observations. The discriminator is what makes a
/// single shared table safe.
///
/// This source is free in bandwidth terms: Full mode is already subscribed and
/// the 5 levels already arrive in every packet. Until this landed, the drain
/// read the price out of the packet and threw the book away.
pub const DEPTH_KIND_5: &str = "d5";

/// `side` SYMBOL value for the buy side (feed response code 41).
pub const DEPTH_SIDE_BID: &str = "bid";

/// `side` SYMBOL value for the sell side (feed response code 51).
pub const DEPTH_SIDE_ASK: &str = "ask";

// COMPILE-TIME proof that every closed-set ILP label is already safe, so the
// write paths can pass them through with ZERO runtime work.
//
// Adding a label with a comma, an equals sign, a control byte or a non-ASCII
// character is a BUILD FAILURE here, not a malformed row discovered in
// QuestDB. That is what makes it sound for `append_row` to skip the sanitiser
// for these values — the check did not disappear, it moved to the compiler.
const _: () = {
    assert!(tickvault_common::sanitize::ilp_symbol_is_clean(
        DEPTH_KIND_20
    ));
    assert!(tickvault_common::sanitize::ilp_symbol_is_clean(
        DEPTH_KIND_200
    ));
    assert!(tickvault_common::sanitize::ilp_symbol_is_clean(
        DEPTH_KIND_5
    ));
    assert!(tickvault_common::sanitize::ilp_symbol_is_clean(
        DEPTH_SIDE_BID
    ));
    assert!(tickvault_common::sanitize::ilp_symbol_is_clean(
        DEPTH_SIDE_ASK
    ));
    // EXHAUSTIVE over the whole byte space: whatever segment code the wire
    // carries, its label is clean. Not a sample — all 256.
    let mut code = 0u16;
    while code <= u8::MAX as u16 {
        assert!(tickvault_common::sanitize::ilp_symbol_is_clean(
            tickvault_common::segment::segment_code_to_str(code as u8)
        ));
        code += 1;
    }
    // Every feed label, from the enum's own exhaustive list.
    let feeds = Feed::ALL;
    let mut i = 0;
    while i < feeds.len() {
        assert!(tickvault_common::sanitize::ilp_symbol_is_clean(
            feeds[i].as_str()
        ));
        i += 1;
    }
};

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

/// The ILP-over-HTTP `request_timeout` in SECONDS, as a number the rest of the
/// workspace can reason with.
///
/// # Why this exists as a constant
///
/// The timeout itself lives inside the `format!` string below, and a source-scan
/// test pins that literal — fine for the conf, useless to anyone who needs the
/// VALUE. The shutdown budget needs the value: the lane's tail flush can block
/// for one full `request_timeout` before the offload join even starts, so
/// `OFFLOAD_SHUTDOWN_GRACE_SECS` plus this number has to fit inside
/// `DHAN_LANE_SHUTDOWN_FLUSH_BUDGET_SECS`.
///
/// Before 2026-08-28 that relationship was asserted against a hardcoded margin
/// of 5, which happened to equal this timeout. Correct — and correct by
/// coincidence: raising the timeout to 10 s would have left the assert passing
/// while the real shutdown ran over its budget and abandoned the tick tail,
/// which is precisely the failure that assert exists to prevent, arriving
/// through the one door it was not watching.
///
/// `ilp_request_timeout_matches_the_conf_literal` fails the build if the two
/// ever disagree.
pub const ILP_REQUEST_TIMEOUT_SECS: u64 = 5;

/// ILP-over-HTTP conf: per-flush server ACK with `retry_timeout=0` (the caller
/// owns retry cadence) and a bounded `request_timeout` so a hung flush cannot
/// wedge the drain.
fn depth_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

// ---------------------------------------------------------------------------
// Depth ILP spill — the durable floor depth did not have
// ---------------------------------------------------------------------------

/// Directory failed-flush depth ILP payloads are appended to.
///
/// A SIBLING of `tick_persistence::TICK_SPILL_DIR` (`data/spill/ticks`), not a
/// parallel invention: same `data/spill` root, same `.ilp` payload, same
/// per-feed-per-hour file naming, so ONE retention sweep and ONE replay task
/// can see both. Kept in its own subdirectory so the byte cap below bounds
/// depth independently — depth is ~24× the tick row volume, so sharing a
/// budget would let depth starve the tick rescue.
pub const DEPTH_SPILL_DIR: &str = "data/spill/depth";

/// Hard ceiling on the depth spill directory, in bytes (512 MiB).
///
/// **The bound is explicit because the disk is the thing that fails first.**
/// The spill lives on the SAME filesystem as QuestDB, which was 86% full on
/// 2026-08-24. An unbounded rescue would trade a bounded depth loss for an
/// unbounded outage of everything sharing that volume — a strictly worse
/// trade. Past the cap the rows ARE dropped and counted, which is the honest
/// failure this module already had, not a new one.
///
/// **Honest horizon:** at the measured 1,530,651,649 rows/session over the
/// 24,000-second window (~63,800 rows/s) and ~140 B of line protocol per row,
/// 512 MiB holds roughly **60 seconds** of full-rate depth. That is the right
/// order of magnitude for the failure this rescues — a QuestDB write stall or
/// a client-side flush timeout — and deliberately NOT sized for an outage of
/// minutes, because 24 GB of depth spill on a volume with ~28 GB free is how a
/// rescue becomes the incident.
pub const DEPTH_SPILL_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Fraction of the volume the depth spill may occupy: one thirty-second.
///
/// Deliberately the SAME fraction as [`crate::tick_persistence::TICK_SPILL_VOLUME_FRACTION`],
/// so the two rescue tiers scale together instead of drifting apart again.
pub const DEPTH_SPILL_VOLUME_FRACTION: u64 = 32;

/// Ceiling on the depth spill directory, in bytes — DERIVED from the volume.
///
/// # Why this stopped being a constant (2026-08-26)
///
/// [`DEPTH_SPILL_MAX_BYTES`] is retained above as the FLOOR and the fallback,
/// and its reasoning was sound *when written*: it cites a volume "86% full on
/// 2026-08-24" with "~28 GB free", and argues that 24 GB of depth spill on
/// such a volume "is how a rescue becomes the incident". That is correct — on
/// that volume.
///
/// The volume is no longer that volume. It was grown to 300 GB and measured at
/// **255 GB free (16% used)** on 2026-08-26. The constant now encodes a disk
/// state that has not existed for two days, and it encodes it invisibly: the
/// number reads as a deliberate safety bound rather than as a snapshot of a
/// disk that has since changed underneath it.
///
/// The asymmetry it produced is the reason this is a defect and not a
/// preference. Measured on prod the same day:
///
/// | tier | ceiling | how sized | rows/s it protects | outage covered |
/// |---|---|---|---|---|
/// | tick  | ~9.4 GB | volume ÷ 32 | 2,706 | **~5.2 hours** |
/// | depth | 512 MiB | a literal | 51,000 | **~75 seconds** |
///
/// The stream carrying **19× the volume** was given **250× less coverage**,
/// and the tick tier was made host-derived on 2026-08-21 for precisely this
/// reason — the depth tier simply was not moved with it. Deriving it restores
/// the original intent (a bound that cannot threaten the database it rescues
/// from) while letting the bound follow the disk: at 1/32 it still leaves
/// **96.8%** of the volume to QuestDB and the frame WAL, exactly as the tick
/// tier does.
///
/// An unmeasurable volume falls back to the floor with a coded warning, never
/// silently — same discipline as the tick tier.
///
/// # Complexity
/// O(1) after the first call (`OnceLock`). The first call spawns one `df`, on
/// the cold flush-failure path only.
#[must_use]
pub fn depth_spill_max_bytes() -> u64 {
    static RESOLVED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        // Probe the deepest EXISTING ancestor: `df` on a path that does not
        // exist yet fails, and the spill dir is created lazily.
        let mut probe: &std::path::Path = std::path::Path::new(DEPTH_SPILL_DIR);
        while !probe.exists() {
            match probe.parent() {
                Some(parent) => probe = parent,
                None => break,
            }
        }
        match crate::disk_health_watcher::probe_disk_free_bytes(probe) {
            crate::disk_health_watcher::DiskHealthOutcome::Ok { total_bytes, .. }
                if total_bytes > 0 =>
            {
                let derived = total_bytes / DEPTH_SPILL_VOLUME_FRACTION;
                // Never BELOW the old fixed cap: this change exists to stop
                // losing depth rows, so it must not reduce headroom on a
                // small volume.
                let ceiling = derived.max(DEPTH_SPILL_MAX_BYTES);
                tracing::info!(
                    total_bytes,
                    ceiling_bytes = ceiling,
                    fraction = DEPTH_SPILL_VOLUME_FRACTION,
                    "depth spill ceiling derived from the volume — a failed flush can be \
                     rescued to disk up to this size before depth rows are dropped"
                );
                ceiling
            }
            _ => {
                tracing::warn!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    fallback_bytes = DEPTH_SPILL_MAX_BYTES,
                    "could not measure the spill volume, so the depth spill ceiling falls \
                     back to the fixed floor. Rescues past that size will be refused and \
                     their depth rows dropped — on a large volume this is far more \
                     conservative than necessary"
                );
                DEPTH_SPILL_MAX_BYTES
            }
        }
    })
}

/// Total bytes currently held in the depth spill directory.
///
/// Non-recursive and best-effort: an unreadable entry contributes 0 rather
/// than aborting, because failing to MEASURE the cap must not also fail the
/// rescue the cap protects. Duplicated from the tick writer's private helper
/// rather than shared, because that one is module-private there.
fn depth_spill_dir_bytes(dir: &Path) -> u64 {
    // O(1) EXEMPT: begin — cold path, bounded by the per-feed-per-hour file count.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter_map(|e| e.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|m| m.len())
        .sum()
    // O(1) EXEMPT: end
}

/// Appends a failed flush's ILP payload to the depth spill directory.
///
/// # Why the payload is stored verbatim
///
/// `Buffer::as_bytes()` is InfluxDB line protocol — byte-for-byte the body
/// QuestDB's own `/write` endpoint accepts. So the file needs no bespoke
/// format and no parser: replay is POSTing the bytes back, which is exactly
/// what `tick_spill_replay` already does for every `.ilp` file in a directory.
/// The extension is taken from that module's own constant so the two cannot
/// silently diverge.
///
/// # Why replaying it twice is safe
///
/// [`DEDUP_KEY_MARKET_DEPTH`] carries `depth_kind` AND `capture_seq`, and both
/// are emitted on every line. A replayed row therefore reproduces an
/// IDENTICAL key and UPSERTs onto itself instead of duplicating — including
/// across the two pools, whose level-5 rows would otherwise collide.
///
/// # Why the cap is a parameter
///
/// `cap_bytes` is injected rather than read from the const so the enforcement
/// can be proven end to end without writing half a gigabyte on an already-full
/// volume — a test that must fill the disk to prove the disk guard is a test
/// nobody runs. Production always passes [`DEPTH_SPILL_MAX_BYTES`].
///
/// # Errors
///
/// `Err` when the directory cannot be created, the cap is reached, or the
/// append fails. The caller treats that as "rescue unavailable" and falls back
/// to the counted drop: **a spill that cannot be written must never mask the
/// loss.** The spill filesystem is the one currently closest to full, so this
/// arm is a live path, not a theoretical one.
fn spill_failed_depth_ilp(
    dir: &Path,
    payload: &[u8],
    feed: Feed,
    now_unix_secs: i64,
    cap_bytes: u64,
) -> std::io::Result<PathBuf> {
    // O(1) EXEMPT: begin — cold path, runs only on a flush failure.
    std::fs::create_dir_all(dir)?;

    // SOFT cap, not a refusal — see the twin in `tick_persistence.rs`.
    //
    // MEASURED IN PRODUCTION 2026-09-01: this exact arm fired 48 times with
    // `cap_bytes = 10_063_871_360` while `df` reported 143 GB free, and
    // 238,615,500 depth rows were permanently discarded. Depth carries ~24x the
    // tick row volume against an IDENTICALLY sized ceiling, which is why the
    // depth twin failed 12x more often than the tick one for the same defect.
    //
    // The rail's intent — the rescue tier must never starve QuestDB — is kept.
    // Only the measured quantity changes: from the volume's TOTAL size, which
    // cannot threaten anything, to FREE space, which is the only thing that can.
    match crate::tick_persistence::classify_spill_ceiling(
        depth_spill_dir_bytes(dir),
        cap_bytes,
        crate::tick_persistence::spill_free_bytes(dir),
        crate::tick_persistence::SPILL_SOFT_CEILING_FREE_RESERVE_BYTES,
    ) {
        crate::tick_persistence::SpillCeilingVerdict::UnderCeiling => {}
        crate::tick_persistence::SpillCeilingVerdict::OverCeilingWithRoom => {
            metrics::counter!("tv_depth_spill_over_soft_cap_total").increment(1);
        }
        crate::tick_persistence::SpillCeilingVerdict::OverCeilingNoRoom => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "depth spill dir past its {cap_bytes}-byte soft cap and free space is \
                     at or below the database reserve — refusing so QuestDB keeps room \
                     to operate"
                ),
            ));
        }
        crate::tick_persistence::SpillCeilingVerdict::OverCeilingProbeFailed => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "depth spill dir past its {cap_bytes}-byte soft cap and the free-space \
                     probe failed — refusing rather than growing blind"
                ),
            ));
        }
    }

    // One file per feed per hour: bounded file count, and an operator replaying
    // a known-bad window does not have to read one ever-growing file.
    let hour = now_unix_secs / 3_600;
    let path = dir.join(format!(
        "depth-{}-{hour}.{SPILL_FILE_EXTENSION}",
        feed.as_str()
    ));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(payload)?;
    file.flush()?;
    Ok(path)
    // O(1) EXEMPT: end
}

/// Publish a zero on EVERY depth loss series before any row can be written.
///
/// The CloudWatch agent computes a counter's alarm value as the DELTA between
/// consecutive samples and DROPS the first sample of a series it has never
/// seen. `tv_depth_rows_dropped_total` increments only when buffered rows are
/// discarded, so without this the first drop episode IS the baseline sample:
/// it publishes no datapoint and any alarm on it stays silent through the very
/// episode it exists to catch. Same discipline as `TickWriter`'s
/// `register_drop_baseline`.
///
/// # Why the SIBLINGS are seeded too (2026-08-28, proven by a live session)
///
/// Until today only the `dropped` series was seeded, and the session of
/// 2026-08-28 showed what that costs. Read from CloudWatch afterwards:
///
/// | series | session total |
/// |---|---|
/// | `tv_ticks_dropped_total` | 308,818 |
/// | `tv_ticks_spilled_total` | 308,818 |
/// | `tv_depth_rows_dropped_total` | 104,540 |
/// | `tv_depth_rows_spilled_total` | **no series at all** |
/// | `tv_depth_spill_write_errors_total` | **no series at all** |
///
/// The tick side seeds both, so `dropped - spilled = 0` proved every dropped
/// tick had been RESCUED and nothing was permanently lost. The depth side
/// seeded one, so the same subtraction was impossible: 104,540 rows that were
/// either all rescued or all permanently gone, and no way to tell which.
///
/// An unseeded counter does not read as "zero". It does not appear, and an
/// absent series is indistinguishable from a healthy one — which is the exact
/// false-OK the discriminators were added to prevent. The instrument could not
/// answer the one question it exists to answer.
///
/// `tv_depth_spill_write_errors_total` is seeded on BOTH of its `stage` label
/// values, because a label set is a separate series: seeding `cap` alone would
/// leave `write` — the more common failure — silent on its first occurrence.
fn register_depth_drop_baseline(feed: Feed) {
    metrics::counter!("tv_depth_rows_dropped_total", "feed" => feed.as_str()).increment(0);
    metrics::counter!("tv_depth_rows_spilled_total", "feed" => feed.as_str()).increment(0);
    metrics::counter!("tv_depth_spill_write_errors_total", "stage" => "cap").increment(0);
    metrics::counter!("tv_depth_spill_write_errors_total", "stage" => "write").increment(0);
    metrics::counter!("tv_depth_persist_errors_total", "stage" => "ensure_client_build")
        .increment(0);
    metrics::counter!("tv_depth_persist_errors_total", "stage" => "ensure_ddl").increment(0);
}

/// Batched `market_depth` ILP writer.
///
/// Lazy: an unreachable QuestDB at construction still builds (rows buffer
/// locally). On a failed `flush` the pending buffer leaves memory — but it is
/// RESCUED to the depth spill tier first, and only genuinely dropped when that
/// rescue itself fails. Either way it is loud.
pub struct DepthWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
    feed: Feed,
    /// Rows that left the buffer without reaching QuestDB, across this
    /// writer's lifetime.
    ///
    /// Deliberately counts the RESCUED rows too, mirroring `TickWriter`: the
    /// EMF-shipped `tv_depth_rows_dropped_total` is the only depth series an
    /// alarm can watch, so diverting the common flush failure off it would
    /// have told the operator LESS than before the rescue existed — a false-OK
    /// (audit Rule 11). `rescued` is the strictly narrower "and it is
    /// recoverable"; `dropped - rescued` is the unrecoverable loss.
    dropped: u64,
    /// Rows durably captured to the spill tier — the recoverable subset of
    /// `dropped`.
    rescued: u64,
    /// Spill directory. Production uses [`DEPTH_SPILL_DIR`]; tests get an
    /// isolated temp dir so they never write into the repo.
    spill_dir: PathBuf,
    /// Hand-off queue to the depth writer thread when this writer has been
    /// split by [`DepthWriter::split_for_offload`]. `None` means the
    /// synchronous arm, which is what every non-lane caller still uses.
    offload: Option<std::sync::mpsc::SyncSender<DepthFlushBatch>>,
    /// Set by [`DepthWriter::split_rescue_offload`]. When present,
    /// [`DepthWriter::discard_pending`] hands the buffer to a dedicated rescue
    /// thread instead of writing up to 32 MiB on the frame-drain task.
    ///
    /// Separate from `offload` on purpose: the rescue exists precisely for the
    /// two cases that queue cannot serve — it is FULL, or its thread is GONE.
    rescue: Option<std::sync::mpsc::SyncSender<DepthRescueBatch>>,
    /// Consecutive flush spans the producer has RETAINED because the hand-off
    /// queue was full. Bounded by [`MAX_DEPTH_RETAINED_FLUSH_SPANS`] so
    /// backpressure cannot silently widen a commit without limit.
    retained_spans: u32,
}

/// A unique temp spill directory, so a test writer never touches
/// `data/spill/depth` in the working tree.
///
/// `for_test` is called from the APP crate too, where `cfg(test)` does not
/// reach — so the isolation has to live in the constructor, not behind a test
/// gate.
fn temp_depth_spill_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!(
        "tv-depth-spill-test-{nanos}-{:?}",
        std::thread::current().id()
    ))
}

/// Whether a flush failure is worth ONE retry before the buffer is discarded.
///
/// The discard exists for a real reason, stated at [`DepthPersistence::flush`]:
/// a row the SERVER rejected would be re-sent forever and block every later
/// row. That reasoning covers a rejection and nothing else — and until
/// 2026-08-25 the code applied it to every error alike.
///
/// It cost rows. On 2026-08-25 at 13:12:07 a flush failed with
/// `io: Interrupted system call (os error 4)` and 8,810 depth rows left the
/// buffer. EINTR is a signal arriving mid-syscall. The server never saw a
/// complete request, nothing was rejected, and the buffer was untouched —
/// there was nothing to wedge the session and every row was recoverable by
/// simply trying again.
///
/// `SocketError` is the transport class (`from_ureq_error` maps IO and
/// timeout failures onto it), so it is the one worth retrying.
/// `ServerFlushError` is the rejection the discard was designed for and is
/// deliberately NOT retried. Everything else — a bad name, a bad timestamp,
/// an API misuse — is our own bug and would fail identically forever.
///
/// Retrying is safe here for the SAME reason the rescue path already tells
/// the operator a manual re-ingest is safe: the depth DEDUP key carries
/// `depth_kind` and `capture_seq`, so a partially-applied write plus a
/// re-send collapses instead of duplicating.
/// How fast a retryable flush failure must come back before the ONE retry is
/// allowed.
///
/// The depth flush runs inside `block_in_place` on the drain task — the task
/// that folds ticks — and the sender's `request_timeout` is 5,000 ms. An
/// unconditional retry therefore makes the worst case TEN seconds of drain
/// occupancy on the largest table in the database, which is the same
/// socket-buffer-fills-and-the-vendor-skips-us mechanism the whole
/// zero-tick-loss requirement is about.
///
/// A timed-out request and an `EINTR` both surface as
/// `questdb::ErrorCode::SocketError`, so the error class cannot separate the
/// case the retry exists for from the case that must not be retried. The clock
/// can: an interrupted syscall returns in microseconds, a timeout consumes the
/// full 5,000 ms. One second sits three orders of magnitude above the first
/// and five times below the second.
pub const DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW_MS: u64 = 1_000;

/// [`DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW_MS`] as a `Duration`.
pub const DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW: std::time::Duration =
    std::time::Duration::from_millis(DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW_MS);

#[must_use]
fn flush_failure_is_retryable(err: &questdb::Error) -> bool {
    matches!(err.code(), questdb::ErrorCode::SocketError)
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
                    rescued: 0,
                    spill_dir: PathBuf::from(DEPTH_SPILL_DIR),
                    offload: None,
                    rescue: None,
                    retained_spans: 0,
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
                    rescued: 0,
                    spill_dir: PathBuf::from(DEPTH_SPILL_DIR),
                    offload: None,
                    rescue: None,
                    retained_spans: 0,
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
            rescued: 0,
            spill_dir: temp_depth_spill_dir(),
            offload: None,
            rescue: None,
            retained_spans: 0,
        }
    }

    /// Test-only: points the rescue tier at an isolated directory.
    #[cfg(test)]
    #[must_use]
    fn with_spill_dir_for_test(mut self, dir: PathBuf) -> Self {
        self.spill_dir = dir;
        self
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

    /// Rows durably captured to the depth spill tier — the RECOVERABLE subset
    /// of [`DepthWriter::dropped`]. `dropped - rescued` is the loss nothing
    /// re-inserts.
    #[must_use]
    // TEST-EXEMPT: observability accessor, asserted by the rescue tests below.
    pub fn rescued(&self) -> u64 {
        self.rescued
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
    /// are written first. All four come from closed `&'static str` sets whose
    /// ILP-safety is proven at COMPILE TIME by the `const _` block near the top
    /// of this file, so none of them is re-checked per row. See the note at the
    /// call site for why that mattered.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, row: &DepthRow) -> Result<()> {
        let feed = self.feed.as_str();
        // Passed through WITHOUT `sanitize_ilp_symbol`, deliberately.
        //
        // All four are `&'static str` from CLOSED SETS, and the `const _` block
        // near the top of this file proves at COMPILE TIME that every member of
        // every one of those sets is already ILP-safe -- exhaustively, over all
        // 256 segment codes and every `Feed::ALL` entry. The sanitiser returns
        // `Cow::Borrowed` for clean input, so it allocated nothing and DHAT
        // could not see it; what it DID do was walk the characters of the
        // literal `"bid"` to re-derive an answer fixed when the constant was
        // written -- four times per row, on a path this module's own header
        // measures at ~1.53e9 rows per session. The check did not disappear; it
        // moved to the compiler, which is principle 2 in its literal form.
        self.buffer
            .table(MARKET_DEPTH_TABLE)
            .context("table")?
            .symbol("segment", row.segment)
            .context("segment")?
            .symbol("depth_kind", row.depth_kind)
            .context("depth_kind")?
            .symbol("side", row.side)
            .context("side")?
            .symbol("feed", feed)
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

    /// Rescues every buffered-but-unflushed row to the depth spill tier, then
    /// clears. Returns how many rows left the buffer.
    ///
    /// # What this replaces, and why
    ///
    /// This was a bare discard whose own log line said *"These levels are gone
    /// from the table"*. Ticks have had a three-tier durable floor since
    /// 2026-08-21 — ring, NDJSON/ILP spill, DLQ — so a tick flush failure is
    /// recoverable. Depth, at a MEASURED 1,530,651,649 rows/session against
    /// 64,349,753 ticks (2026-08-24) — **24× the tick volume and the largest
    /// payload in the process** — had none of it, on a box whose QuestDB write
    /// path is demonstrably stalling (`market_depth`'s WAL 8,061 transactions
    /// behind, the volume pinned at its provisioned throughput ceiling). So
    /// this path is not hypothetical; it is the one most likely to fire.
    ///
    /// It also violated the operator's standing 2026-08-15 directive that
    /// nothing may be missed, hidden or wiped off — the discard did all three
    /// while reporting a healthy table.
    ///
    /// # Why the rescue can still fail, and what happens then
    ///
    /// The spill shares the filesystem with QuestDB, which was 86% full. A
    /// write failure or the [`DEPTH_SPILL_MAX_BYTES`] cap is therefore a live
    /// outcome, and it falls back to the counted drop with its own coded
    /// error. Silent loss is the defect; a NAMED unrecoverable loss is not.
    /// Rescue the buffered levels to the spill tier instead of losing them.
    ///
    /// # Why this hands off instead of writing (2026-08-28)
    ///
    /// The tick writer's twin of this function was taken off the drain earlier
    /// today; this is the same fix on the writer that carries **24x the rows**
    /// — a measured 1,530,651,649 depth rows per session against 64,349,753
    /// ticks — and on a path with one more way in: the frame arm, the 500 ms
    /// timer arm, and the shutdown tail all reach it.
    ///
    /// Inline it did `create_dir_all`, a `read_dir` + per-entry `metadata()`
    /// walk of the depth spill directory, a live free-space probe that FORKS
    /// `df` on its first call, and then up to
    /// [`MAX_DEPTH_PRODUCER_BUFFER_BYTES`] — 32 MiB — of file write. All on the
    /// frame-drain task, and all on the volume QuestDB is stalling on.
    ///
    /// And it fires at the worst instant BY CONSTRUCTION: the cut that calls it
    /// only trips after the hand-off queue has been full for
    /// [`MAX_DEPTH_RETAINED_FLUSH_SPANS`] consecutive flushes, i.e. only when
    /// the database is already behind. A stalled drain stops emptying the
    /// socket, the receive buffer fills, and Dhan — which skips a slow consumer
    /// forward to "the latest available state" with no sequence number —
    /// discards the intermediate ticks at THEIR side, where nothing we own can
    /// see them.
    ///
    /// The fallback is deliberately the OLD behaviour, never a drop: a full
    /// rescue queue or a dead rescue thread writes inline exactly as before.
    pub fn discard_pending(&mut self) -> usize {
        let rows = self.pending;
        if rows == 0 {
            self.buffer.clear();
            return 0;
        }

        // Off-drain hand-off. O(1), no syscall, no allocation: the `Buffer` is
        // MOVED, and the replacement is the same empty one `offload_flush`
        // already installs on every successful hand-off.
        if let Some(tx) = self.rescue.as_ref() {
            let protocol = self.buffer.protocol_version();
            let batch = DepthRescueBatch {
                buffer: std::mem::replace(&mut self.buffer, Buffer::new(protocol)),
                rows,
            };
            match tx.try_send(batch) {
                Ok(()) => {
                    metrics::counter!(
                        DEPTH_RESCUE_QUEUED_COUNTER,
                        "feed" => self.feed.as_str()
                    )
                    .increment(rows as u64);
                    // Counted as rescued HERE, not on the thread, because the
                    // caller's log-wording branch reads this field one line
                    // after the call and a queued payload IS on its way to the
                    // spill file. The rare case where it does not arrive is the
                    // shutdown abandonment, which has its own counter and its
                    // own coded error rather than being folded into this one.
                    self.rescued = self.rescued.saturating_add(rows as u64);
                    self.pending = 0;
                    self.dropped = self.dropped.saturating_add(rows as u64);
                    return rows;
                }
                Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                    self.buffer = returned.buffer;
                    metrics::counter!(
                        DEPTH_RESCUE_INLINE_FALLBACK_COUNTER,
                        "feed" => self.feed.as_str(),
                        "reason" => "queue_full"
                    )
                    .increment(1);
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(returned)) => {
                    self.buffer = returned.buffer;
                    metrics::counter!(
                        DEPTH_RESCUE_INLINE_FALLBACK_COUNTER,
                        "feed" => self.feed.as_str(),
                        "reason" => "thread_gone"
                    )
                    .increment(1);
                }
            }
        }

        if perform_depth_rescue(&self.spill_dir, self.buffer.as_bytes(), self.feed, rows) {
            self.rescued = self.rescued.saturating_add(rows as u64);
        }
        self.buffer.clear();
        self.pending = 0;
        self.dropped = self.dropped.saturating_add(rows as u64);
        rows
    }

    /// Splits this writer into a PRODUCER half and a network SINK half.
    ///
    /// The producer keeps the ILP buffer and the row accounting and stays on
    /// the drain task; the sink takes the `Sender` and belongs on a thread of
    /// its own. They are joined by a bounded queue, so the drain can never be
    /// blocked by the network and can never grow the queue without bound.
    ///
    /// See the module section "Off-drain flush for DEPTH" for why this matters
    /// more here than on the tick path: depth is 24x the tick row volume and
    /// its synchronous flush ran on the frame drain, so a QuestDB stall became
    /// upstream tick loss and a socket disconnect.
    ///
    /// Consuming `self` and returning a new one is deliberate: it makes the
    /// split a one-way door at the type level, so no caller can hold a handle
    /// that still believes it owns the network.
    #[must_use]
    // TEST-EXEMPT: the split itself is exercised by every depth offload test
    // below, each of which calls it to obtain the producer/sink pair.
    pub fn split_for_offload(
        mut self,
    ) -> (
        Self,
        DepthWriterSink,
        std::sync::mpsc::Receiver<DepthFlushBatch>,
    ) {
        let (tx, rx) = std::sync::mpsc::sync_channel(DEPTH_FLUSH_QUEUE_DEPTH);
        let sink = DepthWriterSink {
            sender: self.sender.take(),
            feed: self.feed,
            spill_dir: self.spill_dir.clone(),
        };
        self.offload = Some(tx);
        (self, sink, rx)
    }
    /// Hands the depth rescue write to a dedicated thread.
    ///
    /// Separate from [`DepthWriter::split_for_offload`] because it solves the
    /// case that split CANNOT: the rescue fires exactly when the flush queue is
    /// full or its thread is gone, so it cannot ride the same queue.
    pub fn split_rescue_offload(
        &mut self,
    ) -> (DepthRescueSink, std::sync::mpsc::Receiver<DepthRescueBatch>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(DEPTH_RESCUE_QUEUE_DEPTH);
        let sink = DepthRescueSink {
            spill_dir: self.spill_dir.clone(),
            feed: self.feed,
        };
        self.rescue = Some(tx);
        (sink, rx)
    }

    /// Closes the depth rescue queue so its thread can exit.
    ///
    /// Leaves the writer in the INLINE state on purpose: a rescue after this
    /// point writes synchronously rather than being refused, so end-of-session
    /// levels still reach the spill tier.
    pub fn close_rescue_offload(&mut self) {
        self.rescue = None;
    }

    /// Closes the hand-off queue, so the writer thread sees the end of the
    /// stream and can exit.
    ///
    /// Shutdown-only. Dropping the sender is what turns the writer's blocking
    /// `recv` into a clean exit; without it, a caller that joins the thread
    /// waits forever on a queue nothing will ever close.
    ///
    /// Leaves the writer in the UNSPLIT state, so a flush after this point
    /// takes the synchronous arm and — with the sender long gone to the sink —
    /// rescues to the depth spill tier rather than silently discarding. That is
    /// the correct end-of-session behaviour: rows are on disk and named.
    pub fn close_offload(&mut self) {
        self.offload = None;
    }

    /// True once [`Self::split_for_offload`] has run and the queue is open.
    #[must_use]
    // TEST-EXEMPT: read by the offload tests below and by the lane's wiring.
    pub const fn is_offloaded(&self) -> bool {
        self.offload.is_some()
    }

    /// Hands the pending buffer to the writer thread without touching the
    /// network.
    ///
    /// Uses `try_send`, never `send`: a blocking send would re-create the exact
    /// coupling the split exists to remove, one queue further out.
    fn offload_flush(&mut self) -> DepthOffloadOutcome {
        let rows = self.pending;
        // Read the protocol version BEFORE the replace: a fresh buffer must
        // speak the same protocol the sender negotiated, and borrowing rules
        // will not let both happen in one expression.
        let protocol = self.buffer.protocol_version();
        let batch = DepthFlushBatch {
            buffer: std::mem::replace(&mut self.buffer, Buffer::new(protocol)),
            rows,
        };
        let Some(tx) = self.offload.as_ref() else {
            // Unreachable — `flush` checks. Treated as the gone arm rather than
            // silently succeeding, because "we sent it" when nothing was sent
            // is the one report that must never be wrong.
            self.buffer = batch.buffer;
            return DepthOffloadOutcome::SinkGone(rows);
        };
        match tx.try_send(batch) {
            Ok(()) => {
                self.pending = 0;
                self.retained_spans = 0;
                metrics::counter!(
                    "tv_depth_flush_offloaded_total",
                    "feed" => self.feed.as_str()
                )
                .increment(1);
                DepthOffloadOutcome::Sent(rows)
            }
            Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                // Backpressure, not loss. Put the rows BACK and keep appending
                // — the next flush retries. This arm is what makes the bounded
                // queue safe: without it a full queue would either block the
                // drain (the original defect) or drop rows (a worse one).
                metrics::counter!(
                    "tv_depth_flush_queue_full_total",
                    "feed" => self.feed.as_str()
                )
                .increment(1);
                let held = returned.buffer.as_bytes().len();
                self.buffer = returned.buffer;
                self.retained_spans = self.retained_spans.saturating_add(1);
                // TWO independent cuts. Spans is the primary bound; bytes is
                // the belt-and-braces one that matters MORE on this path than
                // on the tick path, because a single depth-200 snapshot is 400
                // rows and a burst can breach a byte ceiling inside one span.
                //
                // `>` and not `>=`: the constant names how many spans may be
                // RETAINED, so the cut belongs on the span after them.
                if self.retained_spans > MAX_DEPTH_RETAINED_FLUSH_SPANS
                    || held >= MAX_DEPTH_PRODUCER_BUFFER_BYTES
                {
                    metrics::counter!(
                        "tv_depth_flush_width_capped_total",
                        "feed" => self.feed.as_str()
                    )
                    .increment(1);
                    self.retained_spans = 0;
                    // Rescue rather than keep widening. Durable, counted, and
                    // re-ingestable — the same tier a failed flush uses.
                    let dropped = self.discard_pending();
                    return DepthOffloadOutcome::WidthCapped(dropped);
                }
                DepthOffloadOutcome::QueueFull(rows)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(returned)) => {
                // The writer thread died. Rescue rather than drop, and say so.
                self.buffer = returned.buffer;
                let dropped = self.discard_pending();
                DepthOffloadOutcome::SinkGone(dropped)
            }
        }
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
        // OFF-DRAIN ARM. Checked before the sender, because once split there
        // IS no sender on this half — it went to the sink — and falling
        // through would rescue every batch of a perfectly healthy lane.
        //
        // Returns `Ok(())` on QueueFull as well as on Sent, and that is the
        // load-bearing decision in this function: a full queue means the rows
        // are still held and still pending, so reporting `Err` would make the
        // caller decay feed health and log a failure for backpressure that
        // lost nothing. WidthCapped and SinkGone DID move rows out of the
        // buffer (rescued, counted, coded), so they report `Err` exactly as a
        // failed synchronous flush does.
        if self.offload.is_some() {
            return match self.offload_flush() {
                DepthOffloadOutcome::Sent(_) | DepthOffloadOutcome::QueueFull(_) => Ok(()),
                DepthOffloadOutcome::WidthCapped(rows) => {
                    anyhow::bail!(
                        "market_depth producer held {rows} row(s) past its retention bound \
                         while the writer thread was behind; they were rescued to the depth \
                         spill tier — see the preceding coded line"
                    )
                }
                DepthOffloadOutcome::SinkGone(rows) => {
                    anyhow::bail!(
                        "market_depth writer thread is gone; {rows} row(s) were rescued to \
                         the depth spill tier — see the preceding coded line"
                    )
                }
            };
        }
        let Some(sender) = self.sender.as_mut() else {
            let dropped = self.discard_pending();
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                feed = self.feed.as_str(),
                dropped,
                "market_depth flush with no QuestDB connection — {dropped} depth row(s) left \
                 the buffer; see the preceding line for whether they were rescued to the \
                 depth spill tier or permanently lost"
            );
            anyhow::bail!("market_depth writer disconnected; {dropped} row(s) discarded");
        };
        let started = std::time::Instant::now();
        let first = sender.flush(&mut self.buffer);
        let first_failure_elapsed = started.elapsed();
        let outcome = match first {
            Ok(()) => Ok(()),
            Err(err)
                if flush_failure_is_retryable(&err)
                    && first_failure_elapsed < DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW =>
            {
                // ONE retry, only for the transport class, and only when the
                // first attempt failed FAST.
                //
                // EINTR and a connection reset leave the buffer intact and the
                // server holding nothing to reject, so discarding here throws
                // away rows that a second attempt would have written -- which
                // is exactly what happened on 2026-08-25 at 13:12:07, when
                // `io: Interrupted system call (os error 4)` cost 8,810 rows.
                //
                // Bounded at one deliberately. This runs on the flush path, so
                // an unbounded ladder would trade row loss for a stall, and a
                // stall on this path fills the socket buffer and loses ticks
                // upstream instead -- the same loss wearing a different name.
                // A second failure falls through to the rescue-then-discard
                // below, unchanged.
                //
                // The TIMING condition (added 2026-08-26) is what keeps that
                // reasoning true. `request_timeout=5000` and an unconditional
                // retry make the worst case TEN seconds of a flush that runs
                // inside `block_in_place` on the drain task -- and the drain
                // task is the tick fold. A timed-out flush maps to the same
                // `SocketError` class as an EINTR, so without a clock the two
                // are indistinguishable and the retry doubles the exact stall
                // it was reasoned to be safe against. An EINTR returns in
                // microseconds; a timeout consumes the full budget. The window
                // separates them with three orders of magnitude to spare.
                metrics::counter!(
                    "tv_depth_flush_retries_total",
                    "feed" => self.feed.as_str(),
                )
                .increment(1);
                sender.flush(&mut self.buffer)
            }
            Err(err) => {
                if flush_failure_is_retryable(&err) {
                    // Retryable in class but SLOW: not retried, and counted so
                    // "the retry is not firing" is answerable without a guess.
                    metrics::counter!(
                        "tv_depth_flush_retries_skipped_total",
                        "feed" => self.feed.as_str(),
                    )
                    .increment(1);
                }
                Err(err)
            }
        };
        match outcome {
            Ok(()) => {
                self.pending = 0;
                Ok(())
            }
            Err(err) => {
                let rescued_before = self.rescued;
                let dropped = self.discard_pending();
                let rescued = self.rescued > rescued_before;
                // The message used to say "These levels are gone from the
                // table" on EVERY failure -- including the common case where
                // `discard_pending` had just RESCUED the whole buffer to the
                // depth spill file one line earlier. That is not a wording
                // nit: reading the 2026-08-25 log it made a rescued 5,770-row
                // flush look like 5,770 lost rows, and a loss figure that is
                // wrong in the alarming direction gets acted on.
                if rescued {
                    error!(
                        code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                        feed = self.feed.as_str(),
                        dropped,
                        rescued = true,
                        ?err,
                        "market_depth flush FAILED — {dropped} depth row(s) left the buffer \
                         and were RESCUED to the depth spill file named on the preceding \
                         line. They are NOT lost and NOT in QuestDB; re-ingest is safe to \
                         repeat"
                    );
                } else {
                    error!(
                        code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                        feed = self.feed.as_str(),
                        dropped,
                        rescued = false,
                        ?err,
                        "market_depth flush FAILED and the spill rescue failed too — \
                         {dropped} depth row(s) are permanently gone from the table. The \
                         raw frames remain in the write-ahead log"
                    );
                }
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

// ---------------------------------------------------------------------------
// Off-drain flush for DEPTH (2026-08-28)
// ---------------------------------------------------------------------------
//
// # Why this exists, and why it is the single most consequential change here
//
// `TickWriter` was taken off the frame-drain task on 2026-08-25. `DepthWriter`
// was NOT, and it is the writer that needed it more:
//
// | | ticks | depth |
// |---|---|---|
// | rows per session (MEASURED 2026-08-24) | 64,349,753 | **1,530,651,649** |
// | flush off the drain | yes | **no, until this change** |
//
// Depth is 24x the tick volume and the largest payload in the process, and its
// flush ran SYNCHRONOUSLY inside `block_in_place` on the drain task, with a
// 5,000 ms `request_timeout`, up to ~5 times a second at the modelled rate.
// `block_in_place` bounds the damage to the RUNTIME -- a replacement worker is
// spun up so other tasks keep running -- but it does not take the drain out of
// the flush's critical path, and the drain is the only thing emptying the
// socket. So a QuestDB hiccup stalled the fold, filled the receive buffer, and
// Dhan (which skips a slow consumer forward to "the latest available state",
// with no sequence number for us to detect it) discarded the intermediate
// ticks at THEIR side. A storage stall became upstream tick loss and a
// WebSocket disconnect, invisibly, and no amount of provisioned disk
// throughput removes it because the coupling is structural rather than a
// matter of speed.
//
// This is the same split, applied to the bigger writer. Everything below
// mirrors `tick_persistence.rs` deliberately, including the bound names, so
// the two paths cannot drift into different failure semantics.

/// Depth of the hand-off queue between the drain and the depth writer thread.
///
/// FOUR, matching the tick path, and for the same reason: the queue is a shock
/// absorber for a QuestDB hiccup SHORTER than the flush cadence, not a place to
/// store data. Every batch sitting in it is rows that exist only in this
/// process's memory, so a deeper queue converts a database stall into a bigger
/// crash-loss window while making the operator's counters look calmer.
///
/// Depth flushes on the ROW threshold far more often than on the 500 ms timer
/// -- at the modelled ~63,800 rows/s and a 10,000-row threshold that is a flush
/// roughly every 156 ms -- so four batches absorb ~600 ms of stall here rather
/// than the ~2 s it absorbs on the tick path. That is the honest figure and it
/// is deliberately NOT compensated for by a deeper queue: a stall longer than
/// that SHOULD surface as backpressure, because it is one.
pub const DEPTH_FLUSH_QUEUE_DEPTH: usize = 4;

// ---------------------------------------------------------------------------
// Off-drain RESCUE for DEPTH (2026-08-28)
// ---------------------------------------------------------------------------

/// Depth of the hand-off queue between the drain and the depth rescue thread.
///
/// TWO, matching the tick side and small for the same reason: a rescue only
/// happens when the flush queue has already been full for
/// [`MAX_DEPTH_RETAINED_FLUSH_SPANS`] consecutive flushes, so this is not a
/// buffer for a busy day — it is somewhere to put ONE oversized payload while
/// the previous one is being written. Deeper would hold more rows that exist
/// nowhere else, which is the trade the rescue tier exists to avoid.
pub const DEPTH_RESCUE_QUEUE_DEPTH: usize = 2;

/// Depth rows handed to the rescue thread rather than written on the drain.
pub const DEPTH_RESCUE_QUEUED_COUNTER: &str = "tv_depth_rescue_queued_total";

/// Depth rescues that had to be written INLINE on the drain after all.
///
/// Non-zero means the drain took the stall this hand-off exists to remove.
/// Not a loss: nothing is dropped on either arm.
pub const DEPTH_RESCUE_INLINE_FALLBACK_COUNTER: &str = "tv_depth_rescue_inline_fallback_total";

/// Depth rescue payloads abandoned because the rescue thread did not finish.
pub const DEPTH_RESCUE_ABANDONED_COUNTER: &str = "tv_depth_rescue_abandoned_total";

/// One oversized depth ILP payload on its way to the spill tier.
pub struct DepthRescueBatch {
    buffer: Buffer,
    rows: usize,
}

impl DepthRescueBatch {
    /// Rows this payload carries — the number an abandoned batch loses.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }
}

/// The depth rescue thread's half: everything the write needs, and nothing else.
pub struct DepthRescueSink {
    spill_dir: PathBuf,
    feed: Feed,
}

impl DepthRescueSink {
    /// Writes one queued payload to the depth spill tier.
    ///
    /// Identical code to the inline fallback — both call
    /// [`perform_depth_rescue`] — so an operator cannot tell which path ran,
    /// and should not have to.
    pub fn rescue(&self, batch: &DepthRescueBatch) {
        perform_depth_rescue(
            &self.spill_dir,
            batch.buffer.as_bytes(),
            self.feed,
            batch.rows,
        );
    }
}

/// The depth rescue write itself — the part that touches the disk.
///
/// Returns `true` when the payload LANDED, so the inline caller can keep its
/// `rescued` accounting exactly as it was before the split. Extracted so the
/// SAME code serves the thread and the fallback; two copies would have drifted,
/// and the one that drifted would have been the fallback — the path that only
/// runs on the worst day.
fn perform_depth_rescue(spill_dir: &Path, payload: &[u8], feed: Feed, rows: usize) -> bool {
    let payload_len = payload.len();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    match spill_failed_depth_ilp(spill_dir, payload, feed, now, depth_spill_max_bytes()) {
        Ok(path) => {
            // BOTH counters, and the EMF-shipped one is not optional: it is the
            // only depth series an alarm watches, so incrementing only the new
            // name would divert the common flush failure off the pager that
            // exists for it.
            metrics::counter!("tv_depth_rows_dropped_total", "feed" => feed.as_str())
                .increment(rows as u64);
            metrics::counter!("tv_depth_rows_spilled_total", "feed" => feed.as_str())
                .increment(rows as u64);
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                feed = feed.as_str(),
                rescued = rows,
                bytes = payload_len,
                path = %path.display(),
                "market_depth flush failed — the buffered levels were RESCUED to the \
                 depth spill file named here, not lost. They are NOT in QuestDB yet. \
                 Re-ingest is one command and is safe to repeat, because the depth \
                 dedup key carries depth_kind and capture_seq: \
                 curl --data-binary @<path> http://<questdb>:9000/write"
            );
            true
        }
        Err(err) => {
            // The rescue itself failed — cap reached, disk full, or no
            // permission. Count it as the genuine unrecoverable loss it is.
            if err.kind() == std::io::ErrorKind::StorageFull {
                metrics::counter!("tv_depth_spill_write_errors_total", "stage" => "cap")
                    .increment(1);
            } else {
                metrics::counter!("tv_depth_spill_write_errors_total", "stage" => "write")
                    .increment(1);
            }
            metrics::counter!("tv_depth_rows_dropped_total", "feed" => feed.as_str())
                .increment(rows as u64);
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                feed = feed.as_str(),
                dropped = rows,
                cap_bytes = depth_spill_max_bytes(),
                spill_dir = %spill_dir.display(),
                spill_error = %err,
                "market_depth flush failed AND the depth spill rescue also failed — \
                 these levels are permanently lost and nothing re-inserts them. The \
                 raw frames remain in the write-ahead log for manual recovery."
            );
            false
        }
    }
}
/// How much un-handed-off ILP text the depth PRODUCER may hold before it
/// rescues to the spill tier.
///
/// When the queue is full the drain keeps its buffer and keeps appending --
/// that is the whole point, the rows are neither lost nor reported as lost.
/// But "keep appending forever" is an unbounded memory path, and this
/// repository's own complexity table records five uncapped maps found the hard
/// way. Past this ceiling the producer stops accumulating and rescues, which is
/// durable, counted, and re-ingestable -- the same tier a failed flush uses.
///
/// This is the SECONDARY bound; [`MAX_DEPTH_RETAINED_FLUSH_SPANS`] is the
/// primary one and cuts far earlier. It exists so that a pathological append
/// rate cannot reach the questdb-rs wedge inside two spans -- which on THIS
/// path is a real possibility rather than a theoretical one, because a single
/// depth-200 snapshot is 400 rows and a burst can add them faster than any
/// span-based bound can react.
pub const MAX_DEPTH_PRODUCER_BUFFER_BYTES: usize = 32 * 1024 * 1024;

// Same headroom rule as the tick path: the producer ceiling must sit at or
// below half the questdb-rs `max_buf_size` wedge, or the rescue arm only fires
// after every flush is already failing permanently -- making the rescue path
// unreachable exactly when it is needed.
const _: () = assert!(
    MAX_DEPTH_PRODUCER_BUFFER_BYTES * 2 <= crate::tick_persistence::QUESTDB_MAX_BUF_SIZE_BYTES,
    "the depth producer ceiling must sit at or below half the questdb-rs max_buf_size wedge"
);

/// How many consecutive flush spans the depth producer may RETAIN before it
/// stops accumulating and spills.
///
/// TWO, matching the tick path. The queue already absorbs
/// [`DEPTH_FLUSH_QUEUE_DEPTH`] batches before the producer ever sees a full
/// queue, so the honest absorption is that depth plus this; a cap of one would
/// spill on the first hiccup.
///
/// Commit WIDTH matters less here than it does for ticks -- a depth snapshot is
/// stamped at capture, so it does not reopen closed partitions the way the 10%
/// of ticks whose exchange timestamp is over an hour behind arrival do -- but
/// the bound is kept identical anyway. A writer that batches more aggressively
/// under pressure is exactly the own-goal the tick change was measured against,
/// and adopting a looser number here on an unmeasured hunch is how that lesson
/// gets relearned.
pub const MAX_DEPTH_RETAINED_FLUSH_SPANS: u32 = 2;

/// One handed-off depth ILP payload, in flight between the drain and the
/// writer thread.
///
/// Deliberately opaque, like its tick counterpart: the drain must not be able
/// to inspect, re-order, or partially consume a batch, because the only correct
/// thing to do with one is hand it to the network or rescue the whole thing.
pub struct DepthFlushBatch {
    buffer: Buffer,
    rows: usize,
}

impl DepthFlushBatch {
    /// Rows this batch covers.
    #[must_use]
    // TEST-EXEMPT: accessor, exercised by the offload tests below.
    pub const fn rows(&self) -> usize {
        self.rows
    }
}

/// What happened to a depth batch the producer tried to hand off.
///
/// Four arms and not a `Result`, because [`DepthOffloadOutcome::QueueFull`] is
/// NOT a failure and must never be logged or counted as one: the rows are still
/// held, still pending, and go out on the next flush. Collapsing it into `Err`
/// is precisely how a backpressure signal becomes a false loss report.
#[derive(Debug, PartialEq, Eq)]
pub enum DepthOffloadOutcome {
    /// Handed to the writer thread. The rows are no longer the drain's.
    Sent(usize),
    /// The writer is behind. Rows RETAINED by the producer, nothing lost.
    QueueFull(usize),
    /// The writer stayed behind long enough that retaining further would widen
    /// the commit past [`MAX_DEPTH_RETAINED_FLUSH_SPANS`] or breach
    /// [`MAX_DEPTH_PRODUCER_BUFFER_BYTES`]. Rows rescued to the spill tier
    /// rather than accumulated.
    ///
    /// Its own arm and not `SinkGone`, because the writer is alive and well --
    /// reporting "the writer thread is gone" here would send an operator to
    /// diagnose a thread that is running fine.
    WidthCapped(usize),
    /// The writer thread is gone. Rows rescued to the spill tier.
    SinkGone(usize),
}

/// The network half of a split [`DepthWriter`] -- owns the ILP `Sender`.
///
/// Lives on its own OS thread. It never touches the aggregator, the ring, or
/// anything the drain owns, which is the entire reason the split exists: a
/// five-second ILP timeout now blocks a thread whose only job is waiting, not
/// the thread that must keep emptying the socket.
pub struct DepthWriterSink {
    sender: Option<Sender>,
    feed: Feed,
    spill_dir: PathBuf,
}

impl DepthWriterSink {
    /// Writes one batch. Returns the rows that actually LANDED in QuestDB.
    ///
    /// Zero on any failure -- the same contract `DepthWriter::flush` has, and
    /// for the same reason: the caller reports feed health from this number, so
    /// a failed write must decay health rather than forge it.
    ///
    /// A failure rescues the payload to the depth spill tier through the same
    /// counters and the same coded error `discard_pending` uses, so an operator
    /// sees no difference between a synchronous and an offloaded rescue.
    ///
    /// The single retry of the synchronous path is deliberately NOT reproduced
    /// here. That retry exists because the synchronous flush ran on the drain
    /// and a discarded buffer was the cheaper of two bad options; off the drain
    /// the rescue tier is strictly better than a retry -- it is durable,
    /// counted, and re-ingestable, and it cannot stall anything. Keeping the
    /// retry would have carried a drain-shaped mitigation onto a thread that no
    /// longer has a drain to protect.
    pub fn write(&mut self, batch: &mut DepthFlushBatch) -> usize {
        if batch.rows == 0 {
            return 0;
        }
        let Some(sender) = self.sender.as_mut() else {
            self.rescue(batch, "no ILP sender (QuestDB unreachable)");
            return 0;
        };
        match sender.flush(&mut batch.buffer) {
            Ok(()) => {
                let landed = batch.rows;
                batch.rows = 0;
                landed
            }
            Err(err) => {
                let why = format!("{err}");
                self.rescue(batch, &why);
                0
            }
        }
    }

    /// Rescues a batch the network refused, exactly as `discard_pending` does.
    fn rescue(&mut self, batch: &mut DepthFlushBatch, why: &str) {
        let rows = batch.rows;
        if rows == 0 {
            return;
        }
        let payload_len = batch.buffer.as_bytes().len();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0_i64, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        match spill_failed_depth_ilp(
            &self.spill_dir,
            batch.buffer.as_bytes(),
            self.feed,
            now,
            depth_spill_max_bytes(),
        ) {
            Ok(path) => {
                metrics::counter!("tv_depth_rows_dropped_total", "feed" => self.feed.as_str())
                    .increment(rows as u64);
                metrics::counter!("tv_depth_rows_spilled_total", "feed" => self.feed.as_str())
                    .increment(rows as u64);
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    feed = self.feed.as_str(),
                    rescued = rows,
                    bytes = payload_len,
                    reason = why,
                    path = %path.display(),
                    "offloaded market_depth flush failed — the levels were RESCUED to the \
                     depth spill file named here, not lost. They are NOT in QuestDB yet. \
                     Re-ingest is one command and is safe to repeat, because the depth \
                     dedup key carries depth_kind and capture_seq: \
                     curl --data-binary @<path> http://<questdb>:9000/write"
                );
            }
            Err(err) => {
                if err.kind() == std::io::ErrorKind::StorageFull {
                    metrics::counter!("tv_depth_spill_write_errors_total", "stage" => "cap")
                        .increment(1);
                } else {
                    metrics::counter!("tv_depth_spill_write_errors_total", "stage" => "write")
                        .increment(1);
                }
                metrics::counter!("tv_depth_rows_dropped_total", "feed" => self.feed.as_str())
                    .increment(rows as u64);
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    feed = self.feed.as_str(),
                    dropped = rows,
                    reason = why,
                    cap_bytes = depth_spill_max_bytes(),
                    spill_dir = %self.spill_dir.display(),
                    spill_error = %err,
                    "offloaded market_depth flush failed AND the depth spill rescue also \
                     failed — these levels are permanently lost and nothing re-inserts \
                     them. The raw frames remain in the write-ahead log for manual recovery."
                );
            }
        }
        batch.buffer.clear();
        batch.rows = 0;
    }
}
#[cfg(test)]
mod tests {

    /// The retry window must be far above an interrupted syscall and far below
    /// The named timeout and the conf literal must never drift apart.
    ///
    /// [`ILP_REQUEST_TIMEOUT_SECS`] exists so the shutdown budget can reason
    /// about how long a tail flush may block before the offload join starts. If
    /// someone raises the conf literal and leaves the constant behind, that
    /// reasoning silently becomes wrong and the lane's join gets abandoned
    /// mid-flight again — the exact failure the budget assert was added to
    /// prevent, arriving through the one door the assert cannot watch.
    ///
    /// Source-scanned for the same reason the retry-window test below is: the
    /// number that matters is the LITERAL, so a change to it must invalidate
    /// this test rather than be papered over by a fixture.
    #[test]
    fn ilp_request_timeout_matches_the_conf_literal() {
        let src = include_str!("depth_persistence.rs");
        let expected = format!("request_timeout={}", ILP_REQUEST_TIMEOUT_SECS * 1000);
        assert!(
            src.contains(&expected),
            "ILP_REQUEST_TIMEOUT_SECS ({ILP_REQUEST_TIMEOUT_SECS}s) implies the conf \
             literal `{expected}`, which is not present. The shutdown budget in main.rs \
             derives its margin from this constant, so a drift here makes that budget \
             wrong without failing anything else."
        );
    }

    /// the sender's own request timeout — that gap is the whole reason a clock
    /// can separate two failures the error class cannot.
    #[test]
    fn the_retry_window_sits_between_an_eintr_and_the_request_timeout() {
        // Source-scanned rather than built from a config fixture: the number
        // the window is reasoned against is the LITERAL in `depth_ilp_http_conf`,
        // and a change to it must invalidate this test.
        let src = include_str!("depth_persistence.rs");
        assert!(
            src.contains("request_timeout=5000"),
            "the retry window is reasoned against a 5,000 ms request timeout; if that \
             literal moved, re-derive the window instead of re-blessing this test"
        );
        assert!(
            DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW_MS < 5_000,
            "a window at or above the request timeout would allow the retry after a \
             timeout, doubling a 5s stall on the drain task to 10s"
        );
        assert!(
            DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW_MS >= 100,
            "a window this tight would refuse the EINTR retry the 2026-08-25 8,810-row \
             loss is the evidence for"
        );
    }
    use super::*;

    // ---- flush-failure classification (2026-08-25) ----------------------
    //
    // On 2026-08-25 at 13:12:07 a depth flush failed with
    // `io: Interrupted system call (os error 4)` and 8,810 rows left the
    // buffer. Nothing had been rejected; a signal had interrupted a syscall.
    // These pin which failures earn a retry and, more importantly, which
    // must NOT -- retrying a server rejection is the wedge the discard was
    // built to prevent, so widening this predicate re-opens that.

    #[test]
    fn a_transport_failure_is_retried() {
        // The observed 2026-08-25 error, verbatim. `from_ureq_error` maps IO
        // and timeout failures onto SocketError.
        let eintr = questdb::Error::new(
            questdb::ErrorCode::SocketError,
            "Could not flush buffer: io: Interrupted system call (os error 4)",
        );
        assert!(
            flush_failure_is_retryable(&eintr),
            "EINTR left the buffer intact and the server holding nothing — retry it"
        );
    }

    #[test]
    fn a_server_rejection_is_never_retried() {
        // THE reason the discard exists. A row the server refused would be
        // re-sent forever and block every later row, so this must stay false
        // however the retry predicate is edited.
        let rejected = questdb::Error::new(
            questdb::ErrorCode::ServerFlushError,
            "failed to parse line protocol: invalid field format",
        );
        assert!(
            !flush_failure_is_retryable(&rejected),
            "a rejected row must be discarded, not re-sent — this is the wedge case"
        );
    }

    #[test]
    fn our_own_bugs_are_never_retried() {
        // A bad name or timestamp fails identically forever; retrying buys a
        // second stall and no rows.
        for code in [
            questdb::ErrorCode::InvalidName,
            questdb::ErrorCode::InvalidTimestamp,
            questdb::ErrorCode::InvalidApiCall,
            questdb::ErrorCode::ConfigError,
            questdb::ErrorCode::AuthError,
        ] {
            let err = questdb::Error::new(code, "deterministic failure");
            assert!(
                !flush_failure_is_retryable(&err),
                "{code:?} is deterministic — retrying stalls the flush path for nothing"
            );
        }
    }

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

    // -- the durable floor (2026-08-25) -------------------------------------
    //
    // Before this, `discard_pending` threw the pending buffer away and its own
    // comment said the levels were "gone from the table". Depth is 24× the
    // tick volume and QuestDB write stalls are measured on the live box, so
    // this is the largest data-loss surface in the process. Each test below
    // was written against the OLD behaviour first and failed there.

    fn spill_tmp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("tv-depth-spill-{tag}-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn spill_files(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        out.sort();
        out
    }

    /// Extracts the eight [`DEDUP_KEY_MARKET_DEPTH`] columns from one ILP line.
    ///
    /// Returns `None` when any key column is missing — which is the point: a
    /// line that omits `capture_seq` or `depth_kind` cannot be replayed
    /// idempotently, and the test that uses this would rather fail than
    /// silently compare short keys.
    fn dedup_key_of_ilp_line(line: &str) -> Option<String> {
        let (head, ts) = line.rsplit_once(' ')?;
        let (symbols, fields) = head.split_once(' ')?;
        let mut parts: Vec<String> = vec![format!("ts={}", ts.trim())];
        for col in DEDUP_KEY_MARKET_DEPTH.split(',').map(str::trim) {
            if col == "ts" {
                continue;
            }
            let needle = format!("{col}=");
            let hay = symbols
                .split(',')
                .chain(fields.split(','))
                .find(|kv| kv.starts_with(&needle))?;
            parts.push(hay.to_string());
        }
        Some(parts.join("|"))
    }

    /// The drain must not write the depth rescue file itself.
    ///
    /// The tick twin of this was fixed earlier the same day; this is the same
    /// defect on the writer that carries 24x the rows, reachable from the frame
    /// arm, the 500 ms timer arm AND the shutdown tail.
    #[test]
    fn a_split_depth_writer_hands_the_rescue_off_instead_of_writing_it() {
        let dir = spill_tmp("depth-rescue-handoff");
        let mut w = DepthWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        let (_sink, rx) = w.split_rescue_offload();

        w.append_row(&row()).expect("append");
        let rescued = w.discard_pending();

        assert_eq!(rescued, 1, "the rows are accounted for either way");
        assert_eq!(w.pending(), 0, "the producer must let go of them");
        assert_eq!(
            w.rescued(),
            1,
            "a queued payload IS on its way to the spill file — the caller's \
             log-wording branch reads this one line later"
        );
        let batch = rx
            .try_recv()
            .expect("the payload must be on the rescue queue, not on disk");
        assert_eq!(batch.rows(), 1);
        assert_eq!(
            spill_files(&dir).len(),
            0,
            "the drain must not have touched the depth spill directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A full depth rescue queue falls back to the OLD behaviour, never a drop.
    #[test]
    fn a_full_depth_rescue_queue_writes_inline_rather_than_dropping() {
        let dir = spill_tmp("depth-rescue-full");
        let mut w = DepthWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        let (_sink, _rx) = w.split_rescue_offload();

        for _ in 0..DEPTH_RESCUE_QUEUE_DEPTH {
            w.append_row(&row()).expect("append");
            assert_eq!(w.discard_pending(), 1);
        }
        w.append_row(&row()).expect("append");
        assert_eq!(w.discard_pending(), 1);

        assert!(
            spill_files(&dir).len() > 0,
            "with the queue full the rescue must have been written inline"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A depth writer that was never split behaves exactly as before.
    #[test]
    fn an_unsplit_depth_writer_still_rescues_inline() {
        let dir = spill_tmp("depth-rescue-unsplit");
        let mut w = DepthWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        w.append_row(&row()).expect("append");

        assert_eq!(w.discard_pending(), 1);
        assert!(
            spill_files(&dir).len() > 0,
            "no rescue channel means the old synchronous path, unchanged"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Closing the depth rescue queue restores the inline path, not a refusal.
    #[test]
    fn closing_the_depth_rescue_queue_restores_the_inline_path() {
        let dir = spill_tmp("depth-rescue-closed");
        let mut w = DepthWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        let (_sink, _rx) = w.split_rescue_offload();
        w.close_rescue_offload();

        w.append_row(&row()).expect("append");
        assert_eq!(w.discard_pending(), 1);
        assert!(
            spill_files(&dir).len() > 0,
            "after close the rescue must still land on disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both rescue queues are shallow, and identically so: every payload in one
    /// is rows that exist nowhere else in the process.
    #[test]
    fn the_depth_rescue_queue_matches_the_tick_one() {
        assert_eq!(
            DEPTH_RESCUE_QUEUE_DEPTH,
            crate::tick_persistence::RESCUE_QUEUE_DEPTH,
            "two writers with the same failure mode must not drift into \
             different crash-loss windows"
        );
    }

    #[test]
    fn a_failed_flush_leaves_the_rows_recoverable_on_disk_not_merely_counted() {
        // The defect: a dropped-counter increment is NOT a durable floor. This
        // asserts the ROWS survive, which the old discard could never satisfy.
        let dir = spill_tmp("recoverable");
        let mut w = DepthWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        for level in 1..=20 {
            let mut r = row();
            r.level = level;
            w.append_row(&r).expect("append");
        }
        let _ = w.flush();

        let files = spill_files(&dir);
        assert_eq!(files.len(), 1, "exactly one depth spill file: {files:?}");
        let body = std::fs::read_to_string(&files[0]).expect("spill readable");
        assert_eq!(
            body.lines().filter(|l| !l.trim().is_empty()).count(),
            20,
            "every buffered level must be on disk, not merely counted: {body}"
        );
        assert!(body.contains("depth_kind=d20") && body.contains(MARKET_DEPTH_TABLE));
        assert_eq!(w.rescued(), 20, "the rescue is counted as recoverable");
        assert_eq!(
            w.dropped(),
            20,
            "the EMF-shipped series still sees rows leaving the buffer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_spill_file_is_replayable_by_the_existing_tick_replay_contract() {
        // Reuse, not reinvention: `tick_spill_replay` drains every file with
        // this extension by POSTing it verbatim to /write. If that module
        // renames the extension, this fails rather than silently orphaning the
        // depth spill.
        let dir = spill_tmp("extension");
        let mut w = DepthWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        w.append_row(&row()).expect("append");
        let _ = w.flush();
        let files = spill_files(&dir);
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].extension().and_then(|e| e.to_str()),
            Some(SPILL_FILE_EXTENSION),
            "the depth spill must be drainable by tick_spill_replay: {files:?}"
        );
        assert_eq!(
            crate::tick_spill_replay::list_spill_files(&dir).len(),
            1,
            "the shared replay lister must SEE the depth spill file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaying_a_spilled_batch_twice_cannot_duplicate_rows() {
        // Idempotency is a property of the KEY, so this asserts the key: every
        // one of the eight DEDUP columns is present on every emitted line, and
        // the same batch spilled twice yields the same key SET. Drop
        // `capture_seq` or `depth_kind` from the line and this fails — which
        // is exactly the collapse the single shared table risks.
        let dir = spill_tmp("idempotent");
        let mut w = DepthWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        for level in 1..=5 {
            for (kind, side) in [
                (DEPTH_KIND_20, DEPTH_SIDE_BID),
                (DEPTH_KIND_200, DEPTH_SIDE_BID),
                (DEPTH_KIND_20, DEPTH_SIDE_ASK),
            ] {
                let mut r = row();
                r.level = level;
                r.depth_kind = kind;
                r.side = side;
                w.append_row(&r).expect("append");
            }
        }
        let _ = w.flush();
        // Second failed flush of the SAME rows — a real replay: the drain
        // re-folds the same WAL frame, so capture_seq is identical.
        for level in 1..=5 {
            for (kind, side) in [
                (DEPTH_KIND_20, DEPTH_SIDE_BID),
                (DEPTH_KIND_200, DEPTH_SIDE_BID),
                (DEPTH_KIND_20, DEPTH_SIDE_ASK),
            ] {
                let mut r = row();
                r.level = level;
                r.depth_kind = kind;
                r.side = side;
                w.append_row(&r).expect("append");
            }
        }
        let _ = w.flush();

        let files = spill_files(&dir);
        let body: String = files
            .iter()
            .map(|p| std::fs::read_to_string(p).expect("readable"))
            .collect();
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 30, "both batches are on disk");
        let mut keys = std::collections::BTreeSet::new();
        for line in &lines {
            let key = dedup_key_of_ilp_line(line)
                .unwrap_or_else(|| panic!("every DEDUP key column must be on the line: {line}"));
            keys.insert(key);
        }
        assert_eq!(
            keys.len(),
            15,
            "re-applying the same batch must UPSERT onto itself, not duplicate: {keys:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_spill_write_failure_is_counted_and_coded_never_silent() {
        // The spill shares the filesystem with QuestDB, which was 86% full on
        // 2026-08-24 — so this arm is a live path. A regular FILE where the
        // directory should be makes `create_dir_all` fail deterministically on
        // every OS (the chaos suite's own "spill disk dead" injection).
        let dir = spill_tmp("write-fail");
        std::fs::create_dir_all(dir.parent().expect("temp dir has a parent")).ok();
        std::fs::write(&dir, b"not a directory").expect("plant a file at the dir path");

        let mut w = DepthWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        w.append_row(&row()).expect("append");
        let err = w.flush().expect_err("disconnected writer cannot flush");
        assert!(format!("{err}").contains("1 row(s) discarded"));
        assert_eq!(w.dropped(), 1, "the loss is COUNTED");
        assert_eq!(
            w.rescued(),
            0,
            "a failed rescue must NEVER be reported as recoverable — that is \
             the false-OK this tier exists to avoid"
        );
        assert!(
            spill_failed_depth_ilp(&dir, b"x\n", Feed::Dhan, 0, DEPTH_SPILL_MAX_BYTES).is_err(),
            "the spill helper reports the failure rather than claiming success"
        );
        let _ = std::fs::remove_file(&dir);
    }
    #[test]
    fn the_cap_is_a_soft_rail_measured_against_free_space_not_total_size() {
        // RE-BLESSED 2026-09-01, and the reason matters more than the edit.
        //
        // This test used to assert that reaching the cap REFUSES, full stop.
        // That is the behaviour that discarded 238,615,500 depth rows in
        // production on 2026-09-01 with 136 GB free, because the cap was
        // derived from the volume's TOTAL size -- a number that never moves,
        // so it fired identically on an empty disk and a full one.
        //
        // The rail's INTENT is unchanged and still pinned below: the rescue
        // tier must never starve the database it rescues from. What changed is
        // the quantity the refusal is measured against.
        //
        // Note what the OLD shape made untestable. A unit test cannot
        // manufacture a nearly-full filesystem, so the arm that actually
        // protects the database was the one arm no test could reach -- while
        // the arm that lost the data was the one it pinned. Splitting the
        // decision out as `classify_spill_ceiling` fixes that: every arm is
        // now reachable from a table of numbers, and both are asserted here.
        let dir = spill_tmp("at-cap");
        std::fs::create_dir_all(&dir).expect("mk dir");
        let seeded = b"already-here\n";
        std::fs::write(
            dir.join(format!("depth-dhan-0.{SPILL_FILE_EXTENSION}")),
            seeded,
        )
        .expect("seed");
        let held = depth_spill_dir_bytes(&dir);
        assert_eq!(
            held,
            seeded.len() as u64,
            "the cap is measured from real bytes on disk"
        );

        // Under the cap: accepted. (Unchanged.)
        spill_failed_depth_ilp(&dir, b"under\n", Feed::Dhan, 1_700_000_000, held + 1)
            .expect("below the cap the rescue must succeed -- otherwise the test is vacuous");

        // AT the cap: the outcome must AGREE with the classifier for whatever
        // free space this machine actually has.
        //
        // Written this way after the first draft asserted a fixed ALLOW and
        // failed on a build container with 8.6 GiB free -- under the 16 GiB
        // reserve, so the code was right and the test was wrong. CI runners
        // sit in the same range, so a fixed-outcome assertion here is
        // environment-dependent by construction: it would have passed on the
        // prod box (138 GB free) and failed every CI run, which is the worst
        // possible place to learn it.
        //
        // Agreement is the stronger property anyway. A fixed ALLOW only proves
        // one arm on one machine; this proves the writer genuinely CONSULTS
        // `classify_spill_ceiling` rather than deciding on its own -- on any
        // machine, at whatever free space it happens to have. The arms
        // themselves are pinned exhaustively below, where no filesystem is
        // involved at all.
        use crate::tick_persistence::{
            SPILL_SOFT_CEILING_FREE_RESERVE_BYTES, SpillCeilingVerdict, classify_spill_ceiling,
            spill_free_bytes,
        };
        let expected = classify_spill_ceiling(
            depth_spill_dir_bytes(&dir),
            held,
            spill_free_bytes(&dir),
            SPILL_SOFT_CEILING_FREE_RESERVE_BYTES,
        );
        let outcome = spill_failed_depth_ilp(&dir, b"over\n", Feed::Dhan, 1_700_000_000, held);
        match expected {
            SpillCeilingVerdict::OverCeilingWithRoom => {
                outcome.expect(
                    "this machine has room, so the rescue must be ALLOWED -- refusing \
                     with room is what discarded 238,615,500 rows onto a 55%-empty disk",
                );
                let body = std::fs::read_to_string(
                    dir.join(format!("depth-dhan-472222.{SPILL_FILE_EXTENSION}")),
                )
                .unwrap_or_default();
                assert!(
                    body.contains("over"),
                    "an allowed rescue must actually have written the row: {body}"
                );
            }
            SpillCeilingVerdict::OverCeilingNoRoom
            | SpillCeilingVerdict::OverCeilingProbeFailed => {
                let err = outcome.expect_err(
                    "this machine is at or below the database reserve, so the rescue \
                     must REFUSE -- that is the case the rail exists for",
                );
                assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);
                // The refusal is a REFUSAL, not a partial write.
                let body = std::fs::read_to_string(
                    dir.join(format!("depth-dhan-472222.{SPILL_FILE_EXTENSION}")),
                )
                .unwrap_or_default();
                assert!(
                    !body.contains("over"),
                    "a refused rescue must write NOTHING, not a truncated row: {body}"
                );
            }
            SpillCeilingVerdict::UnderCeiling => panic!(
                "the fixture seeds the directory TO the cap, so this arm is \
                 unreachable -- if it fires, the seeding no longer works and \
                 every assertion above it is vacuous"
            ),
        }

        // AT the cap on a disk WITHOUT room: still REFUSED. This is the arm
        // the rail exists for, and the arm the old shape could never test.
        assert_eq!(
            classify_spill_ceiling(
                held,
                held,
                Some(SPILL_SOFT_CEILING_FREE_RESERVE_BYTES),
                SPILL_SOFT_CEILING_FREE_RESERVE_BYTES
            ),
            SpillCeilingVerdict::OverCeilingNoRoom,
            "exactly ON the reserve must refuse -- at a boundary the safe \
             direction is the database's"
        );
        assert_eq!(
            classify_spill_ceiling(
                held,
                held,
                Some(SPILL_SOFT_CEILING_FREE_RESERVE_BYTES - 1),
                SPILL_SOFT_CEILING_FREE_RESERVE_BYTES
            ),
            SpillCeilingVerdict::OverCeilingNoRoom,
            "below the reserve must refuse -- this is the whole point of the rail"
        );
        assert_eq!(
            classify_spill_ceiling(held, held, None, SPILL_SOFT_CEILING_FREE_RESERVE_BYTES),
            SpillCeilingVerdict::OverCeilingProbeFailed,
            "an UNREADABLE free-space number must refuse. Optimism here trades \
             a bounded tick loss for an unbounded disk, which is the outage \
             this tier exists to avoid"
        );
        assert_eq!(
            classify_spill_ceiling(
                held,
                held,
                Some(SPILL_SOFT_CEILING_FREE_RESERVE_BYTES + 1),
                SPILL_SOFT_CEILING_FREE_RESERVE_BYTES
            ),
            SpillCeilingVerdict::OverCeilingWithRoom,
            "one byte over the reserve is room, and must be allowed"
        );
        assert_eq!(
            classify_spill_ceiling(held - 1, held, None, SPILL_SOFT_CEILING_FREE_RESERVE_BYTES),
            SpillCeilingVerdict::UnderCeiling,
            "below the rail the probe must not even be consulted"
        );

        // And production really does pass the const, so the bound above is the
        // one that fires on the box.
        assert!(
            DEPTH_SPILL_MAX_BYTES > 0 && DEPTH_SPILL_MAX_BYTES <= 1024 * 1024 * 1024,
            "the depth spill bound must be explicit and sane: {DEPTH_SPILL_MAX_BYTES}"
        );
        assert_eq!(DEPTH_SPILL_DIR, "data/spill/depth");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_flush_writes_no_spill_file_at_all() {
        let dir = spill_tmp("empty");
        let mut w = DepthWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        w.flush().expect("nothing pending is not a failure");
        assert!(
            spill_files(&dir).is_empty(),
            "an empty buffer must not create a rescue file"
        );
        assert_eq!(w.rescued(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn for_test_never_writes_into_the_production_spill_directory() {
        // `for_test` is called from the app crate too, where cfg(test) does not
        // reach — so the isolation has to be in the constructor.
        let w = DepthWriter::for_test(Feed::Dhan);
        assert_ne!(w.spill_dir, PathBuf::from(DEPTH_SPILL_DIR));
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

    // ---- off-drain flush (2026-08-28) -----------------------------------
    //
    // These pin the SEMANTICS, not the plumbing. The plumbing is a bounded
    // channel; the semantics are which arms report loss and which do not, and
    // getting that wrong is how a backpressure signal becomes a false loss
    // report (or, worse, how a real loss reports as healthy).

    #[test]
    fn a_split_writer_hands_the_batch_to_the_queue_instead_of_the_network() {
        let (mut w, _sink, rx) = DepthWriter::for_test(Feed::Dhan).split_for_offload();
        assert!(w.is_offloaded(), "the split must open the hand-off queue");
        w.append_row(&row()).expect("append");
        assert_eq!(w.pending(), 1);

        w.flush().expect("a healthy hand-off is not a failure");

        assert_eq!(w.pending(), 0, "the rows left the producer");
        let batch = rx.try_recv().expect("the batch must be on the queue");
        assert_eq!(batch.rows(), 1, "and it must carry the row count");
    }

    #[test]
    fn a_full_queue_retains_the_rows_and_is_never_reported_as_a_failure() {
        // THE test. A full queue means the writer is behind, not that anything
        // was lost — the producer keeps its buffer and the next flush retries.
        // Reporting this as `Err` would decay feed health and log a loss for a
        // batch that is still entirely in hand, which is the exact false-loss
        // shape this arm exists to prevent.
        let (mut w, _sink, _rx) = DepthWriter::for_test(Feed::Dhan).split_for_offload();
        for _ in 0..DEPTH_FLUSH_QUEUE_DEPTH {
            w.append_row(&row()).expect("append");
            w.flush().expect("fills the queue");
        }
        // The queue is now full and `_rx` is deliberately not drained.
        w.append_row(&row()).expect("append");
        assert_eq!(w.pending(), 1);

        w.flush()
            .expect("backpressure is not a failure — the rows are still held");

        assert_eq!(
            w.pending(),
            1,
            "the row must still be pending, not silently consumed"
        );
    }

    #[test]
    fn retaining_past_the_span_bound_rescues_rather_than_widening_forever() {
        // "Keep appending while the writer is behind" is correct exactly twice.
        // Past that it is an unbounded memory path wearing a backpressure
        // costume, so the producer cuts to the durable tier and says so.
        let dir = temp_depth_spill_dir();
        let (mut w, _sink, _rx) = DepthWriter::for_test(Feed::Dhan)
            .with_spill_dir_for_test(dir.clone())
            .split_for_offload();
        for _ in 0..DEPTH_FLUSH_QUEUE_DEPTH {
            w.append_row(&row()).expect("append");
            w.flush().expect("fills the queue");
        }
        // Every flush from here finds the queue full.
        let mut last = Ok(());
        for _ in 0..=MAX_DEPTH_RETAINED_FLUSH_SPANS {
            w.append_row(&row()).expect("append");
            last = w.flush();
        }
        assert!(
            last.is_err(),
            "the span past the retention bound must report that rows left the buffer"
        );
        assert_eq!(w.pending(), 0, "and the buffer must be empty afterwards");
        let spilled: Vec<_> = std::fs::read_dir(&dir)
            .expect("spill dir")
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(
            !spilled.is_empty(),
            "the rescue must be DURABLE — a width cap that drops rows is worse \
             than the accumulation it prevents"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_writer_thread_rescues_the_rows_and_reports_it() {
        let dir = temp_depth_spill_dir();
        let (mut w, _sink, rx) = DepthWriter::for_test(Feed::Dhan)
            .with_spill_dir_for_test(dir.clone())
            .split_for_offload();
        drop(rx); // the writer thread died
        w.append_row(&row()).expect("append");

        let outcome = w.flush();

        assert!(
            outcome.is_err(),
            "a gone writer moved rows out of the buffer — that is a failure, \
             and reporting Ok here would forge feed health"
        );
        assert_eq!(w.pending(), 0);
        let spilled: Vec<_> = std::fs::read_dir(&dir)
            .expect("spill dir")
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(
            !spilled.is_empty(),
            "a dead writer must rescue, not drop — the rows are re-ingestable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn close_offload_returns_the_writer_to_the_synchronous_arm() {
        // Shutdown ordering: the queue closes so the writer thread's blocking
        // recv ends, and any last rows then take the synchronous arm — which,
        // with the sender long gone to the sink, rescues to disk. Rows on disk
        // and named is the correct end-of-session outcome; a wedged join is not.
        let (mut w, _sink, _rx) = DepthWriter::for_test(Feed::Dhan).split_for_offload();
        assert!(w.is_offloaded());
        w.close_offload();
        assert!(
            !w.is_offloaded(),
            "after close the writer must take the synchronous arm"
        );
    }

    #[test]
    fn the_sink_rescues_a_batch_the_network_refused() {
        // `for_test` has no ILP sender, so the sink's write is the
        // QuestDB-unreachable arm — the same one a real network failure takes.
        let dir = temp_depth_spill_dir();
        let (mut w, mut sink, rx) = DepthWriter::for_test(Feed::Dhan)
            .with_spill_dir_for_test(dir.clone())
            .split_for_offload();
        sink.spill_dir.clone_from(&dir);
        w.append_row(&row()).expect("append");
        w.flush().expect("hand-off");
        let mut batch = rx.try_recv().expect("batch");

        let landed = sink.write(&mut batch);

        assert_eq!(
            landed, 0,
            "a failed write must report ZERO rows landed — the caller derives \
             feed health from this number, so it may decay it but never forge it"
        );
        assert_eq!(batch.rows(), 0, "and the batch must not be re-writable");
        let spilled: Vec<_> = std::fs::read_dir(&dir)
            .expect("spill dir")
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(
            !spilled.is_empty(),
            "the sink rescues exactly as the producer does"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sink_reports_the_rows_that_actually_landed() {
        // Zero rows is not a failure and must not touch the spill tier — an
        // empty flush that wrote a rescue file every 500 ms would fill the
        // 512 MiB cap with nothing.
        let dir = temp_depth_spill_dir();
        let (_w, mut sink, _rx) = DepthWriter::for_test(Feed::Dhan)
            .with_spill_dir_for_test(dir.clone())
            .split_for_offload();
        sink.spill_dir.clone_from(&dir);
        let mut empty = DepthFlushBatch {
            buffer: Buffer::new(questdb::ingress::ProtocolVersion::V1),
            rows: 0,
        };
        assert_eq!(sink.write(&mut empty), 0);
        assert!(
            std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0) == 0,
            "an empty batch must never write a rescue file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_split_takes_the_sender_so_the_producer_cannot_touch_the_network() {
        // The one-way door. If the producer kept a sender, a stray synchronous
        // flush would put a 5 s blocking round trip back on the drain — the
        // exact coupling this change removes — and nothing would catch it.
        let w = DepthWriter::for_test(Feed::Dhan);
        let had_sender = w.sender.is_some();
        let (producer, sink, _rx) = w.split_for_offload();
        assert!(
            producer.sender.is_none(),
            "the producer must not retain the ILP sender after the split"
        );
        assert_eq!(
            sink.sender.is_some(),
            had_sender,
            "and the sink must hold whatever sender there was"
        );
    }

    #[test]
    fn the_depth_producer_ceiling_leaves_headroom_under_the_questdb_wedge() {
        // Const-asserted at the definition too; restated as a test so the
        // reasoning is discoverable from the test list rather than only from a
        // compile error nobody sees until it fires.
        assert!(
            MAX_DEPTH_PRODUCER_BUFFER_BYTES * 2
                <= crate::tick_persistence::QUESTDB_MAX_BUF_SIZE_BYTES,
            "past the questdb-rs max_buf_size EVERY flush fails permanently — a wedge, \
             not a degrade — so the rescue arm must fire well before it"
        );
        assert!(
            MAX_DEPTH_RETAINED_FLUSH_SPANS >= 1,
            "a retention bound of zero spills on the first hiccup, which converts \
             ordinary backpressure into disk writes"
        );
        assert!(
            DEPTH_FLUSH_QUEUE_DEPTH >= 1,
            "a zero-depth queue makes every flush a QueueFull and the split pointless"
        );
    }

    /// The offloaded buffer swap is safe only while every identifier we emit
    /// fits the DEFAULT name limit.
    ///
    /// # The residual this pins
    ///
    /// A healthy writer builds its buffer with `sender.new_buffer()`, which
    /// carries the sender's negotiated `max_name_len`. `offload_flush` replaces
    /// a handed-off buffer with `Buffer::new(protocol)`, which uses the
    /// questdb-rs default of 127 — and `Buffer` exposes no getter, so the
    /// negotiated value cannot be carried across without pooling buffers or
    /// pinning the conf.
    ///
    /// That is a real divergence and it is deliberately NOT engineered around,
    /// because it cannot bite: every table and column name this writer emits is
    /// far under 127 bytes, so both limits validate identically. The failure
    /// would need a server reporting a limit BELOW our longest identifier,
    /// which this test makes impossible to reach silently — add a long column
    /// name and it fires here rather than as whole rejected flushes in prod.
    #[test]
    fn every_depth_identifier_fits_the_default_name_limit() {
        // questdb-rs `MAX_NAME_LEN_DEFAULT`, restated rather than imported: it
        // is not re-exported, and a number that only exists in a dependency's
        // private module is a number nothing here can check.
        const QUESTDB_DEFAULT_MAX_NAME_LEN: usize = 127;
        // A generous floor under it. If an identifier ever approaches the real
        // limit, the design decision above needs revisiting rather than the
        // test re-blessing.
        const HEADROOM_FLOOR: usize = 64;

        let mut names: Vec<&str> = vec![MARKET_DEPTH_TABLE];
        names.extend(DEDUP_KEY_MARKET_DEPTH.split(',').map(str::trim));
        for name in names {
            assert!(
                name.len() <= HEADROOM_FLOOR,
                "`{name}` is {} bytes. The offloaded flush replaces the buffer with one \
                 built at the questdb-rs default limit of {QUESTDB_DEFAULT_MAX_NAME_LEN}, \
                 which is safe only while every identifier sits comfortably under it. \
                 Past this floor, carry the sender's negotiated max_name_len across the \
                 split instead of re-blessing this number.",
                name.len()
            );
        }
    }
}
