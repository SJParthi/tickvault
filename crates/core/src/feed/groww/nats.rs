//! Native NATS wire-protocol framing for the Groww live feed (operator
//! 2026-06-19 "implement everything"; §32 native-Rust path) — zero new deps.
//!
//! Groww's feed is NATS-over-WebSocket: each WebSocket frame carries NATS
//! text-protocol bytes. This module is the pure, streaming **parser** for the
//! server→client frames we receive, plus the trivial client→server frame
//! **builders** we send. The NATS protocol is line-oriented (`\r\n`-terminated
//! control lines); `MSG` additionally carries a binary payload.
//!
//! Reference: NATS client protocol (<https://docs.nats.io/reference/reference-protocols/nats-protocol>).
//! The auth handshake (`CONNECT` with the nkey-signed nonce) lands in the
//! nkey/JWT slice — this module exposes the `INFO` nonce the signer needs.
//!
//! ## Guarantees (operator O(1) / worst-case demand)
//!
//! - **Streaming, zero-copy parse:** [`parse_frame`] borrows `subject`/`payload`
//!   from the input buffer (no copy) and reports bytes consumed, so the connector
//!   advances its read buffer. Returns `Ok(None)` when a frame is incomplete
//!   (need more bytes) — never blocks, never partial-consumes.
//! - **No panic on ANY input:** every index is bounds-checked; the error type
//!   [`NatsParseError`] is a `Copy` enum (no heap on the error path). A malformed
//!   `MSG` byte-count, non-UTF-8 subject, unknown verb, or an absurd payload size
//!   all return `Err`, never crash the feed task.
//! - **Bounded:** a `MSG` claiming more than [`MAX_MSG_PAYLOAD_BYTES`] is rejected
//!   (`PayloadTooLarge`) so a hostile/garbled length can never make us buffer
//!   unbounded memory.

/// NATS default max payload is 1 MiB; reject anything larger as malformed so a
/// corrupt byte-count cannot drive unbounded buffering.
pub const MAX_MSG_PAYLOAD_BYTES: usize = 1024 * 1024;

/// A parsed server→client NATS frame. `subject`/`payload` borrow the input buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatsServerOp<'a> {
    /// `INFO {json}` — the server handshake; `0` is the raw JSON bytes (carries
    /// the `nonce` the nkey signer needs).
    Info(&'a [u8]),
    /// `MSG <subject> <sid> [reply] <#bytes>` + payload.
    Msg {
        subject: &'a str,
        sid: &'a str,
        payload: &'a [u8],
    },
    /// `PING` — the client must reply `PONG`.
    Ping,
    /// `PONG` — reply to our `PING`.
    Pong,
    /// `+OK` — verbose-mode acknowledgement.
    Ok,
    /// `-ERR <message>` — a protocol error from the server.
    Err(&'a str),
}

/// A framing parse failure. `Copy` (no heap) so the error path is zero-alloc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatsParseError {
    /// A control line that is not a known verb.
    UnknownVerb,
    /// `MSG` control line did not have 3 or 4 space-separated arguments.
    MalformedMsgHeader,
    /// `MSG` byte-count token was not a valid integer.
    InvalidByteCount,
    /// `MSG` declared a payload larger than [`MAX_MSG_PAYLOAD_BYTES`].
    PayloadTooLarge,
    /// `MSG` payload was not terminated by `\r\n` where the byte-count said it ends.
    MissingPayloadTerminator,
    /// `subject`/`sid` were not valid UTF-8.
    NotUtf8,
}

impl core::fmt::Display for NatsParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::UnknownVerb => "nats parse: unknown verb",
            Self::MalformedMsgHeader => "nats parse: malformed MSG header",
            Self::InvalidByteCount => "nats parse: invalid MSG byte count",
            Self::PayloadTooLarge => "nats parse: MSG payload too large",
            Self::MissingPayloadTerminator => "nats parse: MSG payload not CRLF-terminated",
            Self::NotUtf8 => "nats parse: subject/sid not UTF-8",
        };
        f.write_str(s)
    }
}

impl std::error::Error for NatsParseError {}

/// Find the first `\r\n` in `buf`, returning the index of the `\r`.
#[inline]
fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Parse the next complete server frame from the front of `buf`.
///
/// Returns:
/// - `Ok(Some((op, consumed)))` — a full frame; the caller drops the first
///   `consumed` bytes.
/// - `Ok(None)` — the buffer does not yet contain a complete frame (read more).
/// - `Err(_)` — a protocol violation (the connector should close + reconnect).
///
/// Never panics, never allocates, never partially consumes.
pub fn parse_frame(buf: &[u8]) -> Result<Option<(NatsServerOp<'_>, usize)>, NatsParseError> {
    let Some(crlf) = find_crlf(buf) else {
        return Ok(None); // control line not yet complete
    };
    let line = &buf[..crlf];
    let after_line = crlf + 2; // past the \r\n

    // MSG <subject> <sid> [reply] <#bytes>\r\n<payload>\r\n
    if let Some(rest) = line.strip_prefix(b"MSG ") {
        return parse_msg(rest, buf, after_line);
    }
    if let Some(json) = line.strip_prefix(b"INFO ") {
        return Ok(Some((NatsServerOp::Info(json), after_line)));
    }
    if line == b"PING" {
        return Ok(Some((NatsServerOp::Ping, after_line)));
    }
    if line == b"PONG" {
        return Ok(Some((NatsServerOp::Pong, after_line)));
    }
    if line == b"+OK" {
        return Ok(Some((NatsServerOp::Ok, after_line)));
    }
    if let Some(err) = line.strip_prefix(b"-ERR") {
        // message may be space-padded and single-quoted; trim both.
        let msg = core::str::from_utf8(err)
            .map_err(|_| NatsParseError::NotUtf8)?
            .trim()
            .trim_matches('\'');
        return Ok(Some((NatsServerOp::Err(msg), after_line)));
    }
    Err(NatsParseError::UnknownVerb)
}

/// Parse a `MSG` body (everything after `MSG `) given the full buffer + the index
/// just past the control line's `\r\n`.
fn parse_msg<'a>(
    header: &'a [u8],
    buf: &'a [u8],
    payload_start: usize,
) -> Result<Option<(NatsServerOp<'a>, usize)>, NatsParseError> {
    let header_str = core::str::from_utf8(header).map_err(|_| NatsParseError::NotUtf8)?;
    let mut parts = header_str.split(' ').filter(|p| !p.is_empty());
    let subject = parts.next().ok_or(NatsParseError::MalformedMsgHeader)?;
    let sid = parts.next().ok_or(NatsParseError::MalformedMsgHeader)?;
    // third token is either <#bytes> (no reply) or <reply> then <#bytes>.
    let third = parts.next().ok_or(NatsParseError::MalformedMsgHeader)?;
    let nbytes_str = match parts.next() {
        Some(fourth) => fourth, // had a reply-to; fourth is the byte count
        None => third,          // no reply-to; third is the byte count
    };
    if parts.next().is_some() {
        return Err(NatsParseError::MalformedMsgHeader); // too many tokens
    }
    let nbytes: usize = nbytes_str
        .parse()
        .map_err(|_| NatsParseError::InvalidByteCount)?;
    if nbytes > MAX_MSG_PAYLOAD_BYTES {
        return Err(NatsParseError::PayloadTooLarge);
    }
    // need: payload (nbytes) + trailing \r\n
    let payload_end = payload_start
        .checked_add(nbytes)
        .ok_or(NatsParseError::PayloadTooLarge)?;
    let frame_end = payload_end
        .checked_add(2)
        .ok_or(NatsParseError::PayloadTooLarge)?;
    if buf.len() < frame_end {
        return Ok(None); // payload not fully arrived yet
    }
    if &buf[payload_end..frame_end] != b"\r\n" {
        return Err(NatsParseError::MissingPayloadTerminator);
    }
    let payload = &buf[payload_start..payload_end];
    Ok(Some((
        NatsServerOp::Msg {
            subject,
            sid,
            payload,
        },
        frame_end,
    )))
}

// --- Client → server frame builders (cold path: connect/subscribe-time) ---

/// `SUB <subject> <sid>\r\n` — subscribe `sid` to `subject`.
#[must_use]
pub fn build_sub(subject: &str, sid: &str) -> String {
    let mut s = String::with_capacity(5 + subject.len() + 1 + sid.len() + 2);
    s.push_str("SUB ");
    s.push_str(subject);
    s.push(' ');
    s.push_str(sid);
    s.push_str("\r\n");
    s
}

/// `UNSUB <sid>\r\n` — unsubscribe `sid`.
#[must_use]
pub fn build_unsub(sid: &str) -> String {
    let mut s = String::with_capacity(7 + sid.len() + 2);
    s.push_str("UNSUB ");
    s.push_str(sid);
    s.push_str("\r\n");
    s
}

/// `PING\r\n`.
#[must_use]
pub fn build_ping() -> &'static str {
    "PING\r\n"
}

/// `PONG\r\n` — the reply the client MUST send on receiving a `PING`.
#[must_use]
pub fn build_pong() -> &'static str {
    "PONG\r\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frame_ping_pong_ok() {
        assert_eq!(parse_frame(b"PING\r\n"), Ok(Some((NatsServerOp::Ping, 6))));
        assert_eq!(parse_frame(b"PONG\r\n"), Ok(Some((NatsServerOp::Pong, 6))));
        assert_eq!(parse_frame(b"+OK\r\n"), Ok(Some((NatsServerOp::Ok, 5))));
    }

    #[test]
    fn test_parse_info_returns_json_bytes() {
        let frame = b"INFO {\"nonce\":\"abc\",\"max_payload\":1048576}\r\n";
        let (op, consumed) = parse_frame(frame).expect("ok").expect("complete");
        assert_eq!(consumed, frame.len());
        match op {
            NatsServerOp::Info(json) => {
                assert!(json.starts_with(b"{\"nonce\""));
                assert!(json.ends_with(b"}"));
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_err_trims_quotes() {
        let (op, _) = parse_frame(b"-ERR 'Authorization Violation'\r\n")
            .expect("ok")
            .expect("complete");
        assert_eq!(op, NatsServerOp::Err("Authorization Violation"));
    }

    #[test]
    fn test_parse_msg_no_reply() {
        let frame = b"MSG /ld/eq/nse/price.1234 3 5\r\nhello\r\n";
        let (op, consumed) = parse_frame(frame).expect("ok").expect("complete");
        assert_eq!(consumed, frame.len());
        match op {
            NatsServerOp::Msg {
                subject,
                sid,
                payload,
            } => {
                assert_eq!(subject, "/ld/eq/nse/price.1234");
                assert_eq!(sid, "3");
                assert_eq!(payload, b"hello");
            }
            other => panic!("expected Msg, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_msg_with_reply_to() {
        // MSG subject sid reply nbytes
        let frame = b"MSG sub 7 _INBOX.x 2\r\nhi\r\n";
        let (op, _) = parse_frame(frame).expect("ok").expect("complete");
        match op {
            NatsServerOp::Msg {
                subject,
                sid,
                payload,
            } => {
                assert_eq!(subject, "sub");
                assert_eq!(sid, "7");
                assert_eq!(payload, b"hi");
            }
            other => panic!("expected Msg, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_msg_with_binary_payload_containing_crlf() {
        // payload bytes may themselves contain \r\n — the byte-count, not a scan,
        // delimits the payload. 4-byte payload [0x0d,0x0a,0x01,0x02].
        let mut frame = Vec::new();
        frame.extend_from_slice(b"MSG s 1 4\r\n");
        frame.extend_from_slice(&[0x0d, 0x0a, 0x01, 0x02]);
        frame.extend_from_slice(b"\r\n");
        let (op, consumed) = parse_frame(&frame).expect("ok").expect("complete");
        assert_eq!(consumed, frame.len());
        match op {
            NatsServerOp::Msg { payload, .. } => assert_eq!(payload, &[0x0d, 0x0a, 0x01, 0x02]),
            other => panic!("expected Msg, got {other:?}"),
        }
    }

    #[test]
    fn test_incomplete_control_line_returns_none() {
        assert_eq!(parse_frame(b"PI"), Ok(None));
        assert_eq!(parse_frame(b"MSG s 1 5"), Ok(None)); // no CRLF yet
    }

    #[test]
    fn test_incomplete_msg_payload_returns_none() {
        // header says 5 bytes but only 3 present → need more
        assert_eq!(parse_frame(b"MSG s 1 5\r\nhel"), Ok(None));
    }

    #[test]
    fn test_msg_missing_payload_terminator_errors() {
        // 5-byte payload present but followed by "XX" not "\r\n"
        assert_eq!(
            parse_frame(b"MSG s 1 5\r\nhelloXX"),
            Err(NatsParseError::MissingPayloadTerminator)
        );
    }

    #[test]
    fn test_msg_invalid_byte_count_errors() {
        assert_eq!(
            parse_frame(b"MSG s 1 notanumber\r\n"),
            Err(NatsParseError::InvalidByteCount)
        );
    }

    #[test]
    fn test_msg_too_few_tokens_errors() {
        assert_eq!(
            parse_frame(b"MSG onlysubject\r\n"),
            Err(NatsParseError::MalformedMsgHeader)
        );
    }

    #[test]
    fn test_msg_too_many_tokens_errors() {
        assert_eq!(
            parse_frame(b"MSG s 1 reply 5 extra\r\n"),
            Err(NatsParseError::MalformedMsgHeader)
        );
    }

    #[test]
    fn test_msg_payload_too_large_rejected_not_buffered() {
        // a byte-count above the 1 MiB cap is rejected immediately (never buffers).
        let huge = MAX_MSG_PAYLOAD_BYTES + 1;
        let frame = format!("MSG s 1 {huge}\r\n");
        assert_eq!(
            parse_frame(frame.as_bytes()),
            Err(NatsParseError::PayloadTooLarge)
        );
    }

    #[test]
    fn test_unknown_verb_errors() {
        assert_eq!(
            parse_frame(b"WAT foo\r\n"),
            Err(NatsParseError::UnknownVerb)
        );
    }

    #[test]
    fn test_non_utf8_subject_errors_not_panics() {
        // invalid UTF-8 in the MSG header
        let mut frame = Vec::new();
        frame.extend_from_slice(b"MSG ");
        frame.push(0xff); // invalid UTF-8 byte
        frame.extend_from_slice(b" 1 0\r\n\r\n");
        assert_eq!(parse_frame(&frame), Err(NatsParseError::NotUtf8));
    }

    #[test]
    fn test_zero_length_payload_msg() {
        let frame = b"MSG s 1 0\r\n\r\n";
        let (op, consumed) = parse_frame(frame).expect("ok").expect("complete");
        assert_eq!(consumed, frame.len());
        match op {
            NatsServerOp::Msg { payload, .. } => assert!(payload.is_empty()),
            other => panic!("expected Msg, got {other:?}"),
        }
    }

    #[test]
    fn test_two_frames_consumed_one_at_a_time() {
        let buf = b"PING\r\nPONG\r\n";
        let (op1, c1) = parse_frame(buf).expect("ok").expect("complete");
        assert_eq!(op1, NatsServerOp::Ping);
        assert_eq!(c1, 6);
        let (op2, c2) = parse_frame(&buf[c1..]).expect("ok").expect("complete");
        assert_eq!(op2, NatsServerOp::Pong);
        assert_eq!(c2, 6);
    }

    #[test]
    fn test_build_sub_build_unsub_build_ping_build_pong() {
        assert_eq!(
            build_sub("/ld/eq/nse/price.13", "1"),
            "SUB /ld/eq/nse/price.13 1\r\n"
        );
        assert_eq!(build_unsub("1"), "UNSUB 1\r\n");
        assert_eq!(build_ping(), "PING\r\n");
        assert_eq!(build_pong(), "PONG\r\n");
    }

    #[test]
    fn test_empty_buffer_is_none() {
        assert_eq!(parse_frame(b""), Ok(None));
    }

    #[test]
    fn test_err_with_no_crlf_yet_is_incomplete() {
        // a partial -ERR line (no CRLF) must be need-more, not a bad parse.
        assert_eq!(parse_frame(b"-ERR 'Auth"), Ok(None));
    }

    #[test]
    fn test_msg_header_with_double_space_parses() {
        // multiple spaces in the header collapse (filter !is_empty).
        let frame = b"MSG  sub  1  5\r\nhello\r\n";
        let (op, _) = parse_frame(frame).expect("ok").expect("complete");
        match op {
            NatsServerOp::Msg {
                subject,
                sid,
                payload,
            } => {
                assert_eq!(subject, "sub");
                assert_eq!(sid, "1");
                assert_eq!(payload, b"hello");
            }
            other => panic!("expected Msg, got {other:?}"),
        }
    }

    #[test]
    fn test_payload_crlf_does_not_misalign_next_frame() {
        // THE framing guarantee: a payload that contains \r\n (or a fake MSG line)
        // must not break parsing of the NEXT frame in the same buffer. Payload =
        // "MSG x 9 9\r\n" (10 bytes), then a real PING.
        let mut buf = Vec::new();
        let fake = b"MSG x 9 9\r\n"; // 11-byte payload that itself looks like a frame
        buf.extend_from_slice(format!("MSG s 1 {}\r\n", fake.len()).as_bytes());
        buf.extend_from_slice(fake);
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(b"PING\r\n");
        let (op1, consumed) = parse_frame(&buf).expect("ok").expect("complete");
        match op1 {
            NatsServerOp::Msg { payload, .. } => assert_eq!(payload, fake),
            other => panic!("expected Msg, got {other:?}"),
        }
        // the NEXT parse must see the real PING, not the fake frame in the payload.
        let (op2, _) = parse_frame(&buf[consumed..])
            .expect("ok")
            .expect("complete");
        assert_eq!(op2, NatsServerOp::Ping);
    }

    #[test]
    fn test_info_with_empty_body() {
        let (op, _) = parse_frame(b"INFO \r\n").expect("ok").expect("complete");
        assert_eq!(op, NatsServerOp::Info(b""));
    }

    #[test]
    fn test_parse_error_display_stable() {
        assert!(
            NatsParseError::PayloadTooLarge
                .to_string()
                .contains("too large")
        );
        assert!(
            NatsParseError::UnknownVerb
                .to_string()
                .contains("unknown verb")
        );
    }
}
