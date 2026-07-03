# Groww Second Feed — Pluggable-Feeds Scope Extension (Operator Lock 2026-06-19)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > `websocket-connection-scope-lock.md` (EXTENDED below) > `daily-universe-scope-expansion-2026-05-27.md` > defaults.
> **Scope:** PERMANENT. Every Phase. Every PR. Every future Claude/Cowork session.
> **Operator-locked:** 2026-06-19 (verbatim quotes preserved below).
> **Status:** Authorization record + contract. Sub-PR #0 of the Groww-second-feed sequence (PR-0 = this file + the plan + the two pointer lines; code lands in PR-1..PR-4 per `.claude/plans/active-plan-groww-second-feed.md`).
> **Extends (does NOT supersede):** the 2-Dhan-WebSocket lock (1 main-feed + 1 order-update) is **UNCHANGED**. This file authorizes a SEPARATE, INDEPENDENT, **default-OFF** second market-data feed to a DIFFERENT provider (Groww) under a per-feed enable/disable contract. No Dhan connection is added, removed, or modified.
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demands (preserve exactly, do not paraphrase)

**Quote 1 (2026-06-19, add Groww as a second feed, reuse the architecture):**
> "along with dhan we need to add this groww mechanism also dude okay? That's it bro but we need to follow our current architectures like wal ring buffer spill disk everything as it is dude okay? Just by adding our new groww live feed dude because in groww the main advantage is timestamp has milliseconds so that we should fetch the live ticks and need to generate our one minute especially to check whether atleast groww live feed data and groww backtesting is same or not dude"

**Quote 2 (2026-06-19, motivation + enable/disable per feed):**
> "groww as the second feed that's it dude … the main problem with dhan live feed is they clearly told us there will be a minor changes and fluctuations like something will miss in live feed websocket and it's literally irritated dude so that's why I want to check this groww live feed dude … let the code be there as a separated groww vs dhan but in the future or current always when we need to go ahead with only one or multiple feeds means then it should be allowed to enabled and disabled right dude"

**Quote 3 (2026-06-19, native Rust ONLY — brutex is reference/idea, NOT pulled):**
> "you are going to pull the code directly from brutex live package right if that's the case never … only the idea behind that will be used right dude except reference nothing else will be pulled from there right dude so that … our strict hard earned rust tick vault always O(1) will be used even for this added groww right I need the real time guarantee dude because even with groww also disconnect reconnect and not even a single tick miss also should happen dude"

**Approval (2026-06-19, AskUserQuestion):**
> "Go — start PR-0 (Groww default OFF)"

---

## §1. The rule — one paragraph

Market-data feeds become **pluggable**. There are exactly two feed providers, each
independently **enable/disable**-able by config: **Dhan** (feed #1, the existing system,
**UNCHANGED** — still ≤2 Dhan WebSocket connections per `websocket-connection-scope-lock.md`)
and **Groww** (feed #2, **NEW**, **default OFF**). Groww is implemented **natively in tickvault
Rust** — **NOTHING is pulled, vendored, or imported from the brutex repo; brutex is used ONLY as
a design/protocol REFERENCE (the idea), never as code.** Groww is added by **reusing the existing
resilience architecture AS-IS** — the same WAL frame spill, rescue ring buffer, disk spill,
DLQ, tick processor, and 1-minute aggregator that Dhan uses — never by redesigning them. Groww
therefore inherits the SAME hard-earned **O(1)** hot path and the SAME **bounded zero-tick-loss
capture guarantee** as Dhan: WAL-before-broadcast, disconnect/reconnect with subscription
preservation, ring → spill → DLQ — so **not a single tick we receive is dropped** inside the
tested chaos envelope, on Groww exactly as on Dhan. The two feeds are kept **fully separate**
(separate connection, separate parser, separate QuestDB tables); they never share a table or a
pipeline instance. Groww's tick timestamps carry
**millisecond** precision (Dhan's carry whole seconds), which is the reason for adding it. The
deliverable goal is a **Groww-live-1-minute vs Groww-backtest-1-minute parity check** (exact
match, minute-by-minute) so the operator can confirm whether Groww's live feed agrees with
Groww's own historical/backtest data — and, by extension, quantify the Dhan-live-feed
drift/missing-tick problem against an independent second source.

---

## §2. The pluggable-feed contract (LOCKED)

| Feed | Provider | Default | Connection | Reuses tickvault resilience chain | Writes to |
|---|---|---|---|---|---|
| **#1 Dhan** | Dhan | **ON** (unchanged) | ≤2 WS (main-feed + order-update) | yes (existing) | `ticks`, `candles_*`, audit tables (UNCHANGED) |
| **#2 Groww** | Groww | **OFF** | independent Groww live feed — **native Rust client** (token + TOTP); brutex is reference only | **yes — same WAL→ring→spill→DLQ→aggregator, reused unchanged** | **the SAME shared tables `ticks` / `candles_1m` / audit tables**, every row tagged `feed='groww'` (operator decision 2026-06-19 — supersedes the original `groww_*` namespace) |

> **⚠ TABLE MODEL SUPERSEDED 2026-06-19 (operator decision, AskUserQuestion "SAME tables + feed column"):** Groww does NOT write parallel `groww_*` tables. It writes into the **SAME shared tables** as Dhan (`ticks`, `candles_1m`, `prev_day_ohlcv`, audit tables), distinguished ONLY by the `feed` SYMBOL column (`'dhan'` / `'groww'`). The 3 parallel tables (`groww_live_ticks`, `groww_candles_1m`, `groww_cross_verify_1m_audit`) are RETIRED. Operator quote: *"i clearly told you same tables and extra columns alone"* + *"our entire architecture is same and precise … same codes and everything is same … especially db tables should be same … only new new feeds can be added and whenever some feeds are not needed means then just disable and enable."* Uniqueness across feeds is preserved by adding `feed` to the `ticks` DEDUP key → `(ts, security_id, segment, capture_seq, feed)` (O(1) hash, zero-alloc `&'static str` — a Dhan tick and a Groww tick for the same instrument/second are BOTH kept, never collide). Lane isolation is now by **connection + pipeline instance**, NOT by table.

| Run mode | `feeds.dhan_enabled` | `feeds.groww_enabled` | Behaviour |
|---|---|---|---|
| **Today / prod default** | `true` | `false` | Byte-identical to current Dhan-only system. Groww code present but never spawned. |
| Groww-only | `false` | `true` | Only Groww feed runs (isolated test). |
| Both (the target) | `true` | `true` | Dhan + Groww live in parallel, each in its own lane. |

**Config home:** `config/base.toml` `[feeds]` section → `FeedsConfig` in `crates/common/src/config.rs`
(lands in PR-1). Boot reads the flags ONCE (O(1)) and spawns ONLY enabled feeds.

---

## §3. What is UNCHANGED (still locked)

- **Exactly ≤2 Dhan WebSocket connections** (1 main-feed + 1 order-update). Groww does NOT add a Dhan connection. The `SubscriptionScope` enum, `effective_main_feed_pool_size`, the Dhan subscription planner, and `indices4only_scope_lock_guard.rs` are untouched.
- **Dhan pipeline, tables, boot path** — not modified. Groww default-OFF ⇒ zero behaviour change until explicitly enabled.
- **The resilience architecture is REUSED, never redesigned** — WAL frame spill (`ws_frame_spill.rs`), rescue ring (`TICK_BUFFER_CAPACITY`), disk spill (NDJSON), DLQ, tick processor, 1-minute aggregator. Groww plugs into these as a second producer; it does not change their design.
- **Composite uniqueness** `(security_id/symbol, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS on the SHARED tables, now extended with `feed` so multi-feed rows never collide (`ticks` key = `(ts, security_id, segment, capture_seq, feed)`).
- **Indicators/strategies boundary** (daily-universe lock §28) — untouched. Groww is feed + 1m candles + parity check only; it drives NO strategy and places NO order.

---

## §4. What a PR that violates this lock looks like (REJECT)

- Modifies the Dhan feed, the Dhan pipeline, the Dhan tables, `SubscriptionScope`, or `effective_main_feed_pool_size` in the name of "adding Groww".
- Redesigns / forks / duplicates the WAL/ring/spill/DLQ/aggregator instead of reusing the existing blocks.
- Re-introduces parallel `groww_*` data tables (RETIRED 2026-06-19 — Groww writes the SAME shared tables tagged `feed='groww'`).
- Writes a Groww row to a shared table WITHOUT setting `feed='groww'`, OR omits `feed` from the `ticks` DEDUP key (would let a Groww tick overwrite a Dhan tick = silent loss).
- Ships Groww **default ON** without a fresh dated operator quote (default is OFF; flipping it on by default changes prod behaviour).
- Removes the per-feed enable/disable flags or makes a feed un-disableable.
- Wires Groww into any strategy/order path (parity-check + observability ONLY in this scope).
- Adds a NEW Dhan WebSocket endpoint (that remains forbidden by `websocket-connection-scope-lock.md`).
- **Pulls, vendors, copies, or imports ANY code from the brutex repo (Python or otherwise).** brutex is a design/protocol REFERENCE ONLY — the Groww implementation is native tickvault Rust. A PR containing vendored brutex code, a `growwapi` Python dependency, or a Python sidecar process = REJECT.
- Gives Groww a weaker resilience guarantee than Dhan (e.g. skips WAL-before-broadcast, skips reconnect, or allows a tick-drop path Dhan doesn't have). Groww MUST inherit the same bounded zero-tick-loss chain.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator
must update this rule file FIRST with a dated quote, only then can the PR land.

---

> **2026-06-29 audit-coverage close-out (operator 100%-audit-coverage directive):** the Groww
> shared-master writer now ALSO emits `instrument_lifecycle_audit` TRANSITION rows tagged
> `feed='groww'` (appeared/updated/expired/reactivated/segment_moved, diffed vs yesterday's
> `feed='groww'` snapshot via the SAME pure `classify_transition` the Dhan reconciler uses),
> audit-first (§24) + best-effort/degrade-safe (`GROWW-MASTER-01`, stage `lifecycle_audit`).
> This closes the gap where Groww wrote `instrument_lifecycle`/`index_constituency` DATA rows but
> ZERO audit rows. See `groww-shared-master-error-codes.md` §0 (2026-06-29 close-out).

## §5. Honest envelope (mandatory per `operator-charter-forever.md` §F)

When any PR / commit / Telegram for this work invokes "100% guarantee", qualify it exactly:

> "100% inside the tested envelope, with ratcheted regression coverage: Groww feed reuses the
> existing zero-tick-loss chain (ring → NDJSON spill → DLQ — `TICK_BUFFER_CAPACITY`, ratcheted
> by `zero_tick_loss_alert_guard.rs`); Groww default OFF ⇒ Dhan-only behaviour is byte-identical;
> per-feed enable/disable is an O(1) boot-time config read + per-feed pipeline isolation; the
> live-vs-backtest parity check is an EXACT 1-minute OHLCV compare that NAMES every mismatched
> minute (never a false 'all match' on an empty/partial set, per audit Rule 11). Groww's
> millisecond timestamps improve minute-boundary accuracy. Two distinct senses of 'no tick
> missed', kept honest: (a) CAPTURE — every tick we RECEIVE on the Groww socket is durably
> recorded inside the tested chaos envelope (WAL-before-broadcast + reconnect + ring→spill→DLQ),
> identical to Dhan — 'not a single tick missed' holds here; (b) UPSTREAM — we do NOT claim
> Groww's exchange feed never drops a tick before it reaches us — measuring that drift against
> Dhan is the whole point, not asserting its absence."

Stronger phrasing ("Groww never misses a tick", "live == backtest always") without the
envelope qualifier = REJECT IN REVIEW.

---

## §6. Auto-driver / Insta-reel explanation

> Sir, imagine your juice shop has ONE price board from supplier Dhan. Dhan himself admits his
> board sometimes blinks and skips a price. Annoying. So we add a SECOND price board from
> supplier Groww — and Groww's board even shows the *milliseconds*, sharper than Dhan's
> seconds-only board. Both boards hang side by side, each with its own ON/OFF switch — show only
> Dhan, only Groww, or both. Groww's switch starts OFF so nothing about your existing shop
> changes until you flip it. And the clever bit: we collect Groww's live prices for one minute,
> then check that minute against Groww's OWN official record book. If they match, Groww's live
> board is honest — and now you have a trustworthy second board to catch when Dhan's blinks.

---

## §7. Trigger (auto-loaded paths)

Always loaded. Activates on any session that:
- Edits `crates/common/src/config.rs` (`FeedsConfig` / `[feeds]` flags)
- Edits `config/base.toml` `[feeds]` section
- Adds/edits any file under a `crates/*/src/**/groww*` path or containing `groww`, `GrowwFeed`, `groww_live_ticks`, `groww_candles_1m`, `FeedsConfig`, `feeds.groww_enabled`, `feeds.dhan_enabled`
- Adds any new `wss://` Groww feed URL constant
- Edits `.claude/plans/active-plan-groww-second-feed.md`

---

# §32 — Python-SDK live-feed validation path (operator authorization 2026-06-19, evening)

> **⚠ PARTIALLY SUPERSEDED 2026-06-26 by §33 below:** §32 was authorized for the live-vs-backtest VALIDATION goal ("is Groww-live == Groww-backtest?"). Per the operator's 2026-06-26 LIVE-FEED-ONLY directive (§33), Groww historical/backtest pulls are now FORBIDDEN, so the §32.1/§32.2 backtest-comparison purpose (the `get_historical_candles` capture, "parity checker" role of the Rust consumer) is HALTED. The §32 Python LIVE-feed front-end itself (default-OFF, local-first, capture-at-receipt → the existing Rust ring→spill→DLQ→aggregator) remains valid for LIVE ticks only — it just no longer feeds a backtest comparator. §32 is retained for audit; read §33 first.

> **Authority:** this section AMENDS §4 of this same lock. The §4 blanket
> "a `growwapi` Python dependency, or a Python sidecar process = REJECT" is
> NARROWED below: a Python `growwapi` process is **AUTHORIZED — but ONLY** under
> the validation contract in this section, default-OFF, isolated, never in the
> Dhan path. All OTHER §4 REJECT conditions stand unchanged.

## §32.0 The verbatim operator demands (preserve exactly)

**Quote 7 (2026-06-19 evening — use the Python SDK because Groww hides the WS):**
> "But see when they don't reveal the websocket its best to use their python sdk
> live feed right dude"

**Quote 8 (2026-06-19 evening — verify the WS URL from inside the SDK / GitHub first):**
> "how about fetching the websocket url from inside groww python sdk dude?
> especially does groww display any groww sdk github codes dude meanwhile can you
> check this also dude?"

**Approval (2026-06-19, AskUserQuestion):** Groww approach = "go ahead" with the
Python-SDK live feed, after confirming the WS URL is not cleanly extractable for a
native client today.

## §32.1 Why this amendment (the verified facts — see `docs/groww-ref/02-verified-endpoints.md`)

Research (cited, 2026-06-19) established:
- Groww publishes **no official SDK source repo**; the live feed is **NATS-over-
  WebSocket + Protobuf** at `wss://socket-api.groww.in`, with the `.proto` schema +
  NATS subject grammar **embedded in the PyPI wheel** (not citable open source).
- `async-nats` (the earlier-approved dep) is **TCP-only** and does NOT speak
  NATS-over-WebSocket — so a native Rust client is **weeks** of work (NATS-over-WS
  layer + nkey/JWT + extracted `.proto`), not days.
- The auth REST URL `POST /v1/token/api/access` is **confirmed** and already
  shipped natively in `crate::feed::groww::auth` (PR-2) — UNCHANGED.

Therefore, for the operator's actual immediate goal — **"is Groww-live == Groww-
backtest?"** (a validation question, §0 Quote 1) — the Python SDK is the honest
fast path; it already contains the NATS-over-WS + nkey + protobuf decode.

## §32.2 The authorized validation contract (LOCKED)

| Constraint | Locked value |
|---|---|
| Runtime | **Local validation first** (operator's Mac; Dhan OFF, Groww ON). NO AWS/prod deploy of Python without a further dated quote. |
| Process model | A **separate `growwapi` Python process** running `GrowwFeed`, default-OFF behind `feeds.groww_enabled`. NEVER imported into the Rust binary; NEVER on the Dhan path. |
| Capture-at-receipt | The Python callback MUST append each received tick to a **durable append-only file the instant it arrives** (before any IPC), preserving the zero-tick-loss PRINCIPLE one hop downstream of the socket. |
| Bridge to Rust | Python → durable file/IPC → the **existing** Rust ring→spill→DLQ→aggregator (reused, not redesigned). Rust is the consumer + 1m sealer + parity checker. |
| Tables | **The SAME shared tables** (`ticks`, `candles_1m`, audit tables) tagged `feed='groww'` (operator decision 2026-06-19). The original `groww_*`-namespaced-only rule is RETIRED. |
| Native Rust | **Kept as the production option, not burned.** When the `.proto` is extracted from the wheel + a NATS-over-WS path exists, the Python front-end is swapped for native Rust; everything downstream stays. |

## §32.3 Honest envelope (mandatory per §5 / operator-charter §F)

> "100% inside the tested envelope: the Groww validation path uses the official
> `growwapi` Python SDK (default-OFF, isolated, `groww_*` tables only); the Dhan
> path is byte-identical. Zero-tick-loss is preserved as **capture-at-receipt**
> (durable append-only file in the Python callback) → the existing Rust
> ring→spill→DLQ chain — an HONEST step down from at-socket capture: a Python
> crash in the microsecond between socket-recv and file-append is a tiny bounded
> loss window, far better than none. O(1) is NOT claimed on the Python hop (GIL/GC
> jitter); it IS preserved on the Rust consumer/aggregator. Native Rust (at-socket
> O(1) capture) remains the production option once the wheel's `.proto` is
> extracted."

## §32.4 What still REJECTS (unchanged from §4, except the narrow Python carve-out above)

- Python on the Dhan path, or writing to Dhan tables → REJECT.
- Python in AWS/prod WITHOUT a further dated operator quote (this carve-out is
  local-validation-first) → REJECT.
- Importing `growwapi` into the Rust binary (it stays a separate process) → REJECT.
- A weaker-than-Dhan resilience guarantee on the Groww path beyond the honestly
  documented at-receipt window → REJECT.
- Adding a Dhan WebSocket, or any change to the 2-Dhan-WS lock → REJECT (unchanged).

---

# §33 — Groww LIVE-FEED-ONLY (operator lock 2026-06-26)

> **Authority:** this section SUPERSEDES the §0 Quote-1 live-vs-backtest deliverable and
> HALTS the §32 Python-sidecar backtest-comparison purpose (the `get_historical_candles`
> capture + the Rust consumer's "parity checker" role). It does NOT lift the §32 Python
> LIVE-feed front-end for live ticks, and it does NOT touch Dhan. All other locks in this
> file (the 2-Dhan-WS lock §3, the shared-table model, default-OFF, per-feed enable/disable,
> native-Rust-only/no-brutex-code, the §4/§32.4 REJECT conditions) stand unchanged.

## §33.0 The verbatim operator demands (preserve exactly, do not paraphrase)

**Quote 9 (2026-06-26 — no Groww historical pulls):**
> "as of now we should never ever pull any historical data for groww dude okay?"

**Quote 10 (2026-06-26 — Groww is purely live, no backtest focus):**
> "nowhere focus on the backtest here dude because this is purely for live alone dude okay?"

## §33.1 The rule (one line)

**Groww is LIVE-FEED-ONLY. Pulling ANY Groww historical / backtest / intraday-candle data is
FORBIDDEN until the operator re-authorizes with a fresh dated quote. The live-vs-backtest
1-minute parity track is CANCELLED.**

## §33.2 What this HALTS / CANCELS

| Cancelled | Detail |
|---|---|
| §0 Quote-1 live↔backtest goal | The "check whether at least groww live feed data and groww backtesting is same or not" deliverable is HALTED. Groww has no backtest source to compare against. |
| §32.1/§32.2 backtest-comparison purpose | The Python sidecar's role is reduced to **live tick capture only**. No `get_historical_candles` / `growwapi` historical call. The Rust consumer is a live 1m/21-TF sealer, NOT a parity checker, for Groww. |
| Plan parity track | `.claude/plans/active-plan-groww-live-backtest-parity.md` SP5 (parity audit table) / SP6 / SP6a / SP6b (`BacktestSource` trait + Dhan/Groww fetchers) / SP7 (parity orchestrator) are CANCELLED. The abandoned SP6a branch (`claude/parity-sp6-backtest-source`) was discarded. |
| Any Groww `BacktestSource` impl | `crates/core/src/feed/groww/backtest.rs`, `scripts/groww-sidecar/groww_backtest_fetch.py`, and any Groww historical fetcher MUST NOT be created. |

## §33.3 What is UNCHANGED

- **Dhan is completely unaffected.** Dhan's own historical cross-verify (`crates/app/src/cross_verify_1m_boot.rs`, `cross_verify_1m_audit`) and the boot-time prev-day OHLCV fetch (Dhan REST, fail-soft, NOT a boot-blocker — `prev-day-ohlcv-error-codes.md`) stay exactly as-is. This directive is Groww-only.
- **The Groww LIVE feed continues.** Groww live ticks → WAL → ring → spill → DLQ → 1m/21-TF candles tagged `feed='groww'`, default-OFF behind `feeds.groww_enabled`, per the §3 shared-table model and the §32 live-capture front-end. Zero-tick-loss capture-at-receipt is preserved.
- The 2-Dhan-WebSocket lock, the native-Rust-only rule (brutex = reference only), and all §4/§32.4 REJECT conditions remain in force.

## §33.4 What a PR that violates this lock looks like (REJECT)

- Creates or wires ANY Groww historical / backtest / intraday-candle fetcher (`BacktestSource` for Groww, `groww_backtest_fetch.py`, a `growwapi` `get_historical_candles` call, etc.).
- Re-activates SP5/SP6/SP7 of the parity plan for Groww, or wires a Groww parity orchestrator.
- Touches the Dhan historical cross-verify or Dhan prev-day OHLCV in the name of this directive (it is Groww-only — leave Dhan alone).
- Disables the Groww LIVE feed in the name of "no backtest" (live is explicitly KEPT).

Any such PR MUST be rejected in review even if the operator approves verbally — the operator
must update this §33 FIRST with a fresh dated quote re-authorizing Groww historical, only then
can the PR land.

## §33.5 Honest envelope (mandatory per §5 / operator-charter §F)

> "Groww is live-feed-only as of 2026-06-26. We capture and seal Groww LIVE ticks (1m + 21 TFs,
> tagged `feed='groww'`) with the same bounded zero-tick-loss chain as Dhan — but we make NO
> claim about Groww live-vs-backtest agreement, because we deliberately fetch NO Groww backtest
> data. The Dhan-side historical cross-verify is the only OHLCV parity signal in the system and
> is untouched."

## §33.6 Auto-driver / Insta-reel explanation

> Sir, earlier we wanted to hang Groww's LIVE price board next to Groww's OLD record book and
> check they match. The operator now says: forget the old record book entirely — for Groww we
> only care about the LIVE board, right now, this second. So we keep the live board running, we
> stop ordering the old record book completely, and we throw away the half-built "fetch the old
> book" machine. Nothing about supplier Dhan changes — Dhan keeps both its boards.

## §33.7 Trigger (auto-loaded)

Always loaded (this file is under `.claude/rules/project/`). Reinforced on any session that
edits Groww feed code, the parity plan, or proposes any Groww historical/backtest fetch.

---

# §34 — Groww multi-connection AUTO-SCALE (operator authorization 2026-07-03)

> **Authority:** this section EXTENDS the Groww feed scope (§1/§2). The **Dhan
> 2-WS lock is UNCHANGED** — zero new Dhan connections, no Dhan code touched.
> The §33 LIVE-FEED-ONLY lock is UNCHANGED — the scaled connections carry LIVE
> ticks only, no historical pulls. Companion plan:
> `.claude/plans/active-plan-groww-autoscale.md`. Companion error codes:
> `.claude/rules/project/groww-scale-error-codes.md` (GROWW-SCALE-01..04).

## §34.0 The verbatim operator demands (preserve exactly, do not paraphrase)

**Quote 11 (2026-07-03 evening — multi-connection auto-scale):**
> "I planned to establish multiple connections… 100 parallel connections to
> test 100k instruments… every ten connection should hold 1k instruments per
> connection… incremental dynamic scalable connections of max 100 connections
> starting 10… when auto scale up happens if there is any failure how that can
> be auto corrected"

**Execution confirmation (2026-07-03 evening):** Mac-first execution confirmed
— the first live scale test runs on the operator's Mac (Dhan disabled,
groww-only profile, local QuestDB), NOT on the AWS prod box.

## §34.1 The rule — one paragraph

The GROWW feed (only) is authorized to open up to `target_connections` Groww
NATS-over-WS connections, each subscribing ≤ 1,000 instruments (the documented
Groww per-session cap), driven by a **gated auto-scale ladder** (day-1 rungs
`[1, 2, 5, 10]`; PROBING → HOLDING → ADVANCING with rollback-to-last-healthy +
exponential hold on any rung failure, and global cooldown + halve on
fleet-wide failure). Sharding is deterministic RANGE-BASED (contiguous
`(exchange, segment, security_id)` ranges of ≤ `instruments_per_conn`), with a
fail-closed disjointness + coverage assertion. Every shard reuses the existing
capture-at-receipt → ring → spill → DLQ → shared-aggregator chain unchanged;
capture_seq stays globally unique via the ONE shared process-wide atomic. The
default remains `feeds.groww.scale.enabled = false` — single connection,
byte-identical to today.

## §34.2 Capacity tiers (capacity-math, 2026-07-03 — mechanical bounds)

| Tier | Conns / SIDs | Status | Gate to enter |
|---|---|---|---|
| **A** | ≤ 10 conns / ≤ 10k SIDs | **Approved for Monday** (Mac-first, then r8g.large 16 GiB within measured envelope) | this §34 authorization |
| **B** | 11–25 conns / ≤ 25k SIDs | GATED | live RAM + CPU + disk measurement at the Tier A ceiling, recorded with a dated note |
| **C** | 26–100 conns / ≤ 100k SIDs | REQUIRES INFRA SIGN-OFF | instance re-size per §7 Mechanical Rule 1 of the daily-universe lock + QuestDB write-pressure re-measure + EBS re-size, each with dated operator approval |

A PR raising `target_connections` beyond the current tier without the dated
measurement + sign-off = REJECT.

## §34.3 Honest envelope (mandatory §F wording)

> "100% inside the tested envelope, with ratcheted regression coverage:
> auto-correction covers the 16 failure classes in the design taxonomy —
> detect ≤30s (per-conn stall watchdog), rollback to last-healthy rung, expo
> hold 10m→4h, retry forever in market hours; capture-at-receipt durability
> per shard → the existing ring→spill→DLQ. NOT claimed: Groww's server-side
> per-account connection AND subscription caps are UNKNOWN until live-probed —
> the ladder (and the 2×600 cap probe) DISCOVERS them and holds below; that is
> the design, not a defect. The Python hop is not O(1) (GIL jitter, existing
> §32 envelope); upstream ticks Groww never delivers are invisible (existing
> §5 CAPTURE vs UPSTREAM split)."

## §34.4 What a PR that violates this section looks like (REJECT)

- Ships `feeds.groww.scale.enabled = true` as the DEFAULT without a fresh
  dated operator quote.
- Exceeds the current tier's connection cap without the §34.2 gate evidence.
- Adds a Dhan connection, or touches the Dhan pipeline, in the name of
  scaling Groww.
- Bypasses the ladder (jumping straight to N conns with no gates/rollback).
- Weakens the shard disjointness/coverage assertion or removes the shared
  capture_seq atomic (silent cross-shard dedup collision risk).
- Gives a scaled shard a weaker resilience chain than the single-conn path.

## §34.5 Auto-driver / Insta-reel explanation

> Sir, the juice shop's Groww price board gets helpers. Today ONE boy watches
> 1,000 fruits. The new plan: start with a few boys, and only when every boy
> has been reading prices smoothly for 15 minutes do we add more — 1, then 2,
> then 5, then 10 boys (each watching his OWN 1,000 fruits, no two boys
> watching the same fruit). If a new boy faints, we send the newest boys home
> and go back to the last team that worked, wait, and try again — all
> automatically, no phone call needed. If ALL boys collapse at once, that's
> the market gate telling us to slow down: we wait 5 minutes and come back
> with half the team. The dream is 100 boys / 100,000 fruits — but only after
> we PROVE the shop floor (the Mac first, then the server) can hold them.

## §34.6 Trigger (auto-loaded)

Always loaded. Reinforced on any session editing `crates/core/src/feed/groww/shard_cutter.rs`,
`crates/app/src/groww_scale_ladder.rs`, `crates/app/src/groww_sidecar_supervisor.rs`,
`[feeds.groww.scale]` config, or any file containing `GrowwScaleConfig`, `cut_shards`,
`GROWW_SHARD_SPEC`, or `groww_scale_audit`.
