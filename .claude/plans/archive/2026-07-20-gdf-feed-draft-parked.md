> **PARKED 2026-07-20 (never started — resurrectable):** this DRAFT was moved
> out of the active set to honor the 5-active-plan cap (plan-gate) when the
> cadence-late-repull plan entered. No implementation exists for it; the
> future GDF session should move it back to
> `.claude/plans/active-plan-gdf-feed.md` before starting.

# Implementation Plan: GDF (Global Data Feeds) as feed #3 — trial-ready before the key arrives

**Status:** DRAFT
**Date:** 2026-07-13
**Approved by:** pending (Parthiban — operator quote 2026-07-13 recorded in
`gdf-third-feed-scope-2026-07-13.md` §0 authorizes the DESIGN; the operator has NOT yet
approved this plan — implementation PRs B–G may only start once this file's Status flips
to APPROVED per plan-enforcement.md / the design-first wall)
**Authority/rule file:** `.claude/rules/project/gdf-third-feed-scope-2026-07-13.md` (PR-A)
**Ground truth:** `docs/gdf-ref/` (15-file reference pack, merged PR #1493, 2026-07-13)
**Guarantee matrices:** every PR below carries the 15-row + 7-row matrices per
`.claude/rules/project/per-wave-guarantee-matrix.md` (cross-referenced, not duplicated
here; `per-item-guarantee-check.sh` enforces at PR time).
**Evidence tiers:** LIVE-DOC / SDK (CLIENT-LIB-SOURCE) / **[R#n]** (row n of the
reconciled master claims table in `docs/gdf-ref/README.md`) / **[U-n]**
(`docs/gdf-ref/99-UNKNOWNS.md`) / Assumed / Unknown — carried inline; nothing Unknown is
built as if Verified.

---

## Design

### D1. Position in the system
GDF = feed #3 under the pluggable-feed contract (`Feed::Gdf`, `feed='gdf'`, default OFF).
Crates touched follow the compiler-enumerated sweep in
`docs/gdf-ref/13-integration-proposal.md` §2 (the `Feed` enum is exhaustive-match by
design — adding the variant turns every 2-feed assumption into a compile error; the 8
production two-feed hardcode hotspots + the listed extras — `WsType::all()` arity, lane
spawn sites, chaos matrix, error codes — are enumerated there). All rows land in the SAME
shared tables with `feed='gdf'` and feed-in-key DEDUP; NO parallel tables. Coordination:
PR-B rebases on the post-Dhan-removal `Feed` shape (Dhan variant expected to survive
dormant for historical `feed='dhan'` rows — SEBI retention).

### D2. Protocol surface built (and ONLY this)
- Connect `ws://<endpoint>:<port>` (SSM), tokio-tungstenite 0.29 (already pinned; NO new
  deps expected), frame cap lifted (server sends huge frames; the official SDK uses 2^50
  [R#26]) [SDK].
- `Authenticate {"MessageType":"Authenticate","Password":<key>}` → `AuthenticateResult`
  [R#6; SDK verbatim; LIVE-DOC]. 500 ms post-auth settle (official JS-sample behavior —
  `docs/gdf-ref/02-authentication-and-connection.md`).
- Session bootstrap requests: `GetLimitation` (entitlement oracle [R#23]),
  `GetServerInfo`, `GetExchanges`, `GetInstruments` per enabled exchange (huge frame)
  [SDK].
- Mode A firehose: `StreamAllSymbols` — authenticate-only; short-key
  `{"T":"Batch","data":[{E,I,LTT,STT,ATP,BP,BQ,C,H,L,LTP,LTQ,O,OI,QL,SP,SQ,TTQ,VAL,PO,PC,PCP,OIC,T:"RT"}]}`
  [LIVE-DOC, `docs/gdf-ref/10-streaming-push-api.md` §2]. Whether it shares the WS-API
  endpoint/session budget: **Unknown** (day-0 probe, [U-1]).
- Mode B subscribe: `SubscribeRealtime` per symbol; long-key `RealtimeResult` rows
  (~1/sec/symbol full-image L1 [R#12]) [LIVE-DOC + SDK].
- `Echo` server heartbeat (ignore, feeds liveness) [R#10]. Plain-TEXT diagnostic frames
  tolerated (never a parser panic) [R#8].
- Post-close `GetHistory` MINUTE/1 for the bounded comparison set (rule file §3 grant row).
- NOTHING else (Greeks/OptionChain/ExchangeSnapshot/REST = REJECT class).

### D3. Identity & mapping
`security_id` = GDF `TokenNumber` (numeric exchange token, string in the master [R#29])
| bit 61 (GDF namespace; Groww index bit = 62, Dhan < 2^32 — disjoint). Missing/non-numeric
token (observed `TokenNumber:null` in one SnapshotResult sample —
`docs/gdf-ref/04-websocket-snapshot-bars.md`) → FNV-1a64 of `"<EXCH>:<IDENTIFIER>"`
truncated into the low bits | bit 60, with a build-time fail-closed collision check over
the day's master; raw `InstrumentIdentifier` always persisted in
`instrument_lifecycle.symbol_name` (feed='gdf') so every id is reversible (SEBI). Why not
hash-primary: token is vendor-stable + reversible + mirrors the Groww exchange_token
pattern; why not ISIN-join to another feed's ids: couples GDF boot to another vendor's
master and breaks for indices/futures (rejected in
`docs/gdf-ref/13-integration-proposal.md` §4). Cross-feed joins/presence/scoreboard by
ISIN (stocks), `canonicalize_index_symbol` (indices), `(underlying, expiry)` (futures) —
never native id, and NEVER the rolling `-I/-II/-III` aliases. Segment map: NSE→1,
NSE_IDX/BSE_IDX→0, NFO→2, BSE→4, BFO→8; CDS/MCX/BSE_DEBT excluded [R#3]. Watch file
`data/gdf/gdf-watch-<date>.json` mirrors the Groww watch-file pattern; index identifiers
are display names WITH spaces ("NIFTY 50") on NSE_IDX — NOT "NIFTY-I" (that is the
future) [R#28; `docs/gdf-ref/06-instruments-and-identity.md`].

### D4. FIREHOSE SIZING (measured math — the load-bearing capacity decision)
Row size MEASURED from the three LIVE-DOC batch rows in
`docs/gdf-ref/10-streaming-push-api.md` §2: 301/292/308 B → **~300 B/row** (+1 comma) —
NOT the earlier 215 B guess. Session = 6.25 h = 22,500 s. NFO enabled-universe estimate
60–100K instruments (Assumed; GetLimitation decides [R#22]).

| Scenario | rows/s | wire MB/s | raw GB/day | zstd (8–12×) GB/day | 5 trading days (zstd) |
|---|---|---|---|---|---|
| WORST: 100K syms × 1 row/s | 100,000 | 30.1 | 676 | 56–85 | 282–423 GB |
| 80K × 1 row/s | 80,000 | 24.1 | 541 | 45–68 | 226–338 GB |
| 60K × 1 row/s | 60,000 | 18.0 | 406 | 34–51 | 169–254 GB |
| LIKELY (conflated feed — only symbols with an update push; far-OTM wings mostly silent; fraction Assumed): 20% of 80K | 16,000 | 4.8 | 108 | ~9–14 | ~45–68 GB |
| Optimistic: 10% of 80K | 8,000 | 2.4 | 54 | ~4.5–7 | ~23–34 GB |

- zstd 8–12× on repetitive short-key JSON is Assumed (typical for this shape; measured on
  day 0 and the plan revised with the real ratio).
- **QuestDB verdict: full firehose live ingest is FORBIDDEN.** 8K–100K rows/s vs the
  ~5K ticks/s sustained design envelope (daily-universe §7 memory budget) — would wedge
  ILP + WAL apply. Only the TRACKED universe (~343 SIDs + operator extras, ≤ ~500) goes
  live to `ticks` (≤500 rows/s ≈ 7.7–11M rows/day — comparable to today's Dhan volume,
  inside envelope).
- **Full firehose → compressed NDJSON segment files on disk** (`data/gdf/firehose/`),
  zstd-framed hourly segments, durable + replayable (a later backfill/analysis can replay
  any symbol without having subscribed it). CPU: zstd-3 at ≤30 MB/s input is trivial on
  2 vCPU (zstd ≈ 500 MB/s/core). Network 18–30 MB/s worst = 144–241 Mbit/s — fine on
  r8g.large, but flag: sustained-worst is also a signal the vendor would throttle first.
- **DISK VERDICT: 30 GB EBS is INSUFFICIENT in every scenario** (even optimistic 5-day
  ≈ 23–34 GB compressed, on a volume already holding QuestDB). Options for the operator:
  (a) **RECOMMENDED — gp3 grows online: bump 30 → 150 GB for the trial week**
  (~$0.0912/GB-mo → +$10.9/mo pre-GST, prorated ≈ $2.7 for one week; revert after),
  covers the LIKELY band with headroom + 1-day worst-case; (b) daily S3 offload to
  `tv-prod-cold` + 1-day local retention (worst single day still 56–85 GB → needs ≥120 GB
  local anyway at full-worst); (c) run the trial capture on the Mac (Groww-scale Mac-first
  precedent) with AWS carrying only the tracked-universe lane. If the day-0 measured rate
  lands in the WORST band, cap the firehose capture to a configured exchange subset
  (e.g. NFO only) — config `feeds.gdf.firehose_exchanges`.

### D5. SSM parameters (ready BEFORE the key arrives)
- `/tickvault/<env>/gdf/endpoint` — String, `ws://host:port` (issued by GDF [R#4]).
- `/tickvault/<env>/gdf/access-key` — SecureString, read into `Secret<String>`, memory
  only, re-read (never cached past) an auth reject at ≥60s pacing (token-minter §2 class).
Operator one-liners (documented here so day 0 is copy-paste):
```
aws ssm put-parameter --name /tickvault/prod/gdf/endpoint  --type String       --value 'ws://HOST:PORT' --overwrite
aws ssm put-parameter --name /tickvault/prod/gdf/access-key --type SecureString --value 'THE_TRIAL_KEY'  --overwrite
```
IAM: instance role gains `ssm:GetParameter` on `/tickvault/<env>/gdf/*` (+kms:Decrypt) —
read-only, mirror of the Groww reader scoping.

### D6. Lane architecture
`gdf_activation.rs` watcher (mirror `groww_activation.rs`): level-triggered on
`feeds.gdf_enabled` → acquire `instance-lock-gdf` (fail-closed; 90s TTL; heartbeat) →
connect → Authenticate → GetLimitation/GetServerInfo (log entitlements verbatim, key
redacted) → master pull (or read today's cached master) → mode A firehose (or mode B
subscribe loop) → frames to WAL-at-receipt → parser (both dialects → the ONE internal
`GdfTick`) → tracked-filter → validator → shared pipeline (persist `feed='gdf'` +
aggregator instance) ∥ full-stream → zstd NDJSON segment writer. Supervised respawn
(WS-GAP-05 family), stall watchdog via `FeedHealthRegistry` (feed-agnostic
`should_restart_on_stall` already takes `Feed`). Maintenance windows 02:00–02:30 &
08:00–08:45 IST [R#39, Assumed; U-18] — the reconnect ladder treats in-window failures
as expected (no page storm); NOTE the 08:00–08:45 overlap with our boot window
(`docs/gdf-ref/02-authentication-and-connection.md` §8 operational flag).

---

## Plan Items (the PR series — serial, one at a time, All Green each)

- [ ] **PR-A (S) — scope rule + this plan (docs only; THIS PR).** Land the scope
  authorization rule file + this plan (Status stays DRAFT until the operator approves;
  the operator flips Status→APPROVED before PR-B may start).
  - Files: `.claude/rules/project/gdf-third-feed-scope-2026-07-13.md`,
    `.claude/plans/active-plan-gdf-feed.md`
  - Tests: n/a (docs) — plan-gate/plan-verify/per-item-guarantee-check pass; PLAN cap ≤5
    respected (5 active plans incl. this one)

- [ ] **PR-B (L) — `Feed::Gdf` + config + the compiler-enumerated sweep (+ companion
  rule-file edits).** Enum variant, `gdf_enabled` + `GdfConfig { mode,
  firehose_exchanges, extras }`, `WsType::GdfFeed`, and the 8 hotspots + extras from
  `docs/gdf-ref/13-integration-proposal.md` §2 (feed_state atomics + `FeedStatus`,
  `both_enabled`→`multi_enabled`, seal_routing arm + `drop_d1` decision (default: route,
  Groww-style), feed_consumer stage list (Gdf ⇒ persist+aggregate) + convergence guard,
  feed_lag_monitor `GDF_DAY_LAG_HIST` static + ring, `LAG_FLOOR_MS_GDF = 1000`,
  `ws_frame_spill.rs:371` feed-parameterization, tick_row_builder/seal label consts,
  `WsType::all()` 5→6, chaos_feed_state_matrix 3rd row placeholder). Companion docs in
  the SAME PR: `gdf-error-codes.md` runbook stub (GDF-01..GDF-06 — crossref-ready),
  `no-rest-except-live-feed-2026-06-27.md` §3 GDF rows + the GetHistory grant,
  third-feed banners in `websocket-connection-scope-lock.md` +
  `operator-charter-forever.md` §I.
  - Files: `crates/common/src/feed.rs`, `crates/common/src/config.rs`,
    `crates/common/src/ws_event_types.rs`, `crates/api/src/feed_state.rs`,
    `crates/app/src/seal_routing.rs`, `crates/core/src/pipeline/feed_consumer.rs`,
    `crates/core/src/pipeline/feed_lag_monitor.rs`, `crates/common/src/feed_blame.rs`,
    `crates/storage/src/ws_frame_spill.rs`, `crates/storage/src/tick_row_builder.rs`,
    `crates/app/src/feed_scoreboard_boot.rs`, `config/base.toml`,
    `.claude/rules/project/gdf-error-codes.md`,
    `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md`,
    `.claude/rules/project/websocket-connection-scope-lock.md`,
    `.claude/rules/project/operator-charter-forever.md`
  - Tests: `feed.rs` exhaustive-match/ALL/COUNT tests, `test_gdf_default_off`,
    `ws_event_types` label tests, `feed_consumer_convergence_guard.rs` update,
    `chaos_feed_state_matrix.rs` row; **workspace-wide test run** (crates/common touched)

- [ ] **PR-C (M) — instrument master + identity.** `GetInstruments` fetch/parse (huge-frame
  tolerant), TokenNumber-as-security_id | bit 61 + hash fallback | bit 60 + fail-closed
  collision check, segment map, `gdf-watch-<date>.json` builder from the daily universe
  (ISIN join for stocks, canonical index names, (underlying, expiry) futures),
  `instrument_lifecycle`/`instrument_lifecycle_audit` feed='gdf' rows (audit-first §24,
  `shared_master_writer` pattern), GDF-INSTR typed errors.
  - Files: `crates/core/src/feed/gdf/instruments.rs`, `crates/core/src/feed/gdf/identity.rs`,
    `crates/core/src/feed/gdf/mod.rs`, `crates/core/src/feed/gdf/shared_master_writer.rs`,
    `crates/common/src/error_code.rs`, `.claude/rules/project/gdf-error-codes.md`
  - Tests: identity unit+proptest (collision fail-closed, namespace-bit disjointness,
    null-token fallback), master-parse fixtures (detailed + sparse-era rows), watch-build
    join tests, lifecycle-row DEDUP/dry_run tests, error-code crossref green

- [ ] **PR-D (M) — native WS client (shadow, default-OFF).** tokio-tungstenite connect
  (lifted frame cap) → Authenticate → GetLimitation/ServerInfo → mode A (StreamAllSymbols:
  auth-only, parse `{"T":"Batch"}` short-key rows) AND mode B (SubscribeRealtime long-key
  rows) → ONE `GdfTick`; serde structs for BOTH dialects (all fields `Option`-al; text
  diagnostic frames → typed non-JSON arm; string bools; sci-notation floats); reconnect
  ladder (5s→60s, full re-auth+re-subscribe) with the "key already in use" reject arm
  (back off, page once, NEVER storm — the peer holds the key); `instance-lock-gdf` SSM
  named lock gate + heartbeat; SSM key read (`Secret<String>`, re-read on auth reject
  ≥60s pacing); Echo→liveness stamp; shadow NDJSON capture only (no shared-table writes —
  the Groww §35 PR-R1 shape). Plaintext-ws risk note in code + rule file.
  - Files: `crates/core/src/feed/gdf/client.rs`, `crates/core/src/feed/gdf/protocol.rs`
    (both dialects), `crates/core/src/feed/gdf/session.rs`, `crates/app/src/gdf_shadow.rs`,
    `crates/core/src/auth/secret_manager.rs` (`fetch_gdf_access_key`),
    `crates/app/src/gdf_lock.rs` (named-lock reuse), `crates/common/src/constants.rs`
  - Tests: dialect decode fixtures (LIVE-DOC verbatim rows incl. LTQ:0 + zero-value
    quoted-only rows), long-key RealtimeResult fixture, hostile-input fuzz-ish unit set
    (text frames, truncated JSON, huge frame), reconnect/backoff pure-fn tests, lock
    gate tests, no-key-in-logs ratchet

- [ ] **PR-E (L) — capture lane (promote shadow → live).** WAL-at-receipt
  (`data/spill/gdf/`, feed-param ws_frame_spill, boot replay), tracked-filter →
  validator → `ticks` feed='gdf' + `MultiTfAggregator` GDF instance
  (`FeedStrategy::GDF` = Refold default, own catch-up margin + driver, seal routing arm
  live), FULL-firehose zstd NDJSON segment writer (`data/gdf/firehose/*.ndjson.zst`,
  hourly rotation, retention sweeper + disk-free guard), `gdf_activation.rs` watcher +
  supervised bridge, ws_event_audit `WsType::GdfFeed` lifecycle rows.
  - Files: `crates/app/src/gdf_activation.rs`, `crates/app/src/gdf_bridge.rs`,
    `crates/storage/src/ws_frame_spill.rs`, `crates/core/src/feed/gdf/firehose_writer.rs`,
    `crates/trading/src/candles/` (FeedStrategy::GDF), `crates/app/src/main.rs`,
    `crates/app/src/seal_routing.rs`
  - Tests: WAL replay idempotency (DEDUP), aggregator GDF-instance seal tests,
    firehose-writer rotation/retention/disk-guard tests, DHAT zero-alloc on the per-row
    hot parse path + Criterion budget entry, activation watcher tests

- [ ] **PR-F (M) — trial measurement + observability.** Per-symbol coverage counters
  (rows/sym/day, firehose batch cadence + batch-size histogram), STT−LTT and
  receipt−LTT latency histograms (lag floor 1000ms), daily 15:50 GetHistory 1m
  cross-verify: GDF-official vs our GDF-built `candles_1m` (exact-compare, audit table +
  CSV, cross-verify-1m pattern) AND GDF-vs-Groww on shared surfaces (scoreboard three-way
  columns), daily Telegram trial report (coverage %, latency p50/p99, batches/s, disk
  used, mismatches), GDF-01..06 emit sites + runbook flesh-out, feed-health `set_subscribed`,
  scoreboard lag/blame/presence third feed, `aws-budget.md` cost note for any new CW metric.
  - Files: `crates/app/src/gdf_trial_report.rs`, `crates/app/src/gdf_crossverify_boot.rs`,
    `crates/storage/src/gdf_crossverify_audit_persistence.rs` (AUDIT table — allowed;
    not a data table), `crates/common/src/feed_blame.rs`, `crates/core/src/pipeline/
    feed_presence.rs` registration site, `.claude/rules/project/gdf-error-codes.md`,
    `deploy/aws/terraform/` (only if a metric is promoted; dated cost note)
  - Tests: comparer pure-fn tests (paise-exact, missing_live/missing_backtest,
    tail-unsealed), report formatting (10 Telegram commandments litmus), counter presence
    ratchets, crossref/tag-guard green

- [ ] **PR-G (S–M) — e2e/chaos + dry-run rehearsal.** Mock GDF server (tokio test
  fixture speaking BOTH dialects + Echo + text diagnostics + "key already in use" +
  mid-stream disconnect), full-lane e2e (mock → WAL → ticks feed='gdf' → 1m seal),
  chaos: disconnect storms, huge-frame, firehose burst (100K rows/s synthetic) proving
  tracked-filter + disk writer keep up and QuestDB stays inside envelope, 3-feed
  coexistence guard, doctor/MCP surface, dry-run rehearsal script for day 0.
  - Files: `crates/core/tests/gdf_mock_server.rs` (fixture),
    `crates/app/tests/gdf_live_pipeline_e2e.rs`, `crates/storage/tests/chaos_gdf_firehose_burst.rs`,
    `crates/app/tests/chaos_feed_state_matrix.rs`, `scripts/gdf-trial-rehearsal.sh`
  - Tests: the files ARE the tests; plus `make doctor` section extension

---

## Edge Cases

- Quoted-only batch rows (ATP/H/L/O/TTQ/VAL all 0, LTP 0 or stale) [LIVE-DOC,
  `docs/gdf-ref/10-streaming-push-api.md` §2 observed behaviors] — validator must not
  reject-as-junk a legitimate quote refresh; LTP 0 rows are quote-only (no trade print)
  → update bid/ask state, do NOT emit a 0-price trade tick.
- `LTQ:0` on traded instruments (quote-refresh second) [LIVE-DOC ARITH] — accepted.
- `Close` dual semantics: prev-day close in realtime/quote rows; BAR close in
  Snapshot/History rows [R#14] — two distinct struct fields, never one.
- 2018-era rows missing PriceChange/OIChange; schema growth → every field `Option`-al
  [R#16].
- Plain-text diagnostic frames ("Reached instrument limitation", expired-key text) —
  parser returns a typed NonJson arm, counted, never panics [R#8; full catalog unknown,
  U-10].
- Huge single frames (full GetInstruments dump) — frame cap lifted [R#26]; bounded by a
  hard own-cap (e.g. 256 MiB) + streaming parse to avoid RAM spikes [SDK ws.py].
- "Access Denied. Key already in use by other session." — 1-session lock; the reject arm
  backs off (never storms — reconnecting would kill the legitimate peer session) [R#7;
  wire envelope unknown, U-11].
- PreOpen:true in-band ticks [R#43] — session-gated exactly like Dhan pre-open (day-OHLC
  purity); pre-open field semantics unknown [U-15].
- Whole-second stamps → two ticks same second same symbol: capture_seq keeps rows unique
  (DEDUP key `(ts, security_id, segment, capture_seq, feed)`).
- Maintenance windows 02:00–02:30 / 08:00–08:45 IST [R#39, Assumed; U-18] — reconnect
  ladder in-window = expected, no page.
- Lot size drift (NIFTY QuotationLot changes across eras) — never hardcode.
- Trial key entitlement narrower than expected (e.g. 200 syms/exchange cap like the 2018
  key [R#24]) — GetLimitation-first; firehose may effectively stream a subset; report
  honestly.
- StreamAllSymbols not entitled / endpoint different [U-1] — mode B fallback
  (SubscribeRealtime over the tracked universe only), report the gap;
  GetExchangeSnapshot NOT built (REJECT).
- IST-midnight rollover: firehose segments rotate; watch file rebuilt per date; watermark
  reset per existing aggregator contract.

## Failure Modes

| # | Failure | Detect | Absorb/Recover | Page |
|---|---|---|---|---|
| 1 | Endpoint unreachable / DNS | connect Err | ladder 5s→60s, lock held | GDF-01 after threshold |
| 2 | Auth reject (bad/expired trial key) | AuthenticateResult Complete:false or text frame | re-read SSM ≥60s pacing; never cache past reject | GDF-02 edge-latched |
| 3 | Key-in-use (peer session) | reject text | back off (5 min), lock audit, never storm | GDF-02 (reason=key_in_use), once |
| 4 | Silent socket (Echo stops, no rows in-session) | FeedHealthRegistry last-tick + Echo stamp | stall watchdog kill+reconnect (FEED-STALL family) | existing stall pagers |
| 5 | Firehose burst > disk write rate | writer channel depth + disk-free guard | drop-to-DLQ ONLY for the untracked stream (tracked lane isolated); counted | GDF-03 |
| 6 | Disk full (firehose) | RESOURCE-03 / disk guard | stop firehose writer FIRST (tracked lane + QuestDB protected), counted | GDF-03 critical arm |
| 7 | QuestDB down | existing BOOT-01/02 + ring→spill→DLQ | unchanged chain absorbs tracked lane | existing |
| 8 | Parse drift (vendor schema change) | per-dialect decode-error counters | skip+count, WAL retains raw frame for replay after fix | GDF-04 on sustained rate |
| 9 | Master pull fails at boot | typed GDF-INSTR errors | bounded retry + degrade (yesterday's watch file with loud flag — trial only; NOT the §4 infinite-retry contract, GDF is not the trading feed) | GDF-05 |
| 10 | GetHistory cross-verify degraded | fetch/persist error arms | partial coverage stamped, never false-OK (Rule 11) | GDF-06 |
| 11 | Lane task panic | supervised respawn (WS-GAP-05 family) | respawn + counter; release panic=abort honesty note | FEED-SUPERVISOR-01 |
| 12 | Lock lost mid-run (SSM) | heartbeat not-owned | page + lane teardown (peer owns the key) | GDF-02 |

## Test Plan

Per `testing-scope.md`: PR-B touches `crates/common` → workspace-wide; others scoped.
Categories (of the 22) engaged: unit (dialect decode, identity, comparer, backoff),
property (identity collision, decoder never-panics on arbitrary bytes/JSON), integration
(mock-server e2e, WAL replay, aggregator seal), DHAT (per-row parse path zero-alloc
budget), Criterion (parse + tracked-filter budgets in `quality/benchmark-budgets.toml`),
chaos (firehose burst, disconnect storm, disk-full firehose), ratchets (default-OFF pin,
no-key-in-logs, feed-in-key DEDUP meta-guard auto-covers, convergence guard, error-code
crossref/tag-guard), guard-test updates listed per PR above. Mock GDF server (PR-G) is
the trial rehearsal: both dialects, Echo, text diagnostics, key-in-use, disconnects.

## Rollback

Config-only: `feeds.gdf_enabled = false` (or `POST /api/feeds/gdf {enabled:false}`,
bearer-gated) restores byte-identical non-GDF behavior — the lane never spawns; shared
tables simply gain no new `feed='gdf'` rows (existing rows retained per SEBI, never
deleted). Each PR is independently revertable (serial merges, no cross-PR runtime
coupling until PR-E; PR-E's activation watcher is the single spawn site — reverting it
de-animates PR-C/D code). Firehose disk: stop writer → segments are inert files; delete
after operator sign-off. EBS bump reverts by gp3 shrink-via-new-volume (or simply lapses
— cost note in Design D4).

## Observability

7-layer per new path (wave-4 preamble §4): counters
(`tv_gdf_rows_total{dialect,tracked}`, `tv_gdf_batches_total` + size histogram,
`tv_gdf_decode_errors_total{kind}`, `tv_gdf_firehose_bytes_total{stage=raw|zstd}`,
`tv_gdf_reconnects_total{reason}`, lock counters), gauges (feed-health subscribed counts,
firehose disk used, `tv_gdf_lag_p99_seconds` day gauge), tracing spans, `error!` with
`code=GDF-0x` (tag-guard), Telegram (typed events: trial daily report Info; auth/entitle
High; disk Critical arm), audit tables (`ws_event_audit` WsType::GdfFeed lifecycle rows;
`gdf_crossverify` audit rows; lifecycle feed='gdf'), scoreboard third-feed columns +
presence slots. ErrorCode family GDF-01..GDF-06 lands WITH `gdf-error-codes.md` (crossref
test enforced same-PR). Delivery boundary stated honestly: GDF-0x are log-sink +
typed-Telegram; CloudWatch filter promotion = dated cost note only if the trial converts.

## Scenarios

| # | Scenario | Expected |
|---|---|---|
| 1 | Key arrives, params set, flip ON | Day-0 runbook end-to-end green, first Telegram report same evening |
| 2 | StreamAllSymbols entitled, 16K rows/s | tracked lane live, firehose ~5 MB/s→zstd disk, no QuestDB pressure |
| 3 | StreamAllSymbols NOT entitled | mode B fallback on tracked universe; gap reported honestly |
| 4 | Trial key capped at 200 syms | GetLimitation logged; coverage report shows the cap; no false-OK |
| 5 | Second session opened by mistake (laptop) | lock refuses locally; if external, key-in-use arm backs off + one page |
| 6 | Disk approaches full mid-firehose | firehose writer stops first, tracked lane + QuestDB unaffected, Critical page |
| 7 | Vendor renames a JSON key | decode-error counter + WAL raw retention; fix + replay |
| 8 | Trial ends (key expires) | auth-reject arm pages once, lane idles; operator flips OFF |

---

## TRIAL-DAY-0 RUNBOOK (execute in order when GDF enables the key)

1. `aws ssm put-parameter … /tickvault/prod/gdf/endpoint` + `…/gdf/access-key` (D5 one-liners).
2. (If AWS capture) confirm EBS headroom per D4 verdict — bump to 150 GB gp3 BEFORE flip, or run day 0 on the Mac.
3. Flip ON: `POST /api/feeds/gdf {"enabled":true}` (bearer) or `feeds.gdf_enabled=true` + restart.
4. **GetLimitation FIRST** — the lane logs the full entitlement matrix (AllowedFunctions, per-exchange AllowedInstruments + DataDelay, AllowedCallsPerHour/Month, Max EOD/Intraday/Ticks, ExpirationDate) [R#23]. Record verbatim (key redacted) into the trial log + Telegram. This answers most Unknowns before a single tick.
5. `GetExchanges` + `GetInstruments` per enabled exchange → master persisted, watch file built, identity collision check green.
6. Connect firehose (mode A). If not entitled / unknown endpoint → mode B on the tracked universe.
7. Verify capture within 5 min: `questdb_sql "select count(*) from ticks where feed='gdf' and ts > dateadd('m',-5,now())"`; firehose segment files growing; `make doctor` green; ws_event_audit Connected row present.
8. Evening: first daily Telegram trial report + 15:50 GetHistory cross-verify ran; eyeball CSV.
9. File the live-probe answers (below) as dated notes in the rule file / plan.

**Live-probe checklist (the residual Unknowns — answer each with evidence on day 0/1; U-n = `docs/gdf-ref/99-UNKNOWNS.md`):**
- [ ] Streaming endpoint: same host:port as WS API? separate? counts against the 1-session budget? [U-1]
- [ ] Firehose cadence reality: rows only for updated symbols, or all symbols every second? measured rows/s + batch sizes + batches/s → revise D4 with real numbers. [U-1/U-17]
- [ ] zstd ratio measured on real segments (revise disk plan).
- [ ] Minute-bar stamp open-vs-close (one SubscribeSnapshot/GetSnapshot probe vs wall clock) [U-6 — Assumed open].
- [ ] STT−LTT distribution + receipt latency under burst (is 1s cadence honored?) [U-17].
- [ ] TLS: does the issued endpoint accept `wss://`? (ask sales in writing too) [U-2].
- [ ] Diagnostic frame catalog: capture verbatim every text frame seen [U-10].
- [ ] Echo cadence + payload shape [U-12].
- [ ] Unsubscribe ack shape (mode B); subscribe-reject shape at symbol-cap [U-13/U-14].
- [ ] GetLimitation values for THE trial key (symbol caps, history depths, function flags incl. StreamAllSymbols/GetHistory enablement) [U-9].
- [ ] TokenNumber presence for NSE_IDX indices (identity fallback exercised or not) [U-21].
- [ ] Epoch-UTC confirm per entitled exchange (NSE/NSE_IDX/NFO/BSE/BFO) [U-23].
- [ ] Trial duration + non-display/algo entitlement — ask GDF in writing [U-3/U-5].
- [ ] Maintenance windows confirmation (02:00–02:30 / 08:00–08:45 IST) [U-18, Assumed].

---

## Open decisions for the operator (defaults applied if silent)

1. Firehose disk: EBS bump 30→150 GB (default) vs Mac-first trial vs S3-offload.
2. `FeedStrategy::GDF` late-tick policy: Refold (default, Dhan-class) vs Discard.
3. `drop_d1`: route D1 like Groww (default) vs drop like Dhan.
4. Tracked-universe extras beyond the ~343 daily-universe SIDs (name symbols, ≤500 total).
5. Whether `Feed::Dhan` survives dormant post-removal (recommended; coordinate with the parallel session).
6. Ask GDF sales in writing: wss availability, StreamAllSymbols entitlement on trial, trial duration, non-display use [U-1/U-2/U-3/U-5].
