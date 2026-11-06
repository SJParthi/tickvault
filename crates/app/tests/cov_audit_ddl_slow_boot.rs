//! PR TG-1 (2026-11-06 live incident): the audit-table DDL against a QuestDB
//! that is slow or refusing at boot.
//!
//! On 2026-11-06 the box booted at 95% disk and QuestDB's single HTTP worker
//! was held by the disk-pressure archiver's `/exp` exports, so `/exec` DDL
//! and ILP `/write` waited or failed. Two properties keep the six order-side
//! and socket audit tables (`order_audit`, `pnl_audit`, `order_update_events`,
//! `position_update_events`, `order_leg_pnl`, `ws_event_audit`) correct in
//! that state, and these tests pin them against a local stand-in QuestDB:
//!
//! 1. **The ensures retry.** `candle_ddl_boot::ensure_until_keyed` with
//!    `DdlTables::AuditTables` re-runs every one of the six ensures (no
//!    short-circuit) until QuestDB confirms each DEDUP key, and each table's
//!    closing `DEDUP ENABLE UPSERT KEYS` carries `feed`.
//! 2. **DEDUP before the first ILP row (spill replay).** The audit spill
//!    drain never POSTs a spilled row to `/write` while the table's ensure is
//!    refused (the rows stay on disk), and once QuestDB accepts, the table's
//!    `DEDUP ENABLE` is answered before the first `/write`.
//!
//! Honest limit (recorded at `DdlTables::AuditTables`): a LIVE row written by
//! a consumer before the background re-run lands can still auto-create its
//! table without the key; only the spill-replay path is gated, and that is
//! what test 2 pins.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tickvault_app::candle_ddl_boot::{DdlTables, ensure_until_keyed};
use tickvault_common::config::QuestDbConfig;
use tickvault_storage::audit_spill::{AuditSpill, AuditSpillTable, spawn_audit_spill_drain_once};
use tickvault_storage::order_audit_persistence::ORDER_AUDIT_TABLE;
use tickvault_storage::order_leg_pnl_persistence::ORDER_LEG_PNL_TABLE;
use tickvault_storage::order_update_events_persistence::ORDER_UPDATE_EVENTS_TABLE;
use tickvault_storage::pnl_audit_persistence::PNL_AUDIT_TABLE;
use tickvault_storage::position_update_events_persistence::POSITION_UPDATE_EVENTS_TABLE;
use tickvault_storage::ws_event_audit_persistence::WS_EVENT_AUDIT_TABLE;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The six tables `DdlTables::AuditTables` ensures.
const AUDIT_TABLES: [&str; 6] = [
    ORDER_AUDIT_TABLE,
    PNL_AUDIT_TABLE,
    ORDER_UPDATE_EVENTS_TABLE,
    POSITION_UPDATE_EVENTS_TABLE,
    ORDER_LEG_PNL_TABLE,
    WS_EVENT_AUDIT_TABLE,
];

/// One request the stand-in QuestDB saw and the status it answered.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    /// Decoded path + query (the SQL for `/exec`).
    target: String,
    status: u16,
}

type Log = Arc<Mutex<Vec<Seen>>>;
/// Given the decoded request target and the log so far, the status to answer.
type Decide = Arc<dyn Fn(&str, &[Seen]) -> u16 + Send + Sync>;

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A keep-alive HTTP/1.1 stand-in for QuestDB on 127.0.0.1. Every request is
/// logged with the status `decide` chose; a refusal is answered after
/// `refuse_delay` (the busy single worker of the incident).
async fn spawn_questdb(decide: Decide, refuse_delay: Duration) -> (QuestDbConfig, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let accept_log = Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&accept_log);
            let decide = Arc::clone(&decide);
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    let header_end = loop {
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4;
                        }
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                    let mut lines = head.split("\r\n");
                    let mut first = lines.next().unwrap_or_default().split(' ');
                    let method = first.next().unwrap_or_default().to_string();
                    let target = url_decode(first.next().unwrap_or_default());
                    let content_length = lines
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let mut rest = buf.split_off(header_end);
                    while rest.len() < content_length {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => rest.extend_from_slice(&tmp[..n]),
                        }
                    }
                    buf = rest.split_off(content_length.min(rest.len()));
                    let status = {
                        let mut guard = log.lock().expect("log");
                        let status = decide(&target, &guard);
                        guard.push(Seen {
                            method,
                            target,
                            status,
                        });
                        status
                    };
                    if !(200..300).contains(&status) {
                        tokio::time::sleep(refuse_delay).await;
                    }
                    let body: &[u8] = if (200..300).contains(&status) {
                        b"{\"ddl\":\"OK\"}"
                    } else {
                        b"{\"error\":\"busy\"}"
                    };
                    let head = format!(
                        "HTTP/1.1 {status} Stub\r\ncontent-length: {}\r\n\r\n",
                        if status == 204 { 0 } else { body.len() }
                    );
                    if sock.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    if status != 204 && sock.write_all(body).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let cfg = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: port,
        pg_port: 1,
        ilp_port: 1,
    };
    (cfg, log)
}

fn dedup_statement(table: &str) -> String {
    format!("ALTER TABLE {table} DEDUP ENABLE UPSERT KEYS(")
}

/// Attempt number a request belongs to: how many `order_audit` CREATEs (the
/// first statement of every `AuditTables` attempt) the log holds.
fn attempts_started(log: &[Seen]) -> usize {
    let create = format!("CREATE TABLE IF NOT EXISTS {ORDER_AUDIT_TABLE} ");
    log.iter().filter(|s| s.target.contains(&create)).count()
}

/// QuestDB refuses (503, slowly) every statement of the first two attempts
/// of the six-table audit ensure, then accepts. The re-run must keep going
/// past the refusals, run all six ensures on EVERY attempt, and finish with
/// each table's DEDUP key (feed included) accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_regression_tg1_audit_ensures_retry_until_all_six_tables_are_keyed() {
    const REFUSED_ATTEMPTS: usize = 2;
    let decide: Decide = Arc::new(|target: &str, log: &[Seen]| {
        let create = format!("CREATE TABLE IF NOT EXISTS {ORDER_AUDIT_TABLE} ");
        let attempt = attempts_started(log) + usize::from(target.contains(&create));
        if attempt <= REFUSED_ATTEMPTS {
            503
        } else {
            200
        }
    });
    let (questdb, log) = spawn_questdb(decide, Duration::from_millis(5)).await;

    let quick = Duration::from_millis(20);
    let attempts = tokio::time::timeout(
        Duration::from_secs(120),
        ensure_until_keyed(&questdb, DdlTables::AuditTables, quick, quick * 4),
    )
    .await
    .expect("the re-run must finish once QuestDB accepts");
    assert_eq!(
        usize::try_from(attempts).expect("u32 fits"),
        REFUSED_ATTEMPTS + 1,
        "two refused attempts, then keyed"
    );

    let log = log.lock().expect("log").clone();
    for table in AUDIT_TABLES {
        let dedup = dedup_statement(table);
        let sent: Vec<&Seen> = log.iter().filter(|s| s.target.contains(&dedup)).collect();
        assert_eq!(
            sent.len(),
            REFUSED_ATTEMPTS + 1,
            "{table}: its DEDUP ENABLE is re-sent on every attempt (one refused \
             table never stops the others)"
        );
        assert!(
            sent[..REFUSED_ATTEMPTS].iter().all(|s| s.status == 503),
            "{table}: refused while QuestDB was busy"
        );
        let last = sent[REFUSED_ATTEMPTS];
        assert_eq!(last.status, 200, "{table}: keyed on the final attempt");
        let key = last
            .target
            .split_once("UPSERT KEYS(")
            .and_then(|(_, k)| k.split_once(')'))
            .map(|(k, _)| k)
            .unwrap_or_default();
        assert!(
            key.split(',').any(|c| c.trim() == "feed"),
            "{table}: the DEDUP key carries feed: {key}"
        );
    }
    assert!(
        log.iter().all(|s| !s.target.starts_with("/write")),
        "the ensure itself never writes a row"
    );
}

/// The spill tests drain the production spill folder, which is relative to
/// the working directory; move this test process into a fresh temporary
/// folder once so nothing touches the repository.
fn enter_temp_cwd() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "tv-tg1-audit-spill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp cwd");
        std::env::set_current_dir(&dir).expect("enter temp cwd");
        dir
    })
}

fn spill_files(table: AuditSpillTable) -> usize {
    std::fs::read_dir(table.default_dir())
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| e.path().is_file())
                .filter(|e| e.path().extension().is_none_or(|x| x != "tmp"))
                .count()
        })
        .unwrap_or(0)
}

async fn wait_until(what: &str, deadline: Duration, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// QuestDB refuses every DDL (the slow boot): the `order_audit` spill drain
/// runs the table's ensure, sees the DEDUP key refused, and sends NOTHING to
/// `/write`; the spilled row stays on disk for the next round.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_regression_tg1_spill_drain_writes_nothing_while_the_dedup_key_is_refused() {
    let _cwd = enter_temp_cwd();
    let table = AuditSpillTable::OrderAudit;
    let refusals = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&refusals);
    let decide: Decide = Arc::new(move |target: &str, _log: &[Seen]| {
        if target.starts_with("/exec") {
            counter.fetch_add(1, Ordering::SeqCst);
            503
        } else {
            204
        }
    });
    let (questdb, log) = spawn_questdb(decide, Duration::from_millis(5)).await;

    AuditSpill::for_table(table)
        .spill(b"order_audit,feed=dhan detail=\"tg1\" 1\n", 1)
        .expect("spill one row");
    assert_eq!(spill_files(table), 1);
    assert!(spawn_audit_spill_drain_once(table, &questdb));

    let dedup = dedup_statement(table.table_name());
    wait_until(
        "the drain's ensure to reach DEDUP",
        Duration::from_secs(20),
        || {
            log.lock()
                .expect("log")
                .iter()
                .any(|s| s.target.contains(&dedup))
        },
    )
    .await;
    // Give the round time to (wrongly) replay after the refused ensure.
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    let log = log.lock().expect("log").clone();
    assert!(refusals.load(Ordering::SeqCst) > 0);
    assert!(
        log.iter()
            .all(|s| !(s.method == "POST" && s.target.starts_with("/write"))),
        "no row may reach /write before the table is keyed: {log:?}"
    );
    assert_eq!(
        spill_files(table),
        1,
        "the spilled row stays on disk for the next round"
    );
}

/// QuestDB accepts: the `pnl_audit` spill drain keys the table first and only
/// then replays the spilled row, which is then removed from disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_regression_tg1_spill_drain_keys_the_table_before_the_first_write() {
    let _cwd = enter_temp_cwd();
    let table = AuditSpillTable::PnlAudit;
    let decide: Decide = Arc::new(|target: &str, _log: &[Seen]| {
        if target.starts_with("/write") {
            204
        } else {
            200
        }
    });
    let (questdb, log) = spawn_questdb(decide, Duration::ZERO).await;

    AuditSpill::for_table(table)
        .spill(b"pnl_audit,feed=dhan detail=\"tg1\" 1\n", 1)
        .expect("spill one row");
    assert!(spawn_audit_spill_drain_once(table, &questdb));

    wait_until(
        "the spilled row to be replayed",
        Duration::from_secs(20),
        || spill_files(table) == 0,
    )
    .await;

    let log = log.lock().expect("log").clone();
    let dedup = dedup_statement(table.table_name());
    let keyed_at = log
        .iter()
        .position(|s| s.target.contains(&dedup) && s.status == 200)
        .expect("the table's DEDUP key was accepted");
    let first_write = log
        .iter()
        .position(|s| s.method == "POST" && s.target.starts_with("/write"))
        .expect("the spilled row was written");
    assert!(
        keyed_at < first_write,
        "DEDUP ENABLE (request {keyed_at}) must be answered before the first /write \
         (request {first_write})"
    );
}
