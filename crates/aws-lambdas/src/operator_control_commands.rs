//! AUTO-GENERATED command/SQL goldens for the operator-control port —
//! produced by RUNNING the python oracle
//! (`deploy/aws/lambda/operator-control/handler.py`) via the wave-4 build
//! script, NEVER hand-transcribed. Byte-exact with `_VIEW_COMMANDS`,
//! `_LATENCY_COMMANDS`, `_STORAGE_COMMANDS`, `_FEEDS_VIEW_COMMANDS` and
//! `_rest_latency_sql()`.
//!
//! The `=` in `upsertKey=true` MUST stay URL-encoded as %3D in the
//! DEDUP_KEYS view command — it is the ONLY view query carrying a raw `=`
//! inside the ?query= value, which the QuestDB /exp query-string parser
//! mis-handled, returning empty so the dashboard dedup panel showed "?".
//! The encoded form yields a clean count of the 4 upsert-key columns — the
//! REAL `spot_1m_rest` DEDUP key is (ts, security_id, exchange_segment, feed)
//! per DEDUP_KEY_SPOT_1M_REST in crates/storage/src/spot_1m_rest_persistence.rs
//! (the designated ts is always an upsertKey column).
//!
//! REST-era scope (2026-07-16 cleanup): every live-feed WebSocket is deleted
//! (Dhan 2026-07-13, Groww 2026-07-15), so the old per-feed WS TCP/TLS probes
//! to api-feed.dhan.co / socket-api.groww.in and the lag-percentile grid over
//! `ticks` are all retired — those hosts appear in THIS comment only, never in
//! any command below (the retired-hosts absence scan pins that).

/// python: `_VIEW_COMMANDS` (handler.py:332-355).
pub const VIEW_COMMANDS: [&str; 13] = [
    r"set +e",
    r#"echo "APP=$(systemctl is-active tickvault 2>/dev/null || echo inactive)""#,
    r"Q='http://127.0.0.1:9000/exp?query='",
    r#"echo "SPOT_TODAY=$(curl -fsS "${Q}SELECT%20count()%20FROM%20spot_1m_rest%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)""#,
    r#"echo "CHAIN_TODAY=$(curl -fsS "${Q}SELECT%20count()%20FROM%20option_chain_1m%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)""#,
    r#"echo "CONTRACT_TODAY=$(curl -fsS "${Q}SELECT%20count()%20FROM%20option_contract_1m_rest%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)""#,
    r#"echo "SPOT_BY_FEED=$(curl -fsS "${Q}SELECT%20feed%2C%20count()%20FROM%20spot_1m_rest%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20feed" 2>/dev/null | tail -n +2 | tr '\n' ';')""#,
    r#"echo "CHAIN_BY_FEED=$(curl -fsS "${Q}SELECT%20feed%2C%20count()%20FROM%20option_chain_1m%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20feed" 2>/dev/null | tail -n +2 | tr '\n' ';')""#,
    r#"echo "CONTRACT_BY_FEED=$(curl -fsS "${Q}SELECT%20feed%2C%20count()%20FROM%20option_contract_1m_rest%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20feed" 2>/dev/null | tail -n +2 | tr '\n' ';')""#,
    r#"echo "DEDUP_KEYS=$(curl -fsS "${Q}SELECT%20count()%20FROM%20table_columns(%27spot_1m_rest%27)%20WHERE%20upsertKey%3Dtrue" 2>/dev/null | tail -1)""#,
    r#"echo "ERRORS_BEGIN""#,
    r"journalctl -u tickvault -p err -n 5 --no-pager 2>/dev/null | tail -5 || true",
    r#"echo "ERRORS_END""#,
];

/// python: `_LATENCY_COMMANDS = _latency_commands()` (handler.py:461-483).
pub const LATENCY_COMMANDS: [&str; 7] = [
    r"set +e",
    r#"echo "METRICS_BEGIN""#,
    r"curl -fsS --max-time 3 http://127.0.0.1:9091/metrics 2>/dev/null | grep -E '^tv_order_placement_duration_ns' || echo none",
    r#"echo "METRICS_END""#,
    r#"echo "QDB=$(curl -o /dev/null -s -w '%{time_total}' --max-time 3 'http://127.0.0.1:9000/exec?query=SELECT%201' 2>/dev/null)""#,
    r#"echo "SKEW=$(chronyc tracking 2>/dev/null | awk '/Last offset/{print $4}')""#,
    r#"curl -fsS --max-time 4 -G 'http://127.0.0.1:9000/exp' --data-urlencode "query=select feed, leg, count() ok_rows, approx_percentile(close_to_data_ms, 0.5, 3) p50, approx_percentile(close_to_data_ms, 0.99, 3) p99 from rest_fetch_audit where ts in today() and outcome = 'ok' and close_to_data_ms >= 0 group by feed, leg order by feed, leg" 2>/dev/null | tail -n +2 | sed 's/^/RESTLAT_ROW=/'"#,
];

/// python: `_STORAGE_COMMANDS` (handler.py:591-595).
pub const STORAGE_COMMANDS: [&str; 3] = [
    r"set +e",
    r#"df -BG / | tail -1 | awk '{print "DISK_USED="$3"\nDISK_FREE="$4"\nDISK_PCT="$5}'"#,
    r#"echo "DB_SIZE=$(du -sBG /var/lib/docker/volumes/tv-questdb-data/_data 2>/dev/null | cut -f1)""#,
];

/// python: `_FEEDS_VIEW_COMMANDS` (handler.py:641-683).
pub const FEEDS_VIEW_COMMANDS: [&str; 9] = [
    r"set +e",
    r#"echo "FEEDS_BEGIN""#,
    r"curl -fsS --max-time 8 http://127.0.0.1:3001/api/feeds 2>/dev/null || echo TV_CURL_FAILED; echo",
    r#"echo "FEEDS_END""#,
    r#"echo "FEEDS_HEALTH_BEGIN""#,
    r"curl -fsS --max-time 8 http://127.0.0.1:3001/api/feeds/health 2>/dev/null || echo TV_CURL_FAILED; echo",
    r#"echo "FEEDS_HEALTH_END""#,
    r#"echo "REST_AUDIT=$(curl -fsS --max-time 4 -G 'http://127.0.0.1:9000/exp' --data-urlencode "query=select feed, outcome, count() from rest_fetch_audit where ts in today() group by feed, outcome" 2>/dev/null | tail -n +2 | tr '\n' ';')""#,
    r#"echo "REST_LAT_HOUR=$(curl -fsS --max-time 4 -G 'http://127.0.0.1:9000/exp' --data-urlencode "query=select feed, approx_percentile(close_to_data_ms, 0.5, 3), approx_percentile(close_to_data_ms, 0.99, 3) from rest_fetch_audit where ts > dateadd('h', -1, dateadd('m', 330, now())) and outcome = 'ok' and close_to_data_ms >= 0 group by feed" 2>/dev/null | tail -n +2 | tr '\n' ';')""#,
];

/// python: `_rest_latency_sql()` (handler.py:444-458) — static return value.
pub const REST_LATENCY_SQL: &str = r"select feed, leg, count() ok_rows, approx_percentile(close_to_data_ms, 0.5, 3) p50, approx_percentile(close_to_data_ms, 0.99, 3) p99 from rest_fetch_audit where ts in today() and outcome = 'ok' and close_to_data_ms >= 0 group by feed, leg order by feed, leg";
