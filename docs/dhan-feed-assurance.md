# Dhan feed reliability: verified contract and remaining limits

Review date: 2026-09-13. Code baseline: `d78c431fe5842a87ccd74a7f350dbaedb3a2e346`; current workspace contains separately reviewed changes. This document is a read-only transport review, not a claim that every test below ran or that production has zero gaps.

## Which “superfast lane” is actually available?

The public documentation describes DhanHQ's API offering as Superfast APIs; this review found no separately documented “superfast lane” WebSocket endpoint or delivery SLA. Do not treat product language as a configuration switch or a nanosecond guarantee. The public site's sub-50 ms claim concerns **average order latency**, not market-feed capture or QuestDB commit. [DhanHQ product page](https://dhanhq.co/), [developer portal](https://docs.dhanhq.co/).

The configured main market endpoint is `wss://api-feed.dhan.co` (`config/base.toml:170`), matching the v2 feed endpoint. Public main-feed rules: at most five connections per user, 5,000 instruments per connection and 100 instruments per subscription JSON message. Authentication carries token, clientId, version=2 and authType=2. Server pings are documented every ten seconds; more than forty seconds without response can close the connection. The documented packet header contains no sequence/resume cursor, and the reviewed page specifies no replay API or lossless-delivery SLA. Therefore a client cannot prove recovery of every event never delivered to it. [Official live-feed documentation](https://dhanhq.co/docs/v2/live-market-feed/).

The depth endpoints are separate products, not an upgrade that proves main-feed tick completeness. Public depth docs limit 20-level subscriptions to fifty instruments/socket and 200-level subscriptions to one; they cover NSE equities and derivatives. Their current 200-level URL includes `/twohundreddepth`, whereas TickVault's URL tests use the host root. This discrepancy needs a vendor-confirmed compatibility check before any production URL change; the working deployment may intentionally use a legacy route. [Official depth documentation](https://dhanhq.co/docs/v2/full-market-depth/), `crates/core/src/websocket/connection.rs:1980`.

Access tokens generated via Dhan Web/TOTP are documented with 24-hour validity; Web-token renewal invalidates the old token and cannot renew an already expired token. Coordinate renewal with the existing token manager rather than generating competing tokens from an extra test client. [Official authentication documentation](https://dhanhq.co/docs/v2/authentication/).

Code 805 denotes excessive requests/connections and can lead to blocking; 804 is instrument-limit excess, 806 absent data subscription, and 807–810 concern expired/invalid authentication. Fast unconditional reconnect loops can worsen these conditions. [Official error annexure](https://dhanhq.co/docs/v2/annexure/).

## What TickVault already does

| Layer | Source-supported behavior | What it does not prove |
|---|---|---|
| Transport-only reader | `connection.rs` receives binary/control messages; `WalRingSink::accept` sends a refcounted frame toward WAL and a bounded live queue (`pool_supervisor.rs:3643–3735`). Database flushes are offloaded from the socket reader. | Merely enqueuing a WAL frame is not a disk durability acknowledgement. |
| Subscription pacing | Endpoint-specific batch limits (`pool_budget.rs:140–177`); 25 ms batch interval and pre-open readiness deadline of 09:12 (`pool_supervisor.rs:427–443`). | Other clients using the same account can still consume vendor connection allowance. |
| Bounded dialing/sending | 15 s dial timeout, 10 s subscribe-send timeout, 1 s client-ping-send timeout (`connection.rs:213,298,331`). | These are operation deadlines, not guaranteed reconnect completion. DNS/TLS/network/vendor rejection remains possible. |
| Heartbeats | Incoming Ping/Pong becomes KeepAlive, resetting traffic watchdog rather than falsely counting a quiet instrument as disconnected (`connection.rs:1690–1722`). Depth-200 additionally requests client-originated pings (`pool_budget.rs:274–329`). | A responsive heartbeat is not proof market data continues arriving. Source comments describe historic depth-200 heartbeat differences; current behavior should be measured. |
| Silence detection | Monotonic idle watchdog, nominal 27 s threshold checked every 1 s; separate five-minute frame-silence redial during applicable session (`idle_watchdog.rs`; `pool_supervisor.rs:107–178`). | A stalled runtime can delay the watchdog itself. Five minutes is a recovery policy, not zero-gap capture. |
| Reconnect handling | Ladder 0/1/2/5/15 s, then30s, with per-slot staggering up to375 ms and flap damping (`reconnect_ladder.rs:44–97`). Failure classification/token handling may impose other behavior. | Retry is intentionally controlled, not “never disconnect.” Re-subscription does not reconstruct missing intervening events. |
| TLS | Depth-200 uses no-ALPN profile; other endpoints use HTTP/1.1 profile (`connection.rs:1302–1310`). | Changing TLS uniformly as a supposed speed optimization can reintroduce failed upgrades. |
| Frame limits | Main frame ceiling is worst packet size ×5,000×2; depth20=256 KiB, depth200=512 KiB (`connection.rs:128–175`). Oversized frames are counted/refused. | A hard frame ceiling trades memory safety against rejecting unexpectedly large messages. |
| Local replay | WAL uses checksummed versioned frames and recorded receive/sequence information; downstream replay supports reconnect/process recovery (`ws_frame_spill.rs` header and `dhan_feed_stack.rs` replay path). | WAL cannot replay packets absent from its records. Ranking intentionally excludes historical WAL observations. |

## The architecture article supplied by the operator

Dhan's July 27, 2026 article places bounded per-client delivery queues **inside Dhan**, before the internet connection to external API clients. It says slow clients may receive the latest relevant state instead of a backlog of intermediate updates, depending on channel/update type. Its fast-versus-slow consumer discussion describes consumers keeping pace; it does not identify a separate subscription endpoint or entitlement. Internal exchange-sequence checks and replay systems are not evidence that those capabilities are exposed by the retail WebSocket. [Dhan architecture article](https://madefortrade.in/t/from-the-exchange-to-your-screen-how-dhan-built-a-fast-market-data-platform-for-retail-traders/92131), especially the architecture, backpressure and client-boundary sections.

**Implication for TickVault:** keep the reader fast and move database/analytics work off its critical path to reduce the chance of upstream coalescing. But a local WAL only records what reaches this client: it cannot recover intermediate events Dhan omitted before delivery. That is a demonstrated architectural limitation of the capture boundary, not proof this account has already lost a particular tick. The article does not promise zero disconnects or a client-visible sequence/retransmission contract. [Same primary article](https://madefortrade.in/t/from-the-exchange-to-your-screen-how-dhan-built-a-fast-market-data-platform-for-retail-traders/92131).

## Why zero tick loss cannot honestly be guaranteed

Separate the stages: vendor produced → socket received → WAL queued → file flushed → file synced → decoded → ILP acknowledged → QuestDB WAL applied → query visible. Equality at one pair does not certify all the others.

`ws_frame_spill.rs:1–39,547+` uses a bounded asynchronous writer and periodic `sync_all`, default 1,000 ms with an environment override permitting0 to disable syncing. A process killed before queued/buffered records flush can lose that in-memory tail; flushed OS-page-cache records survive a mere process crash but are still vulnerable to host failure before sync. The configured interval is not a strict worst-case age bound under disk stalls, queue backlog or failed syncs. Its successful sync/latency/error metrics must be examined. A WAL append metric alone cannot prove stable storage.

Queue capacity, frame limits, disk exhaustion, rejected packets, expired credentials, internet/host/vendor outages and upstream omission are explicit failure domains. Rust avoids neither all algorithmic faults nor network interruptions. “No unexplained local loss in a defined test envelope, with measured recovery and visible gaps” is an achievable acceptance target. “Every market event survives every failure with no reconnect” is unsupported.

## Safe 09:15 stress validation, without orders or vendor overload

1. Use an isolated Rust mock WebSocket server and a separate QuestDB instance/volume. Replay a recorded lawful session or synthetic protocol frames with unique test sequence IDs; never emit order API calls. Deny egress from the harness except its local dependencies.
2. Match production CPU architecture/config. Start before simulated 09:00; pace subscription acknowledgements and server pings, then generate the09:15 burst at measured peak,2× and bounded higher rates. Cap runtime/disk/RAM. Report workload and sample count rather than claiming all possible permutations.
3. Independently inject ping-only quiet periods, a live heartbeat with missing data, delayed/missing pongs, TCP resets, close805/807, bounded TLS/handshake failure, malformed/truncated/oversized frames and repeated reconnects. Assert subscribed identities and recovery time after each. Never send these fault patterns to Dhan.
4. Stall isolated DB and WAL disk independently; fill configured queues; kill/restart only the harness process; test simulated abrupt host failure separately. Compare generated IDs with received, synced and query-visible IDs, accounting separately for deliberately refused records, duplicates and unrecoverable source gaps.
5. At overlapping 1 s/3 s/5 s/1m ranking timers, measure read-loop service delay, ping-response age, queue bytes, WAL persisted/synced lag, DB apply lag and p50/p95/p99/p999/max latency. Exercise full population, all-active contracts and all-falling gainer filtering.
6. For real09:15 observation, use existing production subscriptions and passive metrics only. A second live client needs spare authorized connection budget; adding one can itself evict an existing connection. Confirm current account entitlement with Dhan before changing limits. Do not restart, force errors, rotate credentials or saturate vendor endpoints to create a test.

Record explicit pass/fail SLOs and failures. Run order entry disabled. No successful stress run can establish an absolute guarantee beyond its conditions, but a failed invariant is actionable evidence for a scoped repair.

## Completed AWS validation snapshot

The AWS targeted campaign passed 60 connection, 181 supervisor, 37 parser/stress and 3 WebSocket-WAL tests, with zero failures. These groups overlap the capture document and are included in the consolidated 760-test campaign. They establish finite fixture behavior for local transport/capture; they do not test Dhan under adversarial traffic, establish an upstream replay entitlement, or promise zero disconnects and every exchange update.

Source implementation remains undeployed. SQL read-only checks and healthy services are distinct from applying view DDL or deploying a new executable. The test evidence file records the checked source and binary hashes; the final Top Volume cutoff retest and timing campaign completed separately at 18:57:13 UTC.
