# FINAL SPEC reader-memory (ship=true)

## Summary
Ship, but smaller than first asked. The order is: measure first (M0), then set the receive buffer size per socket (A), then lock the program's code pages in memory only if M0 shows the reader thread taking major page faults (B, conditional). Replacing the frame ring with a pre-allocated queue (C) is dropped; only a DHAT test is added that records the ring's current allocation behaviour.

**Is the defect real?** Yes, but the evidence is weaker than the original ask implies.
- The 371,828 memory-throttle events came from one incident on 2026-09-03: a 21 GB spill file read whole, since fixed to stream. Verified from the comments in tickvault.service and boot_helpers.rs.
- Whether the cgroup memory throttle still hits during a session is Unknown. `oom_monitor` parses only the `oom_kill` line (Verified), and nothing publishes the throttle count, memory pressure, or the reader thread's major faults.
- No code sets SO_RCVBUF. Verified: the dial goes through `connect_async_tls_with_config` at connection.rs:1874, and tokio-tungstenite 0.29 calls `TcpStream::connect` internally.
- The host is tuned: `rmem_max` is 128 MiB and `tcp_rmem` is `4096 1048576 134217728` with automatic sizing on (Verified, `99-tickvault-net.conf`). Each socket starts at 1 MiB and is only bounded per socket. That conf records the per-socket size as "unfixed (PR #1738)", and 26 × 128 MiB = 3.25 GiB is what holds the depth account OFF.
- Nothing locks memory pages (no mlock), and the host has no swap.
- The frame ring is a tokio mpsc. Under backlog it allocates a 32-slot block per 32 frames (tokio `block.rs` / `list.rs::reclaim_block`, Verified). Tungstenite already allocates on the same thread for every message, so the ring adds little.

**Review points folded in:**
1. Do not set SO_RCVBUF when `rmem_max` is below the request. Any user value locks the buffer and turns off automatic sizing, so an untuned host would end up with about 416 KiB instead of autotuning from 1 MiB.
2. Ban `client_async_tls_with_config(request, tcp, None` in the existing explicit-config test. Without it, that test passes vacuously after the change and tungstenite's default 64 MiB message ceiling could silently come back.
3. Fix the M0 thread-id source:
   - `on_thread_start` is installed only when `pin_core` is `Some` (Verified, reader_runtime.rs:386).
   - There can be up to `MAX_WS_READER_THREADS` = 4 workers.
   - The runtime can be disabled.
   - Fix: walk `/proc/self/task/*/comm` for the exact name `tv-ws-reader` and sum the faults. This needs no libc.
4. Do not put any file I/O or `mlock` in `reader_runtime.rs`. `hot_path_no_blocking_guard` scans that whole file and bans `std::fs::`, `File::open(` and `.lock()` (Verified).
5. The kernel-tuning guard must keep `rmem_max × N` as the bound it enforces, because the rollback path restores automatic sizing up to 128 MiB per socket. It must also record `tcp_mem`, which the conf leaves at the kernel default (Verified unset).
6. `majflt` is the 10th whitespace token after the last `)` in the stat line.
7. A missing PSI file reads as "unavailable", never as 0.
8. The doubling of the buffer value on read-back applies only on Linux.
9. Strip IPv6 brackets from the host and keep the old error text for an address that resolves to nothing.
10. Keep `count_dial_failure("connect")` for both TCP and TLS failures, so the metric's labels do not change.
11. The rollback test asserts the returned outcome, not a value read back from the kernel.
12. Roll back with an env var read in core. This keeps `crates/common` untouched and avoids workspace-wide test escalation.
13. B locks only the program's code (`r-xp`) mappings, including the dynamic loader. It lives in a new boot-only module in core and measures before it locks.
14. If C is ever built, the last `Sender`'s Drop must wake the receiver.

**Dropped or reworded:**
- "1 MiB ≈ one second of open-hour traffic" is withdrawn. The repo's own figures suggest about 4 s per main socket. The per-kind sizes are now labelled Assumed and are to be re-checked against the existing receive-queue max gauge.
- "The reader is exposed because tungstenite allocates on every frame" is withdrawn. A malloc that reuses freed heap charges no new memory to the cgroup. The real exposure is new-page faults and major faults on file-backed pages.
- A is no longer claimed to absorb memory-throttle stalls. While the cgroup reclaims memory, the kernel signals socket pressure and stops growing the TCP window whatever SO_RCVBUF says (Assumed kernel behaviour). A reliably helps only with CPU, scheduling or lock stalls and global `tcp_mem` pressure.
- The order-update socket's 128 MiB in the worst-case sum stays as a conservative bound. Whether that socket is spawned at all was not checked.

**Rules this respects:**
- No new or changed Telegram or CloudWatch page, so the noise-lock file needs no row.
- No subscribe, redial, `ROTATION_HALTED` or 805 path is touched.
- Open sockets pick up the new buffer size only on a genuine reconnect or a restart. A deliberate redial to apply it would be a REJECT under the 2026-10-01 and 2026-10-02 scope-lock sections.
- Nothing is added per frame.

PROCEDURE: create `.claude/plans/active-plan-reader-memory.md`. It needs the 6 design-first sections (Design, Edge Cases, Failure Modes, Test Plan, Rollback, Observability), the 15-row and 7-row matrices, and must name the crates core, app and storage. Ship three serial PRs: PR1 = M0, PR2 = A, PR3 = B, and PR3 only if the gate in section B passes. C is a single DHAT test, added in PR1.

The 2026-10-06 operator quotes are "Go ahead with whatever you want dude" and "See do everything whatever is recommended dude okay?". Record them in the plan as the approval, and in the `Cargo.toml` libc comment if B ships. No noise-lock edit: no page is added or changed. If a throttle page is ever wanted, it needs §2.11 in `docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md` citing those quotes. Not recommended: HOT-PATH-STALL-01 already covers a reader stalled for 2 s or more.

========================================
M0 — MEASURE (PR1; zero hot-path cost)
========================================
M0.1 `crates/storage/src/oom_monitor.rs`
  - Add `pub struct MemoryEvents { pub high: Option<u64>, pub max: Option<u64>, pub oom_kill: Option<u64> }`.
  - Add `pub fn parse_memory_events(body: &str) -> MemoryEvents`.
    - Match the first token exactly: `oom` must not match `oom_kill`, and `high` must not match `max`.
    - Rewrite `parse_oom_kill_count` as a wrapper over it, so existing callers and tests are unchanged.
  - Add `pub fn memory_pressure_path_for(events_path: &Path) -> PathBuf`: the sibling `memory.pressure` file.
  - Add `pub fn parse_psi_avg10(body: &str) -> PsiAvg10 { some: Option<f64>, full: Option<f64> }`.
  - In the existing 60 s poll loop (all day, cold, O(lines)):
    - counters `tv_cgroup_memory_high_events_total` and `tv_cgroup_memory_max_events_total`: the change against the boot baseline, with the same reset handling as `classify_oom_delta`;
    - gauges `tv_cgroup_memory_pressure_some_avg10` and `tv_cgroup_memory_pressure_full_avg10`.
  - If the PSI file is absent or returns EOPNOTSUPP, leave the gauges UNSET (never 0), set `tv_cgroup_memory_pressure_available` = 0, and log one `info!` per process. Otherwise set it to 1.
  - Register both counters at 0 at boot.

M0.2 `crates/app/src/kernel_rx_queue_sampler.rs`
  - Inside the existing `spawn_blocking` sample (in session, 1 s; Verified at lines 333/346), add a reader-thread fault probe.
  - Pure function `pub fn parse_task_majflt(stat: &str) -> Option<u64>`:
    - take the text after the LAST `)`;
    - split on whitespace and read token index 9 (0-based), i.e. the 10th token, which is overall field 12.
  - Pure function `pub fn is_reader_worker_comm(comm: &str) -> bool`:
    - `comm.trim_end() == tickvault_core::websocket::reader_runtime::WS_READER_THREAD_NAME`.
    - That name is "tv-ws-reader", 12 characters. The blocking-pool threads are "tv-ws-reader-bk", 15 characters, which fits the kernel's 15-character name limit, so the exact match distinguishes them.
  - Each sample:
    - read `/proc/self/task`; for each task, read `comm`; on a match, read `stat` and sum `majflt`;
    - publish the gauge `tv_ws_reader_major_faults` (summed) and the counter `tv_ws_reader_major_faults_total` (non-negative change against the previous sample);
    - publish the gauge `tv_ws_reader_threads_found` (count);
    - when the count is 0 (reader runtime disabled or not built), leave `tv_ws_reader_major_faults` UNSET. Never publish 0.
  - Cost: O(threads in process), cold, on the blocking pool. Do NOT add libc to the app crate.
  - Doc note: this probe covers the session only, while the OOM monitor polls all day. A quiet gauge after hours is not evidence of zero faults.

M0.3 C measurement: new `crates/core/tests/dhat_frame_ring_backlog.rs`.
  - Build a `tokio::sync::mpsc::channel::<CapturedFrame>(FRAME_RING_CAPACITY)` the way the drain does. Import the real type and constant from where they live today; make them `pub` only if they are not already reachable.
  - Fill it with 65,536 frames and assert `blocks > 0`.
  - Name the test `frame_ring_allocates_blocks_under_backlog_known_limit`, with a header comment saying it records a LIMIT. If a future tokio change makes it fail (0 blocks), that is good news: flip the assertion and update the doc.
  - Document that "pre-reserve at boot" does not work: `reclaim_block` recycles only within 3 steps of the tail and frees the rest (Verified, tokio 1.53.1 `list.rs`).

========================================
A — SO_RCVBUF PER SOCKET (PR2; cold, once per dial)
========================================
A.1 `crates/core/src/websocket/connection.rs`, new pub constants. These are the values requested from the kernel; Linux stores double.

  | Constant | Requested | Effective on Linux |
  |---|---|---|
  | `MAIN_FEED_RCVBUF_REQUEST_BYTES: u32` | 32 MiB | 64 MiB |
  | `DEPTH20_RCVBUF_REQUEST_BYTES: u32` | 16 MiB | 32 MiB |
  | `DEPTH200_RCVBUF_REQUEST_BYTES: u32` | 8 MiB | 16 MiB |
  | `SYSCTL_RMEM_MAX_BYTES: u64` | 134_217_728 | (mirror of the conf) |

  - Doc: the sizes are Assumed. Re-check them against the `kernel_rx_queue_sampler` max gauge before calling them correct; they are not stall-seconds.
  - `pub const fn rcvbuf_request_bytes(endpoint: DhanEndpointType) -> u32`, an exhaustive match. An order-update endpoint, if it is in the enum, returns 0, meaning "not requested".
  - `const _: () = assert!(...)`: 2 × every request is at most `SYSCTL_RMEM_MAX_BYTES`.
  - Rollback env var `pub const WS_RCVBUF_ENV: &str = "TICKVAULT_WS_RCVBUF";`. A value of `off` disables the feature; absent or any other value enables it.
    - Resolve it ONCE into a `static OnceLock<bool>` through a pure `pub fn resolve_ws_rcvbuf_enabled(raw: Option<&str>) -> bool`, the same pattern as `resolve_ws_reader_threads`.
    - The params struct and `crates/common` are left alone.
  - `rmem_max` probe: a `static OnceLock<Option<u64>>` filled by a pure `pub fn parse_rmem_max(body: &str) -> Option<u64>` over `/proc/sys/net/core/rmem_max`.
    - Read once on the first dial. This is cold; it runs at dial time inside an async fn, so use `tokio::fs::read_to_string`.
    - On a non-Linux target it is `None`.

A.2 New enum:
  `pub enum RcvbufOutcome { NotRequested, Disabled, SkippedRmemMaxLow { rmem_max: u64 }, Applied { effective: u64 }, Clamped { effective: u64 }, SetFailed }`
  New pure classifier:
  `pub fn classify_rcvbuf_readback(requested: u32, readback: u64) -> RcvbufOutcome`
  - On Linux (`cfg(target_os = "linux")`), `Applied` when the read-back is at least 2 × requested.
  - Elsewhere, `Applied` when it is at least the requested value.
  - Otherwise `Clamped`.

A.3 New `async fn dial_tcp(host: &str, port: u16, request: u32) -> std::io::Result<(tokio::net::TcpStream, RcvbufOutcome)>`, mirroring tokio 1.53.1 `TcpStream::connect` (Verified, `stream.rs` 118–136):
  - Strip `[`/`]` from an IPv6 literal host, as tokio-tungstenite's private `domain()` does.
  - Run `tokio::net::lookup_host((host, port)).await?`.
  - Decide the request once:
    - feature disabled → `Disabled`;
    - `request == 0` → `NotRequested`;
    - `rmem_max` known and below 2 × request → `SkippedRmemMaxLow` (do NOT call setsockopt, so automatic sizing stays on);
    - otherwise, apply.
  - For each address, in order:
    - create `TcpSocket::new_v4()` or `new_v6()` to match the address;
    - if applying: call `set_recv_buffer_size(request)`, which must come before `connect`; on an error the outcome is `SetFailed` and the dial continues; otherwise read `recv_buffer_size()` back and classify it;
    - call `set_nodelay(true)?` and then `connect(addr).await`;
    - on success, return the stream and the outcome; on failure, remember the error.
  - When no address succeeds, return the last error. If resolution yielded nothing, return `io::Error::new(InvalidInput, "could not resolve to any address")`, tokio's text.
  - Never include the URL in an error. The URL carries the JWT.

A.4 In `DhanFeedSocketImpl::connect` (around line 1874), replace the single call with:
  - Take the host from `request.uri().host()`. If it is missing, call `count_dial_failure("bad_url")` and return `Err`.
  - Take the port from `request.uri().port_u16()`, defaulting to 443 for `wss` and 80 for `ws`.
  - Build the future: `async { let (tcp, outcome) = dial_tcp(host, port, rcvbuf_request_bytes(endpoint)).await.map_err(DialErr::Tcp)?; let (ws, resp) = client_async_tls_with_config(request, tcp, config, Some(connector)).await.map_err(DialErr::Ws)?; Ok((ws, resp, outcome)) }`.
  - Keep the existing `tokio::time::timeout(DIAL_TIMEOUT, ...)`.
  - A TCP failure and a WebSocket failure both call `count_dial_failure("connect")`, so the label set is unchanged. The `warn!` keeps `code = ErrorCode::WsGapConnectionState.code_str()`, `url = %self.loggable_url()` and `reason = %safe_err(..)`. Use a `std::io::Error` reason for the TCP case, without the URL.
  - Change the import to `client_async_tls_with_config`. The returned type is the same `WebSocketStream<MaybeTlsStream<TcpStream>>` (Verified). SNI and ALPN come from the connector, so the depth-200 no-ALPN profile is unchanged.
  - Move the existing Nagle comment into `dial_tcp`.
  - Record the outcome on success, once per dial:
    - counter `tv_dhan_ws_rcvbuf_total{outcome}` with outcome one of `applied`, `clamped`, `set_failed`, `skipped_rmem_max_low`, `disabled`, `not_requested`; seed all of them at 0 in `DhanSocketParams::new`, beside the dial-failure seeds;
    - gauge `tv_dhan_ws_rcvbuf_effective_bytes{kind=main_feed|depth20|depth200}`, three fixed series, set on `Applied` or `Clamped`.
  - Once per kind per process, guarded by a `static [AtomicBool; 3]`: on `Clamped`, `SetFailed` or `SkippedRmemMaxLow`, log `warn!(code = ErrorCode::HotPath04RcvbufNotApplied.code_str(), endpoint, requested, effective/rmem_max, ...)`.
  - A dial never fails because of a buffer setting.

A.5 `crates/common/src/error_code.rs`: add `HotPath04RcvbufNotApplied => "HOT-PATH-04"`, log-only like HOT-PATH-03 and with no `error_code_alerts` entry. Point `runbook_path()` at `docs/error-runbooks/hot-path-04-rcvbuf.md`. The runbook mentions "HOT-PATH-04" and says:
  - The dev Mac/Docker and CI hosts have the default `rmem_max` of 212,992, so `skipped_rmem_max_low` is expected there.
  - In production it means the sysctl did not apply.
  - The rollback is `TICKVAULT_WS_RCVBUF=off`.
  This touches common, so the error-code change ships in PR2 and runs the workspace tests.

A.6 The order-update socket (`order_update_connection.rs`, line 672) is unchanged.

A.7 `deploy/aws/sysctl/99-tickvault-net.conf` (no sysctl VALUE changes):
  - Replace the "app still does not set SO_RCVBUF" line with the per-kind sizes.
  - Re-derive the budget blocks:
    - configured case: 16 sockets = 5×64 + 5×32 + 5×16 + 1×128 (order update, conservative, possibly not spawned) = 688 MiB; 26 sockets = 688 + 5×32 + 5×16 = 928 MiB;
    - rollback/autotune case: still N × 128 MiB = 2.0 / 3.25 GiB.
  - Record that `net.ipv4.tcp_mem` is NOT set. The kernel default on 32 GiB is about 1.45 GiB pressure and 1.93 GiB limit (Assumed). The configured case fits under it; the rollback case does not, which is pre-existing.
  - State that socket memory is charged to the service cgroup and counts against `MemoryHigh` (Assumed).
  - The depth-account default stays OFF; that decision is the operator's.

========================================
B — LOCK CODE PAGES (PR3; ONLY IF the gate passes)
========================================
Gate: on at least one real session after M0, `tv_ws_reader_major_faults_total` increases during 09:00–15:40 IST, OR the operator explicitly asks for it as insurance. Do NOT use `mlockall`. With `MCL_FUTURE` it would lock the ~15.6 GiB replay peak and the queues, which reclaim cannot evict anyway on a host with no swap, so it would only harden the throttle sleep. `MCL_CURRENT` would force untouched statics resident, such as the 32 MiB `FRAME_FATE` table.

B.1 New boot-only module `crates/core/src/mem_lock.rs`. NOT in `reader_runtime.rs`, which `hot_path_no_blocking_guard` scans whole, and not in its `affinity` module, documented as "nothing else that needs unsafe".
  - Pure function `pub fn select_code_mappings(maps: &str, exe: &str) -> Vec<(usize, usize, String)>` keeps only `r-xp` lines whose path is one of:
    - the resolved exe;
    - a path containing `/libc.so`, `/libgcc_s.so`, or `/ld-linux` (aarch64 or x86-64).
  - It skips `[heap]`, `[stack]`, `[vdso]`, anonymous lines, `r--p` (relocated read-only pages become private dirty, and read-only data size is unknown), `rw-p` and `---p`.
  - A musl or static binary has no libc line; that must be handled.
  - `pub fn lock_code_pages(mode: CodeLockMode) -> CodeLockReport { mappings, bytes, locked_bytes, error: Option<i32> }`:
    - read `/proc/self/maps` and `read_link("/proc/self/exe")` once (cold);
    - mode `DryRun` sums only;
    - mode `Lock` calls `libc::mlock` per range inside one `#[allow(unsafe_code)]` block with a SAFETY comment; stop at the first error and record errno.
  - Env `TICKVAULT_LOCK_CODE_PAGES` = `off` | `dry` | `on`, pure resolver `resolve_code_lock_mode`.
    - Ship with default `dry` in the first deploy, read `tv_code_pages_bytes`, then flip the default to `on` in a follow-up commit once `LimitMEMLOCK` is sized.
  - Call it once from `main` immediately after `install_reader_runtime`, before `block_on`.
  - Gauges `tv_code_pages_bytes` and `tv_code_pages_locked_bytes`.
  - On an error (`EPERM` or `ENOMEM` from `RLIMIT_MEMLOCK`), log `warn!(code = ErrorCode::HotPath05CodePagesNotLocked.code_str(), errno, bytes, ...)` once; boot continues.
B.2 Add `HotPath05CodePagesNotLocked => "HOT-PATH-05"`, log-only, runbook `docs/error-runbooks/hot-path-05-code-lock.md` mentioning "HOT-PATH-05".
B.3 `deploy/systemd/tickvault.service`: `LimitMEMLOCK=<measured tv_code_pages_bytes rounded up + 25%>`, with a dated comment. Never `infinity`. Check `shell_budget_guard.rs`: a directive is not a shell line (Assumed; confirm).
B.4 `Cargo.toml` libc comment: record the second use (`mlock` of code pages, `crates/core/src/mem_lock.rs`) with the 2026-10-06 quotes. No new dependency.
B.5 Boot timing: prefaulting the code pages from a cold disk runs on the boot path. Log the lock duration and keep it inside the boot-step deadlines; prefault cost scales with text size, which DryRun reports first.

========================================
C — NOT BUILT
========================================
Only M0.3. Record in the plan, so a future build does not repeat the mistakes:
  - Value is about 2,048 block allocations (about 5–6 MB, Assumed) per full-ring backlog episode, and 0 in steady state.
  - The risk is hand-written wake-up code under the drain's biased `select!`, its `None => break` closed-channel detection, and the `RingByteBudget` rule that a granted reservation is a held slot.
  - If it is ever built, the sketch is `crossbeam_channel::bounded(FRAME_RING_CAPACITY)` plus `futures_util::task::AtomicWaker`:
    - sender: `try_send`, then `wake`;
    - receiver: `poll_fn` doing `try_recv`; if empty, `register`, then `try_recv` again; disconnected returns `Ready(None)`;
    - the last `Sender`'s Drop MUST call `wake()`, through a shared sender count, or the drain hangs at shutdown or pool teardown;
    - DHAT target: 0 blocks for 65,536 queued frames plus 1,000 refused sends.

========================================
CLAUDE.md O(1) table
========================================
- PR2: amend the `connection.rs` / `reader_runtime.rs` rows: SO_RCVBUF is set once per dial, O(addresses), cold.
- PR3: add a `crates/core/src/mem_lock.rs::lock_code_pages` row: O(mappings), once at boot, locks only `r-xp` code.
- PR1: add a row for the M0 probes: O(threads) per second in session on the blocking pool, and O(lines) per 60 s.

## Tests
["M0 `oom_monitor.rs`: `parse_memory_events` on a realistic fixture (low, high, max, oom, oom_kill) gives high, max and oom_kill exactly, with `oom` not taken as `oom_kill` and `high` not as `max`. Malformed lines are skipped. Existing `parse_oom_kill_count` tests stay green.","M0: `parse_psi_avg10` reads `some avg10=1.23 ...` and `full avg10=0.45 ...` correctly. An empty or garbage body gives `None`.","M0: a missing `memory.pressure` file leaves the PSI gauges UNSET and sets `tv_cgroup_memory_pressure_available` = 0. It never publishes 0 (false-OK pin).","M0: the high/max counters are seeded at 0 at boot, and a counter reset is handled the way `classify_oom_delta` handles it.","M0 `kernel_rx_queue_sampler.rs`: `parse_task_majflt` on a fixture whose name contains spaces and parentheses, e.g. `1234 (tv-ws (reader)) S 1 ...`, returns the value of overall field 12. The fixture gives minflt, cminflt, majflt and utime distinct values, so a wrong offset fails.","M0: `is_reader_worker_comm` accepts `tv-ws-reader\\n` and rejects `tv-ws-reader-bk` and `tokio-runtime-w`.","M0: when zero reader threads are found, `tv_ws_reader_major_faults` is left unset and `tv_ws_reader_threads_found` = 0. With two matching tasks, their faults are summed.","M0.3 `crates/core/tests/dhat_frame_ring_backlog.rs`: 65,536 `CapturedFrame`s queued on the tokio mpsc observe blocks > 0. This is a known limit, documented as such.","A `connection.rs`: `rcvbuf_request_bytes` returns 32/16/8 MiB for main feed, depth-20 and depth-200, and the const assert keeps 2 × request at or below `SYSCTL_RMEM_MAX_BYTES`.","A: `resolve_ws_rcvbuf_enabled` returns false for `off`, true for None and other values. `parse_rmem_max` handles `134217728\\n`, `212992` and garbage.","A: `classify_rcvbuf_readback` on Linux gives Applied when the read-back is at least 2n, otherwise Clamped. A non-Linux cfg test gives Applied when it is at least n.","A integration `crates/core/tests/ws_rcvbuf_dial.rs` (loopback `TcpListener`): `dial_tcp(\"127.0.0.1\", port, 1 MiB)` connects and returns Applied, Clamped or SkippedRmemMaxLow, each consistent with the host's `/proc/sys/net/core/rmem_max`. It never fails on CI's 212,992 default.","A: `dial_tcp` with request 0 returns `NotRequested`, and with the env disabled returns `Disabled`. The test asserts the returned outcome, not a value read back from the kernel (avoids a flaky check).","A: the rmem_max-low path issues no setsockopt and returns `SkippedRmemMaxLow` (inject `rmem_max` through the pure decision function `decide_rcvbuf(request, enabled, rmem_max)`).","A: multiple addresses (`localhost` resolving to ::1 refused and 127.0.0.1 listening, or a test-only injected address list) fall through to the second. When every address fails, the last error is returned. An empty resolution returns 'could not resolve to any address'.","A: an IPv6 bracketed host `[::1]` is stripped before `lookup_host` (unit test of the host-normalising helper).","A source scan, `connection.rs` internal test `test_the_production_half_never_dials_without_an_explicit_config`: also forbid `client_async_tls_with_config(request, tcp, None` and `client_async(` in production code, and require `client_async_tls_with_config(request, tcp, config,`. Add a bite self-test on a synthetic string.","A guard `crates/core/tests/ws_nagle_disabled_guard.rs`, split per site: `connection.rs` must contain `client_async_tls_with_config(`, and inside `fn dial_tcp` `set_recv_buffer_size(` and `.set_nodelay(true)` must both precede `.connect(`, after `//` comments are stripped. `order_update_connection.rs` keeps `connect_async_tls_with_config(request, None, true,`. Add a bite self-test with `set_nodelay(false)`.","A guard `crates/app/tests/kernel_tuning_16ws_guard.rs`: ENFORCED bound stays `rmem_max × sockets` (the rollback case). Add a configured-case calculation from the pub core constants (688/928 MiB) and assert it is at or below the recorded `ASSUMED_TCP_MEM_PRESSURE_BYTES` (~1.45 GiB, labelled Assumed). The conf text matches both figures. The depth-account gate test still holds the default OFF.","A: `tv_dhan_ws_rcvbuf_total` outcome labels are seeded at 0 in `DhanSocketParams::new`. The HOT-PATH-04 warn fires once per kind across two dials (static latch).","A: error-code cross-reference/tag guards pass for HOT-PATH-04 (`error_code.rs`, `runbook_path`, runbook file mentions the code).","B `mem_lock.rs` unit: `select_code_mappings` on a `/proc/self/maps` fixture keeps the exe's r-xp, libc's r-xp, libgcc's r-xp and ld-linux's r-xp. It rejects [heap], [stack], [vdso], anonymous, r--p, rw-p and ---p, and a fixture with no libc (musl) works.","B: `resolve_code_lock_mode` maps off, dry, on and absent to the documented defaults.","B integration (Linux): `lock_code_pages(DryRun)` reports bytes > 0 and locks nothing. `lock_code_pages(Lock)` either locks more than 0 bytes or returns errno EPERM/ENOMEM under CI's memlock limit, and never panics.","B: HOT-PATH-05 cross-reference, plus `hot_path_no_blocking_guard` still green (`mem_lock.rs` is not a scanned target, `reader_runtime.rs` unchanged).","No DHAT test is needed for A or B: both run once per dial or once per boot, never per frame."]

## Guards to update
["crates/core/src/websocket/connection.rs internal test test_the_production_half_never_dials_without_an_explicit_config — add bans on `client_async_tls_with_config(request, tcp, None` and `client_async(`, require the config-carrying call, bite self-test","crates/core/tests/ws_nagle_disabled_guard.rs — split per site; connection.rs requires client_async_tls_with_config + set_recv_buffer_size and set_nodelay(true) before .connect( inside dial_tcp; order_update_connection.rs unchanged; bite self-test","crates/app/tests/kernel_tuning_16ws_guard.rs — keep rmem_max×N as the enforced bound (rollback case), add configured-case figure from pub core constants vs recorded ASSUMED_TCP_MEM_PRESSURE_BYTES, re-derived conf text; depth-account default stays OFF","crates/common error_code tag + rule-file/runbook crossref guards — HOT-PATH-04 (PR2), HOT-PATH-05 (PR3 only)","crates/common/tests/hot_path_no_blocking_guard.rs — no edit; must stay green (no fs/lock/mlock added to reader_runtime.rs; mem_lock.rs is boot-only and not a target)","crates/app/tests/alarmed_counters_are_seeded_guard.rs and resilience_sla_alert_guard / operator_health_dashboard_guard — confirm new counters (tv_cgroup_memory_*_events_total, tv_ws_reader_major_faults_total, tv_dhan_ws_rcvbuf_total) are seeded at 0 and do not trigger a required alarm","crates/common/tests/shell_budget_guard.rs — confirm LimitMEMLOCK directive in tickvault.service is not counted as a shell line (PR3 only)"]

## Residual risks
["The defect's live-session impact is Unknown until M0 runs. The 371,828 throttle events are from one incident that has since been fixed. B is gated on M0; A ships on its own merits (bounded budget, unlocks the depth-account headroom).","A does not reliably protect against stalls caused by the cgroup memory throttle. While the cgroup reclaims memory, the kernel signals socket pressure, stops growing the window and may prune the queue whatever SO_RCVBUF says (Assumed kernel behaviour). It protects against CPU, scheduling or lock stalls and global tcp_mem pressure.","A fixed SO_RCVBUF turns off automatic sizing for that socket. If 64 MiB is below a burst that autotuning would have reached (up to 128 MiB), the fixed size is worse. Assumed unlikely. Watch `kernel_rx_queue_max` against `tv_dhan_ws_rcvbuf_effective_bytes`; roll back with `TICKVAULT_WS_RCVBUF=off`.","The per-kind sizes (64/32/16 MiB effective) are Assumed: the per-socket byte rate was not measured, and usable payload is maybe 50–75% of the buffer because of per-packet overhead.","Already-open sockets keep their old buffers until a genuine reconnect or a restart. A deliberate redial to apply the change is forbidden by the 2026-10-01 and 2026-10-02 scope-lock sections.","The rollback or autotune case (N × 128 MiB = 2.0/3.25 GiB) still exceeds the default net.ipv4.tcp_mem (Assumed ~1.45/1.93 GiB on 32 GiB). That is pre-existing, not introduced here. Setting tcp_mem explicitly is a separate, unmeasured host change.","The rewritten dial must keep exact parity with TcpStream::connect (address order, last error, empty-resolution text) and with tungstenite's host and port handling. A bug fails a dial that works today. Covered by the loopback tests, but production DNS behaviour cannot be fully reproduced in CI.","On dev and CI hosts HOT-PATH-04 logs once per kind (`skipped_rmem_max_low`). Expected, and documented in the runbook.","B, if shipped: LimitMEMLOCK must be sized from the measured DryRun gauge. Locked code pages are never reclaimed. Prefaulting runs on the boot path. B does nothing for the throttle sleep caused by the reader's own new-page faults; the only cure is staying below memory.high (the WAL catch-up stand-down at 60% is the existing defence).","C stays unbuilt. The ring still allocates about one block per 32 frames while a backlog grows (recorded by the DHAT limit test).","The `resilience_sla_alert_guard`/`operator_health_dashboard_guard` name-matching rules for new `tv_*_total` counters were not checked (Assumed). If either guard demands an alarm for a new counter, rename or exempt the counter rather than add a page. Adding a page would need a noise-lock §2.11 row."]

