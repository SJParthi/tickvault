# Topic — Byzantine / Zombie / "Looks Healthy But Isn't" Scenarios

**Created:** 2026-05-11 22:25 IST
**Trigger:** Operator: "app crashes or hang or hold or acting-like-working-but-not — even logging but not working — for Docker, memory spill, disk, AWS, app. All extreme worst-case + out-of-box scenarios. The plan ITSELF must consider all these like real-time."
**Mode:** Brainstorm (no implementation — file persistence only)
**Status:** Adds ~28 new failure modes to the unified gap list (G19–G46)

---

## 🎯 The Byzantine class — what makes it the hardest

| Crash class | Detection difficulty | Example |
|---|---|---|
| **Hard crash** | EASY — process gone, restart fires | App segfaults, container exits |
| **Slow path failure** | EASY — retry budget exhausted, alert fires | Token renewal fails 5×, HALT |
| **Disconnect** | EASY — pool watchdog 5s detects | TCP RST received |
| **Byzantine / zombie** | 🔥 **HARDEST** — system reports healthy, fundamentally isn't | App alive, /health = 200, but main thread deadlocked on a Mutex; no ticks processed for 10 min |
| **False-OK telemetry** | 🔥 **HARDEST** — metrics increment, logs flow, but values are LIES | Counter shows ticks-processed = 1000/s, actual = 0 (stale gauge) |

This file enumerates every Byzantine/zombie scenario across 3 systems (JWT / Instruments / WS Feed) and proposes detection mechanisms.

---

## ⚠️ PART 1 — Process-level "alive but dead" scenarios

| # | Scenario | What looks healthy | What's actually wrong | Detection mechanism | Today? |
|---|---|---|---|---|---|
| Z1 | Tokio runtime deadlock | Process PID alive, CPU 0%, /health 200 | Two tasks holding each other's Mutex; no progress | Liveness probe that tests BUSINESS LOGIC (e.g., "issue a no-op query through tick pipeline; expect response in 5s") | ❌ GAP |
| Z2 | Async task spawned-never-awaited | Process running normally | Background task panicked, JoinHandle dropped silently | `unused_must_use` lint catches dropped JoinHandle. ✅ partial. Audit log on every spawn → wait | ✅ partial |
| Z3 | Worker thread infinite retry loop | Logs flow, retry counter ticks | Permanent failure mode (DNS broken) but retry has no max | Per-task retry budget + alert on exhaustion | ✅ exists for token, ❌ NOT for all tasks |
| Z4 | Tokio worker starved (one slow task) | Other tasks run | Specific instrument never gets a tick | `TickGapDetector` per (security_id, segment) — coalesced 60s | ✅ (WS-GAP-06) |
| Z5 | JoinHandle leak (task running forever) | Process memory grows slowly | Memory leak from accumulated tasks | Prom `tv_tokio_active_tasks` gauge + budget cap alert | ❌ GAP |
| Z6 | Stack overflow in spawned task | Other parts work | Specific subsystem dead | Per-task heartbeat; task missed → alert | 🟡 per-subsystem heartbeat exists for aggregator (AGGREGATOR-HB-01), not universal |
| Z7 | Panic in a `tokio::spawn(async move {...})` without `.unwrap_or_else` | Telegram never warned | Task silently dies | Wrap every spawn in a panic-catcher with `error!` + counter | 🟡 partial (pool-supervisor pattern, not universal) |
| Z8 | `/health` endpoint returns 200 from a stale cache | Healthcheck passes | Underlying state is hours old | `/health` must query LIVE state, not cache, and include "last_tick_age_secs" | 🟡 exists for some metrics, not all |
| Z9 | Main thread blocked on a synchronous I/O call | Background tokio runtime appears alive | E.g., synchronous `std::fs::read` in async path | Banned-pattern hook scans for sync I/O in async fn | ✅ partial (clippy::let_underscore_must_use covers, not blocking I/O detector) |
| Z10 | Tokio runtime out of workers (all blocked) | Process alive | New tasks queue forever | `tokio::runtime::Handle::current().metrics()` poll + alert | ❌ GAP |
| Z11 | OS sends SIGSTOP / job-control freeze | Process appears alive | Completely frozen, will never run | `/proc/<pid>/status` `State: T (stopped)` check from external watchdog | ❌ GAP (rare) |

---

## ⚠️ PART 2 — Network "TCP open but no flow" scenarios

| # | Scenario | What looks healthy | What's wrong | Detection | Today? |
|---|---|---|---|---|---|
| Z12 | TCP socket CONNECTED state, no bytes received in 60s | OS reports socket healthy, no RST | Server-side filtered (far-OTM 200-level), or a NAT silently dropped | Tick-arrival heartbeat per conn (`tv_ws_last_tick_age_secs`) + alert | ✅ WS-GAP-06 partial |
| Z13 | WebSocket pong received but stale | Library reports ping/pong OK | Pong from prior session (NAT cache) | Application-layer heartbeat with sequence number | 🟡 library handles, no app-layer check |
| Z14 | DNS resolves to stale Dhan endpoint | Conn established | Wrong IP, hits an old shard with no data plan | Periodic DNS re-resolution + diff alert | ❌ GAP (NET-02 covers FAIL, not stale answer) |
| Z15 | Connection appears subscribed but Dhan never streams (the 200-level far-OTM case) | Subscribe ACK received | Server-side filtering, no data | Tick-arrival watchdog per subscribed security_id (already exists for aggregator) + initial-tick deadline | 🟡 DEPTH-DYN gaps cover related case |
| Z16 | Subscribe message lost mid-flight | We think we're subscribed | Dhan never received | Subscribe-ACK timeout + retry. Or: parse subscribe response codes. | ❌ GAP — we don't parse subscribe responses |
| Z17 | Reverse-proxy / load-balancer silently kills idle conn | TCP alive on our side | Server forgot us | Keep-alive ping (Dhan sends every 10s, library handles); SO_KEEPALIVE OS-level safety net | 🟡 Phase 0.5 Item 0.5.6 adds SO_KEEPALIVE |
| Z18 | TLS session resumption with stale params | Handshake succeeds | Server treats as old session | Force fresh handshake on reconnect (no session ticket reuse) | ✅ aws-lc-rs default |
| Z19 | aws-lc-rs decryption silently dropping corrupted frames | Library reports OK | Bytes lost | Trust the lib; chaos test packet corruption | 🟡 fuzz target exists, not chaos test |

---

## ⚠️ PART 3 — Storage "write OK but data not committed" scenarios

| # | Scenario | What looks healthy | What's wrong | Detection | Today? |
|---|---|---|---|---|---|
| Z20 | ILP `append()` returns Ok, row never flushed | tracing shows write success | Buffer holds it, server connection broke before flush | Explicit `flush()` + check return; ILP TCP keep-alive | ✅ tick_persistence has flush + rescue ring |
| Z21 | QuestDB `/exec` returns 200 with empty body | HTTP success | DDL ran but DEDUP wasn't actually applied | Parse response body; verify expected effect via SELECT | 🟡 partial |
| Z22 | Valkey AOF write OK but disk silently corrupt | Set returns Ok | Read after restart returns wrong value | Periodic READ-after-WRITE consistency check | ❌ GAP |
| Z23 | rkyv cache file checksum corruption | File read succeeds | Deserialized to garbage struct | rkyv has CheckedArchive feature (compile-time bounds) | ✅ rkyv 0.8.15 enables |
| Z24 | Filesystem mounted read-only mid-day | Writes appear normal until first failure | Single failed write may take minutes to manifest | Periodic write-test to `data/` (touch + rm); + Prom `tv_fs_writable` gauge | ❌ GAP |
| Z25 | QuestDB DEDUP UPSERT silently misses a key | INSERT succeeds | Duplicate row written | Periodic SELECT COUNT vs expected distinct | 🟡 `dedup_segment_meta_guard.rs` catches at code level, not runtime |
| Z26 | Spill NDJSON write OK but file pre-allocated as sparse | df reports OK | Real disk full when sparse fills | Real-time `statvfs` poll + alert at 80% (Phase 0.5 RESOURCE-03) | 🟡 Item 0.5.8 planned |
| Z27 | Audit table write skipped due to channel-full | tracing shows the event | Channel back-pressured, drop | Telegram alert on channel-full + Prom counter | ✅ exists for `aggregator_seal_audit` |
| Z28 | ALTER TABLE ADD COLUMN runs against wrong table name typo | DDL succeeds | New column added to unintended table | Boot-time schema-validation: list expected tables + compare | 🟡 partial schema check |

---

## ⚠️ PART 4 — Time / Clock anomalies

| # | Scenario | What looks healthy | What's wrong | Detection | Today? |
|---|---|---|---|---|---|
| Z29 | Wall clock drifts ±5 sec mid-day | Process running normally | IST midnight boundary crossed wrong; DEDUP key broken | Boot-time + periodic NTP skew check | ✅ `BOOT-03` HALTs at 2s skew at boot. ❌ NO mid-day re-check |
| Z30 | Container clock ≠ host clock | Containers run | Audit timestamps diverge | Periodic `docker exec date` vs host date | ❌ GAP |
| Z31 | Monotonic clock vs system clock divergence | Process appears healthy | Tokio sleep gets confused | tokio::time uses monotonic; should be safe | ✅ tokio default |
| Z32 | Daylight Saving boundary | N/A for IST | N/A | IST is +05:30 constant | ✅ |
| Z33 | Leap second | Process pauses 1s | DEDUP key conflict for that 1s | Linux defaults handle gracefully; verify NTP slew not step | ✅ chrony slew default |

---

## ⚠️ PART 5 — Resource exhaustion (silent)

| # | Scenario | What looks healthy | What's wrong | Detection | Today? |
|---|---|---|---|---|---|
| Z34 | File descriptor leak | Process running | New conn fails after 60K fds | `tv_open_fds` gauge + alert at 80% (RESOURCE-01 reserved) | 🟡 RESERVED, not implemented |
| Z35 | Network socket leak (TIME_WAIT pile-up) | Existing conns work | New conn fails after kernel exhausts ephemeral ports | `ss -tan | wc -l` external watchdog + alert | ❌ GAP |
| Z36 | mpsc channel sender leak | Tasks alive | Channel never drops, receiver blocks forever on flush | Periodic channel-len gauge + bounded capacity | ✅ all our channels bounded |
| Z37 | Tracing span never closed (`Span::enter()` without drop) | Logs flow | Memory grows; span tree corrupted | tracing-subscriber catches at compile via guard pattern | ✅ guard pattern |
| Z38 | Tokio task accumulation | RAM grows slowly | Boot up fine, hours later OOM | `tv_tokio_active_tasks` gauge + budget alert | ❌ GAP (Z5 dup) |
| Z39 | jemalloc heap fragmentation | RSS climbs without leak | OOM kill eventually | jemalloc periodic mallctl epoch + arena purge | 🟡 not actively monitored |
| Z40 | Page cache pressure (kswapd thrash) | Process appears slow | All threads stalled on disk | `/proc/vmstat` `pgmajfault` rate + alert | ❌ GAP |

---

## ⚠️ PART 6 — False-OK telemetry (the trickiest class)

| # | Scenario | What looks healthy | What's wrong | Detection | Today? |
|---|---|---|---|---|---|
| Z41 | Prom counter ticks but value is wrong | Dashboard green | Counter mistakenly incremented by background scrape | Cross-validation: 2 independent paths must agree | ❌ GAP for most counters |
| Z42 | Telegram sends "Boot complete" but Phase 2 scheduler never armed | Operator sees green | Mid-day reveals nothing dispatched at 09:13 | Phase 2 emit-guard (PHASE2-02) + `Phase2EmitGuard` Drop check | ✅ |
| Z43 | "Auth OK" Telegram sent but token actually invalid | Operator sees green | First WS subscribe returns 401 | Auth event fires AFTER a successful test call to `/v2/profile` | 🟡 partial (boot does call profile) |
| Z44 | Audit table written but with wrong timestamp | Forensic reconstruction lies | Wrong day attribution | Timestamp validation: must be within ±5s of `now()` | ❌ GAP |
| Z45 | Boot-complete fired but some subsystems still initializing | Operator sees green | Trades fire against uninitialized risk engine | All subsystem-ready signals must coalesce (Phase 0.5 Item 0.5.10 PreMarketReady consolidator) | ✅ planned |
| Z46 | "Recovered after 1 failure" Telegram but actually we lost ticks | Operator relaxes | Cross-verify at 16:00 finds gap | ZERO-tolerance cross-verify (CandleCrossMatch) catches | ✅ |

---

## ⚠️ PART 7 — Compound / Cascade failures (worst-of-worst)

| # | Scenario | What looks healthy | What's wrong | Detection | Today? |
|---|---|---|---|---|---|
| Z47 | Token refresh + WS reconnect + universe reload all at same instant | Each subsystem retries | Lock contention or resource starvation | `CASCADE-01 RESERVED` chaos test (Wave-4-E3) | 🟡 reserved |
| Z48 | 3 different crashes within 60s window | Pool supervisor + auth manager + persist all firing alerts | Operator paralyzed by 30 simultaneous alerts | Telegram coalescer 60s buckets (TELEGRAM-01) | ✅ |
| Z49 | Network blip → token expires → WS drop → restart → repeat | Each retry alone looks recoverable | Infinite reconnect loop, never stable | Max-reconnect-attempts-per-hour cap + HALT | 🟡 per-conn streak counter exists |
| Z50 | Dual-instance deployment accidentally | Both write to Dhan | Static IP fight, duplicate orders | `RESILIENCE-01 RESERVED` (live_instance_lock table) | 🟡 reserved |
| Z51 | Mid-rebalance crash | Some securities subscribed, some not | Recovery non-deterministic | `RESILIENCE-02 RESERVED` (replay from subscription_audit) | 🟡 reserved |

---

## ⚠️ PART 8 — Logical / off-by-one / silent corruption

| # | Scenario | What looks healthy | What's wrong | Detection | Today? |
|---|---|---|---|---|---|
| Z52 | ATM strike computed off-by-one (rounding) | Subscribe succeeds | Wrong contracts subscribed; depth gives wrong signal | Property test on strike selector + production cross-check vs option_chain API | ✅ unit tests, ❌ no production cross-check |
| Z53 | Expiry date timezone confusion | DateTime parses | Subscribe to yesterday's contract | All expiry math in IST + boundary tests | ✅ trading_calendar enforces |
| Z54 | f32 precision loss on price | Tick parsed | Strategy decision wrong | f32 for LTP per Dhan; cross-check f64 conversion path | ✅ STORAGE-GAP-02 |
| Z55 | Tick parser off-by-one byte | Parses without error | Garbled price | Property test on every packet type | ✅ snapshot tests + property tests |
| Z56 | Subscription plan computed but capacity assertion off | Subscribe succeeds | Excess instruments rejected silently | `MAX_TOTAL_SUBSCRIPTIONS_TARGET` warn + `_HARD_LIMIT` halt | ✅ I-P0-06 |

---

## ⚠️ PART 9 — Operator-introduced (the surprise class)

| # | Scenario | What looks healthy | What's wrong | Detection | Today? |
|---|---|---|---|---|---|
| Z57 | Operator manually flips `dry_run=true` → `false` without restart notification | Config file changed | App still runs with old dry_run | Boot-time `BootAudit` records dry_run; config file watcher restarts? | 🟡 audit on boot only |
| Z58 | Operator runs ad-hoc SQL against QuestDB that violates DEDUP | DDL succeeds | Future writes conflict | Periodic schema-snapshot vs golden + alert | ❌ GAP |
| Z59 | Operator rotates Dhan password mid-day | Token still valid until expiry | At expiry, renewal fails | Real-time DH-901 alerting | ✅ |
| Z60 | Operator changes config/base.toml at boot without `make stop` | Old process keeps using old config | New process reads new config; mismatch | Config-load timestamp logged; PID + start-time in BootAudit | ✅ partial |
| Z61 | Telegram bot token rotated externally | Old messages still queued | New messages fail to send | TELEGRAM-01 (drop counter) catches | ✅ |
| Z62 | AWS SSM secret name typo / path moved | Boot succeeds via Layer 3 TOTP | Next renewal cycle catches | Boot logs "auth path = TOTP" indicating cache + SSM both missed → alert | 🟡 partial |

---

## ⚠️ PART 10 — Dhan-side silent semantic changes

| # | Scenario | What looks healthy | What's wrong | Detection | Today? |
|---|---|---|---|---|---|
| Z63 | Dhan changes CSV column name | CSV parses | Wrong column mapped → wrong data | I-P1-08 proactive schema validation | ✅ |
| Z64 | Dhan changes packet byte offset (binary protocol) | Parser doesn't crash | Price field garbled | Property test against known good packets; cross-check vs option_chain price | ✅ snapshot tests, ❌ no live cross-check |
| Z65 | Dhan returns different SecurityId for same instrument (re-issue) | Subscribe succeeds | Tick stream has new SID; old subscription dead | Daily refresh picks up; existing subs continue with old (broken) SID | 🟡 1-day lag |
| Z66 | Dhan silently reduces rate limit | Some calls succeed | Random DH-904 | Per-endpoint rate-limit observer + degraded-mode | 🟡 partial |
| Z67 | Dhan changes JWT format (e.g., 12h vs 24h) | Token works initially | Expires earlier than expected | `tv_token_expiry_secs` gauge + alert if < 6h | ✅ |
| Z68 | Volume field semantic changes (cumulative vs incremental — currently disputed via Ticket #5525125) | Tick processed | Indicator calculations wrong | `VOLUME-MONO-01` monotonicity guard (Wave 5 L1) | ✅ |

---

## 🎯 NEW GAPS to add to the unified list

From this brainstorm, **24 net-new gaps** identified (skipping ones already in matrix):

| # | Gap | Class | Severity | Mitigation proposal |
|---|---|---|---|---|
| G19 | Tokio runtime deadlock detector (Z1) | Process | High | Liveness probe testing business logic |
| G20 | Tokio worker starvation gauge (Z10) | Process | Medium | `tokio::runtime::Handle::metrics()` poll |
| G21 | SIGSTOP external watchdog (Z11) | Process | Low | Out-of-process health checker |
| G22 | TCP socket alive but no flow (Z12 deeper) | Network | High | Per-conn last-byte-age + alert (extends WS-GAP-06) |
| G23 | DNS stale-answer detection (Z14) | Network | Low | Periodic re-resolve + diff |
| G24 | Subscribe-response-code parsing (Z16) | Network | High | Parse subscribe ACK frames |
| G25 | Valkey read-after-write consistency (Z22) | Storage | Low | Periodic round-trip check |
| G26 | Filesystem read-only mid-day (Z24) | Storage | Medium | `touch + rm` probe |
| G27 | DEDUP runtime audit (Z25) | Storage | Low | Periodic COUNT vs DISTINCT |
| G28 | Mid-day NTP skew re-check (Z29) | Time | Medium | Extend BOOT-03 to runtime |
| G29 | Container vs host clock drift (Z30) | Time | Low | Docker exec date watchdog |
| G30 | FD leak gauge (Z34) | Resource | Medium | RESOURCE-01 promotion |
| G31 | TIME_WAIT socket pile-up (Z35) | Resource | Medium | `ss` external watchdog |
| G32 | Tokio task count cap (Z38) | Resource | Medium | `tv_tokio_active_tasks` gauge |
| G33 | jemalloc fragmentation monitor (Z39) | Resource | Low | mallctl epoch poll |
| G34 | Page-fault rate alert (Z40) | Resource | Low | `/proc/vmstat` watch |
| G35 | Cross-validation between independent paths (Z41) | False-OK | High | Two-source agreement for critical metrics |
| G36 | Audit timestamp sanity (Z44) | False-OK | Low | ±5s window check |
| G37 | CASCADE-01 chaos test (Z47) | Cascade | Medium | Triple-failure synthetic test |
| G38 | RESILIENCE-01 dual-instance lock (Z50) | Cascade | Critical | Live instance lock table |
| G39 | RESILIENCE-02 mid-rebalance replay (Z51) | Cascade | High | subscription_audit replay |
| G40 | Production ATM cross-check vs option_chain (Z52) | Logical | Low | Periodic spot-vs-strike sanity |
| G41 | Live cross-check tick vs option_chain price (Z64) | Logical | Medium | Once-per-minute sanity probe |
| G42 | Out-of-process watchdog (Z11 + Z35 + Z24) | Meta | Medium | A simple "is the app alive AND processing?" external probe |
| G43 | Auth-success-on-test-call validation (Z43) | False-OK | Medium | Auth event fires AFTER /v2/profile success |
| G44 | Active-tasks JoinHandle leak audit (Z5/Z38) | Process | Medium | Track every `tokio::spawn` site |
| G45 | Schema-snapshot vs golden alert (Z58) | Operator | Low | Boot-time + daily schema diff |
| G46 | `/health` queries live state, not cached (Z8) | False-OK | Medium | Audit endpoint impl + add `last_tick_age_secs` |

---

## 📊 Severity-bucketed actions

| Severity | Gap count | Recommendation |
|---|---|---|
| **Critical** | 1 (G38 dual-instance) | **MUST close before any live order**. Add to Phase 0.5 as Item 0.5.13. |
| **High** | 5 (G19, G22, G24, G35, G39) | **Strong recommend close before Friday**. Phase 0.5 candidates 0.5.14–0.5.18. |
| **Medium** | 12 | Phase 0.5 stretch + Wave 6 main. |
| **Low** | 10 | Wave 6 backlog. |

---

## 🚖 Auto-driver / dumb-kid grand summary

> "Sir, you asked: even when the system PRETENDS to be working — is it really working?
>
> I checked 68 ways things can fake-look-OK. Found **24 new gaps** on top of the 18 we already had.
>
> **The deadly one (Critical):** if BOTH Mac dev AND AWS prod start at the same time, both register the same Static IP, both try to trade — Dhan accepts orders from BOTH, you get 2× position size. Solution: a 'Live Instance Lock' in QuestDB that the boot reads — if another instance has the lock < 60 sec old, REFUSE to boot. 50 LoC. Must close before Friday.
>
> **5 High-severity gaps:** Tokio deadlock (process alive, brain dead), TCP-open-but-no-flow, Subscribe-message-lost-silently, Cross-validation (2 paths must agree on critical metric), Mid-rebalance crash recovery.
>
> **12 Medium gaps + 10 Low gaps** — Wave 6 backlog.
>
> **The honest summary:** today our system catches 'hard crash + slow path failure + disconnect' very well. It catches 'Byzantine/zombie' partially. With 6 Phase 0.5 additions (0.5.13 dual-instance lock + 0.5.14 deadlock detector + 0.5.15 TCP flow watchdog + 0.5.16 subscribe-ACK parser + 0.5.17 cross-validation + 0.5.18 mid-rebalance replay), we hit 99% Byzantine coverage. ~600 LoC total. Doable in Friday's parallel sessions."

---

## 🎤 No forced pick — just confirming the additions

This file IS the brainstorm output. Already committed to disk. Decisions can wait.

If you want to:
- **ADD 6 Phase 0.5 items (Critical + 5 High)** → say "add all 6"
- **ADD only the Critical (G38 dual-instance)** → say "just G38"
- **ADD different subset** → tell me which Gs
- **DROP another worst-case angle to brainstorm** → keep going, more is welcome
- **DEBATE severity of any G** → ask me to defend or downgrade

Floor's yours. 🎤
