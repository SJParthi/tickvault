# AWS vs Home — latency & operations (every nook and corner)

> **Why this doc exists:** the whole reason tickvault runs on AWS `ap-south-1`
> (Mumbai) instead of a home Mac is *per-tick reliability and latency*. This is
> the honest, full pros/cons — and the **⚡ Latency** tab in the operator portal
> shows the **real measured numbers** live (see §6).

## 1. The one-line reason

We put the machine **physically next to Dhan's servers in Mumbai** so every
tick travels the **shortest, most stable path**, is processed on a
**dedicated, never-throttled CPU**, on a box that **never sleeps**, with a
**fixed IP** — instead of fighting home WiFi, a laptop that throttles/sleeps,
and a changing BSNL IP.

> Analogy: AWS = renting a stall **inside the fruit market**. Home = driving
> from your house through traffic every morning. Same fruit; one of you stands
> next to the supply.

## 2. Per-tick latency — hop by hop

A tick's journey: `Dhan → internet → our machine → parse → store`.

| Hop | 🏠 Home (BSNL + Mac) | ☁️ AWS Mumbai (m8g.large) | Why |
|---|---|---|---|
| Network RTT to Dhan | ~20–60 ms, spikes 100 ms+ | ~1–10 ms, stable | Same-region + AWS backbone |
| Jitter (variation) | High (WiFi, evening peak) | Near-zero | Datacenter vs consumer line |
| Packet loss / micro-drops | Occasional → WS reconnects | ~0 | Carrier-grade |
| WS reconnect frequency | Higher (WiFi blips, NAT) | Rare | Each reconnect = gap + re-subscribe |
| Parse (binary tick) | ~10 ns *if* CPU free | ~10 ns *always* | Same code, uncontended CPU |
| Process → RAM → QuestDB | Varies with laptop load | Consistent | No Chrome/Slack stealing cycles |

**Honest caveat:** Dhan's `exchange_timestamp` (LTT) is **1-second granular**
and the feed is edge-routed, so AWS does **not** buy microsecond HFT
co-location. It buys **low, predictable, jitter-free arrival** so our
nanosecond `received_at` capture is meaningful and no tick is delayed enough to
pile up and drop.

## 3. Advantages (pros)

| Dimension | Why AWS wins | Impact on our system |
|---|---|---|
| Latency | Same-region, low RTT, low jitter | Ticks arrive promptly & evenly → ring never floods |
| Network reliability | ~100% link uptime | Fewer reconnects = fewer gaps |
| Processing | Graviton4 fixed-performance, no throttle/sleep | O(1) hot path stays O(1); core-pinned |
| 24/7 uptime | Runs while you sleep/travel | Captures every session |
| Clock accuracy | AWS Time Sync (chrony), sub-ms | Correct candle buckets; BOOT-03 halts >2 s skew |
| Static IP (EIP) | Stable IP registered with Dhan | **Mandatory for Order APIs (SEBI, Apr 2026)** |
| Durable storage | EBS gp3 survives reboots | QuestDB data safe |
| Automation | Auto schedule, auto-deploy, CloudWatch alarms | Hands-off; portal controls from phone |
| Reproducible | Same Docker image dev=prod | No "works on my Mac" drift |

## 4. Disadvantages (cons)

| Dimension | Downside | Reality / mitigation |
|---|---|---|
| Cost | ~₹2,058/mo vs ~₹0 at home | Capped, weekday schedule, budget alarm |
| Remote from the box | Can't glance at a screen | **The portal solves this** |
| Single AZ | One Mumbai zone | Honest-envelope; manual AZ failover |
| Cold boot | ~60 s each morning | Schedule starts 08:30 IST |
| Market-hours deploy guard | No redeploy 09:15–15:30 | By design — protects the session |
| Egress data transfer | Small per-GB | ~14 GB/mo, free tier |
| Vendor lock-in | AWS APIs | Docker-based → portable |
| Per-account 429 limit | **NOT fixed by AWS** | Proven: IP rotation didn't help; cooldown is the fix |
| Dhan-side limits | 1-second LTT, edge routing | No AWS setting changes this |

## 5. Verdict for our exact use case

Intraday option-buying on indices + O(1) capture — **not** microsecond HFT.

| What matters most | AWS? |
|---|---|
| Never miss a tick (low jitter, few reconnects) | ✅ big win |
| Consistent never-throttled processing | ✅ big win |
| 24/7 capture without babysitting | ✅ big win |
| Correct clock for candle buckets | ✅ big win |
| Static IP for live orders (Apr 2026) | ✅ decisive |
| Raw microsecond edge | ➖ modest (Dhan caps at 1-s LTT) |
| Cheapest possible | ❌ home is cheaper |

**Bottom line:** AWS isn't about shaving microseconds — it's about
**reliability, consistency, uptime, correct time, and a registrable static IP**.
For a regulated live-trading system that must *never miss a tick* and *must
place orders from a fixed IP*, that's the right trade.

## 6. See the REAL numbers — portal ⚡ Latency tab

The operator portal measures these live on the box (no estimates):

| Card | What it measures | Source |
|---|---|---|
| Network to Dhan (TCP) | TCP-connect RTT to `api-feed.dhan.co` (min of 3) | `curl -w %{time_connect}` |
| + TLS handshake | TLS-complete time | `curl -w %{time_appconnect}` |
| QuestDB round-trip | local `SELECT 1` time | `curl -w %{time_total}` |
| Clock skew | offset vs NTP source | `chronyc tracking` Last offset |
| Per-tick process | avg ns to parse+process one tick | `tv_tick_processing_duration_ns` sum/count |
| Wire → done (full) | avg ns wire→parsed→routed | `tv_wire_to_done_duration_ns` sum/count |
| Order placement | avg ns to place an order | `tv_order_placement_duration_ns` sum/count |

**Budgets (from `quality/benchmark-budgets.toml`):** tick parse ≤10 ns ·
full tick processing ≤10 µs · order state transition ≤100 ns. The panel
colours green when inside budget. Processing numbers are averages since boot;
they only populate during/after a live session (need real ticks).

## 7. Trigger / cross-refs

- `quality/benchmark-budgets.toml` — the latency budgets the panel references
- `crates/core/src/pipeline/tick_processor.rs` — records the two tick histograms
- `crates/app/src/observability.rs` — `:9091/metrics` exporter + histogram buckets
- `.claude/rules/project/aws-budget.md` / `daily-universe-scope-expansion-2026-05-27.md` — instance + cost lock
