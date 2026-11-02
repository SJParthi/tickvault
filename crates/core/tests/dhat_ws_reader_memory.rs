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
//! The gated runs measure that under DHAT through the production socket:
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
//! nonblocking writes on pre-built bytes and reads cancellation atomics. Its
//! prebuilt batches remain allocated outside each measured delta; they are
//! not absent from the process heap. The client runs on a current-thread runtime built
//! before any measurement, and the held frames go into a `VecDeque` sized
//! before the window opens.
//!
//! ## Honest limits
//!
//! * Plain `ws://` on loopback. TLS record buffering/decryption, the live
//!   network, and the vendor packet mix are not measured. Synthetic payloads
//!   exercise transport buffering; they do not prove accepted market ticks.
//! * All frame lengths, order markers, and remaining bytes are checked. This
//!   extra fixture work affects scheduling; these are allocation observations,
//!   not latency measurements or a production allocation guarantee.
//! * Loopback segmentation is not the vendor's, so allocation counts move a
//!   little from run to run. The exact figures belong to the optional report;
//!   the ratchet bounds are wide on purpose.
//! * One socket per endpoint, measured one at a time. Figures for 16 or 26
//!   sockets are this per-socket figure multiplied, not a live measurement.
//!
//! DHAT counts all threads. Every test in this binary shares one lock.
//!
//! Run the normal allocation gate with `cargo test --locked -p tickvault-core
//! --test dhat_ws_reader_memory`. For the additional large shape report, set
//! `TV_READER_MEMORY_REPORT=1` and select `dhat_reader_memory_ratchet -- --nocapture`.
//! The normal gate still runs first; no test is ignored or conditionally skipped.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
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

/// Serialize every test so auxiliary fixture allocations cannot enter DHAT windows.
static PROFILER_LOCK: Mutex<()> = Mutex::new(());

/// Write chunk size only. TCP may combine or split these writes; this is not
/// a claim about packet boundaries or the vendor's segmentation.
const WRITE_CHUNK_BYTES: usize = 1_448;
const SERVER_POLL_INTERVAL: Duration = Duration::from_micros(200);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

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
    assert!(out.len() >= 8, "the fixture needs an order marker");
    out[4..8].copy_from_slice(&marker.to_le_bytes());
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
    let mut body = payload(endpoint, frame_len, 1);
    let mut out = Vec::with_capacity(count * (frame_len + 10));
    for index in 0..count {
        body[4..8].copy_from_slice(&((index + 1) as u32).to_le_bytes());
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
    accepted: Arc<AtomicBool>,
    /// Set by the client when it has read everything; the server then drops
    /// the connection.
    finished: Arc<AtomicBool>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl FakeServer {
    fn spawn(batches: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback"); // APPROVED: test
        let port = listener.local_addr().expect("local addr").port(); // APPROVED: test
        let released = Arc::new(AtomicUsize::new(0));
        let handshake_done = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicBool::new(false));
        let accepted_writer = Arc::clone(&accepted);
        let finished = Arc::new(AtomicBool::new(false));
        let (r, h, f) = (
            Arc::clone(&released),
            Arc::clone(&handshake_done),
            Arc::clone(&finished),
        );
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener"); // APPROVED: test
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
            let mut stream = loop {
                check_io_deadline(&f, deadline)?;
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(SERVER_POLL_INTERVAL);
                    }
                    Err(error) => return Err(error),
                }
            };
            drop(listener);
            stream.set_nonblocking(true)?;
            accepted_writer.store(true, Ordering::Release);
            handshake(&mut stream, &f, deadline)?;
            h.store(true, Ordering::Release);
            for (i, batch) in batches.iter().enumerate() {
                let deadline = Instant::now() + SCENARIO_TIMEOUT;
                while r.load(Ordering::Acquire) <= i {
                    check_io_deadline(&f, deadline)?;
                    std::thread::sleep(SERVER_POLL_INTERVAL);
                }
                for chunk in batch.chunks(WRITE_CHUNK_BYTES) {
                    write_until(&mut stream, chunk, &f, deadline)?;
                }
            }
            while !f.load(Ordering::Acquire) {
                std::thread::sleep(SERVER_POLL_INTERVAL);
            }
            Ok(())
        });
        Self {
            port,
            released,
            handshake_done,
            accepted,
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
            assert!(
                !self.thread.as_ref().expect("server thread").is_finished(),
                "server ended before its handshake"
            );
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
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) if error.kind() == ErrorKind::Interrupted => {}
                    other => panic!("fake server failed: {other:?}"),
                }
            }
        }
    }
}

/// Every network syscall is nonblocking. Cancellation and an absolute deadline
/// are checked even while a peer makes partial progress, so Drop cannot become
/// trapped in accept/read/write on a stalled loopback peer.
fn check_io_deadline(stop: &AtomicBool, deadline: Instant) -> io::Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err(ErrorKind::Interrupted.into());
    }
    if Instant::now() >= deadline {
        return Err(ErrorKind::TimedOut.into());
    }
    Ok(())
}

fn write_until<W: Write>(
    stream: &mut W,
    mut bytes: &[u8],
    stop: &AtomicBool,
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        check_io_deadline(stop, deadline)?;
        match stream.write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(SERVER_POLL_INTERVAL);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Reads the upgrade request into a fixed stack buffer. The accept key/reply
/// allocations are freed before handshake_done is published.
fn handshake(stream: &mut TcpStream, stop: &AtomicBool, deadline: Instant) -> io::Result<()> {
    let mut buf = [0u8; 8_192];
    let mut len = 0;
    loop {
        check_io_deadline(stop, deadline)?;
        match stream.read(&mut buf[len..]) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(read) => len += read,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(SERVER_POLL_INTERVAL);
                continue;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        if buf[..len].windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if len == buf.len() {
            return Err(ErrorKind::InvalidData.into());
        }
    }
    let request = std::str::from_utf8(&buf[..len]).map_err(|_| ErrorKind::InvalidData)?;
    let key = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim())
        })
        .ok_or(ErrorKind::InvalidData)?;
    let accept = derive_accept_key(key.as_bytes());
    let reply = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    write_until(stream, reply.as_bytes(), stop, deadline)
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

type ProductionReader = DhanFeedSocketImpl<fn() -> Option<FeedTokenBuffer<String>>>;

/// The production socket and, for the bite check only, a raw
/// `tokio-tungstenite` stream with a caller-chosen config.
enum Client {
    Production(Box<ProductionReader>),
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
        tokio::time::timeout(SCENARIO_TIMEOUT, async {
            match raw {
                None => Client::production(endpoint, url).await,
                Some(config) => Client::raw(url, config).await,
            }
        })
        .await
        .expect("loopback connection timed out") // APPROVED: test
    });
    server.wait_handshake();
    // Give the writer task an initial scheduling opportunity. This is an
    // after-connect observation, not proof that every lazy path is warmed.
    rt.block_on(tokio::task::yield_now());
    let after = stats();
    let idle_kept_bytes = curr_delta(&after, &before);
    let connect_alloc_bytes = after.total_bytes - before.total_bytes;

    for plan in plans {
        let expected = payload(endpoint, plan.frame_len(), 1);
        let start = stats();
        server.release_next();
        let mut frames = 0u64;
        let mut payload_bytes = 0u64;
        rt.block_on(async {
            tokio::time::timeout(SCENARIO_TIMEOUT, async {
                for index in 0..plan.count() {
                    let frame = client.next_frame().await;
                    assert_eq!(
                        frame.len(),
                        plan.frame_len(),
                        "frame length changed in transit"
                    );
                    assert_eq!(&frame[..4], &expected[..4], "frame header changed");
                    assert_eq!(
                        &frame[4..8],
                        &((index + 1) as u32).to_le_bytes(),
                        "frame order changed"
                    );
                    assert_eq!(&frame[8..], &expected[8..], "frame payload changed");
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
    // Dropping the production socket closes its writer channels. Await actual
    // task completion before another scenario can measure its deferred frees.
    rt.block_on(async {
        tokio::time::timeout(SCENARIO_TIMEOUT, async {
            while rt.metrics().num_alive_tasks() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("socket writer did not terminate"); // APPROVED: test
    });
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
// Optional extended report (the normal ratchet always runs first)
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

fn reader_memory_report(rt: &tokio::runtime::Runtime) {
    let mut idle = Vec::new();
    for endpoint in [
        DhanEndpointType::MainFeed,
        DhanEndpointType::Depth20,
        DhanEndpointType::Depth200,
    ] {
        let plans = report_plans(endpoint);
        let run = run_socket(rt, endpoint, &plans, None);
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
        rt,
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

/// Ceilings inherited unchanged from the paused 2026-10-30 WIP report
/// (x86 dev container, debug build). That historical description is not a
/// fresh measurement of this port. Each is a ceiling, never a target.
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
    assert_eq!(run.windows.len(), plans.len(), "missing measurement window");
    assert_eq!(
        run.after_release.len(),
        plans.len(),
        "missing release observation"
    );
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
fn dhat_reader_memory_ratchet() {
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
    if std::env::var("TV_READER_MEMORY_REPORT").is_ok_and(|value| value == "1") {
        reader_memory_report(&rt);
    }
}

#[test]
fn test_fixture_cancellation_and_expired_io_deadline() {
    let _lock = PROFILER_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let stop = AtomicBool::new(false);
    assert_eq!(
        check_io_deadline(&stop, Instant::now()).unwrap_err().kind(),
        ErrorKind::TimedOut
    );
    stop.store(true, Ordering::Release);
    assert_eq!(
        check_io_deadline(&stop, Instant::now() + HANDSHAKE_TIMEOUT)
            .unwrap_err()
            .kind(),
        ErrorKind::Interrupted
    );

    // No client: the listener's accept loop must observe cancellation.
    let mut server = FakeServer::spawn(Vec::new());
    server.finished.store(true, Ordering::Release);
    assert_eq!(
        server
            .thread
            .take()
            .expect("thread")
            .join()
            .expect("join")
            .unwrap_err()
            .kind(),
        ErrorKind::Interrupted
    );

    // A client connects but never supplies an HTTP upgrade. Once a connection
    // is accepted, cancelling still exits the read loop instead of hanging Drop.
    let mut server = FakeServer::spawn(Vec::new());
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], server.port));
    let mut client = TcpStream::connect_timeout(&address, HANDSHAKE_TIMEOUT).expect("connect");
    client
        .set_write_timeout(Some(HANDSHAKE_TIMEOUT))
        .expect("client write deadline");
    client
        .write_all(b"GET / HTTP/1.1\r\n")
        .expect("partial upgrade");
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while !server.accepted.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "server never accepted the partial upgrade"
        );
        std::thread::sleep(SERVER_POLL_INTERVAL);
    }
    server.finished.store(true, Ordering::Release);
    assert_eq!(
        server
            .thread
            .take()
            .expect("thread")
            .join()
            .expect("join")
            .unwrap_err()
            .kind(),
        ErrorKind::Interrupted
    );
    assert!(!server.handshake_done.load(Ordering::Acquire));

    struct StalledWriter<'a> {
        calls: usize,
        cancel: &'a AtomicBool,
    }
    impl Write for StalledWriter<'_> {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            self.cancel.store(true, Ordering::Release);
            Err(ErrorKind::WouldBlock.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    stop.store(false, Ordering::Release);
    let mut writer = StalledWriter {
        calls: 0,
        cancel: &stop,
    };
    assert_eq!(
        write_until(&mut writer, b"unwritten", &stop, Instant::now())
            .unwrap_err()
            .kind(),
        ErrorKind::TimedOut
    );
    assert_eq!(writer.calls, 0, "expired I/O must not write");
    assert_eq!(
        write_until(
            &mut writer,
            b"unwritten",
            &stop,
            Instant::now() + HANDSHAKE_TIMEOUT
        )
        .unwrap_err()
        .kind(),
        ErrorKind::Interrupted
    );
    assert_eq!(
        writer.calls, 1,
        "backpressure must return to the cancellation check"
    );
}

#[test]
fn test_reader_memory_violations_each_ceiling_and_missing_window() {
    let _lock = PROFILER_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let plans = ratchet_plans();
    let mut run = Run {
        idle_kept_bytes: BOUNDS.idle_kept_bytes,
        connect_alloc_bytes: 1,
        windows: vec![
            Window {
                frames: plans[0].count() as u64,
                payload_bytes: (plans[0].count() * plans[0].frame_len()) as u64,
                alloc_blocks: 1,
                alloc_bytes: 1,
                kept_bytes: 1,
            },
            Window {
                frames: plans[1].count() as u64,
                payload_bytes: (plans[1].count() * plans[1].frame_len()) as u64,
                alloc_blocks: 1,
                alloc_bytes: 1,
                kept_bytes: (held_payload(plans[1]) * 4) as i64,
            },
        ],
        after_release: vec![0, 0],
    };
    assert_not_vacuous(&run, &plans);
    assert!(violations(&run, &plans, &BOUNDS).is_empty());
    run.idle_kept_bytes += 1;
    assert!(violations(&run, &plans, &BOUNDS)[0].starts_with("idle socket"));
    run.idle_kept_bytes = BOUNDS.idle_kept_bytes;
    run.windows[0].alloc_blocks = (run.windows[0].mib() * BOUNDS.steady_blocks_per_mib) as u64 + 1;
    assert!(violations(&run, &plans, &BOUNDS)[0].starts_with("steady read"));
    run.windows[0].alloc_blocks = 1;
    run.windows[1].kept_bytes += 1;
    assert!(violations(&run, &plans, &BOUNDS)[0].starts_with("held frames"));
    run.windows.clear();
    assert!(std::panic::catch_unwind(|| assert_not_vacuous(&run, &plans)).is_err());
}
