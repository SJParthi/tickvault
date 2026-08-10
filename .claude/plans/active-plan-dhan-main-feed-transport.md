# Implementation Plan: Dhan main-feed WebSocket transport (the missing socket)

**Status:** DRAFT
**Date:** 2026-08-10
**Approved by:** pending — Parthiban
**Authority:** `.claude/rules/project/websocket-connection-scope-lock.md`, the two dated
2026-08-09 operator quotes: the revival quote (*"…C — revive Dhan live WS…"*, reversing the
2026-07-13 retirement) and the same-day 16-connection quote (*"our idea is toa dd live feed so
oevrall 16 websocket conbections rigth"*), which lifted the depth-20/depth-200 ban and raised
the main-feed pool from 1 to 5.
**Guarantee matrices:** carried by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row), per that file's
cross-reference clause.

---

## What is actually missing (measured, not assumed)

The gap is far narrower than "rebuild the deleted feed". Verified in source 2026-08-10:

| Layer | State |
|---|---|
| Binary parsers (header, ticker, quote, full, oi, prev_close, disconnect, market_status, depth) | **INTACT** — survived the July deletions, DHAT-gated, `dispatch_frame` at `parser/dispatcher.rs:156` |
| Pool budgeting, slot admission | **INTACT** — `websocket/pool_budget.rs` |
| Reconnect state machine, backoff ladder, idle watchdog | **INTACT** — `pool_supervisor.rs:344`, `reconnect_ladder.rs:119`, `idle_watchdog.rs:145` |
| Subscribe batching + confirm/lost tracking | **INTACT** — `SubscribeGuard`, `pool_supervisor.rs:625` |
| WAL-before-broadcast frame sink | **INTACT** — `FrameSink`/`WalRingSink`, `pool_supervisor.rs:766`/`:788` |
| Connection driver loop | **INTACT** — `run_connection`, `pool_supervisor.rs:1012` |
| Planning / gauge publication | **INTACT** — `app/src/dhan_feed_stack.rs` |
| **The socket itself** | **MISSING** — nothing implements the `DhanFeedSocket` trait (`pool_supervisor.rs:968`) |

So this plan implements **one trait**, not a subsystem. `dhan_feed_stack.rs:448` currently
emits a loud error and pins `tv_dhan_feed_stack_up` at 0 precisely because that impl is absent.

---

## Design

**Shape.** Add `crates/core/src/websocket/main_feed_socket.rs` providing `MainFeedSocket`, a
concrete `DhanFeedSocket`. `run_connection` already owns the lifecycle (connect → subscribe →
read → classify → reconnect), so the impl supplies only: `connect`, `subscribe`, `next_event`,
`close`. Everything else is existing, tested code.

**The ratchet is updated, not evaded.** `crates/core/tests/dhan_live_ws_retired_guard.rs`
hard-fails on a recreated `connection.rs` (`:24`), on the literal `api-feed.dhan.co` anywhere in
`crates/core/src` (`:77`), and on `pub mod connection;` in `mod.rs` (`:95`). Those pins enforce
the 2026-07-13 retirement that the 2026-08-09 quotes **reverse**, so the correct move is to
retire the guard's main-feed rows with a dated in-file note citing those quotes. Choosing a
different filename to slip past `:24` while leaving the guard "green" would be gaming a ratchet
into a false-OK — explicitly rejected. The endpoint literal stays out of `crates/core/src`
regardless: the base URL is already config-driven (`config.rs:2697`, `config/base.toml:148`),
so `:77` is satisfied on its merits, not by exemption.

**Connect.** `wss://<configured-base>?version=2&token=<JWT>&clientId=<id>&authType=2`
(`WEBSOCKET_PROTOCOL_VERSION`, `WEBSOCKET_AUTH_TYPE`, `constants.rs:92`/`:89`). TLS via the
existing `tls::build_websocket_tls_connector`. The token is a `Secret<String>` read at dial
time and never logged; `sanitize.rs:440` already redacts these two params.

**Subscribe.** Reuse `SubscriptionRequest` (`websocket/types.rs:211`). `SecurityId` is a
**String** on the wire (`:194`) — an int would be silently rejected. Quote mode
(`RequestCode 17`) per the daily-universe lock, batched at `SUBSCRIPTION_BATCH_SIZE = 100`
via `SubscribeGuard::batches`.

**Read → parse → publish.** Each frame goes to `FrameSink::accept` **before** any parse
(capture-at-receipt: WAL → ring → spill → DLQ), then `dispatch_frame(raw, received_at_nanos)`,
then the resulting `ParsedTick` is published on the existing
`broadcast::Sender<ParsedTick>` (`main.rs:2441`, publisher-less today — this plan makes it a
publisher). Ordering is non-negotiable: a parse panic or a full ring must never cost a frame
that was already durably captured.

**Byte offsets.** The parsers are intact and already correct; this plan adds no offset
arithmetic. Recorded because it was mid-flight uncertainty: the doc-vs-code offset hazard is
**partially true**. `docs/dhan-ref/03-live-market-feed-websocket.md:174` gives quote ATP as
bytes 19–22 while `QUOTE_OFFSET_ATP = 18` (`constants.rs:2835`) — 1-based doc, 0-based code,
confirmed. But that same doc is internally inconsistent: its §7 tables print the header range
as `0-8` (nine values under either convention) while §7.5 and §11 are explicitly 0-based. Every
existing parser matches the docs after the −1 conversion. **Implication for this plan: do not
"fix" any offset against the prose.** The parsers plus their locked-facts pins
(`crates/common/tests/dhan_locked_facts.rs:58-110`) are the authority.

**Scope held to 1 connection in this plan.** The quote authorizes 16, and `pool_budget`
already models 5-per-endpoint-type independently (Dhan support confirmed 2026-04-06,
`constants.rs:44-53`). But one working socket is the unit that proves the design; fanning to 5
main-feed + 5 depth-20 + 5 depth-200 is a follow-up whose risk is entirely in the pool, not the
socket. Depth stays off here: `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL` has an unresolved
root-path-vs-`/twohundreddepth` conflict (`constants.rs:1561` vs `04-...md:37-59`), neither
side re-verified, and guessing it would burn a live probe.

## Edge Cases

- **Frame stacking.** Multiple packets in one WS message. Undocumented for the main feed; a
  splitter exists only for depth (`split_depth_frame`). Loop on the header's `message_length`
  (u16 LE at `[1..3]`) until the buffer is drained; a residue shorter than a header is a
  counted, logged anomaly, never a silent discard.
- **`message_length` untrusted.** Treated as adversarial input: bounds-check against the
  remaining buffer before slicing. `MAX_WEBSOCKET_FRAME_SIZE = 65536` is **our assumption**
  (`constants.rs:41`), not a documented Dhan value — recorded as such.
- **Zero-length / non-binary frames.** Text frames and pings are not ticks; pong within 40 s of
  a server ping (server pings every 10 s, `constants.rs:78`/`:81`).
- **No snapshot on subscribe.** Only the code-6 previous-close is auto-sent, and per Ticket
  #5525125 that is IDX_I-only. A freshly subscribed instrument is legitimately silent until it
  trades — silence must not be read as a fault before first tick.
- **Unknown response code.** Counted and dropped, never a panic (annexure rule 15).
- **Token expiry mid-stream** (disconnect 807): refresh then reconnect, not a bare retry.
- **Market-hours boundary.** `defer_until_market_open_ist` before the first dial, mirroring
  `order_update_connection.rs:117`; post-close sleep rather than a reconnect flap.

## Failure Modes

| Mode | Behaviour |
|---|---|
| Disconnect 800 / 807 | **Recoverable** — backoff reconnect; 807 refreshes the token first (`types.rs:145-158`) |
| 804, 805, 806, 808–814 | **Terminal** — stop, page, do not flap. 805 means a 6th connection killed the oldest: reconnecting would evict a healthy peer |
| Unknown disconnect code | Treated as transient (`types.rs:150`), bounded by the attempt cap |
| Parse error | Counted per code, frame already WAL'd, connection survives — one malformed packet never kills a socket |
| Ring full | Frame spills to NDJSON then DLQ; capture-at-receipt means the frame is already durable before the ring is consulted |
| **Silent packet loss** | **UNDETECTABLE, stated plainly.** The 8-byte main-feed header carries **no sequence number** (`constants.rs:2794-2803`); one exists only in the 12-byte depth header. There is also no subscribe ACK. So the transport cannot prove completeness, and this plan does not claim it. The 15:31 REST cross-verify is the only ground truth, and must be live from day one |

## Test Plan

- Unit, pure: URL construction (token/clientId redacted in every rendering), subscribe-payload
  shape incl. **string** SecurityId, frame-splitting on `message_length` (single, stacked,
  truncated, oversized, zero-length), disconnect-code classification for all 13 codes.
- Property (`proptest`): arbitrary byte buffers into the split+dispatch path — never panics,
  never slices out of bounds.
- DHAT zero-alloc: the read→WAL→dispatch path across 10k frames, against the existing hot-path
  budget. This is the principle-1 gate and is non-negotiable.
- Integration: a local `wss` stub that replays captured frame fixtures — connect, subscribe,
  stream, mid-stream disconnect, reconnect, resubscribe.
- Ratchet: the `dhan_live_ws_retired_guard` rows retired with a dated note; a new guard pinning
  that `FrameSink::accept` precedes `dispatch_frame` in source order (capture-at-receipt is the
  zero-loss guarantee — inverting it must fail the build).
- Bite-test every new guard by mutation before claiming it passes.

## Rollback

Config flip, no revert needed: the stack is gated (`dhan_feed_stack.rs:127`) and
`dhan_enabled = false` in base + production config today. Setting it false returns the process
to byte-identical current behaviour — the socket is never constructed. The `FEED_STACK_SPAWNED`
once-guard (`:86`) means no partial second stack can exist. Code rollback is a single revert:
nothing outside `websocket/` and the guard file changes.

## Observability

- `tv_dhan_feed_stack_up` moves 0 → 1 **only** when a socket is streaming — not when one is
  planned. `test_stack_never_reports_itself_up` (`dhan_feed_stack.rs:771`) must be retired in
  the same commit that earns the 1, or the gauge becomes a lie.
- Existing `ws_event_audit` lifecycle rows (Connected / Disconnected / Reconnected / Sleep)
  via the same consumer the order-update channel now uses.
- Per-code parse-error counters; frames-received and ticks-published counters; the existing
  heartbeat gauge via `activity_watchdog`.
- `/health` websocket row flips from `retired` to live reporting on first setter call
  (arm-on-arrival, `handlers/health.rs:62-76`).
- **No new CloudWatch alarm in this plan.** Alarms land with the pool fan-out, when the
  counters have a sustained baseline. An alarm on a metric with no history is noise, and an
  alarm on a code path with no call site is the false-OK class the charter forbids.

## Plan Items

- [ ] Item 1 — `MainFeedSocket` implementing `DhanFeedSocket` (connect/subscribe/next_event/close)
  - Files: `crates/core/src/websocket/main_feed_socket.rs`, `crates/core/src/websocket/mod.rs`
  - Tests: `test_connect_url_redacts_token_and_client_id`, `test_subscribe_payload_security_id_is_string`, `test_subscribe_batches_at_100`
- [ ] Item 2 — frame splitting + dispatch, capture-at-receipt ordering
  - Files: `crates/core/src/websocket/main_feed_socket.rs`
  - Tests: `test_split_stacked_frames`, `test_truncated_frame_is_counted_not_panicked`, `test_oversized_message_length_is_rejected`, `proptest_arbitrary_bytes_never_panic`
- [ ] Item 3 — disconnect classification + reconnect/token-refresh wiring
  - Files: `crates/core/src/websocket/main_feed_socket.rs`
  - Tests: `test_807_triggers_token_refresh_before_reconnect`, `test_805_is_terminal_not_retried`
- [ ] Item 4 — wire the socket into `dhan_feed_stack`, publish ticks, flip the up-gauge
  - Files: `crates/app/src/dhan_feed_stack.rs`
  - Tests: `test_stack_reports_up_only_when_streaming`, `test_ticks_reach_the_broadcast_channel`
- [ ] Item 5 — retire the superseded ratchet rows with the dated 2026-08-09 authority; add the capture-at-receipt ordering guard
  - Files: `crates/core/tests/dhan_live_ws_retired_guard.rs`, `crates/core/tests/main_feed_capture_order_guard.rs`
  - Tests: `test_capture_precedes_parse_in_source_order`
- [ ] Item 6 — DHAT zero-alloc gate on the read path
  - Files: `crates/core/tests/dhat_main_feed_read_path.rs`
  - Tests: `test_read_path_zero_alloc_over_10k_frames`

## Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: every frame we RECEIVE is
durably captured before parse (WAL → ring → NDJSON spill → DLQ, DEDUP-idempotent replay), the
read path is DHAT-gated zero-alloc, and parse is fixed-offset `from_le_bytes` with no loop.

**NOT claimed:** completeness. The main feed has no sequence number and no subscribe ACK, so
silent packet loss is undetectable at the protocol level — the 15:31 REST cross-verify is the
only ground truth. **NOT claimed:** that this fixes the reason the feed was retired on
2026-07-13. That retirement was driven by Dhan-side behaviour measured 2026-07-06 — p99
delivery lag 46.37 s (max 198.69 s) against Groww's 562 ms on the same host in the same
minutes, 29–67 silent instruments per minute, and live-vs-historical candle mismatches. Every
cause is upstream; reviving the lane repairs none of it. The operator accepted this knowingly
in the 2026-08-09 quote, which named the reason back. What this plan ships is the mismatch
DETECTION, not a claim the mismatch is gone.
