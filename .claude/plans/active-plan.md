# Implementation Plan: Telegram UX Overhaul — Episode Live-Edit Coalescing (One Incident = One Bubble)

**Status:** VERIFIED
**Date:** 2026-07-07
**Approved by:** Parthiban (operator) — directive 2026-07-07
**Branch:** `claude/blissful-fermi-3by0no`

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

---

## Design

Two config kill switches ship with the feature: `[notification] episode_mode = true` and
`[notification] digest_window_secs = 900` (clamped [60,3600] at figment load; 60 == legacy
behavior). `episode_mode=false` makes `episode_key()` consultation a no-op → byte-identical
legacy dispatch.

### Module 1 — `crates/core/src/notification/episode.rs` (NEW, pure core)

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

- Telegram 400 'message is not modified' — classified NotModifiedNoop (success), never triggers fallback spam; FNV render-hash skip prevents the call in the first place.
- Restart mid-outage — rehydrate resumes editing the same bubble (explained flag persisted so boilerplate not re-sent); lost/corrupt episodes.json → fresh first page (one duplicate bubble, never a lost page).
- >48h-old bubble or operator-deleted message — editMessageText returns permanent 400 → Fallback arm sends a fresh bubble, replaces message_id.
- sendMessage 200 with unparseable body — delivered-without-id: no re-send; subsequent events take SendNewFallback for that episode (duplicate-avoidance over id capture).
- Flap cadence: reconnect then re-disconnect <60s → Recovering reverts to Down on the same bubble; re-disconnect <120s after close → tombstone reopen on the same bubble; >120s → new episode (edge-triggered, never suppressed).
- Market-close 15:30 IST straddle — digest force-drained at the boundary; off-hours reverts to the 60s per-topic coalescing.
- digest_window_secs fat-fingered to 0 or 86400 — clamped to [60,3600] at config load.
- Multi-episode simultaneous storm (main-feed + order-update both flapping) — independent keys, each bounded by the 20s edit throttle + hash guard (~3 edits/min/key worst case).
- NoOp-mode boot — episode path short-circuits with the existing noop_mode drop counter; rehydrate skipped.
- Lambda cold start during ALARM→OK gap — warm cache empty → duplicate ✅ possible (accepted); ALARM never suppressed by design.
- AlarmName containing '*'/'`'/'[' — plain-text mode means no Markdown parse failure, no silent 400 drop.
- Poisoned episode Mutex — into_inner() recovery (coalescer house pattern), never a panic in the dispatch task.
- count==1 Low/Info event in the 15-min digest — existing bare-sample fast path + single-prefix contract preserved (no double [LOW] tag).

---

## Failure Modes

- Edit transport fails transient (429/5xx) — 3-attempt ladder; after edit_failures≥2 the fallback ladder sends a fresh bubble (duplicate-over-drop); tv_telegram_edit_fallback_total{reason=transient_exhausted}.
- Edit fails permanent (message gone) — immediate Fallback fresh send; reason=not_found counter.
- Fallback send ALSO fails — existing tv_telegram_dropped_total{reason=send_failed} + error!(code=Telegram01Dropped): identical terminal loudness to today; no new silent path exists.
- Episode store write fails (disk full/unwritable) — error!(code=TELEGRAM-03, reason=store_write_failed); in-memory state keeps working; only cross-restart linkage degrades.
- Rehydrate hits corrupt JSON — error!(code=TELEGRAM-03, reason=rehydrate_corrupt), empty registry, boot proceeds (fail-open, never gates).
- Registry ticker dies with the coalescer drain task — Recovering episodes never promote to green; bounded impact (bubble stays amber); the drain task is the same task that already owns digest delivery, so its death is visible via missing digests + existing TELEGRAM-01 signals.
- Telegram edit rate-limit under a pathological 1-event/sec multi-key storm — bounded by 20s/key throttle + EditThrottled folding; residual 429s burn the retry ladder then fallback (louder, never silent).
- Medium-severity re-route regression risk — a genuinely urgent non-episode Medium now waits ≤~16min in market hours; escape hatches: DispatchPolicy::Immediate per event, digest_window_secs=60 config rollback.
- Lambda regression shipping silently — closed: make lambda-test in Repo Guards + the Rust source-scan guard both fail the build.
- Duplicate Severity enum confusion (events.rs vs error_code.rs) — episode code imports ONLY notification::events::Severity; guard test greps for the wrong import path.

---

## Test Plan

- `episode.rs::test_first_high_event_opens_episode_with_send_new` — no state + role Open, every severity ≥ High → SendFirstPage (preserved bypass first page)
- `episode.rs::test_repeat_progress_edits_not_sends` — state+message_id, Progress → Edit, NEVER a second SendFirstPage until close
- `episode.rs::test_resolve_enters_recovering_not_instant_green` — Resolve → Edit with phase Recovering (no premature ✅)
- `episode.rs::test_recovering_promotes_to_closed_after_60s_stability_tick` — registry.tick with injected clock issues the final green Edit{close:true} + tombstone
- `episode.rs::test_flap_during_recovering_reverts_same_bubble_to_down` — Open/Progress while Recovering → Edit on the SAME message_id
- `episode.rs::test_reopen_within_120s_reuses_tombstone_message_id` — flap after close folds into the same bubble; past 120s → fresh SendFirstPage
- `episode.rs::test_edit_throttle_folds_without_network_action` — <20s since last edit → EditThrottled, occurrences/attempts still advance
- `episode.rs::test_no_message_id_falls_back_to_send_new` — message_id None → SendNewFallback
- `episode.rs::proptest_fsm_total_never_ignores_high_critical` — arbitrary state×role×severity×clock: total, and severity ≥ High with role Open|Progress never → Ignore (the never-drop pin)
- `episode.rs::test_steady_render_max_3_lines_320_chars_adversarial` — 10KB cause string + u32::MAX counters → ≤2 newlines, ≤ EPISODE_STEADY_MAX_CHARS
- `episode.rs::test_first_page_render_always_single_chunk` — split_message_for_telegram(render_episode_first_page(..)) == 1 chunk (edits can never need chunking)
- `episode.rs::test_explanation_paragraph_only_on_first_render` — first render contains 'Likely source:'; explained=true render does NOT
- `episode.rs::test_recovery_render_one_line_green_ist_12h` — starts '✅', zero newlines, matches [0-9]{1,2}:[0-9]{2} (AM|PM) regex
- `episode.rs::test_episode_renders_pass_commandments_banned_strings` — bans 'rkyv','papaya','mpsc','.rs','data/','QuestDB','editMessageText', x.y.z version patterns in every render
- `episode.rs::test_snapshot_roundtrip_stale_day_age_and_corrupt_fail_open` — encode/decode identity; previous-IST-day OR >7200s entries dropped; corrupt JSON → empty, no panic
- `service.rs::test_parse_send_message_id_extracts_and_rejects_garbage`
- `service.rs::test_classify_edit_body_matrix` — 400 'message is not modified' → NotModifiedNoop (success); 'message to edit not found'/'message can't be edited' → Fallback; 429/5xx → Transient
- `service.rs::test_edit_fallback_replaces_message_id_and_counts` — mock 400 → fresh sendMessage, id replaced, tv_telegram_edit_fallback_total incremented; terminal failure routes error!(code=Telegram01Dropped) (base_url-override fixture @1542 pattern)
- `service.rs::test_sms_fires_once_per_episode_open` — SNS-SMS leg exactly once per episode, on SendFirstPage only
- `service.rs::test_shutdown_flush_drains_digest_and_persists_store` — drain_all + deliver + store flush within 10s bound
- `coalescer.rs::test_classify_dispatch_high_critical_never_digest_full_matrix` — all 5 severities × {in,out} market × {Default,Immediate} × {episode,None}: High/Critical → Bypass-class in every cell
- `coalescer.rs::test_digest_window_900_market_60_off_and_config_clamped` — pure window fn + [60,3600] clamp
- `coalescer.rs::test_digest_force_drain_at_market_close_boundary` — injected clock crossing 15:30 IST drains the 15-min bucket
- `coalescer.rs::test_render_digest_header_topic_counts_ist_window` — '🔵 15-min digest (…AM/PM IST)' + '• <topic> xN' + '(+M more)'
- `crates/core/tests/dhat_telegram_dispatcher.rs` — RE-PINNED: {Critical,High} zero-alloc bypass over 30k calls + episode_key() zero-alloc on the bypass arm (dated operator quote in the diff comment)
- `crates/core/tests/episode_edit_wiring_guard.rs` (NEW source-scan) — (a) notify() consults episode_key BEFORE the coalescer branch, (b) 'editMessageText' URL built in exactly ONE site in service.rs, (c) dispatch_episode_event is the sole edit caller, (d) EPISODE_* consts pinned, (e) no warn! in any edit/send/store failure arm, (f) TELEGRAM-01 error! retained on terminal failure, (g) emit_ws_audit/emit_order_update_ws_audit call sites byte-unchanged
- `crates/core/tests/telegram_lambda_house_style_guard.rs` (NEW source-scan of handler.py) — _house_line present; literal 'Reason: {reason}' absent from the Telegram text path; 'parse_mode' Markdown absent; _ist_12h + _fold_records present; ALARM state never routed through the warm-cache suppression
- `test_handler.py` — test_house_line_no_raw_threshold_json, test_ok_flip_single_line_recovered, test_alarm_ok_pair_in_batch_folds_to_recovered_only, test_ist_12_hour_timestamp, test_alarm_never_suppressed_by_warm_cache, test_unknown_alarm_name_fallback_still_plain_english — wired via `make lambda-test` + a step in the existing Repo Guards CI job

---

## Rollback

Three independent levels, no code revert needed for the first two:
1. `[notification] episode_mode = false` in config/base.toml — episode_key consultation short-circuits, dispatch reverts to today's per-event sendMessage path byte-identically (bubbles already sent simply stop being edited).
2. `[notification] digest_window_secs = 60` — restores today's 60s Low/Info coalescing window (Medium re-route to the coalescer remains, the only residual behavior delta).
3. Full `git revert` of the single squash-merged PR restores Medium-bypass + the old DHAT pin + the old Lambda formatter (Lambda redeploys via the existing terraform apply path).

The episodes.json file is advisory — stale files are ignored on any rollback (decode drops unknown/stale entries fail-open). No schema, no QuestDB table, no SSM param changes anywhere in the diff, so rollback has zero data-migration surface.

---

## Observability

New counters (static labels only): `tv_telegram_episode_events_total{action="open"|"edit"|"edit_throttled"|"close"|"reopen"}`, `tv_telegram_edit_fallback_total{reason="not_found"|"transient_exhausted"}`. Existing counters unchanged in meaning: `tv_telegram_dispatched_total{severity,coalesced}`, `tv_telegram_dropped_total{reason=noop_mode|send_failed|coalesced_sample_capped}`.

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
| 100% code coverage | ratcheted floors in `quality/crate-coverage-thresholds.toml`; ~28 new unit/proptest tests in episode.rs/service.rs/coalescer.rs | post-merge llvm-cov | PR includes coverage delta ≥ 0 |
| 100% audit coverage | ws_event_audit remains the forensic record — choke points byte-unchanged (wiring guard (g)); no new SEBI event class introduced | `mcp__tickvault-logs__questdb_sql` | N/A — no new table; guard-pinned untouched |
| 100% testing coverage | unit + proptest (FSM totality) + DHAT (zero-alloc bypass) + source-scan ratchets + Lambda unittest — categories declared per item above | `cargo test -p tickvault-core` + `make lambda-test` | each item names its tests |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + design-first wall | pre-push mandatory | all gates green |
| 100% code performance | DHAT re-pin {Critical,High} + episode_key() zero-alloc on bypass arm; episode/edit path is COLD (notify tokio::spawn), no hot-path change | `cargo test --features dhat` | Item 7 re-pins |
| 100% monitoring | tv_telegram_episode_events_total + tv_telegram_edit_fallback_total (static labels) + existing dispatched/dropped counters | `mcp__tickvault-logs__run_doctor` | Observability section |
| 100% logging | error! with code= on every persist/edit/send failure arm; NO warn! (wiring guard (e)) | errors.jsonl | error_level_meta_guard |
| 100% alerting | terminal failure keeps TELEGRAM-01 error!+counter; TELEGRAM-03 new degraded signal; Lambda self-defense alarm unchanged | run_doctor (CloudWatch alarms) | Item 5 |
| 100% security | SecretString bot token (expose_secret only at URL build, existing pattern); snapshot serializes NO reasons/secrets; secret-scan | `cargo audit` | security-reviewer agent pass |
| 100% security hardening | plain-text Lambda mode kills the Markdown-injection 400 class; no new ingress surface | post-deploy verify | Item 8 |
| 100% bugs fixing | adversarial 3-agent review BEFORE + AFTER impl (>3 crates touched) | pre-PR + post-impl | PR body records both passes |
| 100% scenarios covering | 10-row scenario table above; flap/restart/store-corrupt/rate-limit all ratcheted | scenario tests | items declare scenarios |
| 100% functionalities covering | every new pub fn has call site + test (pub-fn-wiring + pub-fn-test guards) | pre-push gates 6+11 | per-item test lists |
| 100% code review | 3-agent adversarial on diff before AND after | per-PR | PR body |
| 100% extreme check | episode_edit_wiring_guard + telegram_lambda_house_style_guard + DHAT re-pin + proptest never-drop pin all fail the build on regression | every commit | Item 7 |

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

## 2026-07-07 Hostile-Review Fix Addendum (post-implementation, pre-merge)

The adversarial review of the implemented diff surfaced 3 CRITICAL/HIGH behavioral gaps +
2 delivery gaps; all are FIXED in this branch (same PR):

1. **Low→High escalation pages fresh (push + SMS).** `next_episode_action` now routes an
   Open/Progress event at ≥High into a sub-High-peak episode (live state OR ≤120s
   tombstone, both consult `severity_peak`) to `SendFirstPage` — the preserved bypass arm
   the SNS-SMS leg rides. The pre-open Low off-hours storm can never swallow an in-market
   HIGH outage as a silent edit. Counter: `tv_telegram_episode_events_total{action="escalate"}`.
   Tests: test_next_episode_action_escalation_low_peak_to_high_pages_fresh,
   test_apply_event_escalation_marks_decision_and_updates_peak,
   test_tombstone_high_reopen_over_low_peak_pages_fresh,
   test_low_episode_escalation_to_high_sends_fresh_page (mock transport),
   guard_escalation_edge_reaches_first_page_bypass. `severity_peak` is now PERSISTED in the
   snapshot (missing/legacy label → Low, so a post-restart High event re-pages —
   duplicate-over-drop).
2. **Stale-Down expiry + legacy resolve routing (restart edge).** `EpisodeRegistry::tick`
   expires Down episodes with no events for `EPISODE_DOWN_STALE_EXPIRE_SECS = 1800` (no
   tombstone — the next outage opens a FRESH first page with the SMS leg); the drain ticker
   neutralizes the old bubble with a one-line ⚪ close. A `Resolve` with no state and no
   fresh tombstone now maps to `EpisodeAction::SendLegacy` (delivered via the legacy
   immediate lane + counted `legacy_passthrough`) instead of a silent Ignore; Ignore remains
   ONLY for a resolve against a fresh tombstone (recovery already announced by the green
   close). Tests: test_tick_expires_stale_down_and_live_count_drops,
   test_run_episode_tick_expires_stale_down_bubble,
   test_resolve_with_no_state_routes_send_legacy_and_fresh_tombstone_ignores,
   test_resolve_without_state_sends_legacy_message_not_dropped,
   guard_stale_expiry_and_legacy_resolve_wired.
3. **No silent sub-threshold transient edit failure.** The Edit/Transient arm now (a) falls
   back to a FRESH send on the FIRST exhausted transient for episodes with
   `severity_peak ≥ High` (a structurally-final event — e.g. the once-per-outage WS-GAP-10
   page — may never re-drive the ladder), and (b) for sub-High episodes below the threshold
   emits `error!(code=TELEGRAM-03, reason="edit_transient_deferred")` +
   `tv_telegram_edit_fallback_total{reason="transient_deferred"}` — no silent terminal path
   remains. Tests: test_edit_transient_high_falls_back_immediately_low_stays_loud,
   guard_escalation_edge_reaches_first_page_bypass.
4. **`crates/core/tests/telegram_lambda_house_style_guard.rs` DELIVERED** (was promised in
   Item 7 but missing from the diff) — the independent build-failing Rust source-scan of
   handler.py: house-style chain present, `Reason: {reason}` + `parse_mode` payload key
   banned, NewStateReason print-forensics-only, ALARM never routed through the warm-cache
   suppression, the 6 Python contract test names pinned.
5. **pub-fn-test-guard back at baseline 113** — name-matched tests added for every new pub
   fn (episode.rs registry/render/accessor fns, coalescer window fns, service transport fns,
   events episode_role, config digest_window_secs_clamped renamed test). No baseline bump.

New TELEGRAM-03 reasons + counter labels are documented in
`.claude/rules/project/wave-3-error-codes.md` (same PR). Honest envelope unchanged: a
High-over-High tombstone reopen (<120s) still folds into the same bubble by design — that
incident already paged + SMS'd at its open.
