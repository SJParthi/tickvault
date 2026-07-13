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

> **⚠ 2026-07-13 (extended in Phase B):** the "#1 Dhan … Default **ON** (unchanged)" row and the run-mode table's "Today / prod default: dhan=true" are SUPERSEDED — the Dhan live WS is RETIRED (operator directive 2026-07-13, verbatim + full contract in `websocket-connection-scope-lock.md`'s "2026-07-13 Amendment" §-section). **Groww is now the SOLE live market-data feed**; prod = `dhan_enabled = false` + `groww_enabled = true`; Dhan is REST-only (spot-1m / chain / historical) plus the functional-dormant order-update WS inside `dhan_rest_stack`. The per-feed enable/disable CONTRACT itself is UNCHANGED and load-bearing — it is the pluggable seam feed #3 (GDF, `gdf-third-feed-scope-2026-07-13.md`) plugs into; the "Both (the target)" run mode row now reads Groww + GDF prospectively, not Groww + Dhan. §5's and §34.3's honest-envelope references to measuring/comparing against the Dhan live feed (e.g. "quantify the Dhan-live-feed drift... against an independent second source", "measuring that drift against Dhan is the whole point") are OVERTAKEN BY EVENTS — the measurement CONCLUDED (it is what retired the Dhan feed; evidence table in the scope-lock amendment §E); until GDF is live, Groww runs single-source and the §37/§38 REST comparisons are the parity signals.

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

> **⚠ 2026-07-13:** the "by extension, quantify the Dhan-live-feed drift" clause below is CONCLUDED, not ongoing — the comparison retired the Dhan live WS (see the §2 banner + `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §E for the quantified record). The CAPTURE-vs-UPSTREAM split and every Groww-side guarantee below stand unchanged.

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

> **⚠ PARTIALLY SUPERSEDED 2026-07-12 by §37 below** — the operator re-authorized ONE narrow backtest-comparison mechanism: read-only consumption of BruteX-produced backtest CSVs from OUR OWN S3 bucket. §33's ban on TickVault-side Groww historical/API fetching stands unchanged; see §37.
>
> **⚠ PARTIALLY SUPERSEDED 2026-07-13 by §38 below** — the operator authorized the Groww PER-MINUTE SCHEDULED REST pipeline (spot 1m + option chain + bounded per-contract 1m — the Dhan §8-pattern mirror). §33's ban on BULK Groww historical / backtest sweeps otherwise stands unchanged; see §38.
>
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

> **2026-07-12 note:** the "pulling from Groww" ban above stands UNCHANGED. §37 (dated quote) re-authorizes ONLY the read-only consumption of BruteX-PRODUCED backtest CSVs from our own S3 bucket — a comparison mechanism, not a Groww fetch.

> **2026-07-13 note:** §38 (dated quotes) re-authorizes ONE narrow Groww-fetch class: PER-MINUTE SCHEDULED in-session pulls (spot 1m + option chain + bounded per-contract 1m, day-granular candle windows only). Everything else in this rule — bulk historical sweeps, past-day backfills beyond §38's one-minute/15:31-sweep patterns, any backtest-fetch track — stands FORBIDDEN.

## §33.2 What this HALTS / CANCELS

| Cancelled | Detail |
|---|---|
| §0 Quote-1 live↔backtest goal | The "check whether at least groww live feed data and groww backtesting is same or not" deliverable is HALTED. Groww has no backtest source to compare against. |
| §32.1/§32.2 backtest-comparison purpose | The Python sidecar's role is reduced to **live tick capture only**. No `get_historical_candles` / `growwapi` historical call. The Rust consumer is a live 1m/21-TF sealer, NOT a parity checker, for Groww. |
| Plan parity track | `.claude/plans/active-plan-groww-live-backtest-parity.md` SP5 (parity audit table) / SP6 / SP6a / SP6b (`BacktestSource` trait + Dhan/Groww fetchers) / SP7 (parity orchestrator) are CANCELLED. The abandoned SP6a branch (`claude/parity-sp6-backtest-source`) was discarded. |
| Any Groww `BacktestSource` impl | `crates/core/src/feed/groww/backtest.rs`, `scripts/groww-sidecar/groww_backtest_fetch.py`, and any Groww historical fetcher MUST NOT be created. |

> **2026-07-12 note:** the SP5–SP7 cancellation and the `BacktestSource` / `backtest`-module naming ban stand. The §37 consumer is a DIFFERENT mechanism (S3-CSV read of BruteX artifacts) named `brutex_crossverify` — never `backtest`/`BacktestSource`.

> **2026-07-13 note:** the SP5–SP7 cancellation and the `BacktestSource`/`backtest` naming ban STILL stand. The §38 pipeline is a `GET /v1/historical/candles` consumer, but only in the per-minute scheduled shape (day-granular window, target-minute filter) — it is NOT the cancelled backtest-comparison fetcher, is never named `backtest`/`BacktestSource`, and performs no bulk sweeps.

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

> **2026-07-12 note:** the required §33 update happened — §37 below carries the fresh dated operator quote authorizing the narrow BruteX-S3-CSV consumption path (and ONLY that path). Every REJECT bullet above still applies to any TickVault-side Groww API/historical fetch.

> **2026-07-13 note:** a SECOND required §33 update happened — §38 below carries the fresh dated operator quotes authorizing the per-minute SCHEDULED Groww REST pipeline (spot 1m + option chain + bounded per-contract 1m, and ONLY that shape). The REJECT bullets above still apply to any BULK Groww historical/backtest fetch outside the §38 grant.

## §33.5 Honest envelope (mandatory per §5 / operator-charter §F)

> "Groww is live-feed-only as of 2026-06-26. We capture and seal Groww LIVE ticks (1m + 21 TFs,
> tagged `feed='groww'`) with the same bounded zero-tick-loss chain as Dhan — but we make NO
> claim about Groww live-vs-backtest agreement, because we deliberately fetch NO Groww backtest
> data. The Dhan-side historical cross-verify is the only OHLCV parity signal in the system and
> is untouched."

> **2026-07-12 note:** TickVault still fetches NO Groww backtest data from Groww; since 2026-07-12 it CONSUMES BruteX-produced backtest CSVs from our own S3 per §37, so the Dhan cross-verify is no longer the ONLY OHLCV parity signal — the §37 BruteX↔TickVault daily comparison is the second one.

> **2026-07-13 note:** the "we deliberately fetch NO Groww backtest data" claim narrows: since 2026-07-13 TickVault fetches Groww's OFFICIAL per-minute candles + option chain IN-SESSION per §38 (scheduled pulls only — still no bulk backtest fetch). The §38 REST tables become the fill-model parity source alongside the §37 comparison.

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

---

# §35 — Native-Rust SHADOW client authorized (operator "go" 2026-07-04 — PR-R1 of the parity migration)

> **Authority:** this section EXTENDS §32 ("native Rust remains the production
> option") and honors §33 (LIVE-FEED-ONLY — the shadow client makes ZERO
> historical calls). The Python sidecar path (§32) is UNCHANGED and remains the
> production capture chain. Companion plan:
> `.claude/plans/active-plan-groww-native-r1.md`. Companion error codes:
> `.claude/rules/project/groww-native-rust-error-codes.md`
> (GROWW-NATIVE-01..04).

## §35.0 The operator authorization (2026-07-04, this session)

Operator approved **"go"** for PR-R1: the native-Rust Groww client, built from
the growwapi-1.5.0 wheel protocol extraction (the sanctioned §32 reference;
brutex code banned), runs **DEFAULT-OFF in SHADOW** alongside the Python
sidecar — same watch set, its OWN NDJSON capture
(`data/groww/rust-live-ticks.ndjson`, same line schema as the sidecar's) for
the future exact per-tick parity comparer (PR-R2). NO shared-table writes, NO
strategy/order wiring, NO sidecar changes. The approval INCLUDED new
dependencies (`async-nats` + `nkeys` + `prost`).

**Dependency outcome (honest record, SMALLER than approved):** implementation
found the repo already carries merged, tested, zero-dep native equivalents —
`feed/groww/nats.rs` (framing), `feed/groww/nkey.rs` (nkey codec),
`feed/groww/proto.rs` (protobuf decode), `feed/groww/subjects.rs` (subject
grammar) — so NONE of the 3 approved crates was added. The ONLY dependency
change is promoting **`aws-lc-rs` 1.17.0** (already resolved in Cargo.lock via
rustls) to a direct workspace dep for ed25519 session-keypair generation +
NATS nonce signing. Rationale: (a) zero new supply-chain roots; (b) full
control of the `CONNECT` frame fields to mimic nats-py (the #1 live-probe
risk — `async-nats` does not allow that); (c) the hand-written decoders are
exactly the "prefer hand-written structs" guidance. Adding any of the 3
approved crates later requires no new approval (already granted) but MUST
update this section with the reason.

## §35.1 The contract (LOCKED)

| Aspect | Locked value |
|---|---|
| Config gate | `[feeds] groww_native_shadow = false` (default OFF; `#[serde(default)]`) — flipping the DEFAULT needs a fresh dated quote |
| Universe | READ the Rust-built watch file (`data/groww/groww-watch-<date>.json`) — the sidecar's exact source; NEVER a self-built universe |
| Auth | Access token = SSM READ-ONLY (token-minter lock 2026-07-02 UNTOUCHED). Per-session socket-token mint = live-feed-AUTH KEEP class, recorded in `no-rest-except-live-feed-2026-06-27.md` §3 |
| Output | `data/groww/rust-live-ticks.ndjson` ONLY (GrowwTickLine schema, IST-midnight rotation mirroring the sidecar). NO `ticks`/`candles_*`/audit-table writes in R1 |
| Recovery | Bounded expo reconnect (5s→60s); auth-class failure → fresh keypair + fresh socket-token mint; SSM re-read ≥60s pacing; supervised respawn (WS-GAP-05 pattern) |
| Historical | ZERO (§33 stands — live feed only) |
| Migration | PR-R2 = parity comparer; PR-R3 = cutover ONLY after clean parity days + a fresh dated operator quote (sidecar demoted to kill-switch fallback first) |

## §35.2 What a PR that violates this looks like (REJECT)

- Ships `groww_native_shadow = true` as the DEFAULT without a fresh dated quote.
- Makes the shadow client write ANY shared table, or wires it into strategy/orders.
- Builds its own universe instead of reading the watch file.
- Mints or caches-past-auth-failure the ACCESS token (token-minter lock).
- Adds a Groww historical/backtest call (§33).
- Removes the Python sidecar or changes its behaviour in the name of the shadow client (that is PR-R3, gated on parity evidence + a dated quote).

## §35.3 Trigger (auto-loaded)

Always loaded. Reinforced on any session editing
`crates/core/src/feed/groww/native/`, `crates/app/src/groww_native_shadow.rs`,
the `[feeds] groww_native_shadow` key, or any file containing
`groww_native_shadow` / `rust-live-ticks.ndjson` / `GROWW_SOCKET_TOKEN_URL`.


---

# §36 — FUTIDX-4 in the Groww watch set (operator authorization 2026-07-08)

> **Cross-link:** the Dhan-side grant is `daily-universe-scope-expansion-2026-05-27.md` §36
> (same section number deliberately, cross-linked both ways).

## §36.0 The verbatim operator demand (preserve exactly, do not paraphrase)

**Quote (2026-07-08):**
> "for both dhan and groww we need to add futures and those also should be subscribed along with this, especially only for nifty banknifty and sensex nifty midcap."

**Quote (2026-07-10, relayed verbatim via the coordinator session — §36.7 all-months expansion):**
> "instead of only one current month futures contracts just take all the futures of these
> indices — I mean take all available applicable months futures."

## §36.1 The grant

The Groww watch set gains one entry per (underlying, monthly expiry `>= today`) —
`{exchange: NSE×3 / BSE×1, segment: "FNO", exchange_token: <numeric>, kind: ltp}` — ALL
vendor-listed monthly serials of the SAME 4 underlyings the Dhan side subscribes (§36.7 of
the daily-universe lock, 2026-07-10; typically ~12, envelope ≤6 serials/underlying;
NIFTY, BANKNIFTY, MIDCPNIFTY on NSE; SENSEX on BSE), ALL cap-priority in the live-subscribe
set. Expiry resolution comes ONLY from the static Groww master CSV
(`GROWW_INSTRUMENT_CSV_URL` — the `no-rest-except-live-feed-2026-06-27.md` §3 KEEP class);
§33 LIVE-FEED-ONLY is untouched (NO Groww historical call ever). ONE Groww connection unchanged
(`groww-scale-aws-lockout-2026-07-06.md` untouched). Rows land in the SAME shared tables tagged
`feed='groww'` with feed-in-key DEDUP. Both feeds select the expiries via ONE shared pure function
(`crates/core/src/instrument/index_futures.rs::select_index_future_expiries` — ALL monthly
expiries >= today, nearest first, NEVER rolls); missing FUT rows degrade per-underlying —
or per-(underlying, month) for month-scoped reasons (FUTIDX-01, feed=groww) —
never fail the watch build; cross-feed expiry-SET divergence in any comparable month pages
FUTIDX-02 (a far-suffix depth difference is an info-level note, never a page).

> **2026-07-13 note:** the "(NO Groww historical call ever)" parenthetical is narrowed by
> §38 — the per-minute SCHEDULED REST pulls of §38 are the sole authorized exception. §36's
> own machinery is unchanged: expiry resolution still comes ONLY from the static master CSV.

## §36.2 Honest notes

Groww LTP carries only `{ltp, tsInMillis}` — no volume/OI for futures (`cum_volume` stays 0). A
Groww future's `security_id` is the Groww exchange_token — a DIFFERENT id space from Dhan's
FUTIDX SID for the same contract (cross-feed comparison maps by contract = underlying+expiry,
never by id). Live Groww FNO `subscribe_ltp` delivery is UNVERIFIED-LIVE — the first enabled
session is the probe; the feed-stall watchdog + FUTIDX-01 payloads make silence loud. Live
Groww FNO LTP delivery for months 2..N is UNVERIFIED-LIVE (§36.7 — same class as the
original 4). Under §36.7 the live set is ~779 of the 1000 cap (~767 spots + ~12 futures).

## §36.3 REJECT list (Groww side)

OPTIDX/CE/PE entries in the watch set; futures of a 5th underlying; any non-monthly-serial
entry; >`MAX_MONTHLY_EXPIRIES_PER_UNDERLYING` (6) serials per underlying (fail-closed, never
truncated); any new Groww connection; any Groww historical fetch (§33); resolving expiry from
anything but the static master CSV. (The pre-2026-07-10 single-expiry-per-underlying ban is
REMOVED by this dated §36.7 edit.) This file must be edited FIRST with a fresh dated quote
for any of the above.

> **2026-07-13 note:** "any Groww historical fetch (§33)" reads: any Groww historical fetch
> OUTSIDE the §38 per-minute scheduled grant. The §36 watch-set/expiry machinery itself
> still makes no Groww REST call.

---

# §37 — BruteX backtest cross-verification: S3-CSV read-only consumption (operator authorization 2026-07-12)

> **Authority:** this section PARTIALLY SUPERSEDES §33 (see the dated banner on §33's header
> and the dated notes on §33.1/§33.2/§33.4/§33.5). It re-authorizes exactly ONE narrow
> backtest-comparison mechanism — read-only consumption of BruteX-PRODUCED backtest CSVs from
> OUR OWN S3 bucket — and NOTHING else. §33's ban on TickVault-side Groww historical/API
> fetching stands unchanged. The 2-Dhan-WS lock (§3), the shared-table model, default-OFF
> per-feed toggles, native-Rust-only/no-brutex-code, and the §4/§32.4/§36.3 REJECT conditions
> all stand unchanged.
> **Cross-ref:** `no-rest-except-live-feed-2026-06-27.md` §3 (the 2026-07-12 KEEP row for the
> S3 read); companion plan `.claude/plans/active-plan-brutex-crossverify.md`.

## §37.0 The verbatim operator quotes (preserve exactly, do not paraphrase)

**Quote 1 (2026-07-12, the directive):**
> "Build the daily cross-verification CHECK on the TickVault side, full protocol, read-only on the live capture. Every day after close, compare BruteX's backtest 1-minute against TickVault's own live 1-minute and flag divergence. INPUT (from BruteX): CSVs at s3://<CROSSVERIFY_BUCKET>/crossverify/groww/<YYYY-MM-DD>/<segment>/<symbol>.csv, columns symbol,timestamp_ist,open,high,low,close,volume,oi (spot indices+stocks + NIFTY/BANKNIFTY/SENSEX futures; feed=groww). TICKVAULT SIDE: read your own candles_1m where feed='groww' for the same day. Join BruteX symbol ↔ your security_id via instrument_lifecycle... COMPARE per (symbol, minute) in BOTH: open/high/low/close within a CONFIGURABLE tolerance (default: equal to 2 decimals)... Minutes in only one side → flag missing_backtest / missing_live. REPORT (daily): per symbol — matched · diverged (minute + both OHLC + diff) · missing_backtest · missing_live; + a run summary that QUANTIFIES the typical fluctuation (so we SEE the normal noise) and FLAGS anything beyond tolerance. Surface on the TickVault dashboard + a summary. HONESTY: neither side is ground truth — you have a known tick-conservation residual, Groww historical has vendor-fill fluctuation — separate 'expected fluctuation' from 'real divergence,' never assume live is correct."

**Quote 2 (2026-07-12, webpage):**
> "view report wise and even webpage view dashboard everywhere it should be easily accessible"

**Quote 3 (2026-07-12, OHLC-only):**
> "as of now except volume only OHLC alone should be checked... since volume is directly fetched in live websocket feed"

## §37.1 The grant — one paragraph

A post-close (15:50 IST) read-only comparer reads BruteX-produced backtest 1m CSVs from
`s3://tv-prod-cold/crossverify/groww/<YYYY-MM-DD>/<segment>/<symbol>.csv` (our own bucket —
`tv-${environment}-cold` in terraform; the prod instance role already has read access on the
whole bucket, zero IAM change) and compares them against TickVault's own `candles_1m`
(`feed='groww'`). TickVault makes ZERO Groww API historical calls — §33 stands for the Groww
API surface. S3 GetObject/ListObjectsV2 on our own bucket is NOT a market-data REST pull
under `no-rest-except-live-feed-2026-06-27.md` (internal artifact transfer, same class as the
S3 cold-archive KEEP row — see the 2026-07-12 KEEP row added there). The new workspace dep
`aws-sdk-s3` (exact pin, same feature set as the sibling aws-sdk-* pins) is authorized by
Quote 1 as the read mechanism (flagged in the PR for the operator's visibility per the
CLAUDE.md new-dep approval rule).

> **2026-07-13 note:** "TickVault makes ZERO Groww API historical calls" narrows: zero
> BULK/backtest Groww API calls remains true, but since 2026-07-13 the §38 per-minute
> SCHEDULED pulls (spot 1m + option chain + bounded per-contract 1m) are the sole exception.
> §33 stands for the Groww API surface outside the §38 grant.

## §37.2 The comparison contract (LOCKED)

- Per (symbol, minute): **O/H/L/C ONLY** (Quote 3), via **integer-paise equality** with a
  configurable `tolerance_paise` (default `0` = exact at 2 decimals — the honest form of
  Quote 1's "equal to 2 decimals"; never a float `|diff| <= epsilon` compare).
- **Volume** is STORED both sides in audit rows but NOT classified. Flipping volume checking
  on = a config toggle `compare_volume`, which is a HARD NO-OP for `feed='groww'` (whose live
  candles carry `volume=0` always — sidecar is price-only) — refused loudly, never a silent
  100%-divergence storm.
- **Index presence** = live bar existence (a live bar implies ≥1 tick — the aggregator opens
  `tick_count` at 1, so `tick_count>0` is equivalent to bar existence; documented as a
  bar-existence check, no extra rigor claimed).
- Minutes present in only one side → `missing_backtest` / `missing_live` categories —
  REPORTED, never counted as "real divergence" (our side only bars minutes that actually
  ticked; the vendor fills).
- Backtest bars outside `[09:15, 15:30)` IST → `out_of_session` (window-filtered on the
  backtest side, never counted `missing_live`).
- The LAST TWO session minutes (15:28, 15:29) are compared ONLY when the live bar exists,
  else counted `tail_unsealed` — Groww has no close-time force-seal (the 15:30:05 IST
  close-time force-seal is `Feed::Dhan`-only, verified in main.rs Task 3b) and the watermark
  catch-up's 60s Groww margin leaves those buckets unsealed at 15:50.

## §37.3 The BruteX producer contract (the shared contract; relay target for the BruteX repo)

- `symbol` = the NSE trading symbol for stocks (`RELIANCE`); the canonical index name
  (`NIFTY` / `BANKNIFTY` / `SENSEX`) for indices; `<UNDERLYING>-<YYYY-MM-DD>-FUT` for futures
  (`NSE-` / `BSE-` prefixed forms tolerated).
- `<segment>` path component ∈ {`IDX_I`, `NSE_EQ`, `BSE_EQ`, `NSE_FNO`, `BSE_FNO`} —
  ADVISORY only (the §37.4 join decides; a label disagreement is counted, never trusted
  verbatim as the join key).
- `timestamp_ist` = `"YYYY-MM-DD HH:MM:SS"`, the minute-OPEN IST label, session
  `09:15:00`–`15:29:00`.
- Header row REQUIRED. Optional `_MANIFEST.json` written AFTER all files (the
  publish-complete marker).
- Publish deadline **15:45 IST**; if absent at 15:50 → bounded re-poll until **16:05**, then
  the day reads `NO_DATA` LOUDLY — never a pass (Rule 11: `compared == 0` can never render
  green).

## §37.4 Mapping (the answered question)

The join table is `instrument_lifecycle WHERE feed='groww' AND dry_run=false` (one
current-state row per instrument, ts pinned):

- **Stocks:** by `symbol_name` (the NSE trading symbol, `instrument_type='EQUITY'`).
- **Indices:** by `symbol_name` AFTER stripping the `NSE-`/`BSE-` prefix +
  `canonicalize_index_symbol` (`instrument_type='INDEX'`).
- **Futures:** by `(underlying_symbol, expiry_date)` CONTRACT identity
  (`instrument_type='FUTIDX'`) — never by native id (§36.2: Groww token ≠ Dhan SID).
- Symbols are NEVER matched raw across classes (an index join without the
  `instrument_type` filter can swallow FUT rows — both start `NSE-`).

## §37.5 Honest envelope (mandatory per §5 / operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: a pure ratchet-tested
> comparer (paise-integer compare, hostile CSV parse, mapping normalization, keep-better
> rerun guard). NOT claimed: EITHER side as ground truth — TickVault carries the known
> tick-conservation residual, Groww historical carries vendor-fill fluctuation — the report
> separates 'expected fluctuation' from 'real divergence' and never assumes live is correct
> (Quote 1). The live S3 + QuestDB legs are verified on the box at the first enabled run (no
> AWS creds in CI). The results page is a read-only public GET per Quote 2 + the 2026-06-23
> public-read precedent. Codes BRUTEX-XVERIFY-01/02 are log-sink-only today (no CloudWatch
> `error_code_alerts` entry) — the Telegram daily summary is the operator signal."

## §37.6 What a violating PR looks like (REJECT)

- Any Groww API historical/candle fetch from TickVault OUTSIDE the §38 grant (2026-07-13 narrowing: §38's per-minute SCHEDULED pulls are the sole authorized exception — §33 stands for every other Groww fetch, incl. all bulk/backtest sweeps).
- Any module/type named `backtest` / `BacktestSource` (§33.2 naming ban stands — the
  consumer is `brutex_crossverify`).
- Reviving SP5–SP7 of the cancelled parity plan.
- Writing to `ticks` / `candles_*` from this path (audit tables ONLY — live-feed purity
  rule 4).
- Any hot-path involvement (this is a once-a-day cold-path task).
- A mutating web endpoint (the §37 surface is read-only GET).
- Classifying volume in the verdict without a fresh dated quote flipping Quote 3.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator
must update THIS §37 first with a fresh dated quote.

## §37.7 Trigger (auto-loaded)

Always loaded. Reinforced on any session editing `crates/app/src/brutex_crossverify*`,
`crates/storage/src/brutex_crossverify*`, the `[brutex_crossverify]` config section, or any
file containing `BRUTEX-XVERIFY` or `brutex_crossverify`.

---

# §38 — Groww per-minute REST pipeline: spot 1m + option chain + per-contract 1m (operator authorization 2026-07-13)

> **Authority:** this section PARTIALLY SUPERSEDES §33 (see the dated banner on §33's header
> and the dated notes on §33.1/§33.2/§33.4/§33.5). It re-authorizes exactly ONE narrow
> Groww-fetch class — PER-MINUTE SCHEDULED in-session REST pulls mirroring the Dhan
> `no-rest-except-live-feed-2026-06-27.md` §8 pipeline — and NOTHING else. §33's ban on bulk
> Groww historical / backtest fetching otherwise stands. The 2-Dhan-WS lock (§3), the
> shared-table model + feed-in-key, default-OFF per-feed toggles, the token-minter lock
> (`groww-shared-token-minter-2026-07-02.md`), and the §4/§32.4/§36.3 REJECT conditions all
> stand unchanged; the §37.6 REJECT conditions stand EXCEPT the historical-fetch bullet,
> which this §38 grant narrows (per-minute scheduled pulls are the sole exception — reworded
> in place with a dated 2026-07-13 qualifier).
> **Cross-ref:** `no-rest-except-live-feed-2026-06-27.md` §9 (the same-day KEEP-class rows +
> grant); companion plan `.claude/plans/active-plan-groww-rest-1m.md` (APPROVED 2026-07-13).

## §38.0 The verbatim operator authorization (2026-07-13, relayed verbatim via the coordinator session — preserve exactly, typos included)

**Quote 1 (the directive):**
> "can we implement the same Groww one min fetch which is precisely very similar to the same Dhan — REST api pull ohlcv entirely and even then instantly option chain api also... for Groww live feed and now we planned to add this live REST which is very similar to Dhan. That's it."

**Quote 2 (latency visibility):**
> "always clearly note within a second — or within how many seconds precisely — we are fetching this live real OHLCV, along with the option chain API."

**Context 3 (verbatim intent, not a quote — labeled as such):** the purpose is backtest↔live
parity of the operator's fill model. BruteX backtests on Groww 1-minute candles with a
worst-case fill rule: signal on the current candle → fill at the NEXT minute's worst-case
high/low, for BOTH the underlying AND the option leg. Live must recreate exactly that combo
from the same data source; therefore per-contract 1m candles for selected option contracts
are in scope (the chain endpoint serves strike discovery/liquidity), and the close-to-data
latency measurement is load-bearing (the signal→next-minute-fill window depends on it).

## §38.1 The grant — one paragraph

PER-MINUTE SCHEDULED pulls only, in-session ([09:15, 15:30) IST trading days, the Dhan §8
fire pattern): (a) **spot 1m** — the 3 indices (NIFTY / BANKNIFTY on NSE, SENSEX on BSE,
segment CASH) via `GET https://api.groww.in/v1/historical/candles`
(`candle_interval="1minute"`, `groww_symbol` identity), one day-granular
`start_time`/`end_time` window per fire with client-side target-minute filtering (the
Dhan-#1499 lesson — never an undocumented sub-minute window); (b) **option chain** — the
SAME 3 underlyings' CURRENT expiry (resolved from the already-ingested daily Groww
instruments CSV — never guessed, zero extra rate cost) via
`GET /v1/option-chain/exchange/{exchange}/underlying/{underlying}?expiry_date=...`,
sequenced after the spot leg; (c) **per-contract 1m** — the same `/v1/historical/candles`
endpoint with `segment=FNO` for a BOUNDED selected set of active option contracts (selection
fed by the chain snapshot / instruments master; envelope cap on contracts per minute). The
bulk-history / backtest-fetch ban of §33 OTHERWISE STANDS — no 30-day sweeps, no past-day
backfills beyond the one-minute-lookback and 15:31 post-session sweep patterns (the Dhan
PR #1499 pattern, pending merge), no `BacktestSource`/`backtest` naming. Cold-path scheduled
tasks only; the live WS capture chain, the tick hot path, and the Dhan legs are untouched.

## §38.2 The locked contract table

| Aspect | Locked value |
|---|---|
| Endpoints | `GET api.groww.in/v1/historical/candles` (`candle_interval="1minute"`, day-granular `start_time`/`end_time` in `yyyy-MM-dd HH:mm:ss` (IST Assumed — Indian-exchange convention, confirm live), target-minute filtered client-side) + `GET api.groww.in/v1/option-chain/exchange/{e}/underlying/{u}?expiry_date=YYYY-MM-DD` |
| Identity | the CANDLE legs (spot + contracts) take `groww_symbol`: indices `NSE-NIFTY` / `NSE-BANKNIFTY` / `BSE-SENSEX` (segment CASH); contracts `NSE-NIFTY-04Jan24-19200-CE`-shape (segment FNO). The CHAIN endpoint's `underlying` path param is the PLAIN symbol (`NIFTY` / `BANKNIFTY` / `SENSEX`), NOT groww_symbol. Persisted `security_id` = the SAME ids the Groww live lane uses (`stable_index_security_id` indices; `exchange_token` contracts) |
| Token | shared-minter SSM READ-ONLY via the existing `fetch_groww_access_token` (`/tickvault/<env>/groww/access-token`) — NEVER minted, never logged, never in URLs; `Authorization: Bearer <token>` + `x-api-version: 1.0`. The token's ~06:00 IST daily expiry is OFFICIALLY documented (upgraded from assumption, 2026-07-13 docs research); the bruteX minter Lambda re-mints ~06:05 IST |
| Tables | SAME shared tables + feed-in-key: `spot_1m_rest` + `option_chain_1m` tagged `feed='groww'` (their DEDUP keys already carry `feed`); the per-contract leg gets ONE new table (proposal `option_contract_1m_rest`) with `feed` in its DEDUP key + retention registration. NEVER `ticks` / `candles_*` / `historical_candles` |
| Expiry source | the already-ingested daily Groww instruments CSV (nearest expiry ≥ today; never-roll) — no new expiry REST endpoint |
| Rate budget | ~6–12 requests/min in-session against the documented Live Data 10/sec + 300/min type bucket (the `/historical/*` + `/option-chain/*` bucket is UNNAMED in the docs → conservatively assumed Live Data); own min-gap pacing on the chain/contract legs |
| Latency mandate (Quote 2) | per-fetch close-to-data latency stored PER-ROW + histograms (`tv_groww_spot1m_close_to_data_ms` / `tv_groww_chain1m_close_to_data_ms`) + a plain-English daily digest/scorecard line per feed per leg |
| Config gates | `[groww_spot_1m]` / `[groww_option_chain_1m]` / the contract-leg section — all serde default OFF; base.toml opts in per leg; the chain leg's DEFAULT stayed OFF pending first-live-session verification + a dated note — **verified + flipped ON in base.toml 2026-07-13 after the live probe PASSED (§38.6; the serde DEFAULT stays OFF)** |

## §38.3 Honest envelope (mandatory per §5 / operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: bounded scheduled
> fetchers with per-request timeouts, per-leg hard budgets, discard-pending persist defense,
> DEDUP-idempotent re-fetch, and edge-triggered pagers. NOT claimed: just-closed-minute
> freshness — Groww documents NO availability delay for the sealing minute, so it is
> UNVERIFIED-LIVE; we NEVER assert 'within a second' — we MEASURE and SHOW the number
> (per-row latency columns + histograms + the daily digest line, per Quote 2). The V2
> candle timestamp type and the endpoints' rate bucket are also UNVERIFIED-LIVE (first live
> session is the probe; defensive dual-format parse; 429s counted, never out-polled).
> Neither side is ground truth per the §37 doctrine — TickVault carries the known
> tick-conservation residual, Groww's candle store carries vendor-fill/sealing fluctuation —
> the parity comparison separates expected fluctuation from real divergence and never
> assumes live is correct. Capacity verdict (2026-07-13 docs research): the rate buckets are
> TYPE-LEVEL POOLED (exhausting one API throttles the whole type; Orders changed 15→10
> between Dec'25 and Mar'26 — numbers CAN change, re-verify on the box), Groww documents NO
> Retry-After/ban/cooldown for 429 and the SDK ships ZERO client-side throttling — pacing
> (minute-boundary bursts spread to ≤6 req/s against the shared 10/s ceiling) and timeouts
> are entirely ours; worst-case ~18 req/min ≈ 6% of the 300/min budget solo, ~66% with
> in-session BruteX co-tenancy — still inside."

## §38.4 What a violating PR looks like (REJECT)

- Any BULK Groww historical / backtest fetch (multi-day sweeps, 30-day windows consumed as
  history, past-day backfills beyond the one-minute-lookback / 15:31-sweep patterns) — §33
  stands for everything outside the per-minute scheduled shape.
- Any module/type named `backtest` / `BacktestSource` (§33.2 naming ban stands).
- Any UNBOUNDED or tighter-than-per-minute polling, or exceeding the shared Live Data
  10/sec + 300/min budget share (BruteX shares the same token's bucket).
- Minting a Groww token, caching one past an auth failure, or reading the credential params
  (token-minter lock 2026-07-02).
- Writing any leg's output to `ticks` / `candles_*` / `historical_candles`, or omitting
  `feed` from a DEDUP key.
- Touching GDF in the name of this grant (GDF is a separate parked trial —
  `gdf-third-feed-scope-2026-07-13.md`).
- Shipping the chain leg DEFAULT-ON before first-live-session verification AND a fresh dated
  quote recorded here.
- Any hot-path / WS-read-loop / strategy / order involvement (cold-path only; §28 boundary).
- Extending scope (a 4th index, stocks, non-current expiries, an unbounded contract set)
  without a fresh dated quote HERE first.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator
must update THIS §38 first with a fresh dated quote.

## §38.5 Trigger (auto-loaded)

Always loaded. Reinforced on any session editing `crates/app/src/groww_spot_1m*`,
`crates/app/src/groww_option_chain_1m*`, `crates/app/src/groww_contract_1m*`,
`crates/storage/src/option_contract_1m_rest*`, the `[groww_spot_1m]` /
`[groww_option_chain_1m]` config sections, or any file containing `groww_spot_1m`,
`groww_option_chain_1m`, `option_contract_1m_rest`, `v1/historical/candles`,
`v1/option-chain`, `tv_groww_spot1m_close_to_data_ms`, or `tv_groww_chain1m_close_to_data_ms`.

## §38.6 — 2026-07-13: chain-leg first-live verification PASSED → `[groww_option_chain_1m].enabled` flips to true in base.toml (dated note)

**Live-probe evidence (Verified, prod box, build `eeca0ec`, ~11:47 PM IST 2026-07-13 —
the operator's Telegram probe report, relayed verbatim via the coordinator session):**
the boot-time Groww chain probe (`run_groww_chain_1m_probe`, the probe-only path this
grant shipped DEFAULT-OFF) hit the live `GET /v1/option-chain/...` endpoint for all 3
underlyings' current expiry and PASSED:

| Underlying | Strikes | Contract prices | Fetch time | Payload |
|---|---|---|---|---|
| NIFTY | 99 | 198 | 0.2s | 36 KB |
| BANKNIFTY | 172 | 344 | 0.2s | 64 KB |
| SENSEX | 189 | 378 | 0.2s | 69 KB |

The probe report closed with: *"recording currently switched OFF — to start, turn ON the
setting and restart."*

**Authorization (coordinator-relayed operator directive, 2026-07-13):** the operator
directed enabling the chain leg TONIGHT so that tomorrow's session records option chains
from the first in-session minute close (09:16 IST). Per that directive, and mirroring the
Dhan precedent exactly (`no-rest-except-live-feed-2026-06-27.md` §8.7),
`config/base.toml [groww_option_chain_1m].enabled` flips `false → true` in the PR carrying
this note. This satisfies BOTH conditions of the §38.4 REJECT row ("Shipping the chain leg
DEFAULT-ON before first-live-session verification AND a fresh dated quote recorded here"):
the first-live verification is the probe evidence above, and this dated note is the quote
record — recorded HERE first, in the same PR as the flip.

**What stays unchanged (fail-safe posture, the Dhan §8.7 shape):** the serde DEFAULT in
`crates/common/src/config.rs` stays OFF (an absent `[groww_option_chain_1m]` section still
means disabled); `probe_and_report` stays `true` (inert while enabled; the automatic
fallback canary on any rollback to `enabled = false`); everything else in the §38.2 scope
table — 3 underlyings, current expiry only, per-minute cadence sequenced after the spot
leg, `option_chain_1m` table only (`feed='groww'`), the shared Live Data budget share —
is UNCHANGED.

**Honest caveat (§38.3 discipline):** the probe proves ENTITLEMENT + response SHAPE +
fetch SPEED with the market CLOSED (~11:47 PM IST). It does NOT prove in-session
just-closed-minute freshness or per-minute delivery under load — those are MEASURED from
the first enabled session onward via the per-row `close_to_data_ms` column + the
`tv_groww_chain1m_close_to_data_ms` histogram (Quote 2), and any in-session problem
surfaces through the leg's own edge-triggered paging (the CHAIN-02-class 3-minute
escalation + the coded per-minute failure logs — see
`rest-1m-pipeline-error-codes.md`), never a silent gap.
