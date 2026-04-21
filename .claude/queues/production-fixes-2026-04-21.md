# Production fixes queue — 2026-04-21

> **Owner:** Claude sessions reading this file must pick up the next
> unchecked item and ship it on its own branch. Do not skip items.
> Do not combine items. Each item gets its own PR.
>
> **Context:** session 01Cs3Z9DzSVjGj11c3iFtnxq — Parthiban reported
> live production failures on 2026-04-21 morning. Original task was
> `claude/fix-websocket-timeout-AYEcL`; the scope expanded as more
> symptoms surfaced. This file captures every known issue so nothing
> is lost across sessions or compactions.

## In-flight PRs

| PR | Branch | Status | Tests | Scope |
|---|---|---|---|---|
| #308 | `claude/fix-websocket-timeout-AYEcL` | CI green, draft | 16 ratchet + 9 unit | Full WS Telegram visibility + no-tick watchdog + 500ms fast-retry + order-update watchdog raise 660s→1800s |
| #309 | `claude/add-premarket-profile-halt` | CI running, draft | 6 ratchet | Pre-market profile check HALTs boot + CRITICAL Telegram |
| TBD | `claude/fix-depth-200-exhaustion` | commit ready, not pushed | (pending) | Depth retry cap 20→60 |

## Queue — fix each item on its own branch

### I1 — Depth-200 retry budget too aggressive *(in-flight)*

**Symptom** (Telegram, 2026-04-21 13:16 IST):
> ⚠️ [HIGH] Depth 200-level DISCONNECTED — Reconnection failed after 20 attempts for connection 0

**Scope:** All 4 depth-200 contracts (NIFTY 24550 CE/PE, BANKNIFTY 57300 CE/PE) exhausted the 20-attempt budget during market hours and terminated permanently.

**Root cause:** At 30 s max backoff, 20 attempts = ~10 min. Rebalancer runs every 60 s but may be pre-empted by backoff sleeps; gives < 10 rebalance cycles to land a correct ATM.

**Fix:** Raise `DEPTH_RECONNECT_MAX_ATTEMPTS` 20 → 60 in `crates/core/src/websocket/depth_connection.rs`. ~30 min retry window, 30 rebalance cycles.

**Branch:** `claude/fix-depth-200-exhaustion`

### I2 — Instrument cache missing at boot during market hours

**Symptom** (2026-04-21 13:06:02 IST, ERROR level):
```
CRITICAL: no instrument cache available during market hours — triggering emergency download
```

**Root cause:** Previous runs should have persisted `data/instrument-cache/api-scrip-master-detailed.csv`. Either a) persistence path is wrong b) filesystem is ephemeral c) not flushed before previous shutdown.

**Fix:** audit cache write path, ensure atomic write + fsync, add boot-time log that shows cache age + size, add Prometheus gauge `tv_instrument_cache_age_secs` for monitoring staleness.

**Branch:** `claude/fix-instrument-cache-persist`

### I3 — Constituency cache missing at boot

**Symptom** (2026-04-21 13:06:15 IST, WARN):
```
no constituency cache available — continuing without constituency data
```

**Root cause:** Same class as I2 — cache file `data/instrument-cache/constituency-map.json` not persisted across runs.

**Fix:** Combine with I2 fix on same branch (same bug class).

### I4 — tick_gap_tracker post-restart spam *(high severity, live spam)*

**Symptom** (2026-04-21 13:06–13:51 IST):
```
30s summary: 365 instruments had gaps (30-119s)
individual ERRORs firing with gap_secs 30-1024
```

**Root cause:** `TickGapTracker` state appears to persist logical "last tick" across process restarts (or the tracker compares live ticks against very old timestamps from the WAL replay / WAL-spill restoration). After a 234-min crash, resuming instruments all show 30-1000+ second gaps even though they're flowing normally now.

**Fix:** On boot, the tracker state MUST reset for each instrument's first tick (warmup). Verify warmup logic handles process-restart case. Likely fix: explicitly reset state at tick_processor startup, OR have `TickGapTracker` treat "first tick since boot" as the baseline regardless of persisted timestamps.

**Scope:** `crates/trading/src/risk/tick_gap_tracker.rs` + `crates/core/src/pipeline/tick_processor.rs`.

**Branch:** `claude/fix-tick-gap-tracker-restart-warmup`

### I5 — depth-200 ResetWithoutClosingHandshake diagnostic *(SUPERSEDED by I14)*

**Original hypothesis:** OFF-ATM strike selection.

**Correction (Parthiban 2026-04-21):** strikes ARE ATM — verified with the Python SDK on the same account + same strikes, Python works. The Rust client is the bug. I5 diagnostic (log current spot + subscribed strike) is still useful for future debugging but does NOT explain the 2026-04-21 failure.

**Action:** Keep the diagnostic log (cheap); move the real investigation to I14 below.

### I14 — Python SDK vs Rust client depth-200 protocol diff *(HIGH severity, root cause)*

**Symptom** (2026-04-21 13:06:26 IST, 4/4 depth-200 contracts):
```
ConnectionFailed { url: wss://full-depth-api.dhan.co/twohundreddepth,
                   source: Protocol(ResetWithoutClosingHandshake) }
```

**Root cause:** unknown. Dhan's server TCP-resets our socket repeatedly. The same account + strikes work with the Python SDK — so it is NOT account / dataPlan / IP / ATM / URL-path.

**Investigation checklist:**
1. Diff our `wss://full-depth-api.dhan.co/twohundreddepth` subscribe JSON against the Python SDK's: `{"RequestCode": 23, "ExchangeSegment": "NSE_FNO", "SecurityId": "<sid>"}`. Field order, casing, types (STRING vs numeric SID).
2. Diff TLS handshake params — Python SDK `websocket-client` + `ssl` default vs our `aws-lc-rs` rustls.
3. Diff HTTP headers on the WS upgrade — `User-Agent`, `Origin`, `Sec-WebSocket-Protocol` etc.
4. Capture a packet diff with `tcpdump` on both Python + Rust clients, compare byte-for-byte.
5. Test hypothesis: is there an `Origin` header Dhan's WAF requires that `tokio-tungstenite` omits?

**Fix target:** close the protocol gap so Rust gets the same behaviour as Python.

**Branch:** `claude/investigate-depth-200-rust-vs-python`

### I6 — TV_API_TOKEN unset in prod should be CRITICAL

**Symptom** (2026-04-21 13:06:15 IST, WARN):
```
GAP-SEC-01: TV_API_TOKEN not set — API authentication disabled (dry-run mode)
```

**Root cause:** Production boot without API auth token. In dev mode WARN is fine, in prod this should be CRITICAL and ideally HALT.

**Fix:** Detect prod vs dev from `config.env` or `NODE_ENV`-equivalent. In prod, this is CRITICAL + HALT. Keep WARN in dev.

**Branch:** `claude/fix-api-auth-required-in-prod`

### I7 — Stale tick data in QuestDB from previous session *(partial fix live)*

**Symptom** (from earlier in session, Grafana + QuestDB):
- `ticks` table only had 5,406 rows, all with `ts = 2026-04-20T15:29:59Z` (yesterday's close)
- During market hours today (14:45 IST), no fresh ticks

**Root cause hypothesis:** Dhan `dataPlan` / `activeSegment` / token-validity issue; WebSocket reported "connected" but Dhan silently did not stream.

**Partial fix already live:** PR #308's `NoLiveTicksDuringMarketHours` watchdog catches the symptom; PR #309's pre-market profile HALT prevents boot into a known-bad state.

**Still unfixed:** mid-session profile invalidation (e.g. dataPlan expires at 11:00 IST). Requires a periodic profile check during market hours that HALTs or fires CRITICAL if dataPlan/segment changes.

**Branch:** `claude/fix-mid-session-profile-watchdog`

### I8 — 33 clippy errors in ws_frame_spill.rs

**Symptom:** `cargo clippy -p tickvault-storage --lib --tests -- -D warnings` fails with 33 `let _ = Result` errors at specific line numbers in `crates/storage/src/ws_frame_spill.rs`.

**Root cause:** `#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]` applied to storage crate, but `let _ = std::fs::...` patterns remain.

**Fix:** Replace every `let _ = <expr>` with explicit error handling or `drop(expr)` where the result truly doesn't matter. Mechanical, ~50 LoC.

**Branch:** `claude/fix-ws-frame-spill-clippy`

### I9 — 2 pre-existing notification test failures

**Symptom:** `cargo test -p tickvault-core --test plan_p4_p5_alert_proof` fails:
- `test_all_4_ws_notification_events_include_identifiers`
- `test_ws_reconnection_exhausted_includes_connection_and_attempts`

Both assert that `msg.contains("3")` or `msg.contains("#3")` where the actual message shows `4/5` (because `connection_index + 1`). Off-by-one in test or in format.

**Fix:** Adjust test assertions to match the actual display format (index+1).

**Branch:** `claude/fix-notification-test-format`

### I10 — Dependabot 12 CVEs

**Symptom:** GitHub surface: 4 high / 2 moderate / 6 low CVEs from transitive dependencies.

**Fix:** Audit each Dependabot PR, cherry-pick approved updates into one roll-up PR, verify workspace builds + tests green.

**Branch:** `claude/update-vulnerable-deps`

### I11 — CLAUDE.md rule mismatch after retry-cap raise

**Symptom:** `.claude/rules/project/websocket-enforcement.md` rule 14 says "Depth connections cap at 20 retry attempts". After I1 ships, this is 60. Rule doc must be updated in the same PR.

**Fix:** Update rule to 60 + add justification note in the PR body.

**Branch:** (merge into I1)

### I12 — Dhan profile auto-diagnostic at boot

**Symptom:** When boot fails the profile check (PR #309 HALTs), operator must manually run `curl /v2/profile` and `curl /v2/ip/getIP` to diagnose.

**Fix:** On HALT, automatically call both endpoints and embed the redacted response in the CRITICAL Telegram message. Operator gets the diagnosis in the page, not in a follow-up manual step.

**Branch:** `claude/auto-diagnose-profile-halt`

### I13 — errors.summary.md still shows 660s watchdog fire

**Symptom:** errors.summary.md (screenshot) shows novel signature `5c6c5240efeaace6` — "WS activity watchdog fired — no frames or pings within threshold" from `tickvault_core::websocket::activity_watchdog`.

**Root cause:** PR #308 raises the threshold 660s→1800s, but that PR is still a draft and the running process is on older code.

**Fix:** Merge PR #308. No new code change needed.

**Action:** Ready PR #308 for merge once CI stable.

---

## Priority order (for execution)

1. **I1 + I5 + I11** *(current branch, immediate)* — depth-200 retry raise + diagnostic + rule sync
2. **I4** *(next, highest spam volume)* — tick_gap_tracker restart warmup
3. **I13** — merge PR #308
4. **I7** *(mid-session profile watchdog)* — prevents today's bug from happening again mid-session
5. **I2 + I3** — instrument + constituency cache persistence
6. **I9** *(quick win, unblocks CI)* — 2 notification tests
7. **I8** *(unblocks workspace clippy)* — ws_frame_spill.rs fixups
8. **I6** — TV_API_TOKEN CRITICAL in prod
9. **I12** — profile auto-diagnostic
10. **I10** — Dependabot CVEs (last, most churny)
