//! `option_chain_1m` table — per-minute option-chain REST pipeline
//! (operator grant 2026-07-12, PR-3 the OPTION-CHAIN half; runbook
//! `.claude/rules/project/rest-1m-pipeline-error-codes.md`).
//!
//! ONE row per `(minute, underlying, expiry, strike, leg)` fetched by the
//! per-minute Dhan `POST /v2/optionchain` fetcher
//! (`crates/app/src/option_chain_1m_boot.rs`) for the 3 IDX_I underlyings
//! (NIFTY 13 / BANKNIFTY 25 / SENSEX 51), current expiry only. The
//! designated `ts` is the MINUTE-OPEN IST stamp of the minute the snapshot
//! belongs to (the minute that just closed when the fetch fired — same
//! IST-as-epoch convention as `spot_1m_rest.ts` / `candles_1m.ts`), so a
//! re-fetch of the same minute UPSERTs in place (DEDUP idempotency by
//! construction).
//!
//! ## Honesty columns
//! - `underlying_spot` — the chain response's own `data.last_price`
//!   (Dhan's view of the underlying at snapshot time), forensic.
//! - `fetched_at` — the retrieval wall-clock instant (IST), forensic.
//! - `source` — `'rest_optionchain'` label (NOT in the DEDUP key).
//! - `contract_security_id` — Dhan's per-contract SecurityId from the
//!   response (0 when absent) — usable for future subscriptions without an
//!   instrument-master lookup (option-chain.md rule 8); NOT in the key.
//!
//! ## Disk envelope (flagged operator follow-up — retention)
//! ~150 strikes × 2 legs × 3 underlyings ≈ ~900 rows/minute worst case
//! (typically less — one-sided deep-OTM strikes are skipped), ~2–3K/min at
//! wide chains. At ~200 B/row that is roughly ~70 MB/trading-day and
//! ~6–8 GB per 90-day hot window — an order of magnitude above every other
//! DAY-partitioned table. The table IS registered with the partition
//! manager (DAY partitions age to S3 like the rest); whether the hot
//! window should be SHORTER for this table is a flagged operator
//! follow-up per the plan.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS option_chain_1m (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP,
//!     underlying_security_id LONG, exchange_segment SYMBOL, feed SYMBOL,
//!     source SYMBOL, underlying_symbol SYMBOL, expiry TIMESTAMP,
//!     strike DOUBLE, leg SYMBOL, contract_security_id LONG,
//!     last_price DOUBLE, iv DOUBLE, delta DOUBLE, theta DOUBLE,
//!     gamma DOUBLE, vega DOUBLE, oi LONG, volume LONG, previous_oi LONG,
//!     underlying_spot DOUBLE, fetched_at TIMESTAMP,
//!     rho DOUBLE, close_to_data_ms LONG, moneyness SYMBOL,
//!     contract SYMBOL
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, underlying_security_id, exchange_segment,
//!                     expiry, strike, leg, feed);
//! ```

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::moneyness::MoneynessStepLabel;

/// QuestDB table name — one row per fetched `(minute, underlying, expiry,
/// strike, leg)`.
pub const OPTION_CHAIN_1M_TABLE: &str = "option_chain_1m";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule);
/// `exchange_segment` alongside the underlying id (I-P1-11); `feed` in-key
/// (operator override 2026-06-28 — feed-in-key EVERYWHERE); `expiry` +
/// `strike` + `leg` complete the per-contract identity. A re-fetch of the
/// same minute UPSERTs in place, never duplicates. `strike` is a DOUBLE
/// key column — safe here because every strike value comes from the SAME
/// deterministic decimal-string parse (`"25650.000000"` → f64), so a
/// re-fetch produces a bit-identical key.
///
/// **PARSE-ONLY INVARIANT (hostile-review L3, ratcheted):** the float key
/// is safe ONLY while strikes are NEVER computed arithmetically. Any
/// future writer that DERIVES strikes (e.g. `ATM ± k×step`) would produce
/// bit-different f64s for the same logical strike and silently mint
/// duplicate rows — such a change MUST first move this key to a paise
/// LONG (`strike_paise`, with `strike DOUBLE` kept as a display column).
/// Build-failing ratchet:
/// `crates/app/tests/option_chain_1m_wiring_guard.rs::ratchet_chain1m_strike_is_parse_only_never_computed`.
pub const DEDUP_KEY_OPTION_CHAIN_1M: &str =
    "ts, underlying_security_id, exchange_segment, expiry, strike, leg, feed";

/// `feed` SYMBOL value — the source broker is Dhan (REST leg).
pub const OPTION_CHAIN_1M_FEED_DHAN: &str = "dhan";

/// `feed` SYMBOL value — the source broker is Groww (the 2026-07-13 PR-3
/// Groww chain leg; feed-in-key DEDUP keeps the two feeds' rows distinct).
pub const OPTION_CHAIN_1M_FEED_GROWW: &str = "groww";

/// `source` SYMBOL label — provenance beyond `feed` (label, NOT in-key).
///
/// SLUG CONVENTION (the ONE consistent choice — hostile-round-1 NIT-7):
/// every REST-leg `source` slug is `rest_` + the VENDOR ENDPOINT path
/// snake_cased, exactly the Groww-SPOT-leg convention —
/// Dhan `/v2/charts/intraday` → `rest_intraday`, Groww `/v1/candles` →
/// `rest_candles`, Dhan `/v2/optionchain` → `rest_optionchain`, Groww
/// `/v1/option-chain` → `rest_option_chain`. The `feed` column (in the
/// DEDUP key) is what distinguishes vendors; `source` records WHICH
/// endpoint produced the row. The two chain slugs differ by one
/// underscore because the vendors' endpoint paths do — query the chain
/// class with `source LIKE 'rest_option%chain'` (or just filter `feed`).
pub const OPTION_CHAIN_1M_SOURCE: &str = "rest_optionchain";

/// `source` SYMBOL label for the Groww chain leg — endpoint-derived per
/// the convention above (Groww `GET /v1/option-chain/...`).
pub const OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN: &str = "rest_option_chain";

/// `exchange_segment` SYMBOL value — the 3 underlyings are all IDX_I.
pub const OPTION_CHAIN_1M_SEGMENT_IDX_I: &str = "IDX_I";

/// `leg` SYMBOL values — call / put.
pub const OPTION_CHAIN_1M_LEG_CE: &str = "CE";
/// `leg` SYMBOL value for the put side.
pub const OPTION_CHAIN_1M_LEG_PE: &str = "PE";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// One fetched option-chain leg row, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct OptionChain1mRow {
    /// Designated timestamp — the snapshot's MINUTE-OPEN, IST nanoseconds
    /// (the minute that closed when the fetch fired).
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Dhan IDX_I SecurityId of the UNDERLYING (13 / 25 / 51).
    pub underlying_security_id: i64,
    /// Human underlying symbol (`NIFTY` / `BANKNIFTY` / `SENSEX`).
    pub underlying_symbol: &'static str,
    /// The chain's expiry date — IST midnight nanoseconds of the expiry
    /// day (the house TIMESTAMP convention for expiry columns, per
    /// `instrument_lifecycle.expiry_date`).
    pub expiry_ist_nanos: i64,
    /// Strike price (parsed from Dhan's decimal-string key).
    pub strike: f64,
    /// `"CE"` or `"PE"` (one of the `OPTION_CHAIN_1M_LEG_*` constants).
    pub leg: &'static str,
    /// Dhan SecurityId of THIS option contract (0 when absent).
    pub contract_security_id: i64,
    pub last_price: f64,
    /// Implied volatility of this strike.
    pub iv: f64,
    pub delta: f64,
    pub theta: f64,
    pub gamma: f64,
    pub vega: f64,
    /// Current open interest.
    pub oi: i64,
    /// Today's traded volume.
    pub volume: i64,
    /// Previous day's open interest.
    pub previous_oi: i64,
    /// The response's own `data.last_price` for the underlying.
    pub underlying_spot: f64,
    /// Retrieval wall-clock instant, IST nanoseconds.
    pub fetched_at_ist_nanos: i64,
    /// Write-time COMBINED moneyness label (operator ruling 2026-07-20 —
    /// the strike-step count lives IN the label itself: `"ITM-1"`,
    /// `"ITM-2"`, …, `"ATM"`, `"OTM+1"`, …; bare `"ITM"`/`"OTM"` when the
    /// step is unresolvable; `"UNKNOWN"` for unclassifiable rows —
    /// `tickvault_common::moneyness::moneyness_step_label` is the ONLY
    /// producer). AUDIT MIRROR ONLY: the RAM chain snapshot is the
    /// decision source of truth (operator directive 2026-07-14); this
    /// column exists so `WHERE moneyness='ATM'` /
    /// `WHERE moneyness='ITM-1'` are precomputed filters, never a
    /// query-time recompute. NOT in the DEDUP key (label column — the
    /// `contract_security_id` precedent). The retired 2026-07-17
    /// `moneyness_depth` DOUBLE column is DROPPED (operator ruling
    /// 2026-07-20 — see the marker-gated migration in the ensure fn).
    pub moneyness: MoneynessStepLabel,
}

/// The idempotent `CREATE TABLE` DDL for `option_chain_1m`. Pure.
#[must_use]
pub fn option_chain_1m_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {OPTION_CHAIN_1M_TABLE} (\
            ts                     TIMESTAMP, \
            trading_date_ist       TIMESTAMP, \
            underlying_security_id LONG, \
            exchange_segment       SYMBOL, \
            feed                   SYMBOL, \
            source                 SYMBOL, \
            underlying_symbol      SYMBOL, \
            expiry                 TIMESTAMP, \
            strike                 DOUBLE, \
            leg                    SYMBOL, \
            contract_security_id   LONG, \
            last_price             DOUBLE, \
            iv                     DOUBLE, \
            delta                  DOUBLE, \
            theta                  DOUBLE, \
            gamma                  DOUBLE, \
            vega                   DOUBLE, \
            oi                     LONG, \
            volume                 LONG, \
            previous_oi            LONG, \
            underlying_spot        DOUBLE, \
            fetched_at             TIMESTAMP, \
            rho                    DOUBLE, \
            close_to_data_ms       LONG, \
            moneyness              SYMBOL, \
            contract               SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_OPTION_CHAIN_1M});"
    )
}

/// Every non-designated column as `(name, type)` — the single source for
/// both the CREATE DDL sanity tests and the per-column `ALTER ADD COLUMN
/// IF NOT EXISTS` self-heal below. The 2026-07-13 Groww-leg additions
/// (`rho`, `close_to_data_ms`) ride the SAME self-heal: a live table
/// created by an earlier build gains them via `ALTER ADD COLUMN IF NOT
/// EXISTS` at the next boot (nullable — the Dhan rows simply leave them
/// NULL until the Dhan leg ever stamps them). Pure.
#[must_use]
pub fn option_chain_1m_columns() -> [(&'static str, &'static str); 25] {
    [
        ("trading_date_ist", "TIMESTAMP"),
        ("underlying_security_id", "LONG"),
        ("exchange_segment", "SYMBOL"),
        ("feed", "SYMBOL"),
        ("source", "SYMBOL"),
        ("underlying_symbol", "SYMBOL"),
        ("expiry", "TIMESTAMP"),
        ("strike", "DOUBLE"),
        ("leg", "SYMBOL"),
        ("contract_security_id", "LONG"),
        ("last_price", "DOUBLE"),
        ("iv", "DOUBLE"),
        ("delta", "DOUBLE"),
        ("theta", "DOUBLE"),
        ("gamma", "DOUBLE"),
        ("vega", "DOUBLE"),
        ("oi", "LONG"),
        ("volume", "LONG"),
        ("previous_oi", "LONG"),
        ("underlying_spot", "DOUBLE"),
        ("fetched_at", "TIMESTAMP"),
        // Groww-leg additions (2026-07-13, PR-3 — additive, benefit both
        // feeds; grounding: `docs/groww-ref/14-option-chain.md` §2 response
        // schema (per-leg greeks carry `rho`; verified against BruteX's
        // production use of the same account) + the spot-leg
        // `close_to_data_ms` honesty-column precedent (the chain response
        // carries NO timestamp — Verified-absence, §3 — so the measured
        // close→data delay is the ONLY freshness signal).
        // 2026-07-17 correction: the "Dhan rows leave both NULL" era ended
        // — the Dhan leg now stamps `close_to_data_ms` (the already-
        // measured fetch latency is persisted per row); `rho` remains NULL
        // on Dhan FOREVER — the Dhan option-chain response carries NO rho
        // (greeks are delta/theta/gamma/vega only —
        // `docs/dhan-ref/06-option-chain.md` /
        // `.claude/rules/dhan/option-chain.md` rule 10; grep-verified
        // against the Dhan leg's response parser).
        ("rho", "DOUBLE"),
        ("close_to_data_ms", "LONG"),
        // Moneyness classification (2026-07-14, operator directive relayed
        // via the coordinator session — moneyness is CRITICAL and MANDATORY,
        // computed at WRITE time; the RAM chain snapshot is the decision
        // surface, this column is the write-only AUDIT MIRROR so
        // `WHERE moneyness='ATM'` is a precomputed filter, never a
        // query-time recompute). Values since the 2026-07-20 combined-
        // label law: "ITM-1"…, "ATM", "OTM+1"…, bare "ITM"/"OTM"
        // (off-grid fallback), "UNKNOWN" (~21 nominal distinct values
        // under the ATM±10 plan — SYMBOL-friendly cardinality; rows
        // written before 2026-07-20 keep the bare 4-value labels).
        // Additive + nullable; rows written by earlier builds stay
        // NULL FOREVER (never backfilled). NOT in the DEDUP key (label
        // column — the contract_security_id precedent).
        ("moneyness", "SYMBOL"),
        // Human-readable contract label (operator addition 2026-07-20 —
        // "NIFTY 28 JUL 25000 CALL", the Dhan-web style), derived at
        // WRITE time from fields already on the row
        // (`contract_label`) — zero extra fetch, cold write path.
        // Additive + nullable; rows written by earlier builds stay NULL
        // FOREVER (self-heal ADD, never backfilled). NOT in the DEDUP
        // key (display label). The retired `moneyness_depth` DOUBLE
        // (2026-07-17 → 2026-07-20) is deliberately ABSENT here — it is
        // DROPPED from live tables by the marker-gated migration below.
        ("contract", "SYMBOL"),
    ]
}

/// Uppercase 3-letter month names for [`contract_label`] (1-indexed via
/// `month0`).
const CONTRACT_LABEL_MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

/// Human-readable contract label, Dhan-web style (operator addition
/// 2026-07-20, verbatim: *"one extra column to mention everything like
/// this — for example NIFTY 28 JUL 25000 CALL"*):
/// `<UNDERLYING> <D> <MMM> <STRIKE> <CALL|PUT>` — day WITHOUT a leading
/// zero, month uppercase 3-letter, strike as an integer when whole
/// (`25000`, never `25000.0`; a genuinely fractional strike renders its
/// shortest decimal form), `CALL`/`PUT` words (never CE/PE — an unknown
/// leg falls back to the raw label, honest). Derived at WRITE time from
/// fields already on the row — zero extra fetch, cold ILP write path
/// (allocation is fine here; this is not the tick hot path).
///
/// `expiry_ist_nanos` follows the house IST-as-epoch convention (the
/// row's own `expiry` column value); an out-of-range timestamp renders
/// the honest `?` day/month sentinel instead of a fabricated date.
#[must_use]
pub fn contract_label(
    underlying_symbol: &str,
    expiry_ist_nanos: i64,
    strike: f64,
    leg: &str,
) -> String {
    use chrono::Datelike;
    let side = match leg {
        OPTION_CHAIN_1M_LEG_CE => "CALL",
        OPTION_CHAIN_1M_LEG_PE => "PUT",
        other => other,
    };
    let date = chrono::DateTime::from_timestamp(expiry_ist_nanos.div_euclid(1_000_000_000), 0)
        .map(|dt| dt.date_naive());
    let (day, month) = match date {
        Some(d) => (
            d.day(),
            CONTRACT_LABEL_MONTHS
                .get(d.month0() as usize)
                .copied()
                .unwrap_or("?"),
        ),
        None => (0, "?"),
    };
    // Whole strikes render as integers (his example: 25000, not 25000.0);
    // NOTE: display-only formatting — the strike VALUE is never computed
    // or rounded (parse-only invariant; `{:.0}` is applied only when
    // `fract() == 0`, i.e. losslessly).
    if strike.is_finite() && strike.fract() == 0.0 {
        format!("{underlying_symbol} {day} {month} {strike:.0} {side}")
    } else {
        format!("{underlying_symbol} {day} {month} {strike} {side}")
    }
}

/// One-shot marker file for the 2026-07-20 `moneyness_depth` column DROP
/// (operator ruling: *"what is this moneyness_depth column, just remove
/// it"*). Mirrors the `index_constituency` ts-pin marker convention
/// (`data/state/…`).
pub const OPTION_CHAIN_1M_DEPTH_DROP_MARKER_PATH: &str =
    "data/state/option-chain-1m-moneyness-depth-dropped.marker";

/// PURE marker-gate predicate: should the one-shot depth-column DROP run?
/// `true` when the marker file is ABSENT.
#[must_use]
pub fn option_chain_1m_depth_drop_should_run(marker_path: &std::path::Path) -> bool {
    !marker_path.exists()
}

/// One-shot, marker-gated `ALTER TABLE option_chain_1m DROP COLUMN
/// moneyness_depth` (operator ruling 2026-07-20 — the 2026-07-17 depth
/// column is REMOVED; its historic rupee/step values are the garbage the
/// ruling ordered gone). Idempotent + fail-safe:
/// * marker PRESENT → skip (one-shot).
/// * DROP succeeds → `info!` + marker written.
/// * DROP rejected because the column does not exist (a fresh table
///   created by the post-2026-07-20 DDL) → ALREADY the migrated state →
///   marker written.
/// * QuestDB down / other non-2xx → `error!` (CHAIN-03,
///   `stage="depth_column_drop"`), marker NOT written — retried next boot.
///   Never touches any other column; never blocks any boot leg.
///
/// A SEPARATE boot fn (the `index_constituency` ts-pin migration
/// convention) — deliberately NOT wired inside
/// [`ensure_option_chain_1m_table`], so the ensure's mock-HTTP unit
/// tests can never trigger a marker write (the boot legs call BOTH,
/// side by side).
// TEST-EXEMPT: live-QuestDB ALTER runner (marker predicate + DDL string unit-tested; transport arms are the ensure-fn error-arm class)
pub async fn migrate_drop_moneyness_depth_column(questdb_config: &QuestDbConfig) {
    let marker_path = std::path::Path::new(OPTION_CHAIN_1M_DEPTH_DROP_MARKER_PATH);
    if !option_chain_1m_depth_drop_should_run(marker_path) {
        return;
    }
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
            metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "depth_column_drop")
                .increment(1);
            error!(
                code = "CHAIN-03",
                stage = "depth_column_drop",
                ?err,
                "CHAIN-03: HTTP client build failed — moneyness_depth DROP \
                 skipped, will retry next boot"
            );
            return;
        }
    };
    let ddl = format!("ALTER TABLE {OPTION_CHAIN_1M_TABLE} DROP COLUMN moneyness_depth;");
    let done = match client
        .get(base_url)
        .query(&[("query", ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(
                table = OPTION_CHAIN_1M_TABLE,
                "one-time 2026-07-20 migration: dropped the retired moneyness_depth column"
            );
            true
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let lower = body.to_ascii_lowercase();
            // A fresh table created by the post-2026-07-20 DDL has no such
            // column — QuestDB rejects the DROP with an invalid/unknown
            // column error; that IS the migrated state.
            if lower.contains("invalid column")
                || lower.contains("column") && lower.contains("does not exist")
            {
                true
            } else {
                metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "depth_column_drop")
                    .increment(1);
                error!(
                    code = "CHAIN-03",
                    stage = "depth_column_drop",
                    %status,
                    body = %body.chars().take(200).collect::<String>(),
                    "CHAIN-03: moneyness_depth DROP COLUMN returned non-2xx — \
                     marker NOT written, will retry next boot"
                );
                false
            }
        }
        Err(err) => {
            metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "depth_column_drop")
                .increment(1);
            error!(
                code = "CHAIN-03",
                stage = "depth_column_drop",
                ?err,
                "CHAIN-03: moneyness_depth DROP COLUMN request failed — \
                 marker NOT written, will retry next boot"
            );
            false
        }
    };
    if !done {
        return;
    }
    // Best-effort marker write; a failed write just repeats the (now
    // no-op) DROP next boot.
    if let Some(parent) = marker_path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        warn!(
            ?err,
            path = %parent.display(),
            "moneyness_depth drop migration: failed to create data/state/ dir for marker"
        );
        return;
    }
    if let Err(err) = std::fs::write(
        marker_path,
        "option_chain_1m moneyness_depth column dropped on this deployment (operator ruling 2026-07-20).\n",
    ) {
        warn!(
            ?err,
            path = OPTION_CHAIN_1M_DEPTH_DROP_MARKER_PATH,
            "moneyness_depth drop migration: failed to write marker (will repeat next boot)"
        );
    }
}

/// Create the `option_chain_1m` table if absent (idempotent schema-self-heal
/// order: CREATE → per-column `ALTER ADD COLUMN IF NOT EXISTS` → DEDUP
/// ENABLE, so a table created by an earlier build auto-migrates; never a
/// table drop — SEBI retention). Greenfield table, writer always stamps
/// `feed` ⇒ no NULL-feed backfill UPDATE.
///
/// Failures log at `error!` (code CHAIN-03) but never block — NOTE the
/// HTTP-CLIENT-01-class consequence: a failed ensure leaves the table to be
/// auto-created by the first ILP write WITHOUT DEDUP UPSERT KEYS — a
/// duplicate-row window until a later ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested via the create_ddl tests; the 200/500/unreachable arms exercised via the mock-HTTP tokio tests below)
pub async fn ensure_option_chain_1m_table(questdb_config: &QuestDbConfig) {
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
            metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = "CHAIN-03",
                stage = "ensure_client_build",
                ?err,
                "CHAIN-03: HTTP client build failed — option_chain_1m table \
                 not ensured (first ILP write may auto-create it WITHOUT \
                 dedup — duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let mut statements = vec![option_chain_1m_create_ddl()];
    // Per-column self-heal for tables created by earlier builds
    // (observability-architecture.md schema-self-heal pattern). QuestDB
    // ignores ADDs that already exist, so running every boot is free.
    for (col, ty) in option_chain_1m_columns() {
        statements.push(format!(
            "ALTER TABLE {OPTION_CHAIN_1M_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {OPTION_CHAIN_1M_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_OPTION_CHAIN_1M});"
    ));
    for ddl in &statements {
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
                metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(code = "CHAIN-03", stage = "ensure_ddl",
                    %status, ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "CHAIN-03: option_chain_1m DDL returned non-2xx (dedup may \
                     be missing — duplicate-row window until a later ensure \
                     succeeds)");
            }
            Err(err) => {
                metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = "CHAIN-03",
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "CHAIN-03: option_chain_1m DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf for the per-minute writer — per-flush server ACK (the
/// 2026-07-05 fire-and-forget lesson) with the shadow-candle-writer knobs:
/// `retry_timeout=0` (the questdb-rs internal 10s sleep-and-resend loop is
/// disabled — the fetch loop owns retry cadence) + `request_timeout=5000` ms
/// (bounds a hung flush).
fn option_chain_1m_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for `option_chain_1m`. Mirrors
/// `Spot1mRestWriter`: unreachable QuestDB at construction still builds
/// (rows buffer locally); `flush` returns `Err` — incl. server-side rejects
/// via the HTTP ACK — and DISCARDS pending on failure (poisoned-buffer
/// defense; rows are re-fetchable + DEDUP-idempotent).
pub struct OptionChain1mWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
    /// The `feed` SYMBOL stamped on every appended row (`'dhan'`/`'groww'`).
    feed: &'static str,
    /// The `source` SYMBOL stamped on every appended row (provenance label).
    source: &'static str,
}

/// The per-feed discarded-rows counter name — the Dhan and Groww chain
/// legs each keep their own series so a Groww discard can never inflate
/// the Dhan signal (the spot-writer precedent). Pure.
#[must_use]
fn chain_1m_discard_counter_for(feed: &str) -> &'static str {
    if feed == OPTION_CHAIN_1M_FEED_GROWW {
        "tv_groww_chain1m_rows_discarded_total"
    } else {
        "tv_chain1m_rows_discarded_total"
    }
}

/// The per-feed written-rows counter name (same per-feed split). Pure.
#[must_use]
fn chain_1m_written_counter_for(feed: &str) -> &'static str {
    if feed == OPTION_CHAIN_1M_FEED_GROWW {
        "tv_groww_chain1m_rows_written_total"
    } else {
        "tv_chain1m_rows_written_total"
    }
}

impl OptionChain1mWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    /// Stamps `feed='dhan'` / `source='rest_optionchain'` (the original
    /// PR-3 behaviour, byte-identical — the Groww leg uses
    /// [`Self::new_with_feed`]).
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via test_chain1m_writer_new_is_lazy...); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        Self::new_with_feed(config, OPTION_CHAIN_1M_FEED_DHAN, OPTION_CHAIN_1M_SOURCE)
    }

    /// Feed-parameterized production constructor (operator grant 2026-07-13
    /// — the Groww chain leg writes the SAME table tagged `feed='groww'`,
    /// source [`OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN`]). Lazy on failure
    /// like [`Self::new`].
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (same lazy-build contract as new(); the feed/source stamping is exercised via for_test_with_feed()).
    pub fn new_with_feed(config: &QuestDbConfig, feed: &'static str, source: &'static str) -> Self {
        let conf = option_chain_1m_ilp_http_conf(config);
        match Sender::from_conf(&conf) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                    feed,
                    source,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    feed, "option_chain_1m writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                    feed,
                    source,
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer (Dhan stamps).
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test() -> Self {
        Self::for_test_with_feed(OPTION_CHAIN_1M_FEED_DHAN, OPTION_CHAIN_1M_SOURCE)
    }

    /// Test constructor with explicit feed/source stamps.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the feed-stamp unit tests below.
    pub fn for_test_with_feed(feed: &'static str, source: &'static str) -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
            feed,
            source,
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

    /// Appends one option-chain leg row with NO extension columns (cold
    /// path, ≤~3K rows/minute, batched: the fetcher appends a whole
    /// minute then flushes once) — `rho` / `close_to_data_ms` stay
    /// UNWRITTEN (NULL in QuestDB). Since 2026-07-17 the production legs
    /// both route through [`Self::append_row_ext`] (the Dhan leg now
    /// stamps `close_to_data_ms`); this stays the plain convenience form.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &OptionChain1mRow) -> Result<()> {
        self.append_row_inner(r, None, None)
    }

    /// Appends one row WITH the independently-optional extension columns
    /// (2026-07-17 refactor of the 2026-07-13 pair — `rho` and
    /// `close_to_data_ms` are now separately `Option`al so each leg
    /// stamps exactly what it has; ILP-sparse — a `None` lands NULL):
    /// - Groww chain leg: `(Some(rho), Some(close_to_data_ms))` — the
    ///   per-leg `rho` greek (`docs/groww-ref/14-option-chain.md` §2) +
    ///   the measured delay (the chain response carries NO timestamp).
    /// - Dhan chain leg: `(None, Some(close_to_data_ms))` — the Dhan
    ///   option-chain response carries NO rho (greeks are
    ///   delta/theta/gamma/vega only — `docs/dhan-ref/06-option-chain.md`
    ///   / `.claude/rules/dhan/option-chain.md` rule 10), so `rho` stays
    ///   NULL on Dhan rows forever; the already-measured fetch latency IS
    ///   persisted per row since 2026-07-17.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row_ext(
        &mut self,
        r: &OptionChain1mRow,
        rho: Option<f64>,
        close_to_data_ms: Option<i64>,
    ) -> Result<()> {
        self.append_row_inner(r, rho, close_to_data_ms)
    }

    /// Shared append body — `rho` / `close_to_data_ms` are the
    /// independently-optional extension columns (ILP rows are sparse:
    /// absent columns land NULL). The `contract` display label is derived
    /// here from fields already on the row ([`contract_label`]).
    fn append_row_inner(
        &mut self,
        r: &OptionChain1mRow,
        rho: Option<f64>,
        close_to_data_ms: Option<i64>,
    ) -> Result<()> {
        let b = self
            .buffer
            .table(OPTION_CHAIN_1M_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("exchange_segment", OPTION_CHAIN_1M_SEGMENT_IDX_I)
            .context("exchange_segment")?
            .symbol("feed", self.feed)
            .context("feed")?
            .symbol("source", self.source)
            .context("source")?
            .symbol("underlying_symbol", r.underlying_symbol)
            .context("underlying_symbol")?
            .symbol("leg", r.leg)
            .context("leg")?
            // Moneyness audit-mirror stamp (2026-07-14; combined step
            // label since 2026-07-20) — a SYMBOL tag, written
            // UNCONDITIONALLY on BOTH feeds (UNKNOWN covers every
            // unclassifiable case, so it is never absent).
            .symbol("moneyness", r.moneyness.as_str())
            .context("moneyness")?
            // Human-readable contract label (2026-07-20 operator
            // addition) — derived from row fields, Dhan-web style.
            .symbol(
                "contract",
                contract_label(r.underlying_symbol, r.expiry_ist_nanos, r.strike, r.leg).as_str(),
            )
            .context("contract")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("underlying_security_id", r.underlying_security_id)
            .context("underlying_security_id")?
            .column_ts("expiry", TimestampNanos::new(r.expiry_ist_nanos))
            .context("expiry")?
            .column_f64("strike", r.strike)
            .context("strike")?
            .column_i64("contract_security_id", r.contract_security_id)
            .context("contract_security_id")?
            .column_f64("last_price", r.last_price)
            .context("last_price")?
            .column_f64("iv", r.iv)
            .context("iv")?
            .column_f64("delta", r.delta)
            .context("delta")?
            .column_f64("theta", r.theta)
            .context("theta")?
            .column_f64("gamma", r.gamma)
            .context("gamma")?
            .column_f64("vega", r.vega)
            .context("vega")?
            .column_i64("oi", r.oi)
            .context("oi")?
            .column_i64("volume", r.volume)
            .context("volume")?
            .column_i64("previous_oi", r.previous_oi)
            .context("previous_oi")?
            .column_f64("underlying_spot", r.underlying_spot)
            .context("underlying_spot")?
            .column_ts("fetched_at", TimestampNanos::new(r.fetched_at_ist_nanos))
            .context("fetched_at")?;
        // Independently-optional columns (ILP-sparse — a None lands NULL).
        let b = if let Some(rho) = rho {
            b.column_f64("rho", rho).context("rho")?
        } else {
            b
        };
        let b = if let Some(ms) = close_to_data_ms {
            b.column_i64("close_to_data_ms", ms)
                .context("close_to_data_ms")?
        } else {
            b
        };
        b.at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK).
    ///
    /// On ANY failed flush the pending buffer is DISCARDED (the 2026-07-06
    /// shadow-writer `discard_pending` precedent): a server-REJECTED row
    /// retained across flushes would be re-sent every minute forever and
    /// block ALL later rows for the session. The rows are re-fetchable and
    /// DEDUP-idempotent, so the durable floor for a discarded minute is a
    /// re-fetch/backfill, and the miss is LOUD (CHAIN-03 error at the call
    /// site + `tv_chain1m_rows_discarded_total` + the minute feeds the
    /// CHAIN-02 failure edge).
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (pending discarded).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        if self.sender.is_none() {
            let dropped = self.discard_pending();
            anyhow::bail!(
                "option_chain_1m: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending row(s) discarded (re-fetchable, DEDUP-idempotent)"
            );
        }
        let flushed = self
            .sender
            .as_mut()
            .map(|sender| sender.flush(&mut self.buffer));
        match flushed {
            Some(Ok(())) => {
                metrics::counter!(chain_1m_written_counter_for(self.feed))
                    .increment(self.pending as u64);
                self.pending = 0;
                Ok(())
            }
            Some(Err(err)) => {
                let dropped = self.discard_pending();
                Err(anyhow::Error::new(err).context(format!(
                    "option_chain_1m ILP flush failed — {dropped} pending row(s) \
                     discarded (poisoned-buffer defense; rows are re-fetchable)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!("option_chain_1m: ILP sender vanished — {dropped} row(s) discarded");
            }
        }
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded row count; counted per feed
    /// (`tv_chain1m_rows_discarded_total` /
    /// `tv_groww_chain1m_rows_discarded_total`) so a discard is never
    /// silent and a Groww discard never inflates the Dhan signal.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!(chain_1m_discard_counter_for(self.feed)).increment(dropped as u64);
        }
        self.buffer.clear();
        self.pending = 0;
        dropped
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::moneyness::{Moneyness, StepIndexOutcome, moneyness_step_label};

    fn sample_row() -> OptionChain1mRow {
        OptionChain1mRow {
            // 2026-07-10 09:15:00 IST-as-epoch minute-open (illustrative).
            ts_ist_nanos: 1_770_000_900_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            underlying_security_id: 13,
            underlying_symbol: "NIFTY",
            // 2026-07-16 IST midnight (illustrative weekly expiry).
            expiry_ist_nanos: 1_770_508_800_000_000_000,
            strike: 25_650.0,
            leg: OPTION_CHAIN_1M_LEG_CE,
            contract_security_id: 42_528,
            last_price: 134.0,
            iv: 9.789,
            delta: 0.538_71,
            theta: -15.153_9,
            gamma: 0.001_32,
            vega: 12.185_93,
            oi: 3_786_445,
            volume: 117_567_970,
            previous_oi: 402_220,
            underlying_spot: 25_642.8,
            fetched_at_ist_nanos: 1_770_000_961_042_000_000,
            // 25650 CE @ spot 25642.8, Rs.50 grid → the grid-rounded ATM
            // (the anchor strike indexes 0 → the bare "ATM" label under
            // the 2026-07-20 combined-label law).
            moneyness: moneyness_step_label(Moneyness::Atm, StepIndexOutcome::Aligned(0)),
        }
    }

    #[test]
    fn test_option_chain_1m_create_ddl_contains_expected_columns() {
        let ddl = option_chain_1m_create_ddl();
        assert!(ddl.contains("ts "), "designated ts missing: {ddl}");
        for (col, ty) in option_chain_1m_columns() {
            assert!(
                ddl.contains(&format!("{col} ")),
                "DDL missing column {col:?}: {ddl}"
            );
            assert!(!ty.is_empty());
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_OPTION_CHAIN_1M})")));
    }

    /// DEDUP-key discipline: designated `ts` FIRST (2026-04-28 regression
    /// rule); `exchange_segment` with the underlying id (I-P1-11); `feed`
    /// in-key (operator override 2026-06-28); per-contract identity via
    /// `expiry` + `strike` + `leg` — whole-token matches, exactly 7 keys.
    #[test]
    fn test_option_chain_1m_dedup_key_shape() {
        assert!(DEDUP_KEY_OPTION_CHAIN_1M.trim_start().starts_with("ts,"));
        let has_token = |t: &str| {
            DEDUP_KEY_OPTION_CHAIN_1M
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        for key in [
            "underlying_security_id",
            "exchange_segment",
            "expiry",
            "strike",
            "leg",
            "feed",
        ] {
            assert!(has_token(key), "DEDUP key missing {key:?}");
        }
        // Exactly (ts, underlying_security_id, exchange_segment, expiry,
        // strike, leg, feed).
        assert_eq!(DEDUP_KEY_OPTION_CHAIN_1M.matches(',').count() + 1, 7);
        // The per-contract SecurityId is a LABEL, never a key (derivative
        // ids are unstable across relists — instrument-master.md rule 3).
        assert!(!has_token("contract_security_id"));
        // The retired moneyness_depth (2026-07-17 → dropped 2026-07-20)
        // must never re-enter the key; the human-readable contract label
        // (2026-07-20) is a display column, never a key.
        assert!(!has_token("moneyness_depth"));
        assert!(!has_token("contract"));
    }

    #[test]
    fn test_option_chain_1m_symbol_labels_stable() {
        assert_eq!(OPTION_CHAIN_1M_TABLE, "option_chain_1m");
        assert_eq!(OPTION_CHAIN_1M_FEED_DHAN, "dhan");
        assert_eq!(OPTION_CHAIN_1M_FEED_GROWW, "groww");
        assert_eq!(OPTION_CHAIN_1M_SOURCE, "rest_optionchain");
        assert_eq!(OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN, "rest_option_chain");
        assert_ne!(
            OPTION_CHAIN_1M_SOURCE, OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN,
            "provenance labels must stay distinguishable beyond feed"
        );
        assert_eq!(OPTION_CHAIN_1M_SEGMENT_IDX_I, "IDX_I");
        assert_eq!(OPTION_CHAIN_1M_LEG_CE, "CE");
        assert_eq!(OPTION_CHAIN_1M_LEG_PE, "PE");
    }

    /// Per-feed counter routing (the spot-writer precedent): a Groww
    /// discard/write can never inflate the Dhan series.
    #[test]
    fn test_chain1m_per_feed_counter_names() {
        assert_eq!(
            chain_1m_discard_counter_for(OPTION_CHAIN_1M_FEED_DHAN),
            "tv_chain1m_rows_discarded_total"
        );
        assert_eq!(
            chain_1m_discard_counter_for(OPTION_CHAIN_1M_FEED_GROWW),
            "tv_groww_chain1m_rows_discarded_total"
        );
        assert_eq!(
            chain_1m_written_counter_for(OPTION_CHAIN_1M_FEED_DHAN),
            "tv_chain1m_rows_written_total"
        );
        assert_eq!(
            chain_1m_written_counter_for(OPTION_CHAIN_1M_FEED_GROWW),
            "tv_groww_chain1m_rows_written_total"
        );
    }

    /// The Groww-parameterized writer stamps `feed=groww` + its own source
    /// label, and `append_row_ext` writes both extension columns
    /// (`rho` + `close_to_data_ms`). Since 2026-07-17 the two are
    /// independently optional: a Dhan-style `(None, Some(ms))` call DOES
    /// stamp `close_to_data_ms` and does NOT stamp `rho` (the Dhan
    /// option-chain response carries no rho — delta/theta/gamma/vega
    /// only); the plain `append_row` leaves both UNWRITTEN.
    #[test]
    fn test_chain1m_append_row_ext_groww_stamps_feed_rho_and_latency() {
        let mut w = OptionChain1mWriter::for_test_with_feed(
            OPTION_CHAIN_1M_FEED_GROWW,
            OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN,
        );
        w.append_row_ext(&sample_row(), Some(5.1802), Some(1_042))
            .expect("ext append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.contains(",feed=groww"), "feed tag missing: {line}");
        assert!(
            line.contains(",source=rest_option_chain"),
            "groww source tag missing: {line}"
        );
        assert!(line.contains("rho=5.1802"), "rho missing: {line}");
        assert!(
            line.contains("close_to_data_ms=1042i"),
            "close_to_data_ms missing: {line}"
        );

        // The Dhan-style ext call (2026-07-17): close_to_data_ms IS
        // stamped, rho is NOT (no rho exists in the Dhan response).
        let mut dhan_ext = OptionChain1mWriter::for_test();
        dhan_ext
            .append_row_ext(&sample_row(), None, Some(2_314))
            .expect("ext append must succeed");
        let dhan_ext_line = dhan_ext.buffer_utf8();
        assert!(dhan_ext_line.contains(",feed=dhan"), "got: {dhan_ext_line}");
        assert!(
            dhan_ext_line.contains("close_to_data_ms=2314i"),
            "Dhan ext call must stamp close_to_data_ms: {dhan_ext_line}"
        );
        assert!(
            !dhan_ext_line.contains("rho="),
            "Dhan must never stamp rho (absent from the response): {dhan_ext_line}"
        );

        // The plain append_row writes NEITHER extension column.
        let mut dhan = OptionChain1mWriter::for_test();
        dhan.append_row(&sample_row()).expect("append must succeed");
        let dhan_line = dhan.buffer_utf8();
        assert!(dhan_line.contains(",feed=dhan"), "got: {dhan_line}");
        assert!(
            dhan_line.contains(",source=rest_optionchain"),
            "got: {dhan_line}"
        );
        assert!(
            !dhan_line.contains("rho="),
            "plain append_row must not stamp rho: {dhan_line}"
        );
        assert!(
            !dhan_line.contains("close_to_data_ms="),
            "plain append_row must not stamp close_to_data_ms: {dhan_line}"
        );
    }

    /// The 2026-07-20 combined-label law + column removal: the step
    /// count lives IN the `moneyness` SYMBOL (`ITM-1`/`OTM+1`…), the
    /// retired `moneyness_depth` column is NEVER written, absent from the
    /// manifest (so the self-heal can never re-add it), and the DROP
    /// migration machinery exists (marker predicate + path). The NEW
    /// `contract` display label lands as a SYMBOL tag.
    #[test]
    fn test_chain1m_combined_label_no_depth_column_and_contract_tag() {
        // A one-step-ITM CE row carries the combined label.
        let mut itm_row = sample_row();
        itm_row.strike = 25_600.0;
        itm_row.moneyness = moneyness_step_label(Moneyness::Itm, StepIndexOutcome::Aligned(-1));
        let mut w = OptionChain1mWriter::for_test();
        w.append_row(&itm_row).expect("append must succeed");
        let line = w.buffer_utf8();
        let tags = line.split(' ').next().unwrap_or_default();
        assert!(
            tags.contains(",moneyness=ITM-1"),
            "combined ITM-1 label must land as an ILP TAG: {line}"
        );
        // The retired depth column is NEVER written — any path.
        assert!(
            !line.contains("moneyness_depth"),
            "moneyness_depth must never be written (dropped 2026-07-20): {line}"
        );
        let mut ext = OptionChain1mWriter::for_test_with_feed(
            OPTION_CHAIN_1M_FEED_GROWW,
            OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN,
        );
        ext.append_row_ext(&itm_row, Some(5.1802), Some(1_042))
            .expect("ext append must succeed");
        assert!(
            !ext.buffer_utf8().contains("moneyness_depth"),
            "moneyness_depth must never be written on the ext path: {}",
            ext.buffer_utf8()
        );
        // UNKNOWN rows keep the bare label.
        let mut unknown_row = sample_row();
        unknown_row.moneyness = moneyness_step_label(Moneyness::Unknown, StepIndexOutcome::Invalid);
        let mut w2 = OptionChain1mWriter::for_test();
        w2.append_row_ext(&unknown_row, None, Some(1_042))
            .expect("append must succeed");
        assert!(
            w2.buffer_utf8().contains(",moneyness=UNKNOWN"),
            "UNKNOWN rows keep the bare label: {}",
            w2.buffer_utf8()
        );
        // Manifest: no depth column (the self-heal must never re-add it);
        // the contract display column IS present as SYMBOL.
        assert!(
            !option_chain_1m_columns()
                .iter()
                .any(|&(col, _)| col == "moneyness_depth"),
            "moneyness_depth must be ABSENT from the manifest (dropped 2026-07-20)"
        );
        assert!(
            option_chain_1m_columns()
                .iter()
                .any(|&(col, ty)| col == "contract" && ty == "SYMBOL"),
            "contract must be a SYMBOL column in the manifest"
        );
        assert!(
            !option_chain_1m_create_ddl().contains("moneyness_depth"),
            "the CREATE DDL must not carry the dropped column"
        );
        // The contract tag lands (spaces ILP-escaped inside the tag block).
        assert!(
            line.contains(",contract=NIFTY\\ "),
            "contract label tag missing: {line}"
        );
        // Migration machinery: pure marker predicate + stable paths.
        let missing = std::path::Path::new("data/state/definitely-absent-marker-for-test");
        assert!(option_chain_1m_depth_drop_should_run(missing));
        assert!(OPTION_CHAIN_1M_DEPTH_DROP_MARKER_PATH.starts_with("data/state/"));
    }

    /// The 2026-07-20 depth-drop migration marker gate: runs when the
    /// marker is absent, skips when present (one-shot per deployment).
    #[test]
    fn test_option_chain_1m_depth_drop_should_run_marker_gate() {
        let dir =
            std::env::temp_dir().join(format!("tv-depth-drop-marker-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let marker = dir.join("marker");
        assert!(
            option_chain_1m_depth_drop_should_run(&marker),
            "absent marker => migration runs"
        );
        std::fs::write(&marker, "done").expect("write marker");
        assert!(
            !option_chain_1m_depth_drop_should_run(&marker),
            "present marker => one-shot skip"
        );
        std::fs::remove_file(&marker).ok();
        std::fs::remove_dir(&dir).ok();
        // The production marker path lives under data/state/ (the house
        // migration-marker convention).
        assert!(OPTION_CHAIN_1M_DEPTH_DROP_MARKER_PATH.starts_with("data/state/"));
    }

    /// The 2026-07-20 contract-label format law, pinned with exact
    /// literals (the operator's own example first): day WITHOUT a leading
    /// zero, month uppercase 3-letter, whole strikes as integers,
    /// CALL/PUT words.
    #[test]
    fn test_contract_label_format_law() {
        // 2026-07-28 IST midnight (IST-as-epoch) = 1_785_196_800 s.
        let jul28 = 1_785_196_800_000_000_000_i64;
        assert_eq!(
            contract_label("NIFTY", jul28, 25_000.0, OPTION_CHAIN_1M_LEG_CE),
            "NIFTY 28 JUL 25000 CALL"
        );
        assert_eq!(
            contract_label("BANKNIFTY", jul28, 59_000.0, OPTION_CHAIN_1M_LEG_CE),
            "BANKNIFTY 28 JUL 59000 CALL"
        );
        // PUT side + a single-digit day (2026-08-04 = jul28 + 7d) —
        // no leading zero ("4 AUG", never "04 AUG").
        let aug4 = jul28 + 7 * 86_400 * 1_000_000_000;
        assert_eq!(
            contract_label("SENSEX", aug4, 81_000.0, OPTION_CHAIN_1M_LEG_PE),
            "SENSEX 4 AUG 81000 PUT"
        );
        // A genuinely fractional strike renders its decimal form (no NSE
        // index grid has one today — defensive, never rounded).
        assert_eq!(
            contract_label("NIFTY", jul28, 25_050.5, OPTION_CHAIN_1M_LEG_PE),
            "NIFTY 28 JUL 25050.5 PUT"
        );
        // Unknown leg falls back to the raw label (honest, never guessed).
        assert_eq!(
            contract_label("NIFTY", jul28, 25_000.0, "XX"),
            "NIFTY 28 JUL 25000 XX"
        );
        // Extreme timestamps stay total (chrono covers the whole
        // i64-nanos range once divided to seconds, so the `?` sentinel
        // arm is pure defense): i64::MAX nanos = 2262-04-11.
        assert_eq!(
            contract_label("NIFTY", i64::MAX, 25_000.0, OPTION_CHAIN_1M_LEG_CE),
            "NIFTY 11 APR 25000 CALL"
        );
        // Negative nanos (pre-epoch) floor-divide correctly.
        assert_eq!(
            contract_label(
                "NIFTY",
                -86_400_000_000_000,
                25_000.0,
                OPTION_CHAIN_1M_LEG_CE
            ),
            "NIFTY 31 DEC 25000 CALL"
        );
    }

    /// The 2026-07-14 moneyness audit-mirror stamp: a SYMBOL tag (ILP
    /// tags-before-fields — it must sit in the tag section, before the
    /// first space), written on BOTH feeds' append paths.
    #[test]
    fn test_chain1m_append_row_stamps_moneyness_symbol() {
        // Dhan path (plain append_row).
        let mut dhan = OptionChain1mWriter::for_test();
        dhan.append_row(&sample_row()).expect("append must succeed");
        let dhan_line = dhan.buffer_utf8();
        let dhan_tags = dhan_line.split(' ').next().unwrap_or_default();
        assert!(
            dhan_tags.contains(",moneyness=ATM"),
            "moneyness must be an ILP TAG (before the first field) on the \
             Dhan line: {dhan_line}"
        );

        // Groww path (append_row_ext) stamps the SAME tag.
        let mut groww = OptionChain1mWriter::for_test_with_feed(
            OPTION_CHAIN_1M_FEED_GROWW,
            OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN,
        );
        groww
            .append_row_ext(&sample_row(), Some(5.1802), Some(1_042))
            .expect("ext append must succeed");
        let groww_line = groww.buffer_utf8();
        let groww_tags = groww_line.split(' ').next().unwrap_or_default();
        assert!(
            groww_tags.contains(",moneyness=ATM"),
            "moneyness must be an ILP TAG on the Groww line: {groww_line}"
        );

        // The DDL manifest carries the column as SYMBOL (self-heal covers
        // live tables created by earlier builds).
        assert!(
            option_chain_1m_columns()
                .iter()
                .any(|&(col, ty)| col == "moneyness" && ty == "SYMBOL"),
            "moneyness must be a SYMBOL column in the manifest"
        );
        // NEVER in the DEDUP key (label column).
        assert!(
            !DEDUP_KEY_OPTION_CHAIN_1M
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == "moneyness"),
            "moneyness must never enter the DEDUP key"
        );
    }

    #[test]
    fn test_chain1m_append_row_writes_symbols_and_columns() {
        let mut w = OptionChain1mWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(OPTION_CHAIN_1M_TABLE));
        assert!(
            line.contains(",exchange_segment=IDX_I"),
            "segment tag missing: {line}"
        );
        assert!(line.contains(",feed=dhan"), "feed tag missing: {line}");
        assert!(
            line.contains(",source=rest_optionchain"),
            "source tag missing: {line}"
        );
        assert!(
            line.contains(",underlying_symbol=NIFTY"),
            "underlying tag missing: {line}"
        );
        assert!(line.contains(",leg=CE"), "leg tag missing: {line}");
        assert!(
            line.contains("underlying_security_id=13i"),
            "underlying sid missing: {line}"
        );
        assert!(
            line.contains("contract_security_id=42528i"),
            "contract sid missing: {line}"
        );
        assert!(line.contains("strike=25650"), "strike missing: {line}");
        assert!(line.contains("oi=3786445i"), "oi missing: {line}");
        assert!(
            line.contains("previous_oi=402220i"),
            "previous_oi missing: {line}"
        );
        assert!(line.contains("volume=117567970i"), "volume missing: {line}");
    }

    #[test]
    fn test_chain1m_flush_when_disconnected_errors_and_discards_pending() {
        // Poisoned-buffer defense (the spot-writer M2 precedent): a failed
        // flush DISCARDS the pending buffer — one rejected row can never
        // wedge the rest of the session; rows are re-fetchable +
        // DEDUP-idempotent.
        let mut w = OptionChain1mWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert!(err.to_string().contains("discarded"));
        assert_eq!(w.pending(), 0, "failed flush discards pending");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
        // Empty flush is a no-op Ok.
        let mut empty = OptionChain1mWriter::for_test();
        assert!(empty.flush().is_ok());
    }

    /// `discard_pending` clears BOTH the row count and the ILP buffer so
    /// the next fire starts from a clean slate (shadow-writer precedent).
    #[test]
    fn test_chain1m_discard_pending_clears_buffer_and_count() {
        let mut w = OptionChain1mWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        w.append_row(&sample_row()).expect("append must succeed");
        assert_eq!(w.pending(), 2);
        assert!(!w.buffer_utf8().is_empty());
        assert_eq!(w.discard_pending(), 2, "returns the discarded count");
        assert_eq!(w.pending(), 0);
        assert!(w.buffer_utf8().is_empty());
        // Idempotent: a second discard drops nothing.
        assert_eq!(w.discard_pending(), 0);
    }

    /// Transport ratchet (2026-07-05 fire-and-forget lesson): ILP-over-HTTP
    /// with the shadow-candle-writer knobs — never ILP TCP 9009.
    #[test]
    fn test_chain1m_writer_uses_ilp_http_conf_with_bounded_knobs() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = option_chain_1m_ilp_http_conf(&cfg);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(!conf.contains("9009"), "must not target ILP TCP: {conf}");
    }

    // ========================================================================
    // Persistence-helper tests — mock QuestDB /exec HTTP server + unreachable
    // host (the spot_1m_rest_persistence pattern). These exercise the real
    // ensure/constructor code paths: success (200), non-2xx (500) and
    // transport-error arms.
    // ========================================================================

    const MOCK_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const MOCK_HTTP_500: &str =
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 13\r\n\r\n{\"error\":\"x\"}";

    async fn spawn_mock_http(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    fn mock_cfg(http_port: u16) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    fn unreachable_cfg() -> QuestDbConfig {
        // Port 1 is reserved and never listening; guarantees a real HTTP
        // transport failure without touching any live service.
        mock_cfg(1)
    }

    #[tokio::test]
    async fn test_ensure_option_chain_1m_table_mock_200_completes() {
        // Success path: the CREATE + every ADD COLUMN self-heal + the DEDUP
        // ENABLE all take the Ok(2xx) arm.
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_option_chain_1m_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_option_chain_1m_table_mock_500_degrades_without_panic() {
        // Non-2xx path: every DDL statement takes the log-and-continue arm
        // (best-effort degrade — CHAIN-03, never a panic, never blocks).
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_option_chain_1m_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_option_chain_1m_table_unreachable_degrades_without_panic() {
        // Transport-error path: every DDL send Err arm logs and continues.
        ensure_option_chain_1m_table(&unreachable_cfg()).await;
    }

    #[tokio::test]
    async fn test_chain1m_writer_new_is_lazy_and_buffers_without_network() {
        // `Sender::from_conf` with `http::` does not dial at construction
        // (the ws_event_audit precedent), so new() against an unreachable
        // host still builds a sender-backed writer whose appends land in
        // the local buffer — the lazy-construction contract.
        let mut w = OptionChain1mWriter::new(&unreachable_cfg());
        assert_eq!(w.pending(), 0, "fresh writer has nothing pending");
        w.append_row(&sample_row())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1, "row buffered locally");
    }
}
