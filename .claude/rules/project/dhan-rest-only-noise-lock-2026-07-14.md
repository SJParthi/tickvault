# Dhan REST-Only Noise Lock — Operator Lock 2026-07-14 — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small). **Read the full file before adding, removing, rewording or re-routing ANY Dhan-scoped Telegram page, `NotificationEvent`, `error_code_alerts` entry, CloudWatch alarm, metric filter or EMF metric — and before any change with CloudWatch/AWS cost impact.** It holds the contract table (§2 and §2.1/§2.2), the live-lane family (§2.3 and its addenda), the dated COST NOTES / CORRECTED / MEASURED budget sections, and §2.4–§2.8. It is a dated log; later sections supersede earlier ones. Where this summary and the full file differ, the full file wins. Guards (`live_lane_paging_lockstep_guard.rs`, `dhan_exit_order_lockout_guard.rs`) read the FULL file.

**Authority:** CLAUDE.md > `operator-charter-forever.md` §D/§F > `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A.1 > `no-rest-except-live-feed-2026-06-27.md` > this file > defaults. **Scope:** PERMANENT.

**Operator quote (2026-07-14):** "for Dhan except spot 1m and option chain nothing else should work… these fucking issues of mid profile and all other fucking issues of Dhan should be entirely removed… always make the telegram messages/notifications cleaner, always mention precisely which broker."

**The rule (§1):** only an enumerated set of Dhan-scoped Telegram alerts may ever fire; every other Dhan-era page, probe, watchdog Telegram and dead alarm is deleted or silenced; "the token machinery self-heals SILENTLY."

**Current allowed set:**
- Families (1) spot-1m and (2) option-chain — **RETIRED 2026-09-17 (§2.4)**; their producers were removed by the SOCKETS-ONLY narrowing.
- (3) token could not be obtained — `AuthenticationFailed` / `TokenRenewalFailed` (Critical).
- (4) `tv-<env>-token-remaining-low` CloudWatch 4h early warning.
- (§2.2) the cadence expiry cross-broker DISAGREEMENT page.
- (5, §2.3, 2026-08-14) LIVE-LANE INTEGRITY alarms (lane down, ticks dropped, socket parked, drain respawn cap, and later dated rows). Latency is dimensioned "per WebSocket connection — 16 fixed slots — never per instrument" (cost: per-instrument metrics would trip the budget kill-switch).
- (§2.5, 2026-09-24) the three `ws-gap-03-xverify-{vacuous,failed,diverged}` alarms, restored with the 1-minute cross-verification.
- (§2.6, 2026-09-25) `dhan-feed-delay-high`, `dhan-main-reconnect-slow`, `dhan-depth-new-contract-blank` — `notBreaching`, no `ok_actions`, no per-connection/per-instrument dimension.
- (§2.7, 2026-09-27) `errcode-aggregator-stall-01` — the live feed waited at least 1 s on a stalled disk while writing a refused candle itself; at most one line a minute, no `ok_actions`, ungated.
- (§2.8, 2026-10-02, owner-approved "go ahead approved everyhtign dude okay?") `errcode-hot-path-stall-01` — in session, a tokio runtime or the frame drain made no progress for 2 s, or one hot-path step took 2 s; edge-triggered per signal, at most one line a minute, no `ok_actions`, 09:00–15:40 IST only.
- (§2.9, 2026-10-04, owner tapped "Page me" on the data-at-risk card) five pages over ten counters: `cold-backup-failing` (2 of 3 × 900 s), `disk-kept-not-backed-up`, `order-update-dropped`, `log-lines-dropped`, `feed-thread-wrote-to-disk` — `notBreaching`, no `ok_actions`, `host` dimension only, every counter registered at 0 at boot.
- (§2.10, 2026-10-05, owner tapped "Turn on" on the H1/N3 card) six loss-group pages over 55 counters through `/metrics` log filters (no EMF name): `market-data-refused`, `subscription-coverage-lost`, `audit-rows-lost`, `durability-sync-failing` (2 of 3 × 300 s), `order-path-dropped`, `safety-bound-or-monitor-blind` — `notBreaching`, no `ok_actions`, `host` dimension only, one `tv_loss_*` derived metric per group, every counter registered at 0 at boot.

**REJECT (§3, verbatim headlines):**
- Adds ANY new Dhan-scoped Telegram page outside the allowed set without a fresh dated operator quote HERE first.
- Re-introduces the mid-session profile / forced-re-mint Telegram pages, the REST canary, the no-tick watchdog, the fast-boot validation call, or the order-update spawn.
- Removes the SILENT self-heal machinery (900s profile probe, AUTH-GAP-05 forced re-mint + GAP-04 latch re-arm, GAP-02 REST-stack token sweep, token-health gauge poller, `tv-<env>-token-remaining-low`).
- Downgrades / removes the family-(3) Critical on a terminally-dead token.
- Makes a Dhan-scoped Telegram body stop naming the broker.
- (§2.6) alarms the feed-delay gauge on `Maximum`; includes depth endpoints in the reconnect gauge; pages on a single `silent_window`; adds `ok_actions`; adds a per-connection or per-instrument dimension.
- (§2.7) adds `ok_actions` to the stall alarm; logs every wait instead of once a minute; reads the clock on the normal queued path; drops or buffers seals without bound to avoid the wait; routes the wait through `AGGREGATOR-DROP-01`.
- (§2.8) adds `ok_actions` or a per-connection/per-instrument dimension to the hot-path stall alarm; emits it level-triggered or more than once a minute; pages on the small dashboard stall budgets; removes the session gate or the broker name.
- (§2.9) adds `ok_actions` or a dimension beyond `host` to the five data-at-risk alarms; splits a group into per-counter alarms; drops the cold-backup alarm to one period; drops a counter's boot-time zero registration; adds a counter to a group without a dated row.
- (§2.10) adds `ok_actions` or a dimension beyond `host` to the six loss-group alarms; publishes a group under a raw counter name; splits a group into per-counter alarms; drops the durability alarm to one period; drops a counter's boot-time zero registration; adds a counter to a group or widens a slice without a dated row.

"Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote."
