# Live-Feed Health — the whole journey, in one table

> **The ultimate aim (your words, 2026-06-22):** *"forget about backtest… our
> extreme ultimate aim is to achieve the LIVE FEED CHECK"* — a webpage + an API
> that, in real time, **truthfully** says for EACH feed: **OK / not-OK**, with
> ticks flowing, candles generated, nothing lost, reconnects handled.
>
> **Where we are:** the engine, the registry, the endpoint, and the Groww wiring
> are MERGED or merging. One more slice (SP5) lights up Dhan's per-tick freshness.

---

## 🍊 The juice-shop picture (5-second read)

> Sir, imagine two price-boards in your shop — **Dhan** and **Groww**. You wanted
> ONE honest light on each board that says GREEN (working), RED (broken), GREY
> (switched off), or AMBER (we haven't wired the sensor on that board yet — so we
> say "don't know" instead of lying RED). This whole list is us building that one
> honest light, board by board, never faking the colour.

---

## ✅ DONE / 🔜 IN-FLIGHT / 🛠️ NEXT — every PR

| PR | What it delivered | Why it matters to YOU | Status |
|----|-------------------|------------------------|--------|
| **#1175** | NTM 3-role boot-panic fix — universe split (Index / F&O-underlying / Index-constituent) now sums correctly | The Mac boot crash (`775 ≠ 244`) you hit is gone; the live run won't panic on the role count | ✅ Merged |
| **#1176** (SP1) | `Feed` enum — ONE list of all feeds (`Feed::ALL`), dense index, parse/labels | The single source of truth so every page/endpoint covers **all** feeds, never a hard-coded row | ✅ Merged |
| **#1177** (SP2) | `feed_parity` comparator — exact 1-min OHLCV compare, false-OK guarded | The honest "live == its own record?" check primitive, reusable per feed | ✅ Merged |
| **#1178** | Feed-control webpage made **fully dynamic** — rows rendered from `Feed::ALL`, zero static rows | Your "I need all dude, no static, always dynamic" — add a feed later, the page grows itself | ✅ Merged |
| **#1179** (SP3a) | `OneMinFoldCell` — ONE common 1-minute candle engine (cumulative volume, late-tick policy) | The "everything downstream is ONE common engine" lock — Dhan & Groww fold the same way | ✅ Merged |
| **#1180** (SP3b) | Groww 1-min aggregator delegates to the common fold cell (golden-tested) | Groww candles now use the **same** engine as everything else — no parallel logic to drift | ✅ Merged |
| **#1181** (Health SP1) | `evaluate_feed_health` — the truthful per-feed verdict engine (Disabled/Ok/Degraded/Down) | The brain of the live-feed light. Pure, offline-tested, no false green | ✅ Merged |
| **#1182** (Health SP2) | `FeedHealthRegistry` — lock-free per-feed counters (ticks/candles/drops/connected) | The O(1) place lanes write health signals to; one slot per feed, scales automatically | ✅ Merged |
| **#1183** (Health SP3) | `GET /api/feeds/health` — one truthful JSON row per feed, behind auth | The endpoint you (or the page) hit to SEE each feed's live verdict in real time | ✅ Merged |
| **#1184** (Health SP4) | Wires the **Groww** lane into the registry + shares it into boot; **+ Q6/Q2 hardening** | Groww's light goes truly live (connected + ticks + candles + last-tick age). **No false RED** (un-wired feeds report `unknown`), **no clock-skew false "silent"** (records receipt time) | 🔜 In-flight (auto-merge armed) |
| **Health SP5** | Wire **Dhan**'s per-tick freshness — `record_tick(Dhan)` next to the existing core heartbeat + `set_connected(Dhan, pool>0)` | Dhan's light goes truly live too — **without touching the hot path you're running** (additive, O(1), next to a signal that already exists) | 🛠️ Next (planned) |
| **Health SP6** | Render the verdict on the `/feeds` webpage (GREEN/RED/GREY/AMBER badges, auto-refresh) | The visual dashboard — glance at the page, see every feed's colour, no JSON reading | 🛠️ Future |

---

## 🔬 What the live-feed light actually checks (the honest verdict logic)

| Light | Means | Fires when… |
|-------|-------|-------------|
| 🟢 **OK** | Live & healthy | enabled + lane running + connected + (market open ⇒ a recent tick) + **zero drops** |
| 🟡 **Degraded** | Working but wounded | lane wasn't started at boot, **or** ticks are being dropped |
| 🔴 **Down** | Enabled but dark | socket disconnected, **or** silent past 30s during market hours |
| ⚪ **Disabled** | Switched off | you turned the feed off — intentional, **not** a fault |
| 🟠 **Unknown** | Sensor not wired yet | feed enabled + running but its record-sites haven't landed (e.g. Dhan until SP5) — **honest "don't know", never a fake RED** |

**No-illusion guarantees baked in (from the adversarial review):**
- A silent feed during market hours is **Down**, never a misleading green (audit Rule 11).
- An un-instrumented feed is **Unknown**, never a scary false Down (Q6 fix).
- Last-tick age uses **wall-clock receipt time**, so exchange-clock skew can't fake "silent" (Q2 fix).
- Per-feed isolation: a problem on Dhan can't colour Groww's light, and vice-versa.

---

## 🧱 The architecture lock you asked for (common runtime, dynamic, scalable)

> *"only feed live ticks fetching/pulling is feed-specific; everything else is
> common runtime dynamic scalable."*

| Layer | Feed-specific? | Proof |
|-------|----------------|-------|
| Live-tick fetch (Dhan WS / Groww sidecar) | **YES** — the only feed-specific part | separate Dhan WS vs Groww bridge |
| Tick validation, dedup, WAL→ring→spill→DLQ | NO — one common chain | both feeds reuse it |
| 1-minute candle fold | NO — `OneMinFoldCell` | SP3a/SP3b |
| Health verdict | NO — `evaluate_feed_health` over `Feed::ALL` | SP1–SP4 |
| Storage tables | NO — shared `ticks`/`candles_1m`, tagged `feed=` | one DEDUP key + `feed` |
| Webpage + API | NO — render `Feed::ALL` | #1178, SP3 |

**Add a third feed tomorrow** → write only its tick fetcher; the registry slot,
the verdict, the page row, the candle fold, the tables all appear for free.

---

## ▶️ What YOU can do once SP4 merges (and SP5 after)

1. Start the app. Open **`/feeds`** → flip Dhan/Groww on or off (single or multiple).
2. Only the **selected** feeds run and store — real-time, no restart for Groww.
3. Hit **`GET /api/feeds/health`** (or, after SP6, just watch the page) → each feed's
   honest light: connected, ticks/candles counts, last-tick age, verdict.
4. Run it live, send me the logs — the verdict is computed from the **same**
   registry the lanes write, so what the page shows is what's truly happening.

> **Honest envelope:** today the endpoint tells the truth for **Groww** (SP4) and
> reports **Dhan** as `unknown` until **SP5** wires Dhan's per-tick signal — by
> design, so this work never touches the live Dhan hot path you're running.
