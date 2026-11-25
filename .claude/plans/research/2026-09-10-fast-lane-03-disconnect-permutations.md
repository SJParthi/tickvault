# Fast-lane 03 — disconnect / reconnect permutations (hostile review, 2026-09-10)

Read-only source audit. P = prevented, D = detected+bounded, N = neither. Dhan never resends and has no sequence number, so "ticks lost" = the gap DURATION; the count is uncountable.

| # | Trigger → sockets | Verdict | Ticks lost / bound | Sev | Evidence | Label |
|---|---|---|---|---|---|---|
| 1 | Dhan pings 10 s, closes after 40 s silence (main×5, d20×5); auto-pong only while `recv()` polls | D — idle watchdog redials at 27 s | ≤27 s + rung 0 + resubscribe | Med | `idle_watchdog.rs:82-105`; `pool_supervisor.rs:1227,1300-1318` | Verified |
| 2 | depth-200 gets ZERO server pings → quiet book read as idle (2026-08-26: 112/210 redials) | P — client ping every 10 s resets watchdog | none | — | `pool_budget.rs:323-330`; `pool_supervisor.rs:127,4645` | Verified |
| 3 | Read loop stalls >27 s (MemoryHigh=20G throttle — 371,828 events 2026-09-03; sync spill writes on drain) → no pong | D, not P | gap + whatever Dhan skipped while our buffer was full | High | `tickvault.service` MemoryHigh; `pool_supervisor.rs:1300` | Cause Verified, loss Unknown |
| 4 | Frame watchdog: Live socket, no DATA frame 300 s in session → redial. depth-200 now holds thin STOCK options only | N — self-inflicted on a healthy quiet socket | ≤ rung 0 | Med | `pool_supervisor.rs:168-190,1320-1347`; gate `dhan_feed_stack.rs:10375` | Assumed (silence unmeasured) |
| 5 | 807/809 stale token → all 16 | D — ≥5 s floor + jitter, single-flight renew (1 mint for 16) | ≥5 s + mint RTT | Med | `pool_supervisor.rs:95,1264-1290,3649-3658`; `token_manager.rs:195-240,1272` | Verified |
| 6 | Two minters: 06:05 Lambda vs box. Box ADOPTS SSM token at boot (48 h exp cap + `/v2/profile` probe) and publishes its own mint back | P by schedule; N vs a THIRD minter (laptop `cargo run`, manual) → 807 ×16 | one row-5 cycle per foreign mint | Med | `token_manager.rs:402-470`; `dhan_token_publisher.rs:177`; minter-lock §10.3 | Assumed |
| 7 | 805 → `PoolOverflow` park, no respawn, only a process restart recovers. Cause: a 2nd process on the account kills prod's OLDEST socket | N — park alarm pages, nothing stops the 2nd process | that shard for the session | High | `pool_supervisor.rs:513,863-872,1241` | Verified |
| 8 | 804/806/808/810 → `Fatal` park. 804 on depth-200 (cap 1) after a swap whose unsubscribe Dhan ignored; ghost redial cannot reach a parked socket (`take_ghost_redial` only under `Continue`) | N — code 25 proven ignored 2026-09-10; code 24 UNVERIFIED-LIVE | that socket for the session | High | `pool_supervisor.rs:511-547,1247-1256,4634` | Mechanism Verified; 24 Unknown |
| 9 | Ghost instrument still delivering ≥90 s after unsubscribe → redial; cooldown 180 s, pool spacing 20 s, ceiling 8/socket | D — 20 ignored / 10 redials 2026-09-10, no park | ≤8 gaps/socket/session | Med | `pool_supervisor.rs:652-686,741-749`; `dhan_feed_stack.rs:5320-5360` | Verified |
| 10 | Swap emptied socket (unsub OK/timed-out, subscribe failed) | D — redial + `WS-GAP-02 swap_emptied_socket` alarm | one gap | Med | noise-lock §2.3m; `depth_rebalance.rs:951-993` | Verified |
| 11 | Dial/TLS/15 s timeout/bad URL, TCP RST, vendor RST, EIP/AZ blip, subscribe-send failure → `DialFailed`/`Disconnected`/`SubscribeFailed` | D — ladder [0,1,2,5,15] s cap 30 s + ≤375 ms jitter; flap ceiling 6/300 s → 30 s floor; never parks | outage + ≤30 s per attempt; unbounded on a persistent 400 (2026-08-12, now pinned) | High | `connection.rs:213,982,1290-1296,1356,1866-1899`; `reconnect_ladder.rs:60-98,182-191` | Verified |
| 12 | Process dies/hangs: `Restart=always`, `RestartSec=3`, `WatchdogSec=150`; >5 starts/60 min → `failed` = down until a human | N — all 16; each restart replays the WAL backlog | ≥3 s + boot + replay; unbounded once `failed` | Crit | `tickvault.service` StartLimit/Watchdog | Verified |
| 13 | Budget: AWS action at 90 % ($135, `STANDBY`, armed) stops the box; `hard_stop_guard` at $150 also disables the 08:30 start. Forecast $142.24 | N | rest of day / rest of month, all 16 | Crit | `budget.tf:397-429`; `budget-guards.tf:274`; `hard_stop_guard.rs:10-55` | Verified |
| 14 | Order-update WS: 10-attempt cap applies only OUTSIDE hours | D (no market data on it) | none | Low | `order_update_connection.rs:1319-1335` | Verified |
| 15 | 15:40 capture close | P — sockets stay up; frame watchdog gated off; `Shutdown` park only at 17:30 | none | — | `dhan_feed_stack.rs:10370-10375`; `pool_supervisor.rs:1149` | Verified |
| 16 | In-session deploy/restart → `ShutdownRequested` parks all 16 | N — "no in-session deploy" is prose only; 7 restarts on 2026-09-03 | full boot + replay each | High | `pool_supervisor.rs:1149`; daily-universe Quote 21 | Verified |
| 17 | 24 h JWT expiry mid-session | P by schedule (minted 06:05, box off 17:30) unless row 6 | — | Low | minter-lock §10.2 | Assumed |
| 18 | Frame ring full under burst | P — reader never stops polling; frames refused, socket up | refused frames counted | Med | `pool_supervisor.rs:7381` | Verified |

## Honest envelope

1. No socket is torn down by OUR watchdogs while it answers a ping AND delivers ≥1 data frame per 300 s in session (rows 1,2,4,18) — Verified in source, unproven on a thin-book depth-200 day.
2. Every transport-class disconnect (rows 5,9-11) redials on a ≤30 s + 375 ms ladder with a 6-per-5-min flap floor and never parks; the GAP is bounded, the ticks inside it are lost upstream and uncountable.
3. Four classes park a socket for the whole session with NO automatic recovery: 805, 804 (code 24 still unverified live), 806/808/810, shutdown (rows 7,8,16).
4. Whole-process events (rows 12,13,16) take all 16 sockets and are bounded only by systemd's 5/60 min limit, the budget lines, or a human.
5. "Never disconnect" honestly means: no self-inflicted disconnect on a healthy socket and a ≤30 s redial on every vendor/network one — never zero ticks lost, and no in-session guarantee against 805/804/budget/restart.
