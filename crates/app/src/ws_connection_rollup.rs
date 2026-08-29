//! Per-CONNECTION daily WebSocket rollup — the fold that fills
//! `ws_connection_daily`.
//!
//! ## What this answers, and why the existing tables did not
//!
//! Operator directive 2026-08-29 (typos preserved): *"see how will you always
//! montiro in a day si there a wesbcoekt dsiconnect or websocket reocnenct
//! happened for all the conecntons dude ... based on evry day we ened to
//! cpature this rpeicsley rigth dude"*, under the standing *"always ensure to
//! achieve O(1) always"*.
//!
//! The capture was never the gap. `ws_event_audit` has carried one row per
//! lifecycle event per connection since 2026-08-20, and `feed_episode_audit`
//! carries the classified episodes with blame — both keyed on
//! `(feed, ws_type, connection_index)`. What did not exist was a DAILY
//! VERDICT at that grain: `feed_scoreboard_daily` sums all sixteen
//! connections into one row per feed, so "did connection 7 drop today?" could
//! only be answered by scanning a day of events and grouping them by hand.
//!
//! This module folds both sources into one row per connection per day, so
//! that question becomes a single keyed row read.
//!
//! ## Complexity
//!
//! O(events) once per day over a bounded day of rows, O(1) per event, and
//! O(connections) rows written — sixteen at the authorized socket budget.
//! Nothing here runs on the tick hot path; it is a post-close cold-path job.
//! The QUERY it enables is the O(1) part: one row per connection per day,
//! reached by its DEDUP key.
//!
//! ## Why both sources, and not just the episodes
//!
//! A connection that ran all day without incident produces ZERO episode rows.
//! Folding episodes alone would therefore emit no row for exactly the
//! connections that were healthy — and an absent row is indistinguishable
//! from a connection that never started, or from the rollup not having run.
//! That is the absent-series false-OK this repository was bitten by on
//! 2026-08-28 (`tv_depth_rows_spilled_total` published no series at all while
//! its sibling counted 104,540 drops, and the absence read as a healthy zero).
//!
//! So the connection SET comes from `ws_event_audit` — every connection that
//! produced any lifecycle event, healthy or not — and the classified incident
//! counts come from `feed_episode_audit`. A connection present in the episode
//! table but absent from the event table still gets a row, with
//! `saw_any_event = false`, because that combination is itself a finding.
//!
//! ## ⚠ What this CANNOT answer — a socket that never came up at all
//!
//! The connection set is OBSERVED, not AUTHORIZED. Both sources are things
//! that HAPPENED: a lifecycle event, or a classified episode. A connection
//! the lane planned and then failed to dial produces NEITHER, because there
//! is no `WsEventKind` for a failed dial — the seven kinds are connected /
//! disconnected / disconnected_off_hours / reconnected / sleep_entered /
//! sleep_resumed / stall_restarted, and every one presupposes a socket the
//! supervisor had something to act on.
//!
//! ⚠ CORRECTED 2026-08-29, hours after this section was written, and the
//! error is worth keeping rather than quietly editing away: the first
//! version of this paragraph enumerated SIX kinds and omitted
//! `StallRestarted`. A section written specifically to stop a claim rotting
//! shipped a false enumeration on its first day — and this very file already
//! names the seventh kind further down, so it contradicted its own module.
//! That is the `day_ohlc_tracker` class exactly: the rot is not in the old
//! text, it is in the moment of writing.
//!
//! The narrower honesty that omission cost: `StallRestarted` maps to the
//! episode kind `never_streamed_restart`, whose own doc describes killing
//! and relaunching an *"alive-but-silent (or never-streamed)"* child. So a
//! socket that connected but never delivered a byte CAN produce an episode
//! row and IS visible here. The blind spot is narrower than the first draft
//! implied and is stated precisely below.
//!
//! So the blind spot is precisely this: a connection that never completed a
//! TCP/TLS handshake at all. It is absent from this table, and an absent row
//! reads exactly like a connection that was never planned. A socket that
//! connected and then went silent is NOT in the blind spot — the stall
//! supervisor acts on it and that action is recorded.
//!
//! The blind spot is not hypothetical: on 2026-08-12 the main feed failed
//! twelve dial attempts in a row with `HTTP 400` and never completed a
//! handshake all session. That socket would appear here as nothing at all.
//!
//! It is stated here rather than papered over because the question this
//! module was built to answer is *"for ALL the connections"*, and on this
//! one case the honest answer is that it does not know. The signals that DO
//! see it today are `tv_dhan_ws_dial_failed_total{endpoint,reason}` and the
//! `WS-GAP-03` coded error — both CloudWatch-side, neither in this table.
//!
//! Closing it properly means recording the PLANNED connection set at attach
//! (a new kind, written once per connection before the first dial) so that
//! planned-minus-observed becomes a keyed row rather than an inference.
//! That is a change to the live lane's boot path and a new SYMBOL value in a
//! DEDUP-keyed table, so it is deliberately NOT smuggled in beside a fold —
//! it is its own change with its own review.
//!
//! Deriving the expected set from the 16-socket authorization instead would
//! be WORSE, and the reason is worth recording: the pool plans sockets from
//! instrument counts (the main feed packs, depth spreads one instrument per
//! connection), so the planned count moves day to day. Asserting sixteen
//! would manufacture eight false "never came up" rows on an ordinary day —
//! a fabricated finding, which is worse than a missing one.

use std::collections::BTreeMap;

use tickvault_storage::ws_connection_daily_persistence::WsConnectionDailyRow;

use crate::feed_scoreboard_boot::{EpisodeTally, day_bounds_micros, fold_episode_into_tally};

/// The composite connection identity: `(feed, ws_type, connection_index)`.
/// This is the I-P1-11 composite-uniqueness discipline extended to WebSocket
/// streams — the same key `ws_event_audit` and `feed_episode_audit` use, so
/// the three tables join without translation.
pub type ConnKey = (String, String, i64);

/// Wire label for a `connected` lifecycle event.
pub const EVENT_KIND_CONNECTED: &str = "connected";
/// Wire label for an in-session `disconnected` lifecycle event.
pub const EVENT_KIND_DISCONNECTED: &str = "disconnected";
/// Wire label for an off-hours `disconnected` lifecycle event.
pub const EVENT_KIND_DISCONNECTED_OFF_HOURS: &str = "disconnected_off_hours";
/// Wire label for a successful `reconnected` lifecycle event.
pub const EVENT_KIND_RECONNECTED: &str = "reconnected";

/// The day's `ws_event_audit` rows, reduced to what the rollup folds.
///
/// A SEPARATE builder from `feed_scoreboard_boot::build_ws_events_day_sql`
/// deliberately: that shape is pinned by its own tests and read by the
/// classifier, and widening it to carry `pool_size` / `attempts` would put
/// this table's needs inside a query the blame path depends on. Two small
/// queries over the same bounded day cost less than one coupled one.
///
/// Micros literals — the ONLY representation legal in an embedded QuestDB
/// TIMESTAMP comparison (the 2026-04-28 regression lock).
#[must_use]
pub fn build_ws_connection_day_sql(target_ist_day: u64) -> String {
    let (start, end) = day_bounds_micros(target_ist_day);
    format!(
        "select feed, ws_type, connection_index, pool_size, event_kind, \
         down_secs, attempts \
         from ws_event_audit where ts >= {start} and ts < {end}"
    )
}

/// Extract the `/exec` dataset rows. `None` on any shape mismatch — the
/// caller records that as "could not read", never as "no events".
fn parse_dataset(body: &str) -> Option<Vec<serde_json::Value>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(v.get("dataset")?.as_array()?.clone())
}

/// Fold a [`build_ws_connection_day_sql`] response into per-connection rows.
/// Pure. Returns the number of rows folded; `None` = the body itself was
/// unparsable (distinct from a body that parsed to zero rows).
///
/// Rows with an unexpected column shape are SKIPPED rather than panicking,
/// and skipped rows are excluded from the returned count so a caller can see
/// the difference between "folded 100" and "read 100, folded 3".
#[must_use]
pub fn fold_ws_event_rows(
    out: &mut BTreeMap<ConnKey, WsConnectionDailyRow>,
    body: &str,
) -> Option<usize> {
    let rows = parse_dataset(body)?;
    let mut folded = 0_usize;
    for row in rows {
        let cols = match row.as_array() {
            Some(c) if c.len() >= 7 => c,
            _ => continue,
        };
        let feed = cols[0].as_str().unwrap_or("").to_string();
        let ws_type = cols[1].as_str().unwrap_or("").to_string();
        let Some(conn_idx) = cols[2].as_i64() else {
            continue;
        };
        let pool_size = cols[3].as_i64().unwrap_or(0);
        let kind = cols[4].as_str().unwrap_or("");
        let down_secs = cols[5].as_i64().unwrap_or(0);
        let attempts = cols[6].as_i64().unwrap_or(0);

        let entry = out
            .entry((feed.clone(), ws_type.clone(), conn_idx))
            .or_insert_with(|| WsConnectionDailyRow {
                feed,
                ws_type,
                connection_index: conn_idx,
                ..WsConnectionDailyRow::default()
            });

        // The connection produced a lifecycle event, whatever kind it was.
        // This is the flag that separates "healthy all day" from "never
        // appeared" — see the module header.
        entry.saw_any_event = true;
        // pool_size is a configured value, identical on every row of a given
        // ws_type; take the largest seen so a truncated/zero row cannot
        // shrink it below the real pool.
        entry.pool_size = entry.pool_size.max(pool_size);

        match kind {
            EVENT_KIND_CONNECTED => entry.connects = entry.connects.saturating_add(1),
            EVENT_KIND_RECONNECTED => {
                entry.reconnects = entry.reconnects.saturating_add(1);
                entry.total_down_secs = entry.total_down_secs.saturating_add(down_secs);
                entry.max_down_secs = entry.max_down_secs.max(down_secs);
                entry.total_attempts = entry.total_attempts.saturating_add(attempts);
                entry.max_attempts = entry.max_attempts.max(attempts);
            }
            EVENT_KIND_DISCONNECTED | EVENT_KIND_DISCONNECTED_OFF_HOURS => {
                entry.disconnect_events = entry.disconnect_events.saturating_add(1);
            }
            // sleep_entered / sleep_resumed / stall_restarted are real events
            // and DO set `saw_any_event`, but they are not disconnects and
            // must not be counted as any. The stall count comes from the
            // classified episode table, which is the single tally rule.
            _ => {}
        }
        folded += 1;
    }
    Some(folded)
}

/// Fold a `feed_scoreboard_boot::build_episode_day_sql` response into the
/// SAME per-connection rows, using the SINGLE shared tally rule
/// [`fold_episode_into_tally`].
///
/// Reusing that function rather than re-implementing the classification is
/// deliberate: its own doc calls out that the SQL read-back path and the
/// in-memory path must never diverge on what counts as a headline incident.
/// A third, per-connection copy of those rules would be a third way to
/// disagree with the daily scoreboard about the same day.
///
/// Unlike the per-feed scoreboard fold, this does NOT filter out
/// non-market-data ws_types: the order-update socket is one of the sixteen
/// connections the operator asked about, and excluding it would make the
/// table silently incomplete. It is separable at read time by `ws_type`.
///
/// Pure. Returns rows folded; `None` = unparsable body.
#[must_use]
pub fn fold_episode_rows_per_connection(
    out: &mut BTreeMap<ConnKey, WsConnectionDailyRow>,
    body: &str,
) -> Option<usize> {
    let rows = parse_dataset(body)?;
    let mut folded = 0_usize;
    for row in rows {
        // Shape from build_episode_day_sql:
        // feed, episode_kind, blame, market_hours, ws_type, connection_index, ts
        let cols = match row.as_array() {
            Some(c) if c.len() >= 6 => c,
            _ => continue,
        };
        let feed = cols[0].as_str().unwrap_or("").to_string();
        let kind = cols[1].as_str().unwrap_or("");
        let blame = cols[2].as_str().unwrap_or("");
        let market_hours = cols[3].as_bool().unwrap_or(false);
        let ws_type = cols[4].as_str().unwrap_or("").to_string();
        let Some(conn_idx) = cols[5].as_i64() else {
            continue;
        };

        let entry = out
            .entry((feed.clone(), ws_type.clone(), conn_idx))
            .or_insert_with(|| WsConnectionDailyRow {
                feed,
                ws_type,
                connection_index: conn_idx,
                ..WsConnectionDailyRow::default()
            });

        let mut tally = EpisodeTally {
            disconnects_market: entry.disconnects_market,
            disconnects_off_hours: entry.disconnects_off_hours,
            stalls: entry.stalls,
            restarts: entry.restarts,
            blame_broker: entry.blame_broker,
            blame_ours: entry.blame_ours,
            blame_indeterminate: entry.blame_indeterminate,
        };
        fold_episode_into_tally(&mut tally, kind, blame, market_hours);
        entry.disconnects_market = tally.disconnects_market;
        entry.disconnects_off_hours = tally.disconnects_off_hours;
        entry.stalls = tally.stalls;
        entry.restarts = tally.restarts;
        entry.blame_broker = tally.blame_broker;
        entry.blame_ours = tally.blame_ours;
        entry.blame_indeterminate = tally.blame_indeterminate;
        folded += 1;
    }
    Some(folded)
}

/// Stamp the day's deterministic timestamps onto every folded row and return
/// them ready for ILP write, ordered by connection identity.
///
/// The timestamp is DETERMINISTIC (IST midnight of the target day) so a
/// re-run of the daily job UPSERTs its rows in place rather than duplicating
/// them — the same idempotence contract `feed_episode_audit` relies on.
#[must_use]
pub fn finalize_rows(
    folded: BTreeMap<ConnKey, WsConnectionDailyRow>,
    day_ist_midnight_nanos: i64,
) -> Vec<WsConnectionDailyRow> {
    folded
        .into_values()
        .map(|mut r| {
            r.ts_ist_nanos = day_ist_midnight_nanos;
            r.trading_date_ist_nanos = day_ist_midnight_nanos;
            r
        })
        .collect()
}

/// One-line operator verdict for the day, built from the finalized rows.
/// Returns `(connections_seen, connections_with_incidents, total_incidents)`.
///
/// `connections_seen` counts only connections that actually produced an
/// event — a row with `saw_any_event = false` is deliberately NOT counted as
/// seen, so "16 connections seen, 0 incidents" can never be produced by
/// sixteen connections that all failed to start.
#[must_use]
pub fn day_verdict(rows: &[WsConnectionDailyRow]) -> (usize, usize, i64) {
    let seen = rows.iter().filter(|r| r.saw_any_event).count();
    let with_incidents = rows.iter().filter(|r| r.incident_total() > 0).count();
    let total = rows
        .iter()
        .fold(0_i64, |acc, r| acc.saturating_add(r.incident_total()));
    (seen, with_incidents, total)
}

// ---------------------------------------------------------------------------
// The daily job step (orchestration over the pure folds above)
// ---------------------------------------------------------------------------

use tickvault_common::config::QuestDbConfig;
use tickvault_storage::ws_connection_daily_persistence::{
    WsConnectionDailyWriter, ensure_ws_connection_daily_table,
};
use tracing::{error, info};

/// HTTP timeout for the two `/exec` reads. Matches the scoreboard's own.
const ROLLUP_HTTP_TIMEOUT_SECS: u64 = 20;

/// Build the day's per-connection rollup and persist it. Best-effort by
/// design: this table SUMMARISES `ws_event_audit` + `feed_episode_audit`,
/// both of which are already durable, so a failure here loses a convenience
/// view and never loses evidence. Every failure is loud and coded.
///
/// Returns the rows written, or `None` when nothing could be built.
///
/// FAIL-LOUD, NEVER FAIL-QUIET: an unreadable `ws_event_audit` aborts the
/// rollup with a coded error rather than writing a table full of zeros. A
/// zero-filled row is indistinguishable from a genuinely quiet day, and this
/// table exists precisely so that distinction is never lost again.
// TEST-EXEMPT: orchestration over the unit-tested pure folds above (build_ws_connection_day_sql / fold_ws_event_rows / fold_episode_rows_per_connection / finalize_rows / day_verdict); a direct test needs live QuestDB.
pub async fn run_ws_connection_rollup(
    questdb: &QuestDbConfig,
    target_ist_day: u64,
    day_ist_midnight_nanos: i64,
    episode_day_sql: &str,
) -> Option<Vec<WsConnectionDailyRow>> {
    ensure_ws_connection_daily_table(questdb).await;

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(ROLLUP_HTTP_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = "SCOREBOARD-01",
                stage = "ws_connection_rollup_client",
                ?err,
                "SCOREBOARD-01: ws_connection_daily rollup skipped — HTTP client \
                 build failed. The per-connection evidence is UNAFFECTED (it \
                 lives in ws_event_audit and feed_episode_audit); only the daily \
                 per-connection summary row is missing for this day."
            );
            return None;
        }
    };
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let fetch = |sql: String| {
        let client = client.clone();
        let url = url.clone();
        async move {
            match client
                .get(&url)
                .query(&[("query", sql.as_str())])
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => resp.text().await.ok(),
                Ok(resp) => {
                    error!(
                        code = "SCOREBOARD-01",
                        stage = "ws_connection_rollup_query",
                        status = %resp.status(),
                        "SCOREBOARD-01: ws_connection_daily rollup query returned non-2xx"
                    );
                    None
                }
                Err(err) => {
                    error!(
                        code = "SCOREBOARD-01",
                        stage = "ws_connection_rollup_query",
                        ?err,
                        "SCOREBOARD-01: ws_connection_daily rollup query failed"
                    );
                    None
                }
            }
        }
    };

    let mut folded: BTreeMap<ConnKey, WsConnectionDailyRow> = BTreeMap::new();

    // The connection SET comes from the event table. Without it there is no
    // honest denominator, so an unreadable body ABORTS rather than producing
    // a table of zeros that would read as a clean day.
    let ws_body = fetch(build_ws_connection_day_sql(target_ist_day)).await?;
    let ws_folded = match fold_ws_event_rows(&mut folded, &ws_body) {
        Some(n) => n,
        None => {
            error!(
                code = "SCOREBOARD-01",
                stage = "ws_connection_rollup_parse",
                "SCOREBOARD-01: ws_event_audit body unparsable — ws_connection_daily \
                 NOT written for this day. Writing it would have produced zeros, \
                 which is indistinguishable from a day with no disconnects."
            );
            return None;
        }
    };

    // Episodes are additive detail. A failure here degrades the row (counts
    // present, blame absent) rather than losing it — so it is reported, not
    // fatal.
    let mut episodes_folded = 0_usize;
    let mut episodes_complete = false;
    if let Some(ep_body) = fetch(episode_day_sql.to_string()).await {
        match fold_episode_rows_per_connection(&mut folded, &ep_body) {
            Some(n) => {
                episodes_folded = n;
                episodes_complete = true;
            }
            None => error!(
                code = "SCOREBOARD-01",
                stage = "ws_connection_rollup_episode_parse",
                "SCOREBOARD-01: feed_episode_audit body unparsable — \
                 ws_connection_daily rows are written WITHOUT classified \
                 incident counts or blame for this day (raw event counts are \
                 still present and correct)"
            ),
        }
    }

    let rows = finalize_rows(folded, day_ist_midnight_nanos);
    if rows.is_empty() {
        info!(
            ws_events_folded = ws_folded,
            "ws_connection_daily: no WebSocket lifecycle events for this day — \
             no per-connection rows written. On a trading day this is itself a \
             finding: it means no connection produced any event at all."
        );
        return Some(rows);
    }

    let mut writer = WsConnectionDailyWriter::new(questdb);
    let mut appended = 0_usize;
    for r in &rows {
        if let Err(err) = writer.append_row(r) {
            error!(
                code = "SCOREBOARD-01",
                stage = "ws_connection_rollup_append",
                feed = r.feed.as_str(),
                ws_type = r.ws_type.as_str(),
                connection_index = r.connection_index,
                ?err,
                "SCOREBOARD-01: ws_connection_daily append failed for one connection"
            );
            continue;
        }
        appended += 1;
    }
    if let Err(err) = writer.flush() {
        error!(
            code = "SCOREBOARD-01",
            stage = "ws_connection_rollup_flush",
            rows = appended,
            ?err,
            "SCOREBOARD-01: ws_connection_daily flush failed — the per-connection \
             summary is missing for this day (the underlying ws_event_audit and \
             feed_episode_audit rows are unaffected)"
        );
        return Some(rows);
    }

    let (seen, with_incidents, total_incidents) = day_verdict(&rows);
    let never_appeared = rows.len().saturating_sub(seen);
    info!(
        rows = appended,
        connections_seen = seen,
        connections_never_appeared = never_appeared,
        connections_with_incidents = with_incidents,
        total_incidents,
        ws_events_folded = ws_folded,
        episodes_folded,
        episodes_complete,
        "ws_connection_daily: per-connection day written — \
         'did connection N drop today?' is now a single keyed row read"
    );
    // A connection that produced NO event all day is a finding, not a clean
    // day, and it is the one shape a zero-incident summary would hide.
    if never_appeared > 0 {
        error!(
            code = "SCOREBOARD-01",
            stage = "ws_connection_rollup_never_appeared",
            connections_never_appeared = never_appeared,
            "SCOREBOARD-01: {never_appeared} connection(s) recorded classified \
             incidents but produced NO lifecycle event all day — the event and \
             episode tables disagree about whether those sockets existed"
        );
    }
    Some(rows)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_body(rows: &str) -> String {
        format!("{{\"dataset\":[{rows}]}}")
    }

    #[test]
    fn test_sql_bounds_the_day_and_selects_the_folded_columns() {
        let sql = build_ws_connection_day_sql(20_000);
        for col in [
            "feed",
            "ws_type",
            "connection_index",
            "pool_size",
            "event_kind",
            "down_secs",
            "attempts",
        ] {
            assert!(sql.contains(col), "SQL is missing `{col}`");
        }
        assert!(sql.contains("from ws_event_audit"));
        let (start, end) = day_bounds_micros(20_000);
        assert!(sql.contains(&format!("ts >= {start}")));
        assert!(sql.contains(&format!("ts < {end}")));
    }

    #[test]
    fn test_sql_uses_micros_not_nanos() {
        // The 2026-04-28 regression: a nanosecond literal in an embedded
        // QuestDB timestamp comparison puts the window in the year 58502 and
        // silently matches ZERO rows — which renders as "no disconnects".
        let sql = build_ws_connection_day_sql(20_000);
        let (start_micros, _) = day_bounds_micros(20_000);
        let nanos = start_micros.saturating_mul(1_000);
        assert!(sql.contains(&start_micros.to_string()));
        assert!(
            !sql.contains(&nanos.to_string()),
            "a nanos literal would match no rows and read as a clean day"
        );
    }

    #[test]
    fn test_a_healthy_connection_still_gets_a_row() {
        // The whole point: a connection with one connect and nothing else
        // must produce a row saying so, not silence.
        let body = ws_body(r#"["dhan","main_feed",0,5,"connected",0,0]"#);
        let mut out = BTreeMap::new();
        assert_eq!(fold_ws_event_rows(&mut out, &body), Some(1));
        let rows = finalize_rows(out, 1_769_990_400_000_000_000);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].saw_any_event);
        assert!(rows[0].clean_day());
        assert_eq!(rows[0].connects, 1);
        assert_eq!(rows[0].pool_size, 5);
    }

    #[test]
    fn test_each_connection_is_kept_separate() {
        // If the key collapsed to `feed`, fifteen of sixteen sockets would be
        // silently merged — the exact failure the per-feed scoreboard has.
        let body = ws_body(
            r#"["dhan","main_feed",0,5,"connected",0,0],
               ["dhan","main_feed",1,5,"connected",0,0],
               ["dhan","main_feed",1,5,"disconnected",0,0],
               ["dhan","depth_20",0,5,"connected",0,0],
               ["dhan","order_update",0,1,"connected",0,0]"#,
        );
        let mut out = BTreeMap::new();
        assert_eq!(fold_ws_event_rows(&mut out, &body), Some(5));
        assert_eq!(out.len(), 4, "four distinct connections");
        let rows = finalize_rows(out, 0);
        let conn1 = rows
            .iter()
            .find(|r| r.ws_type == "main_feed" && r.connection_index == 1)
            .expect("main_feed conn 1");
        assert_eq!(conn1.disconnect_events, 1);
        let conn0 = rows
            .iter()
            .find(|r| r.ws_type == "main_feed" && r.connection_index == 0)
            .expect("main_feed conn 0");
        assert_eq!(
            conn0.disconnect_events, 0,
            "conn 0 must not inherit conn 1's disconnect"
        );
        assert!(conn0.clean_day());
        assert!(!conn1.clean_day());
    }

    #[test]
    fn test_reconnect_carries_downtime_and_attempts() {
        let body = ws_body(
            r#"["dhan","main_feed",2,5,"reconnected",12,3],
               ["dhan","main_feed",2,5,"reconnected",40,1]"#,
        );
        let mut out = BTreeMap::new();
        fold_ws_event_rows(&mut out, &body).expect("parse");
        let rows = finalize_rows(out, 0);
        assert_eq!(rows[0].reconnects, 2);
        assert_eq!(rows[0].total_down_secs, 52);
        assert_eq!(
            rows[0].max_down_secs, 40,
            "worst single outage, not the last"
        );
        assert_eq!(rows[0].total_attempts, 4);
        assert_eq!(rows[0].max_attempts, 3, "worst single ladder, not the last");
    }

    #[test]
    fn test_sleep_events_set_seen_but_count_as_no_disconnect() {
        let body = ws_body(
            r#"["dhan","main_feed",0,5,"sleep_entered",0,0],
               ["dhan","main_feed",0,5,"sleep_resumed",0,0]"#,
        );
        let mut out = BTreeMap::new();
        fold_ws_event_rows(&mut out, &body).expect("parse");
        let rows = finalize_rows(out, 0);
        assert!(rows[0].saw_any_event);
        assert_eq!(rows[0].disconnect_events, 0);
        assert_eq!(rows[0].reconnects, 0);
        assert!(
            rows[0].clean_day(),
            "the scheduled post-close sleep is not an incident"
        );
    }

    #[test]
    fn test_off_hours_disconnect_counts_as_a_disconnect_event() {
        let body = ws_body(r#"["dhan","main_feed",0,5,"disconnected_off_hours",0,0]"#);
        let mut out = BTreeMap::new();
        fold_ws_event_rows(&mut out, &body).expect("parse");
        let rows = finalize_rows(out, 0);
        assert_eq!(rows[0].disconnect_events, 1);
        assert!(!rows[0].clean_day());
    }

    #[test]
    fn test_unparsable_body_is_none_not_an_empty_clean_day() {
        // Rule 11: "could not read" must never render as "no disconnects".
        let mut out = BTreeMap::new();
        assert_eq!(fold_ws_event_rows(&mut out, "not json"), None);
        assert_eq!(fold_ws_event_rows(&mut out, r#"{"no_dataset":1}"#), None);
        assert!(out.is_empty());
    }

    #[test]
    fn test_short_rows_are_skipped_not_counted() {
        let body = ws_body(r#"["dhan","main_feed",0],["dhan","main_feed",0,5,"connected",0,0]"#);
        let mut out = BTreeMap::new();
        assert_eq!(
            fold_ws_event_rows(&mut out, &body),
            Some(1),
            "the malformed row must not inflate the folded count"
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn test_a_null_connection_index_is_skipped_rather_than_defaulted_to_zero() {
        // Defaulting a missing index to 0 would file another socket's event
        // against connection 0 — a wrong answer that looks like a real one.
        let body = ws_body(r#"["dhan","main_feed",null,5,"disconnected",0,0]"#);
        let mut out = BTreeMap::new();
        assert_eq!(fold_ws_event_rows(&mut out, &body), Some(0));
        assert!(out.is_empty());
    }

    #[test]
    fn test_episodes_fold_onto_the_same_connection_row() {
        let ws = ws_body(r#"["dhan","main_feed",1,5,"connected",0,0]"#);
        // feed, episode_kind, blame, market_hours, ws_type, connection_index, ts
        let ep = ws_body(r#"["dhan","disconnect","broker",true,"main_feed",1,0]"#);
        let mut out = BTreeMap::new();
        fold_ws_event_rows(&mut out, &ws).expect("ws parse");
        fold_episode_rows_per_connection(&mut out, &ep).expect("ep parse");
        assert_eq!(out.len(), 1, "both sources fold onto ONE row");
        let rows = finalize_rows(out, 0);
        assert!(rows[0].saw_any_event);
        assert_eq!(rows[0].disconnects_market, 1);
        assert_eq!(rows[0].blame_broker, 1);
        assert!(!rows[0].clean_day());
        assert_eq!(rows[0].incident_total(), 1);
    }

    #[test]
    fn test_an_episode_for_a_connection_with_no_events_is_still_recorded() {
        // The combination is itself a finding: classified incidents exist for
        // a connection that produced no lifecycle event. Dropping the row
        // would hide it.
        let ep = ws_body(r#"["dhan","stall_restart","ours",true,"main_feed",4,0]"#);
        let mut out = BTreeMap::new();
        fold_episode_rows_per_connection(&mut out, &ep).expect("ep parse");
        let rows = finalize_rows(out, 0);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].saw_any_event);
        assert_eq!(rows[0].stalls, 1);
        assert!(!rows[0].clean_day());
    }

    #[test]
    fn test_a_connection_that_never_opened_produces_no_row() {
        // PASSES BY DESIGN — this pins the limitation, it does not assert a
        // feature. Both sources are things that HAPPENED, and a socket that
        // failed every dial produces neither a lifecycle event nor a
        // classified episode, so it is absent from the fold entirely.
        //
        // The 2026-08-12 main feed is the real instance: twelve dial failures,
        // `HTTP 400`, zero handshakes all session. Here that connection is
        // simply not in the input, and the output is empty rather than a row
        // saying it never came up.
        //
        // If a future change records the PLANNED set at attach, THIS test is
        // the one that must flip — and the module header section it mirrors
        // must be rewritten in the same change.
        let events = ws_body(r#"["dhan","main_feed",0,5,"connected",0,0]"#);
        let mut out = BTreeMap::new();
        fold_ws_event_rows(&mut out, &events).expect("parse");
        let rows = finalize_rows(out, 0);

        // Connection 0 came up and is here. Connections 1..=4 of the same
        // planned pool never dialled and are invisible.
        assert_eq!(rows.len(), 1, "only the socket that OPENED is represented");
        assert_eq!(rows[0].connection_index, 0);
        assert!(
            !rows.iter().any(|r| r.connection_index == 1),
            "a planned-but-never-dialled connection is absent, NOT reported \
             as down — the documented blind spot"
        );
    }

    #[test]
    fn test_the_never_opened_blind_spot_stays_documented() {
        // A limitation that lives only in prose rots (this repository has the
        // receipts). Pin the header section so the gap cannot be quietly
        // dropped from the docs while it is still real in the code.
        //
        // Scan the PRODUCTION region only — split at the first column-0
        // `#[cfg(test)]`, the house pattern. The first draft of this test
        // scanned the whole file and was therefore VACUOUS: the pin strings
        // live in the array below, so `include_str!` found them in the test's
        // own source and the assertion could never fail. Bite-proven after
        // the split (deleting the header section turns it red).
        let whole = include_str!("ws_connection_rollup.rs");
        let src = whole
            .split_once("\n#[cfg(test)]")
            .map_or(whole, |(production, _)| production);
        assert!(
            src.len() < whole.len(),
            "the production/test split marker vanished — without it this test \
             reads its own pin array and passes vacuously"
        );
        for pin in [
            "What this CANNOT answer",
            "The connection set is OBSERVED, not AUTHORIZED",
            "would manufacture eight false",
            // The seventh kind. The first draft of the section above listed
            // six and omitted it; pin the corrected count so the same slip
            // cannot land twice.
            "stall_restarted",
        ] {
            assert!(
                src.contains(pin),
                "the never-opened blind spot lost its header pin `{pin}` — either \
                 the limitation was fixed (then rewrite the section and flip \
                 test_a_connection_that_never_opened_produces_no_row) or the \
                 warning was deleted while the gap remains, which is the \
                 false-completeness class this module exists to avoid"
            );
        }
    }

    #[test]
    fn test_the_order_update_socket_is_not_excluded() {
        // The per-feed scoreboard deliberately drops non-market-data ws_types
        // from its headline. This table must NOT, or it silently covers 15 of
        // the 16 connections the operator asked about.
        let ep = ws_body(r#"["dhan","disconnect","broker",true,"order_update",0,0]"#);
        let mut out = BTreeMap::new();
        fold_episode_rows_per_connection(&mut out, &ep).expect("ep parse");
        let rows = finalize_rows(out, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ws_type, "order_update");
        assert_eq!(rows[0].disconnects_market, 1);
    }

    #[test]
    fn test_finalize_stamps_the_same_deterministic_day_on_every_row() {
        let body = ws_body(
            r#"["dhan","main_feed",0,5,"connected",0,0],
               ["dhan","depth_200",0,5,"connected",0,0]"#,
        );
        let mut out = BTreeMap::new();
        fold_ws_event_rows(&mut out, &body).expect("parse");
        let day = 1_769_990_400_000_000_000_i64;
        let rows = finalize_rows(out, day);
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r.ts_ist_nanos, day, "re-runs must UPSERT, not duplicate");
            assert_eq!(r.trading_date_ist_nanos, day);
        }
    }

    #[test]
    fn test_day_verdict_does_not_call_sixteen_dead_connections_a_clean_day() {
        // Sixteen rows, none of which ever produced an event, all with zero
        // incidents. A naive verdict reads "0 incidents" and looks perfect.
        let dead: Vec<WsConnectionDailyRow> = (0..16)
            .map(|i| WsConnectionDailyRow {
                feed: "dhan".to_string(),
                ws_type: "main_feed".to_string(),
                connection_index: i,
                saw_any_event: false,
                ..WsConnectionDailyRow::default()
            })
            .collect();
        let (seen, with_incidents, total) = day_verdict(&dead);
        assert_eq!(seen, 0, "none of them was ever seen — that is the finding");
        assert_eq!(with_incidents, 0);
        assert_eq!(total, 0);
    }

    #[test]
    fn test_day_verdict_counts_seen_connections_and_incidents() {
        let rows = vec![
            WsConnectionDailyRow {
                feed: "dhan".to_string(),
                ws_type: "main_feed".to_string(),
                connection_index: 0,
                saw_any_event: true,
                connects: 1,
                ..WsConnectionDailyRow::default()
            },
            WsConnectionDailyRow {
                feed: "dhan".to_string(),
                ws_type: "main_feed".to_string(),
                connection_index: 1,
                saw_any_event: true,
                connects: 1,
                disconnects_market: 2,
                stalls: 1,
                ..WsConnectionDailyRow::default()
            },
        ];
        let (seen, with_incidents, total) = day_verdict(&rows);
        assert_eq!(seen, 2);
        assert_eq!(with_incidents, 1);
        assert_eq!(total, 3);
    }

    #[test]
    fn test_pool_size_takes_the_largest_seen_not_the_last() {
        // A truncated or zero-valued row must not shrink the recorded pool
        // below its real size — otherwise "conn 4 of 1" reads as impossible.
        let body = ws_body(
            r#"["dhan","main_feed",0,5,"connected",0,0],
               ["dhan","main_feed",0,0,"disconnected",0,0]"#,
        );
        let mut out = BTreeMap::new();
        fold_ws_event_rows(&mut out, &body).expect("parse");
        let rows = finalize_rows(out, 0);
        assert_eq!(rows[0].pool_size, 5);
    }
}
