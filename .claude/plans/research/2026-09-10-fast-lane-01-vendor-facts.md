# Fast-lane research 01 — vendor facts (2026-09-10)

`UP/` = operator pack at `/tmp/claude-0/…/scratchpad/upload/`. **The Dhan engineering post was UNREACHABLE** (`madefortrade.in` and `web.archive.org` both CONNECT 403 — `docs/dhan-ref/verification-2026-07-13.md:30,32`), so post-attributed claims are **Assumed (secondary)**: this repo's prior quotations, not a fresh read.

## 1. Slow consumer

| Question | Answer | Label | Evidence |
|---|---|---|---|
| Slow-consumer treatment | Isolated queue; slow client "catch[es] up with the latest available state" — intermediate updates not queued | Assumed (secondary) | `.claude/reports/2026-08-26-two-clocks-investigation.md:95`; `docs/error-runbooks/dhan-live-crossverify-error-codes.md:69` |
| Disconnects slow client? | Only via ping timeout (>40 s no pong) | Verified | `UP/15-live-market-feed.md:77` |
| Sequence number, main feed | NONE (header = code/len/segment/sid) | Verified | `UP/15:110-115` |
| Sequence number, depth | d20 bytes 9-12 "Message Sequence (to be ignored)"; d200 = row count | Verified | `UP/16-full-market-depth.md:117,149` |
| Snapshot-on-subscribe | India: only Prev Close (code 6). Live snapshot documented ONLY for US global-stocks socket | Verified | `UP/15:127-128`; `UP/25-global-stocks.md:718-720` |
| Conflation / cadence | Pack says "tick-by-tick event based"; no cadence/batching statement | Verified (absence); true cadence Unknown | `UP/15:16`; measured ~41% repeated-state rows (`two-clocks…:104`) |
| Skip detectable? | Not at protocol level; only LTT/volume/OI monotonicity, silence timers, REST cross-verify | Verified (absence) | proposal `2026-08-09-dhan-16-connection-architecture.md:125-131` |

## 2. Connection contract

| Fact | Value | Label | Evidence |
|---|---|---|---|
| Ping | server every 10 s; auto-pong | Verified | `UP/15:73-75`, `UP/16:91-93` |
| Timeout | >40 s ⇒ server closes | Verified | `UP/15:77`, `UP/16:96` |
| Token | 24 h; renew returns NEW token; one active at a time | Verified | `UP/02-authentication.md:78,257-261,301`; `docs/dhan-ref/02:216` |
| Connections | 5/user, 5,000 instruments each | Verified | `UP/15:18` |
| 5-cap per endpoint type | independent pools | Assumed (support ticket, not pack) | `docs/dhan-ref/04:64-67` |
| Per-message cap | 100/JSON (main); all 50 in one (d20) | Verified | `UP/15:50`, `UP/16:58` |
| Depth caps | d20 50/conn; d200 1/conn | Verified | `UP/16:54,77` |
| 805 | "more than 5 websockets … first socket disconnected with 805 with every additional connection"; "further requests may result in blocking" | Verified | `UP/15:209`; `UP/20-annexure.md:148` |
| 800-814 | 800 internal · 804 instrument limit · 805 · 806 not subscribed · 807 expired · 808 auth · 809 token invalid · 810 client invalid · 811-814 bad expiry/date/sid/request | Verified | `UP/20:146-157` |
| Reconnectable? | Vendor silent. Ours: 805→PoolOverflow; 807/809→refresh; 804→Fatal; rest Transient | Unknown (vendor) / Verified (ours) | `crates/core/src/websocket/pool_supervisor.rs:513-544` |
| Disconnect packet | main 8B hdr(50)+i16; depth 12B hdr+i16 | Verified | `UP/15:211-214`; `UP/16:185-188` |

## 3. Packets / frames

| Fact | Value | Label | Evidence |
|---|---|---|---|
| Main sizes | hdr 8; ticker 16; prev-close 16; quote 50; OI 12; full 162; disconnect 10 | Verified | `UP/15:106-196` |
| Depth sizes | hdr 12; d20 side 332; d200 ≤3,212, actual `12+rows×16` | Verified | `UP/16:109-167`; `docs/dhan-ref/04:160` |
| Depth frames stack | "stacked one after another in a single message … break down … on the basis of length" | Verified | `UP/16:169` |
| Main frames stack | NOT in vendor doc; observed live (163,934 packets walked frame-by-frame) and coded O(packets/frame), cap 70,000 | Assumed (vendor) / Verified (live+code) | `websocket-connection-scope-lock.md:1582`; `parser/dispatcher.rs:325,341`; `constants.rs:4942` |

## 4. Contradictions

| Pack | Our notes | Verdict |
|---|---|---|
| Depth unsubscribe `25` (`UP/20:106`) | `24` ships; 25 ignored live 2026-09-10 (`docs/dhan-ref/08:105`, `03:346`) | Wire beats pack |
| Data APIs **7,000/day**; Orders 100,000 (`UP/22-rate-limits.md:20-21`) | `no-rest-except-live-feed…md` §8.0: "Data-API … 100,000/day" | Our rule quotes the ORDER cap — re-verify |
| "tick-by-tick" (`UP/15:16`) | ~1/s conflated, 41% repeats (`two-clocks…:99-104`) | Measurement wins |

## 5. Client requirements derived

| Vendor fact | Requirement |
|---|---|
| Skip-forward, no seq, no snapshot | Drain never blocks on I/O (reader→WAL→ring; every flush off-drain). Loss only via synthesised monotonicity + REST cross-verify. |
| Pong ≤40 s | Read task must poll under any load; watchdog <40 s (ours 27 s). |
| 805 kills OLDEST | Never >5 per endpoint; no retry storms. |
| 804 replays same set | Must be Fatal, never Transient. |
| Frames stack | O(packets) per frame, bounded; abandon on unknown code, never resync by guess. |
| 24 h single token | One minter; refresh on 807/809 then redial. |
