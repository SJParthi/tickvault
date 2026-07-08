# Implementation Plan: Telegram UX Overhaul — Episode Live-Edit Coalescing (One Incident = One Bubble)

**Status:** VERIFIED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator directive 2026-07-06)
**Changed crates:** core (`crates/core`), app (`crates/app`), common (test ratchets in `crates/common`) — plus terraform (`deploy/aws/terraform/`) and the operator-portal Lambda (`deploy/aws/lambda/operator-control/handler.py`).

> **Judge verdict (3-lens adversarial plan bake-off):** WINNER BASE = purity-and-ratchet-first
> (total pure FSM + proptest never-drop pin + pure classifiers/renders/ceilings, reuse of
> TELEGRAM-01), GRAFTED WITH robustness-lens restart-survival hardening (rehydrate age bound,
> shutdown_flush, Recovering stability phase, market-close force-drain, ALARM-never-suppressed
> Lambda warm cache, duplicate-over-drop ladder) and mvp-lens diff discipline (exactly ONE new
> ErrorCode instead of two, reopen-window flap fold, Makefile-wired Lambda tests, no new CI job).
>
> **Guarantee matrices:** every item below carries — by cross-reference — the mandatory
> 15-row "100% everything" matrix + the 7-row "Resilience demand" matrix from
> `.claude/rules/project/per-wave-guarantee-matrix.md` (see the "Per-Item Guarantee Matrix"
> section at the bottom, which instantiates both matrices for this plan).

**Touched crates / paths (design-first-wall binding):**
- `crates/core` — `crates/core/src/notification/episode.rs` (NEW), `crates/core/src/notification/service.rs`, `crates/core/src/notification/coalescer.rs`, `crates/core/src/notification/events.rs`, `crates/core/src/notification/mod.rs`, `crates/core/tests/dhat_telegram_dispatcher.rs`, `crates/core/tests/episode_edit_wiring_guard.rs` (NEW), `crates/core/tests/telegram_lambda_house_style_guard.rs` (NEW)
- `crates/common` — `crates/common/src/error_code.rs` (Telegram03EpisodeDegraded), `crates/common/src/config.rs` (NotificationConfig knobs)
- `crates/app` — `crates/app/src/main.rs` (shutdown_flush wiring + rehydrate at boot)
- `deploy/aws/lambda/telegram-webhook` — `handler.py` (house-style formatter), `test_handler.py` (6 new tests)
- `config/base.toml`, `Makefile`, `.github/workflows/ci.yml` (one step in the EXISTING Repo Guards job), `.claude/rules/project/wave-3-error-codes.md` (TELEGRAM-03 section append)

- [x] Item 1 — Retune `tick_gap_instruments_silent` IN PLACE (same resource address, no state churn): threshold 100 → 25 (fires ≥26; incident floor 29/min, peak 67 → pages within 10–12 min), period=60, statistic=Maximum, M-of-N 10-of-12 (`evaluation_periods=12`, `datapoints_to_alarm=10`), `treat_missing_data=notBreaching`, ABSOLUTE threshold (no %-of-universe denominator exists in CW; universe envelope-locked [100,1200] so 25 = 3.2% today / 2.1% at cap; dated revisit comment). Market-hours gate: `actions_enabled=false` + append alarm name to the window-gate Lambda `ALARM_NAMES` join (gauge is set only in-session, so the last in-session value keeps being scraped 15:30→16:30). Update stale alarm_description ("> 100" → "> 25 sustained ≥10 of 12 min in-market; WS-GAP-06 runbook") + the hardcoded alarm-count/cost text in the output block.
  - Files: deploy/aws/terraform/app-alarms.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf
  - Tests: test_tick_gap_silent_alarm_threshold_is_twenty_five, test_tick_gap_silent_alarm_is_window_gated
  - Impl: deploy/aws/terraform/app-alarms.tf `resource "aws_cloudwatch_metric_alarm" "tick_gap_instruments_silent"` (same resource address, threshold 25, 10-of-12, actions_enabled=false) + market-hours-liveness-alarm.tf ALARM_NAMES join + output-block cost text

- [x] Item 2 — NEW alarm `realtime_guarantee_degraded` in NEW FILE `silent-feed-alarms.tf`: metric `tv_realtime_guarantee_score` (ALREADY CW-exported — zero Rust, zero allowlist change), `LessThanThreshold` 0.95 (== `SLO_WARN_THRESHOLD`; score==0.95 is Healthy and correctly non-breaching), period=60, statistic=Minimum (1 datapoint/60s scrape today so Min==Max; Minimum is future-proof and mirrors the UNTOUCHED sibling `realtime_guarantee_critical` at <0.80), M-of-N 9-of-15 (round-2 correction 2026-07-07, review finding 1 — the first cut's 12-of-15 justification "incident tick_freshness 0.914–0.963 satisfies it" was WRONG: with universe 776, freshness breaches <0.95 only at ≥39 silent, so the incident's 29–38-silent minutes SAMPLE Healthy on the once-per-60s point scrape and 12-of-15 could fail to latch on the very incident; strict 15/15 would never latch on the 125-crossing oscillation; 9-of-15 latches when ≥60% of sampled minutes breach while a 2–3 min single-reconnect dip (≤3 breaching points) cannot reach 9; the 26–38-silent sub-band is independently paged by the retuned tick-gap alarm; M=9 is threshold-adjacent PROVISIONAL — one-trading-week observation mandated), `treat_missing_data=notBreaching`, `actions_enabled=false` + gate-Lambda `ALARM_NAMES` append (publisher runs 24/7). `ok_actions=[SNS tv_alerts]` for the falling-edge recovery note; edge-trigger is CloudWatch-native (Rule 4). Name/description say "degraded (Medium)" — no "critical" token. Same PR: fix the stale main.rs comment citing the retired Prometheus "tv-realtime-score-degraded" alert — point it at this alarm. Medium-tier Telegram VISUAL rendering is a flagged follow-up (telegram-webhook handler.py stamps the same emoji on any ALARM).
  - Files: deploy/aws/terraform/silent-feed-alarms.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, crates/app/src/main.rs
  - Tests: test_realtime_guarantee_degraded_alarm_threshold_matches_slo_warn, test_silent_feed_alarms_are_window_gated
  - Impl: deploy/aws/terraform/silent-feed-alarms.tf `realtime_guarantee_degraded` (<0.95 Minimum 9-of-15, ok_actions, no "critical" token) + main.rs stale-comment fix (now cites tv-prod-realtime-guarantee-degraded)

- [x] Item 3 — BOUNDARY-01 catch-up storm alarm, two parts. EXPORT: add a SECOND byte-identical `emf_processor` metric_declaration entry to BOTH `deploy/aws/cloudwatch-agent.json` and `deploy/aws/terraform/user-data.sh.tftpl`: `{"source_labels":["host"],"label_matcher":"^tickvault-prod$","dimensions":[["host","feed"]],"metric_selectors":["^tv_boundary_catchup_total$"]}` — per-feed dimension is Rule-11-MANDATORY (Groww's 60s catch-up margin makes catch-up sealing its ROUTINE steady-state for quiet SIDs; host-only folding would mask a Dhan storm under the Groww baseline or page on healthy Groww). Zero Rust — both emit sites exist (main.rs dhan driver, groww_bridge.rs groww driver). Cost 2 series ~$0.60/mo, dated comment + aws-budget.md note. ALARM in `silent-feed-alarms.tf`: `boundary_catchup_storm_dhan`, metric `tv_boundary_catchup_total`, dimensions = explicit map `{ host = "tickvault-prod", feed = "dhan" }` (NOT `local.app_dimensions`), statistic=Sum, period=300 (CW agent ships counters as per-scrape DELTAS ⇒ Sum/300s IS increase(5m), Rule 12 — house precedent `tv_disk_watcher_respawn_total`), `GreaterThanThreshold` 2000 (incident 9k–11.5k/10min ≈ 4.5–5.75k/5min = 2.25–2.9× threshold → pages at minute 10; PROVISIONAL in alarm_description — healthy Dhan floor UNMEASURED, mandate 1 trading-week soak of the exported per-feed Sum(5m) distribution + dated ratchet note), 2-of-2 × 5min (a bounded one-shot restart catch-up wave ≤~25K drains inside one 5-min period → absorbed; the 08:30 boot wave sits outside the 09:20–15:35 gate), `notBreaching`, `actions_enabled=false` + gate append. Ratchets: EMF name pin bump + teach the two-file byte-equality + alarm-metric-superset tests the second-declaration SHAPE (extend, never weaken).
  - Files: deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/user-data.sh.tftpl, deploy/aws/terraform/silent-feed-alarms.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, crates/common/tests/cloudwatch_app_alarms_wiring.rs, .claude/rules/project/aws-budget.md
  - Tests: test_emf_metric_selectors_name_count_is_twenty_three, test_reference_cloudwatch_agent_json_matches_deployed_emf_declaration, test_boundary_catchup_alarm_uses_per_feed_dimensions
  - Impl: second [["host","feed"]] emf metric_declaration in cloudwatch-agent.json + user-data.sh.tftpl (byte-identical); silent-feed-alarms.tf `boundary_catchup_storm_dhan` (explicit {host,feed=dhan} dims, Sum/300s, PROVISIONAL 2000, 2-of-2); ratchets: EMF pin 21->23, alarm-count 17->20 (scan extended to silent-feed-alarms.tf), test_second_emf_declaration_publishes_boundary_catchup_per_feed (round-2 hardened per review finding 2: new `emf_declaration_objects` brace-parser binds selector↔dimensions WITHIN one declaration object + exactly-one-declaration check — the previous three independent whole-file substrings were bypassable by a multi-declaration reshuffle, verified by mutation); aws-budget.md dated cost note

- [x] Item 4 — THE ONLY RUST ITEM: new gauge `tv_dhan_exchange_lag_p99_seconds` (unlabeled, dhan-only NAME — sidesteps the host-only EMF dimension label-folding trap; a future Groww gauge gets its own name). HOT PATH (tick_processor.rs dhan persist site, `received_at_nanos` + `capture_seq` in scope — verify at impl time): `lag_ns = received_at_nanos.saturating_sub(exchange_ts × 1e9)`, clamp negatives to 0 (count clamps), write into a preallocated single-writer 32,768-slot ring with relaxed atomic head (~65s headroom at 500 ticks/s) — O(1), zero-alloc, DHAT + Criterion ratcheted + benchmark-budgets entry. REPLAY EXCLUSION — TWO-condition discriminator (round-2 fix 2026-07-07, review finding 3 — the dwell ALONE misclassified live pipeline-delayed ticks as replay: received_at is stamped at DEQUEUE, so receipt−capture measures in-channel dwell for LIVE frames too; the frame channel holds ~4 min at ~500 ticks/s and an ILP stall >60s would have excluded EVERY live tick, starved the window below MIN_LAG_SAMPLES, and frozen the gauge at the last healthy value — a stale-healthy false-OK during the exact lag class the alarm exists for): exclude ONLY when `received_at_nanos − capture_seq ≥ REPLAY_EXCLUDE_DWELL_NANOS = 60_000_000_000` (named constant) AND `capture_seq < live_boundary` (stamped once at ring init; a boot-time WAL-replayed row was captured by a PREVIOUS process so it always predates the boundary — the tick processor spawns before the reinject await, and live captures happen after the WS pool spawns, i.e. after the boundary). capture_seq is stamped once at the original WS-read instant and PRESERVED through WAL re-injection while received_at is RE-stamped at dequeue, so a replayed row shows receipt−capture = downtime (≥minutes, pre-boundary, excluded EXACTLY) while genuinely-lagged live rows (the incident's real 46s/199s AND >60s stall-delayed live rows) are KEPT — no Rule-11 censoring of the measured signal. Honest residuals: a RUNTIME D2b lane cold-start reinject (same-process captures) is admitted → bounded transient contamination (drain + 60s window, cannot latch 10-of-10); boundary clock-read failure (0) fails OPEN (nothing excluded). Every exclusion increments `tv_dhan_lag_samples_excluded_total` (CloudWatch-exported — in the 23-name EMF allowlist, ~$0.30/mo; visible, never silent. Round-1 correction: earlier drafts said "/metrics-only" — the shipped allowlists + cost notes + EMF ratchet export it, and the docs now match the deploy). PUBLISHER: supervised 10s cold task (WS-GAP-05/SLO-03 respawn pattern) in NEW module `crates/core/src/pipeline/feed_lag_monitor.rs`, spawned ONCE per process from BOTH boot arms (round-1 fix, findings 1/6/9: the FAST crash-recovery arm returns via `run_shutdown_fast` and never reaches `start_dhan_lane` — lane-only wiring left the gauge silently dark after any mid-market crash restart; both sites share the once-per-process guard, pinned by the 2-call-site ratchet): snapshot trailing-60s window into preallocated scratch, p99 via `select_nth_unstable` — honestly labeled **O(N-window)**, N≤32,768, off the tick thread (NEVER claimed O(1)). Publish ONLY when (a) inside a trading-session window — the regular [09:00,15:30) IST persist window OR the Muhurat [18:00,19:30) window when the boot flag is set (round-1 fix, finding 5: mirrors the persist sites' `is_within_persist_window(_, muhurat_active)`; Muhurat coverage is gauge-visibility only — the CW window-gate Lambda still enables actions 09:20–15:35 only; §30 window-exempt always-on SIDs can put off-session samples in the RING, the gate only controls PUBLISHING) — (Rule 3, prevents the stale-gauge-after-close artifact) AND (b) window holds ≥ `MIN_LAG_SAMPLES = 50` (empty/thin window publishes NOTHING — 0 = "perfect lag" is a Rule-11 false-OK; feed-dead is owned by the silent-instruments + WS alarms via notBreaching). QUANTIZATION HONESTY (module doc + alarm_description + metrics catalog + portal caveat): Dhan LTT is u32 whole IST SECONDS → ≥1s floor; healthy p99 reads ~1–2s and can never read 0; sub-second wire lag UNMEASURABLE for feed=dhan; alarm threshold sits 10× above the floor. EXPORT: add the gauge to both allowlist regexes (+$0.30/mo, part of the 21→23 pin bump). ALARM (`silent-feed-alarms.tf`): `dhan_exchange_lag_p99_high` — `GreaterThanThreshold` 10 (seconds), period=60, statistic=Maximum, strict 10-of-10 (safe HERE unlike items 1/2: the metric is itself a trailing-60s p99 recomputed every 10s, so a one-burst transient decays out within ~60s and cannot hold 10 consecutive breaching minutes; incident all-day p99 46s pages at minute 10), `notBreaching`, `actions_enabled=false` + gate.
  - Files: crates/core/src/pipeline/feed_lag_monitor.rs, crates/core/src/pipeline/tick_processor.rs, crates/core/src/pipeline/mod.rs, crates/app/src/main.rs, deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/user-data.sh.tftpl, deploy/aws/terraform/silent-feed-alarms.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, quality/benchmark-budgets.toml, crates/core/tests/dhat_feed_lag_ring.rs
  - Tests: test_replay_dwell_boundary_excludes_at_exactly_60s, test_live_stall_dwell_over_60s_post_boundary_capture_is_kept, test_zero_boundary_fails_open_never_excludes, test_thin_window_publishes_nothing, test_compute_window_p99_ns_on_known_distribution, test_ring_wraparound, test_negative_lag_clamped, test_out_of_session_publishes_nothing
  - Impl: crates/core/src/pipeline/feed_lag_monitor.rs (32,768-slot single-writer ring, strict-< 60s replay dwell, IST clock alignment, p99 via select_nth_unstable labeled O(N-window), Muhurat-aware session gate); tick_processor.rs record_dhan_tick at BOTH Dhan persist sites; main.rs spawn_supervised_feed_lag_publisher at BOTH boot arms (fast crash-recovery + start_dhan_lane, once-per-process guard); silent-feed-alarms.tf `dhan_exchange_lag_p99_high` (>10s, 10-of-10); dhat_feed_lag_ring.rs + benches/feed_lag_ring.rs + benchmark-budgets.toml entries; secret_manager.rs wiring ratchet (2-call-site pin); metrics_catalog.rs doc registration

- [x] Item 5 — Rewrite `_pctl_feed_sql` + the feed-discovery query in the operator-portal Lambda with the two-layer predicate + IST-now correction. VERIFIED LATENT BUG folded in: `ts` and `received_at` are stored IST-SHIFTED while QuestDB `now()` is UTC → the current `ts > dateadd('m',-10,now())` is really a ~5h40m window (root cause of the all-day replay conflation), AND an exchange-time window LAG-CENSORS (any row whose lag exceeds the window has ts outside it — the panel structurally cannot show lag above its own window, Rule-11 false-OK). All predicates use `IST_now := dateadd('m', 330, now())`, population defined by RECEIVE time. Exact new inner WHERE (per feed $f): `feed = '$f' AND received_at != null AND ts > dateadd('h', -6, IST_now) AND received_at >= dateadd('s', -120, IST_now) AND received_at < dateadd('s', 60, cast(capture_seq / 1000 + 19800000000 as timestamp))`. Roles: (1) K=120s receive-recency — honest population "rows received in the last 2 minutes" (6K–60K rows at incident rates → stable p99); (2) EXACT replay excluder (capture_seq nanos → micros → +19800000000µs IST alignment): frame-WAL re-injection re-stamps received_at but preserves capture_seq, so replayed rows show receipt−capture = downtime and are excluded EVEN INSIDE the drain window; genuinely-lagged live rows (46s/199s) pass; groww never excluded by construction (sidecar capture_ns predates bridge capture_seq → receipt−capture ≤ 0); TVW2 spill-drain rows preserve original received_at (correctly kept); (3) 6h ts bound = partition-pruning ONLY (`ticks` is PARTITION BY HOUR on ts; received_at is not the designated timestamp — without it, full-table scan); the implied >6h-lag ceiling is documented. Rule-11 additions: (a) companion cheap count of the excluded complement per feed, rendered in the panel ("N replay-restamped rows excluded"); (b) rows==0 renders "no rows received in last 120s" — never zero-valued percentiles; (c) extend the existing whole-second caveat block with the 6h ceiling + replay-exclusion + ≥1s Dhan LTT floor. Keep `received_at != null` (Groww NULL receipt legitimate) and `ORDER BY ts DESC LIMIT 50000`. Feed-discovery query gets the same IST fix with the 6h window. Live verification best-effort, non-blocking: `select now()` should confirm UTC; if the box returns IST, drop the `dateadd('m',330)` terms — nothing else changes; default: ship WITH the +330m shift.
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: N/A — Lambda Python; verified by live portal render + the QuestDB `select now()` timezone probe (documented in Test Plan)
  - Impl: deploy/aws/lambda/operator-control/handler.py `_pctl_feed_sql` + feed-discovery query (IST_now = dateadd('m',330,now()), K=120s receive-recency, exact capture_seq replay excluder, 6h ts partition-prune, replay-excluded count rendered, zero-row honesty line, extended caveat block); round-2 honesty (finding 3): SQL has no process-boundary column, so a live row delayed ≥60s in-pipeline is indistinguishable from a replay-restamped row and lands in the same excluded count — the column/notes/caveat now say "replay/stall rows excluded" (the in-app gauge KEEPS such rows via its live-boundary discriminator, so the CW alarm still sees stall lag); test_handler.py pins updated (169 tests green)

## Design

Two config kill switches ship with the feature: `[notification] episode_mode = true` and
`[notification] digest_window_secs = 900` (clamped [60,3600] at figment load; 60 == legacy
behavior). `episode_mode=false` makes `episode_key()` consultation a no-op → byte-identical
legacy dispatch.

1. **Silent-instruments retune (Item 1)** — the existing
   `tick_gap_instruments_silent` alarm keeps its resource address (in-place
   update in `app-alarms.tf`, zero state churn) and drops its threshold from
   100 to 25 with an M-of-N 10-of-12 latch (a value flapping 24/26/24
   neither pages nor lets one clean scrape erase 9 minutes of evidence; a
   1–3 min reconnect blip cannot reach 10 breaching minutes). Absolute
   threshold, not %-of-universe: no universe-size denominator exists in
   CloudWatch (`tv_universe_size` is kind-labeled and folds under host-only
   dims) and the universe is envelope-locked [100,1200].
2. **SLO dead-band closure (Item 2)** — a NEW Medium-tier CloudWatch alarm on
   the ALREADY-exported `tv_realtime_guarantee_score` at <0.95, 9-of-15
   (round-2 corrected latch — see Item 2).
   In-app SLO-02 stays log-only (2026-05-11 suppression untouched); the
   <0.80 Critical sibling alarm stays UNTOUCHED. This closes the exact
   0.80–0.95 dead band with zero Rust and zero allowlist change.
3. **Catch-up storm detector (Item 3)** — per-feed EMF export of
   `tv_boundary_catchup_total` + a dhan-dimensioned Sum/5m alarm at a
   PROVISIONAL 2000 threshold (2-of-2). Per-feed is mandatory: Groww's 60s
   margin makes catch-up its healthy steady state; folding feeds together is
   either a mask or a false page.
4. **Exchange-lag gauge + alarm (Item 4, the only Rust)** — no
   exchange→received metric exists today (`tv_wire_to_done_duration_ns` is
   receive→done only). A zero-alloc O(1) ring write on the dhan persist path
   feeds a supervised 10s publisher computing trailing-60s p99 (honestly
   O(N-window), never O(1)) into `tv_dhan_exchange_lag_p99_seconds`, with an
   exact WAL-replay exclusion via the received_at−capture_seq dwell
   discriminator and Rule-3/Rule-11 publish gating.
5. **Portal lag SQL truth (Item 5)** — IST-now correction + receive-time
   population + exact capture_seq replay excluder + partition-pruning ts
   bound, with Rule-11 companion rendering (excluded-count, no zero-valued
   percentiles on empty sets).

Types (all Copy where possible; no I/O, no clock reads in this file):
- `#[derive(Copy,Clone,Hash,PartialEq,Eq,Debug)] pub struct EpisodeKey { pub family: EpisodeFamily, pub conn: u8 }`
- `#[derive(Copy,Clone,...)] pub enum EpisodeFamily { MainFeedWs, OrderUpdateWs }` (extensible; only these two today — covers the actual 40-message storms)
- `pub enum EpisodeRole { Open, Progress, Resolve }`
- `pub enum EpisodePhase { Down, Recovering }`
- `pub struct EpisodeState { pub key: EpisodeKey, pub message_id: Option<i64>, pub opened_at_ms: u64, pub last_event_ms: u64, pub occurrences: u32, pub attempts: u32, pub severity_peak: Severity, pub explained: bool, pub last_render_hash: u64 /* FNV-1a — skip no-op edits */, pub last_edit_ms: u64, pub edit_failures: u8, pub phase: EpisodePhase }`
- `pub struct ClosedTombstone { pub message_id: i64, pub closed_at_ms: u64 }` (enables flap-reopen)
- `pub struct EpisodeConfig { pub edit_min_interval_secs: u64 /*20*/, pub stability_secs: u64 /*60*/, pub reopen_secs: u64 /*120*/ }` with pinned consts `EPISODE_EDIT_MIN_INTERVAL_SECS=20`, `EPISODE_STABILITY_SECS=60`, `EPISODE_REOPEN_SECS=120`, `EPISODE_STEADY_MAX_CHARS=320`, `EPISODE_STEADY_MAX_LINES=3`, `EPISODE_FIRST_PAGE_MAX_CHARS=3800` (== TELEGRAM_CHUNK_LIMIT_CHARS), `EPISODE_REHYDRATE_MAX_AGE_SECS=7200`.
- `pub enum EpisodeAction { SendFirstPage, Edit { message_id: i64, close: bool }, EditThrottled, SendNewFallback, Ignore }`

THE pure total FSM (ratchet target #1):
`pub fn next_episode_action(state: Option<&EpisodeState>, tombstone: Option<&ClosedTombstone>, role: EpisodeRole, severity: Severity, now_ms: u64, cfg: &EpisodeConfig) -> EpisodeAction`
- No state, no fresh tombstone → SendFirstPage (this IS the preserved High/Critical bypass first page; SNS-SMS leg rides it).
- No state, tombstone within reopen_secs, role=Open|Progress → Edit{message_id: tombstone.message_id, close:false} (flap folds into the SAME bubble; if the edit later returns Fallback the ladder sends fresh).
- State + Progress → Edit unless (now - last_edit_ms) < edit_min_interval*1000 (→ EditThrottled; counters still folded by caller) or render-hash unchanged (caller-side skip).
- State + Resolve → Edit{close:false} rendering "reconnected — confirming…" and phase:=Recovering (robustness graft: NO instant green).
- State(Recovering) + Open|Progress → Edit (revert to Down on the same bubble).
- state.message_id == None → SendNewFallback.
- Ignore is reachable ONLY for role=Resolve with no state/tombstone. Proptest pins: total (never panics) AND severity ≥ High never maps to Ignore when role ∈ {Open, Progress}.

Stability promotion (shell, not FSM): `EpisodeRegistry::tick(now_ms)` — called from the EXISTING coalescer drain ticker (service.rs:503-517) each 10s tick (and from a tiny spawn_episode_ticker only when the coalescer is disabled) — promotes Recovering→removed after stability_secs with no new Open/Progress, issuing the final green Edit{close:true} and writing a ClosedTombstone (kept reopen_secs, then GC'd by the same tick).

Registry: `pub struct EpisodeRegistry { inner: Mutex<HashMap<EpisodeKey, EpisodeState>>, tombstones: Mutex<HashMap<EpisodeKey, ClosedTombstone>> }` field on NotificationService (cold path; poisoned-mutex recovery via `into_inner()` — coalescer.rs:278 pattern).

Pure renderers (in episode.rs; DisconnectCause strings embedded VERBATIM — disconnect_cause.rs mutation pins untouched):
- `pub fn render_episode_first_page(event_body: &str) -> String` — the existing full message_body() output (Likely source/Confirm block, WS-GAP-10 paragraph via the event's reason field), raw reason clipped at 600 chars, defensively truncated at a newline boundary to ≤ EPISODE_FIRST_PAGE_MAX_CHARS so the first page is ALWAYS one chunk (its message_id unambiguously names the bubble).
- `pub fn render_episode_steady(state: &EpisodeState, ctx: &EpisodeRenderCtx) -> String` — exactly ≤3 lines / ≤320 chars, e.g. line1 `⚠️ [HIGH] 🔷 DHAN — WS feed 1/1 DOWN`, line2 `Since 10:02 AM IST · 7 drops · 31 reconnect attempts`, line3 `Now: reconnecting automatically`. Truncates defensively, never panics/never chunks.
- `pub fn render_episode_recovering(...)` — steady + `Now: reconnected — confirming…`.
- `pub fn render_episode_recovered(state, ctx) -> String` — ONE green line: `✅ Recovered — down 28m 40s, 31 attempts (10:02–10:31 AM IST)`.
- `explained: bool` gates the boilerplate: set true after first page render; PERSISTED so a restart never re-sends the paragraph.

Snapshot codec (pure): `pub mod episode_snapshot { pub fn encode(entries: &[EpisodeState]) -> String; pub fn decode(json: &str, now_ms: u64, today_ist: NaiveDate) -> Vec<EpisodeState> }` — decode drops entries older than EPISODE_REHYDRATE_MAX_AGE_SECS OR from a previous IST trading day (both bounds — robustness graft); corrupt JSON → empty Vec, fail-open. Serialized fields only: {family, conn, message_id, opened_at_ms, occurrences, attempts, explained, phase} — no reasons, no secrets.

### Module 2 — `crates/core/src/notification/events.rs` (accessors only; message_body/to_message untouched)

- `pub fn episode_key(&self) -> Option<EpisodeKey>` next to topic() @2223: WebSocketDisconnected/WebSocketDisconnectedOffHours/WebSocketReconnected → Some(MainFeedWs, connection_index as u8); OrderUpdateDisconnected/OrderUpdateReconnected → Some(OrderUpdateWs, 0); ALL other variants → None (legacy path byte-identical). Zero-alloc (Copy) — DHAT-pinned on the bypass arm.
- `pub fn episode_role(&self) -> EpisodeRole`: Disconnected* → Open (FSM decides Open-vs-Progress by state presence), Reconnected* → Resolve.
- WS-GAP-10: NO change to order_update_connection.rs emit args or emit_order_update_ws_audit/emit_ws_audit calls — the outage_paged latch already makes the 5-line paragraph once-per-episode; it rides the episode first page. Subsequent failures reach notify() as the same OrderUpdateDisconnected variant → FSM folds them into edits. ws_event_audit provably untouched (0 refs from events.rs; choke points not edited).

### Module 3 — `crates/core/src/notification/service.rs` (thin transport shell)

- Pure `pub(crate) fn parse_send_message_id(body: &str) -> Option<i64>` — parses `result.message_id` from the sendMessage response (today discarded at :804). 200-with-unparseable-body counts as DELIVERED without id (never re-send; subsequent events take SendNewFallback for that episode).
- `pub(crate) async fn send_telegram_chunk_with_retry_returning_id(...) -> (bool, Option<i64>)` — same 3-attempt/100ms→2s ladder + classify_telegram_status; used ONLY by the episode path; existing send_telegram_chunk_with_retry untouched.
- `pub(crate) async fn edit_telegram_message_with_retry(client, base_url, bot_token: &SecretString, chat_id, message_id: i64, text: &str) -> EditOutcome` — raw reqwest POST `format!("{}/bot{}/editMessageText", base_url, token.expose_secret())`, JSON {chat_id, message_id, text, parse_mode:"HTML"}, same ladder. NO teloxide.
- Pure `pub(crate) fn classify_edit_body(status: u16, body: &str) -> EditOutcome { Applied, NotModifiedNoop /*400 'message is not modified' = success*/, Fallback /*400 'message to edit not found'|"message can't be edited"|other permanent 4xx*/, Transient /*429,5xx*/ }` (robustness matrix — kills the mvp fallback-spam bug).
- CALL SITE: inside notify()'s tokio::spawn, BEFORE the force_immediate/coalescer branch (:339): `if self.episode_mode && let Some(key) = event.episode_key() { self.dispatch_episode_event(key, event.episode_role(), severity, &event).await; return; }`. `dispatch_episode_event` is the ONLY caller of edit_telegram_message_with_retry (guard-pinned). Action map: SendFirstPage → existing telegram_message_prefix + single-chunk send via ..._returning_id, store message_id, fire SNS-SMS leg (severity≥High) — SMS exactly once per episode open; Edit → render (steady/recovering/recovered) with prefix, FNV-hash skip, edit with retry; Fallback OR edit_failures≥2 → fresh sendMessage_returning_id, replace message_id, `counter!("tv_telegram_edit_fallback_total","reason"=>"not_found"|"transient_exhausted")`; terminal send failure → EXISTING tv_telegram_dropped_total{reason="send_failed"} + error!(code=Telegram01Dropped) — suppression is never a decision, only transport can fail (never-drop ladder).
- Persistence shell: mpsc-fed writer task (prev_close_writer pattern), debounced ≤1/s, tokio::fs write to `data/notify/episodes.json`; write failure → error!(code = ErrorCode::Telegram03EpisodeDegraded.code_str(), reason="store_write_failed") (satisfies error_level_meta_guard — never warn!). Boot: `EpisodeRegistry::rehydrate(path, now)` called from NotificationService::initialize (skipped in NoOp mode); fail-open, never gates boot or any notify().
- `pub async fn shutdown_flush(&self)` (robustness graft): drain_all() → deliver_summaries → synchronous episode-store flush, bounded 10s; wired in main.rs graceful-shutdown teardown.

ONE new ErrorCode (mvp discipline over robustness's two): `ErrorCode::Telegram03EpisodeDegraded` (code_str "TELEGRAM-03", Severity::Low, auto-triage-safe) covering reason ∈ {store_write_failed, rehydrate_corrupt, edit_fallback_storm}. Rule-file mention: new §"TELEGRAM-03" section APPENDED to `.claude/rules/project/wave-3-error-codes.md` in the SAME PR (crossref test satisfied; no new rule file).

### Module 4 — `crates/core/src/notification/coalescer.rs` (digest)

- Pure `pub fn classify_dispatch(severity: Severity, policy: DispatchPolicy, episode: Option<EpisodeRole>, in_market_hours: bool) -> DispatchLane { Immediate, EpisodeEdit, Digest, Coalesce60 }`: Critical|High → NEVER Digest (Immediate or EpisodeEdit — unrepresentable, exhaustive-match ratcheted); DispatchPolicy::Immediate → Immediate always (green boot pings unchanged); episode Some(_) → EpisodeEdit; else Medium|Low|Info → Digest in-market, Coalesce60 off-hours (today's 60s per-topic behavior preserved off-hours).
- `CoalescerConfig` gains `market_hours_window: Duration` from `[notification] digest_window_secs = 900` (figment default 900, clamped [60,3600]); `drain_mature` grows a window parameter (`drain_mature_with_window`); the drain ticker computes the effective window via injectable `now_fn: fn() -> u64` + is_within_market_hours_ist() and FORCE-DRAINS at the 15:30 IST close boundary (robustness graft — no digest straddles overnight).
- Pure `pub fn render_digest(summaries: &[DrainedSummary], window_start_ms: u64, window_end_ms: u64) -> String`: `🔵 15-min digest (10:00–10:15 AM IST)` + one `• <topic> xN` line per bucket + `(+M more)` cap markers; same 10-sample cap + tv_telegram_dropped_total{reason="coalesced_sample_capped"}. Single severity-tag prefix contract unchanged (deliver_summaries adds it once).
- CONSCIOUS RATCHET RE-PINS in the SAME PR with the dated 2026-07-07 operator directive quoted in the PR body: (a) dhat_telegram_dispatcher.rs bypass zero-alloc set narrows to {Critical, High} + NEW assertion episode_key() allocates zero blocks on the bypass arm; (b) coalescer Medium-bypass tests rewritten against classify_dispatch.

### Module 5 — Lambda (`deploy/aws/lambda/telegram-webhook/handler.py`)

- Pure `_house_line(alarm: dict) -> str` → `{emoji} {plain_line}\n{ist_time} IST`: plain line from `ALARM_PHRASES: dict[str,str]` (known tv-* alarm names → auto-driver English, e.g. 'tv-prod-order-update-ws-inactive' → 'Order confirmations feed has gone quiet') with tokenized fallback (strip `tv-<env>-`, de-hyphenate — fail-open, never KeyError); NewStateReason NEVER enters Telegram text (print()-logged to CloudWatch Logs only).
- Pure `_ist_12h(state_change_time: str) -> str` (+05:30, `%-I:%M %p`, falls back to invocation time on malformed input).
- `parse_mode` REMOVED from _post_to_telegram payload (plain text) — deletes the unescaped-Markdown silent-400 drop class.
- Pure `_fold_records(records) -> list[str]`: within one invocation, ALARM+OK pair for the same AlarmName folds to ONLY `✅ {name} recovered — {ist} IST`; all lone-OK records fold into ONE recovered line; ALARM records stay individual (never digested).
- Warm-container `_LAST_SENT: dict[name,(state,epoch)]` suppresses duplicate SAME-state OK repeats within 300s; ALARM-state records are NEVER suppressed by this cache (robustness graft; fail-open on cold start — duplicate ✅ acceptable, dropped 🆘 never). SSM failure keeps re-raising (SNS retry). Cross-invocation ALARM→OK edit-in-place is explicitly OUT of scope (stateless Lambda; documented envelope).
- CI wiring (mvp discipline — no new job, no all-green needs change): Makefile target `lambda-test` running `python3 -m unittest discover deploy/aws/lambda/telegram-webhook` + ONE step added to the EXISTING `Repo Guards` job in ci.yml invoking `make lambda-test`. Plus Rust source-scan ratchet `crates/core/tests/telegram_lambda_house_style_guard.rs` (build-failing on main).

### How each of the 5 scope items is satisfied

1. One-incident-one-bubble live edit: EpisodeKey/FSM + editMessageText transport + message_id capture + Recovering→Closed green flip + 120s flap-reopen tombstone; edge-triggered (state presence IS the latch, audit-findings Rule 4).
2. Boilerplate diet: explained-flag gates DisconnectCause block + WS-GAP-10 paragraph to first page only; steady renders ≤3 lines/320 chars; mutation-pinned strings embedded verbatim, never edited.
3. Lambda house-style: _house_line/_ist_12h/_fold_records, NewStateReason dropped, plain-text mode, tests CI-wired.
4. LOW/MED 15-min digest: classify_dispatch + market_hours_window=900s + render_digest + close-boundary force-drain; off-hours keeps today's 60s coalescing.
5. Never drop CRITICAL/HIGH + High-first-page bypass + no hot-path alloc + audit untouched: proptest never-Ignore pin; SendFirstPage is the unchanged immediate bypass send (coalescer never consulted); fallback ladder terminates at the existing TELEGRAM-01 loudness; DHAT re-pin {Critical,High} + zero-alloc episode_key; ws_event_audit choke points not edited (guard-verified).

---

## Edge Cases

1. **Market-close boundary (15:30→16:30 scrape tail):** the silent-instruments
   gauge and the lag gauge stop being SET after close but the agent keeps
   scraping the LAST value until box stop — handled by (a) the window-gate
   Lambda disabling actions outside 09:20–15:35 IST and (b) Item 4's Rule-3
   in-session publish gate (gauge simply stops updating; notBreaching absorbs
   the missing-data tail after box stop).
2. **IST midnight / date rollover:** capture_seq is UTC-wall-nanos-monotonic
   and received_at is IST-shifted storage; the Item 5 predicates compare
   like-with-like (both IST-aligned after the +19800000000µs alignment) so
   midnight does not flip sign; the Item 4 dwell discriminator is a pure
   duration difference, date-free.
3. **WAL replay at EXACTLY the 60s dwell boundary:** admission uses strict
   `< REPLAY_EXCLUDE_DWELL_NANOS`; a PRE-live-boundary row dwelling exactly
   60.000000000s is EXCLUDED (pinned by
   `test_replay_dwell_boundary_excludes_at_exactly_60s`); a POST-boundary
   (live) row is KEPT at ANY dwell — a >60s consumer stall must never
   self-censor the lag signal (round-2 fix, finding 3 — pinned by
   `test_live_stall_dwell_over_60s_post_boundary_capture_is_kept`).
4. **Negative lag / clock skew:** exchange_ts ahead of received_at (Dhan
   second-granularity rounding, ≤2s host skew per BOOT-03) →
   `saturating_sub` clamps to 0 and the clamp is COUNTED — never a panic,
   never a negative sample.
5. **Groww rows at the dhan site:** the ring write is wired at the DHAN
   persist site only; the dhan-only metric NAME prevents label folding even
   if routing ever changed. Item 5 SQL excluder for groww (round-1
   correction, finding 10 — the earlier "no-op by construction,
   receipt−capture ≤ 0" rationale was FALSE): groww `capture_seq` is
   seeded from the tick's EXCHANGE timestamp in IST nanos
   (`groww_bridge.rs` `next_capture_seq(ts_ist_nanos)` — replay-stable,
   NOT a receipt stamp), so receipt−capture equals the positive lag. The
   dhan discriminator would censor genuine groww lag >60s and would NOT
   exclude replay-restamped groww rows — so the excluder is FEED-AWARE
   (applied to `feed='dhan'` only); groww's "replay rows excluded" renders
   "—" (n/a, documented limitation — replay-restamped groww rows are not
   distinguishable via capture_seq), never a fake verified-0.
6. **Thin window (pre-open trickle, holiday, feed warming up):** <50 samples
   in the trailing 60s → publish NOTHING (never 0 = "perfect lag");
   notBreaching keeps the alarm quiet; feed-dead ownership stays with the
   silent-instruments + WS alarms.
7. **QuestDB now() is actually IST (fallback):** best-effort `select now()`
   probe; if the box returns IST, drop the `dateadd('m',330)` terms — the
   two-layer predicate structure is unchanged. Default ships WITH +330m.
8. **CW agent restart mid-day:** counter DELTA semantics reset cleanly (first
   post-restart scrape publishes the delta since agent start); Sum/5m dips
   for ≤1 period; M-of-N latches tolerate the gap; missing scrapes are
   notBreaching.
9. **Gate-window edges (09:20 / 15:35 IST):** the 08:30 boot catch-up wave
   sits entirely outside the gate so it can never page; an alarm latched
   into ALARM before 09:20 pages only once actions are enabled by the gate
   Lambda.
10. **Universe re-scope toward the 1200 cap:** absolute threshold 25 becomes
    2.1% of universe — dated comment in app-alarms.tf mandates a revisit if
    the subscription set re-scopes; the [100,1200] envelope lock bounds the
    drift.
11. **Ring wraparound under burst (>32,768 ticks between snapshots):** oldest
    samples overwritten — the trailing-60s window is best-effort above ~546
    ticks/s sustained; 32,768 slots = ~65s headroom at the measured ~500/s;
    documented in the module doc, and the p99 remains a valid sample-based
    estimate of the window (pinned by `test_ring_wraparound`).
12. **A 1–3 min reconnect blip:** cannot reach 10-of-12 breaching minutes
    (Item 1), cannot reach 9-of-15 (Item 2), cannot hold 10/10 on a
    self-decaying trailing p99 (Item 4) — by construction none of the new
    alarms page on a healthy bounded reconnect.

## Failure Modes

1. **Window-gate Lambda dead → all gated alarms stay actions-disabled
   (dark):** accepted single point of coupling — it already gates the
   existing liveness alarms and its own health is covered by the
   market-hours-liveness machinery. Documented in Open Questions.
2. **Lag publisher task dies:** supervised respawn (WS-GAP-05/SLO-03
   pattern) with a respawn counter; a flapping publisher is visible, and a
   dead-forever publisher leaves the gauge UNSET → missing data →
   notBreaching (silent-instruments + WS alarms still own feed-dead).
   **Never-spawned case (round-1 fix, findings 1/6/9):** the supervisor is
   spawned from BOTH boot arms (fast crash-recovery + start_dhan_lane)
   behind a once-per-process guard, and the secret_manager.rs ratchet pins
   exactly 2 guarded call sites — removing either boot-path spawn fails
   the build (a lane-only spawn left the gauge dark for the whole session
   after a mid-market crash restart, silently, because the lag alarm is
   notBreaching on missing data).
3. **Ring writer starved / tick thread stalls:** no new samples → thin
   window → publisher publishes nothing (never stale-republishes) — the
   stall itself is owned by the existing tick-gap/WS alarms.
4. **CW export lag / EMF drop:** missing datapoints are notBreaching on
   every new alarm; M-of-N latches tolerate isolated gaps without either
   paging falsely or erasing accumulated evidence.
5. **Threshold-2000 healthy floor unknown:** labeled PROVISIONAL in the
   alarm_description; one trading-week observation of the per-feed Sum(5m)
   distribution is mandated; ratchet with a dated note if the healthy floor
   approaches 2000.
6. **Correlated 4-page fatigue:** one real feed-degradation incident will
   page up to 4 alarms (silent-instruments, SLO-degraded, catchup-storm,
   lag-p99) — ACCEPTED BY DESIGN: after a zero-page all-day incident,
   redundant paging is the chosen failure direction.
7. **Terraform apply partially fails (new file applies, gate append
   doesn't):** alarms are created with `actions_enabled=false` → fail-dark,
   not fail-noisy; the gate append is in the same PR/apply and the
   liveness-alarm ratchets pin the ALARM_NAMES join.

## Test Plan

- **Rust unit (crates/core, `feed_lag_monitor` + tick_processor wiring):**
  `test_replay_dwell_boundary_excludes_at_exactly_60s`,
  `test_thin_window_publishes_nothing`, `test_compute_window_p99_ns_on_known_distribution`,
  `test_ring_wraparound`, `test_negative_lag_clamped`,
  `test_out_of_session_publishes_nothing`.
- **DHAT zero-alloc:** `crates/core/tests/dhat_feed_lag_ring.rs` — ring write
  path allocates nothing across 10K calls.
- **Criterion + budget:** ring-write bench with a
  `quality/benchmark-budgets.toml` entry; the publisher's p99 computation is
  COLD path (10s cadence) — measured but budgeted as cold, labeled
  O(N-window) in comments and PR body (NEVER O(1)).
- **Ratchets (crates/common/tests/cloudwatch_app_alarms_wiring.rs):** EMF
  name-count pin 21→23 (`test_emf_metric_selectors_name_count_is_twenty_three`
  replaces `test_emf_metric_selectors_name_count_is_twenty_one`), two-file
  byte-equality (`test_reference_cloudwatch_agent_json_matches_deployed_emf_declaration`)
  + superset (`test_deployed_emf_declaration_is_superset_of_every_alarm_metric`)
  tests extended to parse the SECOND metric_declaration shape; alarm-count
  pin extended to scan `silent-feed-alarms.tf` (currently file-scoped to
  app-alarms.tf — verified); new pins:
  `test_tick_gap_silent_alarm_threshold_is_twenty_five`,
  `test_tick_gap_silent_alarm_is_window_gated`,
  `test_realtime_guarantee_degraded_alarm_threshold_matches_slo_warn`,
  `test_silent_feed_alarms_are_window_gated`,
  `test_boundary_catchup_alarm_uses_per_feed_dimensions`.
- **Terraform:** `terraform validate` + `terraform plan` — Item 1 must show
  an IN-PLACE update (`~`) on the existing alarm resource, never
  destroy/create.
- **Lambda (Item 5):** local `python -m py_compile`; live best-effort
  `select now()` probe against QuestDB (UTC assumption); portal render check
  post-deploy: excluded-count line present, zero-row case renders "no rows
  received in last 120s", never zero-valued percentiles.
- **Scoped per testing-scope.md:** `cargo test -p tickvault-core`; the
  `crates/common` ratchet-file edit escalates to `cargo test --workspace`.

## Rollback

- **Terraform:** revert the PR and `terraform apply` — new alarms in
  `silent-feed-alarms.tf` are deleted wholesale (new file, no shared state);
  Item 1 reverts in place to threshold 100 (same resource address, no state
  surgery); the gate-Lambda ALARM_NAMES join reverts with the same apply.
- **Rust:** reverting the commit removes the ring write (both Dhan persist
  sites in `run_tick_processor` — which runs on BOTH boot arms, fast and
  slow) and the publisher spawns (both boot arms, once-per-process guard);
  the metric simply stops being published (missing data = notBreaching, no
  false page during rollback).
- **Portal SQL:** single-function revert of `_pctl_feed_sql` (+ the
  feed-discovery query) in handler.py restores the previous behavior
  verbatim.
- No schema changes, no data migration, no DEDUP-key changes anywhere — all
  rollbacks are pure code/config reverts.

## Observability

- **New metrics:** `tv_dhan_exchange_lag_p99_seconds` (gauge, in-session
  only — regular + Muhurat windows — ≥50-sample gate),
  `tv_dhan_lag_samples_excluded_total` (exclusion visibility — never
  silent censoring; CloudWatch-exported, part of the 23-name EMF
  allowlist), `tv_dhan_lag_negative_clamped_total` (/metrics-only),
  publisher respawn counter (WS-GAP-05-pattern `{reason}` labels),
  per-feed `tv_boundary_catchup_total` CW series (host,feed dims).
- **New/retuned alarms (4):** `tick_gap_instruments_silent` (retuned 25,
  10-of-12), `realtime_guarantee_degraded` (<0.95, 9-of-15),
  `boundary_catchup_storm_dhan` (Sum/5m >2000 PROVISIONAL, 2-of-2),
  `dhan_exchange_lag_p99_high` (>10s, 10/10) — all window-gated
  09:20–15:35 IST, all notBreaching, all edge-triggered natively (Rule 4).
- **Portal:** honest lag panel — receive-time population, replay-excluded
  count rendered, empty-population honesty line, extended caveat block (6h
  ceiling, replay exclusion, ≥1s Dhan LTT floor).
- **Docs:** metrics catalog entries, aws-budget.md cost note (~$1.20/mo
  total), dated PROVISIONAL-threshold soak note for the catch-up storm
  alarm.

New ErrorCode TELEGRAM-03 (`Telegram03EpisodeDegraded`, Severity::Low, auto-triage-safe) emitted at error! level on store_write_failed / rehydrate_corrupt / edit_fallback_storm — rule-file section appended to `.claude/rules/project/wave-3-error-codes.md` in the same PR (crossref + tag-guard + error_level_meta_guard all satisfied; no warn! on any persist/send failure arm, guard-pinned). Terminal delivery failure keeps the existing TELEGRAM-01 error! + counter contract exactly.

Telegram surface itself is the primary operator observability: one bubble per incident with live counters, one green close line, one 15-min digest bubble; ws_event_audit remains the untouched forensic record (choke points byte-unchanged, guard-verified). Lambda: NewStateReason forensics preserved in CloudWatch Logs (print-logged), removed only from the operator Telegram surface; the existing tv-<env>-telegram-webhook-errors self-defense alarm is unchanged.

---

## Plan Items

- [x] Item 1 — Pure episode core: `crates/core/src/notification/episode.rs` (NEW)
  - Files: crates/core/src/notification/episode.rs, crates/core/src/notification/mod.rs
  - Tests: test_first_high_event_opens_episode_with_send_new, test_repeat_progress_edits_not_sends, test_resolve_enters_recovering_not_instant_green, test_recovering_promotes_to_closed_after_60s_stability_tick, test_flap_during_recovering_reverts_same_bubble_to_down, test_reopen_within_120s_reuses_tombstone_message_id, test_edit_throttle_folds_without_network_action, test_no_message_id_falls_back_to_send_new, proptest_fsm_total_never_ignores_high_critical, test_steady_render_max_3_lines_320_chars_adversarial, test_first_page_render_always_single_chunk, test_explanation_paragraph_only_on_first_render, test_recovery_render_one_line_green_ist_12h, test_episode_renders_pass_commandments_banned_strings, test_snapshot_roundtrip_stale_day_age_and_corrupt_fail_open

- [x] Item 2 — Event accessors: episode_key()/episode_role() in `crates/core/src/notification/events.rs` (message_body/to_message untouched; ws_event_audit choke points untouched)
  - Files: crates/core/src/notification/events.rs
  - Tests: guard_g_ws_audit_choke_points_untouched, test_episode_key_ws_lifecycle_variants_map_to_families, test_episode_role_resolve_for_reconnect_open_for_disconnect, bypass_path_zero_allocation

- [x] Item 3 — Transport shell + episode dispatch + persistence + shutdown_flush in `crates/core/src/notification/service.rs`
  - Files: crates/core/src/notification/service.rs
  - Tests: test_parse_send_message_id_extracts_and_rejects_garbage, test_classify_edit_body_matrix, test_edit_fallback_replaces_message_id_and_counts, test_sms_fires_once_per_episode_open, test_shutdown_flush_drains_digest_and_persists_store

- [x] Item 4 — Digest lane: classify_dispatch + market_hours_window + close-boundary force-drain + render_digest in `crates/core/src/notification/coalescer.rs`
  - Files: crates/core/src/notification/coalescer.rs
  - Tests: test_classify_dispatch_high_critical_never_digest_full_matrix, test_digest_window_900_market_60_off_and_config_clamped, test_digest_force_drain_at_market_close_boundary, test_render_digest_header_topic_counts_ist_window

- [x] Item 5 — ONE new ErrorCode + config knobs + rule-file append
  - Files: crates/common/src/error_code.rs, crates/common/src/config.rs, config/base.toml, .claude/rules/project/wave-3-error-codes.md
  - Tests: existing error_code cross-ref/tag-guard suites — crossref satisfied by the wave-3-error-codes.md append, test_notification_digest_window_secs_clamped_to_60_3600

- [x] Item 6 — Boot/teardown wiring: rehydrate at boot + shutdown_flush in `crates/app/src/main.rs`
  - Files: crates/app/src/main.rs
  - Tests: test_shutdown_flush_drains_digest_and_persists_store, guard_a_notify_consults_episode_key_before_coalescer

- [x] Item 7 — Ratchet re-pins + NEW guards
  - Files: crates/core/tests/dhat_telegram_dispatcher.rs, crates/core/tests/episode_edit_wiring_guard.rs, crates/core/tests/telegram_lambda_house_style_guard.rs
  - Tests: bypass_path_zero_allocation, guard_a_notify_consults_episode_key_before_coalescer, guard_b_edit_message_text_single_build_site, guard_c_edit_transport_blessed_callers_only, guard_d_episode_constants_pinned, guard_e_no_warn_in_episode_failure_arms, guard_f_telegram01_retained_on_terminal_failure, guard_g_ws_audit_choke_points_untouched, guard_sms_leg_rides_first_page_only, guard_escalation_edge_reaches_first_page_bypass, guard_stale_expiry_and_legacy_resolve_wired, guard_house_line_ist_12h_and_fold_records_present, guard_new_state_reason_never_in_telegram_text, guard_parse_mode_absent_from_payload, guard_alarm_never_suppressed_by_warm_cache

- [x] Item 8 — Lambda house-style formatter + tests + CI wiring
  - Files: deploy/aws/lambda/telegram-webhook/handler.py, deploy/aws/lambda/telegram-webhook/test_handler.py, Makefile, .github/workflows/ci.yml
  - Tests: guard_python_test_lane_carries_contract_tests, guard_house_line_ist_12h_and_fold_records_present (the six Python unittest names — test_house_line_no_raw_threshold_json etc. — run via make lambda-test in the Repo Guards CI job and are name-pinned by guard_python_test_lane_carries_contract_tests)

---

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Main-feed WS drops, 40 disconnect events over 30 min | ONE bubble: first page + throttled live edits (≤3/min), never 40 messages |
| 2 | Feed reconnects and stays up 60s | Bubble edits to "confirming…" then ONE green close line |
| 3 | Reconnect flap <60s | Same bubble reverts to Down; no new bubble |
| 4 | Re-disconnect <120s after close | Tombstone reopen on the SAME bubble |
| 5 | App restart mid-outage | Rehydrated registry resumes editing the same bubble; boilerplate not re-sent |
| 6 | Telegram edit permanently fails | Fresh bubble sent (duplicate-over-drop); counter + TELEGRAM-01 loudness on terminal failure |
| 7 | Low/Info chatter in market hours | ONE 15-min digest bubble; force-drained at 15:30 IST |
| 8 | Critical/High event, episode or not | NEVER digested; immediate first page; SNS-SMS once per episode open |
| 9 | CloudWatch ALARM→OK in one Lambda batch | ONE plain-English ✅ recovered line, IST 12h time, no raw JSON |
| 10 | episode_mode=false rollback | Byte-identical legacy per-event dispatch |

---

## Per-Item Guarantee Matrix (15-row + 7-row — per `.claude/rules/project/per-wave-guarantee-matrix.md`)

This plan carries the mandatory matrices by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md`; the per-plan instantiation follows.

### 15-row 100% Guarantee Matrix

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | unit tests on every `feed_lag_monitor` pure fn; coverage delta ≥ 0 vs `quality/crate-coverage-thresholds.toml` floors | post-merge llvm-cov | Item 4 PR includes coverage delta |
| 100% audit coverage | N/A — no new SEBI-relevant typed event; alarms/metrics are observability-plane (CloudWatch alarm history is the record) | CW alarm history | noted per item |
| 100% testing coverage | unit + DHAT + Criterion + ratchet source-scan, scoped to crates/core + crates/common per testing.md | `cargo test -p tickvault-core` + workspace on common | Test Plan section |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + pre-commit gates | pre-push mandatory | all gates green before PR |
| 100% code performance | DHAT zero-alloc on the ring write + Criterion + benchmark-budgets entry; publisher honestly O(N-window) COLD | `scripts/bench-gate.sh` | Item 4 is the only hot-path item |
| 100% monitoring | the deliverable IS monitoring: 1 new gauge + 1 exclusion counter + 1 respawn counter + per-feed CW series + 4 alarms | CW console + `/metrics` | Observability section |
| 100% logging | publisher respawn logs `error!` + respawn counter (WS-GAP-05 supervisor pattern); deliberately NO new ErrorCode / `code =` field (least-new-surface decision documented at the supervisor doc comment — the tag-guard requires `code =` only when the message mentions a tracked code, which this one does not); no new silent path | errors.jsonl | Item 4 |
| 100% alerting | 4 window-gated CW alarms → SNS → Telegram; notBreaching + M-of-N semantics documented per alarm | gate-Lambda ALARM_NAMES pin | Items 1–4 |
| 100% security | no secrets, no new inputs, no unsafe; Lambda SQL interpolates only a fixed internal feed-name set (no user input) | secret-scan + cargo audit | all items |
| 100% security hardening | no new attack surface (no new endpoints, no new deps) | post-deploy IP verify unchanged | N/A — observability-only change |
| 100% bugs fixing | adversarial 3-agent review BEFORE + AFTER impl | per-PR | required |
| 100% scenarios covering | 12 edge cases enumerated above, each with an owner (test, gate, or notBreaching) | Edge Cases section | per item |
| 100% functionalities covering | every new pub fn in feed_lag_monitor has a call site + matching #[test] | pre-push gates 6+11 | Item 4 |
| 100% code review | 3-agent adversarial on the diff before AND after implementation | per-PR | required |
| 100% extreme check | ratchet tests fail the build on regression (thresholds, gate-wiring, EMF pins, alarm counts) | every commit | Items 1–4 ratchets |

### 7-row Resilience Demand Matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | ZERO tick-path changes — notification cold path only; ring→spill→DLQ untouched | no tick-pipeline/storage file in the diff |
| WS never disconnects | SubscribeRxGuard / pool watchdog / emit_ws_audit call sites byte-unchanged (wiring guard (g)) | Item 2 |
| Never slow/locked/hanged | episode dispatch runs inside notify()'s existing tokio::spawn (cold); 20s/key edit throttle bounds Telegram traffic; DHAT pins zero-alloc on the bypass arm | Item 7 DHAT |
| QuestDB never fails | no QuestDB surface in this diff (episodes.json is a plain advisory file, fail-open) | Rollback section |
| O(1) latency | episode_key() is a Copy match (O(1)); registry is a cold-path HashMap behind a Mutex — honestly O(1) amortized lookup, never on the tick path | Items 1/2 |
| Uniqueness + dedup | EpisodeKey (family, conn) is the composite episode identity; FNV render-hash dedups no-op edits; no new DB table so no DEDUP-key surface | Item 1 |
| Real-time proof | live-edited bubble + green close + digest ARE the operator real-time surface; counters + TELEGRAM-01/03 codes back it | Observability section |

### Honest 100% claim (mandatory wording)

"100% inside the tested envelope, with ratcheted regression coverage: the pure total FSM is
proptest-pinned never to Ignore a High/Critical Open/Progress event; every transport failure
terminates at the existing TELEGRAM-01 error!+counter loudness (suppression is never a decision,
only transport can fail); the episode store is advisory fail-open (corrupt/stale → fresh first
page, one duplicate bubble, never a lost page); DHAT re-pins the {Critical,High} zero-alloc
bypass; `episode_mode=false` restores byte-identical legacy dispatch. Beyond the envelope, a
dead drain ticker leaves a bubble amber (visible via missing digests + TELEGRAM-01 signals) —
bounded degradation, never silent loss."

---

1. **QuestDB `now()` timezone** — Assumed UTC (matches storage code:
   received_at is written `Utc::now()+IST_UTC_OFFSET_NANOS`, i.e. the DB
   clock is UTC and rows are IST-shifted). Ship WITH
   `dateadd('m',330,now())`; best-effort live `select now()` probe; if the
   box returns IST, drop the +330m terms — nothing else changes.
2. **capture_seq wall-clock assumption** — Assumed: capture_seq ≈ UTC wall
   nanos at the original WS-read instant (`max(prev+1, wall_nanos)` per
   data-integrity.md) and is PRESERVED through WAL re-injection. Spot-check
   on live data pre-merge (compare capture_seq vs received_at on fresh
   rows).
3. **Boundary-catchup healthy Dhan floor** — Unknown. Ship PROVISIONAL 2000
   (2.25–2.9× below incident rate) + mandatory 1-trading-week soak of the
   exported per-feed Sum(5m) distribution; ratchet with a dated note if the
   healthy floor approaches 2000.
4. **M-of-N deviation from literal "sustained N×M" wording** — Deliberate:
   strict-consecutive evaluation on the oscillating incident data (125
   crossings, 24/26/24 flapping) would never latch and reproduces the miss
   (Rule-11 false-OK). 10-of-12 / 9-of-15 are the honest latches (9-of-15
   is the round-2 correction: freshness-only breach needs ≥39 of 776
   silent, so incident minutes at 29–38 silent sample Healthy and a
   12-of-15 latch could miss the marginal band); Item 4
   keeps strict 10/10 because its metric is self-smoothing.
5. **Medium-tier Telegram rendering** — Limitation: telegram-webhook
   handler.py stamps the same emoji on any ALARM; a Medium-vs-Critical
   visual split is a FLAGGED FOLLOW-UP, not in this PR.
6. **Alarm-count ratchet scan scope** — Verified file-scoped to
   `app-alarms.tf` (`alarm_metric_names()` reads only that file); MUST be
   extended in this PR to also scan `silent-feed-alarms.tf`, else the new
   alarms escape the three-way drift guard.
7. **Gate-Lambda single point of coupling** — Accepted risk: all four
   alarms are dark if the window-gate Lambda dies; it already gates the
   existing liveness alarms and is itself covered by the
   market-hours-liveness machinery.
8. **Correlated-page fatigue** — Accepted by design: one real feed
   degradation may fire up to 4 pages; after a zero-page all-day incident,
   redundancy is the chosen failure direction.
9. **Seconds-vs-milliseconds units** — Seconds chosen for the lag gauge:
   Dhan LTT has a ≥1s quantization floor, so millisecond units would imply
   false precision; the name carries `_seconds` and the module doc states
   the floor.
