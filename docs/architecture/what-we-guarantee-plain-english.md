# What tickvault Guarantees — in plain English

> **Who this is for:** anyone. No code, no jargon, no file names. If you can
> read a phone screen, you can understand every promise tickvault makes here.
>
> **The honesty rule:** we never say "never." We say exactly what we promise,
> exactly where the limit is, and exactly what happens at that limit. No
> illusion. If a promise has an edge, the edge is written down — and an alarm
> rings the moment we reach it.
>
> **The engineer's version** (with the exact test that proves each row) lives
> in `guarantees.md`. THIS page is the same truth, in human words.

---

## 🧃 The one-line story

> Imagine a juice shop that takes a million orders a second. Not one order
> is ever lost. The shop never jams. If the fridge breaks, a backup fridge
> catches every fruit. If the lights go out, every order is written on paper
> so nothing is forgotten. And a phone rings the instant anything wobbles —
> before the customer even notices.
>
> tickvault is that juice shop, for stock-market price ticks.

---

## 1. "Not a single tick is ever lost"

| In plain words | The honest truth |
|---|---|
| Every price update is written to disk **first**, before anything else can drop it. | Even if everything downstream jams or crashes, the update is already safe on disk and replays when we restart. **Zero lost.** |
| If the database goes down, updates pile into a safety net (memory → disk → backup file). | Tested: database down → **0 lost**. Disk full → a backup text file catches them → **0 lost**. Power-kill mid-write → replays on restart → **0 lost**. |
| The only way to truly lose one | Memory full **and** disk full **and** backup file unwritable — all at the same instant. That's physics, not a bug. **And the moment it happens, your phone rings** (a dedicated alarm) — it is never silent. |

> **Picture it:** a fruit on a conveyor belt. Before it can fall off, a net is
> already under it. Then a second net. Then a third. Only if all three nets
> tear at once does a fruit drop — and a siren goes off instantly.

---

## 2. "Ultra-fast — every answer in a blink"

| In plain words | The honest truth (measured, not claimed) |
|---|---|
| Looking up any instrument takes the **same tiny time** whether we track 4 or 4 million — it never slows down as we grow. | Measured: ~10 billionths of a second. Same speed for a found item and a missing item — proof it doesn't slow down with size. |
| Reading a price packet | ~15 billionths of a second, fixed — never changes with input. |
| Processing one tick end-to-end inside the shop | ~5 billionths of a second per tick. |
| The honest edge | The *network* trip to the exchange is ~thousandths of a second — that's the speed of the internet, which no software can beat. Inside our shop, everything is blink-fast. |

> **"O(1)" in human words:** the time to do the job does **not** grow when the
> pile grows. Finding one fruit in a basket of 4 takes the same time as in a
> basket of 4 million. That's the promise — and we measure it to prove it.

---

## 3. "Never a duplicate, never a mix-up"

| In plain words | The honest truth |
|---|---|
| Every instrument is identified by **two things together**, never one. | The exchange reuses the same number for different things — so we always pair the number with its market. Two different things can never collide into one. |
| The same price, written twice, becomes one row. | The database is told the unique key up front, so a repeat simply overwrites — never a phantom double. |

> **Picture it:** two students both named "Rahul." We never file by first name
> alone — always "Rahul + class." So the two Rahuls never get each other's
> report card.

---

## 4. "Something broke? You know in seconds — automatically"

| In plain words | The honest truth |
|---|---|
| 18 always-on watchers | Each watches one vital sign — feed alive, database alive, disk space, login still valid, ticks flowing, etc. Any one trips → your phone, SMS, email, and a voice call. |
| No silent failures | Every error is written down within ~1 second and the important ones page you. A success message is only sent when there's real proof behind it — never a hollow "all OK." |
| Even if our own monitoring host dies | A separate watcher living elsewhere notices and pages you anyway. |

> **Picture it:** smoke detectors in every room, plus one in the neighbour's
> house pointed at yours — so even if your whole house loses power, the alarm
> still rings.

---

## 5. "It deploys itself — no human babysitting"

| In plain words | The honest truth |
|---|---|
| New code goes live automatically before market open and after close. | Never during trading hours (no risk to a live session). |
| Two alarm clocks, not one | The main scheduler does it; a second, independent one checks 5 minutes later "did it actually happen?" — and does it itself if the first overslept. |
| It never deploys on a guess | If it can't be **certain** something's out of date, it does nothing — better to wait than to act on doubt. |

---

## 6. "Tested every way, automatically, on every change"

| In plain words | The honest truth |
|---|---|
| ~6,600 automated tests | Run on every single change before it can go live. |
| Every kind of test | Normal cases, broken cases, empty cases, extreme cases, random-garbage cases, speed cases, crash-and-recover cases. |
| Can't sneak past | A change that deletes a test, slows the hot path, or breaks a promise on this page is **blocked automatically** — it cannot merge. |
| Recently hardened | We found and fixed a gap where some tests only ran *after* code went live; now they run *before*, on every change. |

---

## The promise, and the honest edge (side by side)

| What people wish for | What we actually promise (and prove) |
|---|---|
| "Never lose a tick, ever" | Zero loss in every real failure — slow db, db down, crash, restart, disk full. The one physical edge (memory **and** disk **and** backup all full at once) is **alarmed, never silent.** |
| "Infinitely fast" | Blink-fast inside the shop (billionths of a second, measured). The network trip to the exchange is the internet's speed — nobody beats that. |
| "Never breaks" | We can't stop a third party or the internet from hiccuping. But we **detect it in seconds, recover automatically, and ring your phone.** |
| "Trust me, it's safe" | No "trust me." Every promise on this page has a real automated test behind it (see the engineer's version). If a test goes missing, the build fails. |

---

## 🎤 The 5-second version

> Not one tick lost. Blink-fast. Never a duplicate. If anything wobbles, your
> phone rings in seconds — automatically. Deploys itself. Tested ~6,600 ways
> on every change. And every promise here is backed by a real test, not a
> "trust me." That's tickvault.
