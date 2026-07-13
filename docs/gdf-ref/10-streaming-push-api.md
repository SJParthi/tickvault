# GDF 10 — Streaming "Push Type" API (StreamAllSymbols / StreamAllSnapshots)

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/streaming_api/ (landing) · https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/streaming_api/stream-all-symbols/ (operator-supplied URL) · …/advanced_streaming_api/ (hosts SubscribeOptionChain) · …/introduction/type-of-apis-available/
> **Fetched:** 2026-07-13 · **Evidence tier:** SEARCH (existence + one-liners ONLY — the function pages themselves were unfetchable and are absent from every accessible mirror). **This file is mostly a documented GAP** — see §3.

---

## 1. What IS established (Verified)

| Fact | Evidence |
|---|---|
| A separate **"Streaming API"** doc section exists — described as a "**Push Type API**": once connected, it pushes realtime and snapshot data whenever new updates are available | SEARCH: type-of-apis page + streaming_api landing (re-confirmed by a fresh search in the 2026-07-13 reconciliation pass) |
| **`StreamAllSymbols`** — "streams data of ALL symbols of the exchange" — NO per-symbol subscribe | SEARCH (function listed on the landing page) |
| **`StreamAllSnapshots`** — streams snapshot bars of all symbols | SEARCH |
| An **`advanced_streaming_api/`** section additionally hosts `SubscribeOptionChain` (07 §2) | SEARCH (URL evidence) |
| **NO official client implements either Stream function** — verified absence across ALL official SDK generations 2018 → wsgfdl-py 1.3.5 (2025-11-28) and the JS sample | CLIENT-LIB-SOURCE (grep, verified absence) |
| Zero wire-shape leakage anywhere — no request/response JSON in any search snippet, mirror, or package | Verified absence |
| This family is DISTINCT from the standard WS API's per-symbol `SubscribeRealtime`/`SubscribeSnapshot`; the standard API's package-gated concurrent-symbol limits (50–500-class per search snippets) apply to the normal WS API instead | SEARCH |

## 2. Adjacent mechanism that EXISTS today (fallback)

Whole-exchange data is already pullable WITHOUT the Streaming API: `GetExchangeSnapshot` (minute-bar granularity, entire exchange, `From`/`To` paging, max 5 snapshots/call — 04 §5) and the server-side `GetExchangeSnapshotAfterMarket` capability name. `StreamAllSnapshots` is PRESUMABLY the push variant of GetExchangeSnapshot — **Assumed, unverified**.

## 3. What is NOT known (all → `99-UNKNOWNS.md` U-1)

| Gap | Why it matters |
|---|---|
| Request/response wire JSON (both functions) | cannot build a client without it |
| Same socket/port as the standard WS API, or a separate endpoint? | connection architecture |
| Tick-level or bar-level for StreamAllSymbols? Cadence? | whether it replaces per-symbol SubscribeRealtime for a large universe |
| Scope: whole exchange vs the key's entitled symbol set? | entitlement + filtering design |
| Bandwidth (an NFO-wide 1s stream could be very large) | host sizing |
| Entitlement/pricing (separate SKU?) | purchase decision |
| Interaction with `AllowedInstruments` symbol caps | quota model |

**This is the #1 sales/live-probe item.** For tickvault, a stream-all mode would eliminate per-symbol subscribe batching entirely (at the cost of app-side watch-set filtering) — but nothing may be designed against it until U-1 is answered.

## What this prevents

- Designing feed-#3 around a function family whose wire format has literally never been observed.
- Confusing `advanced_streaming_api`'s SubscribeOptionChain (known shape, 07 §2) with the unknown Stream* pair.
