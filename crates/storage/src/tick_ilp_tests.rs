//! Our ILP encoder and sender against the questdb-rs library (plan ITEM DT,
//! 2026-10-30): every primitive must write the library's bytes, refuse what
//! the library refuses, and the sender must make the library's HTTP request.

use super::*;
use proptest::prelude::*;
use questdb::ingress::{ColumnName, TableName};
use std::io::{BufRead, BufReader, Read, Write};

// ---------------------------------------------------------------------------
// Helpers: one call through each encoder
// ---------------------------------------------------------------------------

/// `t v=<value>` through the library.
fn library_f64(value: f64) -> Vec<u8> {
    let mut b = Buffer::new(ProtocolVersion::V1);
    let written = b
        .table(TableName::new_unchecked("t"))
        .and_then(|b| b.column_f64(ColumnName::new_unchecked("v"), value));
    assert!(written.is_ok(), "library refused {value:?}");
    b.as_bytes().to_vec()
}

/// `t v=<value>` through our encoder.
fn own_f64(value: f64) -> Vec<u8> {
    let mut b = OwnIlpBuffer::new();
    assert!(IlpLine::table(&mut b, "t").is_ok());
    assert!(IlpLine::column_f64(&mut b, "v", value).is_ok());
    b.as_bytes().to_vec()
}

fn assert_same_f64(value: f64) {
    let (own, lib) = (own_f64(value), library_f64(value));
    assert_eq!(
        own,
        lib,
        "f64 {value:?} (bits {:#018x}): own {:?} library {:?}",
        value.to_bits(),
        String::from_utf8_lossy(&own),
        String::from_utf8_lossy(&lib)
    );
}

/// One whole line of every kind of call, through any encoder.
fn full_line<B: IlpLine>(
    b: &mut B,
    symbol: &str,
    int: i64,
    float: f64,
    ts: i64,
    at: i64,
) -> Result<(), TickIlpError> {
    b.table("ticks")?;
    b.symbol("segment", symbol)?;
    b.symbol("feed", "dhan")?;
    b.column_i64("security_id", int)?;
    b.column_f64("ltp", float)?;
    b.column_ts_nanos("received_at", ts)?;
    b.column_i64("capture_seq", int.wrapping_neg())?;
    b.at_nanos(at)
}

fn any_text() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<String>(),
        "[ ,=\\\\\r\na-zA-Z0-9_-]{0,24}",
        Just(String::new()),
    ]
}

// ---------------------------------------------------------------------------
// f64 text
// ---------------------------------------------------------------------------

#[test]
fn test_exact_cents_f64_edge_values_match_the_library() {
    let edges = [
        0.0,
        -0.0,
        f64::NAN,
        -f64::NAN,
        f64::from_bits(0x7ff0_0000_0000_0001),
        f64::from_bits(0xfff8_0000_0000_0123),
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::EPSILON,
        f64::MAX,
        f64::MIN,
        0.01,
        -0.01,
        0.05,
        0.1,
        0.005,
        0.015,
        1.0,
        10.0,
        245.35,
        2400.0,
        24_500.05,
        99_999.99,
        131_071.99,
        1e13,
        9_999_999_999_999.99,
        1e15 / 100.0,
        1e15,
        1e15 - 1.0,
        1e16,
        1e21,
        1e22,
        123_456_789_012.34,
        -123_456_789_012.34,
        0.1 + 0.2,
        1.0 / 3.0,
    ];
    for v in edges {
        assert_same_f64(v);
    }
}

#[test]
fn test_exact_cents_every_paise_up_to_ten_lakh_rupees_matches_the_library() {
    // Every whole number of paise from -10,00,000.00 to +10,00,000.00 in
    // steps of one paisa near zero and a coprime stride beyond, so every
    // digit pattern of one and two decimals is reached.
    for cents in -20_000_i64..=20_000 {
        #[allow(clippy::cast_precision_loss)]
        let v = cents as f64 / 100.0;
        assert_same_f64(v);
    }
    let mut cents = -100_000_000_i64;
    while cents <= 100_000_000 {
        #[allow(clippy::cast_precision_loss)]
        let v = cents as f64 / 100.0;
        assert_same_f64(v);
        cents += 997;
    }
}

#[test]
fn test_exact_cents_admits_only_whole_paise() {
    assert_eq!(exact_cents(0.0), Some(0));
    assert_eq!(exact_cents(-0.0), None, "-0.0 prints with its sign");
    assert_eq!(exact_cents(245.35), Some(24_535));
    assert_eq!(exact_cents(-0.05), Some(-5));
    assert_eq!(exact_cents(0.005), None, "half a paisa is not whole");
    assert_eq!(exact_cents(1.0 / 3.0), None);
    assert_eq!(exact_cents(f64::NAN), None);
    assert_eq!(exact_cents(f64::INFINITY), None);
    assert_eq!(
        exact_cents(1e13),
        None,
        "at the bound the library path runs"
    );
    assert_eq!(exact_cents(9_999_999_999_999.99), Some(999_999_999_999_999));
}

/// Every `f32` a tick price can be below 2^17 (the stored price, after the
/// writer's two-decimal rounding), sampled at a stride that visits every
/// exponent and both signs. The exhaustive sweep is the ignored test below.
#[test]
fn test_exact_cents_sampled_f32_prices_match_the_library() {
    let mut bits = 0_u32;
    while bits < 0x4800_0000 {
        let v = f32::from_bits(bits);
        assert_same_f64(crate::tick_persistence::tick_price_f64(v));
        assert_same_f64(crate::tick_persistence::tick_price_f64(-v));
        bits += 4_099;
    }
}

/// Every `f32` with magnitude below 2^17, both signs, through the writer's
/// price conversion and then both encoders. About 2.2 billion values;
/// release build only (`cargo test --release -- --ignored`).
#[test]
#[ignore = "exhaustive: every f32 below 2^17, minutes in release"]
fn test_exact_cents_every_f32_price_below_2_pow_17_matches_the_library() {
    let mut own = OwnIlpBuffer::new();
    let mut lib = Buffer::new(ProtocolVersion::V1);
    for bits in 0_u32..0x4800_0000 {
        for v in [f32::from_bits(bits), -f32::from_bits(bits)] {
            let price = crate::tick_persistence::tick_price_f64(v);
            own.clear();
            lib.clear();
            assert!(IlpLine::table(&mut own, "t").is_ok());
            assert!(IlpLine::column_f64(&mut own, "v", price).is_ok());
            assert!(
                lib.table(TableName::new_unchecked("t"))
                    .and_then(|b| b.column_f64(ColumnName::new_unchecked("v"), price))
                    .is_ok()
            );
            assert_eq!(own.as_bytes(), lib.as_bytes(), "f32 bits {bits:#010x}");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4_096))]

    #[test]
    fn test_exact_cents_any_f64_bits_match_the_library(bits in any::<u64>()) {
        let v = f64::from_bits(bits);
        prop_assert_eq!(own_f64(v), library_f64(v));
    }

    #[test]
    fn test_exact_cents_any_paise_count_matches_the_library(cents in -999_999_999_999_999_i64..=999_999_999_999_999) {
        #[allow(clippy::cast_precision_loss)]
        let v = cents as f64 / 100.0;
        prop_assert_eq!(own_f64(v), library_f64(v));
    }

    /// A whole line of every call kind, with hostile symbol text and any
    /// integers: same bytes, or both refuse (a negative designated timestamp).
    #[test]
    fn test_full_line_matches_the_library(
        symbol in any_text(),
        int in any::<i64>(),
        float in any::<f64>(),
        ts in any::<i64>(),
        at in any::<i64>(),
    ) {
        let mut own = OwnIlpBuffer::new();
        let mut lib = Buffer::new(ProtocolVersion::V1);
        let own_ok = full_line(&mut own, &symbol, int, float, ts, at).is_ok();
        let lib_ok = full_line(&mut lib, &symbol, int, float, ts, at).is_ok();
        prop_assert_eq!(own_ok, lib_ok);
        prop_assert_eq!(own_ok, at >= 0);
        if own_ok {
            prop_assert_eq!(own.as_bytes(), lib.as_bytes());
            prop_assert_eq!(own.row_count(), 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Call order and markers
// ---------------------------------------------------------------------------

/// Each out-of-order call our encoder refuses, the library refuses too.
#[test]
fn test_call_order_refusals_match_the_library() {
    fn run<B: IlpLine>(b: &mut B, calls: &[&str]) -> Vec<bool> {
        calls
            .iter()
            .map(|c| match *c {
                "table" => b.table("t").is_ok(),
                "symbol" => b.symbol("s", "x").is_ok(),
                "i64" => b.column_i64("i", 1).is_ok(),
                "f64" => b.column_f64("f", 1.5).is_ok(),
                "ts" => b.column_ts_nanos("ts", 1_000).is_ok(),
                "at" => b.at_nanos(5).is_ok(),
                _ => false,
            })
            .collect()
    }
    let sequences: &[&[&str]] = &[
        &["symbol"],
        &["i64"],
        &["at"],
        &["table", "table"],
        &["table", "at"],
        &["table", "i64", "symbol"],
        &[
            "table", "symbol", "symbol", "f64", "ts", "at", "table", "i64", "at",
        ],
        &["table", "f64", "at", "at"],
    ];
    for calls in sequences {
        let mut own = OwnIlpBuffer::new();
        let mut lib = Buffer::new(ProtocolVersion::V1);
        let own_steps = run(&mut own, calls);
        let lib_steps = run(&mut lib, calls);
        // After the first refusal the two buffers may differ in what they
        // half-wrote, so compare outcomes up to and including it.
        let upto = own_steps
            .iter()
            .position(|ok| !ok)
            .map_or(own_steps.len(), |i| i + 1);
        assert_eq!(&own_steps[..upto], &lib_steps[..upto], "sequence {calls:?}");
        if !own_steps.contains(&false) {
            assert_eq!(own.as_bytes(), lib.as_bytes(), "sequence {calls:?}");
        }
    }
}

#[test]
fn test_set_marker_rewind_to_marker_and_clear_marker() {
    let mut b = OwnIlpBuffer::new();
    assert!(b.is_empty());
    assert!(b.can_flush());
    assert!(b.rewind_to_marker().is_err(), "no marker yet");
    assert!(full_line(&mut b, "NSE_FNO", 7, 1.25, 1_000, 2_000).is_ok());
    let first = b.as_bytes().to_vec();
    assert!(b.set_marker().is_ok());
    assert!(IlpLine::table(&mut b, "t").is_ok());
    assert!(!b.can_flush(), "a row is half-written");
    assert!(b.set_marker().is_err(), "marker inside a row is refused");
    assert!(b.rewind_to_marker().is_ok());
    assert_eq!(b.as_bytes(), &first[..]);
    assert_eq!(b.row_count(), 1);
    assert!(b.can_flush());
    // The marker is consumed by the rewind.
    assert!(b.rewind_to_marker().is_err());
    assert!(b.set_marker().is_ok());
    b.clear_marker();
    assert!(b.rewind_to_marker().is_err(), "cleared");
    assert_eq!(b.len(), first.len());
}

#[test]
fn test_clear_reserve_capacity_and_len() {
    let mut b = OwnIlpBuffer::new();
    assert_eq!(b.capacity(), 0, "new allocates nothing");
    b.reserve(4_096);
    assert!(b.capacity() >= 4_096);
    assert!(full_line(&mut b, "x", 1, 2.0, 3, 4).is_ok());
    assert!(!b.is_empty());
    let cap = b.capacity();
    b.clear();
    assert!(b.is_empty());
    assert_eq!(b.len(), 0);
    assert_eq!(b.row_count(), 0);
    assert_eq!(b.capacity(), cap, "clear keeps the capacity");
    assert!(b.rewind_to_marker().is_err(), "clear drops the marker");
}

// ---------------------------------------------------------------------------
// The tick writer's buffer and sender enums
// ---------------------------------------------------------------------------

#[test]
fn test_kind_and_empty_buffer_keep_the_encoder_and_protocol() {
    let lib = TickRowBuffer::new(TickIlpEncoder::Library);
    let own = TickRowBuffer::new(TickIlpEncoder::Own);
    assert_eq!(lib.kind(), TickRowBufferKind::Library(ProtocolVersion::V1));
    assert_eq!(own.kind(), TickRowBufferKind::Own);
    assert_ne!(lib.kind(), own.kind(), "a spare never crosses encoders");
    let v2 = TickRowBufferKind::Library(ProtocolVersion::V2).empty_buffer();
    assert_eq!(v2.kind(), TickRowBufferKind::Library(ProtocolVersion::V2));
    assert_ne!(v2.kind(), lib.kind());
    assert_eq!(own.kind().empty_buffer().kind(), TickRowBufferKind::Own);
    assert_eq!(own.capacity(), 0);
}

#[test]
fn test_tick_row_buffer_dispatch_writes_each_encoder_identically() {
    let mut lib = TickRowBuffer::new(TickIlpEncoder::Library);
    let mut own = TickRowBuffer::new(TickIlpEncoder::Own);
    for b in [&mut lib, &mut own] {
        b.reserve(1_024);
        assert!(b.set_marker().is_ok());
        assert!(full_line(b, "NSE_EQ", 42, 245.35, 9_000, 10_000).is_ok());
        b.clear_marker();
        assert!(b.set_marker().is_ok());
        assert!(IlpLine::table(b, "ticks").is_ok());
        assert!(b.rewind_to_marker().is_ok());
    }
    assert_eq!(lib.as_bytes(), own.as_bytes());
    assert_eq!(lib.len(), own.len());
    assert!(!own.is_empty());
    own.clear();
    assert!(own.is_empty());
}

#[test]
fn test_is_retryable_only_for_a_transport_failure() {
    let send = |retryable| {
        TickIlpError::Send(RawSendError {
            message: String::new(),
            status: None,
            retryable,
        })
    };
    assert!(send(true).is_retryable());
    assert!(!send(false).is_retryable());
    assert!(!TickIlpError::Encoder("x").is_retryable());
    let mut b = Buffer::new(ProtocolVersion::V1);
    let refused = b.at(TimestampNanos::new(1)).map_err(TickIlpError::from);
    assert!(
        refused.is_err_and(|e| !e.is_retryable()),
        "an API refusal is not transport"
    );
}

#[test]
fn test_raw_request_timeout_matches_the_library_formula() {
    assert_eq!(raw_request_timeout(0), Duration::from_secs(5));
    assert_eq!(raw_request_timeout(102_400), Duration::from_secs(6));
    assert_eq!(
        raw_request_timeout(51_200),
        Duration::from_millis(5_500),
        "half of 100 KiB adds half a second"
    );
}

#[test]
fn test_flush_refuses_a_buffer_of_the_other_encoder() {
    let mut sender = TickIlpSender::Own(RawIlpSender::new("127.0.0.1", 9).expect("sender"));
    let mut lib = TickRowBuffer::new(TickIlpEncoder::Library);
    assert!(full_line(&mut lib, "x", 1, 1.0, 1, 1).is_ok());
    let before = lib.as_bytes().to_vec();
    let err = sender.flush(&mut lib).expect_err("mixed encoders");
    assert!(matches!(err, TickIlpError::Encoder(_)));
    assert_eq!(lib.as_bytes(), &before[..], "nothing is dropped");
}

// ---------------------------------------------------------------------------
// The HTTP request: a local server records what each sender sends
// ---------------------------------------------------------------------------

/// One request as a server sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    request_line: String,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// Serves `replies` (status line, extra headers, body) one per request on
/// keep-alive connections and records each request. Returns the port.
fn serve(replies: Vec<(&'static str, &'static str, &'static str)>) -> (u16, mpsc::Receiver<Seen>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = mpsc::channel();
    let replies = std::sync::Arc::new(std::sync::Mutex::new(replies.into_iter()));
    std::thread::spawn(move || {
        // One thread per connection: a client that keeps its connection open
        // (the library does) never blocks the next client.
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (tx, replies) = (tx.clone(), std::sync::Arc::clone(&replies));
            std::thread::spawn(move || serve_connection(stream, &tx, &replies));
        }
    });
    (port, rx)
}

type Replies = std::sync::Mutex<std::vec::IntoIter<(&'static str, &'static str, &'static str)>>;

fn serve_connection(stream: std::net::TcpStream, tx: &mpsc::Sender<Seen>, replies: &Replies) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut writer = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let mut content_type = None;
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
            if let Some((name, value)) = line.split_once(':') {
                let value = value.trim().to_owned();
                match name.to_ascii_lowercase().as_str() {
                    "content-type" => content_type = Some(value),
                    "content-length" => length = value.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let _ = tx.send(Seen {
            request_line: request_line.trim_end().to_owned(),
            content_type,
            body,
        });
        let next = replies.lock().ok().and_then(|mut r| r.next());
        let Some((status, headers, body)) = next else {
            return;
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        if writer.write_all(reply.as_bytes()).is_err() {
            return;
        }
    }
}

fn config(port: u16) -> tickvault_common::config::QuestDbConfig {
    tickvault_common::config::QuestDbConfig {
        host: "127.0.0.1".to_owned(),
        http_port: port,
        pg_port: 8812,
        ilp_port: 9009,
    }
}

fn sample_lines<B: IlpLine>(b: &mut B) {
    for i in 0_i64..50 {
        assert!(
            full_line(
                b,
                "NSE FNO,x=y",
                40_000 + i,
                #[allow(clippy::cast_precision_loss)]
                ((24_535 + 5 * i) as f64 / 100.0),
                1_793_300_000_000_000_000 + i * 1_000,
                1_793_300_000_000_000_000 + i,
            )
            .is_ok()
        );
    }
}

/// The owner's check: our sender's HTTP body, path, query and content type
/// are exactly what the library sends for the same rows.
#[test]
fn test_send_makes_the_same_request_as_the_library() {
    let (port, seen) = serve(vec![("204 No Content", "", ""), ("204 No Content", "", "")]);
    let conf = crate::tick_persistence::ticks_ilp_http_conf(&config(port));
    let mut library = questdb::ingress::Sender::from_conf(conf).expect("library sender");
    let mut lib_buffer = library.new_buffer();
    sample_lines(&mut lib_buffer);
    library.flush(&mut lib_buffer).expect("library flush");
    let from_library = seen
        .recv_timeout(Duration::from_secs(10))
        .expect("library request");

    let ours = RawIlpSender::new("127.0.0.1", port).expect("our sender");
    assert_eq!(ours.url(), format!("http://127.0.0.1:{port}/write"));
    let mut own = OwnIlpBuffer::new();
    sample_lines(&mut own);
    ours.send(&mut own).expect("our send");
    let from_us = seen
        .recv_timeout(Duration::from_secs(10))
        .expect("our request");

    assert_eq!(from_us, from_library);
    assert_eq!(
        from_us.body.iter().filter(|b| **b == b'\n').count(),
        50,
        "all fifty rows were sent"
    );
    assert!(from_us.request_line.starts_with("POST /write?precision=n "));
    assert!(
        own.is_empty(),
        "a sent buffer is cleared, as the library does"
    );
    assert!(lib_buffer.is_empty());
}

#[test]
fn test_send_maps_refusals_like_the_library_and_keeps_the_bytes() {
    let (port, _seen) = serve(vec![
        (
            "400 Bad Request",
            "Content-Type: application/json\r\n",
            r#"{"code":"invalid","message":"bad line 3"}"#,
        ),
        ("401 Unauthorized", "", "nope"),
        ("404 Not Found", "", ""),
        ("500 Internal Server Error", "", "disk full"),
        ("204 No Content", "", ""),
    ]);
    let ours = RawIlpSender::new("127.0.0.1", port).expect("sender");
    let mut b = OwnIlpBuffer::new();
    sample_lines(&mut b);
    let bytes = b.as_bytes().to_vec();
    let expect = |b: &mut OwnIlpBuffer, status: u16, text: &str| {
        let err = ours.send(b).expect_err("refused");
        assert_eq!(err.status, Some(status));
        assert!(!err.retryable, "a server answer is never retried");
        assert!(err.message.contains(text), "{status}: {}", err.message);
        assert_eq!(b.as_bytes(), &bytes[..], "the bytes stay for the rescue");
    };
    expect(&mut b, 400, "bad line 3");
    expect(&mut b, 401, "authentication error");
    expect(&mut b, 404, "does not support ILP");
    expect(&mut b, 500, "disk full");
    assert!(ours.send(&mut b).is_ok());
    assert!(b.is_empty());
}

#[test]
fn test_send_transport_failure_is_retryable_and_keeps_the_bytes() {
    // A port nothing listens on: bind, read the port, drop the listener.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let ours = RawIlpSender::new("127.0.0.1", port).expect("sender");
    let mut b = OwnIlpBuffer::new();
    sample_lines(&mut b);
    let bytes = b.as_bytes().to_vec();
    let err = ours.send(&mut b).expect_err("nothing listens");
    assert!(err.retryable, "a refused connection is the transport class");
    assert!(err.status.is_none());
    assert!(
        err.message
            .starts_with("Could not flush buffer: http://127.0.0.1:"),
        "names the endpoint, as the library does: {}",
        err.message
    );
    assert_eq!(b.as_bytes(), &bytes[..]);
    let wrapped = TickIlpError::Send(err);
    assert!(wrapped.is_retryable());
}

#[test]
fn test_send_refuses_a_half_written_row_and_sends_nothing_when_empty() {
    let ours = RawIlpSender::new("127.0.0.1", 9).expect("sender");
    let mut b = OwnIlpBuffer::new();
    assert!(ours.send(&mut b).is_ok(), "empty: nothing to send");
    assert!(IlpLine::table(&mut b, "t").is_ok());
    let err = ours.send(&mut b).expect_err("half row");
    assert!(!err.retryable);
}

#[test]
fn test_can_flush_only_between_rows() {
    let mut b = OwnIlpBuffer::new();
    assert!(b.can_flush(), "empty");
    assert!(IlpLine::table(&mut b, "t").is_ok());
    assert!(!b.can_flush(), "table written");
    assert!(IlpLine::column_i64(&mut b, "v", 1).is_ok());
    assert!(!b.can_flush(), "column written");
    assert!(IlpLine::at_nanos(&mut b, 1).is_ok());
    assert!(b.can_flush(), "row ended");
}

/// Every digit count from 1 to 20, at both ends of each length, against the
/// standard formatter (the library writes integers with `itoa`, which prints
/// the same text; the line proptest checks that end to end).
#[test]
fn test_push_u64_digits_matches_the_standard_formatter_at_every_length() {
    let mut values = vec![0_u64, 1, 9, u64::MAX, u64::MAX - 1];
    let mut p = 1_u64;
    while let Some(next) = p.checked_mul(10) {
        values.extend([p - 1, p, p + 1, next - 1]);
        p = next;
    }
    values.extend([p - 1, p, p + 1]);
    let mut out = Vec::new();
    for v in values {
        out.clear();
        out.extend_from_slice(b"x=");
        push_u64_digits(&mut out, v);
        assert_eq!(&out[2..], v.to_string().as_bytes(), "{v}");
    }
}
