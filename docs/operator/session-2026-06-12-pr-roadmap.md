# What got built today — the simple picture (2026-06-12)

> **One-line story:** Sir, imagine your trading shop has a **price phone line** to the market.
> Today's work makes sure that whenever that phone line **drops or reconnects**, you (a) get a
> phone alert with the *exact reason*, (b) it's written in the permanent **black-box recorder**,
> and (c) you can **read the shop's diary yourself** without asking anyone. Plus a few safety
> switches and clean-up buttons. Nothing secret ever leaks into the diary.

## ✅ FINISHED today (all merged into the main product)

| # | In plain English (what it does FOR YOU) | Status |
|---|---|---|
| #1102 | Stops a weird symbol in an alert from **silently eating the whole alert** — you now always receive every alert | ✅ merged |
| #1103 | When the database goes down you got a "DOWN" alert but never a **"it's back up"** alert. Now you do. | ✅ merged |
| #1104 | If the morning instrument list **fails to load**, you now get a phone alert instead of a silent death | ✅ merged |
| #1105 | First fix attempt to make the shop's **diary actually reach the cloud** (logs were empty) | ✅ merged |
| #1106 | Wrote the **plan** for "tell me WHO dropped the phone line — us, the internet, or the broker" | ✅ merged |
| #1107 | **Found + fixed the real reason** the cloud diary was empty (one missing setting blocked everything) | ✅ merged |
| #1108 | The disconnect alert now **names the likely culprit**: broker / network / token — in plain words | ✅ merged |
| #1109 | I (Claude) can now **read your cloud diary myself**, automatically — no copy-paste from you | ✅ merged |
| #1110 | A **"☢️ Bare Nuke"** button — wipes ALL docker (containers+images+volumes) and leaves the box empty, like deleting everything by hand on a Mac | ✅ merged |
| #1111 | **Every** phone-line drop/reconnect is now written to the cloud diary with the exact reason — and a **secret-leak hole was found + sealed** (your login token can never appear in the diary) | ✅ merged |

## 🔵 CURRENT (just opened, finishing the merge now)

| # | In plain English | Status |
|---|---|---|
| #1112 | A permanent **black-box recorder** (`ws_event_audit`): every connect / disconnect / reconnect / sleep of the phone line gets a forensic row in the database — answerable months later ("show me every drop this month, with the reason + how long it was down"). **Built to scale to your future 16 phone lines** (5+5+5+1) with zero rework. | 🔵 auto-merge armed, CI running |

## 🟡 FUTURE — queued next (in order)

| Next | In plain English | Why it's small |
|---|---|---|
| Order-update recorder | Wire the SAME black-box recorder onto the **order-confirmation** phone line too | The slot is already built + tested in #1112 — just plug it in |
| 3 missing safety pings | Turn on 3 alerts that exist but were never switched on: **token-renewal-late**, **boot-health**, **late-tick** | One small switch each |
| Retire a dead alarm | Remove a "gave-up-reconnecting" alarm that can **never fire** (we retry forever now) | Pure cleanup |
| Pretty log viewer | A nice **animated web page** to read the diary like a movie, instead of raw text | Front-end only |

## 🔴 DELIBERATELY NOT TOUCHING (your explicit instruction)

| Item | Why we're leaving it |
|---|---|
| Shrinking the AWS instance | You said don't downgrade |
| Order / risk safety alerts | You said leave these — they need your live-trading decision, not mine |
| Adding real depth phone lines | Still locked to 2 lines; adding more needs your separate go-ahead (today's work only makes the *recorder* ready for them) |

## The promise we keep on every one of these
- 🔒 No secret (your login token) can ever reach a log, the recorder, or an alert — proven by tests.
- 🧱 Three robots inspect every change before it ships: speed, security, and a hostile bug-hunter.
- ✅ One change at a time, merged before the next starts — never half-finished.
- 📣 No exaggeration: we say exactly what's guaranteed and exactly what isn't.
