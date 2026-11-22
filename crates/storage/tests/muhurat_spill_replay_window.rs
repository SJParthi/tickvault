//! B1 review 5 (2026-11-13): the spill replay's window filter judges a row
//! with the Muhurat flag of the ROW's own IST date, through the same
//! `session_window` functions the writers call.
//!
//! The replay had its own private copy of the per-date lookup, injected into
//! a helper only a unit test called, so a change that passed the process
//! flag (today's) to the production path again would have left every test
//! green while a Muhurat evening tick spilled and replayed on a later day was
//! dropped as out of window. This binary drives the PRODUCTION replay
//! (`replay_spill_dir`) with the shipped trading calendar installed and an
//! ordinary-day process (the Muhurat flag reads `false`), and checks what
//! reaches the database.
//!
//! Its own binary because the calendar `OnceLock` is process-wide.
//!
//! The 18:30 rows are a synthetic evening session on 2026-11-08 (the shipped
//! 2026 session is 14:05 to 15:40, inside the regular window); see
//! `crates/common/tests/muhurat_row_day_window.rs` for that scope note.

use std::sync::{Arc, Mutex};

use tickvault_common::config::TradingConfig;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_storage::ilp_table_gate::EnsuredTables;
use tickvault_storage::tick_spill_replay::{SpillSinkView, replay_spill_dir};

/// The shipped `[trading]` section (2026-11-08 is a Muhurat date there).
fn shipped_trading_config() -> TradingConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/base.toml");
    let raw = std::fs::read_to_string(path).expect("config/base.toml");
    let table: toml::Table = raw.parse().expect("base.toml parses");
    table
        .get("trading")
        .expect("[trading]")
        .clone()
        .try_into()
        .expect("[trading] deserializes")
}

/// IST epoch nanoseconds of `date` at 18:30 IST.
fn evening(y: i32, mo: u32, d: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(y, mo, d)
        .and_then(|day| day.and_hms_opt(18, 30, 0))
        .map(|t| t.and_utc().timestamp())
        .expect("valid time")
        .saturating_mul(1_000_000_000)
}

/// A listener that records every POST body and answers 204.
async fn spawn_recording_listener() -> (String, Arc<Mutex<Vec<u8>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}/write", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte).await {
                    Ok(1) => head.push(byte[0]),
                    _ => break,
                }
            }
            let content_length = String::from_utf8_lossy(&head)
                .to_ascii_lowercase()
                .lines()
                .find_map(|l| l.strip_prefix("content-length:").map(str::to_owned))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0_u8; content_length];
            if sock.read_exact(&mut body).await.is_err() {
                continue;
            }
            sink.lock().expect("lock").extend_from_slice(&body);
            let _ = sock
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await;
            let _ = sock.flush().await;
        }
    });
    (url, seen, handle)
}

/// A view for a closed, quiet file with every table ensured and no floor.
fn view() -> SpillSinkView {
    let mut view = SpillSinkView::current();
    view.suspect = false;
    view.clean_probes = 0;
    view.suspect_episodes = 0;
    view.episode_start = None;
    view.live = false;
    view.now_unix_secs = view.now_unix_secs.saturating_add(2 * 3600);
    view.market_data_floor_unix_nanos = None;
    view.ensured_tables = EnsuredTables::ALL;
    view
}

/// Regression, B1 review 5: the exchange-clocked (tick) and arrival-clocked
/// (depth) replay filters keep an 18:30 row from a Muhurat date and still
/// refuse the same evening on an ordinary date, while this process's own
/// Muhurat flag is `false`.
///
/// Bite: make `line_is_positively_out_of_window` judge with
/// `muhurat::current()` (for example `nanos_in_any_open_window(nanos,
/// tickvault_common::muhurat::current())`) and the Muhurat-day rows are
/// dropped (the first asserts).
#[tokio::test]
async fn test_regression_replay_keeps_a_muhurat_evening_row_on_a_later_day() {
    let calendar = TradingCalendar::from_config(&shipped_trading_config()).expect("calendar");
    assert!(
        calendar.is_muhurat_trading_day(chrono::NaiveDate::from_ymd_opt(2026, 11, 8).expect("d")),
        "the shipped calendar must name 2026-11-08 a Muhurat date for this test"
    );
    assert!(tickvault_common::market_hours::set_market_calendar_for_session(Arc::new(calendar)));
    assert!(
        !tickvault_common::muhurat::current(),
        "this process plays a replaying boot on an ordinary day"
    );

    let root = std::env::temp_dir().join(format!(
        "tv-muhurat-replay-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let client = reqwest::Client::builder().build().expect("client");
    for (folder, measurement) in [("ticks", "ticks"), ("depth", "market_depth")] {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).expect("dir");
        let muhurat_line = format!(
            "{measurement},segment=NSE_EQ,feed=dhan security_id=1i,ltp=1.0 {}\n",
            evening(2026, 11, 8)
        );
        let ordinary_line = format!(
            "{measurement},segment=NSE_EQ,feed=dhan security_id=2i,ltp=2.0 {}\n",
            evening(2026, 11, 9)
        );
        std::fs::write(
            dir.join(format!("{folder}-dhan-1.ilp")),
            format!("{muhurat_line}{ordinary_line}"),
        )
        .expect("write");
        let (url, seen, server) = spawn_recording_listener().await;
        let _ = replay_spill_dir(&dir, &url, &client, &view()).await;
        server.abort();
        let posted = String::from_utf8(seen.lock().expect("lock").clone()).expect("utf8");
        assert!(
            posted.contains(&muhurat_line),
            "{folder}: a Muhurat-day evening row is kept on a later day: {posted:?}"
        );
        assert!(
            !posted.contains(&ordinary_line),
            "{folder}: an ordinary-day evening row is still refused: {posted:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
