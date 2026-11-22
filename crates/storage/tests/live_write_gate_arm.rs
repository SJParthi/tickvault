//! B1 (2026-11-13): the live writers' table gate, in a process of its own.
//!
//! Arming the gate is process-wide and never undone, so it is exercised here,
//! in its own test binary, where no other test posts through a live writer.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tickvault_common::config::QuestDbConfig;
use tickvault_storage::ilp_table_gate::{
    GatedTable, LIVE_TICK_TABLES, arm_live_write_gate, live_tables_writable,
};

/// A local QuestDB `/exec` stand-in that answers 200 to every statement
/// except a column self-heal (`ADD COLUMN`), which it refuses with a 500, as
/// QuestDB does for a column that exists with another type. Counts the
/// refusals so the test can prove the refused path really ran.
fn refuse_column_self_heals() -> (u16, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let refused = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&refused);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut buf = [0u8; 8_192];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let line = String::from_utf8_lossy(&request).to_ascii_uppercase();
            let is_column = line.contains("ADD+COLUMN") || line.contains("ADD%20COLUMN");
            let reply: &[u8] = if is_column {
                seen.fetch_add(1, Ordering::SeqCst);
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
            };
            let _ = stream.write_all(reply);
        }
    });
    (port, refused)
}

/// Before arming, a live batch posts (a process that never ran the boot DDL
/// step, a unit test against a mock server); once armed it waits until every
/// table it can hold rows for has been keyed, table by table.
///
/// Regression, B1 review (2026-11-13): the keyed verdict rests on the
/// key-bearing statements only (CREATE, DEDUP ENABLE). Against a QuestDB that
/// refuses every column self-heal, each ensure returns `false` (so the boot
/// still retries it) and the gate still OPENS. Before the fix the gate needed
/// every statement, so one refused `ADD COLUMN` on an already-keyed table shut
/// the live tick writer and the spill replay for the whole session.
#[test]
fn test_arm_live_write_gate_then_live_tables_writable_waits_for_every_keyed_ensure() {
    assert!(
        live_tables_writable(&LIVE_TICK_TABLES),
        "unarmed: posts as before"
    );
    arm_live_write_gate();
    assert!(
        !live_tables_writable(&LIVE_TICK_TABLES),
        "armed, nothing keyed"
    );

    let (port, refused) = refuse_column_self_heals();
    let questdb = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: port,
        pg_port: 1,
        ilp_port: 1,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let ticks_all = rt.block_on(tickvault_storage::tick_persistence::ensure_ticks_table(
        &questdb,
    ));
    assert!(
        !ticks_all,
        "a refused self-heal still reads as not all accepted"
    );
    assert!(refused.load(Ordering::SeqCst) > 0, "the refusal path ran");
    assert!(
        !live_tables_writable(&LIVE_TICK_TABLES),
        "feed_aux_packets shares the batch, so it must be keyed too"
    );
    let aux_all =
        rt.block_on(tickvault_storage::feed_aux_persistence::ensure_feed_aux_table(&questdb));
    assert!(!aux_all);
    assert!(
        live_tables_writable(&LIVE_TICK_TABLES),
        "CREATE and DEDUP ENABLE were accepted, so the tick tables are keyed"
    );

    assert!(
        !live_tables_writable(&[GatedTable::MarketDepth]),
        "each table is judged on its own ensure"
    );
    let depth_all =
        rt.block_on(tickvault_storage::depth_persistence::ensure_market_depth_table(&questdb));
    assert!(!depth_all);
    assert!(live_tables_writable(&[GatedTable::MarketDepth]));
}
