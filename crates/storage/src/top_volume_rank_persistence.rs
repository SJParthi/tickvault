//! `top_volume_rank` table — a queryable record of WHICH option contracts
//! were the busiest, and whether we were actually watching them.
//!
//! Operator directive 2026-09-06: *"ensure to capture the top volume gainers
//! of the entire options contracts starting 9.15 am till 3.39 pm ... meanwhile
//! ensure to have it as a separate table to have ticks 1 second and 5 second
//! to have these top volume gainers ... because only using db i can see this
//! right dude"*.
//!
//! ## Why this table exists
//!
//! The ranking itself lives in RAM (`crates/app/src/volume_leaderboard.rs`)
//! and dies with the process. That is correct for the DECISION path — a
//! trading decision never reads the database — but it means three questions
//! were unanswerable after a session ended:
//!
//! * which contracts were actually the heaviest, minute by minute?
//! * did the heaviest contract actually get a depth socket, or did we miss it?
//! * did the ranking churn, or was it stable?
//!
//! The `subscribed` column is what earns the table: it is the difference
//! between "we ranked it first" and "we were watching it". Without it the
//! rows are trivia; with it, one query audits the whole steering decision.
//!
//! ## What a row is
//!
//! ONE row per (snapshot boundary, timeframe, family, contract). The
//! designated `ts` is the SNAPSHOT boundary — a whole second — so a
//! re-emitted snapshot UPSERTs in place rather than duplicating (the
//! scoreboard mechanic). `rank` is deliberately NOT in the DEDUP key: it is
//! the value being recorded, and a contract appears at most once per
//! (snapshot, timeframe, family) already.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS top_volume_rank (
//!     ts TIMESTAMP, tf SYMBOL, family SYMBOL, feed SYMBOL,
//!     segment SYMBOL, rank LONG, security_id LONG,
//!     underlying_id LONG, volume LONG, gain_pct DOUBLE,
//!     subscribed BOOLEAN
//! ) timestamp(ts) PARTITION BY HOUR
//!   DEDUP UPSERT KEYS(ts, tf, family, feed, security_id, segment);
//! ```
//!
//! `PARTITION BY HOUR` matches `ticks` and `market_depth` — this is
//! high-frequency snapshot data and the hour is the archival unit that makes
//! same-day archive-then-drop possible. `underlying_id` is stored as the
//! numeric id we actually hold, NOT a name: the ranking layer never sees a
//! symbol, and inventing one here would be fabrication. Join to
//! `instrument_lifecycle` for names.
//!
//! ## Honest volume
//!
//! At the authorized 250 contracts per family, both families, 09:15-15:39:
//! 500 rows/second x 22,440 s = ~11.2M rows for the `1s` stream plus ~2.2M
//! for `5s` -- ~13.4M rows, ~860 MB per session at the ~64 B row width. A
//! session already writes ~307 GB, so this is ~0.28% -- real, and small.
//! The `5s` rows are numerically a subset of the `1s` rows and are kept
//! anyway because they mean something different: a `5s` row is the ranking
//! that ACTUALLY DROVE a re-steer decision, which is the row an audit wants.
//!
//! RETENTION, stated because it is a standing commitment and not a one-day
//! cost: `HOUR_PARTITIONED_TABLES` membership puts this table in
//! `RetentionClass::MarketData`, whose window is `market_data_hot_days`
//! (default 15). ~860 MB/session x 15 sessions is roughly **13 GB resident**
//! on the 600 GB volume -- about 2%. That is the deliberate trade: 15 days of
//! "was the busiest contract actually watched?" for 2% of the disk. It is
//! NOT exempt from the sweep; an exempt table growing most of a gigabyte a
//! day is the disk-fill class that cost 2026-09-04 an entire session.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;

/// QuestDB table name — one row per (snapshot, timeframe, family, contract).
pub const TOP_VOLUME_RANK_TABLE: &str = "top_volume_rank";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule); `segment`
/// alongside `security_id` (I-P1-11 — the bare id is reused across segments
/// and two instruments would silently collapse into one row); `feed` in-key
/// (operator override 2026-06-28, feed-in-key EVERYWHERE).
///
/// Declared as a `DEDUP_KEY_*` const rather than an inline literal because
/// `dedup_segment_meta_guard` discovers keys by scanning for that name
/// pattern — an inline literal would put this key OUTSIDE the guard.
///
/// `rank` is deliberately ABSENT: it is the recorded value, not part of the
/// identity, and including it would let one contract occupy several rows in
/// one snapshot as its rank moved between a retry and its re-emit.
pub const DEDUP_KEY_TOP_VOLUME_RANK: &str = "ts, tf, family, feed, security_id, segment";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Snapshot cadence label — the `tf` SYMBOL column. Stable wire strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SnapshotCadence {
    /// Every whole second — the fine-grained record.
    OneSecond,
    /// Every five seconds — the boundary at which the depth set is re-steered,
    /// so these rows are the ones that actually drove a subscription decision.
    FiveSecond,
}

impl SnapshotCadence {
    /// Stable wire label. Never reworded — it is a persisted SYMBOL value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OneSecond => "1s",
            Self::FiveSecond => "5s",
        }
    }

    /// How often this cadence fires, in whole seconds.
    ///
    /// The scheduler's timer period comes from HERE rather than from a literal
    /// at the call site, so the interval a snapshot is actually taken at and
    /// the `cadence` SYMBOL it is stored under cannot drift apart. A row
    /// labelled `5s` written every four seconds would be a quiet lie in a
    /// table whose whole purpose is to be queried after the fact.
    #[must_use]
    pub const fn interval_secs(self) -> u64 {
        match self {
            Self::OneSecond => 1,
            Self::FiveSecond => 5,
        }
    }
}

/// One ranked contract at one snapshot boundary, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct TopVolumeRankRow {
    /// Designated timestamp — the SNAPSHOT boundary in IST nanoseconds.
    /// A whole second, so a re-emit UPSERTs in place.
    pub snapshot_ts_ist_nanos: i64,
    /// Snapshot cadence (`1s` / `5s`).
    pub cadence: SnapshotCadence,
    /// Option family — stock and index options are ranked in SEPARATE
    /// leaderboards, and the column records which one this row came from.
    pub family: &'static str,
    /// Feed that produced the volume (`dhan`).
    pub feed: &'static str,
    /// Segment SYMBOL string (`NSE_FNO`, ...). Owned — the caller holds it.
    pub segment: String,
    /// 1-based position within its family at this snapshot.
    pub rank: i64,
    /// The contract's own security id.
    pub security_id: i64,
    /// The underlying's numeric id. NOT a name — see the module header.
    pub underlying_id: i64,
    /// Cumulative day volume as observed, AFTER the monotonicity gate.
    pub volume: i64,
    /// Percentage change from the previous close. Already proven finite by
    /// the ranking layer's `eligible_gain_pct` refusal.
    pub gain_pct: f64,
    /// Whether this contract actually held a depth subscription at this
    /// snapshot. The column that makes the table an audit rather than trivia.
    pub subscribed: bool,
}

/// The idempotent `CREATE TABLE` DDL for `top_volume_rank`. Pure.
#[must_use]
pub fn top_volume_rank_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {TOP_VOLUME_RANK_TABLE} (\
            ts            TIMESTAMP, \
            tf            SYMBOL, \
            family        SYMBOL, \
            feed          SYMBOL, \
            segment       SYMBOL, \
            rank          LONG, \
            security_id   LONG, \
            underlying_id LONG, \
            volume        LONG, \
            gain_pct      DOUBLE, \
            subscribed    BOOLEAN\
        ) timestamp(ts) PARTITION BY HOUR \
        DEDUP UPSERT KEYS({DEDUP_KEY_TOP_VOLUME_RANK});"
    )
}

/// Every non-designated column, for the per-column self-heal ALTER manifest.
/// Kept beside the DDL so `table_schema_lockstep_guard` can compare them.
const TOP_VOLUME_RANK_COLUMNS: &[(&str, &str)] = &[
    ("tf", "SYMBOL"),
    ("family", "SYMBOL"),
    ("feed", "SYMBOL"),
    ("segment", "SYMBOL"),
    ("rank", "LONG"),
    ("security_id", "LONG"),
    ("underlying_id", "LONG"),
    ("volume", "LONG"),
    ("gain_pct", "DOUBLE"),
    ("subscribed", "BOOLEAN"),
];

/// The full idempotent statement list: CREATE, then per-column
/// `ADD COLUMN IF NOT EXISTS`, then `DEDUP ENABLE`. Never a DROP. Pure, so
/// the ordering is unit-testable without a live QuestDB.
#[must_use]
pub fn top_volume_rank_ensure_statements() -> Vec<String> {
    let mut statements = vec![top_volume_rank_create_ddl()];
    for (col, ty) in TOP_VOLUME_RANK_COLUMNS {
        statements.push(format!(
            "ALTER TABLE {TOP_VOLUME_RANK_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {TOP_VOLUME_RANK_TABLE} DEDUP ENABLE \
         UPSERT KEYS({DEDUP_KEY_TOP_VOLUME_RANK});"
    ));
    statements
}

/// Creates the `top_volume_rank` table if absent (schema-self-heal order:
/// CREATE -> per-column ALTER -> DEDUP ENABLE; never a table drop).
///
/// Fail-SOFT: every failure logs at `error!` with `STORAGE-GAP-03` and
/// returns. The honest consequence, stated rather than hidden: a failed
/// ensure leaves the table to be auto-created by the first ILP write WITHOUT
/// `DEDUP UPSERT KEYS` — a duplicate-row window until a later ensure
/// succeeds. Blocking the boot instead would trade a duplicate-row window
/// for no session at all.
// TEST-EXEMPT: live-QuestDB DDL runner; the statement list it sends is pure and is asserted by the ensure-statement tests below.
pub async fn ensure_top_volume_rank_table(questdb_config: &QuestDbConfig) {
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
            error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                stage = "ensure_client_build",
                ?err,
                "STORAGE-GAP-03: HTTP client build failed — top_volume_rank not \
                 ensured (the first ILP write may auto-create it WITHOUT dedup, \
                 a duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    for ddl in &top_volume_rank_ensure_statements() {
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
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "STORAGE-GAP-03: top_volume_rank DDL returned non-2xx \
                     (dedup may be missing — duplicate-row window)"
                );
            }
            Err(err) => {
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "STORAGE-GAP-03: top_volume_rank DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf — per-flush server ACK (the 2026-07-05
/// fire-and-forget lesson): `retry_timeout=0` so questdb-rs does not run its
/// own sleep-and-resend loop inside our flush, and a bounded
/// `request_timeout` so a stalled server cannot hold the caller.
fn top_volume_rank_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// ILP-over-HTTP writer for `top_volume_rank`.
///
/// Lazy: an unreachable QuestDB at construction still builds and buffers
/// locally. On ANY failed flush the pending buffer is DISCARDED rather than
/// retained — a server-REJECTED row kept across flushes replays forever and
/// poisons every later flush with it. Discarding is safe HERE specifically
/// because these rows are an observability record of a ranking that is
/// recomputed from scratch on the next tick: nothing downstream depends on
/// a snapshot having been written, and the loss is loud.
pub struct TopVolumeRankWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
    /// Set by [`TopVolumeRankWriter::split_for_offload`]. When present, `flush`
    /// hands the buffer to the writer thread instead of touching the network.
    ///
    /// `None` means this writer still flushes synchronously — the shape that
    /// must never reach the frame drain, because a 5,000 ms `request_timeout`
    /// on the drain task stalls the fold, fills the socket receive buffer, and
    /// Dhan skips a slow consumer forward to "the latest available state" with
    /// no sequence number. The ticks lost that way are lost at THEIR side and
    /// are invisible to every counter we own. That is the 2026-08-28 depth
    /// lesson, and this field is how this table avoids repeating it.
    offload: Option<std::sync::mpsc::SyncSender<TopVolumeFlushBatch>>,
    /// Consecutive flushes the producer has retained because the queue was
    /// full. Bounded by [`MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS`].
    retained_spans: u32,
}

impl TopVolumeRankWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor; the lazy contract and every append/flush path are covered via for_test() below.
    pub fn new(config: &QuestDbConfig) -> Self {
        // Seed the discard series at zero BEFORE the first row can be
        // dropped. A counter never incremented is ABSENT from the exporter,
        // not zero — and an absent series reads as health. If this name is
        // ever EMF-selected, the CloudWatch agent's dropped-first-sample rule
        // turns that into total silence on the one episode that matters; that
        // rule cost 104,540 depth rows their classification on 2026-08-28.
        metrics::counter!("tv_top_volume_rank_rows_discarded_total").increment(0);
        let conf = top_volume_rank_ilp_http_conf(config);
        match Sender::from_conf(&conf) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                    offload: None,
                    retained_spans: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "top_volume_rank writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                    offload: None,
                    retained_spans: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
            offload: None,
            retained_spans: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Test-only view of the ILP buffer bytes (shape assertions).
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one ranked-contract row.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &TopVolumeRankRow) -> Result<()> {
        self.buffer
            .table(TOP_VOLUME_RANK_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("tf", r.cadence.as_str())
            .context("tf")?
            .symbol("family", r.family)
            .context("family")?
            .symbol("feed", r.feed)
            .context("feed")?
            .symbol("segment", r.segment.as_str())
            .context("segment")?
            .column_i64("rank", r.rank)
            .context("rank")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_i64("underlying_id", r.underlying_id)
            .context("underlying_id")?
            .column_i64("volume", r.volume)
            .context("volume")?
            .column_f64("gain_pct", r.gain_pct)
            .context("gain_pct")?
            .column_bool("subscribed", r.subscribed)
            .context("subscribed")?
            .at(TimestampNanos::new(r.snapshot_ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK).
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (pending discarded).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        // OFF-DRAIN arm first: once split, the network is the sink thread's
        // problem and this call must not touch it.
        if self.offload.is_some() {
            return match self.offload_flush() {
                // Handed off, or held for the next flush — neither is an error
                // to the caller. `QueueFull` in particular is BACKPRESSURE:
                // reporting it as a failure would decay a health signal for
                // rows that are still safely held.
                TopVolumeOffloadOutcome::Sent(_) | TopVolumeOffloadOutcome::QueueFull(_) => Ok(()),
                TopVolumeOffloadOutcome::WidthCapped(dropped) => anyhow::bail!(
                    "top_volume_rank: writer thread stayed behind — {dropped} pending \
                     row(s) dropped (no spill tier for a periodic snapshot)"
                ),
                TopVolumeOffloadOutcome::SinkGone(dropped) => anyhow::bail!(
                    "top_volume_rank: writer thread is gone — {dropped} pending row(s) dropped"
                ),
            };
        }
        if self.sender.is_none() {
            let dropped = self.discard_pending();
            anyhow::bail!(
                "top_volume_rank: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending row(s) discarded"
            );
        }
        let flushed = self
            .sender
            .as_mut()
            .map(|sender| sender.flush(&mut self.buffer));
        match flushed {
            Some(Ok(())) => {
                self.pending = 0;
                Ok(())
            }
            Some(Err(err)) => {
                let dropped = self.discard_pending();
                Err(anyhow::Error::new(err).context(format!(
                    "top_volume_rank ILP flush failed — {dropped} pending row(s) \
                     discarded (poisoned-buffer defense)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!(
                    "top_volume_rank: sender vanished — {dropped} pending row(s) discarded"
                );
            }
        }
    }

    /// Splits this writer into a drain-side PRODUCER and a thread-side SINK.
    ///
    /// After this, `flush` hands the buffer to a bounded queue and returns
    /// without touching the network. That is the whole point: the caller is the
    /// frame drain, and a 5,000 ms `request_timeout` on the drain task stalls
    /// the fold — after which the socket receive buffer fills and Dhan skips a
    /// slow consumer forward with no sequence number, losing ticks at THEIR
    /// side where no counter of ours can see it.
    ///
    /// Consuming `self` makes the split a ONE-WAY DOOR at the type level: there
    /// is no way to keep a synchronous handle to the same writer and
    /// accidentally flush it on the drain later. The tick and depth writers use
    /// the same signature for the same reason.
    #[must_use]
    // TEST-EXEMPT: the split itself is exercised by every offload test below.
    pub fn split_for_offload(
        mut self,
    ) -> (
        Self,
        TopVolumeRankWriterSink,
        std::sync::mpsc::Receiver<TopVolumeFlushBatch>,
    ) {
        let (tx, rx) = std::sync::mpsc::sync_channel(TOP_VOLUME_FLUSH_QUEUE_DEPTH);
        let sink = TopVolumeRankWriterSink {
            sender: self.sender.take(),
        };
        self.offload = Some(tx);
        (self, sink, rx)
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
    /// Uses `try_send`, NEVER `send`: a blocking send would re-create the exact
    /// coupling the split exists to remove, one queue further out.
    fn offload_flush(&mut self) -> TopVolumeOffloadOutcome {
        let rows = self.pending;
        // Read the protocol version BEFORE the replace: a fresh buffer must
        // speak the same protocol the sender negotiated, and borrowing rules
        // will not let both happen in one expression.
        let protocol = self.buffer.protocol_version();
        let batch = TopVolumeFlushBatch {
            buffer: std::mem::replace(&mut self.buffer, Buffer::new(protocol)),
            rows,
        };
        let Some(tx) = self.offload.as_ref() else {
            // Unreachable — `flush` checks. Treated as the gone arm rather than
            // silently succeeding, because "we sent it" when nothing was sent
            // is the one report that must never be wrong.
            self.buffer = batch.buffer;
            return TopVolumeOffloadOutcome::SinkGone(rows);
        };
        match tx.try_send(batch) {
            Ok(()) => {
                self.pending = 0;
                self.retained_spans = 0;
                metrics::counter!("tv_top_volume_rank_flush_offloaded_total").increment(1);
                TopVolumeOffloadOutcome::Sent(rows)
            }
            Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                // Backpressure, not loss. Put the rows BACK and keep appending
                // — the next flush retries. This arm is what makes the bounded
                // queue safe: without it a full queue would either block the
                // drain (the original defect) or drop rows (a worse one).
                metrics::counter!("tv_top_volume_rank_flush_queue_full_total").increment(1);
                let held = returned.buffer.as_bytes().len();
                self.buffer = returned.buffer;
                self.retained_spans = self.retained_spans.saturating_add(1);
                // TWO independent cuts, spans and bytes. `>` and not `>=`: the
                // constant names how many spans may be RETAINED, so the cut
                // belongs on the span AFTER them.
                if self.retained_spans > MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS
                    || held >= MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES
                {
                    metrics::counter!("tv_top_volume_rank_flush_width_capped_total").increment(1);
                    self.retained_spans = 0;
                    // No spill tier for this table — see
                    // `TopVolumeOffloadOutcome::WidthCapped` for why dropping
                    // is the right call here and rescuing is not.
                    let dropped = self.discard_pending();
                    return TopVolumeOffloadOutcome::WidthCapped(dropped);
                }
                TopVolumeOffloadOutcome::QueueFull(rows)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(returned)) => {
                self.buffer = returned.buffer;
                let dropped = self.discard_pending();
                TopVolumeOffloadOutcome::SinkGone(dropped)
            }
        }
    }

    /// Drops the pending buffer, counts and LOGS the loss, returns the count.
    fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_top_volume_rank_rows_discarded_total").increment(dropped as u64);
            // The counter is NOT EMF-selected — deliberately. Shipping a new
            // metric name costs ~$0.30/mo against a September forecast of
            // $142.24 with the automatic STOP_EC2_INSTANCES line at $135.00,
            // and this table is an observability record whose loss costs no
            // tick. `loss_counter_visibility_guard` accepts the LOGGED route,
            // and this is it: the error! below sits beside the increment.
            error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                metric = "tv_top_volume_rank_rows_discarded_total",
                dropped,
                "STORAGE-GAP-03: top_volume_rank rows discarded after a failed \
                 flush — the ranking record has a hole for those snapshots. No \
                 tick is lost by this: the leaderboard is in RAM and the next \
                 snapshot rebuilds it."
            );
        }
        self.buffer.clear();
        self.pending = 0;
        dropped
    }
}

// ---------------------------------------------------------------------------
// Off-drain WRITE for top_volume_rank (2026-09-07)
// ---------------------------------------------------------------------------

/// Depth of the hand-off queue between the drain and the top-volume writer.
///
/// FOUR, the same as [`crate::depth_persistence::DEPTH_FLUSH_QUEUE_DEPTH`] and
/// deliberately not tuned differently: one number is one thing to keep true,
/// and the three writer paths sharing a shape is what stops them drifting into
/// different failure semantics.
///
/// What four batches actually buy here is far more than on the depth path,
/// because the volume is not comparable. Depth is a MEASURED ~63,800 rows/s;
/// this table emits one snapshot per cadence — 250 rows at 1 s and 5 at 5 s,
/// so ~255 rows/s, roughly 250x less. Four batches is therefore ~4 seconds of
/// stall absorbed rather than depth's ~600 ms. The queue should never fill in
/// practice; bounded is still bounded.
pub const TOP_VOLUME_FLUSH_QUEUE_DEPTH: usize = 4;

/// Consecutive full-queue flushes the producer may RETAIN before it stops
/// widening the buffer and drops instead.
///
/// TWO, matching [`crate::depth_persistence::MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS`]'s
/// sibling for the same reason as the queue depth.
pub const MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS: u32 = 2;

/// Byte ceiling on the buffer the producer retains while the writer is behind.
///
/// 4 MiB, EIGHT TIMES TIGHTER than the depth path's 32 MiB, and that is the
/// point rather than an oversight: a single depth-200 snapshot is 400 rows and
/// a burst can breach a byte ceiling inside one span, whereas a top-volume
/// snapshot is a bounded 255 rows of ~120 B ILP ≈ 30 KB/s. 4 MiB is ~136
/// seconds of backlog at that rate — an eternity for a table whose next
/// snapshot supersedes the last one. A ceiling sized for depth's traffic would
/// hold minutes of stale rankings that nothing downstream wants.
pub const MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// One ILP payload on its way from the drain to the writer thread.
pub struct TopVolumeFlushBatch {
    buffer: Buffer,
    rows: usize,
}

impl TopVolumeFlushBatch {
    /// Rows this batch covers.
    #[must_use]
    // TEST-EXEMPT: accessor, exercised by the offload tests below.
    pub const fn rows(&self) -> usize {
        self.rows
    }
}

/// What happened to a batch the producer tried to hand off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopVolumeOffloadOutcome {
    /// Handed to the writer thread. The rows are no longer the drain's.
    Sent(usize),
    /// The writer is behind. Rows RETAINED by the producer, nothing lost.
    QueueFull(usize),
    /// The writer stayed behind long enough that retaining further would breach
    /// [`MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS`] or
    /// [`MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES`]. Rows DROPPED.
    ///
    /// # Why dropped and not rescued — the one place this differs from depth
    ///
    /// The depth and tick paths rescue an over-wide payload to a spill tier,
    /// durable and re-ingestable. **There is no spill tier for this table, and
    /// building one would be the wrong trade.** A tick is a unique event that
    /// exists nowhere else; a top-volume snapshot is a PERIODIC SAMPLE of a
    /// leaderboard that lives in RAM and is recomputed from scratch a second
    /// later. Dropping one thins the record; it cannot corrupt it, and it
    /// cannot lose a tick.
    ///
    /// That is a real loss of an observability record and it is never silent:
    /// `discard_pending` counts it and logs a coded error naming the hole.
    WidthCapped(usize),
    /// The writer thread is gone. Rows DROPPED, counted and logged.
    SinkGone(usize),
}

/// The network half of a split [`TopVolumeRankWriter`] — owns the ILP `Sender`.
///
/// Lives on its own OS thread. It never touches the aggregator, the ring, the
/// leaderboard or anything the drain owns, which is the entire reason the split
/// exists: a five-second ILP timeout now blocks a thread whose only job is
/// waiting, instead of the task that empties the socket.
pub struct TopVolumeRankWriterSink {
    sender: Option<Sender>,
}

impl TopVolumeRankWriterSink {
    /// Writes one batch. Returns the rows that actually LANDED in QuestDB.
    ///
    /// Zero on any failure — the same contract [`TopVolumeRankWriter::flush`]
    /// has, and for the same reason: a failed write must be reported as a
    /// failure rather than forged into a success.
    ///
    /// # The bounded retry, and why it is here from the start
    ///
    /// `depth_persistence` records the cost of NOT doing this: its
    /// synchronous path carried a single retry from 2026-08-25, the offload
    /// shipped three days later without it, and the measured price on
    /// 2026-09-03 was **41,204 coded flush failures in one session**, every one
    /// reading `Connection reset by peer (os error 104)` — a transport class
    /// the buffer survives intact. Shipping this sink without the retry would
    /// repeat that mistake knowingly.
    ///
    /// Retrying is idempotent BY CONSTRUCTION, which is what makes one attempt
    /// safe rather than merely cheap: [`DEDUP_KEY_TOP_VOLUME_RANK`] is
    /// `ts, tf, family, feed, security_id, segment`, and every one of those is
    /// fixed for a given snapshot — so a first POST that landed before the
    /// reset is collapsed by the same UPSERT that makes a manual re-ingest
    /// safe to repeat.
    ///
    /// Bounded at ONE and gated on the first failure being FAST, mirroring the
    /// depth arm: `request_timeout` is 5,000 ms, so an unconditional retry
    /// makes the worst case ten seconds. A reset returns in microseconds; a
    /// timeout consumes the whole budget. The window separates them with three
    /// orders of magnitude to spare.
    pub fn write(&mut self, batch: &mut TopVolumeFlushBatch) -> usize {
        if batch.rows == 0 {
            return 0;
        }
        let Some(sender) = self.sender.as_mut() else {
            Self::report_lost(batch, "no ILP sender (QuestDB unreachable)");
            return 0;
        };
        let started = std::time::Instant::now();
        let first = sender.flush(&mut batch.buffer);
        let first_elapsed = started.elapsed();
        let outcome = match first {
            Ok(()) => Ok(()),
            Err(err)
                if crate::depth_persistence::flush_failure_is_retryable(&err)
                    && first_elapsed
                        < crate::depth_persistence::DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW =>
            {
                metrics::counter!("tv_top_volume_rank_flush_retries_total").increment(1);
                sender.flush(&mut batch.buffer)
            }
            Err(err) => Err(err),
        };
        match outcome {
            Ok(()) => {
                let landed = batch.rows;
                batch.rows = 0;
                landed
            }
            Err(err) => {
                let why = format!("{err}"); // APPROVED: loss arm after a flush already failed, never per row
                Self::report_lost(batch, &why);
                0
            }
        }
    }

    /// Counts and LOGS a batch the network refused, through the same counter
    /// and the same coded error the synchronous `discard_pending` uses — so an
    /// operator sees no difference between a synchronous and an offloaded loss.
    fn report_lost(batch: &mut TopVolumeFlushBatch, why: &str) {
        let dropped = batch.rows;
        if dropped == 0 {
            return;
        }
        metrics::counter!("tv_top_volume_rank_rows_discarded_total").increment(dropped as u64);
        error!(
            code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
            metric = "tv_top_volume_rank_rows_discarded_total",
            dropped,
            why,
            "STORAGE-GAP-03: top_volume_rank rows discarded by the offload \
             writer — the ranking record has a hole for those snapshots. No \
             tick is lost by this: the leaderboard is in RAM and the next \
             snapshot rebuilds it."
        );
        batch.buffer.clear();
        batch.rows = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> TopVolumeRankRow {
        TopVolumeRankRow {
            snapshot_ts_ist_nanos: 1_757_000_000_000_000_000,
            cadence: SnapshotCadence::FiveSecond,
            family: "stock_option",
            feed: "dhan",
            segment: "NSE_FNO".to_string(),
            rank: 1,
            security_id: 44_321,
            underlying_id: 2885,
            volume: 117_567_970,
            gain_pct: 4.25,
            subscribed: true,
        }
    }

    /// I-P1-11: the bare `security_id` is reused across segments, so a key
    /// without `segment` silently collapses two distinct instruments.
    #[test]
    fn the_dedup_key_carries_segment_beside_security_id() {
        assert!(DEDUP_KEY_TOP_VOLUME_RANK.contains("security_id"));
        assert!(
            DEDUP_KEY_TOP_VOLUME_RANK.contains("segment"),
            "a key naming security_id without segment collapses two instruments \
             that share an id across segments"
        );
    }

    /// feed-in-key EVERYWHERE (operator override 2026-06-28). Whole-token, so
    /// a column merely CONTAINING "feed" cannot satisfy it.
    #[test]
    fn the_dedup_key_carries_feed_as_a_whole_token() {
        assert!(
            DEDUP_KEY_TOP_VOLUME_RANK
                .split(',')
                .any(|t| t.trim() == "feed"),
            "feed must be a whole key token, not a substring of another column"
        );
    }

    /// The designated timestamp comes FIRST (2026-04-28 regression rule).
    #[test]
    fn the_designated_timestamp_leads_the_dedup_key() {
        assert_eq!(
            DEDUP_KEY_TOP_VOLUME_RANK.split(',').next().map(str::trim),
            Some("ts")
        );
    }

    /// `rank` is the recorded VALUE, not part of the row's identity. In the
    /// key, one contract could occupy several rows in one snapshot as its
    /// rank moved between an emit and a re-emit — the duplicate the DEDUP
    /// key exists to prevent.
    #[test]
    fn rank_is_deliberately_absent_from_the_dedup_key() {
        assert!(
            !DEDUP_KEY_TOP_VOLUME_RANK
                .split(',')
                .any(|t| t.trim() == "rank"),
            "rank in the key lets one contract hold several rows per snapshot"
        );
    }

    /// The two cadences must be distinguishable in the key, or the 5s
    /// snapshot silently overwrites the 1s snapshot that shares its second.
    #[test]
    fn the_cadence_is_in_the_key_so_5s_never_overwrites_1s() {
        assert!(
            DEDUP_KEY_TOP_VOLUME_RANK
                .split(',')
                .any(|t| t.trim() == "tf"),
            "without tf in the key, every 5s row lands on the 1s row of the \
             same second and one of the two is lost"
        );
    }

    /// Families are ranked separately, so both can hold rank 1 in the same
    /// second for different contracts. Without `family` in the key that is
    /// still fine (security_id differs) — but a contract listed in BOTH
    /// families would collide. Pinned so the separation survives a refactor.
    #[test]
    fn the_family_is_in_the_key() {
        assert!(
            DEDUP_KEY_TOP_VOLUME_RANK
                .split(',')
                .any(|t| t.trim() == "family")
        );
    }

    #[test]
    fn top_volume_rank_create_ddl_partitions_by_hour_and_enables_dedup() {
        let ddl = top_volume_rank_create_ddl();
        assert!(ddl.contains("PARTITION BY HOUR"), "{ddl}");
        assert!(
            ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_TOP_VOLUME_RANK})")),
            "{ddl}"
        );
        assert!(ddl.contains("timestamp(ts)"), "{ddl}");
    }

    /// Schema-self-heal order: CREATE, then every column, then DEDUP ENABLE.
    /// Never a DROP — this table is append-only history.
    #[test]
    fn top_volume_rank_ensure_statements_never_drop_and_end_with_dedup_enable() {
        let statements = top_volume_rank_ensure_statements();
        assert!(statements[0].starts_with("CREATE TABLE IF NOT EXISTS"));
        assert!(
            statements
                .last()
                .is_some_and(|s| s.contains("DEDUP ENABLE")),
            "DEDUP ENABLE must come last, after the columns it names exist"
        );
        assert!(
            !statements.iter().any(|s| s.to_uppercase().contains("DROP")),
            "a DROP in the self-heal path would delete history on any boot"
        );
    }

    /// Every column in the CREATE must also appear in the ALTER manifest, or
    /// a table created by an older build never gains it and every write of
    /// that column fails forever.
    #[test]
    fn every_created_column_is_also_in_the_self_heal_manifest() {
        let ddl = top_volume_rank_create_ddl();
        for (col, ty) in TOP_VOLUME_RANK_COLUMNS {
            assert!(
                ddl.contains(&format!("{col}        ").trim_end().to_string())
                    || ddl.contains(*col),
                "column {col} is in the manifest but not in the CREATE"
            );
            assert!(
                ddl.contains(ty),
                "type {ty} for column {col} is absent from the CREATE"
            );
        }
    }

    #[test]
    fn append_row_carries_every_symbol_and_column() {
        let mut w = TopVolumeRankWriter::for_test();
        w.append_row(&row()).expect("append");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        for expected in [
            "top_volume_rank",
            "tf=5s",
            "family=stock_option",
            "feed=dhan",
            "segment=NSE_FNO",
            "rank=1i",
            "security_id=44321i",
            "underlying_id=2885i",
            "volume=117567970i",
            "subscribed=t",
        ] {
            assert!(line.contains(expected), "missing {expected} in: {line}");
        }
    }

    #[test]
    fn the_cadence_labels_are_the_stable_wire_strings() {
        assert_eq!(SnapshotCadence::OneSecond.as_str(), "1s");
        assert_eq!(SnapshotCadence::FiveSecond.as_str(), "5s");
    }

    /// The label and the interval are two halves of the same claim: a row
    /// stamped `5s` asserts that the snapshot behind it was taken every five
    /// seconds. They are separate `match` arms, so nothing but a test stops
    /// one from being edited without the other — and the drift would be
    /// invisible, because a wrongly-paced snapshot still writes a
    /// well-formed row under a label that reads correct.
    #[test]
    fn interval_secs_agrees_with_the_label_it_is_stored_under() {
        for cadence in [SnapshotCadence::OneSecond, SnapshotCadence::FiveSecond] {
            let from_label: u64 = cadence
                .as_str()
                .trim_end_matches('s')
                .parse()
                .expect("every cadence label is <n>s");
            assert_eq!(
                cadence.interval_secs(),
                from_label,
                "{} claims {}s in its label but fires every {}s",
                cadence.as_str(),
                from_label,
                cadence.interval_secs()
            );
        }
    }

    /// A disconnected writer must not silently retain rows: a buffer kept
    /// across flushes replays a rejected row forever and poisons every later
    /// flush with it.
    #[test]
    fn a_flush_without_a_sender_discards_and_reports() {
        let mut w = TopVolumeRankWriter::for_test();
        w.append_row(&row()).expect("append");
        let err = w.flush().expect_err("no sender must be an error");
        assert!(
            format!("{err}").contains("discarded"),
            "the error must say the rows were discarded: {err}"
        );
        assert_eq!(w.pending(), 0, "pending must be cleared after a discard");
    }

    /// Flushing nothing is a no-op, not an error — otherwise a quiet second
    /// produces a spurious failure every time.
    #[test]
    fn flushing_an_empty_buffer_is_not_an_error() {
        let mut w = TopVolumeRankWriter::for_test();
        assert!(w.flush().is_ok());
    }

    /// `subscribed=false` is the whole point of the column — it must be
    /// written, not skipped as a default.
    #[test]
    fn append_row_writes_subscribed_false_rather_than_omitting_it() {
        let mut w = TopVolumeRankWriter::for_test();
        let mut r = row();
        r.subscribed = false;
        w.append_row(&r).expect("append");
        assert!(
            w.buffer_utf8().contains("subscribed=f"),
            "a false must be recorded, or the audit cannot tell 'we missed it' \
             from 'no row was written'"
        );
    }

    // -----------------------------------------------------------------------
    // Off-drain write (2026-09-07)
    // -----------------------------------------------------------------------

    #[test]
    fn split_for_offload_moves_the_sender_and_opens_the_queue() {
        let (producer, sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        assert!(
            producer.is_offloaded(),
            "the producer must know it is split — the drain reads this to refuse a synchronous flush"
        );
        assert!(
            producer.sender.is_none(),
            "the ILP sender must MOVE to the sink; leaving a copy behind is how a \
             5-second network call finds its way back onto the drain"
        );
        assert!(
            sink.sender.is_none(),
            "for_test() has no sender to move — this pins the MOVE, not the presence"
        );
    }

    #[test]
    fn a_flush_after_the_split_hands_off_instead_of_writing() {
        let (mut producer, _sink, rx) = TopVolumeRankWriter::for_test().split_for_offload();
        producer.append_row(&row()).expect("append");
        assert_eq!(producer.pending(), 1);

        producer.flush().expect("hand-off is not an error");

        assert_eq!(
            producer.pending(),
            0,
            "the rows are the writer thread's now, not the drain's"
        );
        let batch = rx.try_recv().expect("the batch must be on the queue");
        assert_eq!(batch.rows(), 1, "the batch must carry the row count");
    }

    #[test]
    fn a_full_queue_is_backpressure_and_never_loss() {
        let (mut producer, _sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        // Fill every slot. `_rx` is held so nothing drains it.
        for _ in 0..TOP_VOLUME_FLUSH_QUEUE_DEPTH {
            producer.append_row(&row()).expect("append");
            assert!(matches!(
                producer.offload_flush(),
                TopVolumeOffloadOutcome::Sent(1)
            ));
        }

        producer.append_row(&row()).expect("append");
        let outcome = producer.offload_flush();

        assert_eq!(
            outcome,
            TopVolumeOffloadOutcome::QueueFull(1),
            "a full queue must report backpressure, not success and not loss"
        );
        assert_eq!(
            producer.pending(),
            1,
            "the rows must be RETAINED — this is the arm that makes the bounded \
             queue safe. Without it a full queue either blocks the drain (the \
             original defect) or drops rows (a worse one)."
        );
    }

    #[test]
    fn queue_full_is_not_reported_to_the_caller_as_a_failure() {
        let (mut producer, _sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        for _ in 0..TOP_VOLUME_FLUSH_QUEUE_DEPTH {
            producer.append_row(&row()).expect("append");
            producer.flush().expect("hand-off");
        }
        producer.append_row(&row()).expect("append");

        assert!(
            producer.flush().is_ok(),
            "backpressure is not a failure: reporting it as one would decay a \
             health signal for rows that are still safely held"
        );
    }

    #[test]
    fn retaining_past_the_span_cap_drops_rather_than_widening_forever() {
        let (mut producer, _sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        for _ in 0..TOP_VOLUME_FLUSH_QUEUE_DEPTH {
            producer.append_row(&row()).expect("append");
            producer.flush().expect("hand-off");
        }

        // Every flush from here retains, until the span cut fires.
        let mut outcomes = Vec::new();
        for _ in 0..=MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS {
            producer.append_row(&row()).expect("append");
            outcomes.push(producer.offload_flush());
        }

        let capped = outcomes
            .iter()
            .filter(|o| matches!(o, TopVolumeOffloadOutcome::WidthCapped(_)))
            .count();
        assert_eq!(
            capped, 1,
            "exactly one cut on the span AFTER the retained ones — the constant \
             names how many may be RETAINED, so `>` and not `>=`. Got {outcomes:?}"
        );
        assert_eq!(
            producer.pending(),
            0,
            "the cut must clear the buffer; retaining past it is the unbounded-\
             memory path the cap exists to close"
        );
    }

    #[test]
    fn a_disconnected_sink_reports_gone_and_never_forges_success() {
        let (mut producer, _sink, rx) = TopVolumeRankWriter::for_test().split_for_offload();
        drop(rx);
        producer.append_row(&row()).expect("append");

        let outcome = producer.offload_flush();

        assert!(
            matches!(outcome, TopVolumeOffloadOutcome::SinkGone(1)),
            "a vanished writer thread must be reported as gone. \"We sent it\" \
             when nothing was sent is the one report that must never be wrong. \
             Got {outcome:?}"
        );
        // A fresh row, because the outcome above already discarded the first:
        // `flush` returns early on an empty buffer, so asserting on it without
        // re-appending would test the early return rather than the gone arm.
        producer.append_row(&row()).expect("append");
        assert!(
            producer.flush().is_err(),
            "and the caller must see the failure, not a silent Ok"
        );
    }

    #[test]
    fn the_sink_reports_zero_landed_when_it_cannot_write() {
        let (mut producer, mut sink, rx) = TopVolumeRankWriter::for_test().split_for_offload();
        producer.append_row(&row()).expect("append");
        producer.flush().expect("hand-off");
        let mut batch = rx.try_recv().expect("batch");

        // `for_test()` has no ILP sender, so this is the no-sender arm.
        let landed = sink.write(&mut batch);

        assert_eq!(
            landed, 0,
            "a failed write must report ZERO landed. The caller reports health \
             from this number, so forging it is worse than the failure."
        );
        assert_eq!(
            batch.rows(),
            0,
            "and the batch must be cleared so a retry cannot double-count it"
        );
    }

    #[test]
    fn the_sink_reports_the_rows_that_actually_landed() {
        // No sender means nothing can land; the positive direction is covered
        // by the live QuestDB path. What this pins is that an EMPTY batch is a
        // no-op rather than an error — the drain flushes on a timer, and a
        // timer that fires on an idle second must not log a loss.
        let (_producer, mut sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        let mut empty = TopVolumeFlushBatch {
            buffer: Buffer::new(ProtocolVersion::V1),
            rows: 0,
        };
        assert_eq!(sink.write(&mut empty), 0);
    }

    #[test]
    fn the_producer_byte_ceiling_is_far_tighter_than_the_depth_path() {
        // Not a style preference — a sizing claim, pinned so it cannot drift
        // into depth's 32 MiB by copy-paste. This table emits ~255 rows/s
        // against depth's measured ~63,800, so a ceiling sized for depth would
        // hold minutes of stale rankings nothing downstream wants.
        assert!(
            MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES
                < crate::depth_persistence::MAX_DEPTH_PRODUCER_BUFFER_BYTES,
            "the top-volume ceiling must stay tighter than depth's"
        );
        assert_eq!(
            TOP_VOLUME_FLUSH_QUEUE_DEPTH,
            crate::depth_persistence::DEPTH_FLUSH_QUEUE_DEPTH,
            "the queue DEPTH is deliberately the same across writers — one \
             number is one thing to keep true, and a shared shape is what stops \
             the three paths drifting into different failure semantics"
        );
    }
}
