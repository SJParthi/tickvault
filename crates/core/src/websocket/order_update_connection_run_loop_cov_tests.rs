//! Coverage tests for the order-update WebSocket reconnect loop
//! (`run_order_update_connection`), its post-close sleep, the outage page,
//! the audit-row emitter and the pre-dial half of `connect_and_listen`.
//!
//! Every test asserts an outcome: the `ws_event_audit` rows the loop emits
//! (kind, source, reason, attempts, down_secs), counters observed through a
//! thread-local metrics recorder, the login frame a local 127.0.0.1 server
//! receives, or the returned error variant. Time is paused (tokio test-util),
//! so backoff, post-close sleep and the stability window run in virtual time.
//!
//! The market-hours branch of the loop reads the wall clock through
//! `is_within_market_hours(&calendar)`. Tests pin it OFF deterministically by
//! listing today (IST) as a holiday; the one in-market test asserts whichever
//! branch the wall clock actually selects.

use super::*;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use arc_swap::ArcSwap;
use chrono::{Datelike, Weekday};
use secrecy::SecretString;
use tickvault_common::config::{NseHolidayEntry, TradingConfig};
use tickvault_common::ws_event_types::{WsEventAuditRow, WsEventKind};
use tokio::sync::mpsc;

use crate::auth::types::TokenState;
use crate::notification::NotificationService;

/// Serialises the tests in this file: they share the process-wide token
/// manager (read by the post-close sleep) and, in one test, the market-hours
/// override.
static RUN_LOOP_TEST_LOCK: Mutex<()> = Mutex::new(());

const TEST_CLIENT_ID: &str = "1000000001";
const NO_TOKEN_REASON: &str = "no authentication token available";

fn lock() -> std::sync::MutexGuard<'static, ()> {
    RUN_LOOP_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Metrics recorder (thread-local, via `metrics::with_local_recorder`)
// ---------------------------------------------------------------------------

type CounterMap = Arc<Mutex<BTreeMap<String, u64>>>;

#[derive(Default)]
struct CountingRecorder {
    counters: CounterMap,
}

struct CountingCounter {
    key: String,
    counters: CounterMap,
}

impl metrics::CounterFn for CountingCounter {
    fn increment(&self, value: u64) {
        if let Ok(mut map) = self.counters.lock() {
            let slot = map.entry(self.key.clone()).or_insert(0);
            *slot = slot.saturating_add(value);
        }
    }
    fn absolute(&self, value: u64) {
        if let Ok(mut map) = self.counters.lock() {
            map.insert(self.key.clone(), value);
        }
    }
}

impl metrics::Recorder for CountingRecorder {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        let mut labels: Vec<String> = key
            .labels()
            .map(|l| format!("{}={}", l.key(), l.value()))
            .collect();
        labels.sort();
        let rendered = if labels.is_empty() {
            key.name().to_string()
        } else {
            format!("{}{{{}}}", key.name(), labels.join(","))
        };
        metrics::Counter::from_arc(Arc::new(CountingCounter {
            key: rendered,
            counters: Arc::clone(&self.counters),
        }))
    }
    fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        metrics::Gauge::noop()
    }
    fn register_histogram(
        &self,
        _: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::Histogram::noop()
    }
}

impl CountingRecorder {
    fn get(&self, key: &str) -> u64 {
        self.counters
            .lock()
            .map(|m| m.get(key).copied().unwrap_or(0))
            .unwrap_or(0)
    }
}

/// Runs `make()`'s future on a paused current-thread runtime with `recorder`
/// installed on this thread. Every task of a current-thread runtime is polled
/// on this thread, so spawned tasks record into `recorder` too.
fn run_paused<T, F>(recorder: &CountingRecorder, make: impl FnOnce() -> F) -> T
where
    F: std::future::Future<Output = T>,
{
    metrics::with_local_recorder(recorder, || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("paused runtime builds");
        rt.block_on(make())
    })
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn today_ist() -> chrono::NaiveDate {
    Utc::now().with_timezone(&ist_offset()).date_naive()
}

/// A calendar with no holidays, or with today (IST) listed as a holiday so
/// `is_within_market_hours` is false whatever the wall clock reads. A weekend
/// is already a non-trading day (and the calendar refuses weekend holidays).
fn calendar(holiday_today: bool) -> TradingCalendar {
    let today = today_ist();
    let mut holidays = Vec::new();
    if holiday_today && !matches!(today.weekday(), Weekday::Sat | Weekday::Sun) {
        holidays.push(NseHolidayEntry {
            date: today.format("%Y-%m-%d").to_string(),
            name: "Test holiday".to_string(),
        });
    }
    let cfg = TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: holidays,
        muhurat_trading_dates: vec![],
        nse_mock_trading_dates: vec![],
    };
    TradingCalendar::from_config(&cfg).expect("test calendar builds")
}

fn off_hours_calendar() -> Arc<TradingCalendar> {
    let cal = calendar(true);
    assert!(
        !is_within_market_hours(&cal),
        "precondition: today is a holiday or weekend, so the loop runs off-hours"
    );
    Arc::new(cal)
}

fn token_state(valid_for_secs: i64) -> TokenState {
    let now = Utc::now().with_timezone(&ist_offset());
    TokenState::from_cached(
        SecretString::from("eyJ.test-order-update.token".to_string()),
        now + chrono::Duration::seconds(valid_for_secs),
        now - chrono::Duration::hours(1),
    )
}

fn token_handle(state: Option<TokenState>) -> TokenHandle {
    Arc::new(ArcSwap::new(Arc::new(state)))
}

/// The post-close sleep consults the process-wide token manager. Make sure it
/// holds a token valid for a day, so `force_renewal_if_stale` takes its
/// fresh-token arm and never reaches the network. The only other reader of
/// the global manager is an idempotency test that does not read its token.
fn install_fresh_global_token_manager() {
    let _ = crate::auth::token_manager::set_global_token_manager(
        crate::auth::token_manager::TokenManager::new_for_test(Some(token_state(86_400))),
    );
    let tm =
        crate::auth::token_manager::global_token_manager().expect("global token manager installed");
    tm.token_handle().store(Arc::new(Some(token_state(86_400))));
}

fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

async fn recv_rows(rx: &mut mpsc::Receiver<WsEventAuditRow>, n: usize) -> Vec<WsEventAuditRow> {
    let mut rows = Vec::with_capacity(n);
    while rows.len() < n {
        rows.push(rx.recv().await.expect("the loop keeps emitting audit rows"));
    }
    rows
}

fn assert_order_update_row_shape(row: &WsEventAuditRow) {
    assert_eq!(row.feed, tickvault_common::feed::Feed::Dhan);
    assert_eq!(
        row.ws_type,
        tickvault_common::ws_event_types::WsType::OrderUpdate
    );
    assert_eq!(row.connection_index, 0);
    assert_eq!(row.pool_size, 1);
    assert_eq!(
        row.dhan_code,
        tickvault_common::ws_event_types::WS_EVENT_NO_DHAN_CODE
    );
    assert_eq!(
        row.trading_date_ist_nanos % (86_400 * 1_000_000_000),
        0,
        "the trading date is an IST midnight"
    );
    assert!(row.event_ts_ist_nanos >= row.trading_date_ist_nanos);
}

/// A 127.0.0.1 WebSocket server. Connection `i` reads the login frame, waits
/// `hold_secs[i]` (0 when absent) and sends a Close frame. Returns the URL,
/// the login frames received, and the accepted-connection count.
async fn spawn_close_server(
    hold_secs: Vec<u64>,
) -> (String, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1");
    let port = listener.local_addr().expect("local addr").port();
    let logins = Arc::new(Mutex::new(Vec::new()));
    let accepted = Arc::new(AtomicUsize::new(0));
    let logins_server = Arc::clone(&logins);
    let accepted_server = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let index = accepted_server.fetch_add(1, Ordering::SeqCst);
            let hold = hold_secs.get(index).copied().unwrap_or(0);
            let logins_conn = Arc::clone(&logins_server);
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                if let Some(Ok(Message::Text(login))) = ws.next().await
                    && let Ok(mut l) = logins_conn.lock()
                {
                    l.push(login.to_string());
                }
                if hold > 0 {
                    tokio::time::sleep(Duration::from_secs(hold)).await;
                }
                let _ = ws.close(None).await;
                while let Some(Ok(_)) = ws.next().await {}
            });
        }
    });
    (format!("ws://127.0.0.1:{port}"), logins, accepted)
}

// ---------------------------------------------------------------------------
// run_order_update_connection — the reconnect loop
// ---------------------------------------------------------------------------

/// Off-hours, every connect fails (no token). Attempts 1..=10 retry with
/// backoff; the 11th exceeds `ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS`, so the loop
/// sleeps until the next open (SleepEntered/SleepResumed rows), resets the
/// streak and retries from attempt 1.
#[test]
fn test_run_loop_off_hours_failures_exhaust_then_sleep_and_restart_streak() {
    let _guard = lock();
    install_fresh_global_token_manager();
    let recorder = CountingRecorder::default();
    let cal = off_hours_calendar();
    let max = ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS as usize;

    let rows = run_paused(&recorder, || async move {
        let (audit_tx, mut audit_rx) = mpsc::channel(512);
        let (order_tx, _order_rx) = broadcast::channel(16);
        let task = tokio::spawn(run_order_update_connection(
            "ws://127.0.0.1:9".to_string(),
            TEST_CLIENT_ID.to_string(),
            token_handle(None),
            order_tx,
            cal,
            None,
            None,
            None,
            Some(NotificationService::disabled()),
            Some(audit_tx),
            None,
        ));
        // max + 1 failure rows, SleepEntered, SleepResumed, first failure of
        // the next episode.
        let rows = recv_rows(&mut audit_rx, max + 4).await;
        task.abort();
        let _ = task.await;
        rows
    });

    let expected_source =
        tickvault_common::disconnect_cause::classify_disconnect_cause(NO_TOKEN_REASON, None)
            .label();
    for (i, row) in rows.iter().take(max + 1).enumerate() {
        assert_order_update_row_shape(row);
        assert_eq!(row.event_kind, WsEventKind::DisconnectedOffHours, "row {i}");
        assert_eq!(row.reason, NO_TOKEN_REASON, "row {i}");
        assert_eq!(row.source, expected_source, "row {i}");
        assert_eq!(row.down_secs, 0, "row {i}");
        assert_eq!(
            row.attempts,
            i64::try_from(i + 1).expect("small"),
            "row {i}"
        );
    }
    let entered = &rows[max + 1];
    let resumed = &rows[max + 2];
    assert_order_update_row_shape(entered);
    assert_order_update_row_shape(resumed);
    assert_eq!(entered.event_kind, WsEventKind::SleepEntered);
    assert_eq!(resumed.event_kind, WsEventKind::SleepResumed);
    assert_eq!(entered.source, "n/a");
    assert_eq!(resumed.source, "n/a");
    assert_eq!(entered.attempts, i64::try_from(max + 1).expect("small"));
    assert_eq!(resumed.attempts, entered.attempts);
    assert!(entered.down_secs > 0, "sleeps until the next market open");
    assert_eq!(resumed.down_secs, entered.down_secs);
    assert!(
        entered.down_secs <= 15 * 86_400,
        "the next open is within the calendar lookahead"
    );
    let restarted = &rows[max + 3];
    assert_eq!(restarted.event_kind, WsEventKind::DisconnectedOffHours);
    assert_eq!(
        restarted.attempts, 1,
        "the post-sleep attempt starts a fresh streak"
    );

    assert_eq!(
        recorder.get("tv_ws_post_close_sleep_total{feed=order_update}"),
        1
    );
    assert!(
        recorder.get("tv_order_update_reconnections_total") >= max as u64,
        "every retried failure goes through the backoff arm"
    );
    assert_eq!(
        recorder.get("tv_order_update_outage_pages_total"),
        0,
        "never paged off-hours"
    );
    assert_eq!(
        recorder.get("tv_telegram_dropped_total{reason=noop_mode}"),
        2,
        "SleepEntered + SleepResumed reach the notifier; a fresh token sends no renewal notice"
    );
    assert_eq!(
        recorder.get("tv_ws_event_audit_dropped_total{reason=full}"),
        0
    );
}

/// PR-E runtime gate: while the Dhan feed flag is off the loop does not
/// connect (no rows); turning it back on resumes, and an off/on cycle resets
/// the failure streak.
#[test]
fn test_run_loop_dhan_feed_flag_holds_the_loop_dormant_and_resets_the_streak() {
    let _guard = lock();
    install_fresh_global_token_manager();
    let recorder = CountingRecorder::default();
    let cal = off_hours_calendar();

    run_paused(&recorder, || async move {
        let (audit_tx, mut audit_rx) = mpsc::channel(512);
        let (order_tx, _order_rx) = broadcast::channel(16);
        let flag = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run_order_update_connection(
            "ws://127.0.0.1:9".to_string(),
            TEST_CLIENT_ID.to_string(),
            token_handle(None),
            order_tx,
            cal,
            None,
            None,
            None,
            None,
            Some(audit_tx),
            Some(Arc::clone(&flag)),
        ));
        // Wait past the boot-time market-open defer (virtual time) so the loop
        // is inside the dormant poll when the flag flips.
        let defer_secs = if tickvault_common::market_hours::is_within_market_hours_ist() {
            0
        } else {
            tickvault_common::market_hours::secs_until_next_market_open_ist().max(1)
        };
        tokio::time::sleep(Duration::from_secs(defer_secs + 30)).await;
        assert!(
            audit_rx.try_recv().is_err(),
            "a disabled Dhan feed never dials, so no audit row exists"
        );
        assert!(!task.is_finished(), "the loop idles, it does not exit");

        flag.store(true, Ordering::Relaxed);
        let first = recv_rows(&mut audit_rx, 2).await;
        assert_eq!(first[0].attempts, 1);
        assert_eq!(first[1].attempts, 2);
        assert_eq!(first[0].event_kind, WsEventKind::DisconnectedOffHours);

        // The loop is now in its 500 ms backoff; switch Dhan off again.
        flag.store(false, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(
            audit_rx.try_recv().is_err(),
            "dormant again: no dial while the flag is off"
        );

        flag.store(true, Ordering::Relaxed);
        let resumed = recv_rows(&mut audit_rx, 1).await;
        assert_eq!(
            resumed[0].attempts, 1,
            "a Dhan re-enable starts a fresh failure streak"
        );
        task.abort();
        let _ = task.await;
    });
}

/// In market hours (trading-day calendar, wall clock 09:00-16:00 IST) the loop
/// never gives up and pages exactly once per outage episode at the third
/// failure; off-hours (any other wall time) it never pages and gives up after
/// the attempt budget. The test asserts whichever branch the clock selects.
#[test]
fn test_run_loop_pages_once_per_episode_in_market_hours_and_never_off_hours() {
    let _guard = lock();
    install_fresh_global_token_manager();
    let recorder = CountingRecorder::default();
    let cal = Arc::new(calendar(false));
    let in_hours_before = is_within_market_hours(&cal);
    let rows_wanted = 13;
    let cal_run = Arc::clone(&cal);

    let rows = run_paused(&recorder, || async move {
        let (audit_tx, mut audit_rx) = mpsc::channel(512);
        let (order_tx, _order_rx) = broadcast::channel(16);
        let task = tokio::spawn(run_order_update_connection(
            "ws://127.0.0.1:9".to_string(),
            TEST_CLIENT_ID.to_string(),
            token_handle(None),
            order_tx,
            cal_run,
            None,
            None,
            None,
            Some(NotificationService::disabled()),
            Some(audit_tx),
            None,
        ));
        let rows = recv_rows(&mut audit_rx, rows_wanted).await;
        task.abort();
        let _ = task.await;
        rows
    });
    let in_hours_after = is_within_market_hours(&cal);
    if in_hours_before != in_hours_after {
        // The wall clock crossed 09:00/16:00 IST mid-test; neither branch is
        // pinned. Shape still holds.
        rows.iter().for_each(assert_order_update_row_shape);
        return;
    }

    if in_hours_before {
        for (i, row) in rows.iter().enumerate() {
            assert_order_update_row_shape(row);
            assert_eq!(row.event_kind, WsEventKind::Disconnected, "row {i}");
            assert_eq!(
                row.attempts,
                i64::try_from(i + 1).expect("small"),
                "in market hours the streak keeps growing past the budget"
            );
        }
        assert_eq!(
            recorder.get("tv_order_update_outage_pages_total"),
            1,
            "one HIGH page per outage episode, however long it lasts"
        );
        assert_eq!(
            recorder.get("tv_ws_post_close_sleep_total{feed=order_update}"),
            0
        );
        assert!(
            recorder.get("tv_telegram_dropped_total{reason=noop_mode}") >= 1,
            "the page reached the notifier"
        );
    } else {
        let max = ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS as usize;
        for row in rows.iter().take(max + 1) {
            assert_eq!(row.event_kind, WsEventKind::DisconnectedOffHours);
        }
        assert_eq!(rows[max + 1].event_kind, WsEventKind::SleepEntered);
        assert_eq!(recorder.get("tv_order_update_outage_pages_total"), 0);
    }
}

/// Clean server closes before the stability window count as failures:
/// Connected, then alternating clean-close / Reconnected rows with a growing
/// streak, until the budget is spent; off-hours that ends in the post-close
/// sleep, after which the next connect is a fresh `Connected`.
#[test]
fn test_run_loop_clean_close_flap_counts_failures_and_sleeps_off_hours() {
    let _guard = lock();
    install_fresh_global_token_manager();
    install_crypto_provider();
    let recorder = CountingRecorder::default();
    let cal = off_hours_calendar();
    let max = ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS as usize;

    let (rows, logins, accepted) = run_paused(&recorder, || async move {
        let (url, logins, accepted) = spawn_close_server(Vec::new()).await;
        let (audit_tx, mut audit_rx) = mpsc::channel(512);
        let (order_tx, _order_rx) = broadcast::channel(16);
        let task = tokio::spawn(run_order_update_connection(
            url,
            TEST_CLIENT_ID.to_string(),
            token_handle(Some(token_state(86_400))),
            order_tx,
            cal,
            None,
            None,
            None,
            Some(NotificationService::disabled()),
            Some(audit_tx),
            None,
        ));
        // Connected + (max+1) clean closes + max Reconnected + 2 sleep rows +
        // the next Connected.
        let rows = recv_rows(&mut audit_rx, 2 * max + 5).await;
        task.abort();
        let _ = task.await;
        (rows, logins, accepted)
    });

    assert_eq!(rows[0].event_kind, WsEventKind::Connected);
    assert_eq!(rows[0].attempts, 0);
    for k in 1..=max + 1 {
        let close = &rows[2 * k - 1];
        assert_order_update_row_shape(close);
        assert_eq!(
            close.event_kind,
            WsEventKind::DisconnectedOffHours,
            "close {k}"
        );
        assert_eq!(close.source, "clean close", "close {k}");
        assert_eq!(
            close.attempts,
            i64::try_from(k).expect("small"),
            "close {k}"
        );
        if k <= max {
            let reconnect = &rows[2 * k];
            assert_eq!(
                reconnect.event_kind,
                WsEventKind::Reconnected,
                "reconnect {k}"
            );
            assert_eq!(
                reconnect.attempts,
                i64::try_from(k).expect("small"),
                "a reconnect carries the streak it recovers from"
            );
        }
    }
    let entered = &rows[2 * max + 2];
    assert_eq!(entered.event_kind, WsEventKind::SleepEntered);
    assert_eq!(entered.attempts, i64::try_from(max + 1).expect("small"));
    assert_eq!(rows[2 * max + 3].event_kind, WsEventKind::SleepResumed);
    let after = &rows[2 * max + 4];
    assert_eq!(
        after.event_kind,
        WsEventKind::Connected,
        "the post-sleep connect starts a fresh episode"
    );
    assert_eq!(after.attempts, 0);

    let logins = logins.lock().expect("logins").clone();
    assert!(logins.len() > max, "every connect sent the login frame");
    for login in &logins {
        let v: serde_json::Value = serde_json::from_str(login).expect("login is JSON");
        assert_eq!(v["LoginReq"]["MsgCode"], 42);
        assert_eq!(v["LoginReq"]["ClientId"], TEST_CLIENT_ID);
        assert_eq!(v["LoginReq"]["Token"], "eyJ.test-order-update.token");
        assert_eq!(v["UserType"], "SELF");
    }
    assert!(accepted.load(Ordering::SeqCst) > max);
    assert_eq!(
        recorder.get("tv_ws_post_close_sleep_total{feed=order_update}"),
        1
    );
    assert_eq!(recorder.get("tv_order_update_outage_pages_total"), 0);
}

/// The clean-close arm pages too: in market hours a dead-token outage
/// delivered as clean Close frames reaches the same once-per-episode HIGH page
/// at the third short session. Off-hours it never pages. The test asserts
/// whichever branch the wall clock selects (trading-day calendar).
#[test]
fn test_run_loop_clean_close_flap_pages_once_in_market_hours_only() {
    let _guard = lock();
    install_fresh_global_token_manager();
    install_crypto_provider();
    let recorder = CountingRecorder::default();
    let cal = Arc::new(calendar(false));
    let in_hours_before = is_within_market_hours(&cal);
    let cal_run = Arc::clone(&cal);

    let rows = run_paused(&recorder, || async move {
        let (url, _logins, _accepted) = spawn_close_server(Vec::new()).await;
        let (audit_tx, mut audit_rx) = mpsc::channel(512);
        let (order_tx, _order_rx) = broadcast::channel(16);
        let task = tokio::spawn(run_order_update_connection(
            url,
            TEST_CLIENT_ID.to_string(),
            token_handle(Some(token_state(86_400))),
            order_tx,
            cal_run,
            None,
            None,
            None,
            Some(NotificationService::disabled()),
            Some(audit_tx),
            None,
        ));
        // Connected, then 5 x (clean close, Reconnected), then a 6th close.
        let rows = recv_rows(&mut audit_rx, 12).await;
        task.abort();
        let _ = task.await;
        rows
    });
    let in_hours_after = is_within_market_hours(&cal);
    let closes: Vec<&WsEventAuditRow> = rows.iter().filter(|r| r.source == "clean close").collect();
    assert_eq!(closes.len(), 6);
    for (i, close) in closes.iter().enumerate() {
        assert_eq!(close.attempts, i64::try_from(i + 1).expect("small"));
    }
    if in_hours_before != in_hours_after {
        return;
    }
    if in_hours_before {
        assert!(
            closes
                .iter()
                .all(|r| r.event_kind == WsEventKind::Disconnected)
        );
        assert_eq!(
            recorder.get("tv_order_update_outage_pages_total"),
            1,
            "the third short session pages once; later ones do not repeat it"
        );
    } else {
        assert!(
            closes
                .iter()
                .all(|r| r.event_kind == WsEventKind::DisconnectedOffHours)
        );
        assert_eq!(recorder.get("tv_order_update_outage_pages_total"), 0);
    }
}

/// A session that survives the stability window ends the outage episode: its
/// clean close resets the streak to 0 and the next connect is a fresh
/// `Connected`, not a `Reconnected`.
#[test]
fn test_run_loop_stable_session_resets_the_streak_and_next_connect_is_fresh() {
    let _guard = lock();
    install_fresh_global_token_manager();
    install_crypto_provider();
    let recorder = CountingRecorder::default();
    let cal = off_hours_calendar();
    let hold = ORDER_UPDATE_RECONNECT_STABILITY_SECS + 1;

    let rows = run_paused(&recorder, || async move {
        // Connection 0 closes at once (a failure), connection 1 is held past
        // the stability window, connection 2 closes at once.
        let (url, _logins, _accepted) = spawn_close_server(vec![0, hold, 0]).await;
        let (audit_tx, mut audit_rx) = mpsc::channel(512);
        let (order_tx, _order_rx) = broadcast::channel(16);
        let task = tokio::spawn(run_order_update_connection(
            url,
            TEST_CLIENT_ID.to_string(),
            token_handle(Some(token_state(86_400))),
            order_tx,
            cal,
            None,
            None,
            None,
            Some(NotificationService::disabled()),
            Some(audit_tx),
            None,
        ));
        let rows = recv_rows(&mut audit_rx, 6).await;
        task.abort();
        let _ = task.await;
        rows
    });

    let kinds: Vec<WsEventKind> = rows.iter().map(|r| r.event_kind).collect();
    assert_eq!(
        kinds,
        vec![
            WsEventKind::Connected,
            WsEventKind::DisconnectedOffHours,
            WsEventKind::Reconnected,
            WsEventKind::DisconnectedOffHours,
            WsEventKind::Connected,
            WsEventKind::DisconnectedOffHours,
        ]
    );
    let attempts: Vec<i64> = rows.iter().map(|r| r.attempts).collect();
    assert_eq!(
        attempts,
        vec![0, 1, 1, 0, 0, 1],
        "the stable session's close resets the streak; the next short session starts it again"
    );
    assert_eq!(rows[3].source, "clean close");
    assert_eq!(
        recorder.get("tv_order_update_stable_reconnects_total"),
        1,
        "the recovery is confirmed once, after the window"
    );
}

// ---------------------------------------------------------------------------
// emit_order_update_ws_audit / ws_audit_drop_reason
// ---------------------------------------------------------------------------

#[test]
fn test_emit_ws_audit_without_a_channel_is_a_no_op() {
    let recorder = CountingRecorder::default();
    metrics::with_local_recorder(&recorder, || {
        emit_order_update_ws_audit(None, WsEventKind::Connected, "n/a", "r", 0, 0);
    });
    assert!(
        recorder.counters.lock().expect("map").is_empty(),
        "no channel means nothing to send and nothing to count"
    );
}

#[test]
fn test_emit_ws_audit_sends_one_row_with_the_given_fields() {
    let (tx, mut rx) = mpsc::channel(4);
    emit_order_update_ws_audit(
        Some(&tx),
        WsEventKind::SleepEntered,
        "src-label",
        "reason text",
        u64::MAX,
        7,
    );
    let row = rx.try_recv().expect("one row");
    assert_order_update_row_shape(&row);
    assert_eq!(row.event_kind, WsEventKind::SleepEntered);
    assert_eq!(row.source, "src-label");
    assert_eq!(row.reason, "reason text");
    assert_eq!(row.down_secs, i64::MAX, "u64 down_secs saturates into i64");
    assert_eq!(row.attempts, 7);
    assert_eq!(
        row.subscription,
        tickvault_common::ws_event_types::WsSubscriptionDetail::NONE
    );
    assert!(rx.try_recv().is_err(), "exactly one row");
}

#[test]
fn test_emit_ws_audit_full_channel_drops_the_row_and_counts_full() {
    let recorder = CountingRecorder::default();
    let (tx, mut rx) = mpsc::channel(1);
    metrics::with_local_recorder(&recorder, || {
        emit_order_update_ws_audit(Some(&tx), WsEventKind::Connected, "a", "first", 0, 0);
        emit_order_update_ws_audit(Some(&tx), WsEventKind::Connected, "b", "second", 0, 0);
    });
    assert_eq!(
        recorder.get("tv_ws_event_audit_dropped_total{reason=full}"),
        1
    );
    assert_eq!(rx.try_recv().expect("first row kept").reason, "first");
    assert!(rx.try_recv().is_err(), "the second row was dropped");
}

#[test]
fn test_emit_ws_audit_closed_channel_drops_the_row_and_counts_closed() {
    let recorder = CountingRecorder::default();
    let (tx, rx) = mpsc::channel(4);
    drop(rx);
    metrics::with_local_recorder(&recorder, || {
        emit_order_update_ws_audit(Some(&tx), WsEventKind::Disconnected, "a", "r", 0, 1);
    });
    assert_eq!(
        recorder.get("tv_ws_event_audit_dropped_total{reason=closed}"),
        1
    );
    assert_eq!(
        recorder.get("tv_ws_event_audit_dropped_total{reason=full}"),
        0
    );
}

#[test]
fn test_ws_audit_drop_reason_maps_full_and_closed() {
    let (tx, rx) = mpsc::channel::<WsEventAuditRow>(1);
    let row = || WsEventAuditRow {
        event_ts_ist_nanos: 0,
        trading_date_ist_nanos: 0,
        feed: tickvault_common::feed::Feed::Dhan,
        ws_type: tickvault_common::ws_event_types::WsType::OrderUpdate,
        connection_index: 0,
        pool_size: 1,
        event_kind: WsEventKind::Connected,
        source: String::new(),
        reason: String::new(),
        dhan_code: tickvault_common::ws_event_types::WS_EVENT_NO_DHAN_CODE,
        down_secs: 0,
        attempts: 0,
        market_hours: false,
        subscription: tickvault_common::ws_event_types::WsSubscriptionDetail::NONE,
    };
    tx.try_send(row()).expect("room for one");
    let full = tx.try_send(row()).expect_err("full");
    assert_eq!(ws_audit_drop_reason(&full), "full");
    drop(rx);
    let closed = tx.try_send(row()).expect_err("closed");
    assert_eq!(ws_audit_drop_reason(&closed), "closed");
}

// ---------------------------------------------------------------------------
// emit_in_market_outage_page
// ---------------------------------------------------------------------------

#[test]
fn test_outage_page_counts_and_notifies_when_a_notifier_is_attached() {
    let recorder = CountingRecorder::default();
    let notifier = NotificationService::disabled();
    metrics::with_local_recorder(&recorder, || {
        emit_in_market_outage_page(Some(&notifier), 3, "clean close");
    });
    assert_eq!(recorder.get("tv_order_update_outage_pages_total"), 1);
    assert_eq!(
        recorder.get("tv_telegram_dropped_total{reason=noop_mode}"),
        1,
        "the OrderUpdateDisconnected event reached the notifier"
    );
}

#[test]
fn test_outage_page_without_a_notifier_still_counts() {
    let recorder = CountingRecorder::default();
    metrics::with_local_recorder(&recorder, || {
        emit_in_market_outage_page(None, 5, "io");
    });
    assert_eq!(recorder.get("tv_order_update_outage_pages_total"), 1);
    assert_eq!(
        recorder.get("tv_telegram_dropped_total{reason=noop_mode}"),
        0
    );
}

// ---------------------------------------------------------------------------
// order_update_post_close_sleep
// ---------------------------------------------------------------------------

#[test]
fn test_post_close_sleep_sleeps_until_the_next_open_and_audits_both_edges() {
    let _guard = lock();
    install_fresh_global_token_manager();
    let recorder = CountingRecorder::default();
    let cal = calendar(true);

    let (rows, elapsed, expected) = run_paused(&recorder, || async move {
        let (tx, mut rx) = mpsc::channel(8);
        let notifier = NotificationService::disabled();
        let expected = cal
            .secs_until_next_market_open(Utc::now().timestamp())
            .expect("next open within the lookahead");
        let start = tokio::time::Instant::now();
        order_update_post_close_sleep(&cal, 11, Some(&notifier), Some(&tx)).await;
        let elapsed = start.elapsed();
        let rows = recv_rows(&mut rx, 2).await;
        assert!(rx.try_recv().is_err(), "exactly two rows");
        (rows, elapsed, expected)
    });

    assert_eq!(rows[0].event_kind, WsEventKind::SleepEntered);
    assert_eq!(rows[1].event_kind, WsEventKind::SleepResumed);
    let slept = u64::try_from(rows[0].down_secs).expect("non-negative");
    assert!(
        slept.abs_diff(expected) <= 1,
        "sleeps until the calendar's next open ({slept} vs {expected})"
    );
    assert_eq!(rows[1].down_secs, rows[0].down_secs);
    assert_eq!(rows[0].attempts, 11);
    assert_eq!(
        elapsed,
        Duration::from_secs(slept),
        "the task slept exactly that long"
    );
    assert_eq!(
        recorder.get("tv_ws_post_close_sleep_total{feed=order_update}"),
        1
    );
    assert_eq!(
        recorder.get("tv_telegram_dropped_total{reason=noop_mode}"),
        2,
        "SleepEntered and SleepResumed are sent; a fresh token is not renewed"
    );
    assert_eq!(
        recorder.get("tv_token_force_renewal_total{trigger=ws_wake}"),
        0,
        "a token valid for a day is above the 4h threshold"
    );
}

#[test]
fn test_post_close_sleep_without_notifier_or_audit_channel_still_sleeps() {
    let _guard = lock();
    install_fresh_global_token_manager();
    let recorder = CountingRecorder::default();
    let cal = calendar(true);
    let elapsed = run_paused(&recorder, || async move {
        let start = tokio::time::Instant::now();
        order_update_post_close_sleep(&cal, 1, None, None).await;
        start.elapsed()
    });
    assert!(
        elapsed >= Duration::from_secs(1),
        "it slept until the next open"
    );
    assert_eq!(
        recorder.get("tv_ws_post_close_sleep_total{feed=order_update}"),
        1
    );
    assert_eq!(
        recorder.get("tv_telegram_dropped_total{reason=noop_mode}"),
        0
    );
}

// ---------------------------------------------------------------------------
// connect_and_listen — the pre-dial checks
// ---------------------------------------------------------------------------

fn connect_once(url: &str, handle: &TokenHandle) -> (Result<(), OrderUpdateConnectionError>, bool) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let cal = calendar(true);
    let (order_tx, _order_rx) = broadcast::channel(4);
    let mut stable = false;
    let result = rt.block_on(connect_and_listen(
        url,
        TEST_CLIENT_ID,
        handle,
        &order_tx,
        &cal,
        None,
        None,
        None,
        None,
        0,
        None,
        &mut stable,
    ));
    (result, stable)
}

#[test]
fn test_connect_without_a_token_fails_with_no_token() {
    let (result, stable) = connect_once("ws://127.0.0.1:9", &token_handle(None));
    assert!(matches!(result, Err(OrderUpdateConnectionError::NoToken)));
    assert!(!stable);
}

#[test]
fn test_connect_with_an_expired_token_fails_before_dialling() {
    let (result, stable) = connect_once("ws://127.0.0.1:9", &token_handle(Some(token_state(-60))));
    assert!(matches!(
        result,
        Err(OrderUpdateConnectionError::TokenExpired)
    ));
    assert!(!stable);
}

#[test]
fn test_connect_with_an_unparseable_url_fails_with_connect_error() {
    install_crypto_provider();
    let (result, stable) = connect_once(
        "not a websocket url",
        &token_handle(Some(token_state(3_600))),
    );
    match result {
        Err(OrderUpdateConnectionError::Connect(msg)) => {
            assert!(!msg.is_empty());
            assert!(
                !msg.contains("eyJ.test-order-update.token"),
                "the token never appears in a connect error"
            );
        }
        other => panic!("expected Connect error, got {other:?}"),
    }
    assert!(!stable);
}

#[test]
fn test_connect_to_a_closed_port_fails_with_connect_error() {
    install_crypto_provider();
    // Bind then drop to get a port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("free port");
    let url = format!("ws://127.0.0.1:{port}");
    let (result, stable) = connect_once(&url, &token_handle(Some(token_state(3_600))));
    assert!(
        matches!(result, Err(OrderUpdateConnectionError::Connect(_))),
        "got {result:?}"
    );
    assert!(!stable);
}
