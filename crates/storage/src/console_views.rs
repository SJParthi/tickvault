//! Human-readable analyst console views — `ticks_named` + `candles_named`.
//!
//! Plain (non-materialized) QuestDB views that LEFT JOIN the raw
//! `ticks` / `candles_1m` tables against the `instrument_lifecycle`
//! master so an operator pasting `SELECT * FROM ticks_named WHERE
//! symbol_name = 'NIFTY'` into the QuestDB web console gets a
//! human-readable answer without hand-writing the composite join.
//!
//! **Cold-path analyst tooling ONLY.** A plain view is computed at
//! query time — O(join) at SELECT time, honestly O(N), NEVER claimed
//! O(1). No writer / indicator / strategy / risk code reads these
//! views (the RAM-first hot-path SELECT ban is untouched); the boot
//! cost is a handful of idempotent, timeout-bounded HTTP GETs.
//!
//! ## Join correctness
//!
//! * **Duplicate-safety HONEST ENVELOPE (review round 2):** the
//!   lifecycle master's designated `ts` is a PINNED epoch-0 constant
//!   (`lifecycle_designated_ts_nanos() -> 0`, I-P1-08), so **while its
//!   DEDUP is live** the key `(ts, security_id, exchange_segment,
//!   feed)` collapses to exactly ONE row per `(security_id,
//!   exchange_segment, feed)` and the join cannot multiply rows. That
//!   precondition is CONDITIONAL — two already-documented degraded
//!   windows leave PERSISTENT same-key duplicates that QuestDB's later
//!   `DEDUP ENABLE` does NOT retro-collapse (it applies to future
//!   writes only), and then every joined fact row is N-fold
//!   multiplied in the views:
//!   1. the lifecycle HTTP-CLIENT-01 degrade arm (`lifecycle_ensure`
//!      site, `instrument_lifecycle_persistence.rs`) — the table is
//!      ILP-auto-created WITHOUT dedup keys and every reconcile
//!      UPSERT in that window appends full duplicate row sets;
//!   2. a partial `run_lifecycle_feed_self_heal` boot where the
//!      NULL→'dhan' backfill step fails but `DEDUP ENABLE` succeeds —
//!      the NEXT boot's backfill mints same-key duplicates.
//!   Detector (copy-paste form in the runbook; QuestDB does not
//!   support standard SQL HAVING, so this is the documented
//!   subquery + WHERE equivalent):
//!   `SELECT * FROM (SELECT security_id, exchange_segment, feed,
//!   count() AS n FROM instrument_lifecycle GROUP BY security_id,
//!   exchange_segment, feed) WHERE n > 1;`
//!   Deliberately NO `LATEST ON` collapse in the view: it is un-probed
//!   QuestDB-9.3.5-inside-a-VIEW territory (evidence discipline —
//!   zero-loss charter §4), and silently collapsing would HIDE the
//!   master-table corruption the detector exists to surface (audit
//!   Rule 11: the duplicates are the bug; remediation is the manual
//!   dedup sweep already named by `http-client-error-codes.md` §1,
//!   an operator decision under the lifecycle no-DELETE lock).
//! * Join predicate is the I-P1-11 composite `(security_id, segment)`
//!   PLUS `feed` (feed-in-key everywhere, operator 2026-06-28): a
//!   Dhan row and a Groww row for the same instrument coexist by
//!   design, and Groww bit-62 synthetic index ids exist only under
//!   `feed = 'groww'`.
//! * LEFT JOIN + `dry_run = false` INSIDE the dimension subquery (an
//!   outer WHERE would break LEFT semantics; §27 dry-run isolation).
//!   An instrument absent from the lifecycle master surfaces as a
//!   NULL `symbol_name` row — itself a diagnostic (audit Rule 11: a
//!   vanishing row would be a false-OK), never a dropped row.
//!
//! ## Idempotency + convergence
//!
//! The DDL is `CREATE OR REPLACE VIEW` (probe-Verified on the pinned
//! QuestDB 9.3.5): every boot CONVERGES the deployed definition to the
//! code, so a future definition change self-heals on the next boot with
//! zero manual prod steps (`aws-budget.md` rule 8). `IF NOT EXISTS`
//! would 2xx-no-op on a changed definition — a stale-definition
//! false-OK (audit Rule 11) — and is ratchet-banned.
//!
//! ## Call sites (every feed-enabled boot mode)
//!
//! 1. `crates/app/src/main.rs` FAST crash-recovery arm (Dhan)
//! 2. `crates/app/src/main.rs::start_dhan_lane` slow arm (Dhan,
//!    incl. the D2b runtime cold-start)
//! 3. `crates/app/src/groww_activation.rs::activate_groww_lane` — so a
//!    Groww-only boot (`feeds.dhan_enabled = false`, the scale-test /
//!    groww-only lab mode) creates the views too. Double execution on
//!    dual-feed boots is harmless (convergent DDL).
//!
//! A boot with BOTH feeds disabled ensures no base tables and creates
//! no views — nothing streams in that mode, so there is nothing to name.
//!
//! Runbook: `docs/runbooks/questdb-console-queries.md`.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::QUESTDB_TABLE_TICKS;

#[cfg(test)]
use crate::top_volume_rank_persistence::SnapshotCadence;

/// Wire-format name of the human-readable ticks console view.
pub const VIEW_TICKS_NAMED: &str = "ticks_named";
/// Wire-format name of the human-readable 1m-candles console view.
pub const VIEW_CANDLES_NAMED: &str = "candles_named";
/// Live 1m candle base table (Engine B plain table per #T1a — matches
/// `TfIndex::table_name()` for M1; storage cannot depend on trading,
/// so the name is pinned here).
pub const NAMED_VIEW_CANDLES_BASE: &str = "candles_1m";
/// Mirrors `instrument_lifecycle_persistence::QUESTDB_TABLE_INSTRUMENT_LIFECYCLE`.
/// (Historically hardcoded because that module was feature-gated; the
/// `daily_universe_fetcher` feature was deleted in PR-C3, 2026-07-14.)
/// Equality is pinned by the ratchet test
/// `test_lifecycle_dim_matches_persistence_const`.
const NAMED_VIEW_LIFECYCLE_DIM: &str = "instrument_lifecycle";
/// Wire-format name of the DERIVED 10-minute candle view.
///
/// The operator's 2026-09-18 timeframe list ends
/// `… 5m, 10m, 15m, 30m, 60m`, and his ruling on this one entry was explicit:
/// *"no 10s derive the 10m"*. So `M10` is deliberately NOT a `TfIndex`
/// variant — adding one would move `TF_COUNT`, resize the seal ring
/// (`AGGREGATOR_MAX_SLOTS × TF_COUNT`), and add a tenth scalar fold to every
/// tick, for a frame that is exactly recoverable from the 1m bars.
pub const VIEW_CANDLES_10M: &str = "candles_10m";
/// Wire-format name of the human-readable market-depth console view.
pub const VIEW_DEPTH_NAMED: &str = "market_depth_named";
/// Market-depth base table. Mirrors
/// `depth_persistence::MARKET_DEPTH_TABLE`; equality is pinned by
/// `test_depth_base_matches_persistence_const`.
const NAMED_VIEW_DEPTH_BASE: &str = "market_depth";
// The per-cadence top-volume VIEWS (and their base-table const) lived here
// until 2026-09-22. They are now four DIRECT TABLES owned by
// `top_volume_rank_persistence` (`SnapshotCadence::table_name()`), written
// `ts ASC, volume_percentage_change DESC` at append time — see the
// "2026-09-22" section of `websocket-connection-scope-lock.md`.

/// The FOUR per-cadence view names this table's views carried BEFORE the
/// 2026-09-12 `top_volume_rank` → `top_volume` rename.
///
/// # Why this list exists at all, and why it is not cosmetic
///
/// A QuestDB view is stored as its SQL TEXT and resolved at query time, so
/// renaming the base table does not carry its views across: after the rename
/// these four still name `top_volume_rank`, a table that no longer exists.
/// Left alone they are four permanently-broken surfaces sitting in the table
/// list beside the four working ones, each answering `SELECT * FROM
/// top_volume_rank_1s` with a resolution error — created by this repository,
/// on this repository's own box, by a rename this repository performed.
///
/// It is also a possible PRECONDITION for the rename, which is the reason the
/// sweep runs where it does. Whether QuestDB REFUSES to rename a table that
/// views depend on is **UNVERIFIED** — no QuestDB is reachable from a dev
/// container, so this could not be probed. If it does refuse, a rename
/// attempted with these views still present fails every boot forever and the
/// history stays stranded under the legacy name. Dropping them first removes
/// that failure mode whether or not it exists; dropping them second would not.
/// ⚠ 2026-09-22: there is no rename any more. `top_volume` became four
/// direct per-cadence TABLES (`top_volume_1s/3s/5s/1m`), created by
/// `ensure_top_volume_tables` on the live-table DDL step, and the fresh-start
/// reset drops both legacy tables outright. This sweep survives only so a box
/// that still carries these four views has them removed; the rename-ordering
/// argument above is history, kept per house convention.
///
/// # An explicit four-name list, never a prefix match
///
/// `top_volume_rank` — the legacy TABLE, which holds every ranking row
/// written before the rename — is a strict prefix of all four names here. A
/// sweep written as "drop everything starting `top_volume_rank`" would
/// therefore have the legacy table's own name in its blast radius, and
/// `shadow_persistence` already records what that costs: a table carrying
/// real tick volume whose name sat in a `DROP TABLE IF EXISTS` sweep. These
/// are four literals, and `the_legacy_view_sweep_can_never_name_a_table`
/// fails the build if any of them is ever a table name.
///
/// # Bounded and self-terminating
///
/// `DROP VIEW IF EXISTS` on an absent view is a no-op, so from the second
/// boot after the deploy this is four cheap statements that do nothing. It is
/// deliberately NOT made conditional on a probe: a probe is a second round
/// trip that can itself fail, and the statement it would guard is already the
/// idempotent form. `run_view_ddl` counts the outcome and warns on non-2xx
/// rather than panicking, so a QuestDB that does not accept the statement at
/// all degrades to a warn instead of blocking every other view behind it.
///
/// Fixed at four entries FOREVER: this is a historical fact about what was
/// once created, not a mirror of `SnapshotCadence`. A fifth cadence added
/// tomorrow never had a `top_volume_rank_*` view, so it must NOT appear here
/// — which is why this is a literal array and not derived from the enum, and
/// why `the_legacy_view_list_is_history_not_a_mirror_of_the_enum` pins it.
const LEGACY_TOP_VOLUME_VIEWS: [&str; 4] = [
    "top_volume_rank_1s",
    "top_volume_rank_3s",
    "top_volume_rank_5s",
    "top_volume_rank_1m",
];

/// DDL HTTP timeout (same value as every other boot-DDL ensure site).
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DDL that removes one pre-rename top-volume view.
///
/// A FUNCTION rather than a `format!` inline at the call site, and the
/// distinction is the whole reason this exists: the first draft of
/// `the_legacy_view_drop_is_idempotent_and_one_statement` built its own copy
/// of this string, so deleting `IF EXISTS` from the production site left the
/// test green. A guard that constructs the artefact it is checking is testing
/// itself — the same vacuous shape as the `2X >= X` assert this PR replaces,
/// written by the same hands on the same day. Both the sweep and the test now
/// read this one function.
fn legacy_top_volume_view_drop_ddl(view: &str) -> String {
    format!("DROP VIEW IF EXISTS {view};")
}

/// The shared lifecycle dimension subquery both view DDLs join against.
///
/// `dry_run = false` lives INSIDE the subquery so the outer LEFT JOIN
/// semantics are preserved (§27 dry-run isolation; an outer WHERE on a
/// dimension column would silently drop unmapped fact rows).
fn lifecycle_dim_subquery() -> String {
    format!(
        "(SELECT security_id, exchange_segment, feed, symbol_name, display_name, \
         instrument_type FROM {NAMED_VIEW_LIFECYCLE_DIM} WHERE dry_run = false) il"
    )
}

/// DDL for the `ticks_named` view: every `ticks` column that matters to
/// an analyst (only `payload_hash` plumbing omitted), identity-first
/// column order, LEFT-joined against the lifecycle master.
pub fn ticks_named_view_ddl() -> String {
    let dim = lifecycle_dim_subquery();
    format!(
        "CREATE OR REPLACE VIEW {VIEW_TICKS_NAMED} AS \
         SELECT t.ts, il.symbol_name, il.display_name, il.instrument_type, \
         t.ltp, t.open, t.high, t.low, t.close, t.volume, t.oi, t.avg_price, \
         t.last_trade_qty, t.total_buy_qty, t.total_sell_qty, \
         t.feed, t.segment, t.security_id, t.exchange_timestamp, t.received_at, \
         t.capture_seq \
         FROM {QUESTDB_TABLE_TICKS} t \
         LEFT JOIN {dim} \
         ON t.security_id = il.security_id \
         AND t.segment = il.exchange_segment \
         AND t.feed = il.feed;"
    )
}

/// DDL for the `candles_named` view: every `candles_1m` column,
/// identity-first column order, LEFT-joined against the lifecycle master.
///
/// `c.volume` is the SIGNED gross volume (2026-09-18) and `net_volume` is
/// gone from the base table, so the view no longer selects it — a view that
/// named a dropped column would fail to create on a fresh volume and leave
/// the operator with no named candle face at all.
pub fn candles_named_view_ddl() -> String {
    let dim = lifecycle_dim_subquery();
    format!(
        "CREATE OR REPLACE VIEW {VIEW_CANDLES_NAMED} AS \
         SELECT c.ts, il.symbol_name, il.display_name, il.instrument_type, \
         c.open, c.high, c.low, c.close, c.volume, c.oi, c.tick_count, \
         c.total_buy_qty, c.total_sell_qty, \
         c.feed, c.segment, c.security_id, \
         c.percentage_change, c.open_percentage_change \
         FROM {NAMED_VIEW_CANDLES_BASE} c \
         LEFT JOIN {dim} \
         ON c.security_id = il.security_id \
         AND c.segment = il.exchange_segment \
         AND c.feed = il.feed;"
    )
}

/// DDL for `candles_10m` — the tenth timeframe of the 2026-09-18 list,
/// DERIVED from `candles_1m` rather than folded per tick.
///
/// # Why a view and not a `TfIndex` variant
///
/// The operator ruled this entry explicitly: *"no 10s derive the 10m"*. A
/// tenth fold frame would move `TF_COUNT` 9 → 10, which resizes the seal
/// ring (`AGGREGATOR_MAX_SLOTS × TF_COUNT`, 225,000 today), shifts nothing
/// on disk but adds a scalar fold to EVERY tick — all for a frame that is
/// recoverable exactly. Zero per-tick work is the point.
///
/// # Why the derivation is EXACT, and what makes it so
///
/// `abs(volume) == gross` holds on every persisted bar, because the
/// 2026-09-18 rule signs the whole magnitude and never zeroes it
/// (`LiveCandleState::signed_volume`). So the gross of a 10-minute bucket is
/// `sum(abs(volume))` of its 1m bars — no information was lost to recover.
///
/// That invariant is the whole reason the stored form is ±gross rather than
/// net flow: a flat bar stored as `0`, TradingView's convention, would have
/// destroyed its own magnitude and made this derivation impossible. A view
/// can always render the zero-on-flat form from ±gross; the reverse cannot be
/// done at all.
///
/// # The sign, applied at the 10m level
///
/// Clause 4 of the directive compares a bar's close against the PREVIOUS
/// bar's close **of the same timeframe**, so the sign cannot be summed up
/// from the 1m bars — five `+100` and five `-100` 1m bars sum to `0` while
/// the 10m bar is `±1000`. The gross is summed; the sign is derived once, at
/// this level, from `lag(close)` over the same instrument.
///
/// A bucket with no predecessor (`prev_close` NULL, or a non-positive value)
/// signs POSITIVE — the same "no baseline" default `signed_volume()` applies,
/// and not a claim that the close rose.
///
/// # The day partition, which is NOT cosmetic
///
/// The window is partitioned by IST DAY as well as by instrument,
/// because the native fold refuses a cross-day baseline and says why in
/// as many words (`aggregator_cell::net_volume_baseline`): *"Signing
/// today's first bar against yesterday's close reports an OVERNIGHT GAP
/// as intraday direction, on exactly the bar an operator looks at
/// hardest."* Without the day key this view re-created exactly that
/// defect, on the first bucket of every day and worse across a weekend:
/// an instrument that gapped DOWN overnight and then ROSE through
/// 09:00-09:10 would have read negative here and positive in
/// `candles_1m`, so the two tables would contradict each other on the
/// most-read bar of the session. `date_trunc` is the lowest-risk
/// construct available for it -- `date_trunc('minute', ts)` is already
/// live against this QuestDB in `feed_scoreboard_boot`, so unlike
/// `SAMPLE BY` and `lag()` it is not a first use.
///
/// # ⚠ UNVERIFIED against a live QuestDB
///
/// No QuestDB was reachable when this was written (no docker daemon in the
/// build container), and this is the first view in this module to use
/// `SAMPLE BY` or a window function — there is no precedent here to copy. The
/// failure mode is bounded and already engineered for: `run_view_ddl`
/// degrades a refusal to a counted `warn!` and never blocks the views behind
/// it, so a dialect rejection costs one log line and the other console views
/// still create. The first boot after deploy is the measurement.
pub fn candles_10m_view_ddl() -> String {
    format!(
        "CREATE OR REPLACE VIEW {VIEW_CANDLES_10M} AS \
         SELECT b.ts, b.security_id, b.segment, b.feed, \
         b.open, b.high, b.low, b.close, \
         CASE WHEN b.prev_close > 0 AND b.close < b.prev_close \
         THEN -b.gross ELSE b.gross END AS volume, \
         b.oi, b.tick_count \
         FROM ( \
         SELECT a.*, \
         lag(a.close) OVER ( \
         PARTITION BY a.security_id, a.segment, a.feed, \
         date_trunc('day', a.ts) ORDER BY a.ts \
         ) AS prev_close \
         FROM ( \
         SELECT ts, security_id, segment, feed, \
         first(open) AS open, max(high) AS high, min(low) AS low, \
         last(close) AS close, sum(abs(volume)) AS gross, \
         last(oi) AS oi, sum(tick_count) AS tick_count \
         FROM {NAMED_VIEW_CANDLES_BASE} SAMPLE BY 10m \
         ) a \
         ) b;"
    )
}

/// DDL for the `market_depth_named` view — the human-readable face of the ONE
/// COMMON depth table (operator 2026-08-15).
///
/// `depth_kind` is projected immediately after the identity columns rather
/// than buried among the numbers, deliberately: it is the column that says
/// WHICH book a row came from, and an analyst who cannot see it at a glance
/// will read a depth-20 row and a depth-200 row as the same observation. It is
/// also the DEDUP discriminator, so a row where it is unexpectedly NULL is the
/// first sign the table was auto-created without its keys.
///
/// `ORDER BY` is deliberately absent: at 20–200 rows per packet an implicit
/// sort would make every ad-hoc query expensive. Analysts filter first
/// (`WHERE symbol_name = … AND depth_kind = 'd200'`) and sort what remains.
pub fn depth_named_view_ddl() -> String {
    let dim = lifecycle_dim_subquery();
    format!(
        "CREATE OR REPLACE VIEW {VIEW_DEPTH_NAMED} AS \
         SELECT d.ts, il.symbol_name, il.display_name, il.instrument_type, \
         d.depth_kind, d.side, d.level, d.price, d.quantity, d.orders, \
         d.feed, d.segment, d.security_id, d.capture_seq \
         FROM {NAMED_VIEW_DEPTH_BASE} d \
         LEFT JOIN {dim} \
         ON d.security_id = il.security_id \
         AND d.segment = il.exchange_segment \
         AND d.feed = il.feed;"
    )
}

// `top_volume_cadence_view_ddl` was DELETED 2026-09-22: the four per-cadence
// top-volume faces are no longer views over one table but four direct tables
// (`top_volume_rank_persistence::top_volume_create_ddl`). The candle-derived
// `candle_lots` column the view computed is `volume / per_lot_quantity` over
// the stored columns, so nothing is lost that a query cannot rebuild.

/// Issue one view-DDL statement to QuestDB's `/exec` endpoint.
///
/// Mirrors `shadow_persistence::run_ddl` levels exactly: `/exec` is
/// GET-only (POST 405s per the #T1a regression note); 2xx → `info!`,
/// non-2xx → `warn!` (retries next boot — idempotent), transport
/// `Err` → `error!`.
async fn run_view_ddl(client: &Client, base_url: &str, view: &str, action: &str, ddl: &str) {
    match client.get(base_url).query(&[("query", ddl)]).send().await {
        Ok(resp) if resp.status().is_success() => {
            metrics::counter!(VIEW_DDL_COUNTER, "outcome" => "ok").increment(1);
            info!(view, action, "named console view DDL accepted");
        }
        Ok(resp) => {
            metrics::counter!(VIEW_DDL_COUNTER, "outcome" => "non_2xx").increment(1);
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let body_prefix: String = body.chars().take(200).collect();
            warn!(view, action, %status, body = %body_prefix, "named view DDL non-2xx — retries next boot");
        }
        Err(err) => {
            metrics::counter!(VIEW_DDL_COUNTER, "outcome" => "transport").increment(1);
            error!(view, action, ?err, "named view DDL request failed");
        }
    }
}

/// Counter: named-view DDL statements at boot, by `outcome` (`ok`,
/// `non_2xx`, `transport`).
///
/// Added 2026-09-08 after the hostile sweep found a refused view DDL was a
/// `warn!` and nothing else — a `top_volume_1s` view absent for a whole
/// session had no number behind it. A view is a READ projection, so its
/// absence loses no data (the base table keeps every row) and this is
/// deliberately NOT loss-shaped and NOT EMF-selected: the operator's
/// question is "did my view come up?", answered locally on `/metrics` and by
/// the coded line beside it.
pub const VIEW_DDL_COUNTER: &str = "tv_console_view_ddl_total";

/// Every outcome [`VIEW_DDL_COUNTER`] carries, for pre-registration.
pub const VIEW_DDL_OUTCOMES: [&str; 3] = ["ok", "non_2xx", "transport"];

/// Seeds every outcome series at zero so the first refusal is a delta the
/// scrape can see rather than a dropped first sample.
pub fn pre_register_view_ddl_counter() {
    for outcome in VIEW_DDL_OUTCOMES {
        metrics::counter!(VIEW_DDL_COUNTER, "outcome" => outcome).increment(0);
    }
}

/// Idempotently create-or-converge the `ticks_named` + `candles_named`
/// analyst console views (`CREATE OR REPLACE VIEW` — every boot converges
/// the deployed definition to the code). Fail-soft: never `Result`, never
/// panic — a failure logs and the next boot re-runs (house `ensure_*` norm).
///
/// Call ONLY after the base-table ensures (`ensure_tick_table_dedup_keys`
/// + `ensure_shadow_candle_tables`, or the Groww delegating wrappers) have
/// completed, so `ticks` + `candles_1m` exist before CREATE VIEW validates
/// its column references. Safe to call from multiple feed lanes — the DDL
/// is convergent, so double execution on dual-feed boots is harmless.
// TEST-EXEMPT: requires a running QuestDB; the DDL strings are ratcheted by the pure-builder unit tests below; exercised by boot integration + `make doctor`.
pub async fn ensure_named_views(questdb_config: &QuestDbConfig) {
    pre_register_view_ddl_counter();
    // FEATURE-GATE FIX: the lifecycle dimension table must exist before
    // CREATE VIEW validates the join. Prod app builds enable
    // `daily_universe_fetcher` by default (crates/app/Cargo.toml); without
    // the feature the lifecycle table never exists and the CREATE VIEW
    // warn-fails harmlessly each boot (documented; those builds never
    // write lifecycle rows anyway). The daily-universe cold path's own
    // lifecycle ensure runs too late / conditionally for this call site.
    crate::instrument_lifecycle_persistence::ensure_instrument_lifecycle_table(questdb_config)
        .await;

    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    // C2 pattern (2026-07-03): panic-free client build — Client::new()
    // panics on TLS/resolver/fd init failure (silent tokio-task death).
    // Degrade: skip the view DDL this boot; the next boot re-runs it
    // (idempotent) — read-only projections, no data path affected.
    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            error!(
                error = %err,
                code = tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed.code_str(),
                "HTTP-CLIENT-01 reqwest client build failed — named-view DDL skipped: \
                 ticks_named/candles_named not created this boot; analyst console views \
                 unavailable until the next successful boot (idempotent re-run; read-only \
                 projections — no data path affected, no duplicate-row window)"
            );
            metrics::counter!(
                "tv_http_client_build_failed_total",
                "site" => "named_views_ensure"
            )
            .increment(1);
            return;
        }
    };

    // Both views attempted independently — one failing never blocks the other.
    run_view_ddl(
        &client,
        &base_url,
        VIEW_TICKS_NAMED,
        "create",
        &ticks_named_view_ddl(),
    )
    .await;
    run_view_ddl(
        &client,
        &base_url,
        VIEW_CANDLES_NAMED,
        "create",
        &candles_named_view_ddl(),
    )
    .await;
    // The derived tenth timeframe, attempted AFTER `candles_named` and
    // independently of it. It is the one view here that uses `SAMPLE BY` and a
    // window function, so it is also the one most likely to be refused by a
    // dialect detail — placing it after the everyday faces means a refusal
    // costs a warn and nothing an analyst opens daily.
    run_view_ddl(
        &client,
        &base_url,
        VIEW_CANDLES_10M,
        "create",
        &candles_10m_view_ddl(),
    )
    .await;
    // Depth joined its siblings 2026-08-15. It is attempted LAST and
    // independently: on a box whose `market_depth` table does not exist yet
    // (no depth socket has ever opened) this DDL warn-fails and the two views
    // an analyst uses every day are already created.
    run_view_ddl(
        &client,
        &base_url,
        VIEW_DEPTH_NAMED,
        "create",
        &depth_named_view_ddl(),
    )
    .await;
    // The pre-2026-09-12 `top_volume_rank_*` faces. See
    // `LEGACY_TOP_VOLUME_VIEWS` for why this is four literals rather than a
    // prefix sweep.
    for legacy in LEGACY_TOP_VOLUME_VIEWS {
        run_view_ddl(
            &client,
            &base_url,
            legacy,
            "drop_legacy",
            &legacy_top_volume_view_drop_ddl(legacy),
        )
        .await;
    }

    // The per-cadence top-volume VIEWS were created here until 2026-09-22.
    // They are now DIRECT TABLES (`top_volume_rank_persistence::
    // ensure_top_volume_tables`), which pre-drop a same-named view before creating the
    // table — so nothing here builds a top-volume object any more.
}

// ---------------------------------------------------------------------------
// Ratchet tests — pure DDL-string pins (house string-assert pattern).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Named `both_ddls` when there were two. Kept under its original name so
    // the shared assertions that call it stay greppable across history; it now
    // carries all THREE console views, and every new view must join it or the
    // shared invariants (single statement, LEFT JOIN, dry-run isolation) would
    // silently not apply to it.
    fn both_ddls() -> Vec<(&'static str, String)> {
        // A `Vec`, not a fixed-size array, since 2026-09-12: the top-volume
        // half is now `SnapshotCadence::ALL`, so the count moves with the
        // enum. The array form had to be hand-widened for every cadence, and
        // a forgotten widen dropped that cadence's view out of every shared
        // invariant below (single statement, LEFT JOIN, dry-run isolation)
        // while still compiling.
        // The four top-volume faces left this list on 2026-09-22, when they
        // became direct tables rather than views.
        vec![
            ("ticks_named", ticks_named_view_ddl()),
            ("candles_named", candles_named_view_ddl()),
            ("market_depth_named", depth_named_view_ddl()),
        ]
    }

    /// The sweep can never name a TABLE — the one way a drop list destroys
    /// data rather than tidying a surface.
    ///
    /// `top_volume_rank` (the legacy table, holding every ranking row written
    /// before the rename) and `top_volume` (the current one) are both
    /// prefixes or near-neighbours of the four view names, so a sweep written
    /// as a prefix match, or a fifth entry added carelessly, lands on real
    /// data. `shadow_persistence` records the precedent in its own words: a
    /// table carrying real tick volume whose name sat in a `DROP TABLE IF
    /// EXISTS` sweep.
    #[test]
    fn the_legacy_view_sweep_can_never_name_a_table() {
        for name in LEGACY_TOP_VOLUME_VIEWS {
            assert_ne!(
                name,
                crate::top_volume_rank_persistence::LEGACY_TOP_VOLUME_RANK_TABLE,
                "the legacy TABLE is in the drop sweep — this destroys the \
                 pre-rename ranking history"
            );
            assert_ne!(
                name,
                crate::top_volume_rank_persistence::LEGACY_TOP_VOLUME_TABLE,
                "the retired shared table is in the drop sweep"
            );
            for c in SnapshotCadence::ALL {
                assert_ne!(
                    name,
                    c.table_name(),
                    "a live per-timeframe TABLE is in the sweep"
                );
            }
            // Every other table this crate ensures is out of range by
            // construction, but the two above share a prefix with all four
            // entries, which is exactly why the sweep is a literal list.
            assert!(
                name.starts_with("top_volume_rank_"),
                "{name}: a legacy view name, not something else"
            );
        }
    }

    /// The sweep can never drop a view the current code is about to CREATE.
    ///
    /// Both loops run in the same function, the drop first. An entry that
    /// collided with a current `table_name()` would drop the view and then
    /// immediately recreate it — harmless today, and a silent way to make the
    /// sweep look busy while achieving nothing. More importantly it would mean
    /// the rename had not actually changed that view's name, which is the
    /// premise the whole sweep rests on.
    #[test]
    fn the_legacy_view_sweep_never_collides_with_a_current_view() {
        for legacy in LEGACY_TOP_VOLUME_VIEWS {
            for c in SnapshotCadence::ALL {
                assert_ne!(legacy, c.table_name(), "legacy name equals a live table");
            }
            for live in [
                VIEW_TICKS_NAMED,
                VIEW_CANDLES_NAMED,
                VIEW_CANDLES_10M,
                VIEW_DEPTH_NAMED,
            ] {
                assert_ne!(legacy, live, "legacy name equals a live console view");
            }
        }
    }

    /// The list is HISTORY, not a mirror of the enum.
    ///
    /// It is fixed at the four faces that were once created under the old base
    /// name. A fifth cadence added tomorrow never had a `top_volume_rank_*`
    /// view, so it must not be swept — and the tempting "derive it from
    /// `SnapshotCadence`" refactor would add one, issuing a `DROP VIEW IF
    /// EXISTS` for a name that never existed. Harmless in effect, wrong in
    /// meaning, and it would quietly make this list unfalsifiable.
    #[test]
    fn the_legacy_view_list_is_history_not_a_mirror_of_the_enum() {
        assert_eq!(LEGACY_TOP_VOLUME_VIEWS.len(), 4, "fixed at four, forever");
        let mut sorted = LEGACY_TOP_VOLUME_VIEWS;
        sorted.sort_unstable();
        for pair in sorted.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate entry in the sweep");
        }
    }

    /// The statement is the idempotent form, so the second boot is a no-op.
    ///
    /// Without `IF EXISTS` this warns on every boot from the second onward —
    /// the "one error per boot, forever" shape the sibling rename comment
    /// already records as the thing that trains an operator to discount a
    /// counter.
    #[test]
    fn the_legacy_view_drop_is_idempotent_and_one_statement() {
        for legacy in LEGACY_TOP_VOLUME_VIEWS {
            let ddl = legacy_top_volume_view_drop_ddl(legacy);
            assert!(ddl.contains("IF EXISTS"), "{legacy}: not idempotent");
            assert_eq!(ddl.matches(';').count(), 1, "{legacy}: one statement");
            assert!(
                !ddl.contains("TABLE"),
                "{legacy}: drops a VIEW, never a table"
            );
        }
    }
    #[test]
    fn pre_register_view_ddl_counter_covers_every_outcome_and_does_not_panic() {
        pre_register_view_ddl_counter();
        pre_register_view_ddl_counter();
        assert_eq!(VIEW_DDL_OUTCOMES.len(), 3);
        assert!(VIEW_DDL_COUNTER.ends_with("_total"));
        assert!(
            !VIEW_DDL_COUNTER.contains("dropped")
                && !VIEW_DDL_COUNTER.contains("refused")
                && !VIEW_DDL_COUNTER.contains("lost"),
            "a view is a read projection; its DDL outcome is not loss-shaped"
        );
    }

    /// `net_volume_chg_pct` is a percentage CHANGE measured from one lot.
    ///
    /// The worked example is the operator's own, cross-checked against an
    /// independent percentage calculator on 2026-09-12: a contract whose lot
    /// is 200 units and which traded 3,200 units in the window reads
    /// **+1500%**, because `(3200 - 200) / 200 = 15`.
    ///
    /// The rival reading — 3,200 as a percentage OF 200 — gives 1600%. The two
    /// differ by exactly 100 for EVERY row, so they rank identically; the
    /// change form is stored because its zero carries meaning that the ratio
    /// form's does not.
    #[test]
    fn net_volume_chg_pct_is_a_change_from_one_lot_not_a_ratio_of_one_lot() {
        // The same arithmetic the view performs, in Rust, so the SQL's
        // semantics are pinned by a value and not only by its own text.
        fn chg_pct(window_lots_milli: i64) -> f64 {
            window_lots_milli as f64 / 10.0 - 100.0
        }
        fn milli(delta_units: i64, lot_size: i64) -> i64 {
            delta_units * 1_000 / lot_size
        }

        // The operator's example.
        assert!((chg_pct(milli(3_200, 200)) - 1_500.0).abs() < f64::EPSILON);

        // Exactly one lot is the ZERO of this scale — the property the ratio
        // form cannot express, where one lot reads as a plausible "100%".
        assert!((chg_pct(milli(200, 200)) - 0.0).abs() < f64::EPSILON);

        // Less than one lot is NEGATIVE, not a reassuring 75%.
        assert!((chg_pct(milli(150, 200)) + 25.0).abs() < f64::EPSILON);

        // The gap to the ratio reading is exactly 100, at every magnitude, so
        // the ordering is a rigid shift and the board is unchanged by it.
        for (delta, lot) in [(3_200_i64, 200_i64), (9_000, 200), (20_000, 225), (1, 15)] {
            let m = milli(delta, lot);
            let ratio_pct = m as f64 / 10.0;
            assert!(
                (ratio_pct - chg_pct(m) - 100.0).abs() < 1e-9,
                "delta {delta} lot {lot}"
            );
        }
    }
    #[test]
    fn test_depth_named_view_ddl_is_single_terminated_statement() {
        let ddl = depth_named_view_ddl();
        assert!(ddl.ends_with(';'), "depth view DDL must end with ';'");
        assert_eq!(
            ddl.matches(';').count(),
            1,
            "depth view DDL must be exactly ONE statement (no injection surface)"
        );
    }

    #[test]
    fn test_depth_view_shows_the_kind_discriminator_next_to_identity() {
        // An analyst who cannot see `depth_kind` at a glance will read a
        // depth-20 row and a depth-200 row as the same observation.
        let ddl = depth_named_view_ddl();
        let kind_at = ddl.find("d.depth_kind").expect("depth_kind projected");
        let price_at = ddl.find("d.price").expect("price projected");
        assert!(
            kind_at < price_at,
            "depth_kind must come before the numbers, not after them: {ddl}"
        );
        for col in ["d.side", "d.level", "d.quantity", "d.orders"] {
            assert!(ddl.contains(col), "{col} missing from the depth view");
        }
    }

    #[test]
    fn test_depth_base_matches_persistence_const() {
        assert_eq!(
            NAMED_VIEW_DEPTH_BASE,
            crate::depth_persistence::MARKET_DEPTH_TABLE,
            "the view's base table drifted from the writer's table"
        );
    }

    #[test]
    fn test_depth_view_name_is_stable() {
        assert_eq!(VIEW_DEPTH_NAMED, "market_depth_named");
    }

    #[test]
    fn test_ticks_named_view_ddl_is_single_terminated_statement() {
        let ddl = ticks_named_view_ddl();
        assert!(ddl.ends_with(';'), "ticks_named DDL must end with ';'");
        assert_eq!(
            ddl.matches(';').count(),
            1,
            "ticks_named DDL must be exactly ONE statement (no injection surface)"
        );
    }

    #[test]
    fn test_candles_named_view_ddl_is_single_terminated_statement() {
        let ddl = candles_named_view_ddl();
        assert!(ddl.ends_with(';'), "candles_named DDL must end with ';'");
        assert_eq!(
            ddl.matches(';').count(),
            1,
            "candles_named DDL must be exactly ONE statement (no injection surface)"
        );
    }

    #[test]
    fn the_ten_minute_view_is_a_single_terminated_statement() {
        let ddl = candles_10m_view_ddl();
        assert!(ddl.ends_with(';'), "candles_10m DDL must end with ';'");
        assert_eq!(
            ddl.matches(';').count(),
            1,
            "candles_10m DDL must be exactly ONE statement (no injection surface)"
        );
        assert!(
            ddl.starts_with("CREATE OR REPLACE VIEW candles_10m AS"),
            "convergent idempotency: a bare CREATE VIEW fails on the second \
             boot, which leaves the view frozen at its first definition"
        );
    }

    /// The gross is summed, the SIGN is derived — and the two must not be
    /// confused, because summing the signed values is the defect this whole
    /// derivation exists to avoid.
    ///
    /// Five `+100` and five `-100` one-minute bars sum to `0`, while the
    /// ten-minute bar they compose is `±1000`. A view that did
    /// `sum(volume)` would therefore report a busy ten minutes as flat — and
    /// it would be wrong quietly, since `0` is a legal reading.
    #[test]
    fn candles_10m_view_ddl_sums_the_magnitude_never_the_signed_value() {
        let ddl = candles_10m_view_ddl();
        assert!(
            ddl.contains("sum(abs(volume)) AS gross"),
            "the gross must come from the MAGNITUDE: {ddl}"
        );
        assert!(
            !ddl.contains("sum(volume)"),
            "summing the signed value cancels opposite minutes to zero: {ddl}"
        );
    }

    /// The window is partitioned by IST DAY, so the first bucket of a
    /// session is never signed against the PREVIOUS session's close.
    ///
    /// The native fold refuses that baseline explicitly
    /// (`aggregator_cell::net_volume_baseline` returns `0.0` when the last
    /// seal belongs to a different IST day), and its docstring names the
    /// reason: an overnight gap reported as intraday direction, on the bar
    /// an operator reads hardest. Without this key the view contradicted
    /// `candles_1m` on the first bucket of every trading day.
    #[test]
    fn the_ten_minute_sign_never_crosses_a_day_boundary() {
        let ddl = candles_10m_view_ddl();
        assert!(
            ddl.contains("date_trunc('day', a.ts) ORDER BY a.ts"),
            "the lag window must be partitioned by IST day, or the first \
             bucket of each session is signed against yesterday: {ddl}"
        );
        // The day key belongs to the PARTITION, never the ORDER BY: ordering
        // by a truncated day would tie every bucket of a session together and
        // make `lag` pick an arbitrary neighbour.
        let partition = ddl
            .find("PARTITION BY a.security_id")
            .expect("the window must partition by instrument");
        let order = ddl
            .find("ORDER BY a.ts")
            .expect("the window must order by ts");
        let day = ddl
            .find("date_trunc('day', a.ts)")
            .expect("the window must carry a day key");
        assert!(
            partition < day && day < order,
            "the day key must sit INSIDE the PARTITION BY list, before the \
             ORDER BY: {ddl}"
        );
    }

    /// The sign is applied ONCE, at the 10m level, against the previous 10m
    /// close — clause 4 of the 2026-09-18 directive compares a bar against the
    /// previous bar OF THE SAME TIMEFRAME.
    #[test]
    fn the_ten_minute_sign_is_derived_at_the_ten_minute_level() {
        let ddl = candles_10m_view_ddl();
        assert!(
            ddl.contains("lag(a.close) OVER"),
            "the baseline is the previous TEN-MINUTE close: {ddl}"
        );
        assert!(
            ddl.contains("PARTITION BY a.security_id, a.segment, a.feed"),
            "the previous bar must be the previous bar of the SAME instrument \
             on the same feed, or one instrument's close signs another's \
             volume: {ddl}"
        );
        assert!(
            ddl.contains("b.prev_close > 0 AND b.close < b.prev_close"),
            "negative only when the close FELL against a real baseline: {ddl}"
        );
        assert!(
            ddl.contains("THEN -b.gross ELSE b.gross END AS volume"),
            "the magnitude is never altered — only its sign: {ddl}"
        );
    }

    /// A bucket with no predecessor signs POSITIVE, never zero and never
    /// NULL.
    ///
    /// `lag()` returns NULL for the first bucket of an instrument, and in SQL
    /// `NULL > 0` is NULL, which is not true — so the `CASE` falls to its
    /// `ELSE` and the bar reports `+gross`. That is the same "no baseline"
    /// default `LiveCandleState::signed_volume` applies, and it is a default
    /// rather than a claim that the close rose.
    #[test]
    fn a_ten_minute_bucket_with_no_predecessor_falls_through_to_positive() {
        let ddl = candles_10m_view_ddl();
        // The guard is a positivity test rather than an IS NOT NULL test, so
        // the NULL first bucket and a zero baseline take the SAME branch.
        assert!(
            ddl.contains("CASE WHEN b.prev_close > 0"),
            "a NULL or non-positive baseline must fall through to ELSE: {ddl}"
        );
        assert!(
            !ddl.contains("IS NOT NULL"),
            "a separate NULL test would leave a zero baseline on a different \
             branch from a missing one, which are the same state: {ddl}"
        );
    }

    /// The view reads the ONE-MINUTE table. It is a derivation, not a frame.
    ///
    /// If a future change ever gives `10m` a `TfIndex` variant and a
    /// `candles_10m` TABLE, this view would shadow it — a view and a table
    /// cannot share a name — so this assertion is also what keeps the
    /// derive-don't-fold decision honest.
    #[test]
    fn the_ten_minute_view_derives_from_the_one_minute_table() {
        let ddl = candles_10m_view_ddl();
        assert!(
            ddl.contains("FROM candles_1m SAMPLE BY 10m"),
            "the source is the 1m frame, sampled: {ddl}"
        );
        assert_eq!(
            NAMED_VIEW_CANDLES_BASE, "candles_1m",
            "the base table name this view derives from"
        );
    }

    #[test]
    fn the_ten_minute_view_name_is_stable() {
        // Wire-format stability: the operator's timeframe list names this
        // surface, and an analyst's saved query references it verbatim.
        assert_eq!(VIEW_CANDLES_10M, "candles_10m");
    }

    #[test]
    fn test_view_name_constants_stable() {
        // Wire-format stability: operators + the runbook reference these
        // names verbatim; renaming is a breaking console-surface change.
        assert_eq!(VIEW_TICKS_NAMED, "ticks_named");
        assert_eq!(VIEW_CANDLES_NAMED, "candles_named");
        assert_eq!(NAMED_VIEW_CANDLES_BASE, "candles_1m");
    }

    #[test]
    fn test_both_ddls_use_create_or_replace_view() {
        // CONVERGENT idempotency (review round 1 fix): bare CREATE VIEW is
        // NOT idempotent on QuestDB ("view already exists" on re-boot), and
        // `CREATE VIEW IF NOT EXISTS` no-ops with 2xx when the definition
        // CHANGED — a future DDL edit would deploy green while every
        // already-provisioned DB silently kept the OLD definition forever
        // (audit Rule 11 false-OK class; the "ready" info! would lie).
        // `CREATE OR REPLACE VIEW` (probe-Verified on the pinned QuestDB
        // 9.3.5: ddl OK + definition actually replaced) converges the
        // deployed definition to the code on EVERY boot — the house
        // every-boot self-heal pattern. Regressing to either weaker form
        // is a stale-definition / boot-error regression class.
        for (name, ddl) in both_ddls() {
            assert!(
                ddl.starts_with("CREATE OR REPLACE VIEW "),
                "{name} DDL must start with CREATE OR REPLACE VIEW: {ddl}"
            );
            assert!(
                !ddl.contains("IF NOT EXISTS"),
                "{name} DDL must not use IF NOT EXISTS (stale-definition false-OK): {ddl}"
            );
        }
    }

    #[test]
    fn test_both_ddls_left_join_on_composite_feed_key() {
        // I-P1-11 composite key + feed-in-key: dropping `feed` from the
        // predicate is the row-multiplication / cross-feed-mislabel
        // regression class; a non-LEFT join would drop unmapped rows
        // (audit Rule 11 false-OK class).
        for (name, ddl) in both_ddls() {
            assert!(ddl.contains("LEFT JOIN"), "{name} DDL must LEFT JOIN");
            assert!(
                ddl.contains("security_id = il.security_id"),
                "{name} DDL join must include security_id"
            );
            assert!(
                ddl.contains("= il.exchange_segment"),
                "{name} DDL join must include exchange_segment"
            );
            assert!(
                ddl.contains("= il.feed"),
                "{name} DDL join must include feed"
            );
            // Every ` JOIN ` occurrence must carry a `LEFT ` prefix.
            let mut search_from = 0;
            while let Some(rel) = ddl[search_from..].find(" JOIN ") {
                let idx = search_from + rel;
                let prefix_start = idx.saturating_sub(4);
                assert_eq!(
                    &ddl[prefix_start..idx],
                    "LEFT",
                    "{name} DDL contains a non-LEFT JOIN at byte {idx}: {ddl}"
                );
                search_from = idx + " JOIN ".len();
            }
        }
    }

    #[test]
    fn test_ddls_filter_dry_run_inside_subquery() {
        // §27 dry-run isolation: the filter must live INSIDE the
        // dimension subquery — an outer WHERE would break LEFT-join
        // semantics (unmapped fact rows would vanish).
        for (name, ddl) in both_ddls() {
            let where_idx = ddl
                .find("WHERE dry_run = false")
                .unwrap_or_else(|| panic!("{name} DDL must filter dry_run = false: {ddl}"));
            let subquery_close_idx = ddl
                .find(") il")
                .unwrap_or_else(|| panic!("{name} DDL must alias the subquery as il: {ddl}"));
            assert!(
                where_idx < subquery_close_idx,
                "{name} DDL dry_run filter must be INSIDE the `( ... ) il` subquery"
            );
        }
    }

    #[test]
    fn test_ddls_never_select_star() {
        // Schema-drift honesty: an explicit column list surfaces a
        // renamed/dropped base column as a loud view error instead of
        // silently changing the view shape. `t.*`-in-view is also
        // un-probed QuestDB territory.
        for (name, ddl) in both_ddls() {
            assert!(!ddl.contains("SELECT *"), "{name} DDL must not SELECT *");
            assert!(
                !ddl.contains(".*"),
                "{name} DDL must not use a .* projection"
            );
        }
    }

    #[test]
    fn test_ddls_select_identity_columns_first_from_correct_bases() {
        // UX pin: identity-first column order (ts, symbol_name,
        // display_name, instrument_type, then numbers, ids/plumbing last)
        // and each view reads its correct base table.
        for (name, ddl) in both_ddls() {
            for col in ["il.symbol_name", "il.display_name", "il.instrument_type"] {
                assert!(ddl.contains(col), "{name} DDL must select {col}: {ddl}");
            }
        }
        let ticks = ticks_named_view_ddl();
        let name_idx = ticks.find("il.symbol_name").unwrap();
        let ltp_idx = ticks.find("t.ltp").unwrap();
        assert!(
            name_idx < ltp_idx,
            "ticks_named must list symbol_name before ltp (identity-first)"
        );
        assert!(
            ticks.contains("FROM ticks t"),
            "ticks_named must read FROM ticks: {ticks}"
        );
        let candles = candles_named_view_ddl();
        let name_idx = candles.find("il.symbol_name").unwrap();
        let open_idx = candles.find("c.open").unwrap();
        assert!(
            name_idx < open_idx,
            "candles_named must list symbol_name before open (identity-first)"
        );
        assert!(
            candles.contains("FROM candles_1m c"),
            "candles_named must read FROM candles_1m: {candles}"
        );
    }

    #[test]
    fn test_duplicate_dim_honest_envelope_ratchet() {
        // Review round 2 (LOW): the original unconditional join-safety
        // claim held ONLY while lifecycle DEDUP was live — duplicate
        // lifecycle rows from the documented HTTP-CLIENT-01 auto-create
        // window / partial feed self-heal N-fold-multiply view output,
        // and DEDUP ENABLE does not retro-collapse them. This ratchet pins (a) the honest-envelope
        // wording + the operator detector query in the runbook, and
        // (b) that neither the runbook, this module, nor the plan record
        // regresses to the unconditional claim.
        //
        // Review round 3 (MEDIUM): QuestDB does not support standard SQL
        // HAVING (questdb.com/docs/concepts/sql-extensions), so the
        // detector is pinned in the documented subquery + WHERE form and
        // the broken HAVING form is banned from the runbook.
        let runbook_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/runbooks/questdb-console-queries.md"
        );
        let runbook = std::fs::read_to_string(runbook_path)
            .unwrap_or_else(|e| panic!("runbook must exist at {runbook_path}: {e}"));
        // Detector query is copy-paste present, in QuestDB's subquery form.
        for fragment in [
            "GROUP BY security_id, exchange_segment, feed",
            ") WHERE n > 1",
            "count() AS n",
            "FROM instrument_lifecycle",
        ] {
            assert!(
                runbook.contains(fragment),
                "runbook must carry the duplicate-dim detector fragment {fragment:?}"
            );
        }
        // The HAVING form errors verbatim on QuestDB — the copy-paste
        // detector must never regress to it (mentioning the keyword in
        // prose to explain the limitation is fine; the executable
        // `HAVING count()` fragment is what is banned).
        assert!(
            !runbook.contains("HAVING count()"),
            "runbook regressed to the QuestDB-unsupported HAVING detector form"
        );
        // Honest envelope named; unconditional claim banned. The banned
        // phrase is built dynamically so this test's own source never
        // matches the scan.
        let banned = format!("{} {}", "never", "multiply");
        let normalize = |s: &str| {
            s.to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let runbook_norm = normalize(&runbook);
        assert!(
            runbook_norm.contains("while dedup is live"),
            "runbook must state the WHILE-DEDUP-is-live honest envelope"
        );
        assert!(
            !runbook_norm.contains(&banned),
            "runbook regressed to the unconditional '{banned}' claim"
        );
        let module_norm = normalize(include_str!("console_views.rs"));
        assert!(
            !module_norm.contains(&banned),
            "console_views.rs regressed to the unconditional '{banned}' claim"
        );
        // Review round 3: the operator-approved plan record (active now,
        // archived after merge per plan-enforcement.md) is a PR-facing
        // surface too — it must carry the conditional envelope and never
        // regress to the unconditional claim.
        let plans_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.claude/plans");
        let mut plan_paths: Vec<std::path::PathBuf> = Vec::new();
        let active = std::path::Path::new(plans_dir).join("active-plan-questdb-named-views.md");
        if active.is_file() {
            plan_paths.push(active);
        }
        if let Ok(entries) = std::fs::read_dir(std::path::Path::new(plans_dir).join("archive")) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .contains("questdb-named-views")
                {
                    plan_paths.push(entry.path());
                }
            }
        }
        assert!(
            !plan_paths.is_empty(),
            "the questdb-named-views plan must exist (active or archived) so its wording stays scanned"
        );
        for path in plan_paths {
            let plan = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("plan {} unreadable: {e}", path.display()));
            let plan_norm = normalize(&plan);
            assert!(
                !plan_norm.contains(&banned),
                "plan {} regressed to the unconditional '{banned}' claim",
                path.display()
            );
            assert!(
                plan_norm.contains("while dedup is live"),
                "plan {} must state the WHILE-DEDUP-is-live honest envelope",
                path.display()
            );
        }
    }

    /// Pins the hardcoded lifecycle-table mirror against the persistence
    /// module's canonical constant (unconditional since PR-C3, 2026-07-14 —
    /// the `daily_universe_fetcher` feature was deleted).
    #[test]
    fn test_lifecycle_dim_matches_persistence_const() {
        assert_eq!(
            NAMED_VIEW_LIFECYCLE_DIM,
            crate::instrument_lifecycle_persistence::QUESTDB_TABLE_INSTRUMENT_LIFECYCLE,
            "NAMED_VIEW_LIFECYCLE_DIM drifted from the canonical lifecycle table name"
        );
    }

    /// Column names a `CREATE TABLE ... ( ... ) timestamp(ts)` statement
    /// declares, read from the DDL string itself.
    fn declared_columns(create_ddl: &str) -> Vec<String> {
        let open = create_ddl.find('(').expect("DDL has a column list");
        // Case-insensitive: `ticks` writes `TIMESTAMP(ts)`, the rest lowercase.
        let close = create_ddl
            .to_ascii_lowercase()
            .find(") timestamp(ts)")
            .expect("DDL declares a designated timestamp");
        create_ddl[open + 1..close]
            .split(',')
            .filter_map(|c| c.split_whitespace().next().map(str::to_string))
            .collect()
    }

    /// Every `alias.column` a view DDL references, excluding `alias.*`.
    ///
    /// The alias must start a token: `il.display_name` never yields a `d.`
    /// reference, because the character before the `d` is not a boundary.
    fn alias_references(ddl: &str, alias: &str) -> Vec<String> {
        let needle = format!("{alias}.");
        let bytes = ddl.as_bytes();
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(pos) = ddl[from..].find(&needle) {
            let at = from + pos;
            from = at + needle.len();
            let boundary = at == 0 || {
                let prev = bytes[at - 1];
                !(prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'.')
            };
            if !boundary {
                continue;
            }
            let ident: String = ddl[from..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !ident.is_empty() {
                out.push(ident);
            }
        }
        out
    }

    /// Every column a view SELECTs or JOINs on must exist in the table it
    /// reads — checked against the table's own DDL, never a hand-kept list.
    ///
    /// The defect this pins (found 2026-09-22): the `top_volume` table lost
    /// three delay columns in the 2026-09-19 schema reset while the four
    /// per-cadence views still selected them, so on a fresh volume every one
    /// of those `CREATE OR REPLACE VIEW` statements refused and the operator's
    /// top-volume faces did not exist. `run_view_ddl` counts a refusal and
    /// warns, so nothing failed loudly. A view is a SQL string resolved at
    /// query time, so only a test that reads both sides can catch this.
    #[test]
    fn every_column_a_view_selects_exists_in_its_base_table() {
        let mut candle_cols: Vec<String> = crate::shadow_persistence::CANDLE_SELF_HEAL_COLUMNS
            .iter()
            .map(|(c, _)| (*c).to_string())
            .collect();
        candle_cols.push("ts".to_string());
        let lifecycle_dim: Vec<String> = [
            "security_id",
            "exchange_segment",
            "feed",
            "symbol_name",
            "display_name",
            "instrument_type",
        ]
        .iter()
        .map(|c| (*c).to_string())
        .collect();
        let dim_sql = lifecycle_dim_subquery();
        for col in &lifecycle_dim {
            assert!(dim_sql.contains(col.as_str()), "dim subquery lost `{col}`");
        }

        let mut checked = 0usize;
        let mut check = |view: &str, ddl: &str, alias: &str, cols: &[String]| {
            let refs = alias_references(ddl, alias);
            assert!(!refs.is_empty(), "{view}: found no `{alias}.` references");
            for r in refs {
                assert!(
                    cols.contains(&r),
                    "{view} selects `{alias}.{r}`, which its base table does not declare — \
                     the CREATE VIEW refuses on a fresh volume"
                );
                checked += 1;
            }
        };

        let ticks = declared_columns(&crate::tick_persistence::ticks_create_ddl());
        let depth = declared_columns(&crate::depth_persistence::market_depth_create_ddl());

        let t = ticks_named_view_ddl();
        check("ticks_named", &t, "t", &ticks);
        check("ticks_named", &t, "il", &lifecycle_dim);
        let c = candles_named_view_ddl();
        check("candles_named", &c, "c", &candle_cols);
        check("candles_named", &c, "il", &lifecycle_dim);
        let d = depth_named_view_ddl();
        check("market_depth_named", &d, "d", &depth);
        check("market_depth_named", &d, "il", &lifecycle_dim);
        // candles_10m reads bare names from candles_1m inside its innermost
        // SELECT; those are checked by name.
        let ten = candles_10m_view_ddl();
        for col in [
            "open",
            "high",
            "low",
            "close",
            "volume",
            "oi",
            "tick_count",
            "security_id",
            "segment",
            "feed",
            "ts",
        ] {
            assert!(ten.contains(col), "candles_10m no longer reads `{col}`");
            assert!(
                candle_cols.iter().any(|c| c == col),
                "candles_10m reads `{col}` from candles_1m, which no longer declares it"
            );
        }
        assert!(
            checked > 40,
            "the reference scan read suspiciously little: {checked}"
        );
    }

    /// The scanner must CATCH a dropped column — proven on the exact shape
    /// that shipped (`t.open_latency` against the 15-column table).
    #[test]
    fn the_view_column_scan_catches_a_column_the_table_dropped() {
        let top = declared_columns(&crate::top_volume_rank_persistence::top_volume_create_ddl(
            SnapshotCadence::OneSecond,
        ));
        let stale = "SELECT t.ts, t.open_latency FROM top_volume t";
        let refs = alias_references(stale, "t");
        assert_eq!(refs, vec!["ts".to_string(), "open_latency".to_string()]);
        assert!(!top.iter().any(|c| c == "open_latency"));
        assert!(alias_references("SELECT il.display_name", "d").is_empty());
    }

    /// Since 2026-09-22 the console-view pass creates NO `top_volume_*` object.
    /// The four faces are direct tables owned by the persistence module; a
    /// `CREATE VIEW top_volume_1s` re-added here would run BEFORE the table
    /// ensure on the boot path and occupy the name the table needs.
    #[test]
    fn ensure_named_views_creates_no_top_volume_object() {
        let src = include_str!("console_views.rs");
        let prod = src
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("prod half");
        let start = prod
            .find("pub async fn ensure_named_views")
            .expect("fn present");
        let body = &prod[start..];
        let end = body.find("\n}\n").expect("fn end");
        let body = &body[..end];
        assert!(
            !body.contains("\"CREATE"),
            "ensure_named_views builds a CREATE literal inline"
        );
        for c in SnapshotCadence::ALL {
            assert!(
                !body.contains(c.table_name()),
                "ensure_named_views names the direct table {}",
                c.table_name()
            );
        }
        assert!(
            body.contains("LEGACY_TOP_VOLUME_VIEWS"),
            "legacy sweep removed"
        );
    }
}
