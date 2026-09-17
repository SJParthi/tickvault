//! AUTO-GENERATED command/SQL goldens for the operator-control port —
//! produced by RUNNING the legacy oracle
//! (`deploy/aws/lambda/operator-control/handler.py`) via the wave-4 build
//! script, NEVER hand-transcribed. Byte-exact with `_VIEW_COMMANDS`,
//! `_LATENCY_COMMANDS`, `_STORAGE_COMMANDS`, `_FEEDS_VIEW_COMMANDS` and
//! `_rest_latency_sql()`.
//!
//! The `=` in `upsertKey=true` MUST stay URL-encoded as %3D in the
//! DEDUP_KEYS view command — it is the ONLY view query carrying a raw `=`
//! inside the ?query= value, which the QuestDB /exp query-string parser
//! mis-handled, returning empty so the dashboard dedup panel showed "?".
//! The encoded form yields a clean count of the 5 upsert-key columns — the
//! REAL `ticks` DEDUP key is (ts, security_id, segment, capture_seq, feed)
//! per DEDUP_KEY_TICKS in crates/storage/src/tick_persistence.rs
//! (the designated ts is always an upsertKey column).
//!
//! RE-POINTED 2026-09-17 from `rest_spot_1m` (4 columns), whose writer went
//! with the per-minute REST legs — `no-rest-except-live-feed-2026-06-27.md`
//! §12.12. The COUNT moves with the table: keeping the 4 would have shown a
//! red "DEDUP disabled / schema drift" shield on a healthy box, every day.
//! This shield has now moved twice and the direction reversed — 2026-07-16
//! took it OFF `ticks` when the live feed was retired; today takes it back.
//!
//! REST-era scope (2026-07-16 cleanup): every live-feed WebSocket is deleted
//! (Dhan 2026-07-13, Groww 2026-07-15), so the old per-feed WS TCP/TLS probes
//! to api-feed.dhan.co / socket-api.groww.in and the lag-percentile grid over
//! `ticks` are all retired — those hosts appear in THIS comment only, never in
//! any command below (the retired-hosts absence scan pins that).

/// legacy: `_VIEW_COMMANDS` (handler.py:332-355).
///
/// **2026-09-03 — the `rest_option_contract_1m` pair is REMOVED (13 → 11).**
/// That table does not exist and cannot: its only DDL entry point,
/// `ensure_option_contract_1m_rest_table`, has had ZERO callers since the
/// 2026-08-21 Groww removal deleted `groww_contract_1m_boot` (the per-contract
/// 1m leg was a Groww-only leg — `no-rest-except-live-feed-2026-06-27.md` §9,
/// retired that day).
///
/// MEASURED on the box 2026-09-03: `SELECT count() FROM
/// rest_option_contract_1m` returns `table does not exist`, and QuestDB logged
/// **61 errors in 30 minutes** naming it — the two lines below, about once a
/// minute, for a leg that no longer has a writer.
///
/// The log noise is the smaller half. `-f` makes curl swallow the 400, so the
/// field arrived EMPTY and the Data tab rendered a blank "contracts" bar —
/// which reads as *"the per-contract leg captured nothing today"* when the
/// truth is *"there is no per-contract leg"*. An operator cannot tell a broken
/// leg from a removed one, and that is the dead-monitor class the 2026-08-21
/// removal directive's own REJECT list forbids.
///
/// Re-adding these needs the per-contract leg back FIRST (a writer and a DDL
/// caller, under its own dated authorization) — never the query alone.
pub const VIEW_COMMANDS: [&str; 10] = [
    r"set +e",
    r#"echo "APP=$(systemctl is-active tickvault 2>/dev/null || echo inactive)""#,
    r"Q='http://127.0.0.1:9000/exp?query='",
    r#"echo "TICKS_TODAY=$(curl -fsS "${Q}SELECT%20count()%20FROM%20ticks%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)""#,
    r#"echo "DEPTH_TODAY=$(curl -fsS "${Q}SELECT%20count()%20FROM%20market_depth%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)""#,
    r#"echo "TICKS_BY_FEED=$(curl -fsS "${Q}SELECT%20feed%2C%20count()%20FROM%20ticks%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20feed" 2>/dev/null | tail -n +2 | tr '\n' ';')""#,
    r#"echo "DEDUP_KEYS=$(curl -fsS "${Q}SELECT%20count()%20FROM%20table_columns(%27ticks%27)%20WHERE%20upsertKey%3Dtrue" 2>/dev/null | tail -1)""#,
    r#"echo "ERRORS_BEGIN""#,
    r"journalctl -u tickvault -p err -n 5 --no-pager 2>/dev/null | tail -5 || true",
    r#"echo "ERRORS_END""#,
];

/// legacy: `_LATENCY_COMMANDS = _latency_commands()` (handler.py:461-483).
pub const LATENCY_COMMANDS: [&str; 7] = [
    r"set +e",
    r#"echo "METRICS_BEGIN""#,
    r"curl -fsS --max-time 3 http://127.0.0.1:9091/metrics 2>/dev/null | grep -E '^tv_order_placement_duration_ns' || echo none",
    r#"echo "METRICS_END""#,
    r#"echo "QDB=$(curl -o /dev/null -s -w '%{time_total}' --max-time 3 'http://127.0.0.1:9000/exec?query=SELECT%201' 2>/dev/null)""#,
    r#"echo "SKEW=$(chronyc tracking 2>/dev/null | awk '/Last offset/{print $4}')""#,
    // 2026-09-17: the `RESTLAT_ROW=` curl is REMOVED (8 -> 7).
    //
    // It aggregated p50/p99 of `close_to_data_ms` from `rest_fetch_audit` --
    // the NINTH read of a retired table in this file, and the one neither
    // `no-rest-except-live-feed-2026-06-27.md` §12.11's disposition table nor
    // §12.12's own eight-row correction named, because its query is embedded
    // in a shell string rather than behind a `const` a reader would grep. The
    // standalone `REST_LATENCY_SQL` twin below WAS found; this one was not.
    //
    // REMOVED rather than re-pointed: `close_to_data_ms` measures how many
    // seconds after a minute CLOSED its data arrived, and there is no
    // per-minute fetch left to time. The live lane's
    // `tv_dhan_feed_last_tick_age_secs` measures ARRIVAL freshness -- a
    // different question, and presenting it as an answer to this one is the
    // false-OK §12.12 names. Left in place it returned an empty result for
    // `today()` every day, forever, on the tab an operator opens to ask
    // whether the box is slow.
    //
    // Every BOX-WIDE probe on this tab is KEPT: QuestDB round-trip, clock
    // skew, the order-placement histogram, and the per-socket live lag below.
    // LIVE WebSocket delivery lag, per socket.
    //
    // Read from /metrics, NOT from QuestDB: no table carries live-socket lag,
    // and the histogram is deliberately not EMF-shipped (a Prometheus histogram
    // is exposed as _bucket/_sum/_count, which the anchored EMF selector cannot
    // match — see cloudwatch_app_alarms_wiring.rs). Port 9091 matches the
    // order-placement curl three lines above; the metrics endpoint binds
    // 127.0.0.1, so this only works from ON the box, which is where SSM
    // RunCommand puts us.
    //
    // The raw lines are shipped and the percentiles computed in Rust rather
    // than in awk: bucket arithmetic that silently produces a plausible wrong
    // number is exactly the failure this panel exists to end.
    //
    // 2026-09-08: the per-connection gauges ride the same line — instruments
    // held on the wire, data-bearing frames, and seconds since the last one —
    // so the console can show all sixteen sockets, not only the ones whose
    // lag histogram happened to have samples.
    r"curl -fsS --max-time 3 http://127.0.0.1:9091/metrics 2>/dev/null | grep -E '^tv_dhan_ws_(lag_ms_(bucket|count|sum)|conn_[a-z_]+)' | sed 's/^/WSLAT_RAW=/' || true",
];

/// legacy: `_STORAGE_COMMANDS` (handler.py:591-595).
pub const STORAGE_COMMANDS: [&str; 3] = [
    r"set +e",
    r#"df -BG / | tail -1 | awk '{print "DISK_USED="$3"\nDISK_FREE="$4"\nDISK_PCT="$5}'"#,
    r#"echo "DB_SIZE=$(du -sBG /var/lib/docker/volumes/tv-questdb-data/_data 2>/dev/null | cut -f1)""#,
];

/// legacy: `_FEEDS_VIEW_COMMANDS` (handler.py:641-683).
pub const FEEDS_VIEW_COMMANDS: [&str; 7] = [
    r"set +e",
    r#"echo "FEEDS_BEGIN""#,
    r"curl -fsS --max-time 8 http://127.0.0.1:3001/api/feeds 2>/dev/null || echo TV_CURL_FAILED; echo",
    r#"echo "FEEDS_END""#,
    r#"echo "FEEDS_HEALTH_BEGIN""#,
    r"curl -fsS --max-time 8 http://127.0.0.1:3001/api/feeds/health 2>/dev/null || echo TV_CURL_FAILED; echo",
    r#"echo "FEEDS_HEALTH_END""#,
    // 2026-09-17: the `REST_AUDIT` and `REST_LAT_HOUR` lines are REMOVED.
    // Both queried `rest_fetch_audit`, whose writer went with the per-minute
    // REST legs under the operator's SOCKETS-ONLY narrowing
    // (`no-rest-except-live-feed-2026-06-27.md` §12.10; the §12.11
    // disposition table names both REMOVE). The table is RETAINED and holds
    // real history, so unlike the 2026-09-03 pair above these did NOT error —
    // they returned an empty result for `today()`, every day, forever. That is
    // the WORSE shape: a silent empty field an operator cannot tell from a
    // broken pull, which is exactly the class the docstring above records.
];

// ---- `REST_LATENCY_SQL` is RETIRED 2026-09-17 ----
//
// The console's "REST pull latency" canned query: per (feed, leg) ok-row
// count plus p50/p99 of `close_to_data_ms` from `rest_fetch_audit` for
// `today()`. Its source has no writer after the SOCKETS-ONLY narrowing
// (§12.10; §12.11 names it REMOVE), so it returned an empty result set every
// day — a canned query that can only ever answer "nothing" teaches an
// operator that the button is broken.
//
// The QUESTION it answered — *how late was each minute's data?* — has no
// surviving equivalent, because there is no per-minute fetch left to time.
// Recorded here rather than silently re-pointed: the live lane measures
// ARRIVAL freshness (`tv_dhan_feed_last_tick_age_secs`), which is a different
// question and must not be presented as an answer to this one.
