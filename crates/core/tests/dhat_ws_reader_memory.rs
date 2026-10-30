//! Reader memory: what the PRODUCTION socket's read path keeps on the heap.
//! Plan item 52c, 2026-10-30.
//!
//! ## Why this file exists
//!
//! The frame ring row in CLAUDE.md and the header of
//! `dhat_ws_reader_zero_alloc.rs` both say the read buffering inside
//! `tokio-tungstenite` was never measured. That buffering is the first heap
//! a frame touches. tungstenite reads the socket into one `BytesMut` of
//! `read_buffer_size` bytes and hands each frame up as a `Bytes` that SHARES
//! that buffer (`split_to` then `freeze`, no copy). While any handed-up frame
//! is alive the buffer is not unique, so the next `reserve` allocates a new
//! chunk. A frame held in the ring or the WAL queue therefore keeps its whole
//! chunk alive, and `RingByteBudget` counts only `frame.len()`.
//!
//! This file measures that under DHAT, through the production code only:
//! `DhanFeedSocketImpl::connect` and `recv`, which build their config with
//! `websocket_config(endpoint)`.
//!
//! ## What it measures, per endpoint (MainFeed, Depth20, Depth200)
//!
//! 1. **Idle.** Heap the socket keeps after `connect` (read buffer, stream
//!    state, the writer task and its queue).
//! 2. **Steady.** Blocks and bytes allocated per frame and per MiB received
//!    while each frame is dropped at once.
//! 3. **Held.** Heap kept while the newest K frames are held FIFO, the way the
//!    ring and the WAL queue hold them, against the payload bytes held. The
//!    ratio is the pinning amplification.
//! 4. **Cap frame.** Heap while one frame at `max_frame_bytes(endpoint)` is
//!    held, after it is dropped, and after later small frames.
//!
//! ## How the window stays clean
//!
//! DHAT counts every thread. The fake server is a plain std thread: it does
//! the WebSocket handshake by hand into a stack buffer, encodes every frame
//! it will send BEFORE the first measurement, and inside a window only calls
//! `write_all` on pre-built bytes and reads one atomic. It keeps no heap
//! after the handshake. The client runs on a current-thread runtime built
//! before any measurement, and the held frames go into a `VecDeque` sized
//! before the window opens.
//!
//! ## Honest limits
//!
//! * Plain `ws://` on loopback. Production is `wss://`: rustls copies each
//!   decrypted record into a fresh `Vec` (up to 16 KiB of received
//!   plaintext) on the same read task, and keeps its own deframer buffer.
//!   None of that is measured here. Measuring it needs a loopback TLS server,
//!   which needs a new dev dependency or a committed test key (owner).
//! * Loopback segmentation is not the vendor's, so allocation counts move a
//!   little from run to run. The exact figures belong to the ignored report;
//!   the ratchet bounds are wide on purpose.
//! * One socket per endpoint, measured one at a time. Figures for 16 or 26
//!   sockets are this per-socket figure multiplied, not a live measurement.
//!
//! DHAT allows one profiler at a time, so the two tests share a lock.
//!
//! Run the report: `cargo test -p tickvault-core --test dhat_ws_reader_memory
//! -- --ignored --nocapture`.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use tickvault_core::websocket::connection::{
    DhanFeedSocketImpl, DhanSocketParams, FeedTokenBuffer, max_frame_bytes,
};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::{DhanFeedSocket, SocketEvent};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// DHAT allows one running profiler per process; the report and the ratchet
/// take this lock so `--include-ignored` cannot start two.
static PROFILER_LOCK: Mutex<()> = Mutex::new(());

/// Bytes the fake server writes per `write_all`: one TCP segment on a
/// 1,500-byte MTU, so the client sees reads the size real traffic gives.
const SEGMENT_BYTES: usize = 1_448;

/// How long any one scenario may take before the test gives up.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(120);

const MIB: f64 = 1_048_576.0;

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// One main-feed ticker packet (16 B, code 2).
fn ticker_packet(marker: u32) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = 2;
    p[1..3].copy_from_slice(&16u16.to_le_bytes());
    p[3] = 2;
    p[4..8].copy_from_slice(&marker.to_le_bytes());
    p
}

/// A payload of `len` bytes. Main-feed payloads are whole ticker packets (a
/// tail shorter than one packet is zero), depth payloads are zero bytes.
/// Neither can read as a disconnect packet, so `recv` hands every one up as
/// a frame.
fn payload(endpoint: DhanEndpointType, len: usize, marker: u32) -> Vec<u8> {
    let mut out = vec![0u8; len];
    if matches!(endpoint, DhanEndpointType::MainFeed) {
        for (i, chunk) in out.chunks_exact_mut(16).enumerate() {
            chunk.copy_from_slice(&ticker_packet(marker.wrapping_add(i as u32)));
        }
    }
    out
}

/// Appends one unmasked server-to-client binary frame (RFC 6455 5.2).
fn encode_frame(out: &mut Vec<u8>, body: &[u8]) {
    out.push(0x82);
    let len = body.len();
    if len < 126 {
        out.push(len as u8);
    } else if len <= usize::from(u16::MAX) {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(body);
}

/// `count` frames of `frame_len` bytes, encoded back to back.
fn encode_batch(endpoint: DhanEndpointType, frame_len: usize, count: usize) -> Vec<u8> {
    let body = payload(endpoint, frame_len, 1);
    let mut out = Vec::with_capacity(count * (frame_len + 10));
    for _ in 0..count {
        encode_frame(&mut out, &body);
    }
    out
}

// ---------------------------------------------------------------------------
// Fake server
// ---------------------------------------------------------------------------

/// A loopback WebSocket server that sends pre-encoded batches on demand.
struct FakeServer {
    port: u16,
    /// Batch `i` is written once `released > i`.
    released: Arc<AtomicUsize>,
    /// Set by the server once the handshake reply is written and its
    /// temporary strings are freed.
    handshake_done: Arc<AtomicBool>,
    /// Set by the client when it has read everything; the server then drops
    /// the connection.
    finished: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeServer {
    fn spawn(batches: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback"); // APPROVED: test
        let port = listener.local_addr().expect("local addr").port(); // APPROVED: test
        let released = Arc::new(AtomicUsize::new(0));
        let handshake_done = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (r, h, f) = (
            Arc::clone(&released),
            Arc::clone(&handshake_done),
            Arc::clone(&finished),
        );
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept"); // APPROVED: test
            drop(listener);
            handshake(&mut stream);
            h.store(true, Ordering::Release);
            for (i, batch) in batches.iter().enumerate() {
                while r.load(Ordering::Acquire) <= i {
                    if f.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
                for segment in batch.chunks(SEGMENT_BYTES) {
                    if stream.write_all(segment).is_err() {
                        return;
                    }
                }
            }
            while !f.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            drop(batches);
        });
        Self {
            port,
            released,
            handshake_done,
            finished,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    fn wait_handshake(&self) {
        let deadline = Instant::now() + SCENARIO_TIMEOUT;
        while !self.handshake_done.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "server handshake never finished");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn release_next(&self) {
        self.released.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Reads the client's upgrade request into a stack buffer and answers 101.
/// The only heap it touches is the accept key and the reply line, both freed
/// before `handshake_done` is set.
fn handshake(stream: &mut TcpStream) {
    let mut buf = [0u8; 8_192];
    let mut len = 0;
    loop {
        let n = stream.read(&mut buf[len..]).expect("read upgrade request"); // APPROVED: test
        assert!(n > 0, "client closed during the upgrade");
        len += n;
        if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        assert!(len < buf.len(), "upgrade request larger than 8 KiB");
    }
    let request = std::str::from_utf8(&buf[..len]).expect("ascii request"); // APPROVED: test
    let key = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim())
        })
        .expect("Sec-WebSocket-Key header"); // APPROVED: test
    let accept = derive_accept_key(key.as_bytes());
    let reply = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(reply.as_bytes()).expect("write 101"); // APPROVED: test
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

/// The production socket and, for the bite check only, a raw
/// `tokio-tungstenite` stream with a caller-chosen config.
enum Client {
    Production(Box<DhanFeedSocketImpl<fn() -> Option<FeedTokenBuffer<String>>>>),
    Raw(
        Box<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    ),
}

fn test_token() -> Option<FeedTokenBuffer<String>> {
    Some(FeedTokenBuffer::new("token".to_string()))
}

impl Client {
    async fn production(endpoint: DhanEndpointType, url: String) -> Self {
        let params = DhanSocketParams::new(endpoint, url, "1".to_string());
        let token: fn() -> Option<FeedTokenBuffer<String>> = test_token;
        let mut socket = Box::new(DhanFeedSocketImpl::new(params, token));
        assert!(
            socket.connect().await.is_ok(),
            "the production socket failed to dial the loopback server"
        );
        Client::Production(socket)
    }

    async fn raw(url: String, config: WebSocketConfig) -> Self {
        let (stream, response) =
            tokio_tungstenite::connect_async_with_config(url, Some(config), true)
                .await
                .expect("raw dial"); // APPROVED: test
        drop(response);
        Client::Raw(Box::new(stream))
    }

    /// The next data frame. Anything else fails the test: the server sends
    /// only binary frames.
    async fn next_frame(&mut self) -> Bytes {
        match self {
            Client::Production(socket) => match socket.recv().await {
                SocketEvent::Frame(bytes) => bytes,
                SocketEvent::KeepAlive => panic!("unexpected keepalive from the fake server"),
                SocketEvent::Closed { code } => panic!("socket closed early: {code:?}"),
            },
            Client::Raw(stream) => match stream.next().await {
                Some(Ok(Message::Binary(bytes))) => bytes,
                other => panic!("raw stream: unexpected {other:?}"),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// One received batch as DHAT saw it.
#[derive(Debug, Clone, Copy, Default)]
struct Window {
    frames: u64,
    payload_bytes: u64,
    alloc_blocks: u64,
    alloc_bytes: u64,
    /// `curr_bytes` at the end of the window minus at its start.
    kept_bytes: i64,
}

impl Window {
    fn blocks_per_frame(&self) -> f64 {
        self.alloc_blocks as f64 / self.frames.max(1) as f64
    }
    fn mib(&self) -> f64 {
        self.payload_bytes as f64 / MIB
    }
    fn blocks_per_mib(&self) -> f64 {
        self.alloc_blocks as f64 / self.mib().max(f64::MIN_POSITIVE)
    }
    fn bytes_per_mib(&self) -> f64 {
        self.alloc_bytes as f64 / self.mib().max(f64::MIN_POSITIVE)
    }
}

/// What one batch is for.
#[derive(Debug, Clone, Copy)]
enum Plan {
    /// Read `count` frames and drop each at once.
    Drop { frame_len: usize, count: usize },
    /// Read `count` frames, keeping the newest `hold` in a FIFO.
    Hold {
        frame_len: usize,
        count: usize,
        hold: usize,
    },
}

impl Plan {
    fn frame_len(self) -> usize {
        match self {
            Plan::Drop { frame_len, .. } | Plan::Hold { frame_len, .. } => frame_len,
        }
    }
    fn count(self) -> usize {
        match self {
            Plan::Drop { count, .. } | Plan::Hold { count, .. } => count,
        }
    }
}

/// One socket's run: the idle figure and one window per plan, plus the
/// heap kept after the held frames of each `Hold` plan are released.
struct Run {
    idle_kept_bytes: i64,
    connect_alloc_bytes: u64,
    windows: Vec<Window>,
    after_release: Vec<i64>,
}

fn stats() -> dhat::HeapStats {
    dhat::HeapStats::get()
}

fn curr_delta(after: &dhat::HeapStats, before: &dhat::HeapStats) -> i64 {
    after.curr_bytes as i64 - before.curr_bytes as i64
}

/// Dials one socket (production unless `raw` is given), then runs every plan
/// against it in order. Everything the client keeps between windows is sized
/// before the first window.
fn run_socket(
    rt: &tokio::runtime::Runtime,
    endpoint: DhanEndpointType,
    plans: &[Plan],
    raw: Option<WebSocketConfig>,
) -> Run {
    let batches: Vec<Vec<u8>> = plans
        .iter()
        .map(|p| encode_batch(endpoint, p.frame_len(), p.count()))
        .collect();
    let max_hold = plans
        .iter()
        .map(|p| match p {
            Plan::Hold { hold, .. } => *hold,
            Plan::Drop { .. } => 0,
        })
        .max()
        .unwrap_or(0); // APPROVED: test
    let mut held: VecDeque<Bytes> = VecDeque::with_capacity(max_hold.max(1));
    let mut windows = Vec::with_capacity(plans.len());
    let mut after_release = Vec::with_capacity(plans.len());

    let server = FakeServer::spawn(batches);
    let url = server.url();

    let before = stats();
    let mut client = rt.block_on(async {
        match raw {
            None => Client::production(endpoint, url).await,
            Some(config) => Client::raw(url, config).await,
        }
    });
    server.wait_handshake();
    // Let the writer task reach its first await so its state is settled.
    rt.block_on(tokio::task::yield_now());
    let after = stats();
    let idle_kept_bytes = curr_delta(&after, &before);
    let connect_alloc_bytes = after.total_bytes - before.total_bytes;

    for plan in plans {
        let start = stats();
        server.release_next();
        let mut frames = 0u64;
        let mut payload_bytes = 0u64;
        rt.block_on(async {
            tokio::time::timeout(SCENARIO_TIMEOUT, async {
                for _ in 0..plan.count() {
                    let frame = client.next_frame().await;
                    assert_eq!(
                        frame.len(),
                        plan.frame_len(),
                        "frame length changed in transit"
                    );
                    frames += 1;
                    payload_bytes += frame.len() as u64;
                    if let Plan::Hold { hold, .. } = plan {
                        if held.len() == *hold {
                            held.pop_front();
                        }
                        held.push_back(frame);
                    }
                }
            })
            .await
            .expect("scenario timed out"); // APPROVED: test
        });
        let end = stats();
        windows.push(Window {
            frames,
            payload_bytes,
            alloc_blocks: end.total_blocks - start.total_blocks,
            alloc_bytes: end.total_bytes - start.total_bytes,
            kept_bytes: curr_delta(&end, &start),
        });
        held.clear();
        after_release.push(curr_delta(&stats(), &start));
    }
    drop(client);
    drop(server);
    rt.block_on(tokio::task::yield_now());
    Run {
        idle_kept_bytes,
        connect_alloc_bytes,
        windows,
        after_release,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime") // APPROVED: test
}

/// The payload bytes a `Hold` plan has in its FIFO at the end.
fn held_payload(plan: Plan) -> u64 {
    match plan {
        Plan::Hold {
            frame_len,
            count,
            hold,
        } => (frame_len * hold.min(count)) as u64,
        Plan::Drop { .. } => 0,
    }
}

/// One warm-up dial so lazy one-time state (TLS roots, runtime pieces) is
/// built before anything is measured.
fn warm_up(rt: &tokio::runtime::Runtime) {
    let _ = run_socket(
        rt,
        DhanEndpointType::MainFeed,
        &[Plan::Drop {
            frame_len: 162,
            count: 256,
        }],
        None,
    );
}

// ---------------------------------------------------------------------------
// Report (ignored)
// ---------------------------------------------------------------------------

fn report_plans(endpoint: DhanEndpointType) -> Vec<Plan> {
    let cap = max_frame_bytes(endpoint);
    let shapes: &[usize] = match endpoint {
        // One ticker, one Full packet, fifty Full packets stacked.
        DhanEndpointType::MainFeed | DhanEndpointType::OrderUpdate => &[16, 162, 8_100],
        DhanEndpointType::Depth20 => &[332],
        DhanEndpointType::Depth200 => &[3_212],
    };
    let mut plans = Vec::new();
    for &len in shapes {
        // About 4 MiB per shape, at least 4,096 frames.
        let count = (4 * 1_048_576 / len).max(4_096);
        plans.push(Plan::Drop {
            frame_len: len,
            count,
        });
    }
    for &len in shapes {
        plans.push(Plan::Hold {
            frame_len: len,
            count: 2_048,
            hold: 1_024,
        });
    }
    // The ring holds up to 65,536 frames: fill it with the smallest frame.
    plans.push(Plan::Hold {
        frame_len: shapes[0],
        count: 65_536 + 1_024,
        hold: 65_536,
    });
    // One frame at the endpoint cap, then small frames after it.
    plans.push(Plan::Hold {
        frame_len: cap,
        count: 1,
        hold: 1,
    });
    plans.push(Plan::Drop {
        frame_len: shapes[0],
        count: 4_096,
    });
    plans
}

fn describe(plan: Plan) -> String {
    match plan {
        Plan::Drop { frame_len, count } => format!("drop  {count:>7} x {frame_len:>7} B"),
        Plan::Hold {
            frame_len,
            count,
            hold,
        } => format!("hold {hold:>6}/{count:>6} x {frame_len:>7} B"),
    }
}

#[test]
#[ignore = "report: run with --ignored --nocapture"]
fn reader_memory_report() {
    let _lock = PROFILER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _profiler = dhat::Profiler::builder().testing().build();
    let rt = runtime();
    warm_up(&rt);

    let mut idle = Vec::new();
    for endpoint in [
        DhanEndpointType::MainFeed,
        DhanEndpointType::Depth20,
        DhanEndpointType::Depth200,
    ] {
        let plans = report_plans(endpoint);
        let run = run_socket(&rt, endpoint, &plans, None);
        eprintln!(
            "\n== {} (cap {} B) ==\nidle kept {} B, allocated during connect {} B",
            endpoint.as_str(),
            max_frame_bytes(endpoint),
            run.idle_kept_bytes,
            run.connect_alloc_bytes
        );
        eprintln!(
            "{:<32} {:>9} {:>10} {:>11} {:>13} {:>12} {:>12} {:>7}",
            "plan", "MiB", "blocks", "blk/frame", "alloc B/MiB", "kept B", "after rel B", "amp"
        );
        for ((plan, w), rel) in plans.iter().zip(&run.windows).zip(&run.after_release) {
            let held = held_payload(*plan);
            let amp = if held > 0 {
                format!("{:.2}", w.kept_bytes as f64 / held as f64)
            } else {
                "-".to_string()
            };
            eprintln!(
                "{:<32} {:>9.3} {:>10} {:>11.4} {:>13.0} {:>12} {:>12} {:>7}",
                describe(*plan),
                w.mib(),
                w.alloc_blocks,
                w.blocks_per_frame(),
                w.bytes_per_mib(),
                w.kept_bytes,
                rel,
                amp
            );
        }
        idle.push((endpoint, run.idle_kept_bytes));
    }

    // Bite reference: the library default read buffer (128 KiB).
    let raw = run_socket(
        &rt,
        DhanEndpointType::MainFeed,
        &ratchet_plans(),
        Some(WebSocketConfig::default()),
    );
    eprintln!(
        "\nraw tokio-tungstenite, default config (128 KiB read buffer): idle kept {} B",
        raw.idle_kept_bytes
    );

    let main = idle[0].1.max(0) as f64;
    let depth = idle[1].1.max(idle[2].1).max(0) as f64;
    eprintln!(
        "\nprojection (plain ws, no TLS): 5 main-feed sockets idle {:.1} KiB; \
         26 sockets (5 main + 10 depth-20 + 10 depth-200 + 1 order-update as main) \
         idle {:.1} KiB",
        5.0 * main / 1024.0,
        (6.0 * main + 20.0 * depth) / 1024.0
    );
}

// ---------------------------------------------------------------------------
// Ratchet (runs in CI)
// ---------------------------------------------------------------------------

/// Small enough to run fast in a debug build, large enough to cross many
/// read-buffer chunks.
fn ratchet_plans() -> Vec<Plan> {
    vec![
        Plan::Drop {
            frame_len: 162,
            count: 8_192,
        },
        Plan::Hold {
            frame_len: 162,
            count: 2_048,
            hold: 1_024,
        },
    ]
}

/// Bounds set from the 2026-10-30 report run (x86 dev container, debug
/// build), with wide margins. Each is a ceiling, never a target.
struct Bounds {
    /// Heap one idle socket keeps after `connect`.
    idle_kept_bytes: i64,
    /// Blocks allocated per MiB received while frames are dropped at once.
    steady_blocks_per_mib: f64,
    /// Heap kept by 1,024 held frames over the payload they hold.
    held_amplification: f64,
}

const BOUNDS: Bounds = Bounds {
    idle_kept_bytes: 48 * 1024,
    steady_blocks_per_mib: 400.0,
    held_amplification: 4.0,
};

/// Every bound `run` breaks, as text. Empty means inside every bound.
fn violations(run: &Run, plans: &[Plan], bounds: &Bounds) -> Vec<String> {
    let mut out = Vec::new();
    if run.idle_kept_bytes > bounds.idle_kept_bytes {
        out.push(format!(
            "idle socket keeps {} B > {} B",
            run.idle_kept_bytes, bounds.idle_kept_bytes
        ));
    }
    let steady = run.windows[0];
    if steady.blocks_per_mib() > bounds.steady_blocks_per_mib {
        out.push(format!(
            "steady read allocates {:.1} blocks/MiB > {:.1}",
            steady.blocks_per_mib(),
            bounds.steady_blocks_per_mib
        ));
    }
    let held = run.windows[1];
    let amp = held.kept_bytes as f64 / held_payload(plans[1]) as f64;
    if amp > bounds.held_amplification {
        out.push(format!(
            "held frames pin {amp:.2}x their payload > {:.2}x",
            bounds.held_amplification
        ));
    }
    out
}

/// Every frame the server sent arrived, whole, and the read path really
/// allocated: a run that measured nothing cannot pass.
fn assert_not_vacuous(run: &Run, plans: &[Plan]) {
    for (plan, w) in plans.iter().zip(&run.windows) {
        assert_eq!(w.frames, plan.count() as u64, "frames lost in {plan:?}");
        assert_eq!(
            w.payload_bytes,
            (plan.count() * plan.frame_len()) as u64,
            "bytes lost in {plan:?}"
        );
    }
    assert!(
        run.idle_kept_bytes > 0,
        "connect kept no heap: nothing measured"
    );
    assert!(
        run.windows[0].alloc_blocks > 0,
        "the steady read allocated nothing: DHAT saw no read buffer chunk"
    );
    assert!(
        run.windows[1].kept_bytes > 0,
        "1,024 held frames pinned no heap: the hold did not happen"
    );
}

#[test]
fn reader_memory_ratchet() {
    let _lock = PROFILER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _profiler = dhat::Profiler::builder().testing().build();
    let rt = runtime();
    warm_up(&rt);
    let plans = ratchet_plans();

    for endpoint in [
        DhanEndpointType::MainFeed,
        DhanEndpointType::Depth20,
        DhanEndpointType::Depth200,
    ] {
        let run = run_socket(&rt, endpoint, &plans, None);
        assert_not_vacuous(&run, &plans);
        let broken = violations(&run, &plans, &BOUNDS);
        assert!(
            broken.is_empty(),
            "{}: the production read path grew: {broken:?}",
            endpoint.as_str()
        );
    }

    // The bounds bite: the same run through a raw stream with the library
    // default config (128 KiB read buffer) must break at least one.
    let raw = run_socket(
        &rt,
        DhanEndpointType::MainFeed,
        &plans,
        Some(WebSocketConfig::default()),
    );
    assert_not_vacuous(&raw, &plans);
    assert!(
        !violations(&raw, &plans, &BOUNDS).is_empty(),
        "a 128 KiB read buffer passed every bound: the ratchet does not bite"
    );
}
