//! B3: SIGKILL-mid-batch chaos test.
//!
//! Simulates a previous process that wrote N ticks to the spill file and
//! then died via `SIGKILL` — Drop impls did NOT run, the BufWriter never
//! flushed its internal buffer, but whatever hit `write(2)` before the
//! kill is still on disk as durable bytes.
//!
//! We can't literally `fork` a Rust subprocess from a cargo test on every
//! platform, so we simulate the post-crash state by writing the known
//! binary spill format directly to a file with the exact name the
//! recovery code looks for. Then we construct a fresh
//! `TickPersistenceWriter` pointing at the same spill directory and call
//! `recover_stale_spill_files()`, which must drain the file to QuestDB
//! (mocked via a TCP drain server).
//!
//! # Binary format (TICK_SPILL_RECORD_SIZE = 112 bytes)
//!
//! Matches `serialize_tick()` in `tick_persistence.rs`:
//!
//! ```text
//! 0..4    u32 LE  security_id
//! 4       u8      exchange_segment_code
//! 5       u8      padding
//! 6..10   f32 LE  last_traded_price
//! 10..12  u16 LE  last_trade_quantity
//! 12..16  u32 LE  exchange_timestamp
//! 16..24  i64 LE  received_at_nanos
//! 24..28  f32 LE  average_traded_price
//! 28..32  u32 LE  volume
//! 32..36  u32 LE  total_sell_quantity
//! 36..40  u32 LE  total_buy_quantity
//! 40..44  f32 LE  day_open
//! 44..48  f32 LE  day_close
//! 48..52  f32 LE  day_high
//! 52..56  f32 LE  day_low
//! 56..60  u32 LE  open_interest
//! 60..64  u32 LE  oi_day_high
//! 64..68  u32 LE  oi_day_low
//! 68..76  f64 LE  iv
//! 76..84  f64 LE  delta
//! 84..92  f64 LE  gamma
//! 92..100 f64 LE  theta
//! 100..108 f64 LE vega
//! 108..112 padding
//! ```
//!
//! If this format changes in `tick_persistence.rs`, this test fails
//! loudly — which is exactly what we want. The binary spill format is a
//! persistence boundary and changing it without a migration path would
//! lose in-flight ticks across deploys.
//!
//! # Environment
//!
//! `#[ignore]` by default. Run with:
//!
//! ```bash
//! cargo test -p dhan-live-trader-storage --test chaos_sigkill_replay -- --ignored
//! ```

#![cfg(test)]

use dhan_live_trader_common::config::QuestDbConfig;
use std::io::{Read, Write};

const TICK_SPILL_RECORD_SIZE: usize = 112;

/// Mimics the TCP drain server used by `tick_persistence` unit tests.
/// Accepts one connection and drains bytes until close.
fn spawn_tcp_drain_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 65536];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    });
    port
}

/// Builds one spill record with the given security_id + LTP. All other
/// fields are defaulted. Format must match `serialize_tick()`.
fn build_spill_record(security_id: u32, ltp: f32, ts: u32) -> [u8; TICK_SPILL_RECORD_SIZE] {
    let mut buf = [0u8; TICK_SPILL_RECORD_SIZE];
    buf[0..4].copy_from_slice(&security_id.to_le_bytes());
    buf[4] = 1; // NSE_EQ
    // buf[5] padding
    buf[6..10].copy_from_slice(&ltp.to_le_bytes());
    // buf[10..12] last_trade_quantity = 0
    buf[12..16].copy_from_slice(&ts.to_le_bytes());
    // buf[16..24] received_at_nanos = 0
    // buf[24..28] average_traded_price = 0.0
    // buf[28..32] volume = 0
    // ... remaining fields all zeroed
    // f64::NAN bytes for iv/delta/gamma/theta/vega would match Default,
    // but zeroed is also valid — the recovery path doesn't gate on NaN.
    buf
}

/// B3: SIGKILL simulation — pre-write a spill file with N ticks, then
/// start a fresh writer pointing at the same directory and verify
/// recover_stale_spill_files() drains everything.
#[test]
#[ignore = "B3 chaos test — run manually with `-- --ignored`"]
fn chaos_sigkill_spill_replay_zero_loss() {
    let tmp_spill =
        std::env::temp_dir().join(format!("dlt-b3-chaos-sigkill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_spill);
    std::fs::create_dir_all(&tmp_spill).expect("create tmp spill dir"); // APPROVED: test

    // Pre-populate the spill file as if a previous process SIGKILLed
    // mid-stream. The file name must match the recovery scanner's
    // `ticks-YYYYMMDD.bin` pattern.
    let date = chrono::Utc::now().format("%Y%m%d");
    let spill_path = tmp_spill.join(format!("ticks-{date}.bin"));
    {
        let mut file = std::fs::File::create(&spill_path).expect("create spill file"); // APPROVED: test
        for i in 0..50_u32 {
            let record = build_spill_record(1000 + i, 100.0 + i as f32, 1_700_000_000 + i);
            file.write_all(&record).expect("write record"); // APPROVED: test
        }
        // Drop file with a sync_all — emulates the durable on-disk state
        // after the kernel has flushed the write() calls but before
        // whatever in-RAM userspace state would've been synced.
        file.sync_all().expect("sync"); // APPROVED: test
    }

    let file_len = std::fs::metadata(&spill_path).unwrap().len();
    assert_eq!(
        file_len,
        50 * TICK_SPILL_RECORD_SIZE as u64,
        "B3: pre-populated spill file must contain exactly 50 records"
    );

    // Now spawn a "fresh process" writer pointing at the same directory.
    let port = spawn_tcp_drain_server();
    let config = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        ilp_port: port,
        http_port: port,
        pg_port: port,
    };
    let mut writer =
        dhan_live_trader_storage::tick_persistence::TickPersistenceWriter::new(&config)
            .expect("new writer"); // APPROVED: test
    writer.set_spill_dir_for_test(tmp_spill.clone());

    // Run recovery. Must return the number of ticks drained.
    let recovered = writer.recover_stale_spill_files();
    assert_eq!(
        recovered, 50,
        "B3: SIGKILL replay must recover all 50 ticks from the stale spill file, got {recovered}"
    );

    // Spill file must be deleted after successful drain.
    assert!(
        !spill_path.exists(),
        "B3: spill file must be deleted after successful drain, still at {}",
        spill_path.display()
    );

    let _ = std::fs::remove_dir_all(&tmp_spill);
}

/// B3: Recovery is idempotent — running `recover_stale_spill_files` on an
/// already-empty dir returns 0 without error.
#[test]
#[ignore = "B3 chaos test — run manually with `-- --ignored`"]
fn chaos_sigkill_recovery_idempotent_on_empty_dir() {
    let tmp_spill =
        std::env::temp_dir().join(format!("dlt-b3-chaos-idempotent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_spill);
    std::fs::create_dir_all(&tmp_spill).expect("create"); // APPROVED: test

    let port = spawn_tcp_drain_server();
    let config = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        ilp_port: port,
        http_port: port,
        pg_port: port,
    };
    let mut writer =
        dhan_live_trader_storage::tick_persistence::TickPersistenceWriter::new(&config)
            .expect("new writer"); // APPROVED: test
    writer.set_spill_dir_for_test(tmp_spill.clone());

    let recovered = writer.recover_stale_spill_files();
    assert_eq!(
        recovered, 0,
        "B3: recovery on empty dir must be a no-op, got {recovered}"
    );

    let _ = std::fs::remove_dir_all(&tmp_spill);
}
