---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/app/src/clock_offset_page.rs"
  - "crates/app/src/infra.rs"
  - "crates/app/src/network_rtt.rs"
  - "crates/storage/src/host_capacity_minute.rs"
  - "crates/storage/src/network_rtt_session_persistence.rs"
---

# Clock Offset and Network Handshake Error Codes

> **Authority:** `docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md`
> §2.22 and Quote 57 in
> `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md`
> (owner, 2026-11-12). `ErrorCode::ClockSkew01OffsetOverOneMs.runbook_path()`
> and `ErrorCode::NetRtt01HandshakeCheckFailed.runbook_path()` point here.
> Plan item LF-6a (`.claude/plans/active-plan-audit-2026-10-20.md`).

## CLOCK-SKEW-01 — the clock was more than 1 ms off for 5 minutes in a row

**Severity:** High. **Pages:** `tv-<env>-errcode-clock-skew-01` (Telegram:
"The server clock has been more than 1 millisecond off…"). **No OK page.**

**What fired it.** The clock-skew poller reads `chronyc tracking` every 30 s.
Once per IST minute, the first reading of that minute is fed to a small state
machine (`ClockOffsetEpisode`). Inside 09:00–15:40 IST it counts consecutive
minute samples whose absolute offset is over 1 ms (1,000,000 ns; exactly 1 ms
does not count). At the fifth one it writes ONE coded `error!` and latches.

| Sample | Effect |
|---|---|
| offset over 1 ms, inside 09:00–15:40 IST | count + 1; the 5th pages once |
| offset at or under 1 ms | count and latch reset, silently (no recovery line); the saved latch file is removed |
| failed sample (no chrony, or a non-finite reading; any finite offset counts, however large) | coded `warn!` (`reason = "sample_failed"`, first failure then powers of two); count and latch unchanged |
| any sample outside 09:00–15:40 IST | count and latch reset, nothing logged |

**Once per episode, across restarts.** When it pages, the IST date is saved to
`data/state/clock-offset-page.state`; a process that starts the same day
with that file starts latched and does not page the same unbroken episode
again. The file is removed when the episode ends (a reading at or under 1 ms,
or the session end); a file from another date is ignored and removed. A
failed read or write of it is a coded `warn!` (never a page); a lost file
fails toward one extra page.

**Fields:** `offset_ns` (signed: positive = this clock AHEAD), `consecutive_minutes`,
`threshold_ns` (1,000,000).

**What it means.** Signal times, order send times and fill times read from
this clock are off by about `offset_ns`. Exchange timestamps are not affected.

**Triage:**
1. On the box: `chronyc tracking` (offset, leap status, reference) and
   `chronyc sources -v` (is the Amazon Time Sync Service reachable?).
2. `SELECT ts, clock_offset_ns FROM host_capacity_minute WHERE ts IN today()
   ORDER BY ts` shows the minute series (NULL = no fresh reading).
3. A steady offset after a step usually settles on its own once chrony slews;
   a growing one means chronyd lost its source. Restarting chronyd outside
   market hours is the usual fix; never step the clock in session.

**Not covered:** an offset under 1 ms; fewer than 5 minutes in a row; anything
outside 09:00–15:40 IST; a box without chrony (failed samples never page; the
`BOOT-03` probe-unavailable warning and `tv_clock_skew_probe_available = 0`
say so).

**Source:** `crates/app/src/clock_offset_page.rs` (`ClockOffsetEpisode`),
`crates/app/src/infra.rs` (`run_clock_skew_loop`),
`crates/storage/src/host_capacity_minute.rs` (`publish_clock_offset`,
`clock_offset_ns` column).

## NET-RTT-01 — the session handshake check could not finish an attempt or write its rows

**Severity:** Low. **Log-only, no page.** A failed handshake attempt is a coded
`warn!`. A failed row write, or a table that could not be created or checked
after one retry (`stage = "ensure_failed"`), is a coded `error!` (still log
only: no CloudWatch filter, no Telegram).

**What it is.** Once per session (09:25 IST, or about a minute after a later
boot) a task opens 5 TCP connections plus TLS handshakes to each Dhan host
(the market feed, the order-update feed and the REST API), 1 s apart, 2 s
budget each, and closes each right after the handshake. No HTTP request is
sent. Each attempt is one row in `network_rtt_session`
(`tcp_connect_us`, `tls_handshake_us`, `total_us`, `ok`, `error`).

**Fields:** `host`, `sample_idx`, `error` (one of `dns_failed`,
`dns_no_address`, `connect_timeout`, `connect_failed`, `tls_timeout`,
`tls_failed`, `tls_config_failed`) on the attempt `warn!`; `stage = "write"`
(rows could not be written) or `stage = "ensure_failed"` (the table could not
be created or checked after one retry, so nothing was written; `rows` = the
count not written) on the `error!`.

**What it means.** A failed attempt is itself a measurement: the row is kept
with `ok = false`. One failure is a network blip; all 15 failing means the box
could not reach Dhan at that minute, and the live-feed alarms will say more.

**Triage:** `SELECT * FROM network_rtt_session WHERE ts IN today() ORDER BY
ts`. A write failure means QuestDB was unreachable; the rows of that session
are lost (they are not spilled) and the next session writes new ones. Rows
are written only after the table is ensured, so an ILP write never creates
`network_rtt_session` (QuestDB would give it the wrong designated timestamp
and no DEDUP key).

**Source:** `crates/app/src/network_rtt.rs`,
`crates/storage/src/network_rtt_session_persistence.rs`.
