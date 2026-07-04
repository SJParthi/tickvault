# Topic — Telegram Message Style Rules (DEAD-SIMPLE only)

**Created:** 2026-05-11 23:00 IST
**Trigger:** Operator: "every Telegram message needs to be fucking easy and understandable, right dude?"
**Mode:** Rule + concrete rewrites
**Status:** **CANONICAL** — every Telegram message in the system MUST follow these rules. Cross-ref'd in `step-1-honest-envelope.md`.

---

## 📍 2026-07-04 Update — runtime source badge (💻 LOCAL / ☁️ AWS)

Operator directive 2026-07-04 (verbatim): *"in telegram when it displays
message then it should clearly tell whether it is from local or aws"*.

Every outgoing Telegram message now carries a source badge immediately after
the severity tag on the first line:

| Runtime | First line looks like |
|---|---|
| Operator's Mac (`make run`) | `🚨 [CRITICAL] 💻 LOCAL <message>` |
| AWS prod box (systemd) | `🚨 [CRITICAL] ☁️ AWS <message>` |

- ONE implementation point: `telegram_message_prefix` in the notification
  service — both the immediate path and the coalesced-summary path build
  their prefix there. No per-event edits, ever.
- Commandment 10 preserved: the severity emoji stays at the very START of
  the subject; the badge follows the severity tag block.
- The badge is plain English (one emoji + one word) — no env-var names, no
  hostnames, no paths. Honest envelope: AWS = systemd-managed process (the
  prod unit is the only systemd deployment); everything else badges LOCAL.

---

## 🎯 The 10 commandments of Telegram messages

| # | Rule | Why |
|---|---|---|
| 1 | **Plain English ONLY.** No jargon. | Operator reads on phone in 3 seconds. Wife / partner / accountant / future-you in 5 years must also read it. |
| 2 | **NO library names.** rkyv / papaya / DEDUP / arc-swap / mpsc / GCRA / DHAT / Criterion = BANNED. | Means nothing to a non-coder. |
| 3 | **NO file paths.** No `crates/core/src/...` | Phone screen too small + irrelevant. |
| 4 | **NO version numbers.** No `tokio 1.49.0` or `aws-lc-rs`. | Irrelevant to "is it working?" |
| 5 | **YES emoji for status.** ✅ working / ❌ broken / ⚠️ warning / 🚀 live / 🔔 reminder / 🆘 emergency. | Instant visual parse. |
| 6 | **YES specific numbers.** "11,034 instruments" / "5/5 connections" / "30 seconds in". | Concrete = trustworthy. |
| 7 | **YES action verbs in degraded.** "What you need to do RIGHT NOW: 1. SSH ... 2. Restart ..." | Operator panics → needs steps, not diagnosis. |
| 8 | **One Telegram = one decision.** "Is this healthy or not? What do I do?" | Avoid scrolling. |
| 9 | **Time stamps in IST 12-hour.** "9:00 AM", not "09:00:00.000Z". | Indian operator reads IST naturally. |
| 10 | **Severity ALWAYS in subject line.** ✅ / ⚠️ / 🆘 emoji + ALL-CAPS label. | Glance = know level. |

---

## 🚫 BANNED phrases in Telegram (auto-driver doesn't know these)

| Banned word | Replace with |
|---|---|
| "rkyv binary cache" | "instrument data" |
| "DEDUP UPSERT KEYS" | "duplicate protection" |
| "papaya" | "lookup table" |
| "arc-swap" | "fast switch" |
| "mpsc channel" | "internal queue" |
| "GCRA" | "rate limiter" |
| "Tokio runtime" | "task system" |
| "ILP append" | "saving to database" |
| "rescue ring" | "backup buffer" |
| "spill NDJSON" | "overflow file" |
| "DLQ" | "safety net" |
| "f32 / f64" | (just drop — never relevant) |
| "ALTER TABLE" | "updating database structure" |
| "Layer 1 / Layer 2 / Layer 3" | "first try / backup / last resort" |
| "DataAPI-807" | "Dhan expired our login" |
| "DH-901" | "Dhan rejected our login" |
| "I-P1-11" | (just drop — internal code) |
| "FeedRequestCode=23" | "subscribe message" |
| "Static IP ordersAllowed=true" | "approved to place orders" |
| "Phase 2 dispatch" | "stock list finalize" |
| "ATM ± 24 strikes" | "around current price" |
| "core_affinity" | "speed-tuning" |
| "papaya cross_segment_collisions" | (just drop) |
| "SubscribeRxGuard" | (just drop — internal) |

---

## 📱 PART 1 — The 4 pre-market pings, REWRITTEN dead-simple

### Ping 1 — 08:33 AM ✅ READY (replaces 18-line technical version)

```
✅ tickvault READY

Running on: AWS Mumbai
Took 3 minutes to start
Mode: PAPER (no real money yet)

──── 18 checks, all green ────
✅ AWS server up
✅ Database up
✅ Internet good
✅ Logged into Dhan (good for 24 hours)
✅ Loaded all 11,034 stocks + indices
✅ Live data plan: active
✅ Approved to place orders
✅ Only ONE copy of me running (no duplicate)
✅ Safety net: ready (can absorb 5 million ticks)
✅ Auto-restart: ready
✅ Daily audit: ready
✅ Backup logins: ready (3 fallbacks)
✅ Stock list scheduler: armed for 9:13 AM
✅ Watchdog: checking every 5 sec
✅ Telegram alerts: ready
✅ Speed-tuning: done (4 CPU cores assigned)
✅ Clock: in sync (0.04 sec off real time)
✅ Recovery from yesterday: clean

──── Today's plan ────
🕘 9:00 AM   — Connect to live market
🕘 9:13 AM   — Finalize stock list (using pre-open prices)
🕘 9:15 AM   — Market opens
🕘 9:15:30   — Confirm we're recording ticks
🕘 3:30 PM   — Market closes
🕘 4:00 PM   — Daily audit (compare our ticks vs Dhan's official)

🟢 ALL GOOD. Sleep easy until 9 AM.
```

### Ping 2 — 9:00 AM 🔔 MARKET OPENING

```
🔔 tickvault — Market opens in 13 minutes

✅ Connected to Dhan (5/5 live data channels open)
✅ Recording pre-open prices for 216 stocks
✅ Login good for the rest of the day

Standing by for 9:13 AM stock list finalize.
```

### Ping 3 — 9:13:30 AM ✅ STOCK LIST FINALIZED

```
✅ Stock list finalized

216 stocks subscribed (100%)
Indices: NIFTY, BANKNIFTY, SENSEX
Deep order book: NIFTY + BANKNIFTY (20-level + 200-level)

NIFTY anchor: 22,684.50 → focus around 22,700
BANKNIFTY anchor: 48,210 → focus around 48,200

Market opens in 90 seconds.
```

### Ping 4 — 9:15:30 AM 🚀 STREAMING LIVE

```
🚀 Market OPEN — streaming confirmed

30 seconds in:
✅ 487,213 ticks received and saved
✅ All 5 main connections: healthy
✅ Order book channels: healthy
✅ Zero ticks lost
✅ Backup buffer: 0% used (smooth flow)

🟢 Trading session is LIVE.
```

---

## ⚠️ PART 2 — Degraded path, REWRITTEN

### Degraded at 08:34 AM (1 subsystem failed)

```
⚠️ tickvault — something failed at startup

❌ What's broken:
   Could not log into Dhan
   (the 6-digit OTP code from our authenticator was rejected 5 times)

✅ What's working:
   Everything else (17 of 18 checks green)

🔄 Auto-fixing:
   Trying again every 60 seconds until 8:55 AM
   If still broken at 9:00 AM → no live trading today
   (paper-mode still runs, your real money is SAFE)

🛠 What YOU need to do (before 9 AM):
   1. SSH into AWS box
   2. Open Dhan web app → regenerate the TOTP secret
   3. Update it in AWS settings
   4. Restart tickvault (one command — I'll Telegram you the exact command)

Time: 8:34 AM
Detailed logs are saved on the server.
```

### Critical at 9:00 AM (system gave up)

```
🆘 tickvault — TRADING HALTED FOR TODAY

Reason: Could not log into Dhan (tried for 25 minutes)
Time: 9:00 AM

✅ Your real Dhan account is SAFE — we haven't placed any orders.
✅ Paper-mode continues running so we can audit later.

🛠 What YOU need to do (whenever you're free):
   1. SSH: ssh user@<server-ip>
   2. Check the error logs: tail /var/log/tickvault.log
   3. Regenerate Dhan TOTP secret in their web portal
   4. Update AWS secret
   5. Restart tickvault tomorrow morning

Live trading will NOT resume today.
We'll try again at 8:30 AM tomorrow automatically.

Sleep easy — nothing got broken in your real account.
```

---

## 🔔 PART 3 — Routine alert examples (during market hours)

### Mid-market WS reconnect (LOW — informational)

**BEFORE (technical):**
```
✅ [LOW] Order Update WS reconnected
Recovered after 1 consecutive failures
07:02 PM
```

**AFTER (simple):**
```
✅ Order channel briefly disconnected — fixed itself in 4 sec
(this happens 0-2× per day, normal)
Time: 7:02 PM
```

### Mid-market depth rebalance (LOW — informational, currently SUPPRESSED per Wave 6)

**BEFORE (technical):**
```
ℹ️ [LOW] Depth-20+200 rebalance: BANKNIFTY
Old strike: 48,100 → New strike: 48,200
Action: zero-disconnect swap — 20-level + 200-level unsub old / sub new on same socket
```

**AFTER (if it ever fires):**
```
ℹ️ BANKNIFTY moved — refocusing order book
Was: 48,100  →  Now: 48,200
Zero data lost (same connection, just changed contract)
```

### Critical: QuestDB down

**BEFORE (technical):**
```
🆘 [CRITICAL] BOOT-02 — boot deadline exceeded — HALTING
Failed at boot Step 8b: QuestDB DDL + schema self-heal
tv_questdb_disconnected_seconds > 60
```

**AFTER (simple):**
```
🆘 tickvault — database is DOWN

Could not connect to our local database (QuestDB) for 60+ seconds.
Trading is HALTED so we don't lose data.

🛠 Most common fix:
   1. SSH into server
   2. Run: docker restart tv-questdb
   3. Wait 30 seconds
   4. Restart tickvault

If that doesn't work → check disk space (df -h)
```

### Post-market cross-verify FAILED

**BEFORE (technical):**
```
⚠️ [HIGH] NSE bhavcopy cross-check FAILED
2026-05-11
Reason: questdb_query_failed
No audit rows produced — investigate before next trading day.
```

**AFTER (simple):**
```
⚠️ Daily audit: PROBLEM

Today's check could not compare our data vs Dhan's official numbers.
Reason: database query failed.

✅ Live trading was not affected.
🛠 What to investigate before tomorrow:
   1. Check QuestDB is healthy (Grafana dashboard)
   2. Review the failed query in the audit log
   3. If unclear → don't go live tomorrow, ask in this chat

Time: 4:30 PM today
```

---

## 🟢 PART 4 — One-line health pings during trading

**BEFORE (technical):**
```
ℹ️ tv_realtime_guarantee_score = 1.0
```

**AFTER (simple):**
```
✅ Health: 100% (all good)
```

**BEFORE:**
```
⚠️ tv_realtime_guarantee_score = 0.83  weakest_dimension=tick_freshness
```

**AFTER:**
```
⚠️ Health: 83% (warning)
Cause: tick stream slowing down for 1 stock
Auto-fix in progress.
```

---

## 🚖 PART 5 — The auto-driver litmus test

For EVERY new Telegram message proposal, ask:

| Question | If NO → REWRITE |
|---|---|
| Can the operator's wife read it and know "OK or not OK"? | rewrite |
| Are there any words that need a Google search? | remove them |
| Is there a "what to do RIGHT NOW" line on errors? | add one |
| Does the time say "9:13 AM" (not "0913" or "13:13" or "09:13 IST")? | fix |
| Is the severity emoji at the START of subject? | move it |
| Could a 60-year-old auto-driver get it in 5 seconds? | simplify |
| Does it have file paths or version numbers? | remove |

---

## 🛡️ PART 6 — Mechanical enforcement

For every new `NotificationEvent` variant in code:
1. Reviewer (or grill agent) reads the rendered Telegram output
2. Cross-checks against the banned-word list above
3. Cross-checks against the 10 commandments
4. If any rule violated → REWRITE before merge

Suggested **future ratchet test**: `crates/core/tests/telegram_message_jargon_guard.rs`
- Scans every rendered notification body
- Greps for banned words (rkyv, DEDUP, papaya, etc.)
- Fails build if any banned word appears

This is a Friday task suggestion — added as Phase 0.5 Item 0.5.19.

---

## 📌 Item 0.5.19 NEW — Telegram Jargon Guard

| | |
|---|---|
| What | Banned-word scanner for every `NotificationEvent::*::body()` output |
| LoC | ~80 |
| Severity | Low (style) |
| Closes gap | New (G47 — telegram messages too technical for operator) |
| Goes on Friday Task Board | Yes |

**Phase 0.5 total now: 19 items, ~1,770 LoC.**

---

## 🚖 Summary

> "Sir, you said: Telegram must be auto-driver simple. Locked. From now on EVERY Telegram message follows 10 rules — plain English, no library names, no file paths, emoji for status, specific numbers, action verbs on errors, ONE decision per message, IST 12-hour time, severity emoji first. I rewrote all 4 pre-market pings + degraded/critical + 4 mid-market examples in this file. Also added Item 0.5.19 (Friday task) — a mechanical scanner that fails the build if banned jargon words sneak into Telegram messages."

---

## 🎤 Floor's yours

Any phrase / message style you'd flag for further simplification?
Any other "non-technical reader must understand" rule I missed?
Or move to next angle?
