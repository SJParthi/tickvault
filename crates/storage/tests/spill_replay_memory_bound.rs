//! Does the streaming spill replay ACTUALLY hold its memory bound on a file
//! far larger than RAM allows?
//!
//! # Why this test exists
//!
//! On 2026-09-03 the production app restart-looped from 08:31 to 09:02 IST.
//! `replay_one_file` did `std::fs::read(&path)` on a **20.21 GB** spill file:
//! RSS reached 20.96 GB, `MemoryHigh` throttled the process, the throttled
//! process missed its watchdog ping, and systemd killed it. Four cycles.
//!
//! The fix streams the file through a fixed `REPLAY_STREAM_BUFFER_BYTES`
//! window. Every existing test for it is a *unit* test on the surrounding
//! logic or a *source scan* asserting the whole-file read is gone. **Not one
//! of them runs the replay against a large file and measures memory** — so
//! the central claim, "peak memory is now the buffer, not the file", was
//! argued rather than observed. That gap is exactly how the original defect
//! survived: the code looked right.
//!
//! This test closes it empirically. It is the difference between "the source
//! no longer contains `fs::read`" and "the process does not grow".
//!
//! # Why `#[ignore]`
//!
//! It writes multiple gigabytes to disk. That is not something to do on every
//! CI run, and a disk-full CI agent failing this test would say nothing about
//! the code. Run it deliberately:
//!
//! ```text
//! cargo test -p tickvault-storage --test spill_replay_memory_bound -- --ignored --nocapture
//! ```
//!
//! `TV_SPILL_MEMORY_TEST_BYTES` overrides the file size (default 2 GiB).
//!
//! # What it does NOT prove
//!
//! It does not prove QuestDB accepts the bytes — the listener here accepts
//! everything, deliberately, because the subject is the READ path. It does
//! not reproduce 21 GB. And `VmHWM` is a high-water mark for the whole test
//! process, so it includes the harness itself; that only makes the assertion
//! HARDER to pass, never easier.

use std::io::Write as _;

/// Peak resident set size of this process, in bytes.
///
/// `VmHWM` is the kernel's own high-water mark — the same quantity
/// `tv_process_rss_bytes` reports and the same one that crossed `MemoryHigh`
/// during the incident. It never goes down, which is what makes it the honest
/// measure here: a transient 2 GB spike that is freed before the test ends
/// would be invisible to a sample of current RSS and is caught by this.
fn peak_rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status")
        .expect("this test measures memory and therefore requires /proc");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .expect("VmHWM should carry a numeric first field");
            return kb.saturating_mul(1024);
        }
    }
    panic!("VmHWM absent from /proc/self/status");
}

/// A listener that accepts every POST with 204 and discards the body.
///
/// It must consume the FULL body before replying: reqwest streams the request
/// and a server that replies early can leave the client erroring on a broken
/// pipe, which would end the replay after one chunk and make this test pass
/// vacuously — measuring a bound on a file it never actually read.
async fn spawn_accepting_listener() -> (String, tokio::task::JoinHandle<u64>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("http://{addr}/write");

    let handle = tokio::spawn(async move {
        let mut total_body_bytes: u64 = 0;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut head = Vec::with_capacity(1024);
            let mut byte = [0u8; 1];
            // Read the header line-by-line until the blank line.
            let mut content_length: usize = 0;
            loop {
                match sock.read(&mut byte).await {
                    Ok(0) => break,
                    Ok(_) => head.push(byte[0]),
                    Err(_) => break,
                }
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let head_str = String::from_utf8_lossy(&head).to_ascii_lowercase();
            for line in head_str.lines() {
                if let Some(v) = line.strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            // Drain the body in small reads — the listener must not itself
            // buffer a whole chunk, or IT would be the thing allocating.
            let mut sink = [0u8; 64 * 1024];
            let mut read_so_far = 0usize;
            while read_so_far < content_length {
                let want = sink.len().min(content_length - read_so_far);
                match sock.read(&mut sink[..want]).await {
                    Ok(0) => break,
                    Ok(n) => read_so_far += n,
                    Err(_) => break,
                }
            }
            total_body_bytes = total_body_bytes.saturating_add(read_so_far as u64);
            let _ = sock
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await;
            let _ = sock.flush().await;
        }
        total_body_bytes
    });

    (url, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "writes multiple GB to disk; run deliberately with --ignored"]
async fn a_multi_gigabyte_spill_file_does_not_grow_the_process() {
    let target_bytes: u64 = std::env::var("TV_SPILL_MEMORY_TEST_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024);

    // Plain std, no new dependency: `tempfile` is not a dev-dep of this
    // crate and adding one needs the operator's approval per CLAUDE.md.
    let dir = std::env::temp_dir().join(format!(
        "tv-spill-mem-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create test dir");
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            // Multi-GB fixture: remove it even when the assertion panics, or
            // a failing run leaves the disk full for the next one.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let path = dir.join("depth-dhan-000001.ilp");

    // One realistic ILP line, repeated. ~120 bytes, the shape the depth
    // writer emits.
    let line = b"market_depth,feed=dhan,segment=NSE_FNO,depth_kind=d200,side=bid \
security_id=1234i,level=7i,price=24273.15,quantity=50i,orders=3i,capture_seq=99i 1756800000000000000\n";

    {
        let file = std::fs::File::create(&path).expect("create spill file");
        let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
        let mut written: u64 = 0;
        while written < target_bytes {
            w.write_all(line).expect("write");
            written = written.saturating_add(line.len() as u64);
        }
        w.flush().expect("flush");
    }

    let file_len = std::fs::metadata(&path).expect("stat").len();
    assert!(
        file_len >= target_bytes,
        "the fixture did not reach its target size: {file_len} < {target_bytes}"
    );

    let (url, listener) = spawn_accepting_listener().await;
    let client = reqwest::Client::builder().build().expect("client");

    // Baseline AFTER writing the fixture, so the BufWriter's own memory is
    // already in the high-water mark and cannot be credited to the replay.
    let before = peak_rss_bytes();

    let outcome = tickvault_storage::tick_spill_replay::replay_spill_dir(&dir, &url, &client).await;

    let after = peak_rss_bytes();
    let growth = after.saturating_sub(before);

    listener.abort();

    // The bound. `REPLAY_STREAM_BUFFER_BYTES` is the window; one 8 MiB chunk
    // is copied out per POST (`to_vec`, owned by reqwest); the client keeps
    // its own connection buffers. Eight times the window is generous headroom
    // over all of that and still ~two orders of magnitude below a 2 GiB file
    // — the assertion cannot pass by accident on a whole-file read.
    let bound =
        (tickvault_storage::tick_spill_replay::REPLAY_STREAM_BUFFER_BYTES as u64).saturating_mul(8);

    println!(
        "file {:.2} GB | peak RSS before {:.1} MB | after {:.1} MB | growth {:.1} MB | bound {:.1} MB | outcome {:?}",
        file_len as f64 / 1e9,
        before as f64 / 1e6,
        after as f64 / 1e6,
        growth as f64 / 1e6,
        bound as f64 / 1e6,
        outcome
    );

    assert!(
        growth < bound,
        "STREAMING BOUND BROKEN: replaying a {:.2} GB file grew peak RSS by \
         {:.1} MB, above the {:.1} MB bound. A whole-file read would grow it \
         by roughly the file size — this is the exact shape that restart-looped \
         production on 2026-09-03.",
        file_len as f64 / 1e9,
        growth as f64 / 1e6,
        bound as f64 / 1e6
    );

    // NON-VACUITY. Without this the test is worthless: if the replay had
    // bailed after one chunk — a listener replying early, a connection error,
    // a torn line — growth would ALSO be small and the bound above would pass
    // while proving nothing at all.
    //
    // Three independent facts have to line up, and each closes a different way
    // of passing by accident:
    //
    //   * `bytes_replayed` equals the file length — every byte was read AND
    //     accepted by the endpoint, not merely read.
    //   * `files_failed` is zero — no error path was taken.
    //   * the file is TRUNCATED to zero and still present — the drain-complete
    //     branch is `File::create(&path)`, reached only after the
    //     did-it-grow-while-we-read re-check passes. A partial drain leaves
    //     the file at full length.
    //
    // (The first version of this asserted the file was DELETED. It is not —
    // it is truncated in place. Recorded rather than quietly corrected: the
    // assertion failed on the very run that produced the good number, which
    // is the test doing its job on itself.)
    assert_eq!(
        outcome.bytes_replayed, file_len,
        "the replay accepted {} of {file_len} bytes — the memory bound above \
         was measured on a PARTIAL read and proves nothing",
        outcome.bytes_replayed
    );
    assert_eq!(outcome.files_failed, 0, "an error path was taken");
    assert_eq!(
        outcome.files_replayed, 1,
        "the file was not counted as drained"
    );
    let residual = std::fs::metadata(&path)
        .map(|m| m.len())
        .unwrap_or(u64::MAX);
    assert_eq!(
        residual, 0,
        "the spill file still holds {residual} bytes, so the drain-complete \
         branch was never reached"
    );
}
