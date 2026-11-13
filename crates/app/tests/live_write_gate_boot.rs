//! B1 (2026-11-13): the boot's live-table DDL step arms the live writers'
//! table gate BEFORE its first ensure, so no live row can reach a table ILP
//! would create without its key.
//!
//! A behaviour test, in its own binary because arming is process-wide and
//! never undone: a QuestDB stand-in reads the gate the moment the FIRST DDL
//! statement arrives. Armed first, the gate reads shut then (nothing keyed
//! yet). Armed after (or not at all), it reads open, which is the window in
//! which a live flush could have let QuestDB auto-create the table.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use tickvault_common::config::QuestDbConfig;
use tickvault_storage::ilp_table_gate::{GatedTable, LIVE_TICK_TABLES, live_tables_writable};

/// Answers 200 to every statement and records, for the first request only,
/// whether the live gate let a tick batch through at that instant.
fn accept_everything_recording_the_gate() -> (u16, Arc<Mutex<Option<bool>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let first = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&first);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if let Ok(mut slot) = seen.lock()
                && slot.is_none()
            {
                *slot = Some(live_tables_writable(&LIVE_TICK_TABLES));
            }
            let mut request = Vec::new();
            let mut buf = [0u8; 8_192];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        }
    });
    (port, first)
}

#[tokio::test]
async fn test_boot_live_ddl_arms_the_write_gate_before_its_first_ensure() {
    assert!(
        live_tables_writable(&LIVE_TICK_TABLES),
        "unarmed before the boot step: a process without it posts as before"
    );
    let (port, first) = accept_everything_recording_the_gate();
    let questdb = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: port,
        pg_port: 1,
        ilp_port: 1,
    };
    assert!(tickvault_app::candle_ddl_boot::run_live_table_ddl_at_boot(&questdb).await);
    let at_first_ddl = first.lock().expect("lock").expect("a DDL arrived");
    assert!(
        !at_first_ddl,
        "the gate must already be armed (and shut) when the first ensure statement runs"
    );
    assert!(
        live_tables_writable(&LIVE_TICK_TABLES) && live_tables_writable(&[GatedTable::MarketDepth]),
        "every live table keyed by the accepted ensures, so the gate is open"
    );
}
