# Implementation Plan: TrueData Fourth Live-Tick Feed (feed='truedata')

**Status:** APPROVED
**Date:** 2026-07-24
**Approved by:** Parthiban (operator) — approved 2026-07-24 in-session (AskUserQuestion: "Approve the plan, don't merge yet")
**Scope authority:** `.claude/rules/project/truedata-feed-scope-2026-07-24.md` (operator quotes 2026-07-24)
**Guarantee matrices:** the 15-row + 7-row matrices of `.claude/rules/project/per-wave-guarantee-matrix.md` apply to every PR below (cross-referenced, not re-pasted).

---

## Design

**Goal:** add TrueData as a native-Rust, default-OFF, fourth market-data feed
that is the intended LIVE-TICK source, deriving ALL timeframes from ticks
(operator Quote 2), with zero tick loss (WAL + per-symbol Sequence-No proof),
RAM-first decisions (Quote 4), everything persisted async to QuestDB, on an
upgraded m8g.large instance, reversible to OFF + t4g.medium (Quote 5).

**RUST O(1) everywhere (operator directive 2026-07-24 + `rust-only-forever-lock-2026-07-24.md`):**
- Native Rust ONLY. No Python/Node/.Net sidecar (TrueData SDK = reference). The
  `rust_only_guard.rs` shrinking allowlist stays shrinking.
- Tick parse = **O(1) fixed-offset `from_le_bytes` on the 90-byte Trade Binary
  struct** (byte map in the scope file §2) — no loop, no alloc.
- Registry lookup = O(1) `papaya` map on `(security_id, exchange_segment)`.
- Ring push = O(1) SPSC bounded (`seal_ring.rs`).
- Per-symbol Sequence-No gap check = O(1) (`HashMap<(sid,seg), last_seq>`).
- Aggregator fold = O(1) per tick per timeframe.

**Two-path architecture (RAM-first absolute):**
```
TrueData WSS ──(read task: drain→WAL→ring ONLY)──▶ ring(200k seal) ──▶ parse+dedup+seq-check
      │                                                                        │
      │ (doc (f): no logic on read task)                    ┌──────────────────┴──────────────────┐
      ▼                                                     ▼ HOT (RAM only, ns)     ▼ COLD (async, fire-and-forget)
   WAL-before-broadcast                          tick→TF aggregator (1s/1m/3m)   QuestDB writer feed='truedata'
   (zero-loss floor)                             → RAM ring 7-10d → decisions    (persistence/audit; DB-down never blocks)
```

**Feed plumbing:** `Feed::Truedata` added to the enum + `Feed::ALL` (every
exhaustive match forces a compile error until updated); `feeds.truedata_enabled`
(serde default false); `WsType::TruedataFeed`; `FeedStrategy::Truedata`
aggregator instance; SSM 1-session lock `instance-lock-truedata`.

**Instance:** m8g.large (8 GiB) for the trial; reverse downsize to t4g.medium is
the documented exit (scope §3).

## Edge Cases

- **Sequence-No gap** (doc (e)): `seq_n ≠ seq_{n-1}+1` → `TD-GAP-01` with missing
  range; the WAL still captured every RECEIVED frame — a gap means the vendor/
  network dropped upstream, proven not hidden.
- **Sequence-No reset** at reconnect / new session / daily rollover — treated as
  a fresh baseline, not a false gap (per-symbol first-seq re-seed).
- **Receive-buffer pressure** (doc (f)): read task must never block; a slow
  consumer backs up into the bounded ring → spill → DLQ, never into the socket.
- **Combined tick+bid/ask frame** (doc (g)) vs trade-only (doc (h) with bidask
  deactivated): parser handles BOTH shapes (90-byte with bid/ask populated, or
  bid/ask zeroed) — never assume bid/ask present.
- **BSE bidaskL2** 5-level control frame (doc (iii)): recognized + ignored for
  the trial (L1 only); never a parse panic.
- **1-session-per-key** ("User Already Connected"): lock refused → degrade, no
  TrueData this boot, page once, never reconnect-storm the peer.
- **Endianness / timestamp epoch** UNVERIFIED-LIVE → sandbox probe confirms
  before prod; defensive.
- **MaxSymbols cap** from the 128-byte auth reply bounds the subscribe set
  (300→500 fail-closed, never truncated silently).
- **f32 widening** → `f32_to_f64_clean` (STORAGE-GAP-02).
- **Cross-feed id collision** — TrueData token ≠ Dhan/Groww id; joins by ISIN /
  canonical symbol / (underlying,expiry), never native id (I-P1-11).

## Failure Modes

- **WSS disconnect** → bounded expo reconnect (5s→60s) + full re-auth + re-subscribe + lock re-check; every disconnect stamps `ws_event_audit` (`WsType::TruedataFeed`, feed='truedata').
- **QuestDB stall/outage** → cold path only; decisions unaffected (Quote 4); rows absorbed by rescue→spill→DLQ, replayed DEDUP-idempotent.
- **Ring saturation** → NDJSON spill → DLQ (bounded zero-loss envelope).
- **Auth failure / stale creds** → SSM re-read (never mint; creds are operator-set), bounded retry, edge-latched page; never a credential in a log.
- **Parser sees a non-90-byte / unknown Msg Code frame** → counted + dropped, no panic (annexure no-panic-on-unknown discipline).
- **Instance RSS exceeds m8g budget** → measured via `tv_process_rss_bytes`; r8g.large rip-cord (scope §3).

## Test Plan

- **Unit** (`crates/core`): 90-byte parser fixed-offset tests against the v2.6 doc's exact sample (`Symbol Id 100001262, LTP 1472.8, Seq 12345, Bid 1472.8/429, Ask 1473.3/34`); byte-offset regression pins; f32→f64 clean; unknown-Msg-Code no-panic.
- **Sequence-No** unit: monotonic ok / gap detected / reset re-seed / per-symbol isolation.
- **Property** (proptest): random 90-byte buffers never panic; seq-gap detector never underflows.
- **DHAT** zero-alloc: parse+enqueue path ≤ budget across 10K ticks (hot-path O(1) proof).
- **Criterion** bench: `dispatch_frame/truedata_trade` ≤ 10 ns (dispatch budget).
- **Integration**: WAL→ring→spill→DLQ replay DEDUP-idempotent for feed='truedata'; aggregator derives 1s/1m/3m from a tick sequence; feed toggle ON/OFF.
- **Config**: `truedata_enabled` serde default false; empty-TOML disabled; independent of dhan/groww flags.
- **Scope guard** (`crates/storage/tests/…`): the scope-file phrases + the 90-byte layout constants pinned; `feed` in every truedata DEDUP key.
- Scoped per changed crate (`common` change ⇒ workspace escalation per testing-scope).

## Rollback

- **Feed OFF** = default state: `truedata_enabled=false` (or runtime toggle) → WSS closed, no storage, byte-identical to today. Zero code change.
- **Instance downsize** = the guarded workflow m8g.large → t4g.medium (EIP+EBS preserved, snapshot-first), authorized by scope §3 / operator Quote 5.
- **Per-PR** rollback: each PR ships default-OFF and is independently revertible; feature-flag + config-flip path is a tested code path.
- Persisted `feed='truedata'` data is retained (SEBI never-delete); turning the feed off never deletes it.

## Observability

- **Counters:** `tv_truedata_ticks_total`, `tv_truedata_seq_gaps_total`, `tv_truedata_frames_dropped_total{reason}`, `tv_truedata_reconnects_total`, `tv_truedata_lock_refused_total`, `tv_process_rss_bytes` (instance-fit).
- **Histograms:** parse latency ns; close-to-derive latency per timeframe.
- **Audit:** `ws_event_audit` (connect/disconnect/auth, feed='truedata'); tick loss provable from `tv_truedata_seq_gaps_total` + the WAL.
- **Error codes:** `TD-FEED-01` (feed degraded), `TD-GAP-01` (sequence gap with missing range) — runbooks in `docs/error-runbooks/`, log-sink-only for the trial (Telegram wiring is a flagged follow-up needing a noise-lock rule edit).
- **Telegram:** plain-English feed-health (which broker named) per the 10 commandments — trial posture is quiet (counters + daily digest), a page only on terminal feed-down.

---

## Plan Items (serial PRs — B..F start only after Status = APPROVED)

- [ ] **PR-A** — Scope-lock rule file (DONE, committed with this plan) + this plan (DRAFT) + the two pointer lines in `websocket-connection-scope-lock.md` + `operator-charter-forever.md` §I
  - Files: `.claude/rules/project/truedata-feed-scope-2026-07-24.md`, `.claude/plans/active-plan-truedata-feed.md`, `.claude/rules/project/websocket-connection-scope-lock.md`, `.claude/rules/project/operator-charter-forever.md`
  - Tests: docs-only (design-first wall PASS on non-impl)
- [ ] **PR-B** — `Feed::Truedata` (→ `Feed::ALL`/`COUNT=3`/`index()=2`/`as_str()="truedata"`/`display_name()`; **`live_ws_retired()=FALSE`** — its WS is NOT retired, unlike Dhan/Groww) + `FeedsConfig.truedata_enabled` (default OFF) + `WsType::TruedataFeed` (grow `all()` array + length pin) + `TruedataConfig` + `[feeds.truedata]` in base.toml. **Namespace bit pair (bit 59 token / 58 FNV-fallback)** with build-time pairwise-disjoint assertion vs GDF(61/60)/Groww(62)
  - Files: `crates/common/src/feed.rs`, `crates/common/src/config.rs`, `crates/common/src/ws_event_types.rs`, `config/base.toml`
  - Tests: `feed.rs` round-trip/index/exhaustive; `live_ws_retired` false-arm; namespace-disjoint assertion; config serde-default-off; workspace escalation (common change)
- [ ] **PR-C** — Native Rust WSS client + 90-byte Trade Binary parser (O(1) fixed-offset) + 128-byte auth-reply parser + SSM 1-session lock (+ force-logout `logoutRequest`+60s on ghost session; no reconnect-storm) + SSM cred read (`Secret<String>`) + **session-0 probe gate** (binary-vs-JSON default, endianness, Bid/Ask mode, LTP-vs-touchline sanity — fail-closed) + **TCP port-7070 binary fallback** if WSS is JSON (guarantees O(1)). **Decode: first-byte sniff; decompress LZ4/GZIP into a PRE-ALLOCATED per-connection scratch buffer (never per-frame alloc — re-DHAT)**. **Identity: `security_id` = STABLE name-derived id (`stable_index_security_id`/ISIN/(underlying,expiry,strike)) — SymbolID is a session routing key in a `papaya` map only, resolved at parse time (SymbolID is session-volatile)**. **Volume: saturate `u32::try_from(TTQ i64).unwrap_or(MAX)` + counter + WARN (no silent `as u32`)**. Bounded pre-touchline buffer (overflow → WAL). Reconnect = full re-auth + re-addsymbol (no resume); daily blackout ~07:30–08:00 IST
  - Files: `crates/core/src/feed/truedata/{mod,connection,parser,auth,lock,probe,decode,symbol_map}.rs`
  - Tests: parser fixed-offset + doc sample vector; LZ4/GZIP decompress zero-alloc (DHAT); auth-reply parse; no-panic unknown; len-dispatch (90/128/10/62); stable-id resolution; volume saturation; Criterion
- [ ] **PR-D** — Wire the TrueData lane into WAL→ring→spill→DLQ (**add per-feed WAL path `data/spill/truedata/` + feed tag — the WAL is transport-typed today, no feed byte**) + **REBUILD** the tick→TF aggregator (`MultiTfAggregator`/`FeedStrategy::Truedata` — HARD-DELETED 2026-07-17, recover from git pre-sweep; NOT a revive) deriving 1m/3m via the surviving `TfIndex` + a **SEPARATE 1s lane** (1s is NOT in TfIndex) + per-symbol Sequence-No gap detector (with reset-on-reconnect/new-day heuristic so it never false-pages)
  - Files: `crates/core/src/feed/truedata/pipeline.rs`, `crates/storage/src/ws_frame_spill.rs` (per-feed path), aggregator rebuild, `crates/app/src/truedata_lane.rs`
  - Tests: WAL replay DEDUP-idempotent per-feed; aggregator derives 1m/3m; 1s-lane; seq-gap TD-GAP-01; seq-reset suppression
- [ ] **PR-E** — **REBUILD the deleted tick writer** (`tick_persistence.rs` + `DEDUP_KEY_TICKS` were deleted 2026-07-17; revive `DEDUP_KEY_TICKS` as a **CONST, never an inline write-site literal** — an inline key evades the feed-in-key allowlist guard) — async cold-path QuestDB writer (`feed='truedata'`, feed-in-key DEDUP) for ticks + derived candles + **new `candles_1s` table** (1s lane; the 21 `candles_*` need no schema change, feed already in-key); RAM-first guard (no DB read in decision paths)
  - Files: `crates/storage/src/tick_persistence.rs` (rebuild + `DEDUP_KEY_TICKS` const), `crates/storage/src/*` (feed-parameterized + `candles_1s`), banned-pattern guard extension
  - Tests: `DEDUP_KEY_TICKS` includes `feed` (meta-guard); `candles_1s` DDL; DB-down does not block; storage meta-guard
- [ ] **PR-F** — Observability (counters/histograms/audit/error-codes) + tests + **reuse the existing PR #1701 m8g.large upgrade (do NOT duplicate the terraform/instance-lock edits — depend on #1701 landing)** + sandbox trial (8086) → prod (8084) runbook
  - Files: `crates/app/src/observability.rs` (metrics), `docs/error-runbooks/truedata-feed-error-codes.md`, trial runbook
  - Tests: metric registration; alarm wiring

## Sequencing dependencies (from the open-PR review — Verified)

| Depends on | Why | Action |
|---|---|---|
| **#1696 (timeframe diet)** land FIRST | reshapes the aggregator + `TF_COUNT` + `candles_Ns` surface PR-D writes into | build PR-D after #1696 merges (else rebase onto a changed TF contract) |
| **#1701 (m8g.large prep)** land FIRST | our instance upgrade is already prepped there — reuse, don't duplicate | PR-F depends on #1701; no new terraform |
| ~~#1700 (GDF #3 design)~~ | **NOT needed (operator 2026-07-24: "gdf design is not yet needed … one and only truedata is needed")** — TrueData is a standalone independent feed; the pluggable pattern already shipped for Groww, no GDF dependency | dropped |
| All open PRs are **DO-NOT-MERGE by design** | operator/coordinator merges them, not this session | do NOT auto-merge; sequence around them |

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | TrueData OFF (default) | Byte-identical to today; no WSS, no storage |
| 2 | Sequence gap mid-session | TD-GAP-01 with missing range; WAL still complete; no crash |
| 3 | Slow consumer / ring pressure | Spill→DLQ; socket never blocked; no upstream drop |
| 4 | 2nd session attempt (lock held) | Refused, degrade, one page, no reconnect storm |
| 5 | QuestDB outage during session | Decisions unaffected (RAM); rows absorbed + replayed |
| 6 | Operator flips feed OFF later + downsize | Config flip + guarded m8g→t4g downsize; data retained |
| 7 | 300 → 500 symbols | O(1) hot path flat; RSS measured vs m8g budget; r8g rip-cord |

---

## Pre-implementation blockers (operator-owned)

1. **Flip this plan DRAFT → APPROVED** (design-first wall).
2. **Provide TrueData trial creds + endpoint** (SSM: user/password/endpoint) + plan MaxSymbols/segments — before the sandbox probe.
3. **Active-plan count:** 5 active plans exist now; this is the 6th → `PLAN_GATE_MAX_ACTIVE=5` will BLOCK impl pushes until ≥1 stale plan is archived (plan-enforcement rule 7). Archive one merged/complete plan before PR-B pushes.
