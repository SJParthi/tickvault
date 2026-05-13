# Topic — Crisis / Outage / Breakage Telegram Visualization

**Created:** 2026-05-12 00:15 IST
**Trigger:** Operator: "Pick A approved (full rich treatment 0.5.20). Now show me how CRITICAL / breakage / outage / issues will visually display."
**Mode:** Brainstorm + design (no implementation)
**Status:** Extends Phase 0.5 Item 0.5.20 — adds crisis-tier visual hierarchy

---

## 🎯 The 5-tier visual severity hierarchy

| Tier | Color | GIF | Sound | Pin to top? | Repeat if no ack? |
|---|---|---|---|---|---|
| 🟢 **INFO** (LOW) | green | static ✅ checkmark | default | no | no |
| 🔵 **NORMAL** (MEDIUM) | blue | subtle pulse | default | no | no |
| 🟡 **WARNING** (HIGH) | yellow | pulsing ⚠️ triangle | default+vibrate | no | no |
| 🔴 **CRITICAL** | red | flashing 🆘 siren (1s loop) | URGENT sound | **YES** | every 5 min |
| 🚨 **EMERGENCY** | dark red + black | strobe ALARM (0.5s loop) | URGENT + AWS SNS SMS | **YES** | every 2 min + escalates to SMS |

Bot uses Telegram API `disable_notification=false` + sound override + pinned message + auto-repeat scheduler.

---

## 🟡 WARNING TIER — example: 1 connection dropped

**GIF:** pulsing yellow triangle (subtle, 1.5s loop)

**Sound:** default Telegram notification

**Pinned:** no

**Message:**

```
⚠️ <b>WARNING — 1 connection dropped</b>
<i>09:42 AM · Self-healing in progress</i>

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔌 <b>CONNECTION STATUS</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Main feed:   🟢 🟢 🔴 🟢 🟢  (4/5)
Depth-20:    🟢 🟢
Depth-200:   🟢 🟢 🟢 🟢
Order WS:    🟢

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔄 <b>AUTO-RECOVERY</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Conn #3 disconnected at 9:42:14 AM
Reconnect in progress... <code>▓▓▓░░░░░░░</code> 30%
Expected restore: 9:42:19 AM (5 sec)

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🛡 <b>NO TICK LOSS</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Safety buffer absorbed 1,247 ticks during gap
No operator action needed.
```

**Inline buttons:**
```
[ 📊 Dashboard ]  [ 📜 What happened? ]
```

**Follow-up at 9:42:20 AM (5 sec later)** — live-edit the same message:

```
✅ <b>RECOVERED — all 5 connections back</b>
<i>Was down for 6 seconds · No tick loss</i>

Main feed: 🟢 🟢 🟢 🟢 🟢 (5/5)

Total ticks during gap: 1,247 (all preserved in safety buffer)
```

---

## 🔴 CRITICAL TIER — example: QuestDB outage

**GIF:** flashing red 🆘 siren (1s loop, attention-grabbing)

**Sound:** URGENT (Telegram override — custom sound `urgent.ogg`)

**Pinned to chat top:** YES (until acknowledged)

**Repeat every 5 min** if not acked

**Initial message at 10:15:00 AM:**

```
🆘 🆘 🆘  <b>CRITICAL — DATABASE DISCONNECTED</b>  🆘 🆘 🆘
<i>10:15:00 AM · Severity: CRITICAL · Auto-retry active</i>

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
💥 <b>WHAT BROKE</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  QuestDB stopped responding 60 sec ago
  Ticks are flowing into safety buffer NOT to database

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🛡 <b>SAFETY NET ACTIVE</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Layer 1 (RAM ring):   <code>▓▓░░░░░░░░</code>  20% used
Layer 2 (Disk spill): <code>░░░░░░░░░░</code>   0% used
Layer 3 (DLQ):        <code>░░░░░░░░░░</code>   0% used

Capacity remaining: <b>4,000,000 ticks (~4 min)</b>

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔄 <b>AUTO-RECOVERY IN PROGRESS</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Attempt 1: failed (10:14:00)
Attempt 2: failed (10:14:30)
Attempt 3: trying now... <code>▓▓▓▓▓░░░░░</code>

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🛠 <b>YOUR ACTION (if not auto-fixed in 2 min)</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  <b>1.</b> SSH into AWS box
  <b>2.</b> <code>docker restart tv-questdb</code>
  <b>3.</b> Wait 30 seconds
  <b>4.</b> Watch this Telegram for ✅ recovery message

If still broken: check disk space → <code>df -h</code>
```

**Inline buttons:**
```
[ 📊 Dashboard ]  [ 🚑 Runbook ]  [ ✅ I'm on it ]  [ 🔁 Force Retry ]
```

Operator taps `✅ I'm on it` → bot stops the 5-min re-pinging + records ack in audit table.

**Live-edits every 10 sec** to show new safety buffer level. Operator watches buffer fill in real-time.

**At 10:18:00 AM — recovered message (live-edit same message):**

```
✅ <b>RECOVERED — DATABASE BACK ONLINE</b>
<i>Was down for 3 min · 42,891 ticks flushed from buffer</i>

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
📊 <b>FINAL SAFETY BUFFER STATE</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Peak usage: 31% (1.5M of 5M)
Tick loss: 0 ✅
Buffer drained back to 0% in 12 sec after recovery

🟢 Normal operation resumed
```

[Pin removed automatically. Sound goes back to default.]

---

## 🚨 EMERGENCY TIER — example: Dual-instance detected (the G38 dragon)

**GIF:** strobe ALARM (0.5s loop, extremely attention-grabbing)

**Sound:** URGENT + AWS SNS SMS sent to operator phone

**Pinned to chat top:** YES (cannot be unpinned automatically — operator must manually)

**Repeat every 2 min** + escalates to SMS after 5 min

**Message at any time it fires:**

```
🚨🚨🚨 <b>EMERGENCY — DUPLICATE TICKVAULT DETECTED</b> 🚨🚨🚨
<i>10:23:18 AM · Severity: EMERGENCY · BOOT HALTED</i>

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
💀 <b>THE DANGER</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Two tickvault instances are trying to run simultaneously:

  Instance A: <code>host_id=aws-mumbai-1, boot_ts=10:23:01</code>
  Instance B: <code>host_id=mac-parthi, boot_ts=10:23:18</code>  ← THIS one

If both connect to Dhan:
  💀 Orders fire 2×
  💀 Position size doubles
  💀 Real money at risk

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🛡 <b>WHAT WE DID</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  ✅ THIS instance (B) REFUSED to boot
  ✅ Only Instance A is running
  ✅ Your account is SAFE — no duplicate orders

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🛠 <b>YOUR ACTION RIGHT NOW</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  <b>1.</b> Identify which instance you actually wanted (A or B)
  <b>2.</b> Stop the WRONG one:
       • Stop A:  ssh AWS → <code>make stop</code>
       • Stop B:  On Mac → Ctrl+C in terminal
  <b>3.</b> Wait 60 seconds (live-lock expires)
  <b>4.</b> Restart the RIGHT one
```

**Inline buttons:**
```
[ 🆘 Emergency Runbook ]  [ 📜 Audit Log ]  [ 🛑 Kill BOTH ]  [ ✅ I'm on it ]
```

`🛑 Kill BOTH` button → bot sends SIGTERM to both instances + Telegram confirmation. Operator's nuclear option.

**Also sent via AWS SNS SMS:**
```
🚨 tickvault EMERGENCY 10:23 AM
DUPLICATE INSTANCE DETECTED
Boot halted — your account safe
Open Telegram for full info + fix
```

---

## 💀 CASCADE — example: 3 things break in 60 seconds

When multiple alerts fire within 60s, Telegram coalescer prevents spam — sends ONE consolidated message.

**At 11:05:00 AM (after 3 alerts in 47 sec):**

```
🆘 <b>CASCADE FAILURE — 3 critical events in 47 sec</b>
<i>11:05:00 AM · Coalesced from 3 alerts</i>

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
💥 <b>FAILURE TIMELINE</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  <code>11:04:13</code>  🔴 WebSocket #2 dropped
  <code>11:04:31</code>  🔴 Token expired (DH-901)
  <code>11:05:00</code>  🆘 QuestDB disconnected

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🎯 <b>ROOT CAUSE LIKELY</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  Network egress blocked from AWS Mumbai
  (3 separate failures, all network-related, in 47 sec)

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🛡 <b>SAFETY NET STATE</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Buffer:  <code>▓▓▓▓░░░░░░</code>  40% (2M of 5M ticks)
Time to overflow if not resolved: ~3 min

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔄 <b>AUTO-RECOVERY</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  ✅ Token refresh: in progress
  🔄 WS reconnect: 5/5 attempting
  🔄 QuestDB ping: every 10 sec
  ⏱  Network test (DNS resolve api.dhan.co): trying...

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🛠 <b>YOUR ACTION (if not auto-fixed in 3 min)</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  <b>1.</b> SSH into AWS box
  <b>2.</b> Test internet: <code>ping 8.8.8.8</code>
  <b>3.</b> If fails: check AWS console for region issue
  <b>4.</b> If passes: <code>tail -f /var/log/tickvault.log</code>
```

**Inline buttons:**
```
[ 📊 Live Dashboard ]  [ 🚑 Cascade Runbook ]  [ ✅ I'm on it ]
```

---

## 🎬 Visual hierarchy comparison side-by-side

```
   🟢 INFO         🔵 NORMAL       🟡 WARNING      🔴 CRITICAL      🚨 EMERGENCY
   
   Static          Pulse           Pulse           Flash 1s         Strobe 0.5s
   ✅              ℹ️              ⚠️              🆘 🆘 🆘         🚨🚨🚨
   
   Default sound   Default sound   Default+vibrate URGENT sound     URGENT + SMS
                                                   pinned           pinned + escalates
   
   No repeat       No repeat       No repeat       Every 5 min      Every 2 min
                                                   until acked      + SMS after 5 min
```

---

## 📋 Specific crisis scenarios + their visual

### Scenario 1 — All 5 WS connections drop (network blip)

| Tier | 🔴 CRITICAL |
|---|---|
| GIF | flashing red siren |
| Sound | URGENT |
| Pinned | YES |
| Title | `🆘 CRITICAL — ALL WEBSOCKET CONNECTIONS DROPPED` |
| Subtitle | "0/5 main feed active · Pool reconnecting in parallel" |
| Buffer state | Show real-time `▓▓▓░░░░░░░ 30%` |
| Action steps | "Wait 30 sec. If not recovered → check Dhan status page" |
| Inline buttons | `[ 📊 Dashboard ] [ 🌐 Dhan Status ] [ ✅ I'm on it ]` |

### Scenario 2 — Auth expired + can't refresh (DH-901 × 5)

| Tier | 🔴 CRITICAL |
|---|---|
| GIF | flashing red 🔐 lock icon |
| Sound | URGENT |
| Pinned | YES |
| Title | `🆘 CRITICAL — AUTHENTICATION FAILED` |
| Subtitle | "Cannot log in to Dhan · TOTP rejected 5×" |
| Action steps | "1. SSH 2. Regenerate Dhan TOTP secret 3. Update AWS secret 4. Restart" |

### Scenario 3 — OOM kill imminent (RAM > 90%)

| Tier | 🔴 CRITICAL |
|---|---|
| GIF | flashing red 💾 memory icon |
| Sound | URGENT |
| Title | `🆘 CRITICAL — MEMORY ALMOST EXHAUSTED` |
| Body | "RSS = 7.2GB / 8GB total · OOM kill imminent · Tick processing slowing" |
| Action | "1. Investigate memory leak 2. If urgent → restart tickvault" |

### Scenario 4 — Disk full (data/ partition > 90%)

| Tier | 🔴 CRITICAL |
|---|---|
| GIF | flashing red 💿 disk icon |
| Sound | URGENT |
| Title | `🆘 CRITICAL — DISK ALMOST FULL` |
| Body | "data/ partition: 92% used (47GB / 50GB) · Spill files cannot grow" |
| Action | "1. SSH 2. `du -sh data/*` 3. Archive old QuestDB partitions to S3" |

### Scenario 5 — Cross-verify FAILED (data integrity issue)

| Tier | 🟡 WARNING (not critical — already settled) |
|---|---|
| GIF | pulsing yellow ⚖️ scale icon |
| Sound | default+vibrate |
| Title | `⚠️ DAILY AUDIT FOUND MISMATCHES` |
| Body | "Our 2,341,891 ticks vs Dhan's 2,341,894 ticks → 3 ticks missing" |
| Show first 10 mismatches in fixed-width table |
| Action | "1. Review the 3 missing ticks list 2. Decide if live trading tomorrow" |

### Scenario 6 — Dhan-side outage (their problem)

| Tier | 🟡 WARNING |
|---|---|
| GIF | pulsing yellow 🏢 building icon |
| Sound | default |
| Title | `⚠️ Dhan API not responding` |
| Body | "Not our issue — Dhan's servers slow · Other tickvault users likely affected too" |
| Action | "Wait 5 min. If still slow → @ Dhan support" |
| Inline button | `[ 🌐 Dhan Status Page ]` |

### Scenario 7 — App panic / Tokio deadlock (Byzantine — looks healthy isn't)

| Tier | 🆘 CRITICAL |
|---|---|
| GIF | flashing red 🧟 zombie icon (`/health` returned 200 but no progress) |
| Sound | URGENT |
| Title | `🆘 CRITICAL — APP STUCK (LOOKS HEALTHY BUT ISN'T)` |
| Body | "Liveness probe failed · `/health` says 200 OK but no ticks for 90 sec" |
| Action | "1. SSH 2. `systemctl restart tickvault` 3. Watch this chat for recovery" |

### Scenario 8 — Strategy fired bad order (risk breach)

| Tier | 🚨 EMERGENCY (if dry_run=false; just WARNING if dry_run=true paper) |
|---|---|
| GIF | strobe ALARM + dollar sign |
| Sound | URGENT + SMS |
| Title | `🚨 EMERGENCY — RISK LIMIT BREACHED` |
| Body | "Daily loss exceeded ₹2,000 budget · Kill switch activated · All positions exited" |
| Action | "1. Review trades 2. Investigate why strategy fired" |

---

## 🔁 Cascade auto-escalation chain

```
   T+0s     Issue detected → 🆘 Telegram message + pin
   T+5min   No ack → Re-send Telegram (same content, pinned-top refresh)
   T+10min  Still no ack → 🚨 escalate to EMERGENCY tier + SMS
   T+15min  No ack → SMS again + email to operator's backup address
   T+30min  No ack → call AWS Lambda voice-call (future enhancement)
```

Operator can ack via:
- Tap `✅ I'm on it` button → records timestamp, stops escalation
- Reply with any text → auto-acks
- Set `/mute 1h` → suppresses all but EMERGENCY for 1 hour

---

## 🎵 Custom Telegram notification sounds

Operator uploads to Telegram Bot:
| Tier | Sound file | Description |
|---|---|---|
| INFO / NORMAL | default | standard Telegram chime |
| WARNING | `warn-double-beep.ogg` | two short beeps |
| CRITICAL | `critical-siren.ogg` | 3-tone urgent alert |
| EMERGENCY | `emergency-strobe.ogg` | aggressive alarm |

Bot uses Telegram `disable_notification=false` + per-chat custom sound profile.

---

## 🟢 Recovery messages — celebrate the fix

When a CRITICAL or EMERGENCY resolves, send a celebratory message:

```
✅ <b>RECOVERED — System back to healthy</b>
<i>10:18:00 AM · Was critical from 10:15:00 to 10:18:00 (3 min)</i>

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
📊 <b>INCIDENT SUMMARY</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Issue:        Database disconnected
Duration:     3 minutes
Tick loss:    0 ✅
Buffer peak:  31% (1.5M of 5M)
Auto-fix:     ✅ Docker restart succeeded

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
✅ <b>NO ACTION NEEDED — back to normal</b>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Audit log entry created in: questdb_outage_audit table
```

**Inline button:** `[ 📊 Post-mortem ]` → links to QuestDB query showing the full incident

---

## 📋 Phase 0.5 Item 0.5.20 expanded — Crisis-Tier additions

| Sub-item | What | LoC |
|---|---|---|
| 0.5.20a | HTML mode + Unicode bars + sparklines (existing) | 80 |
| 0.5.20b | PNG charts via plotters (existing) | 200 |
| 0.5.20c | Animated GIFs — 12 assets (5 healthy + 7 crisis variants) | 150 |
| 0.5.20d | Inline keyboards + callback handler (existing) | 150 |
| 0.5.20e | Live message editing (existing) | 200 |
| **0.5.20f NEW** | **Severity-tier visual hierarchy** (sound override + pinning + auto-repeat escalation) | 200 |
| **0.5.20g NEW** | **Cascade coalescer with root-cause hint** | 100 |
| **0.5.20h NEW** | **Recovery celebration messages + post-mortem links** | 80 |

**Item 0.5.20 grows: 5 sub-items → 8 sub-items, ~730 LoC → ~1,160 LoC.**

**Phase 0.5 total: 20 items, ~2,930 LoC.** Friday parallel session distribution:
- Session 5 absorbs 0.5.20a + 0.5.20c + 0.5.20f + 0.5.20h = ~510 LoC
- Wave 6 follow-up gets 0.5.20b + 0.5.20d + 0.5.20e + 0.5.20g = ~650 LoC (deferred polish)

---

## 🚖 Auto-driver / Insta-reel summary

> "Sir, normal day → 4 calm green ✅ messages with progress bars + tiny GIFs. Looks like a polished trading app.
>
> Something breaks → escalating drama:
> - 🟡 YELLOW pulse for "1 thing wrong, self-healing in 5 sec"
> - 🔴 RED flashing siren + URGENT sound + pinned-to-top for "database down 1 min, you may need to act"
> - 🚨 EMERGENCY strobe + SMS to your phone + auto-resend every 2 min for "duplicate instance — real money at risk"
>
> No SMS spam on healthy days. ONLY when something actually matters.
>
> Each crisis Telegram has CLICKABLE buttons: 📊 see dashboard, 🚑 see runbook, ✅ I'm on it (stops the repeat), 🔁 force retry, 🛑 emergency kill switch.
>
> When fixed → ✅ green celebration message with summary: 'Was down 3 min, zero ticks lost, here's why.' Closure."

---

## 🎤 Status

| Pick | Outcome |
|---|---|
| **A** (full 730 LoC rich treatment) | LOCKED Mon 23:55 IST |
| Crisis visualization | This file (Item 0.5.20 expanded to 1,160 LoC) |
| Severity tiers | 5 tiers locked (INFO/NORMAL/WARNING/CRITICAL/EMERGENCY) |
| Auto-escalation | T+5min Telegram repeat / T+10min SMS / T+15min email |
| Recovery messages | Celebrate the fix with post-mortem link |

---

## 🎤 What's next dude

You've now got:
1. ✅ Normal-day 4-ping rich animated visualization (Item 0.5.20a-e)
2. ✅ Crisis-tier visual hierarchy (Item 0.5.20f-h NEW)
3. ✅ 8 concrete crisis scenarios with visual treatment
4. ✅ Auto-escalation chain (Telegram → SMS → email → voice)
5. ✅ Recovery celebration messages

Pick:
- **A** "Lock all 1,160 LoC for Friday Session 5"
- **B** "Defer crisis-tier (f/g/h) to Wave 6 — Friday does 730 LoC only"
- **C** "Show me a specific scenario rendered (e.g., what does Scenario 7 zombie look like exactly?)"
- **D** "Next angle — different topic entirely"

Floor's yours. 🫡
