//! QuestDB partition lifecycle manager.
//!
//! Detaches partitions older than the configured retention window.
//! Runs daily at 16:30 IST (after market close) to keep hot data bounded.
//!
//! # Partition Strategy
//! - HOUR-partitioned tables (ticks, depth, OBI, greeks): detach hourly partitions > N days old
//! - DAY-partitioned tables (candles, instruments, calendar): detach daily partitions > N days old
//!
//! # SEBI Compliance
//! Detached partitions remain on disk until physically removed.
//! Phase B.7 (S3 archival) exports to S3 before removal for 5-year retention.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::{debug, info, warn};

use tickvault_common::config::QuestDbConfig;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// HTTP timeout for partition management queries.
const PARTITION_DDL_TIMEOUT_SECS: u64 = 30;

/// Tables with HOUR partitioning (high-frequency data).
///
/// 2026-04-28 (Phase 8): added 22 `movers_{T}` tables for the movers
/// 22-timeframe redesign. Each table is `PARTITION BY DAY WAL` but is
/// listed here because it's high-frequency snapshot data subject to the
/// same retention rotation. The constant name predates the rotation-vs-
/// partition-period distinction.
// 2026-06-05: corrected to the live post-cleanup reality. The previous lists
// named DELETED tables (greeks / depth / movers / option_chain — modules gone)
// and OMITTED every live growing table, so the retention sweep visited nothing
// real → unbounded active-table growth (a storage/cost runaway). `ticks` is the
// only live HOUR-partitioned table.
pub(crate) const HOUR_PARTITIONED_TABLES: &[&str] = &["ticks", "market_depth"];

/// DAY-partitioned **audit + daily-data** tables the retention sweep DETACHes
/// past the hot window. The 5 live **candle** tables (`candles_1m` …
/// `candles_1d`) are NOT listed here — they are swept by iterating
/// [`crate::shadow_persistence::candle_table_names`] (the single source of
/// truth, `TfIndex::table_name()`) so the candle names can never drift.
///
/// **Honest boundary:** `detach_old_partitions` uses `ALTER TABLE … DETACH
/// PARTITION` — it moves old partitions to a local `detached/` dir (SEBI-safe,
/// never DROP). Actually freeing EBS requires S3-upload + local cleanup of the
/// detached partitions — a SEPARATE, AWS-dependent piece that is NOT yet
/// implemented. This list bounds the *active* table and is the prerequisite for
/// archival; it does not by itself reclaim disk.
pub(crate) const DAY_PARTITIONED_TABLES: &[&str] = &[
    // Audit + daily-data tables (SEBI 5y → detach to S3 cold when archival ships).
    "instrument_fetch_audit",
    "instrument_lifecycle_audit",
    "prev_day_ohlcv",
    "cross_verify_1m_audit",
    // NOTE: `index_constituency` was moved to RETENTION_EXEMPT_TABLES (2026-06-28,
    // ts-pin fix). Its designated `ts` is now pinned to constant epoch 0, so ALL
    // rows land in the `1970-01-01` partition; if it stayed in the DAY sweep list
    // the retention pass would DETACH that >90d partition every run, silently
    // removing the live current-state master. It is now exempt, exactly like
    // `instrument_lifecycle` (also a pinned-ts current-state master).
    // TICK-CONSERVE-01 (2026-06-10): one row per daily conservation-audit
    // run — same SEBI-audit class + DAY partitioning as cross_verify_1m_audit.
    // (Audit RETIRED 2026-07-18 with its modules + ErrorCode — dead-WS sweep
    // follow-up; the TABLE stays retained/swept here per SEBI 5y, exactly
    // like cross_verify_1m_audit above whose writer also retired.)
    "tick_conservation_audit",
    // AUDIT-WS-01 (2026-06-12): one row per WebSocket lifecycle event —
    // same SEBI-audit class + DAY partitioning as the audit tables above.
    "ws_event_audit",
    // (2026-06-19 "same tables + feed column") Groww live ticks NO LONGER have a
    // separate table — Groww writes the SHARED `ticks` table tagged feed='groww',
    // which is already HOUR-partitioned + retention-swept above. So no
    // `groww_live_ticks` entry here.
    // (2026-06-19 Step 3b-ii) Groww 1-minute candles ALSO no longer have a
    // separate `groww_candles_1m` table — Groww writes the SHARED `candles_1m`
    // tagged feed='groww', which is already swept via `candle_table_names()` in
    // `detach_old_partitions` (the 5 plain candle tables). So no
    // `groww_candles_1m` entry here.
    // Groww second-feed live-vs-backtest 1m parity mismatches (operator lock §32):
    // one row per mismatched (instrument, minute, field) — same SEBI-audit class +
    // DAY partitioning as cross_verify_1m_audit. Isolated `groww_*` namespace.
    // RETAINED (NEVER dropped, SEBI 5y) even after SP5 unified the writer — these
    // two siloed tables hold pre-SP5 history (the Dhan one) / are empty (the Groww
    // one); new mismatches go to `feed_parity_1m_audit` below.
    "groww_cross_verify_1m_audit",
    // SP5 (parity plan): the ONE unified live-vs-backtest 1m parity audit table —
    // both feeds write here, `feed` is in the DEDUP key. Same SEBI-audit class +
    // DAY partitioning as the two siloed tables it merges.
    "feed_parity_1m_audit",
    // SCOREBOARD-01 (2026-07-10, dual-feed scoreboard PR-A): one row per
    // (trading day, feed) verdict / per (trading day, feed) coverage snapshot /
    // per outage episode — same SEBI-audit class + DAY partitioning as the
    // audit tables above; `feed` is in every DEDUP key.
    "feed_scoreboard_daily",
    "feed_coverage_daily",
    "feed_episode_audit",
    // BRUTEX-XVERIFY-01/02 (2026-07-12, §37): one row per divergent cell /
    // one row per trading-day summary — same SEBI-audit class + DAY
    // partitioning as cross_verify_1m_audit / feed_scoreboard_daily.
    "brutex_crossverify_cell_audit",
    "brutex_crossverify_daily",
    // SPOT-XVERIFY-01/02 (2026-07-16, revival of the cross-broker OHLC parity
    // signal): one row per divergent/missing cell / one row per trading-day
    // summary — same SEBI-audit class + DAY partitioning as brutex_crossverify_*
    // above; `feed` is in both DEDUP keys (label `dhan_x_groww`: an inherently
    // cross-feed comparison row).
    "spot_crossverify_cell_audit",
    "spot_crossverify_daily",
    // Dhan live-WS vs Dhan REST 15:31 cross-verification (2026-08-09, the
    // 16-connection live-feed revival): one row per divergent/missing cell /
    // one row per trading-day summary. Same SEBI-audit class and DAY
    // partitioning as the two cross-verify pairs above.
    //
    // This comparison carries more weight than its siblings: the Dhan feed has
    // no sequence number and no snapshot-on-subscribe, so this is the ONLY
    // ground truth available for whether the live capture is complete. The
    // cell table is bounded by divergences, not by instruments, so it is
    // normally near-empty — a large day IS the signal.
    "dhan_live_crossverify_cell_audit",
    "dhan_live_crossverify_daily",
    // The vendor's own 1-minute tape, captured verbatim by the same 15:41
    // sweep that produces the two rows above (2026-08-26, Item 38). It is
    // the ONLY copy of what Dhan actually answered: without it a divergence
    // verdict can be read but never re-checked by hand, because the REST
    // side was judged and then discarded.
    //
    // DAY-partitioned and swept exactly like its two siblings, but it is NOT
    // near-empty like them: the cell table is bounded by divergences, this
    // one is bounded by INSTRUMENTS. One rotation third (~290 of ~865 spots)
    // x 375 session minutes ~= 108K rows/day at full success, ~30 MB/day at
    // the measured ~272 B/row, so ~2.7 GB across the 90-day hot window. At
    // today's measured 50-successful-fetches ceiling it is ~19K rows/day.
    // Stated as a number rather than "trivial" because it is the largest
    // per-day writer in this list and the volume is what the retention sweep
    // exists to bound.
    "dhan_rest_1m_tape",
    // SPOT1M-01/02 (2026-07-12, per-minute REST pipeline PR-2): one row per
    // fetched (minute, index) — ~1,125 rows/day (375 min × 3 IDX_I SIDs),
    // trivial disk; same DAY partitioning + retention class as the
    // daily-data tables above; `feed` is in the DEDUP key.
    "rest_spot_1m",
    // CHAIN-03 (2026-07-12, per-minute REST pipeline PR-3): one row per
    // fetched (minute, underlying, expiry, strike, leg) — ~1-3K rows/min
    // worst case (~70 MB/day at wide chains; disk envelope flagged as an
    // operator retention follow-up in option_chain_1m_persistence.rs);
    // same DAY partitioning + retention class; `feed` is in the DEDUP key.
    "rest_option_chain_1m",
    // Partition archive→verify→drop forensic chain (2026-07-13, disk-pressure
    // remediation): one row per archive attempt outcome — same SEBI-audit
    // class + DAY partitioning as the audit tables above; `feed` is in the
    // DEDUP key (label `all`: an archived partition contains EVERY feed's
    // rows). Tiny (≤ a few hundred rows/day) — the 90d standard class.
    "partition_archive_audit",
    // SPOT1M-01/02 leg=contract_1m (2026-07-13, Groww REST plan PR-4 — the
    // fill-model leg): one row per fetched (minute, contract) — bounded by
    // the per-minute selection cap (~30 contracts × 375 min ≈ ~11K rows/day,
    // trivial disk); same DAY partitioning + retention class; `feed` +
    // `exchange_segment` are in the DEDUP key.
    "rest_option_contract_1m",
    // SPOT1M-02 stages audit_ensure_* (2026-07-13, Groww spot-1m PR-2): one
    // row per REST fetch attempt outcome per (minute, symbol, feed, leg) —
    // ~1-2K rows/day for the Groww spot leg (375 min x 3 symbols x attempts),
    // trivial disk; same DAY partitioning + SEBI retention class as the audit
    // tables above; `feed` + `exchange_segment` are in the DEDUP key.
    "rest_fetch_audit",
    // TF-VERIFY-01/02 (2026-07-13, daily timeframe-consistency verifier):
    // one row per finding cell where a stored higher-TF candle disagrees
    // with its recompute-from-1m — bounded at 10K rows/run by
    // TF_VERIFY_MAX_AUDIT_ROWS_PER_RUN, normally ~0; same SEBI-audit class
    // + DAY partitioning as cross_verify_1m_audit; `feed` is in the DEDUP key.
    "tf_consistency_audit",
    // AUDIT-06 / STORAGE-GAP-03 (2026-07-14, cluster-C order-side): one row
    // per order lifecycle event / per P&L snapshot — the strict SEBI 5y
    // order-audit class (disaster-recovery.md: "order_audit is the strict
    // 5y SEBI-mandate table"): 90d hot in QuestDB, then the DAY sweep's
    // detach→S3-archive leg owns the cold path exactly like the audit
    // tables above. Tiny in dry-run (a handful of paper rows/day + 1 OnEod
    // pnl heartbeat row); `feed` is in both DEDUP keys.
    "order_audit",
    "pnl_audit",
    // ORDER-EVT-01 (2026-07-18, full-fidelity push-event capture): one row
    // per received broker order/position push event, BOTH feeds — the same
    // Standard 90d order-audit class as order_audit/pnl_audit above (90d hot
    // in QuestDB, then the DAY sweep's detach→S3-archive cold path). Tiny in
    // paper mode (a handful of rows/day); `feed` is in both DEDUP keys.
    "order_leg_pnl",
    "order_update_events",
    "position_update_events",
    // CADENCE-04 (2026-07-20, cross-fill visibility, operator directive):
    // one row per cadence cross-fill firing / Groww fallback launch — a
    // handful of rows/day by construction (one per degraded lane per
    // cycle); same DAY partitioning + SEBI audit retention class as the
    // audit tables above. `lane` is the per-feed identity in its DEDUP key.
];

/// Tables EXEMPT from retention sweeping — NEVER detached or dropped.
///
/// `instrument_lifecycle` is the SEBI point-in-time master: every instrument
/// EVER observed, never deleted (daily-universe §5/§25). It must stay whole for
/// point-in-time reconstruction, so it is never swept even though it is
/// `PARTITION BY DAY`. It is small (~219K rows) so it does not need sweeping.
///
/// `index_constituency` (added 2026-06-28, ts-pin fix) is a CURRENT-STATE
/// index→constituents master with its designated `ts` pinned to constant epoch
/// 0 — so EVERY row sits in the `1970-01-01` partition. If it were swept, that
/// (always >90d-old) partition would be detached every run, removing the live
/// master. It is small (~742 rows) + fully re-derivable from CSV each boot, so
/// it is exempt exactly like `instrument_lifecycle`.
///
/// The three `spot_1m_rest` / `option_chain_1m` / `option_contract_1m_rest`
/// entries (added 2026-08-14) are the PRE-RENAME names, and they are exempt for
/// a different reason than the two above: **they name a table that ceases to
/// exist.** The boot migration issues `RENAME TABLE '<old>' TO '<new>'`
/// (`spot_1m_rest_persistence.rs`, `option_chain_1m_persistence.rs`,
/// `option_contract_1m_rest_persistence.rs`), which carries every row forward
/// under the new name — so after the first boot on this build there is no old
/// table left to sweep. They are listed only because their `LEGACY_*_TABLE`
/// constants still exist to drive that rename, and
/// `partition_retention_coverage_guard` — correctly — demands a decision for
/// every table-name constant in the crate rather than letting one slip through
/// undeclared.
///
/// Putting them in a SWEEP list instead would be wrong twice over: the sweeper
/// would issue partition detaches against a name that no longer resolves, and
/// the real data would be swept anyway under its new name, which is already
/// covered above. Exempt is the honest answer, and it costs nothing — the row
/// data itself is retained and swept under the new name.
///
/// **CORRECTION, same day (2026-08-14).** The paragraph above named all three
/// modules as issuing the rename. That was true of two of them:
/// `option_contract_1m_rest_persistence.rs` had NO rename at all — its
/// `LEGACY_*` constant existed but nothing consumed it, so on any box that had
/// already written that table the CREATE would mint an empty table beside the
/// populated one, and this exemption would have shielded the resulting orphan
/// from ever being swept. The missing rename was added in the same change.
///
/// **SECOND CORRECTION, same day, and this one matters more.** Adding the
/// rename does NOT make the reasoning above hold — an earlier version of this
/// paragraph claimed it did, which repeated the exact error it was written to
/// record. A rename that is ISSUED is not a rename that SUCCEEDED. If the new
/// name already exists (a box that ran the CREATE-without-rename build, or a
/// rollback to a pre-rename build whose ILP writer re-creates the old name),
/// QuestDB refuses the rename and BOTH tables survive — at which point this
/// list exempts a populated, growing, SEBI-retained orphan from ever being
/// swept, silently, on a 100 GB root.
///
/// What actually discharges the exemption is DETECTION, not intent:
/// `try_rename_legacy_table` probes for the legacy table after any refusal and
/// returns `LegacyRenameOutcome::Split` when it is still there, and all three
/// call sites turn that into a coded `error!` plus a `*_persist_errors_total`
/// increment on EVERY boot until an operator resolves it. So the exemption is
/// safe in the only sense that can be verified at runtime: either the old table
/// is gone (nothing to sweep) or its continued existence is loud.
///
/// **Un-discharged today:** `option_contract_1m_rest`. Its `ensure_*` has one
/// caller, `groww_contract_1m_boot`, gated on `groww_contract_1m.enabled`,
/// which `config/base.toml` ships `false`. The rename and its split detection
/// therefore do not execute in the shipped configuration, so that third
/// exemption rests on nothing being written to the table either. Stated rather
/// than assumed — a retention exemption granted on a premise that does not hold
/// is the failure mode this list must never carry, and it has now been recorded
/// twice in one day for the same reason.
pub(crate) const RETENTION_EXEMPT_TABLES: &[&str] = &[
    "instrument_lifecycle",
    "index_constituency",
    // Pre-2026-08-14 names, renamed forward at boot — see the doc comment above.
    "spot_1m_rest",
    "option_chain_1m",
    "option_contract_1m_rest",
];

// ---------------------------------------------------------------------------
// Partition Manager
// ---------------------------------------------------------------------------

/// Manages QuestDB partition lifecycle (detach old partitions).
pub struct PartitionManager {
    base_url: String,
    client: Client,
    retention_days: u32,
}

impl PartitionManager {
    /// Creates a new partition manager.
    ///
    /// # Arguments
    /// * `config` — QuestDB connection config.
    /// * `retention_days` — Hot partition retention in days (0 = disabled).
    // TEST-EXEMPT: HTTP client creation — requires live QuestDB
    pub fn new(config: &QuestDbConfig, retention_days: u32) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(PARTITION_DDL_TIMEOUT_SECS))
            .build()
            .context("failed to create HTTP client for partition manager")?;

        Ok(Self {
            base_url: format!("http://{}:{}/exec", config.host, config.http_port),
            client,
            retention_days,
        })
    }

    /// Detaches all partitions older than the retention window.
    ///
    /// Iterates over all known tables and detaches partitions where
    /// the partition timestamp is older than `now - retention_days`.
    ///
    /// Returns the total count of partitions detached.
    // TEST-EXEMPT: requires live QuestDB with populated tables
    pub async fn detach_old_partitions(&self) -> Result<u32> {
        if self.retention_days == 0 {
            info!("partition detach disabled (retention_days=0)");
            return Ok(0);
        }

        let cutoff_days = self.retention_days;
        let mut total_detached: u32 = 0;

        info!(
            retention_days = cutoff_days,
            hour_tables = HOUR_PARTITIONED_TABLES.len(),
            day_tables = DAY_PARTITIONED_TABLES.len(),
            "starting partition detach cycle"
        );

        // HOUR-partitioned tables. Defense-in-depth: never sweep an exempt
        // (SEBI point-in-time, never-delete) table even if one is mistakenly
        // added to a sweep list — the exempt set is the final guard.
        for table in HOUR_PARTITIONED_TABLES {
            if RETENTION_EXEMPT_TABLES.contains(table) {
                continue;
            }
            match self
                .detach_partitions_for_table(table, cutoff_days, "HOUR")
                .await
            {
                Ok(count) => {
                    total_detached = total_detached.saturating_add(count);
                    if count > 0 {
                        info!(table, detached = count, "partitions detached");
                    }
                }
                Err(err) => {
                    warn!(?err, table, "failed to detach partitions");
                }
            }
        }

        // DAY-partitioned audit/data tables (same exempt guard as the HOUR loop).
        for table in DAY_PARTITIONED_TABLES {
            if RETENTION_EXEMPT_TABLES.contains(table) {
                continue;
            }
            match self
                .detach_partitions_for_table(table, cutoff_days, "DAY")
                .await
            {
                Ok(count) => {
                    total_detached = total_detached.saturating_add(count);
                    if count > 0 {
                        info!(table, detached = count, "partitions detached");
                    }
                }
                Err(err) => {
                    warn!(?err, table, "failed to detach partitions");
                }
            }
        }

        // The 5 live candle tables (`candles_1m` … `candles_1d`), DAY-partitioned.
        // Derived from `candle_table_names()` (the single source of truth,
        // `TfIndex::table_name()`) so the swept names can NEVER drift from what is
        // actually created/written. This is the dominant disk-growth source —
        // #1022 named phantom `candles_*_shadow` tables and missed it.
        for table in crate::shadow_persistence::candle_table_names() {
            if RETENTION_EXEMPT_TABLES.contains(&table) {
                continue;
            }
            match self
                .detach_partitions_for_table(table, cutoff_days, "DAY")
                .await
            {
                Ok(count) => {
                    total_detached = total_detached.saturating_add(count);
                    if count > 0 {
                        info!(table, detached = count, "partitions detached");
                    }
                }
                Err(err) => {
                    warn!(?err, table, "failed to detach partitions");
                }
            }
        }

        info!(total_detached, "partition detach cycle complete");

        Ok(total_detached)
    }

    /// Detaches old partitions for a single table.
    ///
    /// Lists partitions via QuestDB's `table_partitions()` (filtered to those
    /// older than the retention cutoff), then `ALTER TABLE DETACH PARTITION`
    /// for the INACTIVE ones — the active (currently-written) partition is
    /// never selected (QuestDB forbids detaching it, and it may hold
    /// current-day data).
    async fn detach_partitions_for_table(
        &self,
        table: &str,
        cutoff_days: u32,
        _partition_by: &str,
    ) -> Result<u32> {
        // Query partitions older than the retention cutoff. The cutoff/lookback
        // filter stays server-side (unchanged) to preserve the exact retention
        // semantics and avoid re-deriving an IST cutoff in Rust (the +5:30
        // timestamp landmine in data-integrity.md). We project `active` so the
        // active-partition EXCLUSION can be done by the pure, unit-testable
        // `select_partitions_to_detach` below.
        let list_sql = build_detach_list_sql(table, cutoff_days);

        let response = self
            .client
            .get(&self.base_url)
            .query(&[("query", &list_sql)])
            .send()
            .await
            .context("partition list query failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>(); // O(1) EXEMPT: error logging
            debug!(%status, body, table, "partition list query returned non-success (table may not exist yet)");
            return Ok(0);
        }

        let body = response
            .text()
            .await
            .context("failed to read partition list response")?;

        let rows = parse_partition_rows(&body);
        // Fix (2026-07-09): detach the aged-out INACTIVE partitions, NEVER the
        // active one. The previous predicate selected only `active = true`,
        // which is contradictory with the cutoff filter (the active partition
        // is the newest, never older than the cutoff), so the sweep detached
        // ZERO partitions — a silent no-op.
        let partition_names = select_partitions_to_detach(&rows);

        if partition_names.is_empty() {
            return Ok(0);
        }

        // Proof the fixed predicate is non-vacuous: how many partitions were
        // SELECTED for detach this sweep (intent). Effect is
        // `tv_partition_detach_total` below (successful detaches only); a
        // sustained `selected > detached` gap signals detach DDLs failing.
        // Cold once-daily path, no labels (zero cardinality risk).
        metrics::counter!("tv_partition_detach_selected_total")
            .increment(partition_names.len() as u64);

        let mut detached: u32 = 0;
        for name in &partition_names {
            let detach_sql = format!("ALTER TABLE {} DETACH PARTITION LIST '{}'", table, name);

            match self
                .client
                .get(&self.base_url)
                .query(&[("query", &detach_sql)])
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    detached = detached.saturating_add(1);
                    metrics::counter!("tv_partition_detach_total").increment(1);
                    debug!(table, partition = name, "partition detached");
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp
                        .text()
                        .await
                        .unwrap_or_default()
                        .chars()
                        .take(200)
                        .collect::<String>(); // O(1) EXEMPT: error logging
                    warn!(%status, body, table, partition = name, "partition detach failed");
                }
                Err(err) => {
                    warn!(
                        ?err,
                        table,
                        partition = name,
                        "partition detach request failed"
                    );
                }
            }
        }

        Ok(detached)
    }
}

/// A single row from QuestDB's `table_partitions('<table>')` result that the
/// retention sweep needs to decide whether to detach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartitionRow {
    /// Partition name (e.g. `2026-04-01` for DAY, `2026-04-01T09` for HOUR).
    pub(crate) name: String,
    /// `true` for the single currently-written partition. QuestDB refuses to
    /// detach the active partition, so it is NEVER selected for detach.
    pub(crate) active: bool,
}

/// Builds the partition-list SQL for a single table.
///
/// The cutoff/lookback filter is server-side (`minTimestamp < dateadd('d',
/// -N, now())`) to preserve the existing retention semantics exactly. `active`
/// is projected so the active-partition exclusion is done by
/// [`select_partitions_to_detach`] (pure + unit-testable), NOT in SQL — the
/// previous `AND active = true` predicate was the bug (it selected only the
/// active partition, which is contradictory with the cutoff and which QuestDB
/// refuses to detach, so ZERO partitions were ever detached).
///
/// `table` is ALWAYS a trusted compile-time / derived constant (one of
/// `HOUR_PARTITIONED_TABLES`, `DAY_PARTITIONED_TABLES`, or
/// `candle_table_names()`) — never external input — so it is not
/// injection-validated here; the untrusted `name` values are validated by
/// [`is_valid_partition_name`] before they reach a DETACH statement.
pub(crate) fn build_detach_list_sql(table: &str, cutoff_days: u32) -> String {
    format!(
        "SELECT name, active FROM table_partitions('{}') \
         WHERE minTimestamp < dateadd('d', -{}, now())",
        table, cutoff_days
    )
}

/// Validates a QuestDB partition name before it is interpolated into a
/// `DETACH PARTITION LIST '<name>'` DDL statement.
///
/// QuestDB partition names are either `YYYY-MM-DD` (DAY) or `YYYY-MM-DDTHH`
/// (HOUR). QuestDB's `/exec` endpoint has NO parameterized-query mechanism, so
/// the name is string-interpolated; a name carrying a quote or DDL keyword
/// (only possible from a corrupt / MITM'd QuestDB response, since the value is
/// second-order data we did not author) would break out of the string literal.
/// This fail-closed allowlist — digits, `-`, and a single `T` only — makes such
/// a name impossible to inject: it is rejected here and never selected for
/// detach.
pub(crate) fn is_valid_partition_name(name: &str) -> bool {
    // DAY: "2026-04-01" (10) or HOUR: "2026-04-01T09" (13). Allow the small
    // range rather than a strict length so future QuestDB partition-by widths
    // still pass, while a quote / space / DDL keyword can never appear.
    !name.is_empty()
        && name.len() <= 20
        && name
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == 'T')
}

/// Selects the partition NAMES to DETACH from the parsed `table_partitions`
/// rows: every partition that is NOT active AND whose name is a
/// well-formed partition name (see [`is_valid_partition_name`]). The rows are
/// already restricted to those older than the retention cutoff by
/// [`build_detach_list_sql`], so the remaining decision is "exclude the active
/// (current-day) partition" plus the fail-closed name allowlist.
///
/// This is the fix for the retention no-op bug: the previous SQL predicate
/// selected `active = true` (only the active partition, which QuestDB refuses
/// to detach and which is never older than the cutoff), so the sweep detached
/// nothing. Detaching only INACTIVE partitions can never touch the active /
/// current-day partition, in the common case AND in the edge case where a table
/// has not been written for more than `retention_days` (its active partition
/// would pass the SQL cutoff but is still excluded here).
pub(crate) fn select_partitions_to_detach(rows: &[PartitionRow]) -> Vec<String> {
    rows.iter()
        .filter(|row| !row.active && is_valid_partition_name(&row.name))
        .map(|row| row.name.clone())
        .collect()
}

/// Parses partition rows (`name`, `active`) from QuestDB's JSON response.
///
/// Expected format:
/// `{"columns":[{"name":"name",...},{"name":"active",...}],"dataset":[["2026-04-01",false],...],...}`
///
/// Columns are located BY NAME (robust to projection reordering). A row missing
/// the `name` or `active` field is skipped (fail-closed — never selected for
/// detach), so a malformed response can never panic or detach the wrong set.
pub(crate) fn parse_partition_rows(json: &str) -> Vec<PartitionRow> {
    // O(1) EXEMPT: begin — JSON parsing for DDL response (cold path, not hot)
    let parsed: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Map column name -> index so we do not rely on projection order.
    let columns = match parsed.get("columns").and_then(|c| c.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };
    let mut name_idx: Option<usize> = None;
    let mut active_idx: Option<usize> = None;
    for (idx, col) in columns.iter().enumerate() {
        match col.get("name").and_then(|n| n.as_str()) {
            Some("name") => name_idx = Some(idx),
            Some("active") => active_idx = Some(idx),
            _ => {}
        }
    }
    let (name_idx, active_idx) = match (name_idx, active_idx) {
        (Some(n), Some(a)) => (n, a),
        _ => return Vec::new(),
    };

    let dataset = match parsed.get("dataset").and_then(|d| d.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };

    dataset
        .iter()
        .filter_map(|row| {
            let cols = row.as_array()?;
            let name = cols.get(name_idx)?.as_str()?.to_string();
            let active = cols.get(active_idx)?.as_bool()?;
            Some(PartitionRow { name, active })
        })
        .collect()
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hour_partitioned_tables_not_empty() {
        assert!(!HOUR_PARTITIONED_TABLES.is_empty());
    }

    #[test]
    fn test_day_partitioned_tables_not_empty() {
        assert!(!DAY_PARTITIONED_TABLES.is_empty());
    }

    #[test]
    fn test_all_table_names_lowercase() {
        for table in HOUR_PARTITIONED_TABLES {
            assert_eq!(
                *table,
                table.to_ascii_lowercase(),
                "table name must be lowercase: {}",
                table
            );
        }
        for table in DAY_PARTITIONED_TABLES {
            assert_eq!(
                *table,
                table.to_ascii_lowercase(),
                "table name must be lowercase: {}",
                table
            );
        }
    }

    #[test]
    fn test_no_duplicate_tables() {
        let mut all_tables: Vec<&str> =
            Vec::with_capacity(HOUR_PARTITIONED_TABLES.len() + DAY_PARTITIONED_TABLES.len());
        all_tables.extend_from_slice(HOUR_PARTITIONED_TABLES);
        all_tables.extend_from_slice(DAY_PARTITIONED_TABLES);
        all_tables.sort();
        let before = all_tables.len();
        all_tables.dedup();
        assert_eq!(
            before,
            all_tables.len(),
            "duplicate table in partition lists"
        );
    }

    /// Renamed from `test_ticks_is_the_only_hour_table` on 2026-08-15, when
    /// `market_depth` joined the list. A test whose NAME asserts one thing
    /// while its body asserts another is worse than no test: the next reader
    /// greps the name, believes it, and never opens the body.
    ///
    /// Both entries are HOUR-partitioned for the same reason — they are the
    /// two highest-volume tables on the box, and an hourly partition is what
    /// makes archive→verify→drop possible without dropping a partition that
    /// is still being written. They do NOT share a retention window:
    /// `market_depth` is its own `RetentionClass::Depth` (see
    /// `partition_archive::retention_class`), because at ~21 GB/day the
    /// 35-day market-data window would commit 735 GB on a 100 GB root.
    #[test]
    fn test_hour_partitioned_list_is_ticks_and_market_depth() {
        assert_eq!(HOUR_PARTITIONED_TABLES, &["ticks", "market_depth"]);
    }

    #[test]
    fn test_day_list_holds_the_audit_data_tables() {
        // The DAY const holds ONLY the audit/data tables; candle tables are
        // swept via candle_table_names() in detach_old_partitions (see
        // test_candle_tables_are_real_plain_names).
        // NOTE: `index_constituency` was MOVED out of this list to
        // RETENTION_EXEMPT_TABLES (2026-06-28, ts-pin fix) — see
        // test_index_constituency_is_exempt_never_swept below.
        for live in [
            "instrument_fetch_audit",
            "instrument_lifecycle_audit",
            "prev_day_ohlcv",
            "cross_verify_1m_audit",
        ] {
            assert!(
                DAY_PARTITIONED_TABLES.contains(&live),
                "audit/data table not covered by retention sweep: {live}"
            );
        }
        // index_constituency must NOT be in the DAY sweep list anymore — its
        // pinned-ts rows all sit in the 1970 partition, which the sweep would
        // detach every run.
        assert!(
            !DAY_PARTITIONED_TABLES.contains(&"index_constituency"),
            "index_constituency must be RETENTION_EXEMPT, not DAY-swept (ts pinned to epoch 0)"
        );
    }

    #[test]
    fn test_index_constituency_is_exempt_never_swept() {
        // ts-pin fix (2026-06-28): `index_constituency` is a CURRENT-STATE master
        // with its designated ts pinned to epoch 0 → all rows in the 1970
        // partition. It must be exempt (like instrument_lifecycle) so the
        // retention sweep never detaches that >90d partition.
        assert!(
            RETENTION_EXEMPT_TABLES.contains(&"index_constituency"),
            "index_constituency must be retention-exempt (pinned-ts current-state master)"
        );
        assert!(
            !DAY_PARTITIONED_TABLES.contains(&"index_constituency"),
            "index_constituency must NOT be DAY-swept"
        );
        assert!(
            !HOUR_PARTITIONED_TABLES.contains(&"index_constituency"),
            "index_constituency must NOT be HOUR-swept"
        );
    }

    #[test]
    fn test_candle_tables_are_real_plain_names_not_shadow() {
        // The candle tables swept by detach_old_partitions come from the single
        // source of truth. They MUST be plain `candles_<TF>` (no `_shadow`) and
        // number 21 (M1/M3/M5/M15/D1 + S1..S15 + S30, TF-diet second-scale) —
        // the exact bug #1022 had (phantom `_shadow` names).
        let names = crate::shadow_persistence::candle_table_names();
        assert_eq!(names.len(), 24, "expected 24 live candle tables");
        for name in names {
            assert!(
                name.starts_with("candles_"),
                "candle table name not canonical: {name}"
            );
            assert!(
                !name.contains("_shadow"),
                "candle table must be plain (no _shadow): {name}"
            );
        }
    }

    #[test]
    fn test_instrument_lifecycle_is_exempt_never_swept() {
        // SEBI point-in-time master — never detached/dropped (daily-universe §5/§25).
        assert!(RETENTION_EXEMPT_TABLES.contains(&"instrument_lifecycle"));
        assert!(
            !HOUR_PARTITIONED_TABLES.contains(&"instrument_lifecycle"),
            "instrument_lifecycle must NEVER be swept (SEBI)"
        );
        assert!(
            !DAY_PARTITIONED_TABLES.contains(&"instrument_lifecycle"),
            "instrument_lifecycle must NEVER be swept (SEBI)"
        );
    }

    #[test]
    fn test_exempt_tables_never_appear_in_a_sweep_list() {
        for ex in RETENTION_EXEMPT_TABLES {
            assert!(!HOUR_PARTITIONED_TABLES.contains(ex));
            assert!(!DAY_PARTITIONED_TABLES.contains(ex));
        }
    }

    // ---- pure selection-predicate tests (the retention no-op fix) ----------

    fn row(name: &str, active: bool) -> PartitionRow {
        PartitionRow {
            name: name.to_string(),
            active,
        }
    }

    #[test]
    fn test_select_partitions_to_detach_excludes_active_selects_inactive() {
        // Fixture already restricted to <cutoff by the SQL: two aged inactive
        // partitions + the active current-day partition.
        let rows = vec![
            row("2026-01-01", false),
            row("2026-01-02", false),
            row("2026-07-09", true), // active current-day partition
        ];
        let selected = select_partitions_to_detach(&rows);
        assert_eq!(selected, vec!["2026-01-01", "2026-01-02"]);
        // The active partition must NEVER be selected for detach.
        assert!(
            !selected.contains(&"2026-07-09".to_string()),
            "active partition must never be detached"
        );
    }

    #[test]
    fn test_regression_old_active_true_predicate_would_detach_wrong_set() {
        // Regression: 2026-07-09 — the OLD `active = true` predicate selected
        // the active partition (the one QuestDB refuses to detach) while the
        // FIXED predicate selects the aged inactive partitions and excludes the
        // active one. Proves the fix is NON-VACUOUS (the two disagree).
        let rows = vec![
            row("2026-01-01", false),
            row("2026-01-02", false),
            row("2026-07-09", true),
        ];
        // Mimic the OLD buggy selection (`active == true`).
        let old_buggy: Vec<String> = rows
            .iter()
            .filter(|r| r.active)
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(
            old_buggy,
            vec!["2026-07-09"],
            "old predicate picked the active partition"
        );
        let fixed = select_partitions_to_detach(&rows);
        assert_ne!(fixed, old_buggy, "fix must diverge from the old predicate");
        assert!(!fixed.contains(&"2026-07-09".to_string()));
        assert_eq!(fixed, vec!["2026-01-01", "2026-01-02"]);
    }

    #[test]
    fn test_select_partitions_to_detach_empty_when_all_active() {
        // A table whose only partition is the active one → detach nothing.
        let rows = vec![row("2026-07-09", true)];
        assert!(select_partitions_to_detach(&rows).is_empty());
    }

    #[test]
    fn test_select_partitions_to_detach_empty_input() {
        assert!(select_partitions_to_detach(&[]).is_empty());
    }

    #[test]
    fn test_is_valid_partition_name_accepts_day_and_hour() {
        assert!(is_valid_partition_name("2026-04-01")); // DAY
        assert!(is_valid_partition_name("2026-04-01T09")); // HOUR
    }

    #[test]
    fn test_is_valid_partition_name_rejects_injection_and_garbage() {
        // A quote / space / DDL keyword can never reach the DETACH statement.
        assert!(!is_valid_partition_name("2026-04-01'; DROP TABLE ticks--"));
        assert!(!is_valid_partition_name("2026-04-01 09"));
        assert!(!is_valid_partition_name("'; DELETE"));
        assert!(!is_valid_partition_name(""));
        assert!(!is_valid_partition_name(
            "2026-04-01T09-with-a-very-long-suffix"
        ));
    }

    #[test]
    fn test_select_partitions_to_detach_skips_injection_name() {
        // A hostile / corrupt partition name is inactive but must NOT be
        // selected for detach (fail-closed allowlist).
        let rows = vec![
            row("2026-01-01", false),
            row("2026-01-02'; DROP TABLE ticks--", false),
        ];
        assert_eq!(select_partitions_to_detach(&rows), vec!["2026-01-01"]);
    }

    #[test]
    fn test_detach_list_sql_uses_inactive_not_active() {
        // Extreme-check ratchet: the selection must not reintroduce the buggy
        // `active = true` predicate, and must project `active` for the pure
        // Rust exclusion.
        let sql = build_detach_list_sql("ticks", 90);
        assert!(
            !sql.contains("active = true"),
            "must not resurrect the no-op `active = true` predicate: {sql}"
        );
        assert!(
            sql.contains("SELECT name, active"),
            "must project active: {sql}"
        );
        assert!(
            sql.contains("minTimestamp < dateadd('d', -90, now())"),
            "cutoff filter must stay server-side: {sql}"
        );
    }

    // ---- parse_partition_rows tests ---------------------------------------

    #[test]
    fn test_parse_partition_rows_valid() {
        let json = r#"{"columns":[{"name":"name","type":"VARCHAR"},{"name":"active","type":"BOOLEAN"}],"dataset":[["2026-04-01",false],["2026-04-02",true]],"count":2}"#;
        let rows = parse_partition_rows(json);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], row("2026-04-01", false));
        assert_eq!(rows[1], row("2026-04-02", true));
    }

    #[test]
    fn test_parse_partition_rows_column_order_independent() {
        // active projected BEFORE name — parser locates columns by name.
        let json = r#"{"columns":[{"name":"active","type":"BOOLEAN"},{"name":"name","type":"VARCHAR"}],"dataset":[[false,"2026-04-01"]],"count":1}"#;
        let rows = parse_partition_rows(json);
        assert_eq!(rows, vec![row("2026-04-01", false)]);
    }

    #[test]
    fn test_parse_partition_rows_empty_dataset() {
        let json = r#"{"columns":[{"name":"name","type":"VARCHAR"},{"name":"active","type":"BOOLEAN"}],"dataset":[],"count":0}"#;
        assert!(parse_partition_rows(json).is_empty());
    }

    #[test]
    fn test_parse_partition_rows_invalid_json() {
        assert!(parse_partition_rows("not json").is_empty());
    }

    #[test]
    fn test_parse_partition_rows_missing_dataset() {
        let json =
            r#"{"columns":[{"name":"name","type":"VARCHAR"},{"name":"active","type":"BOOLEAN"}]}"#;
        assert!(parse_partition_rows(json).is_empty());
    }

    #[test]
    fn test_parse_partition_rows_missing_active_column() {
        // Without an `active` column we cannot safely decide — return nothing.
        let json = r#"{"columns":[{"name":"name","type":"VARCHAR"}],"dataset":[["2026-04-01"]],"count":1}"#;
        assert!(parse_partition_rows(json).is_empty());
    }

    #[test]
    fn test_parse_partition_rows_hour_format() {
        let json = r#"{"columns":[{"name":"name","type":"VARCHAR"},{"name":"active","type":"BOOLEAN"}],"dataset":[["2026-04-01T09",false],["2026-04-01T10",true]],"count":2}"#;
        let rows = parse_partition_rows(json);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], row("2026-04-01T09", false));
    }

    #[test]
    fn test_parse_partition_rows_panic_safety_malformed_row() {
        // A row missing the active value (wrong-typed / short) is skipped, not
        // panicked over, and never selected for detach.
        let json = r#"{"columns":[{"name":"name","type":"VARCHAR"},{"name":"active","type":"BOOLEAN"}],"dataset":[["2026-04-01"],["2026-04-02",false]],"count":2}"#;
        let rows = parse_partition_rows(json);
        assert_eq!(rows, vec![row("2026-04-02", false)]);
        assert!(select_partitions_to_detach(&rows).contains(&"2026-04-02".to_string()));
    }

    // (removed test_obi_in_hour_partitioned_tables — `obi_snapshots` is a
    // deleted table; the 2026-06-05 retention fix dropped it from the list.)

    #[test]
    fn test_ticks_in_hour_partitioned_tables() {
        assert!(
            HOUR_PARTITIONED_TABLES.contains(&"ticks"),
            "ticks must be in HOUR-partitioned list"
        );
    }

    #[test]
    fn test_default_retention_config() {
        use tickvault_common::config::PartitionRetentionConfig;
        let config = PartitionRetentionConfig::default();
        assert_eq!(config.retention_days, 90);
    }
}
