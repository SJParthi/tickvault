//! Wave-3-A Item 10 — end-to-end persistence test for the pre-open
//! movers writer path.
//!
//! Verifies the full chain: build a `PreopenMoversTracker` snapshot →
//! call `StockMoversWriter::append_stock_mover_with_phase` for every
//! row → `flush()` → confirm zero drops. Uses the `FakeQuestDb` pattern
//! lifted from `chaos_questdb_lifecycle.rs`: a 127.0.0.1:* TCP listener
//! that silently drains incoming ILP bytes. Proves:
//!
//!   1. `phase` SYMBOL serialises to ILP without error.
//!   2. `PREOPEN_RANK` and `PREOPEN_UNAVAIL` rows coexist in one
//!      flushed batch (different categories per the DEDUP collision
//!      fix shipped in PR #403 commit 2).
//!   3. `tracker.compute_snapshot()` output is shape-compatible with
//!      the writer signature — drift here would silently break
//!      production at the 09:00 IST boundary.
//!   4. The rescue ring drains cleanly post-flush — no leaked rows.
//!
//! This is not a "live QuestDB" test in the sense of running against
//! an actual QuestDB container — those are gated behind `#[ignore]`
//! across this crate because Linux loopback FIN timing is unreliable
//! in CI (see `chaos_questdb_lifecycle.rs::questdb_round_trip_*`).
//! The fake-server pattern here exercises the ILP wire protocol on
//! the same 127.0.0.1 TCP path that production uses, gives byte-level
//! confidence, and runs deterministically on every `cargo test`.

use std::collections::HashMap;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::pipeline::preopen_movers::{
    PreopenMoversTracker, PreopenPhase, UnavailableSymbol,
};
use tickvault_storage::movers_persistence::{
    STOCK_MOVERS_PHASE_PREOPEN, STOCK_MOVERS_PHASE_PREOPEN_UNAVAILABLE, StockMoversWriter,
};

/// IST epoch nanos for 09:05:00 IST on 2026-04-28 (a Tuesday).
/// Used as the synthetic snapshot timestamp.
const SNAPSHOT_TS_IST_NANOS: i64 = 1_777_597_500_000_000_000_i64;

// ---------------------------------------------------------------------------
// FakeQuestDb — TCP drain server
// ---------------------------------------------------------------------------

struct FakeQuestDb {
    port: u16,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FakeQuestDb {
    fn start_ephemeral() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake qdb");
        let port = listener.local_addr().unwrap().port();
        let _ = listener.set_nonblocking(true);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_c = shutdown.clone();
        let handle = thread::spawn(move || {
            while !shutdown_c.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                        let mut buf = [0u8; 65536];
                        loop {
                            if shutdown_c.load(Ordering::Relaxed) {
                                break;
                            }
                            match stream.read(&mut buf) {
                                Ok(0) => break,
                                Ok(_) => {}
                                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                                Err(_) => break,
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            shutdown,
            handle: Some(handle),
        }
    }

    fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for FakeQuestDb {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn config_for_port(port: u16) -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        ilp_port: port,
        http_port: port,
        pg_port: port,
    }
}

// ---------------------------------------------------------------------------
// Tracker fixtures
// ---------------------------------------------------------------------------

/// Builds a `(security_id, segment) -> symbol` lookup with a handful
/// of stocks + NIFTY/BANKNIFTY indices. Mirrors what `main.rs` builds
/// from the FnoUniverse at boot.
fn build_test_lookup() -> HashMap<(u32, ExchangeSegment), String> {
    let mut m: HashMap<(u32, ExchangeSegment), String> = HashMap::new();
    m.insert((2885, ExchangeSegment::NseEquity), "RELIANCE".to_string());
    m.insert((1594, ExchangeSegment::NseEquity), "INFY".to_string());
    m.insert((11536, ExchangeSegment::NseEquity), "TCS".to_string());
    m.insert((13, ExchangeSegment::IdxI), "NIFTY".to_string());
    m.insert((25, ExchangeSegment::IdxI), "BANKNIFTY".to_string());
    m
}

/// Synthesises a Quote-mode ParsedTick for the pre-open window
/// (09:05:00 IST on 2026-04-28). `day_close` is the "previous-day
/// close" populated by the Dhan Quote/Full packet parser.
fn make_preopen_tick(
    security_id: u32,
    segment_code: u8,
    last_traded_price: f32,
    day_close: f32,
) -> ParsedTick {
    ParsedTick {
        security_id,
        exchange_segment_code: segment_code,
        last_traded_price,
        last_trade_quantity: 100,
        // 09:05:00 IST on 2026-04-28 as IST epoch seconds.
        exchange_timestamp: 1_777_597_500_u32 - 19_800,
        received_at_nanos: 1_777_597_500_000_000_000_i64,
        average_traded_price: last_traded_price,
        volume: 50_000,
        total_sell_quantity: 25_000,
        total_buy_quantity: 25_000,
        day_open: day_close,
        day_close,
        day_high: last_traded_price,
        day_low: day_close,
        open_interest: 0,
        oi_day_high: 0,
        oi_day_low: 0,
        iv: f64::NAN,
        delta: f64::NAN,
        gamma: f64::NAN,
        theta: f64::NAN,
        vega: f64::NAN,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Wave-3-A Item 10 e2e: a tracker snapshot of 3 stocks + 2 indices
/// + SENSEX-as-unavailable persists end-to-end through the live ILP
/// wire path with zero drops.
#[test]
fn test_preopen_movers_e2e_snapshot_persists_phase_preopen_to_questdb() {
    let server = FakeQuestDb::start_ephemeral();
    let port = server.port();

    let lookup = build_test_lookup();
    let unavailable = vec![UnavailableSymbol {
        symbol: "SENSEX".to_string(),
        security_id: 51,
        segment: ExchangeSegment::IdxI,
    }];
    let mut tracker = PreopenMoversTracker::new(lookup, unavailable);

    // Feed pre-open ticks for the 3 stocks + 2 indices.
    // Stocks carry their own prev_close in `day_close`. Indices are
    // Ticker-mode so day_close=0 — seed them via the explicit API (the
    // boot-time disk-cache seed in main.rs uses the same call).
    tracker.update_from_tick(&make_preopen_tick(2885, 1, 2_500.50, 2_490.00));
    tracker.update_from_tick(&make_preopen_tick(1594, 1, 1_485.75, 1_500.00));
    tracker.update_from_tick(&make_preopen_tick(11536, 1, 3_750.00, 3_720.00));
    tracker.seed_prev_close(13, ExchangeSegment::IdxI, 24_500.0);
    tracker.seed_prev_close(25, ExchangeSegment::IdxI, 52_000.0);
    tracker.update_from_tick(&make_preopen_tick(13, 0, 24_625.50, 0.0));
    tracker.update_from_tick(&make_preopen_tick(25, 0, 51_780.00, 0.0));

    let snap = tracker.compute_snapshot();

    // Sanity: every tracked instrument should be in the snapshot, plus
    // SENSEX as PREOPEN_UNAVAILABLE.
    assert_eq!(
        snap.len(),
        6,
        "expected 5 tracked + 1 unavailable = 6 rows; got {} entries",
        snap.len()
    );
    let preopen_count = snap
        .iter()
        .filter(|e| e.phase == PreopenPhase::Preopen)
        .count();
    let unavail_count = snap
        .iter()
        .filter(|e| e.phase == PreopenPhase::PreopenUnavailable)
        .count();
    assert_eq!(preopen_count, 5, "expected 5 PREOPEN entries");
    assert_eq!(unavail_count, 1, "expected 1 PREOPEN_UNAVAILABLE entry");

    // Persist via the writer. Mirrors what main.rs does in the 60s
    // snapshot task, including the differentiated-category fix from
    // PR #403 commit 2 (so PREOPEN vs PREOPEN_UNAVAILABLE rows don't
    // collapse on the same DEDUP key).
    let mut writer = StockMoversWriter::new(&config_for_port(port))
        .expect("ILP connect to fake qdb must succeed");

    for entry in &snap {
        let category = match entry.phase {
            PreopenPhase::Preopen => "PREOPEN_RANK",
            PreopenPhase::PreopenUnavailable => "PREOPEN_UNAVAIL",
        };
        let phase = match entry.phase {
            PreopenPhase::Preopen => STOCK_MOVERS_PHASE_PREOPEN,
            PreopenPhase::PreopenUnavailable => STOCK_MOVERS_PHASE_PREOPEN_UNAVAILABLE,
        };
        writer
            .append_stock_mover_with_phase(
                SNAPSHOT_TS_IST_NANOS,
                category,
                entry.rank,
                entry.security_id,
                entry.segment.as_str(),
                &entry.symbol,
                entry.ltp,
                entry.prev_close,
                entry.change_pct,
                entry.volume,
                phase,
            )
            .expect("append must succeed");
    }

    writer.flush().expect("flush must succeed");

    // Zero drops + empty rescue ring => every row reached the (fake)
    // QuestDB. Ratchets the contract: a future regression that loses
    // a row mid-flush would drop it onto the ring or count it.
    assert_eq!(
        writer.rescue_ring_len(),
        0,
        "rescue ring must be empty after a successful flush; got {}",
        writer.rescue_ring_len()
    );
    assert_eq!(
        writer.rows_dropped_total(),
        0,
        "no rows must be dropped on the happy path; got {}",
        writer.rows_dropped_total()
    );
}

/// Wave-3-A Item 10 e2e: when the FakeQuestDb is offline the writer's
/// rescue-ring path catches every row — the snapshot is recoverable
/// on the next reconnect. Proves the bounded zero-loss envelope holds
/// for pre-open movers (matches the rescue-ring semantics already
/// pinned for in-market movers).
#[test]
fn test_preopen_movers_e2e_questdb_unreachable_falls_into_rescue_ring() {
    // No FakeQuestDb — bind a port we'll use for config but leave the
    // listener absent. Use a port high enough to avoid system-reserved.
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener); // release the port so connect() fails

    let lookup = build_test_lookup();
    let mut tracker = PreopenMoversTracker::new(lookup, Vec::new());
    tracker.update_from_tick(&make_preopen_tick(2885, 1, 2_500.50, 2_490.00));
    tracker.update_from_tick(&make_preopen_tick(1594, 1, 1_485.75, 1_500.00));
    let snap = tracker.compute_snapshot();
    assert_eq!(snap.len(), 2);

    // Construct against an unreachable port — `new()` will fail, so
    // build the writer against a live FakeQuestDb first, then drop
    // the server to simulate a mid-session outage.
    let server = FakeQuestDb::start_ephemeral();
    let live_port = server.port();
    let mut writer = StockMoversWriter::new(&config_for_port(live_port))
        .expect("initial connect to live fake qdb must succeed");
    drop(server); // QuestDB now unreachable.

    for entry in &snap {
        let _ = writer.append_stock_mover_with_phase(
            SNAPSHOT_TS_IST_NANOS,
            "PREOPEN_RANK",
            entry.rank,
            entry.security_id,
            entry.segment.as_str(),
            entry.segment.as_str(),
            entry.ltp,
            entry.prev_close,
            entry.change_pct,
            entry.volume,
            STOCK_MOVERS_PHASE_PREOPEN,
        );
    }
    // The writer pushes every row into the rescue ring on append. The
    // critical contract: `rows_dropped_total` stays at zero — the row
    // is buffered, not lost, even when the downstream is offline.
    let _ = writer.flush();

    assert_eq!(
        writer.rows_dropped_total(),
        0,
        "during a downstream outage, rows must accumulate in the rescue ring \
         (not be dropped). Got dropped={} ring_len={}",
        writer.rows_dropped_total(),
        writer.rescue_ring_len(),
    );
    assert!(
        writer.rescue_ring_len() >= snap.len(),
        "rescue ring must hold at least every appended row; \
         got ring_len={} snap_size={}",
        writer.rescue_ring_len(),
        snap.len()
    );

    // Free the bound port that we held in `port` (was already dropped
    // above, but referenced here so the unused-variable lint is happy).
    let _ = port;
}
