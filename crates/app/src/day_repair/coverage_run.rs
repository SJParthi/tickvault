//! The packet coverage check of the one-off 2026-11-20 day repair, as one
//! function over a copy store and QuestDB's `/exp` (so it is tested end to
//! end against a fake store and a stub server; the box runs it over the cold
//! bucket). Added 2026-11-21 after the first verify; hardened by two hostile
//! reviews the same night.
//!
//! It refuses unless every packet the repair dropped from `ticks`,
//! `tick_quotes` and `feed_aux_packets` is in the rebuilt day:
//! 1. every quarantine copy of each dropped partition is read (the recorded
//!    first copy and any drop-time re-export at a `.sha-` key), the recorded
//!    one at exactly its recorded row count;
//! 2. each dropped `capture_seq` is looked up in the rebuilt day;
//! 3. STRICT: every dropped packet must be there under its own number, the
//!    Quote 61 repeat-skip kinds included, and every contract that had a
//!    repeat-kind row keeps one. Nothing is excused: three hostile reviews
//!    each found a way an accepted look-alike could hide a lost packet;
//! 4. for the report only, a packet missing by number is looked up as an
//!    exact copy under another number (the backup socket's copy of the same
//!    vendor packet), and live out-of-window aux rows are counted, so a
//!    refusal says why.
//!
//! Cold, offline, once per verify: O(bytes) per copy read (twice when any
//! packet is missing), O(n log n) to sort the numbers. Deleted with the rest
//! of the one-off tool.

use std::path::Path;

use anyhow::Context as _;
use tickvault_storage::day_repair_coverage as cov;

use super::CopyRecord;

/// Where the quarantine copies live: the cold bucket on the box, a folder in
/// the tests.
pub(crate) trait CopyStore {
    /// Every key under `prefix`.
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>>;
    /// Writes object `key` to `path` (a new file). Returns its bytes.
    async fn fetch(&self, key: &str, path: &Path) -> anyhow::Result<u64>;
}

/// The cold bucket.
pub(crate) struct S3Store(pub tickvault_storage::s3_cold::S3Cold);

impl CopyStore for S3Store {
    // TEST-EXEMPT: one paginated ListObjectsV2 through day_repair_coverage::list_copy_keys.
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        cov::list_copy_keys(&self.0, prefix).await
    }
    // TEST-EXEMPT: one GetObject through day_repair_coverage::download_copy.
    async fn fetch(&self, key: &str, path: &Path) -> anyhow::Result<u64> {
        cov::download_copy(&self.0, key, path).await
    }
}

/// The repair's trade-time shift (IST minus UTC) in µs: the copies hold the
/// exchange time 5 h 30 m early, the rebuild holds it corrected.
const SHIFT_MICROS: i64 = (tickvault_common::constants::IST_UTC_OFFSET_SECONDS as i64) * 1_000_000;

/// Fetches `key` to a fresh `path`, reads it on the blocking pool with
/// `read`, removes the file. O(bytes).
async fn read_copy<S: CopyStore, T: Send + 'static>(
    store: &S,
    key: &str,
    path: &Path,
    acc: T,
    read: impl FnOnce(&Path, T) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    // The staging folder holds only this run's files.
    let _stale = std::fs::remove_file(path);
    store
        .fetch(key, path)
        .await
        .with_context(|| format!("download {key}"))?;
    let read_path = path.to_path_buf();
    let acc = tokio::task::spawn_blocking(move || read(&read_path, acc))
        .await
        .context("the copy reader stopped")??;
    std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    Ok(acc)
}

/// The coverage check. `window` is asked before every long step and stops
/// the run when it errors. Returns the findings (empty = every dropped packet
/// is in the rebuilt day).
///
/// # Errors
/// A listing, download, read or export failed, or the window closed.
pub(crate) async fn run_coverage<S: CopyStore>(
    store: &S,
    client: &reqwest::Client,
    exp_url: &str,
    day_text: &str,
    staging: &Path,
    copies: &[(String, CopyRecord)],
    mut window: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<Vec<String>> {
    std::fs::create_dir_all(staging).with_context(|| format!("create {}", staging.display()))?;
    let mut findings = Vec::new();
    // 1. Every copy in the store of each dropped partition: the recorded
    //    first copy AND any drop-time re-export that landed other bytes at a
    //    `.sha-` key. The drop re-exports and drops against that newer copy,
    //    which the progress file does not record, so the rows dropped can be
    //    more than the first copy held. Expected rows: the recorded copy only.
    let mut objects: Vec<(String, String, Option<u64>)> = Vec::new();
    for (object, rec) in copies {
        window()?;
        let (table, partition) = object
            .split_once('/')
            .with_context(|| format!("{object} is not table/partition"))?;
        let prefix = cov::partition_copy_prefix(table, partition);
        let listed = cov::copies_under_prefix(
            &store
                .list(&prefix)
                .await
                .with_context(|| format!("list the copies of {object}"))?,
            &prefix,
        );
        if !listed.contains(&rec.key) {
            findings.push(format!(
                "the recorded copy {} of {object} is not in the bucket",
                rec.key
            ));
            continue;
        }
        objects.push((object.clone(), rec.key.clone(), Some(rec.rows)));
        for key in listed.into_iter().filter(|k| *k != rec.key) {
            tracing::info!(object = %object, key = %key, "day repair: a second copy of a dropped partition is read too");
            objects.push((object.clone(), key, None));
        }
    }
    // Every quarantine copy of the three tables must belong to a partition
    // the progress record lists: a progress file lost or replaced between
    // runs would otherwise leave earlier-dropped partitions unread
    // (2026-11-21 strict check).
    let recorded: std::collections::BTreeSet<&str> =
        copies.iter().map(|(o, _)| o.as_str()).collect();
    for table in super::TICK_PATH_TABLES {
        window()?;
        let prefix = cov::table_copy_prefix(table);
        let listed = store
            .list(&prefix)
            .await
            .with_context(|| format!("list the copies of {table}"))?;
        let mut unrecorded = std::collections::BTreeSet::new();
        for key in &listed {
            match cov::partition_of_copy_key(table, key) {
                Some(p) if recorded.contains(format!("{table}/{p}").as_str()) => {}
                Some(p) => {
                    unrecorded.insert(format!("{table}/{p}"));
                }
                None => {
                    unrecorded.insert(key.clone());
                }
            }
        }
        if !unrecorded.is_empty() {
            findings.push(format!(
                "the quarantine holds {} {table} copies the progress record does not list: {:?}",
                unrecorded.len(),
                unrecorded.iter().take(10).collect::<Vec<_>>()
            ));
        }
    }
    let expected_rows: u64 = copies.iter().map(|(_, r)| r.rows).sum();
    let mut dropped = cov::CopySeqs {
        // Sized once: no doubling past ~0.5 GB on the box.
        checked: Vec::with_capacity(usize::try_from(expected_rows).unwrap_or(0)),
        ..cov::CopySeqs::default()
    };
    for (i, (object, key, want_rows)) in objects.iter().enumerate() {
        window()?;
        let rows_before = dropped.rows;
        let path = staging.join(format!("coverage-{i}.csv.gz"));
        dropped = read_copy(store, key, &path, dropped, |p, mut acc| {
            cov::read_copy_gz(p, &mut acc).map(|()| acc)
        })
        .await
        .with_context(|| format!("read {key} ({object})"))?;
        // The recorded copy must read the rows its record says: the object
        // read is then the copy the repair verified.
        let read = dropped.rows.saturating_sub(rows_before);
        if let Some(want) = want_rows
            && read != *want
        {
            findings.push(format!(
                "the copy {key} of {object} read {read} rows, its record says {want}"
            ));
        }
    }
    let rows = dropped.rows;
    // 2. The rebuilt day's numbers.
    let mut rebuilt = Vec::with_capacity(usize::try_from(expected_rows).unwrap_or(0));
    for table in super::TICK_PATH_TABLES {
        window()?;
        let sql = format!("SELECT capture_seq FROM {table} WHERE ts IN '{day_text}'");
        super::rebuild::export_seqs(client, exp_url, &sql, &mut rebuilt).await?;
    }
    // 4. A bound on the repeat-kind exemption: every contract that had a
    //    `no_trade_today` / `connect_snapshot` row keeps at least one.
    let mut rebuilt_contracts = std::collections::BTreeSet::new();
    let kinds = cov::REPEAT_SKIP_KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ");
    window()?;
    let sql = format!(
        "SELECT DISTINCT security_id, segment FROM feed_aux_packets WHERE ts IN '{day_text}' \
         AND kind IN ({kinds})"
    );
    super::rebuild::export_contracts(client, exp_url, &sql, &mut rebuilt_contracts).await?;
    let repeat_contracts = dropped.repeat_contracts.len();
    let lost_contracts: Vec<String> = dropped
        .repeat_contracts
        .difference(&rebuilt_contracts)
        .cloned()
        .collect();
    if !lost_contracts.is_empty() {
        findings.push(format!(
            "{} contracts had a no-trade-today row before and none in the rebuild (first: {:?})",
            lost_contracts.len(),
            lost_contracts.iter().take(10).collect::<Vec<_>>()
        ));
    }
    let (need, repeat, have, missing_list, repeat_missing) =
        tokio::task::spawn_blocking(move || {
            let mut need = dropped.checked;
            let mut repeat = dropped.repeat_kind;
            cov::sort_dedup(&mut need);
            cov::sort_dedup(&mut repeat);
            cov::sort_dedup(&mut rebuilt);
            let (_, missing_list) = cov::missing_from(&need, &rebuilt, usize::MAX);
            let (repeat_missing, _) = cov::missing_from(&repeat, &rebuilt, 0);
            (need, repeat, rebuilt.len(), missing_list, repeat_missing)
        })
        .await
        .context("the coverage compare stopped")?;
    let (checked, repeat_n, missing) = (need.len(), repeat.len(), missing_list.len());
    let missing_sample: Vec<u64> = missing_list.iter().take(10).copied().collect();
    // 3. A packet missing by number may be in the rebuilt day under the other
    //    number of the same vendor packet: a contract on the backup socket
    //    arrives twice, and the live drain and the replay can keep different
    //    copies.
    let (mut matched, mut unmatched) = (0u64, Vec::new());
    // How many unmatched packets were live `out_of_window_tick` aux rows (a
    // tick live filed in the aux table because of the wrong time, which the
    // rebuild files in a tick table; the copy match never crosses families).
    let mut unmatched_out_of_window = 0u64;
    if !missing_list.is_empty() {
        let missing_sorted = std::sync::Arc::new(missing_list);
        let mut contents = std::collections::BTreeMap::new();
        for (i, (object, key, _)) in objects.iter().enumerate() {
            window()?;
            let path = staging.join(format!("coverage-missing-{i}.csv.gz"));
            let wanted = std::sync::Arc::clone(&missing_sorted);
            contents = read_copy(store, key, &path, contents, move |p, mut acc| {
                cov::read_missing_contents_gz(p, &wanted, SHIFT_MICROS, &mut acc).map(|()| acc)
            })
            .await
            .with_context(|| format!("read {key} ({object}) again"))?;
        }
        if contents.len() != missing_sorted.len() {
            findings.push(format!(
                "{} of {} missing packets were not found again in their copies",
                missing_sorted.len().saturating_sub(contents.len()),
                missing_sorted.len()
            ));
        }
        let mut dropped_all: Vec<u64> = need;
        dropped_all.extend_from_slice(&repeat);
        cov::sort_dedup(&mut dropped_all);
        let sids: std::collections::BTreeSet<i64> =
            contents.values().map(|m| m.shifted.sid).collect();
        let sids: Vec<i64> = sids.into_iter().collect();
        let mut spare = std::collections::BTreeMap::new();
        for table in super::TICK_PATH_TABLES {
            for chunk in sids.chunks(500) {
                window()?;
                let list = chunk
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "SELECT * FROM {table} WHERE ts IN '{day_text}' AND security_id IN ({list})"
                );
                super::rebuild::export_spares(client, exp_url, &sql, &dropped_all, &mut spare)
                    .await?;
            }
        }
        (matched, unmatched) = cov::match_spares(&contents, &mut spare);
        unmatched_out_of_window = unmatched
            .iter()
            .filter_map(|s| contents.get(s))
            .filter(|m| {
                m.shifted.class == cov::RowClass::Aux
                    && m.shifted.values.first().map(String::as_str) == Some("out_of_window_tick")
            })
            .count() as u64;
        // A packet not found again in its copies stays unmatched.
        for seq in missing_sorted.iter() {
            if !contents.contains_key(seq) {
                unmatched.push(*seq);
            }
        }
    }
    let unmatched_n = unmatched.len();
    tracing::info!(
        copies = copies.len(),
        objects_read = objects.len(),
        dropped_rows = rows,
        dropped_packets_checked = checked,
        repeat_kind_packets = repeat_n,
        repeat_kind_not_rebuilt = repeat_missing,
        rebuilt_packets = have,
        missing,
        matched_by_content = matched,
        unmatched = unmatched_n,
        "day repair: packet coverage of the dropped copies"
    );
    super::say(&format!(
        "coverage: {} copies read ({} recorded), {rows} dropped rows, {checked} distinct packets \
         checked, {missing} missing by number ({matched} with an exact copy under another number, \
         {unmatched_n} without, {unmatched_out_of_window} of those live out-of-window aux rows); \
         {repeat_n} repeat-kind packets, {repeat_missing} of them not rebuilt; {repeat_contracts} \
         repeat-kind contracts, {} without a rebuilt row; {have} distinct packets rebuilt",
        objects.len(),
        copies.len(),
        lost_contracts.len()
    ));
    // STRICT (2026-11-21, after three hostile reviews): every dropped packet
    // must be in the rebuilt day under its own number, repeat kinds
    // included. The copy match and the repeat-skip counts above are
    // REPORTED, never accepted: each round of review found another way an
    // accepted look-alike could hide a lost packet, so nothing is excused.
    if missing > 0 {
        let mut first = missing_sample;
        first.truncate(10);
        findings.push(format!(
            "{missing} dropped packets are not in the rebuilt day under their own number \
             ({matched} of them have an exact copy under another number, \
             {unmatched_n} have none, {unmatched_out_of_window} of those were live \
             out-of-window aux rows; first capture_seq: {first:?})"
        ));
    }
    if repeat_missing > 0 {
        findings.push(format!(
            "{repeat_missing} dropped no-trade-today / connect-snapshot packets are not in the \
             rebuilt day (the repeat skip is not accepted)"
        ));
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// A gzip of `data` with stored (uncompressed) deflate blocks.
    fn gzip_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
        let mut chunks = data.chunks(65_535).peekable();
        if chunks.peek().is_none() {
            out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
        }
        while let Some(c) = chunks.next() {
            out.push(u8::from(chunks.peek().is_none()));
            let len = u16::try_from(c.len()).expect("len");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(c);
        }
        out.extend_from_slice(&super::super::state::crc32_ieee(data).to_le_bytes());
        out.extend_from_slice(&u32::try_from(data.len()).expect("size").to_le_bytes());
        out
    }

    #[derive(Default)]
    struct FakeStore {
        objects: BTreeMap<String, Vec<u8>>,
    }

    impl CopyStore for FakeStore {
        async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(self
                .objects
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }
        async fn fetch(&self, key: &str, path: &Path) -> anyhow::Result<u64> {
            let bytes = self
                .objects
                .get(key)
                .with_context(|| format!("no object {key}"))?;
            std::fs::write(path, bytes)?;
            Ok(bytes.len() as u64)
        }
    }

    fn key(table: &str, partition: &str, suffix: &str) -> String {
        format!("{}{suffix}", cov::partition_copy_prefix(table, partition))
    }

    fn decode(q: &str) -> String {
        let b = q.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < b.len() => {
                    let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00");
                    out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                    i += 2;
                }
                c => out.push(c),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// What the stub answers to an unscripted query: the table's header
    /// and no rows for a `SELECT *`, else a one-column header.
    fn default_body(query: &str) -> String {
        if query.contains("SELECT * FROM feed_aux_packets") {
            AUX_HEADER.to_owned()
        } else if query.contains("SELECT *") {
            TICK_HEADER.to_owned()
        } else {
            String::from("x\r\n")
        }
    }

    /// An `/exp` stub: the first `(needle, body)` whose needle the query
    /// contains answers; otherwise a header-only CSV.
    async fn exp_stub(script: Vec<(&'static str, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut part = [0u8; 4096];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut part).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&part[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&req).into_owned();
                let line = head.lines().next().unwrap_or_default().to_owned();
                let query = line
                    .split("query=")
                    .nth(1)
                    .and_then(|q| q.split([' ', '&']).next())
                    .map(decode)
                    .unwrap_or_default();
                log.lock().expect("lock").push(query.clone());
                let body = script
                    .iter()
                    .find(|(n, _)| query.contains(n))
                    .map_or_else(|| default_body(&query), |(_, b)| b.clone());
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (format!("http://{addr}/exp"), seen)
    }

    /// The real `ticks` columns (its `ts` is the exchange time).
    const TICK_HEADER: &str = "\"ts\",\"security_id\",\"segment\",\"ltp\",\"open\",\"high\",\"low\",\"close\",\"volume\",\"oi\",\"oi_day_high\",\"oi_day_low\",\"avg_price\",\"last_trade_qty\",\"total_buy_qty\",\"total_sell_qty\",\"received_at\",\"capture_seq\"\n";
    /// The real `feed_aux_packets` columns.
    const AUX_HEADER: &str = "ts,received_at,feed,segment,kind,contract,security_id,oi,prev_close,prev_oi,ltp,exchange_ts,volume,day_open,day_high,day_low,day_close,oi_day_high,oi_day_low,reason_code,capture_seq\n";

    /// One `ticks` row at exchange time `ts`.
    fn trow(ts: &str, sid: i64, ltp: &str, vol: i64, seq: u64) -> String {
        format!(
            "{ts},{sid},NSE_FNO,{ltp},1,2,0.5,1.5,{vol},,,,1.25,75,40,9,2026-11-20T09:31:00.000000Z,{seq}\n"
        )
    }

    /// One `feed_aux_packets` row.
    fn arow(sid: i64, seg: &str, kind: &str, oi: &str, seq: u64) -> String {
        format!(
            "2026-11-20T09:00:00.000000Z,2026-11-20T09:00:00.000000Z,dhan,{seg},{kind},c,{sid},{oi},,,1.0,,,,,,,,,,{seq}\n"
        )
    }

    /// One dropped `ticks` hour (packets 1..=3, packet 3 at 04:00 UTC-shifted)
    /// and one dropped aux day (packet 4 a repeat kind of contract 9, packet
    /// 5 an open-interest row).
    fn dropped_store() -> (FakeStore, Vec<(String, CopyRecord)>) {
        let ticks = format!(
            "{TICK_HEADER}{}{}{}",
            trow("2026-11-20T03:45:00.000000Z", 7, "5.5", 100, 1),
            trow("2026-11-20T03:45:01.000000Z", 7, "5.6", 101, 2),
            trow("2026-11-20T04:00:00.000000Z", 8, "9.0", 7, 3),
        );
        let aux = format!(
            "{AUX_HEADER}{}{}",
            arow(9, "NSE_EQ", "no_trade_today", "", 4),
            arow(7, "NSE_FNO", "oi", "55", 5),
        );
        let mut store = FakeStore::default();
        let tk = key("ticks", "2026-11-20T09", "csv.gz");
        let ak = key("feed_aux_packets", "2026-11-20", "csv.gz");
        store
            .objects
            .insert(tk.clone(), gzip_stored(ticks.as_bytes()));
        store
            .objects
            .insert(ak.clone(), gzip_stored(aux.as_bytes()));
        let rec = |k: &str, rows| CopyRecord {
            key: k.to_owned(),
            sha256: "ab".repeat(32),
            rows,
            bytes: 1,
        };
        let copies = vec![
            ("ticks/2026-11-20T09".to_owned(), rec(&tk, 3)),
            ("feed_aux_packets/2026-11-20".to_owned(), rec(&ak, 2)),
        ];
        (store, copies)
    }

    fn staging(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tv-cov-run-{name}-{}", std::process::id()))
    }

    async fn run(
        store: &FakeStore,
        copies: &[(String, CopyRecord)],
        script: Vec<(&'static str, String)>,
        name: &str,
    ) -> (anyhow::Result<Vec<String>>, Vec<String>) {
        let (url, seen) = exp_stub(script).await;
        let dir = staging(name);
        let got = run_coverage(
            store,
            &reqwest::Client::new(),
            &url,
            "2026-11-20",
            &dir,
            copies,
            || Ok(()),
        )
        .await;
        std::fs::remove_dir_all(&dir).ok();
        let queries = seen.lock().expect("lock").clone();
        (got, queries)
    }

    fn seqs(v: &[u64]) -> String {
        let mut s = String::from("capture_seq\r\n");
        for x in v {
            s.push_str(&format!("{x}\r\n"));
        }
        s
    }

    #[test]
    fn test_run_coverage_shift_is_the_ist_offset() {
        assert_eq!(SHIFT_MICROS, 19_800_000_000);
    }

    #[tokio::test]
    async fn test_run_coverage_passes_when_every_dropped_packet_is_rebuilt() {
        let (store, copies) = dropped_store();
        let (got, queries) = run(
            &store,
            &copies,
            vec![
                (
                    "SELECT DISTINCT",
                    String::from("security_id,segment\r\n9,NSE_EQ\r\n"),
                ),
                ("SELECT capture_seq FROM ticks", seqs(&[1, 2])),
                ("SELECT capture_seq FROM tick_quotes", seqs(&[3])),
                ("SELECT capture_seq FROM feed_aux_packets", seqs(&[4, 5])),
            ],
            "clean",
        )
        .await;
        assert_eq!(got.expect("ran"), Vec::<String>::new());
        assert!(
            queries.iter().all(|q| !q.contains("security_id IN")),
            "{queries:?}"
        );
    }

    #[tokio::test]
    async fn test_run_coverage_refuses_a_backup_copy_under_another_number_and_reports_it() {
        let (store, copies) = dropped_store();
        // Packet 3 rebuilt as 99 (the other socket's copy), time corrected.
        let spare = format!(
            "{TICK_HEADER}{}",
            trow("2026-11-20T09:30:00.000000Z", 8, "9.0", 7, 99)
        );
        let (got, queries) = run(
            &store,
            &copies,
            vec![
                (
                    "SELECT DISTINCT",
                    String::from("security_id,segment\r\n9,NSE_EQ\r\n"),
                ),
                ("SELECT * FROM ticks", spare),
                ("SELECT capture_seq FROM ticks", seqs(&[1, 2, 99])),
                ("SELECT capture_seq FROM feed_aux_packets", seqs(&[4, 5])),
            ],
            "spare",
        )
        .await;
        // Strict: the exact copy under number 99 is reported, not accepted.
        let findings = got.expect("ran");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("1 dropped packets"), "{findings:?}");
        assert!(
            findings[0].contains("1 of them have an exact copy"),
            "{findings:?}"
        );
        assert!(findings[0].contains("0 have none"), "{findings:?}");
        assert!(
            queries.iter().any(|q| q.contains("security_id IN (8)")),
            "{queries:?}"
        );
    }

    #[tokio::test]
    async fn test_run_coverage_refuses_a_lost_packet_a_lost_repeat_contract_and_a_wrong_row_count()
    {
        let (store, mut copies) = dropped_store();
        copies[0].1.rows = 4;
        let (got, _) = run(
            &store,
            &copies,
            vec![
                ("SELECT DISTINCT", String::from("security_id,segment\r\n")),
                ("SELECT capture_seq FROM ticks", seqs(&[1, 2])),
                ("SELECT capture_seq FROM feed_aux_packets", seqs(&[5])),
            ],
            "lost",
        )
        .await;
        let findings = got.expect("ran");
        assert_eq!(findings.len(), 4, "{findings:?}");
        assert!(findings[0].contains("read 3 rows, its record says 4"));
        assert!(findings[1].contains("no-trade-today row before and none"));
        assert!(findings[2].contains("1 dropped packets are not in the rebuilt day"));
        assert!(findings[2].contains("0 of them have an exact copy"));
        assert!(findings[2].contains("[3]"));
        // The repeat-kind packet 4 is not rebuilt: not excused.
        assert!(
            findings[3].contains("1 dropped no-trade-today"),
            "{findings:?}"
        );
    }

    #[tokio::test]
    async fn test_run_coverage_reads_a_drop_time_second_copy_and_refuses_its_extra_packet() {
        let (mut store, copies) = dropped_store();
        // The drop re-export landed other bytes holding one more packet (6).
        let second = format!(
            "{TICK_HEADER}{}",
            trow("2026-11-20T04:05:00.000000Z", 7, "6.0", 120, 6)
        );
        store.objects.insert(
            key("ticks", "2026-11-20T09", "sha-00ff.csv.gz"),
            gzip_stored(second.as_bytes()),
        );
        // Another partition's copy under a near prefix is not read.
        store.objects.insert(
            key("ticks", "2026-11-20T10", "csv.gz"),
            b"not gzip".to_vec(),
        );
        let (got, _) = run(
            &store,
            &copies,
            vec![
                (
                    "SELECT DISTINCT",
                    String::from("security_id,segment\r\n9,NSE_EQ\r\n"),
                ),
                ("SELECT capture_seq FROM ticks", seqs(&[1, 2, 3])),
                ("SELECT capture_seq FROM feed_aux_packets", seqs(&[4, 5])),
            ],
            "second",
        )
        .await;
        let findings = got.expect("ran");
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(
            findings[0].contains("does not list: [\"ticks/2026-11-20T10\"]"),
            "{findings:?}"
        );
        assert!(findings[1].contains("[6]"), "{findings:?}");
    }

    #[tokio::test]
    async fn test_run_coverage_refuses_a_recorded_copy_missing_from_the_store() {
        let (mut store, copies) = dropped_store();
        store.objects.remove(&copies[1].1.key);
        let (got, _) = run(
            &store,
            &copies,
            vec![
                ("SELECT DISTINCT", String::from("security_id,segment\r\n")),
                ("SELECT capture_seq FROM ticks", seqs(&[1, 2, 3])),
            ],
            "gone",
        )
        .await;
        let findings = got.expect("ran");
        assert!(findings[0].contains("is not in the bucket"), "{findings:?}");
    }

    #[tokio::test]
    async fn test_run_coverage_stops_when_the_window_closes() {
        let (store, copies) = dropped_store();
        let (url, _) = exp_stub(Vec::new()).await;
        let dir = staging("window");
        let got = run_coverage(
            &store,
            &reqwest::Client::new(),
            &url,
            "2026-11-20",
            &dir,
            &copies,
            || anyhow::bail!("window closed"),
        )
        .await;
        std::fs::remove_dir_all(&dir).ok();
        assert!(format!("{:#}", got.expect_err("closed")).contains("window closed"));
    }
}
