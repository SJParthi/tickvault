# GDF 13 — tickvault Integration PROPOSAL (feed #3, `feed='gdf'`)

> **Status: PROPOSAL ONLY — research artifact. NO code exists, NO plan is approved, and NO code PR may open until the operator provides a dated-quote authorization recorded FIRST in the rule files (`websocket-connection-scope-lock.md` re-approval protocol + a new `gdf-third-feed-scope-<date>.md`).** This file translates the pack's protocol facts into the repo's established feed-#N shape so the eventual PR-0 rule file can be drafted with zero re-research.
> **Fetched:** 2026-07-13 · **Evidence tier:** repo-verified (file:line references checked against the working tree 2026-07-13) + this pack's GDF facts (per-file tiers). GDF-side Assumed items are marked.

---

## 1. Shape of the integration (one paragraph)

GDF becomes a third pluggable feed under the existing per-feed contract (`groww-second-feed-scope-2026-06-19.md` §1/§2): `Feed::Gdf`, **default OFF**, native tickvault Rust (tokio-tungstenite JSON client — no vendor SDK code), writing the SAME shared tables with `feed='gdf'` (feed-in-key everywhere; **zero schema change**), reusing the WAL→ring→spill→DLQ→21-TF-aggregator chain AS-IS. The `Feed` enum is exhaustive-match with no `_` arms (`crates/common/src/feed.rs`), so adding the variant is a compiler-enumerated sweep (518 `Feed::Dhan|Feed::Groww` occurrences, mostly tests / `Feed::ALL`-driven).

## 2. Integration map

| Need | Mirrors | 3rd-arm work |
|---|---|---|
| `FeedsConfig.gdf_enabled` (default OFF, `#[serde(default)]`) | `crates/common/src/config.rs:110-152` | new field + `any_enabled()`; ⚠ `both_enabled()` (config.rs:548) is a 2-feed assumption — redefine (e.g. `multi_enabled()`) |
| `Feed::Gdf` | `crates/common/src/feed.rs` (`ALL`, `COUNT`, `index()`, `as_str()="gdf"`, `display_name()`, `is_runtime_toggleable()`) | 5 match arms + ALL entry; downstream exhaustive matches become compile errors (by design) |
| `WsType::GdfFeed` | `crates/common/src/ws_event_types.rs:28-69` (GrowwBridge precedent) | new variant + `all()` 5→6 + label tests |
| Shared-table rows | `DEDUP_KEY_TICKS = "security_id, segment, capture_seq, feed"` (`crates/storage/src/tick_persistence.rs:94`); lifecycle/constituency/ws_event_audit already feed-keyed | zero schema change; NO `gdf_*` tables (REJECT class) |
| Durable capture floor | Dhan WAL-before-broadcast (`crates/storage/src/ws_frame_spill.rs`) — the stronger floor for an in-process client | feed-parameterize the `ws_frame_spill.rs:371` `record_drops(Feed::Dhan, 1)` hardcode; per-feed WAL dir (`data/spill/gdf/`) |
| Aggregator | per-feed `MultiTfAggregator` instance (`crates/app/src/groww_bridge.rs:2168` precedent) | `FeedStrategy::GDF` (late-tick policy = operator decision); `CATCHUP_SEAL_LATENESS_MARGIN_SECS_GDF`; per-feed catch-up driver (BOUNDARY-01 contract) |
| ws_event_audit | `emit_groww_ws_audit` pattern (edge-latched; boot Connected row) | same helper with `WsType::GdfFeed` + `feed=Feed::Gdf`; AUDIT-WS-01 covers failures |
| Scoreboard/presence/blame | `feed_presence.rs` arrays are `[_; Feed::COUNT]` (auto-grow); `feed_blame.rs` lag floors; `feed_scoreboard_boot.rs:3389` match | **`LAG_FLOOR_MS_GDF = 1000`** (second-resolution feed — 12 §5); GDF slot registration by ISIN/canonical-index/(underlying,expiry); GDF blame slugs |
| Lag monitor | `crates/core/src/pipeline/feed_lag_monitor.rs:497-663` static per-feed histograms (NOT auto-growing) | `GDF_DAY_LAG_HIST` static + ring + arm |
| Credentials | `groww-shared-token-minter-2026-07-02.md` pattern | `/tickvault/<env>/gdf/api-key` — SSM read-only + `Secret<String>`, in memory only, never logged. GDF keys are long-lived (no daily mint observed) → no minter Lambda needed (Assumed); re-read on auth failure with bounded pacing, never cache past an auth reject |
| Instrument master | `crates/core/src/feed/groww/instruments.rs` pattern | GetInstruments pull (WS frame on the same socket — NOT a REST fetch, so no new REST class; if the REST master route is preferred instead, it needs a `no-rest-except-live-feed` §3 KEEP row FIRST) → `gdf-watch-<date>.json`; lifecycle rows `feed='gdf'` audit-first |
| Feed toggle/page | `/feeds` renders from `Feed::ALL` | `POST /api/feeds/gdf` works once `Feed::parse("gdf")` exists; bearer-gated automatically (2026-07-04 lock) |

### The 8 two-feed hardcode hotspots (production code — the real 3rd-arm list)

1. `crates/api/src/feed_state.rs:495-560` — per-feed `AtomicBool` fields + `lane_running()/is_enabled()/set_enabled()` matches + `FeedStatus` named per-feed fields.
2. `crates/common/src/config.rs:541-548` — `any_enabled()` / `both_enabled()`.
3. `crates/app/src/seal_routing.rs:150-158` — per-feed drop-counter label arms + `drop_d1` policy (GDF D1 decision needed).
4. `crates/core/src/pipeline/feed_consumer.rs:84-92,191-192` — per-feed post-parse stage lists (+ its literal source-scan guard `crates/core/tests/feed_consumer_convergence_guard.rs` must be updated in the same PR).
5. `crates/core/src/pipeline/feed_lag_monitor.rs:497-663` — static histograms/rings.
6. `crates/app/src/feed_scoreboard_boot.rs:3389-3390` — `LAG_FLOOR_MS_*` match.
7. `crates/storage/src/ws_frame_spill.rs:371` — `record_drops(Feed::Dhan, 1)` hardcode.
8. `crates/storage/src/tick_row_builder.rs` / `generic_candle_writer.rs` / `shadow_seal_columns.rs` / `seal_spill.rs` / `seal_dlq.rs` — feed-label plumbing (writers take `Feed` by value so `gdf` mostly flows through; per-feed `&'static str` consts + pinned test literals need the third entry).

Plus: `ws_event_types.rs:61` fixed `[WsType; 5]`, `main.rs` lane spawn sites (a `gdf_activation.rs` watcher mirroring `groww_activation.rs`/`dhan_activation.rs`), `crates/app/tests/chaos_feed_state_matrix.rs` third DurableFloor row, and GDF-* ErrorCodes + rule-file mentions in the SAME PR (`error_code_rule_file_crossref.rs`).

## 3. Native-client feasibility (why GDF is the EASY feed)

| Layer | GDF | Verdict |
|---|---|---|
| Transport | plain `ws://` — `tokio-tungstenite 0.29` already pinned + battle-tested in `crates/core/src/websocket/connection.rs` (⚠ plaintext, NOT wss — key travels cleartext until U-2 is answered; operator must accept or GDF must confirm TLS) | zero new framing code |
| Auth | one JSON `Authenticate` frame post-connect — the exact shape class of the Dhan order-update WS (MsgCode-42 precedent, `order_update_connection.rs`) | trivial |
| Decode | `serde_json` (pinned) over MessageType-tagged JSON; hot-path note: per-tick `serde_json::from_str` allocates — budget a zero-alloc/borrowed-str or `RawValue` approach + a DHAT test per `hot-path.md` | small |
| Keepalive | passive Echo observation → existing feed-stall watchdog (feed-agnostic `should_restart_on_stall` already takes `Feed`) | free |
| Frame sizes | must NOT cap frames at defaults (instrument master = one multi-MB frame) | config knob |
| New dependencies | **NONE expected** (tokio-tungstenite, serde_json, secrecy, aws-sdk-ssm all pinned) | zero approval asks |

Complexity ≈ 25–40% of the Groww native client (no NATS framing, no nkey, no protobuf, no per-session token mint). Design decision flagged for PR-0 — **the architecture fork is now REAL (StreamAllSymbols wire LIVE-DOC-captured 2026-07-13, 10 §2)**: (a) SubscribeRealtime-per-watch-entry — closest to existing patterns; 1 frame/symbol, quota-aware resubscribe, decodes the PascalCase `RealtimeResult` dialect; vs (b) StreamAllSymbols firehose + client-side watch-set filter — zero subscribe management (auth-only; auto-starts for the key's enabled exchanges), but requires its OWN serde struct for the Batch/abbreviated-key dialect (`{"T":"Batch","data":[{"E":…,"I":…,"LTT":…,"T":"RT"},…]}` — NOT the RealtimeResult shape), must tolerate untraded zero-OHLC rows, and its cadence/bandwidth/endpoint remain unmeasured (U-1 residual). The daily-universe scope still bounds what is PERSISTED either way.

## 4. Identity recommendation

**Option A (preferred): vendor numeric token.** `TokenNumber` EXISTS in GetInstruments (06 §3) → mirror the Groww pattern exactly: token as the per-feed `security_id` with a GDF namespace bit disjoint from Groww's `1<<62` (e.g. `1<<61`), reversible via the daily master, raw `Identifier` persisted in `instrument_lifecycle.symbol_name` (`feed='gdf'`).
**Fallback Option B:** 64-bit hash of `"<EXCH>:<IDENTIFIER>"`, namespaced, with a build-time fail-closed collision check (I-P1-11 discipline) — only if TokenNumber proves unpopulated for some class.
**Rejected:** ISIN-primary reuse of another feed's id space (violates the per-feed-id-space model + couples boots); locally-assigned sequential ids (ordering-fragile).
Cross-feed comparison stays attribute-based, never native-id: ISIN (stocks) / `canonicalize_index_symbol` (indices) / `(underlying, expiry)` (futures) — the `presence_registration.rs` + brutex-crossverify §37.4 rule. Segment mapping per 06 §4. NEVER key on `-I/-II/-III` rolling aliases.

## 5. Proposed serial PR series (design-first-wall compliant; sizes S/M/L)

| # | PR | Scope | Size |
|---|---|---|---|
| 0 | **Rules FIRST** | `gdf-third-feed-scope-<date>.md` (verbatim operator quote, default-OFF, shared tables + feed column, capture floor, REJECT list) + banners in `operator-charter-forever.md` §I + `websocket-connection-scope-lock.md` + `no-rest-except-live-feed-2026-06-27.md` §3 KEEP rows (GDF WS auth; instrument master route; historical = REMOVE unless separately quoted) + `gdf-error-codes.md` stub + APPROVED active plan (6 sections + 15/7 matrices) | S (docs) |
| 1 | Feed enum + config sweep | `Feed::Gdf`, `gdf_enabled`, `WsType::GdfFeed`, all §2 hotspots, guard/test-literal updates | L (mechanical; `crates/common` change → workspace tests) |
| 2 | Instrument master + identity | master pull → mapping → `gdf-watch-<date>.json` → lifecycle rows + audit; INSTR-FETCH-class typed errors | M |
| 3 | Native WS client (SHADOW) | connect→Authenticate→SubscribeRealtime, decode, reconnect ladder, supervised task, shadow NDJSON default-OFF (the §35 `groww_native_shadow` precedent) — the live-probe PR that answers U-6/U-12/U-13/U-16/U-17/U-23 | M |
| 4 | Capture → persist → candles | promote to lane: WAL floor, validator, `ticks feed='gdf'`, aggregator instance + seal routing + catch-up driver + activation watcher | L |
| 5 | Observability + scoreboard | audits, feed-health, GDF-* ErrorCodes + runbooks, lag floor/presence/blame, stall-watchdog analog, CloudWatch cost note (`aws-budget.md` dated) | M |
| 6 | E2E + chaos | chaos matrix 3rd row, live-pipeline e2e, tri-feed coexistence guard, doctor/MCP | S–M |

Each PR: draft → All Green on the exact final head → coordinator merges (merge-gate-lock §3.2); one at a time; 3-agent adversarial review on PRs 1–5 (>3 crates).

## 6. Binding rule-file constraints (MUST respect — no exceptions)

| Rule file | Binding clause |
|---|---|
| `websocket-connection-scope-lock.md` + `operator-charter-forever.md` §I | **rule-file edit FIRST with a dated operator quote** before ANY new WS endpoint lands; verbal approval is insufficient by the lock's own text |
| `groww-second-feed-scope-2026-06-19.md` §1/§2/§4 | pluggable-feed contract: SAME shared tables + `feed` column; default OFF; reuse the resilience chain AS-IS; no weaker guarantee than existing feeds; native Rust only (no vendor SDK code vendored) |
| `live-feed-purity.md` | `ticks` populated EXCLUSIVELY from live WS frames — GDF historical/snapshot pulls NEVER into `ticks`/`candles_*` |
| `no-rest-except-live-feed-2026-06-27.md` §2/§3 | only live-feed AUTH + static instrument master are KEEP classes without a §-edit; a GDF REST-master route or ANY GDF historical use needs its own dated KEEP/grant row FIRST |
| `groww-shared-token-minter-2026-07-02.md` (pattern) | SSM read-only credentials, `Secret<String>`, memory-only, bounded re-read pacing on auth failure |
| 2026-07-04 toggle lock | `POST /api/feeds/gdf` bearer-gated unconditionally; GET stays public read |
| `security-id-uniqueness.md` I-P1-11 + `data-integrity.md` | composite `(security_id, exchange_segment)` in every collection; `(…, feed)` in every persisted DEDUP key |
| `observability-architecture.md` + error-code guards | every GDF `error!` carries `code =`; every new ErrorCode gets a rule-file mention in the SAME PR; flush/persist failures at `error!`; edge-triggered alerts; market-hours gates |
| `design-first-wall.md` / `plan-enforcement.md` / `per-wave-guarantee-matrix.md` | APPROVED plan with 6 sections + 15/7 matrices before any `crates/*/src` change; ≤5 active plans |
| `zero-loss-guarantee-charter.md` / `z-plus-defense-doctrine.md` | capture-at-receipt/WAL floor; honest-envelope wording on any "100%"; L1–L7 per item |
| `aws-budget.md` | any new CloudWatch metric/alarm = dated cost note (the $35/mo pre-GST ceiling) |
| `merge-gate-lock-2026-07-04.md` + `pr-completion-protocol.md` | serial PRs; All Green on exact final head; coordinator merges |

## 7. Operator decisions to collect in PR-0 (flagged, not assumed)

1. Authorize GDF as feed #3 at all (dated quote) + purchase/trial decision (blocked on U-1/U-2/U-3/U-5 sales answers).
2. `FeedStrategy::GDF` late-tick policy (Refold like Dhan vs Discard like Groww) + `drop_d1` policy.
3. Per-symbol subscribe vs StreamAllSymbols firehose+filter — the fork is now real (wire dialect captured, 10 §2); still gated on the U-1 RESIDUAL answers (endpoint, cadence, bandwidth, same-connection request support).
4. Plaintext-ws acceptance if U-2 answers "no TLS" (API key cleartext on the wire).
5. GDF historical API usage (default: forbidden — no KEEP row).
6. Whether `Feed::Dhan` survives as a dormant legacy variant if Dhan is removed concurrently (recommended: yes — `feed='dhan'` rows are SEBI-retained forever; GDF PRs must rebase on the post-removal `Feed` shape).
7. Maintenance-window handling at boot (02 §8 collision with the 08:30–08:45 window).

## What this prevents

- Code-first scope creep (the scope lock's edit-first protocol is the front door).
- A fourth table namespace (`gdf_*` tables are a REJECT class).
- An unbounded-trust identity scheme (namespaced token + fail-closed collision check).
