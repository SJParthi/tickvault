#![cfg(test)]
//! CASCADE-01 LEG 2, rebuilt 2026-11-07 against the LIVE tick writer.
//!
//! "QuestDB down" was retired from `crates/core/tests/chaos_cascade_triple_failure.rs`
//! on 2026-07-17 because the tick writer it exercised had been deleted. The
//! tick writer came back with the 2026-08-09 Dhan live-feed revival
//! (`TickWriter`, this module's parent), and nothing in the chaos suite drove
//! it against an unreachable database until this file.
//!
//! What it proves, end to end through the production types, for BOTH row
//! encoders:
//!
//! 1. A `TickWriter` built by its production constructor
//!    ([`TickWriter::new_with_encoder`]) and split for offload exactly as the
//!    live lane splits it, pointed at a CLOSED local port (connection refused,
//!    no Docker), hands every batch to its writer thread, the writer thread's
//!    real HTTP flush fails, and every row is rescued to the tick spill
//!    directory. Zero rows are reported landed, nothing stays pending, and the
//!    spill files hold every appended `capture_seq` exactly once.
//! 2. Once a reachable endpoint appears (an in-process HTTP server that
//!    records each POST and answers 204), the production spill replay
//!    ([`crate::tick_spill_replay::replay_spill_dir`]) delivers every rescued
//!    row, each `capture_seq` exactly once, and a second round after the acks
//!    are confirmed sends nothing more.
//!
//! A child of `tick_persistence`, so it can assert the sink really holds a
//! network sender (the rescue is caused by the network, not by the test
//! constructor's "no sender" shortcut) and can point the spill tier at a
//! scratch directory.

use super::*;
use std::io::{BufRead as _, BufReader, Read as _};
use std::sync::{Arc, Mutex};

/// Rows per hand-off, and hand-offs per encoder: 400 rows in four batches,
/// the hand-off queue's depth, so the queue is exercised without filling.
const ROWS_PER_BATCH: i64 = 100;
const BATCHES: i64 = FLUSH_QUEUE_DEPTH as i64;

fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "tv-cascade-qdb-down-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A port nothing listens on: bound, read, released. Connecting to it is
/// refused at once, which is how an unreachable QuestDB looks to the writer.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

fn questdb_config(port: u16) -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_owned(),
        http_port: port,
        pg_port: 8812,
        ilp_port: 9009,
    }
}

/// An IDX_I tick inside the session window (2026-05-28 12:30:00 IST), with
/// the price moved per row so no two rows share their text.
fn tick(n: i64) -> ParsedTick {
    #[allow(clippy::cast_precision_loss)] // APPROVED: test fixture, n < 1,000
    let price = 23_146.45 + (n as f32) * 0.05;
    ParsedTick {
        security_id: 13,
        exchange_segment_code: 0,
        last_traded_price: price,
        last_trade_quantity: 50,
        exchange_timestamp: 1_779_971_400,
        received_at_nanos: 1_779_951_600_111_000_000,
        average_traded_price: 23_145.1,
        volume: 1_234_567,
        total_sell_quantity: 9_000,
        total_buy_quantity: 10_000,
        day_open: 23_100.0,
        day_close: 23_090.0,
        day_high: 23_200.0,
        day_low: 23_050.0,
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

/// Every `capture_seq` carried by a `ticks` line in `bytes`, in order. A line
/// of another table, or a `ticks` line without the column, fails the test:
/// both would mean the bytes are not the rows this test appended.
fn capture_seqs(bytes: &[u8]) -> Vec<i64> {
    let text = std::str::from_utf8(bytes).expect("ILP text is UTF-8");
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            assert!(
                line.starts_with("ticks,"),
                "a non-ticks line in the bytes: {line}"
            );
            let at = line
                .find("capture_seq=")
                .unwrap_or_else(|| panic!("ticks line without capture_seq: {line}"));
            let digits: String = line[at + "capture_seq=".len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().expect("capture_seq digits")
        })
        .collect()
}

/// Every `capture_seq` in every spill file in `dir`.
fn spilled_seqs(dir: &Path) -> Vec<i64> {
    let mut seqs = Vec::new();
    for path in crate::tick_spill_replay::list_spill_files(dir) {
        seqs.extend(capture_seqs(&std::fs::read(&path).expect("read spill")));
    }
    seqs
}

/// The sorted list `1..=n`, the set the test appended.
fn expected_seqs() -> Vec<i64> {
    (1..=ROWS_PER_BATCH * BATCHES).collect()
}

/// An in-process "QuestDB came back": answers every request 204 on
/// keep-alive connections and records each `POST /write` body.
fn recording_questdb() -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || serve_connection(stream, &seen));
        }
    });
    (port, bodies)
}

fn serve_connection(stream: std::net::TcpStream, seen: &Mutex<Vec<Vec<u8>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut writer = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let mut length = 0_usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        if request_line.starts_with("POST /write") {
            seen.lock().expect("bodies").push(body);
        }
        if writer
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .is_err()
        {
            return;
        }
    }
}

/// LEG 1 of the scenario for one encoder: QuestDB unreachable, the live
/// offloaded writer rescues every row. Returns the spill directory.
fn assert_rows_survive_an_unreachable_questdb(encoder: TickIlpEncoder) -> PathBuf {
    let dir = scratch_dir(match encoder {
        TickIlpEncoder::Library => "library",
        TickIlpEncoder::Own => "own",
    });
    // The spill refuses a write that would leave less than
    // SPILL_MIN_FREE_HEADROOM_BYTES free, by design. On a host that full the
    // rows below would be refused and counted, and this test would fail with
    // no hint of why, so say so first (the seal escalation tests' precedent).
    let free = spill_free_bytes(&dir).unwrap_or(0);
    let needed = SPILL_MIN_FREE_HEADROOM_BYTES + 64 * 1024 * 1024;
    assert!(
        free >= needed,
        "HOST PRECONDITION UNMET, not a code defect: {} has {free} bytes free, the tick \
         spill refuses writes below {SPILL_MIN_FREE_HEADROOM_BYTES} \
         (SPILL_MIN_FREE_HEADROOM_BYTES). Free up disk and re-run; do NOT lower the floor.",
        dir.display()
    );
    let writer = TickWriter::new_with_encoder(&questdb_config(closed_port()), Feed::Dhan, encoder)
        .with_spill_dir_for_test(dir.clone());
    let (mut producer, sink, rx) = writer.split_for_offload();
    assert!(
        sink.sender.is_some(),
        "{encoder:?}: the sink must hold a real network sender, so the rescue below is \
         caused by the refused connection and not by a writer built without one"
    );
    assert_eq!(sink.spill_dir, dir, "the sink spills where the writer does");

    // The writer thread, as the live lane runs it: write every batch until
    // the producer closes the queue.
    let writer_thread = std::thread::spawn(move || {
        let mut sink = sink;
        let mut landed = 0_usize;
        let mut batches = 0_usize;
        for mut batch in rx {
            landed += sink.write(&mut batch);
            batches += 1;
        }
        (landed, batches)
    });

    for batch in 0..BATCHES {
        for row in 1..=ROWS_PER_BATCH {
            let seq = batch * ROWS_PER_BATCH + row;
            producer
                .append_tick_with_seq(&tick(seq), seq)
                .expect("append in-window tick");
        }
        producer.flush().expect("hand-off to the writer thread");
    }
    // A full queue retains rows (backpressure, not loss); keep handing off
    // until nothing is pending. Bounded: 200 tries, 10 ms apart.
    for _ in 0..200 {
        if producer.pending() == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _retained_or_rescued = producer.flush();
    }
    assert_eq!(
        producer.pending(),
        0,
        "{encoder:?}: every row left the producer"
    );
    producer.close_offload();
    let (landed, batches) = writer_thread.join().expect("writer thread");

    assert_eq!(
        landed, 0,
        "{encoder:?}: QuestDB is unreachable, so the writer must report nothing landed"
    );
    assert!(
        batches >= 1,
        "{encoder:?}: the writer thread received the batches"
    );
    let mut seqs = spilled_seqs(&dir);
    let spilled = seqs.len();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(
        spilled,
        expected_seqs().len(),
        "{encoder:?}: the spill must hold every row exactly once (no loss, no copy)"
    );
    assert_eq!(
        seqs,
        expected_seqs(),
        "{encoder:?}: every appended capture_seq must be in the spill"
    );
    dir
}

/// LEG 2 of the scenario: QuestDB is reachable again; the production replay
/// lands every spilled row exactly once.
fn assert_spilled_rows_land_once_questdb_is_back(encoder: TickIlpEncoder, dir: &Path) {
    let (port, bodies) = recording_questdb();
    let url = crate::tick_spill_replay::write_url("127.0.0.1", port);
    let client = crate::http_client::build_probe_client(5).expect("client");
    // Clean sink, clock two hours on so the just-written file reads closed
    // and quiet; no market-data floor, so a floor another test installed in
    // this process cannot drop the fixture rows.
    let mut view = crate::tick_spill_replay::tests::test_spill_view();
    view.market_data_floor_unix_nanos = None;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let first = runtime.block_on(crate::tick_spill_replay::replay_spill_dir(
        dir, &url, &client, &view,
    ));
    assert_eq!(first.files_failed, 0, "{encoder:?}: {first:?}");
    assert_eq!(first.files_quarantined, 0, "{encoder:?}: {first:?}");
    assert!(first.files_replayed >= 1, "{encoder:?}: {first:?}");
    assert_eq!(first.lines_below_floor, 0, "{encoder:?}: {first:?}");
    let mut delivered: Vec<i64> = bodies
        .lock()
        .expect("bodies")
        .iter()
        .flat_map(|body| capture_seqs(body))
        .collect();
    let posted = delivered.len();
    delivered.sort_unstable();
    delivered.dedup();
    assert_eq!(
        posted,
        expected_seqs().len(),
        "{encoder:?}: the replay must deliver every spilled row exactly once"
    );
    assert_eq!(delivered, expected_seqs(), "{encoder:?}: every row landed");
    let posts_after_first_round = post_count(&bodies);

    // The acks confirmed: the drained files are emptied and nothing is sent
    // again, so a row lands once, not once per round.
    let confirmed = crate::tick_spill_replay::tests::confirmed_spill_view(&view);
    let second = runtime.block_on(crate::tick_spill_replay::replay_spill_dir(
        dir, &url, &client, &confirmed,
    ));
    assert_eq!(second.files_failed, 0, "{encoder:?}: {second:?}");
    assert_eq!(second.bytes_replayed, 0, "{encoder:?}: nothing is re-sent");
    assert_eq!(
        post_count(&bodies),
        posts_after_first_round,
        "{encoder:?}: no POST after the confirmed round"
    );
    assert!(
        spilled_seqs(dir).is_empty(),
        "{encoder:?}: the drained spill files are emptied once their acks are confirmed"
    );
}

/// The `POST /write` requests recorded so far.
fn post_count(bodies: &Mutex<Vec<Vec<u8>>>) -> usize {
    bodies.lock().expect("bodies").len()
}

/// CASCADE-01 LEG 2 (QuestDB down), both encoders: the live offloaded tick
/// writer loses no row while QuestDB is unreachable, and the rows land exactly
/// once when it is back.
#[test]
fn chaos_cascade_01_leg2_questdb_down_offloaded_tick_rows_spill_then_land_once() {
    for encoder in [TickIlpEncoder::Library, TickIlpEncoder::Own] {
        let dir = assert_rows_survive_an_unreachable_questdb(encoder);
        assert_spilled_rows_land_once_questdb_is_back(encoder, &dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
